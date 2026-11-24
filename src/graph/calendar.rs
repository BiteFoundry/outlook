//! Microsoft Graph v1.0 Outlook Calendar operations (`/me/calendarView`, `/me/events`, RSVP, Teams online meetings).

use std::borrow::Cow;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Deserialize;

use super::{
    client::{GraphDeltaPage, GraphListPage},
    mail::{extract_teams_meetup_url, normalize_outlook_body, GraphOutlookBody, GraphRecipient},
    GraphClient,
};
use crate::models::{CalendarEvent, EventAttendee, EventRsvpAction, OutgoingEvent};

/// Allows `GraphClient::respond_calendar_event` to accept either `EventRsvpAction` or `&str` / `String`.
pub trait IntoEventRsvp {
    fn into_rsvp_str(self) -> Cow<'static, str>;
}

impl IntoEventRsvp for EventRsvpAction {
    fn into_rsvp_str(self) -> Cow<'static, str> {
        Cow::Borrowed(self.as_graph_endpoint())
    }
}

impl IntoEventRsvp for &EventRsvpAction {
    fn into_rsvp_str(self) -> Cow<'static, str> {
        Cow::Borrowed(self.as_graph_endpoint())
    }
}

impl IntoEventRsvp for &str {
    fn into_rsvp_str(self) -> Cow<'static, str> {
        Cow::Owned(self.to_string())
    }
}

impl IntoEventRsvp for String {
    fn into_rsvp_str(self) -> Cow<'static, str> {
        Cow::Owned(self)
    }
}

