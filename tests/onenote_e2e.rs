//! End-to-end integration test for the OneNote Graph surface (`/me/onenote`) driven through the
//! real `GraphClient` against an in-process mock Microsoft Graph v1.0 + Entra OAuth server.
//!
//! Every request is captured by the mock (method path, query string, serialized body and bearer
//! header) so the assertions verify the wire contract, not only the client-side return values.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Once, OnceLock, RwLock},
};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use teams_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    models::{
        OnenoteBlock, OnenoteBlockKind, OnenotePageDetail, OnenoteScope, OnenoteTagKind,
        OutgoingOnenotePage,
    },
    parser,
    storage::Storage,
};

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: OnceLock<TempDir> = OnceLock::new();

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

// ---------------------------------------------------------------------------
// Seeded OneNote fixtures
// ---------------------------------------------------------------------------

const MOCK_ACCESS_TOKEN: &str = "onenote-access-tok-2026";
const MOCK_REFRESH_TOKEN: &str = "onenote-refresh-tok";

const DIV_ID: &str = "div:{0A1B2C3D-0000-4000-8000-000000000001}{32}";
const HEADING_ID: &str = "h1:{0A1B2C3D-0000-4000-8000-000000000002}{20}";
const PARAGRAPH_ID: &str = "p:{0A1B2C3D-0000-4000-8000-000000000003}{40}";
const TODO_ID: &str = "p:{0A1B2C3D-0000-4000-8000-000000000004}{40}";

/// OneNote page XHTML exactly as `GET /pages/{id}/content?includeIDs=true` returns it: a primary
/// `div`, a heading, a normal paragraph and a `to-do` tagged paragraph.
const PAGE_XHTML: &str = r#"<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>아키텍처 설계</title>
    <meta name="created" content="2026-11-15T09:00:00Z" />
  </head>
  <body>
    <div id="div:{0A1B2C3D-0000-4000-8000-000000000001}{32}">
      <h1 id="h1:{0A1B2C3D-0000-4000-8000-000000000002}{20}">아키텍처 설계</h1>
      <p id="p:{0A1B2C3D-0000-4000-8000-000000000003}{40}">Kubernetes 롤아웃 점검을 완료했습니다.</p>
      <p id="p:{0A1B2C3D-0000-4000-8000-000000000004}{40}" data-tag="to-do">마이그레이션 점검</p>
    </div>
  </body>
</html>"#;

fn notebook_json(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "displayName": name,
        "isDefault": false,
        "isShared": false,
        "userRole": "Owner",
        "color": "Blue",
        "createdDateTime": "2026-11-01T09:00:00Z",
        "lastModifiedDateTime": "2026-11-15T11:00:00Z",
        "createdBy": { "user": { "id": "user-onenote", "displayName": "홍길동" } },
        "links": { "oneNoteWebUrl": { "href": format!("https://onenote.example.com/{id}") } }
    })
}

fn section_json(id: &str, name: &str, notebook_id: &str, notebook_name: &str) -> Value {
    json!({
        "id": id,
        "displayName": name,
        "isDefault": false,
        "color": "Green",
        "pagesUrl": format!("https://graph.microsoft.com/v1.0/me/onenote/sections/{id}/pages"),
        "createdDateTime": "2026-11-05T09:00:00Z",
        "lastModifiedDateTime": "2026-11-15T10:30:00Z",
        "parentNotebook": { "id": notebook_id, "displayName": notebook_name },
        "links": { "oneNoteWebUrl": { "href": format!("https://onenote.example.com/{id}") } }
    })
}

fn page_json(
    id: &str,
    title: &str,
    section_id: &str,
    section_name: &str,
    notebook_id: &str,
    notebook_name: &str,
    level: i32,
) -> Value {
    json!({
        "id": id,
        "title": title,
        "level": level,
        "order": 0,
        "createdDateTime": "2026-11-15T09:00:00Z",
        "lastModifiedDateTime": "2026-11-15T10:00:00Z",
        "contentUrl": format!("https://graph.microsoft.com/v1.0/me/onenote/pages/{id}/content"),
        "links": { "oneNoteWebUrl": { "href": format!("https://onenote.example.com/pages/{id}") } },
        "parentSection": { "id": section_id, "displayName": section_name },
        "parentNotebook": { "id": notebook_id, "displayName": notebook_name }
    })
}

