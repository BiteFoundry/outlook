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

fn ensure_isolated_env() {
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

fn make_test_jwt(payload: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
    format!("{header}.{body}.sig")
}

fn make_test_png_bytes(width: u32, height: u32) -> Vec<u8> {
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
struct MockGraphState {
    base_url: RwLock<String>,
    expected_token: RwLock<String>,
    refreshed_token_to_issue: RwLock<String>,
    refresh_calls: AtomicUsize,
    me_auth_headers: RwLock<Vec<String>>,
    list_queries: RwLock<Vec<String>>,
    attachment_fetch_calls: AtomicUsize,
    delta_calls: AtomicUsize,
    sent_mail_payloads: RwLock<Vec<Value>>,
    replied_ids: RwLock<Vec<(String, Value)>>,
    forwarded_ids: RwLock<Vec<(String, Value)>>,
    patched_messages: RwLock<Vec<(String, Value)>>,
    moved_messages: RwLock<Vec<(String, String)>>,
    created_events: RwLock<Vec<Value>>,
    rsvp_calls: RwLock<Vec<(String, String, Value)>>,
    deleted_event_ids: RwLock<Vec<String>>,
    created_todo_tasks: RwLock<Vec<Value>>,
    patched_todo_tasks: RwLock<Vec<(String, String, Value)>>,
    deleted_todo_tasks: RwLock<Vec<(String, String)>>,
    inline_png_b64: RwLock<String>,
    inline_png_bytes: RwLock<Vec<u8>>,
}

fn check_bearer(headers: &HeaderMap, expected: &str) -> Result<(), Response> {
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if actual == Some(expected) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "code": "InvalidAuthenticationToken",
                    "message": format!("Expected Bearer {expected}, got {actual:?}")
                }
            })),
        )
            .into_response())
    }
}

