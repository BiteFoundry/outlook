use std::path::PathBuf;

use base64::Engine as _;
use clap::Parser;
use serde_json::json;

use crate::{
    config::{AppConfig, Persona},
    models::MailFolder,
};

use super::args::*;
use super::format::*;
use super::mail::*;
use super::parse::*;
use super::runtime::*;

#[cfg(test)]
mod scope_tests {
    use super::super::account::missing_configured_scopes;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_scopes_lists_configured_scopes_absent_from_the_token() {
        let configured = list(&[
            "offline_access",
            "User.Read",
            "User.ReadBasic.All",
            "Mail.ReadWrite",
            "Contacts.Read",
        ]);
        assert_eq!(
            missing_configured_scopes(&configured, &list(&["user.read", "Mail.ReadWrite", "Contacts.Read"])),
            ["User.ReadBasic.All"]
        );
        // A read/write grant covers the read-only scope; offline_access never appears in `scp`.
        assert!(missing_configured_scopes(
            &configured,
            &list(&["User.Read", "User.ReadBasic.All", "Mail.ReadWrite", "Contacts.ReadWrite"])
        )
        .is_empty());
        // Opaque tokens (no `scp`) cannot be judged.
        assert!(missing_configured_scopes(&configured, &[]).is_empty());
    }
}


#[test]
fn parses_default_tui_without_subcommand() {
    let cli = Cli::try_parse_from(["outlook"]).unwrap();
    assert!(cli.command.is_none());
}

