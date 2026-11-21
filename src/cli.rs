//! `outlook` CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::json;

use crate::{
    auth::{
        diagnose_access_token_scopes, explain_graph_permission_error, read_external_token_file,
        AuthManager, LoginOptions, StoredTokens,
    },
    config::{self, AppConfig, Persona},
    graph::{calendar::CalendarEditTime, GraphClient},
    img_preview,
    models::{
        CalendarEvent, ContactItem, InlineImageUpload, MailAttachment, MailFolder, MailMessage,
        OutgoingEvent, OutgoingMail, OutgoingTodoTask, SyncCursor, TodoList, TodoTask,
    },
    resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    tui, updater,
};

const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

const ENV_AFTER_HELP: &str = "\
Environment Variables:
  OUTLOOK_TOKEN_FILE=<PATH>             Load an external token file (JWT or JSON) directly (skips OS keyring)
  OUTLOOK_TOKEN_REFRESH_CMD=<CMD>       Command hook to run automatically when the token is about to expire
  OUTLOOK_TOKEN_REFRESH_MINS=<MINS>     Minutes remaining before triggering refresh hook [default: 5]
  OUTLOOK_ACCESS_TOKEN=<JWT>            Inject a Bearer access token directly (skips OS keyring)
  OUTLOOK_SHARE_M365_AUTH=<1|0>         Reuse stored teams-cli / TEAMS_TOKEN_FILE session when no outlook-cli token exists [default: 1]
  OUTLOOK_DISABLE_KEYRING=1             Disable the OS keyring (use credentials.json file only)
  OUTLOOK_CLIENT_ID / OUTLOOK_TENANT_ID Override Entra app Client ID / Tenant ID
  OUTLOOK_CONFIG_DIR / OUTLOOK_DATA_DIR Override config and local SQLite cache directories
  OUTLOOK_CLI_CACHE_SECS=<SECS>         Local cache TTL in seconds for CLI read commands [default: 0; --cached/--fast prefers cache]
  OUTLOOK_AUTO_DAEMON=1                 Auto-start/connect to background sync daemon (config.toml: auto_daemon = true) [default: false]
  OUTLOOK_IMAGE_PROTOCOL=<PROTO>        Terminal image protocol (auto, kitty, iterm2, sixel, halfblock)

Background Sync Daemon:
  outlook daemon start | status | stop | restart";

