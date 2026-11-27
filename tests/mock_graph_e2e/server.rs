#![allow(dead_code, unused_imports)]

//! End-to-end integration tests for `outlook-cli` against an in-process mock
//! Microsoft Graph v1.0, Entra OAuth 2.0, and GitHub Releases server.

use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Once, RwLock,
    },
};

use axum::{
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD as B64_STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use image::{ImageBuffer, Rgba, RgbaImage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;

use outlook_cli::{
    auth::{AuthManager, StoredTokens},
    config::{AppConfig, ImageProtocolPreference},
    graph::GraphClient,
    img_preview, mcp,
    models::{
        CalendarEvent, EventRsvpAction, InlineImageUpload, MailMessage, OutgoingEvent,
        OutgoingMail, OutgoingTodoTask, RichBlock, SyncCursor, TodoTask,
    },
    parser,
    serve::{self, ServeOptions},
    storage::Storage,
    sync,
    ui::{
        action::{self, Action},
        app::{AppState as UiAppState, UiState},
        overlays::{image_preview::ImagePreviewOverlay, Overlay, OverlayKind},
    },
    updater,
};

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();

pub(crate) fn ensure_isolated_env() {
    INIT_TEST_ENV.call_once(|| {
        let dir = tempfile::tempdir().expect("create shared test config tempdir");
        std::env::set_var("OUTLOOK_DISABLE_KEYRING", "1");
        std::env::set_var("OUTLOOK_CONFIG_DIR", dir.path());
        std::env::set_var("OUTLOOK_DATA_DIR", dir.path());
        std::env::remove_var("OUTLOOK_ACCESS_TOKEN");
        std::env::remove_var("OUTLOOK_TOKEN_FILE");
        std::env::remove_var("OUTLOOK_TOKEN_REFRESH_CMD");
        std::env::remove_var("TEAMS_TOKEN_FILE");
        std::env::remove_var("TEAMS_CONFIG_DIR");
        let _ = TEST_CONFIG_DIR.set(dir);
    });
}

pub(crate) fn make_test_jwt(payload: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
    format!("{header}.{body}.sig")
}

pub(crate) fn make_test_png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut img: RgbaImage = ImageBuffer::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let r = ((x * 255) / width.max(1)) as u8;
        let g = ((y * 255) / height.max(1)) as u8;
        *px = Rgba([r, g, 190, 255]);
    }
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode PNG bytes");
    buf.into_inner()
}

#[derive(Default)]
pub(crate) struct MockGraphState {
    pub(crate) base_url: RwLock<String>,
    pub(crate) expected_token: RwLock<String>,
    pub(crate) refreshed_token_to_issue: RwLock<String>,
    pub(crate) refresh_calls: AtomicUsize,
    pub(crate) me_auth_headers: RwLock<Vec<String>>,
    pub(crate) list_queries: RwLock<Vec<String>>,
    pub(crate) attachment_fetch_calls: AtomicUsize,
    pub(crate) delta_calls: AtomicUsize,
    pub(crate) sent_mail_payloads: RwLock<Vec<Value>>,
    pub(crate) replied_ids: RwLock<Vec<(String, Value)>>,
    pub(crate) forwarded_ids: RwLock<Vec<(String, Value)>>,
    pub(crate) patched_messages: RwLock<Vec<(String, Value)>>,
    pub(crate) moved_messages: RwLock<Vec<(String, String)>>,
    pub(crate) created_events: RwLock<Vec<Value>>,
    pub(crate) rsvp_calls: RwLock<Vec<(String, String, Value)>>,
    pub(crate) deleted_event_ids: RwLock<Vec<String>>,
    pub(crate) created_todo_tasks: RwLock<Vec<Value>>,
    pub(crate) patched_todo_tasks: RwLock<Vec<(String, String, Value)>>,
    pub(crate) deleted_todo_tasks: RwLock<Vec<(String, String)>>,
    inline_png_b64: RwLock<String>,
    pub(crate) inline_png_bytes: RwLock<Vec<u8>>,
}

