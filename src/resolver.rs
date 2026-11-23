//! Smart entity resolver for CLI, HTTP API, and MCP.
//!
//! Allows users and AI agents to pass either exact Microsoft Graph IDs OR
//! human-friendly names, well-known aliases, and prefixes for Mail folders,
//! To Do task lists, cached mail messages, and cached calendar events.

use anyhow::{bail, Result};
use serde_json::json;

use crate::{
    cli,
    graph::GraphClient,
    models::{CalendarEvent, MailFolder, MailMessage, TodoList},
    storage::Storage,
};

/// Maps common folder aliases (case-insensitive) to canonical Microsoft Graph well-known folder names.
pub fn canonical_well_known_folder(query: &str) -> Option<&'static str> {
    match query.trim().to_ascii_lowercase().as_str() {
        "inbox" => Some("inbox"),
        "sent" | "sentitems" | "sent items" | "sent_items" => Some("sentitems"),
        "draft" | "drafts" => Some("drafts"),
        "archive" => Some("archive"),
        "trash" | "deleted" | "deleteditems" | "deleted items" | "deleted_items" => {
            Some("deleteditems")
        }
        "junk" | "junkemail" | "junk email" | "junk_email" | "spam" => Some("junkemail"),
        "outbox" => Some("outbox"),
        _ => None,
    }
}

fn well_known_display_name(well_known: &str) -> &'static str {
    match well_known {
        "inbox" => "Inbox",
        "sentitems" => "Sent Items",
        "drafts" => "Drafts",
        "archive" => "Archive",
        "deleteditems" => "Deleted Items",
        "junkemail" => "Junk Email",
        "outbox" => "Outbox",
        _ => "Inbox",
    }
}

fn synthetic_well_known_folder(well_known: &str) -> MailFolder {
    MailFolder {
        id: well_known.to_string(),
        display_name: well_known_display_name(well_known).to_string(),
        well_known_name: Some(well_known.to_string()),
        parent_folder_id: None,
        child_folder_count: 0,
        unread_item_count: 0,
        total_item_count: 0,
    }
}

/// Resolves a mail folder query against an in-memory slice of `MailFolder`s.
///
/// Priority:
/// 1. Exact `id` match
/// 2. Canonical well-known alias (`inbox`, `sent`/`sentitems`, `drafts`, `archive`, `trash`/`deleteditems`, `junk`/`junkemail`)
/// 3. Exact case-insensitive `well_known_name` match
/// 4. Unique case-insensitive exact `display_name` match
/// 5. Unique case-insensitive `display_name` prefix match
/// 6. Unique case-insensitive `display_name` substring match
pub fn resolve_mail_folder_from_slice(
    folders: &[MailFolder],
    folder_query: &str,
) -> Result<MailFolder> {
    let q = folder_query.trim();
    if q.is_empty() {
        bail!("Mail folder name or ID is empty.");
    }

    if let Some(exact_id) = folders.iter().find(|f| f.id == q) {
        return Ok(exact_id.clone());
    }

    let q_lower = q.to_lowercase();

    if let Some(wk) = canonical_well_known_folder(q) {
        if let Some(matched) = folders.iter().find(|f| {
            f.well_known_name
                .as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case(wk))
                || f.id.eq_ignore_ascii_case(wk)
        }) {
            return Ok(matched.clone());
        }
        let expected_display = well_known_display_name(wk);
        if let Some(matched) = folders
            .iter()
            .find(|f| f.display_name.trim().eq_ignore_ascii_case(expected_display))
        {
            return Ok(matched.clone());
        }
    }

    if let Some(wk_match) = folders.iter().find(|f| {
        f.well_known_name
            .as_deref()
            .is_some_and(|w| w.eq_ignore_ascii_case(&q_lower))
    }) {
        return Ok(wk_match.clone());
    }

    let exact_name: Vec<&MailFolder> = folders
        .iter()
        .filter(|f| f.display_name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous mail folder '{q}' matches multiple folders exactly:\n{}",
                format_folder_candidates(&exact_name, folders)
            ),
            Some(json!({ "candidates": folder_candidates_json(&exact_name, folders) })),
        ));
    }

    let prefix_matches: Vec<&MailFolder> = folders
        .iter()
        .filter(|f| f.display_name.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous mail folder prefix '{q}' matches multiple folders:\n{}",
                format_folder_candidates(&prefix_matches, folders)
            ),
            Some(json!({ "candidates": folder_candidates_json(&prefix_matches, folders) })),
        ));
    }

    let sub_matches: Vec<&MailFolder> = folders
        .iter()
        .filter(|f| f.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous mail folder '{q}' matches {n} folders:\n{}",
                format_folder_candidates(&sub_matches, folders)
            ),
            Some(json!({ "candidates": folder_candidates_json(&sub_matches, folders) })),
        )),
        _ => bail!("Mail folder not found: {q}"),
    }
}