#[derive(Parser, Debug)]
#[command(
    name = "outlook",
    version,
    about = "Microsoft Outlook terminal client (CLI · TUI · HTTP API · MCP)",
    long_about = "Terminal client for Microsoft Outlook (Mail, Calendar, To Do, Contacts) powered by Microsoft Graph v1.0.\n\
                  Running without a subcommand checks authentication and launches the interactive TUI.",
    after_help = ENV_AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in with Microsoft Entra ID (OAuth 2.0) and store tokens
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        tenant_id: Option<String>,
        #[arg(long)]
        device_code: bool,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long, visible_alias = "token")]
        access_token: Option<String>,
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored authentication tokens
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Display the currently signed-in user profile, token expiry, scopes, and auth source
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Show config/DB/keyring paths, image protocol, and optional live Graph diagnostics
    Doctor {
        /// Probe live Graph connectivity across Mail, Calendar, Contacts, and To Do
        #[arg(long, visible_alias = "probe")]
        online: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Outlook mail folders with unread and total item counts
    Folders {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        /// Read from local SQLite cache first (skips network requests when populated)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// List messages in Inbox or a specified mail folder
    #[command(visible_aliases = ["ls", "inbox"])]
    Mail {
        /// Folder name, well-known alias (inbox/sent/drafts/archive/trash/junk), or ID
        #[arg(long, default_value = "inbox")]
        folder: String,
        /// Maximum number of messages to fetch
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        flagged: bool,
        #[arg(long)]
        focused: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        /// Read from local SQLite cache first (falls back to network if empty)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
        /// With --json, return only list fields for each message
        #[arg(long, requires = "json")]
        compact: bool,
    },
    /// Read a mail message with full headers, body, and attachments
    #[command(visible_alias = "cat")]
    Read {
        message_id: String,
        /// Print raw HTML body instead of plain text / Markdown
        #[arg(long = "raw-html")]
        raw_html: bool,
        /// Mark the message as read on Microsoft Graph
        #[arg(long = "mark-read")]
        mark_read: bool,
        /// Render image attachments directly in the terminal
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        /// Read from local SQLite cache first
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
        /// With --json, return mail headers and the complete plain-text body
        #[arg(long, requires = "json", conflicts_with_all = ["raw_html", "preview_images"])]
        compact: bool,
    },
    /// Compose and send a new email (supports Markdown, attachments, and inline images)
    #[command(visible_alias = "compose")]
    Send {
        /// Recipient email address (repeatable)
        #[arg(long = "to", required = true, value_name = "ADDR")]
        to: Vec<String>,
        /// CC email address (repeatable)
        #[arg(long = "cc", value_name = "ADDR")]
        cc: Vec<String>,
        /// BCC email address (repeatable)
        #[arg(long = "bcc", value_name = "ADDR")]
        bcc: Vec<String>,
        /// Email subject line
        #[arg(long, required = true, value_name = "SUBJ")]
        subject: String,
        /// Markdown email body (reads from stdin when omitted)
        #[arg(long = "body", value_name = "MD")]
        body: Option<String>,
        /// File attachment path (repeatable)
        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<PathBuf>,
        /// Inline image path embedded via CID (repeatable)
        #[arg(long = "inline", value_name = "IMG_PATH")]
        inline: Vec<PathBuf>,
        /// Message importance (normal, high, low)
        #[arg(long, default_value = "normal", value_parser = ["normal", "high", "low"])]
        importance: String,
        #[arg(long)]
        json: bool,
    },
    /// Reply to a mail message
    Reply {
        message_id: String,
        body_md: String,
        /// Reply to all recipients
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Forward a mail message to one or more recipients
    Forward {
        message_id: String,
        #[arg(long = "to", required = true, value_name = "ADDR")]
        to: Vec<String>,
        #[arg(value_name = "COMMENT_MD")]
        comment_md: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Edit an existing Outlook draft
    #[command(group(clap::ArgGroup::new("draft_edit").args(["subject", "body", "body_html"]).multiple(true).required(true)))]
    EditDraft {
        message_id: String,
        #[arg(long)]
        subject: Option<String>,
        /// Replace the body with rendered Markdown
        #[arg(long, conflicts_with = "body_html")]
        body: Option<String>,
        /// Replace the body with the supplied HTML
        #[arg(long)]
        body_html: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update read/unread or flag status on a mail message
    Mark {
        message_id: String,
        #[arg(long, conflicts_with = "unread")]
        read: bool,
        #[arg(long, conflicts_with = "read")]
        unread: bool,
        #[arg(long, conflicts_with = "unflag")]
        flag: bool,
        #[arg(long, conflicts_with = "flag")]
        unflag: bool,
        #[arg(long)]
        json: bool,
    },
    /// Move a mail message to another folder (accepts folder name, alias, or ID)
    Move {
        message_id: String,
        dest_folder: String,
        #[arg(long)]
        json: bool,
    },
    /// Archive a mail message (shortcut for `outlook move <MESSAGE_ID> archive`)
    Archive {
        message_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete a mail message
    #[command(visible_alias = "rm")]
    Delete {
        message_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search messages in local SQLite FTS5 or remotely via Graph KQL `$search`
    Search {
        query: String,
        #[arg(long)]
        folder: Option<String>,
        /// Query Microsoft Graph KQL `$search` in addition to local cache
        #[arg(long)]
        remote: bool,
        /// Maximum number of results to return
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Download a mail attachment to disk
    #[command(visible_alias = "dl")]
    Download {
        message_id: String,
        /// Attachment 0-based index, attachment ID, or filename
        #[arg(long, value_name = "IDX_OR_NAME")]
        attachment: Option<String>,
        /// Output directory (defaults to configured download_dir)
        #[arg(long, visible_alias = "out", value_name = "DIR")]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Render a mail image attachment directly in the terminal
    Preview {
        message_id: String,
        /// Attachment 0-based index, attachment ID, or filename
        #[arg(long, value_name = "IDX_OR_NAME")]
        attachment: Option<String>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        /// Image rendering protocol override (auto, kitty, iterm2, sixel, halfblock)
        #[arg(long, value_name = "PROTO")]
        protocol: Option<String>,
    },
    /// Render a local image file in the terminal
    #[command(name = "preview-file")]
    PreviewFile {
        image_path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        /// Image rendering protocol override (auto, kitty, iterm2, sixel, halfblock)
        #[arg(long, value_name = "PROTO")]
        protocol: Option<String>,
    },
    /// List upcoming calendar events
    #[command(visible_aliases = ["cal", "events"])]
    Calendar {
        /// Number of days to show starting from `--from` (or today)
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// Window start timestamp (ISO-8601 or YYYY-MM-DD)
        #[arg(long, visible_alias = "start", value_name = "ISO")]
        from: Option<String>,
        /// Window end timestamp (ISO-8601 or YYYY-MM-DD)
        #[arg(long, visible_alias = "end", value_name = "ISO")]
        to: Option<String>,
        /// Maximum number of events to return
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read full details and attendees of a calendar event
    Event {
        event_id: String,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new calendar event
    #[command(name = "create-event")]
    CreateEvent {
        #[arg(long, required = true)]
        subject: String,
        #[arg(long, required = true, value_name = "ISO")]
        start: String,
        #[arg(long, required = true, value_name = "ISO")]
        end: String,
        #[arg(long)]
        location: Option<String>,
        #[arg(long = "attendee", visible_alias = "to", value_name = "EMAIL")]
        attendee: Vec<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "online", visible_alias = "teams-meeting")]
        online: bool,
        #[arg(long = "all-day")]
        all_day: bool,
        #[arg(long)]
        time_zone: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update the subject or times of an existing calendar event
    #[command(group(clap::ArgGroup::new("event_edit").args(["subject", "start", "end"]).multiple(true).required(true)))]
    UpdateEvent {
        event_id: String,
        #[arg(long, visible_alias = "title")]
        subject: Option<String>,
        /// RFC3339, wall-clock time in the event's zone, or an all-day date
        #[arg(long)]
        start: Option<String>,
        /// RFC3339, wall-clock time in the event's zone, or an exclusive all-day end date
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Respond (RSVP) to a calendar event invitation (`accept`, `tentative`, `decline`)
    Rsvp {
        event_id: String,
        response: String,
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a calendar event
    #[command(name = "delete-event")]
    DeleteEvent {
        event_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Microsoft To Do tasks
    #[command(visible_alias = "todo")]
    Tasks {
        #[arg(long, default_value = "tasks", value_name = "NAME_OR_ID")]
        list: String,
        /// Include completed tasks
        #[arg(long)]
        all: bool,
        /// Maximum number of tasks to return
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Microsoft To Do task lists
    #[command(name = "task-lists")]
    TaskLists {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Add a new Microsoft To Do task
    #[command(name = "add-task")]
    AddTask {
        title: String,
        #[arg(long, default_value = "tasks", value_name = "NAME_OR_ID")]
        list: String,
        #[arg(long, value_name = "YYYY-MM-DD")]
        due: Option<String>,
        #[arg(long, default_value = "normal", value_parser = ["normal", "high", "low"])]
        importance: String,
        #[arg(long, visible_alias = "body", value_name = "TEXT")]
        notes: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Mark a Microsoft To Do task as completed (or incomplete with `--undo`)
    Done {
        task_id: String,
        #[arg(long, default_value = "tasks", value_name = "NAME_OR_ID")]
        list: String,
        #[arg(long)]
        undo: bool,
        #[arg(long)]
        json: bool,
    },
    /// Update an existing Microsoft To Do task's title or due date
    #[command(group(clap::ArgGroup::new("task_edit").args(["title", "due"]).multiple(true).required(true)))]
    UpdateTask {
        task_id: String,
        #[arg(long, default_value = "tasks", value_name = "NAME_OR_ID")]
        list: String,
        #[arg(long)]
        title: Option<String>,
        /// New due date (YYYY-MM-DD at midnight UTC, or RFC3339)
        #[arg(long)]
        due: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a Microsoft To Do task
    #[command(name = "delete-task")]
    DeleteTask {
        task_id: String,
        #[arg(long, default_value = "tasks", value_name = "NAME_OR_ID")]
        list: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// List or search personal Outlook contacts and org directory
    Contacts {
        #[arg(long, visible_alias = "q", value_name = "Q")]
        query: Option<String>,
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Perform a full sync of mail folders, inbox, calendar, tasks, and contacts to SQLite
    Sync {
        /// Calendar lookahead window in days
        #[arg(long, default_value_t = 14)]
        days: i64,
        /// Maximum items per domain to sync
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Check for or apply updates from GitHub Releases
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Launch the interactive terminal UI (TUI)
    Tui,
    /// Start the local HTTP JSON API server for agents
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = config::DEFAULT_SERVE_PORT)]
        port: u16,
        /// Secret Bearer API key (mandatory when binding to a non-loopback address)
        #[arg(long = "api-key", visible_alias = "token", value_name = "KEY")]
        api_key: Option<String>,
    },
    /// Start the stdio MCP (JSON-RPC 2.0) server for agents
    #[command(visible_alias = "mcp-server")]
    Mcp,
    /// Control the background sync daemon (`start` | `stop` | `status` | `restart` | `run`)
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the background sync daemon (returns status if already running)
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Stop the running background sync daemon
    Stop {
        #[arg(long)]
        json: bool,
    },
    /// Query the background sync daemon status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Restart the background sync daemon
    Restart {
        #[arg(long)]
        json: bool,
    },
    /// Run the background sync daemon in the foreground (internal use)
    Run,
}

fn hyphen_text_arg_ids(subcommand: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match subcommand {
        "send" => (&["subject", "body"], &[]),
        "reply" => (&[], &["body_md"]),
        "forward" => (&[], &["comment_md"]),
        "edit-draft" => (&["subject", "body"], &[]),
        "search" => (&[], &["query"]),
        "create-event" => (&["subject", "location", "body"], &[]),
        "update-event" => (&["subject"], &[]),
        "rsvp" => (&["comment"], &[]),
        "add-task" => (&["notes"], &["title"]),
        "update-task" => (&["title"], &[]),
        "contacts" => (&["query"], &[]),
        _ => (&[], &[]),
    }
}

fn starts_with_hyphen(arg: &std::ffi::OsStr) -> bool {
    arg.as_encoded_bytes().starts_with(b"-")
}

fn is_generic_flag_like_token(arg: &std::ffi::OsStr) -> bool {
    let Some(s) = arg.to_str() else {
        return false;
    };
    if s == "--" {
        return true;
    }
    if let Some(rest) = s.strip_prefix("--") {
        let name = rest.split_once('=').map_or(rest, |(k, _)| k);
        return !name.is_empty()
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    if let Some(rest) = s.strip_prefix('-') {
        if rest.is_empty() {
            return false;
        }
        if rest.len() == 1 && rest.as_bytes()[0].is_ascii_alphabetic() {
            return true;
        }
        if rest.chars().all(|ch| ch == 'h' || ch == 'V') {
            return true;
        }
    }
    false
}

fn is_flag_like_token(arg: &std::ffi::OsStr, cmd: &clap::Command) -> bool {
    if is_generic_flag_like_token(arg) {
        return true;
    }
    if let Some(rest) = arg.to_str().and_then(|s| s.strip_prefix('-')) {
        if !rest.is_empty() && !rest.starts_with('-') {
            return rest.chars().all(|ch| {
                ch == 'h' || ch == 'V' || cmd.get_arguments().any(|a| a.get_short() == Some(ch))
            });
        }
    }
    false
}

fn has_potential_hyphen_text(args: &[std::ffi::OsString]) -> bool {
    for tok in args {
        if tok == "--" {
            break;
        }
        if starts_with_hyphen(tok) && tok != "-" && !is_generic_flag_like_token(tok) {
            return true;
        }
    }
    false
}

fn parse_with_cmd(
    mut cmd: clap::Command,
    args: &[std::ffi::OsString],
) -> std::result::Result<Cli, clap::Error> {
    let matches = cmd.try_get_matches_from_mut(args)?;
    Cli::from_arg_matches(&matches).map_err(|err| err.format(&mut cmd))
}

fn find_long_arg<'a>(sub: &'a clap::Command, name: &str) -> Option<&'a clap::Arg> {
    sub.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn is_known_subcommand_short_flag(arg: &std::ffi::OsStr, sub: &clap::Command) -> bool {
    let Some(rest) = arg.to_str().and_then(|s| s.strip_prefix('-')) else {
        return false;
    };
    !rest.is_empty()
        && !rest.starts_with('-')
        && rest
            .chars()
            .all(|ch| ch == 'h' || sub.get_arguments().any(|a| a.get_short() == Some(ch)))
}

/// Parses `outlook` CLI arguments while allowing separate hyphen-prefixed text literals
/// (such as `"- 목록 항목"` or `"- first\n- second"`) for free-text options and positionals,
/// without swallowing real flags, `--help`/`-h`, `--`, or flag-like tokens such as `--dry-run`.
pub fn parse_cli_from<I, S>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let raw: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    if raw.len() <= 2 || !has_potential_hyphen_text(&raw[2..]) {
        return Cli::try_parse_from(&raw);
    }

    let Some(first_sub) = raw[1].to_str() else {
        return Cli::try_parse_from(&raw);
    };

    let mut cmd = <Cli as CommandFactory>::command();
    let rewrite = {
        cmd.get_subcommands()
            .find(|sc| sc.get_name() == first_sub || sc.get_all_aliases().any(|a| a == first_sub))
            .and_then(|sub| {
                let (affected_opts, affected_pos) = hyphen_text_arg_ids(sub.get_name());
                if affected_opts.is_empty() && affected_pos.is_empty() {
                    return None;
                }

                let mut rewritten = Vec::with_capacity(raw.len());
                rewritten.push(raw[0].clone());
                rewritten.push(raw[1].clone());

                let mut enable_pos_hyphen: Vec<&'static str> = Vec::new();
                let mut block_pos_hyphen = false;
                let mut pos_index: usize = 1;
                let mut i = 2usize;

                while i < raw.len() {
                    let tok = &raw[i];
                    if tok == "--" {
                        rewritten.extend_from_slice(&raw[i..]);
                        break;
                    }

                    if let Some(long_rest) = tok.to_str().and_then(|s| s.strip_prefix("--")) {
                        if let Some((name, _)) = long_rest.split_once('=') {
                            if find_long_arg(sub, name).is_some() {
                                rewritten.push(tok.clone());
                                i += 1;
                                continue;
                            }
                            if is_flag_like_token(tok, sub) {
                                block_pos_hyphen = true;
                                rewritten.push(tok.clone());
                                i += 1;
                                continue;
                            }
                        } else if long_rest == "help" {
                            rewritten.push(tok.clone());
                            i += 1;
                            continue;
                        } else if let Some(arg) = find_long_arg(sub, long_rest) {
                            if arg.get_action().takes_values() {
                                if let Some(next) = raw.get(i + 1) {
                                    let arg_id = arg.get_id().as_str();
                                    if affected_opts.contains(&arg_id) {
                                        if starts_with_hyphen(next)
                                            && !is_flag_like_token(next, sub)
                                        {
                                            let mut attached = std::ffi::OsString::with_capacity(
                                                tok.len() + 1 + next.len(),
                                            );
                                            attached.push(tok);
                                            attached.push("=");
                                            attached.push(next);
                                            rewritten.push(attached);
                                            i += 2;
                                            continue;
                                        }
                                        if starts_with_hyphen(next) {
                                            block_pos_hyphen = true;
                                            rewritten.push(tok.clone());
                                            i += 1;
                                            continue;
                                        }
                                        rewritten.push(tok.clone());
                                        rewritten.push(next.clone());
                                        i += 2;
                                        continue;
                                    }
                                    if starts_with_hyphen(next) {
                                        block_pos_hyphen = true;
                                        rewritten.push(tok.clone());
                                        i += 1;
                                        continue;
                                    }
                                    rewritten.push(tok.clone());
                                    rewritten.push(next.clone());
                                    i += 2;
                                    continue;
                                }
                                block_pos_hyphen = true;
                                rewritten.push(tok.clone());
                                i += 1;
                                continue;
                            }
                            rewritten.push(tok.clone());
                            i += 1;
                            continue;
                        } else if is_flag_like_token(tok, sub) {
                            block_pos_hyphen = true;
                            rewritten.push(tok.clone());
                            i += 1;
                            continue;
                        }
                    } else if starts_with_hyphen(tok) && tok != "-" && is_flag_like_token(tok, sub)
                    {
                        if !is_known_subcommand_short_flag(tok, sub) {
                            block_pos_hyphen = true;
                        }
                        rewritten.push(tok.clone());
                        i += 1;
                        continue;
                    }

                    if starts_with_hyphen(tok) && tok != "-" && !block_pos_hyphen {
                        if let Some(pos_arg) = sub.get_positionals().nth(pos_index - 1) {
                            if let Some(&id) = affected_pos
                                .iter()
                                .find(|&&id| id == pos_arg.get_id().as_str())
                            {
                                if !enable_pos_hyphen.contains(&id) {
                                    enable_pos_hyphen.push(id);
                                }
                            }
                        }
                    }
                    rewritten.push(tok.clone());
                    pos_index += 1;
                    i += 1;
                }

                Some((sub.get_name().to_string(), rewritten, enable_pos_hyphen))
            })
    };

    let Some((sub_name, rewritten, enable_pos_hyphen)) = rewrite else {
        return parse_with_cmd(cmd, &raw);
    };

    if !enable_pos_hyphen.is_empty() {
        cmd = cmd.mut_subcommand(&sub_name, |mut sc| {
            for arg_id in &enable_pos_hyphen {
                sc = sc.mut_arg(*arg_id, |arg| arg.allow_hyphen_values(true));
            }
            sc
        });
    }

    parse_with_cmd(cmd, &rewritten)
}

/// Synchronously handles `--help`, `-h`, `--version`, `-V`, and `help` before Tokio/rustls/Keyring/SQLite init.
pub fn try_fast_help_or_version() -> Option<Result<()>> {
    try_fast_help_or_version_from(std::env::args_os(), Persona::detect())
}

fn try_fast_help_or_version_from<I, S>(args: I, persona: Persona) -> Option<Result<()>>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let raw: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let mut after_bin = raw.iter().skip(1);
    let first = after_bin.next()?;
    let has_help_or_version = first.to_str() == Some("help")
        || std::iter::once(first)
            .chain(after_bin)
            .take_while(|arg| arg.to_str() != Some("--"))
            .any(|arg| matches!(arg.to_str(), Some("--help" | "-h" | "--version" | "-V")));

    if !has_help_or_version {
        return None;
    }

    let err = match persona {
        Persona::Outlook => parse_cli_from(&raw).err()?,
    };
    if matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    ) {
        let _ = err.print();
        return Some(Ok(()));
    }
    None
}

pub async fn run_from_env() -> Result<()> {
    let persona = Persona::detect();
    let cli = parse_cli_from(std::env::args_os()).unwrap_or_else(|err| err.exit());
    let mut cfg = AppConfig::load();
    Box::pin(dispatch_command(&mut cfg, persona, cli.command)).await
}

async fn dispatch_command(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<Command>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Login {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            access_token,
            token_file,
            port,
            json,
        }) => {
            let resolved_access_token = match (access_token, token_file) {
                (Some(tok), _) => Some(tok),
                (None, Some(path)) => {
                    let loaded = read_external_token_file(&path, cfg)?;
                    Some(loaded.access_token)
                }
                (None, None) => None,
            };
            Box::pin(cmd_login(
                cfg,
                LoginOptions {
                    client_id,
                    tenant_id,
                    device_code,
                    client_secret,
                    access_token: resolved_access_token,
                    port,
                },
                json,
            ))
            .await
        }
        Some(Command::Logout { json }) => Box::pin(cmd_logout(cfg, json)).await,
        Some(Command::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(Command::Doctor { online, json }) => {
            Box::pin(cmd_doctor(cfg, persona, online, json)).await
        }
        Some(Command::Folders {
            refresh,
            offline,
            cached,
            json,
        }) => Box::pin(cmd_folders(cfg, refresh, offline, cached, json)).await,
        Some(Command::Mail {
            folder,
            limit,
            unread,
            flagged,
            focused,
            refresh,
            offline,
            cached,
            json,
            compact,
        }) => {
            Box::pin(cmd_mail_list(
                cfg, &folder, limit, unread, flagged, focused, refresh, offline, cached, json,
                compact,
            ))
            .await
        }
        Some(Command::Read {
            message_id,
            raw_html,
            mark_read,
            preview_images,
            offline,
            cached,
            json,
            compact,
        }) => {
            Box::pin(cmd_mail_read(
                cfg,
                &message_id,
                raw_html,
                mark_read,
                preview_images,
                offline,
                cached,
                json,
                compact,
            ))
            .await
        }
        Some(Command::Send {
            to,
            cc,
            bcc,
            subject,
            body,
            attach,
            inline,
            importance,
            json,
        }) => {
            Box::pin(cmd_mail_send(
                cfg,
                to,
                cc,
                bcc,
                &subject,
                body,
                attach,
                inline,
                &importance,
                json,
            ))
            .await
        }
        Some(Command::Reply {
            message_id,
            body_md,
            all,
            json,
        }) => Box::pin(cmd_mail_reply(cfg, &message_id, &body_md, all, json)).await,
        Some(Command::Forward {
            message_id,
            to,
            comment_md,
            json,
        }) => {
            Box::pin(cmd_mail_forward(
                cfg,
                &message_id,
                to,
                comment_md.as_deref().unwrap_or(""),
                json,
            ))
            .await
        }
        Some(Command::EditDraft {
            message_id,
            subject,
            body,
            body_html,
            json,
        }) => {
            Box::pin(cmd_edit_draft(
                cfg,
                &message_id,
                subject.as_deref(),
                body.as_deref(),
                body_html.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Mark {
            message_id,
            read,
            unread,
            flag,
            unflag,
            json,
        }) => {
            let is_read = if read {
                Some(true)
            } else if unread {
                Some(false)
            } else {
                None
            };
            let flagged = if flag {
                Some(true)
            } else if unflag {
                Some(false)
            } else {
                None
            };
            Box::pin(cmd_mail_mark(cfg, &message_id, is_read, flagged, json)).await
        }
        Some(Command::Move {
            message_id,
            dest_folder,
            json,
        }) => Box::pin(cmd_mail_move(cfg, &message_id, &dest_folder, json)).await,
        Some(Command::Archive { message_id, json }) => {
            Box::pin(cmd_mail_move(cfg, &message_id, "archive", json)).await
        }
        Some(Command::Delete {
            message_id,
            yes,
            json,
        }) => Box::pin(cmd_mail_delete(cfg, &message_id, yes, json)).await,
        Some(Command::Search {
            query,
            folder,
            remote,
            limit,
            json,
        }) => {
            Box::pin(cmd_mail_search(
                cfg,
                &query,
                folder.as_deref(),
                remote,
                limit,
                json,
            ))
            .await
        }
        Some(Command::Download {
            message_id,
            attachment,
            dir,
            json,
        }) => {
            Box::pin(cmd_mail_download(
                cfg,
                &message_id,
                attachment.as_deref(),
                dir.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Preview {
            message_id,
            attachment,
            offline,
            cols,
            rows,
            protocol,
        }) => {
            Box::pin(cmd_preview_mail(
                cfg,
                &message_id,
                attachment.as_deref(),
                offline,
                cols,
                rows,
                protocol.as_deref(),
            ))
            .await
        }
        Some(Command::PreviewFile {
            image_path,
            cols,
            rows,
            protocol,
        }) => cmd_preview_local_file(cfg, &image_path, cols, rows, protocol.as_deref()),
        Some(Command::Calendar {
            days,
            from,
            to,
            limit,
            offline,
            cached,
            json,
        }) => {
            Box::pin(cmd_calendar_list(
                cfg,
                from.as_deref(),
                to.as_deref(),
                days,
                limit,
                offline,
                cached,
                json,
            ))
            .await
        }
        Some(Command::Event {
            event_id,
            offline,
            cached,
            json,
        }) => Box::pin(cmd_event_show(cfg, &event_id, offline, cached, json)).await,
        Some(Command::CreateEvent {
            subject,
            start,
            end,
            location,
            attendee,
            body,
            online,
            all_day,
            time_zone,
            json,
        }) => {
            Box::pin(cmd_event_create(
                cfg, &subject, &start, &end, attendee, location, body, online, all_day, time_zone,
                json,
            ))
            .await
        }
        Some(Command::UpdateEvent {
            event_id,
            subject,
            start,
            end,
            json,
        }) => {
            Box::pin(cmd_update_event(
                cfg,
                &event_id,
                subject.as_deref(),
                start.as_deref(),
                end.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Rsvp {
            event_id,
            response,
            comment,
            json,
        }) => {
            Box::pin(cmd_event_rsvp(
                cfg,
                &event_id,
                &response,
                comment.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::DeleteEvent {
            event_id,
            yes,
            json,
        }) => Box::pin(cmd_event_delete(cfg, &event_id, yes, json)).await,
        Some(Command::Tasks {
            list,
            all,
            limit,
            refresh,
            offline,
            cached,
            json,
        }) => {
            Box::pin(cmd_todo_tasks(
                cfg, &list, all, limit, refresh, offline, cached, json,
            ))
            .await
        }
        Some(Command::TaskLists {
            refresh,
            offline,
            cached,
            json,
        }) => Box::pin(cmd_todo_lists(cfg, refresh, offline, cached, json)).await,
        Some(Command::AddTask {
            title,
            list,
            due,
            importance,
            notes,
            json,
        }) => {
            Box::pin(cmd_todo_add(
                cfg,
                &title,
                &list,
                due.as_deref(),
                &importance,
                notes,
                json,
            ))
            .await
        }
        Some(Command::Done {
            task_id,
            list,
            undo,
            json,
        }) => Box::pin(cmd_todo_done(cfg, &task_id, &list, !undo, json)).await,
        Some(Command::UpdateTask {
            task_id,
            list,
            title,
            due,
            json,
        }) => {
            Box::pin(cmd_update_task(
                cfg,
                &task_id,
                &list,
                title.as_deref(),
                due.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::DeleteTask {
            task_id,
            list,
            yes,
            json,
        }) => Box::pin(cmd_todo_delete(cfg, &task_id, &list, yes, json)).await,
        Some(Command::Contacts {
            query,
            limit,
            refresh,
            offline,
            cached,
            json,
        }) => {
            Box::pin(cmd_contacts(
                cfg,
                query.as_deref(),
                limit,
                refresh,
                offline,
                cached,
                json,
            ))
            .await
        }
        Some(Command::Sync { days, limit, json }) => {
            Box::pin(cmd_sync(cfg, days, limit, json)).await
        }
        Some(Command::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(Command::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Serve {
            bind,
            port,
            api_key,
        }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions {
                    bind,
                    port,
                    api_key,
                },
            ))
            .await
        }
        Some(Command::Mcp) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(crate::mcp::run(cfg.clone(), graph, storage)).await
        }
        Some(Command::Daemon { command }) => Box::pin(dispatch_daemon_command(cfg, command)).await,
    }
}

// ---------------------------------------------------------------------------
// Daemon management
// ---------------------------------------------------------------------------

fn print_daemon_status_text(header: &str, cfg: &AppConfig, status: &crate::daemon::DaemonStatus) {
    println!(
        "{header}: running={}, pid={}, transport={}, endpoint={}, attached_clients={}, auto_daemon={}",
        status.running,
        status
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string()),
        status.transport.as_deref().unwrap_or("-"),
        status.endpoint.as_deref().unwrap_or("-"),
        status.attached_clients,
        cfg.auto_daemon,
    );
    if status.running {
        println!(
            "  version={}, uptime_secs={}, sync_busy={}, last_sync_at={}",
            status.version.as_deref().unwrap_or("-"),
            status
                .uptime_secs
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string()),
            status.sync_busy,
            status.last_sync_at.as_deref().unwrap_or("-"),
        );
        if !status.sync_summary.trim().is_empty() {
            println!("  sync_summary: {}", status.sync_summary);
        }
    }
    if let Some(ref err) = status.last_error {
        println!("  last_error: {err}");
    }
}

async fn dispatch_daemon_command(cfg: &AppConfig, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start { json } => {
            let status = crate::daemon::ensure_daemon_started(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon started", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Stop { json } => {
            let stopped = crate::daemon::stop_daemon(cfg).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "stopped": stopped,
                    }))?
                );
            } else if stopped {
                println!("Background sync daemon stopped.");
            } else {
                println!("Background sync daemon is not running.");
            }
            Ok(())
        }
        DaemonCommand::Status { json } => {
            let status = crate::daemon::query_daemon_status(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon status", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Restart { json } => {
            let _ = crate::daemon::stop_daemon(cfg).await?;
            let status = crate::daemon::ensure_daemon_started(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon restarted", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Run => crate::daemon::run_daemon_foreground(cfg.clone()).await,
    }
}

// ---------------------------------------------------------------------------
// Runtime / Storage fast-open helpers
// ---------------------------------------------------------------------------

pub(crate) async fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    if require_login && tokens.is_none() {
        bail!(
            "Sign-in required. Run `{} login` first (or enable shared teams-cli auth / set OUTLOOK_ACCESS_TOKEN).",
            Persona::detect().bin_name()
        );
    }
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let storage = Storage::open(&config::db_path(user_id))?;
    let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
    if let Some(uid) = user_id {
        graph = graph.with_my_user_id(uid);
    }
    Ok((auth, graph, storage))
}

fn db_path_in_dir(data_dir: &Path, user_id: Option<&str>) -> PathBuf {
    let default_path = config::db_path(user_id);
    if let Ok(rel) = default_path.strip_prefix(config::data_dir()) {
        return data_dir.join(rel);
    }
    let bucket = default_path
        .parent()
        .and_then(|p| p.file_name())
        .unwrap_or_else(|| std::ffi::OsStr::new("default"));
    data_dir.join(bucket).join("cache.db")
}

fn extract_user_id_from_jwt(raw_token: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let trimmed = raw_token.trim();
    let token = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    };
    let payload_b64 = token.split('.').nth(1)?.trim();
    if payload_b64.is_empty() {
        return None;
    }
    let normalized: String = payload_b64
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            _ => c,
        })
        .collect();
    let decoded = URL_SAFE_NO_PAD.decode(&normalized).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    val.get("oid")
        .or_else(|| val.get("sub"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn extract_user_id_from_token_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cleaned = raw.trim_start_matches('\u{feff}').trim();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.starts_with('{') {
        let val: serde_json::Value = serde_json::from_str(cleaned).ok()?;
        for key in ["user_id", "userId"] {
            if let Some(uid) = val
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(uid.to_string());
            }
        }
        for key in ["access_token", "accessToken", "token"] {
            if let Some(tok) = val.get(key).and_then(|v| v.as_str()) {
                if let Some(uid) = extract_user_id_from_jwt(tok) {
                    return Some(uid);
                }
            }
        }
        return None;
    }
    let first_line = cleaned.lines().map(str::trim).find(|l| !l.is_empty())?;
    extract_user_id_from_jwt(first_line)
}

fn find_single_existing_db(data_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(data_dir).ok()?;
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let db = path.join("cache.db");
            if db.is_file() {
                if found.is_some() {
                    return None;
                }
                found = Some(db);
            }
        } else if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("cache-") && name.ends_with(".db") {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(path);
                }
            }
        }
    }
    found
}

fn resolve_storage_db_path_fast(
    cfg: &AppConfig,
    env_access_token: Option<&str>,
    data_dir: &Path,
    cred_path: &Path,
) -> Option<PathBuf> {
    if let Some(raw_env) = env_access_token.map(str::trim).filter(|s| !s.is_empty()) {
        let uid = extract_user_id_from_jwt(raw_env);
        return Some(db_path_in_dir(data_dir, uid.as_deref()));
    }

    if let Some(tf) = cfg.resolved_token_file() {
        if tf.is_file() {
            let uid = extract_user_id_from_token_file(&tf);
            return Some(db_path_in_dir(data_dir, uid.as_deref()));
        }
    }

    if cred_path.is_file() {
        if let Some(uid) = extract_user_id_from_token_file(cred_path) {
            return Some(db_path_in_dir(data_dir, Some(&uid)));
        }
    }

    if let Some(single_db) = find_single_existing_db(data_dir) {
        return Some(single_db);
    }

    None
}

fn infer_user_id_fast_with(
    cfg: &AppConfig,
    env_access_token: Option<&str>,
    cred_path: &Path,
) -> Option<String> {
    if let Some(raw_env) = env_access_token.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(uid) = extract_user_id_from_jwt(raw_env) {
            return Some(uid);
        }
    }
    if let Some(tf) = cfg.resolved_token_file() {
        if tf.is_file() {
            if let Some(uid) = extract_user_id_from_token_file(&tf) {
                return Some(uid);
            }
        }
    }
    if cred_path.is_file() {
        if let Some(uid) = extract_user_id_from_token_file(cred_path) {
            return Some(uid);
        }
    }
    None
}

fn env_access_token_value() -> Option<String> {
    std::env::var("OUTLOOK_ACCESS_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn resolve_daemon_warm(cfg: &AppConfig, refresh: bool, cached: bool) -> bool {
    if refresh || cached {
        return false;
    }
    let env_tok = env_access_token_value();
    let cred_path = config::credential_fallback_path();
    let inferred_user_id = infer_user_id_fast_with(cfg, env_tok.as_deref(), &cred_path);
    let daemon_warm = crate::daemon::is_daemon_alive_fast(cfg, inferred_user_id.as_deref());
    if cfg.auto_daemon && !daemon_warm {
        let _ = crate::daemon::ensure_daemon_started(cfg).await;
    }
    daemon_warm
}

/// Opens local SQLite `Storage` without touching OS Keyring or initializing `reqwest::Client` TLS roots
/// whenever `OUTLOOK_ACCESS_TOKEN`, `OUTLOOK_TOKEN_FILE`, `credentials.json`, or a single local `cache.db` resolves `user_id`.
pub(crate) fn open_storage_only(cfg: &AppConfig) -> Result<Storage> {
    let env_tok = env_access_token_value();
    if let Some(path) = resolve_storage_db_path_fast(
        cfg,
        env_tok.as_deref(),
        &config::data_dir(),
        &config::credential_fallback_path(),
    ) {
        return Storage::open(&path);
    }

    if cfg.share_m365_auth {
        if let Some(teams_dir) = config::legacy_teams_config_dir() {
            let teams_cred = teams_dir.join("credentials.json");
            if teams_cred.is_file() {
                if let Some(uid) = extract_user_id_from_token_file(&teams_cred) {
                    return Storage::open(&config::db_path(Some(&uid)));
                }
            }
        }
    }

    let uid = crate::auth::load_stored_tokens()
        .ok()
        .flatten()
        .and_then(|t| t.user_id);
    Storage::open(&config::db_path(uid.as_deref()))
}

#[cfg(test)]
fn parse_cli_cache_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)
}

pub(crate) fn cli_cache_ttl_secs(cfg: &AppConfig) -> u64 {
    if let Ok(raw) = std::env::var("OUTLOOK_CLI_CACHE_SECS") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            return parsed;
        }
    }
    cfg.cli_cache_ttl_secs
}

pub(crate) fn is_cursor_fresh(storage: &Storage, scope_keys: &[&str], ttl_secs: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    let now = Utc::now();
    for key in scope_keys {
        if let Ok(Some(cursor)) = storage.get_sync_cursor(key) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(&cursor.last_synced_at) {
                let age = now.signed_duration_since(dt.with_timezone(&Utc));
                if age.num_seconds() >= 0 && (age.num_seconds() as u64) <= ttl_secs {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn record_cli_sync_cursor(storage: &Storage, scope_key: &str) {
    let cursor = SyncCursor {
        scope_key: scope_key.to_string(),
        delta_link: None,
        history_next_link: None,
        last_synced_at: Utc::now().to_rfc3339(),
    };
    let _ = storage.save_sync_cursor(&cursor);
}

pub(crate) fn rsvp_action_to_graph_status(normalized: &str) -> &'static str {
    match normalized {
        "accept" => "accepted",
        "tentativelyAccept" => "tentativelyAccepted",
        "decline" => "declined",
        _ => "notResponded",
    }
}

// ---------------------------------------------------------------------------
// Auth & Diagnostics commands
// ---------------------------------------------------------------------------

async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    if auth.current_tokens().await.is_none() {
        println!("No stored sign-in credentials found. Starting Microsoft account sign-in.");
        println!(
            "(To use a custom Azure App: {} login --client-id <APP_ID> --tenant-id <TENANT_ID>)",
            persona.bin_name()
        );
        cmd_login(cfg, LoginOptions::default(), false).await?;
        println!();
    }

    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    tui::run(cfg.clone(), auth, graph, storage).await
}

async fn cmd_login(cfg: &mut AppConfig, opts: LoginOptions, json_out: bool) -> Result<()> {
    let mut config_changed = false;
    if let Some(ref cid) = opts.client_id {
        let trimmed = cid.trim();
        if !trimmed.is_empty() && cfg.client_id != trimmed {
            cfg.client_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if let Some(ref tid) = opts.tenant_id {
        let trimmed = tid.trim();
        if !trimmed.is_empty() && cfg.tenant_id != trimmed {
            cfg.tenant_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if config_changed {
        cfg.save()?;
    }

    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.login(&opts).await?;
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let me = graph.get_me().await?;
    auth.update_identity(me.id.clone(), me.display_name.clone(), me.email.clone())
        .await?;

    let updated = auth.current_tokens().await.unwrap_or(tokens);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "user": me,
                "client_id": updated.client_id,
                "tenant_id": updated.tenant_id,
                "auth_mode": updated.auth_mode,
                "expires_at": updated.expires_at,
            }))?
        );
    } else {
        println!(
            "Signed in as: {} ({}) [id={}]",
            me.display_name,
            me.email.as_deref().unwrap_or("-"),
            me.id
        );
        println!(
            "Auth mode: {} · tenant={} · expires: {}",
            updated.auth_mode,
            updated.tenant_id,
            format_unix_ts(updated.expires_at)
        );
    }
    Ok(())
}

async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Signed out and removed stored outlook-cli tokens.");
    }
    Ok(())
}

pub(crate) fn auth_source_label(tokens: Option<&StoredTokens>, cfg: &AppConfig) -> &'static str {
    let Some(tok) = tokens else {
        return "not signed in";
    };
    let mode = tok.auth_mode.to_ascii_lowercase();
    if mode == "shared_teams_token_file" {
        return "teams-cli (shared token_file)";
    }
    if mode.contains("teams") || mode.contains("shared") {
        return "teams-cli (shared session)";
    }
    if mode == "env_token" {
        return "outlook-cli (OUTLOOK_ACCESS_TOKEN)";
    }
    if mode == "token_file" {
        if cfg.resolved_token_file().is_some() {
            return "outlook-cli (OUTLOOK_TOKEN_FILE)";
        }
        return "teams-cli (TEAMS_TOKEN_FILE)";
    }
    let outlook_cred = config::credential_fallback_path();
    if outlook_cred.exists() {
        return "outlook-cli (credentials.json)";
    }
    "outlook-cli (OS keyring)"
}

async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let mut tokens = match auth.current_tokens().await {
        Some(t) => t,
        None => {
            if let Some(ref tf) = cfg.resolved_token_file() {
                if !tf.exists() {
                    bail!(
                        "Sign-in required. Run `{} login` or set `OUTLOOK_TOKEN_FILE` / `OUTLOOK_ACCESS_TOKEN`. (Configured OUTLOOK_TOKEN_FILE not found: {})",
                        persona.bin_name(),
                        tf.display()
                    );
                }
                bail!(
                    "Sign-in required. Run `{} login` or set a valid `OUTLOOK_TOKEN_FILE` ({}) / `OUTLOOK_ACCESS_TOKEN`.",
                    persona.bin_name(),
                    tf.display()
                );
            }
            bail!(
                "Sign-in required. Run `{} login` or set `OUTLOOK_TOKEN_FILE` / `OUTLOOK_ACCESS_TOKEN`.",
                persona.bin_name()
            );
        }
    };

    let refresh_skew_secs = (cfg.token_refresh_mins.max(1) * 60) as i64;
    let mut refresh_failure: Option<anyhow::Error> = None;
    if tokens.is_expired_soon(refresh_skew_secs) && auth.is_refreshable().await {
        match auth.refresh_now().await {
            Ok(refreshed) => {
                tokens = refreshed;
            }
            Err(refresh_err) => {
                if tokens.is_expired_soon(0) {
                    refresh_failure = Some(refresh_err);
                } else {
                    eprintln!(
                        "Warning: automatic token refresh failed ({refresh_err}); continuing with existing token (expires: {}).",
                        format_unix_ts(tokens.expires_at)
                    );
                }
            }
        }
    }

    let auth_source = auth_source_label(Some(&tokens), cfg);
    let scope_diag = diagnose_access_token_scopes(&tokens.access_token);

    if tokens.is_expired_soon(0) {
        let err_detail = match refresh_failure {
            Some(ref err) => format!(
                "Token expired (exp: {}, auth_mode={}, source={}) — automatic refresh failed: {err:#}",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode,
                auth_source
            ),
            None => format!(
                "Token expired (exp: {}, auth_mode={}, source={}) — refresh the token and retry.",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode,
                auth_source
            ),
        };
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "persona": persona,
                    "user_id": tokens.user_id,
                    "display_name": tokens.user_display_name,
                    "email": tokens.user_email,
                    "client_id": tokens.client_id,
                    "tenant_id": tokens.tenant_id,
                    "auth_mode": tokens.auth_mode,
                    "auth_source": auth_source,
                    "expires_at": tokens.expires_at,
                    "expires_at_human": format_unix_ts(tokens.expires_at),
                    "expired": true,
                    "expiring_soon": true,
                    "status": "expired",
                    "scopes": scope_diag.scopes,
                    "roles": scope_diag.roles,
                    "scope_warnings": scope_diag.warnings,
                    "error": err_detail,
                }))?
            );
            bail!(
                "Token expired (exp: {}) — refresh the token and retry.",
                format_unix_ts(tokens.expires_at)
            );
        }
        bail!("{err_detail}");
    }

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let mut profile_err: Option<anyhow::Error> = None;
    let live_profile = if tokens.user_id.is_none() || tokens.user_display_name.is_none() {
        match graph.get_me().await {
            Ok(p) => Some(p),
            Err(e) => {
                profile_err = Some(e);
                None
            }
        }
    } else {
        None
    };

    let user_id = tokens
        .user_id
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.id.clone()));
    let display_name = tokens
        .user_display_name
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.display_name.clone()));
    let email = tokens
        .user_email
        .clone()
        .or_else(|| live_profile.as_ref().and_then(|p| p.email.clone()));

    if display_name.is_none() && user_id.is_none() {
        if let Some(err) = profile_err {
            bail!(
                "Failed to fetch profile (/me) (auth_mode={}, source={}, expires: {}): {err:#}",
                tokens.auth_mode,
                auth_source,
                format_unix_ts(tokens.expires_at)
            );
        }
    }

    let expiring_soon = !tokens.is_expired_soon(0) && tokens.is_expired_soon(refresh_skew_secs);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "persona": persona,
                "user_id": user_id,
                "display_name": display_name,
                "email": email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "auth_source": auth_source,
                "share_m365_auth": cfg.share_m365_auth,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": false,
                "expiring_soon": expiring_soon,
                "status": if expiring_soon { "expiring_soon" } else { "valid" },
                "scopes": scope_diag.scopes,
                "roles": scope_diag.roles,
                "scope_warnings": scope_diag.warnings,
            }))?
        );
    } else {
        println!("persona: {}", persona.bin_name());
        println!(
            "user: {} ({})",
            display_name.as_deref().unwrap_or("-"),
            email.as_deref().unwrap_or("-")
        );
        println!("user_id: {}", user_id.as_deref().unwrap_or("-"));
        println!("client_id: {}", tokens.client_id);
        println!("tenant_id: {}", tokens.tenant_id);
        println!("auth_mode: {}", tokens.auth_mode);
        println!("auth_source: {auth_source}");
        println!(
            "token_expires: {} ({})",
            format_unix_ts(tokens.expires_at),
            if expiring_soon {
                "expiring soon — refresh recommended"
            } else {
                "valid"
            }
        );
        if !scope_diag.scopes.is_empty() {
            println!("token_scopes (scp): {}", scope_diag.scopes.join(" "));
        } else if !scope_diag.has_scope_claims {
            println!("token_scopes (scp): (no JWT claims)");
        }
        if !scope_diag.roles.is_empty() {
            println!("token_roles: {}", scope_diag.roles.join(" "));
        }
        for warning in &scope_diag.warnings {
            println!("scope_warning: {warning}");
        }
    }
    Ok(())
}

