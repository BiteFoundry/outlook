//! Captures real `ratatui::TestBackend` frames (120x34) from `outlook_cli::ui::draw`
//! using 100% fictional Acme Engineering demo data, then invokes the shared GIF renderer.
//!
//! Run with:
//!   cargo run --example demo_capture

use std::path::PathBuf;
use std::process::Command;

use chrono::{Duration, Local};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use outlook_cli::{
    auth::AuthManager,
    config::{AppConfig, DensityMode, ThemePreset},
    graph::GraphClient,
    models::{
        CalendarEvent, ContactItem, EmailRecipient, EventAttendee, MailAttachment, MailFolder,
        MailMessage, TodoList, TodoTask, UserProfile,
    },
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::{
        self,
        app::{AppState, CalendarRangeFilter, Focus, UiState},
        nav_pane::{TAB_CALENDAR, TAB_MAIL, TAB_TASKS},
        overlays::{
            command_palette::CommandPalette,
            links::LinkExtractorOverlay,
            mail_compose::MailComposeOverlay,
            settings::SettingsState,
            Overlay,
        },
        theme::Theme,
    },
};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;

const RENDERER_SCRIPT: &str = r"C:\Users\user\.gemini\antigravity-cli\brain\f9e7f082-6ac8-4e73-9827-8af29ccd4ff4\scratch\render_tui_gif.py";
const PREVIEW_DIR: &str = r"C:\Users\user\.gemini\antigravity-cli\brain\f9e7f082-6ac8-4e73-9827-8af29ccd4ff4\scratch\gif-previews\outlook";

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = indexed_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn indexed_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (24, 26, 33),
        1 => (247, 118, 142),
        2 => (158, 206, 106),
        3 => (224, 175, 104),
        4 => (122, 162, 247),
        5 => (187, 154, 247),
        6 => (125, 207, 255),
        7 => (169, 177, 214),
        8 => (86, 95, 137),
        9 => (255, 140, 160),
        10 => (175, 224, 128),
        11 => (240, 200, 130),
        12 => (148, 184, 255),
        13 => (205, 176, 255),
        14 => (155, 222, 255),
        15 => (240, 244, 252),
        16..=231 => {
            let n = idx - 16;
            let b = n % 6;
            let g = (n / 6) % 6;
            let r = n / 36;
            let map = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (map(r), map(g), map(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

fn modifier_to_flags(m: Modifier) -> u8 {
    let mut f = 0u8;
    if m.contains(Modifier::BOLD) {
        f |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        f |= 2;
    }
    if m.contains(Modifier::DIM) {
        f |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        f |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        f |= 16;
    }
    f
}

fn normalize_cell_char(symbol: &str) -> char {
    let ch = symbol
        .chars()
        .find(|c| !matches!(*c, '\u{FE0E}' | '\u{FE0F}' | '\u{200D}'))
        .unwrap_or(' ');
    match ch {
        '▎' | '▏' => '▌',
        '↳' | '↗' => '›',
        '⊘' | '⛔' | '◐' => '●',
        '◆' | '◈' | '⚑' | '🎥' => '♦',
        '✕' => '×',
        '🔒' | '📌' | '✎' | '☰' => '▪',
        '▣' | '☑' => '■',
        '👍' | '📎' => '+',
        '❤' => '♥',
        '★' | '🔥' => '*',
        '❗' | '⚠' => '!',
        '🚀' | '📥' => '▸',
        '🎉' => '♦',
        '⌫' => '<',
        _ => ch,
    }
}

fn capture_buffer(buf: &Buffer, duration_ms: u32, caption: &str, key_badge: &str) -> Value {
    let area = buf.area;
    let mut rows = Vec::with_capacity(area.height as usize);
    for y in area.y..area.bottom() {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_fg: Option<Color> = None;
        let mut cur_bg: Option<Color> = None;
        let mut cur_flags: u8 = 0;

        for x in area.x..area.right() {
            let cell = &buf[(x, y)];
            let ch = if cell.skip {
                ' '
            } else {
                normalize_cell_char(cell.symbol())
            };
            let fg = cell.fg;
            let bg = cell.bg;
            let mut flags = modifier_to_flags(cell.modifier);
            if ch == '✓' {
                flags &= !1;
            }

            if cur_fg == Some(fg) && cur_bg == Some(bg) && cur_flags == flags {
                cur_text.push(ch);
            } else {
                if !cur_text.is_empty() {
                    runs.push(json!([
                        cur_text,
                        color_to_json(cur_fg.unwrap_or(Color::Reset)),
                        color_to_json(cur_bg.unwrap_or(Color::Reset)),
                        cur_flags
                    ]));
                    cur_text.clear();
                }
                cur_text.push(ch);
                cur_fg = Some(fg);
                cur_bg = Some(bg);
                cur_flags = flags;
            }
        }
        if !cur_text.is_empty() {
            runs.push(json!([
                cur_text,
                color_to_json(cur_fg.unwrap_or(Color::Reset)),
                color_to_json(cur_bg.unwrap_or(Color::Reset)),
                cur_flags
            ]));
        }
        rows.push(Value::Array(runs));
    }

    json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    })
}

fn render_and_capture(
    app: &mut AppState,
    ui: &mut UiState,
    duration_ms: u32,
    caption: &str,
    key_badge: &str,
) -> Value {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal init");
    terminal
        .draw(|f| {
            let _ = ui::draw(f, app, ui);
        })
        .expect("ratatui draw failed");
    capture_buffer(terminal.backend().buffer(), duration_ms, caption, key_badge)
}

fn recipient(name: &str, address: &str) -> EmailRecipient {
    EmailRecipient {
        name: Some(name.to_string()),
        address: address.to_string(),
    }
}

fn main() {
    std::env::set_var("OUTLOOK_CONFIG_DIR", "~/.config/outlook-cli");

    let mut cfg = AppConfig::default();
    cfg.theme = ThemePreset::Dark;
    cfg.density = DensityMode::Cozy;
    cfg.show_nav_rail = true;
    cfg.group_threads = true;
    cfg.inline_images = true;

    let auth = AuthManager::new(cfg.clone()).expect("auth init");
    let graph = GraphClient::new(cfg.clone(), auth.clone()).expect("graph init");
    let storage = Storage::open_in_memory().expect("in-memory sqlite");
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    app.download_dir = PathBuf::from("~/Downloads/outlook");
    app.sync_summary = "Synced · Daemon".to_string();
    app.daemon_attached = true;
    app.daemon_pid = Some(5104);
    app.my_display_name = "Alex Rivera".to_string();
    app.my_email = "alex.rivera@acme.dev".to_string();
    app.my_profile = Some(UserProfile {
        id: "u-alex".into(),
        display_name: "Alex Rivera".into(),
        email: Some("alex.rivera@acme.dev".into()),
        job_title: Some("Staff Platform Engineer".into()),
        department: Some("Acme Engineering".into()),
    });

    app.mail_folders = vec![
        MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 1,
            unread_item_count: 3,
            total_item_count: 42,
        },
        MailFolder {
            id: "f-drafts".into(),
            display_name: "Drafts".into(),
            well_known_name: Some("drafts".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 2,
        },
        MailFolder {
            id: "f-sent".into(),
            display_name: "Sent Items".into(),
            well_known_name: Some("sentitems".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 18,
        },
        MailFolder {
            id: "f-archive".into(),
            display_name: "Archive".into(),
            well_known_name: Some("archive".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 124,
        },
        MailFolder {
            id: "f-releases".into(),
            display_name: "Releases".into(),
            well_known_name: None,
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 9,
        },
    ];
    app.active_folder_id = "f-inbox".to_string();

    let today = Local::now().date_naive();
    let tomorrow = today + Duration::days(1);
    let today_str = today.format("%Y-%m-%d").to_string();
    let tomorrow_str = tomorrow.format("%Y-%m-%d").to_string();

    app.mail_messages = vec![
        MailMessage {
            id: "mail-1".into(),
            internet_message_id: None,
            conversation_id: Some("conv-v24".into()),
            folder_id: Some("f-inbox".into()),
            subject: "Q4 Platform v2.4 Release Sign-Off & Benchmarks".into(),
            from: Some(recipient("Elena Park", "elena.park@acme.dev")),
            to_recipients: vec![
                recipient("Alex Rivera", "alex.rivera@acme.dev"),
                recipient("Marcus Chen", "marcus.chen@acme.dev"),
            ],
            cc_recipients: vec![recipient("Priya Patel", "priya.patel@acme.dev")],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: format!("{today_str}T09:15:00Z"),
            sent_at: Some(format!("{today_str}T09:14:50Z")),
            is_read: false,
            is_draft: false,
            flagged: true,
            importance: "high".into(),
            inference_classification: Some("focused".into()),
            has_attachments: true,
            body_loaded: true,
            preview: "All regional canary checks are green. P99 gateway latency dropped to 19ms.".into(),
            body_html: "<h3>v2.4 Canary Summary</h3>\
                <p>Hi team — all regional canary checks are <b>green</b> across US-East and EU-West.</p>\
                <ul>\
                  <li>P99 Gateway Latency: <b>19ms</b> (down from 48ms)</li>\
                  <li>SQLite FTS5 Search: <b>3.2ms</b> cold median</li>\
                  <li>Zero error-budget burn across 24h soak</li>\
                </ul>\
                <p>Please sign off on the <a href=\"https://status.acme.dev/releases/v2.4\">Release Dashboard</a> \
                and review <a href=\"https://github.com/acme/platform/pull/482\">PR #482</a> before 14:00 UTC.</p>\
                <p>Best,<br/>Elena</p>".into(),
            body_plain: "v2.4 Canary Summary\nAll regional canary checks are green.".into(),
            attachments: vec![MailAttachment {
                id: "att-1".into(),
                name: "v24-benchmarks.pdf".into(),
                content_type: "application/pdf".into(),
                size: 842_100,
                is_inline: false,
                content_id: None,
                content_bytes_b64: None,
            }],
            meeting_request: None,
            web_link: None,
        },
        MailMessage {
            id: "mail-2".into(),
            internet_message_id: None,
            conversation_id: Some("conv-sre".into()),
            folder_id: Some("f-inbox".into()),
            subject: "Incident Runbook & On-Call Handoff Links".into(),
            from: Some(recipient("Marcus Chen", "marcus.chen@acme.dev")),
            to_recipients: vec![recipient("Alex Rivera", "alex.rivera@acme.dev")],
            cc_recipients: vec![recipient("Elena Park", "elena.park@acme.dev")],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: format!("{today_str}T08:42:00Z"),
            sent_at: Some(format!("{today_str}T08:41:55Z")),
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: Some("focused".into()),
            has_attachments: false,
            body_loaded: true,
            preview: "Updated the failover docs and Grafana links ahead of tonight's rotation.".into(),
            body_html: "<p>Hi Alex,</p>\
                <p>I refreshed all links ahead of tonight's SRE rotation:</p>\
                <ul>\
                  <li><a href=\"https://docs.acme.dev/runbooks/v24-failover\">Failover Runbook</a></li>\
                  <li><a href=\"https://metrics.acme.dev/d/gw-v24\">Grafana Edge Dashboard</a></li>\
                  <li><a href=\"https://pager.acme.dev/schedules/platform\">On-Call Schedule</a></li>\
                  <li><a href=\"https://status.acme.dev/incidents/archive\">Postmortem Archive</a></li>\
                </ul>\
                <p>Let me know if any alert thresholds need tuning.</p>\
                <p>— Marcus</p>".into(),
            body_plain: "I refreshed all links ahead of tonight's SRE rotation.".into(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        },
        MailMessage {
            id: "mail-3".into(),
            internet_message_id: None,
            conversation_id: Some("conv-sec".into()),
            folder_id: Some("f-inbox".into()),
            subject: "Security Audit: Entra ID PKCE Scope Review".into(),
            from: Some(recipient("Priya Patel", "priya.patel@acme.dev")),
            to_recipients: vec![recipient("Alex Rivera", "alex.rivera@acme.dev")],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: format!("{today_str}T07:55:00Z"),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: true,
            importance: "normal".into(),
            inference_classification: Some("focused".into()),
            has_attachments: true,
            body_loaded: true,
            preview: "Confirmed least-privilege delegated scopes and OS keyring isolation.".into(),
            body_html: "<p>Security review passed with zero findings. Keyring fallback permissions verified at <code>0600</code>.</p>".into(),
            body_plain: "Security review passed with zero findings.".into(),
            attachments: vec![MailAttachment {
                id: "att-2".into(),
                name: "oauth-scope-audit.md".into(),
                content_type: "text/markdown".into(),
                size: 14_320,
                is_inline: false,
                content_id: None,
                content_bytes_b64: None,
            }],
            meeting_request: None,
            web_link: None,
        },
        MailMessage {
            id: "mail-4".into(),
            internet_message_id: None,
            conversation_id: Some("conv-design".into()),
            folder_id: Some("f-inbox".into()),
            subject: "Minimal-Chrome TUI Palette & Accessibility Check".into(),
            from: Some(recipient("Sofia Morales", "sofia.morales@acme.dev")),
            to_recipients: vec![recipient("Alex Rivera", "alex.rivera@acme.dev")],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: format!("{today_str}T06:20:00Z"),
            sent_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: Some("focused".into()),
            has_attachments: false,
            body_loaded: true,
            preview: "Contrast ratios across Midnight Dark and Daylight Light pass WCAG AA.".into(),
            body_html: "<p>Contrast ratios across Midnight Dark and Daylight Light pass WCAG AA.</p>".into(),
            body_plain: "Contrast ratios across Midnight Dark and Daylight Light pass WCAG AA.".into(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        },
        MailMessage {
            id: "mail-5".into(),
            internet_message_id: None,
            conversation_id: Some("conv-ci".into()),
            folder_id: Some("f-inbox".into()),
            subject: "AlmaLinux 8 GLIBC 2.28 Release Pipeline Green".into(),
            from: Some(recipient("David Kim", "david.kim@acme.dev")),
            to_recipients: vec![recipient("Alex Rivera", "alex.rivera@acme.dev")],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: format!("{today_str}T05:10:00Z"),
            sent_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: Some("other".into()),
            has_attachments: false,
            body_loaded: true,
            preview: "All 6 platform binaries verified and signed with SHA256SUMS.".into(),
            body_html: "<p>All 6 platform binaries verified and signed with SHA256SUMS.</p>".into(),
            body_plain: "All 6 platform binaries verified and signed with SHA256SUMS.".into(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        },
    ];
    app.active_mail_id = Some("mail-1".to_string());

    app.calendar_events = vec![
        CalendarEvent {
            id: "ev-1".into(),
            subject: "Platform Daily Standup".into(),
            start_iso: format!("{today_str}T09:30:00+00:00"),
            end_iso: format!("{today_str}T09:45:00+00:00"),
            time_zone: "UTC".into(),
            is_all_day: false,
            location: Some("Microsoft Teams Meeting".into()),
            organizer_name: Some("Elena Park".into()),
            organizer_email: Some("elena.park@acme.dev".into()),
            attendees: vec![
                EventAttendee {
                    name: Some("Elena Park".into()),
                    address: "elena.park@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "organizer".into(),
                },
                EventAttendee {
                    name: Some("Alex Rivera".into()),
                    address: "alex.rivera@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "accepted".into(),
                },
                EventAttendee {
                    name: Some("Marcus Chen".into()),
                    address: "marcus.chen@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "accepted".into(),
                },
            ],
            response_status: "accepted".into(),
            is_online_meeting: true,
            online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/acme-standup".into()),
            body_html: "<p>Quick sync on v2.4 canary rollout and blockers.</p>".into(),
            body_plain: "Quick sync on v2.4 canary rollout and blockers.".into(),
            is_cancelled: false,
            web_link: None,
        },
        CalendarEvent {
            id: "ev-2".into(),
            subject: "v2.4.0 Production Go / No-Go Review".into(),
            start_iso: format!("{today_str}T14:00:00+00:00"),
            end_iso: format!("{today_str}T14:45:00+00:00"),
            time_zone: "UTC".into(),
            is_all_day: false,
            location: Some("Conf Room 4B / Teams".into()),
            organizer_name: Some("Alex Rivera".into()),
            organizer_email: Some("alex.rivera@acme.dev".into()),
            attendees: vec![
                EventAttendee {
                    name: Some("Alex Rivera".into()),
                    address: "alex.rivera@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "organizer".into(),
                },
                EventAttendee {
                    name: Some("Elena Park".into()),
                    address: "elena.park@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "accepted".into(),
                },
                EventAttendee {
                    name: Some("Priya Patel".into()),
                    address: "priya.patel@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "accepted".into(),
                },
                EventAttendee {
                    name: Some("Marcus Chen".into()),
                    address: "marcus.chen@acme.dev".into(),
                    attendee_type: "optional".into(),
                    response_status: "tentativelyAccepted".into(),
                },
            ],
            response_status: "organizer".into(),
            is_online_meeting: true,
            online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/acme-gonogo".into()),
            body_html: "<p>Final telemetry review before promoting US-East and EU-West to 100% traffic.</p>\
                <ul><li>Latency P99 &amp; error budget check</li><li>Rollback trigger verification</li><li>Stakeholder status update</li></ul>".into(),
            body_plain: "Final telemetry review before promoting to 100% traffic.".into(),
            is_cancelled: false,
            web_link: None,
        },
        CalendarEvent {
            id: "ev-3".into(),
            subject: "Architecture Sync: MCP Tooling & Local Daemon".into(),
            start_iso: format!("{today_str}T16:30:00+00:00"),
            end_iso: format!("{today_str}T17:15:00+00:00"),
            time_zone: "UTC".into(),
            is_all_day: false,
            location: Some("Microsoft Teams Meeting".into()),
            organizer_name: Some("Priya Patel".into()),
            organizer_email: Some("priya.patel@acme.dev".into()),
            attendees: vec![
                EventAttendee {
                    name: Some("Priya Patel".into()),
                    address: "priya.patel@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "organizer".into(),
                },
                EventAttendee {
                    name: Some("Alex Rivera".into()),
                    address: "alex.rivera@acme.dev".into(),
                    attendee_type: "required".into(),
                    response_status: "tentativelyAccepted".into(),
                },
            ],
            response_status: "tentativelyAccepted".into(),
            is_online_meeting: true,
            online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/acme-mcp".into()),
            body_html: "<p>Review MCP tool schemas across the @shinick terminal suite.</p>".into(),
            body_plain: "Review MCP tool schemas across the @shinick terminal suite.".into(),
            is_cancelled: false,
            web_link: None,
        },
        CalendarEvent {
            id: "ev-4".into(),
            subject: "Q4 Sprint Demo & Retrospective".into(),
            start_iso: format!("{tomorrow_str}T11:00:00+00:00"),
            end_iso: format!("{tomorrow_str}T12:00:00+00:00"),
            time_zone: "UTC".into(),
            is_all_day: false,
            location: Some("Main Auditorium / Teams".into()),
            organizer_name: Some("Sofia Morales".into()),
            organizer_email: Some("sofia.morales@acme.dev".into()),
            attendees: vec![],
            response_status: "accepted".into(),
            is_online_meeting: true,
            online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/acme-retro".into()),
            body_html: "<p>Live demo of TUI features and sprint retrospective.</p>".into(),
            body_plain: "Live demo of TUI features and sprint retrospective.".into(),
            is_cancelled: false,
            web_link: None,
        },
    ];
    app.active_event_id = Some("ev-2".to_string());

    app.todo_lists = vec![
        TodoList {
            id: "list-release".into(),
            display_name: "v2.4 Release".into(),
            wellknown_list_name: Some("defaultList".into()),
            is_owner: true,
            is_shared: false,
        },
        TodoList {
            id: "list-sre".into(),
            display_name: "SRE & Infra".into(),
            wellknown_list_name: None,
            is_owner: true,
            is_shared: true,
        },
        TodoList {
            id: "list-personal".into(),
            display_name: "Personal".into(),
            wellknown_list_name: None,
            is_owner: true,
            is_shared: false,
        },
    ];
    app.active_todo_list_id = Some("list-release".to_string());
    app.todo_tasks = vec![
        TodoTask {
            id: "t-1".into(),
            list_id: "list-release".into(),
            title: "Promote EU-West & US-East canary to 100%".into(),
            status: "inProgress".into(),
            importance: "high".into(),
            body_plain: Some("Confirm zero 5xx spikes on Grafana Latency Board before flipping final weight.\nNotify #platform-release once complete.".into()),
            due_date_iso: Some(format!("{today_str}T16:00:00Z")),
            reminder_iso: None,
            completed_at: None,
            created_at: None,
            updated_at: None,
        },
        TodoTask {
            id: "t-2".into(),
            list_id: "list-release".into(),
            title: "Publish release notes & SHA256SUMS checksums".into(),
            status: "notStarted".into(),
            importance: "high".into(),
            body_plain: Some("Attach AlmaLinux 8 GLIBC 2.28 verification logs to GitHub Release.".into()),
            due_date_iso: Some(format!("{today_str}T18:00:00Z")),
            reminder_iso: None,
            completed_at: None,
            created_at: None,
            updated_at: None,
        },
        TodoTask {
            id: "t-3".into(),
            list_id: "list-release".into(),
            title: "Review PR #482 connection pool backpressure tests".into(),
            status: "notStarted".into(),
            importance: "normal".into(),
            body_plain: Some("Check edge-case reconnect timeouts under simulated packet loss.".into()),
            due_date_iso: Some(format!("{tomorrow_str}T12:00:00Z")),
            reminder_iso: None,
            completed_at: None,
            created_at: None,
            updated_at: None,
        },
        TodoTask {
            id: "t-4".into(),
            list_id: "list-release".into(),
            title: "Verify Entra ID PKCE scopes & keyring fallback".into(),
            status: "completed".into(),
            importance: "normal".into(),
            body_plain: Some("Signed off by Priya Patel in Security Audit.".into()),
            due_date_iso: Some(format!("{today_str}T09:00:00Z")),
            reminder_iso: None,
            completed_at: Some(format!("{today_str}T08:15:00Z")),
            created_at: None,
            updated_at: None,
        },
        TodoTask {
            id: "t-5".into(),
            list_id: "list-release".into(),
            title: "Enable WAL + FTS5 cache warm-up in daemon".into(),
            status: "completed".into(),
            importance: "low".into(),
            body_plain: Some("Cold search latency verified at 3.2ms median.".into()),
            due_date_iso: None,
            reminder_iso: None,
            completed_at: Some(format!("{today_str}T07:30:00Z")),
            created_at: None,
            updated_at: None,
        },
    ];
    app.active_todo_task_id = Some("t-1".to_string());

    app.contacts = vec![
        ContactItem {
            id: "c-elena".into(),
            display_name: "Elena Park".into(),
            given_name: Some("Elena".into()),
            surname: Some("Park".into()),
            email_addresses: vec!["elena.park@acme.dev".into()],
            mobile_phone: None,
            business_phones: vec![],
            company_name: Some("Acme Corp".into()),
            job_title: Some("Principal Engineer".into()),
            department: Some("Platform".into()),
            notes: None,
            source: "directory".into(),
        },
        ContactItem {
            id: "c-marcus".into(),
            display_name: "Marcus Chen".into(),
            given_name: Some("Marcus".into()),
            surname: Some("Chen".into()),
            email_addresses: vec!["marcus.chen@acme.dev".into()],
            mobile_phone: None,
            business_phones: vec![],
            company_name: Some("Acme Corp".into()),
            job_title: Some("Staff SRE".into()),
            department: Some("Reliability".into()),
            notes: None,
            source: "directory".into(),
        },
        ContactItem {
            id: "c-priya".into(),
            display_name: "Priya Patel".into(),
            given_name: Some("Priya".into()),
            surname: Some("Patel".into()),
            email_addresses: vec!["priya.patel@acme.dev".into()],
            mobile_phone: None,
            business_phones: vec![],
            company_name: Some("Acme Corp".into()),
            job_title: Some("Security Lead".into()),
            department: Some("Security".into()),
            notes: None,
            source: "directory".into(),
        },
    ];

    let mut ui = UiState::from_config(&cfg);
    ui.theme = Theme::midnight_dark();
    ui.theme_preset = ThemePreset::Dark;
    ui.density = DensityMode::Cozy;
    ui.show_nav_rail = true;
    ui.active_nav_tab = TAB_MAIL;
    ui.nav_selected = 1; // Inbox
    ui.focus = Focus::List;
    ui.mail_list_scroll.selected_idx = Some(0);

    let mut frames = Vec::new();

    // Frame 1: Mail 3-Pane Inbox (Folders | Message List | HTML-rendered Mail Preview)
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "3-Pane Mail Inbox · Folders, message list & HTML-rendered reading pane",
        "outlook",
    ));

    // Frame 2: Select another message + toggle inline hrefs (`h`)
    app.active_mail_id = Some("mail-2".to_string());
    ui.mail_list_scroll.selected_idx = Some(1);
    ui.focus = Focus::Reader;
    ui.show_link_urls = true;
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Toggle inline link URLs (href) directly in the Mail reading pane",
        "j / h",
    ));

    // Frame 3: Open Link Extractor overlay (`u`)
    let links_overlay = LinkExtractorOverlay::from_mail(&app.mail_messages[1]);
    ui.push_overlay(Overlay::links(links_overlay));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Link Extractor · inspect, open, or copy any URL in the active email",
        "u",
    ));
    ui.close_top_overlay();
    ui.show_link_urls = false;

    // Frame 4: Switch to Calendar view (`2`) showing today's meetings
    ui.active_nav_tab = TAB_CALENDAR;
    ui.cal_range = CalendarRangeFilter::AllUpcoming;
    ui.focus = Focus::List;
    ui.cal_list_scroll.selected_idx = Some(1);
    app.active_event_id = Some("ev-2".to_string());
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "Calendar Agenda · mini month grid, RSVP badges & online meeting details",
        "2",
    ));

    // Frame 5: Switch to To Do tasks view (`3`) showing tasks & priorities
    ui.active_nav_tab = TAB_TASKS;
    ui.focus = Focus::List;
    ui.todo_list_scroll.selected_idx = Some(0);
    app.active_todo_task_id = Some("t-1".to_string());
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Microsoft To Do Tasks · checkboxes, priority stars & due dates",
        "3",
    ));

    // Frame 6: Open Compose modal (`c`) with attachment pill
    ui.active_nav_tab = TAB_MAIL;
    ui.focus = Focus::List;
    app.active_mail_id = Some("mail-1".to_string());
    ui.mail_list_scroll.selected_idx = Some(0);
    let mut compose = MailComposeOverlay::new();
    compose.to = "elena.park@acme.dev, marcus.chen@acme.dev".to_string();
    compose.cc = "priya.patel@acme.dev".to_string();
    compose.subject = "Re: Q4 Platform v2.4 Release Sign-Off".to_string();
    compose.body.load_text(
        "Approved from Platform SRE!\n\nAttached the final latency summary and rollback checklist.\nPromoting to 100% at 14:00 UTC.",
    );
    compose.attachments.push(PathBuf::from("/workspace/acme-platform/v24-load-test.pdf"));
    compose.attachments.push(PathBuf::from("/workspace/acme-platform/latency-p99.png"));
    compose.field = 4;
    compose.suggestion_dismissed = true;
    ui.push_overlay(Overlay::mail_compose(compose));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "Structured Mail Compose · Markdown body, contact autocomplete & attachments",
        "c",
    ));
    ui.close_top_overlay();

    // Frame 7: Open Command Palette (`Ctrl+K`)
    let mut palette = CommandPalette::new();
    palette.query = ">".to_string();
    palette.selected = 2;
    ui.push_overlay(Overlay::CommandPalette(palette));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2000,
        "Unified Omni-Switcher & Command Palette · #folders, @contacts, >commands",
        "Ctrl+K",
    ));
    ui.close_top_overlay();

    // Frame 8: Open Settings overlay (`Ctrl+,`)
    let settings = SettingsState { selected: 7 };
    ui.push_overlay(Overlay::Settings(settings));
    frames.push(render_and_capture(
        &mut app,
        &mut ui,
        2200,
        "Settings · themes, density, thread grouping & image preview protocol",
        "Ctrl+,",
    ));
    ui.close_top_overlay();

    let spec = json!({
        "title": "outlook — Acme Engineering",
        "badge": "@shinick/outlook v0.1.2",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [20, 22, 32],
        "fg": [226, 230, 243],
        "frames": frames,
    });

    let tmp_json = std::env::temp_dir().join("outlook_demo_frames.json");
    std::fs::write(
        &tmp_json,
        serde_json::to_string(&spec).expect("serialize spec"),
    )
    .expect("write frames json");

    let out_gif = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("demo.gif");

    let status = Command::new("python")
        .arg(RENDERER_SCRIPT)
        .arg(&tmp_json)
        .arg(&out_gif)
        .arg("--preview-png")
        .arg(PREVIEW_DIR)
        .status()
        .expect("failed to invoke python render_tui_gif.py");
    assert!(status.success(), "render_tui_gif.py exited with {status}");
}
