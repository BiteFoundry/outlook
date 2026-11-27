//! Mail folder, listing, reading, composing, mutation, attachment download, and search commands.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    config::AppConfig,
    graph::GraphClient,
    img_preview,
    models::{InlineImageUpload, MailFolder, MailMessage, OutgoingMail},
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    storage::Storage,
};

use super::{
    args::DELETE_JSON_REQUIRES_YES,
    format::{
        clamp_limit, cursor_scope, delete_gate, format_downloaded_attachment_json,
        image_mime_for_path, load_mail_attachment_bytes, mail_cursor_scope,
        normalize_mail_importance, offset_page, prompt, select_mail_attachment,
        truncate_single_line, wrap_graph_permission_error, CursorPos, PageCursor,
        FILTER_SCAN_MAX_PAGES,
    },
    ops::{default_preview_dimensions, print_rendered_preview},
    runtime::{
        check_recipient_addresses, cli_cache_ttl_secs, is_cursor_fresh, open_runtime,
        open_storage_only, record_cli_sync_cursor, resolve_daemon_warm,
    },
};

pub(super) fn format_mail_folders_lines(folders: &[MailFolder]) -> Vec<String> {
    let paths = crate::ui::nav_pane::folder_display_paths(folders);
    let mut ordered: Vec<(&MailFolder, String)> = folders
        .iter()
        .map(|f| {
            let path = paths
                .get(&f.id)
                .cloned()
                .unwrap_or_else(|| f.display_name.clone());
            (f, path)
        })
        .collect();
    ordered.sort_by(|a, b| {
        a.1.to_ascii_lowercase()
            .cmp(&b.1.to_ascii_lowercase())
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    ordered
        .into_iter()
        .map(|(f, path)| {
            let wk = f
                .well_known_name
                .as_deref()
                .map(|w| format!(" [{}]", sanitize_terminal_inline(w)))
                .unwrap_or_default();
            format!(
                "{}{}  unread={}/{}  ({})",
                sanitize_terminal_inline(&path),
                wk,
                f.unread_item_count,
                f.total_item_count,
                sanitize_terminal_inline(&f.id)
            )
        })
        .collect()
}

pub(super) fn print_mail_folders_output(
    folders: &[MailFolder],
    json_out: bool,
    limit: Option<usize>,
    offset: usize,
) -> Result<()> {
    if json_out {
        let lim = limit.map(clamp_limit).unwrap_or(50);
        let scope = cursor_scope(&["folders"]);
        let (page, has_more, next_cursor) = offset_page(folders, offset, lim, &scope);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "folders": page,
                "returned": page.len(),
                "has_more": has_more,
                "next_cursor": next_cursor,
            }))?
        );
    } else if folders.is_empty() {
        println!("No mail folders found.");
    } else {
        let lines = format_mail_folders_lines(folders);
        let take_n = limit.map(clamp_limit).unwrap_or(lines.len());
        for line in lines.into_iter().take(take_n) {
            println!("{line}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_folders(
    cfg: &AppConfig,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    limit: Option<usize>,
    cursor: Option<&str>,
) -> Result<()> {
    if let Some(raw_cursor) = cursor {
        let parsed = PageCursor::parse(raw_cursor)?;
        let CursorPos::Offset(offset) = parsed.pos else {
            bail!("invalid cursor: not valid for this command");
        };
        parsed.require_scope(&cursor_scope(&["folders"]))?;
        let storage = open_storage_only(cfg)?;
        let folders = storage.list_mail_folders()?;
        return print_mail_folders_output(&folders, json_out, limit, offset);
    }

    if offline {
        let storage = open_storage_only(cfg)?;
        let folders = storage.list_mail_folders()?;
        return print_mail_folders_output(&folders, json_out, limit, 0);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:mail_folders"], ttl) {
                let folders = storage.list_mail_folders()?;
                if !folders.is_empty() {
                    return print_mail_folders_output(&folders, json_out, limit, 0);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let cached_folders = storage.list_mail_folders()?;
    let should_fetch = refresh
        || cached_folders.is_empty()
        || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &["cli:mail_folders"], ttl));
    let folders = if should_fetch {
        let (fetched, complete) = match graph.list_mail_folders_complete().await {
            Ok(items) => items,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        storage.store_mail_folder_listing(&fetched, complete)?;
        if complete {
            // A partial listing leaves the cursor stale so the next call retries.
            record_cli_sync_cursor(&storage, "cli:mail_folders");
        }
        storage.list_mail_folders()?
    } else {
        cached_folders
    };

    print_mail_folders_output(&folders, json_out, limit, 0)
}

pub(crate) struct MailListPage {
    pub messages: Vec<MailMessage>,
    pub has_more: Option<bool>,
    pub next_cursor: Option<String>,
    /// `false` when this listing ran a first folder sync that stopped at the interactive page budget.
    pub sync_complete: bool,
}

/// Adds `"sync_complete": false` to a mail list response whose folder is still being bootstrapped
/// (see `sync::sync_mail_folder_for_listing`). The key is absent in every other case.
pub(crate) fn with_sync_complete_marker(mut out: serde_json::Value, sync_complete: bool) -> serde_json::Value {
    if !sync_complete {
        out["sync_complete"] = json!(false);
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn load_cached_mail_list(
    storage: &Storage,
    folder_id: &str,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
    full: bool,
    before: Option<(&str, &str)>,
    scope: &str,
) -> Result<MailListPage> {
    let fetch_limit = limit.saturating_add(1);
    let mut messages = if full {
        storage.list_mail_messages_before(
            Some(folder_id),
            unread_only,
            flagged_only,
            focused_only,
            fetch_limit,
            before,
        )?
    } else {
        storage.list_mail_summaries_before(
            Some(folder_id),
            unread_only,
            flagged_only,
            focused_only,
            fetch_limit,
            before,
        )?
    };
    let has_more = messages.len() > limit;
    if has_more {
        messages.truncate(limit);
    }
    let next_cursor = if has_more {
        messages
            .last()
            .map(|last| format!("c:{scope}:{}|{}", last.received_at, last.id))
    } else {
        None
    };
    Ok(MailListPage {
        messages,
        has_more: Some(has_more),
        next_cursor,
        sync_complete: true,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum Surface {
    Cli,
    Mcp,
}

pub(crate) fn mail_attachments_public_json(
    attachments: &[crate::models::MailAttachment],
) -> serde_json::Value {
    serde_json::Value::Array(
        attachments
            .iter()
            .map(|src| {
                let mut val = json!(src);
                if let Some(obj) = val.as_object_mut() {
                    obj.remove("content_bytes_b64");
                    let cached = src
                        .content_bytes_b64
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty());
                    obj.insert("content_cached".into(), json!(cached));
                }
                val
            })
            .collect(),
    )
}

/// Full message JSON for CLI/MCP output. Attachment bytes are omitted.
pub(crate) fn mail_message_public_json(msg: &MailMessage) -> serde_json::Value {
    let mut value = json!(msg);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "attachments".into(),
            mail_attachments_public_json(&msg.attachments),
        );
    }
    value
}

pub(crate) fn mail_list_json(messages: &[MailMessage], compact: bool) -> serde_json::Value {
    if !compact {
        return serde_json::Value::Array(
            messages.iter().map(mail_message_public_json).collect(),
        );
    }
    serde_json::Value::Array(
        messages
            .iter()
            .map(|msg| {
                json!({
                    "id": msg.id,
                    "subject": msg.subject,
                    "from": msg.from,
                    "received_at": msg.received_at,
                    "is_read": msg.is_read,
                    "is_draft": msg.is_draft,
                    "flagged": msg.flagged,
                    "has_attachments": msg.has_attachments,
                })
            })
            .collect(),
    )
}

pub(crate) fn mail_read_json(
    msg: &MailMessage,
    compact: bool,
    surface: Surface,
) -> serde_json::Value {
    if !compact {
        return json!({ "message": mail_message_public_json(msg) });
    }
    let omitted_attachments: Option<usize> = if msg.body_loaded {
        Some(msg.attachments.len())
    } else if !msg.has_attachments {
        Some(0)
    } else {
        None
    };
    let hint = match surface {
        Surface::Cli => format!("outlook read {} --json --full", msg.id),
        Surface::Mcp => "Call get_message with compact:false for body_html and attachment metadata; use download_attachment for attachment bytes.".to_string(),
    };
    let mut message_obj = json!({
        "id": msg.id,
        "conversation_id": msg.conversation_id,
        "folder_id": msg.folder_id,
        "subject": msg.subject,
        "from": msg.from,
        "to_recipients": msg.to_recipients,
        "cc_recipients": msg.cc_recipients,
        "bcc_recipients": msg.bcc_recipients,
        "reply_to": msg.reply_to,
        "received_at": msg.received_at,
        "sent_at": msg.sent_at,
        "is_read": msg.is_read,
        "is_draft": msg.is_draft,
        "flagged": msg.flagged,
        "importance": msg.importance,
        "has_attachments": msg.has_attachments,
        "meeting_request": msg.meeting_request,
        "body_loaded": msg.body_loaded,
        "preview": msg.preview,
        "body_plain": if msg.body_loaded { json!(msg.body_plain) } else { serde_json::Value::Null },
        "omitted": {
            "attachments": omitted_attachments,
            "fields": [
                "body_html",
                "internet_message_id",
                "inference_classification",
                "last_modified_at",
                "web_link"
            ],
        },
        "hint": hint,
    });
    if let Some(obj) = message_obj.as_object_mut() {
        if let Some(ref sender) = msg.sender {
            obj.insert("sender".into(), json!(sender));
        }
        if !msg.categories.is_empty() {
            obj.insert("categories".into(), json!(msg.categories));
        }
    }
    json!({ "message": message_obj })
}

pub(super) fn print_mail_list_output(
    folder: &str,
    folder_id: &str,
    page: MailListPage,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&with_sync_complete_marker(
                json!({
                    "folder": folder_id,
                    "messages": mail_list_json(&page.messages, compact),
                    "returned": page.messages.len(),
                    "has_more": page.has_more,
                    "next_cursor": page.next_cursor,
                }),
                page.sync_complete,
            ))?
        );
    } else if page.messages.is_empty() {
        println!("No messages found in '{}'.", sanitize_terminal_inline(folder));
    } else {
        for m in &page.messages {
            let unread_mark = if m.is_read { " " } else { "*" };
            let flag_mark = if m.flagged { "!" } else { " " };
            let att_mark = if m.has_attachments { "@" } else { " " };
            println!(
                "{unread_mark}{flag_mark}{att_mark} [{}] {} | {} ({})",
                sanitize_terminal_inline(&m.received_at),
                sanitize_terminal_inline(&m.from_display()),
                truncate_single_line(m.effective_subject(), 60),
                sanitize_terminal_inline(&m.id)
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_mail_list(
    cfg: &AppConfig,
    folder: &str,
    limit: usize,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
    cursor: Option<&str>,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let has_filter = unread_only || flagged_only || focused_only;
    let full = json_out && !compact;

    if let Some(raw_cursor) = cursor {
        let parsed = PageCursor::parse(raw_cursor)?;
        if matches!(parsed.pos, CursorPos::Offset(_)) {
            bail!("invalid cursor: not valid for this command");
        }
        let storage = open_storage_only(cfg)?;
        let resolved = resolver::resolve_mail_folder_cached(&storage, folder)?;
        let scope = mail_cursor_scope(&resolved.id, unread_only, flagged_only, focused_only);
        parsed.require_scope(&scope)?;
        match parsed.pos {
            CursorPos::Cache { received_at, id } => {
                let page = load_cached_mail_list(
                    &storage,
                    &resolved.id,
                    unread_only,
                    flagged_only,
                    focused_only,
                    limit,
                    full,
                    Some((&received_at, &id)),
                    &scope,
                )?;
                return print_mail_list_output(folder, &resolved.id, page, json_out, compact);
            }
            CursorPos::Graph { skip, url } => {
                if offline {
                    bail!("invalid cursor: this cursor needs online access");
                }
                if !crate::graph::client::graph_base_owns_url(&cfg.graph_base_url, &url) {
                    bail!("invalid cursor: url is not a Graph URL for this account");
                }
                drop(storage);
                let (_auth, graph, storage) = open_runtime(cfg, true).await?;
                let filtered = match graph
                    .list_mail_messages_filtered_paged(
                        &resolved.id,
                        unread_only,
                        flagged_only,
                        focused_only,
                        limit,
                        true,
                        &scope,
                        Some((skip, url)),
                        Some(FILTER_SCAN_MAX_PAGES),
                    )
                    .await
                {
                    Ok(res) => res,
                    Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
                };
                let _ = storage.upsert_mail_messages(&filtered.fetched);
                let messages = if full {
                    filtered
                        .matched
                        .into_iter()
                        .map(|m| {
                            storage
                                .get_mail_message(&m.id)
                                .ok()
                                .flatten()
                                .unwrap_or(m)
                        })
                        .collect()
                } else {
                    filtered.matched
                };
                let page = MailListPage {
                    messages,
                    has_more: filtered.has_more,
                    next_cursor: filtered.next_cursor,
                    sync_complete: true,
                };
                return print_mail_list_output(folder, &resolved.id, page, json_out, compact);
            }
            CursorPos::Offset(_) => bail!("invalid cursor: not valid for this command"),
        }
    }

    if offline {
        let storage = open_storage_only(cfg)?;
        let resolved = resolver::resolve_mail_folder_cached(&storage, folder)?;
        let scope = mail_cursor_scope(&resolved.id, unread_only, flagged_only, focused_only);
        let page = load_cached_mail_list(
            &storage,
            &resolved.id,
            unread_only,
            flagged_only,
            focused_only,
            limit,
            full,
            None,
            &scope,
        )?;
        return print_mail_list_output(folder, &resolved.id, page, json_out, compact);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(resolved) = resolver::resolve_mail_folder_cached(&storage, folder) {
                let cli_key = format!("cli:mail:{}", resolved.id);
                let delta_key = format!("mail_delta:{}", resolved.id);
                if cached
                    || daemon_warm
                    || is_cursor_fresh(&storage, &[cli_key.as_str(), delta_key.as_str()], ttl)
                {
                    let has_any_cached =
                        !storage.list_mail_summaries(Some(&resolved.id), 1)?.is_empty();
                    if has_any_cached {
                        let scope = mail_cursor_scope(
                            &resolved.id,
                            unread_only,
                            flagged_only,
                            focused_only,
                        );
                        let page = load_cached_mail_list(
                            &storage,
                            &resolved.id,
                            unread_only,
                            flagged_only,
                            focused_only,
                            limit,
                            full,
                            None,
                            &scope,
                        )?;
                        return print_mail_list_output(
                            folder,
                            &resolved.id,
                            page,
                            json_out,
                            compact,
                        );
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let mut resolved = match resolver::resolve_mail_folder(&graph, &storage, folder, false).await {
        Ok(f) => f,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };

    let has_any_cached = !storage.list_mail_summaries(Some(&resolved.id), 1)?.is_empty();
    let cli_key = format!("cli:mail:{}", resolved.id);
    let delta_key = format!("mail_delta:{}", resolved.id);
    let should_fetch = refresh
        || !has_any_cached
        || (!cached
            && !daemon_warm
            && !is_cursor_fresh(&storage, &[cli_key.as_str(), delta_key.as_str()], ttl));
    let page = if should_fetch {
        if has_filter {
            let scope = mail_cursor_scope(&resolved.id, unread_only, flagged_only, focused_only);
            let filtered = match graph
                .list_mail_messages_filtered_paged(
                    &resolved.id,
                    unread_only,
                    flagged_only,
                    focused_only,
                    limit,
                    true,
                    &scope,
                    None,
                    Some(FILTER_SCAN_MAX_PAGES),
                )
                .await
            {
                Ok(res) => res,
                Err(err) if resolver::is_graph_not_found_error(&err) => {
                    match resolver::refresh_mail_folder_after_not_found(
                        &graph,
                        &storage,
                        folder,
                        &resolved.id,
                    )
                    .await
                    {
                        Ok(Some(new_folder)) => {
                            resolved = new_folder;
                            let new_scope = mail_cursor_scope(
                                &resolved.id,
                                unread_only,
                                flagged_only,
                                focused_only,
                            );
                            match graph
                                .list_mail_messages_filtered_paged(
                                    &resolved.id,
                                    unread_only,
                                    flagged_only,
                                    focused_only,
                                    limit,
                                    true,
                                    &new_scope,
                                    None,
                                    Some(FILTER_SCAN_MAX_PAGES),
                                )
                                .await
                            {
                                Ok(res) => res,
                                Err(retry_err) => {
                                    return Err(wrap_graph_permission_error(&graph, retry_err).await);
                                }
                            }
                        }
                        Ok(None) => return Err(wrap_graph_permission_error(&graph, err).await),
                        Err(refresh_err) => {
                            return Err(wrap_graph_permission_error(&graph, refresh_err).await);
                        }
                    }
                }
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            let _ = storage.upsert_mail_messages(&filtered.fetched);
            let messages = if full {
                filtered
                    .matched
                    .into_iter()
                    .map(|m| {
                        storage
                            .get_mail_message(&m.id)
                            .ok()
                            .flatten()
                            .unwrap_or(m)
                    })
                    .collect()
            } else {
                filtered.matched
            };
            MailListPage {
                messages,
                has_more: filtered.has_more,
                next_cursor: filtered.next_cursor,
                sync_complete: true,
            }
        } else {
            let sync_res = match crate::sync::sync_mail_folder_for_listing(
                &resolved.id,
                &graph,
                &storage,
                limit,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(err) if resolver::is_graph_not_found_error(&err) => {
                    match resolver::refresh_mail_folder_after_not_found(
                        &graph,
                        &storage,
                        folder,
                        &resolved.id,
                    )
                    .await
                    {
                        Ok(Some(new_folder)) => {
                            resolved = new_folder;
                            match crate::sync::sync_mail_folder_for_listing(
                                &resolved.id,
                                &graph,
                                &storage,
                                limit,
                            )
                            .await
                            {
                                Ok(outcome) => outcome,
                                Err(retry_err) => {
                                    return Err(wrap_graph_permission_error(&graph, retry_err).await);
                                }
                            }
                        }
                        Ok(None) => return Err(wrap_graph_permission_error(&graph, err).await),
                        Err(refresh_err) => {
                            return Err(wrap_graph_permission_error(&graph, refresh_err).await);
                        }
                    }
                }
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            record_cli_sync_cursor(&storage, &format!("cli:mail:{}", sync_res.folder_id));
            let scope = mail_cursor_scope(&sync_res.folder_id, false, false, false);
            let mut page = load_cached_mail_list(
                &storage,
                &sync_res.folder_id,
                false,
                false,
                false,
                limit,
                full,
                None,
                &scope,
            )?;
            page.sync_complete = sync_res.complete;
            page
        }
    } else {
        let scope = mail_cursor_scope(&resolved.id, unread_only, flagged_only, focused_only);
        load_cached_mail_list(
            &storage,
            &resolved.id,
            unread_only,
            flagged_only,
            focused_only,
            limit,
            full,
            None,
            &scope,
        )?
    };

    print_mail_list_output(folder, &resolved.id, page, json_out, compact)
}

pub(super) fn print_mail_read_text(msg: &MailMessage, raw_html: bool) {
    println!("Subject: {}", sanitize_terminal_inline(msg.effective_subject()));
    println!("From: {}", sanitize_terminal_inline(&msg.from_display()));
    if !msg.to_recipients.is_empty() {
        let to_list: Vec<String> = msg
            .to_recipients
            .iter()
            .map(|r| sanitize_terminal_inline(&r.display()).into_owned())
            .collect();
        println!("To: {}", to_list.join(", "));
    }
    if !msg.cc_recipients.is_empty() {
        let cc_list: Vec<String> = msg
            .cc_recipients
            .iter()
            .map(|r| sanitize_terminal_inline(&r.display()).into_owned())
            .collect();
        println!("Cc: {}", cc_list.join(", "));
    }
    if !msg.bcc_recipients.is_empty() {
        let bcc_list: Vec<String> = msg
            .bcc_recipients
            .iter()
            .map(|r| sanitize_terminal_inline(&r.display()).into_owned())
            .collect();
        println!("Bcc: {}", bcc_list.join(", "));
    }
    println!(
        "Date: {} | read={} | flagged={} | importance={}",
        sanitize_terminal_inline(&msg.received_at),
        msg.is_read,
        msg.flagged,
        sanitize_terminal_inline(&msg.importance)
    );
    if !msg.attachments.is_empty() {
        println!("Attachments ({}):", msg.attachments.len());
        for (idx, att) in msg.attachments.iter().enumerate() {
            println!(
                "  [{idx}] {} ({}, {} bytes, id={})",
                sanitize_terminal_inline(&att.name),
                sanitize_terminal_inline(&att.content_type),
                att.size,
                sanitize_terminal_inline(&att.id)
            );
        }
    }
    println!("---");
    if raw_html {
        println!("{}", sanitize_terminal_text(&msg.body_html));
    } else {
        let body = if !msg.body_plain.trim().is_empty() {
            msg.body_plain.trim().to_string()
        } else {
            let blocks = crate::parser::parse_message_blocks(&msg.body_html, &msg.attachments);
            crate::parser::blocks_to_plain(&blocks)
        };
        println!("{}", sanitize_terminal_text(&body));
    }
    if !msg.body_loaded {
        eprintln!(
            "note: body not loaded; showing preview only. Run 'outlook read {} --json --full' online to fetch it.",
            sanitize_terminal_inline(&msg.id)
        );
    }
}

pub(super) async fn load_mail_message(
    storage: &Storage,
    graph: &GraphClient,
    mail_id: &str,
    offline: bool,
) -> Result<MailMessage> {
    if offline {
        if let Some(exact) = storage.get_mail_message(mail_id)? {
            return Ok(exact);
        }
        let cached = storage.list_mail_messages(None, false, false, false, 2_000)?;
        return resolver::resolve_mail_message_from_slice(&cached, mail_id);
    }
    let msg = match graph.get_mail_message(mail_id).await {
        Ok(m) => m,
        Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
    };
    let _ = storage.upsert_mail_messages(std::slice::from_ref(&msg));
    Ok(msg)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_mail_read(
    cfg: &AppConfig,
    mail_id: &str,
    raw_html: bool,
    mark_read: bool,
    preview_images: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    if offline && !preview_images {
        let storage = open_storage_only(cfg)?;
        let msg = if let Some(exact) = storage.get_mail_message(mail_id)? {
            exact
        } else {
            let all = storage.list_mail_messages(None, false, false, false, 2_000)?;
            resolver::resolve_mail_message_from_slice(&all, mail_id)?
        };
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&mail_read_json(&msg, compact, Surface::Cli))?
            );
        } else {
            print_mail_read_text(&msg, raw_html);
        }
        return Ok(());
    }

    if !offline && cached && !mark_read && !preview_images {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Some(msg) = storage.get_mail_message(mail_id)? {
                if msg.body_loaded {
                    if json_out {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&mail_read_json(
                                &msg,
                                compact,
                                Surface::Cli
                            ))?
                        );
                    } else {
                        print_mail_read_text(&msg, raw_html);
                    }
                    return Ok(());
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mut msg = load_mail_message(&storage, &graph, mail_id, offline).await?;

    if !offline && mark_read && !msg.is_read {
        if let Err(err) = graph.set_mail_read(&msg.id, true).await {
            return Err(wrap_graph_permission_error(&graph, err).await);
        }
        msg.is_read = true;
        let _ = storage.set_mail_read_local(&msg.id, true);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&mail_read_json(&msg, compact, Surface::Cli))?
        );
        return Ok(());
    }

    print_mail_read_text(&msg, raw_html);

    if preview_images {
        let (cols, rows) = default_preview_dimensions(None, None);
        for att in &msg.attachments {
            if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                match load_mail_attachment_bytes(&graph, &msg.id, att, offline).await {
                    Ok((_name, _ct, bytes)) => {
                        match img_preview::render_ansi_stdout_with_pref(
                            &bytes,
                            cols,
                            rows,
                            cfg.image_protocol,
                        ) {
                            Ok(rendered) => {
                                println!("\n[Image preview: {}]", sanitize_terminal_inline(&att.name));
                                print_rendered_preview(&rendered)?;
                            }
                            Err(err) => {
                                eprintln!(
                                    "Image render failed ({}): {err:#}",
                                    sanitize_terminal_inline(&att.name)
                                );
                            }
                        }
                    }
                    Err(err) => eprintln!(
                        "Attachment fetch failed ({}): {err:#}",
                        sanitize_terminal_inline(&att.name)
                    ),
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_mail_send(
    cfg: &AppConfig,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: &str,
    body_opt: Option<String>,
    attach: Vec<PathBuf>,
    inline: Vec<PathBuf>,
    importance: &str,
    json_out: bool,
) -> Result<()> {
    check_recipient_addresses(&to)?;
    check_recipient_addresses(&cc)?;
    check_recipient_addresses(&bcc)?;
    let markdown_body = match body_opt {
        Some(b) => b,
        None => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("Failed to read email body from stdin")?;
            buf
        }
    };
    if markdown_body.trim().is_empty() {
        bail!("Email body is empty. Pass `--body <MD>` or pipe Markdown via stdin.");
    }
    let normalized_importance = normalize_mail_importance(importance)?;

    for path in &attach {
        if !path.exists() || !path.is_file() {
            bail!("Attachment file not found: {}", path.display());
        }
    }

    let mut inline_images = Vec::with_capacity(inline.len());
    for (idx, path) in inline.iter().enumerate() {
        if !path.exists() || !path.is_file() {
            bail!("Inline image file not found: {}", path.display());
        }
        let content_type = image_mime_for_path(path).with_context(|| {
            format!(
                "Unsupported inline image format (expected png/jpg/jpeg/gif/webp/bmp): {}",
                path.display()
            )
        })?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read inline image: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("inline.png")
            .to_string();
        let temp_id = format!("{}", idx + 1);
        let content_id = format!("inline-img-{}", idx + 1);
        inline_images.push(InlineImageUpload {
            temp_id,
            content_id,
            file_name,
            content_type: content_type.to_string(),
            bytes,
        });
    }

    let body_html = crate::parser::compile_outgoing_mail_html(&markdown_body, &inline_images, None);

    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let mail = OutgoingMail {
        subject: subject.to_string(),
        body_html,
        body_markdown: Some(markdown_body),
        to: to.clone(),
        cc,
        bcc,
        importance: Some(normalized_importance.to_string()),
        attachments: attach,
        inline_images,
        in_reply_to_id: None,
        reply_all: false,
        forward_of_id: None,
        draft_id: None,
        save_to_sent_items: true,
    };
    if let Err(err) = graph.send_mail(&mail).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "to": to,
                "subject": subject,
                "importance": normalized_importance,
            }))?
        );
    } else {
        println!("Email sent: '{}' -> {}", subject, to.join(", "));
    }
    Ok(())
}

pub(super) async fn cmd_mail_reply(
    cfg: &AppConfig,
    mail_id: &str,
    body_md: &str,
    reply_all: bool,
    json_out: bool,
) -> Result<()> {
    if body_md.trim().is_empty() {
        bail!("Reply body is empty.");
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let comment_html = crate::parser::compile_outgoing_mail_html(body_md, &[], None);
    if let Err(err) = graph.reply_mail(mail_id, &comment_html, reply_all).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
                "reply_all": reply_all,
            }))?
        );
    } else {
        println!(
            "{} sent for message {mail_id}.",
            if reply_all { "Reply All" } else { "Reply" }
        );
    }
    Ok(())
}

pub(super) async fn cmd_mail_forward(
    cfg: &AppConfig,
    mail_id: &str,
    to: Vec<String>,
    comment_md: &str,
    json_out: bool,
) -> Result<()> {
    if to.is_empty() {
        bail!("At least one recipient (--to) must be specified.");
    }
    check_recipient_addresses(&to)?;
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let comment_html = if comment_md.trim().is_empty() {
        String::new()
    } else {
        crate::parser::compile_outgoing_mail_html(comment_md, &[], None)
    };
    if let Err(err) = graph.forward_mail(mail_id, &to, &comment_html).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
                "to": to,
            }))?
        );
    } else {
        println!("Forwarded message {mail_id} -> {}", to.join(", "));
    }
    Ok(())
}

