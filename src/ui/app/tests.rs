use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{AppConfig, DraftStore};
use crate::graph::GraphClient;
use crate::models::{
    CalendarEvent, CoreEvent, EmailRecipient, EventRsvpAction, MailAttachment, MailFolder,
    MailMessage, MailSearchHit, TodoList, TodoTask,
};
use crate::storage::Storage;
use crate::sync::{ActiveView, FocusTarget, SyncCommand, SyncController};

use super::super::nav_pane::{VIRTUAL_FLAGGED_FOLDER, VIRTUAL_UNREAD_FOLDER};
use super::super::overlays::mail_compose::{MailComposeMode, MailComposeOverlay};
use super::super::overlays::Overlay;
use super::*;

fn make_mail(id: &str, conv_id: &str, ts: &str, read: bool, flagged: bool) -> MailMessage {
    MailMessage {
        id: id.into(),
        internet_message_id: None,
        conversation_id: Some(conv_id.into()),
        folder_id: Some("inbox".into()),
        subject: format!("Subject {id}"),
        from: Some(EmailRecipient {
            name: Some("Alice".into()),
            address: "alice@contoso.com".into(),
        }),
        to_recipients: vec![],
        cc_recipients: vec![],
        bcc_recipients: vec![],
        reply_to: vec![],
        received_at: ts.into(),
        sent_at: None,
        last_modified_at: None,
        is_read: read,
        is_draft: false,
        flagged,
        importance: "normal".into(),
        inference_classification: Some("focused".into()),
        has_attachments: false,
        body_loaded: true,
        preview: "Hello".into(),
        body_html: "<p>Hello</p>".into(),
        body_plain: "Hello".into(),
        attachments: vec![],
        meeting_request: None,
        web_link: None,
        ..Default::default()
    }
}

fn make_test_app() -> (AppState, UiState) {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (event_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl =
        crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let ui = UiState::from_config(&cfg);
    (app, ui)
}

fn make_event_at(id: &str, start: chrono::DateTime<chrono::Utc>) -> CalendarEvent {
    let fmt = |t: chrono::DateTime<chrono::Utc>| {
        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    CalendarEvent {
        id: id.into(),
        subject: id.into(),
        start_iso: fmt(start),
        end_iso: fmt(start + chrono::Duration::hours(1)),
        time_zone: "UTC".into(),
        is_all_day: false,
        location: None,
        organizer_name: None,
        organizer_email: None,
        attendees: vec![],
        response_status: "none".into(),
        is_online_meeting: false,
        online_meeting_url: None,
        body_html: String::new(),
        body_plain: String::new(),
        is_cancelled: false,
        web_link: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn cached_calendar_load_is_not_starved_by_old_history() {
    let (mut app, _ui) = make_test_app();
    let now = chrono::Utc::now();
    let mut events: Vec<CalendarEvent> = (0..250)
        .map(|i| make_event_at(&format!("old-{i:03}"), now - chrono::Duration::days(60 + i)))
        .collect();
    for day in 1..=3 {
        events.push(make_event_at(
            &format!("upcoming-{day}"),
            now + chrono::Duration::days(day),
        ));
    }
    app.storage.upsert_calendar_events(&events).unwrap();

    app.load_cached_initial();
    let ids: Vec<&str> = app.calendar_events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["upcoming-1", "upcoming-2", "upcoming-3"]);
    assert_eq!(app.active_event_id.as_deref(), Some("upcoming-1"));
}

#[tokio::test]
async fn older_pages_extend_folder_and_survive_new_mail_at_the_head() {
    let (mut app, mut ui) = make_test_app();
    let mail = |idx: usize| {
        let id = format!("m{idx:03}");
        // Equal timestamps exercise SQLite's ID tie-breaker at the history boundary.
        make_mail(&id, &id, "2026-11-19T08:00:00Z", true, false)
    };
    app.storage.upsert_mail_messages(&(101..600).map(mail).collect::<Vec<_>>()).unwrap();
    app.select_mail_folder(&mut ui, "inbox".into());
    assert_eq!(app.mail_messages.len(), 150);

    for (rows, expected_selected, has_more) in [
        (51..101, "m450", true),
        (1..51, "m100", false),
    ] {
        let last_row = app.mail_messages.len() - 1;
        app.select_mail_row(&mut ui, last_row);
        assert_eq!(app.active_mail_id.as_deref(), Some(expected_selected));
        assert!(app.loading_older_mail);
        app.handle_core_event(&mut ui, CoreEvent::MailMessagesUpdated {
            folder_id: "inbox".into(), messages: vec![], notify_candidate: None,
        });
        assert!(app.loading_older_mail);

        app.storage.upsert_mail_messages(&rows.map(mail).collect::<Vec<_>>()).unwrap();
        let seq = app.mail_history_seq;
        app.handle_core_event(&mut ui, CoreEvent::MailHistoryPageFinished {
            folder_id: "inbox".into(), has_more: Some(has_more), seq,
        });
        assert!(!app.loading_older_mail);
        assert_eq!(ui.mail_list_scroll.selected_idx, Some(last_row));
        assert_eq!(app.active_mail_id.as_deref(), Some(expected_selected));
    }

    // A new arrival must keep the prior oldest visible mail, including when selected.
    app.select_mail_row(&mut ui, 549);
    assert_eq!(app.active_mail_id.as_deref(), Some("m050"));
    app.storage.upsert_mail_messages(&[mail(600)]).unwrap();
    app.handle_core_event(&mut ui, CoreEvent::MailMessagesUpdated {
        folder_id: "inbox".into(), messages: vec![], notify_candidate: None,
    });
    assert_eq!(app.mail_messages.len(), 551);
    assert_eq!(app.active_mail_id.as_deref(), Some("m050"));
    assert_eq!(ui.mail_list_scroll.selected_idx, Some(550));

    // An exhausted Graph cursor still allows paging through the remaining local rows.
    let seq = app.mail_history_seq;
    app.handle_core_event(&mut ui, CoreEvent::MailHistoryPageFinished {
        folder_id: "inbox".into(), has_more: Some(false), seq,
    });
    assert_eq!(app.mail_messages.len(), 600);
    assert!(app.mail_oldest_reached);
}

#[tokio::test]
async fn mail_thread_grouping_and_optimistic_triage() {
    let (mut app, mut ui) = make_test_app();
    let m1 = make_mail("m1", "conv-A", "2026-11-19T08:00:00Z", true, false);
    let m2 = make_mail("m2", "conv-A", "2026-11-19T10:00:00Z", false, false);
    let m3 = make_mail("m3", "conv-B", "2026-11-19T09:00:00Z", false, true);
    app.mail_messages = vec![m1, m2, m3];
    app.storage.upsert_mail_messages(&app.mail_messages).unwrap();

    // Thread grouping ON -> 2 rows (conv-A with count 2, conv-B with count 1)
    ui.group_threads = true;
    let grouped = app.visible_mail_rows(&ui);
    assert_eq!(grouped.len(), 2);
    assert_eq!(app.mail_messages[grouped[0].primary_msg_idx].id, "m2");
    assert_eq!(grouped[0].count, 2);
    assert_eq!(grouped[0].unread_count, 1);

    // Thread grouping OFF -> 3 flat rows
    ui.group_threads = false;
    let flat = app.visible_mail_rows(&ui);
    assert_eq!(flat.len(), 3);

    // Optimistic read + flag + archive state
    app.active_mail_id = Some("m2".into());
    ui.mail_list_scroll.selected_idx = Some(0);
    app.toggle_mail_read(&mut ui);
    assert!(app.active_mail().unwrap().is_read);
    app.toggle_mail_flag(&mut ui);
    assert!(app.active_mail().unwrap().flagged);
    app.archive_selected_mail(&mut ui);
    assert!(!app.mail_messages.iter().any(|m| m.id == "m2"));

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn calendar_rsvp_and_todo_completion_toggle_optimistic() {
    let (mut app, mut ui) = make_test_app();
    let ev = CalendarEvent {
        id: "ev-1".into(),
        subject: "Sync".into(),
        start_iso: "2026-11-19T14:00:00Z".into(),
        end_iso: "2026-11-19T15:00:00Z".into(),
        time_zone: "UTC".into(),
        is_all_day: false,
        location: None,
        organizer_name: None,
        organizer_email: None,
        attendees: vec![],
        response_status: "notResponded".into(),
        is_online_meeting: false,
        online_meeting_url: None,
        body_html: String::new(),
        body_plain: String::new(),
        is_cancelled: false,
        web_link: None,
        ..Default::default()
    };
    app.calendar_events = vec![ev.clone()];
    app.storage.upsert_calendar_events(&[ev]).unwrap();
    app.respond_calendar_event(&mut ui, "ev-1".into(), EventRsvpAction::Accept);
    assert_eq!(app.calendar_events[0].response_status, "accepted");

    let task = TodoTask {
        id: "t-1".into(),
        list_id: "list-1".into(),
        title: "Ship Outlook TUI".into(),
        status: "notStarted".into(),
        importance: "high".into(),
        body_plain: None,
        due_date_iso: None,
        reminder_iso: None,
        completed_at: None,
        created_at: None,
        updated_at: None,
    };
    app.todo_tasks = vec![task.clone()];
    app.active_todo_list_id = Some("list-1".into());
    app.active_todo_task_id = Some("t-1".into());
    app.storage.upsert_todo_tasks(&[task]).unwrap();
    app.toggle_todo_completed(&mut ui);
    assert!(app.todo_tasks[0].is_completed());
    app.toggle_todo_completed(&mut ui);
    assert!(!app.todo_tasks[0].is_completed());

    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[test]
fn jump_to_search_hit_preserves_pinned_mail_across_updates_and_triggers_hydration() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    ui.focused_only = true;

    let recent = make_mail("m-recent", "conv-r", "2026-11-19T10:00:00Z", true, false);
    let mut older = make_mail("m-old", "conv-old", "2024-06-01T00:00:00Z", false, false);
    older.folder_id = Some("f-inbox".into());
    older.inference_classification = Some("other".into());
    older.body_loaded = false;
    older.body_plain.clear();
    older.body_html.clear();

    app.storage
        .upsert_mail_messages(&[recent.clone(), older.clone()])
        .unwrap();
    app.search_query = "old".into();
    app.search_results = vec![MailSearchHit {
        id: "m-old".into(),
        conversation_id: Some("conv-old".into()),
        folder_id: Some("f-inbox".into()),
        subject: older.subject.clone(),
        from_display: "Alice".into(),
        received_at: older.received_at.clone(),
        snippet: "Hello".into(),
        has_attachments: false,
        is_read: false,
        flagged: false,
    }];

    let _ = crate::ui::action::dispatch(
        crate::ui::action::Action::JumpToSearchHit(0),
        &mut app,
        &mut ui,
    );

    assert!(!ui.focused_only, "JumpToSearchHit must unhide Other mail");
    assert_eq!(app.active_mail_id.as_deref(), Some("m-old"));
    assert_eq!(
        app.pinned_mail.as_ref().map(|m| m.id.as_str()),
        Some("m-old")
    );

    let mut saw_hydrate_old = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m-old")
        {
            saw_hydrate_old = true;
        }
    }
    assert!(saw_hydrate_old, "JumpToSearchHit must request hydration");

    // Simulate folder sync replacing list without older hit -> pinned_mail must survive!
    app.handle_ui_msg(
        &mut ui,
        UiMsg::FolderMessagesLoaded {
            folder_id: "f-inbox".into(),
            messages: vec![recent.clone()],
        },
    );
    assert_eq!(app.active_mail().map(|m| m.id.as_str()), Some("m-old"));
    assert!(app.mail_messages.iter().any(|m| m.id == "m-old"));

    // Simulate MailMessagesUpdated after older is deleted from top-N storage -> still preserved via pinned_mail!
    app.storage
        .delete_mail_messages_by_ids(&["m-old".into()])
        .unwrap();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailMessagesUpdated {
            folder_id: "f-inbox".into(),
            messages: vec![recent],
            notify_candidate: None,
        },
    );
    assert_eq!(app.active_mail().map(|m| m.id.as_str()), Some("m-old"));
    assert!(app.mail_messages.iter().any(|m| m.id == "m-old"));

    // Hydrating the pinned older message updates both mail_messages and pinned_mail.
    let mut hydrated = older.clone();
    hydrated.body_loaded = true;
    hydrated.body_plain = "Full older body".into();
    app.handle_core_event(&mut ui, CoreEvent::MailMessageHydrated(Box::new(hydrated)));
    assert_eq!(
        app.active_mail().map(|m| (m.body_loaded, m.body_plain.as_str())),
        Some((true, "Full older body"))
    );
    assert_eq!(
        app.pinned_mail
            .as_ref()
            .map(|m| (m.body_loaded, m.body_plain.as_str())),
        Some((true, "Full older body"))
    );
}

#[test]
fn bootstrap_and_folders_updated_resolve_inbox_and_hydrate_cached_active_mail() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    let mut cached = make_mail("m-cached", "conv-c", "2026-11-19T09:00:00Z", false, false);
    cached.folder_id = Some("f-inbox-real".into());
    cached.body_loaded = false;
    app.mail_messages = vec![cached.clone()];
    app.active_mail_id = Some("m-cached".into());
    app.storage.upsert_mail_messages(&[cached.clone()]).unwrap();

    let inbox_folder = MailFolder {
        id: "f-inbox-real".into(),
        display_name: "받은 편지함".into(),
        well_known_name: Some("inbox".into()),
        parent_folder_id: None,
        child_folder_count: 0,
        unread_item_count: 1,
        total_item_count: 1,
    };
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailFoldersUpdated(vec![inbox_folder]),
    );

    assert_eq!(app.active_folder_id, "f-inbox-real");
    let mut hydrated_req = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m-cached")
        {
            hydrated_req = true;
        }
    }
    assert!(
        hydrated_req,
        "unhydrated active_mail_id must be hydrated when Inbox resolves"
    );
}

