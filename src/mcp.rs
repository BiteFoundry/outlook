//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! # Framing
//!
//! MCP's stdio transport uses **newline-delimited JSON**: exactly one JSON-RPC 2.0
//! object per line. `stdout` carries protocol messages only; all diagnostics go to
//! `stderr`. Notifications (`notifications/*` or requests without `id`) emit no
//! response.

use std::{io::Write, path::PathBuf};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    models::{
        ConversationSummary, ConversationTarget, MentionInfo, OutgoingEvent, OutgoingMail,
        OutgoingTodoTask,
    },
    parser, resolver,
    storage::Storage,
    sync,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "teams";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// Run the stdio MCP server until stdin reaches EOF.
pub async fn run(config: AppConfig, graph: GraphClient, storage: Storage) -> Result<()> {
    if storage.list_conversations().unwrap_or_default().is_empty() {
        if let Err(err) = sync::bootstrap_conversations(&graph, &storage).await {
            eprintln!("{SERVER_NAME} MCP warning: initial conversation bootstrap failed: {err:#}");
        }
    }

    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, user_id={})",
        graph.my_user_id().unwrap_or("unknown")
    );

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = std::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(response) = handle_rpc_line(&config, &graph, &storage, trimmed).await {
            stdout.write_all(encode_message(&response).as_bytes())?;
            stdout.flush()?;
        }
    }

    eprintln!("{SERVER_NAME} MCP server: stdin closed, shutting down");
    Ok(())
}

/// Single-line JSON-RPC handler exposed for deterministic unit & integration testing.
pub async fn handle_rpc_line(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    line: &str,
) -> Option<Value> {
    let message = match parse_message(line) {
        Ok(msg) => msg,
        Err(err_resp) => return Some(err_resp),
    };

    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let has_id = message.get("id").is_some();

    // Notifications (including `notifications/initialized`) never receive a response.
    if method.starts_with("notifications/") || !has_id {
        return None;
    }

    match method {
        "initialize" => Some(success_response(id, initialize_result(&message))),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let outcome = match validate_tool_args(name, &arguments) {
                Ok(()) => call_tool(config, graph, storage, name, &arguments).await,
                Err(error) => Err(error),
            };

            Some(success_response(id, tool_result(outcome)))
        }
        _ => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    }
}