// ---------------------------------------------------------------------------
// Mock Graph state & request recorders
// ---------------------------------------------------------------------------

type QueryMap = BTreeMap<String, String>;

#[derive(Default)]
struct MockState {
    expected_token: RwLock<String>,
    base_url: RwLock<Option<String>>,
    token_requests: Mutex<Vec<String>>,
    /// Every `Authorization` header the Graph surface saw, in arrival order.
    auth_headers: Mutex<Vec<String>>,
    notebook_gets: Mutex<Vec<QueryMap>>,
    notebook_posts: Mutex<Vec<Value>>,
    /// `(notebook id from the path, query params)`.
    section_gets: Mutex<Vec<(Option<String>, QueryMap)>>,
    /// `(notebook id from the path, POST body)`.
    section_posts: Mutex<Vec<(String, Value)>>,
    section_group_gets: Mutex<Vec<(Option<String>, QueryMap)>>,
    /// `(section id from the path, query params)`.
    page_list_gets: Mutex<Vec<(String, QueryMap)>>,
    /// `(page id from the path, query params)`.
    page_meta_gets: Mutex<Vec<(String, QueryMap)>>,
    /// `(page id from the path, query params)`.
    page_content_gets: Mutex<Vec<(String, QueryMap)>>,
    /// `(section id, content-type, raw XHTML body)`.
    page_posts: Mutex<Vec<(String, String, String)>>,
    /// `(page id, JSON body)` — the `level` PATCH emitted while creating a subpage.
    page_patches: Mutex<Vec<(String, Value)>>,
    /// `(page id, content-type, JSON command array)`.
    content_patches: Mutex<Vec<(String, String, Value)>>,
    deleted_pages: Mutex<Vec<String>>,
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

fn record<T>(slot: &Mutex<Vec<T>>, item: T) {
    slot.lock().expect("mock recorder lock poisoned").push(item);
}

impl MockState {
    fn new(initial_token: &str) -> Self {
        let state = Self::default();
        *state
            .expected_token
            .write()
            .expect("token lock poisoned") = initial_token.to_string();
        state
    }

    fn base_url(&self) -> String {
        self.base_url
            .read()
            .expect("base url lock poisoned")
            .clone()
            .expect("mock base url initialized before serving")
    }

    /// Records then validates the bearer header so wrong tokens are rejected by every route.
    fn check_bearer(&self, headers: &HeaderMap) -> Result<(), Response> {
        let seen = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(missing)")
            .to_string();
        record(&self.auth_headers, seen);
        let expected = self
            .expected_token
            .read()
            .expect("token lock poisoned")
            .clone();
        check_bearer_header(headers, &expected)
    }
}

// ---------------------------------------------------------------------------
// Mock Entra OAuth endpoint
// ---------------------------------------------------------------------------

async fn oauth_token_handler(State(state): State<Arc<MockState>>, body: String) -> Response {
    record(&state.token_requests, body);
    Json(json!({
        "token_type": "Bearer",
        "expires_in": 3600,
        "access_token": MOCK_ACCESS_TOKEN,
        "refresh_token": MOCK_REFRESH_TOKEN
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Mock OneNote Graph routes
// ---------------------------------------------------------------------------

async fn list_notebooks_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.notebook_gets, params.clone());

    // Two `$top=100` pages to prove `@odata.nextLink` is followed by the client.
    if params.contains_key("$skip") {
        Json(json!({ "value": [notebook_json("nb-archive", "아카이브 노트")] })).into_response()
    } else {
        Json(json!({
            "value": [notebook_json("nb-me", "개인 노트")],
            "@odata.nextLink": format!(
                "{}/v1.0/me/onenote/notebooks?$top=100&$skip=1",
                state.base_url()
            )
        }))
        .into_response()
    }
}

async fn create_notebook_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.notebook_posts, body.clone());
    let name = body
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "nb-created",
            "displayName": name,
            "isDefault": false,
            "isShared": false,
            "color": "Blue",
            "createdDateTime": "2026-11-16T09:00:00Z",
            "lastModifiedDateTime": "2026-11-16T09:00:00Z",
            "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-created" } }
        })),
    )
        .into_response()
}