#[test]
fn parses_mail_aliases_flags_and_archive() {
    let cli = Cli::try_parse_from([
        "outlook",
        "ls",
        "--folder",
        "sent",
        "--limit",
        "50",
        "--unread",
        "--flagged",
        "--focused",
        "--cached",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Mail {
            folder,
            limit,
            unread,
            flagged,
            focused,
            cached,
            json,
            ..
        } => {
            assert_eq!(folder, "sent");
            assert_eq!(limit, 50);
            assert!(unread);
            assert!(flagged);
            assert!(focused);
            assert!(cached);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "outlook",
        "cat",
        "msg-42",
        "--raw-html",
        "--mark-read",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Read {
            message_id,
            raw_html,
            mark_read,
            json,
            ..
        } => {
            assert_eq!(message_id, "msg-42");
            assert!(raw_html);
            assert!(mark_read);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "outlook",
        "compose",
        "--to",
        "alice@example.com",
        "--cc",
        "bob@example.com",
        "--subject",
        "Q4 Plan",
        "--body",
        "Hello **team**",
        "--attach",
        "report.pdf",
        "--inline",
        "chart.png",
        "--importance",
        "high",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Send {
            to,
            cc,
            subject,
            body,
            attach,
            inline,
            importance,
            json,
            ..
        } => {
            assert_eq!(to, vec!["alice@example.com"]);
            assert_eq!(cc, vec!["bob@example.com"]);
            assert_eq!(subject, "Q4 Plan");
            assert_eq!(body.as_deref(), Some("Hello **team**"));
            assert_eq!(attach, vec![PathBuf::from("report.pdf")]);
            assert_eq!(inline, vec![PathBuf::from("chart.png")]);
            assert_eq!(importance, "high");
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from(["outlook", "archive", "msg-99", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Archive {
            message_id,
            json: true,
            full: false,
        } if message_id == "msg-99"
    ));

    let cli = Cli::try_parse_from(["outlook", "mark", "m1", "--read", "--unflag"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Mark {
            read: true,
            unread: false,
            flag: false,
            unflag: true,
            ..
        }
    ));
    assert!(Cli::try_parse_from(["outlook", "mark", "m1", "--read", "--unread"]).is_err());
}

#[test]
fn parses_calendar_tasks_contacts_serve_mcp_and_daemon() {
    let cli = Cli::try_parse_from([
        "outlook",
        "cal",
        "--days",
        "14",
        "--from",
        "2026-11-20",
        "--to",
        "2026-12-01",
        "--json",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Calendar {
            days,
            from,
            to,
            json,
            ..
        } => {
            assert_eq!(days, 14);
            assert_eq!(from.as_deref(), Some("2026-11-20"));
            assert_eq!(to.as_deref(), Some("2026-12-01"));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "outlook",
        "create-event",
        "--subject",
        "Sync",
        "--start",
        "2026-11-20T10:00:00Z",
        "--end",
        "2026-11-20T11:00:00Z",
        "--attendee",
        "a@b.com",
        "--online",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::CreateEvent { online: true, .. }
    ));

    let cli =
        Cli::try_parse_from(["outlook", "todo", "--list", "Work", "--all", "--fast"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Tasks {
            list,
            all: true,
            cached: true,
            ..
        } if list == "Work"
    ));

    let cli = Cli::try_parse_from([
        "outlook",
        "add-task",
        "Ship v1",
        "--due",
        "2026-11-25",
        "--importance",
        "high",
        "--notes",
        "Final check",
    ])
    .unwrap();
    assert!(matches!(cli.command.unwrap(), Command::AddTask { .. }));

    let cli = Cli::try_parse_from(["outlook", "done", "t1", "--undo"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Done { undo: true, .. }
    ));

    let cli = Cli::try_parse_from(["outlook", "serve"]).unwrap();
    match cli.command.unwrap() {
        Command::Serve {
            bind,
            port,
            api_key,
            allow_keyless,
            cors_origin,
        } => {
            assert_eq!(bind, "127.0.0.1");
            assert_eq!(port, 3939);
            assert!(api_key.is_none());
            assert!(!allow_keyless);
            assert!(cors_origin.is_empty());
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "outlook",
        "api",
        "--bind",
        "0.0.0.0",
        "--port",
        "4040",
        "--api-key",
        "secret-key",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Serve {
            bind,
            port,
            api_key,
            ..
        } => {
            assert_eq!(bind, "0.0.0.0");
            assert_eq!(port, 4040);
            assert_eq!(api_key.as_deref(), Some("secret-key"));
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "outlook",
        "serve",
        "--allow-keyless",
        "--cors-origin",
        "http://localhost:5173",
        "--cors-origin",
        "https://app.example.com",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Serve {
            allow_keyless,
            cors_origin,
            ..
        } => {
            assert!(allow_keyless);
            assert_eq!(
                cors_origin,
                ["http://localhost:5173", "https://app.example.com"]
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }
    assert!(
        Cli::try_parse_from(["outlook", "serve", "--allow-keyless", "--api-key", "k"]).is_err(),
        "--allow-keyless contradicts --api-key"
    );

    assert!(matches!(
        Cli::try_parse_from(["outlook", "mcp-server"])
            .unwrap()
            .command
            .unwrap(),
        Command::Mcp
    ));
    assert!(matches!(
        Cli::try_parse_from(["outlook", "daemon", "run"])
            .unwrap()
            .command
            .unwrap(),
        Command::Daemon {
            command: DaemonCommand::Run
        }
    ));
}

#[test]
fn env_help_documents_all_outlook_vars() {
    let help = <Cli as clap::CommandFactory>::command()
        .render_help()
        .to_string();
    for var in [
        "OUTLOOK_TOKEN_FILE",
        "OUTLOOK_TOKEN_REFRESH_CMD",
        "OUTLOOK_ACCESS_TOKEN",
        "OUTLOOK_SHARE_M365_AUTH",
        "OUTLOOK_IMAGE_PROTOCOL",
        "OUTLOOK_CONFIG_DIR",
        "OUTLOOK_DATA_DIR",
    ] {
        assert!(help.contains(var), "help output missing {var}");
    }
}

#[test]
fn fast_help_version_and_normalizers_work() {
    assert!(
        try_fast_help_or_version_from(["outlook", "--help"], Persona::Outlook).is_some()
    );
    assert!(try_fast_help_or_version_from(["outlook", "-V"], Persona::Outlook).is_some());
    assert!(try_fast_help_or_version_from(["outlook", "mail"], Persona::Outlook).is_none());

    assert_eq!(normalize_rsvp_response("ACCEPT").unwrap(), "accept");
    assert_eq!(
        normalize_rsvp_response("tentative").unwrap(),
        "tentativelyAccept"
    );
    assert_eq!(normalize_rsvp_response("Decline").unwrap(), "decline");
    assert!(normalize_rsvp_response("maybe").is_err());

    assert_eq!(
        normalize_todo_due_iso("2026-11-25").unwrap(),
        "2026-11-25T00:00:00Z"
    );
    assert_eq!(normalize_todo_importance("HIGH").unwrap(), "high");
    assert!(normalize_todo_importance("urgent").is_err());

    let html = crate::parser::compile_outgoing_mail_html(
        "# Title\n\nHello **world** `code` [link](https://example.com)",
        &[],
        None,
    );
    assert!(html.contains("<h1>Title</h1>"));
    assert!(html.contains("<strong>world</strong>"));
    assert!(html.contains("<code>code</code>"));
    assert!(html.contains("<a href=\"https://example.com\">link</a>"));

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"oid":"user-123","sub":"sub-1"}"#);
    let jwt = format!("Bearer header.{payload}.sig");
    assert_eq!(extract_user_id_from_jwt(&jwt).as_deref(), Some("user-123"));
    assert_eq!(parse_cli_cache_secs(Some(" 60 ")), 60);
}

#[test]
fn delete_gate_requires_yes_or_confirmation() {
    delete_gate(true, false, None).unwrap();
    delete_gate(true, true, None).unwrap();
    assert!(delete_gate(false, true, Some("yes")).is_err());
    delete_gate(false, false, Some("YES")).unwrap();
    assert!(delete_gate(false, false, Some("no")).is_err());
    assert!(delete_gate(false, false, None).is_err());
}

#[test]
fn format_mail_folders_lines_uses_hierarchical_paths() {
    let folders = vec![
        MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 1,
            unread_item_count: 2,
            total_item_count: 10,
        },
        MailFolder {
            id: "f-proj".into(),
            display_name: "Projects".into(),
            well_known_name: None,
            parent_folder_id: Some("f-inbox".into()),
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 4,
        },
    ];
    let lines = format_mail_folders_lines(&folders);
    assert_eq!(lines[0], "Inbox [inbox]  unread=2/10  (f-inbox)");
    assert_eq!(lines[1], "Inbox / Projects  unread=1/4  (f-proj)");
}

#[test]
fn parses_hyphen_text_and_markdown_values_across_all_audit_paths() {
    let samples = ["- 목록 항목", "- first\n- second", "- 첫째\n- 둘째", "---"];

    for sample in samples {
        // 1. send --body (and --subject)
        let cli = parse_cli_from([
            "outlook",
            "send",
            "--to",
            "audit@example.invalid",
            "--subject",
            sample,
            "--body",
            sample,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Send {
                subject,
                body: Some(body),
                json: true,
                ..
            } if subject == sample && body == sample
        ));

        // compose alias + stdin mode (omitted --body)
        let cli = parse_cli_from([
            "outlook",
            "compose",
            "--to",
            "audit@example.invalid",
            "--subject",
            sample,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Send {
                subject,
                body: None,
                json: true,
                ..
            } if subject == sample
        ));

        // 2. reply body_md + --all before and after
        let cli = parse_cli_from(["outlook", "reply", "id", sample, "--all", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Reply {
                message_id,
                body_md,
                all: true,
                json: true,
            } if message_id == "id" && body_md == sample
        ));
        let cli = parse_cli_from(["outlook", "reply", "id", "--all", sample]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Reply {
                message_id,
                body_md,
                all: true,
                json: false,
            } if message_id == "id" && body_md == sample
        ));

        // 3. forward comment_md before and after --to, plus omitted comment
        let cli = parse_cli_from([
            "outlook",
            "forward",
            "id",
            sample,
            "--to",
            "audit@example.invalid",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Forward {
                message_id,
                to,
                comment_md: Some(comment),
                json: true,
            } if message_id == "id" && to == vec!["audit@example.invalid"] && comment == sample
        ));
        let cli = parse_cli_from([
            "outlook",
            "forward",
            "id",
            "--to",
            "audit@example.invalid",
            sample,
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Forward {
                comment_md: Some(comment),
                ..
            } if comment == sample
        ));

        // 4. edit-draft --body and --subject
        let cli = parse_cli_from(["outlook", "edit-draft", "id", "--body", sample, "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::EditDraft {
                message_id,
                subject: None,
                body: Some(body),
                body_html: None,
                json: true,
            } if message_id == "id" && body == sample
        ));
        let cli =
            parse_cli_from(["outlook", "edit-draft", "id", "--subject", sample, "--json"])
                .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::EditDraft {
                subject: Some(subject),
                body: None,
                json: true,
                ..
            } if subject == sample
        ));

        // 5. create-event --body, --subject, --location
        let cli = parse_cli_from([
            "outlook",
            "create-event",
            "--subject",
            sample,
            "--start",
            "2026-12-01T10:00",
            "--end",
            "2026-12-01T11:00",
            "--location",
            sample,
            "--body",
            sample,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::CreateEvent {
                subject,
                location: Some(loc),
                body: Some(body),
                json: true,
                ..
            } if subject == sample && loc == sample && body == sample
        ));

        // 6. add-task title + --notes and --body alias
        let cli = parse_cli_from(["outlook", "add-task", sample, "--notes", sample, "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::AddTask {
                title,
                notes: Some(notes),
                json: true,
                ..
            } if title == sample && notes == sample
        ));
        let cli = parse_cli_from(["outlook", "add-task", "Test", "--body", sample, "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::AddTask {
                title,
                notes: Some(notes),
                json: true,
                ..
            } if title == "Test" && notes == sample
        ));

        // 7. rsvp --comment
        let cli =
            parse_cli_from(["outlook", "rsvp", "id", "accept", "--comment", sample, "--json"])
                .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Rsvp {
                event_id,
                response,
                comment: Some(comment),
                json: true,
            } if event_id == "id" && response == "accept" && comment == sample
        ));

        // 9. update-task --title (and sibling update-event --subject/--title)
        let cli =
            parse_cli_from(["outlook", "update-task", "id", "--title", sample, "--json"])
                .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::UpdateTask {
                task_id,
                title: Some(title),
                json: true,
                ..
            } if task_id == "id" && title == sample
        ));
        let cli =
            parse_cli_from(["outlook", "update-event", "id", "--title", sample, "--json"])
                .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::UpdateEvent {
                event_id,
                subject: Some(subject),
                json: true,
                ..
            } if event_id == "id" && subject == sample
        ));

        // 10. search <QUERY> (and sibling contacts --query/--q)
        let cli = parse_cli_from(["outlook", "search", sample, "--remote", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Search {
                query,
                remote: true,
                json: true,
                ..
            } if query == sample
        ));
        let cli = parse_cli_from(["outlook", "contacts", "--q", sample, "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Contacts {
                query: Some(query),
                json: true,
                ..
            } if query == sample
        ));
    }

    // Omitted comment on forward still works cleanly
    let cli = parse_cli_from([
        "outlook",
        "forward",
        "id",
        "--to",
        "audit@example.invalid",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Forward {
            message_id,
            to,
            comment_md: None,
            json: true,
        } if message_id == "id" && to == vec!["audit@example.invalid"]
    ));
}

