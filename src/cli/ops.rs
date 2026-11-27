//! Daemon, sync, self-update, and terminal image preview commands.

use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    config::{self, AppConfig},
    img_preview, updater,
};

use super::{
    args::DaemonCommand,
    format::{
        clamp_limit, default_calendar_window, load_mail_attachment_bytes, select_mail_attachment,
        wrap_graph_permission_error,
    },
    mail::load_mail_message,
    runtime::{open_runtime, record_cli_sync_cursor},
};

pub(super) fn print_daemon_status_text(header: &str, cfg: &AppConfig, status: &crate::daemon::DaemonStatus) {
    println!(
        "{header}: running={}, pid={}, transport={}, endpoint={}, attached_clients={}, auto_daemon={}",
        status.running,
        status
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string()),
        status.transport.as_deref().unwrap_or("-"),
        status.endpoint.as_deref().unwrap_or("-"),
        status.attached_clients,
        cfg.auto_daemon,
    );
    if status.running {
        println!(
            "  version={}, uptime_secs={}, sync_busy={}, last_sync_at={}",
            status.version.as_deref().unwrap_or("-"),
            status
                .uptime_secs
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string()),
            status.sync_busy,
            status.last_sync_at.as_deref().unwrap_or("-"),
        );
        if !status.sync_summary.trim().is_empty() {
            println!("  sync_summary: {}", status.sync_summary);
        }
    }
    if let Some(ref err) = status.last_error {
        println!("  last_error: {err}");
    }
}