#[test]
fn remote_search_results_merge_sort_and_failed_clears_searching() {
    let (mut app, mut ui) = {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        (
            AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx),
            UiState::from_config(&cfg),
        )
    };

    app.search_query = "invoice".into();
    app.searching = true;
    app.search_results = vec![MailSearchHit {
        id: "m1".into(),
        conversation_id: None,
        folder_id: Some("f1".into()),
        subject: "Local invoice".into(),
        from_display: "Bob".into(),
        received_at: "2026-11-10T00:00:00Z".into(),
        snippet: "local".into(),
        has_attachments: false,
        is_read: true,
        flagged: false,
    }];

    let dup_local = make_mail("m1", "c1", "2026-11-10T00:00:00Z", true, false);
    let newer_remote = make_mail("m2", "c2", "2026-11-19T00:00:00Z", false, true);
    app.handle_ui_msg(
        &mut ui,
        UiMsg::RemoteSearchResults {
            query: "invoice".into(),
            generation: app.search_generation,
            messages: vec![dup_local, newer_remote],
        },
    );
    assert!(!app.searching);
    assert!(app.search_remote_done);
    assert!(app.search_error.is_none());
    assert_eq!(app.search_results.len(), 2);
    assert_eq!(app.search_results[0].id, "m2");
    assert_eq!(app.search_results[1].id, "m1");

    app.searching = true;
    app.handle_ui_msg(
        &mut ui,
        UiMsg::RemoteSearchFailed {
            query: "invoice".into(),
            generation: app.search_generation,
            error: "timeout".into(),
        },
    );
    assert!(!app.searching);
    assert!(!app.search_remote_done);
    assert_eq!(app.search_error.as_deref(), Some("timeout"));
}

#[test]
fn summary_list_lazy_loads_displayed_conversation_and_expanded_messages() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    app.storage
        .upsert_mail_folders(&[
            MailFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 4,
            },
            MailFolder {
                id: "f-sent".into(),
                display_name: "Sent Items".into(),
                well_known_name: Some("sentitems".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 1,
            },
        ])
        .unwrap();

    let mut t1_unhydrated =
        make_mail("m-t1", "conv-thread", "2026-11-19T08:00:00Z", true, false);
    t1_unhydrated.folder_id = Some("f-inbox".into());
    t1_unhydrated.body_loaded = false;
    t1_unhydrated.body_html.clear();
    t1_unhydrated.body_plain.clear();

    let mut t2_local =
        make_mail("m-t2", "conv-thread", "2026-11-19T09:00:00Z", true, false);
    t2_local.folder_id = Some("f-inbox".into());
    t2_local.body_html = "<p>Local middle body</p>".into();
    t2_local.body_plain = "Local middle body".into();

    let mut t3_latest =
        make_mail("m-t3", "conv-thread", "2026-11-19T11:00:00Z", false, true);
    t3_latest.folder_id = Some("f-inbox".into());
    t3_latest.body_html = "<p>Latest thread body</p>".into();
    t3_latest.body_plain = "Latest thread body".into();

    let mut other = make_mail("m-other", "conv-other", "2026-11-19T10:00:00Z", true, false);
    other.folder_id = Some("f-inbox".into());
    other.body_html = "<p>Other conversation body</p>".into();
    other.body_plain = "Other conversation body".into();

    let mut sent = make_mail("m-sent", "conv-sent", "2026-11-19T12:00:00Z", true, false);
    sent.folder_id = Some("f-sent".into());
    sent.to_recipients = vec![
        EmailRecipient {
            name: Some("Bob".into()),
            address: "bob@contoso.com".into(),
        },
        EmailRecipient {
            name: Some("Carol".into()),
            address: "carol@contoso.com".into(),
        },
    ];
    sent.body_html = "<p>Sent body</p>".into();
    sent.body_plain = "Sent body".into();

    app.storage
        .upsert_mail_messages(&[t1_unhydrated, t2_local, t3_latest, other, sent])
        .unwrap();

    // Initial load keeps summaries in mail_messages and lazily loads ONLY the active message
    app.load_cached_initial();
    assert_eq!(app.mail_messages.len(), 4);
    assert!(app.mail_messages.iter().all(|m| m.body_html.is_empty()));
    assert_eq!(app.active_mail_id.as_deref(), Some("m-t3"));
    assert_eq!(app.mail_detail_cache.len(), 1);
    assert_eq!(
        app.active_mail().map(|m| m.body_html.as_str()),
        Some("<p>Latest thread body</p>")
    );
    // Local body_loaded == true must NOT request Graph hydration
    assert!(cmd_rx.try_recv().is_err());

    // Expanding a locally cached thread message loads it by ID without Graph hydration
    app.expanded_thread_ids.insert("m-t2".into());
    app.sync_displayed_mail_details(true, true);
    assert_eq!(app.mail_detail_cache.len(), 2);
    assert_eq!(
        app.mail_detail("m-t2").map(|m| m.body_html.as_str()),
        Some("<p>Local middle body</p>")
    );
    assert!(cmd_rx.try_recv().is_err());

    // Expanding an unhydrated thread message triggers Graph HydrateMessage
    app.expanded_thread_ids.insert("m-t1".into());
    app.sync_displayed_mail_details(true, true);
    let mut requested_t1 = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m-t1")
        {
            requested_t1 = true;
        }
    }
    assert!(requested_t1, "unhydrated expanded message must request Graph hydration");

    // Hydrating m-t1 caches full body in mail_detail_cache while mail_messages stays summary-only
    let mut t1_full = make_mail("m-t1", "conv-thread", "2026-11-19T08:00:00Z", true, false);
    t1_full.folder_id = Some("f-inbox".into());
    t1_full.body_html = "<p>Hydrated t1 body</p>".into();
    t1_full.body_plain = "Hydrated t1 body".into();
    t1_full.web_link = Some("https://outlook.office.com/mail/m-t1".into());
    app.handle_core_event(&mut ui, CoreEvent::MailMessageHydrated(Box::new(t1_full)));
    assert_eq!(
        app.mail_detail("m-t1").map(|m| m.body_html.as_str()),
        Some("<p>Hydrated t1 body</p>")
    );
    assert!(
        app.mail_messages
            .iter()
            .find(|m| m.id == "m-t1")
            .is_some_and(|m| m.body_loaded
                && m.body_html.is_empty()
                && m.web_link.as_deref() == Some("https://outlook.office.com/mail/m-t1")),
        "mail_messages must keep summary projection with web_link after hydration"
    );

    // Background hydration of an undisplayed message must NOT retain its body in RAM
    let mut other_full =
        make_mail("m-other", "conv-other", "2026-11-19T10:00:00Z", true, false);
    other_full.folder_id = Some("f-inbox".into());
    other_full.body_html = "<p>Prefetched undisplayed body</p>".into();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailMessageHydrated(Box::new(other_full)),
    );
    assert!(!app.mail_detail_cache.contains_key("m-other"));
    assert!(app
        .mail_messages
        .iter()
        .find(|m| m.id == "m-other")
        .unwrap()
        .body_html
        .is_empty());

    // Selecting the next conversation evicts old details and loads only m-other
    app.select_mail_row(&mut ui, 1);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-other"));
    assert_eq!(app.mail_detail_cache.len(), 1);
    assert_eq!(
        app.active_mail().map(|m| m.body_html.as_str()),
        Some("<p>Other conversation body</p>")
    );

    // Switching to Sent retains first to_recipient on summaries and loads active full body
    app.select_mail_folder(&mut ui, "f-sent".into());
    assert_eq!(app.mail_messages.len(), 1);
    assert!(app.mail_messages[0].body_html.is_empty());
    assert_eq!(app.mail_messages[0].to_recipients.len(), 1);
    assert_eq!(app.mail_messages[0].to_recipients[0].short_display(), "Bob");
    assert_eq!(app.active_mail().unwrap().to_recipients.len(), 2);
    assert_eq!(app.active_mail().unwrap().body_html, "<p>Sent body</p>");

    // Virtual Unread & Flagged folders filter via SQLite summary projection
    app.select_mail_folder(&mut ui, VIRTUAL_UNREAD_FOLDER.into());
    assert_eq!(app.mail_messages.len(), 1);
    assert_eq!(app.mail_messages[0].id, "m-t3");
    assert!(app.mail_messages[0].body_html.is_empty());
    assert_eq!(app.active_mail().unwrap().body_html, "<p>Latest thread body</p>");

    app.select_mail_folder(&mut ui, VIRTUAL_FLAGGED_FOLDER.into());
    assert_eq!(app.mail_messages.len(), 1);
    assert_eq!(app.mail_messages[0].id, "m-t3");
}

