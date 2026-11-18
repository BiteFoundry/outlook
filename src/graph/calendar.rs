//! Microsoft Graph v1.0 Outlook Calendar operations (`/me/calendarView`, `/me/events`, RSVP, Teams online meetings).

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;

use super::{
    client::{GraphDeltaPage, GraphListPage},
    mail::{normalize_outlook_body, GraphOutlookBody, GraphRecipient},
    GraphClient,
};
use crate::models::{CalendarEvent, EventAttendee, OutgoingEvent};

static TEAMS_MEETUP_REGEX: OnceLock<Regex> = OnceLock::new();

fn teams_meetup_regex() -> &'static Regex {
    TEAMS_MEETUP_REGEX.get_or_init(|| {
        Regex::new(r#"https://teams\.microsoft\.com/l/meetup-join/[^\s"'<>)]+"#)
            .expect("valid Teams meetup regex")
    })
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

fn extract_teams_meetup_url(text: &str) -> Option<String> {
    let m = teams_meetup_regex().find(text)?;
    let url = m.as_str().replace("&amp;", "&");
    Some(url)
}

impl GraphClient {
    pub async fn list_calendar_view(
        &self,
        start_iso: &str,
        end_iso: &str,
        limit: usize,
    ) -> Result<Vec<CalendarEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
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
            for raw in page.value {
                events.push(CalendarEvent::from(raw));
                if events.len() >= limit {
                    return Ok(events);
                }
            }
            next_url = page.next_link;
        }

        Ok(events)
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

        let headers = [("Prefer", "odata.maxpagesize=50")];
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
                        .context("Graph 캘린더 delta 항목 역직렬화 실패")?;
                    upserted.push(CalendarEvent::from(raw));
                }
            }
            if page.delta_link.is_some() {
                new_delta_link = page.delta_link;
            }
            next_url = page.next_link;
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

        let created: GraphCalendarEvent = self.post_json("/me/events", &payload).await?;
        Ok(CalendarEvent::from(created))
    }

    pub async fn delete_calendar_event(&self, event_id: &str) -> Result<()> {
        let path = format!("/me/events/{}", urlencoding::encode(event_id.trim()));
        self.delete_req(&path).await
    }

    /// `response` is one of `"accept"`, `"tentativelyAccept"`, `"decline"`.
    pub async fn respond_calendar_event(
        &self,
        event_id: &str,
        response: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let endpoint = match response.trim().to_ascii_lowercase().as_str() {
            "accept" | "accepted" => "accept",
            "tentative" | "tentativelyaccept" | "tentativelyaccepted" => "tentativelyAccept",
            "decline" | "declined" => "decline",
            other => {
                bail!("지원하지 않는 일정 응답입니다: {other} (accept, tentative, decline 중 하나)")
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
                                    "subject": "본문 링크 모임",
                                    "start": { "dateTime": "2026-11-16T03:00:00", "timeZone": "UTC" },
                                    "end": { "dateTime": "2026-11-16T04:00:00", "timeZone": "UTC" },
                                    "body": {
                                        "contentType": "html",
                                        "content": "<p>참여: <a href=\"https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc/0?context=1&amp;anon=true\">Join</a></p>"
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
                                    "subject": "스프린트 리뷰",
                                    "start": { "dateTime": "2026-11-15T10:00:00", "timeZone": "UTC" },
                                    "end": { "dateTime": "2026-11-15T11:00:00", "timeZone": "UTC" },
                                    "isAllDay": false,
                                    "location": { "displayName": "회의실 A" },
                                    "organizer": { "emailAddress": { "name": "김철수", "address": "kim@example.com" } },
                                    "attendees": [
                                        {
                                            "emailAddress": { "name": "이영희", "address": "lee@example.com" },
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
                                    "subject": "폴백 URL 모임",
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
                        "subject": "단일 일정",
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
    async fn test_create_delete_and_respond_calendar_event() {
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
                subject: "디자인 싱크".into(),
                start_iso: "2026-11-16T09:00:00".into(),
                end_iso: "2026-11-16T10:00:00".into(),
                time_zone: None,
                is_all_day: false,
                location: Some("Teams".into()),
                body_html: Some("<p>안건</p>".into()),
                attendees: vec!["a@example.com".into()],
                is_online_meeting: true,
            })
            .await
            .unwrap();
        assert_eq!(created.id, "created-ev");
        assert!(created.is_online_meeting);

        client
            .respond_calendar_event("ev1", "accepted", Some("참석합니다"))
            .await
            .unwrap();
        client
            .respond_calendar_event("ev1", "tentative", None)
            .await
            .unwrap();
        client
            .respond_calendar_event("ev1", "declined", Some("불가"))
            .await
            .unwrap();
        client.delete_calendar_event("ev1").await.unwrap();

        let err = client.respond_calendar_event("ev1", "maybe", None).await;
        assert!(err.is_err());

        let log = rec_clone.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "ev1:accept:참석합니다",
                "ev1:tentativelyAccept:",
                "ev1:decline:불가",
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
                     headers: axum::http::HeaderMap,
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
                                        "subject": "2페이지 일정",
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
                                        "subject": "초기 일정",
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
                "odata.maxpagesize=50",
                "odata.maxpagesize=50",
                "odata.maxpagesize=50"
            ]
        );
    }
}
