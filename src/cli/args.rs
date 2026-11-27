//! CLI argument definitions (`Cli`, `Command`, `DaemonCommand`).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config;

pub(super) const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

pub(super) const ENV_AFTER_HELP: &str = "\
Environment Variables:
  OUTLOOK_TOKEN_FILE=<PATH>             Load an external token file (JWT or JSON) directly (skips OS keyring)
  OUTLOOK_TOKEN_REFRESH_CMD=<CMD>       Command hook to run automatically when the token is about to expire
  OUTLOOK_TOKEN_REFRESH_MINS=<MINS>     Minutes remaining before triggering refresh hook [default: 5]
  OUTLOOK_ACCESS_TOKEN=<JWT>            Inject a Bearer access token directly (skips OS keyring)
  OUTLOOK_SHARE_M365_AUTH=<1|0>         Reuse stored teams-cli / TEAMS_TOKEN_FILE session when no outlook-cli token exists [default: 1]
  OUTLOOK_DISABLE_KEYRING=1             Disable the OS keyring (use credentials.json file only)
  OUTLOOK_CLIENT_ID / OUTLOOK_TENANT_ID Override Entra app Client ID / Tenant ID
  OUTLOOK_CONFIG_DIR / OUTLOOK_DATA_DIR Override config and local SQLite cache directories
  OUTLOOK_SERVE_ALLOWED_HOSTS=<HOSTS>   Extra Host names (comma-separated) a keyless `serve` accepts besides loopback
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
        /// Maximum number of items to return (with --json the default is 50)
        #[arg(long, visible_alias = "top")]
        limit: Option<usize>,
        /// With --json, continue from a previous response's next_cursor
        #[arg(long, requires = "json", conflicts_with = "refresh", value_name = "CURSOR")]
        cursor: Option<String>,
    },
    /// List messages in Inbox or a specified mail folder
    #[command(visible_aliases = ["ls", "inbox"])]
    Mail {
        /// Folder name, well-known alias (inbox/sent/drafts/archive/trash/junk), or ID
        #[arg(long, default_value = "inbox")]
        folder: String,
        /// Maximum number of messages to return (a first online listing of a folder syncs the whole folder once)
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
        /// With --json, return every cached field for each message (no attachment bytes)
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
        /// With --json, continue from a previous response's next_cursor
        #[arg(long, requires = "json", conflicts_with = "refresh", value_name = "CURSOR")]
        cursor: Option<String>,
        /// With --json, same as the default list output (kept for compatibility)
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
        /// With --json, return every field including HTML and attachment metadata
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
        /// With --json, same as the default compact output (kept for compatibility)
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
        /// With --json, include the full moved message (no attachment bytes)
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Archive a mail message (shortcut for `outlook move <MESSAGE_ID> archive`)
    Archive {
        message_id: String,
        #[arg(long)]
        json: bool,
        /// With --json, include the full moved message (no attachment bytes)
        #[arg(long, requires = "json")]
        full: bool,
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
        /// With --json, return full message fields in remote results (no attachment bytes)
        #[arg(long, requires = "json")]
        full: bool,
        /// With --remote --json, continue from a previous response's next_cursor
        #[arg(long, requires_all = ["remote", "json"], value_name = "CURSOR")]
        cursor: Option<String>,
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
        /// Use the cached message and cached attachment bytes without calling Graph
        #[arg(long)]
        offline: bool,
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
        /// With --json, include body_html on each event
        #[arg(long, requires = "json")]
        full: bool,
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
        /// With --json, return every field including body_html
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
        /// With --json, same as the default compact output (kept for compatibility)
        #[arg(long, requires = "json")]
        compact: bool,
    },
    /// Create a new calendar event
    #[command(name = "create-event")]
    CreateEvent {
        #[arg(long, required = true)]
        subject: String,
        /// Start: RFC3339 (exact instant), wall clock `YYYY-MM-DD HH:MM` (machine zone, or --time-zone), or `YYYY-MM-DD` with --all-day
        #[arg(long, required = true, value_name = "ISO")]
        start: String,
        /// End, in the same form as --start. With --all-day the end date is exclusive
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
        /// All-day event: --start and --end must be `YYYY-MM-DD`; end is exclusive (next day for a one-day event)
        #[arg(long = "all-day")]
        all_day: bool,
        /// IANA or Windows zone. Wall-clock times are read in this zone; RFC3339 times stay exact instants (UTC)
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
        /// Maximum number of items to return (with --json the default is 50)
        #[arg(long, visible_alias = "top")]
        limit: Option<usize>,
        /// With --json, continue from a previous response's next_cursor
        #[arg(long, requires = "json", conflicts_with = "refresh", value_name = "CURSOR")]
        cursor: Option<String>,
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
        /// Secret Bearer API key (mandatory when binding to a non-loopback address). Without it a
        /// loopback server generates a random token for the run and prints it to stderr
        #[arg(long = "api-key", visible_alias = "token", value_name = "KEY")]
        api_key: Option<String>,
        /// Serve loopback clients without authentication instead of generating a token
        #[arg(long = "allow-keyless", conflicts_with = "api_key")]
        allow_keyless: bool,
        /// Browser origin allowed to call the API, e.g. http://localhost:5173 (repeatable; none by default)
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origin: Vec<String>,
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

impl Command {
    pub(crate) fn json_requested(&self) -> bool {
        match self {
            Command::Login { json, .. }
            | Command::Logout { json }
            | Command::Whoami { json }
            | Command::Doctor { json, .. }
            | Command::Folders { json, .. }
            | Command::Mail { json, .. }
            | Command::Read { json, .. }
            | Command::Send { json, .. }
            | Command::Reply { json, .. }
            | Command::Forward { json, .. }
            | Command::EditDraft { json, .. }
            | Command::Mark { json, .. }
            | Command::Move { json, .. }
            | Command::Archive { json, .. }
            | Command::Delete { json, .. }
            | Command::Search { json, .. }
            | Command::Download { json, .. }
            | Command::Calendar { json, .. }
            | Command::Event { json, .. }
            | Command::CreateEvent { json, .. }
            | Command::UpdateEvent { json, .. }
            | Command::Rsvp { json, .. }
            | Command::DeleteEvent { json, .. }
            | Command::Tasks { json, .. }
            | Command::TaskLists { json, .. }
            | Command::AddTask { json, .. }
            | Command::Done { json, .. }
            | Command::UpdateTask { json, .. }
            | Command::DeleteTask { json, .. }
            | Command::Contacts { json, .. }
            | Command::Sync { json, .. }
            | Command::Update { json, .. } => *json,
            Command::Daemon { command } => match command {
                DaemonCommand::Start { json }
                | DaemonCommand::Stop { json }
                | DaemonCommand::Status { json }
                | DaemonCommand::Restart { json } => *json,
                DaemonCommand::Run => false,
            },
            Command::Preview { .. }
            | Command::PreviewFile { .. }
            | Command::Tui
            | Command::Serve { .. }
            | Command::Mcp => false,
        }
    }
}
