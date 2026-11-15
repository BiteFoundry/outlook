//! End-to-end test of the OneNote HTTP JSON API (`src/serve.rs`) against an in-process
//! mock Microsoft Graph v1.0 + Entra OAuth server.
//!
//! Every route is exercised through the real `serve::build_router` router, while the
//! `GraphClient` behind it talks to the mock over loopback HTTP. The mock records the
//! mutations it receives so the test can prove the HTTP API actually called Graph.

use std::sync::{Arc, RwLock};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use teams_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    serve,
    storage::Storage,
};

const ACCESS_TOKEN: &str = "onenote-access-tok";
const SERVE_SECRET: &str = "e2e-secret";
const MOCK_PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

#[derive(Default)]
struct MockState {
    expected_token: RwLock<String>,
    /// Loopback base URL, learned after bind; embedded in OneNote `content` XHTML
    /// so `download_onenote_resource_bytes` sees an absolute resource URL.
    base_url: RwLock<String>,
    notebook_posts: RwLock<Vec<Value>>,
    section_posts: RwLock<Vec<(String, Value)>>,
    page_posts: RwLock<Vec<(String, String)>>,
    /// (page id, content-type, body)
    content_patches: RwLock<Vec<(String, String, String)>>,
    deleted_pages: RwLock<Vec<String>>,
    page_meta_gets: RwLock<Vec<String>>,
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
    fn check_bearer(&self, headers: &HeaderMap) -> Result<(), Response> {
        let expected = self.expected_token.read().unwrap().clone();
        check_bearer_header(headers, &expected)
    }
}

fn graph_not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "ResourceNotFound",
                "message": message
            }
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Mock Graph / OAuth handlers
// ---------------------------------------------------------------------------

async fn oauth_token_handler(_body: String) -> impl IntoResponse {
    Json(json!({
        "token_type": "Bearer",
        "expires_in": 3600,
        "access_token": ACCESS_TOKEN,
        "refresh_token": "rotated-refresh-tok"
    }))
}

async fn joined_teams_handler(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    // No teams: keeps `list_all_accessible_notebooks(true)` on the personal scope only.
    Json(json!({ "value": [] })).into_response()
}

async fn list_notebooks_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({
        "value": [{
            "id": "nb-personal",
            "displayName": "업무 노트",
            "color": "#1E6FDE",
            "isDefault": true,
            "isShared": false,
            "userRole": "Owner",
            "createdDateTime": "2026-11-01T09:00:00Z",
            "lastModifiedDateTime": "2026-11-15T09:00:00Z",
            "createdBy": { "user": { "id": "me-user-1", "displayName": "나(테스터)" } },
            "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-personal" } }
        }]
    }))
    .into_response()
}

async fn create_notebook_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.notebook_posts.write().unwrap().push(payload.clone());
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "nb-created",
            "displayName": payload["displayName"],
            "isDefault": false,
            "isShared": false,
            "userRole": "Owner",
            "createdDateTime": "2026-11-16T09:00:00Z",
            "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-created" } }
        })),
    )
        .into_response()
}

async fn list_section_groups_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({
        "value": [{
            "id": "sg-1",
            "displayName": "2026 아카이브",
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" }
        }]
    }))
    .into_response()
}

async fn list_sections_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    Json(json!({
        "value": [{
            "id": "sec-1",
            "displayName": "주간 회의",
            "isDefault": true,
            "createdDateTime": "2026-11-01T09:05:00Z",
            "lastModifiedDateTime": "2026-11-15T09:05:00Z",
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" },
            "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/sec-1" } }
        }]
    }))
    .into_response()
}

async fn create_section_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(notebook_id): Path<String>,
    Json(payload): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state
        .section_posts
        .write()
        .unwrap()
        .push((notebook_id, payload.clone()));
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "sec-created",
            "displayName": payload["displayName"],
            "isDefault": false,
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" }
        })),
    )
        .into_response()
}