async fn oauth_token_handler(
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

async fn get_me_handler(State(st): State<Arc<MockGraphState>>, headers: HeaderMap) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn list_mail_folders_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

fn header_message_json(id: &str, subject: &str, preview: &str, is_read: bool) -> Value {
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

async fn list_folder_messages_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(folder_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn list_all_messages_handler(
    State(st): State<Arc<MockGraphState>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn delta_folder_messages_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(folder_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

fn rich_cid_message_html() -> &'static str {
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

async fn get_message_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn patch_message_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.patched_messages
        .write()
        .unwrap()
        .push((msg_id, payload));
    StatusCode::OK.into_response()
}

async fn list_message_attachments_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn get_attachment_meta_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((_msg_id, att_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn get_attachment_value_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.attachment_fetch_calls.fetch_add(1, Ordering::SeqCst);
    let bytes = st.inline_png_bytes.read().unwrap().clone();
    ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
}

async fn send_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.sent_mail_payloads.write().unwrap().push(payload);
    StatusCode::ACCEPTED.into_response()
}

async fn reply_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.replied_ids.write().unwrap().push((msg_id, payload));
    StatusCode::ACCEPTED.into_response()
}

async fn forward_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.forwarded_ids.write().unwrap().push((msg_id, payload));
    StatusCode::ACCEPTED.into_response()
}

async fn move_mail_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(msg_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn calendar_view_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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
fn graph_all_day_rejection(payload: &Value) -> Option<Response> {
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
fn graph_wire_time(endpoint: &Value) -> Value {
    let raw = endpoint["dateTime"].as_str().unwrap_or_default();
    let naive = raw.trim_end_matches('Z');
    let date_time = if naive.contains('.') {
        naive.to_string()
    } else {
        format!("{naive}.0000000")
    };
    json!({ "dateTime": date_time, "timeZone": endpoint["timeZone"] })
}

async fn create_event_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn delete_event_handler(
    State(st): State<Arc<MockGraphState>>,
    Path(event_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.deleted_event_ids.write().unwrap().push(event_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn rsvp_event_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((event_id, action_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.rsvp_calls
        .write()
        .unwrap()
        .push((event_id, action_name, payload));
    StatusCode::ACCEPTED.into_response()
}

async fn list_contacts_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn list_directory_users_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn list_todo_lists_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn list_todo_tasks_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn create_todo_task_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn patch_todo_task_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((list_id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
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

async fn delete_todo_task_handler(
    State(st): State<Arc<MockGraphState>>,
    Path((list_id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.deleted_todo_tasks
        .write()
        .unwrap()
        .push((list_id, task_id));
    StatusCode::NO_CONTENT.into_response()
}

async fn spawn_mock_graph_server(
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

fn test_config_for_server(base_url: &str) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.client_id = "e2e-client-id".to_string();
    cfg.tenant_id = "tenant-e2e".to_string();
    cfg.auth_base_url = base_url.to_string();
    cfg.graph_base_url = format!("{base_url}/v1.0");
    cfg.share_m365_auth = true;
    cfg
}

#[tokio::test]
async fn e2e_mock_graph_auth_shared_m365_mail_delta_and_cid_sixel_preview() {
    ensure_isolated_env();
    let (base_url, state, server) = spawn_mock_graph_server("refreshed-outlook-tok-1").await;
    let cfg = test_config_for_server(&base_url);

    // 1. Expired StoredTokens auto-refreshes via OAuth /token before /me
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now().timestamp();
    let expired = StoredTokens {
        access_token: "expired-tok-0".into(),
        refresh_token: Some("rt-initial".into()),
        expires_at: now - 300,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("user-e2e-1".into()),
        user_display_name: Some("Alice Outlook".into()),
        user_email: Some("alice@contoso.com".into()),
        auth_mode: "pkce".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), expired);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();

    let me = graph.get_me().await.expect("get_me after auto-refresh");
    assert_eq!(me.id, "user-e2e-1");
    assert_eq!(me.email.as_deref(), Some("alice@contoso.com"));
    assert_eq!(state.refresh_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.me_auth_headers.read().unwrap().as_slice(),
        &["Bearer refreshed-outlook-tok-1".to_string()]
    );

    // 2. share_m365_auth shared token_file & shared session bootstrap
    {
        let prev_outlook_cfg = std::env::var_os("OUTLOOK_CONFIG_DIR");
        let prev_teams_cfg = std::env::var_os("TEAMS_CONFIG_DIR");
        let outlook_shared_dir = dir.path().join("outlook-shared");
        let teams_dir = dir.path().join("teams-cli");
        std::fs::create_dir_all(&outlook_shared_dir).unwrap();
        std::fs::create_dir_all(&teams_dir).unwrap();
        std::env::set_var("OUTLOOK_CONFIG_DIR", &outlook_shared_dir);
        std::env::set_var("TEAMS_CONFIG_DIR", &teams_dir);

        let shared_tok_file = dir.path().join("shared_token.txt");
        std::fs::write(
            teams_dir.join("config.toml"),
            format!(
                "token_file = \"{}\"\n",
                shared_tok_file.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        // 2a. Teams-only token is rejected
        let teams_only_jwt = make_test_jwt(&json!({
            "exp": now + 3600,
            "scp": "User.Read Chat.ReadWrite"
        }));
        std::fs::write(&shared_tok_file, &teams_only_jwt).unwrap();
        let auth_skip = AuthManager::new(cfg.clone()).unwrap();
        assert!(auth_skip.valid_access_token().await.is_err());

        // 2b. Outlook-scoped shared token_file is reused
        let outlook_shared_jwt = make_test_jwt(&json!({
            "exp": now + 3600,
            "scp": "User.Read Mail.ReadWrite Calendars.Read"
        }));
        std::fs::write(&shared_tok_file, &outlook_shared_jwt).unwrap();
        *state.expected_token.write().unwrap() = outlook_shared_jwt.clone();
        let auth_file = AuthManager::new(cfg.clone()).unwrap();
        assert_eq!(
            auth_file.current_tokens().await.unwrap().auth_mode,
            "shared_teams_token_file"
        );
        let graph_shared_file = GraphClient::new(cfg.clone(), auth_file).unwrap();
        assert_eq!(graph_shared_file.get_me().await.unwrap().id, "user-e2e-1");

        // 2c. Shared teams-cli session with Chat.Read + refresh_token silently upgrades
        std::fs::remove_file(&shared_tok_file).unwrap();
        std::fs::remove_file(teams_dir.join("config.toml")).unwrap();
        let teams_session_tok = StoredTokens {
            access_token: make_test_jwt(&json!({
                "exp": now + 3600,
                "scp": "User.Read Chat.Read"
            })),
            refresh_token: Some("rt-teams-shared".into()),
            expires_at: now + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("user-e2e-1".into()),
            user_display_name: Some("Alice Outlook".into()),
            user_email: Some("alice@contoso.com".into()),
            auth_mode: "pkce".into(),
        };
        let teams_creds_json = serde_json::to_vec_pretty(&teams_session_tok).unwrap();
        std::fs::write(teams_dir.join("credentials.json"), teams_creds_json).unwrap();
        let upgraded_jwt = make_test_jwt(&json!({
            "exp": now + 3600,
            "scp": "User.Read Mail.ReadWrite Mail.Send Calendars.ReadWrite Contacts.Read Tasks.ReadWrite"
        }));
        *state.refreshed_token_to_issue.write().unwrap() = upgraded_jwt.clone();

        let auth_upgrade = AuthManager::new(cfg.clone()).unwrap();
        let tok = auth_upgrade.valid_access_token().await.unwrap();
        assert_eq!(tok, upgraded_jwt);
        assert_eq!(
            auth_upgrade.current_tokens().await.unwrap().auth_mode,
            "shared_teams_session"
        );
        let graph_upgraded = GraphClient::new(cfg.clone(), auth_upgrade).unwrap();
        assert_eq!(graph_upgraded.get_me().await.unwrap().id, "user-e2e-1");

        match prev_outlook_cfg {
            Some(v) => std::env::set_var("OUTLOOK_CONFIG_DIR", v),
            None => std::env::remove_var("OUTLOOK_CONFIG_DIR"),
        }
        match prev_teams_cfg {
            Some(v) => std::env::set_var("TEAMS_CONFIG_DIR", v),
            None => std::env::remove_var("TEAMS_CONFIG_DIR"),
        }
    }

    // Restore expected token to the first AuthManager's refreshed token for remaining mail ops
    let active_tok = auth.valid_access_token().await.unwrap();
    *state.expected_token.write().unwrap() = active_tok;

    // 3. list_mail_folders + Header-first list_mail_messages + SQLite persistence
    let db_path = dir.path().join("cache.db");
    let storage = Storage::open(&db_path).expect("open sqlite storage");

    let folders = graph.list_mail_folders().await.expect("list_mail_folders");
    assert_eq!(folders.len(), 4);
    assert!(folders.iter().any(|f| f.well_known_name.as_deref() == Some("inbox")));
    assert!(folders.iter().any(|f| f.well_known_name.as_deref() == Some("archive")));
    storage.upsert_mail_folders(&folders).unwrap();

    let header_msgs = graph
        .list_mail_messages("inbox", 20, true)
        .await
        .expect("header-first list_mail_messages");
    assert_eq!(header_msgs.len(), 2);
    assert!(header_msgs.iter().all(|m| !m.body_loaded));
    let last_list_q = state.list_queries.read().unwrap().last().unwrap().clone();
    assert!(
        last_list_q.contains("$select=") && !last_list_q.contains(",body,"),
        "header-first select must omit full body: {last_list_q}"
    );
    storage.upsert_mail_messages(&header_msgs).unwrap();

    // 4. Full message hydration + HTML table / <style> noise / QuotedHistory / cid: inline image
    let hydrated = graph
        .get_mail_message("msg-html-cid")
        .await
        .expect("get_mail_message");
    assert!(hydrated.body_loaded);
    assert_eq!(hydrated.attachments.len(), 1);
    assert!(hydrated.attachments[0].is_inline);
    assert_eq!(
        hydrated.attachments[0].content_id.as_deref(),
        Some("chart01@contoso")
    );
    assert!(hydrated.attachments[0].content_bytes_b64.is_some());
    storage.upsert_mail_messages(&[hydrated.clone()]).unwrap();

    let blocks = parser::html_in::parse_message_blocks(
        &hydrated.body_html,
        &hydrated.attachments,
    );
    let plain = parser::html_in::blocks_to_plain(&blocks);
    assert!(!plain.contains("secret-noise"));
    assert!(!plain.contains("#ff0000"));
    assert!(blocks.iter().any(|b| matches!(b, RichBlock::Table { .. })));
    assert!(blocks.iter().any(|b| matches!(
        b,
        RichBlock::InlineImageChip {
            content_id: Some(cid),
            ..
        } if cid == "chart01@contoso"
    )));
    assert!(blocks
        .iter()
        .any(|b| matches!(b, RichBlock::QuotedHistory { .. })));

    // 5. delta_mail_messages cursor persistence without overwriting hydrated body_loaded == true bodies
    let (d1_msgs, d1_removed, d1_link) = graph
        .delta_mail_messages("inbox", None, 50)
        .await
        .expect("initial delta");
    assert_eq!(d1_msgs.len(), 2);
    assert!(d1_removed.is_empty());
    let d1_link = d1_link.expect("initial deltaLink");
    assert!(d1_link.contains("deltatoken=d1"));
    storage.upsert_mail_messages(&d1_msgs).unwrap();
    storage
        .upsert_sync_cursor(&SyncCursor {
            scope_key: "mail:inbox".into(),
            delta_link: Some(d1_link.clone()),
            history_next_link: None,
            last_synced_at: "2026-11-18T10:05:00Z".into(),
        })
        .unwrap();

    let (d2_msgs, d2_removed, d2_link) = graph
        .delta_mail_messages("inbox", Some(&d1_link), 50)
        .await
        .expect("incremental delta");
    assert_eq!(d2_msgs.len(), 2);
    assert_eq!(d2_removed, vec!["msg-to-remove".to_string()]);
    let d2_link = d2_link.expect("incremental deltaLink");
    assert!(d2_link.contains("deltatoken=d2"));
    storage.upsert_mail_messages(&d2_msgs).unwrap();
    storage.delete_mail_messages_by_ids(&d2_removed).unwrap();
    storage
        .upsert_sync_cursor(&SyncCursor {
            scope_key: "mail:inbox".into(),
            delta_link: Some(d2_link.clone()),
            history_next_link: None,
            last_synced_at: "2026-11-18T10:06:00Z".into(),
        })
        .unwrap();

    assert_eq!(
        storage
            .get_sync_cursor("mail:inbox")
            .unwrap()
            .unwrap()
            .delta_link
            .as_deref(),
        Some(d2_link.as_str())
    );
    let stored_hydrated = storage
        .get_mail_message("msg-html-cid")
        .unwrap()
        .expect("hydrated message remains in storage");
    assert!(stored_hydrated.body_loaded);
    assert!(stored_hydrated.is_read, "header delta must update is_read");
    assert!(
        stored_hydrated.body_html.contains("cid:chart01@contoso"),
        "delta header must not clobber hydrated body_html"
    );
    assert!(
        stored_hydrated.attachments[0].content_bytes_b64.is_some(),
        "delta header must not clobber expanded attachments"
    );
    assert!(storage.get_mail_message("msg-to-remove").unwrap().is_none());

    // 6. img_preview Halfblock, Kitty, iTerm2 & pure-Rust Sixel from fetched cid: bytes + zero-network ImagePreviewOverlay
    let fetch_before = state.attachment_fetch_calls.load(Ordering::SeqCst);
    let (_att_name, att_ct, fetched_png) = graph
        .fetch_mail_attachment_bytes("msg-html-cid", "cid:chart01@contoso")
        .await
        .expect("fetch cid attachment bytes");
    assert_eq!(att_ct, "image/png");
    assert_eq!(fetched_png, *state.inline_png_bytes.read().unwrap());
    assert!(state.attachment_fetch_calls.load(Ordering::SeqCst) > fetch_before);

    let halfblock = img_preview::render_ansi_stdout_with_pref(
        &fetched_png,
        24,
        8,
        ImageProtocolPreference::Halfblock,
    )
    .expect("halfblock render");
    assert!(halfblock.contains("\x1b[38;2;") || halfblock.contains('▀'));

    let kitty = img_preview::render_ansi_stdout_with_pref(
        &fetched_png,
        24,
        8,
        ImageProtocolPreference::Kitty,
    )
    .expect("kitty render");
    assert!(kitty.starts_with("\x1b_G") && kitty.trim_end().ends_with("\x1b\\"));

    let iterm2 = img_preview::render_ansi_stdout_with_pref(
        &fetched_png,
        24,
        8,
        ImageProtocolPreference::Iterm2,
    )
    .expect("iterm2 render");
    assert!(iterm2.starts_with("\x1b]1337;File="));

    let sixel = img_preview::render_ansi_stdout_with_pref(
        &fetched_png,
        24,
        8,
        ImageProtocolPreference::Sixel,
    )
    .expect("sixel render");
    assert!(
        sixel.contains("\x1bP0;1;q") && sixel.trim_end().ends_with("\x1b\\"),
        "expected Sixel DCS payload, got: {:?}",
        &sixel[..sixel.len().min(64)]
    );

    // Verify ImagePreviewOverlay uses preloaded content_bytes_b64 with ZERO redundant Graph attachment fetches
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctl = sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ui_app = UiAppState::new(cfg.clone(), auth.clone(), graph.clone(), storage.clone(), sync_ctl, ui_tx);
    ui_app.mail_messages = vec![stored_hydrated.clone()];
    ui_app.active_mail_id = Some(stored_hydrated.id.clone());
    let mut ui_state = UiState::from_config(&cfg);
    ui_state.term_width = 100;
    ui_state.term_height = 30;

    let calls_before_overlay = state.attachment_fetch_calls.load(Ordering::SeqCst);
    let _ = action::dispatch(Action::OpenImagePreview, &mut ui_app, &mut ui_state);
    assert!(matches!(
        ui_state.overlays.last(),
        Some(Overlay::ImagePreview(_))
    ));
    let mut decoded_ok = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(msg)) = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            ui_rx.recv(),
        )
        .await
        {
            ui_app.handle_ui_msg(&mut ui_state, msg);
            if let Some(Overlay::ImagePreview(ref ov)) = ui_state.overlays.last() {
                if let Some(cur) = ov.current() {
                    let (view_cols, view_rows) = ImagePreviewOverlay::viewport_dimensions(
                        ui_state.term_width,
                        ui_state.term_height,
                    );
                    let key = UiAppState::preview_image_cache_key(
                        cur.owner_id(),
                        cur.att_id(),
                        view_cols,
                        view_rows,
                        ov.zoom_pct,
                        ui_state.graphics_caps.cell_px,
                        ui_state.graphics_caps.protocol,
                    );
                    if ui_app.image_cache.contains_key(&key) {
                        decoded_ok = true;
                        break;
                    }
                }
            }
        }
    }
    assert!(decoded_ok, "ImagePreviewOverlay must decode inline cid image");
    assert_eq!(
        state.attachment_fetch_calls.load(Ordering::SeqCst),
        calls_before_overlay,
        "ImagePreviewOverlay must NOT make redundant network call when content_bytes_b64 is present"
    );
    ui_state.close_overlays_of(OverlayKind::ImagePreview);
    assert!(ImagePreviewOverlay::from_mail_message(&stored_hydrated, None).is_some());

    // 7. send_mail (inline_images cid:), reply_mail, forward_mail, set_mail_read, set_mail_flag, move_mail_message("archive"), FTS5 search_mail
    graph
        .send_mail(&OutgoingMail {
            subject: "Inline Logo Report".into(),
            body_html: "<p>Logo: <img src=\"cid:logo@contoso\"></p>".into(),
            to: vec!["carol@contoso.com".into()],
            inline_images: vec![InlineImageUpload {
                temp_id: "tmp-1".into(),
                content_id: "logo@contoso".into(),
                file_name: "logo.png".into(),
                content_type: "image/png".into(),
                bytes: fetched_png.clone(),
            }],
            save_to_sent_items: true,
            ..Default::default()
        })
        .await
        .expect("send_mail with inline cid");
    let sent = state.sent_mail_payloads.read().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["message"]["attachments"][0]["isInline"], true);
    assert_eq!(
        sent[0]["message"]["attachments"][0]["contentId"],
        "logo@contoso"
    );
    drop(sent);

    graph
        .reply_mail("msg-html-cid", "<p>Thanks Bob!</p>", false)
        .await
        .expect("reply_mail");
    assert_eq!(state.replied_ids.read().unwrap().len(), 1);

    graph
        .forward_mail("msg-html-cid", &["carol@contoso.com".into()], "<p>FYI</p>")
        .await
        .expect("forward_mail");
    assert_eq!(state.forwarded_ids.read().unwrap().len(), 1);

    graph.set_mail_read("msg-html-cid", false).await.unwrap();
    storage.set_mail_read_local("msg-html-cid", false).unwrap();
    graph.set_mail_flag("msg-html-cid", true).await.unwrap();
    storage.set_mail_flagged_local("msg-html-cid", true).unwrap();

    let moved = graph
        .move_mail_message("msg-html-cid", "archive")
        .await
        .expect("move_mail_message archive");
    assert_eq!(moved.folder_id.as_deref(), Some("folder-archive"));
    storage
        .move_mail_folder_local("msg-html-cid", "folder-archive")
        .unwrap();
    assert_eq!(
        storage
            .get_mail_message("msg-html-cid")
            .unwrap()
            .unwrap()
            .folder_id
            .as_deref(),
        Some("folder-archive")
    );

    let fts_en = storage.search_mail("Latency", None, 10).unwrap();
    assert!(fts_en.iter().any(|h| h.id == "msg-html-cid"));
    let fts_ko = storage.search_mail("롤아웃", None, 10).unwrap();
    assert!(fts_ko.iter().any(|h| h.id == "msg-delta-new"));

    server.abort();
}

#[tokio::test]
async fn e2e_mock_graph_calendar_rsvp_contacts_and_todo() {
    ensure_isolated_env();
    let (base_url, state, server) = spawn_mock_graph_server("cal-todo-tok-1").await;
    let cfg = test_config_for_server(&base_url);
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: "cal-todo-tok-1".into(),
        refresh_token: None,
        expires_at: now + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("user-e2e-1".into()),
        user_display_name: Some("Alice Outlook".into()),
        user_email: Some("alice@contoso.com".into()),
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg, auth).unwrap();
    let storage = Storage::open(&dir.path().join("cal_todo.db")).unwrap();

    // 1. Calendar view, create, RSVP (accept, tentativelyAccept, decline), delete
    let events: Vec<CalendarEvent> = graph
        .list_calendar_view("2026-11-18T00:00:00Z", "2026-11-26T00:00:00Z", 20)
        .await
        .expect("list_calendar_view");
    assert_eq!(events.len(), 1);
    assert!(events[0].is_online_meeting);
    assert!(events[0]
        .online_meeting_url
        .as_deref()
        .unwrap()
        .contains("teams.microsoft.com/l/meetup-join/"));
    storage.upsert_calendar_events(&events).unwrap();

    let created_ev = graph
        .create_calendar_event(&OutgoingEvent {
            subject: "Design Kickoff".into(),
            start_iso: "2026-11-21T14:00:00Z".into(),
            end_iso: "2026-11-21T15:00:00Z".into(),
            time_zone: Some("UTC".into()),
            is_all_day: false,
            location: Some("Teams".into()),
            body_html: Some("<p>Kickoff</p>".into()),
            attendees: vec!["bob@contoso.com".into()],
            is_online_meeting: true,
        })
        .await
        .expect("create_calendar_event");
    assert_eq!(created_ev.id, "evt-created-1");
    assert_eq!(
        state.created_events.read().unwrap()[0]["onlineMeetingProvider"],
        "teamsForBusiness"
    );

    graph
        .respond_calendar_event("evt-existing-1", EventRsvpAction::Accept, Some("See you there"))
        .await
        .unwrap();
    storage
        .update_event_response_local("evt-existing-1", "accepted")
        .unwrap();
    graph
        .respond_calendar_event(
            "evt-existing-1",
            EventRsvpAction::TentativelyAccept,
            Some("Might join late"),
        )
        .await
        .unwrap();
    graph
        .respond_calendar_event("evt-existing-1", EventRsvpAction::Decline, None)
        .await
        .unwrap();
    let rsvps = state.rsvp_calls.read().unwrap();
    assert_eq!(rsvps[0].1, "accept");
    assert_eq!(rsvps[1].1, "tentativelyAccept");
    assert_eq!(rsvps[2].1, "decline");
    drop(rsvps);

    graph
        .delete_calendar_event("evt-created-1")
        .await
        .expect("delete_calendar_event");
    assert_eq!(
        state.deleted_event_ids.read().unwrap().as_slice(),
        &["evt-created-1".to_string()]
    );

    // 2. MeetingRequestInfo invite email parsing
    let invite_msg: MailMessage = graph
        .get_mail_message("msg-invite-1")
        .await
        .expect("get_mail_message invite");
    let mr = invite_msg
        .meeting_request
        .expect("MeetingRequestInfo parsed");
    assert_eq!(mr.meeting_message_type.as_deref(), Some("meetingRequest"));
    assert_eq!(mr.event_id.as_deref(), Some("evt-invite-1"));
    assert_eq!(mr.start_iso.as_deref(), Some("2026-11-20T15:00:00Z"));
    assert_eq!(mr.end_iso.as_deref(), Some("2026-11-20T16:00:00Z"));
    assert_eq!(mr.location.as_deref(), Some("Conf Room A"));
    assert!(mr
        .online_meeting_url
        .as_deref()
        .unwrap()
        .contains("teams.microsoft.com/l/meetup-join/"));
    assert_eq!(mr.response_status.as_deref(), Some("notResponded"));

    // 3. Contacts & directory search (with email deduplication)
    let contacts = graph.list_contacts(20).await.expect("list_contacts");
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].source, "contact");

    let merged_contacts = graph
        .search_contacts_and_directory("contoso.com", 20)
        .await
        .expect("search_contacts_and_directory");
    assert_eq!(merged_contacts.len(), 2, "bob@contoso.com should be deduplicated");
    assert!(merged_contacts.iter().any(|c| c.display_name == "Bob Reviewer" && c.source == "contact"));
    assert!(merged_contacts.iter().any(|c| c.display_name == "Carol Organizer" && c.source == "directory"));
    storage.upsert_contacts(&merged_contacts).unwrap();
    assert_eq!(storage.list_contacts(Some("carol"), 10).unwrap().len(), 1);

    // 4. Microsoft To Do CRUD
    let todo_lists = graph.list_todo_lists().await.expect("list_todo_lists");
    assert_eq!(todo_lists.len(), 1);
    storage.upsert_todo_lists(&todo_lists).unwrap();

    let created_task: TodoTask = graph
        .create_todo_task(
            "default",
            &OutgoingTodoTask {
                title: "Ship Outlook v0.1.0".into(),
                body_plain: Some("Run zero-warning gate".into()),
                due_date_iso: Some("2026-11-22T00:00:00Z".into()),
                importance: Some("high".into()),
            },
        )
        .await
        .expect("create_todo_task");
    assert_eq!(created_task.id, "task-created-1");
    assert!(!created_task.is_completed());
    storage.upsert_todo_tasks(&[created_task.clone()]).unwrap();

    let completed = graph
        .set_todo_task_completed("default", &created_task.id, true)
        .await
        .expect("set_todo_task_completed true");
    assert!(completed.is_completed());
    storage
        .set_todo_task_completed_local(&created_task.id, true)
        .unwrap();

    let reopened = graph
        .set_todo_task_completed("default", &created_task.id, false)
        .await
        .expect("set_todo_task_completed false");
    assert!(!reopened.is_completed());

    graph
        .delete_todo_task("default", &created_task.id)
        .await
        .expect("delete_todo_task");
    storage.delete_todo_task(&created_task.id).unwrap();
    assert!(storage
        .list_todo_tasks(Some("todo-list-default"), true)
        .unwrap()
        .is_empty());

    server.abort();
}

#[tokio::test]
async fn e2e_agent_surfaces_http_openapi_mcp_and_self_updater() {
    ensure_isolated_env();
    let (base_url, _state, graph_server) = spawn_mock_graph_server("agent-tok-1").await;
    let cfg = test_config_for_server(&base_url);
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: "agent-tok-1".into(),
        refresh_token: None,
        expires_at: now + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("user-e2e-1".into()),
        user_display_name: Some("Alice Outlook".into()),
        user_email: Some("alice@contoso.com".into()),
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth).unwrap();
    let storage = Storage::open(&dir.path().join("agent.db")).unwrap();

    // Seed local SQLite data via Graph
    let folders = graph.list_mail_folders().await.unwrap();
    storage.upsert_mail_folders(&folders).unwrap();
    let msg = graph.get_mail_message("msg-html-cid").await.unwrap();
    storage.upsert_mail_messages(&[msg]).unwrap();
    let events = graph
        .list_calendar_view("2026-11-18T00:00:00Z", "2026-11-26T00:00:00Z", 10)
        .await
        .unwrap();
    storage.upsert_calendar_events(&events).unwrap();
    let todo_lists = graph.list_todo_lists().await.unwrap();
    storage.upsert_todo_lists(&todo_lists).unwrap();
    let tasks = graph.list_todo_tasks("default", true, 20).await.unwrap();
    storage.upsert_todo_tasks(&tasks).unwrap();

    // 1. Non-loopback --bind auth guard must refuse unauthenticated 0.0.0.0 bind
    let bind_err = serve::run(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        ServeOptions {
            bind: "0.0.0.0".to_string(),
            port: 0,
            api_key: None,
            ..ServeOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        bind_err.to_string().contains("non-loopback") || bind_err.to_string().contains("--token"),
        "unexpected non-loopback error: {bind_err}"
    );

    // 2. HTTP router on local port + /health, /openapi.json, /v1/mail/*, /v1/calendar/events, /v1/todo/tasks
    let router = serve::build_router(cfg.clone(), graph.clone(), storage.clone(), None);
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, router).await;
    });
    let http = reqwest::Client::new();
    let api_base = format!("http://{api_addr}");

    let health: Value = http
        .get(format!("{api_base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));

    let openapi: Value = http
        .get(format!("{api_base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(openapi["openapi"], "3.1.0");
    assert_eq!(openapi["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(openapi["paths"]["/v1/mail/folders"].is_object());
    assert!(openapi["paths"]["/v1/mail/messages"].is_object());
    assert!(openapi["paths"]["/v1/calendar/events"].is_object());
    assert!(openapi["paths"]["/v1/todo/tasks"].is_object());

    let folders_json: Value = http
        .get(format!("{api_base}/v1/mail/folders?offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(folders_json["folders"].as_array().unwrap().len(), 4);

    let msgs_json: Value = http
        .get(format!("{api_base}/v1/mail/messages?folder=inbox&offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(msgs_json["messages"].as_array().unwrap().len(), 1);

    let events_json: Value = http
        .get(format!("{api_base}/v1/calendar/events?offline=true&from=2026-11-18T00:00:00Z&to=2026-11-26T00:00:00Z"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events_json["events"].as_array().unwrap().len(), 1);
    assert_eq!(events_json["events"][0]["start_iso"], "2026-11-19T10:00:00Z");
    assert_eq!(events_json["events"][0]["end_iso"], "2026-11-19T10:30:00Z");

    let tasks_json: Value = http
        .get(format!("{api_base}/v1/todo/tasks?offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tasks_json["tasks"].as_array().unwrap().len(), 1);

    // Bearer-protected router rejects unauthenticated and accepts valid token
    let protected_router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        Some("agent-secret-token".to_string()),
    );
    let prot_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prot_addr = prot_listener.local_addr().unwrap();
    let prot_server = tokio::spawn(async move {
        let _ = axum::serve(prot_listener, protected_router).await;
    });
    let unauth_resp = http
        .get(format!("http://{prot_addr}/v1/mail/folders"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let auth_resp = http
        .get(format!("http://{prot_addr}/v1/mail/folders?offline=true"))
        .bearer_auth("agent-secret-token")
        .send()
        .await
        .unwrap();
    assert_eq!(auth_resp.status(), reqwest::StatusCode::OK);

    // 3. MCP JSON-RPC initialize + tools/list + list_messages + search_mail
    let init_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "outlook");
    assert_eq!(init_resp["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));

    assert!(mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await
    .is_none());

    let tools_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    )
    .await
    .unwrap();
    let tool_names: Vec<&str> = tools_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"list_messages"));
    assert!(tool_names.contains(&"search_mail"));

    let list_call = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_messages","arguments":{"folder":"inbox","offline":true}}}"#,
    )
    .await
    .unwrap();
    let list_text = list_call["result"]["content"][0]["text"].as_str().unwrap();
    assert!(list_text.contains("msg-html-cid"));

    let search_local_call = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_mail","arguments":{"query":"Latency","offline":true}}}"#,
    )
    .await
    .unwrap();
    let search_local_text = search_local_call["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(search_local_text.contains("msg-html-cid"));

    let search_remote_call = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_mail","arguments":{"query":"Architecture","offline":false}}}"#,
    )
    .await
    .unwrap();
    let search_remote_text = search_remote_call["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(search_remote_text.contains("Q4 Metrics & Architecture"));
    assert!(!search_remote_text.contains("body_html"));

    let search_remote_full_call = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search_mail","arguments":{"query":"Architecture","offline":false,"compact":false}}}"#,
    )
    .await
    .unwrap();
    let search_remote_full_text = search_remote_full_call["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(search_remote_full_text.contains("Remote KQL hit"));

    // 4. Self-updater check against a newer release and mandatory SHA256SUMS.txt verification.
    fn next_version() -> String {
        let major: u64 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap();
        format!("{}.0.0", major + 1)
    }
    #[derive(Clone)]
    struct MockGhState {
        base_url: String,
        asset_name: String,
        asset_bytes: Arc<Vec<u8>>,
        sums_text: Arc<RwLock<Option<String>>>,
    }

    let raw_bin_asset = if cfg!(windows) {
        "outlook.exe"
    } else {
        "outlook"
    }
    .to_string();
    let version = next_version();
    let versioned_asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => format!("outlook-{version}-x86_64-pc-windows-msvc.zip"),
        ("windows", "aarch64") => format!("outlook-{version}-aarch64-pc-windows-msvc.zip"),
        ("macos", "x86_64") => format!("outlook-{version}-x86_64-apple-darwin.tar.gz"),
        ("macos", "aarch64") => format!("outlook-{version}-aarch64-apple-darwin.tar.gz"),
        ("linux", "x86_64") => format!("outlook-{version}-x86_64-unknown-linux-gnu.tar.gz"),
        ("linux", "aarch64") => format!("outlook-{version}-aarch64-unknown-linux-gnu.tar.gz"),
        _ => raw_bin_asset.clone(),
    };
    let new_binary = b"NEW-OUTLOOK-VERIFIED-BINARY".to_vec();
    let valid_digest = hex::encode(Sha256::digest(&new_binary));

    // 4a. Check the versioned release archive.
    let gh_listener_check = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gh_addr_check = gh_listener_check.local_addr().unwrap();
    let gh_base_check = format!("http://{gh_addr_check}");
    let ver_asset_clone = versioned_asset.clone();
    let gh_base_clone = gh_base_check.clone();
    let gh_app_check = Router::new().route(
        "/repos/BiteFoundry/outlook/releases/latest",
        get(move || {
            let asset_name = ver_asset_clone.clone();
            let base = gh_base_clone.clone();
            async move {
                Json(json!({
                    "tag_name": format!("v{}", next_version()),
                    "html_url": format!("{base}/BiteFoundry/outlook/releases/tag/v{}", next_version()),
                    "assets": [
                        {
                            "name": asset_name,
                            "size": 1024,
                            "browser_download_url": format!("{base}/download/{asset_name}")
                        },
                        {
                            "name": "SHA256SUMS.txt",
                            "size": 128,
                            "browser_download_url": format!("{base}/download/SHA256SUMS.txt")
                        }
                    ]
                }))
            }
        }),
    );
    let gh_check_server = tokio::spawn(async move {
        let _ = axum::serve(gh_listener_check, gh_app_check).await;
    });

    let check_res = updater::check_or_apply_update_with_api_base(
        &gh_base_check,
        "BiteFoundry/outlook",
        true,
        false,
    )
    .await
    .expect("check_or_apply_update_with_api_base check_only");
    assert!(check_res.update_available);
    assert!(!check_res.updated);
    assert_eq!(check_res.latest_version, version);
    assert_eq!(check_res.asset_name.as_deref(), Some(versioned_asset.as_str()));
    gh_check_server.abort();

    // 4b. Mandatory SHA256SUMS.txt verification (missing -> bad hash -> valid hash)
    let gh_listener_apply = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gh_addr_apply = gh_listener_apply.local_addr().unwrap();
    let gh_base_apply = format!("http://{gh_addr_apply}");
    let gh_state = MockGhState {
        base_url: gh_base_apply.clone(),
        asset_name: raw_bin_asset.clone(),
        asset_bytes: Arc::new(new_binary.clone()),
        sums_text: Arc::new(RwLock::new(None)),
    };
    let gh_app_apply = Router::new()
        .route(
            "/repos/BiteFoundry/outlook/releases/latest",
            get(|State(st): State<MockGhState>| async move {
                let mut assets = vec![json!({
                    "name": st.asset_name,
                    "size": st.asset_bytes.len(),
                    "browser_download_url": format!("{}/download/{}", st.base_url, st.asset_name)
                })];
                if let Some(ref sums) = *st.sums_text.read().unwrap() {
                    assets.push(json!({
                        "name": "SHA256SUMS.txt",
                        "size": sums.len(),
                        "browser_download_url": format!("{}/download/SHA256SUMS.txt", st.base_url)
                    }));
                }
                Json(json!({
                    "tag_name": format!("v{}", next_version()),
                    "html_url": format!("{}/BiteFoundry/outlook/releases/tag/v{}", st.base_url, next_version()),
                    "assets": assets
                }))
            }),
        )
        .route(
            &format!("/download/{raw_bin_asset}"),
            get(|State(st): State<MockGhState>| async move {
                ([(header::CONTENT_TYPE, "application/octet-stream")], (*st.asset_bytes).clone())
            }),
        )
        .route(
            "/download/SHA256SUMS.txt",
            get(|State(st): State<MockGhState>| async move {
                st.sums_text
                    .read()
                    .unwrap()
                    .clone()
                    .unwrap_or_default()
            }),
        )
        .with_state(gh_state.clone());
    let gh_apply_server = tokio::spawn(async move {
        let _ = axum::serve(gh_listener_apply, gh_app_apply).await;
    });

    let fake_exe = dir.path().join(&raw_bin_asset);
    std::fs::write(&fake_exe, b"OLD-OUTLOOK-0.1.0-BINARY").unwrap();

    // Missing SHA256SUMS.txt -> refused
    let err_missing = updater::check_or_apply_update_at(
        &gh_base_apply,
        "BiteFoundry/outlook",
        false,
        false,
        &fake_exe,
        None,
    )
    .await
    .unwrap_err();
    assert!(err_missing.to_string().contains("SHA256SUMS.txt"));
    assert_eq!(std::fs::read(&fake_exe).unwrap(), b"OLD-OUTLOOK-0.1.0-BINARY");

    // Mismatched SHA256SUMS.txt -> refused before replacing fake_exe
    *gh_state.sums_text.write().unwrap() = Some(format!(
        "0000000000000000000000000000000000000000000000000000000000000000  {raw_bin_asset}\n"
    ));
    let err_bad_hash = updater::check_or_apply_update_at(
        &gh_base_apply,
        "BiteFoundry/outlook",
        false,
        false,
        &fake_exe,
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err_bad_hash:#}").contains("SHA-256"));
    assert_eq!(std::fs::read(&fake_exe).unwrap(), b"OLD-OUTLOOK-0.1.0-BINARY");

    // Valid SHA256SUMS.txt -> applied cleanly
    *gh_state.sums_text.write().unwrap() = Some(format!("{valid_digest}  {raw_bin_asset}\n"));
    let applied = updater::check_or_apply_update_at(
        &gh_base_apply,
        "BiteFoundry/outlook",
        false,
        false,
        &fake_exe,
        None,
    )
    .await
    .expect("valid SHA256SUMS update succeeds");
    assert!(applied.updated);
    assert_eq!(applied.latest_version, version);
    assert_eq!(std::fs::read(&fake_exe).unwrap(), new_binary);

    gh_apply_server.abort();
    prot_server.abort();
    api_server.abort();
    graph_server.abort();
}

fn make_regression_jwt() -> String {
    let exp = chrono::Utc::now().timestamp() + 7200;
    make_test_jwt(&json!({
        "oid": "regression-user",
        "upn": "alice@contoso.com",
        "name": "Alice Outlook",
        "scp": "User.Read Mail.ReadWrite Mail.Send Calendars.ReadWrite Tasks.ReadWrite",
        "exp": exp,
    }))
}

async fn call_mcp_tool_json(
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

fn run_outlook_cli(
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_02_preserves_cached_attachments_after_move() {
    let token = make_regression_jwt();
    let png_b64 = B64_STANDARD.encode(make_test_png_bytes(8, 8));
    let graph_calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&graph_calls);
    let png_ref = png_b64.clone();

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get({
                let calls = Arc::clone(&calls_ref);
                move || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "value": [
                                { "id": "folder-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 4, "totalItemCount": 4 },
                                { "id": "folder-archive", "displayName": "Archive", "wellKnownName": "archive", "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 }
                            ]
                        }))
                    }
                }
            }),
        )
        .route(
            "/v1.0/me/messages/{id}",
            get({
                let calls = Arc::clone(&calls_ref);
                let png = png_ref.clone();
                move |Path(id): Path<String>| {
                    let calls = Arc::clone(&calls);
                    let png = png.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": id,
                            "parentFolderId": "folder-inbox",
                            "subject": format!("Attachment test {id}"),
                            "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                            "receivedDateTime": "2026-11-20T10:00:00Z",
                            "isRead": false,
                            "isDraft": false,
                            "hasAttachments": true,
                            "bodyPreview": "See report.pdf",
                            "body": { "contentType": "html", "content": "<p>See report.pdf</p>" },
                            "attachments": [
                                {
                                    "id": "att-report-1",
                                    "name": "report.pdf",
                                    "contentType": "application/pdf",
                                    "size": 1024,
                                    "isInline": false
                                },
                                {
                                    "id": "att-chart-1",
                                    "name": "chart.png",
                                    "contentType": "image/png",
                                    "size": 128,
                                    "isInline": false,
                                    "contentBytes": png
                                }
                            ]
                        }))
                    }
                }
            }),
        )
        .route(
            "/v1.0/me/messages/{id}/move",
            post({
                let calls = Arc::clone(&calls_ref);
                move |Path(id): Path<String>, Json(_payload): Json<Value>| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        match id.as_str() {
                            // Standard Graph /move response: includes body, hasAttachments=true, omits attachments
                            "m-att-1" | "m-att-full" | "m-att-mcp" => Json(json!({
                                "id": format!("{id}-moved"),
                                "parentFolderId": "folder-archive",
                                "subject": format!("Attachment test {id}"),
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": true,
                                "bodyPreview": "See report.pdf",
                                "body": { "contentType": "html", "content": "<p>See report.pdf</p>" }
                            }))
                            .into_response(),
                            // Control 1: explicitly expanded empty attachments array
                            "m-explicit-empty" => Json(json!({
                                "id": "m-explicit-empty-moved",
                                "parentFolderId": "folder-archive",
                                "subject": "Attachment test m-explicit-empty",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": true,
                                "bodyPreview": "See report.pdf",
                                "body": { "contentType": "html", "content": "<p>See report.pdf</p>" },
                                "attachments": []
                            }))
                            .into_response(),
                            // Control 2: server explicitly says hasAttachments=false and omits body (tests independent body/attachment judgment)
                            "m-no-att" => Json(json!({
                                "id": "m-no-att-moved",
                                "parentFolderId": "folder-archive",
                                "subject": "Attachment test m-no-att",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "See report.pdf"
                            }))
                            .into_response(),
                            // Control 3: server explicitly provides a new expanded attachments list
                            "m-server-list" => Json(json!({
                                "id": "m-server-list-moved",
                                "parentFolderId": "folder-archive",
                                "subject": "Attachment test m-server-list",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": true,
                                "bodyPreview": "See report.pdf",
                                "body": { "contentType": "html", "content": "<p>See report.pdf</p>" },
                                "attachments": [
                                    {
                                        "id": "att-server-new",
                                        "name": "server_new.txt",
                                        "contentType": "text/plain",
                                        "size": 12,
                                        "isInline": false
                                    }
                                ]
                            }))
                            .into_response(),
                            // Control 4: move failure preserves existing ID and attachments
                            "m-fail-move" => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({ "error": { "code": "ErrorInternalServerError", "message": "boom" } })),
                            )
                                .into_response(),
                            other => (
                                StatusCode::NOT_FOUND,
                                Json(json!({ "error": { "code": "ErrorItemNotFound", "message": other } })),
                            )
                                .into_response(),
                        }
                    }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let dir = tempfile::tempdir().unwrap();

    // Seed all messages into cache via online read ($expand=attachments)
    for id in [
        "m-att-1",
        "m-att-full",
        "m-att-mcp",
        "m-explicit-empty",
        "m-no-att",
        "m-server-list",
        "m-fail-move",
    ] {
        let out = run_outlook_cli(&dir, &base_url, &token, &["read", id, "--json", "--full"]);
        assert!(
            out.status.success(),
            "seed read {id} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 1. Move m-att-1 -> archive (Graph response has body + hasAttachments=true, omits attachments)
    let mv = run_outlook_cli(&dir, &base_url, &token, &["move", "m-att-1", "archive", "--json"]);
    assert!(
        mv.status.success(),
        "move m-att-1 failed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    let mv_stdout = String::from_utf8_lossy(&mv.stdout);
    assert!(
        !mv_stdout.contains("content_bytes_b64"),
        "move --json must not leak content_bytes_b64: {mv_stdout}"
    );
    assert!(
        !mv_stdout.contains("body_html"),
        "move --json default must not include body_html: {mv_stdout}"
    );
    let mv_val: Value = serde_json::from_slice(&mv.stdout).unwrap();
    assert_eq!(mv_val["ok"], true);
    assert_eq!(mv_val["message_id"], "m-att-1-moved");
    assert_eq!(mv_val["previous_message_id"], "m-att-1");
    assert_eq!(mv_val["destination_folder"]["id"], "folder-archive");
    assert!(
        mv_val.get("message").is_none(),
        "move --json default must omit message object: {mv_val}"
    );

    // 1-full. Archive --json --full returns public message with content_cached and without content_bytes_b64
    let mv_full = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["archive", "m-att-full", "--json", "--full"],
    );
    assert!(
        mv_full.status.success(),
        "archive --full failed: {}",
        String::from_utf8_lossy(&mv_full.stderr)
    );
    let mv_full_stdout = String::from_utf8_lossy(&mv_full.stdout);
    assert!(
        !mv_full_stdout.contains("content_bytes_b64"),
        "archive --json --full must not leak content_bytes_b64"
    );
    let mv_full_val: Value = serde_json::from_slice(&mv_full.stdout).unwrap();
    assert_eq!(mv_full_val["message_id"], "m-att-full-moved");
    assert_eq!(mv_full_val["previous_message_id"], "m-att-full");
    let full_atts = mv_full_val["message"]["attachments"].as_array().unwrap();
    assert_eq!(full_atts.len(), 2);
    assert_eq!(full_atts[0]["content_cached"], false);
    assert_eq!(full_atts[1]["content_cached"], true);
    assert!(full_atts[1].get("content_bytes_b64").is_none());

    // 1-mcp. MCP update_message(destination_folder) returns compact moved {id, folder_id}
    let cfg = test_config_for_server(&base_url);
    let tok = StoredTokens {
        access_token: token.clone(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("regression-user".into()),
        user_display_name: Some("Alice Outlook".into()),
        user_email: Some("alice@contoso.com".into()),
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth).unwrap();
    let db_path = dir.path().join("regression-user").join("cache.db");
    let storage = Storage::open(&db_path).unwrap();
    let (mcp_err, mcp_val) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "update_message",
        json!({ "message_id": "m-att-mcp", "destination_folder": "archive" }),
    )
    .await;
    assert!(!mcp_err, "MCP update_message move failed: {mcp_val}");
    assert_eq!(mcp_val["message_id"], "m-att-mcp-moved");
    assert_eq!(
        mcp_val["moved"],
        json!({ "id": "m-att-mcp-moved", "folder_id": "folder-archive" })
    );
    drop(storage);

    let calls_before_offline = graph_calls.load(Ordering::SeqCst);
    // 1a. read --cached on new ID MUST expose report.pdf & chart.png with IDs
    let read_cached = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-att-1-moved", "--cached", "--json", "--full"],
    );
    assert!(
        read_cached.status.success(),
        "read --cached failed: {}",
        String::from_utf8_lossy(&read_cached.stderr)
    );
    let cached_val: Value = serde_json::from_slice(&read_cached.stdout).unwrap();
    let atts = cached_val["message"]["attachments"].as_array().unwrap();
    assert_eq!(
        atts.len(),
        2,
        "cached attachments must survive move/archive; got {atts:?}"
    );
    assert_eq!(atts[0]["name"], "report.pdf");
    assert_eq!(atts[0]["id"], "att-report-1");
    assert_eq!(atts[1]["name"], "chart.png");
    assert_eq!(atts[1]["id"], "att-chart-1");
    assert_eq!(
        graph_calls.load(Ordering::SeqCst),
        calls_before_offline,
        "read --cached must not call Graph"
    );

    // 1b. Offline preview of cached-byte image succeeds
    let prev_ok = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "preview",
            "m-att-1-moved",
            "--attachment",
            "chart.png",
            "--offline",
            "--protocol",
            "halfblock",
        ],
    );
    assert!(
        prev_ok.status.success(),
        "offline preview of cached-byte attachment must succeed: {}",
        String::from_utf8_lossy(&prev_ok.stderr)
    );
    assert!(
        !prev_ok.stdout.is_empty(),
        "offline preview should render image output"
    );

    // 1c. Offline preview of metadata-only report.pdf returns existing uncached-bytes error (not 'no attachments')
    let prev_uncached = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "preview",
            "m-att-1-moved",
            "--attachment",
            "report.pdf",
            "--offline",
            "--protocol",
            "halfblock",
        ],
    );
    assert!(!prev_uncached.status.success());
    let prev_err = String::from_utf8_lossy(&prev_uncached.stderr);
    assert!(
        prev_err.contains("Cannot fetch uncached attachment bytes in offline mode"),
        "expected uncached-bytes error, got: {prev_err}"
    );
    assert_eq!(
        graph_calls.load(Ordering::SeqCst),
        calls_before_offline,
        "offline preview must make 0 Graph calls"
    );

    // 2. Control 1: explicitly expanded empty list [] MUST NOT revive old attachments
    let mv_empty = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["move", "m-explicit-empty", "archive", "--json"],
    );
    assert!(mv_empty.status.success());
    let read_empty = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-explicit-empty-moved", "--cached", "--json", "--full"],
    );
    assert!(read_empty.status.success());
    let empty_val: Value = serde_json::from_slice(&read_empty.stdout).unwrap();
    assert_eq!(
        empty_val["message"]["attachments"].as_array().unwrap().len(),
        0,
        "explicit attachments=[] must not revive old cached attachments"
    );

    // 3. Control 2: hasAttachments=false with body omitted preserves body but MUST NOT revive old attachments
    let mv_no_att = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["move", "m-no-att", "archive", "--json"],
    );
    assert!(mv_no_att.status.success());
    let read_no_att = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-no-att-moved", "--cached", "--json", "--full"],
    );
    assert!(read_no_att.status.success());
    let no_att_val: Value = serde_json::from_slice(&read_no_att.stdout).unwrap();
    assert_eq!(no_att_val["message"]["body_loaded"], true);
    assert_eq!(no_att_val["message"]["has_attachments"], false);
    assert_eq!(
        no_att_val["message"]["attachments"].as_array().unwrap().len(),
        0,
        "hasAttachments=false must not revive old cached attachments"
    );

    // 4. Control 3: explicit expanded non-empty list adopts server list
    let mv_srv = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["move", "m-server-list", "archive", "--json"],
    );
    assert!(mv_srv.status.success());
    let read_srv = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-server-list-moved", "--cached", "--json", "--full"],
    );
    assert!(read_srv.status.success());
    let srv_val: Value = serde_json::from_slice(&read_srv.stdout).unwrap();
    let srv_atts = srv_val["message"]["attachments"].as_array().unwrap();
    assert_eq!(srv_atts.len(), 1);
    assert_eq!(srv_atts[0]["id"], "att-server-new");
    assert_eq!(srv_atts[0]["name"], "server_new.txt");

    // 5. Control 4: move failure preserves existing ID and attachments, and emits structured JSON error under --json
    let mv_fail = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["move", "m-fail-move", "archive", "--json"],
    );
    assert!(!mv_fail.status.success());
    let mv_fail_json: Value = serde_json::from_slice(&mv_fail.stdout).expect("move --json failure must emit JSON on stdout");
    assert_eq!(mv_fail_json["ok"], false);
    assert!(mv_fail_json["error"]["code"].is_string());
    assert!(mv_fail_json["error"]["message"].is_string());
    assert!(
        !mv_fail.stderr.is_empty(),
        "move --json failure must still emit message on stderr"
    );

    // 5a. Control: same failure without --json leaves stdout empty and writes Error: to stderr
    let mv_fail_text = run_outlook_cli(&dir, &base_url, &token, &["move", "m-fail-move", "archive"]);
    assert!(!mv_fail_text.status.success());
    assert!(
        mv_fail_text.stdout.is_empty(),
        "non-json failure must leave stdout empty"
    );
    assert!(
        String::from_utf8_lossy(&mv_fail_text.stderr).contains("Error:"),
        "non-json failure must print Error: on stderr"
    );

    // 5b. W2: delete --json without --yes emits confirmation_required with 0 Graph calls
    let calls_before_del = graph_calls.load(Ordering::SeqCst);
    let del_no_yes = run_outlook_cli(&dir, &base_url, &token, &["delete", "m-fail-move", "--json"]);
    assert!(!del_no_yes.status.success());
    let del_json: Value = serde_json::from_slice(&del_no_yes.stdout).expect("delete --json failure must emit JSON on stdout");
    assert_eq!(del_json["ok"], false);
    assert_eq!(del_json["error"]["code"], "confirmation_required");
    assert_eq!(graph_calls.load(Ordering::SeqCst), calls_before_del);

    let read_fail = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-fail-move", "--cached", "--json", "--full"],
    );
    assert!(read_fail.status.success());
    let fail_val: Value = serde_json::from_slice(&read_fail.stdout).unwrap();
    assert_eq!(fail_val["message"]["id"], "m-fail-move");
    assert_eq!(
        fail_val["message"]["attachments"].as_array().unwrap().len(),
        2
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_03_filters_before_limit_across_surfaces() {
    let token = make_regression_jwt();

    let make_msg_json = |folder: &str, id: &str, idx: usize, matches: bool| -> Value {
        let mins_ago = idx as i64;
        let ts = chrono::DateTime::parse_from_rfc3339("2026-11-21T12:00:00Z")
            .unwrap()
            - chrono::Duration::minutes(mins_ago);
        json!({
            "id": id,
            "parentFolderId": folder,
            "subject": format!("Mail {id}"),
            "from": { "emailAddress": { "name": "Sender", "address": "sender@contoso.com" } },
            "receivedDateTime": ts.to_rfc3339(),
            "isRead": !matches,
            "isDraft": false,
            "flag": { "flagStatus": if matches { "flagged" } else { "notFlagged" } },
            "importance": "normal",
            "inferenceClassification": if matches { "focused" } else { "other" },
            "hasAttachments": false,
            "bodyPreview": format!("Preview {id}")
        })
    };

    let inbox_all: Arc<Vec<Value>> = Arc::new(
        (0..40)
            .map(|i| make_msg_json("folder-inbox", &format!("m{i:02}"), i, i >= 20))
            .collect(),
    );
    let sparse_all: Arc<Vec<Value>> = Arc::new(
        (0..23)
            .map(|i| make_msg_json("folder-sparse", &format!("s{i:02}"), i, i >= 20))
            .collect(),
    );
    let base_url_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let page_hits = Arc::new(AtomicUsize::new(0));

    let inbox_for_page = Arc::clone(&inbox_all);
    let sparse_for_page = Arc::clone(&sparse_all);
    let base_for_page = Arc::clone(&base_url_slot);
    let hits_for_page = Arc::clone(&page_hits);

    let inbox_for_delta = Arc::clone(&inbox_all);
    let sparse_for_delta = Arc::clone(&sparse_all);
    let base_for_delta = Arc::clone(&base_url_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [
                        { "id": "folder-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 20, "totalItemCount": 40 },
                        { "id": "folder-sparse", "displayName": "Sparse", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 3, "totalItemCount": 23 }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages",
            get(move |Path(fid): Path<String>, RawQuery(q): RawQuery| {
                let inbox = Arc::clone(&inbox_for_page);
                let sparse = Arc::clone(&sparse_for_page);
                let base_slot = Arc::clone(&base_for_page);
                let hits = Arc::clone(&hits_for_page);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let items = if fid == "folder-sparse" {
                        &sparse[..]
                    } else {
                        &inbox[..]
                    };
                    let q_str = q.unwrap_or_default();
                    let skip: usize = q_str
                        .split('&')
                        .find_map(|p| p.strip_prefix("page_skip="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    // Emit 10 items per page regardless of $top to exercise multi-page nextLink traversal
                    let page_len = 10usize;
                    let end = (skip + page_len).min(items.len());
                    let slice = if skip < items.len() {
                        items[skip..end].to_vec()
                    } else {
                        Vec::new()
                    };
                    let mut resp = json!({ "value": slice });
                    if end < items.len() {
                        let base = base_slot.read().unwrap().clone();
                        resp["@odata.nextLink"] = json!(format!(
                            "{base}/v1.0/me/mailFolders/{fid}/messages?page_skip={end}"
                        ));
                    }
                    Json(resp)
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>, RawQuery(q): RawQuery| {
                let inbox = Arc::clone(&inbox_for_delta);
                let sparse = Arc::clone(&sparse_for_delta);
                let base_slot = Arc::clone(&base_for_delta);
                async move {
                    let items = if fid == "folder-sparse" {
                        &sparse[..]
                    } else {
                        &inbox[..]
                    };
                    let q_str = q.unwrap_or_default();
                    let base = base_slot.read().unwrap().clone();
                    if q_str.contains("$deltatoken=") {
                        return Json(json!({
                            "value": [],
                            "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=done")
                        }));
                    }
                    Json(json!({
                        "value": items,
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=done")
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_url_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let expected_top5 = vec!["m20", "m21", "m22", "m23", "m24"];
    let extract_ids = |val: &Value| -> Vec<String> {
        val["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    };

    // 1. CLI online (--refresh) for each single filter and two-filter combo
    for filter_args in [
        vec!["--unread"],
        vec!["--flagged"],
        vec!["--focused"],
        vec!["--unread", "--flagged"],
        vec!["--unread", "--focused"],
        vec!["--flagged", "--focused"],
    ] {
        let mut args = vec!["mail", "--folder", "inbox", "--limit", "5", "--refresh", "--json"];
        args.extend(filter_args.iter().copied());
        let out = run_outlook_cli(&dir, &base_url, &token, &args);
        assert!(
            out.status.success(),
            "CLI online {:?} failed: {}",
            filter_args,
            String::from_utf8_lossy(&out.stderr)
        );
        let val: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            extract_ids(&val),
            expected_top5,
            "CLI online {:?} must return newest 5 matching IDs",
            filter_args
        );
    }

    // Also test CLI online --compact
    let out_online_compact = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "inbox", "--unread", "--flagged", "--limit", "5", "--refresh", "--json", "--compact"],
    );
    assert!(out_online_compact.status.success());
    let val_oc: Value = serde_json::from_slice(&out_online_compact.stdout).unwrap();
    assert_eq!(extract_ids(&val_oc), expected_top5);

    // 2. Seed full folder cache via unfiltered refresh, then verify CLI offline + compact
    let out_unfiltered = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "inbox", "--limit", "5", "--refresh", "--json"],
    );
    assert!(out_unfiltered.status.success());
    let val_unf: Value = serde_json::from_slice(&out_unfiltered.stdout).unwrap();
    assert_eq!(
        extract_ids(&val_unf),
        vec!["m00", "m01", "m02", "m03", "m04"],
        "unfiltered list must preserve newest-first ordering"
    );

    for filter_args in [
        vec!["--unread"],
        vec!["--flagged"],
        vec!["--focused"],
        vec!["--unread", "--flagged"],
        vec!["--unread", "--focused"],
        vec!["--flagged", "--focused"],
    ] {
        for compact_flag in [false, true] {
            let mut args = vec!["mail", "--folder", "inbox", "--limit", "5", "--offline", "--json"];
            args.extend(filter_args.iter().copied());
            if compact_flag {
                args.push("--compact");
            }
            let out = run_outlook_cli(&dir, &base_url, &token, &args);
            assert!(out.status.success());
            let val: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(
                extract_ids(&val),
                expected_top5,
                "CLI offline (compact={compact_flag}) {:?} must return newest 5 matching IDs",
                filter_args
            );
        }
    }

    // 3. Verify partial filtered online fetch does NOT prune unrelated older cached rows
    let cfg = test_config_for_server(&base_url);
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: token.clone(),
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

    // Seed an older row in SQLite that is NOT in the first 3 pages of /messages
    let mut keep_older = storage.get_mail_message("m39").unwrap().unwrap();
    keep_older.id = "m-keep-older".into();
    keep_older.received_at = "2025-01-01T00:00:00Z".into();
    storage.upsert_mail_messages(&[keep_older]).unwrap();

    // Online filtered query via MCP (stops at page 3 once 5 matches collected)
    let (mcp_online_err, mcp_online) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_messages",
        json!({ "folder": "inbox", "unread": true, "flagged": true, "limit": 5, "offline": false }),
    )
    .await;
    assert!(!mcp_online_err, "MCP online list_messages error: {mcp_online}");
    assert_eq!(extract_ids(&mcp_online), expected_top5);
    assert!(
        storage.get_mail_message("m-keep-older").unwrap().is_some(),
        "partial filtered online scan must NOT prune older cached folder rows"
    );

    // MCP offline + compact
    let (mcp_offline_err, mcp_offline) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_messages",
        json!({ "folder": "inbox", "focused": true, "limit": 5, "offline": true, "compact": true }),
    )
    .await;
    assert!(!mcp_offline_err, "MCP offline list_messages error: {mcp_offline}");
    assert_eq!(extract_ids(&mcp_offline), expected_top5);

    // 4. HTTP serve (/v1/mail/messages) online + offline
    let api_router = serve::build_router(cfg.clone(), graph.clone(), storage.clone(), None);
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, api_router).await;
    });
    let http = reqwest::Client::new();
    for qs in [
        "folder=inbox&unread=true&limit=5&refresh=true",
        "folder=inbox&flagged=true&focused=true&limit=5&refresh=true",
        "folder=inbox&unread=true&limit=5&offline=true",
        "folder=inbox&flagged=true&limit=5&offline=true",
    ] {
        let resp: Value = http
            .get(format!("http://{api_addr}/v1/mail/messages?{qs}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            extract_ids(&resp),
            expected_top5,
            "HTTP /v1/mail/messages?{qs} must return newest 5 matching IDs"
        );
    }

    // 5. Control: sparse folder with only 3 total matches across 3 pages returns all 3 after scanning to end
    let out_sparse = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Sparse", "--unread", "--limit", "5", "--refresh", "--json"],
    );
    assert!(
        out_sparse.status.success(),
        "sparse folder query failed: {}",
        String::from_utf8_lossy(&out_sparse.stderr)
    );
    let val_sparse: Value = serde_json::from_slice(&out_sparse.stdout).unwrap();
    assert_eq!(extract_ids(&val_sparse), vec!["s20", "s21", "s22"]);

    api_server.abort();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_04_applies_computed_window_offline_and_cached() {
    use chrono::{Local, SecondsFormat, TimeZone, Utc};

    let token = make_regression_jwt();
    let today_local = Local::now().date_naive();
    let day_start_utc = Local
        .from_local_datetime(&today_local.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .with_timezone(&Utc);
    let iso = |dt: chrono::DateTime<Utc>| dt.to_rfc3339_opts(SecondsFormat::Secs, true);
    let today_str = today_local.format("%Y-%m-%d").to_string();

    let ev_past_start = iso(day_start_utc - chrono::Duration::days(3));
    let ev_past_end = iso(day_start_utc - chrono::Duration::days(3) + chrono::Duration::hours(1));
    let ev_span_start = iso(day_start_utc - chrono::Duration::hours(4));
    let ev_span_end = iso(day_start_utc + chrono::Duration::hours(2));
    let ev_inside_start = iso(day_start_utc + chrono::Duration::hours(5));
    let ev_inside_end = iso(day_start_utc + chrono::Duration::hours(6));
    let ev_future_start = iso(day_start_utc + chrono::Duration::days(5));
    let ev_future_end = iso(day_start_utc + chrono::Duration::days(5) + chrono::Duration::hours(1));

    let all_events_json = Arc::new(vec![
        json!({
            "id": "ev-past",
            "subject": "Past Meeting",
            "start": { "dateTime": ev_past_start, "timeZone": "UTC" },
            "end": { "dateTime": ev_past_end, "timeZone": "UTC" },
            "isAllDay": false,
            "responseStatus": { "response": "accepted" },
            "isCancelled": false
        }),
        json!({
            "id": "ev-spanning",
            "subject": "Spanning Window Start",
            "start": { "dateTime": ev_span_start, "timeZone": "UTC" },
            "end": { "dateTime": ev_span_end, "timeZone": "UTC" },
            "isAllDay": false,
            "responseStatus": { "response": "accepted" },
            "isCancelled": false,
            "body": { "contentType": "html", "content": "<p>Spanning agenda</p>" }
        }),
        json!({
            "id": "ev-inside",
            "subject": "Inside Today Window",
            "start": { "dateTime": ev_inside_start, "timeZone": "UTC" },
            "end": { "dateTime": ev_inside_end, "timeZone": "UTC" },
            "isAllDay": false,
            "responseStatus": { "response": "accepted" },
            "isCancelled": false
        }),
        json!({
            "id": "ev-future",
            "subject": "Far Future Meeting",
            "start": { "dateTime": ev_future_start, "timeZone": "UTC" },
            "end": { "dateTime": ev_future_end, "timeZone": "UTC" },
            "isAllDay": false,
            "responseStatus": { "response": "accepted" },
            "isCancelled": false
        }),
    ]);
    let graph_calls = Arc::new(AtomicUsize::new(0));
    let events_ref = Arc::clone(&all_events_json);
    let calls_ref = Arc::clone(&graph_calls);

    let router = Router::new().route(
        "/v1.0/me/calendarView",
        get(move |RawQuery(q): RawQuery| {
            let events = Arc::clone(&events_ref);
            let calls = Arc::clone(&calls_ref);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let q_str = q.unwrap_or_default();
                let mut start_param = None;
                let mut end_param = None;
                for part in q_str.split('&') {
                    if let Some(v) = part.strip_prefix("startDateTime=") {
                        start_param = urlencoding::decode(v).ok().map(|c| c.into_owned());
                    } else if let Some(v) = part.strip_prefix("endDateTime=") {
                        end_param = urlencoding::decode(v).ok().map(|c| c.into_owned());
                    }
                }
                let filtered: Vec<Value> = events
                    .iter()
                    .filter(|ev| {
                        let s = ev["start"]["dateTime"].as_str().unwrap_or("");
                        let e = ev["end"]["dateTime"].as_str().unwrap_or("");
                        let ok_start = start_param
                            .as_deref()
                            .map(|win_s| e >= win_s || s >= win_s)
                            .unwrap_or(true);
                        let ok_end = end_param
                            .as_deref()
                            .map(|win_e| s <= win_e)
                            .unwrap_or(true);
                        ok_start && ok_end
                    })
                    .cloned()
                    .collect();
                Json(json!({ "value": filtered }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_for_server(&base_url);
    let now = Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: token.clone(),
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

    // Seed all 4 events (past, spanning, inside, future) into CLI SQLite cache
    let db_path = dir.path().join("regression-user").join("cache.db");
    let storage = Storage::open(&db_path).unwrap();
    let seed_events: Vec<CalendarEvent> = vec![
        ("ev-past", "Past Meeting", &ev_past_start, &ev_past_end),
        ("ev-spanning", "Spanning Window Start", &ev_span_start, &ev_span_end),
        ("ev-inside", "Inside Today Window", &ev_inside_start, &ev_inside_end),
        ("ev-future", "Far Future Meeting", &ev_future_start, &ev_future_end),
    ]
    .into_iter()
    .map(|(id, subj, s, e)| CalendarEvent {
        id: id.into(),
        subject: subj.into(),
        start_iso: s.clone(),
        end_iso: e.clone(),
        time_zone: "UTC".into(),
        is_all_day: false,
        location: None,
        organizer_name: None,
        organizer_email: None,
        attendees: vec![],
        response_status: "accepted".into(),
        is_online_meeting: false,
        online_meeting_url: None,
        body_html: String::new(),
        body_plain: String::new(),
        is_cancelled: false,
        web_link: None,
        ..Default::default()
    })
    .collect();
    storage.upsert_calendar_events(&seed_events).unwrap();

    let extract_ids = |val: &Value| -> Vec<String> {
        val["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    };
    let expected = vec!["ev-spanning", "ev-inside"];

    // 1. Offline CLI: --days 1 and --from <today> --days 1 (0 Graph calls!)
    let calls_before = graph_calls.load(Ordering::SeqCst);
    let out_days_offline = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["calendar", "--days", "1", "--offline", "--json"],
    );
    assert!(
        out_days_offline.status.success(),
        "calendar --days 1 --offline failed: {}",
        String::from_utf8_lossy(&out_days_offline.stderr)
    );
    let val_days_offline: Value = serde_json::from_slice(&out_days_offline.stdout).unwrap();
    assert_eq!(
        extract_ids(&val_days_offline),
        expected,
        "calendar --days 1 --offline must only include overlapping window events"
    );

    let out_from_offline = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["calendar", "--from", &today_str, "--days", "1", "--offline", "--json"],
    );
    assert!(out_from_offline.status.success());
    let val_from_offline: Value = serde_json::from_slice(&out_from_offline.stdout).unwrap();
    assert_eq!(
        extract_ids(&val_from_offline),
        expected,
        "calendar --from today --days 1 --offline must apply computed end bound"
    );

    // 2. Cached CLI: --days 1 --cached
    let out_days_cached = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["calendar", "--days", "1", "--cached", "--json"],
    );
    assert!(out_days_cached.status.success());
    let val_days_cached: Value = serde_json::from_slice(&out_days_cached.stdout).unwrap();
    assert_eq!(
        extract_ids(&val_days_cached),
        expected,
        "calendar --days 1 --cached must apply computed window"
    );

    // 3. Control: explicit --from and --to both supplied (still includes spanning + inside)
    let to_str = iso(day_start_utc + chrono::Duration::days(1));
    let out_both = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "calendar",
            "--from",
            &iso(day_start_utc),
            "--to",
            &to_str,
            "--offline",
            "--json",
        ],
    );
    assert!(out_both.status.success());
    let val_both: Value = serde_json::from_slice(&out_both.stdout).unwrap();
    assert_eq!(extract_ids(&val_both), expected);

    // 4. MCP list_events offline (days=1 and from+days=1)
    let (mcp_err1, mcp_days) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_events",
        json!({ "days": 1, "offline": true }),
    )
    .await;
    assert!(!mcp_err1, "MCP list_events error: {mcp_days}");
    assert_eq!(extract_ids(&mcp_days), expected);

    let (mcp_err2, mcp_from_days) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_events",
        json!({ "from": today_str, "days": 1, "offline": true }),
    )
    .await;
    assert!(!mcp_err2);
    assert_eq!(extract_ids(&mcp_from_days), expected);

    // 5. HTTP /v1/calendar/events offline (days=1 and from+days=1)
    let api_router = serve::build_router(cfg.clone(), graph.clone(), storage.clone(), None);
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, api_router).await;
    });
    let http = reqwest::Client::new();
    let http_days: Value = http
        .get(format!("http://{api_addr}/v1/calendar/events?days=1&offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(extract_ids(&http_days), expected);

    let http_from: Value = http
        .get(format!(
            "http://{api_addr}/v1/calendar/events?from={today_str}&days=1&offline=true"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(extract_ids(&http_from), expected);

    // Verify 0 Graph calls occurred across all offline + cached invocations!
    assert_eq!(
        graph_calls.load(Ordering::SeqCst),
        calls_before,
        "offline/cached calendar queries must make 0 Graph calls"
    );

    // 6. Online CLI with same window matches offline/cached/MCP/HTTP IDs, compact omits body_html, --full restores it
    let out_online = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["calendar", "--days", "1", "--json"],
    );
    assert!(out_online.status.success());
    let val_online: Value = serde_json::from_slice(&out_online.stdout).unwrap();
    assert_eq!(extract_ids(&val_online), expected);
    assert_eq!(val_online["start"], val_days_offline["start"]);
    assert_eq!(val_online["end"], val_days_offline["end"]);
    assert!(
        val_online["events"][0].get("body_html").is_none(),
        "calendar --json must omit body_html by default"
    );
    assert!(
        val_online["events"][0].get("body_plain").is_some(),
        "calendar --json must retain body_plain"
    );

    let out_online_full = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["calendar", "--days", "1", "--json", "--full"],
    );
    assert!(out_online_full.status.success());
    let val_online_full: Value = serde_json::from_slice(&out_online_full.stdout).unwrap();
    assert!(
        val_online_full["events"][0].get("body_html").is_some(),
        "calendar --json --full must include body_html"
    );

    // MCP list_events compact default vs compact:false
    let (mcp_c_err, mcp_compact) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_events",
        json!({ "days": 1, "offline": true }),
    )
    .await;
    assert!(!mcp_c_err);
    assert!(mcp_compact["events"][0].get("body_html").is_none());
    let (mcp_f_err, mcp_full) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_events",
        json!({ "days": 1, "offline": true, "compact": false }),
    )
    .await;
    assert!(!mcp_f_err);
    assert!(mcp_full["events"][0].get("body_html").is_some());

    api_server.abort();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_08_remote_search_scopes_to_folder() {
    let token = make_regression_jwt();
    let recorded_requests: Arc<RwLock<Vec<(String, String)>>> = Arc::new(RwLock::new(Vec::new()));

    let make_hit = |folder: &str, id: &str, mins_ago: i64| -> Value {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-11-21T12:00:00Z").unwrap()
            - chrono::Duration::minutes(mins_ago);
        json!({
            "id": id,
            "parentFolderId": folder,
            "subject": format!("Budget 2026 report {id}"),
            "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
            "receivedDateTime": ts.to_rfc3339(),
            "isRead": false,
            "isDraft": false,
            "hasAttachments": false,
            "bodyPreview": "Budget 2026 details"
        })
    };

    let archive_hits: Arc<Vec<Value>> = Arc::new(
        (0..10)
            .map(|i| make_hit("folder-archive", &format!("arch-{i}"), i as i64))
            .collect(),
    );
    let inbox_hits: Arc<Vec<Value>> = Arc::new(
        (0..5)
            .map(|i| make_hit("folder-inbox", &format!("inbox-{i}"), (10 + i) as i64))
            .collect(),
    );

    let parse_top = |q: &str| -> usize {
        q.split('&')
            .find_map(|p| p.strip_prefix("$top="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(50)
    };

    let reqs_all = Arc::clone(&recorded_requests);
    let arch_all = Arc::clone(&archive_hits);
    let inbox_all = Arc::clone(&inbox_hits);

    let reqs_fld = Arc::clone(&recorded_requests);
    let arch_fld = Arc::clone(&archive_hits);
    let inbox_fld = Arc::clone(&inbox_hits);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [
                        { "id": "folder-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 5, "totalItemCount": 5 },
                        { "id": "folder-archive", "displayName": "Archive", "wellKnownName": "archive", "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 10 },
                        { "id": "folder-empty", "displayName": "EmptyFolder", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/me/messages",
            get(move |RawQuery(q): RawQuery| {
                let reqs = Arc::clone(&reqs_all);
                let arch = Arc::clone(&arch_all);
                let inb = Arc::clone(&inbox_all);
                async move {
                    let q_str = q.unwrap_or_default();
                    reqs.write()
                        .unwrap()
                        .push(("/v1.0/me/messages".to_string(), q_str.clone()));
                    let top = parse_top(&q_str);
                    let mut combined = Vec::new();
                    combined.extend(arch.iter().cloned());
                    combined.extend(inb.iter().cloned());
                    combined.truncate(top);
                    Json(json!({ "value": combined }))
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages",
            get(move |Path(fid): Path<String>, RawQuery(q): RawQuery| {
                let reqs = Arc::clone(&reqs_fld);
                let arch = Arc::clone(&arch_fld);
                let inb = Arc::clone(&inbox_fld);
                async move {
                    let q_str = q.unwrap_or_default();
                    reqs.write().unwrap().push((
                        format!("/v1.0/me/mailFolders/{fid}/messages"),
                        q_str.clone(),
                    ));
                    let top = parse_top(&q_str);
                    let mut items = match fid.as_str() {
                        "folder-inbox" => (*inb).clone(),
                        "folder-archive" => (*arch).clone(),
                        "folder-empty" => Vec::new(),
                        _ => Vec::new(),
                    };
                    items.truncate(top);
                    Json(json!({ "value": items }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let expected_inbox = vec!["inbox-0", "inbox-1", "inbox-2", "inbox-3", "inbox-4"];
    let expected_all = vec!["arch-0", "arch-1", "arch-2", "arch-3", "arch-4"];
    let ids_of = |arr: &Value| -> Vec<String> {
        arr.as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    };

    // 1. CLI search --remote --folder inbox --limit 5
    let out_cli = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "search",
            "Budget 2026",
            "--remote",
            "--folder",
            "inbox",
            "--limit",
            "5",
            "--json",
        ],
    );
    assert!(
        out_cli.status.success(),
        "CLI remote folder search failed: {}",
        String::from_utf8_lossy(&out_cli.stderr)
    );
    let cli_val: Value = serde_json::from_slice(&out_cli.stdout).unwrap();
    assert_eq!(
        ids_of(&cli_val["remote"]),
        expected_inbox,
        "CLI search --remote --folder inbox --limit 5 must return all 5 Inbox matches"
    );
    {
        let reqs = recorded_requests.read().unwrap();
        let last = reqs.last().expect("recorded remote search request");
        assert_eq!(last.0, "/v1.0/me/mailFolders/folder-inbox/messages");
        assert!(
            last.1.contains("$search=%22Budget%202026%22"),
            "expected encoded search query, got: {}",
            last.1
        );
    }

    // 2. MCP search_mail (online, folder=inbox, limit=5)
    let cfg = test_config_for_server(&base_url);
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: token.clone(),
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

    let (mcp_err, mcp_val) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_mail",
        json!({ "query": "Budget 2026", "folder": "inbox", "limit": 5, "offline": false }),
    )
    .await;
    assert!(!mcp_err, "MCP folder search error: {mcp_val}");
    assert_eq!(ids_of(&mcp_val["messages"]), expected_inbox);
    assert_eq!(
        recorded_requests.read().unwrap().last().unwrap().0,
        "/v1.0/me/mailFolders/folder-inbox/messages"
    );

    // 3. HTTP GET /v1/mail/search?q=Budget%202026&folder=inbox&remote=true&limit=5
    let api_router = serve::build_router(cfg.clone(), graph.clone(), storage.clone(), None);
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, api_router).await;
    });
    let http = reqwest::Client::new();
    let http_val: Value = http
        .get(format!(
            "http://{api_addr}/v1/mail/search?q=Budget%202026&folder=inbox&remote=true&limit=5"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ids_of(&http_val["remote"]), expected_inbox);
    assert_eq!(
        recorded_requests.read().unwrap().last().unwrap().0,
        "/v1.0/me/mailFolders/folder-inbox/messages"
    );

    // 4. Control: mailbox-wide search (no folder) uses /me/messages and returns top 5 overall
    let out_all = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "Budget 2026", "--remote", "--limit", "5", "--json"],
    );
    assert!(out_all.status.success());
    let all_val: Value = serde_json::from_slice(&out_all.stdout).unwrap();
    assert_eq!(ids_of(&all_val["remote"]), expected_all);
    assert_eq!(
        recorded_requests.read().unwrap().last().unwrap().0,
        "/v1.0/me/messages"
    );

    // 5. Control: valid empty folder succeeds with []
    let out_empty = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "search",
            "Budget 2026",
            "--remote",
            "--folder",
            "EmptyFolder",
            "--limit",
            "5",
            "--json",
        ],
    );
    assert!(out_empty.status.success());
    let empty_val: Value = serde_json::from_slice(&out_empty.stdout).unwrap();
    assert!(empty_val["remote"].as_array().unwrap().is_empty());

    // 6. Control: unresolved folder propagates error across CLI, MCP, and HTTP
    let out_bad = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "search",
            "Budget 2026",
            "--remote",
            "--folder",
            "NoSuchFolder",
            "--limit",
            "5",
            "--json",
        ],
    );
    assert!(!out_bad.status.success());

    let (mcp_bad_err, _) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_mail",
        json!({ "query": "Budget 2026", "folder": "NoSuchFolder", "limit": 5, "offline": false }),
    )
    .await;
    assert!(mcp_bad_err);

    let http_bad = http
        .get(format!(
            "http://{api_addr}/v1/mail/search?q=Budget%202026&folder=NoSuchFolder&remote=true&limit=5"
        ))
        .send()
        .await
        .unwrap();
    assert!(!http_bad.status().is_success());

    api_server.abort();
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_09_resolves_unicode_folder_names() {
    let token = make_regression_jwt();
    let moved_targets: Arc<RwLock<Vec<(String, String)>>> = Arc::new(RwLock::new(Vec::new()));
    let moved_ref = Arc::clone(&moved_targets);
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_delta = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [
                        { "id": "f-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 1, "totalItemCount": 1 },
                        { "id": "f-equipe", "displayName": "Équipe", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 1 },
                        { "id": "f-ubersicht", "displayName": "Übersicht", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 1 },
                        { "id": "f-proekt", "displayName": "Проект", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 1 },
                        { "id": "f-korean", "displayName": "프로젝트", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 1 },
                        { "id": "f-reports", "displayName": "Reports", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 1 },
                        { "id": "f-dup-1", "displayName": "DupÉquipe", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 },
                        { "id": "f-dup-2", "displayName": "DupÉquipe", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let base_s = Arc::clone(&base_delta);
                async move {
                    let base = base_s.read().unwrap().clone();
                    Json(json!({
                        "value": [
                            {
                                "id": format!("msg-in-{fid}"),
                                "parentFolderId": fid,
                                "subject": format!("Mail in {fid}"),
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-21T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "hello"
                            }
                        ],
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1")
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/messages/{id}/move",
            post(move |Path(id): Path<String>, Json(payload): Json<Value>| {
                let targets = Arc::clone(&moved_ref);
                async move {
                    let dest = payload["destinationId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    targets.write().unwrap().push((id.clone(), dest.clone()));
                    Json(json!({
                        "id": format!("{id}-moved"),
                        "parentFolderId": dest,
                        "subject": "Moved",
                        "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                        "receivedDateTime": "2026-11-21T10:00:00Z",
                        "isRead": false,
                        "isDraft": false,
                        "hasAttachments": false,
                        "bodyPreview": "moved"
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // 1. Acceptance: exact Unicode display names & lowercase variants resolve to exact folder IDs
    for (query, expected_id) in [
        ("Équipe", "f-equipe"),
        ("équipe", "f-equipe"),
        ("Übersicht", "f-ubersicht"),
        ("übersicht", "f-ubersicht"),
        ("Проект", "f-proekt"),
        ("проект", "f-proekt"),
        // Controls: Korean, ASCII case-insensitive, exact ID
        ("프로젝트", "f-korean"),
        ("REPORTS", "f-reports"),
        ("f-equipe", "f-equipe"),
    ] {
        let out = run_outlook_cli(
            &dir,
            &base_url,
            &token,
            &["mail", "--folder", query, "--json"],
        );
        assert!(
            out.status.success(),
            "mail --folder '{query}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let val: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            val["folder"].as_str(),
            Some(expected_id),
            "mail --folder '{query}' should resolve to '{expected_id}'"
        );
    }

    // 2. Acceptance: move to Unicode folder names
    for (query, expected_id) in [
        ("Équipe", "f-equipe"),
        ("Übersicht", "f-ubersicht"),
        ("Проект", "f-proekt"),
    ] {
        let out = run_outlook_cli(
            &dir,
            &base_url,
            &token,
            &["move", "m-mv", query, "--json"],
        );
        assert!(
            out.status.success(),
            "move to '{query}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let last = moved_targets.read().unwrap().last().cloned().unwrap();
        assert_eq!(last.1, expected_id);
    }

    // 3. Control: ambiguous duplicate Unicode folder names fail with ambiguity error
    for dup_q in ["DupÉquipe", "dupéquipe"] {
        let out = run_outlook_cli(
            &dir,
            &base_url,
            &token,
            &["mail", "--folder", dup_q, "--json"],
        );
        assert!(!out.status.success(), "duplicate folder '{dup_q}' must fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Ambiguous"),
            "expected Ambiguous error for '{dup_q}', got: {stderr}"
        );
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_10_recovers_recreated_folder_id_on_404() {
    use std::sync::atomic::AtomicBool;

    let token = make_regression_jwt();
    let recreated = Arc::new(AtomicBool::new(false));
    let req_log: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));

    let rec_folders = Arc::clone(&recreated);
    let log_folders = Arc::clone(&req_log);
    let log_delta = Arc::clone(&req_log);
    let base_delta = Arc::clone(&base_slot);
    let log_list = Arc::clone(&req_log);
    let log_move = Arc::clone(&req_log);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(move || {
                let rec = rec_folders.load(Ordering::SeqCst);
                let log = Arc::clone(&log_folders);
                async move {
                    log.write().unwrap().push("GET /v1.0/me/mailFolders".into());
                    let mut folders = vec![
                        json!({ "id": "f-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 }),
                        json!({ "id": "f-denied-old", "displayName": "Denied", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 }),
                    ];
                    if rec {
                        folders.push(json!({ "id": "f-sprint-new", "displayName": "Sprint", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 1, "totalItemCount": 1 }));
                    } else {
                        folders.push(json!({ "id": "f-sprint-old", "displayName": "Sprint", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 1, "totalItemCount": 1 }));
                        folders.push(json!({ "id": "f-ghost-old", "displayName": "Ghost", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 0 }));
                    }
                    Json(json!({ "value": folders }))
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let log = Arc::clone(&log_delta);
                let base_s = Arc::clone(&base_delta);
                async move {
                    log.write()
                        .unwrap()
                        .push(format!("GET /v1.0/me/mailFolders/{fid}/messages/delta"));
                    let base = base_s.read().unwrap().clone();
                    match fid.as_str() {
                        "f-sprint-old" | "f-ghost-old" => (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "error": {
                                    "code": "ErrorItemNotFound",
                                    "message": "The specified object was not found in the store."
                                }
                            })),
                        )
                            .into_response(),
                        "f-denied-old" => (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "error": {
                                    "code": "ErrorAccessDenied",
                                    "message": "Access is denied."
                                }
                            })),
                        )
                            .into_response(),
                        "f-sprint-new" => Json(json!({
                            "value": [{
                                "id": "msg-sprint-new-1",
                                "parentFolderId": "f-sprint-new",
                                "subject": "Recreated Sprint Mail",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-21T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "Fresh sprint item"
                            }],
                            "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/f-sprint-new/messages/delta?$deltatoken=d1")
                        }))
                        .into_response(),
                        _ => Json(json!({ "value": [], "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1") })).into_response(),
                    }
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages",
            get(move |Path(fid): Path<String>| {
                let log = Arc::clone(&log_list);
                async move {
                    log.write()
                        .unwrap()
                        .push(format!("GET /v1.0/me/mailFolders/{fid}/messages"));
                    match fid.as_str() {
                        "f-sprint-old" => (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "error": { "code": "ErrorItemNotFound", "message": "not found" }
                            })),
                        )
                            .into_response(),
                        "f-sprint-new" => Json(json!({
                            "value": [{
                                "id": "msg-sprint-new-1",
                                "parentFolderId": "f-sprint-new",
                                "subject": "Recreated Sprint Mail",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-21T10:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "Fresh sprint item"
                            }]
                        }))
                        .into_response(),
                        _ => Json(json!({ "value": [] })).into_response(),
                    }
                }
            }),
        )
        .route(
            "/v1.0/me/messages/{id}/move",
            post(move |Path(id): Path<String>, Json(payload): Json<Value>| {
                let log = Arc::clone(&log_move);
                async move {
                    let dest = payload["destinationId"].as_str().unwrap_or("");
                    log.write()
                        .unwrap()
                        .push(format!("POST /v1.0/me/messages/{id}/move->{dest}"));
                    if dest == "f-sprint-new" {
                        Json(json!({
                            "id": format!("{id}-moved"),
                            "parentFolderId": "f-sprint-new",
                            "subject": "Moved Sprint Mail",
                            "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                            "receivedDateTime": "2026-11-21T10:00:00Z",
                            "isRead": false,
                            "isDraft": false,
                            "hasAttachments": false,
                            "bodyPreview": "Moved item"
                        }))
                        .into_response()
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "error": { "code": "ErrorItemNotFound", "message": "folder gone" }
                            })),
                        )
                            .into_response()
                    }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // Seed initial folder cache (Sprint -> f-sprint-old, Ghost -> f-ghost-old, Denied -> f-denied-old)
    let seed = run_outlook_cli(&dir, &base_url, &token, &["folders", "--refresh", "--json"]);
    assert!(seed.status.success());

    // Now Sprint is deleted & recreated with ID f-sprint-new; Ghost is deleted
    recreated.store(true, Ordering::SeqCst);

    // 1. Acceptance: `mail --folder Sprint` recovers via old 404 -> refresh folders -> new 200
    req_log.write().unwrap().clear();
    let out1 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Sprint", "--json"],
    );
    assert!(
        out1.status.success(),
        "mail --folder Sprint must recover on 404: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let val1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(val1["folder"], "f-sprint-new");
    assert_eq!(val1["messages"][0]["id"], "msg-sprint-new-1");
    assert_eq!(
        *req_log.read().unwrap(),
        vec![
            "GET /v1.0/me/mailFolders/f-sprint-old/messages/delta",
            "GET /v1.0/me/mailFolders",
            "GET /v1.0/me/mailFolders/f-sprint-new/messages/delta",
        ]
    );

    // 2. Acceptance: re-seed stale f-sprint-old, verify `--refresh` and filtered `--unread --refresh` both recover
    let db_path = dir.path().join("regression-user").join("cache.db");
    let storage = Storage::open(&db_path).unwrap();
    let reset_stale_folders = || {
        recreated.store(false, Ordering::SeqCst);
        let s = run_outlook_cli(&dir, &base_url, &token, &["folders", "--refresh", "--json"]);
        assert!(s.status.success());
        recreated.store(true, Ordering::SeqCst);
    };

    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Sprint", "--refresh", "--json"],
    );
    assert!(
        out2.status.success(),
        "mail --folder Sprint --refresh must recover: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let val2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(val2["folder"], "f-sprint-new");
    assert_eq!(val2["messages"][0]["id"], "msg-sprint-new-1");
    assert_eq!(
        *req_log.read().unwrap(),
        vec![
            "GET /v1.0/me/mailFolders/f-sprint-old/messages/delta",
            "GET /v1.0/me/mailFolders",
            "GET /v1.0/me/mailFolders/f-sprint-new/messages/delta",
        ]
    );

    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out_flt = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Sprint", "--unread", "--refresh", "--json"],
    );
    assert!(
        out_flt.status.success(),
        "mail --folder Sprint --unread --refresh must recover: {}",
        String::from_utf8_lossy(&out_flt.stderr)
    );
    let val_flt: Value = serde_json::from_slice(&out_flt.stdout).unwrap();
    assert_eq!(val_flt["folder"], "f-sprint-new");
    assert_eq!(val_flt["messages"][0]["id"], "msg-sprint-new-1");
    assert_eq!(
        *req_log.read().unwrap(),
        vec![
            "GET /v1.0/me/mailFolders/f-sprint-old/messages",
            "GET /v1.0/me/mailFolders",
            "GET /v1.0/me/mailFolders/f-sprint-new/messages",
        ]
    );

    // 3. Acceptance: deleted folder name still fails with error after re-resolution
    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out_ghost = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Ghost", "--refresh", "--json"],
    );
    assert!(!out_ghost.status.success());
    assert_eq!(
        *req_log.read().unwrap(),
        vec![
            "GET /v1.0/me/mailFolders/f-ghost-old/messages/delta",
            "GET /v1.0/me/mailFolders",
        ]
    );

    // 4. Control: 403 Forbidden does NOT trigger folder refresh retry
    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out_403 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Denied", "--refresh", "--json"],
    );
    assert!(!out_403.status.success());
    assert_eq!(
        *req_log.read().unwrap(),
        vec!["GET /v1.0/me/mailFolders/f-denied-old/messages/delta"]
    );

    // 5. Control: explicit folder ID must NOT substitute another folder on 404
    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out_explicit = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "f-sprint-old", "--refresh", "--json"],
    );
    assert!(!out_explicit.status.success());
    assert_eq!(
        *req_log.read().unwrap(),
        vec!["GET /v1.0/me/mailFolders/f-sprint-old/messages/delta"]
    );

    // 6. Control: offline path makes 0 Graph calls
    req_log.write().unwrap().clear();
    let out_off = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "Sprint", "--offline", "--json"],
    );
    assert!(out_off.status.success());
    assert!(req_log.read().unwrap().is_empty());

    // 7. W5 #5-c: mail move recovers recreated folder ID on 404 when destination is given by name
    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out_mv = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["move", "m-any", "Sprint", "--json"],
    );
    assert!(
        out_mv.status.success(),
        "move m-any Sprint must recover recreated folder on 404: {}",
        String::from_utf8_lossy(&out_mv.stderr)
    );
    let mv_val: Value = serde_json::from_slice(&out_mv.stdout).unwrap();
    assert_eq!(mv_val["destination_folder"]["id"], "f-sprint-new");
    assert_eq!(mv_val["message_id"], "m-any-moved");
    assert_eq!(
        *req_log.read().unwrap(),
        vec![
            "POST /v1.0/me/messages/m-any/move->f-sprint-old",
            "GET /v1.0/me/mailFolders",
            "POST /v1.0/me/messages/m-any/move->f-sprint-new",
        ]
    );

    // 7b. Control: move with explicit folder ID does NOT refresh mailFolders or retry on 404
    reset_stale_folders();
    req_log.write().unwrap().clear();
    let out_mv_id = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["move", "m-any", "f-sprint-old", "--json"],
    );
    assert!(!out_mv_id.status.success());
    assert_eq!(
        *req_log.read().unwrap(),
        vec!["POST /v1.0/me/messages/m-any/move->f-sprint-old"]
    );

    drop(storage);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_11_separates_html_table_cells_and_reindexes() {
    let token = make_regression_jwt();
    let compact_html = "<table><tr><th>Item</th><th>Qty</th><th>Price</th></tr><tr><td>Widget</td><td>42</td><td>$99.00</td></tr></table>";
    let spaced_html = "<table><tr><th>Item</th> <th>Qty</th> <th>Price</th></tr><tr><td>Gadget</td> <td>77</td> <td>$50.00</td></tr></table>";
    let newline_html = "<table><tr><th>Item</th>\n<th>Qty</th></tr><tr><td>Sprocket</td>\n<td>88</td></tr></table>";

    let c_html = compact_html.to_string();
    let s_html = spaced_html.to_string();
    let n_html = newline_html.to_string();

    let router = Router::new().route(
        "/v1.0/me/messages/{id}",
        get(move |Path(id): Path<String>| {
            let c = c_html.clone();
            let s = s_html.clone();
            let n = n_html.clone();
            async move {
                let html = match id.as_str() {
                    "m-tbl-compact" | "m-tbl-stale" => c,
                    "m-tbl-spaced" => s,
                    "m-tbl-newline" => n,
                    _ => "<p>empty</p>".to_string(),
                };
                Json(json!({
                    "id": id,
                    "parentFolderId": "folder-inbox",
                    "subject": format!("Table {id}"),
                    "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                    "receivedDateTime": "2026-11-21T10:00:00Z",
                    "isRead": false,
                    "isDraft": false,
                    "hasAttachments": false,
                    "bodyPreview": "Table preview",
                    "body": { "contentType": "html", "content": html }
                }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // 1. Acceptance: compact no-space HTML table separates cells and indexes 'Widget 42'
    let out_compact = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-tbl-compact", "--json", "--full"],
    );
    assert!(
        out_compact.status.success(),
        "read m-tbl-compact failed: {}",
        String::from_utf8_lossy(&out_compact.stderr)
    );
    let compact_val: Value = serde_json::from_slice(&out_compact.stdout).unwrap();
    assert_eq!(
        compact_val["message"]["body_html"].as_str(),
        Some(compact_html),
        "raw HTML body must NOT be modified"
    );
    let compact_plain = compact_val["message"]["body_plain"].as_str().unwrap();
    assert_eq!(
        compact_plain, "Item Qty Price\nWidget 42 $99.00",
        "compact HTML table cells must be separated by a single space with row newline preserved"
    );

    let search_compact = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "Widget 42", "--json"],
    );
    assert!(search_compact.status.success());
    let hits_compact: Value = serde_json::from_slice(&search_compact.stdout).unwrap();
    let hit_ids: Vec<&str> = hits_compact["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id"].as_str())
        .collect();
    assert!(
        hit_ids.contains(&"m-tbl-compact"),
        "newly indexed table mail must match search 'Widget 42', got {hit_ids:?}"
    );

    // 2. Control 1: cells already separated by space or newline do not duplicate or concatenate words
    let out_spaced = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-tbl-spaced", "--json"],
    );
    assert!(out_spaced.status.success());
    let spaced_val: Value = serde_json::from_slice(&out_spaced.stdout).unwrap();
    assert_eq!(
        spaced_val["message"]["body_plain"].as_str().unwrap(),
        "Item Qty Price\nGadget 77 $50.00"
    );
    let search_spaced = run_outlook_cli(&dir, &base_url, &token, &["search", "Gadget 77", "--json"]);
    assert!(search_spaced.status.success());
    let hits_spaced: Value = serde_json::from_slice(&search_spaced.stdout).unwrap();
    assert!(hits_spaced["hits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h["id"] == "m-tbl-spaced"));

    let out_nl = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-tbl-newline", "--json"],
    );
    assert!(out_nl.status.success());
    let nl_val: Value = serde_json::from_slice(&out_nl.stdout).unwrap();
    let nl_plain = nl_val["message"]["body_plain"].as_str().unwrap();
    assert!(!nl_plain.contains("Sprocket88"));
    assert!(nl_plain.contains("Sprocket") && nl_plain.contains("88"));

    // 3. Control 2: re-reading an existing cached message online refreshes its stale plain text & FTS index
    let db_path = dir.path().join("regression-user").join("cache.db");
    let storage = Storage::open(&db_path).unwrap();
    let mut stale_msg = storage.get_mail_message("m-tbl-compact").unwrap().unwrap();
    stale_msg.id = "m-tbl-stale".into();
    stale_msg.subject = "Table m-tbl-stale".into();
    stale_msg.body_plain = "ItemQtyPrice\nWidget42$99.00".into();
    storage.upsert_mail_messages(&[stale_msg]).unwrap();
    assert!(
        !storage
            .search_mail("Widget 42", None, 20)
            .unwrap()
            .iter()
            .any(|h| h.id == "m-tbl-stale"),
        "pre-upgrade stale cache row should not yet match 'Widget 42'"
    );

    let out_reread = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "m-tbl-stale", "--json"],
    );
    assert!(out_reread.status.success());
    let refreshed_cached = storage.get_mail_message("m-tbl-stale").unwrap().unwrap();
    assert_eq!(refreshed_cached.body_plain, "Item Qty Price\nWidget 42 $99.00");
    assert!(
        storage
            .search_mail("Widget 42", None, 20)
            .unwrap()
            .iter()
            .any(|h| h.id == "m-tbl-stale"),
        "online re-read must refresh both cached body_plain and FTS index"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_12_mcp_offline_attachment_download() {
    let token = make_regression_jwt();
    let total_graph_calls = Arc::new(AtomicUsize::new(0));
    let value_calls = Arc::new(AtomicUsize::new(0));
    let remote_bytes = b"%PDF-1.7 REMOTE SPEC BYTES".to_vec();
    let cached_bytes = b"LOCAL CACHED PAYLOAD 0123456789".to_vec();

    let tot_get = Arc::clone(&total_graph_calls);
    let tot_val = Arc::clone(&total_graph_calls);
    let val_cnt = Arc::clone(&value_calls);
    let rem_ref = remote_bytes.clone();

    let router = Router::new()
        .route(
            "/v1.0/me/messages/m-dl",
            get(move || {
                let tot = Arc::clone(&tot_get);
                async move {
                    tot.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "id": "m-dl",
                        "parentFolderId": "folder-inbox",
                        "subject": "Download test",
                        "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                        "receivedDateTime": "2026-11-21T10:00:00Z",
                        "isRead": true,
                        "isDraft": false,
                        "hasAttachments": true,
                        "bodyPreview": "see spec",
                        "body": { "contentType": "html", "content": "<p>see spec</p>" },
                        "attachments": [
                            {
                                "id": "att-r1",
                                "name": "spec.pdf",
                                "contentType": "application/pdf",
                                "size": 26,
                                "isInline": false
                            }
                        ]
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/messages/m-dl/attachments/att-r1/$value",
            get(move || {
                let tot = Arc::clone(&tot_val);
                let vc = Arc::clone(&val_cnt);
                let bytes = rem_ref.clone();
                async move {
                    tot.fetch_add(1, Ordering::SeqCst);
                    vc.fetch_add(1, Ordering::SeqCst);
                    ([(header::CONTENT_TYPE, "application/pdf")], bytes)
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = test_config_for_server(&base_url);
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: token,
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
    let storage = Storage::open_in_memory().unwrap();

    // Seed m-dl in cache: att-r1 metadata-only (no bytes), att-cached with cached base64 bytes
    storage
        .upsert_mail_messages(&[MailMessage {
            id: "m-dl".into(),
            internet_message_id: None,
            conversation_id: None,
            folder_id: Some("folder-inbox".into()),
            subject: "Download test".into(),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-21T10:00:00Z".into(),
            sent_at: None,
            last_modified_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: None,
            has_attachments: true,
            body_loaded: true,
            preview: "see spec".into(),
            body_html: "<p>see spec</p>".into(),
            body_plain: "see spec".into(),
            attachments: vec![
                outlook_cli::models::MailAttachment {
                    id: "att-r1".into(),
                    name: "spec.pdf".into(),
                    content_type: "application/pdf".into(),
                    size: remote_bytes.len() as u64,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: None,
                },
                outlook_cli::models::MailAttachment {
                    id: "att-cached".into(),
                    name: "cached.bin".into(),
                    content_type: "application/octet-stream".into(),
                    size: cached_bytes.len() as u64,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: Some(B64_STANDARD.encode(&cached_bytes)),
                },
            ],
            meeting_request: None,
            web_link: None,
            ..Default::default()
        }])
        .unwrap();

    let tmp_out = tempfile::tempdir().unwrap();
    let out_empty = tmp_out.path().join("empty");
    std::fs::create_dir_all(&out_empty).unwrap();

    // 1. Acceptance: metadata-only attachment in offline=true -> isError=true, 0 Graph calls, 0 new files
    let (is_err1, val1) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "download_attachment",
        json!({
            "message_id": "m-dl",
            "attachment": "spec.pdf",
            "output_dir": out_empty.to_string_lossy(),
            "offline": true
        }),
    )
    .await;
    assert!(
        is_err1,
        "offline download of metadata-only attachment must return isError=true, got: {val1}"
    );
    assert!(
        val1.to_string()
            .contains("Cannot fetch uncached attachment bytes in offline mode"),
        "expected existing CLI offline error message, got: {val1}"
    );
    assert_eq!(
        total_graph_calls.load(Ordering::SeqCst),
        0,
        "offline download miss must make 0 Graph calls"
    );
    assert_eq!(
        std::fs::read_dir(&out_empty).unwrap().count(),
        0,
        "offline download miss must create 0 files"
    );

    // 2. Acceptance: existing file with same name is NOT modified and no duplicate file is created
    let out_existing = tmp_out.path().join("existing");
    std::fs::create_dir_all(&out_existing).unwrap();
    let existing_file = out_existing.join("spec.pdf");
    std::fs::write(&existing_file, b"ORIGINAL-CONTENT").unwrap();
    let (is_err2, _) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "download_attachment",
        json!({
            "message_id": "m-dl",
            "attachment": "spec.pdf",
            "output_dir": out_existing.to_string_lossy(),
            "offline": true
        }),
    )
    .await;
    assert!(is_err2);
    assert_eq!(total_graph_calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&existing_file).unwrap(), b"ORIGINAL-CONTENT");
    assert_eq!(std::fs::read_dir(&out_existing).unwrap().count(), 1);

    // 3. Acceptance: pre-cached bytes download offline cleanly with matching SHA-256 hash and 0 Graph calls
    let out_cached = tmp_out.path().join("cached");
    let (is_err3, val3) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "download_attachment",
        json!({
            "message_id": "m-dl",
            "attachment": "cached.bin",
            "output_dir": out_cached.to_string_lossy(),
            "offline": true
        }),
    )
    .await;
    assert!(!is_err3, "cached bytes offline download failed: {val3}");
    assert_eq!(total_graph_calls.load(Ordering::SeqCst), 0);
    let saved_cached = std::fs::read(val3["path"].as_str().unwrap()).unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(&saved_cached)),
        hex::encode(Sha256::digest(&cached_bytes))
    );

    // 4. Control: online miss (offline=false) downloads via /$value path
    let out_online = tmp_out.path().join("online");
    let (is_err4, val4) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "download_attachment",
        json!({
            "message_id": "m-dl",
            "attachment": "spec.pdf",
            "output_dir": out_online.to_string_lossy(),
            "offline": false
        }),
    )
    .await;
    assert!(!is_err4, "online attachment download failed: {val4}");
    assert_eq!(value_calls.load(Ordering::SeqCst), 1);
    let saved_online = std::fs::read(val4["path"].as_str().unwrap()).unwrap();
    assert_eq!(saved_online, remote_bytes);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_outlook_outlook_13_prevents_duplicate_inline_images() {
    let token = make_regression_jwt();
    let sent_payloads: Arc<RwLock<Vec<Value>>> = Arc::new(RwLock::new(Vec::new()));
    let payloads_ref = Arc::clone(&sent_payloads);

    let router = Router::new().route(
        "/v1.0/me/sendMail",
        post(move |Json(payload): Json<Value>| {
            let log = Arc::clone(&payloads_ref);
            async move {
                log.write().unwrap().push(payload);
                StatusCode::ACCEPTED
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let png_bytes = make_test_png_bytes(4, 4);
    let img_path = dir.path().join("diagram.png");
    std::fs::write(&img_path, &png_bytes).unwrap();
    let img_str = img_path.to_string_lossy().to_string();

    // 1. Acceptance: 1 inline image with (a) cid:1, (b) cid:inline-img-1, (c) no body ref -> each sends 1 <img> & 1 attachment
    for (label, body_md) in [
        ("cid:1", "See ![diagram](cid:1)"),
        ("cid:inline-img-1", "See ![diagram](cid:inline-img-1)"),
        ("unreferenced", "See attached diagram below."),
    ] {
        sent_payloads.write().unwrap().clear();
        let out = run_outlook_cli(
            &dir,
            &base_url,
            &token,
            &[
                "send",
                "--to",
                "bob@contoso.com",
                "--subject",
                label,
                "--body",
                body_md,
                "--inline",
                &img_str,
                "--json",
            ],
        );
        assert!(
            out.status.success(),
            "send ({label}) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let payloads = sent_payloads.read().unwrap();
        assert_eq!(payloads.len(), 1);
        let html = payloads[0]["message"]["body"]["content"]
            .as_str()
            .unwrap();
        let atts = payloads[0]["message"]["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1, "{label}: expected 1 attachment");
        assert_eq!(
            html.matches("<img ").count(),
            1,
            "{label}: expected exactly 1 <img> in sent HTML, got: {html}"
        );
        assert!(
            html.contains("src=\"cid:inline-img-1\""),
            "{label}: expected normalized cid:inline-img-1, got: {html}"
        );
    }

    // 2. Acceptance: cid:1 and cid:10 with distinct content_ids must each resolve to their exact target (cid:1 must NOT rewrite prefix of cid:10)
    let cfg = test_config_for_server(&base_url);
    let now = chrono::Utc::now().timestamp();
    let tok = StoredTokens {
        access_token: token.clone(),
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

    let two_inlines = vec![
        InlineImageUpload {
            temp_id: "1".into(),
            content_id: "alpha-one@contoso".into(),
            file_name: "one.png".into(),
            content_type: "image/png".into(),
            bytes: png_bytes.clone(),
        },
        InlineImageUpload {
            temp_id: "10".into(),
            content_id: "beta-ten@contoso".into(),
            file_name: "ten.png".into(),
            content_type: "image/png".into(),
            bytes: png_bytes.clone(),
        },
    ];
    let compiled_two =
        parser::compile_outgoing_mail_html("First ![one](cid:1) and tenth ![ten](cid:10)", &two_inlines, None);
    sent_payloads.write().unwrap().clear();
    graph
        .send_mail(&OutgoingMail {
            subject: "cid-1-vs-10".into(),
            body_html: compiled_two,
            to: vec!["bob@contoso.com".into()],
            inline_images: two_inlines.clone(),
            save_to_sent_items: true,
            ..Default::default()
        })
        .await
        .unwrap();
    {
        let payloads = sent_payloads.read().unwrap();
        let html = payloads[0]["message"]["body"]["content"].as_str().unwrap();
        let atts = payloads[0]["message"]["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(
            html.matches("<img ").count(),
            2,
            "cid:1 and cid:10 must produce exactly 2 <img> tags, got: {html}"
        );
        assert!(
            html.contains("src=\"cid:alpha-one@contoso\""),
            "cid:1 must map to alpha-one@contoso, got: {html}"
        );
        assert!(
            html.contains("src=\"cid:beta-ten@contoso\""),
            "cid:10 must map to beta-ten@contoso without prefix corruption, got: {html}"
        );
        assert!(
            !html.contains("alpha-one@contoso0"),
            "cid:1 must not corrupt cid:10 prefix: {html}"
        );
    }

    // Also test raw HTML rewrite path (e.g. TUI/API passing raw HTML with cid:1 and cid:10 directly)
    sent_payloads.write().unwrap().clear();
    graph
        .send_mail(&OutgoingMail {
            subject: "raw-cid-1-vs-10".into(),
            body_html: "<p><img src=\"cid:1\"><img src=\"cid:10\"></p>".into(),
            to: vec!["bob@contoso.com".into()],
            inline_images: two_inlines,
            save_to_sent_items: true,
            ..Default::default()
        })
        .await
        .unwrap();
    {
        let payloads = sent_payloads.read().unwrap();
        let html = payloads[0]["message"]["body"]["content"].as_str().unwrap();
        assert_eq!(
            html,
            "<p><img src=\"cid:alpha-one@contoso\"><img src=\"cid:beta-ten@contoso\"></p>",
            "raw HTML cid:1 must not corrupt cid:10"
        );
    }

    // 3. Control: intentional double reference in body keeps BOTH references and does NOT append a 3rd
    sent_payloads.write().unwrap().clear();
    let out_dup = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "send",
            "--to",
            "bob@contoso.com",
            "--subject",
            "intentional-double",
            "--body",
            "Top ![a](cid:1) and bottom ![b](cid:1)",
            "--inline",
            &img_str,
            "--json",
        ],
    );
    assert!(out_dup.status.success());
    {
        let payloads = sent_payloads.read().unwrap();
        let html = payloads[0]["message"]["body"]["content"].as_str().unwrap();
        let atts = payloads[0]["message"]["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(
            html.matches("<img ").count(),
            2,
            "intentional double reference must keep 2 <img> tags without auto-appending a 3rd: {html}"
        );
        assert_eq!(html.matches("src=\"cid:inline-img-1\"").count(), 2);
    }

    server.abort();
}

// ===========================================================================
// Fable implementation plan acceptance & control tests (OUTLOOK-01..LIM)
// ===========================================================================

fn plan_cursor_scope(parts: &[&str]) -> String {
    let mut h: u32 = 0x811c9dc5;
    for b in parts.join("\u{1f}").bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

fn plan_mcp_runtime(dir: &TempDir, base_url: &str, token: &str) -> (AppConfig, GraphClient, Storage) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_01_detail_json_never_carries_attachment_bytes() {
    let token = make_regression_jwt();
    let png_bytes = make_test_png_bytes(16, 16);
    let png_len = png_bytes.len() as u64;
    let png_b64 = B64_STANDARD.encode(&png_bytes);
    let png_ref = png_b64.clone();

    let router = Router::new().route(
        "/v1.0/me/messages/{id}",
        get(move |Path(id): Path<String>| {
            let png = png_ref.clone();
            async move {
                Json(json!({
                    "id": id,
                    "parentFolderId": "folder-inbox",
                    "subject": "PNG attachment message",
                    "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                    "receivedDateTime": "2026-11-20T09:00:00Z",
                    "isRead": false,
                    "isDraft": false,
                    "hasAttachments": true,
                    "bodyPreview": "See chart",
                    "body": { "contentType": "html", "content": "<p>See chart</p>" },
                    "attachments": [
                        {
                            "id": "att-png-1",
                            "name": "q4-chart.png",
                            "contentType": "image/png",
                            "size": png_len,
                            "isInline": true,
                            "contentId": "chart01@contoso",
                            "contentBytes": png
                        }
                    ]
                }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let out = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "msg-png-1", "--json", "--full"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout_str = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout_str.contains("content_bytes_b64"));
    let val: Value = serde_json::from_str(&stdout_str).unwrap();
    let att = &val["message"]["attachments"][0];
    assert_eq!(att["content_cached"], true);
    assert_eq!(att["name"], "q4-chart.png");
    assert_eq!(att["size"], png_len);

    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (is_err, mcp_val) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "get_message",
        json!({ "message_id": "msg-png-1", "compact": false }),
    )
    .await;
    assert!(!is_err, "{mcp_val}");
    let mcp_str = serde_json::to_string(&mcp_val).unwrap();
    assert!(!mcp_str.contains("content_bytes_b64"));
    assert_eq!(mcp_val["message"]["attachments"][0]["content_cached"], true);
    assert_eq!(mcp_val["message"]["attachments"][0]["name"], "q4-chart.png");
    assert_eq!(mcp_val["message"]["attachments"][0]["size"], png_len);

    let cached = storage.get_mail_message("msg-png-1").unwrap().unwrap();
    assert!(cached.attachments[0].content_bytes_b64.is_some());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_02_detail_compact_default_and_omitted() {
    use std::collections::BTreeSet;

    let token = make_regression_jwt();
    let png_b64 = B64_STANDARD.encode(make_test_png_bytes(8, 8));
    let png_ref = png_b64.clone();

    let router = Router::new().route(
        "/v1.0/me/messages/{id}",
        get(move |Path(id): Path<String>| {
            let png = png_ref.clone();
            async move {
                Json(json!({
                    "@odata.type": "#microsoft.graph.eventMessageRequest",
                    "id": id,
                    "internetMessageId": "<rich-1@contoso.com>",
                    "conversationId": "conv-rich-1",
                    "parentFolderId": "folder-inbox",
                    "subject": "Q4 Architecture Review",
                    "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                    "toRecipients": [{ "emailAddress": { "name": "Alice", "address": "alice@contoso.com" } }],
                    "receivedDateTime": "2026-11-20T09:00:00Z",
                    "sentDateTime": "2026-11-20T08:59:55Z",
                    "lastModifiedDateTime": "2026-11-20T09:01:00Z",
                    "isRead": false,
                    "isDraft": false,
                    "flag": { "flagStatus": "flagged" },
                    "importance": "high",
                    "inferenceClassification": "focused",
                    "hasAttachments": true,
                    "meetingMessageType": "meetingRequest",
                    "event": { "id": "evt-001" },
                    "bodyPreview": "Hi Alice, Q4 notes.",
                    "body": { "contentType": "html", "content": "<p>Hi Alice, Q4 notes.</p>" },
                    "webLink": "https://outlook.office.com/mail/rich-1",
                    "attachments": [
                        {
                            "id": "att-1",
                            "name": "chart.png",
                            "contentType": "image/png",
                            "size": 128,
                            "isInline": false,
                            "contentBytes": png
                        }
                    ]
                }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let expected_keys: BTreeSet<&str> = [
        "id",
        "conversation_id",
        "folder_id",
        "subject",
        "from",
        "to_recipients",
        "cc_recipients",
        "bcc_recipients",
        "reply_to",
        "received_at",
        "sent_at",
        "is_read",
        "is_draft",
        "flagged",
        "importance",
        "has_attachments",
        "meeting_request",
        "body_loaded",
        "preview",
        "body_plain",
        "omitted",
        "hint",
    ]
    .into_iter()
    .collect();

    let out_default = run_outlook_cli(&dir, &base_url, &token, &["read", "msg-rich-1", "--json"]);
    assert!(out_default.status.success(), "{}", String::from_utf8_lossy(&out_default.stderr));
    let val_default: Value = serde_json::from_slice(&out_default.stdout).unwrap();

    let out_alias = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "msg-rich-1", "--json", "--compact"],
    );
    assert!(out_alias.status.success(), "{}", String::from_utf8_lossy(&out_alias.stderr));
    let val_alias: Value = serde_json::from_slice(&out_alias.stdout).unwrap();

    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (is_err, val_mcp) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "get_message",
        json!({ "message_id": "msg-rich-1" }),
    )
    .await;
    assert!(!is_err, "{val_mcp}");

    for (label, val) in [
        ("cli default", &val_default),
        ("cli --compact", &val_alias),
        ("mcp default", &val_mcp),
    ] {
        let obj = val["message"].as_object().unwrap();
        let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(keys, expected_keys, "keys mismatch for {label}");
        assert!(obj.get("body_html").is_none());
        assert_eq!(obj["omitted"]["attachments"], 1);
        assert_eq!(
            obj["omitted"]["fields"],
            json!([
                "body_html",
                "internet_message_id",
                "inference_classification",
                "last_modified_at",
                "web_link"
            ])
        );
        assert_eq!(obj["flagged"], true);
        assert_eq!(obj["importance"], "high");
        assert_eq!(obj["has_attachments"], true);
        assert_eq!(obj["conversation_id"], "conv-rich-1");
        assert_eq!(obj["meeting_request"]["meeting_message_type"], "meetingRequest");
    }

    assert!(val_default["message"]["hint"].as_str().unwrap().ends_with("--full"));
    assert!(val_alias["message"]["hint"].as_str().unwrap().ends_with("--full"));
    assert!(val_mcp["message"]["hint"].as_str().unwrap().contains("compact:false"));

    server.abort();
}

#[test]
fn plan_outlook_02_compact_full_conflict_is_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = make_regression_jwt();
    for args in [
        &["read", "x", "--json", "--compact", "--full"][..],
        &["mail", "--json", "--compact", "--full"][..],
        &["read", "x", "--full"][..],
        &["event", "x", "--json", "--compact", "--full"][..],
        &["event", "x", "--full"][..],
    ] {
        let out = run_outlook_cli(&dir, "http://127.0.0.1:1", &token, args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit 2 for {args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "expected empty stdout for {args:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_03_unloaded_message_distinguishes_unknown() {
    let token = make_regression_jwt();
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_for_delta = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "folder-inbox",
                        "displayName": "Inbox",
                        "wellKnownName": "inbox",
                        "childFolderCount": 0,
                        "unreadItemCount": 2,
                        "totalItemCount": 2
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let base_s = Arc::clone(&base_for_delta);
                async move {
                    let base = base_s.read().unwrap().clone();
                    Json(json!({
                        "value": [
                            {
                                "id": "msg-with-att",
                                "parentFolderId": fid,
                                "subject": "Has Attachment",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T09:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": true,
                                "bodyPreview": "Preview with attachment"
                            },
                            {
                                "id": "msg-no-att",
                                "parentFolderId": fid,
                                "subject": "No Attachment",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T08:00:00Z",
                                "isRead": true,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "Preview plain only"
                            }
                        ],
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1")
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let seed = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json"]);
    assert!(seed.status.success(), "{}", String::from_utf8_lossy(&seed.stderr));

    let out_with = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "msg-with-att", "--offline", "--json"],
    );
    assert!(out_with.status.success(), "{}", String::from_utf8_lossy(&out_with.stderr));
    let val_with: Value = serde_json::from_slice(&out_with.stdout).unwrap();
    assert_eq!(val_with["message"]["body_loaded"], false);
    assert!(val_with["message"]["body_plain"].is_null());
    assert_eq!(val_with["message"]["preview"], "Preview with attachment");
    assert!(val_with["message"]["omitted"]["attachments"].is_null());

    let out_no = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["read", "msg-no-att", "--offline", "--json"],
    );
    assert!(out_no.status.success(), "{}", String::from_utf8_lossy(&out_no.stderr));
    let val_no: Value = serde_json::from_slice(&out_no.stdout).unwrap();
    assert_eq!(val_no["message"]["body_loaded"], false);
    assert!(val_no["message"]["body_plain"].is_null());
    assert_eq!(val_no["message"]["preview"], "Preview plain only");
    assert_eq!(val_no["message"]["omitted"]["attachments"], 0);

    let out_text = run_outlook_cli(&dir, &base_url, &token, &["read", "msg-with-att", "--offline"]);
    assert!(out_text.status.success());
    let stdout_text = String::from_utf8_lossy(&out_text.stdout);
    let stderr_text = String::from_utf8_lossy(&out_text.stderr);
    assert!(stdout_text.contains("Preview with attachment"));
    assert!(stderr_text.contains("body not loaded"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_05_list_size_stable_after_hydration() {
    use std::collections::BTreeSet;

    let token = make_regression_jwt();
    let big_bytes = vec![b'A'; 64 * 1024];
    let big_b64 = B64_STANDARD.encode(&big_bytes);
    let big_ref = big_b64.clone();
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_delta = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "folder-inbox",
                        "displayName": "Inbox",
                        "wellKnownName": "inbox",
                        "childFolderCount": 0,
                        "unreadItemCount": 1,
                        "totalItemCount": 3
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let base_s = Arc::clone(&base_delta);
                async move {
                    let base = base_s.read().unwrap().clone();
                    Json(json!({
                        "value": [
                            {
                                "id": "msg-big-3",
                                "parentFolderId": fid,
                                "subject": "Big attachment",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T09:00:00Z",
                                "isRead": true,
                                "isDraft": false,
                                "hasAttachments": true,
                                "bodyPreview": "Large payload"
                            },
                            {
                                "id": "msg-2",
                                "parentFolderId": fid,
                                "subject": "Second",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T08:00:00Z",
                                "isRead": true,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "Second preview"
                            },
                            {
                                "id": "msg-1",
                                "parentFolderId": fid,
                                "subject": "First",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T07:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "First preview"
                            }
                        ],
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1")
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/messages/msg-big-3",
            get(move || {
                let b64 = big_ref.clone();
                async move {
                    Json(json!({
                        "id": "msg-big-3",
                        "parentFolderId": "folder-inbox",
                        "subject": "Big attachment",
                        "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                        "receivedDateTime": "2026-11-20T09:00:00Z",
                        "isRead": true,
                        "isDraft": false,
                        "hasAttachments": true,
                        "bodyPreview": "Large payload",
                        "body": { "contentType": "html", "content": "<p>Large payload body</p>" },
                        "attachments": [
                            {
                                "id": "att-big",
                                "name": "archive.bin",
                                "contentType": "application/octet-stream",
                                "size": 65536,
                                "isInline": false,
                                "contentBytes": b64
                            }
                        ]
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let list_before = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json"]);
    assert!(list_before.status.success(), "{}", String::from_utf8_lossy(&list_before.stderr));

    let read_big = run_outlook_cli(&dir, &base_url, &token, &["read", "msg-big-3", "--json"]);
    assert!(read_big.status.success(), "{}", String::from_utf8_lossy(&read_big.stderr));

    let list_after = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json"]);
    assert!(list_after.status.success(), "{}", String::from_utf8_lossy(&list_after.stderr));
    assert_eq!(list_before.stdout, list_after.stdout);

    let list_full = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json", "--full"]);
    assert!(list_full.status.success(), "{}", String::from_utf8_lossy(&list_full.stderr));
    let full_str = String::from_utf8_lossy(&list_full.stdout);
    assert!(!full_str.contains("content_bytes_b64"));
    let full_val: Value = serde_json::from_str(&full_str).unwrap();
    let big_item = full_val["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "msg-big-3")
        .unwrap();
    assert_eq!(big_item["body_loaded"], true);
    assert_eq!(big_item["attachments"][0]["content_cached"], true);

    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (err1, mcp_default) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_messages",
        json!({ "folder": "inbox", "offline": true }),
    )
    .await;
    assert!(!err1, "{mcp_default}");
    let expected_list_keys: BTreeSet<&str> = [
        "id",
        "subject",
        "from",
        "received_at",
        "is_read",
        "is_draft",
        "flagged",
        "has_attachments",
    ]
    .into_iter()
    .collect();
    for item in mcp_default["messages"].as_array().unwrap() {
        let keys: BTreeSet<&str> = item.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, expected_list_keys);
    }

    let (err2, mcp_full) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_messages",
        json!({ "folder": "inbox", "offline": true, "compact": false }),
    )
    .await;
    assert!(!err2, "{mcp_full}");
    let mcp_full_str = serde_json::to_string(&mcp_full).unwrap();
    assert!(!mcp_full_str.contains("content_bytes_b64"));
    let mcp_big = mcp_full["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "msg-big-3")
        .unwrap();
    assert_eq!(mcp_big["attachments"][0]["content_cached"], true);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_07_mail_list_cursor_walks_cache() {
    let token = make_regression_jwt();
    let graph_calls = Arc::new(AtomicUsize::new(0));
    let calls_f = Arc::clone(&graph_calls);
    let calls_d = Arc::clone(&graph_calls);
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_d = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(move || {
                let c = Arc::clone(&calls_f);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "value": [{
                            "id": "folder-inbox",
                            "displayName": "Inbox",
                            "wellKnownName": "inbox",
                            "childFolderCount": 0,
                            "unreadItemCount": 5,
                            "totalItemCount": 5
                        }]
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let c = Arc::clone(&calls_d);
                let base_s = Arc::clone(&base_d);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let base = base_s.read().unwrap().clone();
                    let items = vec![
                        ("m-top", "2026-11-20T10:00:00Z"),
                        ("m-tie-c", "2026-11-20T09:00:00Z"),
                        ("m-tie-a", "2026-11-20T09:00:00Z"),
                        ("m-tie-b", "2026-11-20T09:00:00Z"),
                        ("m-old", "2026-11-20T08:00:00Z"),
                    ];
                    let value: Vec<Value> = items
                        .into_iter()
                        .map(|(id, ts)| {
                            json!({
                                "id": id,
                                "parentFolderId": fid,
                                "subject": format!("Subject {id}"),
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": ts,
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": id
                            })
                        })
                        .collect();
                    Json(json!({
                        "value": value,
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1")
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let p1 = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json", "--limit", "2"]);
    assert!(p1.status.success(), "{}", String::from_utf8_lossy(&p1.stderr));
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(v1["returned"], 2);
    assert_eq!(v1["has_more"], true);
    let cur1 = v1["next_cursor"].as_str().unwrap().to_string();
    assert!(cur1.starts_with("c:"));

    let calls_before_cursor = graph_calls.load(Ordering::SeqCst);

    let p2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--json", "--limit", "2", "--cursor", &cur1],
    );
    assert!(p2.status.success(), "{}", String::from_utf8_lossy(&p2.stderr));
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["returned"], 2);
    assert_eq!(v2["has_more"], true);
    let cur2 = v2["next_cursor"].as_str().unwrap().to_string();

    let p3 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--json", "--limit", "2", "--cursor", &cur2],
    );
    assert!(p3.status.success(), "{}", String::from_utf8_lossy(&p3.stderr));
    let v3: Value = serde_json::from_slice(&p3.stdout).unwrap();
    assert_eq!(v3["returned"], 1);
    assert_eq!(v3["has_more"], false);
    assert!(v3["next_cursor"].is_null());

    assert_eq!(
        graph_calls.load(Ordering::SeqCst),
        calls_before_cursor,
        "c: cursor calls must make 0 Graph requests"
    );

    let mut ids = Vec::new();
    for v in [&v1, &v2, &v3] {
        for m in v["messages"].as_array().unwrap() {
            ids.push(m["id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(ids, vec!["m-top", "m-tie-c", "m-tie-b", "m-tie-a", "m-old"]);

    let all5 = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json", "--limit", "5"]);
    assert!(all5.status.success());
    let v_all5: Value = serde_json::from_slice(&all5.stdout).unwrap();
    assert_eq!(v_all5["returned"], 5);
    assert_eq!(v_all5["has_more"], false);
    assert!(v_all5["next_cursor"].is_null());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_07_empty_folder_page() {
    let token = make_regression_jwt();
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_d = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "folder-empty",
                        "displayName": "EmptyBox",
                        "wellKnownName": null,
                        "childFolderCount": 0,
                        "unreadItemCount": 0,
                        "totalItemCount": 0
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let base_s = Arc::clone(&base_d);
                async move {
                    let base = base_s.read().unwrap().clone();
                    Json(json!({
                        "value": [],
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1")
                    }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let out = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--folder", "EmptyBox", "--json"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["messages"], json!([]));
    assert_eq!(val["returned"], 0);
    assert_eq!(val["has_more"], false);
    assert!(val["next_cursor"].is_null());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_08_remote_search_follows_pages_to_limit() {
    use std::collections::HashSet;

    let token = make_regression_jwt();
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_s = Arc::clone(&base_slot);

    let router = Router::new().route(
        "/v1.0/me/messages",
        get(move |RawQuery(raw_q): RawQuery| {
            let base = base_s.read().unwrap().clone();
            async move {
                let q = raw_q.unwrap_or_default();
                let mut top: usize = 50;
                let mut skip: usize = 0;
                for pair in q.split('&') {
                    if let Some((k, v)) = pair.split_once('=') {
                        if k == "$top" {
                            top = v.parse().unwrap_or(50);
                        } else if k == "$skiptoken" {
                            skip = v.parse().unwrap_or(0);
                        }
                    }
                }
                let total = 130usize;
                let end = (skip + top).min(total);
                let items: Vec<Value> = (skip..end)
                    .map(|i| {
                        json!({
                            "id": format!("sr-{i:03}"),
                            "parentFolderId": "folder-inbox",
                            "subject": format!("Search hit {i}"),
                            "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                            "receivedDateTime": "2026-11-20T09:00:00Z",
                            "isRead": true,
                            "isDraft": false,
                            "hasAttachments": false,
                            "bodyPreview": format!("Hit {i}")
                        })
                    })
                    .collect();
                let mut body = json!({ "value": items });
                if end < total {
                    body["@odata.nextLink"] = json!(format!(
                        "{base}/v1.0/me/messages?$search=q&$top={top}&$skiptoken={end}"
                    ));
                }
                Json(body)
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let p1 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "q", "--remote", "--limit", "120", "--json"],
    );
    assert!(p1.status.success(), "{}", String::from_utf8_lossy(&p1.stderr));
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(v1["remote"].as_array().unwrap().len(), 120);
    assert_eq!(v1["returned"], 120);
    assert_eq!(v1["has_more"], true);
    let cur = v1["next_cursor"].as_str().unwrap().to_string();

    let p2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "q", "--remote", "--limit", "120", "--json", "--cursor", &cur],
    );
    assert!(p2.status.success(), "{}", String::from_utf8_lossy(&p2.stderr));
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["remote"].as_array().unwrap().len(), 10);
    assert_eq!(v2["returned"], 10);
    assert_eq!(v2["hits"], json!([]));
    assert_eq!(v2["has_more"], false);
    assert!(v2["next_cursor"].is_null());

    let mut all_ids: HashSet<String> = HashSet::new();
    for m in v1["remote"].as_array().unwrap() {
        all_ids.insert(m["id"].as_str().unwrap().to_string());
    }
    for m in v2["remote"].as_array().unwrap() {
        all_ids.insert(m["id"].as_str().unwrap().to_string());
    }
    assert_eq!(all_ids.len(), 130);

    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (is_err, mcp_val) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_mail",
        json!({ "query": "q", "limit": 200 }),
    )
    .await;
    assert!(!is_err, "{mcp_val}");
    assert_eq!(mcp_val["messages"].as_array().unwrap().len(), 130);
    assert_eq!(mcp_val["returned"], 130);
    assert_eq!(mcp_val["has_more"], false);
    assert!(mcp_val["next_cursor"].is_null());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_08_remote_search_control_small() {
    let token = make_regression_jwt();
    let req_count = Arc::new(AtomicUsize::new(0));
    let cnt = Arc::clone(&req_count);
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_s = Arc::clone(&base_slot);

    let router = Router::new().route(
        "/v1.0/me/messages",
        get(move |RawQuery(raw_q): RawQuery| {
            let c = Arc::clone(&cnt);
            let base = base_s.read().unwrap().clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                let q = raw_q.unwrap_or_default();
                let top: usize = q
                    .split('&')
                    .find_map(|p| p.strip_prefix("$top="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50);
                let skip: usize = q
                    .split('&')
                    .find_map(|p| p.strip_prefix("$skiptoken="))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let total = 60usize;
                let end = (skip + top).min(total);
                let items: Vec<Value> = (skip..end)
                    .map(|i| {
                        json!({
                            "id": format!("sm-{i:03}"),
                            "parentFolderId": "folder-inbox",
                            "subject": format!("Small {i}"),
                            "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                            "receivedDateTime": "2026-11-20T09:00:00Z",
                            "isRead": true,
                            "isDraft": false,
                            "hasAttachments": false,
                            "bodyPreview": "small"
                        })
                    })
                    .collect();
                let mut body = json!({ "value": items });
                if end < total {
                    body["@odata.nextLink"] = json!(format!(
                        "{base}/v1.0/me/messages?$search=q&$top={top}&$skiptoken={end}"
                    ));
                }
                Json(body)
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let out = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "q", "--remote", "--limit", "30", "--json"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["remote"].as_array().unwrap().len(), 30);
    assert_eq!(val["returned"], 30);
    assert_eq!(val["has_more"], true);
    assert_eq!(req_count.load(Ordering::SeqCst), 1);
    assert!(
        val["remote"][0].get("body_html").is_none(),
        "search --remote --json must be compact by default"
    );

    let out_full = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "q", "--remote", "--limit", "30", "--json", "--full"],
    );
    assert!(
        out_full.status.success(),
        "{}",
        String::from_utf8_lossy(&out_full.stderr)
    );
    let val_full: Value = serde_json::from_slice(&out_full.stdout).unwrap();
    assert!(
        val_full["remote"][0].get("body_html").is_some(),
        "search --remote --json --full must include full message fields"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_08_cursor_rejects_foreign_url() {
    let token = make_regression_jwt();
    let req_count = Arc::new(AtomicUsize::new(0));
    let cnt = Arc::clone(&req_count);

    let router = Router::new().fallback(get(move || {
        let c = Arc::clone(&cnt);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            Json(json!({ "value": [] }))
        }
    }));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let other_port = if addr.port() == 65535 { addr.port() - 1 } else { addr.port() + 1 };
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);

    let scope = plan_cursor_scope(&["search", "", "q"]);
    let bad_urls = [
        "https://example.invalid/me/messages".to_string(),
        format!("http://127.0.0.1:{other_port}/v1.0/me/messages"),
        format!("{base_url}/v2.0/me/messages"),
        format!("{base_url}/v1.0/users/x/messages"),
    ];

    for bad_url in bad_urls {
        let cur = format!("g:{scope}:0|{bad_url}");
        let out = run_outlook_cli(
            &dir,
            &base_url,
            &token,
            &["search", "q", "--remote", "--json", "--cursor", &cur],
        );
        assert_eq!(out.status.code(), Some(1));
        let err_json: Value =
            serde_json::from_slice(&out.stdout).expect("expected structured JSON error on stdout");
        assert_eq!(err_json["ok"], false);
        assert_eq!(err_json["error"]["code"], "error");
        assert!(err_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid cursor: url is not a Graph URL for this account"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("invalid cursor: url is not a Graph URL for this account"),
            "unexpected stderr for {bad_url}: {stderr}"
        );

        let (is_err, mcp_val) = call_mcp_tool_json(
            &cfg,
            &graph,
            &storage,
            "search_mail",
            json!({ "query": "q", "cursor": cur }),
        )
        .await;
        assert!(is_err, "expected MCP error for {bad_url}");
        assert_eq!(
            mcp_val.as_str(),
            Some("invalid cursor: url is not a Graph URL for this account")
        );
    }

    assert_eq!(req_count.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_cursor_rejects_changed_folder_or_filter() {
    use std::collections::HashSet;

    let token = make_regression_jwt();
    let req_count = Arc::new(AtomicUsize::new(0));
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));

    let inbox_items: Arc<Vec<Value>> = Arc::new(
        (0..60)
            .map(|i| {
                let ts = chrono::DateTime::parse_from_rfc3339("2026-11-21T12:00:00Z").unwrap()
                    - chrono::Duration::minutes(i as i64);
                json!({
                    "id": format!("inb-{i:03}"),
                    "parentFolderId": "folder-inbox",
                    "subject": format!("Inbox {i}"),
                    "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                    "receivedDateTime": ts.to_rfc3339(),
                    "isRead": i >= 40,
                    "isDraft": false,
                    "flag": { "flagStatus": "notFlagged" },
                    "hasAttachments": false,
                    "bodyPreview": format!("Inbox {i}")
                })
            })
            .collect(),
    );
    let sprint_items: Arc<Vec<Value>> = Arc::new(
        (0..3)
            .map(|i| {
                let ts = chrono::DateTime::parse_from_rfc3339("2026-11-21T10:00:00Z").unwrap()
                    - chrono::Duration::minutes(i as i64);
                json!({
                    "id": format!("spr-{i:03}"),
                    "parentFolderId": "folder-sprint",
                    "subject": format!("Sprint {i}"),
                    "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                    "receivedDateTime": ts.to_rfc3339(),
                    "isRead": false,
                    "isDraft": false,
                    "hasAttachments": false,
                    "bodyPreview": format!("Sprint {i}")
                })
            })
            .collect(),
    );

    let cnt_f = Arc::clone(&req_count);
    let cnt_d = Arc::clone(&req_count);
    let cnt_p = Arc::clone(&req_count);
    let base_d = Arc::clone(&base_slot);
    let base_p = Arc::clone(&base_slot);
    let inb_d = Arc::clone(&inbox_items);
    let spr_d = Arc::clone(&sprint_items);
    let inb_p = Arc::clone(&inbox_items);
    let spr_p = Arc::clone(&sprint_items);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(move || {
                let c = Arc::clone(&cnt_f);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "value": [
                            { "id": "folder-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 40, "totalItemCount": 60 },
                            { "id": "folder-sprint", "displayName": "Sprint", "wellKnownName": null, "childFolderCount": 0, "unreadItemCount": 3, "totalItemCount": 3 }
                        ]
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages/delta",
            get(move |Path(fid): Path<String>| {
                let c = Arc::clone(&cnt_d);
                let base_s = Arc::clone(&base_d);
                let inb = Arc::clone(&inb_d);
                let spr = Arc::clone(&spr_d);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let base = base_s.read().unwrap().clone();
                    let items = if fid == "folder-sprint" {
                        (*spr).clone()
                    } else {
                        (*inb).clone()
                    };
                    Json(json!({
                        "value": items,
                        "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/{fid}/messages/delta?$deltatoken=d1")
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages",
            get(move |Path(fid): Path<String>, RawQuery(raw_q): RawQuery| {
                let c = Arc::clone(&cnt_p);
                let base_s = Arc::clone(&base_p);
                let inb = Arc::clone(&inb_p);
                let spr = Arc::clone(&spr_p);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let base = base_s.read().unwrap().clone();
                    let q = raw_q.unwrap_or_default();
                    let skip: usize = q
                        .split('&')
                        .find_map(|p| p.strip_prefix("$skip="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let all = if fid == "folder-sprint" { &spr } else { &inb };
                    let page_sz = 25usize;
                    let end = (skip + page_sz).min(all.len());
                    let slice = all[skip.min(all.len())..end].to_vec();
                    let mut body = json!({ "value": slice });
                    if end < all.len() {
                        body["@odata.nextLink"] = json!(format!(
                            "{base}/v1.0/me/mailFolders/{fid}/messages?$top={page_sz}&$skip={end}"
                        ));
                    }
                    Json(body)
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // Acquire c: cursor and g: cursor
    let c_p1 = run_outlook_cli(&dir, &base_url, &token, &["mail", "--json", "--limit", "2"]);
    assert!(c_p1.status.success(), "{}", String::from_utf8_lossy(&c_p1.stderr));
    let c_v1: Value = serde_json::from_slice(&c_p1.stdout).unwrap();
    let c_cur = c_v1["next_cursor"].as_str().unwrap().to_string();
    assert!(c_cur.starts_with("c:"));
    let c_ids1: HashSet<String> = c_v1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();

    let g_p1 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--unread", "--limit", "5", "--refresh", "--json"],
    );
    assert!(g_p1.status.success(), "{}", String::from_utf8_lossy(&g_p1.stderr));
    let g_v1: Value = serde_json::from_slice(&g_p1.stdout).unwrap();
    let g_cur = g_v1["next_cursor"].as_str().unwrap().to_string();
    assert!(g_cur.starts_with("g:"));
    let g_ids1: HashSet<String> = g_v1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();

    let expected_err = "invalid cursor: issued for a different command, folder, query or filter";
    let expected_cli_stderr = format!("Error: {expected_err}");
    let baseline = req_count.load(Ordering::SeqCst);

    // 1..5 CLI mismatches
    let cli_bad_cases: Vec<Vec<&str>> = vec![
        vec!["mail", "--folder", "Sprint", "--json", "--limit", "2", "--cursor", &c_cur],
        vec!["mail", "--unread", "--json", "--limit", "2", "--cursor", &c_cur],
        vec!["mail", "--folder", "Sprint", "--unread", "--json", "--limit", "5", "--cursor", &g_cur],
        vec!["mail", "--flagged", "--json", "--limit", "5", "--cursor", &g_cur],
        vec!["mail", "--json", "--limit", "5", "--cursor", &g_cur],
    ];
    for args in cli_bad_cases {
        let out = run_outlook_cli(&dir, &base_url, &token, &args);
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {args:?}");
        let err_json: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("expected JSON error stdout for {args:?}: {e}"));
        assert_eq!(err_json["ok"], false);
        assert_eq!(err_json["error"]["message"], expected_err);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            expected_cli_stderr,
            "unexpected stderr for {args:?}"
        );
    }

    // 6. MCP mismatches repeating (1) and (3)
    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    for mcp_args in [
        json!({ "folder": "Sprint", "limit": 2, "cursor": c_cur }),
        json!({ "folder": "Sprint", "unread": true, "limit": 5, "cursor": g_cur }),
    ] {
        let (is_err, val) =
            call_mcp_tool_json(&cfg, &graph, &storage, "list_messages", mcp_args).await;
        assert!(is_err);
        assert_eq!(val.as_str(), Some(expected_err));
    }

    assert_eq!(
        req_count.load(Ordering::SeqCst),
        baseline,
        "scope mismatches must make 0 Graph calls"
    );

    // 7. Positive controls: original args succeed and continue without overlap
    let c_p2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--json", "--limit", "2", "--cursor", &c_cur],
    );
    assert!(c_p2.status.success(), "{}", String::from_utf8_lossy(&c_p2.stderr));
    let c_v2: Value = serde_json::from_slice(&c_p2.stdout).unwrap();
    let c_ids2: Vec<String> = c_v2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(c_ids2, vec!["inb-002", "inb-003"]);
    assert!(c_ids2.iter().all(|id| !c_ids1.contains(id)));

    let g_p2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--unread", "--limit", "5", "--json", "--cursor", &g_cur],
    );
    assert!(g_p2.status.success(), "{}", String::from_utf8_lossy(&g_p2.stderr));
    let g_v2: Value = serde_json::from_slice(&g_p2.stdout).unwrap();
    let g_ids2: Vec<String> = g_v2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(g_ids2, vec!["inb-005", "inb-006", "inb-007", "inb-008", "inb-009"]);
    assert!(g_ids2.iter().all(|id| !g_ids1.contains(id)));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_cursor_rejects_changed_query_or_operation() {
    let token = make_regression_jwt();
    let req_count = Arc::new(AtomicUsize::new(0));
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));

    let cnt_m = Arc::clone(&req_count);
    let cnt_f = Arc::clone(&req_count);
    let cnt_t = Arc::clone(&req_count);
    let base_m = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/messages",
            get(move |RawQuery(raw_q): RawQuery| {
                let c = Arc::clone(&cnt_m);
                let base = base_m.read().unwrap().clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let q = raw_q.unwrap_or_default();
                    let top: usize = q
                        .split('&')
                        .find_map(|p| p.strip_prefix("$top="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(50);
                    let skip: usize = q
                        .split('&')
                        .find_map(|p| p.strip_prefix("$skiptoken="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let total = 130usize;
                    let end = (skip + top).min(total);
                    let items: Vec<Value> = (skip..end)
                        .map(|i| {
                            json!({
                                "id": format!("alpha-{i:03}"),
                                "parentFolderId": "folder-inbox",
                                "subject": format!("Alpha {i}"),
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T09:00:00Z",
                                "isRead": true,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": format!("Alpha {i}")
                            })
                        })
                        .collect();
                    let mut body = json!({ "value": items });
                    if end < total {
                        body["@odata.nextLink"] = json!(format!(
                            "{base}/v1.0/me/messages?$search=alpha&$top={top}&$skiptoken={end}"
                        ));
                    }
                    Json(body)
                }
            }),
        )
        .route(
            "/v1.0/me/mailFolders",
            get(move || {
                let c = Arc::clone(&cnt_f);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let mut folders = vec![json!({
                        "id": "folder-inbox",
                        "displayName": "Inbox",
                        "wellKnownName": "inbox",
                        "childFolderCount": 0,
                        "unreadItemCount": 0,
                        "totalItemCount": 0
                    })];
                    for i in 1..60 {
                        folders.push(json!({
                            "id": format!("fld-{i:03}"),
                            "displayName": format!("Folder {i:03}"),
                            "wellKnownName": null,
                            "childFolderCount": 0,
                            "unreadItemCount": 0,
                            "totalItemCount": 0
                        }));
                    }
                    Json(json!({ "value": folders }))
                }
            }),
        )
        .route(
            "/v1.0/me/todo/lists",
            get(move || {
                let c = Arc::clone(&cnt_t);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let lists: Vec<Value> = (0..60)
                        .map(|i| {
                            json!({
                                "id": format!("tl-{i:03}"),
                                "displayName": format!("List {i:03}"),
                                "wellknownListName": if i == 0 { json!("defaultList") } else { Value::Null },
                                "isOwner": true,
                                "isShared": false
                            })
                        })
                        .collect();
                    Json(json!({ "value": lists }))
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let s1 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", "alpha", "--remote", "--limit", "50", "--json"],
    );
    assert!(s1.status.success(), "{}", String::from_utf8_lossy(&s1.stderr));
    let s1_val: Value = serde_json::from_slice(&s1.stdout).unwrap();
    let search_cur = s1_val["next_cursor"].as_str().unwrap().to_string();

    let f1 = run_outlook_cli(&dir, &base_url, &token, &["folders", "--json"]);
    assert!(f1.status.success());
    let f1_val: Value = serde_json::from_slice(&f1.stdout).unwrap();
    let folders_cur = f1_val["next_cursor"].as_str().unwrap().to_string();

    let l1 = run_outlook_cli(&dir, &base_url, &token, &["task-lists", "--json"]);
    assert!(l1.status.success());
    let l1_val: Value = serde_json::from_slice(&l1.stdout).unwrap();
    let lists_cur = l1_val["next_cursor"].as_str().unwrap().to_string();

    let expected_err = "invalid cursor: issued for a different command, folder, query or filter";
    let expected_cli_stderr = format!("Error: {expected_err}");
    let baseline = req_count.load(Ordering::SeqCst);

    // 1..4 CLI mismatches
    let bad_cli: Vec<Vec<&str>> = vec![
        vec!["search", "beta", "--remote", "--limit", "50", "--json", "--cursor", &search_cur],
        vec!["search", "ALPHA", "--remote", "--limit", "50", "--json", "--cursor", &search_cur],
        vec!["mail", "--json", "--cursor", &search_cur],
        vec!["task-lists", "--json", "--cursor", &folders_cur],
        vec!["folders", "--json", "--cursor", &lists_cur],
    ];
    for args in bad_cli {
        let out = run_outlook_cli(&dir, &base_url, &token, &args);
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {args:?}");
        let err_json: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("expected JSON error stdout for {args:?}: {e}"));
        assert_eq!(err_json["ok"], false);
        assert_eq!(err_json["error"]["message"], expected_err);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            expected_cli_stderr,
            "unexpected stderr for {args:?}"
        );
    }

    // 5. MCP search_mail mismatch
    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (is_err, mcp_bad) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_mail",
        json!({ "query": "beta", "limit": 50, "cursor": search_cur }),
    )
    .await;
    assert!(is_err);
    assert_eq!(mcp_bad.as_str(), Some(expected_err));

    assert_eq!(
        req_count.load(Ordering::SeqCst),
        baseline,
        "scope mismatches must make 0 Graph calls"
    );

    // 6. Controls: whitespace-trimmed query & CLI cursor reused in MCP
    let s2_cli = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["search", " alpha ", "--remote", "--limit", "20", "--json", "--cursor", &search_cur],
    );
    assert!(s2_cli.status.success(), "{}", String::from_utf8_lossy(&s2_cli.stderr));
    let s2_cli_val: Value = serde_json::from_slice(&s2_cli.stdout).unwrap();
    assert_eq!(s2_cli_val["returned"], 20);
    assert_eq!(s2_cli_val["remote"][0]["id"], "alpha-050");

    let (is_err_ok, mcp_ok) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_mail",
        json!({ "query": "alpha", "limit": 20, "cursor": search_cur }),
    )
    .await;
    assert!(!is_err_ok, "{mcp_ok}");
    assert_eq!(mcp_ok["returned"], 20);
    assert_eq!(mcp_ok["messages"][0]["id"], "alpha-050");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_lim_filter_scan_stops_at_page_cap() {
    let token = make_regression_jwt();
    let msg_page_calls = Arc::new(AtomicUsize::new(0));
    let cnt = Arc::clone(&msg_page_calls);
    let base_slot: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let base_p = Arc::clone(&base_slot);

    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "folder-inbox",
                        "displayName": "Inbox",
                        "wellKnownName": "inbox",
                        "childFolderCount": 0,
                        "unreadItemCount": 1,
                        "totalItemCount": 601
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/mailFolders/{fid}/messages",
            get(move |Path(fid): Path<String>, RawQuery(raw_q): RawQuery| {
                let c = Arc::clone(&cnt);
                let base = base_p.read().unwrap().clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let q = raw_q.unwrap_or_default();
                    let skip: usize = q
                        .split('&')
                        .find_map(|p| p.strip_prefix("$skip="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let total = 601usize;
                    let page_sz = 25usize;
                    let end = (skip + page_sz).min(total);
                    let items: Vec<Value> = (skip..end)
                        .map(|i| {
                            json!({
                                "id": format!("scan-{i:04}"),
                                "parentFolderId": fid,
                                "subject": format!("Mail {i}"),
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-20T09:00:00Z",
                                "isRead": i < 600,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "preview"
                            })
                        })
                        .collect();
                    let mut body = json!({ "value": items });
                    if end < total {
                        body["@odata.nextLink"] = json!(format!(
                            "{base}/v1.0/me/mailFolders/{fid}/messages?$top={page_sz}&$skip={end}"
                        ));
                    }
                    Json(body)
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *base_slot.write().unwrap() = base_url.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let p1 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--unread", "--limit", "5", "--refresh", "--json"],
    );
    assert!(p1.status.success(), "{}", String::from_utf8_lossy(&p1.stderr));
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(msg_page_calls.load(Ordering::SeqCst), 20);
    assert_eq!(v1["messages"], json!([]));
    assert_eq!(v1["returned"], 0);
    assert!(v1["has_more"].is_null());
    let cur = v1["next_cursor"].as_str().unwrap().to_string();
    let expected_scope = plan_cursor_scope(&["mail", "folder-inbox", "100"]);
    assert!(
        cur.starts_with(&format!("g:{expected_scope}:")),
        "unexpected cursor: {cur}"
    );

    let p2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["mail", "--unread", "--limit", "5", "--json", "--cursor", &cur],
    );
    assert!(p2.status.success(), "{}", String::from_utf8_lossy(&p2.stderr));
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["returned"], 1);
    assert_eq!(v2["messages"][0]["id"], "scan-0600");
    assert_eq!(v2["has_more"], false);
    assert!(v2["next_cursor"].is_null());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_lim_folders_and_task_lists_page() {
    use std::collections::HashSet;

    let token = make_regression_jwt();
    let router = Router::new()
        .route(
            "/v1.0/me/mailFolders",
            get(|| async {
                let folders: Vec<Value> = (0..120)
                    .map(|i| {
                        json!({
                            "id": format!("f-{i:03}"),
                            "displayName": format!("Folder {i:03}"),
                            "wellKnownName": if i == 0 { json!("inbox") } else { Value::Null },
                            "childFolderCount": 0,
                            "unreadItemCount": 0,
                            "totalItemCount": 0
                        })
                    })
                    .collect();
                Json(json!({ "value": folders }))
            }),
        )
        .route(
            "/v1.0/me/todo/lists",
            get(|| async {
                let lists: Vec<Value> = (0..120)
                    .map(|i| {
                        json!({
                            "id": format!("tl-{i:03}"),
                            "displayName": format!("List {i:03}"),
                            "wellknownListName": if i == 0 { json!("defaultList") } else { Value::Null },
                            "isOwner": true,
                            "isShared": false
                        })
                    })
                    .collect();
                Json(json!({ "value": lists }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // Folders paging: 50 + 50 + 20
    let mut folder_ids = HashSet::new();
    let mut page_lens = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        let mut args = vec!["folders", "--json"];
        if let Some(ref c) = cur {
            args.push("--cursor");
            args.push(c);
        }
        let out = run_outlook_cli(&dir, &base_url, &token, &args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let items = v["folders"].as_array().unwrap();
        page_lens.push(items.len());
        assert_eq!(v["returned"], items.len());
        if cur.is_none() {
            assert_eq!(v["next_cursor"].as_str(), Some("o:f74800ea:50"));
        }
        for item in items {
            assert!(folder_ids.insert(item["id"].as_str().unwrap().to_string()));
        }
        if v["has_more"] == true {
            cur = Some(v["next_cursor"].as_str().unwrap().to_string());
        } else {
            assert!(v["next_cursor"].is_null());
            break;
        }
    }
    assert_eq!(page_lens, vec![50, 50, 20]);
    assert_eq!(folder_ids.len(), 120);

    let f_all = run_outlook_cli(&dir, &base_url, &token, &["folders", "--json", "--limit", "1000"]);
    assert!(f_all.status.success());
    let f_all_val: Value = serde_json::from_slice(&f_all.stdout).unwrap();
    assert_eq!(f_all_val["returned"], 120);
    assert_eq!(f_all_val["has_more"], false);

    let f_text = run_outlook_cli(&dir, &base_url, &token, &["folders"]);
    assert!(f_text.status.success());
    assert_eq!(String::from_utf8_lossy(&f_text.stdout).lines().count(), 120);

    // Task lists paging: 50 + 50 + 20
    let mut list_ids = HashSet::new();
    let mut list_lens = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        let mut args = vec!["task-lists", "--json"];
        if let Some(ref c) = cur {
            args.push("--cursor");
            args.push(c);
        }
        let out = run_outlook_cli(&dir, &base_url, &token, &args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let items = v["lists"].as_array().unwrap();
        list_lens.push(items.len());
        assert_eq!(v["returned"], items.len());
        if cur.is_none() {
            assert_eq!(v["next_cursor"].as_str(), Some("o:14525d78:50"));
        }
        for item in items {
            assert!(list_ids.insert(item["id"].as_str().unwrap().to_string()));
        }
        if v["has_more"] == true {
            cur = Some(v["next_cursor"].as_str().unwrap().to_string());
        } else {
            assert!(v["next_cursor"].is_null());
            break;
        }
    }
    assert_eq!(list_lens, vec![50, 50, 20]);
    assert_eq!(list_ids.len(), 120);

    let l_all = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["task-lists", "--json", "--limit", "1000"],
    );
    assert!(l_all.status.success());
    let l_all_val: Value = serde_json::from_slice(&l_all.stdout).unwrap();
    assert_eq!(l_all_val["returned"], 120);
    assert_eq!(l_all_val["has_more"], false);

    let l_text = run_outlook_cli(&dir, &base_url, &token, &["task-lists"]);
    assert!(l_text.status.success());
    assert_eq!(String::from_utf8_lossy(&l_text.stdout).lines().count(), 120);

    // MCP list_mail_folders & limit bounds
    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (is_err, mcp_f5) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_mail_folders",
        json!({ "limit": 5 }),
    )
    .await;
    assert!(!is_err, "{mcp_f5}");
    assert_eq!(mcp_f5["folders"].as_array().unwrap().len(), 5);
    assert_eq!(mcp_f5["returned"], 5);
    assert_eq!(mcp_f5["has_more"], true);

    for bad_lim in [0usize, 201usize] {
        let (err_f, _) = call_mcp_tool_json(
            &cfg,
            &graph,
            &storage,
            "list_mail_folders",
            json!({ "limit": bad_lim }),
        )
        .await;
        assert!(err_f, "expected MCP error for limit={bad_lim}");
        let (err_t, _) = call_mcp_tool_json(
            &cfg,
            &graph,
            &storage,
            "list_todo_lists",
            json!({ "limit": bad_lim }),
        )
        .await;
        assert!(err_t, "expected MCP error for limit={bad_lim}");
    }

    let (err_cr, msg_cr) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_mail_folders",
        json!({ "refresh": true, "cursor": "o:f74800ea:50" }),
    )
    .await;
    assert!(err_cr);
    assert_eq!(msg_cr.as_str(), Some("cursor cannot be combined with refresh"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_09_10_truncation_is_disclosed() {
    let token = make_regression_jwt();
    let router = Router::new()
        .route(
            "/v1.0/me/calendarView",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "ev-1",
                            "subject": "Event 1",
                            "start": { "dateTime": "2026-11-22T09:00:00Z", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-11-22T10:00:00Z", "timeZone": "UTC" }
                        },
                        {
                            "id": "ev-2",
                            "subject": "Event 2",
                            "start": { "dateTime": "2026-11-22T11:00:00Z", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-11-22T12:00:00Z", "timeZone": "UTC" }
                        },
                        {
                            "id": "ev-3",
                            "subject": "Event 3",
                            "start": { "dateTime": "2026-11-22T13:00:00Z", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-11-22T14:00:00Z", "timeZone": "UTC" }
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/me/todo/lists",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "tl-default",
                        "displayName": "Tasks",
                        "wellknownListName": "defaultList",
                        "isOwner": true,
                        "isShared": false
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/todo/lists/{lid}/tasks",
            get(|| async {
                let tasks: Vec<Value> = (1..=5)
                    .map(|i| {
                        json!({
                            "id": format!("t-{i}"),
                            "title": format!("Task {i}"),
                            "status": "notStarted",
                            "importance": "normal"
                        })
                    })
                    .collect();
                Json(json!({ "value": tasks }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // Calendar online limit 2 -> has_more true
    let cal2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "calendar",
            "--from",
            "2026-11-22T00:00:00Z",
            "--to",
            "2026-11-23T00:00:00Z",
            "--limit",
            "2",
            "--json",
        ],
    );
    assert!(cal2.status.success(), "{}", String::from_utf8_lossy(&cal2.stderr));
    let cal2_v: Value = serde_json::from_slice(&cal2.stdout).unwrap();
    assert_eq!(cal2_v["returned"], 2);
    assert_eq!(cal2_v["has_more"], true);
    assert!(cal2_v.get("start").is_some() && cal2_v.get("end").is_some());

    // Fetch all 3 online via MCP so cache holds all 3, verify has_more=false
    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (e_m2, mcp_cal2) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_events",
        json!({ "from": "2026-11-22T00:00:00Z", "to": "2026-11-23T00:00:00Z", "limit": 2 }),
    )
    .await;
    assert!(!e_m2, "{mcp_cal2}");
    assert_eq!(mcp_cal2["returned"], 2);
    assert_eq!(mcp_cal2["has_more"], true);

    let (e_m3, mcp_cal3) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "list_events",
        json!({ "from": "2026-11-22T00:00:00Z", "to": "2026-11-23T00:00:00Z", "limit": 3 }),
    )
    .await;
    assert!(!e_m3, "{mcp_cal3}");
    assert_eq!(mcp_cal3["returned"], 3);
    assert_eq!(mcp_cal3["has_more"], false);

    // Offline calendar limit 2 (has_more=true) & limit 3 (has_more=false)
    let cal_off2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "calendar",
            "--from",
            "2026-11-22T00:00:00Z",
            "--to",
            "2026-11-23T00:00:00Z",
            "--limit",
            "2",
            "--offline",
            "--json",
        ],
    );
    assert!(cal_off2.status.success());
    let cal_off2_v: Value = serde_json::from_slice(&cal_off2.stdout).unwrap();
    assert_eq!(cal_off2_v["returned"], 2);
    assert_eq!(cal_off2_v["has_more"], true);

    let cal_off3 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "calendar",
            "--from",
            "2026-11-22T00:00:00Z",
            "--to",
            "2026-11-23T00:00:00Z",
            "--limit",
            "3",
            "--offline",
            "--json",
        ],
    );
    assert!(cal_off3.status.success());
    let cal_off3_v: Value = serde_json::from_slice(&cal_off3.stdout).unwrap();
    assert_eq!(cal_off3_v["returned"], 3);
    assert_eq!(cal_off3_v["has_more"], false);

    // Tasks online limit 2 -> has_more true, limit 5 -> has_more false
    let t2 = run_outlook_cli(&dir, &base_url, &token, &["tasks", "--limit", "2", "--json"]);
    assert!(t2.status.success(), "{}", String::from_utf8_lossy(&t2.stderr));
    let t2_v: Value = serde_json::from_slice(&t2.stdout).unwrap();
    assert_eq!(t2_v["returned"], 2);
    assert_eq!(t2_v["has_more"], true);
    assert_eq!(t2_v["list_id"], "tl-default");

    let t5 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["tasks", "--limit", "5", "--refresh", "--json"],
    );
    assert!(t5.status.success(), "{}", String::from_utf8_lossy(&t5.stderr));
    let t5_v: Value = serde_json::from_slice(&t5.stdout).unwrap();
    assert_eq!(t5_v["returned"], 5);
    assert_eq!(t5_v["has_more"], false);

    let t_off2 = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["tasks", "--limit", "2", "--offline", "--json"],
    );
    assert!(t_off2.status.success());
    let t_off2_v: Value = serde_json::from_slice(&t_off2.stdout).unwrap();
    assert_eq!(t_off2_v["returned"], 2);
    assert_eq!(t_off2_v["has_more"], true);

    // MCP list_tasks
    let (e_t2, mcp_t2) =
        call_mcp_tool_json(&cfg, &graph, &storage, "list_tasks", json!({ "limit": 2 })).await;
    assert!(!e_t2, "{mcp_t2}");
    assert_eq!(mcp_t2["returned"], 2);
    assert_eq!(mcp_t2["has_more"], true);
    assert_eq!(mcp_t2["list_id"], "tl-default");
    assert!(mcp_t2.get("list").is_some());

    let (e_t5, mcp_t5) =
        call_mcp_tool_json(&cfg, &graph, &storage, "list_tasks", json!({ "limit": 5 })).await;
    assert!(!e_t5, "{mcp_t5}");
    assert_eq!(mcp_t5["returned"], 5);
    assert_eq!(mcp_t5["has_more"], false);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_11_directory_failure_keeps_personal_results() {
    let token = make_regression_jwt();
    let users_status = Arc::new(AtomicUsize::new(200));
    let u_st = Arc::clone(&users_status);

    let router = Router::new()
        .route(
            "/v1.0/me/contacts",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "c-001",
                        "displayName": "Bob Reviewer",
                        "givenName": "Bob",
                        "surname": "Reviewer",
                        "emailAddresses": [{ "address": "bob@contoso.com" }],
                        "companyName": "Contoso"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/users",
            get(move || {
                let st = u_st.load(Ordering::SeqCst);
                async move {
                    match st {
                        200 => (
                            StatusCode::OK,
                            Json(json!({
                                "value": [{
                                    "id": "u-001",
                                    "displayName": "Carol Lead",
                                    "mail": "carol@contoso.com",
                                    "companyName": "Contoso"
                                }]
                            })),
                        )
                            .into_response(),
                        403 => (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "error": {
                                    "code": "Authorization_RequestDenied",
                                    "message": "Insufficient privileges to complete the operation."
                                }
                            })),
                        )
                            .into_response(),
                        _ => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": {
                                    "code": "InternalServerError",
                                    "message": "Directory unavailable"
                                }
                            })),
                        )
                            .into_response(),
                    }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // 200 OK
    users_status.store(200, Ordering::SeqCst);
    let out_200 = run_outlook_cli(&dir, &base_url, &token, &["contacts", "--query", "Bob", "--json"]);
    assert!(out_200.status.success());
    let v_200: Value = serde_json::from_slice(&out_200.stdout).unwrap();
    assert_eq!(v_200["directory"]["status"], "ok");
    assert!(v_200["directory"]["error"].is_null());
    assert_eq!(v_200["contacts"][0]["display_name"], "Bob Reviewer");

    // 403 Forbidden (CLI JSON, CLI text, MCP)
    users_status.store(403, Ordering::SeqCst);
    let out_403 = run_outlook_cli(&dir, &base_url, &token, &["contacts", "--query", "Bob", "--json"]);
    assert!(out_403.status.success());
    assert!(out_403.stderr.is_empty());
    let v_403: Value = serde_json::from_slice(&out_403.stdout).unwrap();
    assert_eq!(v_403["contacts"].as_array().unwrap().len(), 1);
    assert_eq!(v_403["contacts"][0]["display_name"], "Bob Reviewer");
    assert_eq!(v_403["directory"]["status"], "forbidden");
    assert!(v_403["directory"]["error"].as_str().is_some_and(|s| !s.is_empty()));

    let out_403_txt = run_outlook_cli(&dir, &base_url, &token, &["contacts", "--query", "Bob"]);
    assert!(out_403_txt.status.success());
    assert!(String::from_utf8_lossy(&out_403_txt.stdout).contains("Bob Reviewer"));
    assert!(String::from_utf8_lossy(&out_403_txt.stderr)
        .contains("warning: directory search failed (forbidden)"));

    let (cfg, graph, storage) = plan_mcp_runtime(&dir, &base_url, &token);
    let (err_m403, mcp_403) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_contacts",
        json!({ "query": "Bob" }),
    )
    .await;
    assert!(!err_m403, "{mcp_403}");
    assert_eq!(mcp_403["contacts"][0]["display_name"], "Bob Reviewer");
    assert_eq!(mcp_403["directory"]["status"], "forbidden");

    // 500 Internal Server Error (CLI JSON, CLI text, MCP)
    users_status.store(500, Ordering::SeqCst);
    let out_500 = run_outlook_cli(&dir, &base_url, &token, &["contacts", "--query", "Bob", "--json"]);
    assert!(out_500.status.success());
    let v_500: Value = serde_json::from_slice(&out_500.stdout).unwrap();
    assert_eq!(v_500["contacts"][0]["display_name"], "Bob Reviewer");
    assert_eq!(v_500["directory"]["status"], "error");
    assert!(v_500["directory"]["error"].as_str().is_some_and(|s| !s.is_empty()));

    let out_500_txt = run_outlook_cli(&dir, &base_url, &token, &["contacts", "--query", "Bob"]);
    assert!(out_500_txt.status.success());
    assert!(String::from_utf8_lossy(&out_500_txt.stdout).contains("Bob Reviewer"));
    assert!(String::from_utf8_lossy(&out_500_txt.stderr)
        .contains("warning: directory search failed (error)"));

    let (err_m500, mcp_500) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "search_contacts",
        json!({ "query": "Bob" }),
    )
    .await;
    assert!(!err_m500, "{mcp_500}");
    assert_eq!(mcp_500["directory"]["status"], "error");

    // Offline -> not_queried
    let out_off = run_outlook_cli(&dir, &base_url, &token, &["contacts", "--offline", "--json"]);
    assert!(out_off.status.success());
    let v_off: Value = serde_json::from_slice(&out_off.stdout).unwrap();
    assert_eq!(v_off["directory"]["status"], "not_queried");
    assert!(v_off["directory"]["error"].is_null());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_12_cli_download_offline() {
    use std::collections::BTreeSet;

    let token = make_regression_jwt();
    let graph_calls = Arc::new(AtomicUsize::new(0));
    let cnt = Arc::clone(&graph_calls);
    let payload_bytes = b"%PDF-1.7 cached offline attachment payload".to_vec();
    let payload_b64 = B64_STANDARD.encode(&payload_bytes);
    let b64_ref = payload_b64.clone();
    let payload_len = payload_bytes.len() as u64;

    let router = Router::new().route(
        "/v1.0/me/messages/msg-dl-off",
        get(move || {
            let c = Arc::clone(&cnt);
            let b64 = b64_ref.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "id": "msg-dl-off",
                    "parentFolderId": "folder-inbox",
                    "subject": "Offline DL",
                    "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                    "receivedDateTime": "2026-11-20T09:00:00Z",
                    "isRead": true,
                    "isDraft": false,
                    "hasAttachments": true,
                    "bodyPreview": "Files attached",
                    "body": { "contentType": "html", "content": "<p>Files attached</p>" },
                    "attachments": [
                        {
                            "id": "att-cached",
                            "name": "cached.pdf",
                            "contentType": "application/pdf",
                            "size": payload_len,
                            "isInline": false,
                            "contentBytes": b64
                        },
                        {
                            "id": "att-uncached",
                            "name": "remote-only.bin",
                            "contentType": "application/octet-stream",
                            "size": 1024,
                            "isInline": false
                        }
                    ]
                }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    let out_dir_str = out_dir.path().to_string_lossy().to_string();

    // Seed cache via compact read
    let seed = run_outlook_cli(&dir, &base_url, &token, &["read", "msg-dl-off", "--json"]);
    assert!(seed.status.success(), "{}", String::from_utf8_lossy(&seed.stderr));

    let calls_before = graph_calls.load(Ordering::SeqCst);

    // 1. Cached attachment offline download -> exit 0, flat 8 keys, 0 Graph calls
    let ok_dl = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "download",
            "msg-dl-off",
            "--attachment",
            "att-cached",
            "--offline",
            "--dir",
            &out_dir_str,
            "--json",
        ],
    );
    assert!(ok_dl.status.success(), "{}", String::from_utf8_lossy(&ok_dl.stderr));
    let ok_val: Value = serde_json::from_slice(&ok_dl.stdout).unwrap();
    let keys: BTreeSet<&str> = ok_val
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let expected_keys: BTreeSet<&str> = [
        "ok",
        "message_id",
        "attachment_id",
        "path",
        "file_name",
        "mime_type",
        "size_bytes",
        "is_image",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, expected_keys);
    assert_eq!(ok_val["ok"], true);
    assert_eq!(ok_val["size_bytes"], payload_len);
    assert_eq!(std::fs::read(ok_val["path"].as_str().unwrap()).unwrap(), payload_bytes);
    assert_eq!(graph_calls.load(Ordering::SeqCst), calls_before);

    // 2. Uncached attachment offline download -> exit 1, explicit error, 0 Graph calls
    let bad_dl = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "download",
            "msg-dl-off",
            "--attachment",
            "att-uncached",
            "--offline",
            "--dir",
            &out_dir_str,
            "--json",
        ],
    );
    assert_eq!(bad_dl.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad_dl.stderr)
        .contains("Cannot fetch uncached attachment bytes in offline mode"));
    assert_eq!(graph_calls.load(Ordering::SeqCst), calls_before);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_outlook_04_event_compact_default() {
    let token = make_regression_jwt();
    let router = Router::new().route(
        "/v1.0/me/events/{id}",
        get(|Path(id): Path<String>| async move {
            Json(json!({
                "id": id,
                "subject": "Architecture Review",
                "start": { "dateTime": "2026-11-23T14:00:00Z", "timeZone": "UTC" },
                "end": { "dateTime": "2026-11-23T15:00:00Z", "timeZone": "UTC" },
                "isAllDay": false,
                "location": { "displayName": "Room A" },
                "organizer": { "emailAddress": { "name": "Alice", "address": "alice@contoso.com" } },
                "attendees": [
                    {
                        "type": "required",
                        "status": { "response": "accepted" },
                        "emailAddress": { "name": "Bob", "address": "bob@contoso.com" }
                    },
                    {
                        "type": "optional",
                        "status": { "response": "tentativelyAccepted" },
                        "emailAddress": { "name": "Carol", "address": "carol@contoso.com" }
                    }
                ],
                "responseStatus": { "response": "organizer" },
                "isOnlineMeeting": true,
                "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/test" },
                "body": { "contentType": "html", "content": "<p>Review slides</p>" },
                "isCancelled": false,
                "webLink": "https://outlook.office.com/calendar/ev-compact-1"
            }))
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    let out_compact = run_outlook_cli(&dir, &base_url, &token, &["event", "ev-compact-1", "--json"]);
    assert!(out_compact.status.success(), "{}", String::from_utf8_lossy(&out_compact.stderr));
    let v_compact: Value = serde_json::from_slice(&out_compact.stdout).unwrap();
    let ev_c = v_compact["event"].as_object().unwrap();
    assert!(ev_c.get("body_html").is_none());
    assert_eq!(ev_c["attendees"].as_array().unwrap().len(), 2);
    assert_eq!(ev_c["omitted"]["fields"], json!(["body_html"]));
    assert_eq!(ev_c["hint"], "outlook event ev-compact-1 --json --full");

    let out_alias = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["event", "ev-compact-1", "--json", "--compact"],
    );
    assert!(out_alias.status.success());
    assert_eq!(out_alias.stdout, out_compact.stdout);

    let out_full = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &["event", "ev-compact-1", "--json", "--full"],
    );
    assert!(out_full.status.success(), "{}", String::from_utf8_lossy(&out_full.stderr));
    let v_full: Value = serde_json::from_slice(&out_full.stdout).unwrap();
    let ev_f = v_full["event"].as_object().unwrap();
    assert_eq!(ev_f["body_html"], "<p>Review slides</p>");
    assert_eq!(ev_f.len(), 17);
    assert!(ev_f.get("omitted").is_none());
    assert!(ev_f.get("hint").is_none());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_01_02_all_day_create_and_utc_normalized_calendar_times() {
    let token = make_regression_jwt();
    let created: Arc<RwLock<Vec<Value>>> = Arc::new(RwLock::new(Vec::new()));
    let created_for_route = created.clone();
    let router = Router::new()
        .route(
            "/v1.0/me/calendarView",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "ev-wire",
                            "subject": "Wire Format",
                            "start": { "dateTime": "2026-11-19T10:00:00.0000000", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-11-19T10:30:00.0000000", "timeZone": "UTC" },
                            "isAllDay": false
                        },
                        {
                            "id": "ev-z",
                            "subject": "Already RFC3339",
                            "start": { "dateTime": "2026-11-19T11:00:00Z", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-11-19T11:30:00Z", "timeZone": "UTC" },
                            "isAllDay": false
                        },
                        {
                            "id": "ev-all-day",
                            "subject": "Holiday",
                            "start": { "dateTime": "2026-11-20T00:00:00.0000000", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-11-21T00:00:00.0000000", "timeZone": "UTC" },
                            "isAllDay": true
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/me/events",
            post(move |Json(payload): Json<Value>| {
                let created = created_for_route.clone();
                async move {
                    if let Some(rejection) = graph_all_day_rejection(&payload) {
                        return rejection;
                    }
                    let id = {
                        let mut list = created.write().unwrap();
                        list.push(payload.clone());
                        format!("evt-new-{}", list.len())
                    };
                    Json(json!({
                        "id": id,
                        "subject": payload["subject"],
                        "start": graph_wire_time(&payload["start"]),
                        "end": graph_wire_time(&payload["end"]),
                        "isAllDay": payload["isAllDay"].as_bool().unwrap_or(false),
                        "responseStatus": { "response": "organizer" }
                    }))
                    .into_response()
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let dir = tempfile::tempdir().unwrap();

    // OUT-02: offset-less Graph UTC values are reported as RFC3339 UTC; all-day values stay floating.
    let listed = run_outlook_cli(&dir, &base_url, &token, &["calendar", "--days", "1", "--json"]);
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let by_id = |id: &str| {
        listed["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|ev| ev["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("missing {id} in {listed}"))
    };
    assert_eq!(by_id("ev-wire")["start_iso"], "2026-11-19T10:00:00Z");
    assert_eq!(by_id("ev-wire")["end_iso"], "2026-11-19T10:30:00Z");
    assert_eq!(by_id("ev-z")["start_iso"], "2026-11-19T11:00:00Z");
    assert_eq!(by_id("ev-all-day")["start_iso"], "2026-11-20T00:00:00.0000000");

    // OUT-01: an all-day create sends midnight boundaries (Graph answers 400 otherwise).
    let all_day = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "create-event", "--subject", "PTO", "--start", "2026-11-23", "--end", "2026-11-24",
            "--all-day", "--json",
        ],
    );
    assert!(all_day.status.success(), "{}", String::from_utf8_lossy(&all_day.stderr));
    let all_day: Value = serde_json::from_slice(&all_day.stdout).unwrap();
    assert_eq!(all_day["ok"], true);
    assert_eq!(all_day["event"]["is_all_day"], true);
    assert_eq!(all_day["event"]["start_iso"], "2026-11-23T00:00:00.0000000");
    {
        let sent = created.read().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["isAllDay"], true);
        assert_eq!(sent[0]["start"]["dateTime"], "2026-11-23T00:00:00");
        assert_eq!(sent[0]["end"]["dateTime"], "2026-11-24T00:00:00");
        assert_eq!(sent[0]["start"]["timeZone"], sent[0]["end"]["timeZone"]);
        assert!(!sent[0]["start"]["timeZone"].as_str().unwrap().is_empty());
    }

    // Invalid all-day input is rejected locally: non-zero exit, actionable message, no Graph call.
    for (start, end, needle) in [
        ("2026-11-23 09:00", "2026-11-24", "YYYY-MM-DD"),
        ("2026-11-23", "2026-11-23", "2026-11-24"),
    ] {
        let rejected = run_outlook_cli(
            &dir,
            &base_url,
            &token,
            &["create-event", "--subject", "PTO", "--start", start, "--end", end, "--all-day"],
        );
        assert!(!rejected.status.success());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.contains(needle), "{stderr}");
    }
    assert_eq!(created.read().unwrap().len(), 1);

    // Wall clock + --time-zone is sent unconverted in that zone; RFC3339 + --time-zone stays a UTC instant.
    let zoned = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "create-event", "--subject", "Standup", "--start", "2026-11-23 09:00", "--end",
            "2026-11-23 09:30", "--time-zone", "Asia/Seoul", "--json",
        ],
    );
    assert!(zoned.status.success(), "{}", String::from_utf8_lossy(&zoned.stderr));
    let instant = run_outlook_cli(
        &dir,
        &base_url,
        &token,
        &[
            "create-event", "--subject", "Instant", "--start", "2026-11-23T09:00:00+09:00",
            "--end", "2026-11-23T09:30:00+09:00", "--time-zone", "Asia/Seoul", "--json",
        ],
    );
    assert!(instant.status.success(), "{}", String::from_utf8_lossy(&instant.stderr));
    let instant: Value = serde_json::from_slice(&instant.stdout).unwrap();
    assert_eq!(instant["event"]["start_iso"], "2026-11-23T00:00:00Z");
    {
        let sent = created.read().unwrap();
        assert_eq!(
            sent[1]["start"],
            json!({ "dateTime": "2026-11-23T09:00:00", "timeZone": "Asia/Seoul" })
        );
        assert_eq!(
            sent[1]["end"],
            json!({ "dateTime": "2026-11-23T09:30:00", "timeZone": "Asia/Seoul" })
        );
        assert_eq!(
            sent[2]["start"],
            json!({ "dateTime": "2026-11-23T00:00:00Z", "timeZone": "UTC" })
        );
    }

    // MCP create_event follows the same contract.
    let cfg = test_config_for_server(&base_url);
    let auth = AuthManager::from_tokens(
        cfg.clone(),
        StoredTokens {
            access_token: token.clone(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("regression-user".into()),
            user_display_name: Some("Alice Outlook".into()),
            user_email: Some("alice@contoso.com".into()),
            auth_mode: "access_token".into(),
        },
    );
    let graph = GraphClient::new(cfg.clone(), auth).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (mcp_err, mcp_created) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "create_event",
        json!({
            "subject": "Offsite", "start": "2026-12-01", "end": "2026-12-03",
            "is_all_day": true, "time_zone": "Asia/Seoul"
        }),
    )
    .await;
    assert!(!mcp_err, "{mcp_created}");
    assert_eq!(mcp_created["event"]["is_all_day"], true);
    assert_eq!(
        created.read().unwrap()[3]["start"],
        json!({ "dateTime": "2026-12-01T00:00:00", "timeZone": "Asia/Seoul" })
    );
    let (mcp_bad_err, mcp_bad) = call_mcp_tool_json(
        &cfg,
        &graph,
        &storage,
        "create_event",
        json!({ "subject": "Bad", "start": "2026-12-01T09:00:00Z", "end": "2026-12-02", "is_all_day": true }),
    )
    .await;
    assert!(mcp_bad_err);
    assert!(mcp_bad.to_string().contains("YYYY-MM-DD"), "{mcp_bad}");
    assert_eq!(created.read().unwrap().len(), 4);

    server.abort();
}

#[test]
fn out_03_doctor_reports_scopes_the_token_is_missing() {
    // The regression JWT predates the directory scope: no User.ReadBasic.All, no Contacts.Read.
    let token = make_regression_jwt();
    let dir = tempfile::tempdir().unwrap();
    let out = run_outlook_cli(&dir, "http://127.0.0.1:9", &token, &["doctor", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let doctor: Value = serde_json::from_slice(&out.stdout).unwrap();

    let configured: Vec<&str> = doctor["configured_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(configured, outlook_cli::auth::DEFAULT_SCOPES);
    assert_eq!(
        doctor["missing_scopes"],
        json!(["User.ReadBasic.All", "Contacts.Read"])
    );

    let text = run_outlook_cli(&dir, "http://127.0.0.1:9", &token, &["doctor"]);
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        stdout.contains("Run 'outlook login' to grant: User.ReadBasic.All Contacts.Read"),
        "{stdout}"
    );
}
