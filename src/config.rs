//! Application configuration, local paths, settings (`config.toml`), and mail drafts (`drafts.json`).
//!
//! Shared foundation module: treat public signatures here as stable across all modules.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "outlook-cli";
pub const LEGACY_TEAMS_APP_DIR_NAME: &str = "teams-cli";
pub const DEFAULT_GRAPH_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
pub const DEFAULT_AUTH_BASE_URL: &str = "https://login.microsoftonline.com";
/// Default Microsoft multi-tenant public client ID fallback when user does not supply a custom Azure App Client ID.
/// Users can always override via `--client-id`, `OUTLOOK_CLIENT_ID`, `TEAMS_CLIENT_ID`, `AZURE_CLIENT_ID`, or `config.toml`.
pub const DEFAULT_PUBLIC_CLIENT_ID: &str = "14d82eec-204b-4c2f-b7e8-296a70dab67e";
pub const DEFAULT_TENANT_ID: &str = "common";
pub const DEFAULT_GITHUB_REPO: &str = "BiteFoundry/outlook";
pub const DEFAULT_SERVE_PORT: u16 = 3939;

/// Message/mail list density mode in TUI (`Cozy` default vs `Compact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DensityMode {
    #[default]
    Cozy,
    Compact,
}

impl DensityMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cozy => Self::Compact,
            Self::Compact => Self::Cozy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cozy => "Cozy",
            Self::Compact => "Compact",
        }
    }
}

/// Terminal color theme preset (`Auto` detects `NO_COLOR`, `COLORTERM`, and `TERM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    #[default]
    Auto,
    Dark,
    Light,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ThemePreset {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::Ansi256,
            Self::Ansi256 => Self::Ansi16,
            Self::Ansi16 => Self::NoColor,
            Self::NoColor => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Dark => "Midnight Dark",
            Self::Light => "Daylight Light",
            Self::Ansi256 => "ANSI 256",
            Self::Ansi16 => "ANSI 16",
            Self::NoColor => "No Color",
        }
    }
}

/// Terminal image rendering protocol preference (`auto` detects Kitty / iTerm2 / Sixel / Halfblock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(alias = "iterm")]
    Iterm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }
}

/// Entry persona (`outlook`), kept as an enum for stable configuration/caller ergonomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    #[default]
    Outlook,
}

impl Persona {
    /// Detects entry persona (pure Microsoft Outlook).
    pub fn detect() -> Self {
        Self::Outlook
    }

