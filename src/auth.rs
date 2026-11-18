//! OAuth 2.0 authentication (PKCE loopback, Device Code Flow, Client Secret / env token)
//! + OS keyring (`keyring`) with file fallback (`credentials.json`) + automatic refresh.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::RwLock,
};
use url::Url;

use crate::config::{credential_fallback_path, write_atomic, AppConfig};

const KEYRING_SERVICE: &str = "teams-cli";
const KEYRING_USER: &str = "default";
const LOGIN_CALLBACK_TIMEOUT_SECS: u64 = 300;
#[cfg(target_os = "linux")]
const LINUX_KEYRING_TIMEOUT: Duration = Duration::from_millis(800);

/// Set when D-Bus Secret Service / keyring fails or times out so subsequent
/// calls in the same process immediately fall back to `credentials.json`.
static KEYRING_RUNTIME_FAILED: AtomicBool = AtomicBool::new(false);
/// Emits the approaching-expiry warning for static external tokens at most once per process.
static EXPIRY_SOON_WARNED: AtomicBool = AtomicBool::new(false);
/// Emits the external refresh-command fallback warning at most once per process.
static REFRESH_CMD_WARNED: AtomicBool = AtomicBool::new(false);
const EXTERNAL_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix epoch seconds when `access_token` expires.
    pub expires_at: i64,
    pub client_id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub user_display_name: Option<String>,
    pub user_email: Option<String>,
    pub auth_mode: String,
}

impl StoredTokens {
    pub fn is_expired_soon(&self, skew_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.expires_at <= now + skew_secs
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub client_id: Option<String>,
    pub tenant_id: Option<String>,
    pub device_code: bool,
    pub client_secret: Option<String>,
    pub access_token: Option<String>,
    pub port: Option<u16>,
}

#[derive(Clone)]
pub struct AuthManager {
    config: AppConfig,
    http: reqwest::Client,
    tokens: Arc<RwLock<Option<StoredTokens>>>,
    refresh_mutex: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    token_path_override: Option<std::path::PathBuf>,
}

impl AuthManager {
    pub fn new(config: AppConfig) -> Result<Self> {
        let loaded = if should_skip_keyring_on_init(&config) {
            None
        } else {
            load_stored_tokens()?
        };
        let http = build_auth_http_client()?;
        Ok(Self {
            config,
            http,
            tokens: Arc::new(RwLock::new(loaded)),
            refresh_mutex: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            token_path_override: None,
        })
    }

    pub fn from_tokens(config: AppConfig, tokens: StoredTokens) -> Self {
        let http = build_auth_http_client().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            http,
            tokens: Arc::new(RwLock::new(Some(tokens))),
            refresh_mutex: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            token_path_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_token_path(mut self, path: std::path::PathBuf) -> Self {
        self.token_path_override = Some(path);
        self
    }

    fn persist_tokens(&self, tokens: &StoredTokens) -> Result<()> {
        #[cfg(test)]
        if let Some(ref path) = self.token_path_override {
            return save_tokens_to_path(tokens, path);
        }
        save_stored_tokens(tokens)
    }

    fn load_from_configured_source(&self) -> Result<Option<StoredTokens>> {
        #[cfg(test)]
        if let Some(ref path) = self.token_path_override {
            return load_tokens_from_path(path);
        }
        load_stored_tokens()
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn has_refresh_cmd(&self) -> bool {
        !self.config.token_refresh_cmd.trim().is_empty()
    }

    fn external_refresh_skew_secs(&self) -> i64 {
        (self.config.token_refresh_mins.min(1440) * 60) as i64
    }

    async fn resolve_current_tokens_raw(&self) -> Option<StoredTokens> {
        let mem_tok = self.tokens.read().await.clone();
        let env_tok = env_access_token(&self.config);
        let file_tok = self
            .config
            .resolved_token_file()
            .and_then(|path| read_external_token_file(&path, &self.config).ok());

        if let Some(ref mem) = mem_tok {
            if mem.auth_mode == "refresh_cmd" {
                let newer_than_env = match env_tok.as_ref() {
                    None => true,
                    Some(env) => {
                        mem.expires_at > env.expires_at
                            || (mem.expires_at == env.expires_at
                                && mem.access_token != env.access_token)
                    }
                };
                let newer_than_file = match file_tok.as_ref() {
                    None => true,
                    Some(file) => {
                        mem.expires_at > file.expires_at
                            || (mem.expires_at == file.expires_at
                                && mem.access_token != file.access_token)
                    }
                };
                if newer_than_env && newer_than_file {
                    return Some(mem.clone());
                }
            }
        }

        if let Some(env) = env_tok {
            return Some(env);
        }
        if let Some(file) = file_tok {
            return Some(file);
        }
        mem_tok
    }

    pub async fn current_tokens(&self) -> Option<StoredTokens> {
        if let Some(tok) = self.resolve_current_tokens_raw().await {
            return Some(tok);
        }
        if self.has_refresh_cmd() {
            return self.run_external_refresh_cmd(None).await.ok();
        }
        None
    }

    /// Returns a valid Bearer access token, hot-reloading external `credentials.json` / `token_file` updates,
    /// running `TEAMS_TOKEN_REFRESH_CMD`, or refreshing via OAuth `refresh_token` automatically when needed.
    pub async fn valid_access_token(&self) -> Result<String> {
        let has_refresh_cmd = self.has_refresh_cmd();

        if let Some(tok) = self.resolve_current_tokens_raw().await {
            let is_external = matches!(
                tok.auth_mode.as_str(),
                "env_token" | "token_file" | "refresh_cmd"
            );
            if is_external {
                if has_refresh_cmd {
                    let skew_secs = self.external_refresh_skew_secs();
                    if !tok.is_expired_soon(skew_secs) {
                        return Ok(tok.access_token);
                    }
                    let refreshed = self.run_external_refresh_cmd(Some(&tok)).await?;
                    return Ok(refreshed.access_token);
                }

                if tok.is_expired_soon(0) {
                    bail!(
                        "토큰 만료 (exp: {}, auth_mode={}) — 토큰을 갱신한 뒤 재시도하세요. (자동 갱신: TEAMS_TOKEN_FILE + TEAMS_TOKEN_REFRESH_CMD 설정 지원)",
                        format_local_exp(tok.expires_at),
                        tok.auth_mode
                    );
                }
                if tok.is_expired_soon(300) && !EXPIRY_SOON_WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "경고: 토큰 만료 임박 (exp: {}) — 곧 갱신이 필요합니다.",
                        format_local_exp(tok.expires_at)
                    );
                }
                return Ok(tok.access_token);
            }

            if !tok.is_expired_soon(90) {
                return Ok(tok.access_token);
            }
            if has_refresh_cmd {
                let refreshed = self.run_external_refresh_cmd(Some(&tok)).await?;
                return Ok(refreshed.access_token);
            }
            if let Some(reloaded) = self
                .try_hot_reload_from_disk(&tok.access_token, tok.expires_at)
                .await
            {
                if !reloaded.is_expired_soon(30) {
                    return Ok(reloaded.access_token);
                }
            }
            if self.can_refresh_token(&tok) {
                let refreshed = self.refresh_now().await?;
                return Ok(refreshed.access_token);
            }
            return Ok(tok.access_token);
        }

        if has_refresh_cmd {
            let refreshed = self.run_external_refresh_cmd(None).await?;
            return Ok(refreshed.access_token);
        }

        if let Some(token_path) = self.config.resolved_token_file() {
            let tok = read_external_token_file(&token_path, &self.config)?;
            if tok.is_expired_soon(0) {
                bail!(
                    "토큰 만료 (exp: {}, auth_mode={}) — 토큰을 갱신한 뒤 재시도하세요. (자동 갱신: TEAMS_TOKEN_FILE + TEAMS_TOKEN_REFRESH_CMD 설정 지원)",
                    format_local_exp(tok.expires_at),
                    tok.auth_mode
                );
            }
            return Ok(tok.access_token);
        }

        if let Ok(Some(reloaded)) = self.load_from_configured_source() {
            let mut guard = self.tokens.write().await;
            *guard = Some(reloaded.clone());
            return Ok(reloaded.access_token);
        }
        bail!("로그인이 필요합니다. 먼저 `teams login` 을 실행하세요.")
    }

