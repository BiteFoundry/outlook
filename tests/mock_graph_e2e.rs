//! End-to-end integration tests against an in-process mock Microsoft Graph v1.0 + Entra OAuth server.

use std::{
    collections::HashMap,
    io::Cursor,
    sync::{Arc, Once, RwLock},
};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use image::{ImageBuffer, Rgba, RgbaImage};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;

use teams_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    img_preview, mcp,
    models::{
        AttachmentInfo, ConversationTarget, MentionInfo, MessageItem, OutgoingContact,
        OutgoingEvent, OutgoingMail, OutgoingTodoTask, TeamSummary,
    },
    parser, resolver, serve,
    storage::Storage,
    updater,
};

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();

fn ensure_isolated_env() {
    INIT_TEST_ENV.call_once(|| {
        let dir = tempfile::tempdir().expect("create shared test config tempdir");
        // Safety: initialized once before any AuthManager writes fallback credentials.
        std::env::set_var("TEAMS_DISABLE_KEYRING", "1");
        std::env::set_var("TEAMS_CONFIG_DIR", dir.path());
        std::env::remove_var("TEAMS_ACCESS_TOKEN");
        let _ = TEST_CONFIG_DIR.set(dir);
    });
}

#[derive(Debug, Clone)]
struct StoredChatMsg {
    id: String,
    reply_to_id: Option<String>,
    subject: Option<String>,
    from_id: String,
    from_name: String,
    created_at: String,
    body_html: String,
    mentions: Vec<Value>,
    attachments: Vec<Value>,
}

impl StoredChatMsg {
    fn to_graph_json(&self, replies: Option<Vec<Value>>) -> Value {
        let mut obj = json!({
            "id": self.id,
            "replyToId": self.reply_to_id,
            "messageType": "message",
            "createdDateTime": self.created_at,
            "subject": self.subject,
            "importance": "normal",
            "from": {
                "user": {
                    "id": self.from_id,
                    "displayName": self.from_name
                }
            },
            "body": {
                "contentType": "html",
                "content": self.body_html
            },
            "mentions": self.mentions,
            "attachments": self.attachments,
            "reactions": []
        });
        if let Some(r) = replies {
            obj["replies"] = Value::Array(r);
        }
        obj
    }
}

#[derive(Default)]
struct MockState {
    expected_token: RwLock<String>,
    refresh_calls: RwLock<usize>,
    me_auth_headers: RwLock<Vec<String>>,
    next_msg_seq: RwLock<u64>,
    chat_messages: RwLock<HashMap<String, Vec<StoredChatMsg>>>,
    channel_messages: RwLock<HashMap<(String, String), Vec<StoredChatMsg>>>,
    channel_replies: RwLock<HashMap<(String, String, String), Vec<StoredChatMsg>>>,
    uploaded_files: RwLock<HashMap<String, Vec<u8>>>,
    share_contents: RwLock<HashMap<String, Vec<u8>>>,
    posted_chat_payloads: RwLock<Vec<Value>>,
    posted_reply_payloads: RwLock<Vec<Value>>,
}

fn check_bearer_header(headers: &HeaderMap, expected: &str) -> Result<(), Response> {
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
                    "message": "Access token has expired or is not valid."
                }
            })),
        )
            .into_response())
    }
}

impl MockState {
    fn new(initial_token: &str) -> Self {
        let state = Self::default();
        *state.expected_token.write().unwrap() = initial_token.to_string();
        *state.next_msg_seq.write().unwrap() = 100;
        state
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut seq = self.next_msg_seq.write().unwrap();
        *seq += 1;
        format!("{prefix}-{}", *seq)
    }

    fn check_bearer(&self, headers: &HeaderMap) -> Result<(), Response> {
        let expected = self.expected_token.read().unwrap().clone();
        check_bearer_header(headers, &expected)
    }
}

fn mock_teams_payload() -> Vec<Value> {
    vec![json!({
        "id": "team-dev",
        "displayName": "개발팀",
        "description": "Engineering Team"
    })]
}

fn mock_channels_payload() -> Vec<Value> {
    vec![
        json!({
            "id": "chan-general",
            "displayName": "일반",
            "description": "General channel",
            "membershipType": "standard"
        }),
        json!({
            "id": "chan-deploy",
            "displayName": "배포공지",
            "description": "Release announcements",
            "membershipType": "standard"
        }),
    ]
}

fn mock_chats_payload() -> Vec<Value> {
    vec![
        json!({
            "id": "19:oneonone-peer@unq.gbl.spaces",
            "chatType": "oneOnOne",
            "lastUpdatedDateTime": "2026-11-15T05:00:00Z",
            "members": [
                { "userId": "me-user-1", "displayName": "나(테스터)" },
                { "userId": "peer-kim", "displayName": "김철수" }
            ],
            "viewpoint": {
                "lastMessageReadDateTime": "2026-11-15T04:00:00Z"
            },
            "lastMessagePreview": {
                "id": "seed-1",
                "createdDateTime": "2026-11-15T05:00:00Z",
                "from": { "user": { "id": "peer-kim", "displayName": "김철수" } },
                "body": { "contentType": "html", "content": "<p>안녕하세요</p>" }
            }
        }),
        json!({
            "id": "19:group-sprint@thread.v2",
            "chatType": "group",
            "topic": "백엔드 스프린트",
            "lastUpdatedDateTime": "2026-11-15T05:10:00Z",
            "members": [
                { "userId": "me-user-1", "displayName": "나(테스터)" },
                { "userId": "peer-kim", "displayName": "김철수" },
                { "userId": "peer-lee", "displayName": "이영희" }
            ]
        }),
    ]
}

async fn oauth_token_handler(
    State(state): State<Arc<MockState>>,
    body: String,
) -> impl IntoResponse {
    assert!(
        body.contains("grant_type=refresh_token"),
        "expected refresh_token grant, got: {body}"
    );
    assert!(
        body.contains("refresh_token=stale-refresh-tok"),
        "expected stored refresh_token in body: {body}"
    );

    *state.refresh_calls.write().unwrap() += 1;
    let new_tok = "fresh-access-tok-2026";
    *state.expected_token.write().unwrap() = new_tok.to_string();

    Json(json!({
        "token_type": "Bearer",
        "expires_in": 3600,
        "access_token": new_tok,
        "refresh_token": "rotated-refresh-tok"
    }))
}

async fn get_me_handler(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    if let Some(auth_val) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        state
            .me_auth_headers
            .write()
            .unwrap()
            .push(auth_val.to_string());
    }
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }

    Json(json!({
        "id": "me-user-1",
        "displayName": "나(테스터)",
        "mail": "tester@contoso.com",
        "userPrincipalName": "tester@contoso.com"
    }))
    .into_response()
}

async fn list_chats_handler(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({ "value": mock_chats_payload() })).into_response()
}

async fn list_chat_messages_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let map = state.chat_messages.read().unwrap();
    let msgs: Vec<Value> = map
        .get(&chat_id)
        .map(|list| list.iter().map(|m| m.to_graph_json(None)).collect())
        .unwrap_or_default();
    Json(json!({ "value": msgs })).into_response()
}

async fn post_chat_message_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.posted_chat_payloads.write().unwrap().push(payload.clone());

    let id = state.next_id("chat-msg");
    let stored = StoredChatMsg {
        id: id.clone(),
        reply_to_id: None,
        subject: payload
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        from_id: "me-user-1".to_string(),
        from_name: "나(테스터)".to_string(),
        created_at: "2026-11-15T06:00:00Z".to_string(),
        body_html: payload
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mentions: payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        attachments: payload
            .get("attachments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let json_resp = stored.to_graph_json(None);
    state
        .chat_messages
        .write()
        .unwrap()
        .entry(chat_id)
        .or_default()
        .push(stored);

    (StatusCode::CREATED, Json(json_resp)).into_response()
}

async fn joined_teams_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({ "value": mock_teams_payload() })).into_response()
}