#[test]
fn summary_detail_cache_invalidates_and_stays_consistent_on_edits_and_sync() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    app.storage
        .upsert_mail_folders(&[MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 2,
            total_item_count: 2,
        }])
        .unwrap();

    let mut m1 = make_mail("m1", "c1", "2026-11-19T09:00:00Z", false, false);
    m1.folder_id = Some("f-inbox".into());
    m1.body_html = "<p>Body v1</p>".into();
    let mut m2 = make_mail("m2", "c2", "2026-11-19T10:00:00Z", false, false);
    m2.folder_id = Some("f-inbox".into());
    m2.body_html = "<p>Body m2</p>".into();
    app.storage
        .upsert_mail_messages(&[m1.clone(), m2.clone()])
        .unwrap();

    app.select_mail_folder(&mut ui, "f-inbox".into());
    assert_eq!(app.active_mail_id.as_deref(), Some("m2"));

    // Read & flag toggles update summary list, detail cache, and SQLite together
    app.toggle_mail_read(&mut ui);
    assert!(app.mail_messages.iter().find(|m| m.id == "m2").unwrap().is_read);
    assert!(app.active_mail().unwrap().is_read);
    assert!(app.storage.get_mail_message("m2").unwrap().unwrap().is_read);

    app.toggle_mail_flag(&mut ui);
    assert!(app.mail_messages.iter().find(|m| m.id == "m2").unwrap().flagged);
    assert!(app.active_mail().unwrap().flagged);
    assert!(app.storage.get_mail_message("m2").unwrap().unwrap().flagged);

    // Sync update invalidates cached detail and refreshes updated body from SQLite
    let mut m2_updated = m2.clone();
    m2_updated.is_read = true;
    m2_updated.flagged = true;
    m2_updated.body_html = "<p>Body m2 edited</p>".into();
    app.storage.upsert_mail_messages(&[m2_updated]).unwrap();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailMessagesUpdated {
            folder_id: "f-inbox".into(),
            messages: vec![],
            notify_candidate: None,
        },
    );
    assert!(app.mail_messages.iter().all(|m| m.body_html.is_empty()));
    assert_eq!(app.active_mail().unwrap().body_html, "<p>Body m2 edited</p>");

    // Archiving removes old detail and lazily loads next message
    app.archive_selected_mail(&mut ui);
    assert!(!app.mail_detail_cache.contains_key("m2"));
    assert_eq!(app.active_mail_id.as_deref(), Some("m1"));
    assert_eq!(app.active_mail().unwrap().body_html, "<p>Body v1</p>");

    // Deleting last message clears active selection and detail cache
    app.delete_selected_mail(&mut ui);
    assert!(app.active_mail_id.is_none());
    assert!(app.mail_detail_cache.is_empty());
}