    async fn try_hot_reload_from_disk(
        &self,
        current_access_token: &str,
        current_expires_at: i64,
    ) -> Option<StoredTokens> {
        let disk_tok = self.load_from_configured_source().ok().flatten()?;
        if disk_tok.access_token != current_access_token
            && disk_tok.expires_at >= current_expires_at
        {
            let mut guard = self.tokens.write().await;
            *guard = Some(disk_tok.clone());
            return Some(disk_tok);
        }
        None
    }

    /// Returns whether stored tokens can currently be refreshed (`token_refresh_cmd`, refresh token, or client secret).
    pub async fn is_refreshable(&self) -> bool {
        if self.has_refresh_cmd() {
            return true;
        }
        if env_access_token(&self.config).is_some() || self.config.resolved_token_file().is_some() {
            return false;
        }
        let guard = self.tokens.read().await;
        guard
            .as_ref()
            .map(|tok| self.can_refresh_token(tok))
            .unwrap_or(false)
    }

    fn can_refresh_token(&self, tok: &StoredTokens) -> bool {
        if tok
            .refresh_token
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        tok.auth_mode == "client_credentials" && env_client_secret().is_some()
    }

    pub async fn login(&self, opts: &LoginOptions) -> Result<StoredTokens> {
        let client_id = opts
            .client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.client_id)
            .to_string();
        let tenant_id = opts
            .tenant_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.tenant_id)
            .to_string();

        // 1. Direct Access Token
        if let Some(raw_token) = opts
            .access_token
            .as_deref()
            .map(strip_bearer_prefix)
            .filter(|s| !s.is_empty())
        {
            let mut stored = stored_tokens_from_jwt(
                raw_token,
                None,
                None,
                &client_id,
                &tenant_id,
                "access_token",
            );
            if opts
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
            {
                stored.client_id = client_id;
            }
            if opts
                .tenant_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
            {
                stored.tenant_id = tenant_id;
            }
            return self.store_tokens(stored).await;
        }