async fn call_tool(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let args = Args::new(arguments);

    match name {
        "list_chats" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;

            let cached: Vec<ConversationSummary> = storage
                .list_conversations()
                .map_err(format_error)?
                .into_iter()
                .filter(|c| !c.kind.is_channel())
                .collect();

            let mut chats = if refresh || cached.is_empty() {
                let fetched = graph.list_chats(limit).await.map_err(format_error)?;
                storage.upsert_conversations(&fetched).map_err(format_error)?;
                let stored: Vec<ConversationSummary> = storage
                    .list_conversations()
                    .map_err(format_error)?
                    .into_iter()
                    .filter(|c| !c.kind.is_channel())
                    .collect();
                if stored.is_empty() {
                    fetched
                } else {
                    stored
                }
            } else {
                cached
            };
            chats.truncate(limit);

            Ok(json!({ "chats": chats }))
        }

        "create_chat" => {
            let members = args.required_str_array("members")?;
            let topic = args.optional_str("topic")?;
            let chat = graph
                .create_chat(&members, topic.as_deref())
                .await
                .map_err(format_error)?;
            storage
                .upsert_conversations(std::slice::from_ref(&chat))
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conversation": chat,
            }))
        }

        "list_teams_channels" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let cached = storage.list_teams().map_err(format_error)?;
            let teams = if refresh || cached.is_empty() {
                let fetched = graph
                    .list_joined_teams_with_channels()
                    .await
                    .map_err(format_error)?;
                storage.upsert_teams(&fetched).map_err(format_error)?;
                let stored = storage.list_teams().map_err(format_error)?;
                if stored.is_empty() {
                    fetched
                } else {
                    stored
                }
            } else {
                cached
            };

            Ok(json!({ "teams": teams }))
        }

        "get_messages" => {
            let target = args.required_str("target")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let conv = cli::resolve_target(storage, graph, &target, offline)
                .await
                .map_err(format_error)?;

            let fetched = if !offline {
                let msgs = graph
                    .list_messages(&conv.target, limit)
                    .await
                    .map_err(format_error)?;
                storage.upsert_messages(&msgs).map_err(format_error)?;
                Some(msgs)
            } else {
                None
            };

            let mut messages = storage
                .list_root_messages(&conv.conv_key, limit, None)
                .map_err(format_error)?;
            if messages.is_empty() {
                if let Some(f) = fetched {
                    messages = f;
                }
            }

            Ok(json!({
                "conversation": conv,
                "messages": messages,
            }))
        }

        "get_thread" => {
            let target = args.required_str("target")?;
            let root_msg_id = args.required_str("root_msg_id")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            let ConversationTarget::Channel {
                ref team_id,
                ref channel_id,
            } = conv.target
            else {
                return Err(format!(
                    "get_thread requires a team channel target, got {}",
                    conv.conv_key
                ));
            };

            let thread = graph
                .get_thread(team_id, channel_id, &root_msg_id, limit)
                .await
                .map_err(format_error)?;
            let mut all = Vec::with_capacity(1 + thread.replies.len());
            all.push(thread.root.clone());
            all.extend(thread.replies.iter().cloned());
            storage.upsert_messages(&all).map_err(format_error)?;

            Ok(json!({
                "conversation": conv,
                "thread": thread,
            }))
        }

        "send_message" => {
            let target = args.required_str("target")?;
            let markdown = args.required_text("markdown")?;
            let subject = args.optional_str("subject")?;
            let mentions = args.optional_mentions("mentions")?;
            let important = args.optional_bool("important")?.unwrap_or(false);

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;

            let mut outgoing = parser::compile_outgoing_markdown(
                &markdown,
                subject,
                mentions,
                vec![],
                vec![],
                None,
            );
            if important {
                outgoing.importance = Some("high".to_string());
            }

            let sent = graph
                .send_message(&conv.target, &outgoing)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_messages(std::slice::from_ref(&sent));

            Ok(json!({
                "ok": true,
                "conversation": conv,
                "message": sent,
            }))
        }

        "reply_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let markdown = args.required_text("markdown")?;

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            let quote = cli::build_quote_reply(storage, graph, &conv, &msg_id).await;
            let outgoing = parser::compile_outgoing_markdown(
                &markdown,
                None,
                vec![],
                vec![],
                vec![],
                quote,
            );

            let sent = graph
                .reply_message(&conv.target, &msg_id, &outgoing)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_messages(std::slice::from_ref(&sent));

            Ok(json!({
                "ok": true,
                "conversation": conv,
                "reply_to": msg_id,
                "message": sent,
            }))
        }

        "send_file" => {
            let target = args.required_str("target")?;
            let path = args.required_str("path")?;
            let caption = args.optional_str("caption")?;
            let thread_root_id = args.optional_str("thread_root_id")?;
            let inline = args.optional_bool("inline")?.unwrap_or(false);

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            let sent = cli::send_file_or_inline(
                graph,
                storage,
                &conv,
                std::path::Path::new(&path),
                caption.as_deref(),
                thread_root_id.as_deref(),
                inline,
            )
            .await
            .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conversation": conv,
                "path": path,
                "message": sent,
            }))
        }

        "download_attachment" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let index = args.optional_usize("index")?.unwrap_or(0);
            let thread_root_id = args.optional_str("thread_root_id")?;
            let out_dir = args
                .optional_str("out_dir")?
                .map(PathBuf::from)
                .unwrap_or_else(|| config.resolved_download_dir());

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            let downloaded = cli::download_message_attachment(
                graph,
                storage,
                &conv,
                &msg_id,
                index,
                thread_root_id.as_deref(),
                &out_dir,
            )
            .await
            .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "path": downloaded.path,
                "file_name": downloaded.file_name,
                "bytes_written": downloaded.bytes_written,
            }))
        }

        "react_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let reaction = cli::normalize_reaction(&args.required_str("reaction")?)
                .map_err(format_error)?;
            let enable = args.optional_bool("enable")?.unwrap_or(true);
            let thread_root_id = args.optional_str("thread_root_id")?;

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            graph
                .set_reaction(
                    &conv.target,
                    &msg_id,
                    thread_root_id.as_deref(),
                    &reaction,
                    enable,
                )
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
                "reaction": reaction,
                "enable": enable,
            }))
        }

        "edit_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let markdown = args.required_text("markdown")?;
            let thread_root_id = args.optional_str("thread_root_id")?;

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            let compiled = parser::compile_outgoing_markdown(
                &markdown,
                None,
                vec![],
                vec![],
                vec![],
                None,
            );
            graph
                .edit_message(
                    &conv.target,
                    &msg_id,
                    thread_root_id.as_deref(),
                    &compiled.body_html,
                )
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))
        }

        "delete_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            args.required_true("confirm")?;
            let thread_root_id = args.optional_str("thread_root_id")?;

            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            graph
                .delete_message(&conv.target, &msg_id, thread_root_id.as_deref())
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))
        }

        "search_messages" => {
            let query = args.required_str("query")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let conv_key_filter = match args.optional_str("target")? {
                Some(t) => {
                    let cached = storage.list_conversations().map_err(format_error)?;
                    Some(
                        resolver::resolve_from_slice(&cached, &t)
                            .map_err(format_error)?
                            .conv_key,
                    )
                }
                None => None,
            };

            let hits = storage
                .search_messages(&query, conv_key_filter.as_deref(), limit)
                .map_err(format_error)?;

            Ok(json!({
                "query": query,
                "hits": hits,
            }))
        }

        "list_members" => {
            let target = args.required_str("target")?;
            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;
            let members = graph
                .list_members(&conv.target)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_members(&conv.conv_key, &members);

            Ok(json!({
                "conv_key": conv.conv_key,
                "title": conv.title,
                "members": members,
            }))
        }

        "mark_read" => {
            let target = args.required_str("target")?;
            let conv = cli::resolve_target(storage, graph, &target, false)
                .await
                .map_err(format_error)?;

            if let ConversationTarget::Chat { ref chat_id } = conv.target {
                graph.mark_chat_read(chat_id).await.map_err(format_error)?;
            }
            let now = chrono::Utc::now().to_rfc3339();
            storage
                .mark_conversation_read(&conv.conv_key, &now)
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conv_key": conv.conv_key,
            }))
        }

        "get_presence" => {
            let user_ids = args.optional_str_array("user_ids")?;
            if let Some(ids) = user_ids {
                let presences = graph
                    .get_presences_by_user_ids(&ids)
                    .await
                    .map_err(format_error)?;
                Ok(json!({ "presences": presences }))
            } else {
                let presence = graph.get_my_presence().await.map_err(format_error)?;
                Ok(json!({ "presence": presence }))
            }
        }

        "set_presence" => {
            let availability =
                cli::normalize_presence_availability(&args.required_str("availability")?)
                    .map_err(format_error)?;
            let activity = args
                .optional_str("activity")?
                .unwrap_or_else(|| availability.clone());
            let expiration_mins = args.optional_u32("expiration_mins")?;

            graph
                .set_my_presence(&availability, &activity, expiration_mins)
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "availability": availability,
                "activity": activity,
                "expiration_mins": expiration_mins,
            }))
        }

        // -------------------------------------------------------------------
        // Outlook Mail tools
        // -------------------------------------------------------------------
        "list_mail_folders" => {
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let folders = if offline {
                storage.list_mail_folders().map_err(format_error)?
            } else {
                let fetched = graph.list_mail_folders().await.map_err(format_error)?;
                let _ = storage.upsert_mail_folders(&fetched);
                fetched
            };
            Ok(json!({ "folders": folders }))
        }

        "list_mails" => {
            let folder = args
                .optional_str("folder")?
                .unwrap_or_else(|| "inbox".to_string());
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(25);
            let unread = args.optional_bool("unread")?.unwrap_or(false);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let folder_id = cli::resolve_mail_folder_id(storage, graph, &folder, offline)
                .await
                .map_err(format_error)?;

            let mut messages = if offline {
                storage
                    .list_mail_messages(Some(&folder_id), limit)
                    .map_err(format_error)?
            } else {
                let fetched = graph
                    .list_mail_messages(&folder_id, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_mail_messages(&fetched);
                fetched
            };
            if unread {
                messages.retain(|m| !m.is_read);
            }
            messages.truncate(limit);

            Ok(json!({
                "folder": folder_id,
                "messages": messages,
            }))
        }

        "get_mail" => {
            let mail_id = args.required_str("mail_id")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let mail = if offline {
                storage
                    .get_mail_message(&mail_id)
                    .map_err(format_error)?
                    .ok_or_else(|| format!("mail message not found in cache: {mail_id}"))?
            } else {
                let fetched = graph.get_mail_message(&mail_id).await.map_err(format_error)?;
                let _ = storage.upsert_mail_messages(std::slice::from_ref(&fetched));
                fetched
            };
            Ok(json!({ "mail": mail }))
        }

        "send_mail" => {
            let to = args.required_str_array("to")?;
            let subject = args.required_str("subject")?;
            let body = args.required_text("body")?;
            let cc = args.optional_str_array("cc")?.unwrap_or_default();
            let bcc = args.optional_str_array("bcc")?.unwrap_or_default();
            let importance = args
                .optional_str("importance")?
                .map(|v| normalize_mail_importance(&v))
                .transpose()?;
            let attachments: Vec<PathBuf> = args
                .optional_str_array("attachments")?
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect();
            for path in &attachments {
                if !path.is_file() {
                    return Err(format!("attachment file does not exist: {}", path.display()));
                }
            }
            let save_to_sent_items = args.optional_bool("save_to_sent_items")?.unwrap_or(true);
            let body_html = parser::compile_outgoing_markdown(
                &body,
                Some(subject.clone()),
                vec![],
                vec![],
                vec![],
                None,
            )
            .body_html;

            let outgoing = OutgoingMail {
                subject: subject.clone(),
                body_html,
                to: to.clone(),
                cc,
                bcc,
                importance,
                attachments,
                save_to_sent_items,
            };
            graph.send_mail(&outgoing).await.map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "to": to,
                "subject": subject,
            }))
        }

        "reply_mail" => {
            let mail_id = args.required_str("mail_id")?;
            let body = args.required_text("body")?;
            let reply_all = args.optional_bool("reply_all")?.unwrap_or(false);
            let body_html =
                parser::compile_outgoing_markdown(&body, None, vec![], vec![], vec![], None)
                    .body_html;
            graph
                .reply_mail(&mail_id, &body_html, reply_all)
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "mail_id": mail_id,
                "reply_all": reply_all,
            }))
        }

        "forward_mail" => {
            let mail_id = args.required_str("mail_id")?;
            let to = args.required_str_array("to")?;
            let comment_html = match args.optional_str("comment")? {
                Some(c) => {
                    parser::compile_outgoing_markdown(&c, None, vec![], vec![], vec![], None)
                        .body_html
                }
                None => String::new(),
            };
            graph
                .forward_mail(&mail_id, &to, &comment_html)
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "mail_id": mail_id,
                "to": to,
            }))
        }

        "update_mail" => {
            let mail_id = args.required_str("mail_id")?;
            let is_read = args.optional_bool("is_read")?;
            let flagged = args.optional_bool("flagged")?;
            let move_to_folder = args.optional_str("move_to_folder")?;
            if is_read.is_none() && flagged.is_none() && move_to_folder.is_none() {
                return Err(
                    "at least one of is_read, flagged, or move_to_folder must be provided"
                        .to_string(),
                );
            }

            if let Some(read_val) = is_read {
                graph
                    .set_mail_read(&mail_id, read_val)
                    .await
                    .map_err(format_error)?;
            }
            if let Some(flag_val) = flagged {
                graph
                    .set_mail_flag(&mail_id, flag_val)
                    .await
                    .map_err(format_error)?;
            }
            if let Ok(Some(mut cached)) = storage.get_mail_message(&mail_id) {
                if let Some(read_val) = is_read {
                    cached.is_read = read_val;
                }
                if let Some(flag_val) = flagged {
                    cached.flagged = flag_val;
                }
                let _ = storage.upsert_mail_messages(&[cached]);
            }

            let moved_message = if let Some(dest) = move_to_folder.as_deref() {
                let dest_id = cli::resolve_mail_folder_id(storage, graph, dest, false)
                    .await
                    .map_err(format_error)?;
                let moved = graph
                    .move_mail_message(&mail_id, &dest_id)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_mail_messages(std::slice::from_ref(&moved));
                Some(moved)
            } else {
                None
            };

            Ok(json!({
                "ok": true,
                "mail_id": moved_message
                    .as_ref()
                    .map(|m| m.id.as_str())
                    .unwrap_or(mail_id.as_str()),
                "is_read": is_read,
                "flagged": flagged,
                "moved": moved_message,
            }))
        }

        "delete_mail" => {
            let mail_id = args.required_str("mail_id")?;
            args.required_true("confirm")?;
            graph
                .delete_mail_message(&mail_id)
                .await
                .map_err(format_error)?;
            let _ = storage.delete_mail_messages_by_ids(&[mail_id.clone()]);
            Ok(json!({
                "ok": true,
                "mail_id": mail_id,
            }))
        }

        "download_mail_attachment" => {
            let mail_id = args.required_str("mail_id")?;
            let attachment_index = args.optional_usize("attachment_index")?;
            let attachment = args.optional_str("attachment")?;
            let output_dir = args
                .optional_str("output_dir")?
                .map(PathBuf::from)
                .unwrap_or_else(|| config.resolved_download_dir());

            let mail = graph.get_mail_message(&mail_id).await.map_err(format_error)?;
            let _ = storage.upsert_mail_messages(std::slice::from_ref(&mail));
            let selected =
                cli::select_mail_attachment(&mail, attachment_index, attachment.as_deref())
                    .map_err(format_error)?;
            let downloaded = graph
                .download_mail_attachment(&mail_id, &selected.id, &output_dir)
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "attachment": {
                    "id": selected.id,
                    "name": selected.name,
                    "content_type": selected.content_type,
                    "size": selected.size,
                },
                "file": {
                    "path": downloaded.path,
                    "file_name": downloaded.file_name,
                    "bytes_written": downloaded.bytes_written,
                }
            }))
        }

        "search_mails" => {
            let query = args.required_str("query")?;
            let folder = args.optional_str("folder")?;
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(25);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let folder_id = match folder.as_deref() {
                Some(f) => Some(
                    cli::resolve_mail_folder_id(storage, graph, f, offline)
                        .await
                        .map_err(format_error)?,
                ),
                None => None,
            };

            if offline {
                let hits = storage
                    .search_mail_messages(&query, folder_id.as_deref(), limit)
                    .map_err(format_error)?;
                Ok(json!({ "query": query, "hits": hits }))
            } else {
                let mut messages = graph
                    .search_mail_remote(&query, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_mail_messages(&messages);
                if let Some(ref fid) = folder_id {
                    messages.retain(|m| m.folder_id.as_deref() == Some(fid.as_str()));
                }
                Ok(json!({ "query": query, "messages": messages }))
            }
        }

        // -------------------------------------------------------------------
        // Outlook Calendar tools
        // -------------------------------------------------------------------
        "list_calendar_events" => {
            let start = args.optional_str("start")?;
            let end = args.optional_str("end")?;
            let days = args
                .optional_i64("days")?
                .map(|v| bounds("days", usize::try_from(v).unwrap_or(0), 1, 365))
                .transpose()?
                .unwrap_or(7) as i64;
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(50);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let (start_iso, end_iso) =
                cli::default_calendar_window(start.as_deref(), end.as_deref(), days)
                    .map_err(format_error)?;

            let events = if offline {
                storage
                    .list_calendar_events(
                        start.as_deref().map(|_| start_iso.as_str()),
                        end.as_deref().map(|_| end_iso.as_str()),
                        limit,
                    )
                    .map_err(format_error)?
            } else {
                let fetched = graph
                    .list_calendar_view(&start_iso, &end_iso, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_calendar_events(&fetched);
                fetched
            };

            Ok(json!({
                "start": start_iso,
                "end": end_iso,
                "events": events,
            }))
        }

        "create_calendar_event" => {
            let subject = args.required_str("subject")?;
            let start_raw = args.required_str("start")?;
            let end_raw = args.required_str("end")?;
            let start_iso = cli::normalize_datetime_iso(&start_raw).map_err(format_error)?;
            let end_iso = cli::normalize_datetime_iso(&end_raw).map_err(format_error)?;
            let time_zone = args.optional_str("time_zone")?;
            let is_all_day = args.optional_bool("is_all_day")?.unwrap_or(false);
            let location = args.optional_str("location")?;
            let body_html = args.optional_str("body")?.map(|md| {
                parser::compile_outgoing_markdown(&md, None, vec![], vec![], vec![], None)
                    .body_html
            });
            let attendees = args.optional_str_array("attendees")?.unwrap_or_default();
            let is_online_meeting = args.optional_bool("is_online_meeting")?.unwrap_or(false);

            let outgoing = OutgoingEvent {
                subject,
                start_iso,
                end_iso,
                time_zone,
                is_all_day,
                location,
                body_html,
                attendees,
                is_online_meeting,
            };
            let created = graph
                .create_calendar_event(&outgoing)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_calendar_events(std::slice::from_ref(&created));

            Ok(json!({
                "ok": true,
                "event": created,
            }))
        }

        "respond_calendar_event" => {
            let event_id = args.required_str("event_id")?;
            let response_raw = args.required_str("response")?;
            let normalized =
                cli::normalize_rsvp_response(&response_raw).map_err(format_error)?;
            let comment = args.optional_str("comment")?;
            graph
                .respond_calendar_event(&event_id, normalized, comment.as_deref())
                .await
                .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "event_id": event_id,
                "response": normalized,
            }))
        }

        "delete_calendar_event" => {
            let event_id = args.required_str("event_id")?;
            args.required_true("confirm")?;
            graph
                .delete_calendar_event(&event_id)
                .await
                .map_err(format_error)?;
            let _ = storage.delete_calendar_event(&event_id);

            Ok(json!({
                "ok": true,
                "event_id": event_id,
            }))
        }

        // -------------------------------------------------------------------
        // Outlook Contacts & Directory tools
        // -------------------------------------------------------------------
        "search_contacts" => {
            let query = args.optional_str("query")?;
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(25);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let contacts = if offline {
                storage
                    .list_contacts(query.as_deref(), limit)
                    .map_err(format_error)?
            } else if let Some(ref q) = query {
                let fetched = graph
                    .search_contacts_and_directory(q, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_contacts(&fetched);
                fetched
            } else {
                let fetched = graph.list_contacts(limit).await.map_err(format_error)?;
                let _ = storage.upsert_contacts(&fetched);
                fetched
            };

            Ok(json!({ "contacts": contacts }))
        }

        // -------------------------------------------------------------------
        // Microsoft To Do tools
        // -------------------------------------------------------------------
        "list_todo_tasks" => {
            let list_query = args.optional_str("list")?;
            let include_completed = args.optional_bool("include_completed")?.unwrap_or(false);
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(50);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let resolved_list_id =
                match resolve_optional_todo_list_id(storage, graph, list_query.as_deref(), offline)
                    .await?
                {
                    Some(id) => id,
                    None => {
                        return Ok(json!({
                            "list_id": Value::Null,
                            "tasks": [],
                        }));
                    }
                };

            let mut tasks = if offline {
                storage
                    .list_todo_tasks(&resolved_list_id, include_completed, limit)
                    .map_err(format_error)?
            } else {
                let fetched = graph
                    .list_todo_tasks(&resolved_list_id, include_completed, limit)
                    .await
                    .map_err(format_error)?;
                let _ = storage.upsert_todo_tasks(&fetched);
                fetched
            };
            tasks.truncate(limit);

            Ok(json!({
                "list_id": resolved_list_id,
                "tasks": tasks,
            }))
        }

        "create_todo_task" => {
            let list_query = args.optional_str("list")?;
            let title = args.required_str("title")?;
            let body_plain = args.optional_str("body")?;
            let due_date_iso = args
                .optional_str("due_date_iso")?
                .map(|raw| cli::normalize_todo_due_iso(&raw))
                .transpose()
                .map_err(format_error)?;
            let importance = args
                .optional_str("importance")?
                .map(|raw| cli::normalize_todo_importance(&raw))
                .transpose()
                .map_err(format_error)?
                .map(str::to_string);

            let resolved_list_id =
                resolve_optional_todo_list_id(storage, graph, list_query.as_deref(), false)
                    .await?
                    .ok_or_else(|| "no To Do list found".to_string())?;

            let outgoing = OutgoingTodoTask {
                title,
                body_plain,
                due_date_iso,
                importance,
            };
            let created = graph
                .create_todo_task(&resolved_list_id, &outgoing)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_todo_tasks(std::slice::from_ref(&created));

            Ok(json!({
                "ok": true,
                "task": created,
            }))
        }

        "complete_todo_task" => {
            let list_query = args.optional_str("list")?;
            let task_id = args.required_str("task_id")?;
            let completed = args.optional_bool("completed")?.unwrap_or(true);

            let resolved_list_id =
                resolve_optional_todo_list_id(storage, graph, list_query.as_deref(), false)
                    .await?
                    .ok_or_else(|| "no To Do list found".to_string())?;
            let updated = graph
                .set_todo_task_completed(&resolved_list_id, &task_id, completed)
                .await
                .map_err(format_error)?;
            let _ = storage.upsert_todo_tasks(std::slice::from_ref(&updated));

            Ok(json!({
                "ok": true,
                "task": updated,
            }))
        }

        "delete_todo_task" => {
            let list_query = args.optional_str("list")?;
            let task_id = args.required_str("task_id")?;
            args.required_true("confirm")?;

            let resolved_list_id =
                resolve_optional_todo_list_id(storage, graph, list_query.as_deref(), false)
                    .await?
                    .ok_or_else(|| "no To Do list found".to_string())?;
            graph
                .delete_todo_task(&resolved_list_id, &task_id)
                .await
                .map_err(format_error)?;
            let _ = storage.delete_todo_task(&task_id);

            Ok(json!({
                "ok": true,
                "list_id": resolved_list_id,
                "task_id": task_id,
            }))
        }

        other => Err(format!("unknown tool: {other}")),
    }
}