/// Resolves a mail folder purely from local SQLite cache (or canonical well-known alias if cache is empty).
pub fn resolve_mail_folder_cached(storage: &Storage, folder_query: &str) -> Result<MailFolder> {
    let q = folder_query.trim();
    if q.is_empty() {
        bail!("Mail folder name or ID is empty.");
    }
    let cached = storage.list_mail_folders()?;
    if let Ok(found) = resolve_mail_folder_from_slice(&cached, q) {
        return Ok(found);
    }
    if let Some(wk) = canonical_well_known_folder(q) {
        return Ok(synthetic_well_known_folder(wk));
    }
    resolve_mail_folder_from_slice(&cached, q)
}

/// Resolves a mail folder by exact ID, well-known alias, or case-insensitive display name/prefix.
///
/// Checks cached folders first; when `prefer_cache` is false (or cache is empty for non-well-known names),
/// refreshes folders from Microsoft Graph and persists them to SQLite.
pub async fn resolve_mail_folder(
    graph: &GraphClient,
    storage: &Storage,
    folder_query: &str,
    prefer_cache: bool,
) -> Result<MailFolder> {
    let q = folder_query.trim();
    if q.is_empty() {
        bail!("Mail folder name or ID is empty.");
    }

    let cached = storage.list_mail_folders()?;
    if !cached.is_empty() {
        match resolve_mail_folder_from_slice(&cached, q) {
            Ok(found) => return Ok(found),
            Err(err) if prefer_cache => {
                if let Some(wk) = canonical_well_known_folder(q) {
                    return Ok(synthetic_well_known_folder(wk));
                }
                return Err(err);
            }
            Err(_) => {}
        }
    } else if prefer_cache {
        if let Some(wk) = canonical_well_known_folder(q) {
            return Ok(synthetic_well_known_folder(wk));
        }
    }

    let (fetched, complete) = graph.list_mail_folders_complete().await?;
    storage.store_mail_folder_listing(&fetched, complete)?;

    match resolve_mail_folder_from_slice(&fetched, q) {
        Ok(found) => Ok(found),
        Err(err) => {
            if let Some(wk) = canonical_well_known_folder(q) {
                Ok(synthetic_well_known_folder(wk))
            } else {
                Err(err)
            }
        }
    }
}

/// Returns `true` when `err` represents a Microsoft Graph HTTP 404 / item-or-folder-not-found
/// response (strictly excluding 401/403 authentication or authorization errors).
pub fn is_graph_not_found_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    let detail = msg
        .strip_prefix(crate::graph::GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    let lower = detail.to_ascii_lowercase();
    if detail.starts_with("401 ")
        || detail.starts_with("403 ")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("erroraccessdenied")
        || lower.contains("insufficient privileges")
    {
        return false;
    }
    detail.starts_with("404 ")
        || lower.contains("404 not found")
        || lower.contains("erroritemnotfound")
        || lower.contains("errorfoldernotfound")
}

