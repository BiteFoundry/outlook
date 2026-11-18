//! Application configuration, local paths, settings (`config.toml`), and per-conversation/thread drafts (`drafts.json`).
//!
//! Shared foundation module: treat public signatures here as stable across all modules.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "teams-cli";
pub const DEFAULT_GRAPH_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
pub const DEFAULT_AUTH_BASE_URL: &str = "https://login.microsoftonline.com";
/// Default Microsoft multi-tenant public client ID fallback when user does not supply a custom Azure App Client ID.
/// Users can always override via `--client-id`, `TEAMS_CLIENT_ID`, `AZURE_CLIENT_ID`, or `config.toml`.
pub const DEFAULT_PUBLIC_CLIENT_ID: &str = "14d82eec-204b-4c2f-b7e8-296a70dab67e";
pub const DEFAULT_TENANT_ID: &str = "common";
pub const DEFAULT_GITHUB_REPO: &str = "shinick-dev/teams";

/// Entry persona (`teams`, `outlook`, or `onenote`), sharing the exact same Entra OAuth token, Graph client, and SQLite DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    #[default]
    Teams,
    Outlook,
    OneNote,
}

impl Persona {
    /// Detects whether the user invoked `onenote`, `outlook`, or `teams` (via `TEAMS_PERSONA` or `argv[0]` stem).
    pub fn detect() -> Self {
        if let Ok(env_val) = std::env::var("TEAMS_PERSONA") {
            let trimmed = env_val.trim();
            if trimmed.eq_ignore_ascii_case("onenote") || trimmed.eq_ignore_ascii_case("note") {
                return Self::OneNote;
            }
            if trimmed.eq_ignore_ascii_case("outlook") {
                return Self::Outlook;
            }
            if trimmed.eq_ignore_ascii_case("teams") {
                return Self::Teams;
            }
        }
        if let Some(arg0) = std::env::args_os().next() {
            if let Some(stem) = Path::new(&arg0).file_stem().and_then(|s| s.to_str()) {
                let lower = stem.to_ascii_lowercase();
                if lower.starts_with("onenote") || lower == "note" {
                    return Self::OneNote;
                }
                if lower.starts_with("outlook") {
                    return Self::Outlook;
                }
            }
        }
        Self::Teams
    }