pub(super) async fn dispatch_daemon_command(cfg: &AppConfig, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start { json } => {
            let status = crate::daemon::ensure_daemon_started(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon started", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Stop { json } => {
            let stopped = crate::daemon::stop_daemon(cfg).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "stopped": stopped,
                    }))?
                );
            } else if stopped {
                println!("Background sync daemon stopped.");
            } else {
                println!("Background sync daemon is not running.");
            }
            Ok(())
        }
        DaemonCommand::Status { json } => {
            let status = crate::daemon::query_daemon_status(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon status", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Restart { json } => {
            let _ = crate::daemon::stop_daemon(cfg).await?;
            let status = crate::daemon::ensure_daemon_started(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon restarted", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Run => crate::daemon::run_daemon_foreground(cfg.clone()).await,
    }
}

// ---------------------------------------------------------------------------
// Runtime / Storage fast-open helpers
// ---------------------------------------------------------------------------

pub(super) async fn cmd_sync(cfg: &AppConfig, days: i64, limit: usize, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let limit = clamp_limit(limit);
    let (start_iso, end_iso) = default_calendar_window(None, None, days)?;

    let (folders_res, cal_res, lists_res, contacts_res) = tokio::join!(
        graph.list_mail_folders_complete(),
        graph.list_calendar_view_complete(&start_iso, &end_iso, limit),
        graph.list_todo_lists(),
        graph.list_contacts_complete(limit),
    );

    let mut errors = Vec::new();
    let mut folder_count = 0usize;
    let mut message_count = 0usize;
    let mut event_count = 0usize;
    let mut todo_list_count = 0usize;
    let mut todo_task_count = 0usize;
    let mut contact_count = 0usize;

    match folders_res {
        Ok((folders, complete)) => {
            folder_count = folders.len();
            storage.store_mail_folder_listing(&folders, complete)?;
            if complete {
                record_cli_sync_cursor(&storage, "cli:mail_folders");
            }
        }
        Err(err) => errors.push(format!(
            "mail_folders: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    let todo_lists_for_tasks = match lists_res {
        Ok(lists) => {
            todo_list_count = lists.len();
            storage.replace_todo_lists(&lists)?;
            record_cli_sync_cursor(&storage, "cli:todo_lists");
            lists
        }
        Err(err) => {
            errors.push(format!(
                "todo_lists: {:#}",
                wrap_graph_permission_error(&graph, err).await
            ));
            Vec::new()
        }
    };

    // Wave 2: overlap inbox delta with bounded-concurrent To Do task fetches across lists.
    let (inbox_res, todo_task_results) = tokio::join!(
        crate::sync::sync_mail_folder_delta("inbox", &graph, &storage, None, false),
        async {
            use futures_util::stream::{self, StreamExt};
            stream::iter(todo_lists_for_tasks.iter())
                .map(|list| {
                    let g = &graph;
                    async move { (list, g.list_todo_tasks_complete(&list.id, true, limit).await) }
                })
                .buffered(4)
                .collect::<Vec<_>>()
                .await
        }
    );

    match inbox_res {
        Ok(outcome) => {
            record_cli_sync_cursor(&storage, &format!("cli:mail:{}", outcome.folder_id));
            message_count = storage
                .list_mail_summaries(Some(&outcome.folder_id), limit)
                .map(|v| v.len())
                .unwrap_or(0);
        }
        Err(err) => errors.push(format!(
            "inbox: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    match cal_res {
        Ok((events, reached_end)) => {
            event_count = events.len();
            if reached_end {
                storage.replace_calendar_events_in_window(&start_iso, &end_iso, &events)?;
            } else {
                storage.upsert_calendar_events(&events)?;
            }
            record_cli_sync_cursor(&storage, "cli:cal");
        }
        Err(err) => errors.push(format!(
            "calendar: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    for (list, tasks_res) in todo_task_results {
        match tasks_res {
            Ok((tasks, reached_end)) => {
                todo_task_count += tasks.len();
                if reached_end {
                    let _ = storage.replace_todo_tasks_for_list(&list.id, true, &tasks);
                } else {
                    let _ = storage.upsert_todo_tasks(&tasks);
                }
                record_cli_sync_cursor(&storage, &format!("cli:todo_tasks:{}", list.id));
            }
            Err(err) => errors.push(format!("todo_tasks({}): {err:#}", list.display_name)),
        }
    }

    match contacts_res {
        Ok((contacts, reached_end)) => {
            contact_count = contacts.len();
            if reached_end {
                storage.replace_personal_contacts(&contacts)?;
            } else {
                storage.upsert_contacts(&contacts)?;
            }
            record_cli_sync_cursor(&storage, "cli:contacts");
        }
        Err(err) => errors.push(format!(
            "contacts: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": errors.is_empty(),
                "folders": folder_count,
                "messages": message_count,
                "events": event_count,
                "todo_lists": todo_list_count,
                "todo_tasks": todo_task_count,
                "contacts": contact_count,
                "errors": errors,
            }))?
        );
    } else {
        println!(
            "Sync complete: folders={folder_count}, inbox={message_count}, events={event_count}, todo_lists={todo_list_count}, tasks={todo_task_count}, contacts={contact_count}"
        );
        for err in &errors {
            eprintln!("Warning: {err}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_update(
    cfg: &AppConfig,
    check: bool,
    force: bool,
    repo_override: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let repo = repo_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.github_repo);
    let result = updater::check_or_apply_update(repo, check, force).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Repository: {repo}");
        println!("Current version: v{}", result.current_version);
        println!("Latest version: v{}", result.latest_version);
        if result.updated {
            println!(
                "Update complete: {} ({})",
                result.binary_path.display(),
                result.asset_name.as_deref().unwrap_or("-")
            );
        } else if result.update_available {
            println!("New version available: {}", result.release_url);
            if check {
                println!("Running in --check mode; binary was not replaced.");
            }
        } else {
            println!("Already up to date.");
        }
    }
    Ok(())
}

pub(crate) fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let c = cols.unwrap_or_else(|| term_cols.clamp(20, 120));
    let r = rows.unwrap_or_else(|| term_rows.saturating_sub(4).clamp(10, 40));
    (c, r)
}

pub(super) fn resolve_image_protocol_preference(
    cfg: &AppConfig,
    protocol: Option<&str>,
) -> Result<config::ImageProtocolPreference> {
    match protocol {
        Some(raw) => match config::ImageProtocolPreference::parse_str(raw) {
            Some(pref) => Ok(pref),
            None => bail!(
                "Invalid image protocol '{}'. Valid values: auto, kitty, iterm2, sixel, halfblock",
                raw.trim()
            ),
        },
        None => Ok(cfg.image_protocol),
    }
}

pub(super) fn print_rendered_preview(rendered: &str) -> Result<()> {
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

pub(super) fn cmd_preview_local_file(
    cfg: &AppConfig,
    path: &Path,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let effective_pref = resolve_image_protocol_preference(cfg, protocol)?;
    if !path.exists() || !path.is_file() {
        bail!("Image file not found: {}", path.display());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read image file: {}", path.display()))?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, effective_pref)?;
    print_rendered_preview(&rendered)
}

pub(super) async fn cmd_preview_mail(
    cfg: &AppConfig,
    mail_id: &str,
    attachment: Option<&str>,
    offline: bool,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let effective_pref = resolve_image_protocol_preference(cfg, protocol)?;
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mail = load_mail_message(&storage, &graph, mail_id, offline).await?;
    let att = select_mail_attachment(&mail, None, attachment)?;
    let (_name, _ct, bytes) = load_mail_attachment_bytes(&graph, &mail.id, att, offline).await?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, effective_pref)?;
    print_rendered_preview(&rendered)
}

// ---------------------------------------------------------------------------
// Shared `pub(crate)` helpers reused by `serve.rs` and `mcp.rs`
// ---------------------------------------------------------------------------
