//! Microsoft Graph v1.0 Outlook Calendar operations (`/me/calendarView`, `/me/events`, RSVP, Teams online meetings).

use std::borrow::Cow;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
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
        for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
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

        let start_iso = raw
            .start
            .as_ref()
            .and_then(|s| s.date_time.clone())
            .unwrap_or_default();
        let end_iso = raw
            .end
            .as_ref()
            .and_then(|s| s.date_time.clone())
            .unwrap_or_default();
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

        Self {
            id: raw.id,
            subject: raw.subject.unwrap_or_default(),
            start_iso,
            end_iso,
            time_zone,
            is_all_day: raw.is_all_day.unwrap_or(false),
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
}