#[test]
fn preserves_equals_positional_separator_flags_missing_values_and_help() {
    // Explicit equals accepts flag-looking text on options
    for literal in ["--dry-run", "--json", "-h", "--", "- 목록 항목"] {
        let body_arg = format!("--body={literal}");
        let cli = parse_cli_from([
            "outlook",
            "send",
            "--to",
            "audit@example.invalid",
            "--subject",
            "Test",
            &body_arg,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Send {
                body: Some(body),
                json: true,
                ..
            } if body == literal
        ));
        assert!(
            try_fast_help_or_version_from(
                [
                    "outlook",
                    "send",
                    "--to",
                    "audit@example.invalid",
                    "--subject",
                    "Test",
                    &body_arg,
                ],
                Persona::Outlook,
            )
            .is_none(),
            "attached value {body_arg} must not trigger fast help"
        );
    }

    // Positional `--` accepts flag-looking text on positionals
    for literal in ["--dry-run", "--all", "--json", "-h", "--", "- 목록 항목"] {
        let cli = parse_cli_from(["outlook", "reply", "id", "--json", "--", literal]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Reply {
                body_md,
                all: false,
                json: true,
                ..
            } if body_md == literal
        ));

        let cli = parse_cli_from([
            "outlook",
            "forward",
            "id",
            "--to",
            "audit@example.invalid",
            "--",
            literal,
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Forward {
                comment_md: Some(comment),
                ..
            } if comment == literal
        ));

        let cli = parse_cli_from(["outlook", "search", "--", literal]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Search {
                query,
                remote: false,
                json: false,
                ..
            } if query == literal
        ));
        assert!(
            try_fast_help_or_version_from(["outlook", "search", "--", literal], Persona::Outlook)
                .is_none(),
            "escaped positional {literal} must not trigger fast help"
        );
    }

    // Missing option values (including `--help`/`-h` immediately after a value-taking option)
    // or flag-looking tokens must NOT be swallowed as values
    let must_fail: &[&[&str]] = &[
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "--json"],
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "--dry-run"],
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "--help"],
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "-h"],
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "--"],
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "-a"],
        &["outlook", "send", "--to", "a@b", "--subject", "--body", "Hi"],
        &["outlook", "edit-draft", "id", "--body", "--json"],
        &["outlook", "edit-draft", "id", "--body", "--dry-run"],
        &["outlook", "edit-draft", "id", "--body", "--help"],
        &["outlook", "edit-draft", "id", "--body", "-h"],
        &["outlook", "edit-draft", "id", "--body", "--"],
        &["outlook", "edit-draft", "id", "--body", "--subject", "New"],
        &["outlook", "edit-draft", "id", "--subject", "--json"],
        &[
            "outlook",
            "create-event",
            "--subject",
            "Test",
            "--start",
            "2026-12-01T10:00",
            "--end",
            "2026-12-01T11:00",
            "--body",
            "--dry-run",
        ],
        &["outlook", "add-task", "Test", "--notes", "--json"],
        &["outlook", "add-task", "Test", "--body", "--dry-run"],
        &["outlook", "rsvp", "id", "accept", "--comment", "--json"],
        &["outlook", "rsvp", "id", "accept", "--comment", "--dry-run"],
        &["outlook", "update-task", "id", "--title", "--json"],
        &["outlook", "update-task", "id", "--title", "--dry-run"],
        &["outlook", "reply", "id", "--all"],
        &["outlook", "reply", "id", "--json"],
        &["outlook", "reply", "id", "--dry-run"],
        &["outlook", "reply", "id", "-a"],
        &["outlook", "reply", "id", "--dry-run", "- 목록 항목"],
        &["outlook", "forward", "id", "--to", "a@b", "--dry-run"],
        &["outlook", "forward", "id", "--dry-run", "- 목록 항목", "--to", "a@b"],
        &["outlook", "search", "--json"],
        &["outlook", "search", "--remote"],
        &["outlook", "search", "--dry-run"],
        &["outlook", "search", "-r"],
        &["outlook", "search", "--folder", "- 목록 항목", "- 쿼리"],
        &["outlook", "add-task", "--json"],
        &["outlook", "add-task", "--dry-run"],
        // Non-text parsers (IDs, emails, dates, enums, numbers) stay unchanged
        &["outlook", "reply", "- 목록 항목", "body"],
        &["outlook", "rsvp", "id", "- 목록 항목"],
        &["outlook", "send", "--to", "- 목록 항목", "--subject", "Test"],
        &[
            "outlook",
            "create-event",
            "--subject",
            "Test",
            "--start",
            "- 목록 항목",
            "--end",
            "2026-12-01T11:00",
        ],
        &["outlook", "update-task", "- 목록 항목", "--title", "Test"],
        &["outlook", "mail", "--limit", "-1"],
    ];
    for argv in must_fail {
        let err = parse_cli_from(*argv).expect_err(&format!("expected parse failure for {argv:?}"));
        assert!(
            !matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ),
            "unexpected help/version error for {argv:?}: {err}"
        );
    }

    // Actual help invocations (standalone and after a completed hyphen value)
    let help_cases: &[&[&str]] = &[
        &["outlook", "send", "--help"],
        &["outlook", "send", "-h"],
        &["outlook", "send", "--to", "a@b", "--subject", "Test", "--body", "- 목록 항목", "--help"],
        &["outlook", "reply", "id", "- 목록 항목", "--help"],
        &["outlook", "reply", "id", "--help"],
        &["outlook", "reply", "id", "-h"],
        &["outlook", "forward", "id", "- 목록 항목", "--to", "a@b", "--help"],
        &["outlook", "edit-draft", "--help"],
        &["outlook", "edit-draft", "-h"],
        &["outlook", "edit-draft", "id", "--body", "- 목록 항목", "--help"],
        &[
            "outlook",
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
        &["outlook", "add-task", "Test", "--notes", "- 목록 항목", "--help"],
        &["outlook", "rsvp", "id", "accept", "--comment", "- 목록 항목", "--help"],
        &["outlook", "send", "--to", "a@b", "--subject", "- 목록 항목", "--help"],
        &["outlook", "update-task", "id", "--title", "- 목록 항목", "--help"],
        &["outlook", "search", "- 목록 항목", "--help"],
        &["outlook", "search", "--help"],
        &["outlook", "search", "-h"],
    ];
    for argv in help_cases {
        let err = parse_cli_from(*argv).expect_err(&format!("expected help for {argv:?}"));
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelp,
            "expected DisplayHelp for {argv:?}, got {err}"
        );
        assert!(
            try_fast_help_or_version_from(*argv, Persona::Outlook).is_some(),
            "fast help must handle {argv:?}"
        );
    }
}