async fn cmd_doctor(
    cfg: &AppConfig,
    persona: Persona,
    online: bool,
    json_out: bool,
) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let token_diag = tokens
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();

    let cfg_path = config::config_file_path();
    let cred_path = config::credential_fallback_path();
    let drafts = config::drafts_path();
    let db = config::db_path(user_id);
    let dl_dir = cfg.resolved_download_dir();
    let configured_scopes = cfg.effective_scopes();
    let token_file = cfg.resolved_token_file();
    let token_refresh_cmd_configured = !cfg.token_refresh_cmd.trim().is_empty();
    let auth_mode = tokens.as_ref().map(|t| t.auth_mode.clone());
    let auth_source = auth_source_label(tokens.as_ref(), cfg);
    let token_expires_at = tokens.as_ref().map(|t| format_unix_ts(t.expires_at));
    let keyring_status = describe_keyring_status(tokens.as_ref(), &cred_path, cfg);

    let (cached_folders, cached_messages, cached_events, cached_contacts, cached_todo_lists) =
        if db.exists() {
            if let Ok(storage) = Storage::open(&db) {
                (
                    storage.list_mail_folders().map(|v| v.len()).unwrap_or(0),
                    storage
                        .list_mail_messages(None, false, false, false, 10_000)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    storage
                        .list_calendar_events(None, None, 10_000)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    storage
                        .list_contacts(None, 10_000)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    storage.list_todo_lists().map(|v| v.len()).unwrap_or(0),
                )
            } else {
                (0, 0, 0, 0, 0)
            }
        } else {
            (0, 0, 0, 0, 0)
        };

    let daemon_status = crate::daemon::query_daemon_status(cfg)
        .await
        .unwrap_or_default();
    let graphics_caps = img_preview::detect_graphics_caps(cfg.image_protocol);

    let mut probe_results: Option<serde_json::Value> = None;
    if online {
        let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
        if let Some(uid) = user_id {
            graph = graph.with_my_user_id(uid);
        }
        let (start_iso, end_iso) = default_calendar_window(None, None, 1)?;
        let (me_res, mail_res, cal_res, contacts_res, todo_res) = tokio::join!(
            graph.get_me(),
            graph.list_mail_folders(),
            graph.list_calendar_view(&start_iso, &end_iso, 5),
            graph.list_contacts(5),
            graph.list_todo_lists(),
        );

        let format_probe = |res: Result<usize>| -> serde_json::Value {
            match res {
                Ok(count) => json!({ "ok": true, "count": count }),
                Err(err) => json!({ "ok": false, "error": format!("{err:#}") }),
            }
        };

        let me_val = match me_res {
            Ok(me) => json!({ "ok": true, "user": me }),
            Err(err) => {
                let wrapped = wrap_graph_permission_error(&graph, err).await;
                json!({ "ok": false, "error": format!("{wrapped:#}") })
            }
        };
        let mail_val = format_probe(mail_res.map(|v| v.len()));
        let cal_val = format_probe(cal_res.map(|v| v.len()));
        let contacts_val = format_probe(contacts_res.map(|v| v.len()));
        let todo_val = format_probe(todo_res.map(|v| v.len()));
        let all_ok = me_val["ok"].as_bool().unwrap_or(false)
            && mail_val["ok"].as_bool().unwrap_or(false)
            && cal_val["ok"].as_bool().unwrap_or(false)
            && contacts_val["ok"].as_bool().unwrap_or(false)
            && todo_val["ok"].as_bool().unwrap_or(false);

        probe_results = Some(json!({
            "ok": all_ok,
            "me": me_val,
            "mail": mail_val,
            "calendar": cal_val,
            "contacts": contacts_val,
            "todo": todo_val,
        }));
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "persona": persona,
                "scopes": token_diag.scopes,
                "roles": token_diag.roles,
                "scope_warnings": token_diag.warnings,
                "configured_scopes": configured_scopes,
                "config_path": cfg_path,
                "config_exists": cfg_path.exists(),
                "credentials_fallback_path": cred_path,
                "credentials_fallback_exists": cred_path.exists(),
                "drafts_path": drafts,
                "db_path": db,
                "db_exists": db.exists(),
                "download_dir": dl_dir,
                "github_repo": cfg.github_repo,
                "client_id": cfg.client_id,
                "tenant_id": cfg.tenant_id,
                "graph_base_url": cfg.graph_base_url,
                "auth_base_url": cfg.auth_base_url,
                "auth_source": auth_source,
                "share_m365_auth": cfg.share_m365_auth,
                "keyring_status": keyring_status,
                "token_file": token_file,
                "token_file_exists": token_file.as_ref().map(|p| p.exists()),
                "token_refresh_cmd_configured": token_refresh_cmd_configured,
                "token_refresh_mins": cfg.token_refresh_mins,
                "auth_mode": auth_mode,
                "token_expires_at": token_expires_at,
                "logged_in": tokens.is_some(),
                "cache_counts": {
                    "mail_folders": cached_folders,
                    "mail_messages": cached_messages,
                    "calendar_events": cached_events,
                    "contacts": cached_contacts,
                    "todo_lists": cached_todo_lists,
                },
                "daemon": {
                    "running": daemon_status.running,
                    "pid": daemon_status.pid,
                    "transport": daemon_status.transport,
                    "endpoint": daemon_status.endpoint,
                    "attached_clients": daemon_status.attached_clients,
                    "auto_daemon": cfg.auto_daemon,
                },
                "image_protocol": graphics_caps.protocol.as_str(),
                "image_protocol_preference": cfg.image_protocol.as_str(),
                "cell_px": [graphics_caps.cell_px.0, graphics_caps.cell_px.1],
                "online_check": probe_results,
            }))?
        );
    } else {
        println!(
            "{} v{} (persona={})",
            persona.bin_name(),
            env!("CARGO_PKG_VERSION"),
            persona.bin_name()
        );
        println!(
            "config.toml: {} (exists={})",
            cfg_path.display(),
            cfg_path.exists()
        );
        println!(
            "credentials.json: {} (exists={})",
            cred_path.display(),
            cred_path.exists()
        );
        println!("drafts.json: {}", drafts.display());
        println!("cache.db: {} (exists={})", db.display(), db.exists());
        println!(
            "cache_counts: folders={cached_folders}, messages={cached_messages}, events={cached_events}, contacts={cached_contacts}, todo_lists={cached_todo_lists}"
        );
        println!("download_dir: {}", dl_dir.display());
        println!("github_repo: {}", cfg.github_repo);
        println!(
            "configured_scopes ({}): {}",
            configured_scopes.len(),
            configured_scopes.join(", ")
        );
        if !token_diag.scopes.is_empty() {
            println!(
                "token_scopes ({}): {}",
                token_diag.scopes.len(),
                token_diag.scopes.join(" ")
            );
        } else if tokens.is_some() {
            println!("token_scopes: (no JWT scp claims)");
        }
        if !token_diag.roles.is_empty() {
            println!(
                "token_roles ({}): {}",
                token_diag.roles.len(),
                token_diag.roles.join(" ")
            );
        }
        for warning in &token_diag.warnings {
            println!("scope_warning: {warning}");
        }
        println!("client_id: {}", cfg.client_id);
        println!("tenant_id: {}", cfg.tenant_id);
        println!("graph_base_url: {}", cfg.graph_base_url);
        println!("auth_base_url: {}", cfg.auth_base_url);
        println!("auth_source: {auth_source}");
        println!("keyring_status: {keyring_status}");
        println!(
            "daemon: running={}, pid={}, endpoint={}, attached_clients={}, auto_daemon={}",
            daemon_status.running,
            daemon_status
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            daemon_status.endpoint.as_deref().unwrap_or("-"),
            daemon_status.attached_clients,
            cfg.auto_daemon,
        );
        println!(
            "token_file: {}",
            token_file
                .as_ref()
                .map(|p| format!("{} (exists={})", p.display(), p.exists()))
                .unwrap_or_else(|| "(unset)".to_string())
        );
        println!(
            "token_refresh_cmd_configured: {token_refresh_cmd_configured} (token_refresh_mins={})",
            cfg.token_refresh_mins
        );
        println!("auth_mode: {}", auth_mode.as_deref().unwrap_or("-"));
        println!(
            "token_expires_at: {}",
            token_expires_at.as_deref().unwrap_or("-")
        );
        let cell_suffix = if graphics_caps.cell_px_exact {
            ""
        } else {
            " est."
        };
        println!(
            "Image protocol: {} (preference: {}, cell: {}x{}px{})",
            graphics_caps.protocol.label(),
            cfg.image_protocol.as_str(),
            graphics_caps.cell_px.0,
            graphics_caps.cell_px.1,
            cell_suffix,
        );
        if let Some(ref online_val) = probe_results {
            println!(
                "online_check: ok={} (me={}, mail={}, calendar={}, contacts={}, todo={})",
                online_val["ok"].as_bool().unwrap_or(false),
                format_probe_status(&online_val["me"]),
                format_probe_status(&online_val["mail"]),
                format_probe_status(&online_val["calendar"]),
                format_probe_status(&online_val["contacts"]),
                format_probe_status(&online_val["todo"]),
            );
        }
    }

    Ok(())
}

