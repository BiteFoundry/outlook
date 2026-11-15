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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Entra ID Application (client) ID.
    pub client_id: String,
    /// Entra ID Tenant ID (`common`, `organizations`, domain, or tenant GUID).
    pub tenant_id: String,
    /// Optional OAuth scopes override. Empty uses the standard Teams delegated scope set.
    pub scopes: Vec<String>,
    /// Base URL for Microsoft Graph v1.0 (overridable for sovereign clouds or mock testing).
    pub graph_base_url: String,
    /// Base URL for Entra OAuth2 endpoints (overridable for sovereign clouds or mock testing).
    pub auth_base_url: String,
    /// Enable desktop toast/bell notifications in TUI.
    pub notify: bool,
    /// Show left navigation rail in wide layout.
    pub show_nav_rail: bool,
    /// Automatically open right thread/info pane on wide terminals.
    pub auto_thread_pane: bool,
    /// Custom download directory. Empty defaults to `Downloads/teams`.
    pub download_dir: String,
    /// Fast poll interval (ms) for active conversation in TUI.
    pub active_poll_ms: u64,
    /// Background chat preview diff interval (ms).
    pub chat_poll_ms: u64,
    /// Background channel delta & presence interval (ms).
    pub channel_poll_ms: u64,
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
            notify: true,
            show_nav_rail: true,
            auto_thread_pane: true,
            download_dir: String::new(),
            active_poll_ms: 2_500,
            chat_poll_ms: 8_000,
            channel_poll_ms: 30_000,
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
        ]
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
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
