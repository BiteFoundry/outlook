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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::{
    auth::diagnose_access_token_scopes,
    cli,
    config::AppConfig,
    graph::{calendar::CalendarEditTime, GraphClient},
    models::{OutgoingEvent, OutgoingMail, OutgoingTodoTask},
    resolver,
    storage::Storage,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "outlook";
const MAX_RPC_LINE_BYTES: usize = 8 * 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

enum BoundedLine {
    Eof,
    Line(String),
    TooLong,
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut buf = Vec::new();
    let mut oversized = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if !oversized {
                if buf.len() + pos > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(&available[..pos]);
                }
            }
            reader.consume(pos + 1);
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        } else {
            if !oversized {
                if buf.len() + available.len() > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(available);
                }
            }
            let len = available.len();
            reader.consume(len);
        }
    }
}

async fn with_storage<T, F>(storage: &Storage, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
{
    let storage = storage.clone();
    tokio::task::spawn_blocking(move || f(&storage))
        .await
        .map_err(|err| anyhow::anyhow!("storage task panicked: {err}"))?
}

async fn map_graph_result<T>(graph: &GraphClient, res: Result<T>) -> Result<T, String> {
    match res {
        Ok(val) => Ok(val),
        Err(err) => Err(format_error(
            cli::wrap_graph_permission_error(graph, err).await,
        )),
    }
}

async fn resolve_mail_folder_mcp(
    storage: &Storage,
    graph: &GraphClient,
    folder_query: &str,
    offline: bool,
) -> Result<crate::models::MailFolder, String> {
    if offline {
        let q = folder_query.to_string();
        return with_storage(storage, move |s| resolver::resolve_mail_folder_cached(s, &q))
            .await
            .map_err(format_error);
    }
    map_graph_result(
        graph,
        resolver::resolve_mail_folder(graph, storage, folder_query, false).await,
    )
    .await
}

async fn resolve_todo_list_mcp(
    storage: &Storage,
    graph: &GraphClient,
    list_query: Option<&str>,
    offline: bool,
) -> Result<crate::models::TodoList, String> {
    let q = list_query
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tasks")
        .to_string();
    if offline {
        return with_storage(storage, move |s| resolver::resolve_todo_list_cached(s, &q))
            .await
            .map_err(format_error);
    }
    map_graph_result(
        graph,
        resolver::resolve_todo_list(graph, storage, &q, false).await,
    )
    .await
}

const COMPACT_MAIL_LIST_OMITTED_FIELDS: &[&str] = &[
    "internet_message_id",
    "conversation_id",
    "folder_id",
    "sender",
    "to_recipients",
    "cc_recipients",
    "bcc_recipients",
    "reply_to",
    "sent_at",
    "last_modified_at",
    "importance",
    "inference_classification",
    "categories",
    "body_loaded",
    "preview",
    "body_plain",
    "body_html",
    "attachments",
    "meeting_request",
    "web_link",
];

const COMPACT_EVENT_LIST_OMITTED_FIELDS: &[&str] = &["body_html", "recurrence"];

fn with_compact_mail_list_metadata(mut payload: Value, compact: bool, tool_name: &str) -> Value {
    if compact {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "omitted".into(),
                json!({ "fields": COMPACT_MAIL_LIST_OMITTED_FIELDS }),
            );
            obj.insert(
                "hint".into(),
                json!(format!(
                    "Compact {tool_name} omits recipients (to/cc/bcc/reply_to), sender, preview, body, importance, categories, meeting_request, attachments, and metadata — omitted fields are NOT absent. Call get_message (optionally with fields: [...] or compact: false) or re-run {tool_name} with compact: false."
                )),
            );
        }
    }
    payload
}

fn compact_event_list_item_json(ev: &crate::models::CalendarEvent, compact: bool) -> Value {
    let mut val = cli::event_json(ev, compact);
    if compact {
        if let Some(obj) = val.as_object_mut() {
            obj.remove("recurrence");
        }
    }
    val
}

fn with_compact_event_list_metadata(mut payload: Value, compact: bool) -> Value {
    if compact {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "omitted".into(),
                json!({ "fields": COMPACT_EVENT_LIST_OMITTED_FIELDS }),
            );
            obj.insert(
                "hint".into(),
                json!(
                    "Compact list_events omits body_html and recurrence — omitted fields are NOT absent. Call get_event (optionally with fields: [...] or compact: false) or re-run list_events with compact: false."
                ),
            );
        }
    }
    payload
}

fn event_read_json(ev: &crate::models::CalendarEvent, compact: bool) -> Value {
    if !compact {
        return json!({ "event": ev });
    }
    let mut val = cli::event_json(ev, true);
    if let Some(obj) = val.as_object_mut() {
        obj.insert("omitted".into(), json!({ "fields": ["body_html"] }));
        obj.insert(
            "hint".into(),
            json!("Call get_event with compact:false or fields:[\"body_html\"] for HTML content."),
        );
    }
    json!({ "event": val })
}

fn project_mail_message_fields(
    msg: &crate::models::MailMessage,
    requested: &[String],
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(msg.id));
    let mut matched_fields: Vec<String> = Vec::new();
    let mut missing_fields: Vec<String> = Vec::new();

    for raw in requested {
        let key = raw.trim().to_ascii_lowercase();
        let mapped: Option<(&str, Value)> = match key.as_str() {
            "id" => Some(("id", json!(msg.id))),
            "internet_message_id" => Some(("internet_message_id", json!(msg.internet_message_id))),
            "conversation_id" => Some(("conversation_id", json!(msg.conversation_id))),
            "folder_id" => Some(("folder_id", json!(msg.folder_id))),
            "subject" => Some(("subject", json!(msg.subject))),
            "from" => Some(("from", json!(msg.from))),
            "sender" => Some(("sender", json!(msg.sender))),
            "to" | "to_recipients" => Some(("to_recipients", json!(msg.to_recipients))),
            "cc" | "cc_recipients" => Some(("cc_recipients", json!(msg.cc_recipients))),
            "bcc" | "bcc_recipients" => Some(("bcc_recipients", json!(msg.bcc_recipients))),
            "reply_to" => Some(("reply_to", json!(msg.reply_to))),
            "received_at" | "received" => Some(("received_at", json!(msg.received_at))),
            "sent_at" | "sent" => Some(("sent_at", json!(msg.sent_at))),
            "last_modified_at" => Some(("last_modified_at", json!(msg.last_modified_at))),
            "is_read" => Some(("is_read", json!(msg.is_read))),
            "is_draft" => Some(("is_draft", json!(msg.is_draft))),
            "flagged" => Some(("flagged", json!(msg.flagged))),
            "importance" => Some(("importance", json!(msg.importance))),
            "inference_classification" => Some((
                "inference_classification",
                json!(msg.inference_classification),
            )),
            "categories" => Some(("categories", json!(msg.categories))),
            "has_attachments" => Some(("has_attachments", json!(msg.has_attachments))),
            "body_loaded" => Some(("body_loaded", json!(msg.body_loaded))),
            "preview" => Some(("preview", json!(msg.preview))),
            "body_plain" | "body" => Some((
                "body_plain",
                if msg.body_loaded {
                    json!(msg.body_plain)
                } else {
                    Value::Null
                },
            )),
            "body_html" | "html" => Some(("body_html", json!(msg.body_html))),
            "attachments" => Some((
                "attachments",
                cli::mail_attachments_public_json(&msg.attachments),
            )),
            "meeting_request" => Some(("meeting_request", json!(msg.meeting_request))),
            "web_link" => Some(("web_link", json!(msg.web_link))),
            _ => None,
        };
        if let Some((canonical, value)) = mapped {
            out.insert(canonical.into(), value);
            if !matched_fields.iter().any(|f| f == canonical) {
                matched_fields.push(canonical.to_string());
            }
        } else if !missing_fields.iter().any(|f| f == raw) {
            missing_fields.push(raw.clone());
        }
    }

    let mut payload = json!({
        "message": Value::Object(out),
        "fields": matched_fields,
    });
    if !missing_fields.is_empty() {
        payload["missing_fields"] = json!(missing_fields);
    }
    payload
}

fn project_calendar_event_fields(
    ev: &crate::models::CalendarEvent,
    requested: &[String],
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(ev.id));
    let mut matched_fields: Vec<String> = Vec::new();
    let mut missing_fields: Vec<String> = Vec::new();

    for raw in requested {
        let key = raw.trim().to_ascii_lowercase();
        if key == "organizer" {
            out.insert("organizer_name".into(), json!(ev.organizer_name));
            out.insert("organizer_email".into(), json!(ev.organizer_email));
            for canonical in ["organizer_name", "organizer_email"] {
                if !matched_fields.iter().any(|f| f == canonical) {
                    matched_fields.push(canonical.to_string());
                }
            }
            continue;
        }
        let mapped: Option<(&str, Value)> = match key.as_str() {
            "id" => Some(("id", json!(ev.id))),
            "subject" => Some(("subject", json!(ev.subject))),
            "start" | "start_iso" => Some(("start_iso", json!(ev.start_iso))),
            "end" | "end_iso" => Some(("end_iso", json!(ev.end_iso))),
            "time_zone" => Some(("time_zone", json!(ev.time_zone))),
            "is_all_day" => Some(("is_all_day", json!(ev.is_all_day))),
            "location" => Some(("location", json!(ev.location))),
            "organizer_name" => Some(("organizer_name", json!(ev.organizer_name))),
            "organizer_email" => Some(("organizer_email", json!(ev.organizer_email))),
            "attendees" => Some(("attendees", json!(ev.attendees))),
            "response_status" => Some(("response_status", json!(ev.response_status))),
            "is_online_meeting" => Some(("is_online_meeting", json!(ev.is_online_meeting))),
            "online_meeting_url" => Some(("online_meeting_url", json!(ev.online_meeting_url))),
            "body_html" | "html" => Some(("body_html", json!(ev.body_html))),
            "body_plain" | "body" => Some(("body_plain", json!(ev.body_plain))),
            "is_cancelled" => Some(("is_cancelled", json!(ev.is_cancelled))),
            "web_link" => Some(("web_link", json!(ev.web_link))),
            "categories" => Some(("categories", json!(ev.categories))),
            "show_as" => Some(("show_as", json!(ev.show_as))),
            "sensitivity" => Some(("sensitivity", json!(ev.sensitivity))),
            "event_type" | "type" => Some(("event_type", json!(ev.event_type))),
            "has_attachments" => Some(("has_attachments", json!(ev.has_attachments))),
            "is_recurring" => Some(("is_recurring", json!(ev.is_recurring))),
            "recurrence" => Some(("recurrence", json!(ev.recurrence))),
            _ => None,
        };
        if let Some((canonical, value)) = mapped {
            out.insert(canonical.into(), value);
            if !matched_fields.iter().any(|f| f == canonical) {
                matched_fields.push(canonical.to_string());
            }
        } else if !missing_fields.iter().any(|f| f == raw) {
            missing_fields.push(raw.clone());
        }
    }

    let mut payload = json!({
        "event": Value::Object(out),
        "fields": matched_fields,
    });
    if !missing_fields.is_empty() {
        payload["missing_fields"] = json!(missing_fields);
    }
    payload
}