pub(crate) fn check_bearer(headers: &HeaderMap, expected: &str) -> Option<Response> {
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if actual == Some(expected) {
        None
    } else {
        Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": {
                        "code": "InvalidAuthenticationToken",
                        "message": format!("Expected Bearer {expected}, got {actual:?}")
                    }
                })),
            )
                .into_response(),
        )
    }
}

pub(crate) async fn oauth_token_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(tenant): Path<String>,
    body: String,
) -> Response {
    let count = st.refresh_calls.fetch_add(1, Ordering::SeqCst) + 1;
    assert_eq!(tenant, "tenant-e2e");
    assert!(
        body.contains("grant_type=refresh_token"),
        "expected refresh_token grant, got: {body}"
    );
    let next_tok = {
        let configured = st.refreshed_token_to_issue.read().unwrap().clone();
        if configured.is_empty() {
            format!("refreshed-outlook-tok-{count}")
        } else {
            configured
        }
    };
    *st.expected_token.write().unwrap() = next_tok.clone();
    Json(json!({
        "token_type": "Bearer",
        "access_token": next_tok,
        "refresh_token": format!("rt-next-{count}"),
        "expires_in": 3600
    }))
    .into_response()
}

pub(crate) async fn get_me_handler(State(st): State<Arc<MockGraphState>>, headers: HeaderMap) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        st.me_auth_headers.write().unwrap().push(auth.to_string());
    }
    Json(json!({
        "id": "user-e2e-1",
        "displayName": "Alice Outlook",
        "mail": "alice@contoso.com",
        "userPrincipalName": "alice@contoso.com",
        "jobTitle": "Principal Engineer",
        "department": "Core Productivity"
    }))
    .into_response()
}

pub(crate) async fn list_mail_folders_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "folder-inbox",
                "displayName": "Inbox",
                "wellKnownName": "inbox",
                "childFolderCount": 0,
                "unreadItemCount": 2,
                "totalItemCount": 3
            },
            {
                "id": "folder-archive",
                "displayName": "Archive",
                "wellKnownName": "archive",
                "childFolderCount": 0,
                "unreadItemCount": 0,
                "totalItemCount": 10
            },
            {
                "id": "folder-sent",
                "displayName": "Sent Items",
                "wellKnownName": "sentitems",
                "childFolderCount": 0,
                "unreadItemCount": 0,
                "totalItemCount": 5
            },
            {
                "id": "folder-drafts",
                "displayName": "Drafts",
                "wellKnownName": "drafts",
                "childFolderCount": 0,
                "unreadItemCount": 0,
                "totalItemCount": 1
            }
        ]
    }))
    .into_response()
}

pub(crate) fn header_message_json(id: &str, subject: &str, preview: &str, is_read: bool) -> Value {
    json!({
        "id": id,
        "internetMessageId": format!("<{id}@contoso.com>"),
        "conversationId": "conv-q4",
        "parentFolderId": "folder-inbox",
        "subject": subject,
        "from": { "emailAddress": { "name": "Bob Reviewer", "address": "bob@contoso.com" } },
        "toRecipients": [{ "emailAddress": { "name": "Alice Outlook", "address": "alice@contoso.com" } }],
        "receivedDateTime": "2026-11-18T10:00:00Z",
        "sentDateTime": "2026-11-18T09:59:55Z",
        "isRead": is_read,
        "isDraft": false,
        "flag": { "flagStatus": "notFlagged" },
        "importance": "normal",
        "inferenceClassification": "focused",
        "hasAttachments": id == "msg-html-cid",
        "bodyPreview": preview
    })
}

pub(crate) async fn list_folder_messages_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(folder_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    let q = query.unwrap_or_default();
    st.list_queries
        .write()
        .unwrap()
        .push(format!("/me/mailFolders/{folder_id}/messages?{q}"));

    Json(json!({
        "value": [
            header_message_json("msg-html-cid", "Q4 Metrics & Architecture", "See Q4 latency table", false),
            header_message_json("msg-to-remove", "Temporary Notice", "Will be deleted by delta", true)
        ]
    }))
    .into_response()
}