pub(crate) fn draft_edit_html(body: Option<&str>, html: Option<&str>) -> Option<String> {
    html.map(str::to_owned).or_else(|| {
        body.map(|markdown| crate::parser::compile_outgoing_mail_html(markdown, &[], None))
    })
}

pub(super) async fn cmd_edit_draft(
    cfg: &AppConfig,
    id: &str,
    subject: Option<&str>,
    body: Option<&str>,
    body_html: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let html = draft_edit_html(body, body_html);
    let preserve_quote = body.is_some();
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let quote_preserved = match graph
        .edit_mail_draft(id, subject, html.as_deref(), preserve_quote)
        .await
    {
        Ok(preserved) => preserved,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    if let Ok(updated) = graph.get_mail_message(id).await {
        let _ = storage.upsert_mail_messages(&[updated]);
    }
    if json_out {
        let mut payload = json!({ "ok": true, "message_id": id });
        if html.is_some() {
            payload["quote_preserved"] = json!(quote_preserved);
        }
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("Updated draft {id}.");
    }
    Ok(())
}

pub(super) async fn cmd_mail_mark(
    cfg: &AppConfig,
    mail_id: &str,
    is_read: Option<bool>,
    flagged: Option<bool>,
    json_out: bool,
) -> Result<()> {
    if is_read.is_none() && flagged.is_none() {
        bail!("Specify at least one of --read, --unread, --flag, or --unflag.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    if let Err(err) = graph.update_mail_flags(mail_id, is_read, flagged).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    if let Some(read_val) = is_read {
        let _ = storage.set_mail_read_local(mail_id, read_val);
    }
    if let Some(flag_val) = flagged {
        let _ = storage.set_mail_flagged_local(mail_id, flag_val);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
                "is_read": is_read,
                "flagged": flagged,
            }))?
        );
    } else {
        println!(
            "Updated message {mail_id}: is_read={:?}, flagged={:?}",
            is_read, flagged
        );
    }
    Ok(())
}

