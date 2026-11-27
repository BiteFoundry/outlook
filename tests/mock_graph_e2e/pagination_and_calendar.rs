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
    storage::Storage,
};

use crate::server::*;

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