pub(crate) async fn list_all_messages_handler(
    State(st): State<Arc<MockGraphState>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    let q = query.unwrap_or_default();
    st.list_queries
        .write()
        .unwrap()
        .push(format!("/me/messages?{q}"));

    if q.contains("$search=") {
        return Json(json!({
            "value": [
                header_message_json("msg-html-cid", "Q4 Metrics & Architecture", "Remote KQL hit", false)
            ]
        }))
        .into_response();
    }

    Json(json!({
        "value": [
            header_message_json("msg-html-cid", "Q4 Metrics & Architecture", "See Q4 latency table", false)
        ]
    }))
    .into_response()
}

pub(crate) async fn delta_folder_messages_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(folder_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    let call_num = st.delta_calls.fetch_add(1, Ordering::SeqCst) + 1;
    let base = st.base_url.read().unwrap().clone();
    let q = query.unwrap_or_default();

    if q.contains("deltatoken=d1") || call_num >= 2 {
        Json(json!({
            "value": [
                header_message_json("msg-html-cid", "Q4 Metrics & Architecture", "See Q4 latency table", true),
                header_message_json("msg-delta-new", "Deployment 롤아웃 Update", "Production 롤아웃 completed", false),
                {
                    "id": "msg-to-remove",
                    "@removed": { "reason": "deleted" }
                }
            ],
            "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{folder_id}/messages/delta?$deltatoken=d2")
        }))
        .into_response()
    } else {
        Json(json!({
            "value": [
                header_message_json("msg-html-cid", "Q4 Metrics & Architecture", "See Q4 latency table", false),
                header_message_json("msg-to-remove", "Temporary Notice", "Will be deleted by delta", true)
            ],
            "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{folder_id}/messages/delta?$deltatoken=d1")
        }))
        .into_response()
    }
}

pub(crate) fn rich_cid_message_html() -> &'static str {
    r#"
    <html>
      <head><style>.secret-noise { display: none; color: #ff0000; }</style></head>
      <body>
        <p>Hi Alice, here are the <strong>Q4</strong> results:</p>
        <table>
          <thead><tr><th>Metric</th><th>Value</th></tr></thead>
          <tbody><tr><td>Latency</td><td>12ms</td></tr></tbody>
        </table>
        <p>Inline chart below:</p>
        <img src="cid:chart01@contoso" alt="Q4 Chart">
        <div id="appendonsend"></div>
        <hr tabindex="-1">
        <div id="divRplyFwdMsg">
          <b>From:</b> Bob Reviewer &lt;bob@contoso.com&gt;<br>
          <b>Sent:</b> Wednesday, Nov 18, 2026<br>
        </div>
        <div><p>Previous thread context.</p></div>
      </body>
    </html>
    "#
}

pub(crate) async fn get_message_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }

    if msg_id == "msg-invite-1" {
        return Json(json!({
            "@odata.type": "#microsoft.graph.eventMessage",
            "id": "msg-invite-1",
            "internetMessageId": "<invite-1@contoso.com>",
            "conversationId": "conv-invite-1",
            "parentFolderId": "folder-inbox",
            "subject": "Invitation: Architecture Review",
            "from": { "emailAddress": { "name": "Carol Organizer", "address": "carol@contoso.com" } },
            "toRecipients": [{ "emailAddress": { "name": "Alice Outlook", "address": "alice@contoso.com" } }],
            "receivedDateTime": "2026-11-18T11:00:00Z",
            "isRead": false,
            "isDraft": false,
            "importance": "normal",
            "hasAttachments": false,
            "meetingMessageType": "meetingRequest",
            "startDateTime": { "dateTime": "2026-11-20T15:00:00Z", "timeZone": "UTC" },
            "endDateTime": { "dateTime": "2026-11-20T16:00:00Z", "timeZone": "UTC" },
            "location": { "displayName": "Conf Room A" },
            "responseType": "notResponded",
            "event": {
                "id": "evt-invite-1",
                "onlineMeeting": {
                    "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3aarch_review/0"
                },
                "responseStatus": { "response": "notResponded" }
            },
            "bodyPreview": "Please join our architecture review",
            "body": {
                "contentType": "html",
                "content": "<p>Join <a href=\"https://teams.microsoft.com/l/meetup-join/19%3aarch_review/0\">Teams Meeting</a></p>"
            }
        }))
        .into_response();
    }

    let b64 = st.inline_png_b64.read().unwrap().clone();
    let png_len = st.inline_png_bytes.read().unwrap().len();
    Json(json!({
        "id": msg_id,
        "internetMessageId": format!("<{msg_id}@contoso.com>"),
        "conversationId": "conv-q4",
        "parentFolderId": "folder-inbox",
        "subject": "Q4 Metrics & Architecture",
        "from": { "emailAddress": { "name": "Bob Reviewer", "address": "bob@contoso.com" } },
        "toRecipients": [{ "emailAddress": { "name": "Alice Outlook", "address": "alice@contoso.com" } }],
        "receivedDateTime": "2026-11-18T10:00:00Z",
        "sentDateTime": "2026-11-18T09:59:55Z",
        "isRead": false,
        "isDraft": false,
        "flag": { "flagStatus": "notFlagged" },
        "importance": "normal",
        "inferenceClassification": "focused",
        "hasAttachments": true,
        "bodyPreview": "See Q4 latency table",
        "body": {
            "contentType": "html",
            "content": rich_cid_message_html()
        },
        "attachments": [
            {
                "@odata.type": "#microsoft.graph.fileAttachment",
                "id": "att-inline-chart01",
                "name": "chart01.png",
                "contentType": "image/png",
                "size": png_len,
                "isInline": true,
                "contentId": "chart01@contoso",
                "contentBytes": b64
            }
        ]
    }))
    .into_response()
}