async fn list_sections_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.section_gets, (None, params));
    Json(json!({
        "value": [section_json("sec-me-1", "주간 회의", "nb-me", "개인 노트")]
    }))
    .into_response()
}

async fn list_notebook_sections_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(notebook_id): Path<String>,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.section_gets, (Some(notebook_id.clone()), params));
    if notebook_id == "nb-me" {
        Json(json!({
            "value": [section_json("sec-me-1", "주간 회의", "nb-me", "개인 노트")]
        }))
        .into_response()
    } else {
        Json(json!({ "value": [] })).into_response()
    }
}

async fn create_section_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(notebook_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.section_posts, (notebook_id.clone(), body.clone()));
    let name = body
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (
        StatusCode::CREATED,
        Json(section_json("sec-created", &name, &notebook_id, "릴리스 노트")),
    )
        .into_response()
}

fn section_groups_payload() -> Value {
    json!({
        "value": [{
            "id": "sg-me-1",
            "displayName": "2026 아카이브",
            "createdDateTime": "2026-11-02T09:00:00Z",
            "lastModifiedDateTime": "2026-11-12T09:00:00Z",
            "parentNotebook": { "id": "nb-me", "displayName": "개인 노트" }
        }]
    })
}

async fn list_section_groups_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.section_group_gets, (None, params));
    Json(section_groups_payload()).into_response()
}

async fn list_notebook_section_groups_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(notebook_id): Path<String>,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.section_group_gets, (Some(notebook_id), params));
    Json(section_groups_payload()).into_response()
}

async fn list_section_pages_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(section_id): Path<String>,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.page_list_gets, (section_id.clone(), params));
    if section_id == "sec-me-1" {
        Json(json!({
            "value": [page_json(
                "pg-me-1",
                "아키텍처 설계",
                "sec-me-1",
                "주간 회의",
                "nb-me",
                "개인 노트",
                0
            )]
        }))
        .into_response()
    } else {
        Json(json!({ "value": [] })).into_response()
    }
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
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    record(
        &state.page_posts,
        (
            section_id.clone(),
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        ),
    );
    (
        StatusCode::CREATED,
        Json(page_json(
            "pg-created",
            "릴리스 체크리스트",
            &section_id,
            "스프린트 42",
            "nb-created",
            "릴리스 노트",
            1,
        )),
    )
        .into_response()
}

async fn get_page_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.page_meta_gets, (page_id.clone(), params));
    Json(page_json(
        &page_id,
        "아키텍처 설계",
        "sec-me-1",
        "주간 회의",
        "nb-me",
        "개인 노트",
        0,
    ))
    .into_response()
}