async fn list_pages_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(section_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    if section_id != "sec-1" {
        return Json(json!({ "value": [] })).into_response();
    }
    Json(json!({
        "value": [{
            "id": "pg-1",
            "title": "Architecture Migration",
            "level": 0,
            "order": 0,
            "createdDateTime": "2026-11-10T09:00:00Z",
            "lastModifiedDateTime": "2026-11-15T10:00:00Z",
            "contentUrl": "https://graph.microsoft.com/v1.0/me/onenote/pages/pg-1/content",
            "parentSection": { "id": "sec-1", "displayName": "주간 회의" },
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" },
            "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/pg-1" } }
        }]
    }))
    .into_response()
}

async fn create_page_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(section_id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state
        .page_posts
        .write()
        .unwrap()
        .push((section_id, String::from_utf8_lossy(&body).to_string()));
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "pg-created",
            "title": "새 페이지",
            "level": 0,
            "parentSection": { "id": "sec-1", "displayName": "주간 회의" },
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" }
        })),
    )
        .into_response()
}

fn page_xhtml(base_url: &str) -> String {
    format!(
        concat!(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Architecture Migration</title></head>"#,
            r#"<body><div id="div:111132">"#,
            r#"<h1 id="h1:333350">Architecture Migration</h1>"#,
            r#"<p id="p:222240" data-tag="to-do">migration checklist rollout</p>"#,
            r#"<p><img id="img:444460" src="{base}/v1.0/me/onenote/resources/thumb-1/content" "#,
            r#"data-fullres-src="{base}/v1.0/me/onenote/resources/res-1/content" "#,
            r#"data-src-type="image/png" width="800" height="600" alt="diagram" /></p>"#,
            r#"</div></body></html>"#
        ),
        base = base_url
    )
}

fn standalone_xhtml() -> &'static str {
    concat!(
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Standalone Page</title></head>"#,
        r#"<body><div id="div:fresh1"><p id="p:fresh2">standalone body</p></div></body></html>"#
    )
}

async fn get_page_meta_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.page_meta_gets.write().unwrap().push(page_id.clone());
    match page_id.as_str() {
        "pg-1" => Json(json!({
            "id": "pg-1",
            "title": "Architecture Migration",
            "level": 0,
            "order": 0,
            "parentSection": { "id": "sec-1", "displayName": "주간 회의" },
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" }
        }))
        .into_response(),
        "pg-fresh" => Json(json!({
            "id": "pg-fresh",
            "title": "Standalone Page",
            "level": 0,
            "order": 1,
            "parentSection": { "id": "sec-1", "displayName": "주간 회의" },
            "parentNotebook": { "id": "nb-personal", "displayName": "업무 노트" }
        }))
        .into_response(),
        other => graph_not_found(&format!("page {other} not found")),
    }
}

async fn get_page_content_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let base = state.base_url.read().unwrap().clone();
    match page_id.as_str() {
        "pg-1" => (StatusCode::OK, page_xhtml(&base)).into_response(),
        "pg-fresh" => (StatusCode::OK, standalone_xhtml()).into_response(),
        other => graph_not_found(&format!("page content {other} not found")),
    }
}

async fn patch_page_content_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    state.content_patches.write().unwrap().push((
        page_id,
        content_type,
        String::from_utf8_lossy(&body).to_string(),
    ));
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_page_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    state.deleted_pages.write().unwrap().push(page_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn get_resource_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(resource_id): Path<String>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    if resource_id != "res-1" {
        return graph_not_found(&format!("resource {resource_id} not found"));
    }
    (StatusCode::OK, MOCK_PNG.to_vec()).into_response()
}