pub(crate) async fn patch_message_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.patched_messages
        .write()
        .unwrap()
        .push((msg_id, payload));
    StatusCode::OK.into_response()
}

pub(crate) async fn list_message_attachments_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.attachment_fetch_calls.fetch_add(1, Ordering::SeqCst);
    let b64 = st.inline_png_b64.read().unwrap().clone();
    let png_len = st.inline_png_bytes.read().unwrap().len();
    Json(json!({
        "value": [
            {
                "@odata.type": "#microsoft.graph.fileAttachment",
                "id": "att-inline-chart01",
                "name": "chart01.png",
                "contentType": "image/png",
                "size": png_len,
                "isInline": true,
                "contentId": "chart01@contoso",
                "contentBytes": b64
            }
        ]
    }))
    .into_response()
}

pub(crate) async fn get_attachment_meta_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((_msg_id, att_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.attachment_fetch_calls.fetch_add(1, Ordering::SeqCst);
    let png_len = st.inline_png_bytes.read().unwrap().len();
    Json(json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "id": att_id,
        "name": "chart01.png",
        "contentType": "image/png",
        "size": png_len,
        "isInline": true,
        "contentId": "chart01@contoso"
    }))
    .into_response()
}

pub(crate) async fn get_attachment_value_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.attachment_fetch_calls.fetch_add(1, Ordering::SeqCst);
    let bytes = st.inline_png_bytes.read().unwrap().clone();
    ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
}

pub(crate) async fn send_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.sent_mail_payloads.write().unwrap().push(payload);
    StatusCode::ACCEPTED.into_response()
}

pub(crate) async fn reply_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.replied_ids.write().unwrap().push((msg_id, payload));
    StatusCode::ACCEPTED.into_response()
}

pub(crate) async fn forward_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.forwarded_ids.write().unwrap().push((msg_id, payload));
    StatusCode::ACCEPTED.into_response()
}

pub(crate) async fn move_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    let dest = payload["destinationId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    st.moved_messages
        .write()
        .unwrap()
        .push((msg_id.clone(), dest.clone()));
    let mut moved = header_message_json(&msg_id, "Q4 Metrics & Architecture", "See Q4 latency table", true);
    moved["parentFolderId"] = json!(if dest == "archive" { "folder-archive" } else { dest.as_str() });
    Json(moved).into_response()
}