fn format_probe_status(val: &serde_json::Value) -> String {
    if val["ok"].as_bool().unwrap_or(false) {
        if let Some(cnt) = val.get("count").and_then(|v| v.as_u64()) {
            format!("ok({cnt})")
        } else if let Some(name) = val
            .get("user")
            .and_then(|u| u.get("display_name"))
            .and_then(|v| v.as_str())
        {
            format!("ok({name})")
        } else {
            "ok".to_string()
        }
    } else {
        val.get("error")
            .and_then(|e| e.as_str())
            .map(|e| format!("err({})", truncate_single_line(e, 48)))
            .unwrap_or_else(|| "err".to_string())
    }
}

fn describe_keyring_status(
    tokens: Option<&StoredTokens>,
    fallback_path: &Path,
    cfg: &AppConfig,
) -> String {
    match tokens {
        None => "not signed in (no stored token)".to_string(),
        Some(tok) if tok.auth_mode == "env_token" => {
            "env_token (using OUTLOOK_ACCESS_TOKEN)".to_string()
        }
        Some(tok) if tok.auth_mode == "token_file" => {
            "token_file (using OUTLOOK_TOKEN_FILE)".to_string()
        }
        Some(_) if fallback_path.exists() => {
            "using outlook-cli credentials.json file fallback".to_string()
        }
        Some(_) if auth_source_label(tokens, cfg).starts_with("teams-cli") => {
            "shared teams-cli session active".to_string()
        }
        Some(_) => "OS keyring active (token loaded)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Mail commands
// ---------------------------------------------------------------------------

fn format_mail_folders_lines(folders: &[MailFolder]) -> Vec<String> {
    let paths = crate::ui::nav_pane::folder_display_paths(folders);
    let mut ordered: Vec<(&MailFolder, String)> = folders
        .iter()
        .map(|f| {
            let path = paths
                .get(&f.id)
                .cloned()
                .unwrap_or_else(|| f.display_name.clone());
            (f, path)
        })
        .collect();
    ordered.sort_by(|a, b| {
        a.1.to_ascii_lowercase()
            .cmp(&b.1.to_ascii_lowercase())
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    ordered
        .into_iter()
        .map(|(f, path)| {
            let wk = f
                .well_known_name
                .as_deref()
                .map(|w| format!(" [{w}]"))
                .unwrap_or_default();
            format!(
                "{path}{wk}  unread={}/{}  ({})",
                f.unread_item_count, f.total_item_count, f.id
            )
        })
        .collect()
}

fn print_mail_folders_output(folders: &[MailFolder], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "folders": folders }))?
        );
    } else if folders.is_empty() {
        println!("No mail folders found.");
    } else {
        for line in format_mail_folders_lines(folders) {
            println!("{line}");
        }
    }
    Ok(())
}

