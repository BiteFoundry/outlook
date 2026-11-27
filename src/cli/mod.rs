//! `outlook` CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

mod account;
mod args;
mod calendar;
mod format;
mod mail;
mod ops;
mod parse;
mod runtime;
mod tasks_contacts;

#[cfg(test)]
mod tests;

pub use args::{Cli, Command, DaemonCommand};
pub use parse::{parse_cli_from, try_fast_help_or_version};

pub(crate) use account::auth_source_label;
pub(crate) use calendar::event_json;
pub(crate) use format::{
    clamp_limit, cursor_scope, default_calendar_window, format_downloaded_attachment_json,
    is_forbidden_error, load_mail_attachment_bytes, mail_cursor_scope, normalize_datetime_iso,
    normalize_mail_importance, normalize_rsvp_response, normalize_todo_due_iso,
    normalize_todo_importance, offset_page, select_mail_attachment, wrap_graph_permission_error,
    CursorPos, PageCursor, FILTER_SCAN_MAX_PAGES,
};
pub(crate) use mail::{
    draft_edit_html, load_cached_mail_list, mail_attachments_public_json, mail_list_json,
    mail_message_public_json, mail_read_json, with_sync_complete_marker, MailListPage, Surface,
};
pub(crate) use runtime::{
    check_recipient_addresses, cli_cache_ttl_secs, error_code, error_with_info, is_cursor_fresh,
    json_error_payload, open_runtime, record_cli_sync_cursor, rsvp_action_to_graph_status,
};
pub(crate) use tasks_contacts::{directory_status_json, normalize_todo_edit_due};

use anyhow::Result;

use crate::{
    auth::{read_external_token_file, LoginOptions},
    config::{AppConfig, Persona},
    serve::{self, ServeOptions},
};

use account::{cmd_doctor, cmd_login, cmd_logout, cmd_tui, cmd_whoami};
use calendar::{
    cmd_calendar_list, cmd_event_create, cmd_event_delete, cmd_event_rsvp, cmd_event_show,
    cmd_update_event,
};
use mail::{
    cmd_edit_draft, cmd_folders, cmd_mail_delete, cmd_mail_download, cmd_mail_forward,
    cmd_mail_list, cmd_mail_mark, cmd_mail_move, cmd_mail_read, cmd_mail_reply, cmd_mail_search,
    cmd_mail_send,
};
use ops::{
    cmd_preview_local_file, cmd_preview_mail, cmd_sync, cmd_update, dispatch_daemon_command,
};
use tasks_contacts::{
    cmd_contacts, cmd_todo_add, cmd_todo_delete, cmd_todo_done, cmd_todo_lists, cmd_todo_tasks,
    cmd_update_task,
};

pub async fn run_from_env() -> Result<()> {
    let persona = Persona::detect();
    let cli = parse_cli_from(std::env::args_os()).unwrap_or_else(|err| err.exit());
    let json_out = cli.command.as_ref().is_some_and(Command::json_requested);
    let mut cfg = AppConfig::load();
    let res = Box::pin(dispatch_command(&mut cfg, persona, cli.command)).await;
    if let Err(ref err) = res {
        if json_out {
            if let Ok(serialized) = serde_json::to_string_pretty(&json_error_payload(err)) {
                println!("{serialized}");
            }
        }
    }
    res
}