pub(super) async fn cmd_mail_move(
    cfg: &AppConfig,
    mail_id: &str,
    dest_folder: &str,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let mut folder = resolver::resolve_mail_folder(&graph, &storage, dest_folder, false).await?;
    let (moved, attachments_expanded) =
        match graph.move_mail_message_with_meta(mail_id, &folder.id).await {
            Ok(m) => m,
            Err(err) if resolver::is_graph_not_found_error(&err) => {
                match resolver::refresh_mail_folder_after_not_found(
                    &graph,
                    &storage,
                    dest_folder,
                    &folder.id,
                )
                .await
                {
                    Ok(Some(new_folder)) => {
                        folder = new_folder;
                        match graph.move_mail_message_with_meta(mail_id, &folder.id).await {
                            Ok(m) => m,
                            Err(retry_err) => {
                                return Err(wrap_graph_permission_error(&graph, retry_err).await);
                            }
                        }
                    }
                    Ok(None) => return Err(wrap_graph_permission_error(&graph, err).await),
                    Err(refresh_err) => {
                        return Err(wrap_graph_permission_error(&graph, refresh_err).await);
                    }
                }
            }
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
    let moved = storage.replace_moved_mail_local_ex(mail_id, &moved, attachments_expanded)?;

    if json_out {
        let payload = if full {
            json!({
                "ok": true,
                "destination_folder": folder,
                "message_id": moved.id,
                "previous_message_id": mail_id,
                "message": mail_message_public_json(&moved),
            })
        } else {
            json!({
                "ok": true,
                "destination_folder": folder,
                "message_id": moved.id,
                "previous_message_id": mail_id,
            })
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Moved message {} -> {} ({})",
            moved.id, folder.display_name, folder.id
        );
    }
    Ok(())
}

pub(super) async fn cmd_mail_delete(
    cfg: &AppConfig,
    mail_id: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete mail message {mail_id}? Type 'yes' to confirm: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    if let Err(err) = graph.delete_mail_message(mail_id).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.delete_mail_messages_by_ids(&[mail_id.to_string()]);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
            }))?
        );
    } else {
        println!("Deleted message: {mail_id}");
    }
    Ok(())
}

