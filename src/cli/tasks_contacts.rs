//! Microsoft To Do task and Contacts CLI commands.

use anyhow::{bail, Result};
use chrono::{DateTime, NaiveDate};
use serde_json::json;

use crate::{
    config::AppConfig,
    models::{ContactItem, OutgoingTodoTask, TodoList, TodoTask},
    resolver,
    safe_text::sanitize_terminal_inline,
};

use super::{
    args::DELETE_JSON_REQUIRES_YES,
    format::{
        clamp_limit, cursor_scope, delete_gate, is_forbidden_error, normalize_todo_due_iso,
        normalize_todo_importance, offset_page, prompt, wrap_graph_permission_error, CursorPos,
        PageCursor,
    },
    runtime::{
        cli_cache_ttl_secs, is_cursor_fresh, open_runtime, open_storage_only,
        record_cli_sync_cursor, resolve_daemon_warm,
    },
};

pub(super) fn print_todo_lists_output(
    lists: &[TodoList],
    json_out: bool,
    limit: Option<usize>,
    offset: usize,
) -> Result<()> {
    if json_out {
        let lim = limit.map(clamp_limit).unwrap_or(50);
        let scope = cursor_scope(&["task-lists"]);
        let (page, has_more, next_cursor) = offset_page(lists, offset, lim, &scope);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "lists": page,
                "returned": page.len(),
                "has_more": has_more,
                "next_cursor": next_cursor,
            }))?
        );
    } else if lists.is_empty() {
        println!("No To Do lists found.");
    } else {
        let take_n = limit.map(clamp_limit).unwrap_or(lists.len());
        for l in lists.iter().take(take_n) {
            let wk = l
                .wellknown_list_name
                .as_deref()
                .map(|w| format!(" [{}]", sanitize_terminal_inline(w)))
                .unwrap_or_default();
            println!(
                "{}{} ({})",
                sanitize_terminal_inline(&l.display_name),
                wk,
                sanitize_terminal_inline(&l.id)
            );
        }
    }
    Ok(())
}

pub(super) fn print_todo_tasks_output(
    list_id: &str,
    tasks: &[TodoTask],
    has_more: bool,
    json_out: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "list_id": list_id,
                "tasks": tasks,
                "returned": tasks.len(),
                "has_more": has_more,
            }))?
        );
    } else if tasks.is_empty() {
        println!("No tasks found.");
    } else {
        for t in tasks {
            let check = if t.is_completed() { "x" } else { " " };
            let imp = if t.importance.eq_ignore_ascii_case("high") {
                "!"
            } else {
                " "
            };
            let due = t
                .due_date_iso
                .as_deref()
                .map(|d| format!(" (due {})", sanitize_terminal_inline(d)))
                .unwrap_or_default();
            println!(
                "[{check}]{imp} {}{}  ({})",
                sanitize_terminal_inline(&t.title),
                due,
                sanitize_terminal_inline(&t.id)
            );
        }
    }
    Ok(())
}