fn normalize_mail_importance(raw: &str) -> Result<String, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low".to_string()),
        "normal" => Ok("normal".to_string()),
        "high" => Ok("high".to_string()),
        other => Err(format!(
            "invalid mail importance '{other}' (expected low, normal, or high)"
        )),
    }
}

async fn resolve_optional_todo_list_id(
    storage: &Storage,
    graph: &GraphClient,
    list_query: Option<&str>,
    offline: bool,
) -> Result<Option<String>, String> {
    if let Some(q) = list_query.map(str::trim).filter(|s| !s.is_empty()) {
        let id = cli::resolve_todo_list_id(storage, graph, q, offline)
            .await
            .map_err(format_error)?;
        return Ok(Some(id));
    }

    let lists = if offline {
        storage.list_todo_lists().map_err(format_error)?
    } else {
        let cached = storage.list_todo_lists().map_err(format_error)?;
        if cached.is_empty() {
            let fetched = graph.list_todo_lists().await.map_err(format_error)?;
            let _ = storage.upsert_todo_lists(&fetched);
            fetched
        } else {
            cached
        }
    };

    Ok(lists
        .iter()
        .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
        .or_else(|| {
            lists.iter().find(|l| {
                l.display_name.eq_ignore_ascii_case("tasks") || l.display_name == "할 일"
            })
        })
        .or_else(|| lists.first())
        .map(|l| l.id.clone()))
}

