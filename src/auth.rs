//! OAuth 2.0 authentication (PKCE loopback, Device Code Flow, Client Secret / env token)
//! + OS keyring (`keyring`) with file fallback (`credentials.json`) + automatic refresh.

use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::AppConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    tokens: Arc<RwLock<Option<StoredTokens>>>,
}

impl AuthManager {
    pub fn new(config: AppConfig) -> Result<Self> {
        let loaded = load_stored_tokens()?;
        Ok(Self {
            config,
            tokens: Arc::new(RwLock::new(loaded)),
        })
    }

    pub fn from_tokens(config: AppConfig, tokens: StoredTokens) -> Self {
        Self {
            config,
            tokens: Arc::new(RwLock::new(Some(tokens))),
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub async fn current_tokens(&self) -> Option<StoredTokens> {
        if let Ok(raw) = std::env::var("TEAMS_ACCESS_TOKEN") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(StoredTokens {
                    access_token: trimmed.to_string(),
                    refresh_token: None,
                    expires_at: chrono::Utc::now().timestamp() + 3600,
                    client_id: self.config.client_id.clone(),
                    tenant_id: self.config.tenant_id.clone(),
                    user_id: None,
                    user_display_name: None,
                    user_email: None,
                    auth_mode: "env_token".to_string(),
                });
            }
        }
        self.tokens.read().await.clone()
    }

    /// Returns a valid Bearer access token, refreshing automatically when needed.
    pub async fn valid_access_token(&self) -> Result<String> {
        if let Some(tok) = self.current_tokens().await {
            if !tok.is_expired_soon(90) {
                return Ok(tok.access_token);
            }
            if tok.refresh_token.is_some() {
                let refreshed = self.refresh_now().await?;
                return Ok(refreshed.access_token);
            }
            return Ok(tok.access_token);
        }
        bail!("로그인이 필요합니다. 먼저 `teams login` 을 실행하세요.")
    }

    pub async fn login(&self, _opts: &LoginOptions) -> Result<StoredTokens> {
        bail!("auth::login not yet implemented")
    }

    pub async fn refresh_now(&self) -> Result<StoredTokens> {
        bail!("auth::refresh_now not yet implemented")
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
            save_stored_tokens(tok)?;
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
    Ok(None)
}

pub fn save_stored_tokens(_tokens: &StoredTokens) -> Result<()> {
    Ok(())
}

pub fn delete_stored_tokens() -> Result<()> {
    Ok(())
}