    pub fn bin_name(self) -> &'static str {
        match self {
            Self::Outlook => "outlook",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Entra ID Application (client) ID.
    pub client_id: String,
    /// Entra ID Tenant ID (`common`, `organizations`, domain, or tenant GUID).
    pub tenant_id: String,
    /// Optional OAuth scopes override. Empty uses the standard Microsoft Outlook delegated scope set.
    pub scopes: Vec<String>,
    /// Base URL for Microsoft Graph v1.0 (overridable for sovereign clouds or mock testing).
    pub graph_base_url: String,
    /// Base URL for Entra OAuth2 endpoints (overridable for sovereign clouds or mock testing).
    pub auth_base_url: String,
    /// GitHub `owner/repo` used by `outlook update`.
    pub github_repo: String,
    /// Enable desktop toast/bell notifications in TUI.
    pub notify: bool,
    /// Show left navigation rail in wide layout.
    pub show_nav_rail: bool,
    /// Group emails by `conversation_id` in Mail list (toggleable with `t`).
    pub group_threads: bool,
    /// Focused Inbox filter (toggleable with `F`).
    pub focused_only: bool,
    /// Render inline image thumbnails directly inside terminal Mail views.
    pub inline_images: bool,
    /// Preferred terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`).
    pub image_protocol: ImageProtocolPreference,
    /// Custom download directory. Empty defaults to `Downloads/outlook`.
    pub download_dir: String,
    /// Active folder fast delta poll interval (ms) in TUI.
    pub active_poll_ms: u64,
    /// Background folders/unread poll interval (ms).
    pub mail_poll_ms: u64,
    /// Calendar & tasks background poll interval (ms).
    pub calendar_poll_ms: u64,
    /// Optional external token file path (`OUTLOOK_TOKEN_FILE` or `config.toml`), read live on demand.
    pub token_file: String,
    /// Refresh command string (`OUTLOOK_TOKEN_REFRESH_CMD` or `config.toml`), executed when token is missing or within `token_refresh_mins` of expiry.
    pub token_refresh_cmd: String,
    /// Refresh threshold in minutes before token expiry (`OUTLOOK_TOKEN_REFRESH_MINS`, default `5`).
    pub token_refresh_mins: u64,
    /// Allow falling back to `TEAMS_TOKEN_FILE` or `teams-cli` stored M365 tokens when no `outlook-cli` token is configured (`OUTLOOK_SHARE_M365_AUTH`, default `true`).
    pub share_m365_auth: bool,
    /// CLI local cache TTL in seconds (`OUTLOOK_CLI_CACHE_SECS`, default `0`).
    /// Defaults to `0` so CLI invocations fetch fresh unless `OUTLOOK_CLI_CACHE_SECS`, `cli_cache_ttl_secs`, or `--cached` is used.
    pub cli_cache_ttl_secs: u64,
    /// Automatically start and attach to the lightweight background sync daemon (`OUTLOOK_AUTO_DAEMON=1|true`, default `false`).
    pub auto_daemon: bool,
    /// TUI message density (`cozy` or `compact`).
    pub density: DensityMode,
    /// TUI color theme preset (`auto`, `dark`, `light`, `ansi256`, `ansi16`, `nocolor`).
    pub theme: ThemePreset,
    /// User-pinned mail folder IDs shown at the top of the Mail sidebar.
    pub favorite_folders: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            client_id: first_non_empty_env(&[
                "OUTLOOK_CLIENT_ID",
                "TEAMS_CLIENT_ID",
                "AZURE_CLIENT_ID",
            ])
            .unwrap_or_else(|| DEFAULT_PUBLIC_CLIENT_ID.to_string()),
            tenant_id: first_non_empty_env(&[
                "OUTLOOK_TENANT_ID",
                "TEAMS_TENANT_ID",
                "AZURE_TENANT_ID",
            ])
            .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string()),
            scopes: Vec::new(),
            graph_base_url: first_non_empty_env(&["OUTLOOK_GRAPH_BASE_URL"])
                .unwrap_or_else(|| DEFAULT_GRAPH_BASE_URL.to_string()),
            auth_base_url: first_non_empty_env(&["OUTLOOK_AUTH_BASE_URL"])
                .unwrap_or_else(|| DEFAULT_AUTH_BASE_URL.to_string()),
            github_repo: first_non_empty_env(&["OUTLOOK_GITHUB_REPO"])
                .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string()),
            notify: true,
            show_nav_rail: true,
            group_threads: true,
            focused_only: false,
            inline_images: true,
            image_protocol: first_non_empty_env(&[
                "OUTLOOK_IMAGE_PROTOCOL",
                "TEAMS_IMAGE_PROTOCOL",
            ])
            .and_then(|s| ImageProtocolPreference::parse_str(&s))
            .unwrap_or_default(),
            download_dir: String::new(),
            active_poll_ms: 5_000,
            mail_poll_ms: 15_000,
            calendar_poll_ms: 60_000,
            token_file: std::env::var("OUTLOOK_TOKEN_FILE")
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
            token_refresh_cmd: std::env::var("OUTLOOK_TOKEN_REFRESH_CMD")
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
            token_refresh_mins: std::env::var("OUTLOOK_TOKEN_REFRESH_MINS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(5),
            share_m365_auth: std::env::var("OUTLOOK_SHARE_M365_AUTH")
                .ok()
                .and_then(|s| parse_env_bool(&s))
                .unwrap_or(true),
            cli_cache_ttl_secs: std::env::var("OUTLOOK_CLI_CACHE_SECS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0),
            auto_daemon: std::env::var("OUTLOOK_AUTO_DAEMON")
                .ok()
                .and_then(|s| parse_env_bool(&s))
                .unwrap_or(false),
            density: DensityMode::default(),
            theme: ThemePreset::default(),
            favorite_folders: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let Ok(mut cfg) = Self::load_from(&config_file_path()) else {
            return Self::default();
        };
        // Environment variables take precedence over file values when set.
        if let Some(cid) =
            first_non_empty_env(&["OUTLOOK_CLIENT_ID", "TEAMS_CLIENT_ID", "AZURE_CLIENT_ID"])
        {
            cfg.client_id = cid;
        }
        if let Some(tid) =
            first_non_empty_env(&["OUTLOOK_TENANT_ID", "TEAMS_TENANT_ID", "AZURE_TENANT_ID"])
        {
            cfg.tenant_id = tid;
        }
        if let Some(gurl) = first_non_empty_env(&["OUTLOOK_GRAPH_BASE_URL"]) {
            cfg.graph_base_url = gurl;
        }
        if let Some(aurl) = first_non_empty_env(&["OUTLOOK_AUTH_BASE_URL"]) {
            cfg.auth_base_url = aurl;
        }
        if let Some(repo) = first_non_empty_env(&["OUTLOOK_GITHUB_REPO"]) {
            cfg.github_repo = repo;
        }
        if let Some(tf) = first_non_empty_env(&["OUTLOOK_TOKEN_FILE"]) {
            cfg.token_file = tf;
        }
        if let Some(rcmd) = first_non_empty_env(&["OUTLOOK_TOKEN_REFRESH_CMD"]) {
            cfg.token_refresh_cmd = rcmd;
        }
        if let Ok(rmins) = std::env::var("OUTLOOK_TOKEN_REFRESH_MINS") {
            if let Ok(parsed) = rmins.trim().parse::<u64>() {
                cfg.token_refresh_mins = parsed;
            }
        }
        if let Ok(share) = std::env::var("OUTLOOK_SHARE_M365_AUTH") {
            if let Some(parsed) = parse_env_bool(&share) {
                cfg.share_m365_auth = parsed;
            }
        }
        if let Ok(ttl) = std::env::var("OUTLOOK_CLI_CACHE_SECS") {
            if let Ok(parsed) = ttl.trim().parse::<u64>() {
                cfg.cli_cache_ttl_secs = parsed;
            }
        }
        if let Ok(ad) = std::env::var("OUTLOOK_AUTO_DAEMON") {
            if let Some(parsed) = parse_env_bool(&ad) {
                cfg.auto_daemon = parsed;
            }
        }
        if let Some(proto) =
            first_non_empty_env(&["OUTLOOK_IMAGE_PROTOCOL", "TEAMS_IMAGE_PROTOCOL"])
        {
            if let Some(parsed) = ImageProtocolPreference::parse_str(&proto) {
                cfg.image_protocol = parsed;
            }
        }
        cfg
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        ensure_parent(path)?;
        let raw = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, raw.as_bytes())
    }

    pub fn effective_scopes(&self) -> Vec<String> {
        if !self.scopes.is_empty() {
            return self.scopes.clone();
        }
        crate::auth::DEFAULT_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect()
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
        }
    }

    pub fn resolved_token_file(&self) -> Option<PathBuf> {
        let trimmed = self.token_file.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(expand_tilde(trimmed))
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("OUTLOOK_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir.trim());
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("OUTLOOK_DATA_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir.trim());
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

/// Returns the legacy `teams-cli` config directory so `auth.rs` can inspect
/// `teams-cli/config.toml` and `teams-cli/credentials.json` read-only when `share_m365_auth` is enabled.
pub fn legacy_teams_config_dir() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var("TEAMS_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return Some(PathBuf::from(override_dir.trim()));
        }
    }
    dirs::config_dir().map(|dir| dir.join(LEGACY_TEAMS_APP_DIR_NAME))
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credential_fallback_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// The pre-0.3.1 draft file, shared by every account on the OS profile. Kept only so
/// [`adopt_legacy_drafts`] can move it; live drafts are per user (see [`drafts_path_for`]).
pub fn drafts_path() -> PathBuf {
    config_dir().join("drafts.json")
}

/// File-system-safe name of a user's bucket: the id with every character outside `[A-Za-z0-9_-]`
/// replaced by `_`, or `default` when the id is unknown.
pub(crate) fn user_bucket(user_id: Option<&str>) -> String {
    user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("default")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Per-user draft file: `<config dir>/drafts/<bucket>.json`.
pub fn drafts_path_for(user_id: Option<&str>) -> PathBuf {
    config_dir()
        .join("drafts")
        .join(format!("{}.json", user_bucket(user_id)))
}

/// Moves the legacy shared `drafts.json` to `user_id`'s draft file when that user has none yet,
/// and returns the per-user path. The file is moved, not copied, so exactly one account adopts it;
/// if it cannot be moved it stays where it is and the user starts with an empty store.
pub fn adopt_legacy_drafts(user_id: Option<&str>) -> PathBuf {
    let per_user = drafts_path_for(user_id);
    let legacy = drafts_path();
    if per_user.exists() || !legacy.is_file() || ensure_parent(&per_user).is_err() {
        return per_user;
    }
    if fs::rename(&legacy, &per_user).is_err() && fs::copy(&legacy, &per_user).is_ok() {
        // Rename can fail across volumes; after a successful copy the legacy file must go, or a
        // second account would adopt the same drafts.
        if fs::remove_file(&legacy).is_err() {
            let _ = fs::remove_file(&per_user);
        }
    }
    per_user
}

pub fn db_path(user_id: Option<&str>) -> PathBuf {
    data_dir().join(user_bucket(user_id)).join("cache.db")
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("outlook")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` using a unique temp file (`create_new(true)`),
/// flushing before rename and avoiding naked deletion of the destination before the
/// replacement is ready. Files whose name contains `"credentials"` or `"drafts"` default to `0o600` on Unix.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let lower = n.to_ascii_lowercase();
            lower.contains("credentials") || lower.contains("drafts")
        })
        .unwrap_or(false);
    write_atomic_with_mode(path, data, is_secret)
}

/// Atomically writes `data` to `path` with `0o600` permissions at creation time on Unix.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, data, true)
}

fn write_atomic_with_mode(path: &Path, data: &[u8], mode_0600: bool) -> Result<()> {
    ensure_parent(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{pid}.{seq}.tmp"));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode_0600 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode_0600;

    let write_res = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(rename_err) = fs::rename(&tmp, path) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, path, parent, name, pid, seq, rename_err)
        {
            let _ = fs::remove_file(&tmp);
            return Err(replace_err);
        }
    }
    Ok(())
}

fn replace_existing_with_backup(
    tmp: &Path,
    path: &Path,
    parent: &Path,
    name: &str,
    pid: u32,
    seq: u64,
    original_err: std::io::Error,
) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Err(original_err)
            .with_context(|| format!("Failed to save file: {}", path.display()));
    }
    let backup = parent.join(format!(".{name}.{pid}.{seq}.bak"));
    fs::rename(path, &backup)
        .with_context(|| format!("Failed to back up existing file: {}", path.display()))?;
    match fs::rename(tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, path);
            Err(err).with_context(|| format!("Atomic file save failed: {}", path.display()))
        }
    }
}

/// Structured draft entry for composing, replying to, or forwarding an email.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailDraftEntry {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub reply_to_id: Option<String>,
    pub forward_of_id: Option<String>,
    pub compose_id: String,
    pub draft_id: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub inline_images: Vec<crate::models::InlineImageUpload>,
    pub thread_headers: Option<MailDraftHeaders>,
    pub synced: MailDraftSync,
}

/// Last acknowledged Graph write for this composition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailDraftSync {
    pub fields: Option<MailDraftFields>,
    pub files: Vec<PathBuf>,
    pub image_ids: Vec<String>,
    pub thread_quote_html: Option<String>,
    pub thread_creation_pending: bool,
    pub send_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailDraftHeaders {
    pub subject: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailDraftFields {
    pub subject: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub body_hash: String,
}

impl MailDraftEntry {
    pub fn is_empty(&self) -> bool {
        self.to.trim().is_empty()
            && self.cc.trim().is_empty()
            && self.bcc.trim().is_empty()
            && self.subject.trim().is_empty()
            && self.body.trim().is_empty()
            && self.draft_id.is_none()
            && self.attachments.is_empty()
            && self.inline_images.is_empty()
    }
}

/// Persistent draft store backed by a per-user `drafts/<bucket>.json` (`0600` permissions on Unix).
/// Supports structured `MailDraftEntry` records alongside simple string entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DraftStore {
    pub mail: HashMap<String, MailDraftEntry>,
    pub text: HashMap<String, String>,
}

impl DraftStore {
    pub fn load() -> Self {
        Self::load_from(&drafts_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        if let Ok(store) = serde_json::from_str::<Self>(&raw) {
            let object: HashMap<String, serde_json::Value> = serde_json::from_str(&raw)?;
            if object.contains_key("mail") || object.contains_key("text") {
                return Ok(store);
            }
        }
        if let Ok(mail_map) = serde_json::from_str::<HashMap<String, MailDraftEntry>>(&raw) {
            return Ok(Self {
                mail: mail_map,
                text: HashMap::new(),
            });
        }
        if let Ok(text_map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
            return Ok(Self {
                mail: HashMap::new(),
                text: text_map,
            });
        }
        Ok(serde_json::from_str::<Self>(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&drafts_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic_0600(path, &bytes)
    }

    pub fn get_mail(&self, key: &str) -> Option<&MailDraftEntry> {
        self.mail.get(key)
    }

    pub fn set_mail(&mut self, key: impl Into<String>, draft: MailDraftEntry) {
        let key = key.into();
        if draft.is_empty() {
            self.mail.remove(&key);
        } else {
            self.mail.insert(key, draft);
        }
    }

    pub fn remove_mail(&mut self, key: &str) -> Option<MailDraftEntry> {
        self.mail.remove(key)
    }

    pub fn get_text(&self, key: &str) -> Option<&str> {
        self.text.get(key).map(String::as_str)
    }

    pub fn set_text(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if value.trim().is_empty() {
            self.text.remove(&key);
        } else {
            self.text.insert(key, value);
        }
    }
}

pub fn load_drafts() -> HashMap<String, String> {
    load_drafts_from(&drafts_path()).unwrap_or_default()
}

pub fn load_drafts_from(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
        return Ok(map);
    }
    Ok(DraftStore::load_from(path)?.text)
}

pub fn save_drafts(drafts: &HashMap<String, String>) -> Result<()> {
    save_drafts_to(drafts, &drafts_path())
}

pub fn save_drafts_to(drafts: &HashMap<String, String>, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(drafts)?;
    write_atomic_0600(path, &bytes)
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    for &key in keys {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn parse_env_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for &(key, val) in vars {
            saved.push((key, std::env::var(key).ok()));
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { saved }
    }

    pub(crate) fn clear_auth_env() -> Self {
        Self::set(&[
            ("OUTLOOK_CONFIG_DIR", None),
            ("OUTLOOK_DATA_DIR", None),
            ("OUTLOOK_CLIENT_ID", None),
            ("OUTLOOK_TENANT_ID", None),
            ("OUTLOOK_CLIENT_SECRET", None),
            ("OUTLOOK_GRAPH_BASE_URL", None),
            ("OUTLOOK_AUTH_BASE_URL", None),
            ("OUTLOOK_GITHUB_REPO", None),
            ("OUTLOOK_TOKEN_FILE", None),
            ("OUTLOOK_TOKEN_REFRESH_CMD", None),
            ("OUTLOOK_TOKEN_REFRESH_MINS", None),
            ("OUTLOOK_SHARE_M365_AUTH", None),
            ("OUTLOOK_CLI_CACHE_SECS", None),
            ("OUTLOOK_AUTO_DAEMON", None),
            ("OUTLOOK_IMAGE_PROTOCOL", None),
            ("OUTLOOK_ACCESS_TOKEN", None),
            ("OUTLOOK_DISABLE_KEYRING", None),
            ("OUTLOOK_NO_BROWSER", None),
            ("OUTLOOK_TOKEN_ORIGIN", None),
            ("TEAMS_CONFIG_DIR", None),
            ("TEAMS_DATA_DIR", None),
            ("TEAMS_CLIENT_ID", None),
            ("TEAMS_TENANT_ID", None),
            ("TEAMS_IMAGE_PROTOCOL", None),
            ("TEAMS_TOKEN_FILE", None),
            ("TEAMS_NO_BROWSER", None),
            ("TEAMS_ACCESS_TOKEN", None),
            ("AZURE_CLIENT_ID", None),
            ("AZURE_TENANT_ID", None),
            ("AZURE_CLIENT_SECRET", None),
        ])
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, orig) in self.saved.drain(..) {
            match orig {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_toml_parses_legacy_unknown_keys_and_outlook_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.toml");
        fs::write(
            &legacy_path,
            "notify = false\nactive_poll_ms = 7500\nbackground_all_personas = true\ncli_cache_ttl_secs = 45\ngroup_threads = false\nfocused_only = true\n",
        )
        .unwrap();

        let _lock = TEST_ENV_LOCK.blocking_lock();
        let _guard = EnvVarGuard::set(&[
            ("OUTLOOK_CLI_CACHE_SECS", None),
            ("OUTLOOK_SHARE_M365_AUTH", None),
        ]);

        let cfg = AppConfig::load_from(&legacy_path).unwrap();
        assert!(!cfg.notify);
        assert_eq!(cfg.active_poll_ms, 7500);
        assert_eq!(cfg.mail_poll_ms, 15_000);
        assert_eq!(cfg.calendar_poll_ms, 60_000);
        assert_eq!(cfg.cli_cache_ttl_secs, 45);
        assert!(!cfg.group_threads);
        assert!(cfg.focused_only);
        assert!(cfg.share_m365_auth);
    }

    #[test]
    fn test_app_config_env_overrides_and_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        fs::write(
            &cfg_path,
            "cli_cache_ttl_secs = 15\nshare_m365_auth = true\n",
        )
        .unwrap();

        let _lock = TEST_ENV_LOCK.blocking_lock();
        let dir_str = dir.path().to_string_lossy().into_owned();

        {
            let _guard = EnvVarGuard::set(&[
                ("OUTLOOK_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_CONFIG_DIR", Some("/should/not/affect/outlook")),
                ("OUTLOOK_CLI_CACHE_SECS", Some("120")),
                ("OUTLOOK_CLIENT_ID", Some("outlook-cid")),
                ("TEAMS_CLIENT_ID", Some("teams-cid")),
                ("OUTLOOK_TENANT_ID", None),
                ("TEAMS_TENANT_ID", Some("teams-tid")),
                ("AZURE_TENANT_ID", Some("azure-tid")),
                ("OUTLOOK_SHARE_M365_AUTH", Some("false")),
                ("OUTLOOK_TOKEN_FILE", Some("/tmp/outlook-token.txt")),
            ]);
            let cfg = AppConfig::load();
            assert_eq!(config_dir(), PathBuf::from(&dir_str));
            assert_eq!(cfg.cli_cache_ttl_secs, 120);
            assert_eq!(cfg.client_id, "outlook-cid");
            assert_eq!(cfg.tenant_id, "teams-tid");
            assert!(!cfg.share_m365_auth);
            assert_eq!(cfg.token_file, "/tmp/outlook-token.txt");
            assert_eq!(
                legacy_teams_config_dir(),
                Some(PathBuf::from("/should/not/affect/outlook"))
            );
        }

        {
            let _guard = EnvVarGuard::set(&[
                ("OUTLOOK_CONFIG_DIR", Some(&dir_str)),
                ("OUTLOOK_DATA_DIR", Some(&dir_str)),
                ("TEAMS_CONFIG_DIR", None),
                ("TEAMS_DATA_DIR", Some("/should/not/affect/data")),
                ("OUTLOOK_CLI_CACHE_SECS", None),
                ("OUTLOOK_CLIENT_ID", None),
                ("TEAMS_CLIENT_ID", None),
                ("AZURE_CLIENT_ID", Some("azure-cid")),
                ("OUTLOOK_SHARE_M365_AUTH", None),
            ]);
            assert_eq!(data_dir(), PathBuf::from(&dir_str));
            let def = AppConfig::default();
            assert_eq!(def.cli_cache_ttl_secs, 0);
            assert_eq!(def.client_id, "azure-cid");
            assert!(def.share_m365_auth);
            assert!(def.group_threads);
            assert!(!def.focused_only);
            let scopes = def.effective_scopes();
            assert!(scopes.iter().any(|s| s == "Mail.ReadWrite"));
            assert!(scopes.iter().any(|s| s == "Mail.Send"));
            assert!(scopes.iter().any(|s| s == "Calendars.ReadWrite"));
            assert!(scopes.iter().any(|s| s == "Contacts.Read"));
            assert!(scopes.iter().any(|s| s == "User.ReadBasic.All"));
            assert!(!scopes.iter().any(|s| s == "Contacts.ReadWrite"));
            assert!(scopes.iter().any(|s| s == "Tasks.ReadWrite"));
            assert!(!scopes.iter().any(|s| s.starts_with("Chat.")));
            assert!(!scopes.iter().any(|s| s.starts_with("Channel")));
            assert!(!scopes.iter().any(|s| s.starts_with("Notes.")));

            if let Some(legacy_dir) = legacy_teams_config_dir() {
                assert_eq!(
                    legacy_dir.file_name().and_then(|n| n.to_str()),
                    Some(LEGACY_TEAMS_APP_DIR_NAME)
                );
            }
        }
        assert_eq!(Persona::detect(), Persona::Outlook);
        assert_eq!(Persona::Outlook.bin_name(), "outlook");
        assert_eq!(DEFAULT_SERVE_PORT, 3939);
        assert_eq!(
            default_download_dir().file_name().and_then(|n| n.to_str()),
            Some("outlook")
        );
    }

    #[test]
    fn test_parse_env_bool_values() {
        assert_eq!(parse_env_bool("1"), Some(true));
        assert_eq!(parse_env_bool(" true "), Some(true));
        assert_eq!(parse_env_bool("TRUE"), Some(true));
        assert_eq!(parse_env_bool("0"), Some(false));
        assert_eq!(parse_env_bool("false"), Some(false));
        assert_eq!(parse_env_bool("invalid"), None);
    }

    #[test]
    fn test_write_atomic_and_draft_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub/settings.toml");
        write_atomic(&target, b"first = 1\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first = 1\n");

        write_atomic(&target, b"second = 2\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second = 2\n");

        let drafts_file = dir.path().join("sub/drafts.json");
        let mut store = DraftStore::default();
        store.set_mail(
            "compose",
            MailDraftEntry {
                to: "alice@example.com".to_string(),
                cc: String::new(),
                bcc: String::new(),
                subject: "Quarterly update".to_string(),
                body: "Hello Alice".to_string(),
                reply_to_id: Some("msg-1".to_string()),
                forward_of_id: None,
                draft_id: Some("draft-graph-id".to_string()),
                ..Default::default()
            },
        );
        store.set_text("quick", "Note");
        store.save_to(&drafts_file).unwrap();

        let reloaded = DraftStore::load_from(&drafts_file).unwrap();
        assert_eq!(reloaded, store);
        DraftStore::default().save_to(&drafts_file).unwrap();
        assert_eq!(
            DraftStore::load_from(&drafts_file).unwrap(),
            DraftStore::default()
        );

        let legacy_drafts_file = dir.path().join("sub/legacy_drafts.json");
        let mut raw_map = HashMap::new();
        raw_map.insert("inbox:m1".to_string(), "Reply text".to_string());
        save_drafts_to(&raw_map, &legacy_drafts_file).unwrap();
        assert_eq!(load_drafts_from(&legacy_drafts_file).unwrap(), raw_map);
        let loaded_store = DraftStore::load_from(&legacy_drafts_file).unwrap();
        assert_eq!(loaded_store.get_text("inbox:m1"), Some("Reply text"));

        let leftovers: Vec<_> = fs::read_dir(dir.path().join("sub"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.ends_with(".tmp") || s.ends_with(".bak")
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn test_drafts_are_per_user_and_legacy_file_is_adopted_once() {
        let _lock = TEST_ENV_LOCK.blocking_lock();
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();
        let _env = EnvVarGuard::set(&[
            ("OUTLOOK_CONFIG_DIR", Some(&dir_str)),
            ("OUTLOOK_DATA_DIR", Some(&dir_str)),
        ]);

        // One file per account, named with the same bucket as the account's cache directory.
        assert_ne!(drafts_path_for(Some("a")), drafts_path_for(Some("b")));
        assert_eq!(drafts_path_for(Some("a")), dir.path().join("drafts").join("a.json"));
        assert_eq!(drafts_path_for(None), dir.path().join("drafts").join("default.json"));
        let odd_id = "user@contoso.com/../x";
        assert_eq!(user_bucket(Some(odd_id)), "user_contoso_com____x");
        assert_eq!(
            drafts_path_for(Some(odd_id)).file_stem(),
            db_path(Some(odd_id)).parent().and_then(|p| p.file_name())
        );
        assert_eq!(user_bucket(Some("  ")), "default");

        // No legacy file: adoption only names the per-user path and creates nothing.
        assert_eq!(adopt_legacy_drafts(Some("a")), drafts_path_for(Some("a")));
        assert!(!drafts_path_for(Some("a")).exists());

        // The legacy shared file goes to the first account that asks, and only to that one.
        let mut legacy = DraftStore::default();
        legacy.set_text("quick", "written before the upgrade");
        legacy.save_to(&drafts_path()).unwrap();
        let path_a = adopt_legacy_drafts(Some("a"));
        assert!(!drafts_path().exists(), "the legacy file is moved, not copied");
        assert_eq!(DraftStore::load_from(&path_a).unwrap(), legacy);
        let path_b = adopt_legacy_drafts(Some("b"));
        assert!(!path_b.exists());
        assert_eq!(DraftStore::load_from(&path_b).unwrap(), DraftStore::default());

        // An account that already has drafts keeps them; a legacy file that shows up later is left alone.
        let mut stray = DraftStore::default();
        stray.set_text("quick", "stray legacy file");
        stray.save_to(&drafts_path()).unwrap();
        assert_eq!(adopt_legacy_drafts(Some("a")), path_a);
        assert_eq!(DraftStore::load_from(&path_a).unwrap(), legacy);
        assert!(drafts_path().exists());
    }

    #[test]
    fn test_image_protocol_preference_and_outlook_env_override() {
        assert_eq!(
            ImageProtocolPreference::parse_str("iterm"),
            Some(ImageProtocolPreference::Iterm2)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("halfblocks"),
            Some(ImageProtocolPreference::Halfblock)
        );

        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().into_owned();
        let _lock = TEST_ENV_LOCK.blocking_lock();
        let _guard = EnvVarGuard::set(&[
            ("OUTLOOK_CONFIG_DIR", Some(&dir_str)),
            ("OUTLOOK_IMAGE_PROTOCOL", Some("kitty")),
            ("TEAMS_IMAGE_PROTOCOL", Some("sixel")),
        ]);
        let cfg = AppConfig::load();
        assert_eq!(cfg.image_protocol, ImageProtocolPreference::Kitty);
    }
}
