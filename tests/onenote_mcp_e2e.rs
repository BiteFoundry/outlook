//! End-to-end integration test for the 13 OneNote MCP tools against an in-process
//! mock Microsoft Graph v1.0 + Entra OAuth server.
//!
//! Harness helpers are intentionally duplicated from `tests/mock_graph_e2e.rs` so this
//! surface can be exercised in isolation (no shared module, no extra dependencies).

use std::{
    collections::HashSet,
    sync::{Arc, Once, OnceLock, RwLock},
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use image::{Rgba, RgbaImage};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use teams_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    mcp,
    storage::Storage,
};

const INITIAL_TOKEN: &str = "onenote-expired-tok";
const REFRESH_TOKEN: &str = "onenote-refresh-tok";
const FRESH_TOKEN: &str = "onenote-fresh-tok";

const NB_PATH: &str = "/v1.0/me/onenote/notebooks";
const SECTIONS_PATH: &str = "/v1.0/me/onenote/sections";
const SECTION_GROUPS_PATH: &str = "/v1.0/me/onenote/sectionGroups";

/// Page XHTML served by `GET .../pages/{id}/content`. `__RESOURCE_URL__` is replaced
/// with an absolute mock-server URL so `download_onenote_resource_bytes` can fetch it.
const PAGE_XHTML: &str = r#"<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>릴리스 점검 노트</title>
    <meta name="created" content="2026-11-15T09:00:00Z" />
  </head>
  <body>
    <div id="div:{1111}{32}">
      <h1 id="h1:{3333}{10}">릴리스 점검 노트</h1>
      <p id="p:{2222}{40}" data-tag="to-do">마이그레이션 점검 항목</p>
      <p id="p:{2222}{41}"><img id="img:{4444}{50}" src="__RESOURCE_URL__" data-src-type="image/png" width="320" height="200" alt="스크린샷" /></p>
    </div>
  </body>
</html>"#;

const RESOURCE_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nonenote-resource-payload-2026";

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

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

#[derive(Default)]
struct MockState {
    oauth_calls: RwLock<usize>,
    next_page_seq: RwLock<u64>,
    requests: RwLock<Vec<RecordedRequest>>,
}

impl MockState {
    fn record(&self, method: &str, path: impl Into<String>, headers: &HeaderMap, body: &[u8]) {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.requests.write().unwrap().push(RecordedRequest {
            method: method.to_string(),
            path: path.into(),
            auth,
            body: String::from_utf8_lossy(body).into_owned(),
        });
    }

    fn calls(&self, method: &str, path: &str) -> Vec<RecordedRequest> {
        self.requests
            .read()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .cloned()
            .collect()
    }

    fn saw(&self, method: &str, path: &str, body_fragment: &str) -> bool {
        self.calls(method, path)
            .iter()
            .any(|r| body_fragment.is_empty() || r.body.contains(body_fragment))
    }

    fn next_page_id(&self) -> String {
        let mut seq = self.next_page_seq.write().unwrap();
        *seq += 1;
        format!("pg-created-{}", *seq)
    }
}

// ---------------------------------------------------------------------------
// Mock Graph / OAuth handlers
// ---------------------------------------------------------------------------

async fn oauth_token_handler(
    State(state): State<Arc<MockState>>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    state.record(
        "POST",
        format!("/{tenant}/oauth2/v2.0/token"),
        &headers,
        body.as_bytes(),
    );
    *state.oauth_calls.write().unwrap() += 1;
    Json(json!({
        "token_type": "Bearer",
        "expires_in": 3600,
        "access_token": FRESH_TOKEN,
        "refresh_token": "onenote-rotated-refresh-tok"
    }))
    .into_response()
}

async fn list_notebooks_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    state.record("GET", NB_PATH, &headers, b"");
    Json(json!({
        "value": [{
            "id": "nb-1",
            "displayName": "릴리스 노트",
            "isDefault": true,
            "isShared": false,
            "userRole": "Owner",
            "createdDateTime": "2026-11-14T09:00:00Z",
            "lastModifiedDateTime": "2026-11-16T09:00:00Z",
            "createdBy": { "user": { "displayName": "테스터" } },
            "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-1" } }
        }]
    }))
    .into_response()
}