#[test]
fn normal_commands_skip_rewrite_tree_and_preserve_bare_dash() {
    let to_os = |items: &[&str]| -> Vec<std::ffi::OsString> {
        items.iter().map(std::ffi::OsString::from).collect()
    };

    // Normal commands, attached `=`, `--` escaped positionals, flags, and bare `-`
    // bypass the hyphen-rewrite inspection and parse in a single clap Command tree.
    assert!(!has_potential_hyphen_text(&to_os(&[
        "--folder", "inbox", "--limit", "30", "--json"
    ])));
    assert!(!has_potential_hyphen_text(&to_os(&[
        "--to",
        "audit@example.invalid",
        "--subject",
        "Test",
        "--body",
        "Hello world",
        "--json",
    ])));
    assert!(!has_potential_hyphen_text(&to_os(&[
        "--body=- 목록 항목",
        "--json"
    ])));
    assert!(!has_potential_hyphen_text(&to_os(&[
        "id",
        "--",
        "- 목록 항목"
    ])));
    assert!(!has_potential_hyphen_text(&to_os(&[
        "id",
        "--body",
        "--dry-run"
    ])));
    assert!(!has_potential_hyphen_text(&to_os(&["--help"])));
    assert!(!has_potential_hyphen_text(&to_os(&["-h"])));
    assert!(!has_potential_hyphen_text(&to_os(&["--body", "-"])));

    // Separate hyphen-prefixed free text triggers the single-tree rewrite path.
    assert!(has_potential_hyphen_text(&to_os(&["--body", "- 목록 항목"])));
    assert!(has_potential_hyphen_text(&to_os(&["id", "- first\n- second"])));
    assert!(has_potential_hyphen_text(&to_os(&["---"])));
    assert!(has_potential_hyphen_text(&to_os(&["-- note"])));
    assert!(has_potential_hyphen_text(&to_os(&["--limit", "-1"])));

    // Bare `-` still parses cleanly through the fast path on both options and positionals.
    let cli = parse_cli_from([
        "outlook",
        "send",
        "--to",
        "audit@example.invalid",
        "--subject",
        "Test",
        "--body",
        "-",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Send {
            body: Some(body),
            ..
        } if body == "-"
    ));
    let cli = parse_cli_from(["outlook", "search", "-"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Search { query, .. } if query == "-"
    ));
}

