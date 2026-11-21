//! Real built-CLI end-to-end tests against a local loopback mock Microsoft Graph server.
//!
//! Verifies all audited hyphen-prefixed text/Markdown CLI inputs, aliases, omitted arguments,
//! stdin composition, missing-value/flag protection, subject-only draft PATCH preservation,
//! Markdown list rendering, and plain-text To Do notes / RSVP comments.

use std::{
    io::Write,
    process::{Command, Output, Stdio},
    sync::{Arc, RwLock},
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
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

const RICH_DRAFT_HTML: &str = r#"<p>Review <a href="https://example.com/spec">the specification</a>.</p><p><img src="cid:diagram01@contoso" alt="Architecture Diagram"></p>"#;

#[derive(Clone)]
struct DraftFixture {
    subject: String,
    body_html: String,
    inline_png_b64: String,
    pdf_b64: String,
}

struct MockCliGraphState {
    expected_token: String,
    draft: RwLock<DraftFixture>,
    sent_mails: RwLock<Vec<Value>>,
    replies: RwLock<Vec<(String, bool, Value)>>,
    forwards: RwLock<Vec<(String, Value)>>,
    draft_patches: RwLock<Vec<(String, Value)>>,
    created_events: RwLock<Vec<Value>>,
    rsvp_calls: RwLock<Vec<(String, String, Value)>>,
    created_tasks: RwLock<Vec<(String, Value)>>,
    patched_tasks: RwLock<Vec<(String, String, Value)>>,
    search_queries: RwLock<Vec<String>>,
}

impl MockCliGraphState {
    fn new(expected_token: String) -> Self {
        Self {
            expected_token,
            draft: RwLock::new(DraftFixture {
                subject: "Original Draft Subject".to_string(),
                body_html: RICH_DRAFT_HTML.to_string(),
                inline_png_b64: B64_STANDARD.encode(b"\x89PNG\r\n\x1a\nmock-inline"),
                pdf_b64: B64_STANDARD.encode(b"%PDF-1.4 mock attachment"),
            }),
            sent_mails: RwLock::new(Vec::new()),
            replies: RwLock::new(Vec::new()),
            forwards: RwLock::new(Vec::new()),
            draft_patches: RwLock::new(Vec::new()),
            created_events: RwLock::new(Vec::new()),
            rsvp_calls: RwLock::new(Vec::new()),
            created_tasks: RwLock::new(Vec::new()),
            patched_tasks: RwLock::new(Vec::new()),
            search_queries: RwLock::new(Vec::new()),
        }
    }

    fn total_writes(&self) -> usize {
        self.sent_mails.read().unwrap().len()
            + self.replies.read().unwrap().len()
            + self.forwards.read().unwrap().len()
            + self.draft_patches.read().unwrap().len()
            + self.created_events.read().unwrap().len()
            + self.rsvp_calls.read().unwrap().len()
            + self.created_tasks.read().unwrap().len()
            + self.patched_tasks.read().unwrap().len()
    }
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
            Json(json!({ "error": { "code": "InvalidAuthenticationToken", "message": "bad token" } })),
        )
            .into_response())
    }
}

fn make_mock_jwt() -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let exp = chrono::Utc::now().timestamp() + 7200;
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "oid": "cli-test-user",
            "upn": "alice@contoso.com",
            "name": "Alice Outlook",
            "scp": "User.Read Mail.ReadWrite Mail.Send Calendars.ReadWrite Tasks.ReadWrite",
            "exp": exp,
        }))
        .unwrap(),
    );
    format!("{header}.{payload}.sig")
}