pub(crate) async fn calendar_view_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "evt-existing-1",
                "subject": "Weekly Sync",
                // Real Graph wire format under `Prefer: outlook.timezone="UTC"`: no offset, 7 fractional digits.
                // The end keeps the `Z` form so both inputs stay covered.
                "start": { "dateTime": "2026-11-19T10:00:00.0000000", "timeZone": "UTC" },
                "end": { "dateTime": "2026-11-19T10:30:00Z", "timeZone": "UTC" },
                "isAllDay": false,
                "location": { "displayName": "Teams" },
                "organizer": { "emailAddress": { "name": "Bob Reviewer", "address": "bob@contoso.com" } },
                "attendees": [
                    {
                        "type": "required",
                        "emailAddress": { "name": "Alice Outlook", "address": "alice@contoso.com" },
                        "status": { "response": "notResponded" }
                    }
                ],
                "responseStatus": { "response": "notResponded" },
                "isOnlineMeeting": true,
                "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3async/0" },
                "body": { "contentType": "html", "content": "<p>Agenda</p>" },
                "isCancelled": false
            }
        ]
    }))
    .into_response()
}

/// Graph rejects all-day events whose start/end are not midnight in the event's time zone.
pub(crate) fn graph_all_day_rejection(payload: &Value) -> Option<Response> {
    if payload["isAllDay"].as_bool() != Some(true) {
        return None;
    }
    let is_midnight = |endpoint: &Value| {
        endpoint["dateTime"]
            .as_str()
            .map(|value| value.trim_end_matches('0').trim_end_matches('.'))
            .is_some_and(|value| value.ends_with("T00:00:00") || value.ends_with("T00:00:"))
    };
    if is_midnight(&payload["start"]) && is_midnight(&payload["end"]) {
        return None;
    }
    Some(
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "ErrorInvalidRequest",
                    "message": "The Event.Start property for an all-day event needs to be set to midnight."
                }
            })),
        )
            .into_response(),
    )
}

/// Graph answers in the event's zone without an offset and with 7 fractional digits.
pub(crate) fn graph_wire_time(endpoint: &Value) -> Value {
    let raw = endpoint["dateTime"].as_str().unwrap_or_default();
    let naive = raw.trim_end_matches('Z');
    let date_time = if naive.contains('.') {
        naive.to_string()
    } else {
        format!("{naive}.0000000")
    };
    json!({ "dateTime": date_time, "timeZone": endpoint["timeZone"] })
}

pub(crate) async fn create_event_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    if let Some(rejection) = graph_all_day_rejection(&payload) {
        return rejection;
    }
    st.created_events.write().unwrap().push(payload.clone());
    Json(json!({
        "id": "evt-created-1",
        "subject": payload["subject"],
        "start": graph_wire_time(&payload["start"]),
        "end": graph_wire_time(&payload["end"]),
        "isAllDay": payload["isAllDay"].as_bool().unwrap_or(false),
        "location": payload["location"],
        "organizer": { "emailAddress": { "name": "Alice Outlook", "address": "alice@contoso.com" } },
        "attendees": [],
        "responseStatus": { "response": "organizer" },
        "isOnlineMeeting": payload["isOnlineMeeting"],
        "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3acreated/0" },
        "body": { "contentType": "html", "content": "<p>Kickoff</p>" },
        "isCancelled": false
    }))
    .into_response()
}

pub(crate) async fn delete_event_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.deleted_event_ids.write().unwrap().push(event_id);
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) async fn rsvp_event_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((event_id, action_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.rsvp_calls
        .write()
        .unwrap()
        .push((event_id, action_name, payload));
    StatusCode::ACCEPTED.into_response()
}

pub(crate) async fn list_contacts_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "contact-1",
                "displayName": "Bob Reviewer",
                "givenName": "Bob",
                "surname": "Reviewer",
                "emailAddresses": [{ "address": "bob@contoso.com", "name": "Bob Reviewer" }],
                "mobilePhone": "+1-555-0100",
                "businessPhones": [],
                "companyName": "Contoso",
                "jobTitle": "Staff Engineer",
                "department": "Platform"
            }
        ]
    }))
    .into_response()
}

