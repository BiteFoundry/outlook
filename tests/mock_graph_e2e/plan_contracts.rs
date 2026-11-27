use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, RwLock};
use axum::{extract::{Path, RawQuery}, routing::get, Json, Router};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::server::*;

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