#[test]
fn page_cursor_parses_each_prefix_and_rejects_garbage() {
    let c = PageCursor::parse("c:4f5129f1:2026-11-20T09:00:00Z|msg-1").unwrap();
    assert_eq!(c.scope, "4f5129f1");
    assert_eq!(
        c.pos,
        CursorPos::Cache {
            received_at: "2026-11-20T09:00:00Z".into(),
            id: "msg-1".into(),
        }
    );

    let c_pipe = PageCursor::parse("c:4f5129f1:2026-11-20T09:00:00Z|a|b|c").unwrap();
    assert_eq!(
        c_pipe.pos,
        CursorPos::Cache {
            received_at: "2026-11-20T09:00:00Z".into(),
            id: "a|b|c".into(),
        }
    );

    let g_url = "http://127.0.0.1:53443/v1.0/me/messages?$skiptoken=a:b|c";
    let g = PageCursor::parse(&format!("g:99e80d3c:12|{g_url}")).unwrap();
    assert_eq!(g.scope, "99e80d3c");
    assert_eq!(
        g.pos,
        CursorPos::Graph {
            skip: 12,
            url: g_url.into(),
        }
    );

    let o = PageCursor::parse("o:f74800ea:50").unwrap();
    assert_eq!(o.scope, "f74800ea");
    assert_eq!(o.pos, CursorPos::Offset(50));

    for (bad, expected_msg) in [
        ("", "invalid cursor: malformed"),
        ("c:a", "invalid cursor: malformed"),
        ("c:2026-11-20T09:00:00Z|m", "invalid cursor: malformed"),
        ("c:4f5129f:2026-11-20T09:00:00Z|m", "invalid cursor: malformed"),
        ("c:4f5129fz:2026-11-20T09:00:00Z|m", "invalid cursor: malformed"),
        ("c:4f5129f1:|m", "invalid cursor: malformed"),
        ("c:4f5129f1:2026-11-20T09:00:00Z|", "invalid cursor: malformed"),
        ("g:99e80d3c:x|u", "invalid cursor: malformed"),
        ("g:99e80d3c:0|", "invalid cursor: malformed"),
        ("o:f74800ea:-1", "invalid cursor: malformed"),
        ("z:f74800ea:1", "invalid cursor: unknown prefix"),
    ] {
        let err = PageCursor::parse(bad)
            .expect_err(&format!("expected failure for {bad:?}"))
            .to_string();
        assert_eq!(err, expected_msg, "input {bad:?}");
    }
}