async fn create_notebook_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.record("POST", NB_PATH, &headers, &body);
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let name = parsed["displayName"].as_str().unwrap_or("").to_string();
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "nb-created",
            "displayName": name,
            "isDefault": false,
            "isShared": false
        })),
    )
        .into_response()
}

async fn list_sections_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    state.record("GET", SECTIONS_PATH, &headers, b"");
    Json(json!({
        "value": [{
            "id": "sec-1",
            "displayName": "주간 회의",
            "isDefault": true,
            "parentNotebook": { "id": "nb-1", "displayName": "릴리스 노트" }
        }]
    }))
    .into_response()
}

async fn list_section_groups_handler(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Response {
    state.record("GET", SECTION_GROUPS_PATH, &headers, b"");
    Json(json!({ "value": [] })).into_response()
}

async fn notebook_sections_handler(
    State(state): State<Arc<MockState>>,
    Path(notebook_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "GET",
        format!("/v1.0/me/onenote/notebooks/{notebook_id}/sections"),
        &headers,
        b"",
    );
    Json(json!({
        "value": [{
            "id": "sec-1",
            "displayName": "주간 회의",
            "isDefault": true,
            "parentNotebook": { "id": notebook_id, "displayName": "릴리스 노트" }
        }]
    }))
    .into_response()
}

async fn notebook_section_groups_handler(
    State(state): State<Arc<MockState>>,
    Path(notebook_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "GET",
        format!("/v1.0/me/onenote/notebooks/{notebook_id}/sectionGroups"),
        &headers,
        b"",
    );
    Json(json!({ "value": [] })).into_response()
}

async fn create_section_handler(
    State(state): State<Arc<MockState>>,
    Path(notebook_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.record(
        "POST",
        format!("/v1.0/me/onenote/notebooks/{notebook_id}/sections"),
        &headers,
        &body,
    );
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let name = parsed["displayName"].as_str().unwrap_or("").to_string();
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "sec-created",
            "displayName": name,
            "parentNotebook": { "id": notebook_id, "displayName": "릴리스 노트" }
        })),
    )
        .into_response()
}

async fn list_pages_handler(
    State(state): State<Arc<MockState>>,
    Path(section_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "GET",
        format!("/v1.0/me/onenote/sections/{section_id}/pages"),
        &headers,
        b"",
    );
    Json(json!({
        "value": [
            {
                "id": "pg-1",
                "title": "릴리스 점검 노트",
                "level": 0,
                "order": 0,
                "createdDateTime": "2026-11-15T09:00:00Z",
                "lastModifiedDateTime": "2026-11-16T09:00:00Z",
                "parentSection": { "id": section_id, "displayName": "주간 회의" }
            },
            {
                "id": "pg-2",
                "title": "두 번째 페이지",
                "level": 0,
                "order": 1,
                "createdDateTime": "2026-11-15T10:00:00Z",
                "lastModifiedDateTime": "2026-11-15T10:00:00Z",
                "parentSection": { "id": section_id, "displayName": "주간 회의" }
            }
        ]
    }))
    .into_response()
}