/// Refreshes the mail folder list from Microsoft Graph and re-resolves `folder_query`
/// when a cached name-resolved folder ID returned HTTP 404 / `ErrorItemNotFound`.
///
/// Explicit folder IDs (`folder_query.trim() == previous_folder_id.trim()`) are never
/// substituted with another folder (`Ok(None)`). Returns `Ok(Some(new_folder))` only when
/// refreshing yields a different folder ID.
pub async fn refresh_mail_folder_after_not_found(
    graph: &GraphClient,
    storage: &Storage,
    folder_query: &str,
    previous_folder_id: &str,
) -> Result<Option<MailFolder>> {
    let q = folder_query.trim();
    if q.is_empty() || q == previous_folder_id.trim() {
        return Ok(None);
    }
    let (fetched, complete) = graph.list_mail_folders_complete().await?;
    storage.store_mail_folder_listing(&fetched, complete)?;
    let refreshed = match resolve_mail_folder_from_slice(&fetched, q) {
        Ok(found) => found,
        Err(err) => {
            if let Some(wk) = canonical_well_known_folder(q) {
                synthetic_well_known_folder(wk)
            } else {
                return Err(err);
            }
        }
    };
    if refreshed.id == previous_folder_id {
        Ok(None)
    } else {
        Ok(Some(refreshed))
    }
}

/// Resolves a Microsoft To Do list query against an in-memory slice of `TodoList`s.
///
/// Matches `"tasks"` / `"default"` / `"defaultList"`, exact `id`, case-insensitive
/// `wellknown_list_name`, exact `display_name`, prefix `display_name`, or unique substring.
pub fn resolve_todo_list_from_slice(lists: &[TodoList], list_query: &str) -> Result<TodoList> {
    let q = list_query.trim();
    if lists.is_empty() {
        bail!("No To Do lists found.");
    }

    if q.is_empty() || q.eq_ignore_ascii_case("default") || q.eq_ignore_ascii_case("defaultList") {
        if let Some(def) = lists.iter().find(|l| {
            l.wellknown_list_name
                .as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case("defaultList"))
        }) {
            return Ok(def.clone());
        }
        return Ok(lists[0].clone());
    }

    if q.eq_ignore_ascii_case("tasks") {
        if let Some(def) = lists.iter().find(|l| {
            l.wellknown_list_name
                .as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case("defaultList"))
        }) {
            return Ok(def.clone());
        }
        if let Some(named) = lists
            .iter()
            .find(|l| l.display_name.trim().eq_ignore_ascii_case("tasks"))
        {
            return Ok(named.clone());
        }
    }

    if let Some(exact_id) = lists.iter().find(|l| l.id == q) {
        return Ok(exact_id.clone());
    }

    let q_lower = q.to_lowercase();
    if let Some(wk) = lists.iter().find(|l| {
        l.wellknown_list_name
            .as_deref()
            .is_some_and(|w| w.to_lowercase() == q_lower)
    }) {
        return Ok(wk.clone());
    }

    let exact_name: Vec<&TodoList> = lists
        .iter()
        .filter(|l| l.display_name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous To Do list '{q}' matches multiple lists:\n{}",
            format_todo_list_candidates(&exact_name)
        );
    }

    let prefix_matches: Vec<&TodoList> = lists
        .iter()
        .filter(|l| l.display_name.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous To Do list prefix '{q}' matches multiple lists:\n{}",
            format_todo_list_candidates(&prefix_matches)
        );
    }

    let sub_matches: Vec<&TodoList> = lists
        .iter()
        .filter(|l| l.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous To Do list '{q}' matches {n} lists:\n{}",
            format_todo_list_candidates(&sub_matches)
        ),
        _ => bail!("To Do list not found: {q}"),
    }
}

/// Resolves a To Do list purely from local SQLite cache.
pub fn resolve_todo_list_cached(storage: &Storage, list_query: &str) -> Result<TodoList> {
    let cached = storage.list_todo_lists()?;
    resolve_todo_list_from_slice(&cached, list_query)
}