async fn batch_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let reqs = payload
        .get("requests")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut responses = Vec::with_capacity(reqs.len());
    for req in reqs {
        let req_id = req.get("id").and_then(|v| v.as_str()).unwrap_or("0");
        let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.contains("/teams/team-dev/channels") {
            responses.push(json!({
                "id": req_id,
                "status": 200,
                "body": { "value": mock_channels_payload() }
            }));
        } else {
            responses.push(json!({
                "id": req_id,
                "status": 404,
                "body": { "error": { "code": "NotFound", "message": "not found" } }
            }));
        }
    }
    Json(json!({ "responses": responses })).into_response()
}

async fn list_channel_messages_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let msgs_guard = state.channel_messages.read().unwrap();
    let replies_guard = state.channel_replies.read().unwrap();
    let list = msgs_guard
        .get(&(team_id.clone(), channel_id.clone()))
        .cloned()
        .unwrap_or_default();
    let values: Vec<Value> = list
        .into_iter()
        .map(|m| {
            let reply_stubs: Vec<Value> = replies_guard
                .get(&(team_id.clone(), channel_id.clone(), m.id.clone()))
                .map(|rs| rs.iter().map(|r| json!({ "id": r.id })).collect())
                .unwrap_or_default();
            m.to_graph_json(Some(reply_stubs))
        })
        .collect();
    Json(json!({ "value": values })).into_response()
}

async fn post_channel_message_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id)): Path<(String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let id = state.next_id("chan-root");
    let stored = StoredChatMsg {
        id: id.clone(),
        reply_to_id: None,
        subject: payload
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        from_id: "me-user-1".to_string(),
        from_name: "나(테스터)".to_string(),
        created_at: "2026-11-15T06:10:00Z".to_string(),
        body_html: payload
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mentions: payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        attachments: payload
            .get("attachments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let resp_json = stored.to_graph_json(Some(vec![]));
    state
        .channel_messages
        .write()
        .unwrap()
        .entry((team_id, channel_id))
        .or_default()
        .push(stored);

    (StatusCode::CREATED, Json(resp_json)).into_response()
}

async fn get_channel_root_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let msgs_guard = state.channel_messages.read().unwrap();
    let found = msgs_guard
        .get(&(team_id.clone(), channel_id.clone()))
        .and_then(|list| list.iter().find(|m| m.id == root_id))
        .cloned();
    match found {
        Some(m) => Json(m.to_graph_json(None)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NotFound", "message": "missing root" } }))).into_response(),
    }
}

async fn list_channel_replies_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let guard = state.channel_replies.read().unwrap();
    let replies: Vec<Value> = guard
        .get(&(team_id, channel_id, root_id))
        .map(|list| list.iter().map(|r| r.to_graph_json(None)).collect())
        .unwrap_or_default();
    Json(json!({ "value": replies })).into_response()
}

async fn post_channel_reply_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((team_id, channel_id, root_id)): Path<(String, String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.posted_reply_payloads.write().unwrap().push(payload.clone());

    let id = state.next_id("chan-reply");
    let stored = StoredChatMsg {
        id: id.clone(),
        reply_to_id: Some(root_id.clone()),
        subject: None,
        from_id: "me-user-1".to_string(),
        from_name: "나(테스터)".to_string(),
        created_at: "2026-11-15T06:15:00Z".to_string(),
        body_html: payload
            .pointer("/body/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        mentions: payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        attachments: payload
            .get("attachments")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    let resp_json = stored.to_graph_json(None);
    state
        .channel_replies
        .write()
        .unwrap()
        .entry((team_id, channel_id, root_id))
        .or_default()
        .push(stored);

    (StatusCode::CREATED, Json(resp_json)).into_response()
}

async fn chat_drive_upload_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(rest): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let file_name = rest
        .trim_end_matches(":/content")
        .rsplit('/')
        .next()
        .unwrap_or("uploaded.bin")
        .to_string();
    let item_id = "drive-item-chat-1".to_string();
    let share_url = format!("https://contoso.sharepoint.com/personal/tester/Documents/Microsoft%20Teams%20Chat%20Files/{file_name}");

    state
        .uploaded_files
        .write()
        .unwrap()
        .insert(item_id.clone(), body.to_vec());
    state
        .share_contents
        .write()
        .unwrap()
        .insert(share_url.clone(), body.to_vec());

    Json(json!({
        "id": item_id,
        "name": file_name,
        "size": body.len(),
        "eTag": "\"{153FA47D-18C9-4179-BE08-9879815A9F90},1\"",
        "webUrl": share_url,
        "webDavUrl": share_url
    }))
    .into_response()
}

async fn create_link_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(_item_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let shared_url = "https://contoso.sharepoint.com/:t:/s/chat/Eabc123";
    // Copy the uploaded chat file bytes under the created sharing link too
    if let Some(bytes) = state.uploaded_files.read().unwrap().get("drive-item-chat-1").cloned() {
        state
            .share_contents
            .write()
            .unwrap()
            .insert(shared_url.to_string(), bytes);
    }
    Json(json!({
        "link": { "webUrl": shared_url },
        "webUrl": shared_url
    }))
    .into_response()
}

async fn channel_files_folder_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((_team_id, _channel_id)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({
        "id": "folder-deploy-1",
        "name": "배포공지",
        "parentReference": {
            "driveId": "drive-sp-dev"
        }
    }))
    .into_response()
}

async fn sp_drive_upload_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path((drive_id, rest)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let file_name = rest
        .trim_end_matches(":/content")
        .rsplit('/')
        .next()
        .unwrap_or("channel_file.bin")
        .to_string();
    let item_id = format!("sp-item-{drive_id}-1");
    let web_url = format!("https://contoso.sharepoint.com/sites/dev/Shared%20Documents/{file_name}");

    state
        .uploaded_files
        .write()
        .unwrap()
        .insert(item_id.clone(), body.to_vec());
    state
        .share_contents
        .write()
        .unwrap()
        .insert(web_url.clone(), body.to_vec());

    Json(json!({
        "id": item_id,
        "name": file_name,
        "size": body.len(),
        "eTag": "\"{223FA47D-18C9-4179-BE08-9879815A9F91},1\"",
        "webUrl": web_url,
        "webDavUrl": web_url
    }))
    .into_response()
}

async fn download_share_content_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(share_token): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let Some(encoded) = share_token.strip_prefix("u!") else {
        return (StatusCode::BAD_REQUEST, "missing u! prefix").into_response();
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let Ok(decoded_bytes) = URL_SAFE_NO_PAD.decode(encoded) else {
        return (StatusCode::BAD_REQUEST, "bad share token").into_response();
    };
    let Ok(decoded_url) = String::from_utf8(decoded_bytes) else {
        return (StatusCode::BAD_REQUEST, "non-utf8 share url").into_response();
    };

    let guard = state.share_contents.read().unwrap();
    match guard.get(&decoded_url) {
        Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, format!("unknown share: {decoded_url}")).into_response(),
    }
}

async fn spawn_mock_graph_server(initial_token: &str) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState::new(initial_token));
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(oauth_token_handler))
        .route("/v1.0/me", get(get_me_handler))
        .route("/v1.0/me/chats", get(list_chats_handler))
        .route(
            "/v1.0/chats/{chat_id}/messages",
            get(list_chat_messages_handler).post(post_chat_message_handler),
        )
        .route("/v1.0/me/joinedTeams", get(joined_teams_handler))
        .route("/v1.0/$batch", post(batch_handler))
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages",
            get(list_channel_messages_handler).post(post_channel_message_handler),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}",
            get(get_channel_root_handler),
        )
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/messages/{root_id}/replies",
            get(list_channel_replies_handler).post(post_channel_reply_handler),
        )
        .route("/v1.0/me/drive/root:/{*rest}", put(chat_drive_upload_handler))
        .route("/v1.0/me/drive/items/{item_id}/createLink", post(create_link_handler))
        .route(
            "/v1.0/teams/{team_id}/channels/{channel_id}/filesFolder",
            get(channel_files_folder_handler),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{*rest}",
            put(sp_drive_upload_handler),
        )
        .route(
            "/v1.0/shares/{share_token}/driveItem/content",
            get(download_share_content_handler),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Graph server");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock Graph");
    });

    (format!("http://{addr}"), state)
}