pub(super) async fn cmd_todo_lists(
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
        parsed.require_scope(&cursor_scope(&["task-lists"]))?;
        let storage = open_storage_only(cfg)?;
        let lists = storage.list_todo_lists()?;
        return print_todo_lists_output(&lists, json_out, limit, offset);
    }

    if offline {
        let storage = open_storage_only(cfg)?;
        let lists = storage.list_todo_lists()?;
        return print_todo_lists_output(&lists, json_out, limit, 0);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:todo_lists"], ttl) {
                let lists = storage.list_todo_lists()?;
                if !lists.is_empty() {
                    return print_todo_lists_output(&lists, json_out, limit, 0);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let cached_lists = storage.list_todo_lists()?;
    let should_fetch = refresh
        || cached_lists.is_empty()
        || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &["cli:todo_lists"], ttl));
    let lists = if should_fetch {
        let fetched = match graph.list_todo_lists().await {
            Ok(l) => l,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        storage.replace_todo_lists(&fetched)?;
        record_cli_sync_cursor(&storage, "cli:todo_lists");
        storage.list_todo_lists()?
    } else {
        cached_lists
    };

    print_todo_lists_output(&lists, json_out, limit, 0)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_todo_tasks(
    cfg: &AppConfig,
    list: &str,
    include_completed: bool,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    if offline {
        let storage = open_storage_only(cfg)?;
        let resolved = resolver::resolve_todo_list_cached(&storage, list)?;
        let mut tasks = storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
        let has_more = tasks.len() > limit;
        tasks.truncate(limit);
        return print_todo_tasks_output(&resolved.id, &tasks, has_more, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(resolved) = resolver::resolve_todo_list_cached(&storage, list) {
                let cursor_key = format!("cli:todo_tasks:{}", resolved.id);
                if cached || daemon_warm || is_cursor_fresh(&storage, &[cursor_key.as_str()], ttl)
                {
                    let mut tasks =
                        storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
                    if !tasks.is_empty() {
                        let has_more = tasks.len() > limit;
                        tasks.truncate(limit);
                        return print_todo_tasks_output(&resolved.id, &tasks, has_more, json_out);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let cached_tasks = storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
    let cursor_key = format!("cli:todo_tasks:{}", resolved.id);
    let should_fetch = refresh
        || cached_tasks.is_empty()
        || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &[cursor_key.as_str()], ttl));
    let (mut tasks, has_more) = if should_fetch {
        let (fetched, reached_end) = match graph
            .list_todo_tasks_complete(&resolved.id, include_completed, limit)
            .await
        {
            Ok(t) => t,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        if reached_end {
            storage.replace_todo_tasks_for_list(&resolved.id, include_completed, &fetched)?;
        } else {
            storage.upsert_todo_tasks(&fetched)?;
        }
        record_cli_sync_cursor(&storage, &cursor_key);
        let stored = storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
        let has_more = !reached_end || stored.len() > limit;
        (stored, has_more)
    } else {
        let has_more = cached_tasks.len() > limit;
        (cached_tasks, has_more)
    };
    tasks.truncate(limit);

    print_todo_tasks_output(&resolved.id, &tasks, has_more, json_out)
}

pub(super) async fn cmd_todo_add(
    cfg: &AppConfig,
    title: &str,
    list: &str,
    due: Option<&str>,
    importance: &str,
    notes: Option<String>,
    json_out: bool,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("Task title is empty.");
    }
    let due_date_iso = due.map(normalize_todo_due_iso).transpose()?;
    let norm_importance = normalize_todo_importance(importance)?;

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;

    let outgoing = OutgoingTodoTask {
        title: title.to_string(),
        body_plain: notes,
        due_date_iso,
        importance: Some(norm_importance.to_string()),
    };
    let created = match graph.create_todo_task(&resolved.id, &outgoing).await {
        Ok(t) => t,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "task": created,
            }))?
        );
    } else {
        println!(
            "Task added: {} ({})",
            sanitize_terminal_inline(&created.title),
            sanitize_terminal_inline(&created.id)
        );
    }
    Ok(())
}

pub(crate) fn normalize_todo_edit_due(due: &str) -> Result<String> {
    let due = due.trim();
    if due.parse::<NaiveDate>().is_ok() || DateTime::parse_from_rfc3339(due).is_ok() {
        return normalize_todo_due_iso(due);
    }
    bail!("Use YYYY-MM-DD or an RFC3339 timestamp for the task due date.")
}

pub(super) async fn cmd_update_task(
    cfg: &AppConfig,
    id: &str,
    list: &str,
    title: Option<&str>,
    due: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let due = due.map(normalize_todo_edit_due).transpose()?;
    if title.is_some_and(|title| title.trim().is_empty()) {
        bail!("Task title must not be empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let task = match graph
        .edit_todo_task(&resolved.id, id, title, due.as_deref())
        .await
    {
        Ok(updated) => updated,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&task));
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "task": task }))?
        );
    } else {
        println!(
            "Updated task {} ({}).",
            sanitize_terminal_inline(&task.title),
            sanitize_terminal_inline(&task.id)
        );
    }
    Ok(())
}

pub(super) async fn cmd_todo_done(
    cfg: &AppConfig,
    task_id: &str,
    list: &str,
    completed: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let updated = match graph
        .set_todo_task_completed(&resolved.id, task_id, completed)
        .await
    {
        Ok(t) => t,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&updated));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "task": updated,
            }))?
        );
    } else {
        println!(
            "Task marked {}: {} ({})",
            if completed { "completed" } else { "incomplete" },
            updated.title,
            updated.id
        );
    }
    Ok(())
}

pub(super) async fn cmd_todo_delete(
    cfg: &AppConfig,
    task_id: &str,
    list: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete task {task_id}? Type 'yes' to confirm: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    if let Err(err) = graph.delete_todo_task(&resolved.id, task_id).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.delete_todo_task(task_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "list_id": resolved.id,
                "task_id": task_id,
            }))?
        );
    } else {
        println!("Deleted task: {task_id}");
    }
    Ok(())
}