// ---------------------------------------------------------------------------
// JSON-RPC framing
// ---------------------------------------------------------------------------

fn parse_message(line: &str) -> Result<Value, Value> {
    let trimmed = line.trim();
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            return Err(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {error}"),
            ));
        }
    };

    let Some(object) = value.as_object() else {
        return Err(error_response(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected a JSON object",
        ));
    };

    let id = object.get("id").cloned().unwrap_or(Value::Null);

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }

    if !object.get("method").is_some_and(Value::is_string) {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: method must be a string",
        ));
    }

    Ok(value)
}

fn encode_message(value: &Value) -> String {
    let body = serde_json::to_string(value).unwrap_or_else(|_| {
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"serialization failed\"}}"
            .to_string()
    });
    format!("{body}\n")
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

fn initialize_result(message: &Value) -> Value {
    let requested = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let protocol_version = requested
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Microsoft 365 (Teams + Outlook Mail, Calendar, Contacts, To Do) tools via Microsoft Graph v1.0. Teams targets accept conv_key (chat:... or chan:team:channel), \"Team/Channel\", or a unique conversation title substring. Destructive operations require confirm: true."
    })
}

// ---------------------------------------------------------------------------
// Tool catalog (36 tools)
// ---------------------------------------------------------------------------

fn tool_catalog() -> Vec<Value> {
    vec![
        json!({
            "name": "list_chats",
            "description": "List 1:1, group, and meeting chats. Set refresh=true to sync from Microsoft Graph first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "description": "Sync chats from Microsoft Graph before returning." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_chat",
            "description": "Create a new 1:1 (1 member) or group (2+ members) Microsoft Teams chat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "members": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Peer Entra user IDs or email addresses."
                    },
                    "topic": { "type": "string", "description": "Optional topic title for group chats." }
                },
                "required": ["members"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_teams_channels",
            "description": "List joined Microsoft Teams and their channels. Set refresh=true to sync from Microsoft Graph first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "description": "Sync teams and channels from Microsoft Graph before returning." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_messages",
            "description": "Read recent messages from a chat or team channel, oldest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "conv_key, \"Team/Channel\", or unique conversation title." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 },
                    "offline": { "type": "boolean", "default": false, "description": "Read from local SQLite cache without hitting Graph." }
                },
                "required": ["target"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_thread",
            "description": "Fetch a team channel root post and all replies in its thread.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Channel conv_key, \"Team/Channel\", or unique channel title." },
                    "root_msg_id": { "type": "string", "minLength": 1 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 }
                },
                "required": ["target", "root_msg_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_message",
            "description": "Send a Markdown message to a chat or team channel. Irreversible side effect.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "conv_key, \"Team/Channel\", or unique conversation title." },
                    "markdown": { "type": "string", "minLength": 1 },
                    "subject": { "type": "string", "description": "Optional subject line for team channel root posts." },
                    "mentions": {
                        "type": "array",
                        "description": "Mentions as \"user_id:DisplayName\" strings or {user_id, display_name} objects.",
                        "items": {
                            "oneOf": [
                                { "type": "string" },
                                {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "integer", "minimum": 0 },
                                        "user_id": { "type": "string", "minLength": 1 },
                                        "display_name": { "type": "string", "minLength": 1 }
                                    },
                                    "required": ["user_id", "display_name"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    },
                    "important": { "type": "boolean", "default": false }
                },
                "required": ["target", "markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "reply_message",
            "description": "Reply to a team channel thread root post or quote-reply a message in a 1:1/group chat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "conv_key, \"Team/Channel\", or unique conversation title." },
                    "msg_id": { "type": "string", "minLength": 1 },
                    "markdown": { "type": "string", "minLength": 1 }
                },
                "required": ["target", "msg_id", "markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_file",
            "description": "Upload a local file (from the host running teams) to OneDrive/SharePoint and post a reference attachment (or inline image when inline=true).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "conv_key, \"Team/Channel\", or unique conversation title." },
                    "path": { "type": "string", "minLength": 1, "description": "Local file path on the host running teams." },
                    "caption": { "type": "string" },
                    "thread_root_id": { "type": "string", "description": "Optional channel thread root post ID." },
                    "inline": { "type": "boolean", "default": false }
                },
                "required": ["target", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "download_attachment",
            "description": "Download an attachment from a message to disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string" },
                    "msg_id": { "type": "string", "minLength": 1 },
                    "index": { "type": "integer", "minimum": 0, "default": 0 },
                    "out_dir": { "type": "string" },
                    "thread_root_id": { "type": "string" }
                },
                "required": ["target", "msg_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "react_message",
            "description": "Set or unset a reaction (like, heart, laugh, surprised, sad, angry) on a message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string" },
                    "msg_id": { "type": "string", "minLength": 1 },
                    "reaction": { "type": "string", "minLength": 1, "description": "like, heart, laugh, surprised, sad, angry, or unicode emoji." },
                    "enable": { "type": "boolean", "default": true },
                    "thread_root_id": { "type": "string" }
                },
                "required": ["target", "msg_id", "reaction"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "edit_message",
            "description": "Edit an existing message sent by the current user.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string" },
                    "msg_id": { "type": "string", "minLength": 1 },
                    "markdown": { "type": "string", "minLength": 1 },
                    "thread_root_id": { "type": "string" }
                },
                "required": ["target", "msg_id", "markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_message",
            "description": "Soft-delete a message sent by the current user. Requires confirm=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string" },
                    "msg_id": { "type": "string", "minLength": 1 },
                    "confirm": { "const": true, "description": "Must be literal boolean true." },
                    "thread_root_id": { "type": "string" }
                },
                "required": ["target", "msg_id", "confirm"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_messages",
            "description": "Search cached messages using local SQLite FTS5.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "target": { "type": "string", "description": "Optional conversation target to restrict search." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50 }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_members",
            "description": "List members of a chat or team channel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string" }
                },
                "required": ["target"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "mark_read",
            "description": "Mark a chat or conversation as read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string" }
                },
                "required": ["target"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_presence",
            "description": "Get presence for the current user or a list of user IDs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "user_ids": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Optional Entra user IDs. Omit to fetch current user's presence."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "set_presence",
            "description": "Set the current user's preferred Teams presence (Available, Busy, DoNotDisturb, BeRightBack, Away, Reset).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "availability": {
                        "type": "string",
                        "enum": ["Available", "Busy", "DoNotDisturb", "BeRightBack", "Away", "Reset"]
                    },
                    "activity": { "type": "string" },
                    "expiration_mins": { "type": "integer", "minimum": 1, "maximum": 1440 }
                },
                "required": ["availability"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_mail_folders",
            "description": "List Outlook mail folders (Inbox, Sent Items, Drafts, custom folders, etc.).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "offline": { "type": "boolean", "description": "Read from local SQLite cache only." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_mails",
            "description": "List Outlook mail messages in a folder (well-known name like 'inbox'/'sentitems', folder display name, or folder ID).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "folder": { "type": "string", "description": "Mail folder well-known name, display name, or folder ID (default: 'inbox')." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "unread": { "type": "boolean", "description": "Filter to unread messages only." },
                    "offline": { "type": "boolean", "description": "Read from local SQLite cache only." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_mail",
            "description": "Get full details, body, and attachment metadata for an Outlook mail message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mail_id": { "type": "string", "description": "Outlook mail message ID." },
                    "offline": { "type": "boolean", "description": "Read from local SQLite cache only." }
                },
                "required": ["mail_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_mail",
            "description": "Send a new Outlook email (Markdown body compiled to HTML) with optional Cc/Bcc recipients and local file attachments.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                    "subject": { "type": "string" },
                    "body": { "type": "string", "description": "Email body in Markdown." },
                    "cc": { "type": "array", "items": { "type": "string" } },
                    "bcc": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"] },
                    "attachments": { "type": "array", "items": { "type": "string" }, "description": "Optional local file paths to attach." },
                    "save_to_sent_items": { "type": "boolean", "description": "Defaults to true." }
                },
                "required": ["to", "subject", "body"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "reply_mail",
            "description": "Reply (or reply-all) to an Outlook mail message with a Markdown body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mail_id": { "type": "string" },
                    "body": { "type": "string", "description": "Reply body in Markdown." },
                    "reply_all": { "type": "boolean" }
                },
                "required": ["mail_id", "body"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "forward_mail",
            "description": "Forward an Outlook mail message to one or more recipient email addresses.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mail_id": { "type": "string" },
                    "to": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                    "comment": { "type": "string", "description": "Optional Markdown comment added above forwarded message." }
                },
                "required": ["mail_id", "to"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_mail",
            "description": "Update an Outlook mail message's read/flagged state and/or move it to another folder.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mail_id": { "type": "string" },
                    "is_read": { "type": "boolean" },
                    "flagged": { "type": "boolean" },
                    "move_to_folder": { "type": "string", "description": "Destination folder name, well-known name, or folder ID." }
                },
                "required": ["mail_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_mail",
            "description": "Delete an Outlook mail message permanently/to trash. Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mail_id": { "type": "string" },
                    "confirm": { "type": "boolean", "const": true }
                },
                "required": ["mail_id", "confirm"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "download_mail_attachment",
            "description": "Download an attachment from an Outlook mail message to disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mail_id": { "type": "string" },
                    "attachment_index": { "type": "integer", "minimum": 0 },
                    "attachment": { "type": "string", "description": "Attachment ID or filename." },
                    "output_dir": { "type": "string" }
                },
                "required": ["mail_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_mails",
            "description": "Search Outlook mail messages online via Graph $search or offline in the local FTS5 cache.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "folder": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "offline": { "type": "boolean" }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_calendar_events",
            "description": "List Outlook calendar events within a time window (defaults to now through +7 days).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start": { "type": "string", "description": "Start date/datetime (RFC3339 or YYYY-MM-DD)." },
                    "end": { "type": "string", "description": "End date/datetime (RFC3339 or YYYY-MM-DD)." },
                    "days": { "type": "integer", "minimum": 1, "maximum": 365 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "offline": { "type": "boolean" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_calendar_event",
            "description": "Create an Outlook calendar event (optionally with attendees and Teams online meeting link).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string" },
                    "start": { "type": "string", "description": "Start time (RFC3339, YYYY-MM-DD HH:MM, or YYYY-MM-DD)." },
                    "end": { "type": "string", "description": "End time (RFC3339, YYYY-MM-DD HH:MM, or YYYY-MM-DD)." },
                    "time_zone": { "type": "string" },
                    "is_all_day": { "type": "boolean" },
                    "location": { "type": "string" },
                    "body": { "type": "string", "description": "Optional event description in Markdown." },
                    "attendees": { "type": "array", "items": { "type": "string" } },
                    "is_online_meeting": { "type": "boolean" }
                },
                "required": ["subject", "start", "end"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "respond_calendar_event",
            "description": "Accept, tentatively accept, or decline an Outlook calendar meeting invitation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event_id": { "type": "string" },
                    "response": { "type": "string", "enum": ["accept", "tentative", "decline"] },
                    "comment": { "type": "string" }
                },
                "required": ["event_id", "response"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_calendar_event",
            "description": "Delete/cancel an Outlook calendar event. Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event_id": { "type": "string" },
                    "confirm": { "type": "boolean", "const": true }
                },
                "required": ["event_id", "confirm"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_contacts",
            "description": "List or search Outlook personal contacts and Entra ID organization directory users.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "offline": { "type": "boolean" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_todo_tasks",
            "description": "List Microsoft To Do tasks in a task list (defaults to the default Tasks list).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "list": { "type": "string", "description": "Task list name or ID (default: default Tasks list)." },
                    "include_completed": { "type": "boolean" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "offline": { "type": "boolean" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_todo_task",
            "description": "Create a Microsoft To Do task in a task list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "list": { "type": "string", "description": "Task list name or ID (default: default Tasks list)." },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "due_date_iso": { "type": "string", "description": "Due date (YYYY-MM-DD or RFC3339)." },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"] }
                },
                "required": ["title"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "complete_todo_task",
            "description": "Mark a Microsoft To Do task as completed or reopen it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "list": { "type": "string", "description": "Task list name or ID (default: default Tasks list)." },
                    "task_id": { "type": "string" },
                    "completed": { "type": "boolean", "description": "Defaults to true." }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_todo_task",
            "description": "Delete a Microsoft To Do task. Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "list": { "type": "string", "description": "Task list name or ID (default: default Tasks list)." },
                    "task_id": { "type": "string" },
                    "confirm": { "type": "boolean", "const": true }
                },
                "required": ["task_id", "confirm"],
                "additionalProperties": false
            }
        }),
    ]
}

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            })
        }
        Err(error) => json!({
            "content": [{ "type": "text", "text": error }],
            "isError": true
        }),
    }
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    let args = Args::new(arguments);

    match name {
        "list_chats" => {
            args.optional_bool("refresh")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "create_chat" => {
            args.required_str_array("members")?;
            args.optional_str("topic")?;
        }
        "list_teams_channels" => {
            args.optional_bool("refresh")?;
        }
        "get_messages" => {
            args.required_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_bool("offline")?;
        }
        "get_thread" => {
            args.required_str("target")?;
            args.required_str("root_msg_id")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "send_message" => {
            args.required_str("target")?;
            args.required_text("markdown")?;
            args.optional_str("subject")?;
            args.optional_mentions("mentions")?;
            args.optional_bool("important")?;
        }
        "reply_message" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_text("markdown")?;
        }
        "send_file" => {
            args.required_str("target")?;
            args.required_str("path")?;
            args.optional_str("caption")?;
            args.optional_str("thread_root_id")?;
            args.optional_bool("inline")?;
        }
        "download_attachment" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.optional_usize("index")?;
            args.optional_str("out_dir")?;
            args.optional_str("thread_root_id")?;
        }
        "react_message" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            let reaction = args.required_str("reaction")?;
            cli::normalize_reaction(&reaction).map_err(format_error)?;
            args.optional_bool("enable")?;
            args.optional_str("thread_root_id")?;
        }
        "edit_message" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_text("markdown")?;
            args.optional_str("thread_root_id")?;
        }
        "delete_message" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_true("confirm")?;
            args.optional_str("thread_root_id")?;
        }
        "search_messages" => {
            args.required_str("query")?;
            args.optional_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "list_members" | "mark_read" => {
            args.required_str("target")?;
        }
        "get_presence" => {
            args.optional_str_array("user_ids")?;
        }
        "set_presence" => {
            let avail = args.required_str("availability")?;
            cli::normalize_presence_availability(&avail).map_err(format_error)?;
            args.optional_str("activity")?;
            if let Some(mins) = args.optional_u32("expiration_mins")? {
                bounds("expiration_mins", mins as usize, 1, 1440)?;
            }
        }
        "list_mail_folders" => {
            args.optional_bool("offline")?;
        }
        "list_mails" => {
            args.optional_str("folder")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("unread")?;
            args.optional_bool("offline")?;
        }
        "get_mail" => {
            args.required_str("mail_id")?;
            args.optional_bool("offline")?;
        }
        "send_mail" => {
            args.required_str_array("to")?;
            args.required_str("subject")?;
            args.required_text("body")?;
            args.optional_str_array("cc")?;
            args.optional_str_array("bcc")?;
            if let Some(imp) = args.optional_str("importance")? {
                normalize_mail_importance(&imp)?;
            }
            args.optional_str_array("attachments")?;
            args.optional_bool("save_to_sent_items")?;
        }
        "reply_mail" => {
            args.required_str("mail_id")?;
            args.required_text("body")?;
            args.optional_bool("reply_all")?;
        }
        "forward_mail" => {
            args.required_str("mail_id")?;
            args.required_str_array("to")?;
            args.optional_str("comment")?;
        }
        "update_mail" => {
            args.required_str("mail_id")?;
            let is_read = args.optional_bool("is_read")?;
            let flagged = args.optional_bool("flagged")?;
            let move_to = args.optional_str("move_to_folder")?;
            if is_read.is_none() && flagged.is_none() && move_to.is_none() {
                return Err(
                    "at least one of is_read, flagged, or move_to_folder must be provided"
                        .to_string(),
                );
            }
        }
        "delete_mail" => {
            args.required_str("mail_id")?;
            args.required_true("confirm")?;
        }
        "download_mail_attachment" => {
            args.required_str("mail_id")?;
            args.optional_usize("attachment_index")?;
            args.optional_str("attachment")?;
            args.optional_str("output_dir")?;
        }
        "search_mails" => {
            args.required_str("query")?;
            args.optional_str("folder")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
        }
        "list_calendar_events" => {
            let start = args.optional_str("start")?;
            let end = args.optional_str("end")?;
            let days = if let Some(d) = args.optional_i64("days")? {
                bounds("days", usize::try_from(d).unwrap_or(0), 1, 365)?;
                d
            } else {
                7
            };
            cli::default_calendar_window(start.as_deref(), end.as_deref(), days)
                .map_err(format_error)?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
        }
        "create_calendar_event" => {
            args.required_str("subject")?;
            let start = args.required_str("start")?;
            let end = args.required_str("end")?;
            cli::normalize_datetime_iso(&start).map_err(format_error)?;
            cli::normalize_datetime_iso(&end).map_err(format_error)?;
            args.optional_str("time_zone")?;
            args.optional_bool("is_all_day")?;
            args.optional_str("location")?;
            args.optional_str("body")?;
            args.optional_str_array("attendees")?;
            args.optional_bool("is_online_meeting")?;
        }
        "respond_calendar_event" => {
            args.required_str("event_id")?;
            let resp = args.required_str("response")?;
            cli::normalize_rsvp_response(&resp).map_err(format_error)?;
            args.optional_str("comment")?;
        }
        "delete_calendar_event" => {
            args.required_str("event_id")?;
            args.required_true("confirm")?;
        }
        "search_contacts" => {
            args.optional_str("query")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
        }
        "list_todo_tasks" => {
            args.optional_str("list")?;
            args.optional_bool("include_completed")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
        }
        "create_todo_task" => {
            args.optional_str("list")?;
            args.required_str("title")?;
            args.optional_str("body")?;
            if let Some(due) = args.optional_str("due_date_iso")? {
                cli::normalize_todo_due_iso(&due).map_err(format_error)?;
            }
            if let Some(imp) = args.optional_str("importance")? {
                cli::normalize_todo_importance(&imp).map_err(format_error)?;
            }
        }
        "complete_todo_task" => {
            args.optional_str("list")?;
            args.required_str("task_id")?;
            args.optional_bool("completed")?;
        }
        "delete_todo_task" => {
            args.optional_str("list")?;
            args.required_str("task_id")?;
            args.required_true("confirm")?;
        }
        other => return Err(format!("unknown tool: {other}")),
    }

    Ok(())
}

