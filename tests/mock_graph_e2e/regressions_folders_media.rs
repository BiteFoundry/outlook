use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, RwLock};
use axum::{
    extract::{Path, RawQuery},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use outlook_cli::{
    auth::{AuthManager, StoredTokens},
    graph::GraphClient,
    models::{InlineImageUpload, MailMessage, OutgoingMail},
    parser,
    serve,
    storage::Storage,
};

use crate::server::*;

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
        outlook_cli::hex_encode(Sha256::digest(&saved_cached)),
        outlook_cli::hex_encode(Sha256::digest(&cached_bytes))
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