pub(super) fn print_contact_line(c: &ContactItem) {
    let email = c.primary_email().unwrap_or("-");
    let org = c
        .company_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {}", sanitize_terminal_inline(s)))
        .unwrap_or_default();
    let phone = c
        .mobile_phone
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {}", sanitize_terminal_inline(s)))
        .unwrap_or_default();
    println!(
        "[{}] {} <{}>{}{} ({})",
        sanitize_terminal_inline(&c.source),
        sanitize_terminal_inline(&c.display_name),
        sanitize_terminal_inline(email),
        org,
        phone,
        sanitize_terminal_inline(&c.id)
    );
}

pub(crate) fn directory_status_json(
    queried: bool,
    error: Option<&anyhow::Error>,
) -> serde_json::Value {
    match (queried, error) {
        (false, _) => json!({ "status": "not_queried", "error": null }),
        (true, None) => json!({ "status": "ok", "error": null }),
        (true, Some(e)) if is_forbidden_error(e) => {
            json!({ "status": "forbidden", "error": format!("{e:#}") })
        }
        (true, Some(e)) => json!({ "status": "error", "error": format!("{e:#}") }),
    }
}

pub(super) fn print_contacts_output(
    contacts: &[ContactItem],
    directory: serde_json::Value,
    json_out: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "contacts": contacts,
                "directory": directory,
            }))?
        );
    } else {
        if let Some(status) = directory.get("status").and_then(|v| v.as_str()) {
            if matches!(status, "forbidden" | "error") {
                let err_msg = directory
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                eprintln!(
                    "warning: directory search failed ({status}); showing personal contacts only: {err_msg}"
                );
            }
        }
        if contacts.is_empty() {
            println!("No contacts found.");
        } else {
            for c in contacts {
                print_contact_line(c);
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_contacts(
    cfg: &AppConfig,
    query: Option<&str>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let trimmed_q = query.map(str::trim).filter(|s| !s.is_empty());

    if offline {
        let storage = open_storage_only(cfg)?;
        let contacts = storage.list_contacts(trimmed_q, limit)?;
        return print_contacts_output(&contacts, directory_status_json(false, None), json_out);
    }

    if trimmed_q.is_none() {
        let ttl = cli_cache_ttl_secs(cfg);
        let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
        if !refresh && (cached || daemon_warm || ttl > 0) {
            if let Ok(storage) = open_storage_only(cfg) {
                if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:contacts"], ttl) {
                    let contacts = storage.list_contacts(None, limit)?;
                    if !contacts.is_empty() {
                        return print_contacts_output(
                            &contacts,
                            directory_status_json(false, None),
                            json_out,
                        );
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let (contacts, directory) = if let Some(q) = trimmed_q {
        if cached && !refresh {
            let local = storage.list_contacts(Some(q), limit)?;
            if !local.is_empty() {
                (local, directory_status_json(false, None))
            } else {
                let outcome = match graph.search_contacts_and_directory_detailed(q, limit).await {
                    Ok(c) => c,
                    Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
                };
                let _ = storage.upsert_contacts(&outcome.contacts);
                let dir = directory_status_json(true, outcome.directory_error.as_ref());
                (outcome.contacts, dir)
            }
        } else {
            let outcome = match graph.search_contacts_and_directory_detailed(q, limit).await {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            let _ = storage.upsert_contacts(&outcome.contacts);
            let dir = directory_status_json(true, outcome.directory_error.as_ref());
            (outcome.contacts, dir)
        }
    } else {
        let ttl = cli_cache_ttl_secs(cfg);
        let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
        let cached_contacts = storage.list_contacts(None, limit)?;
        let should_fetch = refresh
            || cached_contacts.is_empty()
            || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &["cli:contacts"], ttl));
        let contacts = if should_fetch {
            let (fetched, reached_end) = match graph.list_contacts_complete(limit).await {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            if reached_end {
                storage.replace_personal_contacts(&fetched)?;
            } else {
                storage.upsert_contacts(&fetched)?;
            }
            record_cli_sync_cursor(&storage, "cli:contacts");
            storage.list_contacts(None, limit)?
        } else {
            cached_contacts
        };
        (contacts, directory_status_json(false, None))
    };

    print_contacts_output(&contacts, directory, json_out)
}

// ---------------------------------------------------------------------------
// Sync, Update, Preview
// ---------------------------------------------------------------------------