/// Resolves a To Do list by exact ID, `"tasks"`/`"defaultList"`, or case-insensitive display name/prefix.
pub async fn resolve_todo_list(
    graph: &GraphClient,
    storage: &Storage,
    list_query: &str,
    prefer_cache: bool,
) -> Result<TodoList> {
    let cached = storage.list_todo_lists()?;
    if !cached.is_empty() {
        match resolve_todo_list_from_slice(&cached, list_query) {
            Ok(found) => return Ok(found),
            Err(err) if prefer_cache => return Err(err),
            Err(_) => {}
        }
    }

    let fetched = graph.list_todo_lists().await?;
    storage.replace_todo_lists(&fetched)?;
    resolve_todo_list_from_slice(&fetched, list_query)
}

/// Resolves a cached mail message by exact ID, unique ID prefix, or unique subject match.
pub fn resolve_mail_message_from_slice(
    messages: &[MailMessage],
    query: &str,
) -> Result<MailMessage> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Mail message ID or query is empty.");
    }
    if let Some(exact) = messages.iter().find(|m| m.id == q) {
        return Ok(exact.clone());
    }
    let id_prefixes: Vec<&MailMessage> = messages
        .iter()
        .filter(|m| m.id.starts_with(q))
        .collect();
    if id_prefixes.len() == 1 {
        return Ok(id_prefixes[0].clone());
    } else if id_prefixes.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous message ID prefix '{q}' matches {} messages.",
                id_prefixes.len()
            ),
            Some(json!({ "candidates": message_candidates_json(&id_prefixes) })),
        ));
    }

    let q_lower = q.to_lowercase();
    let subj_exact: Vec<&MailMessage> = messages
        .iter()
        .filter(|m| m.subject.trim().to_lowercase() == q_lower)
        .collect();
    if subj_exact.len() == 1 {
        return Ok(subj_exact[0].clone());
    } else if subj_exact.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous message subject '{q}' matches {} messages.",
                subj_exact.len()
            ),
            Some(json!({ "candidates": message_candidates_json(&subj_exact) })),
        ));
    }

    bail!("Mail message not found: {q}")
}

/// Resolves a cached calendar event by exact ID, unique ID prefix, or unique subject match.
pub fn resolve_calendar_event_from_slice(
    events: &[CalendarEvent],
    query: &str,
) -> Result<CalendarEvent> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Calendar event ID or query is empty.");
    }
    if let Some(exact) = events.iter().find(|e| e.id == q) {
        return Ok(exact.clone());
    }
    let id_prefixes: Vec<&CalendarEvent> = events.iter().filter(|e| e.id.starts_with(q)).collect();
    if id_prefixes.len() == 1 {
        return Ok(id_prefixes[0].clone());
    } else if id_prefixes.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous event ID prefix '{q}' matches {} events.",
                id_prefixes.len()
            ),
            Some(json!({ "candidates": event_candidates_json(&id_prefixes) })),
        ));
    }

    let q_lower = q.to_lowercase();
    let subj_exact: Vec<&CalendarEvent> = events
        .iter()
        .filter(|e| e.subject.trim().to_lowercase() == q_lower)
        .collect();
    if subj_exact.len() == 1 {
        return Ok(subj_exact[0].clone());
    } else if subj_exact.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous event subject '{q}' matches {} events.",
                subj_exact.len()
            ),
            Some(json!({ "candidates": event_candidates_json(&subj_exact) })),
        ));
    }

    let subj_prefix: Vec<&CalendarEvent> = events
        .iter()
        .filter(|e| e.subject.trim().to_lowercase().starts_with(&q_lower))
        .collect();
    if subj_prefix.len() == 1 {
        return Ok(subj_prefix[0].clone());
    } else if subj_prefix.len() > 1 {
        return Err(cli::error_with_info(
            "ambiguous",
            format!(
                "Ambiguous event subject prefix '{q}' matches {} events.",
                subj_prefix.len()
            ),
            Some(json!({ "candidates": event_candidates_json(&subj_prefix) })),
        ));
    }

    bail!("Calendar event not found: {q}")
}

