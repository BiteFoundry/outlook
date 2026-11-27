    use std::path::PathBuf;

use super::*;
    use crate::{
        auth::AuthManager,
        models::{MailFolder, MailMessage},
    };

    fn test_ctx() -> (AppConfig, GraphClient, Storage) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        (cfg, graph, storage)
    }

    #[tokio::test]
    async fn handshake_and_notifications_and_ping() {
        let (cfg, graph, storage) = test_ctx();

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "outlook");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");

        assert!(handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await
        .is_none());

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"], json!({}));

        let err = handle_rpc_line(&cfg, &graph, &storage, "{not json")
            .await
            .unwrap();
        assert_eq!(err["error"]["code"], PARSE_ERROR);

        let err = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":4,"method":"unknown/op"}"#,
        )
        .await
        .unwrap();
        assert_eq!(err["error"]["code"], METHOD_NOT_FOUND);
    }

        #[test]
    fn tool_catalog_states_graph_cost_for_cache_and_refresh_flags() {
        let catalog = tool_catalog();
        let mut flagged_tools = 0;
        for tool in &catalog {
            let name = tool["name"].as_str().unwrap();
            for flag in ["refresh", "offline"] {
                if let Some(prop) = tool["inputSchema"]["properties"].get(flag) {
                    let description = prop["description"].as_str().unwrap_or_default();
                    assert!(
                        !description.trim().is_empty(),
                        "{name}.{flag} needs a description of its Graph cost"
                    );
                    flagged_tools += 1;
                }
            }
        }
        assert!(flagged_tools >= 10, "only {flagged_tools} refresh/offline flags found");

        for name in [
            "list_messages",
            "search_mail",
            "list_mail_folders",
            "download_attachment",
        ] {
            let tool = catalog
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("{name} missing from the catalog"));
            let description = tool["description"].as_str().unwrap().to_lowercase();
            assert!(
                description.contains("request") || description.contains("cache"),
                "{name} description must state its Graph cost: {description}"
            );
        }

        // The page caps quoted to agents must match the code.
        let list_messages = catalog
            .iter()
            .find(|tool| tool["name"] == "list_messages")
            .unwrap()["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(list_messages.contains(&format!(
            "at most {} pages (sync_complete",
            crate::sync::INTERACTIVE_DELTA_MAX_PAGES
        )));
        assert!(list_messages.contains(&format!(
            "scan at most {} pages",
            cli::FILTER_SCAN_MAX_PAGES
        )));
    }