#[test]
fn cursor_scope_is_deterministic_and_separates_parts() {
    assert_eq!(cursor_scope(&["mail", "folder-inbox", "000"]), "4f5129f1");
    assert_eq!(cursor_scope(&["mail", "folder-inbox", "100"]), "d0b9c3c6");
    assert_eq!(cursor_scope(&["folders"]), "f74800ea");
    assert_eq!(cursor_scope(&["task-lists"]), "14525d78");
    assert_eq!(cursor_scope(&["search", "", "Architecture"]), "99e80d3c");
    assert_eq!(cursor_scope(&["search", "", "Budget"]), "714ee75a");
    assert_eq!(cursor_scope(&["search", "", "a"]), "1d3b4932");
    assert_ne!(
        cursor_scope(&["mail", "ab", "c"]),
        cursor_scope(&["mail", "a", "bc"])
    );
    assert_ne!(
        mail_cursor_scope("folder-inbox", false, false, false),
        mail_cursor_scope("folder-inbox", true, false, false)
    );
    assert_ne!(
        mail_cursor_scope("folder-inbox", false, false, false),
        mail_cursor_scope("folder-inbox", false, true, false)
    );
    assert_ne!(
        mail_cursor_scope("folder-inbox", false, false, false),
        mail_cursor_scope("folder-inbox", false, false, true)
    );
}