        // 2. Client Credentials (explicit option takes precedence over device_code; env var applies if not device_code)
        if let Some(secret) = opts
            .client_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let stored = self
                .login_client_credentials(&client_id, &tenant_id, secret)
                .await?;
            return self.store_tokens(stored).await;
        }

        if !opts.device_code {
            if let Some(secret) = env_client_secret() {
                let stored = self
                    .login_client_credentials(&client_id, &tenant_id, &secret)
                    .await?;
                return self.store_tokens(stored).await;
            }
        }

        // 3. Device Code Flow
        if opts.device_code {
            let stored = self.login_device_code(&client_id, &tenant_id).await?;
            return self.store_tokens(stored).await;
        }

        // 4. Interactive Browser Flow (Authorization Code + PKCE)
        let stored = self
            .login_pkce_loopback(&client_id, &tenant_id, opts.port)
            .await?;
        self.store_tokens(stored).await
    }

    pub async fn refresh_now(&self) -> Result<StoredTokens> {
        if self.has_refresh_cmd() {
            let current = self.resolve_current_tokens_raw().await;
            return self.run_external_refresh_cmd(current.as_ref()).await;
        }
        if env_access_token(&self.config).is_some() {
            bail!("TEAMS_ACCESS_TOKEN 환경 변수 토큰은 자동 갱신할 수 없습니다. (TEAMS_TOKEN_REFRESH_CMD 미설정)");
        }
        if let Some(path) = self.config.resolved_token_file() {
            return read_external_token_file(&path, &self.config);
        }
        let mut guard = self.tokens.write().await;
        self.refresh_locked(&mut guard).await
    }

    /// Refreshes or hot-reloads the token only if it still matches `failed_access_token` (prevents thundering herd on 401).
    pub(crate) async fn refresh_for_failed_token(
        &self,
        failed_access_token: &str,
    ) -> Result<StoredTokens> {
        if self.has_refresh_cmd() {
            let current = self.resolve_current_tokens_raw().await;
            if let Some(ref cur) = current {
                if cur.access_token != failed_access_token && !cur.is_expired_soon(30) {
                    return Ok(cur.clone());
                }
            }
            let refreshed = self.run_external_refresh_cmd(current.as_ref()).await?;
            if refreshed.access_token == failed_access_token {
                bail!("외부 토큰 갱신 후에도 거부된 토큰과 동일한 토큰이 반환되었습니다.");
            }
            return Ok(refreshed);
        }
        if env_access_token(&self.config).is_some() {
            bail!("TEAMS_ACCESS_TOKEN 환경 변수 토큰은 자동 갱신할 수 없습니다.");
        }
        if let Some(path) = self.config.resolved_token_file() {
            if let Ok(disk_tok) = read_external_token_file(&path, &self.config) {
                if disk_tok.access_token != failed_access_token && !disk_tok.is_expired_soon(0) {
                    return Ok(disk_tok);
                }
            }
            bail!("외부 토큰 파일이 만료되었거나 거부되었습니다. 토큰을 갱신하거나 TEAMS_TOKEN_REFRESH_CMD를 설정하세요.");
        }
        let mut guard = self.tokens.write().await;
        if let Some(current) = guard.as_ref() {
            if current.access_token != failed_access_token && !current.is_expired_soon(30) {
                return Ok(current.clone());
            }
        }
        if let Ok(Some(disk_tok)) = self.load_from_configured_source() {
            if disk_tok.access_token != failed_access_token && !disk_tok.is_expired_soon(10) {
                *guard = Some(disk_tok.clone());
                return Ok(disk_tok);
            }
        }
        self.refresh_locked(&mut guard).await
    }

    async fn run_external_refresh_cmd(
        &self,
        current: Option<&StoredTokens>,
    ) -> Result<StoredTokens> {
        let _lock = self.refresh_mutex.lock().await;
        let skew_secs = self.external_refresh_skew_secs();
        let old_token = current.map(|c| c.access_token.as_str());
        let token_file = self.config.resolved_token_file();

        // Double-check if another concurrent task/process already refreshed `token_file`.
        if let Some(ref path) = token_file {
            if let Ok(reloaded) = read_external_token_file(path, &self.config) {
                if !reloaded.is_expired_soon(skew_secs.min(60))
                    && Some(reloaded.access_token.as_str()) != old_token
                {
                    return Ok(reloaded);
                }
            }
        } else if let Some(mem) = self.tokens.read().await.clone() {
            if mem.auth_mode == "refresh_cmd"
                && !mem.is_expired_soon(skew_secs.min(60))
                && Some(mem.access_token.as_str()) != old_token
            {
                return Ok(mem);
            }
        }

        let cmd_str = self.config.token_refresh_cmd.trim();
        let exec_res = execute_external_refresh_shell(cmd_str, token_file.as_deref()).await;

        match exec_res {
            Ok(stdout) => {
                let parsed = if let Some(ref path) = token_file {
                    if path.exists() {
                        match read_external_token_file(path, &self.config) {
                            Ok(file_tok)
                                if Some(file_tok.access_token.as_str()) != old_token
                                    || stdout.trim().is_empty() =>
                            {
                                Ok(file_tok)
                            }
                            Ok(file_tok) => match parse_external_token_str(
                                &stdout,
                                "refresh_cmd",
                                &self.config,
                                "token_refresh_cmd stdout",
                            ) {
                                Ok(stdout_tok) => {
                                    *self.tokens.write().await = Some(stdout_tok.clone());
                                    Ok(stdout_tok)
                                }
                                Err(_) => Ok(file_tok),
                            },
                            Err(file_err) => parse_external_token_str(
                                &stdout,
                                "refresh_cmd",
                                &self.config,
                                "token_refresh_cmd stdout",
                            )
                            .map_err(|_| file_err),
                        }
                    } else {
                        let tok = parse_external_token_str(
                            &stdout,
                            "refresh_cmd",
                            &self.config,
                            "token_refresh_cmd stdout",
                        )?;
                        *self.tokens.write().await = Some(tok.clone());
                        Ok(tok)
                    }
                } else {
                    let tok = parse_external_token_str(
                        &stdout,
                        "refresh_cmd",
                        &self.config,
                        "token_refresh_cmd stdout",
                    )?;
                    *self.tokens.write().await = Some(tok.clone());
                    Ok(tok)
                };

                let new_tok = match parsed {
                    Ok(t) => t,
                    Err(err) => {
                        return fallback_or_bail_refresh_error(current, &err.to_string());
                    }
                };

                if new_tok.is_expired_soon(0) {
                    bail!(
                        "외부 토큰 갱신 실패 (exp: {}): 갱신 후에도 토큰이 이미 만료되어 있습니다.",
                        format_local_exp(new_tok.expires_at)
                    );
                }
                Ok(new_tok)
            }
            Err(err) => fallback_or_bail_refresh_error(current, &err.to_string()),
        }
    }

    async fn refresh_locked(&self, guard: &mut Option<StoredTokens>) -> Result<StoredTokens> {
        let current = guard
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("저장된 토큰이 없습니다. 먼저 `teams login` 을 실행하세요."))?;

        let token_url = self.token_endpoint(&current.tenant_id);
        let scope = self.config.effective_scopes().join(" ");

        let token_resp = if let Some(rt) = current
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let params = [
                ("client_id", current.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", rt),
                ("scope", scope.as_str()),
            ];
            self.post_token_form(&token_url, &params).await?
        } else if current.auth_mode == "client_credentials" {
            let secret = env_client_secret().ok_or_else(|| {
                anyhow!("client_credentials 토큰 갱신을 위해 TEAMS_CLIENT_SECRET 환경 변수가 필요합니다.")
            })?;
            let params = [
                ("client_id", current.client_id.as_str()),
                ("client_secret", secret.as_str()),
                ("grant_type", "client_credentials"),
                ("scope", "https://graph.microsoft.com/.default"),
            ];
            self.post_token_form(&token_url, &params).await?
        } else {
            bail!("토큰을 갱신할 수 없습니다 (refresh_token 없음). 다시 `teams login` 을 실행하세요.");
        };

        let expires_in = token_resp.expires_in.unwrap_or(3600).max(60);
        let updated = StoredTokens {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token.or(current.refresh_token),
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: current.client_id,
            tenant_id: current.tenant_id,
            user_id: current.user_id,
            user_display_name: current.user_display_name,
            user_email: current.user_email,
            auth_mode: current.auth_mode,
        };

        self.persist_tokens(&updated)?;
        *guard = Some(updated.clone());
        Ok(updated)
    }

    async fn store_tokens(&self, tokens: StoredTokens) -> Result<StoredTokens> {
        let mut guard = self.tokens.write().await;
        self.persist_tokens(&tokens)?;
        *guard = Some(tokens.clone());
        Ok(tokens)
    }

    async fn login_client_credentials(
        &self,
        client_id: &str,
        tenant_id: &str,
        client_secret: &str,
    ) -> Result<StoredTokens> {
        let token_url = self.token_endpoint(tenant_id);
        let params = [
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "client_credentials"),
            ("scope", "https://graph.microsoft.com/.default"),
        ];
        let resp = self.post_token_form(&token_url, &params).await?;
        let expires_in = resp.expires_in.unwrap_or(3600).max(60);
        Ok(StoredTokens {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: client_id.to_string(),
            tenant_id: tenant_id.to_string(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "client_credentials".to_string(),
        })
    }

    async fn login_device_code(&self, client_id: &str, tenant_id: &str) -> Result<StoredTokens> {
        let device_url = self.devicecode_endpoint(tenant_id);
        let token_url = self.token_endpoint(tenant_id);
        let scope = self.config.effective_scopes().join(" ");

        let res = self
            .http
            .post(&device_url)
            .form(&[("client_id", client_id), ("scope", scope.as_str())])
            .send()
            .await
            .context("Device Code 요청 전송 실패")?;

        let status = res.status();
        let body_text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Device Code 시작 실패 ({}): {}", status, body_text);
        }
        let dc: DeviceCodeResponse =
            serde_json::from_str(&body_text).context("Device Code 응답 파싱 실패")?;

        eprintln!("\n[Microsoft Teams 로그인 - Device Code]");
        eprintln!(
            "브라우저에서 다음 주소를 열고 코드를 입력하세요 / Open URL and enter code:"
        );
        eprintln!("  URL : {}", dc.verification_uri);
        eprintln!("  Code: {}", dc.user_code);
        if let Some(msg) = dc.message.as_deref().filter(|s| !s.trim().is_empty()) {
            eprintln!("  {}", msg.trim());
        }
        eprintln!();

        if should_open_browser() {
            let _ = open::that(&dc.verification_uri);
        }

        let mut interval_secs = dc.interval.unwrap_or(5).max(1);
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(dc.expires_in.max(30) as u64);

        loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("Device Code 인증 시간이 만료되었습니다. 다시 시도하세요.");
            }
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;

            let poll_res = self
                .http
                .post(&token_url)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", client_id),
                    ("device_code", dc.device_code.as_str()),
                ])
                .send()
                .await
                .context("Device Code 토큰 폴링 실패")?;

            let poll_status = poll_res.status();
            let poll_text = poll_res.text().await.unwrap_or_default();
            if poll_status.is_success() {
                let tok: TokenResponse =
                    serde_json::from_str(&poll_text).context("토큰 응답 파싱 실패")?;
                let expires_in = tok.expires_in.unwrap_or(3600).max(60);
                return Ok(StoredTokens {
                    access_token: tok.access_token,
                    refresh_token: tok.refresh_token,
                    expires_at: chrono::Utc::now().timestamp() + expires_in,
                    client_id: client_id.to_string(),
                    tenant_id: tenant_id.to_string(),
                    user_id: None,
                    user_display_name: None,
                    user_email: None,
                    auth_mode: "device_code".to_string(),
                });
            }

            let err_body: OAuthErrorResponse =
                serde_json::from_str(&poll_text).unwrap_or(OAuthErrorResponse {
                    error: format!("http_{}", poll_status.as_u16()),
                    error_description: Some(poll_text.clone()),
                });

            match err_body.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval_secs += 5;
                    continue;
                }
                "expired_token" => {
                    bail!("Device Code가 만료되었습니다. 다시 시도하세요.");
                }
                "authorization_declined" | "access_denied" => {
                    bail!("사용자가 인증을 거부했습니다.");
                }
                other => {
                    let desc = err_body.error_description.unwrap_or_default();
                    bail!("Device Code 인증 오류 ({other}): {desc}");
                }
            }
        }
    }

    async fn login_pkce_loopback(
        &self,
        client_id: &str,
        tenant_id: &str,
        preferred_port: Option<u16>,
    ) -> Result<StoredTokens> {
        let code_verifier = generate_random_b64(32);
        let code_challenge = compute_pkce_s256(&code_verifier);
        let expected_state = generate_random_b64(16);

        let bind_port = preferred_port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", bind_port))
            .await
            .with_context(|| format!("로컬 콜백 포트(127.0.0.1:{bind_port}) 바인딩 실패"))?;
        let actual_port = listener.local_addr()?.port();
        let redirect_uri = format!("http://localhost:{actual_port}");
        let scope = self.config.effective_scopes().join(" ");

        let mut auth_url = Url::parse(&self.authorize_endpoint(tenant_id))
            .context("authorize URL 생성 실패")?;
        auth_url
            .query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", &scope)
            .append_pair("state", &expected_state)
            .append_pair("code_challenge", &code_challenge)
            .append_pair("code_challenge_method", "S256");

        eprintln!("\n[Microsoft Teams 로그인 - 브라우저 인증 (PKCE)]");
        eprintln!("브라우저가 자동으로 열리지 않으면 아래 URL을 복사해 여세요:");
        eprintln!("{}\n", auth_url.as_str());

        if should_open_browser() {
            let _ = open::that(auth_url.as_str());
        }

        let code = tokio::time::timeout(
            Duration::from_secs(LOGIN_CALLBACK_TIMEOUT_SECS),
            accept_pkce_callback(&listener, &expected_state),
        )
        .await
        .map_err(|_| anyhow!("브라우저 로그인 콜백 대기 시간이 초과되었습니다 (5분)."))??;

        let token_url = self.token_endpoint(tenant_id);
        let params = [
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", code_verifier.as_str()),
            ("scope", scope.as_str()),
        ];
        let tok = self.post_token_form(&token_url, &params).await?;
        let expires_in = tok.expires_in.unwrap_or(3600).max(60);

        Ok(StoredTokens {
            access_token: tok.access_token,
            refresh_token: tok.refresh_token,
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: client_id.to_string(),
            tenant_id: tenant_id.to_string(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        })
    }

    fn authorize_endpoint(&self, tenant_id: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/authorize",
            self.config.auth_base_url.trim_end_matches('/'),
            tenant_id
        )
    }

    fn token_endpoint(&self, tenant_id: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/token",
            self.config.auth_base_url.trim_end_matches('/'),
            tenant_id
        )
    }

    fn devicecode_endpoint(&self, tenant_id: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/devicecode",
            self.config.auth_base_url.trim_end_matches('/'),
            tenant_id
        )
    }

    async fn post_token_form(&self, url: &str, params: &[(&str, &str)]) -> Result<TokenResponse> {
        let mut req = self.http.post(url).form(params);
        if let Ok(origin) = std::env::var("TEAMS_TOKEN_ORIGIN") {
            let trimmed = origin.trim();
            if !trimmed.is_empty() {
                req = req.header("Origin", trimmed);
            }
        }
        let res = req
            .send()
            .await
            .context("OAuth 토큰 요청 전송 실패")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&text) {
                let desc = err.error_description.unwrap_or_default();
                bail!("OAuth 토큰 오류 ({} - {}): {}", status, err.error, desc);
            }
            bail!("OAuth 토큰 오류 ({}): {}", status, text);
        }
        serde_json::from_str::<TokenResponse>(&text).context("OAuth 토큰 JSON 파싱 실패")
    }

    pub async fn update_identity(
        &self,
        user_id: String,
        display_name: String,
        email: Option<String>,
    ) -> Result<()> {
        let mut guard = self.tokens.write().await;
        if let Some(ref mut tok) = *guard {
            tok.user_id = Some(user_id);
            tok.user_display_name = Some(display_name);
            tok.user_email = email;
            if !matches!(
                tok.auth_mode.as_str(),
                "env_token" | "token_file" | "refresh_cmd"
            ) {
                self.persist_tokens(tok)?;
            }
        }
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        let mut guard = self.tokens.write().await;
        *guard = None;
        delete_stored_tokens()
    }
}