    pub fn bin_name(self) -> &'static str {
        match self {
            Self::Teams => "teams",
            Self::Outlook => "outlook",
            Self::OneNote => "onenote",
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
    /// Optional OAuth scopes override. Empty uses the standard Teams + Outlook M365 delegated scope set.
    pub scopes: Vec<String>,
    /// Base URL for Microsoft Graph v1.0 (overridable for sovereign clouds or mock testing).
    pub graph_base_url: String,
    /// Base URL for Entra OAuth2 endpoints (overridable for sovereign clouds or mock testing).
    pub auth_base_url: String,
    /// GitHub `owner/repo` used by `teams update` / `outlook update`.
    pub github_repo: String,
    /// Enable desktop toast/bell notifications in TUI.
    pub notify: bool,
    /// Show left navigation rail in wide layout.
    pub show_nav_rail: bool,
    /// Automatically open right thread/info pane on wide terminals.
    pub auto_thread_pane: bool,
    /// Render inline image thumbnails directly inside terminal Chat/Mail views.
    pub inline_images: bool,
    /// Custom download directory. Empty defaults to `Downloads/teams`.
    pub download_dir: String,
    /// Fast poll interval (ms) for active conversation in TUI.
    pub active_poll_ms: u64,
    /// Background chat preview diff interval (ms).
    pub chat_poll_ms: u64,
    /// Background channel delta, Outlook mail/calendar, & presence interval (ms).
    pub channel_poll_ms: u64,
    /// Optional external token file path (`TEAMS_TOKEN_FILE` or `config.toml`), read live on demand.
    pub token_file: String,
    /// Optional external token refresh command hook (`TEAMS_TOKEN_REFRESH_CMD` or `config.toml`), executed when token is missing or within `token_refresh_mins` of expiry.
    pub token_refresh_cmd: String,
    /// Refresh threshold in minutes before token `exp` to trigger `token_refresh_cmd` (`TEAMS_TOKEN_REFRESH_MINS`, default 5).
    pub token_refresh_mins: u64,
    /// When `false` (default), TUI only background-polls workspaces/domains that have been activated in the session.
    /// Overrideable via `TEAMS_BACKGROUND_ALL=1|true` or `config.toml` (`background_all_personas = true`).
    pub background_all_personas: bool,
    /// CLI local cache TTL in seconds (`TEAMS_CLI_CACHE_SECS`, default `0`).
    /// Defaults to `0` so CLI invocations fetch fresh unless `TEAMS_CLI_CACHE_SECS`, `cli_cache_ttl_secs`, or `--cached` is used.
    pub cli_cache_ttl_secs: u64,
    /// Automatically start and attach to the lightweight background sync daemon (`TEAMS_AUTO_DAEMON=1|true`, default `false`).
    pub auto_daemon: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            client_id: std::env::var("TEAMS_CLIENT_ID")
                .or_else(|_| std::env::var("AZURE_CLIENT_ID"))
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_PUBLIC_CLIENT_ID.to_string()),
            tenant_id: std::env::var("TEAMS_TENANT_ID")
                .or_else(|_| std::env::var("AZURE_TENANT_ID"))
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string()),
            scopes: Vec::new(),
            graph_base_url: std::env::var("TEAMS_GRAPH_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_GRAPH_BASE_URL.to_string()),
            auth_base_url: std::env::var("TEAMS_AUTH_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_AUTH_BASE_URL.to_string()),
            github_repo: std::env::var("TEAMS_GITHUB_REPO")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string()),
            notify: true,
            show_nav_rail: true,
            auto_thread_pane: true,
            inline_images: true,
            download_dir: String::new(),
            active_poll_ms: 2_500,
            chat_poll_ms: 8_000,
            channel_poll_ms: 30_000,
            token_file: std::env::var("TEAMS_TOKEN_FILE")
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
            token_refresh_cmd: std::env::var("TEAMS_TOKEN_REFRESH_CMD")
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
            token_refresh_mins: std::env::var("TEAMS_TOKEN_REFRESH_MINS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(5),
            background_all_personas: std::env::var("TEAMS_BACKGROUND_ALL")
                .ok()
                .and_then(|s| parse_env_bool(&s))
                .unwrap_or(false),
            cli_cache_ttl_secs: std::env::var("TEAMS_CLI_CACHE_SECS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0),
            auto_daemon: std::env::var("TEAMS_AUTO_DAEMON")
                .ok()
                .and_then(|s| parse_env_bool(&s))
                .unwrap_or(false),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let mut cfg = Self::load_from(&config_file_path()).unwrap_or_default();
        // Environment variables take precedence over file values when set.
        if let Ok(cid) = std::env::var("TEAMS_CLIENT_ID").or_else(|_| std::env::var("AZURE_CLIENT_ID")) {
            if !cid.trim().is_empty() {
                cfg.client_id = cid.trim().to_string();
            }
        }
        if let Ok(tid) = std::env::var("TEAMS_TENANT_ID").or_else(|_| std::env::var("AZURE_TENANT_ID")) {
            if !tid.trim().is_empty() {
                cfg.tenant_id = tid.trim().to_string();
            }
        }
        if let Ok(gurl) = std::env::var("TEAMS_GRAPH_BASE_URL") {
            if !gurl.trim().is_empty() {
                cfg.graph_base_url = gurl.trim().to_string();
            }
        }
        if let Ok(aurl) = std::env::var("TEAMS_AUTH_BASE_URL") {
            if !aurl.trim().is_empty() {
                cfg.auth_base_url = aurl.trim().to_string();
            }
        }
        if let Ok(repo) = std::env::var("TEAMS_GITHUB_REPO") {
            if !repo.trim().is_empty() {
                cfg.github_repo = repo.trim().to_string();
            }
        }
        if let Ok(tf) = std::env::var("TEAMS_TOKEN_FILE") {
            if !tf.trim().is_empty() {
                cfg.token_file = tf.trim().to_string();
            }
        }
        if let Ok(rcmd) = std::env::var("TEAMS_TOKEN_REFRESH_CMD") {
            if !rcmd.trim().is_empty() {
                cfg.token_refresh_cmd = rcmd.trim().to_string();
            }
        }
        if let Ok(rmins) = std::env::var("TEAMS_TOKEN_REFRESH_MINS") {
            if let Ok(parsed) = rmins.trim().parse::<u64>() {
                cfg.token_refresh_mins = parsed;
            }
        }
        if let Ok(bg_all) = std::env::var("TEAMS_BACKGROUND_ALL") {
            if let Some(parsed) = parse_env_bool(&bg_all) {
                cfg.background_all_personas = parsed;
            }
        }
        if let Ok(ttl) = std::env::var("TEAMS_CLI_CACHE_SECS") {
            if let Ok(parsed) = ttl.trim().parse::<u64>() {
                cfg.cli_cache_ttl_secs = parsed;
            }
        }
        if let Ok(ad) = std::env::var("TEAMS_AUTO_DAEMON") {
            if let Some(parsed) = parse_env_bool(&ad) {
                cfg.auto_daemon = parsed;
            }
        }
        cfg
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("설정 파일 읽기 실패: {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("설정 파일 파싱 실패: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        ensure_parent(path)?;
        let raw = toml::to_string_pretty(self).context("설정 직렬화 실패")?;
        write_atomic(path, raw.as_bytes())
    }

    pub fn effective_scopes(&self) -> Vec<String> {
        if !self.scopes.is_empty() {
            return self.scopes.clone();
        }
        vec![
            "offline_access".to_string(),
            "User.Read".to_string(),
            "User.ReadBasic.All".to_string(),
            "Chat.ReadWrite".to_string(),
            "Team.ReadBasic.All".to_string(),
            "Channel.ReadBasic.All".to_string(),
            "ChannelMessage.Read.All".to_string(),
            "ChannelMessage.Send".to_string(),
            "Files.ReadWrite.All".to_string(),
            "Presence.ReadWrite".to_string(),
            "Mail.ReadWrite".to_string(),
            "Mail.Send".to_string(),
            "Calendars.ReadWrite".to_string(),
            "Contacts.ReadWrite".to_string(),
            "Tasks.ReadWrite".to_string(),
            "Notes.ReadWrite".to_string(),
            "Notes.ReadWrite.All".to_string(),
        ]
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
    if let Ok(override_dir) = std::env::var("TEAMS_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir.trim());
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("TEAMS_DATA_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir.trim());
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credential_fallback_path() -> PathBuf {
    config_dir().join("credentials.json")
}

pub fn drafts_path() -> PathBuf {
    config_dir().join("drafts.json")
}

pub fn db_path(user_id: Option<&str>) -> PathBuf {
    let bucket = user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("default");
    let sanitized: String = bucket
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    data_dir().join(sanitized).join("cache.db")
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("teams")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("디렉터리 생성 실패: {}", parent.display()))?;
    }
    Ok(())
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    ensure_parent(path)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data).with_context(|| format!("임시 파일 쓰기 실패: {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, path) {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("기존 파일 교체 실패: {}", path.display()))?;
            fs::rename(&tmp, path)
                .with_context(|| format!("원자적 저장 실패: {}", path.display()))?;
        } else {
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("파일 저장 실패: {}", path.display()));
        }
    }
    Ok(())
}

/// Draft key format: `"{conv_key}"` for conversation root or `"{conv_key}#thread:{root_msg_id}"` for thread replies.
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
    Ok(serde_json::from_str(&raw)?)
}

pub fn save_drafts(drafts: &HashMap<String, String>) -> Result<()> {
    save_drafts_to(drafts, &drafts_path())
}

pub fn save_drafts_to(drafts: &HashMap<String, String>, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(drafts)?;
    write_atomic(path, &bytes)
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
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvVarGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
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
    }

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

    #[test]
    fn test_app_config_toml_parses_background_all_and_cli_cache_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.toml");
        fs::write(&legacy_path, "notify = false\nactive_poll_ms = 3000\n").unwrap();

        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set(&[
            ("TEAMS_BACKGROUND_ALL", None),
            ("TEAMS_CLI_CACHE_SECS", None),
        ]);

        let legacy_cfg = AppConfig::load_from(&legacy_path).unwrap();
        assert!(!legacy_cfg.notify);
        assert_eq!(legacy_cfg.active_poll_ms, 3000);
        assert!(!legacy_cfg.background_all_personas);
        assert_eq!(legacy_cfg.cli_cache_ttl_secs, 0);

        let custom_path = dir.path().join("custom.toml");
        fs::write(
            &custom_path,
            "background_all_personas = true\ncli_cache_ttl_secs = 45\n",
        )
        .unwrap();
        let custom_cfg = AppConfig::load_from(&custom_path).unwrap();
        assert!(custom_cfg.background_all_personas);
        assert_eq!(custom_cfg.cli_cache_ttl_secs, 45);
    }

    #[test]
    fn test_app_config_env_overrides_background_all_and_cli_cache_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        fs::write(
            &cfg_path,
            "background_all_personas = false\ncli_cache_ttl_secs = 15\n",
        )
        .unwrap();

        let _lock = ENV_LOCK.lock().unwrap();
        let dir_str = dir.path().to_string_lossy().into_owned();

        {
            let _guard = EnvVarGuard::set(&[
                ("TEAMS_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_BACKGROUND_ALL", Some("1")),
                ("TEAMS_CLI_CACHE_SECS", Some("120")),
            ]);
            let cfg = AppConfig::load();
            assert!(cfg.background_all_personas);
            assert_eq!(cfg.cli_cache_ttl_secs, 120);
        }

        {
            let _guard = EnvVarGuard::set(&[
                ("TEAMS_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_BACKGROUND_ALL", Some("true")),
                ("TEAMS_CLI_CACHE_SECS", Some("0")),
            ]);
            let cfg = AppConfig::load();
            assert!(cfg.background_all_personas);
            assert_eq!(cfg.cli_cache_ttl_secs, 0);
        }

        {
            let _guard = EnvVarGuard::set(&[
                ("TEAMS_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_BACKGROUND_ALL", None),
                ("TEAMS_CLI_CACHE_SECS", None),
            ]);
            let def = AppConfig::default();
            assert!(!def.background_all_personas);
            assert_eq!(def.cli_cache_ttl_secs, 0);
        }
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
}