fn build_test_config(base_url: &str, download_dir: &std::path::Path) -> AppConfig {
    AppConfig {
        client_id: "test-client-id".to_string(),
        tenant_id: "test-tenant".to_string(),
        graph_base_url: format!("{base_url}/v1.0"),
        auth_base_url: base_url.to_string(),
        download_dir: download_dir.to_string_lossy().to_string(),
        ..AppConfig::default()
    }
}

#[tokio::test]
async fn e2e_mock_graph_auth_chats_channels_files_and_agent_surfaces() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");

    let (base_url, mock) = spawn_mock_graph_server("fresh-access-tok-2026").await;
    let cfg = build_test_config(&base_url, &download_dir);

    // -----------------------------------------------------------------------
    // 1. Token auto-refresh: expired StoredTokens refreshes before calling /me
    // -----------------------------------------------------------------------
    let expired_tokens = StoredTokens {
        access_token: "expired-access-tok".to_string(),
        refresh_token: Some("stale-refresh-tok".to_string()),
        expires_at: chrono::Utc::now().timestamp() - 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: None,
        user_display_name: None,
        user_email: None,
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), expired_tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone())
        .expect("build GraphClient")
        .with_my_user_id("me-user-1");

    let me = graph.get_me().await.expect("get_me after auto-refresh");
    assert_eq!(me.id, "me-user-1");
    assert_eq!(me.display_name, "나(테스터)");
    assert_eq!(*mock.refresh_calls.read().unwrap(), 1);
    assert_eq!(
        mock.me_auth_headers.read().unwrap().as_slice(),
        &["Bearer fresh-access-tok-2026".to_string()]
    );
    let refreshed_state = auth.current_tokens().await.expect("tokens present");
    assert_eq!(refreshed_state.access_token, "fresh-access-tok-2026");
    assert_eq!(
        refreshed_state.refresh_token.as_deref(),
        Some("rotated-refresh-tok")
    );

    // -----------------------------------------------------------------------
    // 2. 1:1 & Group Chats + Markdown/@mention send + SQLite FTS5 Korean/EN search
    // -----------------------------------------------------------------------
    let storage = Storage::open(&db_path).expect("open SQLite storage");
    let chats = graph.list_chats(50).await.expect("list_chats");
    assert_eq!(chats.len(), 2);
    storage.upsert_conversations(&chats).expect("upsert chats");

    let one_on_one = chats
        .iter()
        .find(|c| c.conv_key == "chat:19:oneonone-peer@unq.gbl.spaces")
        .expect("1:1 chat present");
    assert_eq!(one_on_one.title, "김철수");
    assert_eq!(one_on_one.peer_user_id.as_deref(), Some("peer-kim"));

    let group_chat = chats
        .iter()
        .find(|c| c.conv_key == "chat:19:group-sprint@thread.v2")
        .expect("group chat present");
    assert_eq!(group_chat.title, "백엔드 스프린트");

    let outgoing_chat_msg = parser::compile_outgoing_markdown(
        "안녕하세요 @김철수 님! **Kubernetes** 롤아웃 점검 완료했습니다.",
        None,
        vec![MentionInfo {
            id: 0,
            user_id: "peer-kim".to_string(),
            display_name: "김철수".to_string(),
        }],
        vec![],
        vec![],
        None,
    );
    assert!(outgoing_chat_msg.body_html.contains("<at id=\"0\">김철수</at>"));
    assert!(outgoing_chat_msg
        .body_html
        .contains("<strong>Kubernetes</strong>"));

    let sent_chat = graph
        .send_message(&one_on_one.target, &outgoing_chat_msg)
        .await
        .expect("send chat message");
    assert!(sent_chat.is_me);

    let fetched_chat_msgs = graph
        .list_chat_messages("19:oneonone-peer@unq.gbl.spaces", 20)
        .await
        .expect("list_chat_messages");
    assert_eq!(fetched_chat_msgs.len(), 1);
    storage
        .upsert_messages(&fetched_chat_msgs)
        .expect("upsert chat messages");

    let ko_hits = storage
        .search_messages("롤아웃", None, 10)
        .expect("search Korean");
    assert_eq!(ko_hits.len(), 1);
    assert_eq!(ko_hits[0].conv_key, one_on_one.conv_key);
    assert!(ko_hits[0].snippet.contains("롤아웃"));

    let en_hits = storage
        .search_messages("Kubernetes", Some(&one_on_one.conv_key), 10)
        .expect("search English");
    assert_eq!(en_hits.len(), 1);
    assert_eq!(en_hits[0].msg_id, sent_chat.msg_id);

    // -----------------------------------------------------------------------
    // 3. Team -> Channel -> Thread: resolve "개발팀/배포공지", post with subject,
    //    reply in thread, fetch get_thread, verify reply_count in Storage
    // -----------------------------------------------------------------------
    let teams: Vec<TeamSummary> = graph
        .list_joined_teams_with_channels()
        .await
        .expect("list_joined_teams_with_channels");
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0].display_name, "개발팀");
    assert_eq!(teams[0].channels.len(), 2);
    storage.upsert_teams(&teams).expect("upsert teams");

    let all_convs = storage.list_conversations().expect("list_conversations");
    let deploy_chan =
        resolver::resolve_from_slice(&all_convs, "개발팀/배포공지").expect("resolve channel");
    assert_eq!(
        deploy_chan.target,
        ConversationTarget::Channel {
            team_id: "team-dev".to_string(),
            channel_id: "chan-deploy".to_string(),
        }
    );

    let chan_post = parser::compile_outgoing_markdown(
        "프로덕션 v2.4.0 배포 시작합니다.",
        Some("v2.4.0 배포 공지".to_string()),
        vec![],
        vec![],
        vec![],
        None,
    );
    let root_msg = graph
        .send_message(&deploy_chan.target, &chan_post)
        .await
        .expect("send channel root post");
    assert_eq!(root_msg.subject.as_deref(), Some("v2.4.0 배포 공지"));

    let chan_reply = parser::compile_outgoing_markdown(
        "카나리 헬스체크 정상 확인했습니다.",
        None,
        vec![],
        vec![],
        vec![],
        None,
    );
    let reply_msg = graph
        .reply_message(&deploy_chan.target, &root_msg.msg_id, &chan_reply)
        .await
        .expect("reply to channel thread");
    assert_eq!(
        reply_msg.parent_msg_id.as_deref(),
        Some(root_msg.msg_id.as_str())
    );

    let thread = graph
        .get_thread("team-dev", "chan-deploy", &root_msg.msg_id, 50)
        .await
        .expect("get_thread");
    assert_eq!(thread.root.msg_id, root_msg.msg_id);
    assert_eq!(thread.replies.len(), 1);
    assert_eq!(thread.root.reply_count, 1);

    let mut thread_bundle = vec![thread.root.clone()];
    thread_bundle.extend(thread.replies.iter().cloned());
    storage
        .upsert_messages(&thread_bundle)
        .expect("persist thread in storage");

    let stored_root = storage
        .get_message(&deploy_chan.conv_key, &root_msg.msg_id)
        .expect("get stored root")
        .expect("root exists in storage");
    assert_eq!(stored_root.reply_count, 1);

    let stored_thread = storage
        .get_thread(&deploy_chan.conv_key, &root_msg.msg_id, 50)
        .expect("get_thread from storage")
        .expect("thread exists in storage");
    assert_eq!(stored_thread.replies.len(), 1);

    // -----------------------------------------------------------------------
    // 4. File Attachments: Chat send_file, Channel Thread send_file, download_attachment
    // -----------------------------------------------------------------------
    let chat_file_path = temp.path().join("chat-spec.txt");
    let chat_file_bytes = b"Teams Chat Upload Payload \xed\x95\x9c\xea\xb8\x80";
    std::fs::write(&chat_file_path, chat_file_bytes).expect("write chat test file");

    let sent_chat_file = graph
        .send_file(&one_on_one.target, &chat_file_path, Some("<p>채팅 첨부</p>"), None)
        .await
        .expect("send_file to chat");
    assert_eq!(sent_chat_file.attachments.len(), 1);
    assert_eq!(sent_chat_file.attachments[0].content_type, "reference");
    assert!(sent_chat_file
        .body_html
        .contains("<attachment id=\"153fa47d-18c9-4179-be08-9879815a9f90\"></attachment>"));

    // Verify wire payload posted to POST /chats/{id}/messages
    let chat_wire = mock
        .posted_chat_payloads
        .read()
        .unwrap()
        .last()
        .cloned()
        .expect("posted chat payload");
    let wire_body = chat_wire["body"]["content"].as_str().unwrap_or("");
    assert!(wire_body.contains("<attachment id=\"153fa47d-18c9-4179-be08-9879815a9f90\">"));
    assert_eq!(chat_wire["attachments"][0]["contentType"], "reference");

    // Channel Thread send_file
    let chan_file_path = temp.path().join("release-notes.md");
    let chan_file_bytes = b"# Release v2.4.0\nAll checks passed.\n";
    std::fs::write(&chan_file_path, chan_file_bytes).expect("write channel test file");

    let sent_chan_file = graph
        .send_file(
            &deploy_chan.target,
            &chan_file_path,
            Some("<p>배포 노트 첨부</p>"),
            Some(&root_msg.msg_id),
        )
        .await
        .expect("send_file to channel thread");
    assert_eq!(sent_chan_file.attachments.len(), 1);
    assert_eq!(sent_chan_file.attachments[0].content_type, "reference");
    assert_eq!(
        sent_chan_file.parent_msg_id.as_deref(),
        Some(root_msg.msg_id.as_str())
    );
    let reply_wire = mock
        .posted_reply_payloads
        .read()
        .unwrap()
        .last()
        .cloned()
        .expect("posted reply payload");
    assert_eq!(reply_wire["attachments"][0]["contentType"], "reference");
    assert!(reply_wire["body"]["content"]
        .as_str()
        .unwrap_or("")
        .contains("<attachment id=\"223fa47d-18c9-4179-be08-9879815a9f91\">"));

    // download_attachment via /shares/u!.../driveItem/content
    let chat_att: &AttachmentInfo = &sent_chat_file.attachments[0];
    let downloaded = graph
        .download_attachment(chat_att, &download_dir)
        .await
        .expect("download_attachment");
    assert!(downloaded.path.exists());
    assert_eq!(
        std::fs::read(&downloaded.path).expect("read downloaded file"),
        chat_file_bytes
    );

    // -----------------------------------------------------------------------
    // 5. Agent Surfaces: mcp::handle_rpc_line & serve::build_router
    // -----------------------------------------------------------------------
    let init_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0.1"}}}"#,
    )
    .await
    .expect("mcp initialize response");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "teams");

    let tools_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await
    .expect("mcp tools/list response");
    let tool_names: Vec<&str> = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tool_names.contains(&"list_chats"));
    assert!(tool_names.contains(&"get_thread"));
    assert!(tool_names.contains(&"delete_message"));

    let list_chats_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "list_chats",
            "arguments": { "refresh": true }
        }
    });
    let list_chats_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &list_chats_req.to_string())
        .await
        .expect("mcp list_chats response");
    assert_ne!(list_chats_resp["result"]["isError"], Value::Bool(true));
    let chats_text = list_chats_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("list_chats text");
    assert!(chats_text.contains("김철수"));
    assert!(chats_text.contains("백엔드 스프린트"));

    let get_thread_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "get_thread",
            "arguments": {
                "target": "개발팀/배포공지",
                "root_msg_id": root_msg.msg_id
            }
        }
    });
    let get_thread_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &get_thread_req.to_string())
        .await
        .expect("mcp get_thread response");
    assert_ne!(get_thread_resp["result"]["isError"], Value::Bool(true));
    let thread_text = get_thread_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("get_thread text");
    assert!(thread_text.contains("v2.4.0 배포 공지"));
    assert!(thread_text.contains("카나리 헬스체크 정상 확인했습니다."));

    // delete_message without confirm: true must be rejected
    let delete_missing_confirm = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "delete_message",
            "arguments": {
                "target": "개발팀/배포공지",
                "msg_id": root_msg.msg_id
            }
        }
    });
    let del_err_resp =
        mcp::handle_rpc_line(&cfg, &graph, &storage, &delete_missing_confirm.to_string())
            .await
            .expect("mcp delete_message rejection");
    assert_eq!(del_err_resp["result"]["isError"], Value::Bool(true));

    let delete_false_confirm = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "delete_message",
            "arguments": {
                "target": "개발팀/배포공지",
                "msg_id": root_msg.msg_id,
                "confirm": false
            }
        }
    });
    let del_false_resp =
        mcp::handle_rpc_line(&cfg, &graph, &storage, &delete_false_confirm.to_string())
            .await
            .expect("mcp delete_message false rejection");
    assert_eq!(del_false_resp["result"]["isError"], Value::Bool(true));

    // HTTP JSON API (`serve::build_router`)
    let router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        Some("e2e-secret".to_string()),
    );
    let serve_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind serve listener");
    let serve_addr = serve_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(serve_listener, router)
            .await
            .expect("serve HTTP API");
    });

    let http = reqwest::Client::new();
    let serve_base = format!("http://{serve_addr}");

    let health_res = http
        .get(format!("{serve_base}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(health_res.status(), StatusCode::OK);
    let health_json: Value = health_res.json().await.unwrap();
    assert_eq!(health_json["ok"], true);
    assert_eq!(health_json["user_id"], "me-user-1");

    let unauth_res = http
        .get(format!("{serve_base}/conversations"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_res.status(), StatusCode::UNAUTHORIZED);

    let convs_res = http
        .get(format!("{serve_base}/conversations"))
        .bearer_auth("e2e-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(convs_res.status(), StatusCode::OK);
    let convs_json: Value = convs_res.json().await.unwrap();
    assert!(convs_json["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["title"] == "배포공지"));

    let search_res = http
        .get(format!(
            "{serve_base}/search?q={}",
            urlencoding::encode("롤아웃")
        ))
        .bearer_auth("e2e-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(search_res.status(), StatusCode::OK);
    let search_json: Value = search_res.json().await.unwrap();
    assert_eq!(search_json["hits"].as_array().unwrap().len(), 1);

    let thread_res = http
        .get(format!(
            "{serve_base}/conversations/{}/threads/{}",
            urlencoding::encode(&deploy_chan.conv_key),
            urlencoding::encode(&root_msg.msg_id)
        ))
        .bearer_auth("e2e-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(thread_res.status(), StatusCode::OK);
    let thread_json: Value = thread_res.json().await.unwrap();
    assert_eq!(thread_json["thread"]["root"]["msg_id"], root_msg.msg_id);
    assert_eq!(
        thread_json["thread"]["replies"].as_array().unwrap().len(),
        2
    );

    let _: Option<MessageItem> = storage
        .get_message(&deploy_chan.conv_key, &root_msg.msg_id)
        .unwrap();
}

// ===========================================================================
// v0.2.0 Outlook (Mail / Calendar / Contacts / To Do), Image Preview & Updater E2E
// ===========================================================================

struct OutlookMockState {
    token: String,
    mail_messages: RwLock<Vec<Value>>,
    mail_attachments: RwLock<HashMap<(String, String), Value>>,
    sent_mails: RwLock<Vec<Value>>,
    mail_replies: RwLock<Vec<(String, Value)>>,
    calendar_events: RwLock<Vec<Value>>,
    calendar_rsvps: RwLock<Vec<(String, String, Value)>>,
    contacts: RwLock<Vec<Value>>,
    directory_users: RwLock<Vec<Value>>,
    todo_lists: RwLock<Vec<Value>>,
    todo_tasks: RwLock<HashMap<String, Vec<Value>>>,
}

impl OutlookMockState {
    fn new(token: &str) -> Self {
        let att_b64 = BASE64_STANDARD.encode(b"Outlook Attachment Payload \xed\x95\x9c\xea\xb8\x80");
        let att_obj = json!({
            "id": "att-1",
            "name": "architecture-note.txt",
            "contentType": "text/plain",
            "size": 34,
            "isInline": false,
            "contentBytes": att_b64
        });
        let mut mail_attachments = HashMap::new();
        mail_attachments.insert(("mail-1".to_string(), "att-1".to_string()), att_obj.clone());

        let mail_messages = vec![
            json!({
                "id": "mail-1",
                "conversationId": "conv-mail-1",
                "parentFolderId": "folder-inbox",
                "subject": "아키텍처 리뷰 및 Kubernetes 결재 요청",
                "from": {
                    "emailAddress": { "name": "김철수", "address": "kim@contoso.com" }
                },
                "toRecipients": [
                    { "emailAddress": { "name": "나(테스터)", "address": "tester@contoso.com" } }
                ],
                "receivedDateTime": "2026-11-15T08:00:00Z",
                "sentDateTime": "2026-11-15T07:59:50Z",
                "isRead": false,
                "isDraft": false,
                "flag": { "flagStatus": "notFlagged" },
                "importance": "high",
                "hasAttachments": true,
                "bodyPreview": "아키텍처 문서와 Kubernetes 배포 일정 확인 부탁드립니다.",
                "body": {
                    "contentType": "html",
                    "content": "<p>안녕하세요, <strong>아키텍처</strong> 문서와 <em>Kubernetes</em> 배포 결재 요청드립니다.</p>"
                },
                "attachments": [att_obj],
                "webLink": "https://outlook.office.com/mail/id/mail-1"
            }),
            json!({
                "id": "mail-2",
                "conversationId": "conv-mail-2",
                "parentFolderId": "folder-inbox",
                "subject": "Weekly Sprint Sync",
                "from": {
                    "emailAddress": { "name": "Alice", "address": "alice@contoso.com" }
                },
                "toRecipients": [
                    { "emailAddress": { "name": "나(테스터)", "address": "tester@contoso.com" } }
                ],
                "receivedDateTime": "2026-11-15T07:00:00Z",
                "isRead": true,
                "isDraft": false,
                "flag": { "flagStatus": "notFlagged" },
                "importance": "normal",
                "hasAttachments": false,
                "bodyPreview": "Sprint sync notes inside.",
                "body": { "contentType": "text", "content": "Sprint sync notes inside." }
            }),
        ];

        let calendar_events = vec![json!({
            "id": "ev-seed-1",
            "subject": "주간 스탠드업",
            "start": { "dateTime": "2026-11-16T01:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-11-16T01:30:00", "timeZone": "UTC" },
            "isAllDay": false,
            "location": { "displayName": "본사 3층" },
            "organizer": { "emailAddress": { "name": "김철수", "address": "kim@contoso.com" } },
            "attendees": [{
                "emailAddress": { "name": "나(테스터)", "address": "tester@contoso.com" },
                "type": "required",
                "status": { "response": "notResponded" }
            }],
            "responseStatus": { "response": "notResponded" },
            "isOnlineMeeting": false
        })];

        let contacts = vec![json!({
            "id": "contact-1",
            "displayName": "김철수 (개인)",
            "givenName": "철수",
            "surname": "김",
            "emailAddresses": [{ "address": "kim@contoso.com", "name": "김철수" }],
            "mobilePhone": "010-1111-2222",
            "companyName": "Contoso",
            "jobTitle": "Staff Engineer",
            "department": "Platform"
        })];

        let directory_users = vec![json!({
            "id": "dir-user-1",
            "displayName": "김영희 (조직)",
            "givenName": "영희",
            "surname": "김",
            "mail": "younghee.kim@contoso.com",
            "userPrincipalName": "younghee.kim@contoso.com",
            "companyName": "Contoso",
            "jobTitle": "Product Manager",
            "department": "Core"
        })];

        let todo_lists = vec![json!({
            "id": "list-default",
            "displayName": "Tasks",
            "wellknownListName": "defaultList",
            "isOwner": true,
            "isShared": false
        })];

        let mut todo_tasks = HashMap::new();
        todo_tasks.insert(
            "list-default".to_string(),
            vec![json!({
                "id": "task-seed-1",
                "title": "기존 할 일",
                "status": "notStarted",
                "importance": "normal"
            })],
        );

        Self {
            token: token.to_string(),
            mail_messages: RwLock::new(mail_messages),
            mail_attachments: RwLock::new(mail_attachments),
            sent_mails: RwLock::new(Vec::new()),
            mail_replies: RwLock::new(Vec::new()),
            calendar_events: RwLock::new(calendar_events),
            calendar_rsvps: RwLock::new(Vec::new()),
            contacts: RwLock::new(contacts),
            directory_users: RwLock::new(directory_users),
            todo_lists: RwLock::new(todo_lists),
            todo_tasks: RwLock::new(todo_tasks),
        }
    }

    fn check_bearer(&self, headers: &HeaderMap) -> Result<(), Response> {
        check_bearer_header(headers, &self.token)
    }
}

async fn outlook_mail_folders_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({
        "value": [
            {
                "id": "folder-inbox",
                "displayName": "받은 편지함",
                "unreadItemCount": 1,
                "totalItemCount": 2
            },
            {
                "id": "folder-sent",
                "displayName": "보낸 편지함",
                "unreadItemCount": 0,
                "totalItemCount": 0
            }
        ]
    }))
    .into_response()
}

async fn outlook_folder_messages_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path(folder): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let msgs = state.mail_messages.read().unwrap();
    let filtered: Vec<Value> = msgs
        .iter()
        .filter(|m| {
            folder.eq_ignore_ascii_case("inbox")
                && m["parentFolderId"].as_str() == Some("folder-inbox")
                || m["parentFolderId"].as_str() == Some(folder.as_str())
        })
        .cloned()
        .collect();
    Json(json!({ "value": filtered })).into_response()
}

async fn outlook_get_message_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path(msg_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let guard = state.mail_messages.read().unwrap();
    match guard.iter().find(|m| m["id"].as_str() == Some(msg_id.as_str())) {
        Some(m) => Json(m.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NotFound", "message": "mail not found" } }))).into_response(),
    }
}

async fn outlook_patch_message_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path(msg_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let mut guard = state.mail_messages.write().unwrap();
    let Some(msg) = guard
        .iter_mut()
        .find(|m| m["id"].as_str() == Some(msg_id.as_str()))
    else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NotFound", "message": "mail not found" } }))).into_response();
    };
    if let Some(is_read) = payload.get("isRead").and_then(|v| v.as_bool()) {
        msg["isRead"] = Value::Bool(is_read);
    }
    if let Some(flag_status) = payload.pointer("/flag/flagStatus").and_then(|v| v.as_str()) {
        msg["flag"] = json!({ "flagStatus": flag_status });
    }
    Json(msg.clone()).into_response()
}

async fn outlook_send_mail_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.sent_mails.write().unwrap().push(payload);
    StatusCode::ACCEPTED.into_response()
}

async fn outlook_reply_mail_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path(msg_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.mail_replies.write().unwrap().push((msg_id, payload));
    StatusCode::ACCEPTED.into_response()
}

async fn outlook_get_attachment_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path((msg_id, att_id)): Path<(String, String)>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let guard = state.mail_attachments.read().unwrap();
    match guard.get(&(msg_id, att_id)) {
        Some(att) => Json(att.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NotFound", "message": "attachment not found" } }))).into_response(),
    }
}

async fn outlook_calendar_view_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    assert_eq!(
        headers.get("prefer").and_then(|v| v.to_str().ok()),
        Some("outlook.timezone=\"UTC\"")
    );
    assert!(params.contains_key("startDateTime"));
    assert!(params.contains_key("endDateTime"));
    let events = state.calendar_events.read().unwrap().clone();
    Json(json!({ "value": events })).into_response()
}

async fn outlook_create_event_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let is_online = payload
        .get("isOnlineMeeting")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut created = json!({
        "id": "ev-created-1",
        "subject": payload["subject"],
        "start": payload["start"],
        "end": payload["end"],
        "isAllDay": payload["isAllDay"],
        "location": payload.get("location").cloned().unwrap_or(Value::Null),
        "attendees": payload.get("attendees").cloned().unwrap_or_else(|| json!([])),
        "responseStatus": { "response": "organizer" },
        "isOnlineMeeting": is_online,
        "body": payload.get("body").cloned().unwrap_or(Value::Null),
    });
    if is_online {
        created["onlineMeeting"] = json!({
            "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_v2_e2e/0"
        });
    }
    state.calendar_events.write().unwrap().push(created.clone());
    (StatusCode::CREATED, Json(created)).into_response()
}

async fn outlook_rsvp_event_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path((event_id, action)): Path<(String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state
        .calendar_rsvps
        .write()
        .unwrap()
        .push((event_id.clone(), action.clone(), payload));

    let mapped_resp = match action.as_str() {
        "accept" => "accepted",
        "tentativelyAccept" => "tentativelyAccepted",
        "decline" => "declined",
        other => other,
    };
    let mut guard = state.calendar_events.write().unwrap();
    if let Some(ev) = guard.iter_mut().find(|e| e["id"].as_str() == Some(event_id.as_str())) {
        ev["responseStatus"] = json!({ "response": mapped_resp });
    }
    StatusCode::ACCEPTED.into_response()
}

async fn outlook_contacts_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let list = state.contacts.read().unwrap().clone();
    Json(json!({ "value": list })).into_response()
}