async fn spawn_mock_graph(initial_token: &str) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState::default());
    *state.expected_token.write().unwrap() = initial_token.to_string();

    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(oauth_token_handler))
        .route("/v1.0/me/joinedTeams", get(joined_teams_handler))
        .route(
            "/v1.0/me/onenote/notebooks",
            get(list_notebooks_handler).post(create_notebook_handler),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{notebook_id}/sections",
            post(create_section_handler),
        )
        .route(
            "/v1.0/me/onenote/sectionGroups",
            get(list_section_groups_handler),
        )
        .route("/v1.0/me/onenote/sections", get(list_sections_handler))
        .route(
            "/v1.0/me/onenote/sections/{section_id}/pages",
            get(list_pages_handler).post(create_page_handler),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}/content",
            get(get_page_content_handler).patch(patch_page_content_handler),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}",
            get(get_page_meta_handler).delete(delete_page_handler),
        )
        .route(
            "/v1.0/me/onenote/resources/{resource_id}/content",
            get(get_resource_handler),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Graph server");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    *state.base_url.write().unwrap() = base_url.clone();

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock Graph");
    });

    (base_url, state)
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
async fn e2e_onenote_http_api_routes() {
    let temp = tempfile::tempdir().expect("create test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");
    let config_dir = temp.path().join("config");

    // Keep AuthManager/credential fallback writes hermetic.
    std::env::set_var("TEAMS_DISABLE_KEYRING", "1");
    std::env::set_var("TEAMS_CONFIG_DIR", &config_dir);
    std::env::remove_var("TEAMS_ACCESS_TOKEN");

    let (base_url, mock) = spawn_mock_graph(ACCESS_TOKEN).await;
    let cfg = build_test_config(&base_url, &download_dir);

    let tokens = StoredTokens {
        access_token: ACCESS_TOKEN.to_string(),
        refresh_token: Some("onenote-refresh-tok".to_string()),
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
        .expect("build GraphClient")
        .with_my_user_id("me-user-1");
    let storage = Storage::open(&db_path).expect("open SQLite storage");

    // ---- Real HTTP API under test ------------------------------------------------
    let router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        Some(SERVE_SECRET.to_string()),
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
    let api = format!("http://{serve_addr}");
    let sec_key = urlencoding::encode("me|sec-1");
    let page_key = urlencoding::encode("me|pg-1");

    // 1. Canonical mount: GET /v1/onenote/notebooks
    let res = http
        .get(format!("{api}/v1/onenote/notebooks"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET /v1/onenote/notebooks");
    assert_eq!(res.status(), StatusCode::OK);
    let notebooks_json: Value = res.json().await.unwrap();
    let notebooks = notebooks_json["notebooks"]
        .as_array()
        .expect("notebooks array");
    assert_eq!(notebooks.len(), 1);
    assert_eq!(notebooks[0]["notebook_key"], "me|nb-personal");
    assert_eq!(notebooks[0]["id"], "nb-personal");
    assert_eq!(notebooks[0]["display_name"], "업무 노트");
    assert_eq!(notebooks[0]["scope"]["scope_type"], "me");
    assert_eq!(notebooks[0]["is_default"], true);
    assert_eq!(
        notebooks[0]["web_url"],
        "https://onenote.example.com/nb-personal"
    );
    assert_eq!(
        notebooks[0]["created_by"], "나(테스터)",
        "createdBy identity should be projected into the notebook model"
    );

    // 2. Unversioned alias mount returns the exact same payload.
    let res = http
        .get(format!("{api}/onenote/notebooks"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET /onenote/notebooks (alias)");
    assert_eq!(res.status(), StatusCode::OK);
    let alias_json: Value = res.json().await.unwrap();
    assert_eq!(
        alias_json, notebooks_json,
        "alias mount must serve the same payload as /v1/onenote"
    );

    // 3a. GET /v1/onenote/sections
    let res = http
        .get(format!("{api}/v1/onenote/sections"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET /v1/onenote/sections");
    assert_eq!(res.status(), StatusCode::OK);
    let sections_json: Value = res.json().await.unwrap();
    assert!(sections_json["notebook"].is_null());
    let sections = sections_json["sections"]
        .as_array()
        .expect("sections array");
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0]["section_key"], "me|sec-1");
    assert_eq!(sections[0]["display_name"], "주간 회의");
    assert_eq!(sections[0]["notebook_id"], "nb-personal");
    assert_eq!(sections[0]["notebook_name"], "업무 노트");

    // 3b. GET /v1/onenote/sections/{key}/pages
    let res = http
        .get(format!("{api}/v1/onenote/sections/{sec_key}/pages"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET /v1/onenote/sections/{key}/pages");
    assert_eq!(res.status(), StatusCode::OK);
    let pages_json: Value = res.json().await.unwrap();
    assert_eq!(pages_json["section"]["section_key"], "me|sec-1");
    let pages = pages_json["pages"].as_array().expect("pages array");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["page_key"], "me|pg-1");
    assert_eq!(pages[0]["id"], "pg-1");
    assert_eq!(pages[0]["title"], "Architecture Migration");
    assert_eq!(pages[0]["section_id"], "sec-1");
    assert_eq!(pages[0]["section_name"], "주간 회의");
    assert_eq!(pages[0]["level"], 0);

    // 3c. GET /v1/onenote/pages/{key} -> XHTML parsed into blocks/resources/markdown
    let res = http
        .get(format!("{api}/v1/onenote/pages/{page_key}"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET /v1/onenote/pages/{key}");
    assert_eq!(res.status(), StatusCode::OK);
    let page_json: Value = res.json().await.unwrap();
    let detail = &page_json["page"];
    assert_eq!(detail["summary"]["page_key"], "me|pg-1");
    assert_eq!(detail["summary"]["title"], "Architecture Migration");
    assert_eq!(detail["primary_div_id"], "div:111132");
    assert!(
        detail["body_plain"]
            .as_str()
            .unwrap()
            .contains("migration checklist rollout"),
        "XHTML body should be parsed into plain text"
    );
    let blocks = detail["blocks"].as_array().expect("blocks array");
    assert!(blocks.len() >= 2);
    assert!(
        blocks
            .iter()
            .any(|b| b["kind"]["block_type"] == "image" && b["kind"]["resource_index"] == 0),
        "unexpected blocks: {blocks:?}"
    );
    let resources = detail["resources"].as_array().expect("resources array");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["mime_type"], "image/png");
    assert!(resources[0]["fullres_url"]
        .as_str()
        .unwrap()
        .ends_with("/v1.0/me/onenote/resources/res-1/content"));

    // A page known only to Graph exercises the `GET /pages/{id}` metadata route
    // (the cached pg-1 summary above skips it).
    let fresh_key = urlencoding::encode("me|pg-fresh");
    let res = http
        .get(format!("{api}/v1/onenote/pages/{fresh_key}"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET uncached page detail");
    assert_eq!(res.status(), StatusCode::OK);
    let fresh_json: Value = res.json().await.unwrap();
    assert_eq!(fresh_json["page"]["summary"]["id"], "pg-fresh");
    assert_eq!(fresh_json["page"]["summary"]["title"], "Standalone Page");
    assert!(fresh_json["page"]["body_plain"]
        .as_str()
        .unwrap()
        .contains("standalone body"));
    assert!(
        mock.page_meta_gets
            .read()
            .unwrap()
            .iter()
            .any(|id| id == "pg-fresh"),
        "mock Graph should have received GET /pages/pg-fresh"
    );

    // 4. POST /v1/onenote/notebooks -> Graph POST + echoed entity
    let res = http
        .post(format!("{api}/v1/onenote/notebooks"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({ "name": "신규 노트" }))
        .send()
        .await
        .expect("POST /v1/onenote/notebooks");
    assert_eq!(res.status(), StatusCode::OK);
    let created_nb: Value = res.json().await.unwrap();
    assert_eq!(created_nb["ok"], true);
    assert_eq!(created_nb["notebook"]["notebook_key"], "me|nb-created");
    assert_eq!(created_nb["notebook"]["id"], "nb-created");
    assert_eq!(created_nb["notebook"]["display_name"], "신규 노트");
    let nb_posts = mock.notebook_posts.read().unwrap();
    assert_eq!(nb_posts.len(), 1);
    assert_eq!(
        nb_posts[0]["displayName"], "신규 노트",
        "Graph must receive the displayName payload"
    );

    // 5a. POST /v1/onenote/sections -> Graph POST under the resolved notebook
    let res = http
        .post(format!("{api}/v1/onenote/sections"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({ "notebook": "업무 노트", "name": "스프린트" }))
        .send()
        .await
        .expect("POST /v1/onenote/sections");
    assert_eq!(res.status(), StatusCode::OK);
    let created_sec: Value = res.json().await.unwrap();
    assert_eq!(created_sec["ok"], true);
    assert_eq!(created_sec["notebook"]["notebook_key"], "me|nb-personal");
    assert_eq!(created_sec["section"]["section_key"], "me|sec-created");
    assert_eq!(created_sec["section"]["display_name"], "스프린트");
    assert_eq!(created_sec["section"]["notebook_id"], "nb-personal");
    assert_eq!(created_sec["section"]["notebook_name"], "업무 노트");
    let sec_posts = mock.section_posts.read().unwrap();
    assert_eq!(sec_posts.len(), 1);
    assert_eq!(sec_posts[0].0, "nb-personal");
    assert_eq!(sec_posts[0].1["displayName"], "스프린트");

    // 5b. POST /v1/onenote/sections/{key}/pages -> Graph POST (XHTML payload)
    let res = http
        .post(format!("{api}/v1/onenote/sections/{sec_key}/pages"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({
            "title": "새 페이지",
            "markdown": "## 새 페이지\n\n본문입니다."
        }))
        .send()
        .await
        .expect("POST /v1/onenote/sections/{key}/pages");
    assert_eq!(res.status(), StatusCode::OK);
    let created_page: Value = res.json().await.unwrap();
    assert_eq!(created_page["ok"], true);
    assert_eq!(created_page["section"]["section_key"], "me|sec-1");
    assert_eq!(created_page["page"]["page_key"], "me|pg-created");
    assert_eq!(created_page["page"]["id"], "pg-created");
    assert_eq!(created_page["page"]["title"], "새 페이지");
    assert_eq!(created_page["page"]["section_id"], "sec-1");
    let page_posts = mock.page_posts.read().unwrap();
    assert_eq!(page_posts.len(), 1);
    assert_eq!(page_posts[0].0, "sec-1");
    assert!(
        page_posts[0].1.contains("<title>새 페이지</title>"),
        "Graph should receive the compiled XHTML page payload"
    );

    // 6. POST /v1/onenote/pages/{key}/append -> Graph PATCH .../content
    let res = http
        .post(format!("{api}/v1/onenote/pages/{page_key}/append"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({ "markdown": "추가 메모" }))
        .send()
        .await
        .expect("POST append");
    assert_eq!(res.status(), StatusCode::OK);
    let appended: Value = res.json().await.unwrap();
    assert_eq!(appended["ok"], true);
    assert_eq!(appended["page_key"], "me|pg-1");
    assert_eq!(appended["page_id"], "pg-1");
    assert_eq!(appended["primary_div_id"], "div:111132");
    let patches = mock.content_patches.read().unwrap();
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, "pg-1");
    assert!(
        patches[0].1.starts_with("application/json"),
        "append should send a JSON command body, got {}",
        patches[0].1
    );
    assert!(patches[0].2.contains("\"append\""));
    assert!(patches[0].2.contains("추가 메모"));
    drop(patches);

    // 7a. PATCH /v1/onenote/pages/{key} (block replace) -> Graph PATCH .../content
    let res = http
        .patch(format!("{api}/v1/onenote/pages/{page_key}"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({ "block_id": "p:222240", "markdown": "점검 완료" }))
        .send()
        .await
        .expect("PATCH page block");
    assert_eq!(res.status(), StatusCode::OK);
    let updated: Value = res.json().await.unwrap();
    assert_eq!(updated["ok"], true);
    assert_eq!(updated["page_key"], "me|pg-1");
    assert_eq!(updated["commands_applied"], 1);
    let patches = mock.content_patches.read().unwrap();
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[1].0, "pg-1");
    assert!(patches[1].2.contains("\"replace\""));
    assert!(patches[1].2.contains("p:222240"));
    assert!(patches[1].2.contains("점검 완료"));
    drop(patches);

    // 7b. POST /v1/onenote/pages/{key}/toggle-todo -> Graph PATCH .../content
    let res = http
        .post(format!("{api}/v1/onenote/pages/{page_key}/toggle-todo"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({ "block_id": "p:222240" }))
        .send()
        .await
        .expect("POST toggle-todo");
    assert_eq!(res.status(), StatusCode::OK);
    let toggled: Value = res.json().await.unwrap();
    assert_eq!(toggled["ok"], true);
    assert_eq!(toggled["page_key"], "me|pg-1");
    assert_eq!(toggled["element_id"], "p:222240");
    assert_eq!(toggled["completed"], true);
    assert_eq!(toggled["command"]["target"], "p:222240");
    assert!(toggled["command"]["content"]
        .as_str()
        .unwrap()
        .contains("to-do:completed"));
    let patches = mock.content_patches.read().unwrap();
    assert_eq!(patches.len(), 3);
    assert!(patches[2].1.starts_with("application/json"));
    assert!(patches[2].2.contains("to-do:completed"));
    drop(patches);

    // 8. GET /v1/onenote/search?q=... (FTS over the cached page body)
    let res = http
        .get(format!(
            "{api}/v1/onenote/search?q={}",
            urlencoding::encode("migration")
        ))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET /v1/onenote/search");
    assert_eq!(res.status(), StatusCode::OK);
    let search_json: Value = res.json().await.unwrap();
    let hits = search_json["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "search should hit the cached OneNote page"
    );
    assert_eq!(hits[0]["page_key"], "me|pg-1");
    assert_eq!(hits[0]["title"], "Architecture Migration");
    assert_eq!(hits[0]["section_id"], "sec-1");

    // 9. DELETE /v1/onenote/pages/{key} -> Graph DELETE
    let created_key = urlencoding::encode("me|pg-created");
    let res = http
        .delete(format!("{api}/v1/onenote/pages/{created_key}"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("DELETE /v1/onenote/pages/{key}");
    assert_eq!(res.status(), StatusCode::OK);
    let deleted: Value = res.json().await.unwrap();
    assert_eq!(deleted["ok"], true);
    assert_eq!(deleted["page_key"], "me|pg-created");
    assert_eq!(deleted["page_id"], "pg-created");
    assert_eq!(
        mock.deleted_pages.read().unwrap().as_slice(),
        &["pg-created".to_string()],
        "mock Graph should have received DELETE /pages/pg-created"
    );

    // 10. POST /v1/onenote/pages/{key}/resources/{index}/download
    let res = http
        .post(format!(
            "{api}/v1/onenote/pages/{page_key}/resources/0/download"
        ))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("POST resource download");
    assert_eq!(res.status(), StatusCode::OK);
    let downloaded: Value = res.json().await.unwrap();
    assert_eq!(downloaded["ok"], true);
    assert_eq!(downloaded["page_key"], "me|pg-1");
    assert_eq!(downloaded["index"], 0);
    assert_eq!(downloaded["mime_type"], "image/png");
    assert_eq!(downloaded["file_name"], "onenote-resource-0.png");
    assert_eq!(downloaded["bytes_written"], MOCK_PNG.len());
    let written_path =
        std::path::PathBuf::from(downloaded["path"].as_str().expect("download path string"));
    assert_eq!(written_path, download_dir.join("onenote-resource-0.png"));
    let written = std::fs::read(&written_path).expect("download file should exist");
    assert_eq!(written, MOCK_PNG, "downloaded bytes should match Graph");

    // 11a. Negative: 400 with the handler's exact error string.
    let res = http
        .post(format!("{api}/v1/onenote/notebooks"))
        .bearer_auth(SERVE_SECRET)
        .json(&json!({ "name": "   " }))
        .send()
        .await
        .expect("POST blank notebook name");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let err: Value = res.json().await.unwrap();
    assert_eq!(err["error"], "name must not be empty");

    // 11b. Negative: unknown page key -> 404 from the "not found" error mapping.
    let res = http
        .get(format!("{api}/v1/onenote/pages/missingpage"))
        .bearer_auth(SERVE_SECRET)
        .send()
        .await
        .expect("GET unknown page");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let err: Value = res.json().await.unwrap();
    let message = err["error"].as_str().expect("error string");
    assert!(
        message.contains("OneNote 페이지를 찾을 수 없습니다"),
        "unexpected 404 message: {message}"
    );
    assert!(message.contains("missingpage"));
}