async fn dispatch_command(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<Command>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Login {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            access_token,
            token_file,
            port,
            json,
        }) => {
            let resolved_access_token = match (access_token, token_file) {
                (Some(tok), _) => Some(tok),
                (None, Some(path)) => {
                    let loaded = read_external_token_file(&path, cfg)?;
                    Some(loaded.access_token)
                }
                (None, None) => None,
            };
            Box::pin(cmd_login(
                cfg,
                LoginOptions {
                    client_id,
                    tenant_id,
                    device_code,
                    client_secret,
                    access_token: resolved_access_token,
                    port,
                },
                json,
            ))
            .await
        }
        Some(Command::Logout { json }) => Box::pin(cmd_logout(cfg, json)).await,
        Some(Command::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(Command::Doctor { online, json }) => {
            Box::pin(cmd_doctor(cfg, persona, online, json)).await
        }
        Some(Command::Folders {
            refresh,
            offline,
            cached,
            json,
            limit,
            cursor,
        }) => {
            Box::pin(cmd_folders(
                cfg,
                refresh,
                offline,
                cached,
                json,
                limit,
                cursor.as_deref(),
            ))
            .await
        }
        Some(Command::Mail {
            folder,
            limit,
            unread,
            flagged,
            focused,
            refresh,
            offline,
            cached,
            json,
            full,
            cursor,
            compact: _,
        }) => {
            Box::pin(cmd_mail_list(
                cfg,
                &folder,
                limit,
                unread,
                flagged,
                focused,
                refresh,
                offline,
                cached,
                json,
                !full,
                cursor.as_deref(),
            ))
            .await
        }
        Some(Command::Read {
            message_id,
            raw_html,
            mark_read,
            preview_images,
            offline,
            cached,
            json,
            full,
            compact: _,
        }) => {
            Box::pin(cmd_mail_read(
                cfg,
                &message_id,
                raw_html,
                mark_read,
                preview_images,
                offline,
                cached,
                json,
                !full,
            ))
            .await
        }
        Some(Command::Send {
            to,
            cc,
            bcc,
            subject,
            body,
            attach,
            inline,
            importance,
            json,
        }) => {
            Box::pin(cmd_mail_send(
                cfg,
                to,
                cc,
                bcc,
                &subject,
                body,
                attach,
                inline,
                &importance,
                json,
            ))
            .await
        }
        Some(Command::Reply {
            message_id,
            body_md,
            all,
            json,
        }) => Box::pin(cmd_mail_reply(cfg, &message_id, &body_md, all, json)).await,
        Some(Command::Forward {
            message_id,
            to,
            comment_md,
            json,
        }) => {
            Box::pin(cmd_mail_forward(
                cfg,
                &message_id,
                to,
                comment_md.as_deref().unwrap_or(""),
                json,
            ))
            .await
        }
        Some(Command::EditDraft {
            message_id,
            subject,
            body,
            body_html,
            json,
        }) => {
            Box::pin(cmd_edit_draft(
                cfg,
                &message_id,
                subject.as_deref(),
                body.as_deref(),
                body_html.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Mark {
            message_id,
            read,
            unread,
            flag,
            unflag,
            json,
        }) => {
            let is_read = if read {
                Some(true)
            } else if unread {
                Some(false)
            } else {
                None
            };
            let flagged = if flag {
                Some(true)
            } else if unflag {
                Some(false)
            } else {
                None
            };
            Box::pin(cmd_mail_mark(cfg, &message_id, is_read, flagged, json)).await
        }
        Some(Command::Move {
            message_id,
            dest_folder,
            json,
            full,
        }) => Box::pin(cmd_mail_move(cfg, &message_id, &dest_folder, json, full)).await,
        Some(Command::Archive {
            message_id,
            json,
            full,
        }) => Box::pin(cmd_mail_move(cfg, &message_id, "archive", json, full)).await,
        Some(Command::Delete {
            message_id,
            yes,
            json,
        }) => Box::pin(cmd_mail_delete(cfg, &message_id, yes, json)).await,
        Some(Command::Search {
            query,
            folder,
            remote,
            limit,
            json,
            full,
            cursor,
        }) => {
            Box::pin(cmd_mail_search(
                cfg,
                &query,
                folder.as_deref(),
                remote,
                limit,
                json,
                !full,
                cursor.as_deref(),
            ))
            .await
        }
        Some(Command::Download {
            message_id,
            attachment,
            dir,
            offline,
            json,
        }) => {
            Box::pin(cmd_mail_download(
                cfg,
                &message_id,
                attachment.as_deref(),
                dir.as_deref(),
                offline,
                json,
            ))
            .await
        }
        Some(Command::Preview {
            message_id,
            attachment,
            offline,
            cols,
            rows,
            protocol,
        }) => {
            Box::pin(cmd_preview_mail(
                cfg,
                &message_id,
                attachment.as_deref(),
                offline,
                cols,
                rows,
                protocol.as_deref(),
            ))
            .await
        }
        Some(Command::PreviewFile {
            image_path,
            cols,
            rows,
            protocol,
        }) => cmd_preview_local_file(cfg, &image_path, cols, rows, protocol.as_deref()),
        Some(Command::Calendar {
            days,
            from,
            to,
            limit,
            offline,
            cached,
            json,
            full,
        }) => {
            Box::pin(cmd_calendar_list(
                cfg,
                from.as_deref(),
                to.as_deref(),
                days,
                limit,
                offline,
                cached,
                json,
                !full,
            ))
            .await
        }
        Some(Command::Event {
            event_id,
            offline,
            cached,
            json,
            full,
            compact: _,
        }) => Box::pin(cmd_event_show(cfg, &event_id, offline, cached, json, !full)).await,
        Some(Command::CreateEvent {
            subject,
            start,
            end,
            location,
            attendee,
            body,
            online,
            all_day,
            time_zone,
            json,
        }) => {
            Box::pin(cmd_event_create(
                cfg, &subject, &start, &end, attendee, location, body, online, all_day, time_zone,
                json,
            ))
            .await
        }
        Some(Command::UpdateEvent {
            event_id,
            subject,
            start,
            end,
            json,
        }) => {
            Box::pin(cmd_update_event(
                cfg,
                &event_id,
                subject.as_deref(),
                start.as_deref(),
                end.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Rsvp {
            event_id,
            response,
            comment,
            json,
        }) => {
            Box::pin(cmd_event_rsvp(
                cfg,
                &event_id,
                &response,
                comment.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::DeleteEvent {
            event_id,
            yes,
            json,
        }) => Box::pin(cmd_event_delete(cfg, &event_id, yes, json)).await,
        Some(Command::Tasks {
            list,
            all,
            limit,
            refresh,
            offline,
            cached,
            json,
        }) => {
            Box::pin(cmd_todo_tasks(
                cfg, &list, all, limit, refresh, offline, cached, json,
            ))
            .await
        }
        Some(Command::TaskLists {
            refresh,
            offline,
            cached,
            json,
            limit,
            cursor,
        }) => {
            Box::pin(cmd_todo_lists(
                cfg,
                refresh,
                offline,
                cached,
                json,
                limit,
                cursor.as_deref(),
            ))
            .await
        }
        Some(Command::AddTask {
            title,
            list,
            due,
            importance,
            notes,
            json,
        }) => {
            Box::pin(cmd_todo_add(
                cfg,
                &title,
                &list,
                due.as_deref(),
                &importance,
                notes,
                json,
            ))
            .await
        }
        Some(Command::Done {
            task_id,
            list,
            undo,
            json,
        }) => Box::pin(cmd_todo_done(cfg, &task_id, &list, !undo, json)).await,
        Some(Command::UpdateTask {
            task_id,
            list,
            title,
            due,
            json,
        }) => {
            Box::pin(cmd_update_task(
                cfg,
                &task_id,
                &list,
                title.as_deref(),
                due.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::DeleteTask {
            task_id,
            list,
            yes,
            json,
        }) => Box::pin(cmd_todo_delete(cfg, &task_id, &list, yes, json)).await,
        Some(Command::Contacts {
            query,
            limit,
            refresh,
            offline,
            cached,
            json,
        }) => {
            Box::pin(cmd_contacts(
                cfg,
                query.as_deref(),
                limit,
                refresh,
                offline,
                cached,
                json,
            ))
            .await
        }
        Some(Command::Sync { days, limit, json }) => {
            Box::pin(cmd_sync(cfg, days, limit, json)).await
        }
        Some(Command::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(Command::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Serve {
            bind,
            port,
            api_key,
            allow_keyless,
            cors_origin,
        }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions {
                    bind,
                    port,
                    api_key,
                    allow_keyless,
                    cors_origins: cors_origin,
                },
            ))
            .await
        }
        Some(Command::Mcp) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(crate::mcp::run(cfg.clone(), graph, storage)).await
        }
        Some(Command::Daemon { command }) => Box::pin(dispatch_daemon_command(cfg, command)).await,
    }
}

// ---------------------------------------------------------------------------
// Daemon management
// ---------------------------------------------------------------------------