fn bounds(name: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between {min} and {max}"))
    }
}

struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    fn new(value: &'a Value) -> Self {
        Self { value }
    }

    fn object(&self) -> Result<&'a serde_json::Map<String, Value>, String> {
        self.value
            .as_object()
            .ok_or_else(|| "arguments must be a JSON object".to_string())
    }

    fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn required_str(&self, key: &str) -> Result<String, String> {
        self.optional_str(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))
    }

    fn required_text(&self, key: &str) -> Result<String, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn required_true(&self, key: &str) -> Result<(), String> {
        match self.object()?.get(key) {
            Some(Value::Bool(true)) => Ok(()),
            Some(Value::Bool(false)) => Err(format!("{key} must be true")),
            None | Some(Value::Null) => {
                Err(format!("missing required argument: {key} (must be true)"))
            }
            Some(_) => Err(format!("{key} must be a boolean true")),
        }
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("{key} must be a boolean")),
        }
    }

    fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
                .map(Some)
                .ok_or_else(|| format!("{key} must be an integer")),
        }
    }

    fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.optional_i64(key)? {
            None => Ok(None),
            Some(v) => usize::try_from(v)
                .map(Some)
                .map_err(|_| format!("{key} must be a non-negative integer")),
        }
    }

    fn optional_u32(&self, key: &str) -> Result<Option<u32>, String> {
        match self.optional_i64(key)? {
            None => Ok(None),
            Some(v) => u32::try_from(v)
                .map(Some)
                .map_err(|_| format!("{key} must be a non-negative 32-bit integer")),
        }
    }

    fn optional_str_array(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let s = item
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| format!("{key}[{i}] must be a non-empty string"))?;
                    out.push(s.to_string());
                }
                Ok(Some(out))
            }
            Some(_) => Err(format!("{key} must be an array of strings")),
        }
    }

    fn required_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        let items = self
            .optional_str_array(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))?;
        if items.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        Ok(items)
    }

    fn optional_mentions(&self, key: &str) -> Result<Vec<MentionInfo>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    parse_mention_value(item, idx as i32)
                        .map_err(|e| format!("mentions[{idx}]: {e}"))
                })
                .collect(),
            Some(_) => Err(format!("{key} must be an array")),
        }
    }
}