async fn cmd_folders(
    cfg: &AppConfig,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if offline {
        let storage = open_storage_only(cfg)?;
        let folders = storage.list_mail_folders()?;
        return print_mail_folders_output(&folders, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:mail_folders"], ttl) {
                let folders = storage.list_mail_folders()?;
                if !folders.is_empty() {
                    return print_mail_folders_output(&folders, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let cached_folders = storage.list_mail_folders()?;
    let should_fetch = refresh
        || cached_folders.is_empty()
        || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &["cli:mail_folders"], ttl));
    let folders = if should_fetch {
        let fetched = match graph.list_mail_folders().await {
            Ok(items) => items,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        storage.replace_mail_folders(&fetched)?;
        record_cli_sync_cursor(&storage, "cli:mail_folders");
        storage.list_mail_folders()?
    } else {
        cached_folders
    };

    print_mail_folders_output(&folders, json_out)
}

pub(crate) fn load_cached_mail_list(
    storage: &Storage,
    folder_id: &str,
    limit: usize,
    full: bool,
) -> Result<Vec<MailMessage>> {
    if full {
        storage.list_mail_messages(Some(folder_id), false, false, false, limit)
    } else {
        storage.list_mail_summaries(Some(folder_id), limit)
    }
}

pub(crate) fn filter_mail_messages(
    mut messages: Vec<MailMessage>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
) -> Vec<MailMessage> {
    if unread_only {
        messages.retain(|m| !m.is_read);
    }
    if flagged_only {
        messages.retain(|m| m.flagged);
    }
    if focused_only {
        messages.retain(MailMessage::is_focused);
    }
    messages.truncate(limit);
    messages
}

pub(crate) fn mail_list_json(messages: &[MailMessage], compact: bool) -> serde_json::Value {
    if !compact {
        return json!(messages);
    }
    serde_json::Value::Array(
        messages
            .iter()
            .map(|msg| {
                json!({
                    "id": msg.id,
                    "subject": msg.subject,
                    "from": msg.from,
                    "received_at": msg.received_at,
                    "is_read": msg.is_read,
                    "is_draft": msg.is_draft,
                    "flagged": msg.flagged,
                    "has_attachments": msg.has_attachments,
                })
            })
            .collect(),
    )
}

pub(crate) fn mail_read_json(msg: &MailMessage, compact: bool) -> serde_json::Value {
    if !compact {
        return json!({ "message": msg });
    }
    json!({
        "message": {
            "id": msg.id,
            "subject": msg.subject,
            "from": msg.from,
            "to_recipients": msg.to_recipients,
            "cc_recipients": msg.cc_recipients,
            "bcc_recipients": msg.bcc_recipients,
            "reply_to": msg.reply_to,
            "received_at": msg.received_at,
            "sent_at": msg.sent_at,
            "is_read": msg.is_read,
            "is_draft": msg.is_draft,
            "body_loaded": msg.body_loaded,
            "body_plain": msg.body_plain,
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn print_mail_list_output(
    folder: &str,
    folder_id: &str,
    messages: Vec<MailMessage>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    let messages = filter_mail_messages(messages, unread_only, flagged_only, focused_only, limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "folder": folder_id,
                "messages": mail_list_json(&messages, compact),
            }))?
        );
    } else if messages.is_empty() {
        println!("No messages found in '{folder}'.");
    } else {
        for m in &messages {
            let unread_mark = if m.is_read { " " } else { "*" };
            let flag_mark = if m.flagged { "!" } else { " " };
            let att_mark = if m.has_attachments { "@" } else { " " };
            println!(
                "{unread_mark}{flag_mark}{att_mark} [{}] {} | {} ({})",
                m.received_at,
                m.from_display(),
                truncate_single_line(m.effective_subject(), 60),
                m.id
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_mail_list(
    cfg: &AppConfig,
    folder: &str,
    limit: usize,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let fetch_limit = if unread_only || flagged_only || focused_only {
        (limit.saturating_mul(3)).clamp(limit, 500)
    } else {
        limit
    };

    let full = json_out && !compact;
    if offline {
        let storage = open_storage_only(cfg)?;
        let resolved = resolver::resolve_mail_folder_cached(&storage, folder)?;
        let messages = load_cached_mail_list(&storage, &resolved.id, fetch_limit, full)?;
        return print_mail_list_output(
            folder,
            &resolved.id,
            messages,
            unread_only,
            flagged_only,
            focused_only,
            limit,
            json_out,
            compact,
        );
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(resolved) = resolver::resolve_mail_folder_cached(&storage, folder) {
                let cli_key = format!("cli:mail:{}", resolved.id);
                let delta_key = format!("mail_delta:{}", resolved.id);
                if cached
                    || daemon_warm
                    || is_cursor_fresh(&storage, &[cli_key.as_str(), delta_key.as_str()], ttl)
                {
                    let messages =
                        load_cached_mail_list(&storage, &resolved.id, fetch_limit, full)?;
                    if !messages.is_empty() {
                        return print_mail_list_output(
                            folder,
                            &resolved.id,
                            messages,
                            unread_only,
                            flagged_only,
                            focused_only,
                            limit,
                            json_out,
                            compact,
                        );
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = match resolver::resolve_mail_folder(&graph, &storage, folder, false).await {
        Ok(f) => f,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };

    let cached_msgs = load_cached_mail_list(&storage, &resolved.id, fetch_limit, full)?;
    let cli_key = format!("cli:mail:{}", resolved.id);
    let delta_key = format!("mail_delta:{}", resolved.id);
    let should_fetch = refresh
        || cached_msgs.is_empty()
        || (!cached
            && !daemon_warm
            && !is_cursor_fresh(&storage, &[cli_key.as_str(), delta_key.as_str()], ttl));
    let messages = if should_fetch {
        let sync_res = match crate::sync::sync_mail_folder_delta(
            &resolved.id,
            &graph,
            &storage,
            None,
            false,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        record_cli_sync_cursor(&storage, &format!("cli:mail:{}", sync_res.folder_id));
        load_cached_mail_list(&storage, &sync_res.folder_id, fetch_limit, full)?
    } else {
        cached_msgs
    };

    print_mail_list_output(
        folder,
        &resolved.id,
        messages,
        unread_only,
        flagged_only,
        focused_only,
        limit,
        json_out,
        compact,
    )
}

fn print_mail_read_text(msg: &MailMessage, raw_html: bool) {
    println!("Subject: {}", msg.effective_subject());
    println!("From: {}", msg.from_display());
    if !msg.to_recipients.is_empty() {
        let to_list: Vec<String> = msg.to_recipients.iter().map(|r| r.display()).collect();
        println!("To: {}", to_list.join(", "));
    }
    if !msg.cc_recipients.is_empty() {
        let cc_list: Vec<String> = msg.cc_recipients.iter().map(|r| r.display()).collect();
        println!("Cc: {}", cc_list.join(", "));
    }
    if !msg.bcc_recipients.is_empty() {
        let bcc_list: Vec<String> = msg.bcc_recipients.iter().map(|r| r.display()).collect();
        println!("Bcc: {}", bcc_list.join(", "));
    }
    println!(
        "Date: {} | read={} | flagged={} | importance={}",
        msg.received_at, msg.is_read, msg.flagged, msg.importance
    );
    if !msg.attachments.is_empty() {
        println!("Attachments ({}):", msg.attachments.len());
        for (idx, att) in msg.attachments.iter().enumerate() {
            println!(
                "  [{idx}] {} ({}, {} bytes, id={})",
                att.name, att.content_type, att.size, att.id
            );
        }
    }
    println!("---");
    if raw_html {
        println!("{}", msg.body_html);
    } else {
        let body = if !msg.body_plain.trim().is_empty() {
            msg.body_plain.trim().to_string()
        } else {
            let blocks = crate::parser::parse_message_blocks(&msg.body_html, &msg.attachments);
            crate::parser::blocks_to_plain(&blocks)
        };
        println!("{body}");
    }
}

async fn load_mail_message(
    storage: &Storage,
    graph: &GraphClient,
    mail_id: &str,
    offline: bool,
) -> Result<MailMessage> {
    if offline {
        if let Some(exact) = storage.get_mail_message(mail_id)? {
            return Ok(exact);
        }
        let cached = storage.list_mail_messages(None, false, false, false, 2_000)?;
        return resolver::resolve_mail_message_from_slice(&cached, mail_id);
    }
    let msg = match graph.get_mail_message(mail_id).await {
        Ok(m) => m,
        Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
    };
    let _ = storage.upsert_mail_messages(std::slice::from_ref(&msg));
    Ok(msg)
}

#[allow(clippy::too_many_arguments)]
async fn cmd_mail_read(
    cfg: &AppConfig,
    mail_id: &str,
    raw_html: bool,
    mark_read: bool,
    preview_images: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    if offline && !preview_images {
        let storage = open_storage_only(cfg)?;
        let msg = if let Some(exact) = storage.get_mail_message(mail_id)? {
            exact
        } else {
            let all = storage.list_mail_messages(None, false, false, false, 2_000)?;
            resolver::resolve_mail_message_from_slice(&all, mail_id)?
        };
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&mail_read_json(&msg, compact))?
            );
        } else {
            print_mail_read_text(&msg, raw_html);
        }
        return Ok(());
    }

    if !offline && cached && !mark_read && !preview_images {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Some(msg) = storage.get_mail_message(mail_id)? {
                if msg.body_loaded {
                    if json_out {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&mail_read_json(&msg, compact))?
                        );
                    } else {
                        print_mail_read_text(&msg, raw_html);
                    }
                    return Ok(());
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mut msg = load_mail_message(&storage, &graph, mail_id, offline).await?;

    if !offline && mark_read && !msg.is_read {
        if let Err(err) = graph.set_mail_read(&msg.id, true).await {
            return Err(wrap_graph_permission_error(&graph, err).await);
        }
        msg.is_read = true;
        let _ = storage.set_mail_read_local(&msg.id, true);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&mail_read_json(&msg, compact))?
        );
        return Ok(());
    }

    print_mail_read_text(&msg, raw_html);

    if preview_images {
        let (cols, rows) = default_preview_dimensions(None, None);
        for att in &msg.attachments {
            if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                match load_mail_attachment_bytes(&graph, &msg.id, att, offline).await {
                    Ok((_name, _ct, bytes)) => {
                        match img_preview::render_ansi_stdout_with_pref(
                            &bytes,
                            cols,
                            rows,
                            cfg.image_protocol,
                        ) {
                            Ok(rendered) => {
                                println!("\n[Image preview: {}]", att.name);
                                print_rendered_preview(&rendered)?;
                            }
                            Err(err) => {
                                eprintln!("Image render failed ({}): {err:#}", att.name);
                            }
                        }
                    }
                    Err(err) => eprintln!("Attachment fetch failed ({}): {err:#}", att.name),
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_mail_send(
    cfg: &AppConfig,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: &str,
    body_opt: Option<String>,
    attach: Vec<PathBuf>,
    inline: Vec<PathBuf>,
    importance: &str,
    json_out: bool,
) -> Result<()> {
    let markdown_body = match body_opt {
        Some(b) => b,
        None => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("Failed to read email body from stdin")?;
            buf
        }
    };
    if markdown_body.trim().is_empty() {
        bail!("Email body is empty. Pass `--body <MD>` or pipe Markdown via stdin.");
    }
    let normalized_importance = normalize_mail_importance(importance)?;

    for path in &attach {
        if !path.exists() || !path.is_file() {
            bail!("Attachment file not found: {}", path.display());
        }
    }

    let mut inline_images = Vec::with_capacity(inline.len());
    for (idx, path) in inline.iter().enumerate() {
        if !path.exists() || !path.is_file() {
            bail!("Inline image file not found: {}", path.display());
        }
        let content_type = image_mime_for_path(path).with_context(|| {
            format!(
                "Unsupported inline image format (expected png/jpg/jpeg/gif/webp/bmp): {}",
                path.display()
            )
        })?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read inline image: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("inline.png")
            .to_string();
        let temp_id = format!("{}", idx + 1);
        let content_id = format!("inline-img-{}", idx + 1);
        inline_images.push(InlineImageUpload {
            temp_id,
            content_id,
            file_name,
            content_type: content_type.to_string(),
            bytes,
        });
    }

    let body_html = crate::parser::compile_outgoing_mail_html(&markdown_body, &inline_images, None);

    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let mail = OutgoingMail {
        subject: subject.to_string(),
        body_html,
        body_markdown: Some(markdown_body),
        to: to.clone(),
        cc,
        bcc,
        importance: Some(normalized_importance.to_string()),
        attachments: attach,
        inline_images,
        in_reply_to_id: None,
        reply_all: false,
        forward_of_id: None,
        draft_id: None,
        save_to_sent_items: true,
    };
    if let Err(err) = graph.send_mail(&mail).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "to": to,
                "subject": subject,
                "importance": normalized_importance,
            }))?
        );
    } else {
        println!("Email sent: '{}' -> {}", subject, to.join(", "));
    }
    Ok(())
}

async fn cmd_mail_reply(
    cfg: &AppConfig,
    mail_id: &str,
    body_md: &str,
    reply_all: bool,
    json_out: bool,
) -> Result<()> {
    if body_md.trim().is_empty() {
        bail!("Reply body is empty.");
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let comment_html = crate::parser::compile_outgoing_mail_html(body_md, &[], None);
    if let Err(err) = graph.reply_mail(mail_id, &comment_html, reply_all).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
                "reply_all": reply_all,
            }))?
        );
    } else {
        println!(
            "{} sent for message {mail_id}.",
            if reply_all { "Reply All" } else { "Reply" }
        );
    }
    Ok(())
}

async fn cmd_mail_forward(
    cfg: &AppConfig,
    mail_id: &str,
    to: Vec<String>,
    comment_md: &str,
    json_out: bool,
) -> Result<()> {
    if to.is_empty() {
        bail!("At least one recipient (--to) must be specified.");
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let comment_html = if comment_md.trim().is_empty() {
        String::new()
    } else {
        crate::parser::compile_outgoing_mail_html(comment_md, &[], None)
    };
    if let Err(err) = graph.forward_mail(mail_id, &to, &comment_html).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
                "to": to,
            }))?
        );
    } else {
        println!("Forwarded message {mail_id} -> {}", to.join(", "));
    }
    Ok(())
}