async fn patch_page_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.page_patches, (page_id, body));
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
    record(&state.deleted_pages, page_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn get_page_content_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Path(page_id): Path<String>,
    Query(params): Query<QueryMap>,
) -> Response {
    if let Err(resp) = state.check_bearer(&headers) {
        return resp;
    }
    record(&state.page_content_gets, (page_id, params));
    (StatusCode::OK, PAGE_XHTML).into_response()
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
    let parsed = serde_json::from_slice::<Value>(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    record(&state.content_patches, (page_id, content_type, parsed));
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// Server bootstrap (mirrors tests/mock_graph_e2e.rs)
// ---------------------------------------------------------------------------

async fn spawn_mock_onenote_server(initial_token: &str) -> (String, Arc<MockState>) {
    let state = Arc::new(MockState::new(initial_token));
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(oauth_token_handler))
        .route(
            "/v1.0/me/onenote/notebooks",
            get(list_notebooks_handler).post(create_notebook_handler),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{notebook_id}/sections",
            get(list_notebook_sections_handler).post(create_section_handler),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{notebook_id}/sectionGroups",
            get(list_notebook_section_groups_handler),
        )
        .route("/v1.0/me/onenote/sections", get(list_sections_handler))
        .route(
            "/v1.0/me/onenote/sectionGroups",
            get(list_section_groups_handler),
        )
        .route(
            "/v1.0/me/onenote/sections/{section_id}/pages",
            get(list_section_pages_handler).post(create_page_handler),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}",
            get(get_page_handler)
                .patch(patch_page_handler)
                .delete(delete_page_handler),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}/content",
            get(get_page_content_handler).patch(patch_page_content_handler),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock OneNote server");
    let addr = listener.local_addr().expect("read mock listener addr");
    let base_url = format!("http://{addr}");
    *state
        .base_url
        .write()
        .expect("base url lock poisoned") = Some(base_url.clone());

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock OneNote server");
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

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

fn find_block<'a>(detail: &'a OnenotePageDetail, element_id: &str) -> &'a OnenoteBlock {
    detail
        .blocks
        .iter()
        .find(|b| b.element_id.as_deref() == Some(element_id))
        .unwrap_or_else(|| panic!("expected seeded block {element_id} in parsed page detail"))
}