const MAX_CANDIDATE_LIST: usize = 8;

fn folder_display_path(all_folders: &[MailFolder], folder: &MailFolder) -> String {
    let mut parts = vec![folder.display_name.as_str()];
    let mut visited = vec![folder.id.as_str()];
    let mut cur_parent = folder.parent_folder_id.as_deref();
    while let Some(pid) = cur_parent {
        if visited.contains(&pid) {
            break;
        }
        let Some(parent) = all_folders.iter().find(|f| f.id == pid) else {
            break;
        };
        visited.push(parent.id.as_str());
        parts.push(parent.display_name.as_str());
        cur_parent = parent.parent_folder_id.as_deref();
    }
    parts.reverse();
    parts.join("/")
}

fn folder_candidates_json(
    candidates: &[&MailFolder],
    all_folders: &[MailFolder],
) -> Vec<serde_json::Value> {
    candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|f| {
            json!({
                "id": f.id,
                "display_name": f.display_name,
                "path": folder_display_path(all_folders, f),
            })
        })
        .collect()
}

fn message_candidates_json(candidates: &[&MailMessage]) -> Vec<serde_json::Value> {
    candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|m| {
            json!({
                "id": m.id,
                "subject": m.subject,
                "received_at": m.received_at,
            })
        })
        .collect()
}

fn event_candidates_json(candidates: &[&CalendarEvent]) -> Vec<serde_json::Value> {
    candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|e| {
            json!({
                "id": e.id,
                "subject": e.subject,
                "start_iso": e.start_iso,
            })
        })
        .collect()
}

fn format_folder_candidates(candidates: &[&MailFolder], all_folders: &[MailFolder]) -> String {
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|f| format!("  - {} ({})", folder_display_path(all_folders, f), f.id))
        .collect();
    if candidates.len() > MAX_CANDIDATE_LIST {
        lines.push(format!(
            "  ... and {} more",
            candidates.len() - MAX_CANDIDATE_LIST
        ));
    }
    lines.join("\n")
}