#[tokio::test]
async fn repeated_draft_save_uses_created_id_and_patches_only_changed_fields() {
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};

    let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let release_post = Arc::new(tokio::sync::Notify::new());
    let router = Router::new()
        .route(
            "/v1.0/me/messages",
            post({
                let release_post = Arc::clone(&release_post);
                move |State(writes): State<Arc<std::sync::Mutex<Vec<Value>>>>,
                      Json(mut body): Json<Value>| async move {
                    writes.lock().unwrap().push(body.clone());
                    release_post.notified().await;
                    body["id"] = json!("same-id");
                    Json(body)
                }
            }),
        )
        .route(
            "/v1.0/me/messages/same-id",
            axum::routing::patch(
                |State(writes): State<Arc<std::sync::Mutex<Vec<Value>>>>,
                 Json(body): Json<Value>| async move {
                    writes.lock().unwrap().push(body);
                    axum::http::StatusCode::NO_CONTENT
                },
            )
            .get(|| async {
                Json(json!({ "id": "same-id", "subject": "Revised", "isDraft": true }))
            }),
        )
        .with_state(Arc::clone(&writes));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg = AppConfig {
        graph_base_url: format!("http://{}/v1.0", listener.local_addr().unwrap()),
        ..AppConfig::default()
    };
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let auth = AuthManager::from_tokens(
        cfg.clone(),
        crate::auth::StoredTokens {
            access_token: "synthetic-tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        },
    );
    let (mut app, mut ui) = make_test_app();
    app.graph = GraphClient::new(cfg, auth).unwrap();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    app.ui_tx = ui_tx;
    let profile = tempfile::tempdir().unwrap();
    app.draft_store_path = profile.path().join("drafts.json");
    app.draft_store = DraftStore::default();
    let attachment = profile.path().join("check.txt");
    std::fs::write(&attachment, "report").unwrap();

    let mut compose = MailComposeOverlay::new_to("bob@example.com");
    compose.subject = "Original".into();
    compose.cc = "person@example.com".into();
    compose.body.load_text("**Review** the file.");
    compose.attach_file(attachment);
    app.save_mail_draft(&mut ui, &compose);
    compose.subject = "Revised".into();
    ui.push_overlay(Overlay::mail_compose(compose.clone()));
    app.save_mail_draft(&mut ui, &compose);
    release_post.notify_one();

    while app.has_pending_draft_write() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
    }
    let writes = writes.lock().unwrap();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0]["subject"], "Original");
    assert_eq!(writes[0]["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(writes[1], json!({ "subject": "Revised" }));
    let local = DraftStore::load_from(&app.draft_store_path).unwrap();
    let restored = MailComposeOverlay::from_draft_entry(
        MailComposeMode::New,
        local.get_mail("compose:new").unwrap(),
    );
    assert_eq!(restored.draft_id.as_deref(), Some("same-id"));
    assert_eq!(restored.subject, "Revised");
    assert_eq!(restored.body.text(), "**Review** the file.");
    let Some(Overlay::MailCompose(open)) = ui.overlays.last() else {
        panic!("compose closed")
    };
    assert_eq!(open.draft_id, restored.draft_id);
    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn submit_mail_compose_preserves_draft_on_send_failure_and_clears_on_success() {
    use axum::{http::StatusCode, routing::post, Json, Router};
    use std::sync::atomic::{AtomicBool, Ordering};

    let should_fail = Arc::new(AtomicBool::new(true));
    let fail_c = Arc::clone(&should_fail);

    let app_router = Router::new().route(
        "/v1.0/me/sendMail",
        post(move || {
            let fail = fail_c.load(Ordering::SeqCst);
            async move {
                if fail {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": { "code": "ErrorInternalServerError", "message": "SMTP failed" }
                        })),
                    )
                } else {
                    (StatusCode::ACCEPTED, Json(serde_json::json!({})))
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "synthetic-tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("u1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (event_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl =
        crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    let profile = tempfile::tempdir().unwrap();
    app.draft_store_path = profile.path().join("drafts.json");
    app.draft_store = DraftStore::default();
    let mut compose = MailComposeOverlay::new_to("bob@example.com");
    compose.subject = "Important Proposal".into();
    compose.body.load_text("Do not lose this body text.");

    // 1. Send fails with 500 -> draft MUST remain preserved in draft_store
    assert!(app.submit_mail_compose(&mut ui, &compose));
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("received task failure")
        .unwrap();
    app.handle_ui_msg(&mut ui, msg);
    let saved_draft = app
        .draft_store
        .get_mail("compose:new")
        .expect("draft must be preserved after send failure");
    assert_eq!(saved_draft.subject, "Important Proposal");
    assert_eq!(saved_draft.body, "Do not lose this body text.");
    assert_eq!(saved_draft.to, "bob@example.com, ");

    // 2. Retry succeeds -> UiMsg::MailSent clears draft
    should_fail.store(false, Ordering::SeqCst);
    assert!(app.submit_mail_compose(&mut ui, &compose));
    for _ in 0..2 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("received send completion")
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
    }
    assert!(
        app.draft_store.get_mail("compose:new").is_none(),
        "draft must be cleared once Graph send succeeds"
    );
    app.sync_ctrl.request(SyncCommand::Shutdown);
}

#[tokio::test]
async fn tui_move_replaces_old_id_and_real_folder_and_empty_updates_work() {
    use axum::{routing::post, Json, Router};

    let app_router = Router::new().route(
        "/v1.0/me/messages/m1/move",
        post(|| async {
            Json(serde_json::json!({
                "id": "m1-moved",
                "conversationId": "conv-1",
                "parentFolderId": "f-archive",
                "subject": "Move Target",
                "receivedDateTime": "2026-11-19T09:00:00Z",
                "isRead": false,
                "bodyPreview": "Move body",
                "body": { "contentType": "html", "content": "<p>Move body</p>" }
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "synthetic-tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("u1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    app.mail_folders = vec![
        MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 1,
        },
        MailFolder {
            id: "f-archive".into(),
            display_name: "Archive".into(),
            well_known_name: Some("archive".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 0,
        },
    ];
    app.storage.upsert_mail_folders(&app.mail_folders).unwrap();
    let mut m1 = make_mail("m1", "conv-1", "2026-11-19T09:00:00Z", false, false);
    m1.folder_id = Some("f-inbox".into());
    m1.subject = "Move Target".into();
    m1.body_plain = "Move body".into();
    app.storage.upsert_mail_messages(&[m1]).unwrap();
    app.select_mail_folder(&mut ui, "f-inbox".into());
    while nudge_rx.try_recv().is_ok() {}

    // Archive replaces old ID m1 -> m1-moved in SQLite + FTS without duplicates
    app.archive_selected_mail(&mut ui);
    let moved_msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("received MailMoved")
        .unwrap();
    app.handle_ui_msg(&mut ui, moved_msg);
    assert!(app.storage.get_mail_message("m1").unwrap().is_none());
    let moved_row = app.storage.get_mail_message("m1-moved").unwrap().unwrap();
    assert_eq!(moved_row.folder_id.as_deref(), Some("f-archive"));
    let fts_hits = app.storage.search_mail("Move Target", None, 10).unwrap();
    assert_eq!(fts_hits.len(), 1);
    assert_eq!(fts_hits[0].id, "m1-moved");

    // MailMessagesUpdated with alias "inbox" still matches active real folder "f-inbox"
    // and updates list even when folder becomes empty!
    let mut m2 = make_mail("m2", "conv-2", "2026-11-19T10:00:00Z", false, false);
    m2.folder_id = Some("f-inbox".into());
    m2.body_loaded = false;
    app.storage.upsert_mail_messages(&[m2]).unwrap();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailMessagesUpdated {
            folder_id: "inbox".into(),
            messages: vec![],
            notify_candidate: None,
        },
    );
    assert_eq!(app.mail_messages.len(), 1);
    assert_eq!(app.mail_messages[0].id, "m2");
    // Since m2 has body_loaded == false, HydrateMessage was requested
    let mut saw_hydrate = false;
    while let Ok(cmd) = nudge_rx.try_recv() {
        if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m2") {
            saw_hydrate = true;
        }
    }
    assert!(saw_hydrate, "invalidated message must trigger HydrateMessage");

    // Deleting last message remotely empties TUI list on MailMessagesUpdated
    app.storage
        .delete_mail_messages_by_ids(&["m2".into()])
        .unwrap();
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailMessagesUpdated {
            folder_id: "f-inbox".into(),
            messages: vec![],
            notify_candidate: None,
        },
    );
    assert!(app.mail_messages.is_empty());
    assert!(app.active_mail_id.is_none());
}

/// App wired to a mock Graph whose only working route moves `m-a`; every other call fails.
async fn make_mutation_app() -> (
    AppState,
    UiState,
    mpsc::UnboundedReceiver<UiMsg>,
    mpsc::UnboundedReceiver<SyncCommand>,
) {
    use axum::{routing::post, Json, Router};

    let app_router = Router::new().route(
        "/v1.0/me/messages/m-a/move",
        post(|| async {
            Json(serde_json::json!({
                "id": "m-a-moved",
                "conversationId": "conv-a",
                "parentFolderId": "f-archive",
                "subject": "Subject m-a",
                "receivedDateTime": "2026-11-19T11:00:00Z",
                "isRead": false
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "synthetic-tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("u1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let ui = UiState::from_config(&cfg);

    let folder = |id: &str, name: &str, wk: &str| MailFolder {
        id: id.into(),
        display_name: name.into(),
        well_known_name: Some(wk.into()),
        parent_folder_id: None,
        child_folder_count: 0,
        unread_item_count: 0,
        total_item_count: 0,
    };
    app.mail_folders = vec![
        folder("f-inbox", "Inbox", "inbox"),
        folder("f-archive", "Archive", "archive"),
        folder("f-sent", "Sent Items", "sentitems"),
    ];
    app.storage.upsert_mail_folders(&app.mail_folders).unwrap();
    // Newest first: m-a, m-b, m-c in the Inbox, s-1 in Sent.
    let mut seed = vec![
        make_mail("m-a", "conv-a", "2026-11-19T11:00:00Z", false, false),
        make_mail("m-b", "conv-b", "2026-11-19T10:00:00Z", false, false),
        make_mail("m-c", "conv-c", "2026-11-19T09:00:00Z", false, false),
        make_mail("s-1", "conv-s", "2026-11-19T08:00:00Z", true, false),
    ];
    for m in &mut seed {
        m.folder_id = Some(if m.id == "s-1" { "f-sent" } else { "f-inbox" }.into());
    }
    app.storage.upsert_mail_messages(&seed).unwrap();
    (app, ui, ui_rx, nudge_rx)
}

async fn next_ui_msg(ui_rx: &mut mpsc::UnboundedReceiver<UiMsg>) -> UiMsg {
    tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("a UiMsg within 2s")
        .expect("ui channel open")
}

fn listed_ids(app: &AppState) -> Vec<&str> {
    app.mail_messages.iter().map(|m| m.id.as_str()).collect()
}

fn cached_folder(app: &AppState, id: &str) -> Option<String> {
    app.storage.get_mail_message(id).unwrap().and_then(|m| m.folder_id)
}

#[tokio::test]
async fn failed_archive_puts_the_message_back_in_cache_and_list() {
    let (mut app, mut ui, mut ui_rx, _nudge_rx) = make_mutation_app().await;
    app.select_mail_folder(&mut ui, "f-inbox".into());
    app.select_mail_row(&mut ui, 1);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-b"));

    app.archive_selected_mail(&mut ui);
    assert_eq!(listed_ids(&app), ["m-a", "m-c"], "optimistic removal");
    assert_eq!(cached_folder(&app, "m-b").as_deref(), Some("f-archive"));

    let failure = next_ui_msg(&mut ui_rx).await;
    assert!(matches!(failure, UiMsg::MailMoveFailed { ref label, .. } if label == "Archive"));
    app.handle_ui_msg(&mut ui, failure);
    assert_eq!(listed_ids(&app), ["m-a", "m-b", "m-c"]);
    assert_eq!(cached_folder(&app, "m-b").as_deref(), Some("f-inbox"));
}

#[tokio::test]
async fn failed_move_keeps_completed_moves_and_restores_the_rest() {
    let (mut app, mut ui, mut ui_rx, _nudge_rx) = make_mutation_app().await;
    app.select_mail_folder(&mut ui, "f-inbox".into());

    // m-a moves, m-b is rejected by Graph, m-c is never attempted.
    app.move_mail_to_folder(
        &mut ui,
        vec!["m-a".into(), "m-b".into(), "m-c".into()],
        "f-archive".into(),
        "Archive".into(),
    );
    assert!(app.mail_messages.is_empty(), "optimistic removal of all three");

    let moved = next_ui_msg(&mut ui_rx).await;
    assert!(matches!(moved, UiMsg::MailMoved { ref moves } if moves.len() == 1));
    app.handle_ui_msg(&mut ui, moved);
    let failure = next_ui_msg(&mut ui_rx).await;
    match &failure {
        UiMsg::MailMoveFailed { restored, label, .. } => {
            assert_eq!(label, "Move email");
            let ids: Vec<&str> = restored.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, ["m-b", "m-c"]);
        }
        _ => panic!("expected MailMoveFailed"),
    }
    app.handle_ui_msg(&mut ui, failure);

    assert_eq!(listed_ids(&app), ["m-b", "m-c"]);
    assert!(app.active_mail_id.is_some(), "the emptied list gets a selection back");
    assert!(app.storage.get_mail_message("m-a").unwrap().is_none());
    assert_eq!(cached_folder(&app, "m-a-moved").as_deref(), Some("f-archive"));
    assert_eq!(cached_folder(&app, "m-b").as_deref(), Some("f-inbox"));
    assert_eq!(cached_folder(&app, "m-c").as_deref(), Some("f-inbox"));
}

#[tokio::test]
async fn failed_move_seen_from_another_folder_restores_cache_but_not_that_list() {
    let (mut app, mut ui, mut ui_rx, _nudge_rx) = make_mutation_app().await;
    app.select_mail_folder(&mut ui, "f-inbox".into());
    app.select_mail_row(&mut ui, 1);
    app.archive_selected_mail(&mut ui);

    // The user is already looking at Sent when the failure arrives.
    app.select_mail_folder(&mut ui, "f-sent".into());
    assert_eq!(listed_ids(&app), ["s-1"]);
    let failure = next_ui_msg(&mut ui_rx).await;
    app.handle_ui_msg(&mut ui, failure);

    assert_eq!(listed_ids(&app), ["s-1"]);
    assert_eq!(app.active_mail_id.as_deref(), Some("s-1"));
    assert_eq!(cached_folder(&app, "m-b").as_deref(), Some("f-inbox"));
}

#[tokio::test]
async fn failed_read_and_flag_toggles_are_rolled_back() {
    let (mut app, mut ui, mut ui_rx, _nudge_rx) = make_mutation_app().await;
    app.select_mail_folder(&mut ui, "f-inbox".into());
    assert_eq!(app.active_mail_id.as_deref(), Some("m-a"));
    let state = |app: &AppState| {
        let listed = app.mail_messages.iter().find(|m| m.id == "m-a").unwrap();
        let cached = app.storage.get_mail_message("m-a").unwrap().unwrap();
        ((listed.is_read, listed.flagged), (cached.is_read, cached.flagged))
    };
    assert_eq!(state(&app), ((false, false), (false, false)));

    app.toggle_mail_read(&mut ui);
    assert_eq!(state(&app).0, (true, false), "optimistic toggle");
    let failure = next_ui_msg(&mut ui_rx).await;
    assert!(matches!(failure, UiMsg::MailFlagReverted { is_read: Some(false), flagged: None, .. }));
    app.handle_ui_msg(&mut ui, failure);
    assert_eq!(state(&app), ((false, false), (false, false)));

    app.toggle_mail_flag(&mut ui);
    assert_eq!(state(&app).0, (false, true), "optimistic toggle");
    let failure = next_ui_msg(&mut ui_rx).await;
    assert!(matches!(failure, UiMsg::MailFlagReverted { is_read: None, flagged: Some(false), .. }));
    app.handle_ui_msg(&mut ui, failure);
    assert_eq!(state(&app), ((false, false), (false, false)));
}

fn bootstrap_with_inbox(messages: Vec<MailMessage>) -> UiMsg {
    UiMsg::BootstrapDone {
        me: None,
        mail_folders: Vec::new(),
        mail_messages: messages,
        calendar_events: Vec::new(),
        contacts: Vec::new(),
        todo_lists: Vec::new(),
        todo_tasks: Vec::new(),
        scope_warning: None,
    }
}

#[tokio::test]
async fn bootstrap_replaces_the_list_only_while_inbox_is_active() {
    let (mut app, mut ui, _ui_rx, _nudge_rx) = make_mutation_app().await;
    let mut fresh = make_mail("m-new", "conv-n", "2026-11-20T09:00:00Z", false, false);
    fresh.folder_id = Some("f-inbox".into());

    // Sent is on screen: the Inbox payload must not replace it.
    app.select_mail_folder(&mut ui, "f-sent".into());
    app.handle_ui_msg(&mut ui, bootstrap_with_inbox(vec![fresh.clone()]));
    assert_eq!(listed_ids(&app), ["s-1"]);

    // Inbox is on screen: unchanged behaviour, the payload becomes the list.
    app.select_mail_folder(&mut ui, "f-inbox".into());
    app.handle_ui_msg(&mut ui, bootstrap_with_inbox(vec![fresh]));
    assert_eq!(listed_ids(&app), ["m-new"]);
}

#[tokio::test]
async fn startup_selects_the_newest_message_the_focused_filter_shows() {
    let (mut app, mut ui, _ui_rx, _nudge_rx) = make_mutation_app().await;
    let mut newest_other = make_mail("m-other", "conv-o", "2026-11-21T09:00:00Z", false, false);
    newest_other.folder_id = Some("f-inbox".into());
    newest_other.inference_classification = Some("other".into());
    app.storage.upsert_mail_messages(&[newest_other]).unwrap();

    ui.focused_only = false;
    app.load_cached_startup(&mut ui);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-other"));

    ui.focused_only = true;
    app.load_cached_startup(&mut ui);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-a"));
    assert_eq!(ui.mail_list_scroll.selected_idx, Some(0));
}

#[test]
fn tab_switch_virtual_folders_compose_contacts_and_open_only_hydration() {
    use crate::ui::action::{self, Action};
    use crate::ui::nav_pane::{TAB_CALENDAR, TAB_MAIL, TAB_TASKS};

    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    app.mail_folders = vec![MailFolder {
        id: "f-inbox".into(),
        display_name: "Inbox".into(),
        well_known_name: Some("inbox".into()),
        parent_folder_id: None,
        child_folder_count: 0,
        unread_item_count: 1,
        total_item_count: 2,
    }];
    app.storage.upsert_mail_folders(&app.mail_folders).unwrap();
    app.active_folder_id = "f-inbox".into();
    app.active_todo_list_id = Some("todo-work".into());

    // Switching to Calendar sends SyncCalendar and sets ActiveView::Calendar
    action::dispatch(Action::SelectNavTab(TAB_CALENDAR), &mut app, &mut ui);
    assert_eq!(focus_rx.borrow().active_view, ActiveView::Calendar);
    assert!(matches!(nudge_rx.try_recv(), Ok(SyncCommand::SyncCalendar)));

    // Switching to Tasks sends SyncTodo for selected list and sets ActiveView::Tasks
    action::dispatch(Action::SelectNavTab(TAB_TASKS), &mut app, &mut ui);
    assert_eq!(focus_rx.borrow().active_view, ActiveView::Tasks);
    assert!(matches!(
        nudge_rx.try_recv(),
        Ok(SyncCommand::SyncTodo { list_id: Some(ref id) }) if id == "todo-work"
    ));

    // Switching back to Mail on a virtual folder (Unread) uses existing Inbox scope,
    // never requesting SyncFolder with "__virtual_unread__".
    app.active_folder_id = VIRTUAL_UNREAD_FOLDER.into();
    action::dispatch(Action::SelectNavTab(TAB_MAIL), &mut app, &mut ui);
    assert_eq!(focus_rx.borrow().active_view, ActiveView::Mail);
    assert_eq!(focus_rx.borrow().active_folder_id, None);
    assert!(matches!(
        nudge_rx.try_recv(),
        Ok(SyncCommand::SyncFolder { ref folder_id }) if folder_id == "f-inbox"
    ));

    // Opening Mail Compose triggers SyncContacts once; typing in To: does not spam extra SyncContacts
    app.draft_store = DraftStore::default();
    action::dispatch(Action::OpenMailCompose, &mut app, &mut ui);
    assert!(matches!(nudge_rx.try_recv(), Ok(SyncCommand::SyncContacts)));
    action::dispatch(Action::Input('b'), &mut app, &mut ui);
    action::dispatch(Action::Input('o'), &mut app, &mut ui);
    assert!(nudge_rx.try_recv().is_err());
    action::dispatch(Action::CloseOverlay, &mut app, &mut ui);

    // When two messages have body_loaded=false after delta, ONLY the open mail requests HydrateMessage
    app.active_folder_id = "f-inbox".into();
    let mut m_open = make_mail("m-open", "c-open", "2026-11-20T10:00:00Z", false, false);
    m_open.folder_id = Some("f-inbox".into());
    m_open.body_loaded = false;
    let mut m_closed = make_mail("m-closed", "c-closed", "2026-11-20T09:00:00Z", false, false);
    m_closed.folder_id = Some("f-inbox".into());
    m_closed.body_loaded = false;
    app.storage
        .upsert_mail_messages(&[m_open, m_closed])
        .unwrap();
    app.active_mail_id = Some("m-open".into());
    while nudge_rx.try_recv().is_ok() {}
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailMessagesUpdated {
            folder_id: "f-inbox".into(),
            messages: vec![],
            notify_candidate: None,
        },
    );
    let mut hydrated_reqs = Vec::new();
    while let Ok(cmd) = nudge_rx.try_recv() {
        if let SyncCommand::HydrateMessage { message_id } = cmd {
            hydrated_reqs.push(message_id);
        }
    }
    assert_eq!(hydrated_reqs, vec!["m-open".to_string()]);
}

#[tokio::test]
async fn regression_outlook_outlook_14_removes_sent_draft_from_sqlite_and_ui() {
    use axum::{
        extract::Path as AxumPath,
        http::StatusCode,
        routing::post,
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    };

    let created_count = Arc::new(AtomicUsize::new(0));
    let send_should_fail = Arc::new(AtomicBool::new(true));
    let sent_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let created_c = Arc::clone(&created_count);
    let fail_c = Arc::clone(&send_should_fail);
    let sent_c = Arc::clone(&sent_ids);

    let router = Router::new()
        .route(
            "/v1.0/me/messages",
            post(move |Json(payload): Json<serde_json::Value>| {
                let count = created_c.fetch_add(1, Ordering::SeqCst);
                async move {
                    let id = if count == 0 { "draft-a" } else { "draft-b" };
                    let subject = payload
                        .get("subject")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let body = payload
                        .get("body")
                        .and_then(|b| b.get("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Json(serde_json::json!({
                        "id": id,
                        "parentFolderId": "f-drafts",
                        "subject": subject,
                        "receivedDateTime": if count == 0 { "2026-11-21T10:00:00Z" } else { "2026-11-21T09:00:00Z" },
                        "isRead": false,
                        "isDraft": true,
                        "bodyPreview": subject,
                        "body": { "contentType": "html", "content": body }
                    }))
                }
            }),
        )
        .route(
            "/v1.0/me/messages/{id}",
            axum::routing::get(|AxumPath(id): AxumPath<String>| async move {
                Json(serde_json::json!({
                    "id": id,
                    "parentFolderId": "f-drafts",
                    "subject": "Draft A AlphaDraftToken Ready",
                    "receivedDateTime": "2026-11-21T10:00:00Z",
                    "isRead": false,
                    "isDraft": true,
                    "bodyPreview": "AlphaDraftToken updated",
                    "body": { "contentType": "html", "content": "<p>AlphaDraftToken updated</p>" }
                }))
            })
            .patch(
                |AxumPath(id): AxumPath<String>, Json(payload): Json<serde_json::Value>| async move {
                    let subject = payload
                        .get("subject")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Draft A AlphaDraftToken")
                        .to_string();
                    Json(serde_json::json!({
                        "id": id,
                        "parentFolderId": "f-drafts",
                        "subject": subject,
                        "receivedDateTime": "2026-11-21T10:00:00Z",
                        "isRead": false,
                        "isDraft": true,
                        "bodyPreview": subject,
                        "body": { "contentType": "html", "content": "<p>AlphaDraftToken updated</p>" }
                    }))
                },
            ),
        )
        .route(
            "/v1.0/me/messages/{id}/send",
            post(move |AxumPath(id): AxumPath<String>| {
                let fail = fail_c.load(Ordering::SeqCst);
                let sent = Arc::clone(&sent_c);
                async move {
                    if fail {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": { "code": "ErrorInvalidRecipients", "message": "send failed" }
                            })),
                        )
                    } else {
                        sent.lock().unwrap().push(id);
                        (StatusCode::ACCEPTED, Json(serde_json::json!({})))
                    }
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "synthetic-tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("u1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    let profile = tempfile::tempdir().unwrap();
    app.draft_store_path = profile.path().join("drafts.json");
    app.draft_store = DraftStore::default();

    app.mail_folders = vec![
        MailFolder {
            id: "f-drafts".into(),
            display_name: "Drafts".into(),
            well_known_name: Some("drafts".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 2,
            total_item_count: 2,
        },
        MailFolder {
            id: "f-sent".into(),
            display_name: "Sent Items".into(),
            well_known_name: Some("sentitems".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 1,
        },
    ];
    app.storage.upsert_mail_folders(&app.mail_folders).unwrap();

    let mut sent_existing = make_mail(
        "sent-existing-1",
        "conv-sent-1",
        "2026-11-20T08:00:00Z",
        true,
        false,
    );
    sent_existing.folder_id = Some("f-sent".into());
    sent_existing.subject = "Existing Sent Mail".into();
    app.storage.upsert_mail_messages(&[sent_existing]).unwrap();

    // Save Draft A (Ctrl+S)
    let mut compose_a = MailComposeOverlay::new_to("alice@example.com");
    compose_a.subject = "Draft A AlphaDraftToken".into();
    compose_a.body.load_text("AlphaDraftToken body");
    ui.overlays.push(Overlay::MailCompose(compose_a.clone()));
    app.save_mail_draft(&mut ui, &compose_a);
    while app.has_pending_draft_write() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("save draft A event")
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
    }
    let compose_a = match ui.overlays.pop().unwrap() {
        Overlay::MailCompose(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(compose_a.draft_id.as_deref(), Some("draft-a"));

    // Save Draft B (Ctrl+S) and close it without sending
    let mut compose_b = MailComposeOverlay::new_to("bob@example.com");
    compose_b.subject = "Draft B KeepMe".into();
    compose_b.body.load_text("KeepMe body");
    ui.overlays.push(Overlay::MailCompose(compose_b.clone()));
    app.save_mail_draft(&mut ui, &compose_b);
    while app.has_pending_draft_write() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("save draft B event")
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
    }
    let compose_b = match ui.overlays.pop().unwrap() {
        Overlay::MailCompose(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(compose_b.draft_id.as_deref(), Some("draft-b"));
    app.stash_compose_draft(&compose_b).unwrap();

    // Open Drafts folder and populate search results containing draft-a
    app.select_mail_folder(&mut ui, "f-drafts".into());
    assert_eq!(app.mail_messages.len(), 2);
    app.search_results = app.storage.search_mail("AlphaDraftToken", None, 10).unwrap();
    assert_eq!(app.search_results.len(), 1);
    assert_eq!(app.search_results[0].id, "draft-a");

    // Control 1: Send failure preserves draft-a in SQLite, Drafts list, and draft_store (re-editable)
    ui.overlays.push(Overlay::MailCompose(compose_a.clone()));
    assert!(app.submit_mail_compose(&mut ui, &compose_a));
    while app.has_pending_draft_write() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("failed send draft A event")
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
    }
    assert!(
        app.storage.get_mail_message("draft-a").unwrap().is_some(),
        "failed send must NOT delete draft-a from SQLite"
    );
    assert_eq!(app.mail_messages.len(), 2);
    let key_a = app
        .stored_compose_key(&compose_a.mode.draft_key(), &compose_a.compose_id)
        .expect("draft-a must stay in draft_store for re-editing");
    assert!(
        !app.draft_store.get_mail(&key_a).unwrap().synced.send_pending,
        "failed send must clear send_pending so draft-a can be edited and resent"
    );

    // Acceptance: Re-edit draft-a and send successfully -> immediately removed before any folder resync
    send_should_fail.store(false, Ordering::SeqCst);
    let mut compose_a_retry = compose_a.clone();
    compose_a_retry.subject = "Draft A AlphaDraftToken Ready".into();
    assert!(app.submit_mail_compose(&mut ui, &compose_a_retry));
    while app.has_pending_draft_write() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("send draft A success event")
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
    }
    while let Ok(msg) = ui_rx.try_recv() {
        app.handle_ui_msg(&mut ui, msg);
    }

    assert_eq!(*sent_ids.lock().unwrap(), vec!["draft-a".to_string()]);
    assert!(
        app.storage.get_mail_message("draft-a").unwrap().is_none(),
        "sent draft-a must be removed from SQLite immediately after POST /send succeeds"
    );
    let db_drafts = app
        .storage
        .list_mail_messages(Some("f-drafts"), false, false, false, 20)
        .unwrap();
    let db_draft_ids: Vec<&str> = db_drafts.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(db_draft_ids, vec!["draft-b"]);

    let ui_draft_ids: Vec<&str> = app.mail_messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ui_draft_ids, vec!["draft-b"]);
    assert_eq!(app.active_mail_id.as_deref(), Some("draft-b"));
    assert_eq!(
        app.active_mail().map(|m| m.id.as_str()),
        Some("draft-b")
    );
    assert!(
        app.storage
            .search_mail("AlphaDraftToken", None, 10)
            .unwrap()
            .is_empty(),
        "sent draft-a must be removed from mail_fts"
    );
    assert!(
        !app.search_results.iter().any(|h| h.id == "draft-a"),
        "sent draft-a must be removed from TUI search_results"
    );
    let drafts_folder = app
        .mail_folders
        .iter()
        .find(|f| f.id == "f-drafts")
        .unwrap();
    assert_eq!(drafts_folder.total_item_count, 1);
    assert_eq!(drafts_folder.unread_item_count, 1);

    // Control 2: Draft B (saved only) and existing Sent Items row remain untouched
    assert!(app.storage.get_mail_message("draft-b").unwrap().is_some());
    assert!(app
        .storage
        .get_mail_message("sent-existing-1")
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn regression_outlook_outlook_07_smart_folder_delete_does_not_reinsert() {
    use axum::{extract::Path as AxumPath, http::StatusCode, routing::post, Json, Router};
    use std::sync::atomic::{AtomicBool, Ordering};

    let move_should_fail = Arc::new(AtomicBool::new(false));
    let fail_c = Arc::clone(&move_should_fail);

    let router = Router::new().route(
        "/v1.0/me/messages/{id}/move",
        post(move |AxumPath(id): AxumPath<String>, Json(_payload): Json<serde_json::Value>| {
            let fail = fail_c.load(Ordering::SeqCst);
            async move {
                if fail {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": { "code": "ErrorInternalServerError", "message": "move failed" }
                        })),
                    );
                }
                let flagged = id.contains("flagged");
                let is_read = id.contains("flagged");
                let new_id = format!("{id}-moved");
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "id": new_id,
                        "conversationId": format!("conv-{id}"),
                        "parentFolderId": "f-deleted",
                        "subject": format!("Subject {id}"),
                        "receivedDateTime": "2026-11-21T10:00:00Z",
                        "isRead": is_read,
                        "isDraft": false,
                        "flag": { "flagStatus": if flagged { "flagged" } else { "notFlagged" } },
                        "bodyPreview": format!("Body {id}"),
                        "body": { "contentType": "html", "content": format!("<p>Body {id}</p>") }
                    })),
                )
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tok = crate::auth::StoredTokens {
        access_token: "synthetic-tok".into(),
        refresh_token: None,
        expires_at: 9_999_999_999,
        client_id: "c".into(),
        tenant_id: "t".into(),
        user_id: Some("u1".into()),
        user_display_name: None,
        user_email: None,
        auth_mode: "access_token".into(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tok);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    ui.group_threads = false;

    app.mail_folders = vec![
        MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 2,
            total_item_count: 4,
        },
        MailFolder {
            id: "f-deleted".into(),
            display_name: "Deleted Items".into(),
            well_known_name: Some("deleteditems".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 0,
        },
    ];
    app.storage.upsert_mail_folders(&app.mail_folders).unwrap();

    let mut u1 = make_mail("m-unread-del", "c-u1", "2026-11-21T10:00:00Z", false, false);
    u1.folder_id = Some("f-inbox".into());
    let mut u2 = make_mail("m-unread-keep", "c-u2", "2026-11-21T09:00:00Z", false, false);
    u2.folder_id = Some("f-inbox".into());
    let mut f1 = make_mail("m-flagged-del", "c-f1", "2026-11-21T08:00:00Z", true, true);
    f1.folder_id = Some("f-inbox".into());
    let mut f2 = make_mail("m-flagged-keep", "c-f2", "2026-11-21T07:00:00Z", true, true);
    f2.folder_id = Some("f-inbox".into());
    app.storage
        .upsert_mail_messages(&[u1, u2, f1, f2])
        .unwrap();

    // 1. Control: delete request failure preserves original selection and badges
    app.select_mail_folder(&mut ui, VIRTUAL_UNREAD_FOLDER.into());
    assert_eq!(app.mail_messages.len(), 2);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-unread-del"));
    move_should_fail.store(true, Ordering::SeqCst);
    app.delete_selected_mail(&mut ui);
    let fail_msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("delete fail event")
        .unwrap();
    app.handle_ui_msg(&mut ui, fail_msg);
    while let Ok(extra) = ui_rx.try_recv() {
        app.handle_ui_msg(&mut ui, extra);
    }
    assert_eq!(
        app.active_mail_id.as_deref(),
        Some("m-unread-del"),
        "failed delete must preserve original selection"
    );
    assert_eq!(
        app.active_mail().map(|m| m.id.as_str()),
        Some("m-unread-del"),
        "failed delete must preserve reader mail"
    );
    assert_eq!(
        app.mail_messages.iter().filter(|m| !m.is_read).count(),
        2,
        "failed delete must preserve Unread badge count"
    );
    assert_eq!(
        app.mail_folders
            .iter()
            .find(|f| f.id == "f-inbox")
            .unwrap()
            .unread_item_count,
        2,
        "failed delete must preserve Inbox unread badge"
    );

    // 2. Acceptance (Unread): delete selected unread mail -> after MailMoved and after re-entering Unread, only remaining 1 shown and selection == reader
    move_should_fail.store(false, Ordering::SeqCst);
    app.delete_selected_mail(&mut ui);
    let moved_msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("delete move event")
        .unwrap();
    app.handle_ui_msg(&mut ui, moved_msg);

    let unread_ids_after_move: Vec<&str> =
        app.mail_messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        unread_ids_after_move,
        vec!["m-unread-keep"],
        "deleted mail must NOT be re-inserted into Unread smart folder after MailMoved"
    );
    assert_eq!(app.active_mail_id.as_deref(), Some("m-unread-keep"));
    assert_eq!(
        app.active_mail().map(|m| m.id.as_str()),
        Some("m-unread-keep")
    );
    assert_eq!(ui.mail_list_scroll.selected_idx, Some(0));

    // Re-enter Unread smart folder: deleted item still excluded
    app.select_mail_folder(&mut ui, VIRTUAL_UNREAD_FOLDER.into());
    let unread_ids_reenter: Vec<&str> =
        app.mail_messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        unread_ids_reenter,
        vec!["m-unread-keep"],
        "re-entering Unread smart folder must exclude Deleted Items"
    );
    assert_eq!(app.active_mail_id.as_deref(), Some("m-unread-keep"));
    assert_eq!(
        app.active_mail().map(|m| m.id.as_str()),
        Some("m-unread-keep")
    );

    // 3. Acceptance (Flagged): delete selected flagged mail -> after MailMoved and re-entry, only remaining 1 shown
    app.select_mail_folder(&mut ui, VIRTUAL_FLAGGED_FOLDER.into());
    assert_eq!(app.mail_messages.len(), 2);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-flagged-del"));
    app.delete_selected_mail(&mut ui);
    let moved_flag_msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
        .await
        .expect("flagged delete move event")
        .unwrap();
    app.handle_ui_msg(&mut ui, moved_flag_msg);

    let flagged_ids_after: Vec<&str> =
        app.mail_messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(flagged_ids_after, vec!["m-flagged-keep"]);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-flagged-keep"));
    assert_eq!(
        app.active_mail().map(|m| m.id.as_str()),
        Some("m-flagged-keep")
    );

    app.select_mail_folder(&mut ui, VIRTUAL_FLAGGED_FOLDER.into());
    let flagged_ids_reenter: Vec<&str> =
        app.mail_messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(flagged_ids_reenter, vec!["m-flagged-keep"]);
    assert_eq!(app.active_mail_id.as_deref(), Some("m-flagged-keep"));
    assert_eq!(
        app.active_mail().map(|m| m.id.as_str()),
        Some("m-flagged-keep")
    );

    // 4. Control: opening Trash directly DOES show the moved messages
    app.select_mail_folder(&mut ui, "f-deleted".into());
    let trash_ids: Vec<&str> = app.mail_messages.iter().map(|m| m.id.as_str()).collect();
    assert!(
        trash_ids.contains(&"m-unread-del-moved") && trash_ids.contains(&"m-flagged-del-moved"),
        "Trash folder must show moved messages, got {trash_ids:?}"
    );
}

#[test]
fn bootstrap_done_preserves_virtual_mail_folders_and_non_default_todo_list_and_reclamps() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    // User already switched to VIRTUAL_FLAGGED_FOLDER and a non-default To Do list ("list-work")
    let mut flagged_in_archive =
        make_mail("m-flag-arch", "c-fa", "2026-11-21T10:00:00Z", true, true);
    flagged_in_archive.folder_id = Some("f-archive".into());
    let mut inbox_unflagged =
        make_mail("m-inbox-only", "c-in", "2026-11-22T10:00:00Z", true, false);
    inbox_unflagged.folder_id = Some("f-inbox".into());
    app.storage
        .upsert_mail_messages(&[flagged_in_archive.clone(), inbox_unflagged.clone()])
        .unwrap();
    app.select_mail_folder(&mut ui, VIRTUAL_FLAGGED_FOLDER.into());
    assert_eq!(listed_ids(&app), ["m-flag-arch"]);

    let work_task = TodoTask {
        id: "work-1".into(),
        list_id: "list-work".into(),
        title: "Work task".into(),
        status: "notStarted".into(),
        importance: "high".into(),
        body_plain: None,
        due_date_iso: None,
        reminder_iso: None,
        completed_at: None,
        created_at: None,
        updated_at: None,
    };
    let default_task = TodoTask {
        id: "def-1".into(),
        list_id: "list-default".into(),
        title: "Default task".into(),
        status: "notStarted".into(),
        importance: "normal".into(),
        body_plain: None,
        due_date_iso: None,
        reminder_iso: None,
        completed_at: None,
        created_at: None,
        updated_at: None,
    };
    app.active_todo_list_id = Some("list-work".into());
    app.todo_tasks = vec![work_task.clone()];
    app.active_todo_task_id = Some("stale-task-id".into());
    app.active_event_id = Some("stale-event-id".into());

    let ev = CalendarEvent {
        id: "ev-real".into(),
        subject: "Real Meeting".into(),
        start_iso: "2026-11-25T10:00:00Z".into(),
        end_iso: "2026-11-25T11:00:00Z".into(),
        ..Default::default()
    };
    app.handle_ui_msg(
        &mut ui,
        UiMsg::BootstrapDone {
            me: None,
            mail_folders: vec![MailFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 1,
            }],
            mail_messages: vec![inbox_unflagged],
            calendar_events: vec![ev],
            contacts: vec![],
            todo_lists: vec![
                TodoList {
                    id: "list-default".into(),
                    display_name: "Tasks".into(),
                    wellknown_list_name: Some("defaultList".into()),
                    is_owner: true,
                    is_shared: false,
                },
                TodoList {
                    id: "list-work".into(),
                    display_name: "Work".into(),
                    wellknown_list_name: None,
                    is_owner: true,
                    is_shared: false,
                },
            ],
            todo_tasks: vec![default_task],
            scope_warning: None,
        },
    );

    // Virtual Flagged folder must NOT be overwritten by Inbox-only summaries
    assert_eq!(listed_ids(&app), ["m-flag-arch"]);
    // Non-default To Do list must NOT be clobbered by defaultList tasks
    assert_eq!(app.todo_tasks.len(), 1);
    assert_eq!(app.todo_tasks[0].id, "work-1");
    // Stale event & task selections must be re-clamped
    assert_eq!(app.active_event_id.as_deref(), Some("ev-real"));
    assert_eq!(ui.cal_list_scroll.selected_idx, Some(0));
    assert_eq!(app.active_todo_task_id.as_deref(), Some("work-1"));
    assert_eq!(ui.todo_list_scroll.selected_idx, Some(0));
}

#[test]
fn mail_folder_unread_counts_reconcile_and_delete_failed_no_hijack() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    ui.group_threads = false;

    let folders = vec![
        MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 2,
            total_item_count: 2,
        },
        MailFolder {
            id: "f-archive".into(),
            display_name: "Archive".into(),
            well_known_name: Some("archive".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 1,
        },
        MailFolder {
            id: "f-sent".into(),
            display_name: "Sent Items".into(),
            well_known_name: Some("sentitems".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 1,
        },
    ];
    app.storage.upsert_mail_folders(&folders).unwrap();
    app.mail_folders = folders;

    let mut m1 = make_mail("m1", "c1", "2026-11-25T10:00:00Z", false, true);
    m1.folder_id = Some("f-inbox".into());
    let mut m2 = make_mail("m2", "c2", "2026-11-25T09:00:00Z", false, false);
    m2.folder_id = Some("f-inbox".into());
    let mut arch = make_mail("m-arch", "c-a", "2026-11-24T09:00:00Z", false, true);
    arch.folder_id = Some("f-archive".into());
    let mut sent = make_mail("m-sent", "c-s", "2026-11-24T08:00:00Z", true, false);
    sent.folder_id = Some("f-sent".into());
    app.storage
        .upsert_mail_messages(&[m1.clone(), m2, arch, sent])
        .unwrap();

    // Smart badges report mailbox-wide counts even while viewing Sent
    app.select_mail_folder(&mut ui, "f-sent".into());
    assert_eq!(app.smart_unread_count(), 3);
    assert_eq!(app.smart_flagged_count(), 2);

    // MailDeleteFailed for an Inbox message must NOT hijack selection while user is in Sent
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MailDeleteFailed {
            original_id: "m1".into(),
            original_message: Some(m1.clone()),
            error: "network".into(),
        },
    );
    assert_eq!(app.active_mail_id.as_deref(), Some("m-sent"));

    // Under mail_unread_only, toggle_mail_read updates Inbox unread count AND reselects next unread email
    app.select_mail_folder(&mut ui, "f-inbox".into());
    ui.mail_unread_only = true;
    assert_eq!(app.active_mail_id.as_deref(), Some("m1"));
    app.toggle_mail_read(&mut ui);
    assert_eq!(app.unread_inbox_count(), 1);
    assert_eq!(
        app.active_mail_id.as_deref(),
        Some("m2"),
        "marking read under mail_unread_only must advance active_mail_id to next visible unread"
    );

    // MailFlagReverted reloads mail_folders unread count after local SQLite revert
    app.storage.set_mail_read_local("m1", false).unwrap();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MailFlagReverted {
            id: "m1".into(),
            is_read: Some(false),
            flagged: None,
            label: "Mark read".into(),
            error: "500".into(),
        },
    );
    assert_eq!(app.unread_inbox_count(), 2);

    // Archive immediately updates Inbox and Archive folder unread counts
    app.active_mail_id = Some("m1".into());
    app.archive_selected_mail(&mut ui);
    assert_eq!(app.unread_inbox_count(), 1);
    let archive_unread = app
        .mail_folders
        .iter()
        .find(|f| f.id == "f-archive")
        .unwrap()
        .unread_item_count;
    assert_eq!(archive_unread, 2);
}

#[test]
fn search_generation_and_mail_history_seq_guards_drop_stale_messages() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    // Simulate search generation = 2 for query "budget"
    app.search_query = "budget".into();
    app.search_generation = 2;
    app.searching = true;

    // Stale generation 1 (even with identical query string!) must be dropped
    app.handle_ui_msg(
        &mut ui,
        UiMsg::SearchResults {
            query: "budget".into(),
            generation: 1,
            hits: vec![MailSearchHit {
                id: "stale-hit".into(),
                conversation_id: None,
                folder_id: Some("inbox".into()),
                subject: "Stale".into(),
                from_display: "A".into(),
                received_at: "2026-11-20T00:00:00Z".into(),
                snippet: "stale".into(),
                has_attachments: false,
                is_read: true,
                flagged: false,
            }],
        },
    );
    assert!(app.search_results.is_empty());
    assert!(app.searching);

    app.handle_ui_msg(
        &mut ui,
        UiMsg::RemoteSearchFailed {
            query: "budget".into(),
            generation: 1,
            error: "stale failure".into(),
        },
    );
    assert!(app.search_error.is_none());
    assert!(app.searching);

    // Current generation 2 applies cleanly
    app.handle_ui_msg(
        &mut ui,
        UiMsg::SearchResults {
            query: "budget".into(),
            generation: 2,
            hits: vec![MailSearchHit {
                id: "fresh-hit".into(),
                conversation_id: None,
                folder_id: Some("inbox".into()),
                subject: "Fresh".into(),
                from_display: "B".into(),
                received_at: "2026-11-25T00:00:00Z".into(),
                snippet: "fresh".into(),
                has_attachments: false,
                is_read: false,
                flagged: false,
            }],
        },
    );
    assert_eq!(app.search_results.len(), 1);
    assert_eq!(app.search_results[0].id, "fresh-hit");

    // MailHistoryPageFinished sequence guard drops stale seq
    app.active_folder_id = "f-inbox".into();
    app.maybe_autoload_older_mail();
    let seq = app.mail_history_seq;
    assert!(app.loading_older_mail);
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailHistoryPageFinished {
            folder_id: "f-inbox".into(),
            has_more: Some(false),
            seq: seq.wrapping_sub(1),
        },
    );
    assert!(
        app.loading_older_mail,
        "stale MailHistoryPageFinished must be ignored"
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::MailHistoryPageFinished {
            folder_id: "f-inbox".into(),
            has_more: Some(false),
            seq,
        },
    );
    assert!(!app.loading_older_mail);
}

#[test]
fn calendar_and_todo_optimistic_rollback_and_delete_selection_clamp() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    // CalendarEventCreated sets cal_list_scroll.selected_idx
    let ev1 = CalendarEvent {
        id: "ev1".into(),
        subject: "First".into(),
        start_iso: "2026-11-25T09:00:00Z".into(),
        end_iso: "2026-11-25T10:00:00Z".into(),
        response_status: "notResponded".into(),
        ..Default::default()
    };
    let ev2 = CalendarEvent {
        id: "ev2".into(),
        subject: "Second".into(),
        start_iso: "2026-11-25T11:00:00Z".into(),
        end_iso: "2026-11-25T12:00:00Z".into(),
        response_status: "notResponded".into(),
        ..Default::default()
    };
    let ev3 = CalendarEvent {
        id: "ev3".into(),
        subject: "Third".into(),
        start_iso: "2026-11-25T13:00:00Z".into(),
        end_iso: "2026-11-25T14:00:00Z".into(),
        response_status: "notResponded".into(),
        ..Default::default()
    };
    app.calendar_events = vec![ev1.clone(), ev2.clone()];
    app.handle_ui_msg(&mut ui, UiMsg::CalendarEventCreated(ev3.clone()));
    assert_eq!(app.active_event_id.as_deref(), Some("ev3"));
    assert_eq!(ui.cal_list_scroll.selected_idx, Some(2));

    // Deleting index 1 clamps selection to index 1 (ev3) instead of jumping to 0 and resets detail scroll
    app.active_event_id = Some("ev2".into());
    ui.cal_list_scroll.selected_idx = Some(1);
    ui.cal_detail_scroll.offset = 7;
    app.delete_selected_calendar_event(&mut ui);
    assert_eq!(app.active_event_id.as_deref(), Some("ev3"));
    assert_eq!(ui.cal_list_scroll.selected_idx, Some(1));
    assert_eq!(ui.cal_detail_scroll.offset, 0);

    // CalendarEventDeleteFailed restores deleted event
    app.handle_ui_msg(
        &mut ui,
        UiMsg::CalendarEventDeleteFailed {
            original_event: Some(ev2.clone()),
            error: "500".into(),
        },
    );
    assert!(app.calendar_events.iter().any(|e| e.id == "ev2"));

    // CalendarRsvpFailed reverts optimistic RSVP
    app.respond_calendar_event(&mut ui, "ev1".into(), EventRsvpAction::Accept);
    assert_eq!(app.calendar_events[0].response_status, "accepted");
    app.handle_ui_msg(
        &mut ui,
        UiMsg::CalendarRsvpFailed {
            event_id: "ev1".into(),
            prev_status: Some("notResponded".into()),
            prev_mail_statuses: vec![],
            error: "500".into(),
        },
    );
    assert_eq!(app.calendar_events[0].response_status, "notResponded");

    // TodoTask delete clamps near prev_sel, resets detail scroll, and TodoTaskToggleFailed/DeleteFailed roll back
    let mk_task = |id: &str| TodoTask {
        id: id.into(),
        list_id: "l1".into(),
        title: id.into(),
        status: "notStarted".into(),
        importance: "normal".into(),
        body_plain: None,
        due_date_iso: None,
        reminder_iso: None,
        completed_at: None,
        created_at: None,
        updated_at: None,
    };
    let (t1, t2, t3) = (mk_task("t1"), mk_task("t2"), mk_task("t3"));
    app.active_todo_list_id = Some("l1".into());
    app.todo_tasks = vec![t1.clone(), t2.clone(), t3.clone()];
    app.active_todo_task_id = Some("t2".into());
    ui.todo_list_scroll.selected_idx = Some(1);
    ui.todo_detail_scroll.offset = 5;
    app.delete_selected_todo_task(&mut ui);
    assert_eq!(app.active_todo_task_id.as_deref(), Some("t3"));
    assert_eq!(ui.todo_list_scroll.selected_idx, Some(1));
    assert_eq!(ui.todo_detail_scroll.offset, 0);

    app.handle_ui_msg(
        &mut ui,
        UiMsg::TodoTaskDeleteFailed {
            original_task: Some(t2),
            error: "500".into(),
        },
    );
    assert!(app.todo_tasks.iter().any(|t| t.id == "t2"));

    app.active_todo_task_id = Some("t1".into());
    app.toggle_todo_completed(&mut ui);
    assert!(app.todo_tasks.iter().find(|t| t.id == "t1").unwrap().is_completed());
    app.handle_ui_msg(
        &mut ui,
        UiMsg::TodoTaskToggleFailed {
            original_task: t1,
            error: "500".into(),
        },
    );
    assert!(!app.todo_tasks.iter().find(|t| t.id == "t1").unwrap().is_completed());
}