async fn spawn_mock_server(token: String) -> (String, Arc<MockCliGraphState>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(MockCliGraphState::new(token));
    let router = Router::new()
        .route(
            "/v1.0/me/sendMail",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.sent_mails.write().unwrap().push(payload);
                    StatusCode::ACCEPTED.into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/messages/{id}/reply",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path(id): Path<String>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.replies.write().unwrap().push((id, false, payload));
                    StatusCode::ACCEPTED.into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/messages/{id}/replyAll",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path(id): Path<String>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.replies.write().unwrap().push((id, true, payload));
                    StatusCode::ACCEPTED.into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/messages/{id}/forward",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path(id): Path<String>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.forwards.write().unwrap().push((id, payload));
                    StatusCode::ACCEPTED.into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/messages/{id}",
            get(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path(id): Path<String>,
                 headers: HeaderMap| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    let draft = st.draft.read().unwrap().clone();
                    Json(json!({
                        "id": id,
                        "parentFolderId": "folder-drafts",
                        "subject": draft.subject,
                        "from": { "emailAddress": { "name": "Alice Outlook", "address": "alice@contoso.com" } },
                        "toRecipients": [{ "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } }],
                        "receivedDateTime": "2026-11-21T10:00:00Z",
                        "lastModifiedDateTime": "2026-11-21T10:05:00Z",
                        "isRead": true,
                        "isDraft": true,
                        "hasAttachments": true,
                        "bodyPreview": "Review the specification.",
                        "body": {
                            "contentType": "html",
                            "content": draft.body_html,
                        },
                        "attachments": [
                            {
                                "@odata.type": "#microsoft.graph.fileAttachment",
                                "id": "att-inline-diagram",
                                "name": "diagram.png",
                                "contentType": "image/png",
                                "size": 19,
                                "isInline": true,
                                "contentId": "diagram01@contoso",
                                "contentBytes": draft.inline_png_b64,
                            },
                            {
                                "@odata.type": "#microsoft.graph.fileAttachment",
                                "id": "att-file-report",
                                "name": "report.pdf",
                                "contentType": "application/pdf",
                                "size": 24,
                                "isInline": false,
                                "contentBytes": draft.pdf_b64,
                            }
                        ]
                    }))
                    .into_response()
                },
            )
            .patch(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path(id): Path<String>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.draft_patches.write().unwrap().push((id, payload.clone()));
                    let mut draft = st.draft.write().unwrap();
                    if let Some(subject) = payload.get("subject").and_then(|v| v.as_str()) {
                        draft.subject = subject.to_string();
                    }
                    if let Some(content) = payload
                        .get("body")
                        .and_then(|b| b.get("content"))
                        .and_then(|v| v.as_str())
                    {
                        draft.body_html = content.to_string();
                    }
                    StatusCode::OK.into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/messages",
            get(
                |State(st): State<Arc<MockCliGraphState>>,
                 RawQuery(query): RawQuery,
                 headers: HeaderMap| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    let q = query.unwrap_or_default();
                    st.search_queries.write().unwrap().push(q);
                    Json(json!({
                        "value": [
                            {
                                "id": "msg-search-hit",
                                "parentFolderId": "folder-inbox",
                                "subject": "- 목록 항목 검색 결과",
                                "from": { "emailAddress": { "name": "Bob", "address": "bob@contoso.com" } },
                                "receivedDateTime": "2026-11-21T09:00:00Z",
                                "isRead": false,
                                "isDraft": false,
                                "hasAttachments": false,
                                "bodyPreview": "- 목록 항목"
                            }
                        ]
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/events",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.created_events.write().unwrap().push(payload.clone());
                    Json(json!({
                        "id": "evt-created-cli",
                        "subject": payload["subject"],
                        "start": payload["start"],
                        "end": payload["end"],
                        "isAllDay": false,
                        "body": payload.get("body").cloned().unwrap_or_else(|| json!({"contentType":"html","content":""})),
                        "responseStatus": { "response": "organizer" },
                        "isCancelled": false
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/events/{id}/{action}",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path((id, action)): Path<(String, String)>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.rsvp_calls.write().unwrap().push((id, action, payload));
                    StatusCode::ACCEPTED.into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/todo/lists",
            get(
                |State(st): State<Arc<MockCliGraphState>>, headers: HeaderMap| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
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
                },
            ),
        )
        .route(
            "/v1.0/me/todo/lists/{list_id}/tasks",
            post(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path(list_id): Path<String>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.created_tasks
                        .write()
                        .unwrap()
                        .push((list_id, payload.clone()));
                    Json(json!({
                        "id": "task-created-cli",
                        "title": payload["title"],
                        "status": "notStarted",
                        "importance": payload.get("importance").cloned().unwrap_or_else(|| json!("normal")),
                        "body": payload.get("body").cloned().unwrap_or_else(|| json!({"contentType":"text","content":""}))
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/todo/lists/{list_id}/tasks/{task_id}",
            patch(
                |State(st): State<Arc<MockCliGraphState>>,
                 Path((list_id, task_id)): Path<(String, String)>,
                 headers: HeaderMap,
                 Json(payload): Json<Value>| async move {
                    if let Err(resp) = check_bearer(&headers, &st.expected_token) {
                        return resp;
                    }
                    st.patched_tasks
                        .write()
                        .unwrap()
                        .push((list_id, task_id.clone(), payload.clone()));
                    Json(json!({
                        "id": task_id,
                        "title": payload.get("title").cloned().unwrap_or_else(|| json!("Updated")),
                        "status": "notStarted",
                        "importance": "normal"
                    }))
                    .into_response()
                },
            ),
        )
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), state, handle)
}

fn run_outlook(
    dir: &TempDir,
    base_url: &str,
    token: &str,
    args: &[&str],
    stdin_data: Option<&str>,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_outlook"));
    cmd.args(args)
        .env("OUTLOOK_GRAPH_BASE_URL", format!("{base_url}/v1.0"))
        .env("OUTLOOK_AUTH_BASE_URL", base_url)
        .env("OUTLOOK_ACCESS_TOKEN", token)
        .env("OUTLOOK_DISABLE_KEYRING", "1")
        .env("OUTLOOK_SHARE_M365_AUTH", "0")
        .env("OUTLOOK_AUTO_DAEMON", "0")
        .env("OUTLOOK_CONFIG_DIR", dir.path())
        .env("OUTLOOK_DATA_DIR", dir.path())
        .env_remove("OUTLOOK_TOKEN_FILE")
        .env_remove("OUTLOOK_TOKEN_REFRESH_CMD")
        .env_remove("TEAMS_TOKEN_FILE")
        .env_remove("TEAMS_CONFIG_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(input) = stdin_data {
        let mut child = cmd.stdin(Stdio::piped()).spawn().expect("spawn outlook");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().expect("wait outlook")
    } else {
        cmd.stdin(Stdio::null()).output().expect("run outlook")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn built_cli_handles_hyphen_inputs_draft_preservation_and_flag_guards() {
    let token = make_mock_jwt();
    let (base_url, state, server) = spawn_mock_server(token.clone()).await;
    let dir = tempfile::tempdir().unwrap();

    // 1. All 10 audited CLI inputs pass with `--help` (matching audit replay) and trigger 0 writes
    let audit_help_cases: &[&[&str]] = &[
        &["send", "--to", "audit@example.invalid", "--subject", "Test", "--body", "- 목록 항목", "--help"],
        &["reply", "id", "- 목록 항목", "--help"],
        &["forward", "id", "- 목록 항목", "--to", "audit@example.invalid", "--help"],
        &["edit-draft", "id", "--body", "- 목록 항목", "--help"],
        &[
            "create-event",
            "--subject",
            "Test",
            "--start",
            "2026-12-01T10:00",
            "--end",
            "2026-12-01T11:00",
            "--body",
            "- 목록 항목",
            "--help",
        ],
        &["add-task", "Test", "--notes", "- 목록 항목", "--help"],
        &["rsvp", "id", "accept", "--comment", "- 목록 항목", "--help"],
        &["send", "--to", "audit@example.invalid", "--subject", "- 목록 항목", "--help"],
        &["update-task", "id", "--title", "- 목록 항목", "--help"],
        &["search", "- 목록 항목", "--help"],
    ];
    for argv in audit_help_cases {
        let out = run_outlook(&dir, &base_url, &token, argv, None);
        assert!(
            out.status.success(),
            "expected exit 0 for {argv:?}, got {:?}, stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("Usage:"), "missing help usage for {argv:?}: {stdout}");
    }
    assert_eq!(state.total_writes(), 0, "help invocations must perform 0 writes");

    // 2. Missing option values (including `--help`/`-h` directly after an option) and unknown flags trigger exit 2 and 0 writes
    let reject_cases: &[&[&str]] = &[
        &["edit-draft", "draft-rich", "--body", "--json"],
        &["edit-draft", "draft-rich", "--body", "--dry-run"],
        &["edit-draft", "draft-rich", "--body", "--help"],
        &["edit-draft", "draft-rich", "--body", "-h"],
        &["edit-draft", "draft-rich", "--body", "--"],
        &["send", "--to", "audit@example.invalid", "--subject", "Test", "--body", "--dry-run"],
        &["send", "--to", "audit@example.invalid", "--subject", "Test", "--body", "--json"],
        &["send", "--to", "audit@example.invalid", "--subject", "Test", "--body", "--help"],
        &["reply", "msg-1", "--dry-run"],
        &["reply", "msg-1", "--all"],
        &["forward", "msg-1", "--to", "audit@example.invalid", "--dry-run"],
        &["add-task", "Test", "--notes", "--dry-run"],
        &["add-task", "Test", "--body", "--json"],
        &["rsvp", "evt-1", "accept", "--comment", "--dry-run"],
        &["update-task", "task-1", "--title", "--json"],
        &["search", "--dry-run"],
    ];
    for argv in reject_cases {
        let out = run_outlook(&dir, &base_url, &token, argv, None);
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected clap exit code 2 for {argv:?}, stdout={}, stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for help_args in [
        &["edit-draft", "--help"][..],
        &["edit-draft", "-h"][..],
        &["send", "--help"][..],
        &["reply", "msg-1", "--help"][..],
        &["reply", "msg-1", "-h"][..],
        &["search", "--help"][..],
    ] {
        let out = run_outlook(&dir, &base_url, &token, help_args, None);
        assert!(out.status.success(), "expected help exit 0 for {help_args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
    }
    assert_eq!(
        state.total_writes(),
        0,
        "rejected and help calls must perform 0 Graph writes"
    );

    // 3. Draft subject-only edit PATCHes ONLY `subject` and preserves body HTML, link, inline image, and attachments
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["edit-draft", "draft-rich", "--subject", "- 수정된 제목", "--json"],
        None,
    );
    assert!(
        out.status.success(),
        "edit-draft subject failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    {
        let patches = state.draft_patches.read().unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "draft-rich");
        assert_eq!(patches[0].1, json!({ "subject": "- 수정된 제목" }));
    }

    for read_args in [
        &["read", "draft-rich", "--json"][..],
        &["read", "draft-rich", "--offline", "--json"][..],
    ] {
        let read_out = run_outlook(&dir, &base_url, &token, read_args, None);
        assert!(
            read_out.status.success(),
            "read {read_args:?} failed: {}",
            String::from_utf8_lossy(&read_out.stderr)
        );
        let parsed: Value = serde_json::from_slice(&read_out.stdout).unwrap();
        let msg = &parsed["message"];
        assert_eq!(msg["subject"], "- 수정된 제목");
        assert_eq!(msg["body_html"], RICH_DRAFT_HTML);
        let atts = msg["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 2);
        assert!(atts.iter().any(|a| {
            a["id"] == "att-inline-diagram"
                && a["is_inline"] == true
                && a["content_id"] == "diagram01@contoso"
        }));
        assert!(atts.iter().any(|a| {
            a["id"] == "att-file-report" && a["name"] == "report.pdf" && a["is_inline"] == false
        }));
    }

    // 4. Draft Markdown body edit sends <ul><li>first</li><li>second</li></ul>, and `--body=--dry-run` works via `=`
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["edit-draft", "draft-rich", "--body", "- first\n- second", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["edit-draft", "draft-rich", "--body=--dry-run", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let patches = state.draft_patches.read().unwrap();
        assert_eq!(patches.len(), 3);
        assert_eq!(
            patches[1].1,
            json!({ "body": { "contentType": "HTML", "content": "<ul><li>first</li><li>second</li></ul>" } })
        );
        assert_eq!(
            patches[2].1,
            json!({ "body": { "contentType": "HTML", "content": "<p>--dry-run</p>" } })
        );
    }

    // 5. `send`, `compose` alias, and stdin send render two-item Markdown lists as <ul><li>...</li></ul>
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &[
            "compose",
            "--to",
            "audit@example.invalid",
            "--subject",
            "- 목록 항목",
            "--body",
            "- first\n- second",
            "--json",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &[
            "send",
            "--to",
            "audit@example.invalid",
            "--subject",
            "Stdin List",
            "--json",
        ],
        Some("- 첫째 항목\n- 둘째 항목\n"),
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let sent = state.sent_mails.read().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["message"]["subject"], "- 목록 항목");
        assert_eq!(
            sent[0]["message"]["body"]["content"],
            "<ul><li>first</li><li>second</li></ul>"
        );
        assert_eq!(sent[1]["message"]["subject"], "Stdin List");
        assert_eq!(
            sent[1]["message"]["body"]["content"],
            "<ul><li>첫째 항목</li><li>둘째 항목</li></ul>"
        );
    }

    // 6. `reply` (--all, hyphen list, and `-- --dry-run`)
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["reply", "msg-1", "- first\n- second", "--all", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["reply", "msg-1", "--json", "--", "--dry-run"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let replies = state.replies.read().unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].0, "msg-1");
        assert!(replies[0].1, "reply --all must hit replyAll");
        assert_eq!(
            replies[0].2["comment"],
            "<ul><li>first</li><li>second</li></ul>"
        );
        assert!(!replies[1].1);
        assert_eq!(replies[1].2["comment"], "<p>--dry-run</p>");
    }

    // 7. `forward` with hyphen Markdown comment AND with omitted comment
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["forward", "msg-1", "- first\n- second", "--to", "audit@example.invalid", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["forward", "msg-1", "--to", "audit@example.invalid", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let forwards = state.forwards.read().unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(
            forwards[0].1["comment"],
            "<ul><li>first</li><li>second</li></ul>"
        );
        assert_eq!(forwards[1].1["comment"], "");
    }

    // 8. `create-event --body` renders Markdown list
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &[
            "create-event",
            "--subject",
            "- 목록 항목",
            "--start",
            "2026-12-01T10:00",
            "--end",
            "2026-12-01T11:00",
            "--body",
            "- first\n- second",
            "--json",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let events = state.created_events.read().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["subject"], "- 목록 항목");
        assert_eq!(
            events[0]["body"]["content"],
            "<ul><li>first</li><li>second</li></ul>"
        );
    }

    // 9. `add-task --notes` and `--body` alias keep exact plain text (NOT HTML), and `update-task --title` works
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["add-task", "Test", "--notes", "- first\n- second", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["add-task", "- 할 일", "--body", "- 목록 항목", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let tasks = state.created_tasks.read().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].1["title"], "Test");
        assert_eq!(tasks[0].1["body"]["contentType"], "text");
        assert_eq!(tasks[0].1["body"]["content"], "- first\n- second");
        assert_eq!(tasks[1].1["title"], "- 할 일");
        assert_eq!(tasks[1].1["body"]["contentType"], "text");
        assert_eq!(tasks[1].1["body"]["content"], "- 목록 항목");
    }

    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["update-task", "task-1", "--title", "- 목록 항목", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let patched = state.patched_tasks.read().unwrap();
        assert_eq!(patched.len(), 1);
        assert_eq!(patched[0].1, "task-1");
        assert_eq!(patched[0].2["title"], "- 목록 항목");
    }

    // 10. `rsvp --comment` keeps exact plain text string verbatim
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["rsvp", "evt-1", "accept", "--comment", "- 목록 항목", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    {
        let rsvps = state.rsvp_calls.read().unwrap();
        assert_eq!(rsvps.len(), 1);
        assert_eq!(rsvps[0].0, "evt-1");
        assert_eq!(rsvps[0].1, "accept");
        assert_eq!(rsvps[0].2["comment"], "- 목록 항목");
    }

    // 11. `search` with hyphen-prefixed Korean query works end-to-end
    let out = run_outlook(
        &dir,
        &base_url,
        &token,
        &["search", "- 목록 항목", "--remote", "--json"],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let search_json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(search_json["query"], "- 목록 항목");
    assert_eq!(search_json["remote"].as_array().unwrap().len(), 1);
    assert_eq!(state.search_queries.read().unwrap().len(), 1);

    server.abort();
}