/// Run the stdio MCP server until stdin reaches EOF.
pub async fn run(config: AppConfig, graph: GraphClient, storage: Storage) -> Result<()> {
    let is_empty = with_storage(&storage, |s| Ok(s.list_mail_folders()?.is_empty()))
        .await
        .unwrap_or(false);
    if is_empty {
        let bg_graph = graph.clone();
        let bg_storage = storage.clone();
        tokio::spawn(async move {
            match bg_graph.list_mail_folders().await {
                Ok(folders) => {
                    let _ = with_storage(&bg_storage, move |s| s.upsert_mail_folders(&folders)).await;
                }
                Err(err) => {
                    eprintln!(
                        "{SERVER_NAME} MCP warning: initial mail folder bootstrap failed: {err:#}"
                    );
                }
            }
        });
    }

    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, user_id={})",
        graph.my_user_id().unwrap_or("unknown")
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = std::io::stdout();

    loop {
        match read_line_bounded(&mut reader, MAX_RPC_LINE_BYTES).await? {
            BoundedLine::Eof => break,
            BoundedLine::TooLong => {
                let err = error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    format!(
                        "invalid request: line exceeds {} MiB limit",
                        MAX_RPC_LINE_BYTES / (1024 * 1024)
                    ),
                );
                stdout.write_all(encode_message(&err).as_bytes())?;
                stdout.flush()?;
            }
            BoundedLine::Line(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if let Some(response) = handle_rpc_line(&config, &graph, &storage, trimmed).await {
                    stdout.write_all(encode_message(&response).as_bytes())?;
                    stdout.flush()?;
                }
            }
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
        "whoami" => {
            let tokens = graph.auth().current_tokens().await;
            let auth_source = cli::auth_source_label(tokens.as_ref(), config);
            let Some(tok) = tokens else {
                return Ok(json!({
                    "authenticated": false,
                    "auth_source": auth_source,
                }));
            };
            let diag = diagnose_access_token_scopes(&tok.access_token);
            Ok(json!({
                "authenticated": true,
                "user_id": tok.user_id,
                "display_name": tok.user_display_name,
                "email": tok.user_email,
                "client_id": tok.client_id,
                "tenant_id": tok.tenant_id,
                "auth_mode": tok.auth_mode,
                "auth_source": auth_source,
                "expires_at": tok.expires_at,
                "expired": tok.is_expired_soon(0),
                "scopes": diag.scopes,
                "roles": diag.roles,
            }))
        }

        "list_mail_folders" => {
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .unwrap_or(50);
            let cursor = args.optional_str("cursor")?;
            if cursor.is_some() && refresh {
                return Err("cursor cannot be combined with refresh".to_string());
            }
            let scope = cli::cursor_scope(&["folders"]);
            let (folders, offset) = if let Some(raw_cursor) = cursor {
                let parsed = cli::PageCursor::parse(&raw_cursor).map_err(format_error)?;
                let cli::CursorPos::Offset(offset) = parsed.pos else {
                    return Err("invalid cursor: not valid for this command".to_string());
                };
                parsed.require_scope(&scope).map_err(format_error)?;
                let cached = with_storage(storage, |s| s.list_mail_folders())
                    .await
                    .map_err(format_error)?;
                (cached, offset)
            } else {
                let ttl = cli::cli_cache_ttl_secs(config);
                let (cached, fresh) = with_storage(storage, move |s| {
                    let folders = s.list_mail_folders()?;
                    let is_fresh = cli::is_cursor_fresh(s, &["cli:mail_folders"], ttl);
                    Ok((folders, is_fresh))
                })
                .await
                .map_err(format_error)?;
                let folders = if !offline && (refresh || cached.is_empty() || !fresh) {
                    let (fetched, complete) =
                        map_graph_result(graph, graph.list_mail_folders_complete().await).await?;
                    with_storage(storage, move |s| {
                        s.store_mail_folder_listing(&fetched, complete)?;
                        if complete {
                            cli::record_cli_sync_cursor(s, "cli:mail_folders");
                        }
                        s.list_mail_folders()
                    })
                    .await
                    .map_err(format_error)?
                } else {
                    cached
                };
                (folders, 0)
            };
            let (page, has_more, next_cursor) = cli::offset_page(&folders, offset, limit, &scope);
            Ok(json!({
                "folders": page,
                "returned": page.len(),
                "has_more": has_more,
                "next_cursor": next_cursor,
            }))
        }

        "list_messages" => {
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
            let flagged = args.optional_bool("flagged")?.unwrap_or(false);
            let focused = args.optional_bool("focused")?.unwrap_or(false);
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            let cursor = args.optional_str("cursor")?;

            if let Some(raw_cursor) = cursor {
                let parsed = cli::PageCursor::parse(&raw_cursor).map_err(format_error)?;
                if matches!(parsed.pos, cli::CursorPos::Offset(_)) {
                    return Err("invalid cursor: not valid for this command".to_string());
                }
                let q = folder.clone();
                let resolved = with_storage(storage, move |s| {
                    resolver::resolve_mail_folder_cached(s, &q)
                })
                .await
                .map_err(format_error)?;
                let scope = cli::mail_cursor_scope(&resolved.id, unread, flagged, focused);
                parsed.require_scope(&scope).map_err(format_error)?;

                let page = match parsed.pos {
                    cli::CursorPos::Cache { received_at, id } => {
                        let fid = resolved.id.clone();
                        let scope_clone = scope.clone();
                        with_storage(storage, move |s| {
                            cli::load_cached_mail_list(
                                s,
                                &fid,
                                unread,
                                flagged,
                                focused,
                                limit,
                                !compact,
                                Some((&received_at, &id)),
                                &scope_clone,
                            )
                        })
                        .await
                        .map_err(format_error)?
                    }
                    cli::CursorPos::Graph { skip, url } => {
                        if offline {
                            return Err(
                                "invalid cursor: this cursor needs online access".to_string()
                            );
                        }
                        if !graph.owns_url(&url) {
                            return Err(
                                "invalid cursor: url is not a Graph URL for this account"
                                    .to_string(),
                            );
                        }
                        let filtered = map_graph_result(
                            graph,
                            graph
                                .list_mail_messages_filtered_paged(
                                    &resolved.id,
                                    unread,
                                    flagged,
                                    focused,
                                    limit,
                                    true,
                                    &scope,
                                    Some((skip, url)),
                                    Some(cli::FILTER_SCAN_MAX_PAGES),
                                )
                                .await,
                        )
                        .await?;
                        let matched = filtered.matched;
                        let fetched = filtered.fetched;
                        let messages = with_storage(storage, move |s| {
                            s.upsert_mail_messages(&fetched)?;
                            if !compact {
                                Ok(matched
                                    .into_iter()
                                    .map(|m| s.get_mail_message(&m.id).ok().flatten().unwrap_or(m))
                                    .collect())
                            } else {
                                Ok(matched)
                            }
                        })
                        .await
                        .map_err(format_error)?;
                        cli::MailListPage {
                            messages,
                            has_more: filtered.has_more,
                            next_cursor: filtered.next_cursor,
                            sync_complete: true,
                        }
                    }
                    cli::CursorPos::Offset(_) => unreachable!(),
                };

                return Ok(with_compact_mail_list_metadata(
                    json!({
                        "folder": resolved,
                        "messages": cli::mail_list_json(&page.messages, compact),
                        "returned": page.messages.len(),
                        "has_more": page.has_more,
                        "next_cursor": page.next_cursor,
                    }),
                    compact,
                    "list_messages",
                ));
            }

            let mut resolved = resolve_mail_folder_mcp(storage, graph, &folder, offline).await?;
            let has_filter = unread || flagged || focused;

            let page = if offline {
                let fid = resolved.id.clone();
                let scope = cli::mail_cursor_scope(&fid, unread, flagged, focused);
                with_storage(storage, move |s| {
                    cli::load_cached_mail_list(
                        s,
                        &fid,
                        unread,
                        flagged,
                        focused,
                        limit,
                        !compact,
                        None,
                        &scope,
                    )
                })
                .await
                .map_err(format_error)?
            } else if has_filter {
                let scope = cli::mail_cursor_scope(&resolved.id, unread, flagged, focused);
                let filtered = match graph
                    .list_mail_messages_filtered_paged(
                        &resolved.id,
                        unread,
                        flagged,
                        focused,
                        limit,
                        true,
                        &scope,
                        None,
                        Some(cli::FILTER_SCAN_MAX_PAGES),
                    )
                    .await
                {
                    Ok(res) => res,
                    Err(err) if resolver::is_graph_not_found_error(&err) => {
                        match map_graph_result(
                            graph,
                            resolver::refresh_mail_folder_after_not_found(
                                graph,
                                storage,
                                &folder,
                                &resolved.id,
                            )
                            .await,
                        )
                        .await?
                        {
                            Some(new_folder) => {
                                resolved = new_folder;
                                let new_scope = cli::mail_cursor_scope(
                                    &resolved.id,
                                    unread,
                                    flagged,
                                    focused,
                                );
                                map_graph_result(
                                    graph,
                                    graph
                                        .list_mail_messages_filtered_paged(
                                            &resolved.id,
                                            unread,
                                            flagged,
                                            focused,
                                            limit,
                                            true,
                                            &new_scope,
                                            None,
                                            Some(cli::FILTER_SCAN_MAX_PAGES),
                                        )
                                        .await,
                                )
                                .await?
                            }
                            None => {
                                return Err(map_graph_result(graph, Err::<(), _>(err))
                                    .await
                                    .unwrap_err());
                            }
                        }
                    }
                    Err(err) => {
                        return Err(map_graph_result(graph, Err::<(), _>(err)).await.unwrap_err());
                    }
                };
                let matched = filtered.matched;
                let fetched = filtered.fetched;
                let messages = with_storage(storage, move |s| {
                    s.upsert_mail_messages(&fetched)?;
                    if !compact {
                        Ok(matched
                            .into_iter()
                            .map(|m| s.get_mail_message(&m.id).ok().flatten().unwrap_or(m))
                            .collect())
                    } else {
                        Ok(matched)
                    }
                })
                .await
                .map_err(format_error)?;
                cli::MailListPage {
                    messages,
                    has_more: filtered.has_more,
                    next_cursor: filtered.next_cursor,
                    sync_complete: true,
                }
            } else {
                let outcome = match crate::sync::sync_mail_folder_for_listing(
                    &resolved.id,
                    graph,
                    storage,
                    limit,
                )
                .await
                {
                    Ok(o) => o,
                    Err(err) if resolver::is_graph_not_found_error(&err) => {
                        match map_graph_result(
                            graph,
                            resolver::refresh_mail_folder_after_not_found(
                                graph,
                                storage,
                                &folder,
                                &resolved.id,
                            )
                            .await,
                        )
                        .await?
                        {
                            Some(new_folder) => {
                                resolved = new_folder;
                                map_graph_result(
                                    graph,
                                    crate::sync::sync_mail_folder_for_listing(
                                        &resolved.id,
                                        graph,
                                        storage,
                                        limit,
                                    )
                                    .await,
                                )
                                .await?
                            }
                            None => {
                                return Err(map_graph_result(graph, Err::<(), _>(err))
                                    .await
                                    .unwrap_err());
                            }
                        }
                    }
                    Err(err) => {
                        return Err(map_graph_result(graph, Err::<(), _>(err)).await.unwrap_err());
                    }
                };
                let fid = outcome.folder_id;
                let sync_complete = outcome.complete;
                let scope = cli::mail_cursor_scope(&fid, false, false, false);
                let mut page = with_storage(storage, move |s| {
                    cli::record_cli_sync_cursor(s, &format!("cli:mail:{fid}"));
                    cli::load_cached_mail_list(
                        s,
                        &fid,
                        false,
                        false,
                        false,
                        limit,
                        !compact,
                        None,
                        &scope,
                    )
                })
                .await
                .map_err(format_error)?;
                page.sync_complete = sync_complete;
                page
            };

            Ok(with_compact_mail_list_metadata(
                cli::with_sync_complete_marker(
                    json!({
                        "folder": resolved,
                        "messages": cli::mail_list_json(&page.messages, compact),
                        "returned": page.messages.len(),
                        "has_more": page.has_more,
                        "next_cursor": page.next_cursor,
                    }),
                    page.sync_complete,
                ),
                compact,
                "list_messages",
            ))
        }

        "get_message" => {
            let message_id = args.required_str("message_id")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let mark_read = args.optional_bool("mark_read")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            let fields = args.optional_str_array("fields")?;

            let mut msg = if offline {
                let id_owned = message_id.clone();
                with_storage(storage, move |s| s.get_mail_message(&id_owned))
                    .await
                    .map_err(format_error)?
                    .ok_or_else(|| format!("mail message not found in cache: {message_id}"))?
            } else {
                let fetched =
                    map_graph_result(graph, graph.get_mail_message(&message_id).await).await?;
                let fetched_clone = fetched.clone();
                let _ = with_storage(storage, move |s| {
                    s.upsert_mail_messages(std::slice::from_ref(&fetched_clone))
                })
                .await;
                fetched
            };

            if !offline && mark_read && !msg.is_read {
                map_graph_result(graph, graph.set_mail_read(&msg.id, true).await).await?;
                msg.is_read = true;
                let mid = msg.id.clone();
                let _ = with_storage(storage, move |s| s.set_mail_read_local(&mid, true)).await;
            }

            if let Some(requested) = fields.filter(|f| !f.is_empty()) {
                Ok(project_mail_message_fields(&msg, &requested))
            } else {
                Ok(cli::mail_read_json(&msg, compact, cli::Surface::Mcp))
            }
        }

        "send_mail" => {
            let to = args.required_str_array("to")?;
            let subject = args.required_str("subject")?;
            let body = args.required_text("body")?;
            let cc = args.optional_str_array("cc")?.unwrap_or_default();
            let bcc = args.optional_str_array("bcc")?.unwrap_or_default();
            let importance = args
                .optional_str("importance")?
                .map(|v| cli::normalize_mail_importance(&v).map(str::to_string))
                .transpose()
                .map_err(format_error)?;
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
            let body_html = crate::parser::compile_outgoing_mail_html(&body, &[], None);

            let outgoing = OutgoingMail {
                subject: subject.clone(),
                body_html,
                body_markdown: Some(body),
                to: to.clone(),
                cc,
                bcc,
                importance,
                attachments,
                inline_images: Vec::new(),
                in_reply_to_id: None,
                reply_all: false,
                forward_of_id: None,
                draft_id: None,
                save_to_sent_items,
            };
            map_graph_result(graph, graph.send_mail(&outgoing).await).await?;

            Ok(json!({
                "ok": true,
                "to": to,
                "subject": subject,
            }))
        }

        "reply_mail" => {
            let message_id = args.required_str("message_id")?;
            let body = args.required_text("body")?;
            let reply_all = args.optional_bool("reply_all")?.unwrap_or(false);
            let body_html = crate::parser::compile_outgoing_mail_html(&body, &[], None);
            map_graph_result(
                graph,
                graph.reply_mail(&message_id, &body_html, reply_all).await,
            )
            .await?;

            Ok(json!({
                "ok": true,
                "message_id": message_id,
                "reply_all": reply_all,
            }))
        }

        "forward_mail" => {
            let message_id = args.required_str("message_id")?;
            let to = args.required_str_array("to")?;
            let comment_html = match args.optional_str("comment")? {
                Some(c) => crate::parser::compile_outgoing_mail_html(&c, &[], None),
                None => String::new(),
            };
            map_graph_result(
                graph,
                graph.forward_mail(&message_id, &to, &comment_html).await,
            )
            .await?;

            Ok(json!({
                "ok": true,
                "message_id": message_id,
                "to": to,
            }))
        }

        "update_draft" => {
            let id = args.required_str("message_id")?;
            let subject = args.optional_text("subject")?;
            let body = args.optional_text("body")?;
            let body_html = args.optional_text("body_html")?;
            let html = cli::draft_edit_html(body, body_html);
            let preserve_quote = body.is_some();
            let quote_preserved = map_graph_result(
                graph,
                graph
                    .edit_mail_draft(&id, subject, html.as_deref(), preserve_quote)
                    .await,
            )
            .await?;
            if let Ok(updated) = graph.get_mail_message(&id).await {
                let _ = with_storage(storage, move |s| s.upsert_mail_messages(&[updated])).await;
            }
            let mut payload = json!({ "ok": true, "message_id": id });
            if html.is_some() {
                payload["quote_preserved"] = json!(quote_preserved);
            }
            Ok(payload)
        }

        "update_event" => {
            let id = args.required_str("event_id")?;
            let subject = args.optional_text("subject")?;
            let start = args
                .optional_text("start")?
                .map(CalendarEditTime::parse)
                .transpose()
                .map_err(format_error)?;
            let end = args
                .optional_text("end")?
                .map(CalendarEditTime::parse)
                .transpose()
                .map_err(format_error)?;
            let updated = map_graph_result(
                graph,
                graph
                    .edit_calendar_event(&id, subject, start.as_ref(), end.as_ref())
                    .await,
            )
            .await?;
            let cached = updated.clone();
            let _ = with_storage(storage, move |s| s.upsert_calendar_events(&[cached])).await;
            Ok(json!({ "ok": true, "event": updated }))
        }

        "update_task" => {
            let id = args.required_str("task_id")?;
            let list = args.optional_str("list")?;
            let title = args.optional_text("title")?;
            let due = args
                .optional_text("due")?
                .map(cli::normalize_todo_edit_due)
                .transpose()
                .map_err(format_error)?;
            let resolved = resolve_todo_list_mcp(storage, graph, list.as_deref(), false).await?;
            let updated = map_graph_result(
                graph,
                graph
                    .edit_todo_task(&resolved.id, &id, title, due.as_deref())
                    .await,
            )
            .await?;
            let cached = updated.clone();
            let _ = with_storage(storage, move |s| s.upsert_todo_tasks(&[cached])).await;
            Ok(json!({ "ok": true, "task": updated }))
        }

        "update_message" => {
            let message_id = args.required_str("message_id")?;
            let is_read = args.optional_bool("is_read")?;
            let flagged = args.optional_bool("flagged")?;
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            let dest_folder = args
                .optional_str("destination_folder")?
                .or(args.optional_str("destination_folder_id")?)
                .or(args.optional_str("move_to_folder")?);
            if is_read.is_none() && flagged.is_none() && dest_folder.is_none() {
                return Err(
                    "at least one of is_read, flagged, or destination_folder must be provided"
                        .to_string(),
                );
            }

            if let Some(read_val) = is_read {
                map_graph_result(graph, graph.set_mail_read(&message_id, read_val).await).await?;
            }
            if let Some(flag_val) = flagged {
                map_graph_result(graph, graph.set_mail_flag(&message_id, flag_val).await).await?;
            }

            let mid_clone = message_id.clone();
            let _ = with_storage(storage, move |s| {
                if let Some(r) = is_read {
                    s.set_mail_read_local(&mid_clone, r)?;
                }
                if let Some(f) = flagged {
                    s.set_mail_flagged_local(&mid_clone, f)?;
                }
                Ok(())
            })
            .await;

            let moved_result = if let Some(dest) = dest_folder.as_deref() {
                let mut folder = resolve_mail_folder_mcp(storage, graph, dest, false).await?;
                let (moved, attachments_expanded) = match graph
                    .move_mail_message_with_meta(&message_id, &folder.id)
                    .await
                {
                    Ok(m) => m,
                    Err(err) if resolver::is_graph_not_found_error(&err) => {
                        match map_graph_result(
                            graph,
                            resolver::refresh_mail_folder_after_not_found(
                                graph,
                                storage,
                                dest,
                                &folder.id,
                            )
                            .await,
                        )
                        .await?
                        {
                            Some(new_folder) => {
                                folder = new_folder;
                                map_graph_result(
                                    graph,
                                    graph
                                        .move_mail_message_with_meta(&message_id, &folder.id)
                                        .await,
                                )
                                .await?
                            }
                            None => {
                                return Err(map_graph_result(graph, Err::<(), _>(err))
                                    .await
                                    .unwrap_err());
                            }
                        }
                    }
                    Err(err) => {
                        return Err(map_graph_result(graph, Err::<(), _>(err)).await.unwrap_err());
                    }
                };
                let moved_clone = moved.clone();
                let old_id_clone = message_id.clone();
                let persisted = with_storage(storage, move |s| {
                    s.replace_moved_mail_local_ex(&old_id_clone, &moved_clone, attachments_expanded)
                })
                .await
                .unwrap_or(moved);
                Some((persisted, folder.id))
            } else {
                None
            };

            let moved_json = moved_result.as_ref().map(|(m, folder_id)| {
                if compact {
                    json!({
                        "id": m.id,
                        "folder_id": m.folder_id.as_deref().unwrap_or(folder_id.as_str()),
                    })
                } else {
                    cli::mail_message_public_json(m)
                }
            });

            Ok(json!({
                "ok": true,
                "message_id": moved_result
                    .as_ref()
                    .map(|(m, _)| m.id.as_str())
                    .unwrap_or(message_id.as_str()),
                "is_read": is_read,
                "flagged": flagged,
                "moved": moved_json,
            }))
        }

        "delete_message" => {
            let message_id = args.required_str("message_id")?;
            args.required_true("confirm")?;
            map_graph_result(graph, graph.delete_mail_message(&message_id).await).await?;
            let id_clone = message_id.clone();
            let _ = with_storage(storage, move |s| s.delete_mail_messages_by_ids(&[id_clone])).await;
            Ok(json!({
                "ok": true,
                "message_id": message_id,
            }))
        }

        "download_attachment" => {
            let message_id = args.required_str("message_id")?;
            let attachment_index = args.optional_usize("attachment_index")?;
            let attachment = args.optional_str("attachment")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let output_dir = args
                .optional_str("output_dir")?
                .map(PathBuf::from)
                .unwrap_or_else(|| config.resolved_download_dir());

            let mail = if offline {
                let id_owned = message_id.clone();
                with_storage(storage, move |s| s.get_mail_message(&id_owned))
                    .await
                    .map_err(format_error)?
                    .ok_or_else(|| format!("mail message not found in cache: {message_id}"))?
            } else {
                let fetched =
                    map_graph_result(graph, graph.get_mail_message(&message_id).await).await?;
                let fetched_clone = fetched.clone();
                let _ = with_storage(storage, move |s| {
                    s.upsert_mail_messages(std::slice::from_ref(&fetched_clone))
                })
                .await;
                fetched
            };

            let selected =
                cli::select_mail_attachment(&mail, attachment_index, attachment.as_deref())
                    .map_err(format_error)?;
            let downloaded = if offline {
                let _ = cli::load_mail_attachment_bytes(graph, &mail.id, selected, true)
                    .await
                    .map_err(format_error)?;
                graph
                    .download_mail_attachment(&mail.id, selected, &output_dir)
                    .await
                    .map_err(format_error)?
            } else {
                map_graph_result(
                    graph,
                    graph
                        .download_mail_attachment(&mail.id, selected, &output_dir)
                        .await,
                )
                .await?
            };

            Ok(cli::format_downloaded_attachment_json(
                &message_id,
                selected,
                &downloaded,
            ))
        }

        "search_mail" => {
            let query = args.required_str("query")?;
            let folder = args.optional_str("folder")?;
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(25);
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            let cursor = args.optional_str("cursor")?;

            if let Some(raw_cursor) = cursor {
                let parsed = cli::PageCursor::parse(&raw_cursor).map_err(format_error)?;
                if !matches!(parsed.pos, cli::CursorPos::Graph { .. }) {
                    return Err("invalid cursor: not valid for this command".to_string());
                }
                let folder_id = match folder.as_deref() {
                    Some(f) => {
                        let q = f.to_string();
                        Some(
                            with_storage(storage, move |s| {
                                resolver::resolve_mail_folder_cached(s, &q)
                            })
                            .await
                            .map_err(format_error)?
                            .id,
                        )
                    }
                    None => None,
                };
                let scope = cli::cursor_scope(&[
                    "search",
                    folder_id.as_deref().unwrap_or(""),
                    query.trim(),
                ]);
                parsed.require_scope(&scope).map_err(format_error)?;
                let cli::CursorPos::Graph { skip, url } = parsed.pos else {
                    unreachable!();
                };
                if offline {
                    return Err("invalid cursor: this cursor needs online access".to_string());
                }
                if !graph.owns_url(&url) {
                    return Err(
                        "invalid cursor: url is not a Graph URL for this account".to_string()
                    );
                }
                let page = map_graph_result(
                    graph,
                    graph
                        .search_mail_remote_paged(
                            &query,
                            folder_id.as_deref(),
                            limit,
                            &scope,
                            Some((skip, url)),
                        )
                        .await,
                )
                .await?;
                let msgs_clone = page.messages.clone();
                let _ = with_storage(storage, move |s| s.upsert_mail_messages(&msgs_clone)).await;
                return Ok(with_compact_mail_list_metadata(
                    json!({
                        "query": query,
                        "messages": cli::mail_list_json(&page.messages, compact),
                        "returned": page.messages.len(),
                        "has_more": page.has_more,
                        "next_cursor": page.next_cursor,
                    }),
                    compact,
                    "search_mail",
                ));
            }

            let folder_id = match folder.as_deref() {
                Some(f) => Some(resolve_mail_folder_mcp(storage, graph, f, offline).await?.id),
                None => None,
            };

            if offline {
                let q_clone = query.clone();
                let fid_clone = folder_id.clone();
                let hits = with_storage(storage, move |s| {
                    s.search_mail(&q_clone, fid_clone.as_deref(), limit)
                })
                .await
                .map_err(format_error)?;
                Ok(json!({ "query": query, "hits": hits }))
            } else {
                let scope = cli::cursor_scope(&[
                    "search",
                    folder_id.as_deref().unwrap_or(""),
                    query.trim(),
                ]);
                let page = map_graph_result(
                    graph,
                    graph
                        .search_mail_remote_paged(&query, folder_id.as_deref(), limit, &scope, None)
                        .await,
                )
                .await?;
                let msgs_clone = page.messages.clone();
                let _ = with_storage(storage, move |s| s.upsert_mail_messages(&msgs_clone)).await;
                Ok(with_compact_mail_list_metadata(
                    json!({
                        "query": query,
                        "messages": cli::mail_list_json(&page.messages, compact),
                        "returned": page.messages.len(),
                        "has_more": page.has_more,
                        "next_cursor": page.next_cursor,
                    }),
                    compact,
                    "search_mail",
                ))
            }
        }

        "list_events" => {
            let from = args.optional_str("from")?.or(args.optional_str("start")?);
            let to = args.optional_str("to")?.or(args.optional_str("end")?);
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
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let (start_iso, end_iso) =
                cli::default_calendar_window(from.as_deref(), to.as_deref(), days)
                    .map_err(format_error)?;

            let (events, has_more) = if offline {
                let s_filter = start_iso.clone();
                let e_filter = end_iso.clone();
                let mut rows = with_storage(storage, move |s| {
                    s.list_calendar_events(
                        Some(&s_filter),
                        Some(&e_filter),
                        limit.saturating_add(1),
                    )
                })
                .await
                .map_err(format_error)?;
                let has_more = rows.len() > limit;
                if has_more {
                    rows.truncate(limit);
                }
                (rows, has_more)
            } else {
                let (fetched, reached_end) = map_graph_result(
                    graph,
                    graph
                        .list_calendar_view_complete(&start_iso, &end_iso, limit)
                        .await,
                )
                .await?;
                let fetched_clone = fetched.clone();
                let s_iso = start_iso.clone();
                let e_iso = end_iso.clone();
                let _ = with_storage(storage, move |s| {
                    if reached_end {
                        s.replace_calendar_events_in_window(&s_iso, &e_iso, &fetched_clone)?;
                        Ok(())
                    } else {
                        s.upsert_calendar_events(&fetched_clone)
                    }
                })
                .await;
                (fetched, !reached_end)
            };

            let events_json: Vec<Value> = events
                .iter()
                .map(|ev| compact_event_list_item_json(ev, compact))
                .collect();
            Ok(with_compact_event_list_metadata(
                json!({
                    "start": start_iso,
                    "end": end_iso,
                    "events": events_json,
                    "returned": events.len(),
                    "has_more": has_more,
                }),
                compact,
            ))
        }

        "get_event" => {
            let event_id = args.required_str("event_id")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            let fields = args.optional_str_array("fields")?;

            let ev = if offline {
                let id_owned = event_id.clone();
                with_storage(storage, move |s| s.get_calendar_event(&id_owned))
                    .await
                    .map_err(format_error)?
                    .ok_or_else(|| format!("calendar event not found in cache: {event_id}"))?
            } else {
                let fetched =
                    map_graph_result(graph, graph.get_calendar_event(&event_id).await).await?;
                let fetched_clone = fetched.clone();
                let _ = with_storage(storage, move |s| {
                    s.upsert_calendar_events(std::slice::from_ref(&fetched_clone))
                })
                .await;
                fetched
            };

            if let Some(requested) = fields.filter(|f| !f.is_empty()) {
                Ok(project_calendar_event_fields(&ev, &requested))
            } else {
                Ok(event_read_json(&ev, compact))
            }
        }

        "create_event" => {
            let subject = args.required_str("subject")?;
            let start_raw = args.required_str("start")?;
            let end_raw = args.required_str("end")?;
            let time_zone = args.optional_str("time_zone")?;
            let is_all_day = args.optional_bool("is_all_day")?.unwrap_or(false);
            let (start_iso, end_iso, time_zone) = crate::graph::calendar::resolve_new_event_times(
                &start_raw,
                &end_raw,
                is_all_day,
                time_zone.as_deref(),
            )
            .map_err(format_error)?;
            let location = args.optional_str("location")?;
            let body_html = args
                .optional_str("body")?
                .map(|md| crate::parser::compile_outgoing_mail_html(&md, &[], None));
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
            let created =
                map_graph_result(graph, graph.create_calendar_event(&outgoing).await).await?;
            let created_clone = created.clone();
            let _ = with_storage(storage, move |s| {
                s.upsert_calendar_events(std::slice::from_ref(&created_clone))
            })
            .await;

            Ok(json!({
                "ok": true,
                "event": created,
            }))
        }

        "respond_event" => {
            let event_id = args.required_str("event_id")?;
            let response_raw = args.required_str("response")?;
            let normalized =
                cli::normalize_rsvp_response(&response_raw).map_err(format_error)?;
            let comment = args.optional_str("comment")?;
            map_graph_result(
                graph,
                graph
                    .respond_calendar_event(&event_id, normalized, comment.as_deref())
                    .await,
            )
            .await?;
            let eid_clone = event_id.clone();
            let status = cli::rsvp_action_to_graph_status(normalized);
            let _ = with_storage(storage, move |s| {
                s.update_event_response_local(&eid_clone, status)
            })
            .await;

            Ok(json!({
                "ok": true,
                "event_id": event_id,
                "response": normalized,
            }))
        }

        "delete_event" => {
            let event_id = args.required_str("event_id")?;
            args.required_true("confirm")?;
            map_graph_result(graph, graph.delete_calendar_event(&event_id).await).await?;
            let id_clone = event_id.clone();
            let _ = with_storage(storage, move |s| s.delete_calendar_event(&id_clone)).await;

            Ok(json!({
                "ok": true,
                "event_id": event_id,
            }))
        }

        "list_todo_lists" => {
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .unwrap_or(50);
            let cursor = args.optional_str("cursor")?;
            if cursor.is_some() && refresh {
                return Err("cursor cannot be combined with refresh".to_string());
            }
            let scope = cli::cursor_scope(&["task-lists"]);
            let (lists, offset) = if let Some(raw_cursor) = cursor {
                let parsed = cli::PageCursor::parse(&raw_cursor).map_err(format_error)?;
                let cli::CursorPos::Offset(offset) = parsed.pos else {
                    return Err("invalid cursor: not valid for this command".to_string());
                };
                parsed.require_scope(&scope).map_err(format_error)?;
                let cached = with_storage(storage, |s| s.list_todo_lists())
                    .await
                    .map_err(format_error)?;
                (cached, offset)
            } else {
                let ttl = cli::cli_cache_ttl_secs(config);
                let (cached, fresh) = with_storage(storage, move |s| {
                    let lists = s.list_todo_lists()?;
                    let is_fresh = cli::is_cursor_fresh(s, &["cli:todo_lists"], ttl);
                    Ok((lists, is_fresh))
                })
                .await
                .map_err(format_error)?;
                let lists = if !offline && (refresh || cached.is_empty() || !fresh) {
                    let fetched = map_graph_result(graph, graph.list_todo_lists().await).await?;
                    let fetched_clone = fetched.clone();
                    with_storage(storage, move |s| {
                        s.replace_todo_lists(&fetched_clone)?;
                        cli::record_cli_sync_cursor(s, "cli:todo_lists");
                        s.list_todo_lists()
                    })
                    .await
                    .map_err(format_error)?
                } else {
                    cached
                };
                (lists, 0)
            };
            let (page, has_more, next_cursor) = cli::offset_page(&lists, offset, limit, &scope);
            Ok(json!({
                "lists": page,
                "returned": page.len(),
                "has_more": has_more,
                "next_cursor": next_cursor,
            }))
        }

        "list_tasks" => {
            let list_query = args.optional_str("list")?;
            let include_completed = args.optional_bool("include_completed")?.unwrap_or(false);
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(50);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let resolved =
                resolve_todo_list_mcp(storage, graph, list_query.as_deref(), offline).await?;
            let list_id = resolved.id.clone();

            let (mut tasks, has_more) = if offline {
                let lid = list_id.clone();
                let rows = with_storage(storage, move |s| {
                    s.list_todo_tasks(Some(&lid), include_completed)
                })
                .await
                .map_err(format_error)?;
                let has_more = rows.len() > limit;
                (rows, has_more)
            } else {
                let (fetched, reached_end) = map_graph_result(
                    graph,
                    graph
                        .list_todo_tasks_complete(&list_id, include_completed, limit)
                        .await,
                )
                .await?;
                let fetched_clone = fetched.clone();
                let lid_clone = list_id.clone();
                let _ = with_storage(storage, move |s| {
                    if reached_end {
                        s.replace_todo_tasks_for_list(&lid_clone, include_completed, &fetched_clone)
                    } else {
                        s.upsert_todo_tasks(&fetched_clone)
                    }
                })
                .await;
                let has_more = !reached_end || fetched.len() > limit;
                (fetched, has_more)
            };
            tasks.truncate(limit);

            Ok(json!({
                "list": resolved,
                "list_id": list_id,
                "tasks": tasks,
                "returned": tasks.len(),
                "has_more": has_more,
            }))
        }

        "create_task" => {
            let list_query = args.optional_str("list")?;
            let title = args.required_str("title")?;
            let body_plain = args.optional_str("notes")?.or(args.optional_str("body")?);
            let due_raw = args.optional_str("due")?.or(args.optional_str("due_date_iso")?);
            let due_date_iso = due_raw
                .map(|raw| cli::normalize_todo_due_iso(&raw))
                .transpose()
                .map_err(format_error)?;
            let importance = args
                .optional_str("importance")?
                .map(|raw| cli::normalize_todo_importance(&raw).map(str::to_string))
                .transpose()
                .map_err(format_error)?;

            let resolved =
                resolve_todo_list_mcp(storage, graph, list_query.as_deref(), false).await?;

            let outgoing = OutgoingTodoTask {
                title,
                body_plain,
                due_date_iso,
                importance,
            };
            let created =
                map_graph_result(graph, graph.create_todo_task(&resolved.id, &outgoing).await)
                    .await?;
            let created_clone = created.clone();
            let _ = with_storage(storage, move |s| {
                s.upsert_todo_tasks(std::slice::from_ref(&created_clone))
            })
            .await;

            Ok(json!({
                "ok": true,
                "task": created,
            }))
        }

        "complete_task" => {
            let list_query = args.optional_str("list")?;
            let task_id = args.required_str("task_id")?;
            let completed = args.optional_bool("completed")?.unwrap_or(true);

            let resolved =
                resolve_todo_list_mcp(storage, graph, list_query.as_deref(), false).await?;
            let updated = map_graph_result(
                graph,
                graph
                    .set_todo_task_completed(&resolved.id, &task_id, completed)
                    .await,
            )
            .await?;
            let updated_clone = updated.clone();
            let _ = with_storage(storage, move |s| {
                s.upsert_todo_tasks(std::slice::from_ref(&updated_clone))
            })
            .await;

            Ok(json!({
                "ok": true,
                "task": updated,
            }))
        }

        "search_contacts" => {
            let query = args.optional_str("query")?;
            let limit = args
                .optional_usize("limit")?
                .map(|v| bounds("limit", v, 1, 200))
                .transpose()?
                .map(cli::clamp_limit)
                .unwrap_or(25);
            let offline = args.optional_bool("offline")?.unwrap_or(false);

            let (contacts, directory) = if offline {
                let q_clone = query.clone();
                let rows = with_storage(storage, move |s| s.list_contacts(q_clone.as_deref(), limit))
                    .await
                    .map_err(format_error)?;
                (rows, cli::directory_status_json(false, None))
            } else if let Some(ref q) = query {
                let outcome = map_graph_result(
                    graph,
                    graph.search_contacts_and_directory_detailed(q, limit).await,
                )
                .await?;
                let fetched_clone = outcome.contacts.clone();
                let _ = with_storage(storage, move |s| s.upsert_contacts(&fetched_clone)).await;
                let dir = cli::directory_status_json(true, outcome.directory_error.as_ref());
                (outcome.contacts, dir)
            } else {
                let (fetched, reached_end) =
                    map_graph_result(graph, graph.list_contacts_complete(limit).await).await?;
                let fetched_clone = fetched.clone();
                let _ = with_storage(storage, move |s| {
                    if reached_end {
                        s.replace_personal_contacts(&fetched_clone)
                    } else {
                        s.upsert_contacts(&fetched_clone)
                    }
                })
                .await;
                (fetched, cli::directory_status_json(false, None))
            };

            Ok(json!({
                "contacts": contacts,
                "directory": directory,
            }))
        }

        other => Err(format!("unknown tool: {other}")),
    }
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
            "invalid request: expected JSON object",
        ));
    };

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        let id = object.get("id").cloned().unwrap_or(Value::Null);
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: jsonrpc must be '2.0'",
        ));
    }

    if object
        .get("method")
        .and_then(Value::as_str)
        .is_none_or(|m| m.trim().is_empty())
    {
        let id = object.get("id").cloned().unwrap_or(Value::Null);
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: method must be a non-empty string",
        ));
    }

    Ok(value)
}