#[tokio::test]
async fn data_image_png_and_svg_decode_locally_without_graph_fetch() {
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
    let (cmd_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = crate::sync::SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);

    // Build a valid 8x4 PNG data URI
    let img: RgbaImage = ImageBuffer::from_pixel(8, 4, Rgba([220, 40, 60, 255]));
    let mut png_buf = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut png_buf, ImageFormat::Png)
        .unwrap();
    let png_uri = format!(
        "data:image/png;base64,{}",
        B64_STANDARD.encode(png_buf.into_inner())
    );

    let png_att = MailAttachment {
        id: png_uri,
        name: "inline-image-1.png".to_string(),
        content_type: "image/png".to_string(),
        size: 0,
        is_inline: true,
        content_id: None,
        content_bytes_b64: None,
    };
    app.request_mail_image("m-data".into(), png_att, 20, 10, (8, 16), (20, 22, 28));

    let mut png_decoded = None;
    for _ in 0..4 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for data:image/png decode")
            .expect("ui channel closed");
        match msg {
            UiMsg::ImageDecoded { rendered, .. } => {
                png_decoded = Some(rendered);
                break;
            }
            UiMsg::ImageBytesFetched { .. } => continue,
            UiMsg::ImageDecodeFailed { error, .. } => {
                panic!("data:image/png decode failed: {error}");
            }
            _ => panic!("unexpected UiMsg for data:image/png"),
        }
    }
    let png_img = png_decoded.expect("expected ImageDecoded for data:image/png");
    assert_eq!(png_img.format_label, "PNG");
    assert_eq!(png_img.width_px, 8);
    assert_eq!(png_img.height_px, 4);
    assert!(!png_img.lines.is_empty());

    // Also verify data:image/svg+xml decodes and rasterizes locally without network/Graph calls
    let svg_raw = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="#2563eb"/></svg>"##;
    let svg_uri = format!(
        "data:image/svg+xml;base64,{}",
        B64_STANDARD.encode(svg_raw.as_bytes())
    );
    let svg_att = MailAttachment {
        id: svg_uri,
        name: "inline-vector.svg".to_string(),
        content_type: "image/svg+xml".to_string(),
        size: 0,
        is_inline: true,
        content_id: None,
        content_bytes_b64: None,
    };
    app.request_mail_image("m-data".into(), svg_att, 20, 10, (8, 16), (20, 22, 28));

    let mut svg_decoded = None;
    for _ in 0..4 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for data:image/svg+xml decode")
            .expect("ui channel closed");
        match msg {
            UiMsg::ImageDecoded { rendered, .. } => {
                svg_decoded = Some(rendered);
                break;
            }
            UiMsg::ImageBytesFetched { .. } => continue,
            UiMsg::ImageDecodeFailed { error, .. } => {
                panic!("data:image/svg+xml decode failed: {error}");
            }
            _ => panic!("unexpected UiMsg for data:image/svg+xml"),
        }
    }
    let svg_img = svg_decoded.expect("expected ImageDecoded for data:image/svg+xml");
    assert_eq!(svg_img.format_label, "SVG");
    assert!(!svg_img.lines.is_empty());
}