pub(crate) fn draft_edit_html(body: Option<&str>, html: Option<&str>) -> Option<String> {
    html.map(str::to_owned).or_else(|| {
        body.map(|markdown| crate::parser::compile_outgoing_mail_html(markdown, &[], None))
    })
}

async fn cmd_edit_draft(
    cfg: &AppConfig,
    id: &str,
    subject: Option<&str>,
    body: Option<&str>,
    body_html: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let html = draft_edit_html(body, body_html);
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    if let Err(err) = graph.edit_mail_draft(id, subject, html.as_deref()).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    if let Ok(updated) = graph.get_mail_message(id).await {
        let _ = storage.upsert_mail_messages(&[updated]);
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "message_id": id }))?
        );
    } else {
        println!("Updated draft {id}.");
    }
    Ok(())
}

async fn cmd_mail_mark(
    cfg: &AppConfig,
    mail_id: &str,
    is_read: Option<bool>,
    flagged: Option<bool>,
    json_out: bool,
) -> Result<()> {
    if is_read.is_none() && flagged.is_none() {
        bail!("Specify at least one of --read, --unread, --flag, or --unflag.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    if let Some(read_val) = is_read {
        if let Err(err) = graph.set_mail_read(mail_id, read_val).await {
            return Err(wrap_graph_permission_error(&graph, err).await);
        }
    }
    if let Some(flag_val) = flagged {
        if let Err(err) = graph.set_mail_flag(mail_id, flag_val).await {
            return Err(wrap_graph_permission_error(&graph, err).await);
        }
    }
    if let Some(read_val) = is_read {
        let _ = storage.set_mail_read_local(mail_id, read_val);
    }
    if let Some(flag_val) = flagged {
        let _ = storage.set_mail_flagged_local(mail_id, flag_val);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
                "is_read": is_read,
                "flagged": flagged,
            }))?
        );
    } else {
        println!(
            "Updated message {mail_id}: is_read={:?}, flagged={:?}",
            is_read, flagged
        );
    }
    Ok(())
}

async fn cmd_mail_move(
    cfg: &AppConfig,
    mail_id: &str,
    dest_folder: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let folder = resolver::resolve_mail_folder(&graph, &storage, dest_folder, false).await?;
    let moved = match graph.move_mail_message(mail_id, &folder.id).await {
        Ok(m) => m,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    storage.replace_moved_mail_local(mail_id, &moved)?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "destination_folder": folder,
                "message": moved,
            }))?
        );
    } else {
        println!(
            "Moved message {} -> {} ({})",
            moved.id, folder.display_name, folder.id
        );
    }
    Ok(())
}

async fn cmd_mail_delete(
    cfg: &AppConfig,
    mail_id: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete mail message {mail_id}? Type 'yes' to confirm: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    if let Err(err) = graph.delete_mail_message(mail_id).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.delete_mail_messages_by_ids(&[mail_id.to_string()]);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message_id": mail_id,
            }))?
        );
    } else {
        println!("Deleted message: {mail_id}");
    }
    Ok(())
}

async fn cmd_mail_download(
    cfg: &AppConfig,
    mail_id: &str,
    attachment_selector: Option<&str>,
    dir: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let mail = load_mail_message(&storage, &graph, mail_id, false).await?;
    let att = select_mail_attachment(&mail, None, attachment_selector)?;

    let out_dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = graph
        .download_mail_attachment(&mail.id, att, &out_dir)
        .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&format_downloaded_attachment_json(
                &mail.id,
                att,
                &downloaded,
            ))?
        );
    } else {
        println!(
            "Downloaded attachment: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.bytes_written
        );
    }
    Ok(())
}

async fn cmd_mail_search(
    cfg: &AppConfig,
    query: &str,
    folder: Option<&str>,
    remote: bool,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("Search query is empty.");
    }
    let limit = clamp_limit(limit);
    let (hits, remote_messages) = if !remote {
        let storage = open_storage_only(cfg)?;
        let folder_filter = match folder.map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => Some(resolver::resolve_mail_folder_cached(&storage, f)?.id),
            None => None,
        };
        let hits = storage.search_mail(query, folder_filter.as_deref(), limit)?;
        (hits, Vec::new())
    } else {
        let (_auth, graph, storage) = open_runtime(cfg, true).await?;
        let folder_filter = match folder.map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => Some(resolver::resolve_mail_folder(&graph, &storage, f, false).await?.id),
            None => None,
        };
        let hits = storage.search_mail(query, folder_filter.as_deref(), limit)?;
        let mut msgs = match graph.search_mail_remote(query, limit).await {
            Ok(m) => m,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        let _ = storage.upsert_mail_messages(&msgs);
        if let Some(ref fid) = folder_filter {
            msgs.retain(|m| m.folder_id.as_deref() == Some(fid.as_str()));
        }
        (hits, msgs)
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "hits": hits,
                "remote": remote_messages,
            }))?
        );
    } else if hits.is_empty() && remote_messages.is_empty() {
        println!("No mail messages matching '{query}'.");
    } else {
        for hit in &hits {
            println!(
                "[local][{}] {} | {}: {} ({})",
                hit.received_at, hit.from_display, hit.subject, hit.snippet, hit.id
            );
        }
        for msg in &remote_messages {
            println!(
                "[remote][{}] {} | {} ({})",
                msg.received_at,
                msg.from_display(),
                msg.effective_subject(),
                msg.id
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Calendar commands
// ---------------------------------------------------------------------------

fn print_cal_events_output(
    start_iso: &str,
    end_iso: &str,
    events: &[CalendarEvent],
    json_out: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "start": start_iso,
                "end": end_iso,
                "events": events,
            }))?
        );
    } else if events.is_empty() {
        println!("No calendar events found.");
    } else {
        for ev in events {
            let loc = ev
                .location
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(" @ {s}"))
                .unwrap_or_default();
            let meet = if ev.is_online_meeting { " [Online]" } else { "" };
            println!(
                "[{} ~ {}] {}{}{} ({})",
                ev.start_iso, ev.end_iso, ev.subject, loc, meet, ev.id
            );
        }
    }
    Ok(())
}

