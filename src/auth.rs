//! OAuth 2.0 authentication (PKCE loopback, Device Code Flow, Client Secret / env token)
//! + OS keyring (`keyring`) with file fallback (`credentials.json`) + automatic refresh.

use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
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
    #[cfg(test)]
    token_path_override: Option<std::path::PathBuf>,
}

impl AuthManager {
    pub fn new(config: AppConfig) -> Result<Self> {
        let loaded = load_stored_tokens()?;
        let http = build_auth_http_client()?;
        Ok(Self {
            config,
            http,
            tokens: Arc::new(RwLock::new(loaded)),
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

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub async fn current_tokens(&self) -> Option<StoredTokens> {
        if let Some(env_tok) = env_access_token(&self.config) {
            return Some(env_tok);
        }
        self.tokens.read().await.clone()
    }

    /// Returns a valid Bearer access token, refreshing automatically when needed.
    pub async fn valid_access_token(&self) -> Result<String> {
        if let Some(tok) = self.current_tokens().await {
            if !tok.is_expired_soon(90) {
                return Ok(tok.access_token);
            }
            if tok.auth_mode != "env_token" && self.can_refresh_token(&tok) {
                let refreshed = self.refresh_now().await?;
                return Ok(refreshed.access_token);
            }
            return Ok(tok.access_token);
        }
        bail!("로그인이 필요합니다. 먼저 `teams login` 을 실행하세요.")
    }

    /// Returns whether stored tokens can currently be refreshed (refresh token or client secret).
    pub async fn is_refreshable(&self) -> bool {
        if env_access_token(&self.config).is_some() {
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
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let expires_at = extract_jwt_exp(raw_token)
                .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);
            let stored = StoredTokens {
                access_token: raw_token.to_string(),
                refresh_token: None,
                expires_at,
                client_id,
                tenant_id,
                user_id: None,
                user_display_name: None,
                user_email: None,
                auth_mode: "access_token".to_string(),
            };
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
        let mut guard = self.tokens.write().await;
        self.refresh_locked(&mut guard).await
    }

    /// Refreshes the token only if it still matches `failed_access_token` (prevents thundering herd on 401).
    pub(crate) async fn refresh_for_failed_token(
        &self,
        failed_access_token: &str,
    ) -> Result<StoredTokens> {
        if env_access_token(&self.config).is_some() {
            bail!("TEAMS_ACCESS_TOKEN 환경 변수 토큰은 자동 갱신할 수 없습니다.");
        }
        let mut guard = self.tokens.write().await;
        if let Some(current) = guard.as_ref() {
            if current.access_token != failed_access_token && !current.is_expired_soon(30) {
                return Ok(current.clone());
            }
        }
        self.refresh_locked(&mut guard).await
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
        let res = self
            .http
            .post(url)
            .form(params)
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
            self.persist_tokens(tok)?;
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
    if !keyring_disabled() {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            if let Ok(secret_json) = entry.get_password() {
                if !secret_json.trim().is_empty() {
                    if let Ok(parsed) = serde_json::from_str::<StoredTokens>(&secret_json) {
                        return Ok(Some(parsed));
                    }
                }
            }
        }
    }
    load_tokens_from_path(&credential_fallback_path())
}

pub fn save_stored_tokens(tokens: &StoredTokens) -> Result<()> {
    let fallback = credential_fallback_path();
    if keyring_disabled() {
        return save_tokens_to_path(tokens, &fallback);
    }

    let json = serde_json::to_string_pretty(tokens)?;
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

pub fn delete_stored_tokens() -> Result<()> {
    if !keyring_disabled() {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(_) => {}
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

fn keyring_disabled() -> bool {
    std::env::var("TEAMS_DISABLE_KEYRING")
        .ok()
        .map(|v| {
            let t = v.trim().to_ascii_lowercase();
            matches!(t.as_str(), "1" | "true" | "yes")
        })
        .unwrap_or(false)
}

fn should_open_browser() -> bool {
    std::env::var("TEAMS_NO_BROWSER")
        .ok()
        .map(|v| {
            let t = v.trim().to_ascii_lowercase();
            !matches!(t.as_str(), "1" | "true" | "yes")
        })
        .unwrap_or(true)
}

fn env_client_secret() -> Option<String> {
    std::env::var("TEAMS_CLIENT_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_access_token(config: &AppConfig) -> Option<StoredTokens> {
    let raw = std::env::var("TEAMS_ACCESS_TOKEN").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expires_at =
        extract_jwt_exp(trimmed).unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);
    Some(StoredTokens {
        access_token: trimmed.to_string(),
        refresh_token: None,
        expires_at,
        client_id: config.client_id.clone(),
        tenant_id: config.tenant_id.clone(),
        user_id: None,
        user_display_name: None,
        user_email: None,
        auth_mode: "env_token".to_string(),
    })
}

fn extract_jwt_exp(token: &str) -> Option<i64> {
    let payload_b64 = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    val.get("exp").and_then(|v| v.as_i64())
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
            AuthManager::from_tokens(cfg, expired_tok).with_token_path(cred_path.clone()),
        );
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
}