#[test]
fn mail_draft_saved_updates_drafts_view_and_detail_cache_immediately() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, _) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    app.mail_folders = vec![MailFolder {
        id: "f-drafts".into(),
        display_name: "Drafts".into(),
        well_known_name: Some("drafts".into()),
        parent_folder_id: None,
        child_folder_count: 0,
        unread_item_count: 0,
        total_item_count: 0,
    }];
    app.active_folder_id = "f-drafts".into();

    let saved = MailMessage {
        id: "d-new-1".into(),
        folder_id: Some("f-drafts".into()),
        subject: "Draft Subject".into(),
        received_at: "2026-11-25T12:00:00Z".into(),
        is_draft: true,
        body_loaded: true,
        body_plain: "Local draft body".into(),
        body_html: "<p>Local draft body</p>".into(),
        ..Default::default()
    };
    app.handle_ui_msg(
        &mut ui,
        UiMsg::MailDraftSaved {
            draft_key: "new".into(),
            compose_id: "cmp-1".into(),
            result: Ok(Some(saved)),
            send: false,
            sent_draft_id: None,
        },
    );

    assert_eq!(app.mail_messages.len(), 1);
    assert_eq!(app.mail_messages[0].id, "d-new-1");
    assert_eq!(app.active_mail_id.as_deref(), Some("d-new-1"));
    let cached = app
        .mail_detail_cache
        .get("d-new-1")
        .expect("saved draft must populate mail_detail_cache immediately");
    assert!(cached.body_loaded);
    assert_eq!(cached.body_plain, "Local draft body");
}