async fn create_page_handler(
    State(state): State<Arc<MockState>>,
    Path(section_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.record(
        "POST",
        format!("/v1.0/me/onenote/sections/{section_id}/pages"),
        &headers,
        &body,
    );
    let page_id = state.next_page_id();
    (
        StatusCode::CREATED,
        Json(json!({
            "id": page_id,
            "title": "",
            "level": 0,
            "order": 0,
            "parentSection": { "id": section_id, "displayName": "주간 회의" },
            "parentNotebook": { "id": "nb-1", "displayName": "릴리스 노트" }
        })),
    )
        .into_response()
}

async fn page_meta_handler(
    State(state): State<Arc<MockState>>,
    Path(page_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "GET",
        format!("/v1.0/me/onenote/pages/{page_id}"),
        &headers,
        b"",
    );
    Json(json!({
        "id": page_id,
        "title": "릴리스 점검 노트",
        "level": 0,
        "order": 0,
        "parentSection": { "id": "sec-1", "displayName": "주간 회의" },
        "parentNotebook": { "id": "nb-1", "displayName": "릴리스 노트" }
    }))
    .into_response()
}

async fn delete_page_handler(
    State(state): State<Arc<MockState>>,
    Path(page_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "DELETE",
        format!("/v1.0/me/onenote/pages/{page_id}"),
        &headers,
        b"",
    );
    StatusCode::NO_CONTENT.into_response()
}

async fn page_content_get_handler(
    State(state): State<Arc<MockState>>,
    Path(page_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "GET",
        format!("/v1.0/me/onenote/pages/{page_id}/content"),
        &headers,
        b"",
    );
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    let resource_url = format!("http://{host}/v1.0/me/onenote/resources/res-1/content");
    PAGE_XHTML.replace("__RESOURCE_URL__", &resource_url).into_response()
}

async fn page_content_patch_handler(
    State(state): State<Arc<MockState>>,
    Path(page_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.record(
        "PATCH",
        format!("/v1.0/me/onenote/pages/{page_id}/content"),
        &headers,
        &body,
    );
    StatusCode::NO_CONTENT.into_response()
}

async fn resource_handler(
    State(state): State<Arc<MockState>>,
    Path(resource_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.record(
        "GET",
        format!("/v1.0/me/onenote/resources/{resource_id}/content"),
        &headers,
        b"",
    );
    RESOURCE_BYTES.to_vec().into_response()
}

async fn spawn_mock_graph_server(initial_token: &str) -> (String, Arc<MockState>) {
    let _ = initial_token;
    let state = Arc::new(MockState::default());
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(oauth_token_handler))
        .route(
            NB_PATH,
            get(list_notebooks_handler).post(create_notebook_handler),
        )
        .route(SECTIONS_PATH, get(list_sections_handler))
        .route(SECTION_GROUPS_PATH, get(list_section_groups_handler))
        .route(
            "/v1.0/me/onenote/notebooks/{notebook_id}/sections",
            get(notebook_sections_handler).post(create_section_handler),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{notebook_id}/sectionGroups",
            get(notebook_section_groups_handler),
        )
        .route(
            "/v1.0/me/onenote/sections/{section_id}/pages",
            get(list_pages_handler).post(create_page_handler),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}",
            get(page_meta_handler).delete(delete_page_handler),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}/content",
            get(page_content_get_handler).patch(page_content_patch_handler),
        )
        .route(
            "/v1.0/me/onenote/resources/{resource_id}/content",
            get(resource_handler),
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

// ---------------------------------------------------------------------------
// MCP helpers
// ---------------------------------------------------------------------------

async fn call_tool(
    cfg: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    mcp::handle_rpc_line(cfg, graph, storage, &request.to_string())
        .await
        .expect("mcp tools/call response")
}

/// Asserts the envelope reports success and returns the parsed JSON payload text.
fn ok_payload(resp: &Value, tool: &str) -> Value {
    assert_eq!(
        resp["result"]["isError"],
        Value::Bool(false),
        "{tool} failed: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool} returned no text content: {resp}"));
    serde_json::from_str(text).unwrap_or_else(|err| panic!("{tool} payload not JSON: {err}\n{text}"))
}

fn error_text(resp: &Value) -> String {
    assert_eq!(
        resp["result"]["isError"],
        Value::Bool(true),
        "expected a tool error: {resp}"
    );
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn e2e_onenote_mcp_tools() {
    ensure_isolated_env();
    let temp = tempfile::tempdir().expect("create onenote test tempdir");
    let db_path = temp.path().join("data").join("cache.db");
    let download_dir = temp.path().join("downloads");

    let image_path = temp.path().join("screenshot.png");
    RgbaImage::from_pixel(8, 8, Rgba([10u8, 20, 30, 255]))
        .save(&image_path)
        .expect("write test png");

    let (base_url, mock) = spawn_mock_graph_server(INITIAL_TOKEN).await;
    let cfg = build_test_config(&base_url, &download_dir);

    // Expired token forces the OAuth refresh flow before the first Graph call.
    let tokens = StoredTokens {
        access_token: INITIAL_TOKEN.to_string(),
        refresh_token: Some(REFRESH_TOKEN.to_string()),
        expires_at: chrono::Utc::now().timestamp() - 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: None,
        user_display_name: None,
        user_email: None,
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth).expect("build GraphClient");
    let storage = Storage::open(&db_path).expect("open SQLite storage");

    // -----------------------------------------------------------------------
    // 1. MCP handshake + catalog
    // -----------------------------------------------------------------------
    let init_resp = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"onenote-e2e","version":"0.1"}}}"#,
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
    let catalog = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone();
    let onenote_tools: Vec<&Value> = catalog
        .iter()
        .filter(|t| {
            t["name"]
                .as_str()
                .map(|n| n.starts_with("onenote_"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(onenote_tools.len(), 13, "expected 13 onenote tools");

    let mut onenote_names: Vec<&str> = onenote_tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    let unique: HashSet<&str> = onenote_names.iter().copied().collect();
    assert_eq!(
        unique.len(),
        onenote_names.len(),
        "onenote tool names must be unique"
    );
    onenote_names.sort_unstable();
    assert_eq!(
        onenote_names,
        vec![
            "onenote_append_page",
            "onenote_create_notebook",
            "onenote_create_page",
            "onenote_create_section",
            "onenote_delete_page",
            "onenote_download_resource",
            "onenote_get_page",
            "onenote_list_notebooks",
            "onenote_list_pages",
            "onenote_list_sections",
            "onenote_search",
            "onenote_toggle_todo",
            "onenote_update_page",
        ]
    );

    // Schema/validator agreement: create/append page accept image-only payloads.
    for tool_name in ["onenote_create_page", "onenote_append_page"] {
        let tool = catalog
            .iter()
            .find(|t| t["name"].as_str() == Some(tool_name))
            .unwrap_or_else(|| panic!("{tool_name} missing from catalog"));
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["anyOf"].as_array().map(Vec::len),
            Some(2),
            "{tool_name} schema must advertise the two-branch anyOf rule"
        );
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            !required.contains(&"markdown_body"),
            "{tool_name} must not require markdown_body"
        );
        assert!(
            !required.contains(&"image_paths"),
            "{tool_name} must not require image_paths"
        );
    }

    // -----------------------------------------------------------------------
    // 2. Read-only tools
    // -----------------------------------------------------------------------
    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        3,
        "onenote_list_notebooks",
        json!({ "include_teams": false }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_list_notebooks");
    assert_eq!(payload["notebooks"][0]["notebook_key"], "me|nb-1");
    assert_eq!(payload["notebooks"][0]["display_name"], "릴리스 노트");
    let nb_calls = mock.calls("GET", NB_PATH);
    assert_eq!(nb_calls.len(), 1);
    let expected_auth = format!("Bearer {FRESH_TOKEN}");
    assert_eq!(
        nb_calls[0].auth.as_deref(),
        Some(expected_auth.as_str()),
        "refreshed access token must be used"
    );
    assert_eq!(
        *mock.oauth_calls.read().unwrap(),
        1,
        "expected exactly one OAuth refresh"
    );
    let oauth_calls = mock.calls("POST", "/test-tenant/oauth2/v2.0/token");
    assert_eq!(oauth_calls.len(), 1, "expected one OAuth refresh");
    assert!(
        oauth_calls[0].body.contains("grant_type=refresh_token"),
        "oauth body: {}",
        oauth_calls[0].body
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        4,
        "onenote_list_sections",
        json!({ "notebook": "me|nb-1" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_list_sections");
    assert_eq!(payload["notebook"]["id"], "nb-1");
    assert_eq!(payload["sections"][0]["id"], "sec-1");
    assert_eq!(payload["sections"][0]["section_key"], "me|sec-1");
    assert!(mock.saw(
        "GET",
        "/v1.0/me/onenote/notebooks/nb-1/sections",
        ""
    ));

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        5,
        "onenote_list_pages",
        json!({ "section": "me|sec-1", "limit": 20 }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_list_pages");
    assert_eq!(payload["section"]["section_key"], "me|sec-1");
    assert_eq!(payload["pages"].as_array().map(Vec::len), Some(2));
    assert_eq!(payload["pages"][0]["page_key"], "me|pg-1");
    assert!(mock.saw(
        "GET",
        "/v1.0/me/onenote/sections/sec-1/pages",
        ""
    ));

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        6,
        "onenote_get_page",
        json!({ "page": "me|pg-1" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_get_page");
    assert_eq!(payload["page"]["summary"]["id"], "pg-1");
    assert!(
        payload["page"]["body_markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("마이그레이션 점검 항목"),
        "body markdown: {}",
        payload["page"]["body_markdown"]
    );
    assert!(
        payload["page"]["blocks"]
            .as_array()
            .map(|b| !b.is_empty())
            .unwrap_or(false),
        "expected parsed blocks"
    );
    assert_eq!(payload["page"]["primary_div_id"], "div:{1111}{32}");

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        7,
        "onenote_download_resource",
        json!({ "page": "me|pg-1", "index": 0 }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_download_resource");
    assert_eq!(payload["content_type"], "image/png");
    assert_eq!(payload["is_image"], true);
    assert_eq!(payload["size"].as_u64(), Some(RESOURCE_BYTES.len() as u64));
    let decoded = BASE64_STANDARD
        .decode(payload["bytes_base64"].as_str().expect("bytes_base64"))
        .expect("decode bytes_base64");
    assert_eq!(decoded, RESOURCE_BYTES);
    assert!(mock.saw(
        "GET",
        "/v1.0/me/onenote/resources/res-1/content",
        ""
    ));

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        8,
        "onenote_search",
        json!({ "query": "마이그레이션" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_search");
    assert!(
        payload["hits"]
            .as_array()
            .map(|h| !h.is_empty())
            .unwrap_or(false),
        "expected a cached search hit: {payload}"
    );

    // -----------------------------------------------------------------------
    // 3. Mutating tools (each must hit Graph with the expected request)
    // -----------------------------------------------------------------------
    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        9,
        "onenote_create_notebook",
        json!({ "name": "신규 노트" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_create_notebook");
    assert_eq!(payload["notebook"]["id"], "nb-created");
    assert!(
        mock.saw("POST", NB_PATH, "신규 노트"),
        "create notebook POST not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        10,
        "onenote_create_section",
        json!({ "notebook": "me|nb-1", "name": "스프린트" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_create_section");
    assert_eq!(payload["section"]["id"], "sec-created");
    assert!(
        mock.saw(
            "POST",
            "/v1.0/me/onenote/notebooks/nb-1/sections",
            "스프린트"
        ),
        "create section POST not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        11,
        "onenote_create_page",
        json!({
            "section": "me|sec-1",
            "title": "새 페이지",
            "markdown_body": "# 새 페이지\n\n본문입니다."
        }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_create_page");
    assert!(
        payload["page"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("pg-created-"),
        "created page id: {}",
        payload["page"]["id"]
    );
    assert_eq!(payload["page"]["title"], "새 페이지");
    assert!(
        mock.saw(
            "POST",
            "/v1.0/me/onenote/sections/sec-1/pages",
            "새 페이지"
        ),
        "create page POST not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        12,
        "onenote_append_page",
        json!({ "page": "me|pg-1", "markdown_body": "추가 메모 본문" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_append_page");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["page_id"], "pg-1");
    assert!(
        mock.saw(
            "PATCH",
            "/v1.0/me/onenote/pages/pg-1/content",
            "추가 메모 본문"
        ),
        "append PATCH not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        13,
        "onenote_update_page",
        json!({ "page": "me|pg-1", "markdown_body": "교체 본문 내용" }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_update_page");
    assert_eq!(payload["ok"], true);
    assert!(
        payload["commands_applied"].as_u64().unwrap_or(0) >= 1,
        "expected at least one patch command: {payload}"
    );
    assert!(
        mock.saw(
            "PATCH",
            "/v1.0/me/onenote/pages/pg-1/content",
            "교체 본문 내용"
        ),
        "update PATCH not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        14,
        "onenote_toggle_todo",
        json!({ "page": "me|pg-1", "todo_index": 0 }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_toggle_todo");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["completed"], true);
    assert!(
        mock.saw(
            "PATCH",
            "/v1.0/me/onenote/pages/pg-1/content",
            "to-do:completed"
        ),
        "toggle-todo PATCH not observed"
    );

    // -----------------------------------------------------------------------
    // 4. Destructive guard: no confirm => advanced rejection, no DELETE
    // -----------------------------------------------------------------------
    let before_guard = mock.calls("DELETE", "/v1.0/me/onenote/pages/pg-1").len();
    assert_eq!(before_guard, 0, "no DELETE expected before the guard test");

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        15,
        "onenote_delete_page",
        json!({ "page": "me|pg-1" }),
    )
    .await;
    let msg = error_text(&resp);
    assert!(
        msg.contains("confirm"),
        "delete guard message should mention confirm: {msg}"
    );
    assert_eq!(
        mock.calls("DELETE", "/v1.0/me/onenote/pages/pg-1").len(),
        0,
        "DELETE must not be sent without confirm"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        16,
        "onenote_delete_page",
        json!({ "page": "me|pg-1", "confirm": false }),
    )
    .await;
    let msg = error_text(&resp);
    assert!(
        msg.contains("confirm"),
        "delete guard message should mention confirm: {msg}"
    );
    assert_eq!(
        mock.calls("DELETE", "/v1.0/me/onenote/pages/pg-1").len(),
        0,
        "DELETE must not be sent with confirm=false"
    );

    // -----------------------------------------------------------------------
    // 5. Image-only payloads are accepted; empty payloads are rejected
    // -----------------------------------------------------------------------
    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        17,
        "onenote_create_page",
        json!({
            "section": "me|sec-1",
            "title": "이미지만 있는 페이지",
            "image_paths": [image_path.to_string_lossy()]
        }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_create_page(image-only)");
    assert!(
        payload["page"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("pg-created-"),
        "image-only page id: {}",
        payload["page"]["id"]
    );
    assert!(
        mock.saw(
            "POST",
            "/v1.0/me/onenote/sections/sec-1/pages",
            "name:img1"
        ),
        "image-only create page payload not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        18,
        "onenote_append_page",
        json!({ "page": "me|pg-1", "image_paths": [image_path.to_string_lossy()] }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_append_page(image-only)");
    assert_eq!(payload["ok"], true);
    assert!(
        mock.saw(
            "PATCH",
            "/v1.0/me/onenote/pages/pg-1/content",
            "name:img1"
        ),
        "image-only append payload not observed"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        19,
        "onenote_create_page",
        json!({ "section": "me|sec-1", "title": "빈 페이지" }),
    )
    .await;
    let msg = error_text(&resp);
    assert!(
        msg.contains("at least one of markdown_body or image_paths"),
        "create page empty-payload message: {msg}"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        20,
        "onenote_create_page",
        json!({ "section": "me|sec-1", "title": "빈 페이지", "markdown_body": "   " }),
    )
    .await;
    let msg = error_text(&resp);
    assert!(
        msg.contains("at least one of markdown_body or image_paths"),
        "create page blank-markdown message: {msg}"
    );

    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        21,
        "onenote_append_page",
        json!({ "page": "me|pg-1" }),
    )
    .await;
    let msg = error_text(&resp);
    assert!(
        msg.contains("at least one of markdown_body or image_paths"),
        "append page empty-payload message: {msg}"
    );

    // -----------------------------------------------------------------------
    // 6. Confirmed destructive call reaches Graph
    // -----------------------------------------------------------------------
    let resp = call_tool(
        &cfg,
        &graph,
        &storage,
        22,
        "onenote_delete_page",
        json!({ "page": "me|pg-1", "confirm": true }),
    )
    .await;
    let payload = ok_payload(&resp, "onenote_delete_page");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["page_id"], "pg-1");
    assert_eq!(
        mock.calls("DELETE", "/v1.0/me/onenote/pages/pg-1").len(),
        1,
        "exactly one DELETE expected after confirm"
    );
}