pub fn load_stored_tokens() -> Result<Option<StoredTokens>> {
    let mut keyring_tok: Option<StoredTokens> = None;
    if should_use_keyring() {
        #[cfg(target_os = "linux")]
        {
            if let Some(Ok(secret_json)) = run_linux_keyring_op(|| {
                let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
                entry.get_password()
            }) {
                if !secret_json.trim().is_empty() {
                    if let Ok(parsed) = serde_json::from_str::<StoredTokens>(&secret_json) {
                        keyring_tok = Some(parsed);
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
                if let Ok(secret_json) = entry.get_password() {
                    if !secret_json.trim().is_empty() {
                        if let Ok(parsed) = serde_json::from_str::<StoredTokens>(&secret_json) {
                            keyring_tok = Some(parsed);
                        }
                    }
                }
            }
        }
    }
    let file_tok = load_tokens_from_path(&credential_fallback_path())?;
    match (keyring_tok, file_tok) {
        (Some(kr), Some(fl)) => {
            if fl.expires_at > kr.expires_at {
                Ok(Some(fl))
            } else {
                Ok(Some(kr))
            }
        }
        (Some(kr), None) => Ok(Some(kr)),
        (None, Some(fl)) => Ok(Some(fl)),
        (None, None) => Ok(None),
    }
}

pub fn save_stored_tokens(tokens: &StoredTokens) -> Result<()> {
    let fallback = credential_fallback_path();
    if !should_use_keyring() {
        return save_tokens_to_path(tokens, &fallback);
    }

    let json = serde_json::to_string_pretty(tokens)?;
    #[cfg(target_os = "linux")]
    {
        let json_for_keyring = json;
        if let Some(Ok(())) = run_linux_keyring_op(move || {
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
            entry.set_password(&json_for_keyring)
        }) {
            // Mirror to fallback file for consistency across environments.
            let _ = save_tokens_to_path(tokens, &fallback);
            return Ok(());
        }
        // Secret Service unavailable, errored, or timed out: fall back to 0600 credentials.json.
        return save_tokens_to_path(tokens, &fallback);
    }
    #[cfg(not(target_os = "linux"))]
    {
        match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            Ok(entry) => {
                if entry.set_password(&json).is_ok() {
                    // Mirror to fallback file for consistency across environments.
                    let _ = save_tokens_to_path(tokens, &fallback);
                    return Ok(());
                }
                // Keyring failed (e.g., Windows Credential blob limit or headless daemon):
                // remove any stale keyring entry and persist to fallback file.
                let _ = entry.delete_credential();
                save_tokens_to_path(tokens, &fallback)
            }
            Err(_) => save_tokens_to_path(tokens, &fallback),
        }
    }
}

pub fn delete_stored_tokens() -> Result<()> {
    if should_use_keyring() {
        #[cfg(target_os = "linux")]
        {
            let _ = run_linux_keyring_op(|| {
                let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                    Err(e) => Err(e),
                }
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(_) => {}
                }
            }
        }
    }
    delete_tokens_at_path(&credential_fallback_path())
}

pub(crate) fn load_tokens_from_path(path: &Path) -> Result<Option<StoredTokens>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("자격 증명 파일 읽기 실패: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let parsed: StoredTokens = serde_json::from_str(&raw)
        .with_context(|| format!("자격 증명 파일 파싱 실패: {}", path.display()))?;
    Ok(Some(parsed))
}

pub(crate) fn save_tokens_to_path(tokens: &StoredTokens, path: &Path) -> Result<()> {
    let json = serde_json::to_vec_pretty(tokens)?;
    write_atomic(path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub(crate) fn delete_tokens_at_path(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("자격 증명 파일 삭제 실패: {}", path.display()))?;
    }
    Ok(())
}