pub(crate) async fn list_directory_users_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "dir-bob-dup",
                "displayName": "Bob Reviewer",
                "mail": "bob@contoso.com",
                "userPrincipalName": "bob@contoso.com",
                "jobTitle": "Staff Engineer",
                "department": "Platform"
            },
            {
                "id": "dir-carol",
                "displayName": "Carol Organizer",
                "mail": "carol@contoso.com",
                "userPrincipalName": "carol@contoso.com",
                "jobTitle": "Product Manager",
                "department": "Outlook"
            }
        ]
    }))
    .into_response()
}

pub(crate) async fn list_todo_lists_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "todo-list-default",
                "displayName": "Tasks",
                "wellknownListName": "defaultList",
                "isOwner": true,
                "isShared": false
            }
        ]
    }))
    .into_response()
}

pub(crate) async fn list_todo_tasks_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "task-existing-1",
                "title": "Review Q4 RFC",
                "status": "notStarted",
                "importance": "high",
                "body": { "contentType": "text", "content": "Check latency numbers" },
                "dueDateTime": { "dateTime": "2026-11-25T17:00:00Z", "timeZone": "UTC" },
                "createdDateTime": "2026-11-18T08:00:00Z",
                "lastModifiedDateTime": "2026-11-18T08:00:00Z"
            }
        ]
    }))
    .into_response()
}

pub(crate) async fn create_todo_task_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.created_todo_tasks.write().unwrap().push(payload.clone());
    Json(json!({
        "id": "task-created-1",
        "title": payload["title"],
        "status": "notStarted",
        "importance": payload.get("importance").and_then(Value::as_str).unwrap_or("normal"),
        "body": payload.get("body").cloned().unwrap_or_else(|| json!({ "contentType": "text", "content": "" })),
        "dueDateTime": payload.get("dueDateTime").cloned().unwrap_or(Value::Null),
        "createdDateTime": "2026-11-18T12:00:00Z",
        "lastModifiedDateTime": "2026-11-18T12:00:00Z"
    }))
    .into_response()
}

pub(crate) async fn patch_todo_task_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((list_id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    let status = payload["status"]
        .as_str()
        .unwrap_or("notStarted")
        .to_string();
    st.patched_todo_tasks
        .write()
        .unwrap()
        .push((list_id, task_id.clone(), payload));
    Json(json!({
        "id": task_id,
        "title": "Ship Outlook v0.1.0",
        "status": status,
        "importance": "high"
    }))
    .into_response()
}

pub(crate) async fn delete_todo_task_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((list_id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Some(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.deleted_todo_tasks
        .write()
        .unwrap()
        .push((list_id, task_id));
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) async fn spawn_mock_graph_server(
    initial_expected_token: &str,
) -> (String, Arc<MockGraphState>, tokio::task::JoinHandle<()>) {
    let png_bytes = make_test_png_bytes(16, 16);
    let png_b64 = B64_STANDARD.encode(&png_bytes);
    let state = Arc::new(MockGraphState {
        expected_token: RwLock::new(initial_expected_token.to_string()),
        inline_png_b64: RwLock::new(png_b64),
        inline_png_bytes: RwLock::new(png_bytes),
        ..Default::default()
    });

    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(oauth_token_handler))
        .route("/v1.0/me", get(get_me_handler))
        .route("/v1.0/me/mailFolders", get(list_mail_folders_handler))
        .route(
            "/v1.0/me/mailFolders/{folder_id}/messages",
            get(list_folder_messages_handler),
        )
        .route(
            "/v1.0/me/mailFolders/{folder_id}/messages/delta",
            get(delta_folder_messages_handler),
        )
        .route("/v1.0/me/messages", get(list_all_messages_handler))
        .route(
            "/v1.0/me/messages/{msg_id}",
            get(get_message_handler).patch(patch_message_handler),
        )
        .route(
            "/v1.0/me/messages/{msg_id}/attachments",
            get(list_message_attachments_handler),
        )
        .route(
            "/v1.0/me/messages/{msg_id}/attachments/{att_id}",
            get(get_attachment_meta_handler),
        )
        .route(
            "/v1.0/me/messages/{msg_id}/attachments/{att_id}/$value",
            get(get_attachment_value_handler),
        )
        .route("/v1.0/me/sendMail", post(send_mail_handler))
        .route("/v1.0/me/messages/{msg_id}/reply", post(reply_mail_handler))
        .route("/v1.0/me/messages/{msg_id}/forward", post(forward_mail_handler))
        .route("/v1.0/me/messages/{msg_id}/move", post(move_mail_handler))
        .route("/v1.0/me/calendarView", get(calendar_view_handler))
        .route("/v1.0/me/events", post(create_event_handler))
        .route(
            "/v1.0/me/events/{event_id}",
            axum::routing::delete(delete_event_handler),
        )
        .route(
            "/v1.0/me/events/{event_id}/{action_name}",
            post(rsvp_event_handler),
        )
        .route("/v1.0/me/contacts", get(list_contacts_handler))
        .route("/v1.0/users", get(list_directory_users_handler))
        .route("/v1.0/me/todo/lists", get(list_todo_lists_handler))
        .route(
            "/v1.0/me/todo/lists/{list_id}/tasks",
            get(list_todo_tasks_handler).post(create_todo_task_handler),
        )
        .route(
            "/v1.0/me/todo/lists/{list_id}/tasks/{task_id}",
            patch(patch_todo_task_handler).delete(delete_todo_task_handler),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *state.base_url.write().unwrap() = base_url.clone();

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base_url, state, handle)
}

pub(crate) fn test_config_for_server(base_url: &str) -> AppConfig {
    AppConfig {
        client_id: "e2e-client-id".to_string(),
        tenant_id: "tenant-e2e".to_string(),
        auth_base_url: base_url.to_string(),
        graph_base_url: format!("{base_url}/v1.0"),
        share_m365_auth: true,
        ..AppConfig::default()
    }
}

pub(crate) fn make_regression_jwt() -> String {
    let exp = chrono::Utc::now().timestamp() + 7200;
    make_test_jwt(&json!({
        "oid": "regression-user",
        "upn": "alice@contoso.com",
        "name": "Alice Outlook",
        "scp": "User.Read Mail.ReadWrite Mail.Send Calendars.ReadWrite Tasks.ReadWrite",
        "exp": exp,
    }))
}

pub(crate) async fn call_mcp_tool_json(
    cfg: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    name: &str,
    arguments: Value,
) -> (bool, Value) {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let resp = mcp::handle_rpc_line(cfg, graph, storage, &req.to_string())
        .await
        .expect("mcp response");
    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string()));
    (is_error, parsed)
}

