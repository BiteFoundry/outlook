//! Mail MCP tool handlers and response projection helpers.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    models::OutgoingMail,
    resolver,
    storage::Storage,
};

use super::{
    args::{bounds, Args},
    format_error, map_graph_result, resolve_mail_folder_mcp, with_storage,
};

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

pub(super) async fn handle_list_mail_folders(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_list_messages(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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
            cli::CursorPos::Offset(_) => {
                return Err("invalid cursor: not valid for this command".to_string());
            }
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

pub(super) async fn handle_get_message(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_send_mail(
    graph: &GraphClient,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_reply_mail(
    graph: &GraphClient,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_forward_mail(
    graph: &GraphClient,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_update_draft(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_update_message(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

    if is_read.is_some() || flagged.is_some() {
        map_graph_result(
            graph,
            graph.update_mail_flags(&message_id, is_read, flagged).await,
        )
        .await?;
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

pub(super) async fn handle_delete_message(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_download_attachment(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_search_mail(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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
            return Err("invalid cursor: not valid for this command".to_string());
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