impl IntoEventRsvp for &String {
    fn into_rsvp_str(self) -> Cow<'static, str> {
        Cow::Owned(self.clone())
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphDateTimeTimeZone {
    #[serde(rename = "dateTime", default)]
    date_time: Option<String>,
    #[serde(rename = "timeZone", default)]
    time_zone: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphLocation {
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphResponseStatus {
    #[serde(default)]
    response: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphAttendee {
    #[serde(rename = "emailAddress", default)]
    email_address: Option<super::mail::GraphEmailAddress>,
    #[serde(rename = "type", default)]
    attendee_type: Option<String>,
    #[serde(default)]
    status: Option<GraphResponseStatus>,
}

impl GraphAttendee {
    fn into_event_attendee(self) -> Option<EventAttendee> {
        let ea = self.email_address?;
        let address = ea.address?.trim().to_string();
        if address.is_empty() {
            return None;
        }
        let name = ea.name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Some(EventAttendee {
            name,
            address,
            attendee_type: self
                .attendee_type
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "required".to_string()),
            response_status: self
                .status
                .and_then(|s| s.response)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "none".to_string()),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphOnlineMeetingInfo {
    #[serde(rename = "joinUrl", default)]
    join_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphCalendarEvent {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    start: Option<GraphDateTimeTimeZone>,
    #[serde(default)]
    end: Option<GraphDateTimeTimeZone>,
    #[serde(rename = "isAllDay", default)]
    is_all_day: Option<bool>,
    #[serde(rename = "type", default)]
    event_type: Option<String>,
    #[serde(rename = "originalStartTimeZone", default)]
    original_start_zone: Option<String>,
    #[serde(rename = "originalEndTimeZone", default)]
    original_end_zone: Option<String>,
    #[serde(default)]
    location: Option<GraphLocation>,
    #[serde(default)]
    organizer: Option<GraphRecipient>,
    #[serde(default)]
    attendees: Vec<GraphAttendee>,
    #[serde(rename = "responseStatus", default)]
    response_status: Option<GraphResponseStatus>,
    #[serde(rename = "isOnlineMeeting", default)]
    is_online_meeting: Option<bool>,
    #[serde(rename = "onlineMeeting", default)]
    online_meeting: Option<GraphOnlineMeetingInfo>,
    #[serde(rename = "onlineMeetingUrl", default)]
    online_meeting_url: Option<String>,
    #[serde(rename = "bodyPreview", default)]
    body_preview: Option<String>,
    #[serde(default)]
    body: Option<GraphOutlookBody>,
    #[serde(rename = "isCancelled", default)]
    is_cancelled: Option<bool>,
    #[serde(rename = "webLink", default)]
    web_link: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(rename = "showAs", default)]
    show_as: Option<String>,
    #[serde(default)]
    sensitivity: Option<String>,
    #[serde(rename = "hasAttachments", default)]
    has_attachments: Option<bool>,
    #[serde(default)]
    recurrence: Option<serde_json::Value>,
    #[serde(rename = "seriesMasterId", default)]
    series_master_id: Option<String>,
}

/// Normalises a Graph `dateTimeTimeZone.dateTime` value for a timed event into RFC3339 UTC.
///
/// Graph returns offset-less values (`2026-11-23T05:00:00.0000000`) when the request carries
/// `Prefer: outlook.timezone="UTC"`. Every consumer parses RFC3339, so the offset is made explicit here.
/// All-day values are floating dates and are never shifted; values in a non-UTC zone are left as they are.
pub(crate) fn normalize_graph_event_time(raw: &str, time_zone: &str, is_all_day: bool) -> String {
    if is_all_day || DateTime::parse_from_rfc3339(raw).is_ok() {
        return raw.to_string();
    }
    let zone = time_zone.trim();
    let is_utc = zone.is_empty()
        || zone.eq_ignore_ascii_case("UTC")
        || zone.eq_ignore_ascii_case("Etc/UTC");
    if !is_utc {
        return raw.to_string();
    }
    match NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f") {
        Ok(naive) => naive
            .and_utc()
            .to_rfc3339_opts(SecondsFormat::AutoSi, true),
        Err(_) => raw.to_string(),
    }
}

pub(crate) enum CalendarEditTime {
    Date(NaiveDate),
    WallClock(NaiveDateTime),
    Instant(DateTime<Utc>),
}

impl CalendarEditTime {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Ok(value) = DateTime::parse_from_rfc3339(raw) {
            return Ok(Self::Instant(value.with_timezone(&Utc)));
        }
        if let Ok(value) = raw.parse::<NaiveDate>() {
            return Ok(Self::Date(value));
        }
        for format in [
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%dT%H:%M",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%d_%H:%M:%S",
            "%Y-%m-%d %H:%M",
            "%Y-%m-%d_%H:%M",
        ] {
            if let Ok(value) = NaiveDateTime::parse_from_str(raw, format) {
                return Ok(Self::WallClock(value));
            }
        }
        bail!("Invalid event time '{raw}'. Use RFC3339, local YYYY-MM-DDTHH:MM, or YYYY-MM-DD for all-day events.")
    }

    fn graph_value(&self, event: &GraphCalendarEvent, start: bool) -> Result<serde_json::Value> {
        let (origin, current) = if start {
            (&event.original_start_zone, &event.start)
        } else {
            (&event.original_end_zone, &event.end)
        };
        let event_zone = origin
            .as_deref()
            .or_else(|| current.as_ref()?.time_zone.as_deref())
            .filter(|zone| !zone.is_empty() && *zone != "tzone://Microsoft/Custom");
        let all_day = event.is_all_day.unwrap_or(false);
        let date_time;
        let zone = match self {
            Self::Date(date) if all_day => {
                date_time = format!("{date}T00:00:00");
                event_zone.context("Outlook did not return a supported all-day event time zone.")?
            }
            Self::WallClock(time) if !all_day => {
                date_time = time.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
                event_zone.context("Outlook did not return a supported event time zone. Use RFC3339 for a single event.")?
            }
            Self::Instant(time) if !all_day => {
                if matches!(
                    event.event_type.as_deref(),
                    Some("seriesMaster" | "occurrence" | "exception")
                ) && !matches!(event_zone, Some("UTC" | "Etc/UTC"))
                {
                    bail!("Use local wall-clock times to keep this recurring event in its original time zone.");
                }
                date_time = time.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
                "UTC"
            }
            _ if all_day => bail!("Use YYYY-MM-DD when updating an all-day event."),
            _ => bail!("Use a date and time when updating a timed event."),
        };
        Ok(serde_json::json!({ "dateTime": date_time, "timeZone": zone }))
    }
}

/// IANA name of the machine's time zone, or `UTC` when it cannot be determined (stripped containers).
fn system_time_zone() -> String {
    iana_time_zone::get_timezone()
        .ok()
        .map(|zone| zone.trim().to_string())
        .filter(|zone| !zone.is_empty())
        .unwrap_or_else(|| "UTC".to_string())
}

fn is_utc_zone(zone: &str) -> bool {
    zone.eq_ignore_ascii_case("UTC") || zone.eq_ignore_ascii_case("Etc/UTC")
}

/// Resolves `create-event` input into the `(start, end, timeZone)` triple sent to Graph.
///
/// - All-day: `YYYY-MM-DD` for both ends (end exclusive), sent as floating midnights in `time_zone`
///   (or the system zone). Graph rejects all-day events whose boundaries are not midnight.
/// - Timed without `time_zone`: unchanged from 0.3.0 (machine-local or RFC3339 input, sent as a UTC instant).
/// - Timed with `time_zone`: wall-clock input is read in that zone and sent unconverted; RFC3339 input is an
///   exact instant and is sent in UTC. A `Z`-suffixed value is never paired with a non-UTC zone.
pub(crate) fn resolve_new_event_times(
    start: &str,
    end: &str,
    is_all_day: bool,
    time_zone: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    let zone = time_zone.map(str::trim).filter(|zone| !zone.is_empty());

    if is_all_day {
        let parse_date = |raw: &str| {
            raw.trim().parse::<NaiveDate>().ok().context(
                "Use YYYY-MM-DD for --all-day events (end date is exclusive).",
            )
        };
        let start_date = parse_date(start)?;
        let end_date = parse_date(end)?;
        if end_date <= start_date {
            let next_day = start_date.succ_opt().unwrap_or(start_date);
            bail!("All-day end date is exclusive; use {next_day} for a one-day event.");
        }
        let zone = zone.map(str::to_string).unwrap_or_else(system_time_zone);
        return Ok((
            format!("{start_date}T00:00:00"),
            format!("{end_date}T00:00:00"),
            Some(zone),
        ));
    }

    let Some(zone) = zone else {
        return Ok((
            crate::cli::normalize_datetime_iso(start)?,
            crate::cli::normalize_datetime_iso(end)?,
            None,
        ));
    };

    let wall_clock = |time: &CalendarEditTime| match time {
        CalendarEditTime::WallClock(value) => Some(*value),
        CalendarEditTime::Date(date) => date.and_hms_opt(0, 0, 0),
        CalendarEditTime::Instant(_) => None,
    };
    let start_time = CalendarEditTime::parse(start)?;
    let end_time = CalendarEditTime::parse(end)?;
    match (wall_clock(&start_time), wall_clock(&end_time)) {
        (Some(start_wall), Some(end_wall)) => {
            let format = |value: NaiveDateTime| value.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
            Ok((format(start_wall), format(end_wall), Some(zone.to_string())))
        }
        (None, None) => Ok((
            crate::cli::normalize_datetime_iso(start)?,
            crate::cli::normalize_datetime_iso(end)?,
            Some("UTC".to_string()),
        )),
        _ => bail!(
            "With --time-zone, give --start and --end in the same form: both wall-clock (YYYY-MM-DD HH:MM, read in {zone}) or both RFC3339."
        ),
    }
}

impl From<GraphCalendarEvent> for CalendarEvent {
    fn from(raw: GraphCalendarEvent) -> Self {
        let preview = raw.body_preview.unwrap_or_default();
        let (body_html, body_plain) = normalize_outlook_body(raw.body.as_ref(), &preview);

        let online_meeting_url = raw
            .online_meeting
            .and_then(|m| m.join_url)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                raw.online_meeting_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| extract_teams_meetup_url(&body_html))
            .or_else(|| extract_teams_meetup_url(&body_plain));

        let is_online_meeting =
            raw.is_online_meeting.unwrap_or(false) || online_meeting_url.is_some();

        let is_all_day = raw.is_all_day.unwrap_or(false);
        let endpoint_iso = |endpoint: Option<&GraphDateTimeTimeZone>| {
            let Some(endpoint) = endpoint else {
                return String::new();
            };
            let value = endpoint.date_time.as_deref().unwrap_or_default();
            normalize_graph_event_time(
                value,
                endpoint.time_zone.as_deref().unwrap_or_default(),
                is_all_day,
            )
        };
        let start_iso = endpoint_iso(raw.start.as_ref());
        let end_iso = endpoint_iso(raw.end.as_ref());
        let time_zone = raw
            .start
            .as_ref()
            .and_then(|s| s.time_zone.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "UTC".to_string());

        let (organizer_name, organizer_email) = match raw
            .organizer
            .and_then(GraphRecipient::into_email_recipient)
        {
            Some(r) => (r.name, Some(r.address)),
            None => (None, None),
        };

        let categories: Vec<String> = raw
            .categories
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let show_as = raw
            .show_as
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let sensitivity = raw
            .sensitivity
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let event_type = raw
            .event_type
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let has_attachments = raw.has_attachments.unwrap_or(false);
        let recurrence = raw.recurrence.filter(|v| !v.is_null());
        let is_recurring = recurrence.is_some()
            || matches!(
                event_type.as_deref(),
                Some("seriesMaster" | "occurrence" | "exception")
            )
            || raw
                .series_master_id
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());

        Self {
            id: raw.id,
            subject: raw.subject.unwrap_or_default(),
            start_iso,
            end_iso,
            time_zone,
            is_all_day,
            location: raw
                .location
                .and_then(|l| l.display_name)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            organizer_name,
            organizer_email,
            attendees: raw
                .attendees
                .into_iter()
                .filter_map(GraphAttendee::into_event_attendee)
                .collect(),
            response_status: raw
                .response_status
                .and_then(|s| s.response)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "none".to_string()),
            is_online_meeting,
            online_meeting_url,
            body_html,
            body_plain,
            is_cancelled: raw.is_cancelled.unwrap_or(false),
            web_link: raw.web_link.filter(|s| !s.trim().is_empty()),
            categories,
            show_as,
            sensitivity,
            event_type,
            has_attachments,
            is_recurring,
            recurrence,
        }
    }
}

fn build_event_payload(event: &OutgoingEvent) -> serde_json::Value {
    let tz = event
        .time_zone
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("UTC");
    debug_assert!(
        is_utc_zone(tz)
            || [&event.start_iso, &event.end_iso]
                .iter()
                .all(|value| DateTime::parse_from_rfc3339(value).is_err()),
        "an offset-suffixed dateTime must only be paired with the UTC time zone"
    );

    let mut payload = serde_json::json!({
        "subject": event.subject,
        "start": {
            "dateTime": event.start_iso,
            "timeZone": tz,
        },
        "end": {
            "dateTime": event.end_iso,
            "timeZone": tz,
        },
        "isAllDay": event.is_all_day,
    });

    if let Some(loc) = event
        .location
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload["location"] = serde_json::json!({ "displayName": loc });
    }