/// Computes RFC 7636 PKCE `S256` challenge: `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`.
pub fn compute_pkce_s256(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn generate_random_b64(num_bytes: usize) -> String {
    let mut buf = vec![0u8; num_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn build_auth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("teams-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("HTTP 클라이언트 초기화 실패")
}

fn is_truthy_env(val: Option<&str>) -> bool {
    val.map(|v| {
        let t = v.trim().to_ascii_lowercase();
        matches!(t.as_str(), "1" | "true" | "yes")
    })
    .unwrap_or(false)
}

fn keyring_disabled() -> bool {
    is_truthy_env(std::env::var("TEAMS_DISABLE_KEYRING").ok().as_deref())
}

/// Checks whether a `DBUS_SESSION_BUS_ADDRESS` value points to a plausible session bus.
/// Missing/empty addresses or `unix:path=<p>` entries whose socket path does not exist
/// return `false` so headless Linux/SSH environments skip D-Bus Secret Service immediately.
fn linux_secret_service_likely_available_with<F>(
    dbus_addr: Option<&str>,
    runtime_failed: bool,
    path_exists: F,
) -> bool
where
    F: Fn(&Path) -> bool,
{
    if runtime_failed {
        return false;
    }
    let Some(raw) = dbus_addr.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };

    for segment in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(params) = segment.strip_prefix("unix:") {
            let mut socket_path: Option<&str> = None;
            for kv in params.split(',') {
                if let Some(p) = kv.trim().strip_prefix("path=") {
                    socket_path = Some(p.trim());
                    break;
                }
            }
            match socket_path {
                Some(p) if !p.is_empty() && path_exists(Path::new(p)) => return true,
                Some(_) => continue,
                None => return true, // e.g., unix:abstract=...
            }
        } else {
            // Non-unix transport (e.g., tcp:, systemd:, launchd:)
            return true;
        }
    }

    false
}

#[cfg(target_os = "linux")]
fn linux_secret_service_likely_available() -> bool {
    let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
    linux_secret_service_likely_available_with(
        addr.as_deref(),
        KEYRING_RUNTIME_FAILED.load(Ordering::Relaxed),
        |p| p.exists(),
    )
}

fn should_use_keyring() -> bool {
    if keyring_disabled() || KEYRING_RUNTIME_FAILED.load(Ordering::Relaxed) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if !linux_secret_service_likely_available() {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn run_linux_keyring_op<T, F>(op: F) -> Option<std::result::Result<T, keyring::Error>>
where
    T: Send + 'static,
    F: FnOnce() -> std::result::Result<T, keyring::Error> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("teams-keyring".to_string())
        .spawn(move || {
            let _ = tx.send(op());
        })
        .is_err()
    {
        KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
        return None;
    }

    match rx.recv_timeout(LINUX_KEYRING_TIMEOUT) {
        Ok(Ok(val)) => Some(Ok(val)),
        Ok(Err(err)) => {
            if !matches!(err, keyring::Error::NoEntry) {
                KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
            }
            Some(Err(err))
        }
        Err(_) => {
            KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
            None
        }
    }
}

fn should_open_browser_with_env(
    is_unix_non_macos: bool,
    no_browser: Option<&str>,
    display: Option<&str>,
    wayland_display: Option<&str>,
    wsl_distro_name: Option<&str>,
) -> bool {
    if is_truthy_env(no_browser) {
        return false;
    }
    if !is_unix_non_macos {
        return true;
    }
    let has_non_empty = |v: Option<&str>| v.map(|s| !s.trim().is_empty()).unwrap_or(false);
    has_non_empty(display)
        || has_non_empty(wayland_display)
        || has_non_empty(wsl_distro_name)
}

fn should_open_browser() -> bool {
    should_open_browser_with_env(
        cfg!(all(unix, not(target_os = "macos"))),
        std::env::var("TEAMS_NO_BROWSER").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("WSL_DISTRO_NAME").ok().as_deref(),
    )
}

fn env_client_secret() -> Option<String> {
    std::env::var("TEAMS_CLIENT_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn should_skip_keyring_on_init(config: &AppConfig) -> bool {
    env_access_token(config).is_some()
        || config.resolved_token_file().is_some()
        || !config.token_refresh_cmd.trim().is_empty()
}

fn env_access_token(config: &AppConfig) -> Option<StoredTokens> {
    let raw = std::env::var("TEAMS_ACCESS_TOKEN").ok()?;
    env_access_token_from_str(&raw, config)
}

fn env_access_token_from_str(raw: &str, config: &AppConfig) -> Option<StoredTokens> {
    let trimmed = strip_bearer_prefix(raw);
    if trimmed.is_empty() {
        return None;
    }
    Some(stored_tokens_from_jwt(
        trimmed,
        None,
        None,
        &config.client_id,
        &config.tenant_id,
        "env_token",
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct JwtClaims {
    exp: Option<i64>,
    user_id: Option<String>,
    user_display_name: Option<String>,
    user_email: Option<String>,
    tenant_id: Option<String>,
    client_id: Option<String>,
}

fn strip_bearer_prefix(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    }
}

fn parse_jwt_claims(token: &str) -> Option<JwtClaims> {
    let token = strip_bearer_prefix(token);
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

    let claim_str = |key: &str| -> Option<String> {
        val.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let exp = val
        .get("exp")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));

    Some(JwtClaims {
        exp,
        user_id: claim_str("oid").or_else(|| claim_str("sub")),
        user_display_name: claim_str("name"),
        user_email: claim_str("upn")
            .or_else(|| claim_str("preferred_username"))
            .or_else(|| claim_str("email")),
        tenant_id: claim_str("tid"),
        client_id: claim_str("appid").or_else(|| claim_str("azp")),
    })
}

fn stored_tokens_from_jwt(
    raw_token: &str,
    refresh_token: Option<String>,
    expires_at_override: Option<i64>,
    fallback_client_id: &str,
    fallback_tenant_id: &str,
    auth_mode: &str,
) -> StoredTokens {
    let token = strip_bearer_prefix(raw_token).to_string();
    let claims = parse_jwt_claims(&token).unwrap_or_default();
    let expires_at = expires_at_override
        .or(claims.exp)
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);
    StoredTokens {
        access_token: token,
        refresh_token,
        expires_at,
        client_id: claims
            .client_id
            .unwrap_or_else(|| fallback_client_id.to_string()),
        tenant_id: claims
            .tenant_id
            .unwrap_or_else(|| fallback_tenant_id.to_string()),
        user_id: claims.user_id,
        user_display_name: claims.user_display_name,
        user_email: claims.user_email,
        auth_mode: auth_mode.to_string(),
    }
}

fn json_non_empty_str(val: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        val.get(*k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn parse_expiry_value(val: &serde_json::Value) -> Option<i64> {
    use chrono::TimeZone;
    if let Some(ts) = val.as_i64() {
        return Some(ts);
    }
    if let Some(f) = val.as_f64() {
        return Some(f as i64);
    }
    let s = val.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(ts) = s.parse::<i64>() {
        return Some(ts);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%:z",
        "%Y-%m-%d %H:%M:%S%.f %z",
        "%Y-%m-%d %H:%M:%S %z",
    ] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp());
        }
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = chrono::Local.from_local_datetime(&ndt).earliest() {
                return Some(dt.timestamp());
            }
        }
    }
    None
}

fn parse_external_token_str(
    raw: &str,
    auth_mode: &str,
    config: &AppConfig,
    source_label: &str,
) -> Result<StoredTokens> {
    let cleaned = raw.trim_start_matches('\u{feff}').trim();
    if cleaned.is_empty() {
        bail!("토큰 파일이 비어 있습니다: {source_label}");
    }

    if cleaned.starts_with('{') {
        let val: serde_json::Value = serde_json::from_str(cleaned)
            .with_context(|| format!("외부 토큰 JSON 파싱 실패: {source_label}"))?;
        let raw_token = json_non_empty_str(&val, &["access_token", "accessToken", "token"])
            .ok_or_else(|| {
                anyhow!("외부 토큰 JSON에 access_token/accessToken/token 필드가 없습니다: {source_label}")
            })?;
        let refresh_token = json_non_empty_str(&val, &["refresh_token", "refreshToken"]);
        let claims_exp = parse_jwt_claims(&raw_token).and_then(|c| c.exp);
        let exp_override = ["expires_at", "expires_on", "expiresOn"]
            .iter()
            .find_map(|k| val.get(*k).and_then(parse_expiry_value))
            .or(claims_exp)
            .or_else(|| {
                val.get("expires_in")
                    .and_then(|v| v.as_i64())
                    .map(|secs| chrono::Utc::now().timestamp() + secs)
            });

        let mut tok = stored_tokens_from_jwt(
            &raw_token,
            refresh_token,
            exp_override,
            &config.client_id,
            &config.tenant_id,
            auth_mode,
        );
        if let Some(cid) = json_non_empty_str(&val, &["client_id", "clientId"]) {
            tok.client_id = cid;
        }
        if let Some(tid) = json_non_empty_str(&val, &["tenant_id", "tenantId", "tenant"]) {
            tok.tenant_id = tid;
        }
        if tok.user_id.is_none() {
            tok.user_id = json_non_empty_str(&val, &["user_id", "userId"]);
        }
        if tok.user_display_name.is_none() {
            tok.user_display_name =
                json_non_empty_str(&val, &["user_display_name", "userDisplayName"]);
        }
        if tok.user_email.is_none() {
            tok.user_email = json_non_empty_str(&val, &["user_email", "userEmail"]);
        }
        return Ok(tok);
    }

    let first_line = cleaned
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("토큰 파일이 비어 있습니다: {source_label}"))?;
    let token = strip_bearer_prefix(first_line);
    if token.is_empty() {
        bail!("토큰 파일이 비어 있습니다: {source_label}");
    }
    Ok(stored_tokens_from_jwt(
        token,
        None,
        None,
        &config.client_id,
        &config.tenant_id,
        auth_mode,
    ))
}

/// Reads an external access token (raw JWT or JSON from `tc-auth` / Azure CLI / `StoredTokens`)
/// from `path`, normalizing BOM, optional `Bearer ` prefix, and JWT claims.
pub fn read_external_token_file(path: &Path, config: &AppConfig) -> Result<StoredTokens> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("외부 토큰 파일 읽기 실패: {}", path.display()))?;
    parse_external_token_str(&raw, "token_file", config, &path.display().to_string())
}

async fn execute_external_refresh_shell(cmd_str: &str, token_file: Option<&Path>) -> Result<String> {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut std_cmd = std::process::Command::new("cmd");
        std_cmd.raw_arg(format!("/S /C \"{cmd_str}\""));
        tokio::process::Command::from(std_cmd)
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd_str);
        c
    };
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    if let Some(path) = token_file {
        cmd.env("TEAMS_TOKEN_FILE", path);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("외부 토큰 갱신 명령 실행 실패: {cmd_str}"))?;
    let output = tokio::time::timeout(EXTERNAL_REFRESH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("외부 토큰 갱신 명령 시간 초과 (60초)"))??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr
        };
        let code_str = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        if detail.is_empty() {
            bail!("exit code {code_str}");
        }
        bail!("exit code {code_str}: {detail}");
    }
    Ok(stdout)
}

fn fallback_or_bail_refresh_error(
    current: Option<&StoredTokens>,
    err: &str,
) -> Result<StoredTokens> {
    if let Some(cur) = current {
        if !cur.is_expired_soon(0) {
            if !REFRESH_CMD_WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "경고: 외부 토큰 갱신 명령 실패 ({err}); 기존 토큰 만료 시각({})까지 계속 사용합니다.",
                    format_local_exp(cur.expires_at)
                );
            }
            return Ok(cur.clone());
        }
        bail!(
            "외부 토큰 갱신 실패 (exp: {}): {err}",
            format_local_exp(cur.expires_at)
        );
    }
    bail!("외부 토큰 갱신 실패 (exp: 없음): {err}");
}

