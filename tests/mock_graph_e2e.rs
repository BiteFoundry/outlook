//! End-to-end integration tests against an in-process mock Microsoft Graph v1.0 + Entra OAuth server.

use std::{
    collections::HashMap,
    sync::{Arc, Once, RwLock},
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use teams_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    mcp,
    models::{AttachmentInfo, ConversationTarget, MentionInfo, MessageItem, TeamSummary},
    parser, resolver, serve,
    storage::Storage,
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
        let actual = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if actual == Some(expected.as_str()) {
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