    if let Some(body_html) = event
        .body_html
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        payload["body"] = serde_json::json!({
            "contentType": "HTML",
            "content": body_html,
        });
    }

    let attendees: Vec<serde_json::Value> = event
        .attendees
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|addr| {
            serde_json::json!({
                "emailAddress": { "address": addr },
                "type": "required",
            })
        })
        .collect();
    if !attendees.is_empty() {
        payload["attendees"] = serde_json::Value::Array(attendees);
    }

    if event.is_online_meeting {
        payload["isOnlineMeeting"] = serde_json::Value::Bool(true);
        payload["onlineMeetingProvider"] =
            serde_json::Value::String("teamsForBusiness".to_string());
    }

    payload
}

impl GraphClient {
    pub async fn list_calendar_view(
        &self,
        start_iso: &str,
        end_iso: &str,
        limit: usize,
    ) -> Result<Vec<CalendarEvent>> {
        let (events, _) = self.list_calendar_view_complete(start_iso, end_iso, limit).await?;
        Ok(events)
    }

    pub async fn list_calendar_view_complete(
        &self,
        start_iso: &str,
        end_iso: &str,
        limit: usize,
    ) -> Result<(Vec<CalendarEvent>, bool)> {
        if limit == 0 {
            return Ok((Vec::new(), false));
        }
        let top = limit.min(100);
        let mut next_url = Some(format!(
            "/me/calendarView?startDateTime={}&endDateTime={}&$top={top}&$orderby=start/dateTime asc",
            urlencoding::encode(start_iso.trim()),
            urlencoding::encode(end_iso.trim()),
        ));
        let mut events = Vec::new();
        let headers = [("Prefer", "outlook.timezone=\"UTC\"")];

        while let Some(url) = next_url {
            let page: GraphListPage<GraphCalendarEvent> =
                self.get_json_with_headers(&url, &headers).await?;
            let page_has_more = page.next_link.is_some();
            let total_in_page = page.value.len();
            for (idx, raw) in page.value.into_iter().enumerate() {
                events.push(CalendarEvent::from(raw));
                if events.len() >= limit {
                    let reached_end = !page_has_more && idx + 1 >= total_in_page;
                    return Ok((events, reached_end));
                }
            }
            next_url = page.next_link;
        }

        Ok((events, true))
    }