async fn outlook_create_contact_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let created = json!({
        "id": "contact-created-1",
        "displayName": payload["displayName"],
        "givenName": payload.get("givenName").cloned().unwrap_or(Value::Null),
        "surname": payload.get("surname").cloned().unwrap_or(Value::Null),
        "emailAddresses": payload.get("emailAddresses").cloned().unwrap_or_else(|| json!([])),
        "mobilePhone": payload.get("mobilePhone").cloned().unwrap_or(Value::Null),
        "companyName": payload.get("companyName").cloned().unwrap_or(Value::Null),
        "jobTitle": payload.get("jobTitle").cloned().unwrap_or(Value::Null),
        "department": payload.get("department").cloned().unwrap_or(Value::Null),
        "personalNotes": payload.get("personalNotes").cloned().unwrap_or(Value::Null),
    });
    state.contacts.write().unwrap().push(created.clone());
    (StatusCode::CREATED, Json(created)).into_response()
}

async fn outlook_directory_users_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let users = state.directory_users.read().unwrap().clone();
    let search_q = params
        .get("$search")
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let filtered: Vec<Value> = if search_q.is_empty() {
        users
    } else {
        users
            .into_iter()
            .filter(|u| {
                let dn = u["displayName"].as_str().unwrap_or("").to_lowercase();
                let mail = u["mail"].as_str().unwrap_or("").to_lowercase();
                search_q.contains("김") && (dn.contains("김") || mail.contains("kim"))
                    || (!dn.is_empty() && search_q.contains(&dn))
            })
            .collect()
    };
    Json(json!({ "value": filtered })).into_response()
}