fn format_todo_list_candidates(candidates: &[&TodoList]) -> String {
    const MAX_LIST: usize = 8;
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_LIST)
        .map(|l| format!("  - {} ({})", l.display_name, l.id))
        .collect();
    if candidates.len() > MAX_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_LIST));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn folder(id: &str, display_name: &str, well_known: Option<&str>) -> MailFolder {
        MailFolder {
            id: id.to_string(),
            display_name: display_name.to_string(),
            well_known_name: well_known.map(str::to_string),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 10,
        }
    }

    fn todo_list(id: &str, display_name: &str, well_known: Option<&str>) -> TodoList {
        TodoList {
            id: id.to_string(),
            display_name: display_name.to_string(),
            wellknown_list_name: well_known.map(str::to_string),
            is_owner: true,
            is_shared: false,
        }
    }

    #[test]
    fn resolves_mail_folders_by_id_well_known_alias_name_and_prefix() {
        let folders = vec![
            folder("f-inbox", "Inbox", Some("inbox")),
            folder("f-sent", "Sent Items", Some("sentitems")),
            folder("f-drafts", "Drafts", Some("drafts")),
            folder("f-archive", "Archive", Some("archive")),
            folder("f-trash", "Deleted Items", Some("deleteditems")),
            folder("f-junk", "Junk Email", Some("junkemail")),
            folder("f-eng", "Engineering Alerts", None),
            folder("f-fin", "Finance Reports", None),
        ];

        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "f-eng").unwrap().id,
            "f-eng"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "inbox").unwrap().id,
            "f-inbox"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "sent").unwrap().id,
            "f-sent"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "sentitems").unwrap().id,
            "f-sent"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "drafts").unwrap().id,
            "f-drafts"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "archive").unwrap().id,
            "f-archive"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "trash").unwrap().id,
            "f-trash"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "deleteditems").unwrap().id,
            "f-trash"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "junk").unwrap().id,
            "f-junk"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "junkemail").unwrap().id,
            "f-junk"
        );
        // Case-insensitive display name and prefix
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "engineering alerts")
                .unwrap()
                .id,
            "f-eng"
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "fin").unwrap().id,
            "f-fin"
        );
        assert!(resolve_mail_folder_from_slice(&folders, "nonexistent").is_err());
        assert!(resolve_mail_folder_from_slice(&folders, "  ").is_err());
    }

    #[test]
    fn rejects_ambiguous_folder_prefixes() {
        let folders = vec![
            folder("f1", "Project Alpha", None),
            folder("f2", "Project Beta", None),
        ];
        let err = resolve_mail_folder_from_slice(&folders, "Project")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Project Alpha"));
        assert!(err.contains("Project Beta"));
    }

    #[test]
    fn resolves_todo_lists_by_default_alias_id_name_and_prefix() {
        let lists = vec![
            todo_list("l-custom", "Groceries", None),
            todo_list("l-default", "Tasks", Some("defaultList")),
            todo_list("l-work", "Work Backlog", None),
        ];

        assert_eq!(
            resolve_todo_list_from_slice(&lists, "tasks").unwrap().id,
            "l-default"
        );
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "defaultList").unwrap().id,
            "l-default"
        );
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "").unwrap().id,
            "l-default"
        );
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "l-custom").unwrap().id,
            "l-custom"
        );
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "groceries").unwrap().id,
            "l-custom"
        );
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "work").unwrap().id,
            "l-work"
        );
        assert!(resolve_todo_list_from_slice(&lists, "missing").is_err());
    }

    #[tokio::test]
    async fn resolve_mail_folder_uses_cache_and_live_fallback_preserving_errors() {
        use axum::{http::StatusCode, response::IntoResponse, routing::get, Json, Router};
        use serde_json::json;
        use tokio::net::TcpListener;

        use crate::{
            auth::{AuthManager, StoredTokens},
            config::AppConfig,
        };

        let hits = Arc::new(AtomicUsize::new(0));
        let fail_mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hits_c = hits.clone();
        let fail_c = fail_mode.clone();

        let app = Router::new().route(
            "/v1.0/me/mailFolders",
            get(move || {
                let hits = hits_c.clone();
                let fail = fail_c.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if fail.load(Ordering::SeqCst) {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "error": {
                                    "code": "Authorization_RequestDenied",
                                    "message": "Forbidden"
                                }
                            })),
                        )
                            .into_response();
                    }
                    Json(json!({
                        "value": [
                            {
                                "id": "folder-inbox-1",
                                "displayName": "Inbox",
                                "wellKnownName": "inbox",
                                "unreadItemCount": 3,
                                "totalItemCount": 12
                            },
                            {
                                "id": "folder-ops-1",
                                "displayName": "Operations",
                                "unreadItemCount": 0,
                                "totalItemCount": 5
                            }
                        ]
                    }))
                    .into_response()
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("me-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1");
        let storage = Storage::open_in_memory().unwrap();

        // 1. Cold cache + prefer_cache=true + well-known alias resolves immediately without hitting Graph
        let fast = resolve_mail_folder(&graph, &storage, "sent", true)
            .await
            .unwrap();
        assert_eq!(fast.id, "sentitems");
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        // 2. Cold cache + live resolution hits Graph once and caches folders
        let ops = resolve_mail_folder(&graph, &storage, "oper", false)
            .await
            .unwrap();
        assert_eq!(ops.id, "folder-ops-1");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 3. Subsequent call hits SQLite cache without calling Graph again
        let inbox = resolve_mail_folder(&graph, &storage, "inbox", false)
            .await
            .unwrap();
        assert_eq!(inbox.id, "folder-inbox-1");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 4. 403 error on cold cache is preserved rather than masked
        fail_mode.store(true, Ordering::SeqCst);
        let cold_storage = Storage::open_in_memory().unwrap();
        let err = resolve_mail_folder(&graph, &cold_storage, "Operations", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("403") || err.contains("Forbidden"),
            "expected Graph 403 error, got: {err}"
        );

        server.abort();
    }

    #[test]
    fn duplicate_message_subject_event_prefix_and_folder_paths_report_ambiguous() {
        let make_msg = |id: &str, subject: &str| MailMessage {
            id: id.to_string(),
            internet_message_id: None,
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: subject.to_string(),
            from: None,
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: "2026-11-20T09:00:00Z".to_string(),
            sent_at: None,
            last_modified_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: None,
            has_attachments: false,
            body_loaded: false,
            preview: String::new(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: Vec::new(),
            meeting_request: None,
            web_link: None,
        };
        let msgs = vec![
            make_msg("msg-dup-1", "Weekly Sync Notes"),
            make_msg("msg-dup-2", "Weekly Sync Notes"),
            make_msg("msg-uniq", "Unique Subject"),
        ];
        let dup_err = resolve_mail_message_from_slice(&msgs, "Weekly Sync Notes")
            .expect_err("duplicate subject must fail as ambiguous");
        assert!(dup_err.to_string().starts_with("Ambiguous"));
        assert_eq!(cli::error_code(&dup_err), "ambiguous");
        let dup_json = cli::json_error_payload(&dup_err);
        assert_eq!(
            dup_json["error"]["details"]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            resolve_mail_message_from_slice(&msgs, "Unique Subject")
                .unwrap()
                .id,
            "msg-uniq"
        );

        let make_ev = |id: &str, subject: &str| CalendarEvent {
            id: id.to_string(),
            subject: subject.to_string(),
            start_iso: "2026-11-20T09:00:00Z".to_string(),
            end_iso: "2026-11-20T10:00:00Z".to_string(),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: Vec::new(),
            response_status: "accepted".to_string(),
            is_online_meeting: false,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        };
        let events = vec![
            make_ev("ev-1", "Design Review Alpha"),
            make_ev("ev-2", "Design Review Beta"),
        ];
        let ev_err = resolve_calendar_event_from_slice(&events, "Design Review")
            .expect_err("duplicate event subject prefix must fail as ambiguous");
        assert!(ev_err.to_string().starts_with("Ambiguous"));
        assert_eq!(cli::error_code(&ev_err), "ambiguous");

        let mut inbox_reports = folder("folder-dup-1", "Reports", None);
        inbox_reports.parent_folder_id = Some("f-inbox".to_string());
        let mut archive_reports = folder("folder-dup-2", "Reports", None);
        archive_reports.parent_folder_id = Some("f-archive".to_string());
        let folders = vec![
            folder("f-inbox", "Inbox", Some("inbox")),
            folder("f-archive", "Archive", Some("archive")),
            inbox_reports,
            archive_reports,
        ];
        let fld_err = resolve_mail_folder_from_slice(&folders, "Reports")
            .expect_err("duplicate folder display name must fail as ambiguous");
        let fld_msg = fld_err.to_string();
        assert!(fld_msg.contains("Inbox/Reports (folder-dup-1)"), "{fld_msg}");
        assert!(
            fld_msg.contains("Archive/Reports (folder-dup-2)"),
            "{fld_msg}"
        );
        let fld_json = cli::json_error_payload(&fld_err);
        let cands = fld_json["error"]["details"]["candidates"].as_array().unwrap();
        assert_eq!(cands[0]["path"], "Inbox/Reports");
        assert_eq!(cands[1]["path"], "Archive/Reports");

        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "folder-dup-1")
                .unwrap()
                .id,
            "folder-dup-1"
        );
    }
}