#[test]
fn require_scope_rejects_mismatch() {
    let c = PageCursor::parse("o:F74800EA:50").unwrap();
    c.require_scope("f74800ea").unwrap();
    let err = c.require_scope("14525d78").unwrap_err().to_string();
    assert_eq!(
        err,
        "invalid cursor: issued for a different command, folder, query or filter"
    );
}

#[test]
fn offset_page_boundaries() {
    let items: Vec<i32> = (0..5).collect();
    let (p0, more0, cur0) = offset_page(&items, 0, 2, "f74800ea");
    assert_eq!(p0, vec![0, 1]);
    assert!(more0);
    assert_eq!(cur0.as_deref(), Some("o:f74800ea:2"));

    let (p1, more1, cur1) = offset_page(&items, 2, 3, "f74800ea");
    assert_eq!(p1, vec![2, 3, 4]);
    assert!(!more1);
    assert_eq!(cur1, None);

    let (p_end, more_end, cur_end) = offset_page(&items, 5, 2, "f74800ea");
    assert!(p_end.is_empty());
    assert!(!more_end);
    assert_eq!(cur_end, None);

    let (p_over, more_over, cur_over) = offset_page(&items, 99, 2, "f74800ea");
    assert!(p_over.is_empty());
    assert!(!more_over);
    assert_eq!(cur_over, None);
}