#[tokio::test]
    async fn tools_list_exposes_outlook_tools() {
        let (cfg, graph, storage) = test_ctx();
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/list"}"#,
        )
        .await
        .unwrap();

        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "whoami",
                "list_mail_folders",
                "list_messages",
                "get_message",
                "send_mail",
                "reply_mail",
                "forward_mail",
                "update_message",
                "delete_message",
                "download_attachment",
                "search_mail",
                "list_events",
                "get_event",
                "create_event",
                "respond_event",
                "delete_event",
                "list_todo_lists",
                "list_tasks",
                "create_task",
                "complete_task",
                "search_contacts",
                "update_draft",
                "update_event",
                "update_task",
            ]
        );

        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }

    #[tokio::test]
    async fn every_tool_property_in_tools_list_has_non_empty_description() {
        let (cfg, graph, storage) = test_ctx();
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/list"}"#,
        )
        .await
        .unwrap();

        let tools = resp["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());

        let mut property_count = 0usize;
        for tool in tools {
            let tool_name = tool["name"].as_str().unwrap();
            let tool_desc = tool["description"].as_str().unwrap_or_default().trim();
            assert!(!tool_desc.is_empty(), "{tool_name} description is empty");

            let props = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{tool_name} missing inputSchema.properties object"));
            for (prop_name, prop_schema) in props {
                let desc = prop_schema
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                assert!(
                    !desc.is_empty(),
                    "{tool_name}.{prop_name} must have a non-empty description"
                );
                property_count += 1;
            }
        }
        assert!(property_count > 0, "expected tool properties to be audited");
    }

    #[tokio::test]
    async fn delete_tools_reject_missing_or_non_literal_true_confirm() {
        let (cfg, graph, storage) = test_ctx();

        for (tool, bad_args) in [
            ("delete_message", json!({ "message_id": "m1" })),
            (
                "delete_message",
                json!({ "message_id": "m1", "confirm": false }),
            ),
            (
                "delete_message",
                json!({ "message_id": "m1", "confirm": "true" }),
            ),
            ("delete_event", json!({ "event_id": "ev1", "confirm": 1 })),
        ] {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": bad_args,
                }
            })
            .to_string();
            let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
            assert_eq!(resp["result"]["isError"], true, "{resp}");
        }

        assert!(validate_tool_args(
            "delete_message",
            &json!({ "message_id": "m1", "confirm": true }),
        )
        .is_ok());
        assert!(validate_tool_args(
            "delete_event",
            &json!({ "event_id": "ev1", "confirm": true }),
        )
        .is_ok());
    }

    #[test]
    fn edit_tools_validate_field_sets_and_time_formats() {
        assert!(validate_tool_args(
            "update_draft",
            &json!({"message_id":"draft", "body_html":""})
        )
        .is_ok());
        assert!(validate_tool_args(
            "update_draft",
            &json!({"message_id":"draft", "body":"text", "body_html":"<p>text</p>"})
        )
        .is_err());
        assert!(validate_tool_args(
            "update_event",
            &json!({"event_id":"event", "subject":"new", "attendees":[]})
        )
        .is_err());
        assert!(validate_tool_args(
            "update_event",
            &json!({"event_id":"event", "start":"2026-11-24T09:00"})
        )
        .is_ok());
        assert!(
            validate_tool_args("update_task", &json!({"task_id":"task", "due":"tomorrow"}))
                .is_err()
        );
        assert!(validate_tool_args(
            "update_task",
            &json!({"task_id":"task", "due":"2026-11-24"})
        )
        .is_ok());
    }

    #[test]
    fn argument_validation_covers_outlook_tools() {
        assert!(validate_tool_args(
            "send_mail",
            &json!({
                "to": ["alice@example.com"],
                "subject": "Quarterly Review",
                "body": "Please see notes.",
                "importance": "high"
            })
        )
        .is_ok());
        assert!(validate_tool_args(
            "send_mail",
            &json!({
                "to": [],
                "subject": "Quarterly Review",
                "body": "Body"
            })
        )
        .is_err());
        assert!(validate_tool_args("update_message", &json!({ "message_id": "m1" })).is_err());
        assert_eq!(
            validate_tool_args(
                "update_message",
                &json!({
                    "message_id": "m1", "flagged": true, "subject": "unsupported"
                })
            )
            .unwrap_err(),
            "unsupported argument for update_message: subject"
        );
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1", "compact": "true"
            })
        )
        .is_err());
        assert!(validate_tool_args(
            "update_message",
            &json!({ "message_id": "m1", "destination_folder": "archive" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "respond_event",
            &json!({ "event_id": "e1", "response": "tentative" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "respond_event",
            &json!({ "event_id": "e1", "response": "maybe" })
        )
        .is_err());
        assert!(validate_tool_args(
            "create_task",
            &json!({ "title": "Ship release", "due": "2026-11-25", "importance": "normal" })
        )
        .is_ok());
    }

    #[tokio::test]
    async fn offline_list_messages_get_message_and_search_mail_work_against_sqlite() {
        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "folder-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 2,
            }])
            .unwrap();
        storage
            .upsert_mail_messages(&[
                MailMessage {
                    id: "msg-1".to_string(),
                    internet_message_id: None,
                    conversation_id: Some("conv-1".to_string()),
                    folder_id: Some("folder-inbox".to_string()),
                    subject: "Weekly Status".to_string(),
                    from: None,
                    to_recipients: Vec::new(),
                    cc_recipients: Vec::new(),
                    bcc_recipients: Vec::new(),
                    reply_to: Vec::new(),
                    received_at: "2026-11-15T10:00:00Z".to_string(),
                    sent_at: None,
                    last_modified_at: None,
                    is_read: true,
                    is_draft: false,
                    flagged: false,
                    importance: "normal".to_string(),
                    inference_classification: Some("focused".to_string()),
                    has_attachments: false,
                    body_loaded: true,
                    preview: "All checks green".to_string(),
                    body_html: "<p>All checks green</p>".to_string(),
                    body_plain: "All checks green".to_string(),
                    attachments: Vec::new(),
                    meeting_request: None,
                    web_link: None,
                    ..Default::default()
                },
                MailMessage {
                    id: "msg-2".to_string(),
                    internet_message_id: None,
                    conversation_id: Some("conv-2".to_string()),
                    folder_id: Some("folder-inbox".to_string()),
                    subject: "Kubernetes rollout checklist".to_string(),
                    from: None,
                    to_recipients: Vec::new(),
                    cc_recipients: Vec::new(),
                    bcc_recipients: Vec::new(),
                    reply_to: Vec::new(),
                    received_at: "2026-11-16T10:00:00Z".to_string(),
                    sent_at: None,
                    last_modified_at: None,
                    is_read: false,
                    is_draft: false,
                    flagged: true,
                    importance: "high".to_string(),
                    inference_classification: Some("focused".to_string()),
                    has_attachments: false,
                    body_loaded: true,
                    preview: "Deploy v0.1.0 to staging".to_string(),
                    body_html: "<p>Deploy v0.1.0 to staging</p>".to_string(),
                    body_plain: "Deploy v0.1.0 to staging".to_string(),
                    attachments: Vec::new(),
                    meeting_request: None,
                    web_link: None,
                    ..Default::default()
                },
            ])
            .unwrap();

        // list_messages (offline, unread=true)
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "tools/call",
                "params": {
                    "name": "list_messages",
                    "arguments": { "folder": "inbox", "unread": true, "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
        assert_eq!(payload["messages"][0]["id"], "msg-2");
        let listed = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_messages",
            &json!({
                "folder": "inbox", "unread": true, "flagged": true,
                "focused": true, "offline": true, "compact": true, "limit": 1,
            }),
        )
        .await
        .unwrap();
        assert_eq!(listed["messages"][0]["id"], payload["messages"][0]["id"]);
        assert!(listed["messages"][0].get("body_plain").is_none());
        assert!(listed["messages"][0].get("attachments").is_none());

        // get_message (offline)
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": {
                    "name": "get_message",
                    "arguments": { "message_id": "msg-1", "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["message"]["subject"], "Weekly Status");
        let detail = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_message",
            &json!({
                "message_id": "msg-1", "offline": true, "compact": true,
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            detail["message"]["body_plain"],
            payload["message"]["body_plain"]
        );
        assert!(detail["message"].get("body_html").is_none());
        assert!(detail["message"].get("attachments").is_none());

        // search_mail (offline)
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 22,
                "method": "tools/call",
                "params": {
                    "name": "search_mail",
                    "arguments": { "query": "Kubernetes", "folder": "inbox", "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["hits"].as_array().unwrap().len(), 1);
        assert_eq!(payload["hits"][0]["id"], "msg-2");
    }

    #[tokio::test]
    async fn bounded_line_reader_caps_oversized_frames() {
        let data = b"hello\r\n0123456789ABCDEF\nworld";
        let mut reader = BufReader::new(std::io::Cursor::new(&data[..]));

        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Line(s) => assert_eq!(s, "hello"),
            _ => panic!("expected first line"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::TooLong => {}
            _ => panic!("expected TooLong"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Line(s) => assert_eq!(s, "world"),
            _ => panic!("expected trailing line at EOF"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Eof => {}
            _ => panic!("expected Eof"),
        }
    }

    #[tokio::test]
    async fn download_attachment_returns_structured_agent_media_json() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use crate::models::MailAttachment;

        let (cfg, graph, storage) = test_ctx();
        let tmp = tempfile::tempdir().unwrap();
        let png_bytes = crate::ui::composer::encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        let pdf_bytes = b"%PDF-1.4\nhello\n".to_vec();

        storage
            .upsert_mail_messages(&[MailMessage {
                id: "msg-media".to_string(),
                internet_message_id: None,
                conversation_id: Some("conv-media".to_string()),
                folder_id: Some("inbox".to_string()),
                subject: "Architecture Screenshot".to_string(),
                from: None,
                to_recipients: Vec::new(),
                cc_recipients: Vec::new(),
                bcc_recipients: Vec::new(),
                reply_to: Vec::new(),
                received_at: "2026-11-19T12:00:00Z".to_string(),
                sent_at: None,
                last_modified_at: None,
                is_read: true,
                is_draft: false,
                flagged: false,
                importance: "normal".to_string(),
                inference_classification: None,
                has_attachments: true,
                body_loaded: true,
                preview: "Attached screenshot".to_string(),
                body_html: "<p>Attached screenshot</p>".to_string(),
                body_plain: "Attached screenshot".to_string(),
                attachments: vec![
                    MailAttachment {
                        id: "att-img".to_string(),
                        name: "diagram.png".to_string(),
                        content_type: "image/png".to_string(),
                        size: png_bytes.len() as u64,
                        is_inline: false,
                        content_id: None,
                        content_bytes_b64: Some(B64.encode(&png_bytes)),
                    },
                    MailAttachment {
                        id: "att-pdf".to_string(),
                        name: "spec.pdf".to_string(),
                        content_type: "application/octet-stream".to_string(),
                        size: pdf_bytes.len() as u64,
                        is_inline: false,
                        content_id: None,
                        content_bytes_b64: Some(B64.encode(&pdf_bytes)),
                    },
                ],
                meeting_request: None,
                web_link: None,
                ..Default::default()
            }])
            .unwrap();

        // Image attachment download
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 31,
                "method": "tools/call",
                "params": {
                    "name": "download_attachment",
                    "arguments": {
                        "message_id": "msg-media",
                        "attachment": "diagram.png",
                        "output_dir": tmp.path().to_string_lossy(),
                        "offline": true
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["ok"], true);
        assert_eq!(payload["file_name"], "diagram.png");
        assert_eq!(payload["mime_type"], "image/png");
        assert_eq!(payload["size_bytes"], png_bytes.len() as u64);
        assert_eq!(payload["is_image"], true);
        let saved_path = PathBuf::from(payload["path"].as_str().unwrap());
        assert!(saved_path.is_absolute());
        assert_eq!(std::fs::read(&saved_path).unwrap(), png_bytes);

        // Non-image PDF attachment download (infers mime_type from extension when octet-stream)
        let resp_pdf = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 32,
                "method": "tools/call",
                "params": {
                    "name": "download_attachment",
                    "arguments": {
                        "message_id": "msg-media",
                        "attachment_index": 1,
                        "output_dir": tmp.path().to_string_lossy(),
                        "offline": true
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp_pdf["result"]["isError"], false, "{resp_pdf}");
        let pdf_payload: Value =
            serde_json::from_str(resp_pdf["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(pdf_payload["ok"], true);
        assert_eq!(pdf_payload["file_name"], "spec.pdf");
        assert_eq!(pdf_payload["mime_type"], "application/pdf");
        assert_eq!(pdf_payload["size_bytes"], pdf_bytes.len() as u64);
        assert_eq!(pdf_payload["is_image"], false);
        assert!(PathBuf::from(pdf_payload["path"].as_str().unwrap()).is_absolute());
    }

    #[test]
    fn compact_read_tools_reject_unknown_args_and_accept_documented_args() {
        let err = validate_tool_args("list_messages", &json!({ "folder": "inbox", "full": true }))
            .expect_err("list_messages must reject unknown `full` argument");
        assert_eq!(err, "unsupported argument for list_messages: full");

        assert!(validate_tool_args(
            "list_messages",
            &json!({
                "folder": "inbox",
                "limit": 20,
                "cursor": "o:abc12345:10",
                "unread": true,
                "flagged": false,
                "focused": true,
                "compact": false,
                "offline": true
            })
        )
        .is_ok());

        assert_eq!(
            validate_tool_args(
                "get_message",
                &json!({ "message_id": "m1", "full": true })
            )
            .unwrap_err(),
            "unsupported argument for get_message: full"
        );
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1",
                "mark_read": true,
                "compact": false,
                "offline": false
            })
        )
        .is_ok());

        assert_eq!(
            validate_tool_args("search_mail", &json!({ "query": "Q4", "full": true })).unwrap_err(),
            "unsupported argument for search_mail: full"
        );
        assert!(validate_tool_args(
            "search_mail",
            &json!({
                "query": "Q4",
                "folder": "inbox",
                "limit": 25,
                "cursor": "g:12345678:0|https://graph.microsoft.com/v1.0/me/messages",
                "compact": false,
                "offline": false
            })
        )
        .is_ok());

        assert_eq!(
            validate_tool_args("list_events", &json!({ "full": true })).unwrap_err(),
            "unsupported argument for list_events: full"
        );
        assert!(validate_tool_args(
            "list_events",
            &json!({
                "from": "2026-11-20",
                "to": "2026-11-27",
                "days": 7,
                "limit": 50,
                "compact": false,
                "offline": true
            })
        )
        .is_ok());
        assert!(validate_tool_args(
            "list_events",
            &json!({
                "start": "2026-11-20",
                "end": "2026-11-27",
                "compact": true
            })
        )
        .is_ok());

        // get_message and get_event selective `fields` schema validation
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1",
                "fields": ["cc", "bcc", "categories", "attachments"]
            })
        )
        .is_ok());
        assert!(validate_tool_args(
            "get_message",
            &json!({
                "message_id": "m1",
                "fields": "cc"
            })
        )
        .is_err());
        assert!(validate_tool_args(
            "get_event",
            &json!({
                "event_id": "ev1",
                "fields": ["attendees", "body_plain", "recurrence", "categories"],
                "offline": true
            })
        )
        .is_ok());
        assert_eq!(
            validate_tool_args("get_event", &json!({ "event_id": "ev1", "full": true }))
                .unwrap_err(),
            "unsupported argument for get_event: full"
        );
    }

    #[tokio::test]
    async fn compact_list_and_search_emit_honest_omitted_metadata_and_hint() {
        use axum::{routing::get, Json, Router};
        use crate::{
            auth::StoredTokens,
            models::{CalendarEvent, EmailRecipient, EventAttendee, MailAttachment},
        };

        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "folder-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 1,
            }])
            .unwrap();
        storage
            .upsert_mail_messages(&[MailMessage {
                id: "msg-rich".to_string(),
                internet_message_id: Some("<rich@contoso.com>".to_string()),
                conversation_id: Some("conv-rich".to_string()),
                folder_id: Some("folder-inbox".to_string()),
                subject: "Q4 Planning".to_string(),
                from: Some(EmailRecipient {
                    name: Some("CEO".to_string()),
                    address: "ceo@contoso.com".to_string(),
                }),
                sender: Some(EmailRecipient {
                    name: Some("EA".to_string()),
                    address: "ea@contoso.com".to_string(),
                }),
                to_recipients: vec![EmailRecipient {
                    name: Some("Alice".to_string()),
                    address: "alice@contoso.com".to_string(),
                }],
                cc_recipients: vec![EmailRecipient {
                    name: Some("Bob".to_string()),
                    address: "bob@contoso.com".to_string(),
                }],
                bcc_recipients: vec![EmailRecipient {
                    name: None,
                    address: "audit@contoso.com".to_string(),
                }],
                reply_to: vec![EmailRecipient {
                    name: None,
                    address: "reply@contoso.com".to_string(),
                }],
                received_at: "2026-11-20T09:00:00Z".to_string(),
                sent_at: Some("2026-11-20T08:59:00Z".to_string()),
                last_modified_at: Some("2026-11-20T09:05:00Z".to_string()),
                is_read: false,
                is_draft: false,
                flagged: true,
                importance: "high".to_string(),
                inference_classification: Some("focused".to_string()),
                categories: vec!["Executive".to_string(), "Finance".to_string()],
                has_attachments: true,
                body_loaded: true,
                preview: "See attached deck".to_string(),
                body_html: "<html><body><p>Huge HTML body</p></body></html>".to_string(),
                body_plain: "Huge HTML body".to_string(),
                attachments: vec![MailAttachment {
                    id: "att-1".to_string(),
                    name: "deck.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    size: 2048,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: Some("cGRm".to_string()),
                }],
                meeting_request: None,
                web_link: Some("https://outlook.office.com/mail/msg-rich".to_string()),
            }])
            .unwrap();

        storage
            .upsert_calendar_events(&[CalendarEvent {
                id: "ev-series".to_string(),
                subject: "Weekly Architecture Sync".to_string(),
                start_iso: "2026-11-20T15:00:00Z".to_string(),
                end_iso: "2026-11-20T16:00:00Z".to_string(),
                time_zone: "UTC".to_string(),
                is_all_day: false,
                location: Some("Room 42".to_string()),
                organizer_name: Some("Alice".to_string()),
                organizer_email: Some("alice@contoso.com".to_string()),
                attendees: vec![EventAttendee {
                    name: Some("Bob".to_string()),
                    address: "bob@contoso.com".to_string(),
                    attendee_type: "required".to_string(),
                    response_status: "accepted".to_string(),
                }],
                response_status: "organizer".to_string(),
                is_online_meeting: true,
                online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/sync".to_string()),
                body_html: "<div><p>Agenda HTML</p></div>".to_string(),
                body_plain: "Agenda HTML".to_string(),
                is_cancelled: false,
                web_link: Some("https://outlook.office.com/calendar/ev-series".to_string()),
                categories: vec!["Engineering".to_string()],
                show_as: Some("busy".to_string()),
                sensitivity: Some("normal".to_string()),
                event_type: Some("seriesMaster".to_string()),
                has_attachments: true,
                is_recurring: true,
                recurrence: Some(json!({
                    "pattern": { "type": "weekly", "interval": 1, "daysOfWeek": ["friday"] },
                    "range": { "type": "noEnd", "startDate": "2026-11-20" }
                })),
            }])
            .unwrap();

        // 1. Compact list_messages includes honest `omitted.fields` and `hint`; compact:false omits them.
        let list_compact = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_messages",
            &json!({ "folder": "inbox", "offline": true }),
        )
        .await
        .unwrap();
        let omitted_fields = list_compact["omitted"]["fields"].as_array().unwrap();
        for expected in [
            "to_recipients",
            "cc_recipients",
            "bcc_recipients",
            "reply_to",
            "sender",
            "preview",
            "body_plain",
            "importance",
            "categories",
            "meeting_request",
            "attachments",
        ] {
            assert!(
                omitted_fields.iter().any(|v| v == expected),
                "list_messages omitted.fields missing {expected}: {omitted_fields:?}"
            );
        }
        let hint = list_compact["hint"].as_str().unwrap();
        assert!(hint.contains("get_message") && hint.contains("NOT absent"), "{hint}");

        let list_full = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_messages",
            &json!({ "folder": "inbox", "offline": true, "compact": false }),
        )
        .await
        .unwrap();
        assert!(list_full.get("omitted").is_none());
        assert!(list_full.get("hint").is_none());
        assert_eq!(list_full["messages"][0]["categories"], json!(["Executive", "Finance"]));
        assert_eq!(list_full["messages"][0]["sender"]["address"], "ea@contoso.com");

        // 2. Compact list_events emits omitted.fields (body_html, recurrence) and hint; strips recurrence on list items.
        let ev_list_compact = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_events",
            &json!({ "from": "2026-11-19", "to": "2026-11-22", "offline": true }),
        )
        .await
        .unwrap();
        assert_eq!(
            ev_list_compact["omitted"]["fields"],
            json!(["body_html", "recurrence"])
        );
        let ev_hint = ev_list_compact["hint"].as_str().unwrap();
        assert!(ev_hint.contains("get_event") && ev_hint.contains("NOT absent"), "{ev_hint}");
        assert!(ev_list_compact["events"][0].get("body_html").is_none());
        assert!(ev_list_compact["events"][0].get("recurrence").is_none());
        assert_eq!(ev_list_compact["events"][0]["is_recurring"], true);
        assert_eq!(ev_list_compact["events"][0]["categories"], json!(["Engineering"]));
        assert_eq!(ev_list_compact["events"][0]["show_as"], "busy");
        assert_eq!(ev_list_compact["events"][0]["sensitivity"], "normal");
        assert_eq!(ev_list_compact["events"][0]["event_type"], "seriesMaster");
        assert_eq!(ev_list_compact["events"][0]["has_attachments"], true);

        let ev_list_full = call_tool(
            &cfg,
            &graph,
            &storage,
            "list_events",
            &json!({ "from": "2026-11-19", "to": "2026-11-22", "offline": true, "compact": false }),
        )
        .await
        .unwrap();
        assert!(ev_list_full.get("omitted").is_none());
        assert_eq!(ev_list_full["events"][0]["body_html"], "<div><p>Agenda HTML</p></div>");
        assert_eq!(
            ev_list_full["events"][0]["recurrence"]["pattern"]["type"],
            "weekly"
        );

        // 3. Selective `fields` projection on get_message (aliases cc/bcc + canonical categories/attachments + missing_fields).
        let msg_proj = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_message",
            &json!({
                "message_id": "msg-rich",
                "offline": true,
                "fields": ["cc", "bcc", "categories", "sender", "attachments", "not_a_real_field"]
            }),
        )
        .await
        .unwrap();
        assert_eq!(msg_proj["message"]["id"], "msg-rich");
        assert_eq!(msg_proj["message"]["cc_recipients"][0]["address"], "bob@contoso.com");
        assert_eq!(msg_proj["message"]["bcc_recipients"][0]["address"], "audit@contoso.com");
        assert_eq!(msg_proj["message"]["categories"], json!(["Executive", "Finance"]));
        assert_eq!(msg_proj["message"]["sender"]["address"], "ea@contoso.com");
        assert_eq!(msg_proj["message"]["attachments"][0]["name"], "deck.pdf");
        assert_eq!(msg_proj["message"]["attachments"][0]["content_cached"], true);
        assert!(msg_proj["message"]["attachments"][0].get("content_bytes_b64").is_none());
        assert!(msg_proj["message"].get("body_html").is_none());
        assert!(msg_proj["message"].get("body_plain").is_none());
        assert_eq!(msg_proj["missing_fields"], json!(["not_a_real_field"]));

        // 4. Selective `fields` projection on get_event + compact/full get_event.
        let ev_proj = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_event",
            &json!({
                "event_id": "ev-series",
                "offline": true,
                "fields": ["attendees", "body_plain", "recurrence", "categories", "show_as", "bogus_key"]
            }),
        )
        .await
        .unwrap();
        assert_eq!(ev_proj["event"]["id"], "ev-series");
        assert_eq!(ev_proj["event"]["attendees"][0]["address"], "bob@contoso.com");
        assert_eq!(ev_proj["event"]["body_plain"], "Agenda HTML");
        assert_eq!(ev_proj["event"]["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(ev_proj["event"]["categories"], json!(["Engineering"]));
        assert_eq!(ev_proj["event"]["show_as"], "busy");
        assert!(ev_proj["event"].get("body_html").is_none());
        assert!(ev_proj["event"].get("subject").is_none());
        assert_eq!(ev_proj["missing_fields"], json!(["bogus_key"]));

        let ev_compact = call_tool(
            &cfg,
            &graph,
            &storage,
            "get_event",
            &json!({ "event_id": "ev-series", "offline": true }),
        )
        .await
        .unwrap();
        assert!(ev_compact["event"].get("body_html").is_none());
        assert_eq!(ev_compact["event"]["omitted"]["fields"], json!(["body_html"]));
        assert_eq!(ev_compact["event"]["recurrence"]["pattern"]["type"], "weekly");

        // 5. Online search_mail (compact: true) also emits honest `omitted` and `hint`.
        let router = Router::new().route(
            "/v1.0/me/messages",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "msg-search-1",
                        "subject": "Budget Review",
                        "from": { "emailAddress": { "name": "Alice", "address": "alice@contoso.com" } },
                        "sender": { "emailAddress": { "name": "Mailer", "address": "mailer@contoso.com" } },
                        "receivedDateTime": "2026-11-20T10:00:00Z",
                        "isRead": false,
                        "isDraft": false,
                        "categories": ["Budget"],
                        "hasAttachments": false,
                        "bodyPreview": "Numbers inside"
                    }]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let online_cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: Some("tester@contoso.com".to_string()),
            auth_mode: "token".to_string(),
        };
        let online_auth = AuthManager::from_tokens(online_cfg.clone(), tokens);
        let online_graph = GraphClient::new(online_cfg.clone(), online_auth).unwrap();

        let search_compact = call_tool(
            &online_cfg,
            &online_graph,
            &storage,
            "search_mail",
            &json!({ "query": "Budget", "offline": false }),
        )
        .await
        .unwrap();
        assert_eq!(search_compact["messages"].as_array().unwrap().len(), 1);
        assert!(search_compact["omitted"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "categories"));
        assert!(search_compact["hint"]
            .as_str()
            .unwrap()
            .contains("get_message"));

        let search_full = call_tool(
            &online_cfg,
            &online_graph,
            &storage,
            "search_mail",
            &json!({ "query": "Budget", "offline": false, "compact": false }),
        )
        .await
        .unwrap();
        assert!(search_full.get("omitted").is_none());
        assert_eq!(search_full["messages"][0]["categories"], json!(["Budget"]));
        assert_eq!(
            search_full["messages"][0]["sender"]["address"],
            "mailer@contoso.com"
        );

        server.abort();
    }
