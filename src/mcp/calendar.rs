//! Calendar MCP tool handlers and response projection helpers.

use serde_json::{json, Value};

use crate::{
    cli,
    graph::{calendar::CalendarEditTime, GraphClient},
    models::OutgoingEvent,
    storage::Storage,
};

use super::{
    args::{bounds, Args},
    format_error, map_graph_result, with_storage,
};

const COMPACT_EVENT_LIST_OMITTED_FIELDS: &[&str] = &["body_html", "recurrence"];

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

pub(super) async fn handle_list_events(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_get_event(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_create_event(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_update_event(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_respond_event(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn handle_delete_event(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
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