async fn cmd_calendar_list(
    cfg: &AppConfig,
    from: Option<&str>,
    to: Option<&str>,
    days: i64,
    limit: usize,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let (start_iso, end_iso) = default_calendar_window(from, to, days)?;
    let start_filter = from.map(|_| start_iso.as_str());
    let end_filter = to.map(|_| end_iso.as_str());

    if offline {
        let storage = open_storage_only(cfg)?;
        let events = storage.list_calendar_events(start_filter, end_filter, limit)?;
        return print_cal_events_output(&start_iso, &end_iso, &events, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, false, cached).await;
    if cached || daemon_warm || ttl > 0 {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:cal"], ttl) {
                let events = storage.list_calendar_events(start_filter, end_filter, limit)?;
                if !events.is_empty() {
                    return print_cal_events_output(&start_iso, &end_iso, &events, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let (events, reached_end) = match graph
        .list_calendar_view_complete(&start_iso, &end_iso, limit)
        .await
    {
        Ok(ev) => ev,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    if reached_end {
        let _ = storage.replace_calendar_events_in_window(&start_iso, &end_iso, &events);
    } else {
        let _ = storage.upsert_calendar_events(&events);
    }
    record_cli_sync_cursor(&storage, "cli:cal");

    print_cal_events_output(&start_iso, &end_iso, &events, json_out)
}

fn print_event_show_output(ev: &CalendarEvent, json_out: bool) -> Result<()> {
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "event": ev }))?);
    } else {
        println!("Subject: {}", ev.subject);
        println!("Time: {} ~ {} ({})", ev.start_iso, ev.end_iso, ev.time_zone);
        if let Some(ref loc) = ev.location {
            println!("Location: {loc}");
        }
        if let Some(org) = ev.organizer_name.as_deref().or(ev.organizer_email.as_deref()) {
            println!("Organizer: {org}");
        }
        println!("Response: {}", ev.response_status);
        if let Some(ref url) = ev.online_meeting_url {
            println!("Online Meeting: {url}");
        }
        if !ev.attendees.is_empty() {
            println!("Attendees ({}):", ev.attendees.len());
            for a in &ev.attendees {
                println!(
                    "  - {} <{}> [{} / {}]",
                    a.name.as_deref().unwrap_or(&a.address),
                    a.address,
                    a.attendee_type,
                    a.response_status
                );
            }
        }
        if !ev.body_plain.trim().is_empty() {
            println!("---\n{}", ev.body_plain.trim());
        }
    }
    Ok(())
}

async fn cmd_event_show(
    cfg: &AppConfig,
    event_id: &str,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if offline {
        let storage = open_storage_only(cfg)?;
        let events = storage.list_calendar_events(None, None, 10_000)?;
        let ev = resolver::resolve_calendar_event_from_slice(&events, event_id)?;
        return print_event_show_output(&ev, json_out);
    }

    if cached {
        if let Ok(storage) = open_storage_only(cfg) {
            let events = storage.list_calendar_events(None, None, 10_000)?;
            if let Ok(ev) = resolver::resolve_calendar_event_from_slice(&events, event_id) {
                return print_event_show_output(&ev, json_out);
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let ev = match graph.get_calendar_event(event_id).await {
        Ok(e) => e,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&ev));
    print_event_show_output(&ev, json_out)
}

#[allow(clippy::too_many_arguments)]
async fn cmd_event_create(
    cfg: &AppConfig,
    subject: &str,
    start: &str,
    end: &str,
    attendees: Vec<String>,
    location: Option<String>,
    body: Option<String>,
    is_online_meeting: bool,
    is_all_day: bool,
    time_zone: Option<String>,
    json_out: bool,
) -> Result<()> {
    if subject.trim().is_empty() {
        bail!("Event subject is empty.");
    }
    let start_iso = normalize_datetime_iso(start)?;
    let end_iso = normalize_datetime_iso(end)?;
    let body_html = body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|md| crate::parser::compile_outgoing_mail_html(md, &[], None));

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let outgoing = OutgoingEvent {
        subject: subject.to_string(),
        start_iso,
        end_iso,
        time_zone,
        is_all_day,
        location,
        body_html,
        attendees,
        is_online_meeting,
    };
    let created = match graph.create_calendar_event(&outgoing).await {
        Ok(ev) => ev,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event": created,
            }))?
        );
    } else {
        println!(
            "Created event: {} ({} ~ {}) id={}",
            created.subject, created.start_iso, created.end_iso, created.id
        );
        if let Some(ref url) = created.online_meeting_url {
            println!("Online meeting link: {url}");
        }
    }
    Ok(())
}

async fn cmd_update_event(
    cfg: &AppConfig,
    id: &str,
    subject: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let start = start.map(CalendarEditTime::parse).transpose()?;
    let end = end.map(CalendarEditTime::parse).transpose()?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let event = match graph
        .edit_calendar_event(id, subject, start.as_ref(), end.as_ref())
        .await
    {
        Ok(updated) => updated,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&event));
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "event": event }))?
        );
    } else {
        println!("Updated event {} ({}).", event.subject, event.id);
    }
    Ok(())
}

async fn cmd_event_rsvp(
    cfg: &AppConfig,
    event_id: &str,
    response: &str,
    comment: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let normalized = normalize_rsvp_response(response)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    if let Err(err) = graph
        .respond_calendar_event(event_id, normalized, comment)
        .await
    {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.update_event_response_local(event_id, rsvp_action_to_graph_status(normalized));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": event_id,
                "response": normalized,
            }))?
        );
    } else {
        println!("Event RSVP sent: {event_id} -> {normalized}");
    }
    Ok(())
}

async fn cmd_event_delete(
    cfg: &AppConfig,
    event_id: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete event {event_id}? Type 'yes' to confirm: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    if let Err(err) = graph.delete_calendar_event(event_id).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.delete_calendar_event(event_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": event_id,
            }))?
        );
    } else {
        println!("Deleted event: {event_id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// To Do & Contacts commands
// ---------------------------------------------------------------------------

fn print_todo_lists_output(lists: &[TodoList], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "lists": lists }))?
        );
    } else if lists.is_empty() {
        println!("No To Do lists found.");
    } else {
        for l in lists {
            let wk = l
                .wellknown_list_name
                .as_deref()
                .map(|w| format!(" [{w}]"))
                .unwrap_or_default();
            println!("{}{} ({})", l.display_name, wk, l.id);
        }
    }
    Ok(())
}

fn print_todo_tasks_output(list_id: &str, tasks: &[TodoTask], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "list_id": list_id,
                "tasks": tasks,
            }))?
        );
    } else if tasks.is_empty() {
        println!("No tasks found.");
    } else {
        for t in tasks {
            let check = if t.is_completed() { "x" } else { " " };
            let imp = if t.importance.eq_ignore_ascii_case("high") {
                "!"
            } else {
                " "
            };
            let due = t
                .due_date_iso
                .as_deref()
                .map(|d| format!(" (due {d})"))
                .unwrap_or_default();
            println!("[{check}]{imp} {}{}  ({})", t.title, due, t.id);
        }
    }
    Ok(())
}

async fn cmd_todo_lists(
    cfg: &AppConfig,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if offline {
        let storage = open_storage_only(cfg)?;
        let lists = storage.list_todo_lists()?;
        return print_todo_lists_output(&lists, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:todo_lists"], ttl) {
                let lists = storage.list_todo_lists()?;
                if !lists.is_empty() {
                    return print_todo_lists_output(&lists, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let cached_lists = storage.list_todo_lists()?;
    let should_fetch = refresh
        || cached_lists.is_empty()
        || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &["cli:todo_lists"], ttl));
    let lists = if should_fetch {
        let fetched = match graph.list_todo_lists().await {
            Ok(l) => l,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        storage.replace_todo_lists(&fetched)?;
        record_cli_sync_cursor(&storage, "cli:todo_lists");
        storage.list_todo_lists()?
    } else {
        cached_lists
    };

    print_todo_lists_output(&lists, json_out)
}

#[allow(clippy::too_many_arguments)]
async fn cmd_todo_tasks(
    cfg: &AppConfig,
    list: &str,
    include_completed: bool,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    if offline {
        let storage = open_storage_only(cfg)?;
        let resolved = resolver::resolve_todo_list_cached(&storage, list)?;
        let mut tasks = storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
        tasks.truncate(limit);
        return print_todo_tasks_output(&resolved.id, &tasks, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(resolved) = resolver::resolve_todo_list_cached(&storage, list) {
                let cursor_key = format!("cli:todo_tasks:{}", resolved.id);
                if cached || daemon_warm || is_cursor_fresh(&storage, &[cursor_key.as_str()], ttl)
                {
                    let mut tasks =
                        storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
                    if !tasks.is_empty() {
                        tasks.truncate(limit);
                        return print_todo_tasks_output(&resolved.id, &tasks, json_out);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let cached_tasks = storage.list_todo_tasks(Some(&resolved.id), include_completed)?;
    let cursor_key = format!("cli:todo_tasks:{}", resolved.id);
    let should_fetch = refresh
        || cached_tasks.is_empty()
        || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &[cursor_key.as_str()], ttl));
    let mut tasks = if should_fetch {
        let (fetched, reached_end) = match graph
            .list_todo_tasks_complete(&resolved.id, include_completed, limit)
            .await
        {
            Ok(t) => t,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        if reached_end {
            storage.replace_todo_tasks_for_list(&resolved.id, include_completed, &fetched)?;
        } else {
            storage.upsert_todo_tasks(&fetched)?;
        }
        record_cli_sync_cursor(&storage, &cursor_key);
        storage.list_todo_tasks(Some(&resolved.id), include_completed)?
    } else {
        cached_tasks
    };
    tasks.truncate(limit);

    print_todo_tasks_output(&resolved.id, &tasks, json_out)
}

async fn cmd_todo_add(
    cfg: &AppConfig,
    title: &str,
    list: &str,
    due: Option<&str>,
    importance: &str,
    notes: Option<String>,
    json_out: bool,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("Task title is empty.");
    }
    let due_date_iso = due.map(normalize_todo_due_iso).transpose()?;
    let norm_importance = normalize_todo_importance(importance)?;

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;

    let outgoing = OutgoingTodoTask {
        title: title.to_string(),
        body_plain: notes,
        due_date_iso,
        importance: Some(norm_importance.to_string()),
    };
    let created = match graph.create_todo_task(&resolved.id, &outgoing).await {
        Ok(t) => t,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "task": created,
            }))?
        );
    } else {
        println!("Task added: {} ({})", created.title, created.id);
    }
    Ok(())
}

pub(crate) fn normalize_todo_edit_due(due: &str) -> Result<String> {
    let due = due.trim();
    if due.parse::<NaiveDate>().is_ok() || DateTime::parse_from_rfc3339(due).is_ok() {
        return normalize_todo_due_iso(due);
    }
    bail!("Use YYYY-MM-DD or an RFC3339 timestamp for the task due date.")
}

async fn cmd_update_task(
    cfg: &AppConfig,
    id: &str,
    list: &str,
    title: Option<&str>,
    due: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let due = due.map(normalize_todo_edit_due).transpose()?;
    if title.is_some_and(|title| title.trim().is_empty()) {
        bail!("Task title must not be empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let task = match graph
        .edit_todo_task(&resolved.id, id, title, due.as_deref())
        .await
    {
        Ok(updated) => updated,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&task));
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "task": task }))?
        );
    } else {
        println!("Updated task {} ({}).", task.title, task.id);
    }
    Ok(())
}

async fn cmd_todo_done(
    cfg: &AppConfig,
    task_id: &str,
    list: &str,
    completed: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let updated = match graph
        .set_todo_task_completed(&resolved.id, task_id, completed)
        .await
    {
        Ok(t) => t,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&updated));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "task": updated,
            }))?
        );
    } else {
        println!(
            "Task marked {}: {} ({})",
            if completed { "completed" } else { "incomplete" },
            updated.title,
            updated.id
        );
    }
    Ok(())
}

async fn cmd_todo_delete(
    cfg: &AppConfig,
    task_id: &str,
    list: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved = resolver::resolve_todo_list(&graph, &storage, list, false).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete task {task_id}? Type 'yes' to confirm: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    if let Err(err) = graph.delete_todo_task(&resolved.id, task_id).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }
    let _ = storage.delete_todo_task(task_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "list_id": resolved.id,
                "task_id": task_id,
            }))?
        );
    } else {
        println!("Deleted task: {task_id}");
    }
    Ok(())
}

fn print_contact_line(c: &ContactItem) {
    let email = c.primary_email().unwrap_or("-");
    let org = c
        .company_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {s}"))
        .unwrap_or_default();
    let phone = c
        .mobile_phone
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {s}"))
        .unwrap_or_default();
    println!(
        "[{}] {} <{}>{}{} ({})",
        c.source, c.display_name, email, org, phone, c.id
    );
}

fn print_contacts_output(contacts: &[ContactItem], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "contacts": contacts }))?
        );
    } else if contacts.is_empty() {
        println!("No contacts found.");
    } else {
        for c in contacts {
            print_contact_line(c);
        }
    }
    Ok(())
}