fn block_text(block: &OnenoteBlock) -> String {
    match &block.kind {
        OnenoteBlockKind::Heading { spans, .. }
        | OnenoteBlockKind::Paragraph { spans }
        | OnenoteBlockKind::Quote { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
        other => panic!("block {:?} has no inline spans", other),
    }
}

// ---------------------------------------------------------------------------
// The end-to-end product path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_onenote_read_write_through_mock_graph() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");

    let (base_url, mock) = spawn_mock_onenote_server(MOCK_ACCESS_TOKEN).await;
    let cfg = build_test_config(&base_url, &download_dir);

    let tokens = StoredTokens {
        access_token: MOCK_ACCESS_TOKEN.to_string(),
        refresh_token: Some(MOCK_REFRESH_TOKEN.to_string()),
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("user-onenote".to_string()),
        user_display_name: Some("나(테스터)".to_string()),
        user_email: Some("tester@contoso.com".to_string()),
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).expect("build GraphClient");
    let scope = OnenoteScope::Me;

    // -----------------------------------------------------------------------
    // 1. Notebook listing: `$top=100`, `@odata.nextLink` followed, bearer recorded
    // -----------------------------------------------------------------------
    let notebooks = graph
        .list_onenote_notebooks(&scope)
        .await
        .expect("list_onenote_notebooks");
    assert_eq!(
        notebooks.len(),
        2,
        "expected the two seeded notebook pages (nextLink followed), got {notebooks:?}"
    );
    assert_eq!(notebooks[0].notebook_key, "me|nb-me");
    assert_eq!(notebooks[0].display_name, "개인 노트");
    assert_eq!(notebooks[0].scope, OnenoteScope::Me);
    assert_eq!(notebooks[0].user_role.as_deref(), Some("Owner"));
    assert_eq!(notebooks[0].created_by.as_deref(), Some("홍길동"));
    assert_eq!(
        notebooks[0].web_url.as_deref(),
        Some("https://onenote.example.com/nb-me")
    );
    assert_eq!(notebooks[1].notebook_key, "me|nb-archive");

    {
        let gets = mock.notebook_gets.lock().expect("notebook get recorder");
        assert_eq!(gets.len(), 2, "expected first page plus nextLink follow-up");
        assert_eq!(gets[0].get("$top").map(String::as_str), Some("100"));
        assert!(
            !gets[0].contains_key("$skip"),
            "first notebook page must not be the paginated one"
        );
        assert_eq!(gets[1].get("$skip").map(String::as_str), Some("1"));
    }
    {
        let headers = mock.auth_headers.lock().expect("auth header recorder");
        assert_eq!(
            headers.first().map(String::as_str),
            Some("Bearer onenote-access-tok-2026"),
            "mock must receive the stored access token as bearer"
        );
        assert!(
            headers
                .iter()
                .all(|h| h == "Bearer onenote-access-tok-2026"),
            "every Graph request must carry the same valid bearer, saw {headers:?}"
        );
    }

    // -----------------------------------------------------------------------
    // 2. Sections for a notebook (+ through the flat route), section groups, pages
    // -----------------------------------------------------------------------
    let sections = graph
        .list_onenote_sections(&scope, Some("nb-me"))
        .await
        .expect("list_onenote_sections for notebook");
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].section_key, "me|sec-me-1");
    assert_eq!(sections[0].display_name, "주간 회의");
    assert_eq!(sections[0].notebook_id.as_deref(), Some("nb-me"));
    assert_eq!(sections[0].notebook_name.as_deref(), Some("개인 노트"));
    assert_eq!(sections[0].color.as_deref(), Some("Green"));
    assert_eq!(
        sections[0].web_url.as_deref(),
        Some("https://onenote.example.com/sec-me-1")
    );

    let flat_sections = graph
        .list_onenote_sections(&scope, None)
        .await
        .expect("list_onenote_sections flat");
    assert_eq!(flat_sections.len(), 1);
    assert_eq!(flat_sections[0].section_key, "me|sec-me-1");

    let groups = graph
        .list_onenote_section_groups(&scope, Some("nb-me"))
        .await
        .expect("list_onenote_section_groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].section_group_key, "me|sg-me-1");
    assert_eq!(groups[0].display_name, "2026 아카이브");
    assert_eq!(groups[0].notebook_id.as_deref(), Some("nb-me"));

    {
        let gets = mock.section_gets.lock().expect("section get recorder");
        assert_eq!(gets.len(), 2, "notebook-scoped + flat section listing");
        assert_eq!(gets[0].0.as_deref(), Some("nb-me"));
        assert_eq!(gets[0].1.get("$top").map(String::as_str), Some("100"));
        assert_eq!(gets[1].0, None);
        let group_gets = mock
            .section_group_gets
            .lock()
            .expect("section group get recorder");
        assert_eq!(group_gets.len(), 1);
        assert_eq!(group_gets[0].0.as_deref(), Some("nb-me"));
        assert_eq!(group_gets[0].1.get("$top").map(String::as_str), Some("100"));
    }

    let pages = graph
        .list_onenote_pages(&scope, "sec-me-1", 20)
        .await
        .expect("list_onenote_pages");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].page_key, "me|pg-me-1");
    assert_eq!(pages[0].title, "아키텍처 설계");
    assert_eq!(pages[0].section_id, "sec-me-1");
    assert_eq!(pages[0].section_name.as_deref(), Some("주간 회의"));
    assert_eq!(pages[0].notebook_id.as_deref(), Some("nb-me"));
    assert_eq!(pages[0].level, 0);
    assert!(pages[0].content_url.is_some());
    {
        let gets = mock.page_list_gets.lock().expect("page list recorder");
        assert_eq!(gets.len(), 1);
        assert_eq!(gets[0].0, "sec-me-1");
        assert_eq!(gets[0].1.get("$top").map(String::as_str), Some("20"));
        assert_eq!(
            gets[0].1.get("pagelevel").map(String::as_str),
            Some("true"),
            "client must request page levels for the page list"
        );
    }

    // -----------------------------------------------------------------------
    // 3. Page detail: `?pagelevel=true` metadata + XHTML content parse
    // -----------------------------------------------------------------------
    let detail = graph
        .get_onenote_page_detail(&scope, "pg-me-1", None)
        .await
        .expect("get_onenote_page_detail");
    assert_eq!(detail.summary.id, "pg-me-1");
    assert_eq!(detail.summary.title, "아키텍처 설계");
    assert_eq!(detail.primary_div_id.as_deref(), Some(DIV_ID));
    assert!(detail.summary.has_todo, "to-do block must set has_todo");
    assert!(detail.synced_at.is_some());
    assert_eq!(detail.raw_xhtml, PAGE_XHTML);

    let heading = find_block(&detail, HEADING_ID);
    assert!(
        matches!(
            heading.kind,
            OnenoteBlockKind::Heading { level: 1, .. }
        ),
        "seeded h1 must parse as a level-1 heading, got {:?}",
        heading.kind
    );
    assert!(block_text(heading).contains("아키텍처 설계"));

    let paragraph = find_block(&detail, PARAGRAPH_ID);
    assert!(
        matches!(paragraph.kind, OnenoteBlockKind::Paragraph { .. }),
        "seeded <p> must parse as a paragraph, got {:?}",
        paragraph.kind
    );
    assert!(block_text(paragraph).contains("Kubernetes"));

    let todo = find_block(&detail, TODO_ID);
    assert_eq!(
        todo.tag,
        Some(OnenoteTagKind::Todo { completed: false }),
        "data-tag=\"to-do\" must parse as an unchecked todo"
    );
    assert!(
        matches!(todo.kind, OnenoteBlockKind::Paragraph { .. }),
        "todo block must still be a paragraph block"
    );
    assert!(block_text(todo).contains("마이그레이션 점검"));

    assert!(
        detail.body_markdown.contains("# 아키텍처 설계"),
        "heading must round-trip into markdown, got {:?}",
        detail.body_markdown
    );
    assert!(detail
        .body_markdown
        .contains("- [ ] 마이그레이션 점검"));
    {
        let meta_gets = mock.page_meta_gets.lock().expect("page meta recorder");
        assert_eq!(meta_gets.len(), 1);
        assert_eq!(meta_gets[0].0, "pg-me-1");
        assert_eq!(
            meta_gets[0].1.get("pagelevel").map(String::as_str),
            Some("true")
        );
        let content_gets = mock
            .page_content_gets
            .lock()
            .expect("page content recorder");
        assert_eq!(content_gets.len(), 1);
        assert_eq!(content_gets[0].0, "pg-me-1");
        assert_eq!(
            content_gets[0].1.get("includeIDs").map(String::as_str),
            Some("true"),
            "client must ask for element IDs so PATCH targets exist"
        );
    }

    // -----------------------------------------------------------------------
    // 4. Create notebook — assert the POST body the mock received
    // -----------------------------------------------------------------------
    let created_nb = graph
        .create_onenote_notebook(&scope, "릴리스 노트")
        .await
        .expect("create_onenote_notebook");
    assert_eq!(created_nb.notebook_key, "me|nb-created");
    assert_eq!(created_nb.id, "nb-created");
    assert_eq!(created_nb.display_name, "릴리스 노트");
    assert_eq!(created_nb.color.as_deref(), Some("Blue"));
    assert!(!created_nb.is_default);
    assert_eq!(
        created_nb.web_url.as_deref(),
        Some("https://onenote.example.com/nb-created"),
        "returned notebook must be usable for follow-up navigation"
    );
    {
        let posts = mock.notebook_posts.lock().expect("notebook post recorder");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["displayName"], "릴리스 노트");
        assert_eq!(
            posts[0].as_object().map(|o| o.len()),
            Some(1),
            "notebook POST carries only displayName, got {:?}",
            posts[0]
        );
    }

    // -----------------------------------------------------------------------
    // 5. Create section in the new notebook (composite key gets unwrapped)
    // -----------------------------------------------------------------------
    let created_sec = graph
        .create_onenote_section(&scope, &created_nb.notebook_key, "스프린트 42")
        .await
        .expect("create_onenote_section");
    assert_eq!(created_sec.section_key, "me|sec-created");
    assert_eq!(created_sec.id, "sec-created");
    assert_eq!(created_sec.display_name, "스프린트 42");
    assert_eq!(created_sec.notebook_id.as_deref(), Some("nb-created"));
    assert_eq!(created_sec.notebook_name.as_deref(), Some("릴리스 노트"));
    assert_eq!(created_sec.scope, OnenoteScope::Me);
    {
        let posts = mock.section_posts.lock().expect("section post recorder");
        assert_eq!(posts.len(), 1);
        assert_eq!(
            posts[0].0, "nb-created",
            "composite notebook key must be reduced to the raw Graph id"
        );
        assert_eq!(posts[0].1["displayName"], "스프린트 42");
    }

    // -----------------------------------------------------------------------
    // 6. Create page — compiled XHTML payload + returned summary
    // -----------------------------------------------------------------------
    let outgoing = OutgoingOnenotePage {
        title: "릴리스 체크리스트".to_string(),
        markdown_body: "## 배포 준비\n\n- [ ] 태그 정리\n- [x] 릴리스 노트 작성".to_string(),
        inline_images: Vec::new(),
        level: Some(1),
    };
    let created_page = graph
        .create_onenote_page(&scope, "sec-created", &outgoing)
        .await
        .expect("create_onenote_page");
    assert_eq!(created_page.page_key, "me|pg-created");
    assert_eq!(created_page.id, "pg-created");
    assert_eq!(created_page.title, "릴리스 체크리스트");
    assert_eq!(created_page.section_id, "sec-created");
    assert_eq!(created_page.level, 1);
    {
        let posts = mock.page_posts.lock().expect("page post recorder");
        assert_eq!(posts.len(), 1);
        let (section_id, content_type, body) = &posts[0];
        assert_eq!(section_id, "sec-created");
        assert!(
            content_type.starts_with("application/xhtml+xml"),
            "new pages are posted as XHTML, got {content_type:?}"
        );
        assert!(
            body.contains("<title>릴리스 체크리스트</title>"),
            "compiled XHTML must carry the page title, got {body}"
        );
        assert!(
            body.contains("배포 준비") && body.contains("<h2>배포 준비</h2>"),
            "markdown heading must compile to XHTML, got {body}"
        );
        assert!(body.contains("태그 정리"));
        assert!(body.contains("릴리스 노트 작성"));
        assert!(
            body.contains("data-tag=\"to-do\"") && body.contains("data-tag=\"to-do:completed\""),
            "task-list markdown must compile to to-do tags, got {body}"
        );
    }
    {
        let patches = mock.page_patches.lock().expect("page patch recorder");
        assert_eq!(patches.len(), 1, "subpage level triggers one PATCH");
        assert_eq!(patches[0].0, "pg-created");
        assert_eq!(patches[0].1["level"], 1);
    }

    // -----------------------------------------------------------------------
    // 7. Append Markdown — assert the PATCH command the mock received
    // -----------------------------------------------------------------------
    graph
        .append_onenote_page_markdown(
            &scope,
            "pg-me-1",
            detail.primary_div_id.as_deref(),
            "## 후속 조치\n\n- 배포 창구 확정",
            &[],
        )
        .await
        .expect("append_onenote_page_markdown");
    {
        let patches = mock.content_patches.lock().expect("content patch recorder");
        assert_eq!(patches.len(), 1);
        let (page_id, content_type, body) = &patches[0];
        assert_eq!(page_id, "pg-me-1");
        assert_eq!(content_type, "application/json");
        let cmds = body
            .as_array()
            .unwrap_or_else(|| panic!("content PATCH body must be a command array, got {body}"));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["target"], DIV_ID);
        assert_eq!(cmds[0]["action"], "append");
        assert!(
            cmds[0].get("position").is_none(),
            "append commands omit position, got {:?}",
            cmds[0]
        );
        let content = cmds[0]["content"]
            .as_str()
            .expect("append command content is a string");
        assert!(content.contains("후속 조치"), "got {content}");
        assert!(content.contains("배포 창구 확정"), "got {content}");
    }

    // -----------------------------------------------------------------------
    // 8. Toggle the seeded to-do block via a surgical `replace` command
    // -----------------------------------------------------------------------
    let (toggle_cmd, now_completed) =
        parser::build_toggle_todo_patch(todo).expect("todo block yields a toggle patch");
    assert!(now_completed, "unchecked to-do toggles to completed");
    assert_eq!(toggle_cmd.target, TODO_ID);
    assert_eq!(toggle_cmd.action, "replace");

    graph
        .patch_onenote_page_commands(&scope, "pg-me-1", &[toggle_cmd])
        .await
        .expect("patch_onenote_page_commands");
    {
        let patches = mock.content_patches.lock().expect("content patch recorder");
        assert_eq!(patches.len(), 2);
        let (page_id, content_type, body) = &patches[1];
        assert_eq!(page_id, "pg-me-1");
        assert_eq!(content_type, "application/json");
        assert_eq!(body[0]["target"], TODO_ID);
        assert_eq!(body[0]["action"], "replace");
        let content = body[0]["content"]
            .as_str()
            .expect("replace command content is a string");
        assert!(
            content.contains("data-tag=\"to-do:completed\""),
            "replace must flip the data-tag in place, got {content}"
        );
        assert!(
            content.contains("마이그레이션 점검"),
            "replace must preserve the block text, got {content}"
        );
    }

    // -----------------------------------------------------------------------
    // 9. Delete page — assert the DELETE path the mock received
    // -----------------------------------------------------------------------
    graph
        .delete_onenote_page(&scope, "pg-created")
        .await
        .expect("delete_onenote_page");
    {
        let deleted = mock.deleted_pages.lock().expect("delete recorder");
        assert_eq!(deleted.as_slice(), ["pg-created".to_string()]);
    }

    // -----------------------------------------------------------------------
    // 10. Cache the hierarchy + detail in SQLite and search it offline
    // -----------------------------------------------------------------------
    let storage = Storage::open(&db_path).expect("open SQLite storage");
    storage
        .upsert_onenote_notebooks(&notebooks)
        .expect("upsert notebooks");
    storage
        .upsert_onenote_sections(&sections)
        .expect("upsert sections");
    storage
        .upsert_onenote_sections(std::slice::from_ref(&created_sec))
        .expect("upsert created section");
    storage
        .upsert_onenote_page_detail(&detail)
        .expect("upsert page detail");

    let hits = storage
        .search_onenote_pages("Kubernetes", None, 10)
        .expect("search_onenote_pages");
    assert_eq!(hits.len(), 1, "expected one cached hit, got {hits:?}");
    assert_eq!(hits[0].page_key, "me|pg-me-1");
    assert_eq!(hits[0].page_id, "pg-me-1");
    assert_eq!(hits[0].title, "아키텍처 설계");
    assert_eq!(hits[0].section_id, "sec-me-1");
    assert_eq!(hits[0].section_name.as_deref(), Some("주간 회의"));
    assert_eq!(hits[0].scope, OnenoteScope::Me);
    assert!(
        hits[0].snippet.contains("Kubernetes"),
        "snippet must show the match, got {:?}",
        hits[0].snippet
    );

    let scoped_hits = storage
        .search_onenote_pages("Kubernetes", Some("me|sec-me-1"), 10)
        .expect("search_onenote_pages with section filter");
    assert_eq!(scoped_hits.len(), 1);
    assert_eq!(scoped_hits[0].page_key, "me|pg-me-1");

    let other_section_hits = storage
        .search_onenote_pages("Kubernetes", Some("me|sec-created"), 10)
        .expect("search_onenote_pages for unrelated section");
    assert!(
        other_section_hits.is_empty(),
        "section filter must exclude other sections, got {other_section_hits:?}"
    );

    let cached_pages = storage
        .list_onenote_pages("me|sec-me-1")
        .expect("list cached pages");
    assert_eq!(cached_pages.len(), 1);
    assert_eq!(cached_pages[0].page_key, "me|pg-me-1");

    let cached_detail = storage
        .get_onenote_page_detail("me|pg-me-1")
        .expect("get cached detail")
        .expect("cached detail row present");
    assert_eq!(cached_detail.summary.title, "아키텍처 설계");
    assert_eq!(cached_detail.blocks.len(), detail.blocks.len());

    // No OAuth refresh should have been needed: the stored token was still valid.
    assert!(
        mock.token_requests
            .lock()
            .expect("token request recorder")
            .is_empty(),
        "valid stored tokens must not trigger a refresh"
    );
    assert!(
        mock.auth_headers
            .lock()
            .expect("auth header recorder")
            .len()
            >= 14,
        "expected every Graph call to hit the mock's bearer check"
    );
}