async fn outlook_todo_lists_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let lists = state.todo_lists.read().unwrap().clone();
    Json(json!({ "value": lists })).into_response()
}

async fn outlook_create_todo_task_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path(list_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let created = json!({
        "id": "task-created-1",
        "title": payload["title"],
        "status": "notStarted",
        "importance": payload.get("importance").cloned().unwrap_or_else(|| json!("normal")),
        "body": payload.get("body").cloned().unwrap_or(Value::Null),
        "dueDateTime": payload.get("dueDateTime").cloned().unwrap_or(Value::Null),
    });
    state
        .todo_tasks
        .write()
        .unwrap()
        .entry(list_id)
        .or_default()
        .push(created.clone());
    (StatusCode::CREATED, Json(created)).into_response()
}

async fn outlook_patch_todo_task_handler(
    State(state): State<Arc<OutlookMockState>>,
    headers: HeaderMap,
    Path((list_id, task_id)): Path<(String, String)>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let mut guard = state.todo_tasks.write().unwrap();
    let list = guard.entry(list_id).or_default();
    let Some(task) = list
        .iter_mut()
        .find(|t| t["id"].as_str() == Some(task_id.as_str()))
    else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NotFound", "message": "task not found" } }))).into_response();
    };
    if let Some(status) = payload.get("status").and_then(|v| v.as_str()) {
        task["status"] = Value::String(status.to_string());
    }
    Json(task.clone()).into_response()
}