fn format_local_exp(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => ts.to_string(),
    }
}

async fn accept_pkce_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await.context("콜백 연결 수락 실패")?;

        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        loop {
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= 16384 {
                break;
            }
        }

        let req_text = String::from_utf8_lossy(&buf);
        let first_line = req_text.lines().next().unwrap_or("");
        let target = first_line.split_whitespace().nth(1).unwrap_or("/");

        let parsed = match Url::parse(&format!("http://localhost{target}")) {
            Ok(u) => u,
            Err(_) => {
                let _ = write_http_response(
                    &mut socket,
                    "400 Bad Request",
                    "<h1>Invalid request</h1>",
                )
                .await;
                continue;
            }
        };

        if !matches!(parsed.path(), "/" | "/callback") {
            let _ =
                write_http_response(&mut socket, "404 Not Found", "<h1>Not Found</h1>").await;
            continue;
        }

        let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();

        if let Some(err) = params.get("error") {
            let desc = params
                .get("error_description")
                .cloned()
                .unwrap_or_default();
            let html = format!(
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h2>로그인 실패</h2><p>{}: {}</p></body></html>",
                html_escape(err),
                html_escape(&desc)
            );
            let _ = write_http_response(&mut socket, "400 Bad Request", &html).await;
            bail!("브라우저 OAuth 인증 실패 ({err}): {desc}");
        }

        let state = params.get("state").map(String::as_str).unwrap_or("");
        if state != expected_state {
            let _ = write_http_response(
                &mut socket,
                "400 Bad Request",
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h2>State mismatch</h2><p>보안 검증(state)이 일치하지 않습니다.</p></body></html>",
            )
            .await;
            bail!("OAuth state 값이 일치하지 않습니다.");
        }

        let code = match params.get("code").filter(|s| !s.is_empty()) {
            Some(c) => c.clone(),
            None => {
                let _ = write_http_response(
                    &mut socket,
                    "400 Bad Request",
                    "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h2>Missing code</h2><p>인증 코드가 누락되었습니다.</p></body></html>",
                )
                .await;
                bail!("OAuth 콜백에 authorization code가 없습니다.");
            }
        };

        let success_html = r#"<!doctype html>
<html lang="ko">
<head><meta charset="utf-8"><title>Teams CLI Login</title></head>
<body style="font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#f5f5f7;color:#1d1d1f">
  <div style="background:#fff;padding:2.5rem 3rem;border-radius:12px;box-shadow:0 4px 20px rgba(0,0,0,.08);text-align:center">
    <h1 style="margin:0 0 .5rem;color:#464eb8">Microsoft Teams CLI 로그인 완료</h1>
    <p style="margin:0;color:#555">인증이 성공적으로 완료되었습니다. 이 창을 닫고 터미널로 돌아가세요.</p>
  </div>
</body>
</html>"#;
        let _ = write_http_response(&mut socket, "200 OK", success_html).await;
        return Ok(code);
    }
}

