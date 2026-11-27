//! Runtime setup, fast SQLite path resolution, error classification, and cache cursors.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::{
    auth::AuthManager,
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    models::SyncCursor,
    resolver,
    storage::Storage,
};

use super::{args::DELETE_JSON_REQUIRES_YES, format::is_forbidden_error};

#[derive(Debug)]
pub(crate) struct ErrorInfo {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) details: Option<serde_json::Value>,
}

impl std::fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ErrorInfo {}

pub(crate) fn error_with_info(
    code: &'static str,
    message: impl Into<String>,
    details: Option<serde_json::Value>,
) -> anyhow::Error {
    anyhow::Error::new(ErrorInfo {
        code,
        message: message.into(),
        details,
    })
}

pub(super) fn find_error_info(err: &anyhow::Error) -> Option<&ErrorInfo> {
    err.chain().find_map(|cause| cause.downcast_ref::<ErrorInfo>())
}

pub(crate) fn error_code(err: &anyhow::Error) -> &'static str {
    if let Some(info) = find_error_info(err) {
        return info.code;
    }
    let msg = format!("{err:#}");
    if msg.starts_with("Ambiguous ") {
        return "ambiguous";
    }
    let lower = msg.to_ascii_lowercase();
    if resolver::is_graph_not_found_error(err)
        || (!lower.contains("401 ")
            && !lower.contains("403 ")
            && !lower.contains("unauthorized")
            && !lower.contains("forbidden")
            && lower.contains(" not found:"))
    {
        return "not_found";
    }
    if is_forbidden_error(err) {
        return "forbidden";
    }
    let graph_detail = msg
        .strip_prefix(crate::graph::GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    if msg.starts_with("Sign-in required")
        || graph_detail.starts_with("401 ")
        || graph_detail.to_ascii_lowercase().contains("401 unauthorized")
    {
        return "auth_required";
    }
    if msg.starts_with("Token expired ") {
        return "token_expired";
    }
    if msg == DELETE_JSON_REQUIRES_YES {
        return "confirmation_required";
    }
    "error"
}

pub(crate) fn json_error_payload(err: &anyhow::Error) -> serde_json::Value {
    let mut error_obj = json!({
        "code": error_code(err),
        "message": format!("{err:#}"),
    });
    if let Some(details) = find_error_info(err).and_then(|info| info.details.as_ref()) {
        error_obj["details"] = details.clone();
    }
    json!({
        "ok": false,
        "error": error_obj,
    })
}

pub(crate) fn check_recipient_addresses(addrs: &[String]) -> Result<()> {
    for raw in addrs {
        let trimmed = raw.trim();
        if let Some((_name_part, rest)) = trimmed.rsplit_once('<') {
            if let Some(addr) = rest.strip_suffix('>') {
                if addr.trim().is_empty() {
                    return Err(error_with_info(
                        "invalid_recipient",
                        format!(
                            "Recipient '{trimmed}' has an empty address. Use user@example.com or \"Name <user@example.com>\"."
                        ),
                        None,
                    ));
                }
            }
        }
    }
    Ok(())
}

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

pub(super) fn db_path_in_dir(data_dir: &Path, user_id: Option<&str>) -> PathBuf {
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

pub(super) fn extract_user_id_from_jwt(raw_token: &str) -> Option<String> {
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

pub(super) fn extract_user_id_from_token_file(path: &Path) -> Option<String> {
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

pub(super) fn find_single_existing_db(data_dir: &Path) -> Option<PathBuf> {
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

pub(super) fn resolve_storage_db_path_fast(
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
        // A credentials file means someone is signed in. When no user id can be read from it the
        // `default` bucket is used: falling through to "the only cache on disk" could open a
        // different account's mail.
        let uid = extract_user_id_from_token_file(cred_path);
        return Some(db_path_in_dir(data_dir, uid.as_deref()));
    }

    // No token source on disk or in the environment: the session lives in the OS keyring or the user
    // is signed out. A single local cache is then the only candidate, and using it avoids opening
    // the keyring for a read.
    if let Some(single_db) = find_single_existing_db(data_dir) {
        return Some(single_db);
    }

    None
}

pub(super) fn infer_user_id_fast_with(
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

pub(super) fn env_access_token_value() -> Option<String> {
    std::env::var("OUTLOOK_ACCESS_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(super) async fn resolve_daemon_warm(cfg: &AppConfig, refresh: bool, cached: bool) -> bool {
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
pub(super) fn parse_cli_cache_secs(raw: Option<&str>) -> u64 {
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