async fn cmd_contacts(
    cfg: &AppConfig,
    query: Option<&str>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let trimmed_q = query.map(str::trim).filter(|s| !s.is_empty());

    if offline {
        let storage = open_storage_only(cfg)?;
        let contacts = storage.list_contacts(trimmed_q, limit)?;
        return print_contacts_output(&contacts, json_out);
    }

    if trimmed_q.is_none() {
        let ttl = cli_cache_ttl_secs(cfg);
        let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
        if !refresh && (cached || daemon_warm || ttl > 0) {
            if let Ok(storage) = open_storage_only(cfg) {
                if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:contacts"], ttl) {
                    let contacts = storage.list_contacts(None, limit)?;
                    if !contacts.is_empty() {
                        return print_contacts_output(&contacts, json_out);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let contacts = if let Some(q) = trimmed_q {
        if cached && !refresh {
            let local = storage.list_contacts(Some(q), limit)?;
            if !local.is_empty() {
                local
            } else {
                let fetched = match graph.search_contacts_and_directory(q, limit).await {
                    Ok(c) => c,
                    Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
                };
                let _ = storage.upsert_contacts(&fetched);
                fetched
            }
        } else {
            let fetched = match graph.search_contacts_and_directory(q, limit).await {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            let _ = storage.upsert_contacts(&fetched);
            fetched
        }
    } else {
        let ttl = cli_cache_ttl_secs(cfg);
        let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
        let cached_contacts = storage.list_contacts(None, limit)?;
        let should_fetch = refresh
            || cached_contacts.is_empty()
            || (!cached && !daemon_warm && !is_cursor_fresh(&storage, &["cli:contacts"], ttl));
        if should_fetch {
            let (fetched, reached_end) = match graph.list_contacts_complete(limit).await {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            if reached_end {
                storage.replace_personal_contacts(&fetched)?;
            } else {
                storage.upsert_contacts(&fetched)?;
            }
            record_cli_sync_cursor(&storage, "cli:contacts");
            storage.list_contacts(None, limit)?
        } else {
            cached_contacts
        }
    };

    print_contacts_output(&contacts, json_out)
}

// ---------------------------------------------------------------------------
// Sync, Update, Preview
// ---------------------------------------------------------------------------

async fn cmd_sync(cfg: &AppConfig, days: i64, limit: usize, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let limit = clamp_limit(limit);
    let (start_iso, end_iso) = default_calendar_window(None, None, days)?;

    let (folders_res, cal_res, lists_res, contacts_res) = tokio::join!(
        graph.list_mail_folders(),
        graph.list_calendar_view_complete(&start_iso, &end_iso, limit),
        graph.list_todo_lists(),
        graph.list_contacts_complete(limit),
    );

    let mut errors = Vec::new();
    let mut folder_count = 0usize;
    let mut message_count = 0usize;
    let mut event_count = 0usize;
    let mut todo_list_count = 0usize;
    let mut todo_task_count = 0usize;
    let mut contact_count = 0usize;

    match folders_res {
        Ok(folders) => {
            folder_count = folders.len();
            storage.replace_mail_folders(&folders)?;
            record_cli_sync_cursor(&storage, "cli:mail_folders");
        }
        Err(err) => errors.push(format!(
            "mail_folders: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    match crate::sync::sync_mail_folder_delta("inbox", &graph, &storage, None, false).await {
        Ok(outcome) => {
            record_cli_sync_cursor(&storage, &format!("cli:mail:{}", outcome.folder_id));
            message_count = storage
                .list_mail_summaries(Some(&outcome.folder_id), limit)
                .map(|v| v.len())
                .unwrap_or(0);
        }
        Err(err) => errors.push(format!(
            "inbox: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    match cal_res {
        Ok((events, reached_end)) => {
            event_count = events.len();
            if reached_end {
                storage.replace_calendar_events_in_window(&start_iso, &end_iso, &events)?;
            } else {
                storage.upsert_calendar_events(&events)?;
            }
            record_cli_sync_cursor(&storage, "cli:cal");
        }
        Err(err) => errors.push(format!(
            "calendar: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    match lists_res {
        Ok(lists) => {
            todo_list_count = lists.len();
            storage.replace_todo_lists(&lists)?;
            record_cli_sync_cursor(&storage, "cli:todo_lists");
            for list in &lists {
                match graph.list_todo_tasks_complete(&list.id, true, limit).await {
                    Ok((tasks, reached_end)) => {
                        todo_task_count += tasks.len();
                        if reached_end {
                            let _ = storage.replace_todo_tasks_for_list(&list.id, true, &tasks);
                        } else {
                            let _ = storage.upsert_todo_tasks(&tasks);
                        }
                        record_cli_sync_cursor(&storage, &format!("cli:todo_tasks:{}", list.id));
                    }
                    Err(err) => errors.push(format!("todo_tasks({}): {err:#}", list.display_name)),
                }
            }
        }
        Err(err) => errors.push(format!(
            "todo_lists: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    match contacts_res {
        Ok((contacts, reached_end)) => {
            contact_count = contacts.len();
            if reached_end {
                storage.replace_personal_contacts(&contacts)?;
            } else {
                storage.upsert_contacts(&contacts)?;
            }
            record_cli_sync_cursor(&storage, "cli:contacts");
        }
        Err(err) => errors.push(format!(
            "contacts: {:#}",
            wrap_graph_permission_error(&graph, err).await
        )),
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": errors.is_empty(),
                "folders": folder_count,
                "messages": message_count,
                "events": event_count,
                "todo_lists": todo_list_count,
                "todo_tasks": todo_task_count,
                "contacts": contact_count,
                "errors": errors,
            }))?
        );
    } else {
        println!(
            "Sync complete: folders={folder_count}, inbox={message_count}, events={event_count}, todo_lists={todo_list_count}, tasks={todo_task_count}, contacts={contact_count}"
        );
        for err in &errors {
            eprintln!("Warning: {err}");
        }
    }
    Ok(())
}

async fn cmd_update(
    cfg: &AppConfig,
    check: bool,
    force: bool,
    repo_override: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let repo = repo_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.github_repo);
    let result = updater::check_or_apply_update(repo, check, force).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Repository: {repo}");
        println!("Current version: v{}", result.current_version);
        println!("Latest version: v{}", result.latest_version);
        if result.updated {
            println!(
                "Update complete: {} ({})",
                result.binary_path.display(),
                result.asset_name.as_deref().unwrap_or("-")
            );
        } else if result.update_available {
            println!("New version available: {}", result.release_url);
            if check {
                println!("Running in --check mode; binary was not replaced.");
            }
        } else {
            println!("Already up to date.");
        }
    }
    Ok(())
}

pub(crate) fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let c = cols.unwrap_or_else(|| term_cols.clamp(20, 120));
    let r = rows.unwrap_or_else(|| term_rows.saturating_sub(4).clamp(10, 40));
    (c, r)
}

fn resolve_image_protocol_preference(
    cfg: &AppConfig,
    protocol: Option<&str>,
) -> Result<config::ImageProtocolPreference> {
    match protocol {
        Some(raw) => match config::ImageProtocolPreference::parse_str(raw) {
            Some(pref) => Ok(pref),
            None => bail!(
                "Invalid image protocol '{}'. Valid values: auto, kitty, iterm2, sixel, halfblock",
                raw.trim()
            ),
        },
        None => Ok(cfg.image_protocol),
    }
}

fn print_rendered_preview(rendered: &str) -> Result<()> {
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

fn cmd_preview_local_file(
    cfg: &AppConfig,
    path: &Path,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let effective_pref = resolve_image_protocol_preference(cfg, protocol)?;
    if !path.exists() || !path.is_file() {
        bail!("Image file not found: {}", path.display());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read image file: {}", path.display()))?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, effective_pref)?;
    print_rendered_preview(&rendered)
}

async fn cmd_preview_mail(
    cfg: &AppConfig,
    mail_id: &str,
    attachment: Option<&str>,
    offline: bool,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let effective_pref = resolve_image_protocol_preference(cfg, protocol)?;
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mail = load_mail_message(&storage, &graph, mail_id, offline).await?;
    let att = select_mail_attachment(&mail, None, attachment)?;
    let (_name, _ct, bytes) = load_mail_attachment_bytes(&graph, &mail.id, att, offline).await?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, effective_pref)?;
    print_rendered_preview(&rendered)
}

// ---------------------------------------------------------------------------
// Shared `pub(crate)` helpers reused by `serve.rs` and `mcp.rs`
// ---------------------------------------------------------------------------

pub(crate) fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 1_000)
}

pub(crate) fn is_forbidden_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
        || lower.contains("missing scope")
        || lower.contains("aclcheckfailed")
        || lower.contains("insufficient graph permissions")
        || lower.contains("insufficient permissions")
}

pub(crate) async fn wrap_graph_permission_error(
    graph: &GraphClient,
    err: anyhow::Error,
) -> anyhow::Error {
    let raw = format!("{err:#}");
    let token = graph.auth().current_tokens().await.map(|t| t.access_token);
    if let Some(explained) = explain_graph_permission_error(&raw, token.as_deref()) {
        anyhow::anyhow!("{explained}\nRaw error: {raw}")
    } else {
        err
    }
}

pub(crate) fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("No confirmation input received. Item was not deleted.")?;
    if !answer.trim().eq_ignore_ascii_case("yes") {
        bail!("Cancelled. Item was not deleted.");
    }
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(&['\r', '\n'][..]).to_string())
}

pub(crate) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

pub(crate) fn resolved_abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn resolve_attachment_mime_type(content_type: &str, file_name: &str) -> String {
    let ct = content_type.trim();
    if !ct.is_empty() && !ct.eq_ignore_ascii_case("application/octet-stream") {
        return ct.to_string();
    }
    if let Some(inferred) = image_mime_for_path(Path::new(file_name)) {
        return inferred.to_string();
    }
    if Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    {
        return "application/pdf".to_string();
    }
    if ct.is_empty() {
        "application/octet-stream".to_string()
    } else {
        ct.to_string()
    }
}

pub(crate) fn format_downloaded_attachment_json(
    message_id: &str,
    selected: &MailAttachment,
    downloaded: &crate::models::DownloadedFile,
) -> serde_json::Value {
    let abs_path = resolved_abs_path(&downloaded.path);
    let mime_type = resolve_attachment_mime_type(&selected.content_type, &downloaded.file_name);
    let is_image = img_preview::is_image_mime_or_name(&mime_type, Some(&downloaded.file_name));
    json!({
        "ok": true,
        "message_id": message_id,
        "attachment_id": selected.id,
        "path": abs_path.to_string_lossy(),
        "file_name": downloaded.file_name,
        "mime_type": mime_type,
        "size_bytes": downloaded.bytes_written,
        "is_image": is_image,
    })
}

pub(crate) fn select_mail_attachment<'a>(
    mail: &'a MailMessage,
    index: Option<usize>,
    selector: Option<&str>,
) -> Result<&'a MailAttachment> {
    if mail.attachments.is_empty() {
        bail!("Message has no attachments: {}", mail.id);
    }
    if let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(idx) = sel.parse::<usize>() {
            return mail.attachments.get(idx).with_context(|| {
                format!(
                    "Attachment index {idx} is out of range (total {})",
                    mail.attachments.len()
                )
            });
        }
        if let Some(found) = mail
            .attachments
            .iter()
            .find(|a| a.id == sel || a.name.eq_ignore_ascii_case(sel))
        {
            return Ok(found);
        }
        bail!("Matching attachment not found: {sel}");
    }
    if let Some(idx) = index {
        return mail.attachments.get(idx).with_context(|| {
            format!(
                "Attachment index {idx} is out of range (total {})",
                mail.attachments.len()
            )
        });
    }
    Ok(mail
        .attachments
        .iter()
        .find(|a| img_preview::is_image_mime_or_name(&a.content_type, Some(&a.name)))
        .unwrap_or(&mail.attachments[0]))
}

pub(crate) async fn load_mail_attachment_bytes(
    graph: &GraphClient,
    mail_id: &str,
    att: &MailAttachment,
    offline: bool,
) -> Result<(String, String, Vec<u8>)> {
    if let Some(ref b64) = att.content_bytes_b64 {
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .with_context(|| format!("Failed to decode base64 attachment: {}", att.name))?;
        return Ok((att.name.clone(), att.content_type.clone(), bytes));
    }
    if offline {
        bail!(
            "Cannot fetch uncached attachment bytes in offline mode: {}",
            att.name
        );
    }
    graph.fetch_mail_attachment_bytes(mail_id, &att.id).await
}

pub(crate) fn normalize_mail_importance(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low"),
        "normal" => Ok("normal"),
        "high" => Ok("high"),
        other => bail!("Unsupported mail importance: '{other}' (low | normal | high)"),
    }
}

pub(crate) fn normalize_datetime_iso(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Timestamp string is empty.");
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d_%H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            let dt = Local
                .from_local_datetime(&naive)
                .earliest()
                .or_else(|| Local.from_local_datetime(&naive).latest())
                .context("Failed to convert local timestamp")?;
            return Ok(dt.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, true));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).context("Invalid date")?;
        let dt = Local
            .from_local_datetime(&naive)
            .earliest()
            .or_else(|| Local.from_local_datetime(&naive).latest())
            .context("Failed to convert local date")?;
        return Ok(dt.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, true));
    }
    bail!("Invalid timestamp '{trimmed}' (expected ISO-8601 or YYYY-MM-DD HH:MM)")
}

pub(crate) fn default_calendar_window(
    from: Option<&str>,
    to: Option<&str>,
    days: i64,
) -> Result<(String, String)> {
    let clamped_days = days.clamp(1, 365);
    let start_iso = match from.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => normalize_datetime_iso(s)?,
        None => {
            let today = Local::now().date_naive();
            let naive = today.and_hms_opt(0, 0, 0).context("Invalid start of day")?;
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap_or_else(Local::now)
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        }
    };
    let end_iso = match to.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => normalize_datetime_iso(e)?,
        None => {
            let start_dt = DateTime::parse_from_rfc3339(&start_iso)?;
            (start_dt + chrono::Duration::days(clamped_days))
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        }
    };
    Ok((start_iso, end_iso))
}

pub(crate) fn normalize_rsvp_response(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "accept" | "accepted" => Ok("accept"),
        "tentative" | "tentativelyaccept" | "tentatively_accept" | "tentativelyaccepted" => {
            Ok("tentativelyAccept")
        }
        "decline" | "declined" => Ok("decline"),
        other => bail!("Unsupported RSVP response: '{other}' (accept | tentative | decline)"),
    }
}

pub(crate) fn normalize_todo_due_iso(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Due date is empty.");
    }
    if NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok() {
        return Ok(format!("{trimmed}T00:00:00Z"));
    }
    normalize_datetime_iso(trimmed)
}

pub(crate) fn normalize_todo_importance(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low"),
        "normal" => Ok("normal"),
        "high" => Ok("high"),
        other => bail!("Unsupported To Do importance: '{other}' (low | normal | high)"),
    }
}

fn format_unix_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let single = input.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                json: true
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
            } => {
                assert_eq!(bind, "127.0.0.1");
                assert_eq!(port, 3939);
                assert!(api_key.is_none());
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
            } => {
                assert_eq!(bind, "0.0.0.0");
                assert_eq!(port, 4040);
                assert_eq!(api_key.as_deref(), Some("secret-key"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

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
}