#[test]
fn select_mail_row_queues_bounded_adjacent_prefetch_for_uncached_rows() {
    let cfg = AppConfig::default();
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _) = mpsc::unbounded_channel();
    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    ui.group_threads = false;

    let mk = |id: &str, ts: &str, loaded: bool| MailMessage {
        id: id.into(),
        folder_id: Some("f-inbox".into()),
        subject: id.into(),
        received_at: ts.into(),
        body_loaded: loaded,
        ..Default::default()
    };
    app.mail_messages = vec![
        mk("m0", "2026-11-25T10:04:00Z", true),
        mk("m1", "2026-11-25T10:03:00Z", true),
        mk("m2", "2026-11-25T10:02:00Z", false),
        mk("m3", "2026-11-25T10:01:00Z", false),
        mk("m4", "2026-11-25T10:00:00Z", false),
    ];

    app.select_mail_row(&mut ui, 1);

    let mut prefetched = Vec::new();
    while let Ok(cmd) = nudge_rx.try_recv() {
        if let SyncCommand::PrefetchMessages { message_ids } = cmd {
            prefetched.extend(message_ids);
        }
    }
    assert_eq!(
        prefetched,
        vec!["m2".to_string(), "m3".to_string()],
        "must prefetch next 2 uncached rows while skipping loaded m0, active m1, and out-of-window m4"
    );
}