async fn spawn_mock_outlook_graph_server(token: &str) -> (String, Arc<OutlookMockState>) {
    let state = Arc::new(OutlookMockState::new(token));
    let app = Router::new()
        .route("/v1.0/me/mailFolders", get(outlook_mail_folders_handler))
        .route(
            "/v1.0/me/mailFolders/{folder}/messages",
            get(outlook_folder_messages_handler),
        )
        .route(
            "/v1.0/me/messages/{msg_id}",
            get(outlook_get_message_handler).patch(outlook_patch_message_handler),
        )
        .route("/v1.0/me/sendMail", post(outlook_send_mail_handler))
        .route(
            "/v1.0/me/messages/{msg_id}/reply",
            post(outlook_reply_mail_handler),
        )
        .route(
            "/v1.0/me/messages/{msg_id}/attachments/{att_id}",
            get(outlook_get_attachment_handler),
        )
        .route("/v1.0/me/calendarView", get(outlook_calendar_view_handler))
        .route("/v1.0/me/events", post(outlook_create_event_handler))
        .route(
            "/v1.0/me/events/{event_id}/{action}",
            post(outlook_rsvp_event_handler),
        )
        .route(
            "/v1.0/me/contacts",
            get(outlook_contacts_handler).post(outlook_create_contact_handler),
        )
        .route("/v1.0/users", get(outlook_directory_users_handler))
        .route("/v1.0/me/todo/lists", get(outlook_todo_lists_handler))
        .route(
            "/v1.0/me/todo/lists/{list_id}/tasks",
            post(outlook_create_todo_task_handler),
        )
        .route(
            "/v1.0/me/todo/lists/{list_id}/tasks/{task_id}",
            patch(outlook_patch_todo_task_handler),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Outlook Graph server");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock Outlook Graph");
    });

    (format!("http://{addr}"), state)
}

fn build_outlook_graph_client(
    base_url: &str,
    download_dir: &std::path::Path,
    token: &str,
) -> (AppConfig, GraphClient) {
    let cfg = build_test_config(base_url, download_dir);
    let tokens = StoredTokens {
        access_token: token.to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("me-user-1".to_string()),
        user_display_name: Some("나(테스터)".to_string()),
        user_email: Some("tester@contoso.com".to_string()),
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth)
        .expect("build Outlook GraphClient")
        .with_my_user_id("me-user-1");
    (cfg, graph)
}

