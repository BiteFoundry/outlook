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
        "bodyPreview": "Hi Alice, here are the Q4 results",
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
                "start": { "dateTime": "2026-11-19T10:00:00Z", "timeZone": "UTC" },
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

async fn create_event_handler(
    State(st): State<Arc<MockGraphState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let expected = st.expected_token.read().unwrap().clone();
    if let Err(resp) = check_bearer(&headers, &expected) {
        return resp;
    }
    st.created_events.write().unwrap().push(payload.clone());
    Json(json!({
        "id": "evt-created-1",
        "subject": payload["subject"],
        "start": payload["start"],
        "end": payload["end"],
        "isAllDay": false,
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
                    let (cols, rows) = ov.target_dimensions(ui_state.term_width, ui_state.term_height);
                    let key = UiAppState::preview_image_cache_key(
                        cur.owner_id(),
                        cur.att_id(),
                        cols,
                        rows,
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
        .get(format!("{api_base}/v1/calendar/events?offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events_json["events"].as_array().unwrap().len(), 1);

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
    assert!(search_remote_text.contains("Remote KQL hit"));

    // 4. Self-updater mock GitHub release check (outlook-0.2.0-...) + mandatory SHA256SUMS.txt verification
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
    let versioned_asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "outlook-0.2.0-x86_64-pc-windows-msvc.zip".to_string(),
        ("windows", "aarch64") => "outlook-0.2.0-aarch64-pc-windows-msvc.zip".to_string(),
        ("macos", "x86_64") => "outlook-0.2.0-x86_64-apple-darwin.tar.gz".to_string(),
        ("macos", "aarch64") => "outlook-0.2.0-aarch64-apple-darwin.tar.gz".to_string(),
        ("linux", "x86_64") => "outlook-0.2.0-x86_64-unknown-linux-gnu.tar.gz".to_string(),
        ("linux", "aarch64") => "outlook-0.2.0-aarch64-unknown-linux-gnu.tar.gz".to_string(),
        _ => raw_bin_asset.clone(),
    };
    let new_binary = b"NEW-OUTLOOK-0.2.0-VERIFIED-BINARY".to_vec();
    let valid_digest = hex::encode(Sha256::digest(&new_binary));

    // 4a. Versioned release check (outlook-0.2.0-...)
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
                    "tag_name": "v0.2.0",
                    "html_url": format!("{base}/BiteFoundry/outlook/releases/tag/v0.2.0"),
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
    assert_eq!(check_res.latest_version, "0.2.0");
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
                    "tag_name": "v0.2.0",
                    "html_url": format!("{}/BiteFoundry/outlook/releases/tag/v0.2.0", st.base_url),
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
    assert_eq!(applied.latest_version, "0.2.0");
    assert_eq!(std::fs::read(&fake_exe).unwrap(), new_binary);

    gh_apply_server.abort();
    prot_server.abort();
    api_server.abort();
    graph_server.abort();
}