    pub async fn delta_calendar_view(
        &self,
        start_iso: &str,
        end_iso: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<CalendarEvent>, Vec<String>, Option<String>)> {
        let initial_url = match delta_link.map(str::trim).filter(|s| !s.is_empty()) {
            Some(link) => link.to_string(),
            None => format!(
                "/me/calendarView/delta?startDateTime={}&endDateTime={}",
                urlencoding::encode(start_iso.trim()),
                urlencoding::encode(end_iso.trim()),
            ),
        };

        let headers = [("Prefer", "outlook.timezone=\"UTC\", odata.maxpagesize=50")];
        let mut seen_urls = std::collections::HashSet::new();
        seen_urls.insert(
            super::mail::canonical_pagination_key(&initial_url).to_string(),
        );
        let mut next_url = Some(initial_url);
        let mut upserted = Vec::new();
        let mut removed_ids = Vec::new();
        let mut new_delta_link = None;

        while let Some(url) = next_url {
            let page: GraphDeltaPage<serde_json::Value> =
                self.get_json_with_headers(&url, &headers).await?;
            for val in page.value {
                if val.get("@removed").is_some() {
                    if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                        removed_ids.push(id.to_string());
                    }
                } else {
                    let raw: GraphCalendarEvent = serde_json::from_value(val)
                        .context("Failed to deserialize Graph calendar delta item")?;
                    upserted.push(CalendarEvent::from(raw));
                }
            }
            if let Some(dlink) = page
                .delta_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                new_delta_link = Some(dlink);
                break;
            }
            next_url = match page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                Some(next) => {
                    let key = super::mail::canonical_pagination_key(&next).to_string();
                    if !seen_urls.insert(key) {
                        bail!(
                            "Cyclic or repeated @odata.nextLink detected during calendar delta sync: {next}"
                        );
                    }
                    Some(next)
                }
                None => None,
            };
        }

        Ok((upserted, removed_ids, new_delta_link))
    }

    pub async fn get_calendar_event(&self, event_id: &str) -> Result<CalendarEvent> {
        let path = format!("/me/events/{}", urlencoding::encode(event_id.trim()));
        let headers = [("Prefer", "outlook.timezone=\"UTC\"")];
        let raw: GraphCalendarEvent = self.get_json_with_headers(&path, &headers).await?;
        Ok(CalendarEvent::from(raw))
    }

    pub async fn create_calendar_event(&self, event: &OutgoingEvent) -> Result<CalendarEvent> {
        let payload = build_event_payload(event);
        let created: GraphCalendarEvent = self.post_json("/me/events", &payload).await?;
        Ok(CalendarEvent::from(created))
    }

    pub async fn update_calendar_event(
        &self,
        event_id: &str,
        event: &OutgoingEvent,
    ) -> Result<CalendarEvent> {
        let payload = build_event_payload(event);
        self.patch_calendar_event(event_id, &payload).await
    }

    pub(crate) async fn edit_calendar_event(
        &self,
        event_id: &str,
        subject: Option<&str>,
        start: Option<&CalendarEditTime>,
        end: Option<&CalendarEditTime>,
    ) -> Result<CalendarEvent> {
        if subject.is_none() && start.is_none() && end.is_none() {
            bail!("Provide an event subject, start, or end to update.");
        }
        let mut payload = serde_json::json!({});
        if let Some(subject) = subject {
            payload["subject"] = serde_json::json!(subject);
        }
        if start.is_some() || end.is_some() {
            let path = format!("/me/events/{}?$select=id,start,end,isAllDay,type,originalStartTimeZone,originalEndTimeZone",
                urlencoding::encode(event_id.trim()));
            let current: GraphCalendarEvent = self.get_json(&path).await?;
            if let Some(start) = start {
                payload["start"] = start.graph_value(&current, true)?;
            }
            if let Some(end) = end {
                payload["end"] = end.graph_value(&current, false)?;
            }
            if start.is_some()
                && end.is_some()
                && payload["start"]["timeZone"] == payload["end"]["timeZone"]
                && payload["start"]["dateTime"].as_str() >= payload["end"]["dateTime"].as_str()
            {
                bail!("Event end must be after start.");
            }
        }
        self.patch_calendar_event(event_id, &payload).await
    }

    async fn patch_calendar_event(
        &self,
        event_id: &str,
        payload: &serde_json::Value,
    ) -> Result<CalendarEvent> {
        let path = format!("/me/events/{}", urlencoding::encode(event_id.trim()));
        let updated: Option<GraphCalendarEvent> = self.patch_json_optional(&path, payload).await?;
        match updated {
            Some(ev) => Ok(CalendarEvent::from(ev)),
            None => self.get_calendar_event(event_id).await,
        }
    }

    pub async fn delete_calendar_event(&self, event_id: &str) -> Result<()> {
        let path = format!("/me/events/{}", urlencoding::encode(event_id.trim()));
        self.delete_req(&path).await
    }

    /// Responds to a calendar invitation (`EventRsvpAction` or `"accept"` / `"tentative"` / `"decline"`).
    pub async fn respond_calendar_event(
        &self,
        event_id: &str,
        response: impl IntoEventRsvp,
        comment: Option<&str>,
    ) -> Result<()> {
        let raw_response = response.into_rsvp_str();
        let endpoint = match raw_response.trim().to_ascii_lowercase().as_str() {
            "accept" | "accepted" => "accept",
            "tentative" | "tentativelyaccept" | "tentativelyaccepted" => "tentativelyAccept",
            "decline" | "declined" => "decline",
            other => {
                bail!("Unsupported calendar RSVP response '{other}' (expected accept, tentative, or decline)")
            }
        };
        let path = format!(
            "/me/events/{}/{endpoint}",
            urlencoding::encode(event_id.trim())
        );
        let body = serde_json::json!({
            "comment": comment.unwrap_or(""),
            "sendResponse": true,
        });
        self.post_json_empty(&path, &body).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::mail::tests::test_graph_client;
    use axum::{
        extract::{Path as AxumPath, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{delete, get, post},
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    #[test]
    fn normalize_graph_event_time_makes_utc_explicit_only_for_timed_utc_values() {
        for (raw, zone, all_day, expected) in [
            ("2026-11-23T05:00:00.0000000", "UTC", false, "2026-11-23T05:00:00Z"),
            ("2026-11-23T05:00:00", "utc", false, "2026-11-23T05:00:00Z"),
            ("2026-11-23T05:00:00.0000000", "Etc/UTC", false, "2026-11-23T05:00:00Z"),
            ("2026-11-23T05:00:00.5000000", "", false, "2026-11-23T05:00:00.500Z"),
            ("2026-11-19T10:00:00Z", "UTC", false, "2026-11-19T10:00:00Z"),
            ("2026-11-19T19:00:00+09:00", "UTC", false, "2026-11-19T19:00:00+09:00"),
            (
                "2026-11-23T09:00:00.0000000",
                "Pacific Standard Time",
                false,
                "2026-11-23T09:00:00.0000000",
            ),
            ("2026-11-23T00:00:00.0000000", "UTC", true, "2026-11-23T00:00:00.0000000"),
            ("", "UTC", false, ""),
            ("not a time", "UTC", false, "not a time"),
        ] {
            assert_eq!(
                normalize_graph_event_time(raw, zone, all_day),
                expected,
                "raw={raw:?} zone={zone:?} all_day={all_day}"
            );
        }
    }

    #[test]
    fn graph_event_conversion_normalizes_offsetless_utc_wire_format() {
        let timed: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "timed",
            "start": { "dateTime": "2026-11-23T05:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-11-23T06:00:00.0000000", "timeZone": "UTC" },
            "isAllDay": false
        }))
        .unwrap();
        let timed = CalendarEvent::from(timed);
        assert_eq!(timed.start_iso, "2026-11-23T05:00:00Z");
        assert_eq!(timed.end_iso, "2026-11-23T06:00:00Z");
        assert_eq!(timed.time_zone, "UTC");

        let all_day: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "all-day",
            "start": { "dateTime": "2026-11-23T00:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-11-24T00:00:00.0000000", "timeZone": "UTC" },
            "isAllDay": true
        }))
        .unwrap();
        let all_day = CalendarEvent::from(all_day);
        assert!(all_day.is_all_day);
        assert_eq!(all_day.start_iso, "2026-11-23T00:00:00.0000000");
        assert_eq!(all_day.end_iso, "2026-11-24T00:00:00.0000000");

        let zoned: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "zoned",
            "start": { "dateTime": "2026-11-23T09:00:00.0000000", "timeZone": "Asia/Seoul" },
            "end": { "dateTime": "2026-11-23T10:00:00.0000000", "timeZone": "Asia/Seoul" }
        }))
        .unwrap();
        assert_eq!(CalendarEvent::from(zoned).start_iso, "2026-11-23T09:00:00.0000000");
    }

    #[test]
    fn new_event_times_follow_the_create_contract() {
        let s = |v: &str| v.to_string();

        // All-day: floating midnights in the requested zone.
        assert_eq!(
            resolve_new_event_times("2026-11-23", "2026-11-24", true, Some("Asia/Seoul")).unwrap(),
            (s("2026-11-23T00:00:00"), s("2026-11-24T00:00:00"), Some(s("Asia/Seoul")))
        );
        // All-day without a zone: still floating midnights, in a non-empty default zone.
        let (start, end, zone) =
            resolve_new_event_times("2026-11-23", "2026-11-25", true, None).unwrap();
        assert_eq!((start.as_str(), end.as_str()), ("2026-11-23T00:00:00", "2026-11-25T00:00:00"));
        assert!(zone.is_some_and(|zone| !zone.trim().is_empty()));
        let (_, _, blank_zone) =
            resolve_new_event_times("2026-11-23", "2026-11-24", true, Some("  ")).unwrap();
        assert!(blank_zone.is_some_and(|zone| !zone.trim().is_empty()));

        // All-day validation happens before any network call.
        let same_day = resolve_new_event_times("2026-11-23", "2026-11-23", true, None).unwrap_err();
        assert!(same_day.to_string().contains("2026-11-24"), "{same_day}");
        assert!(resolve_new_event_times("2026-11-24", "2026-11-23", true, None).is_err());
        for (start, end) in [
            ("2026-11-23 09:00", "2026-11-24"),
            ("2026-11-23", "2026-11-24T00:00:00Z"),
            ("2026-11-23T00:00:00+09:00", "2026-11-24T00:00:00+09:00"),
        ] {
            let err = resolve_new_event_times(start, end, true, Some("Asia/Seoul")).unwrap_err();
            assert!(err.to_string().contains("YYYY-MM-DD"), "{err}");
        }

        // Timed wall clock + zone: sent unconverted, read in that zone.
        for (start, end) in [
            ("2026-11-23 09:00", "2026-11-23 10:00"),
            ("2026-11-23T09:00", "2026-11-23T10:00:00"),
            ("2026-11-23_09:00:00", "2026-11-23_10:00"),
        ] {
            assert_eq!(
                resolve_new_event_times(start, end, false, Some("Asia/Seoul")).unwrap(),
                (s("2026-11-23T09:00:00"), s("2026-11-23T10:00:00"), Some(s("Asia/Seoul")))
            );
        }

        // Timed RFC3339 + zone: exact instant in UTC; the zone is not sent alongside a `Z` value.
        assert_eq!(
            resolve_new_event_times(
                "2026-11-23T09:00:00+09:00",
                "2026-11-23T10:00:00+09:00",
                false,
                Some("Asia/Seoul")
            )
            .unwrap(),
            (s("2026-11-23T00:00:00Z"), s("2026-11-23T01:00:00Z"), Some(s("UTC")))
        );
        assert!(resolve_new_event_times(
            "2026-11-23 09:00",
            "2026-11-23T10:00:00+09:00",
            false,
            Some("Asia/Seoul")
        )
        .is_err());

        // Timed without a zone: identical to the 0.3.0 path.
        for (start, end) in [
            ("2026-11-23T09:00:00+09:00", "2026-11-23T10:00:00Z"),
            ("2026-11-23 09:00", "2026-11-23 10:00:30"),
            ("2026-11-23", "2026-11-24"),
        ] {
            assert_eq!(
                resolve_new_event_times(start, end, false, None).unwrap(),
                (
                    crate::cli::normalize_datetime_iso(start).unwrap(),
                    crate::cli::normalize_datetime_iso(end).unwrap(),
                    None
                )
            );
        }
        assert!(resolve_new_event_times("nonsense", "2026-11-24", false, None).is_err());
    }

    #[test]
    fn all_day_payload_sends_midnight_boundaries_in_the_event_zone() {
        let (start_iso, end_iso, time_zone) =
            resolve_new_event_times("2026-11-23", "2026-11-24", true, Some("Asia/Seoul")).unwrap();
        let payload = build_event_payload(&OutgoingEvent {
            subject: "PTO".into(),
            start_iso,
            end_iso,
            time_zone,
            is_all_day: true,
            ..Default::default()
        });
        assert_eq!(
            payload,
            serde_json::json!({
                "subject": "PTO",
                "start": { "dateTime": "2026-11-23T00:00:00", "timeZone": "Asia/Seoul" },
                "end": { "dateTime": "2026-11-24T00:00:00", "timeZone": "Asia/Seoul" },
                "isAllDay": true
            })
        );
    }

    #[test]
    fn edit_times_keep_series_zone_and_encode_instants_and_dates() {
        let mut event: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "type": "seriesMaster", "isAllDay": false,
            "originalStartTimeZone": "Pacific Standard Time",
            "originalEndTimeZone": "Pacific Standard Time",
        }))
        .unwrap();
        let local = CalendarEditTime::parse("2026-11-23T09:00").unwrap();
        assert_eq!(
            local.graph_value(&event, true).unwrap(),
            serde_json::json!({
                "dateTime": "2026-11-23T09:00:00", "timeZone": "Pacific Standard Time",
            })
        );
        let instant = CalendarEditTime::parse("2026-11-23T09:00:00+09:00").unwrap();
        assert!(instant.graph_value(&event, true).is_err());
        event.event_type = Some("singleInstance".into());
        assert_eq!(
            instant.graph_value(&event, true).unwrap(),
            serde_json::json!({
                "dateTime": "2026-11-23T00:00:00", "timeZone": "UTC",
            })
        );
        event.is_all_day = Some(true);
        let date = CalendarEditTime::parse("2026-11-24").unwrap();
        assert_eq!(
            date.graph_value(&event, false).unwrap(),
            serde_json::json!({
                "dateTime": "2026-11-24T00:00:00", "timeZone": "Pacific Standard Time",
            })
        );
        assert!(local.graph_value(&event, true).is_err());
    }

    #[tokio::test]
    async fn test_list_calendar_view_and_get_event() {
        #[derive(Clone)]
        struct ServerState {
            base_url: Arc<Mutex<String>>,
        }
        let state = ServerState {
            base_url: Arc::new(Mutex::new(String::new())),
        };
        let state_clone = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/calendarView",
                get(|State(st): State<ServerState>, headers: HeaderMap, Query(params): Query<HashMap<String, String>>| async move {
                    assert_eq!(
                        headers.get("prefer").and_then(|v| v.to_str().ok()),
                        Some("outlook.timezone=\"UTC\"")
                    );
                    if params.get("page").map(String::as_str) == Some("2") {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "ev3",
                                    "subject": "Body Link Meeting",
                                    "start": { "dateTime": "2026-11-16T03:00:00", "timeZone": "UTC" },
                                    "end": { "dateTime": "2026-11-16T04:00:00", "timeZone": "UTC" },
                                    "body": {
                                        "contentType": "html",
                                        "content": "<p>Join: <a href=\"https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc/0?context=1&amp;anon=true\">Join</a></p>"
                                    }
                                }
                            ]
                        }))
                    } else {
                        assert!(params.contains_key("startDateTime"));
                        assert!(params.contains_key("endDateTime"));
                        let base = st.base_url.lock().unwrap().clone();
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "ev1",
                                    "subject": "Sprint Review",
                                    "start": { "dateTime": "2026-11-15T10:00:00", "timeZone": "UTC" },
                                    "end": { "dateTime": "2026-11-15T11:00:00", "timeZone": "UTC" },
                                    "isAllDay": false,
                                    "location": { "displayName": "Room A" },
                                    "organizer": { "emailAddress": { "name": "Alice", "address": "alice@example.com" } },
                                    "attendees": [
                                        {
                                            "emailAddress": { "name": "Bob", "address": "bob@example.com" },
                                            "type": "required",
                                            "status": { "response": "accepted" }
                                        }
                                    ],
                                    "responseStatus": { "response": "organizer" },
                                    "isOnlineMeeting": true,
                                    "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3a111/0" }
                                },
                                {
                                    "id": "ev2",
                                    "subject": "Fallback URL Meeting",
                                    "start": { "dateTime": "2026-11-15T12:00:00", "timeZone": "UTC" },
                                    "end": { "dateTime": "2026-11-15T12:30:00", "timeZone": "UTC" },
                                    "onlineMeetingUrl": "https://teams.microsoft.com/l/meetup-join/19%3a222/0"
                                }
                            ],
                            "@odata.nextLink": format!("{base}/v1.0/me/calendarView?page=2")
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/events/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "id": id,
                        "subject": "Single Event",
                        "start": { "dateTime": "2026-11-15T14:00:00", "timeZone": "UTC" },
                        "end": { "dateTime": "2026-11-15T15:00:00", "timeZone": "UTC" },
                        "isCancelled": true
                    }))
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *state_clone.base_url.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let events = client
            .list_calendar_view("2026-11-15T00:00:00Z", "2026-11-17T00:00:00Z", 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].start_iso, "2026-11-15T10:00:00Z");
        assert_eq!(events[0].end_iso, "2026-11-15T11:00:00Z");
        assert_eq!(
            events[0].online_meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3a111/0")
        );
        assert_eq!(events[0].attendees.len(), 1);
        assert_eq!(
            events[1].online_meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3a222/0")
        );
        assert!(events[2].is_online_meeting);
        assert_eq!(
            events[2].online_meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc/0?context=1&anon=true")
        );

        let single = client.get_calendar_event("ev99").await.unwrap();
        assert_eq!(single.id, "ev99");
        assert!(single.is_cancelled);
    }

    #[tokio::test]
    async fn test_create_update_delete_and_respond_calendar_event() {
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let rec_clone = recorded.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/events",
                post(|Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["isOnlineMeeting"], true);
                    assert_eq!(body["onlineMeetingProvider"], "teamsForBusiness");
                    assert_eq!(body["start"]["timeZone"], "UTC");
                    Json(serde_json::json!({
                        "id": "created-ev",
                        "subject": body["subject"],
                        "start": body["start"],
                        "end": body["end"],
                        "isOnlineMeeting": true,
                        "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/new" }
                    }))
                }),
            )
            .route(
                "/v1.0/me/events/{id}",
                delete(|State(st): State<Arc<Mutex<Vec<String>>>>, AxumPath(id): AxumPath<String>| async move {
                    st.lock().unwrap().push(format!("delete:{id}"));
                    StatusCode::NO_CONTENT
                })
                .patch(|State(st): State<Arc<Mutex<Vec<String>>>>, AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| async move {
                    st.lock().unwrap().push(format!("patch:{id}:{}", body["subject"].as_str().unwrap_or("")));
                    Json(serde_json::json!({
                        "id": id,
                        "subject": body["subject"],
                        "start": body["start"],
                        "end": body["end"]
                    }))
                }),
            )
            .route(
                "/v1.0/me/events/{id}/{action}",
                post(
                    |State(st): State<Arc<Mutex<Vec<String>>>>,
                     AxumPath((id, action)): AxumPath<(String, String)>,
                     Json(body): Json<serde_json::Value>| async move {
                        assert_eq!(body["sendResponse"], true);
                        st.lock()
                            .unwrap()
                            .push(format!("{id}:{action}:{}", body["comment"].as_str().unwrap_or("")));
                        StatusCode::ACCEPTED
                    },
                ),
            )
            .with_state(recorded);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let created = client
            .create_calendar_event(&OutgoingEvent {
                subject: "Design Sync".into(),
                start_iso: "2026-11-16T09:00:00".into(),
                end_iso: "2026-11-16T10:00:00".into(),
                time_zone: None,
                is_all_day: false,
                location: Some("Teams".into()),
                body_html: Some("<p>Agenda</p>".into()),
                attendees: vec!["a@example.com".into()],
                is_online_meeting: true,
            })
            .await
            .unwrap();
        assert_eq!(created.id, "created-ev");
        assert!(created.is_online_meeting);

        let updated = client
            .update_calendar_event(
                "created-ev",
                &OutgoingEvent {
                    subject: "Design Sync Updated".into(),
                    start_iso: "2026-11-16T09:30:00".into(),
                    end_iso: "2026-11-16T10:30:00".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.subject, "Design Sync Updated");

        client
            .respond_calendar_event("ev1", EventRsvpAction::Accept, Some("Attending"))
            .await
            .unwrap();
        client
            .respond_calendar_event("ev1", EventRsvpAction::TentativelyAccept, None)
            .await
            .unwrap();
        client
            .respond_calendar_event("ev1", "declined", Some("Conflict"))
            .await
            .unwrap();
        client.delete_calendar_event("ev1").await.unwrap();

        let err = client.respond_calendar_event("ev1", "maybe", None).await;
        assert!(err.is_err());

        let log = rec_clone.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "patch:created-ev:Design Sync Updated",
                "ev1:accept:Attending",
                "ev1:tentativelyAccept:",
                "ev1:decline:Conflict",
                "delete:ev1",
            ]
        );
    }

    #[tokio::test]
    async fn test_delta_calendar_view_pagination_removed_and_incremental_link() {
        #[derive(Clone, Default)]
        struct DeltaState {
            base_url: Arc<Mutex<String>>,
            prefer_headers: Arc<Mutex<Vec<String>>>,
        }
        let state = DeltaState::default();
        let state_check = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/calendarView/delta",
                get(
                    |State(st): State<DeltaState>,
                     headers: HeaderMap,
                     Query(params): Query<HashMap<String, String>>| async move {
                        let prefer = headers
                            .get("prefer")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        st.prefer_headers.lock().unwrap().push(prefer);
                        let base = st.base_url.lock().unwrap().clone();

                        if params.get("deltatoken").map(String::as_str) == Some("tok2") {
                            Json(serde_json::json!({
                                "value": [
                                    { "id": "ev-del-2", "@removed": { "reason": "deleted" } }
                                ],
                                "@odata.deltaLink": format!("{base}/v1.0/me/calendarView/delta?deltatoken=tok3")
                            }))
                        } else if params.get("page").map(String::as_str) == Some("2") {
                            Json(serde_json::json!({
                                "value": [
                                    { "id": "ev-del-1", "@removed": { "reason": "deleted" } },
                                    {
                                        "id": "ev-2",
                                        "subject": "Page 2 Event",
                                        "start": { "dateTime": "2026-11-16T11:00:00", "timeZone": "UTC" },
                                        "end": { "dateTime": "2026-11-16T12:00:00", "timeZone": "UTC" }
                                    }
                                ],
                                "@odata.deltaLink": format!("{base}/v1.0/me/calendarView/delta?deltatoken=tok2")
                            }))
                        } else {
                            assert_eq!(params.get("startDateTime").map(String::as_str), Some("2026-11-15T00:00:00Z"));
                            assert_eq!(params.get("endDateTime").map(String::as_str), Some("2026-11-22T00:00:00Z"));
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "ev-1",
                                        "subject": "Initial Event",
                                        "start": { "dateTime": "2026-11-15T09:00:00", "timeZone": "UTC" },
                                        "end": { "dateTime": "2026-11-15T10:00:00", "timeZone": "UTC" }
                                    }
                                ],
                                "@odata.nextLink": format!("{base}/v1.0/me/calendarView/delta?page=2")
                            }))
                        }
                    },
                ),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *state.base_url.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let (up1, rem1, dlink1) = client
            .delta_calendar_view("2026-11-15T00:00:00Z", "2026-11-22T00:00:00Z", None)
            .await
            .unwrap();
        assert_eq!(up1.len(), 2);
        assert_eq!(up1[0].id, "ev-1");
        assert_eq!(up1[1].id, "ev-2");
        assert_eq!(rem1, vec!["ev-del-1"]);
        let dlink1 = dlink1.unwrap();
        assert!(dlink1.contains("deltatoken=tok2"));

        let (up2, rem2, dlink2) = client
            .delta_calendar_view("2026-11-15T00:00:00Z", "2026-11-22T00:00:00Z", Some(&dlink1))
            .await
            .unwrap();
        assert!(up2.is_empty());
        assert_eq!(rem2, vec!["ev-del-2"]);
        assert!(dlink2.unwrap().contains("deltatoken=tok3"));

        let prefers = state_check.prefer_headers.lock().unwrap().clone();
        assert_eq!(
            prefers,
            vec![
                "outlook.timezone=\"UTC\", odata.maxpagesize=50",
                "outlook.timezone=\"UTC\", odata.maxpagesize=50",
                "outlook.timezone=\"UTC\", odata.maxpagesize=50"
            ]
        );
    }

    #[test]
    fn graph_calendar_event_preserves_metadata_and_recurrence_with_backcompat_serde() {
        let raw: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "ev-meta-1",
            "subject": "Weekly Staff",
            "start": { "dateTime": "2026-11-20T09:00:00Z", "timeZone": "UTC" },
            "end": { "dateTime": "2026-11-20T10:00:00Z", "timeZone": "UTC" },
            "categories": ["Team", "  ", "Recurring"],
            "showAs": "tentative",
            "sensitivity": "private",
            "type": "seriesMaster",
            "hasAttachments": true,
            "recurrence": {
                "pattern": { "type": "weekly", "interval": 1, "daysOfWeek": ["friday"] },
                "range": { "type": "noEnd", "startDate": "2026-11-20" }
            }
        }))
        .unwrap();
        let ev = CalendarEvent::from(raw);
        assert_eq!(ev.categories, vec!["Team", "Recurring"]);
        assert_eq!(ev.show_as.as_deref(), Some("tentative"));
        assert_eq!(ev.sensitivity.as_deref(), Some("private"));
        assert_eq!(ev.event_type.as_deref(), Some("seriesMaster"));
        assert!(ev.has_attachments);
        assert!(ev.is_recurring);
        assert_eq!(
            ev.recurrence.as_ref().unwrap()["pattern"]["type"],
            "weekly"
        );

        // Occurrence without recurrence payload still marks is_recurring = true.
        let occ: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "ev-occ-1",
            "subject": "Weekly Staff",
            "type": "occurrence",
            "seriesMasterId": "ev-meta-1"
        }))
        .unwrap();
        let occ_ev = CalendarEvent::from(occ);
        assert!(occ_ev.is_recurring);
        assert_eq!(occ_ev.event_type.as_deref(), Some("occurrence"));
        assert!(occ_ev.recurrence.is_none());

        // Plain single event skips all empty/false/None new fields on serialization (backcompat).
        let plain: GraphCalendarEvent = serde_json::from_value(serde_json::json!({
            "id": "ev-plain",
            "subject": "One-off"
        }))
        .unwrap();
        let plain_ev = CalendarEvent::from(plain);
        assert!(!plain_ev.is_recurring);
        assert!(!plain_ev.has_attachments);
        let serialized = serde_json::to_value(&plain_ev).unwrap();
        for key in [
            "categories",
            "show_as",
            "sensitivity",
            "event_type",
            "has_attachments",
            "is_recurring",
            "recurrence",
        ] {
            assert!(
                serialized.get(key).is_none(),
                "expected {key} to be omitted when default/empty"
            );
        }
    }
}