#[tokio::test]
async fn e2e_mock_graph_outlook_mail() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create mail test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");
    let token = "outlook-mail-tok-2026";

    let (base_url, mock) = spawn_mock_outlook_graph_server(token).await;
    let (_cfg, graph) = build_outlook_graph_client(&base_url, &download_dir, token);
    let storage = Storage::open(&db_path).expect("open SQLite storage");

    // 1. Mail folders + Inbox listing
    let folders = graph.list_mail_folders().await.expect("list_mail_folders");
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0].well_known_name.as_deref(), Some("inbox"));
    storage
        .upsert_mail_folders(&folders)
        .expect("upsert_mail_folders");

    let inbox = graph
        .list_mail_messages("inbox", 20)
        .await
        .expect("list_mail_messages");
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].id, "mail-1");
    assert!(inbox[0].has_attachments);

    // 2. Get mail message with expanded attachments + download attachment
    let detail = graph
        .get_mail_message("mail-1")
        .await
        .expect("get_mail_message");
    assert_eq!(detail.attachments.len(), 1);
    assert_eq!(detail.attachments[0].name, "architecture-note.txt");

    let downloaded = graph
        .download_mail_attachment("mail-1", &detail.attachments[0].id, &download_dir)
        .await
        .expect("download_mail_attachment");
    assert!(downloaded.path.exists());
    assert_eq!(
        std::fs::read(&downloaded.path).expect("read downloaded mail attachment"),
        b"Outlook Attachment Payload \xed\x95\x9c\xea\xb8\x80"
    );

    // 3. send_mail (with file attachment) & reply_mail
    let attach_path = temp.path().join("release-summary.txt");
    std::fs::write(&attach_path, b"Release v0.2.0 summary").expect("write mail attachment");
    graph
        .send_mail(&OutgoingMail {
            subject: "v0.2.0 릴리즈 보고".to_string(),
            body_html: "<p>첨부파일 확인 부탁드립니다.</p>".to_string(),
            to: vec!["alice@contoso.com".to_string()],
            cc: vec![],
            bcc: vec![],
            importance: Some("high".to_string()),
            attachments: vec![attach_path],
            save_to_sent_items: true,
        })
        .await
        .expect("send_mail");

    {
        let sent = mock.sent_mails.read().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["saveToSentItems"], true);
        assert_eq!(sent[0]["message"]["subject"], "v0.2.0 릴리즈 보고");
        assert_eq!(
            sent[0]["message"]["attachments"][0]["name"],
            "release-summary.txt"
        );
    }

    graph
        .reply_mail("mail-1", "<p>검토 완료했습니다.</p>", false)
        .await
        .expect("reply_mail");
    assert_eq!(mock.mail_replies.read().unwrap().len(), 1);

    // 4. set_mail_read & set_mail_flag
    graph
        .set_mail_read("mail-1", true)
        .await
        .expect("set_mail_read");
    graph
        .set_mail_flag("mail-1", true)
        .await
        .expect("set_mail_flag");
    let updated_mail = graph
        .get_mail_message("mail-1")
        .await
        .expect("get_mail_message after patch");
    assert!(updated_mail.is_read);
    assert!(updated_mail.flagged);

    // 5. SQLite upsert_mail_messages + Korean/English search_mail_messages
    let mut persisted = inbox;
    persisted[0] = updated_mail;
    storage
        .upsert_mail_messages(&persisted)
        .expect("upsert_mail_messages");

    let ko_hits = storage
        .search_mail_messages("아키텍처", None, 10)
        .expect("Korean trigram search");
    assert_eq!(ko_hits.len(), 1);
    assert_eq!(ko_hits[0].id, "mail-1");

    let ko_2char = storage
        .search_mail_messages("결재", Some("inbox"), 10)
        .expect("Korean 2-char fallback search");
    assert_eq!(ko_2char.len(), 1);
    assert_eq!(ko_2char[0].id, "mail-1");

    let en_hits = storage
        .search_mail_messages("Kubernetes", None, 10)
        .expect("English search");
    assert_eq!(en_hits.len(), 1);
    assert_eq!(en_hits[0].id, "mail-1");

    let en_mail2 = storage
        .search_mail_messages("Sprint", None, 10)
        .expect("English mail-2 search");
    assert_eq!(en_mail2.len(), 1);
    assert_eq!(en_mail2[0].id, "mail-2");
}

#[tokio::test]
async fn e2e_mock_graph_outlook_calendar() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create calendar test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");
    let token = "outlook-cal-tok-2026";

    let (base_url, mock) = spawn_mock_outlook_graph_server(token).await;
    let (_cfg, graph) = build_outlook_graph_client(&base_url, &download_dir, token);
    let storage = Storage::open(&db_path).expect("open SQLite storage");

    // 1. list_calendar_view
    let initial_events = graph
        .list_calendar_view("2026-11-15T00:00:00Z", "2026-11-22T00:00:00Z", 50)
        .await
        .expect("list_calendar_view");
    assert_eq!(initial_events.len(), 1);
    assert_eq!(initial_events[0].id, "ev-seed-1");

    // 2. create_calendar_event with is_online_meeting: true -> Teams joinUrl
    let created = graph
        .create_calendar_event(&OutgoingEvent {
            subject: "v0.2.0 릴리즈 아키텍처 회의".to_string(),
            start_iso: "2026-11-17T02:00:00".to_string(),
            end_iso: "2026-11-17T03:00:00".to_string(),
            time_zone: Some("UTC".to_string()),
            is_all_day: false,
            location: Some("Teams 온라인".to_string()),
            body_html: Some("<p>릴리즈 체크리스트 점검</p>".to_string()),
            attendees: vec!["alice@contoso.com".to_string()],
            is_online_meeting: true,
        })
        .await
        .expect("create_calendar_event");
    assert_eq!(created.id, "ev-created-1");
    assert!(created.is_online_meeting);
    assert_eq!(
        created.online_meeting_url.as_deref(),
        Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_v2_e2e/0")
    );

    // 3. respond_calendar_event("accept")
    graph
        .respond_calendar_event("ev-seed-1", "accept", Some("참석하겠습니다"))
        .await
        .expect("respond_calendar_event");
    assert_eq!(mock.calendar_rsvps.read().unwrap().len(), 1);
    assert_eq!(mock.calendar_rsvps.read().unwrap()[0].1, "accept");

    // 4. SQLite persistence
    let refreshed = graph
        .list_calendar_view("2026-11-15T00:00:00Z", "2026-11-22T00:00:00Z", 50)
        .await
        .expect("list_calendar_view after rsvp");
    assert_eq!(refreshed.len(), 2);
    storage
        .upsert_calendar_events(&refreshed)
        .expect("upsert_calendar_events");

    let stored = storage
        .list_calendar_events(Some("2026-11-15T00:00:00"), Some("2026-11-22T00:00:00"), 20)
        .expect("list_calendar_events from SQLite");
    assert_eq!(stored.len(), 2);
    let stored_seed = stored.iter().find(|e| e.id == "ev-seed-1").unwrap();
    assert_eq!(stored_seed.response_status, "accepted");
    let stored_created = stored.iter().find(|e| e.id == "ev-created-1").unwrap();
    assert_eq!(
        stored_created.online_meeting_url.as_deref(),
        Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_v2_e2e/0")
    );
}

