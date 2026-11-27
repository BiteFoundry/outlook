//! Calendar listing, detail, creation, update, RSVP, and deletion commands.

use anyhow::{bail, Result};
use serde_json::json;

use crate::{
    config::AppConfig,
    graph::calendar::CalendarEditTime,
    models::{CalendarEvent, OutgoingEvent},
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
};

use super::{
    args::DELETE_JSON_REQUIRES_YES,
    format::{
        clamp_limit, default_calendar_window, delete_gate, normalize_rsvp_response, prompt,
        wrap_graph_permission_error,
    },
    runtime::{
        check_recipient_addresses, cli_cache_ttl_secs, is_cursor_fresh, open_runtime,
        open_storage_only, record_cli_sync_cursor, resolve_daemon_warm,
        rsvp_action_to_graph_status,
    },
};

pub(crate) fn event_json(ev: &CalendarEvent, compact: bool) -> serde_json::Value {
    let mut val = json!(ev);
    if compact {
        if let Some(obj) = val.as_object_mut() {
            obj.remove("body_html");
        }
    }
    val
}

pub(super) fn print_cal_events_output(
    start_iso: &str,
    end_iso: &str,
    events: &[CalendarEvent],
    has_more: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    if json_out {
        let events_json: Vec<serde_json::Value> = events
            .iter()
            .map(|ev| event_json(ev, compact))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "start": start_iso,
                "end": end_iso,
                "events": events_json,
                "returned": events.len(),
                "has_more": has_more,
            }))?
        );
    } else if events.is_empty() {
        println!("No calendar events found.");
    } else {
        for ev in events {
            let loc = ev
                .location
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(" @ {}", sanitize_terminal_inline(s)))
                .unwrap_or_default();
            let meet = if ev.is_online_meeting { " [Online]" } else { "" };
            println!(
                "[{} ~ {}] {}{}{} ({})",
                sanitize_terminal_inline(&ev.start_iso),
                sanitize_terminal_inline(&ev.end_iso),
                sanitize_terminal_inline(&ev.subject),
                loc,
                meet,
                sanitize_terminal_inline(&ev.id)
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_calendar_list(
    cfg: &AppConfig,
    from: Option<&str>,
    to: Option<&str>,
    days: i64,
    limit: usize,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let (start_iso, end_iso) = default_calendar_window(from, to, days)?;

    if offline {
        let storage = open_storage_only(cfg)?;
        let mut events =
            storage.list_calendar_events(Some(&start_iso), Some(&end_iso), limit.saturating_add(1))?;
        let has_more = events.len() > limit;
        if has_more {
            events.truncate(limit);
        }
        return print_cal_events_output(&start_iso, &end_iso, &events, has_more, json_out, compact);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, false, cached).await;
    if cached || daemon_warm || ttl > 0 {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:cal"], ttl) {
                let mut events = storage.list_calendar_events(
                    Some(&start_iso),
                    Some(&end_iso),
                    limit.saturating_add(1),
                )?;
                let has_more = events.len() > limit;
                if has_more {
                    events.truncate(limit);
                }
                let has_any_cached =
                    !events.is_empty() || !storage.list_calendar_events(None, None, 1)?.is_empty();
                if !events.is_empty() || (cached && has_any_cached) {
                    return print_cal_events_output(
                        &start_iso,
                        &end_iso,
                        &events,
                        has_more,
                        json_out,
                        compact,
                    );
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let (events, reached_end) = match graph
        .list_calendar_view_complete(&start_iso, &end_iso, limit)
        .await
    {
        Ok(ev) => ev,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    if reached_end {
        let _ = storage.replace_calendar_events_in_window(&start_iso, &end_iso, &events);
    } else {
        let _ = storage.upsert_calendar_events(&events);
    }
    record_cli_sync_cursor(&storage, "cli:cal");

    print_cal_events_output(
        &start_iso,
        &end_iso,
        &events,
        !reached_end,
        json_out,
        compact,
    )
}

pub(super) fn print_event_show_output(ev: &CalendarEvent, json_out: bool, compact: bool) -> Result<()> {
    if json_out {
        let mut val = event_json(ev, compact);
        if compact {
            if let Some(obj) = val.as_object_mut() {
                obj.insert("omitted".into(), json!({ "fields": ["body_html"] }));
                obj.insert(
                    "hint".into(),
                    json!(format!("outlook event {} --json --full", ev.id)),
                );
            }
        }
        let payload = json!({ "event": val });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("Subject: {}", sanitize_terminal_inline(&ev.subject));
        println!(
            "Time: {} ~ {} ({})",
            sanitize_terminal_inline(&ev.start_iso),
            sanitize_terminal_inline(&ev.end_iso),
            sanitize_terminal_inline(&ev.time_zone)
        );
        if let Some(ref loc) = ev.location {
            println!("Location: {}", sanitize_terminal_inline(loc));
        }
        if let Some(org) = ev.organizer_name.as_deref().or(ev.organizer_email.as_deref()) {
            println!("Organizer: {}", sanitize_terminal_inline(org));
        }
        println!("Response: {}", sanitize_terminal_inline(&ev.response_status));
        if let Some(ref url) = ev.online_meeting_url {
            println!("Online Meeting: {}", sanitize_terminal_inline(url));
        }
        if !ev.attendees.is_empty() {
            println!("Attendees ({}):", ev.attendees.len());
            for a in &ev.attendees {
                println!(
                    "  - {} <{}> [{} / {}]",
                    sanitize_terminal_inline(a.name.as_deref().unwrap_or(&a.address)),
                    sanitize_terminal_inline(&a.address),
                    sanitize_terminal_inline(&a.attendee_type),
                    sanitize_terminal_inline(&a.response_status)
                );
            }
        }
        if !ev.body_plain.trim().is_empty() {
            println!("---\n{}", sanitize_terminal_text(ev.body_plain.trim()));
        }
    }
    Ok(())
}

pub(super) async fn cmd_event_show(
    cfg: &AppConfig,
    event_id: &str,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    if offline {
        let storage = open_storage_only(cfg)?;
        let events = storage.list_calendar_events(None, None, 10_000)?;
        let ev = resolver::resolve_calendar_event_from_slice(&events, event_id)?;
        return print_event_show_output(&ev, json_out, compact);
    }

    if cached {
        if let Ok(storage) = open_storage_only(cfg) {
            let events = storage.list_calendar_events(None, None, 10_000)?;
            if let Ok(ev) = resolver::resolve_calendar_event_from_slice(&events, event_id) {
                return print_event_show_output(&ev, json_out, compact);
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let ev = match graph.get_calendar_event(event_id).await {
        Ok(e) => e,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&ev));
    print_event_show_output(&ev, json_out, compact)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_event_create(
    cfg: &AppConfig,
    subject: &str,
    start: &str,
    end: &str,
    attendees: Vec<String>,
    location: Option<String>,
    body: Option<String>,
    is_online_meeting: bool,
    is_all_day: bool,
    time_zone: Option<String>,
    json_out: bool,
) -> Result<()> {
    if subject.trim().is_empty() {
        bail!("Event subject is empty.");
    }
    check_recipient_addresses(&attendees)?;
    let (start_iso, end_iso, time_zone) = crate::graph::calendar::resolve_new_event_times(
        start,
        end,
        is_all_day,
        time_zone.as_deref(),
    )?;
    let body_html = body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|md| crate::parser::compile_outgoing_mail_html(md, &[], None));

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let outgoing = OutgoingEvent {
        subject: subject.to_string(),
        start_iso,
        end_iso,
        time_zone,
        is_all_day,
        location,
        body_html,
        attendees,
        is_online_meeting,
    };
    let created = match graph.create_calendar_event(&outgoing).await {
        Ok(ev) => ev,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event": created,
            }))?
        );
    } else {
        println!(
            "Created event: {} ({} ~ {}) id={}",
            sanitize_terminal_inline(&created.subject),
            sanitize_terminal_inline(&created.start_iso),
            sanitize_terminal_inline(&created.end_iso),
            sanitize_terminal_inline(&created.id)
        );
        if let Some(ref url) = created.online_meeting_url {
            println!("Online meeting link: {}", sanitize_terminal_inline(url));
        }
    }
    Ok(())
}

pub(super) async fn cmd_update_event(
    cfg: &AppConfig,
    id: &str,
    subject: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let start = start.map(CalendarEditTime::parse).transpose()?;
    let end = end.map(CalendarEditTime::parse).transpose()?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let event = match graph
        .edit_calendar_event(id, subject, start.as_ref(), end.as_ref())
        .await
    {
        Ok(updated) => updated,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&event));
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "event": event }))?
        );
    } else {
        println!(
            "Updated event {} ({}).",
            sanitize_terminal_inline(&event.subject),
            sanitize_terminal_inline(&event.id)
        );
    }
    Ok(())
}

pub(super) async fn cmd_event_rsvp(
    cfg: &AppConfig,
    event_id: &str,
    response: &str,
    comment: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let normalized = normalize_rsvp_response(response)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    if let Err(err) = graph
        .respond_calendar_event(event_id, normalized, comment)
        .await
    {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.update_event_response_local(event_id, rsvp_action_to_graph_status(normalized));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": event_id,
                "response": normalized,
            }))?
        );
    } else {
        println!("Event RSVP sent: {event_id} -> {normalized}");
    }
    Ok(())
}

pub(super) async fn cmd_event_delete(
    cfg: &AppConfig,
    event_id: &str,
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
            "Delete event {event_id}? Type 'yes' to confirm: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    if let Err(err) = graph.delete_calendar_event(event_id).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.delete_calendar_event(event_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": event_id,
            }))?
        );
    } else {
        println!("Deleted event: {event_id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// To Do & Contacts commands
// ---------------------------------------------------------------------------