fn initialize_result(request: &Value) -> Value {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL_VERSION
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Microsoft Outlook MCP server (Mail, Calendar, To Do, Contacts). Pass `offline: true` on read/search tools to query the local SQLite cache without network calls. `delete_message` and `delete_event` require `confirm: true`. Reads are compact by default (omitted fields are explicitly listed in `omitted.fields` — never assume omitted fields are absent); pass `fields: [...]` on `get_message`/`get_event` for selective projection or `compact: false` for full fields. Follow next_cursor to continue a listing.",
    })
}

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })
        }
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    })
}

fn encode_message(value: &Value) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

// ---------------------------------------------------------------------------
// Tool catalog (JSON Schemas)
// ---------------------------------------------------------------------------

pub fn tool_catalog() -> Vec<Value> {
    vec![
        json!({
            "name": "whoami",
            "description": "Display current user identity, token expiry, OAuth scopes, and auth source (outlook-cli vs shared teams-cli).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_mail_folders",
            "description": "List Outlook mail folders with unread and total item counts. Graph cost: 1 request plus 1 batch per folder nesting level, skipped while the cache is younger than OUTLOOK_CLI_CACHE_SECS (default 0).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of folders to return (1-200, default 50)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous response" },
                    "refresh": { "type": "boolean", "description": "Force a full folder-tree fetch even when the cache is fresh. Use only when a folder is missing." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "list_messages",
            "description": "List emails in a mail folder (accepts folder ID, display name/prefix, or well-known alias such as 'inbox', 'sent', 'drafts', 'archive', 'trash', 'junk'). Usually 1-2 Graph requests. The first call on a large folder syncs at most 40 pages (sync_complete: false until done); unread/flagged/focused filters scan at most 20 pages. offline: true reads the cache only.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "folder": { "type": "string", "description": "Folder name, alias, or ID (default 'inbox')" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of messages to return (1-200, default 25)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous response" },
                    "unread": { "type": "boolean", "description": "If true, return only unread messages." },
                    "flagged": { "type": "boolean", "description": "If true, return only flagged messages." },
                    "focused": { "type": "boolean", "description": "If true, return only Focused Inbox messages." },
                    "compact": { "type": "boolean", "description": "Default true. Set false for every cached field (no attachment bytes)" },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "get_message",
            "description": "Read an email by ID. Compact by default; pass fields: [...] for selective field projection (e.g. ['cc', 'bcc', 'categories', 'attachments']) without pulling full HTML, or set compact:false for HTML and attachment metadata. 1 Graph request unless offline (plus 1 when mark_read changes the state); compact:false can be large.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID from list_messages or search_mail." },
                    "mark_read": { "type": "boolean", "description": "If true, mark the message as read online (default false)." },
                    "compact": { "type": "boolean", "description": "Default true. Set false for HTML and attachment metadata (no attachment bytes)" },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional selective field projection (e.g. ['cc', 'bcc', 'categories', 'attachments']) returning only matched fields plus missing_fields if unrecognized."
                    },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "send_mail",
            "description": "Compose and send an email with Markdown body and optional file attachments. Attachments over 3 MiB are uploaded in chunks before sending, which takes several requests.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Array of recipient email addresses (at least one required)." },
                    "subject": { "type": "string", "description": "Subject line of the outgoing email." },
                    "body": { "type": "string", "description": "Markdown body" },
                    "cc": { "type": "array", "items": { "type": "string" }, "description": "Optional array of CC recipient email addresses." },
                    "bcc": { "type": "array", "items": { "type": "string" }, "description": "Optional array of BCC recipient email addresses." },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"], "description": "Message priority level: low, normal, or high." },
                    "attachments": { "type": "array", "items": { "type": "string" }, "description": "Optional array of local file paths to attach." },
                    "save_to_sent_items": { "type": "boolean", "description": "Save a copy in Sent Items (default true)." }
                },
                "required": ["to", "subject", "body"]
            }
        }),
        json!({
            "name": "reply_mail",
            "description": "Reply (or reply all) to an email using Markdown. Sends immediately (does not create a draft).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to reply to, from list_messages or search_mail." },
                    "body": { "type": "string", "description": "Reply body content formatted in Markdown." },
                    "reply_all": { "type": "boolean", "description": "If true, reply to all recipients instead of sender only (default false)." }
                },
                "required": ["message_id", "body"]
            }
        }),
        json!({
            "name": "forward_mail",
            "description": "Forward an email (including the original message and attachments) to one or more recipients. Sends immediately (does not create a draft).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to forward, from list_messages or search_mail." },
                    "to": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Array of recipient email addresses (at least one required)." },
                    "comment": { "type": "string", "description": "Optional Markdown note prepended above the forwarded message." }
                },
                "required": ["message_id", "to"]
            }
        }),
        json!({
            "name": "update_message",
            "description": "Update an email's read state, flag status, or move it to another folder. Does not edit message content; use update_draft to edit a draft's subject or body.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to update, from list_messages or search_mail." },
                    "is_read": { "type": "boolean", "description": "Set true to mark read or false to mark unread." },
                    "flagged": { "type": "boolean", "description": "Set true to flag or false to clear the flag." },
                    "destination_folder": { "type": "string", "description": "Target folder name, alias (e.g. 'archive'), or ID" },
                    "destination_folder_id": { "type": "string", "description": "Alias for destination_folder" },
                    "move_to_folder": { "type": "string", "description": "Alias for destination_folder" },
                    "compact": { "type": "boolean", "description": "Default true. Set false to include full moved message fields (no attachment bytes)" }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "delete_message",
            "description": "Destructive: delete an email message from Outlook and local cache. Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to delete, from list_messages or search_mail." },
                    "confirm": { "type": "boolean", "description": "Must be literal boolean true" }
                },
                "required": ["message_id", "confirm"]
            }
        }),
        json!({
            "name": "download_attachment",
            "description": "Download an Outlook email attachment or inline screenshot to disk and return structured JSON { ok, path, file_name, mime_type, size_bytes, is_image } so multimodal AI agents can read media directly from disk. Fetches the full attachment bytes with Graph requests unless offline reads them from the cache; time grows with file size.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Outlook message ID" },
                    "attachment_index": { "type": "integer", "minimum": 0, "description": "0-based attachment index (defaults to first image or first attachment)" },
                    "attachment": { "type": "string", "description": "Attachment ID, filename, or 0-based index" },
                    "output_dir": { "type": "string", "description": "Target directory on disk (defaults to configured download_dir)" },
                    "offline": { "type": "boolean", "description": "Read cached message metadata/inline bytes from SQLite without Graph API call" }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "search_mail",
            "description": "Search emails in local SQLite FTS5 (`offline: true`) or remotely via Microsoft Graph KQL `$search`. Remote search is 1 Graph request per page and is rate-limited by Exchange; prefer offline: true (local FTS, no request) for mail that is already synced.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "query": { "type": "string", "description": "Search query string (Graph KQL online, or FTS5 terms offline)." },
                    "folder": { "type": "string", "description": "Optional folder name, alias, or ID to restrict search." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of matching messages to return (1-200, default 25)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous remote search response" },
                    "compact": { "type": "boolean", "description": "Default true. Set false for full message fields in online results (no attachment bytes)" },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "list_events",
            "description": "List calendar events within a date/time window (defaults to next 7 days). Compact by default (omits body_html and recurrence, listed in omitted.fields). 1 Graph request per 100 events in the window; offline: true reads the cache only.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "from": { "type": "string", "description": "Start ISO-8601 or YYYY-MM-DD" },
                    "start": { "type": "string", "description": "Alias for from" },
                    "to": { "type": "string", "description": "End ISO-8601 or YYYY-MM-DD" },
                    "end": { "type": "string", "description": "Alias for to" },
                    "days": { "type": "integer", "minimum": 1, "maximum": 365, "description": "Number of days from start when to/end is omitted (1-365, default 7)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of events to return (1-200, default 50)." },
                    "compact": { "type": "boolean", "description": "Default true. Set false to include body_html and recurrence on each event" },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "get_event",
            "description": "Read a calendar event by ID. Compact by default (omits body_html); pass fields: [...] for selective field projection (e.g. ['attendees', 'body_plain', 'recurrence', 'categories']) without pulling full HTML, or compact:false for all fields. 1 Graph request unless offline.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID from list_events." },
                    "compact": { "type": "boolean", "description": "Default true. Set false to include body_html and all fields" },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional selective field projection (e.g. ['attendees', 'body_plain', 'recurrence', 'categories']) returning only matched fields plus missing_fields if unrecognized."
                    },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                },
                "required": ["event_id"]
            }
        }),
        json!({
            "name": "create_event",
            "description": "Create a new Outlook calendar event (optionally as an online Teams meeting) using ISO-8601/RFC3339 or wall-clock dates. Sends calendar invitations to all listed attendees.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "Title or subject of the calendar event." },
                    "start": { "type": "string", "description": "RFC3339 instant, wall clock 'YYYY-MM-DD HH:MM' (machine zone, or time_zone when given), or 'YYYY-MM-DD' when is_all_day." },
                    "end": { "type": "string", "description": "Same form as start. For all-day events the end date is exclusive (next day for a one-day event)." },
                    "time_zone": { "type": "string", "description": "IANA or Windows zone. Wall-clock start/end are read in this zone; RFC3339 values stay exact instants in UTC." },
                    "is_all_day": { "type": "boolean", "description": "All-day event: start and end must be 'YYYY-MM-DD'." },
                    "location": { "type": "string", "description": "Optional meeting location or room name." },
                    "body": { "type": "string", "description": "Optional event description or agenda in Markdown." },
                    "attendees": { "type": "array", "items": { "type": "string" }, "description": "Optional array of attendee email addresses to invite." },
                    "is_online_meeting": { "type": "boolean", "description": "If true, attach a Microsoft Teams meeting link (default false)." }
                },
                "required": ["subject", "start", "end"]
            }
        }),
        json!({
            "name": "respond_event",
            "description": "Respond (accept, tentative, or decline) to a calendar event invitation. Sends the RSVP response (and optional comment) to the organizer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID from list_events." },
                    "response": { "type": "string", "enum": ["accept", "tentative", "decline"], "description": "RSVP decision: accept, tentative, or decline." },
                    "comment": { "type": "string", "description": "Optional plain-text note sent to the organizer with your RSVP." }
                },
                "required": ["event_id", "response"]
            }
        }),
        json!({
            "name": "delete_event",
            "description": "Destructive: delete a calendar event from Outlook and local cache (cancels/notifies attendees if organizer). Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID to delete, from list_events." },
                    "confirm": { "type": "boolean", "description": "Must be literal boolean true" }
                },
                "required": ["event_id", "confirm"]
            }
        }),
        json!({
            "name": "list_todo_lists",
            "description": "List Microsoft To Do task lists. Graph cost: 1 request per page of lists, skipped while the cache is younger than OUTLOOK_CLI_CACHE_SECS (default 0).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of task lists to return (1-200, default 50)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous response" },
                    "refresh": { "type": "boolean", "description": "Force a refetch even when the cache is fresh." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "list_tasks",
            "description": "List tasks in a Microsoft To Do list (accepts list name, 'tasks'/'defaultList', or ID). 1 Graph request per 100 tasks unless offline.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "list": { "type": "string", "description": "To Do list name, alias ('tasks'), or ID from list_todo_lists (default 'tasks')." },
                    "include_completed": { "type": "boolean", "description": "If true, include completed tasks in results (default false)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of tasks to return (1-200, default 50)." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "create_task",
            "description": "Create a new task in a Microsoft To Do list (default 'tasks'; use list_todo_lists to find list names or IDs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Title of the new To Do task." },
                    "list": { "type": "string", "description": "Target To Do list name, alias, or ID from list_todo_lists (default 'tasks')." },
                    "notes": { "type": "string", "description": "Optional plain-text notes or body for the task." },
                    "due": { "type": "string", "description": "YYYY-MM-DD or ISO-8601" },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"], "description": "Task priority level: low, normal, or high." }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "complete_task",
            "description": "Mark a Microsoft To Do task completed (or incomplete with completed: false). Get task_id from list_tasks and list names/IDs from list_todo_lists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to update, from list_tasks." },
                    "list": { "type": "string", "description": "Target To Do list name, alias, or ID from list_todo_lists (default 'tasks')." },
                    "completed": { "type": "boolean", "description": "Set true to complete or false to reopen (default true)." }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "search_contacts",
            "description": "List or search personal Outlook contacts and Entra organization directory. A query reads personal contacts and the organization directory: 2-3 Graph requests.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Optional name or email search string; omit to list personal contacts." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of contacts to return (1-200, default 25)." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "update_draft",
            "description": "Edit subject and/or body of an existing Outlook draft only (not sent or received mail). Provide Markdown body or raw body_html.",
            "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["message_id"],
                "properties": {
                    "message_id": { "type": "string", "description": "Draft message ID from list_messages in the drafts folder." },
                    "subject": { "type": "string", "description": "Replacement subject line for the draft." },
                    "body": { "type": "string", "description": "Replacement body in Markdown" },
                    "body_html": { "type": "string", "description": "Replacement body in HTML" }
                }
            }
        }),
        json!({
            "name": "update_event",
            "description": "Edit subject and/or ISO-8601 start/end times of an existing calendar event. Updates notify invited attendees.",
            "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["event_id"],
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID to edit, from list_events." },
                    "subject": { "type": "string", "description": "Replacement title or subject for the event." },
                    "start": { "type": "string", "description": "RFC3339 instant, wall-clock time in the event's zone, or all-day date" },
                    "end": { "type": "string", "description": "RFC3339 instant, wall-clock time in the event's zone, or exclusive all-day end date" }
                }
            }
        }),
        json!({
            "name": "update_task",
            "description": "Edit title and/or due date of an existing Microsoft To Do task. Get task_id from list_tasks and list names/IDs from list_todo_lists.",
            "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["task_id"],
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to edit, from list_tasks." },
                    "list": { "type": "string", "description": "To Do list name, alias, or ID (default tasks)" },
                    "title": { "type": "string", "description": "Replacement title for the To Do task." },
                    "due": { "type": "string", "description": "YYYY-MM-DD at midnight UTC, or RFC3339" }
                }
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Argument validation & typed extraction
// ---------------------------------------------------------------------------

pub fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    let args = Args::new(arguments);
    args.object()?;

    match name {
        "whoami" => Ok(()),
        "list_mail_folders" => {
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            let cursor = args.optional_str("cursor")?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            if cursor.is_some() && refresh {
                return Err("cursor cannot be combined with refresh".to_string());
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        "list_messages" => {
            args.only(
                name,
                &[
                    "folder", "limit", "cursor", "unread", "flagged", "focused", "compact",
                    "offline",
                ],
            )?;
            args.optional_str("folder")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_str("cursor")?;
            args.optional_bool("unread")?;
            args.optional_bool("flagged")?;
            args.optional_bool("focused")?;
            args.optional_bool("compact")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "get_message" => {
            args.only(name, &["message_id", "mark_read", "compact", "fields", "offline"])?;
            args.required_str("message_id")?;
            args.optional_bool("mark_read")?;
            args.optional_bool("offline")?;
            args.optional_bool("compact")?;
            args.optional_str_array("fields")?;
            Ok(())
        }
        "send_mail" => {
            let to = args.required_str_array("to")?;
            cli::check_recipient_addresses(&to).map_err(format_error)?;
            args.required_str("subject")?;
            args.required_text("body")?;
            if let Some(cc) = args.optional_str_array("cc")? {
                cli::check_recipient_addresses(&cc).map_err(format_error)?;
            }
            if let Some(bcc) = args.optional_str_array("bcc")? {
                cli::check_recipient_addresses(&bcc).map_err(format_error)?;
            }
            if let Some(imp) = args.optional_str("importance")? {
                cli::normalize_mail_importance(&imp).map_err(format_error)?;
            }
            args.optional_str_array("attachments")?;
            args.optional_bool("save_to_sent_items")?;
            Ok(())
        }
        "reply_mail" => {
            args.required_str("message_id")?;
            args.required_text("body")?;
            args.optional_bool("reply_all")?;
            Ok(())
        }
        "forward_mail" => {
            args.required_str("message_id")?;
            let to = args.required_str_array("to")?;
            cli::check_recipient_addresses(&to).map_err(format_error)?;
            args.optional_str("comment")?;
            Ok(())
        }
        "update_draft" => {
            args.only(name, &["message_id", "subject", "body", "body_html"])?;
            args.required_str("message_id")?;
            let subject = args.optional_text("subject")?;
            let body = args.optional_text("body")?;
            let html = args.optional_text("body_html")?;
            if body.is_some() && html.is_some() {
                return Err("Provide either body or body_html.".into());
            }
            if subject.is_none() && body.is_none() && html.is_none() {
                return Err("Provide a draft subject or body to update.".into());
            }
            Ok(())
        }
        "update_event" => {
            args.only(name, &["event_id", "subject", "start", "end"])?;
            args.required_str("event_id")?;
            let subject = args.optional_text("subject")?;
            let start = args
                .optional_text("start")?
                .map(CalendarEditTime::parse)
                .transpose()
                .map_err(format_error)?;
            let end = args
                .optional_text("end")?
                .map(CalendarEditTime::parse)
                .transpose()
                .map_err(format_error)?;
            if subject.is_none() && start.is_none() && end.is_none() {
                return Err("Provide an event subject, start, or end to update.".into());
            }
            Ok(())
        }
        "update_task" => {
            args.only(name, &["task_id", "list", "title", "due"])?;
            args.required_str("task_id")?;
            args.optional_str("list")?;
            let title = args.optional_text("title")?;
            let due = args
                .optional_text("due")?
                .map(cli::normalize_todo_edit_due)
                .transpose()
                .map_err(format_error)?;
            if title.is_some_and(|title| title.trim().is_empty()) {
                return Err("Task title must not be empty.".into());
            }
            if title.is_none() && due.is_none() {
                return Err("Provide a task title or due date to update.".into());
            }
            Ok(())
        }
        "update_message" => {
            args.only(
                name,
                &[
                    "message_id",
                    "is_read",
                    "flagged",
                    "destination_folder",
                    "destination_folder_id",
                    "move_to_folder",
                    "compact",
                ],
            )?;
            args.required_str("message_id")?;
            let is_read = args.optional_bool("is_read")?;
            let flagged = args.optional_bool("flagged")?;
            args.optional_bool("compact")?;
            let dest = args
                .optional_str("destination_folder")?
                .or(args.optional_str("destination_folder_id")?)
                .or(args.optional_str("move_to_folder")?);
            if is_read.is_none() && flagged.is_none() && dest.is_none() {
                return Err(
                    "at least one of is_read, flagged, or destination_folder must be provided"
                        .to_string(),
                );
            }
            Ok(())
        }
        "delete_message" => {
            args.required_str("message_id")?;
            args.required_true("confirm")?;
            Ok(())
        }
        "download_attachment" => {
            args.required_str("message_id")?;
            args.optional_usize("attachment_index")?;
            args.optional_str("attachment")?;
            args.optional_str("output_dir")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "search_mail" => {
            args.only(
                name,
                &["query", "folder", "limit", "cursor", "compact", "offline"],
            )?;
            args.required_str("query")?;
            args.optional_str("folder")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_str("cursor")?;
            args.optional_bool("compact")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "list_events" => {
            args.only(
                name,
                &[
                    "from", "start", "to", "end", "days", "limit", "compact", "offline",
                ],
            )?;
            if let Some(f) = args.optional_str("from")?.or(args.optional_str("start")?) {
                cli::normalize_datetime_iso(&f).map_err(format_error)?;
            }
            if let Some(t) = args.optional_str("to")?.or(args.optional_str("end")?) {
                cli::normalize_datetime_iso(&t).map_err(format_error)?;
            }
            if let Some(days) = args.optional_i64("days")? {
                let d = usize::try_from(days)
                    .map_err(|_| "days must be between 1 and 365".to_string())?;
                bounds("days", d, 1, 365)?;
            }
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("compact")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "get_event" => {
            args.only(name, &["event_id", "compact", "fields", "offline"])?;
            args.required_str("event_id")?;
            args.optional_bool("compact")?;
            args.optional_str_array("fields")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "create_event" => {
            args.required_str("subject")?;
            let start = args.required_str("start")?;
            let end = args.required_str("end")?;
            cli::normalize_datetime_iso(&start).map_err(format_error)?;
            cli::normalize_datetime_iso(&end).map_err(format_error)?;
            args.optional_str("time_zone")?;
            args.optional_bool("is_all_day")?;
            args.optional_str("location")?;
            args.optional_str("body")?;
            if let Some(attendees) = args.optional_str_array("attendees")? {
                cli::check_recipient_addresses(&attendees).map_err(format_error)?;
            }
            args.optional_bool("is_online_meeting")?;
            Ok(())
        }
        "respond_event" => {
            args.required_str("event_id")?;
            let resp = args.required_str("response")?;
            cli::normalize_rsvp_response(&resp).map_err(format_error)?;
            args.optional_str("comment")?;
            Ok(())
        }
        "delete_event" => {
            args.required_str("event_id")?;
            args.required_true("confirm")?;
            Ok(())
        }
        "list_todo_lists" => {
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            let cursor = args.optional_str("cursor")?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            if cursor.is_some() && refresh {
                return Err("cursor cannot be combined with refresh".to_string());
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        "list_tasks" => {
            args.optional_str("list")?;
            args.optional_bool("include_completed")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        "create_task" => {
            args.required_str("title")?;
            args.optional_str("list")?;
            args.optional_str("notes")?;
            args.optional_str("body")?;
            if let Some(due) = args.optional_str("due")?.or(args.optional_str("due_date_iso")?) {
                cli::normalize_todo_due_iso(&due).map_err(format_error)?;
            }
            if let Some(imp) = args.optional_str("importance")? {
                cli::normalize_todo_importance(&imp).map_err(format_error)?;
            }
            Ok(())
        }
        "complete_task" => {
            args.required_str("task_id")?;
            args.optional_str("list")?;
            args.optional_bool("completed")?;
            Ok(())
        }
        "search_contacts" => {
            args.optional_str("query")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn bounds(name: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if value < min || value > max {
        Err(format!("{name} must be between {min} and {max} (got {value})"))
    } else {
        Ok(value)
    }
}

struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    fn new(value: &'a Value) -> Self {
        Self(value)
    }

    fn object(&self) -> Result<&serde_json::Map<String, Value>, String> {
        self.0
            .as_object()
            .ok_or_else(|| "arguments must be a JSON object".to_string())
    }

    fn only(&self, tool: &str, fields: &[&str]) -> Result<(), String> {
        if let Some(key) = self
            .object()?
            .keys()
            .find(|key| !fields.contains(&key.as_str()))
        {
            return Err(format!("unsupported argument for {tool}: {key}"));
        }
        Ok(())
    }

    fn optional_text(&self, key: &str) -> Result<Option<&str>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
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
        match self.object()?.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn required_text(&self, key: &str) -> Result<String, String> {
        match self.object()?.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("{key} must be a boolean")),
        }
    }

    fn required_true(&self, key: &str) -> Result<(), String> {
        match self.object()?.get(key) {
            Some(Value::Bool(true)) => Ok(()),
            _ => Err(format!("{key} must be literal boolean true")),
        }
    }

    fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => {
                let val = n
                    .as_u64()
                    .and_then(|u| usize::try_from(u).ok())
                    .ok_or_else(|| format!("{key} must be a non-negative integer"))?;
                Ok(Some(val))
            }
            Some(_) => Err(format!("{key} must be an integer")),
        }
    }

    fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| format!("{key} must be an integer"))
                .map(Some),
            Some(_) => Err(format!("{key} must be an integer")),
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
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        models::{MailFolder, MailMessage},
    };

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

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "outlook");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");

        assert!(handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await
        .is_none());

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"], json!({}));

        let err = handle_rpc_line(&cfg, &graph, &storage, "{not json")
            .await
            .unwrap();
        assert_eq!(err["error"]["code"], PARSE_ERROR);

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

        #[test]
    fn tool_catalog_states_graph_cost_for_cache_and_refresh_flags() {
        let catalog = tool_catalog();
        let mut flagged_tools = 0;
        for tool in &catalog {
            let name = tool["name"].as_str().unwrap();
            for flag in ["refresh", "offline"] {
                if let Some(prop) = tool["inputSchema"]["properties"].get(flag) {
                    let description = prop["description"].as_str().unwrap_or_default();
                    assert!(
                        !description.trim().is_empty(),
                        "{name}.{flag} needs a description of its Graph cost"
                    );
                    flagged_tools += 1;
                }
            }
        }
        assert!(flagged_tools >= 10, "only {flagged_tools} refresh/offline flags found");

        for name in [
            "list_messages",
            "search_mail",
            "list_mail_folders",
            "download_attachment",
        ] {
            let tool = catalog
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} missing from the catalog"));
            let description = tool["description"].as_str().unwrap().to_lowercase();
            assert!(
                description.contains("request") || description.contains("cache"),
                "{name} description must state its Graph cost: {description}"
            );
        }

        // The page caps quoted to agents must match the code.
        let list_messages = catalog
            .iter()
            .find(|tool| tool["name"] == "list_messages")
            .unwrap()["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(list_messages.contains(&format!(
            "at most {} pages (sync_complete",
            crate::sync::INTERACTIVE_DELTA_MAX_PAGES
        )));
        assert!(list_messages.contains(&format!(
            "scan at most {} pages",
            cli::FILTER_SCAN_MAX_PAGES
        )));
    }

#[tokio::test]
    async fn tools_list_exposes_outlook_tools() {
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
                "whoami",
                "list_mail_folders",
                "list_messages",
                "get_message",
                "send_mail",
                "reply_mail",
                "forward_mail",
                "update_message",
                "delete_message",
                "download_attachment",
                "search_mail",
                "list_events",
                "get_event",
                "create_event",
                "respond_event",
                "delete_event",
                "list_todo_lists",
                "list_tasks",
                "create_task",
                "complete_task",
                "search_contacts",
                "update_draft",
                "update_event",
                "update_task",
            ]
        );

        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }

    #[tokio::test]
    async fn every_tool_property_in_tools_list_has_non_empty_description() {
        let (cfg, graph, storage) = test_ctx();
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/list"}"#,
        )
        .await
        .unwrap();

        let tools = resp["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());

        let mut property_count = 0usize;
        for tool in tools {
            let tool_name = tool["name"].as_str().unwrap();
            let tool_desc = tool["description"].as_str().unwrap_or_default().trim();
            assert!(!tool_desc.is_empty(), "{tool_name} description is empty");

            let props = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{tool_name} missing inputSchema.properties object"));
            for (prop_name, prop_schema) in props {
                let desc = prop_schema
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                assert!(
                    !desc.is_empty(),
                    "{tool_name}.{prop_name} must have a non-empty description"
                );
                property_count += 1;
            }
        }
        assert!(property_count > 0, "expected tool properties to be audited");
    }

    #[tokio::test]
    async fn delete_tools_reject_missing_or_non_literal_true_confirm() {
        let (cfg, graph, storage) = test_ctx();

        for (tool, bad_args) in [
            ("delete_message", json!({ "message_id": "m1" })),
            (
                "delete_message",
                json!({ "message_id": "m1", "confirm": false }),
            ),
            (
                "delete_message",
                json!({ "message_id": "m1", "confirm": "true" }),
            ),
            ("delete_event", json!({ "event_id": "ev1", "confirm": 1 })),
        ] {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": bad_args,
                }
            })
            .to_string();
            let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
            assert_eq!(resp["result"]["isError"], true, "{resp}");
        }

        assert!(validate_tool_args(
            "delete_message",
            &json!({ "message_id": "m1", "confirm": true }),
        )
        .is_ok());
        assert!(validate_tool_args(
            "delete_event",
            &json!({ "event_id": "ev1", "confirm": true }),
        )
        .is_ok());
    }

    #[test]
    fn edit_tools_validate_field_sets_and_time_formats() {
        assert!(validate_tool_args(
            "update_draft",
            &json!({"message_id":"draft", "body_html":""})
        )
        .is_ok());
        assert!(validate_tool_args(
            "update_draft",
            &json!({"message_id":"draft", "body":"text", "body_html":"<p>text</p>"})
        )
        .is_err());
        assert!(validate_tool_args(
            "update_event",
            &json!({"event_id":"event", "subject":"new", "attendees":[]})
        )
        .is_err());
        assert!(validate_tool_args(
            "update_event",
            &json!({"event_id":"event", "start":"2026-11-24T09:00"})
        )
        .is_ok());
        assert!(
            validate_tool_args("update_task", &json!({"task_id":"task", "due":"tomorrow"}))
                .is_err()
        );
        assert!(validate_tool_args(
            "update_task",
            &json!({"task_id":"task", "due":"2026-11-24"})
        )
        .is_ok());
    }

    #[test]
    fn argument_validation_covers_outlook_tools() {
        assert!(validate_tool_args(
            "send_mail",
            &json!({
                "to": ["alice@example.com"],
                "subject": "Quarterly Review",
                "body": "Please see notes.",
                "importance": "high"
            })
        )
        .is_ok());
        assert!(validate_tool_args(
            "send_mail",
            &json!({
                "to": [],
                "subject": "Quarterly Review",
                "body": "Body"
            })
        )
        .is_err());
        assert!(validate_tool_args("update_message", &json!({ "message_id": "m1" })).is_err());
        assert_eq!(
            validate_tool_args(
                "update_message",
                &json!({
                    "message_id": "m1", "flagged": true, "subject": "unsupported"
                })
            )
            .unwrap_err(),
            "unsupported argument for update_message: subject"
        );
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1", "compact": "true"
            })
        )
        .is_err());
        assert!(validate_tool_args(
            "update_message",
            &json!({ "message_id": "m1", "destination_folder": "archive" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "respond_event",
            &json!({ "event_id": "e1", "response": "tentative" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "respond_event",
            &json!({ "event_id": "e1", "response": "maybe" })
        )
        .is_err());
        assert!(validate_tool_args(
            "create_task",
            &json!({ "title": "Ship release", "due": "2026-11-25", "importance": "normal" })
        )
        .is_ok());
    }

    #[tokio::test]
    async fn offline_list_messages_get_message_and_search_mail_work_against_sqlite() {
        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "folder-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 2,
            }])
            .unwrap();
        storage
            .upsert_mail_messages(&[
                MailMessage {
                    id: "msg-1".to_string(),
                    internet_message_id: None,
                    conversation_id: Some("conv-1".to_string()),
                    folder_id: Some("folder-inbox".to_string()),
                    subject: "Weekly Status".to_string(),
                    from: None,
                    to_recipients: Vec::new(),
                    cc_recipients: Vec::new(),
                    bcc_recipients: Vec::new(),
                    reply_to: Vec::new(),
                    received_at: "2026-11-15T10:00:00Z".to_string(),
                    sent_at: None,
                    last_modified_at: None,
                    is_read: true,
                    is_draft: false,
                    flagged: false,
                    importance: "normal".to_string(),
                    inference_classification: Some("focused".to_string()),
                    has_attachments: false,
                    body_loaded: true,
                    preview: "All checks green".to_string(),
                    body_html: "<p>All checks green</p>".to_string(),
                    body_plain: "All checks green".to_string(),
                    attachments: Vec::new(),
                    meeting_request: None,
                    web_link: None,
                    ..Default::default()
                },
                MailMessage {
                    id: "msg-2".to_string(),
                    internet_message_id: None,
                    conversation_id: Some("conv-2".to_string()),
                    folder_id: Some("folder-inbox".to_string()),
                    subject: "Kubernetes rollout checklist".to_string(),
                    from: None,
                    to_recipients: Vec::new(),
                    cc_recipients: Vec::new(),
                    bcc_recipients: Vec::new(),
                    reply_to: Vec::new(),
                    received_at: "2026-11-16T10:00:00Z".to_string(),
                    sent_at: None,
                    last_modified_at: None,
                    is_read: false,
                    is_draft: false,
                    flagged: true,
                    importance: "high".to_string(),
                    inference_classification: Some("focused".to_string()),
                    has_attachments: false,
                    body_loaded: true,
                    preview: "Deploy v0.1.0 to staging".to_string(),
                    body_html: "<p>Deploy v0.1.0 to staging</p>".to_string(),
                    body_plain: "Deploy v0.1.0 to staging".to_string(),
                    attachments: Vec::new(),
                    meeting_request: None,
                    web_link: None,
                    ..Default::default()
                },
            ])
            .unwrap();

        // list_messages (offline, unread=true)
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "tools/call",
                "params": {
                    "name": "list_messages",
                    "arguments": { "folder": "inbox", "unread": true, "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
        assert_eq!(payload["messages"][0]["id"], "msg-2");
        let listed = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_messages",
            &json!({
                "folder": "inbox", "unread": true, "flagged": true,
                "focused": true, "offline": true, "compact": true, "limit": 1,
            }),
        )
        .await
        .unwrap();
        assert_eq!(listed["messages"][0]["id"], payload["messages"][0]["id"]);
        assert!(listed["messages"][0].get("body_plain").is_none());
        assert!(listed["messages"][0].get("attachments").is_none());

        // get_message (offline)
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": {
                    "name": "get_message",
                    "arguments": { "message_id": "msg-1", "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["message"]["subject"], "Weekly Status");
        let detail = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_message",
            &json!({
                "message_id": "msg-1", "offline": true, "compact": true,
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            detail["message"]["body_plain"],
            payload["message"]["body_plain"]
        );
        assert!(detail["message"].get("body_html").is_none());
        assert!(detail["message"].get("attachments").is_none());

        // search_mail (offline)
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 22,
                "method": "tools/call",
                "params": {
                    "name": "search_mail",
                    "arguments": { "query": "Kubernetes", "folder": "inbox", "offline": true }
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
        assert_eq!(payload["hits"][0]["id"], "msg-2");
    }

    #[tokio::test]
    async fn bounded_line_reader_caps_oversized_frames() {
        let data = b"hello\r\n0123456789ABCDEF\nworld";
        let mut reader = BufReader::new(std::io::Cursor::new(&data[..]));

        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Line(s) => assert_eq!(s, "hello"),
            _ => panic!("expected first line"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::TooLong => {}
            _ => panic!("expected TooLong"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Line(s) => assert_eq!(s, "world"),
            _ => panic!("expected trailing line at EOF"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Eof => {}
            _ => panic!("expected Eof"),
        }
    }

    #[tokio::test]
    async fn download_attachment_returns_structured_agent_media_json() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use crate::models::MailAttachment;

        let (cfg, graph, storage) = test_ctx();
        let tmp = tempfile::tempdir().unwrap();
        let png_bytes = crate::ui::composer::encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        let pdf_bytes = b"%PDF-1.4\nhello\n".to_vec();

        storage
            .upsert_mail_messages(&[MailMessage {
                id: "msg-media".to_string(),
                internet_message_id: None,
                conversation_id: Some("conv-media".to_string()),
                folder_id: Some("inbox".to_string()),
                subject: "Architecture Screenshot".to_string(),
                from: None,
                to_recipients: Vec::new(),
                cc_recipients: Vec::new(),
                bcc_recipients: Vec::new(),
                reply_to: Vec::new(),
                received_at: "2026-11-19T12:00:00Z".to_string(),
                sent_at: None,
                last_modified_at: None,
                is_read: true,
                is_draft: false,
                flagged: false,
                importance: "normal".to_string(),
                inference_classification: None,
                has_attachments: true,
                body_loaded: true,
                preview: "Attached screenshot".to_string(),
                body_html: "<p>Attached screenshot</p>".to_string(),
                body_plain: "Attached screenshot".to_string(),
                attachments: vec![
                    MailAttachment {
                        id: "att-img".to_string(),
                        name: "diagram.png".to_string(),
                        content_type: "image/png".to_string(),
                        size: png_bytes.len() as u64,
                        is_inline: false,
                        content_id: None,
                        content_bytes_b64: Some(B64.encode(&png_bytes)),
                    },
                    MailAttachment {
                        id: "att-pdf".to_string(),
                        name: "spec.pdf".to_string(),
                        content_type: "application/octet-stream".to_string(),
                        size: pdf_bytes.len() as u64,
                        is_inline: false,
                        content_id: None,
                        content_bytes_b64: Some(B64.encode(&pdf_bytes)),
                    },
                ],
                meeting_request: None,
                web_link: None,
                ..Default::default()
            }])
            .unwrap();

        // Image attachment download
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 31,
                "method": "tools/call",
                "params": {
                    "name": "download_attachment",
                    "arguments": {
                        "message_id": "msg-media",
                        "attachment": "diagram.png",
                        "output_dir": tmp.path().to_string_lossy(),
                        "offline": true
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["file_name"], "diagram.png");
        assert_eq!(payload["mime_type"], "image/png");
        assert_eq!(payload["size_bytes"], png_bytes.len() as u64);
        assert_eq!(payload["is_image"], true);
        let saved_path = PathBuf::from(payload["path"].as_str().unwrap());
        assert!(saved_path.is_absolute());
        assert_eq!(std::fs::read(&saved_path).unwrap(), png_bytes);

        // Non-image PDF attachment download (infers mime_type from extension when octet-stream)
        let resp_pdf = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 32,
                "method": "tools/call",
                "params": {
                    "name": "download_attachment",
                    "arguments": {
                        "message_id": "msg-media",
                        "attachment_index": 1,
                        "output_dir": tmp.path().to_string_lossy(),
                        "offline": true
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp_pdf["result"]["isError"], false, "{resp_pdf}");
        let pdf_payload: Value =
            serde_json::from_str(resp_pdf["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(pdf_payload["ok"], true);
        assert_eq!(pdf_payload["file_name"], "spec.pdf");
        assert_eq!(pdf_payload["mime_type"], "application/pdf");
        assert_eq!(pdf_payload["size_bytes"], pdf_bytes.len() as u64);
        assert_eq!(pdf_payload["is_image"], false);
        assert!(PathBuf::from(pdf_payload["path"].as_str().unwrap()).is_absolute());
    }

    #[test]
    fn compact_read_tools_reject_unknown_args_and_accept_documented_args() {
        let err = validate_tool_args("list_messages", &json!({ "folder": "inbox", "full": true }))
            .expect_err("list_messages must reject unknown `full` argument");
        assert_eq!(err, "unsupported argument for list_messages: full");

        assert!(validate_tool_args(
            "list_messages",
            &json!({
                "folder": "inbox",
                "limit": 20,
                "cursor": "o:abc12345:10",
                "unread": true,
                "flagged": false,
                "focused": true,
                "compact": false,
                "offline": true
            })
        )
        .is_ok());

        assert_eq!(
            validate_tool_args(
                "get_message",
                &json!({ "message_id": "m1", "full": true })
            )
            .unwrap_err(),
            "unsupported argument for get_message: full"
        );
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1",
                "mark_read": true,
                "compact": false,
                "offline": false
            })
        )
        .is_ok());

        assert_eq!(
            validate_tool_args("search_mail", &json!({ "query": "Q4", "full": true })).unwrap_err(),
            "unsupported argument for search_mail: full"
        );
        assert!(validate_tool_args(
            "search_mail",
            &json!({
                "query": "Q4",
                "folder": "inbox",
                "limit": 25,
                "cursor": "g:12345678:0|https://graph.microsoft.com/v1.0/me/messages",
                "compact": false,
                "offline": false
            })
        )
        .is_ok());

        assert_eq!(
            validate_tool_args("list_events", &json!({ "full": true })).unwrap_err(),
            "unsupported argument for list_events: full"
        );
        assert!(validate_tool_args(
            "list_events",
            &json!({
                "from": "2026-11-20",
                "to": "2026-11-27",
                "days": 7,
                "limit": 50,
                "compact": false,
                "offline": true
            })
        )
        .is_ok());
        assert!(validate_tool_args(
            "list_events",
            &json!({
                "start": "2026-11-20",
                "end": "2026-11-27",
                "compact": true
            })
        )
        .is_ok());

        // get_message and get_event selective `fields` schema validation
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1",
                "fields": ["cc", "bcc", "categories", "attachments"]
            })
        )
        .is_ok());
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1",
                "fields": "cc"
            })
        )
        .is_err());
        assert!(validate_tool_args(
            "get_event",
            &json!({
                "event_id": "ev1",
                "fields": ["attendees", "body_plain", "recurrence", "categories"],
                "offline": true
            })
        )
        .is_ok());
        assert_eq!(
            validate_tool_args("get_event", &json!({ "event_id": "ev1", "full": true }))
                .unwrap_err(),
            "unsupported argument for get_event: full"
        );
    }

    #[tokio::test]
    async fn compact_list_and_search_emit_honest_omitted_metadata_and_hint() {
        use axum::{routing::get, Json, Router};
        use crate::{
            auth::StoredTokens,
            models::{CalendarEvent, EmailRecipient, EventAttendee, MailAttachment},
        };

        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "folder-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 1,
            }])
            .unwrap();
        storage
            .upsert_mail_messages(&[MailMessage {
                id: "msg-rich".to_string(),
                internet_message_id: Some("<rich@contoso.com>".to_string()),
                conversation_id: Some("conv-rich".to_string()),
                folder_id: Some("folder-inbox".to_string()),
                subject: "Q4 Planning".to_string(),
                from: Some(EmailRecipient {
                    name: Some("CEO".to_string()),
                    address: "ceo@contoso.com".to_string(),
                }),
                sender: Some(EmailRecipient {
                    name: Some("EA".to_string()),
                    address: "ea@contoso.com".to_string(),
                }),
                to_recipients: vec![EmailRecipient {
                    name: Some("Alice".to_string()),
                    address: "alice@contoso.com".to_string(),
                }],
                cc_recipients: vec![EmailRecipient {
                    name: Some("Bob".to_string()),
                    address: "bob@contoso.com".to_string(),
                }],
                bcc_recipients: vec![EmailRecipient {
                    name: None,
                    address: "audit@contoso.com".to_string(),
                }],
                reply_to: vec![EmailRecipient {
                    name: None,
                    address: "reply@contoso.com".to_string(),
                }],
                received_at: "2026-11-20T09:00:00Z".to_string(),
                sent_at: Some("2026-11-20T08:59:00Z".to_string()),
                last_modified_at: Some("2026-11-20T09:05:00Z".to_string()),
                is_read: false,
                is_draft: false,
                flagged: true,
                importance: "high".to_string(),
                inference_classification: Some("focused".to_string()),
                categories: vec!["Executive".to_string(), "Finance".to_string()],
                has_attachments: true,
                body_loaded: true,
                preview: "See attached deck".to_string(),
                body_html: "<html><body><p>Huge HTML body</p></body></html>".to_string(),
                body_plain: "Huge HTML body".to_string(),
                attachments: vec![MailAttachment {
                    id: "att-1".to_string(),
                    name: "deck.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    size: 2048,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: Some("cGRm".to_string()),
                }],
                meeting_request: None,
                web_link: Some("https://outlook.office.com/mail/msg-rich".to_string()),
            }])
            .unwrap();

        storage
            .upsert_calendar_events(&[CalendarEvent {
                id: "ev-series".to_string(),
                subject: "Weekly Architecture Sync".to_string(),
                start_iso: "2026-11-20T15:00:00Z".to_string(),
                end_iso: "2026-11-20T16:00:00Z".to_string(),
                time_zone: "UTC".to_string(),
                is_all_day: false,
                location: Some("Room 42".to_string()),
                organizer_name: Some("Alice".to_string()),
                organizer_email: Some("alice@contoso.com".to_string()),
                attendees: vec![EventAttendee {
                    name: Some("Bob".to_string()),
                    address: "bob@contoso.com".to_string(),
                    attendee_type: "required".to_string(),
                    response_status: "accepted".to_string(),
                }],
                response_status: "organizer".to_string(),
                is_online_meeting: true,
                online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/sync".to_string()),
                body_html: "<div><p>Agenda HTML</p></div>".to_string(),
                body_plain: "Agenda HTML".to_string(),
                is_cancelled: false,
                web_link: Some("https://outlook.office.com/calendar/ev-series".to_string()),
                categories: vec!["Engineering".to_string()],
                show_as: Some("busy".to_string()),
                sensitivity: Some("normal".to_string()),
                event_type: Some("seriesMaster".to_string()),
                has_attachments: true,
                is_recurring: true,
                recurrence: Some(json!({
                    "pattern": { "type": "weekly", "interval": 1, "daysOfWeek": ["friday"] },
                    "range": { "type": "noEnd", "startDate": "2026-11-20" }
                })),
            }])
            .unwrap();

        // 1. Compact list_messages includes honest `omitted.fields` and `hint`; compact:false omits them.
        let list_compact = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_messages",
            &json!({ "folder": "inbox", "offline": true }),
        )
        .await
        .unwrap();
        let omitted_fields = list_compact["omitted"]["fields"].as_array().unwrap();
        for expected in [
            "to_recipients",
            "cc_recipients",
            "bcc_recipients",
            "reply_to",
            "sender",
            "preview",
            "body_plain",
            "importance",
            "categories",
            "meeting_request",
            "attachments",
        ] {
            assert!(
                omitted_fields.iter().any(|v| v == expected),
                "list_messages omitted.fields missing {expected}: {omitted_fields:?}"
            );
        }
        let hint = list_compact["hint"].as_str().unwrap();
        assert!(hint.contains("get_message") && hint.contains("NOT absent"), "{hint}");

        let list_full = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_messages",
            &json!({ "folder": "inbox", "offline": true, "compact": false }),
        )
        .await
        .unwrap();
        assert!(list_full.get("omitted").is_none());
        assert!(list_full.get("hint").is_none());
        assert_eq!(list_full["messages"][0]["categories"], json!(["Executive", "Finance"]));
        assert_eq!(list_full["messages"][0]["sender"]["address"], "ea@contoso.com");

        // 2. Compact list_events emits omitted.fields (body_html, recurrence) and hint; strips recurrence on list items.
        let ev_list_compact = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_events",
            &json!({ "from": "2026-11-19", "to": "2026-11-22", "offline": true }),
        )
        .await
        .unwrap();
        assert_eq!(
            ev_list_compact["omitted"]["fields"],
            json!(["body_html", "recurrence"])
        );
        let ev_hint = ev_list_compact["hint"].as_str().unwrap();
        assert!(ev_hint.contains("get_event") && ev_hint.contains("NOT absent"), "{ev_hint}");
        assert!(ev_list_compact["events"][0].get("body_html").is_none());
        assert!(ev_list_compact["events"][0].get("recurrence").is_none());
        assert_eq!(ev_list_compact["events"][0]["is_recurring"], true);
        assert_eq!(ev_list_compact["events"][0]["categories"], json!(["Engineering"]));
        assert_eq!(ev_list_compact["events"][0]["show_as"], "busy");
        assert_eq!(ev_list_compact["events"][0]["sensitivity"], "normal");
        assert_eq!(ev_list_compact["events"][0]["event_type"], "seriesMaster");
        assert_eq!(ev_list_compact["events"][0]["has_attachments"], true);

        let ev_list_full = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_events",
            &json!({ "from": "2026-11-19", "to": "2026-11-22", "offline": true, "compact": false }),
        )
        .await
        .unwrap();
        assert!(ev_list_full.get("omitted").is_none());
        assert_eq!(ev_list_full["events"][0]["body_html"], "<div><p>Agenda HTML</p></div>");
        assert_eq!(
            ev_list_full["events"][0]["recurrence"]["pattern"]["type"],
            "weekly"
        );

        // 3. Selective `fields` projection on get_message (aliases cc/bcc + canonical categories/attachments + missing_fields).
        let msg_proj = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_message",
            &json!({
                "message_id": "msg-rich",
                "offline": true,
                "fields": ["cc", "bcc", "categories", "sender", "attachments", "not_a_real_field"]
            }),
        )
        .await
        .unwrap();
        assert_eq!(msg_proj["message"]["id"], "msg-rich");
        assert_eq!(msg_proj["message"]["cc_recipients"][0]["address"], "bob@contoso.com");
        assert_eq!(msg_proj["message"]["bcc_recipients"][0]["address"], "audit@contoso.com");
        assert_eq!(msg_proj["message"]["categories"], json!(["Executive", "Finance"]));
        assert_eq!(msg_proj["message"]["sender"]["address"], "ea@contoso.com");
        assert_eq!(msg_proj["message"]["attachments"][0]["name"], "deck.pdf");
        assert_eq!(msg_proj["message"]["attachments"][0]["content_cached"], true);
        assert!(msg_proj["message"]["attachments"][0].get("content_bytes_b64").is_none());
        assert!(msg_proj["message"].get("body_html").is_none());
        assert!(msg_proj["message"].get("body_plain").is_none());
        assert_eq!(msg_proj["missing_fields"], json!(["not_a_real_field"]));

        // 4. Selective `fields` projection on get_event + compact/full get_event.
        let ev_proj = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_event",
            &json!({
                "event_id": "ev-series",
                "offline": true,
                "fields": ["attendees", "body_plain", "recurrence", "categories", "show_as", "bogus_key"]
            }),
        )
        .await
        .unwrap();
        assert_eq!(ev_proj["event"]["id"], "ev-series");
        assert_eq!(ev_proj["event"]["attendees"][0]["address"], "bob@contoso.com");
        assert_eq!(ev_proj["event"]["body_plain"], "Agenda HTML");
        assert_eq!(ev_proj["event"]["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(ev_proj["event"]["categories"], json!(["Engineering"]));
        assert_eq!(ev_proj["event"]["show_as"], "busy");
        assert!(ev_proj["event"].get("body_html").is_none());
        assert!(ev_proj["event"].get("subject").is_none());
        assert_eq!(ev_proj["missing_fields"], json!(["bogus_key"]));

        let ev_compact = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_event",
            &json!({ "event_id": "ev-series", "offline": true }),
        )
        .await
        .unwrap();
        assert!(ev_compact["event"].get("body_html").is_none());
        assert_eq!(ev_compact["event"]["omitted"]["fields"], json!(["body_html"]));
        assert_eq!(ev_compact["event"]["recurrence"]["pattern"]["type"], "weekly");

        // 5. Online search_mail (compact: true) also emits honest `omitted` and `hint`.
        let router = Router::new().route(
            "/v1.0/me/messages",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "msg-search-1",
                        "subject": "Budget Review",
                        "from": { "emailAddress": { "name": "Alice", "address": "alice@contoso.com" } },
                        "sender": { "emailAddress": { "name": "Mailer", "address": "mailer@contoso.com" } },
                        "receivedDateTime": "2026-11-20T10:00:00Z",
                        "isRead": false,
                        "isDraft": false,
                        "categories": ["Budget"],
                        "hasAttachments": false,
                        "bodyPreview": "Numbers inside"
                    }]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let mut online_cfg = AppConfig::default();
        online_cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: Some("tester@contoso.com".to_string()),
            auth_mode: "token".to_string(),
        };
        let online_auth = AuthManager::from_tokens(online_cfg.clone(), tokens);
        let online_graph = GraphClient::new(online_cfg.clone(), online_auth).unwrap();

        let search_compact = call_tool(
            &online_cfg,
            &online_graph,
            &storage,
            "search_mail",
            &json!({ "query": "Budget", "offline": false }),
        )
        .await
        .unwrap();
        assert_eq!(search_compact["messages"].as_array().unwrap().len(), 1);
        assert!(search_compact["omitted"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "categories"));
        assert!(search_compact["hint"]
            .as_str()
            .unwrap()
            .contains("get_message"));

        let search_full = call_tool(
            &online_cfg,
            &online_graph,
            &storage,
            "search_mail",
            &json!({ "query": "Budget", "offline": false, "compact": false }),
        )
        .await
        .unwrap();
        assert!(search_full.get("omitted").is_none());
        assert_eq!(search_full["messages"][0]["categories"], json!(["Budget"]));
        assert_eq!(
            search_full["messages"][0]["sender"]["address"],
            "mailer@contoso.com"
        );

        server.abort();
    }
}