#[tokio::test]
async fn e2e_mock_graph_outlook_contacts_and_todo() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create contacts/todo test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");
    let token = "outlook-contacts-todo-tok-2026";

    let (base_url, _mock) = spawn_mock_outlook_graph_server(token).await;
    let (_cfg, graph) = build_outlook_graph_client(&base_url, &download_dir, token);
    let storage = Storage::open(&db_path).expect("open SQLite storage");

    // 1. Contacts: list_contacts, search_contacts_and_directory, create_contact
    let contacts = graph.list_contacts(20).await.expect("list_contacts");
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].source, "contact");

    let merged = graph
        .search_contacts_and_directory("김", 20)
        .await
        .expect("search_contacts_and_directory");
    assert!(merged.iter().any(|c| c.source == "contact" && c.primary_email() == Some("kim@contoso.com")));
    assert!(merged.iter().any(|c| c.source == "directory" && c.primary_email() == Some("younghee.kim@contoso.com")));

    let created_contact = graph
        .create_contact(&OutgoingContact {
            display_name: "박민수".to_string(),
            given_name: Some("민수".to_string()),
            surname: Some("박".to_string()),
            email_addresses: vec!["park@contoso.com".to_string()],
            mobile_phone: Some("010-9999-8888".to_string()),
            company_name: Some("Contoso".to_string()),
            job_title: Some("Security Lead".to_string()),
            department: Some("Security".to_string()),
            notes: Some("E2E 연락처".to_string()),
        })
        .await
        .expect("create_contact");
    assert_eq!(created_contact.id, "contact-created-1");
    assert_eq!(created_contact.primary_email(), Some("park@contoso.com"));

    storage
        .upsert_contacts(&[created_contact])
        .expect("upsert_contacts");
    let stored_contacts = storage
        .list_contacts(Some("박민수"), 10)
        .expect("list_contacts from SQLite");
    assert_eq!(stored_contacts.len(), 1);
    assert_eq!(stored_contacts[0].id, "contact-created-1");

    // 2. Microsoft To Do: list_todo_lists, create_todo_task, set_todo_task_completed(true)
    let lists = graph.list_todo_lists().await.expect("list_todo_lists");
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].id, "list-default");
    storage
        .upsert_todo_lists(&lists)
        .expect("upsert_todo_lists");

    let created_task = graph
        .create_todo_task(
            "default",
            &OutgoingTodoTask {
                title: "v0.2.0 통합 테스트 검증".to_string(),
                body_plain: Some("Mail/Cal/Contacts/ToDo E2E".to_string()),
                due_date_iso: Some("2026-11-18T09:00:00".to_string()),
                importance: Some("high".to_string()),
            },
        )
        .await
        .expect("create_todo_task");
    assert_eq!(created_task.id, "task-created-1");
    assert!(!created_task.is_completed());

    let completed_task = graph
        .set_todo_task_completed("default", &created_task.id, true)
        .await
        .expect("set_todo_task_completed");
    assert!(completed_task.is_completed());

    storage
        .upsert_todo_tasks(std::slice::from_ref(&completed_task))
        .expect("upsert_todo_tasks");
    let stored_tasks = storage
        .list_todo_tasks("list-default", true, 20)
        .expect("list_todo_tasks from SQLite");
    assert_eq!(stored_tasks.len(), 1);
    assert!(stored_tasks[0].is_completed());
}

#[tokio::test]
async fn e2e_inline_image_preview_and_self_updater() {
    // 1. Inline Terminal Image Preview: generate in-memory PNG via image::RgbaImage
    let img: RgbaImage = ImageBuffer::from_fn(16, 12, |x, y| {
        Rgba([((x * 16) % 256) as u8, ((y * 20) % 256) as u8, 180, 255])
    });
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .expect("encode RGBA image to PNG");

    let rendered = img_preview::decode_and_render_halfblocks(&png_bytes, 16, 8, (0, 0, 0))
        .expect("decode_and_render_halfblocks");
    assert_eq!(rendered.width_px, 16);
    assert_eq!(rendered.height_px, 12);
    assert_eq!(rendered.cols, 16);
    assert_eq!(rendered.rows, 6);
    assert_eq!(rendered.format_label, "PNG");
    assert_eq!(rendered.lines.len(), 6);
    for line in &rendered.lines {
        assert!(!line.spans.is_empty());
        assert!(line.spans.iter().all(|s| !s.content.is_empty() && s.content.chars().all(|c| c == '▀')));
    }

    let ansi_stdout = img_preview::render_ansi_stdout(&png_bytes, 16, 8)
        .expect("render_ansi_stdout");
    assert!(!ansi_stdout.trim().is_empty());
    assert!(
        ansi_stdout.contains('▀')
            || ansi_stdout.starts_with("\x1b]")
            || ansi_stdout.starts_with("\x1b_G"),
        "expected valid halfblock, iTerm2, or Kitty escape output"
    );

    // 2. Self-Updater E2E against mock GitHub Releases server
    #[derive(Clone)]
    struct MockGhState {
        base_url: String,
        asset_name: String,
        asset_bytes: Vec<u8>,
        sums_text: String,
    }

    let asset_name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "teams.exe".to_string(),
        ("windows", "aarch64") => "teams-9.9.9-aarch64-pc-windows-msvc.zip".to_string(),
        ("macos", "x86_64") => "teams-9.9.9-x86_64-apple-darwin.tar.gz".to_string(),
        ("macos", "aarch64") => "teams-9.9.9-aarch64-apple-darwin.tar.gz".to_string(),
        ("linux", "x86_64") => "teams-9.9.9-x86_64-unknown-linux-gnu.tar.gz".to_string(),
        ("linux", "aarch64") => "teams-9.9.9-aarch64-unknown-linux-gnu.tar.gz".to_string(),
        _ => "teams".to_string(),
    };
    let asset_bytes = b"MOCK-RELEASE-BINARY-V9.9.9".to_vec();
    let digest = hex::encode(Sha256::digest(&asset_bytes));
    let sums_text = format!("{digest}  {asset_name}\n");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock GitHub server");
    let addr = listener.local_addr().unwrap();
    let gh_base = format!("http://{addr}");

    let gh_state = MockGhState {
        base_url: gh_base.clone(),
        asset_name: asset_name.clone(),
        asset_bytes,
        sums_text,
    };

    let gh_app = Router::new()
        .route(
            "/repos/shinick-dev/teams/releases/latest",
            get(|State(st): State<MockGhState>| async move {
                Json(json!({
                    "tag_name": "v9.9.9",
                    "html_url": format!("{}/shinick-dev/teams/releases/tag/v9.9.9", st.base_url),
                    "assets": [
                        {
                            "name": st.asset_name,
                            "size": st.asset_bytes.len() as u64,
                            "url": format!("{}/assets/binary", st.base_url),
                            "browser_download_url": format!("{}/download/{}", st.base_url, st.asset_name)
                        },
                        {
                            "name": "SHA256SUMS.txt",
                            "size": st.sums_text.len() as u64,
                            "url": format!("{}/assets/sums", st.base_url),
                            "browser_download_url": format!("{}/download/SHA256SUMS.txt", st.base_url)
                        }
                    ]
                }))
            }),
        )
        .route(
            "/assets/sums",
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.sums_text) }),
        )
        .route(
            "/download/SHA256SUMS.txt",
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.sums_text) }),
        )
        .route(
            "/assets/binary",
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.asset_bytes) }),
        )
        .route(
            &format!("/download/{asset_name}"),
            get(|State(st): State<MockGhState>| async move { (StatusCode::OK, st.asset_bytes) }),
        )
        .with_state(gh_state);

    let gh_server = tokio::spawn(async move {
        axum::serve(listener, gh_app)
            .await
            .expect("serve mock GitHub");
    });

    let result = updater::check_or_apply_update_with_api_base(
        &gh_base,
        "shinick-dev/teams",
        true,
        false,
    )
    .await
    .expect("check_or_apply_update_with_api_base");

    assert_eq!(result.current_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(result.latest_version, "9.9.9");
    assert!(result.update_available);
    assert!(!result.updated);
    assert_eq!(result.asset_name.as_deref(), Some(asset_name.as_str()));
    assert!(result.release_url.contains("v9.9.9"));

    gh_server.abort();
}