#[test]
fn fast_storage_path_never_borrows_another_accounts_cache_while_signed_in() {
    let data = tempfile::tempdir().unwrap();
    let conf = tempfile::tempdir().unwrap();
    let cfg = AppConfig::default();
    let other_db = data.path().join("someone-else").join("cache.db");
    std::fs::create_dir_all(other_db.parent().unwrap()).unwrap();
    std::fs::write(&other_db, b"").unwrap();
    let cred_path = conf.path().join("credentials.json");

    // Nothing on disk says who is signed in: the single local cache is used (unchanged).
    assert_eq!(
        resolve_storage_db_path_fast(&cfg, None, data.path(), &cred_path),
        Some(other_db.clone())
    );

    // A credentials file without a readable user id: the `default` bucket, not the other account.
    std::fs::write(&cred_path, br#"{"access_token":"not-a-jwt"}"#).unwrap();
    assert_eq!(
        resolve_storage_db_path_fast(&cfg, None, data.path(), &cred_path),
        Some(data.path().join("default").join("cache.db"))
    );

    // A credentials file that names its user: that user's bucket.
    std::fs::write(&cred_path, br#"{"user_id":"user-b"}"#).unwrap();
    assert_eq!(
        resolve_storage_db_path_fast(&cfg, None, data.path(), &cred_path),
        Some(data.path().join("user-b").join("cache.db"))
    );
}

#[test]
fn error_code_classifies_documented_failures() {
    assert_eq!(
        error_code(&anyhow::anyhow!("Ambiguous mail folder 'Reports' matches 2 folders")),
        "ambiguous"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!(
            "Graph API error (GET https://graph.microsoft.com/v1.0/me/messages/x): 404 Not Found [ErrorItemNotFound]"
        )),
        "not_found"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!("Mail message not found: Weekly Sync")),
        "not_found"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!(
            "Insufficient Graph permissions (403): access restricted"
        )),
        "forbidden"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!("Sign-in required. Run `outlook login`.")),
        "auth_required"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!(
            "Graph API error (GET https://graph.microsoft.com/v1.0/me/messages): 401 Unauthorized [InvalidAuthenticationToken]"
        )),
        "auth_required"
    );
    assert_eq!(
        error_code(&error_with_info(
            "token_expired",
            "Token expired (exp: 2026-01-01T00:00:00Z) — refresh the token and retry.",
            Some(json!({ "expired": true })),
        )),
        "token_expired"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!("{DELETE_JSON_REQUIRES_YES}")),
        "confirmation_required"
    );
    assert_eq!(
        error_code(&error_with_info(
            "invalid_recipient",
            "Recipient 'Alex Kim <>' has an empty address.",
            None,
        )),
        "invalid_recipient"
    );
    assert_eq!(
        error_code(&anyhow::anyhow!("Message 'm1' is not an Outlook draft.")),
        "error"
    );

    let payload = json_error_payload(&error_with_info(
        "token_expired",
        "Token expired",
        Some(json!({ "auth_mode": "env_token" })),
    ));
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error"]["code"], "token_expired");
    assert_eq!(payload["error"]["details"]["auth_mode"], "env_token");

    let plain_payload = json_error_payload(&anyhow::anyhow!("boom"));
    assert_eq!(plain_payload["ok"], false);
    assert_eq!(plain_payload["error"]["code"], "error");
    assert!(plain_payload["error"].get("details").is_none());
}

#[test]
fn check_recipient_addresses_rejects_only_empty_angle_brackets() {
    for ok_case in [
        vec![],
        vec!["a@b.com".to_string()],
        vec!["\"Bob\" <bob@x.com>".to_string()],
        vec!["carol".to_string()],
        vec!["Carol Organizer".to_string()],
        vec!["dir-carol".to_string()],
        vec!["".to_string(), "   ".to_string()],
    ] {
        assert!(
            check_recipient_addresses(&ok_case).is_ok(),
            "expected {ok_case:?} to be accepted"
        );
    }

    for bad_case in ["Alex Kim <>", "<>", "Alex Kim <   >"] {
        let err = check_recipient_addresses(&[bad_case.to_string()])
            .expect_err(&format!("expected {bad_case:?} to fail"));
        assert_eq!(error_code(&err), "invalid_recipient");
        assert!(err.to_string().contains("has an empty address"));
    }
}

#[test]
fn format_graph_error_appends_hint_only_for_invalid_recipients() {
    let raw = "Graph API error (POST https://graph.microsoft.com/v1.0/me/sendMail): 400 Bad Request [ErrorInvalidRecipients] One or more recipients are invalid.";
    let formatted = format_graph_error(anyhow::anyhow!("{raw}"), None);
    let text = format!("{formatted:#}");
    assert!(text.starts_with(raw));
    assert!(text.contains("\nHint: recipients must be SMTP addresses"));
    assert_eq!(error_code(&formatted), error_code(&anyhow::anyhow!("{raw}")));

    let raw_404 = "Graph API error (GET https://graph.microsoft.com/v1.0/me/messages/missing): 404 Not Found [ErrorItemNotFound]";
    let formatted_404 = format_graph_error(anyhow::anyhow!("{raw_404}"), None);
    assert_eq!(format!("{formatted_404:#}"), raw_404);
    assert_eq!(error_code(&formatted_404), "not_found");
}