async fn write_http_response(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    body_html: &str,
) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_html}",
        body_html.len()
    );
    socket.write_all(resp.as_bytes()).await?;
    socket.flush().await?;
    let _ = socket.shutdown().await;
    Ok(())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    expires_in: i64,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_pkce_s256_rfc7636_vector() {
        // RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(compute_pkce_s256(verifier), expected);
    }

    #[test]
    fn test_stored_tokens_expiry_skew() {
        let now = chrono::Utc::now().timestamp();
        let fresh = StoredTokens {
            access_token: "tok".into(),
            refresh_token: Some("rt".into()),
            expires_at: now + 300,
            client_id: "cid".into(),
            tenant_id: "common".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        assert!(!fresh.is_expired_soon(90));
        assert!(fresh.is_expired_soon(300));

        let expiring = StoredTokens {
            expires_at: now + 30,
            ..fresh
        };
        assert!(expiring.is_expired_soon(90));
        assert!(!expiring.is_expired_soon(10));
    }

    #[test]
    fn test_file_fallback_roundtrip_and_delete() {
        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("nested").join("credentials.json");

        assert!(load_tokens_from_path(&cred_path).unwrap().is_none());

        let tokens = StoredTokens {
            access_token: "eyJ-access".into(),
            refresh_token: Some("0.AR-refresh".into()),
            expires_at: 1_900_000_000,
            client_id: "client-123".into(),
            tenant_id: "tenant-456".into(),
            user_id: Some("user-1".into()),
            user_display_name: Some("홍길동".into()),
            user_email: Some("hong@example.com".into()),
            auth_mode: "pkce".into(),
        };

        save_tokens_to_path(&tokens, &cred_path).unwrap();
        assert!(cred_path.exists());

        let loaded = load_tokens_from_path(&cred_path).unwrap().unwrap();
        assert_eq!(loaded, tokens);

        delete_tokens_at_path(&cred_path).unwrap();
        assert!(!cred_path.exists());
        assert!(load_tokens_from_path(&cred_path).unwrap().is_none());
    }

    #[tokio::test]
    async fn test_direct_access_token_login_and_valid_token() {
        let mut cfg = AppConfig::default();
        cfg.client_id = "cid".into();
        cfg.tenant_id = "tid".into();

        // Construct without touching real user keyring/config by using from_tokens or temp TEAMS_CONFIG_DIR
        let tok = StoredTokens {
            access_token: "direct-tok".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 1800,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let mgr = AuthManager::from_tokens(cfg, tok);
        assert_eq!(mgr.valid_access_token().await.unwrap(), "direct-tok");
        assert!(!mgr.is_refreshable().await);
    }

    #[tokio::test]
    async fn test_client_credentials_login_and_concurrent_refresh() {
        use axum::{routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();

        let app = Router::new().route(
            "/tenant-1/oauth2/v2.0/token",
            post(move |body: String| {
                let hits = hits_clone.clone();
                async move {
                    let count = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    if body.contains("grant_type=client_credentials") {
                        Json(serde_json::json!({
                            "access_token": format!("cc-token-{count}"),
                            "expires_in": 3600
                        }))
                    } else {
                        Json(serde_json::json!({
                            "access_token": format!("refreshed-token-{count}"),
                            "refresh_token": "new-rt",
                            "expires_in": 3600
                        }))
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("creds.json");

        let mut cfg = AppConfig::default();
        cfg.auth_base_url = format!("http://{addr}");
        cfg.client_id = "cid".into();
        cfg.tenant_id = "tenant-1".into();

        // 1. Client credentials login
        let dummy = StoredTokens {
            access_token: "old".into(),
            refresh_token: Some("rt-0".into()),
            expires_at: 0,
            client_id: "cid".into(),
            tenant_id: "tenant-1".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let mgr = AuthManager::from_tokens(cfg.clone(), dummy).with_token_path(cred_path.clone());
        let logged_in = mgr
            .login(&LoginOptions {
                client_secret: Some("secret-xyz".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(logged_in.auth_mode, "client_credentials");
        assert_eq!(logged_in.access_token, "cc-token-1");

        // 2. Concurrent refresh_for_failed_token prevents thundering herd
        let expired_tok = StoredTokens {
            access_token: "stale-tok".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: chrono::Utc::now().timestamp() + 300,
            client_id: "cid".into(),
            tenant_id: "tenant-1".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let mgr2 = Arc::new(
            AuthManager::from_tokens(cfg, expired_tok.clone()).with_token_path(cred_path.clone()),
        );
        // Pin the on-disk credential to the same stale token so the hot-reload path
        // cannot short-circuit the OAuth refresh this test exercises. The assertions
        // below are unchanged: concurrent callers must coalesce into one token request.
        save_tokens_to_path(&expired_tok, &cred_path).unwrap();
        let hits_before = hits.load(Ordering::SeqCst);
        let (r1, r2, r3) = tokio::join!(
            mgr2.refresh_for_failed_token("stale-tok"),
            mgr2.refresh_for_failed_token("stale-tok"),
            mgr2.refresh_for_failed_token("stale-tok"),
        );
        assert_eq!(r1.unwrap().access_token, r2.unwrap().access_token);
        assert_eq!(r3.unwrap().refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(hits.load(Ordering::SeqCst) - hits_before, 1);
    }

    #[tokio::test]
    async fn test_hot_reload_external_f12_token_from_disk() {
        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("credentials.json");
        let now = chrono::Utc::now().timestamp();

        let initial = StoredTokens {
            access_token: "f12-old-token".into(),
            refresh_token: None,
            expires_at: now + 20, // within 90s skew -> triggers hot-reload check
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: Some("u1".into()),
            user_display_name: Some("Tester".into()),
            user_email: None,
            auth_mode: "access_token".into(),
        };
        save_tokens_to_path(&initial, &cred_path).unwrap();

        let mgr = AuthManager::from_tokens(AppConfig::default(), initial)
            .with_token_path(cred_path.clone());

        // External refresher script updates `credentials.json` on disk without restarting TUI
        let externally_refreshed = StoredTokens {
            access_token: "f12-fresh-token".into(),
            refresh_token: None,
            expires_at: now + 3600,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: Some("u1".into()),
            user_display_name: Some("Tester".into()),
            user_email: None,
            auth_mode: "access_token".into(),
        };
        save_tokens_to_path(&externally_refreshed, &cred_path).unwrap();

        assert_eq!(mgr.valid_access_token().await.unwrap(), "f12-fresh-token");
    }

    #[test]
    fn test_linux_secret_service_availability_detection() {
        let dir = tempdir().unwrap();
        let existing_socket = dir.path().join("bus");
        std::fs::write(&existing_socket, b"").unwrap();
        let existing_str = existing_socket.to_string_lossy().to_string();
        let missing_str = dir.path().join("missing-bus").to_string_lossy().to_string();

        // Missing or empty DBUS_SESSION_BUS_ADDRESS -> unavailable
        assert!(!linux_secret_service_likely_available_with(
            None,
            false,
            |p| p.exists()
        ));
        assert!(!linux_secret_service_likely_available_with(
            Some("   "),
            false,
            |p| p.exists()
        ));

        // unix:path pointing to missing socket -> unavailable
        let missing_addr = format!("unix:path={missing_str}");
        assert!(!linux_secret_service_likely_available_with(
            Some(&missing_addr),
            false,
            |p| p.exists()
        ));

        // unix:path pointing to existing socket (with extra attributes) -> available
        let existing_addr = format!("unix:path={existing_str},guid=123456");
        assert!(linux_secret_service_likely_available_with(
            Some(&existing_addr),
            false,
            |p| p.exists()
        ));

        // Previously failed keyring runtime -> unavailable even with valid socket
        assert!(!linux_secret_service_likely_available_with(
            Some(&existing_addr),
            true,
            |p| p.exists()
        ));

        // Multi-segment where second segment exists -> available
        let multi_addr = format!("unix:path={missing_str};unix:path={existing_str}");
        assert!(linux_secret_service_likely_available_with(
            Some(&multi_addr),
            false,
            |p| p.exists()
        ));

        // Abstract unix socket -> available
        assert!(linux_secret_service_likely_available_with(
            Some("unix:abstract=/tmp/dbus-test"),
            false,
            |p| p.exists()
        ));
    }

    #[test]
    fn test_should_open_browser_headless_linux_and_overrides() {
        // Explicit opt-out disables browser everywhere
        assert!(!should_open_browser_with_env(
            false,
            Some("1"),
            Some(":0"),
            None,
            None
        ));
        assert!(!should_open_browser_with_env(
            true,
            Some("true"),
            Some(":0"),
            Some("wayland-0"),
            Some("Ubuntu")
        ));

        // Windows / macOS defaults to true unless TEAMS_NO_BROWSER is set
        assert!(should_open_browser_with_env(false, None, None, None, None));

        // Headless Linux / SSH without DISPLAY, WAYLAND_DISPLAY, or WSL -> false
        assert!(!should_open_browser_with_env(true, None, None, None, None));
        assert!(!should_open_browser_with_env(
            true,
            Some("0"),
            Some("  "),
            Some(""),
            None
        ));

        // X11, Wayland, or WSL -> true
        assert!(should_open_browser_with_env(
            true,
            None,
            Some(":0"),
            None,
            None
        ));
        assert!(should_open_browser_with_env(
            true,
            None,
            None,
            Some("wayland-0"),
            None
        ));
        assert!(should_open_browser_with_env(
            true,
            None,
            None,
            None,
            Some("AlmaLinux-8")
        ));
    }

    fn make_test_jwt_with_encoder(payload: &serde_json::Value, standard_padded: bool) -> String {
        use base64::engine::general_purpose::STANDARD;
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let json_bytes = serde_json::to_vec(payload).unwrap();
        let body = if standard_padded {
            STANDARD.encode(json_bytes)
        } else {
            URL_SAFE_NO_PAD.encode(json_bytes)
        };
        format!("{header}.{body}.sig")
    }

    #[test]
    fn test_parse_jwt_claims_padded_and_standard_base64() {
        let exp = 1_893_456_000_i64;
        // Include characters that produce '+' / '/' and '=' padding in standard base64
        let payload = serde_json::json!({
            "exp": exp,
            "oid": "oid-user-123?>",
            "name": "김철수",
            "upn": "chulsoo@contoso.com",
            "tid": "tenant-abc",
            "appid": "app-xyz"
        });
        let jwt_std = make_test_jwt_with_encoder(&payload, true);
        let jwt_url = make_test_jwt_with_encoder(&payload, false);

        for tok in [&jwt_std, &jwt_url, &format!("Bearer {jwt_std}")] {
            let claims = parse_jwt_claims(tok).expect("claims must parse");
            assert_eq!(claims.exp, Some(exp));
            assert_eq!(claims.user_id.as_deref(), Some("oid-user-123?>"));
            assert_eq!(claims.user_display_name.as_deref(), Some("김철수"));
            assert_eq!(claims.user_email.as_deref(), Some("chulsoo@contoso.com"));
            assert_eq!(claims.tenant_id.as_deref(), Some("tenant-abc"));
            assert_eq!(claims.client_id.as_deref(), Some("app-xyz"));
        }

        // Fallbacks: sub, preferred_username, azp
        let fallback_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": exp,
                "sub": "sub-999",
                "preferred_username": "pref@contoso.com",
                "azp": "azp-111"
            }),
            false,
        );
        let fallback_claims = parse_jwt_claims(&fallback_jwt).unwrap();
        assert_eq!(fallback_claims.user_id.as_deref(), Some("sub-999"));
        assert_eq!(
            fallback_claims.user_email.as_deref(),
            Some("pref@contoso.com")
        );
        assert_eq!(fallback_claims.client_id.as_deref(), Some("azp-111"));
    }

    #[test]
    fn test_read_external_token_file_raw_and_azure_cli_json() {
        let dir = tempdir().unwrap();
        let cfg = AppConfig::default();
        let future_exp = chrono::Utc::now().timestamp() + 3600;
        let jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": future_exp,
                "oid": "oid-42",
                "name": "Hong",
                "upn": "hong@example.com",
                "tid": "tid-42",
                "appid": "cid-42"
            }),
            true,
        );

        // 1. Raw JWT file with UTF-8 BOM, leading/trailing blank lines, and Bearer prefix
        let raw_path = dir.path().join("access_token");
        std::fs::write(&raw_path, format!("\u{feff}\n  Bearer {jwt}  \n\n")).unwrap();
        let raw_tok = read_external_token_file(&raw_path, &cfg).unwrap();
        assert_eq!(raw_tok.access_token, jwt);
        assert_eq!(raw_tok.expires_at, future_exp);
        assert_eq!(raw_tok.user_id.as_deref(), Some("oid-42"));
        assert_eq!(raw_tok.user_display_name.as_deref(), Some("Hong"));
        assert_eq!(raw_tok.user_email.as_deref(), Some("hong@example.com"));
        assert_eq!(raw_tok.tenant_id, "tid-42");
        assert_eq!(raw_tok.client_id, "cid-42");
        assert_eq!(raw_tok.auth_mode, "token_file");

        // 2. Azure CLI camelCase JSON (`accessToken`, `expiresOn`)
        let az_path = dir.path().join("az.json");
        let az_json = serde_json::json!({
            "accessToken": jwt,
            "expiresOn": "2030-01-15T12:00:00Z",
            "tenant": "az-tenant",
            "tokenType": "Bearer"
        });
        std::fs::write(&az_path, az_json.to_string()).unwrap();
        let az_tok = read_external_token_file(&az_path, &cfg).unwrap();
        assert_eq!(az_tok.access_token, jwt);
        assert_eq!(az_tok.expires_at, 1_894_708_800);
        assert_eq!(az_tok.tenant_id, "az-tenant");
        assert_eq!(az_tok.user_id.as_deref(), Some("oid-42"));
        assert_eq!(az_tok.auth_mode, "token_file");

        // 3. Empty file returns clear error
        let empty_path = dir.path().join("empty");
        std::fs::write(&empty_path, "\u{feff}   \n").unwrap();
        let err = read_external_token_file(&empty_path, &cfg).unwrap_err();
        assert!(err.to_string().contains("비어 있습니다"));
    }

    #[tokio::test]
    async fn test_token_refresh_cmd_auto_refreshes_expiring_token_file() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("access_token");
        let now = chrono::Utc::now().timestamp();

        let soon_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": now + 60, // within default 5-min (300s) skew
                "oid": "u-1",
                "name": "Tester",
                "upn": "tester@example.com"
            }),
            false,
        );
        let fresh_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": now + 3600,
                "oid": "u-1",
                "name": "Tester",
                "upn": "tester@example.com"
            }),
            false,
        );
        std::fs::write(&token_path, &soon_jwt).unwrap();

        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        cfg.token_refresh_mins = 5;
        #[cfg(windows)]
        {
            cfg.token_refresh_cmd = format!("echo {fresh_jwt}> \"{}\"", token_path.display());
        }
        #[cfg(not(windows))]
        {
            cfg.token_refresh_cmd =
                format!("printf '%s\\n' '{fresh_jwt}' > '{}'", token_path.display());
        }

        let mgr = AuthManager::new(cfg).unwrap();
        assert!(mgr.is_refreshable().await);

        let returned = mgr.valid_access_token().await.unwrap();
        assert_eq!(returned, fresh_jwt);
        assert_eq!(
            mgr.current_tokens().await.unwrap().access_token,
            fresh_jwt
        );
    }

    #[tokio::test]
    async fn test_token_refresh_cmd_bootstraps_missing_token_file_and_stdout_only() {
        let dir = tempdir().unwrap();
        let missing_path = dir.path().join("bootstrapped_token");
        let now = chrono::Utc::now().timestamp();
        let fresh_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": now + 3600,
                "oid": "boot-oid",
                "upn": "boot@example.com"
            }),
            false,
        );

        // 1. Missing token_file bootstrapped via token_refresh_cmd writing the file
        let mut cfg = AppConfig::default();
        cfg.token_file = missing_path.to_string_lossy().to_string();
        #[cfg(windows)]
        {
            cfg.token_refresh_cmd = format!("echo {fresh_jwt}> \"{}\"", missing_path.display());
        }
        #[cfg(not(windows))]
        {
            cfg.token_refresh_cmd = format!(
                "printf '%s\\n' '{fresh_jwt}' > '{}'",
                missing_path.display()
            );
        }
        let mgr = AuthManager::new(cfg).unwrap();
        assert_eq!(mgr.valid_access_token().await.unwrap(), fresh_jwt);

        // 2. Stdout-only token_refresh_cmd (no token_file)
        let mut cfg_stdout = AppConfig::default();
        #[cfg(windows)]
        {
            cfg_stdout.token_refresh_cmd = format!("echo {fresh_jwt}");
        }
        #[cfg(not(windows))]
        {
            cfg_stdout.token_refresh_cmd = format!("printf '%s\\n' '{fresh_jwt}'");
        }
        let mgr_stdout = AuthManager::new(cfg_stdout).unwrap();
        let tok = mgr_stdout.current_tokens().await.unwrap();
        assert_eq!(tok.access_token, fresh_jwt);
        assert_eq!(tok.auth_mode, "refresh_cmd");
        assert_eq!(tok.user_id.as_deref(), Some("boot-oid"));
    }

    #[tokio::test]
    async fn test_token_refresh_cmd_failure_fallback_and_bail() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("access_token");
        let now = chrono::Utc::now().timestamp();

        // Still-valid (not hard-expired) token within skew_secs -> falls back with warning
        let soon_jwt = make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 120 }), false);
        std::fs::write(&token_path, &soon_jwt).unwrap();
        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        cfg.token_refresh_mins = 5;
        cfg.token_refresh_cmd = "exit 1".to_string();

        let mgr = AuthManager::new(cfg.clone()).unwrap();
        assert_eq!(mgr.valid_access_token().await.unwrap(), soon_jwt);

        // Hard-expired token + failing refresh_cmd -> bails with clear error
        let expired_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now - 60 }), false);
        std::fs::write(&token_path, &expired_jwt).unwrap();
        let err = mgr.valid_access_token().await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("외부 토큰 갱신 실패"), "unexpected error: {msg}");
        assert!(msg.contains("exp:"), "missing exp in error: {msg}");
    }

    #[tokio::test]
    async fn test_expired_external_tokens_return_clear_expiry_error() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("expired_token");
        let now = chrono::Utc::now().timestamp();
        let expired_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now - 120 }), false);
        std::fs::write(&token_path, &expired_jwt).unwrap();

        // 1. token_file without refresh_cmd
        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        let mgr_file = AuthManager::new(cfg.clone()).unwrap();
        assert!(!mgr_file.is_refreshable().await);
        let err_file = mgr_file.valid_access_token().await.unwrap_err();
        let msg_file = err_file.to_string();
        assert!(
            msg_file.contains("토큰 만료 (exp:") && msg_file.contains("auth_mode=token_file"),
            "unexpected token_file error: {msg_file}"
        );

        // 2. env_token without refresh_cmd (constructed via env_access_token_from_str to avoid env races)
        let env_tok = env_access_token_from_str(&expired_jwt, &AppConfig::default()).unwrap();
        assert_eq!(env_tok.auth_mode, "env_token");
        let mgr_env = AuthManager::from_tokens(AppConfig::default(), env_tok);
        let err_env = mgr_env.valid_access_token().await.unwrap_err();
        let msg_env = err_env.to_string();
        assert!(
            msg_env.contains("토큰 만료 (exp:") && msg_env.contains("auth_mode=env_token"),
            "unexpected env_token error: {msg_env}"
        );
    }

    #[tokio::test]
    async fn test_external_token_skips_keyring_and_persistence() {
        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("should_not_be_created_creds.json");
        let token_path = dir.path().join("ext_token");
        let now = chrono::Utc::now().timestamp();
        let fresh_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 3600 }), false);
        std::fs::write(&token_path, &fresh_jwt).unwrap();

        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        assert!(should_skip_keyring_on_init(&cfg));

        // Even with a fresh AuthManager pointing at token_file, update_identity must NOT persist to disk/keyring
        let ext_tok = read_external_token_file(&token_path, &cfg).unwrap();
        let mgr = AuthManager::from_tokens(cfg, ext_tok).with_token_path(cred_path.clone());
        mgr.update_identity(
            "u-ext".into(),
            "External User".into(),
            Some("ext@example.com".into()),
        )
        .await
        .unwrap();
        assert!(
            !cred_path.exists(),
            "external token must never be persisted to credentials.json/keyring"
        );
        let updated = mgr.current_tokens().await.unwrap();
        assert_eq!(updated.access_token, fresh_jwt);
    }

    #[tokio::test]
    async fn test_token_refresh_cmd_coalesces_concurrent_calls() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("access_token");
        let count_path = dir.path().join("runs.log");
        let now = chrono::Utc::now().timestamp();

        let soon_jwt = make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 60 }), false);
        let fresh_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 3600 }), false);
        std::fs::write(&token_path, &soon_jwt).unwrap();

        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        cfg.token_refresh_mins = 5;
        #[cfg(windows)]
        {
            cfg.token_refresh_cmd = format!(
                "echo hit>> \"{}\" && echo {fresh_jwt}> \"{}\"",
                count_path.display(),
                token_path.display()
            );
        }
        #[cfg(not(windows))]
        {
            cfg.token_refresh_cmd = format!(
                "echo hit >> '{}' && printf '%s\\n' '{fresh_jwt}' > '{}'",
                count_path.display(),
                token_path.display()
            );
        }

        let mgr = Arc::new(AuthManager::new(cfg).unwrap());
        let (r1, r2, r3) = tokio::join!(
            mgr.valid_access_token(),
            mgr.valid_access_token(),
            mgr.valid_access_token(),
        );
        assert_eq!(r1.unwrap(), fresh_jwt);
        assert_eq!(r2.unwrap(), fresh_jwt);
        assert_eq!(r3.unwrap(), fresh_jwt);
        let log = std::fs::read_to_string(&count_path).unwrap();
        assert_eq!(
            log.lines().count(),
            1,
            "concurrent refresh_cmd calls must coalesce into one execution"
        );
    }
}