pub(crate) fn run_outlook_cli(
    dir: &TempDir,
    base_url: &str,
    token: &str,
    args: &[&str],
) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_outlook"))
        .args(args)
        .env("OUTLOOK_GRAPH_BASE_URL", format!("{base_url}/v1.0"))
        .env("OUTLOOK_AUTH_BASE_URL", base_url)
        .env("OUTLOOK_ACCESS_TOKEN", token)
        .env("OUTLOOK_DISABLE_KEYRING", "1")
        .env("OUTLOOK_SHARE_M365_AUTH", "0")
        .env("OUTLOOK_AUTO_DAEMON", "0")
        .env("OUTLOOK_CONFIG_DIR", dir.path())
        .env("OUTLOOK_DATA_DIR", dir.path())
        .env_remove("OUTLOOK_TOKEN_FILE")
        .env_remove("OUTLOOK_TOKEN_REFRESH_CMD")
        .env_remove("TEAMS_TOKEN_FILE")
        .env_remove("TEAMS_CONFIG_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("spawn outlook cli")
}

pub(crate) fn plan_cursor_scope(parts: &[&str]) -> String {
    let mut h: u32 = 0x811c9dc5;
    for b in parts.join("\u{1f}").bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

pub(crate) fn plan_mcp_runtime(dir: &TempDir, base_url: &str, token: &str) -> (AppConfig, GraphClient, Storage) {
    let cfg = test_config_for_server(base_url);
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: token.to_string(),
        refresh_token: None,
        expires_at: now + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("regression-user".into()),
        user_display_name: Some("Alice Outlook".into()),
        user_email: Some("alice@contoso.com".into()),
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth).unwrap();
    let storage = Storage::open(&dir.path().join("regression-user").join("cache.db")).unwrap();
    (cfg, graph, storage)
}