fn parse_mention_value(value: &Value, default_id: i32) -> Result<MentionInfo, String> {
    if let Some(spec) = value.as_str() {
        return cli::parse_mention_spec(spec, default_id).map_err(format_error);
    }

    let obj = value
        .as_object()
        .ok_or_else(|| "mention must be a string or object".to_string())?;
    let user_id = obj
        .get("user_id")
        .or_else(|| obj.get("userId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mention.user_id must be a non-empty string".to_string())?;
    let display_name = obj
        .get("display_name")
        .or_else(|| obj.get("displayName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mention.display_name must be a non-empty string".to_string())?;
    let id = obj
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(default_id);

    Ok(MentionInfo {
        id,
        user_id: user_id.to_string(),
        display_name: display_name.to_string(),
    })
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;

    fn test_ctx() -> (AppConfig, GraphClient, Storage) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        (cfg, graph, storage)
    }

    #[tokio::test]
    async fn handshake_and_notifications_and_ping() {
        let (cfg, graph, storage) = test_ctx();

        // Supported version echoed back
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "teams");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);

        // Unknown protocol version falls back to 2025-06-18
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");

        // Notification produces no response
        assert!(handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await
        .is_none());

        // Ping returns empty object
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"], json!({}));

        // Malformed JSON -> -32700
        let err = handle_rpc_line(&cfg, &graph, &storage, "{not json")
            .await
            .unwrap();
        assert_eq!(err["error"]["code"], PARSE_ERROR);

        // Unknown method -> -32601
        let err = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":4,"method":"unknown/op"}"#,
        )
        .await
        .unwrap();
        assert_eq!(err["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_list_exposes_all_36_tools() {
        let (cfg, graph, storage) = test_ctx();
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/list"}"#,
        )
        .await
        .unwrap();

        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "list_chats",
                "create_chat",
                "list_teams_channels",
                "get_messages",
                "get_thread",
                "send_message",
                "reply_message",
                "send_file",
                "download_attachment",
                "react_message",
                "edit_message",
                "delete_message",
                "search_messages",
                "list_members",
                "mark_read",
                "get_presence",
                "set_presence",
                "list_mail_folders",
                "list_mails",
                "get_mail",
                "send_mail",
                "reply_mail",
                "forward_mail",
                "update_mail",
                "delete_mail",
                "download_mail_attachment",
                "search_mails",
                "list_calendar_events",
                "create_calendar_event",
                "respond_calendar_event",
                "delete_calendar_event",
                "search_contacts",
                "list_todo_tasks",
                "create_todo_task",
                "complete_todo_task",
                "delete_todo_task",
            ]
        );

        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }

    #[tokio::test]
    async fn delete_message_rejects_missing_or_non_literal_true_confirm() {
        let (cfg, graph, storage) = test_ctx();

        for bad_args in [
            json!({ "target": "chat:1", "msg_id": "m1" }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": false }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": "true" }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": 1 }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": null }),
        ] {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": "delete_message",
                    "arguments": bad_args,
                }
            })
            .to_string();
            let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
            assert_eq!(resp["result"]["isError"], true, "{resp}");
            let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
            assert!(msg.contains("confirm"), "{msg}");
        }

        // Literal true passes validation
        validate_tool_args(
            "delete_message",
            &json!({ "target": "chat:1", "msg_id": "m1", "confirm": true }),
        )
        .unwrap();
    }

    #[test]
    fn argument_validation_covers_tools() {
        assert!(validate_tool_args(
            "send_message",
            &json!({
                "target": "개발팀/일반",
                "markdown": "Hello @Alice",
                "mentions": ["u1:Alice", { "user_id": "u2", "display_name": "Bob" }]
            })
        )
        .is_ok());

        assert!(validate_tool_args(
            "send_message",
            &json!({ "target": "chat:1", "markdown": "   " })
        )
        .is_err());

        assert!(validate_tool_args("create_chat", &json!({ "members": [] })).is_err());
        assert!(
            validate_tool_args("create_chat", &json!({ "members": ["a@b.com"] })).is_ok()
        );

        assert!(validate_tool_args(
            "react_message",
            &json!({ "target": "chat:1", "msg_id": "m1", "reaction": "like" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "react_message",
            &json!({ "target": "chat:1", "msg_id": "m1", "reaction": "invalid" })
        )
        .is_err());

        assert!(validate_tool_args(
            "set_presence",
            &json!({ "availability": "Busy", "expiration_mins": 30 })
        )
        .is_ok());
        assert!(validate_tool_args(
            "set_presence",
            &json!({ "availability": "Busy", "expiration_mins": 2000 })
        )
        .is_err());
        assert!(validate_tool_args(
            "set_presence",
            &json!({ "availability": "Offline" })
        )
        .is_err());

        // Outlook Mail / Calendar / Contacts / To Do validation
        assert!(validate_tool_args(
            "send_mail",
            &json!({ "to": ["a@b.com"], "subject": "Hi", "body": "Body" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "send_mail",
            &json!({ "to": [], "subject": "Hi", "body": "Body" })
        )
        .is_err());
        assert!(validate_tool_args(
            "send_mail",
            &json!({ "to": ["a@b.com"], "subject": "Hi", "body": "Body", "importance": "urgent" })
        )
        .is_err());
        assert!(validate_tool_args("update_mail", &json!({ "mail_id": "m1" })).is_err());
        assert!(
            validate_tool_args("update_mail", &json!({ "mail_id": "m1", "is_read": true })).is_ok()
        );
        assert!(validate_tool_args(
            "create_calendar_event",
            &json!({ "subject": "Sync", "start": "2026-11-16 09:00", "end": "2026-11-16 10:00" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "create_calendar_event",
            &json!({ "subject": "Sync", "start": "not-a-date", "end": "2026-11-16 10:00" })
        )
        .is_err());
        assert!(validate_tool_args(
            "respond_calendar_event",
            &json!({ "event_id": "ev1", "response": "accept" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "respond_calendar_event",
            &json!({ "event_id": "ev1", "response": "maybe_later" })
        )
        .is_err());
        assert!(validate_tool_args(
            "create_todo_task",
            &json!({ "title": "Ship release", "due_date_iso": "2026-11-20", "importance": "high" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "create_todo_task",
            &json!({ "title": "Ship release", "importance": "critical" })
        )
        .is_err());
    }

    #[tokio::test]
    async fn outlook_destructive_tools_require_confirm_true() {
        let (cfg, graph, storage) = test_ctx();

        for (tool, id_key, id_val) in [
            ("delete_mail", "mail_id", "m-1"),
            ("delete_calendar_event", "event_id", "ev-1"),
            ("delete_todo_task", "task_id", "t-1"),
        ] {
            for bad_confirm in [
                json!({}),
                json!({ "confirm": false }),
                json!({ "confirm": "true" }),
                json!({ "confirm": 1 }),
            ] {
                let mut args = bad_confirm;
                args.as_object_mut()
                    .unwrap()
                    .insert(id_key.to_string(), json!(id_val));
                let line = json!({
                    "jsonrpc": "2.0",
                    "id": 100,
                    "method": "tools/call",
                    "params": {
                        "name": tool,
                        "arguments": args,
                    }
                })
                .to_string();
                let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
                assert_eq!(resp["result"]["isError"], true, "{tool}: {resp}");
                let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
                assert!(msg.contains("confirm"), "{tool}: {msg}");
            }

            let mut ok_args = json!({ "confirm": true });
            ok_args
                .as_object_mut()
                .unwrap()
                .insert(id_key.to_string(), json!(id_val));
            assert!(validate_tool_args(tool, &ok_args).is_ok(), "{tool}");
        }
    }

    #[tokio::test]
    async fn offline_outlook_tools_return_valid_empty_payloads_on_fresh_cache() {
        let (cfg, graph, storage) = test_ctx();

        for (tool, args) in [
            ("list_mail_folders", json!({ "offline": true })),
            ("list_mails", json!({ "folder": "inbox", "offline": true })),
            ("list_calendar_events", json!({ "days": 7, "offline": true })),
            ("search_contacts", json!({ "offline": true })),
            ("list_todo_tasks", json!({ "offline": true })),
        ] {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 101,
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": args,
                }
            })
            .to_string();
            let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
            assert_eq!(resp["result"]["isError"], false, "{tool}: {resp}");
        }
    }

    #[tokio::test]
    async fn offline_get_messages_and_search_messages_work_against_sqlite() {
        use crate::models::{ConversationKind, MessageItem};

        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_conversations(&[ConversationSummary {
                conv_key: "chat:19:c1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "19:c1".to_string(),
                },
                kind: ConversationKind::Group,
                team_id: None,
                team_name: None,
                title: "개발 논의방".to_string(),
                topic: Some("개발 논의방".to_string()),
                unread_count: 0,
                last_read_at: None,
                last_msg_id: Some("msg-1".to_string()),
                last_msg_author: Some("김철수".to_string()),
                last_msg_preview: Some("릴리스 태그 확인".to_string()),
                last_updated_at: Some("2026-11-15T06:00:00Z".to_string()),
                pinned: false,
                peer_user_id: None,
                web_url: None,
            }])
            .unwrap();
        storage
            .upsert_messages(&[MessageItem {
                msg_key: "chat:19:c1|msg-1".to_string(),
                conv_key: "chat:19:c1".to_string(),
                msg_id: "msg-1".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u1".to_string()),
                author_name: "김철수".to_string(),
                is_me: false,
                created_at: "2026-11-15T06:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>릴리스 태그 확인 부탁합니다</p>".to_string(),
                body_plain: "릴리스 태그 확인 부탁합니다".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            }])
            .unwrap();

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "tools/call",
                "params": {
                    "name": "get_messages",
                    "arguments": { "target": "개발 논의방", "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["messages"][0]["msg_id"], "msg-1");

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": {
                    "name": "search_messages",
                    "arguments": { "query": "릴리스", "target": "개발 논의방" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["hits"].as_array().unwrap().len(), 1);
    }
}
