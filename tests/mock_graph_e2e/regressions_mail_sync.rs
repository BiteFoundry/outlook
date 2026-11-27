use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, RwLock};
use axum::{
    extract::{Path, RawQuery},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use outlook_cli::{
    auth::{AuthManager, StoredTokens},
    graph::GraphClient,
    models::CalendarEvent,
    serve,
    storage::Storage,
};

use crate::server::*;

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