pub(super) async fn cmd_mail_download(
    cfg: &AppConfig,
    mail_id: &str,
    attachment_selector: Option<&str>,
    dir: Option<&Path>,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mail = load_mail_message(&storage, &graph, mail_id, offline).await?;
    let att = select_mail_attachment(&mail, None, attachment_selector)?;
    if offline {
        let _ = load_mail_attachment_bytes(&graph, &mail.id, att, true).await?;
    }

    let out_dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = graph
        .download_mail_attachment(&mail.id, att, &out_dir)
        .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&format_downloaded_attachment_json(
                &mail.id,
                att,
                &downloaded,
            ))?
        );
    } else {
        println!(
            "Downloaded attachment: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.bytes_written
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_mail_search(
    cfg: &AppConfig,
    query: &str,
    folder: Option<&str>,
    remote: bool,
    limit: usize,
    json_out: bool,
    compact: bool,
    cursor: Option<&str>,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("Search query is empty.");
    }
    let limit = clamp_limit(limit);
    let (hits, remote_messages, remote_paging) = if !remote {
        let storage = open_storage_only(cfg)?;
        let folder_filter = match folder.map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => Some(resolver::resolve_mail_folder_cached(&storage, f)?.id),
            None => None,
        };
        let hits = storage.search_mail(query, folder_filter.as_deref(), limit)?;
        (hits, Vec::new(), None)
    } else if let Some(raw_cursor) = cursor {
        let parsed = PageCursor::parse(raw_cursor)?;
        if !matches!(parsed.pos, CursorPos::Graph { .. }) {
            bail!("invalid cursor: not valid for this command");
        }
        let folder_filter = match folder.map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => {
                let storage = open_storage_only(cfg)?;
                Some(resolver::resolve_mail_folder_cached(&storage, f)?.id)
            }
            None => None,
        };
        let scope = cursor_scope(&[
            "search",
            folder_filter.as_deref().unwrap_or(""),
            query.trim(),
        ]);
        parsed.require_scope(&scope)?;
        let CursorPos::Graph { skip, url } = parsed.pos else {
            bail!("invalid cursor: not valid for this command");
        };
        if !crate::graph::client::graph_base_owns_url(&cfg.graph_base_url, &url) {
            bail!("invalid cursor: url is not a Graph URL for this account");
        }
        let (_auth, graph, storage) = open_runtime(cfg, true).await?;
        let page = match graph
            .search_mail_remote_paged(
                query,
                folder_filter.as_deref(),
                limit,
                &scope,
                Some((skip, url)),
            )
            .await
        {
            Ok(p) => p,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        let _ = storage.upsert_mail_messages(&page.messages);
        (
            Vec::new(),
            page.messages,
            Some((page.has_more, page.next_cursor)),
        )
    } else {
        let (_auth, graph, storage) = open_runtime(cfg, true).await?;
        let folder_filter = match folder.map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => Some(resolver::resolve_mail_folder(&graph, &storage, f, false).await?.id),
            None => None,
        };
        let hits = storage.search_mail(query, folder_filter.as_deref(), limit)?;
        let scope = cursor_scope(&[
            "search",
            folder_filter.as_deref().unwrap_or(""),
            query.trim(),
        ]);
        let page = match graph
            .search_mail_remote_paged(query, folder_filter.as_deref(), limit, &scope, None)
            .await
        {
            Ok(p) => p,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        let _ = storage.upsert_mail_messages(&page.messages);
        (hits, page.messages, Some((page.has_more, page.next_cursor)))
    };

    if json_out {
        let remote_json = mail_list_json(&remote_messages, compact);
        let payload = if let Some((has_more, next_cursor)) = remote_paging {
            json!({
                "query": query,
                "hits": hits,
                "remote": remote_json,
                "returned": remote_messages.len(),
                "has_more": has_more,
                "next_cursor": next_cursor,
            })
        } else {
            json!({
                "query": query,
                "hits": hits,
                "remote": remote_json,
            })
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if hits.is_empty() && remote_messages.is_empty() {
        println!(
            "No mail messages matching '{}'.",
            sanitize_terminal_inline(query)
        );
    } else {
        for hit in &hits {
            println!(
                "[local][{}] {} | {}: {} ({})",
                sanitize_terminal_inline(&hit.received_at),
                sanitize_terminal_inline(&hit.from_display),
                sanitize_terminal_inline(&hit.subject),
                sanitize_terminal_inline(&hit.snippet),
                sanitize_terminal_inline(&hit.id)
            );
        }
        for msg in &remote_messages {
            println!(
                "[remote][{}] {} | {} ({})",
                sanitize_terminal_inline(&msg.received_at),
                sanitize_terminal_inline(&msg.from_display()),
                sanitize_terminal_inline(msg.effective_subject()),
                sanitize_terminal_inline(&msg.id)
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Calendar commands
// ---------------------------------------------------------------------------
