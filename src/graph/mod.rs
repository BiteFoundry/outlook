//! Microsoft Graph v1.0 client for Outlook Mail, Calendar, Contacts, To Do, and Media attachments.

pub mod calendar;
pub mod client;
pub mod contacts;
pub mod mail;
pub mod media;
pub mod todo;

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;

use crate::{auth::AuthManager, config::AppConfig};

/// Canonical prefix emitted by `GraphClient` HTTP errors; kept as a shared constant for safe parsing.
pub const GRAPH_API_ERROR_PREFIX: &str = "Graph API error (";

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    my_user_id: Option<String>,
    cached_my_user_id: Arc<OnceLock<String>>,
    resolve_user_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("outlook-cli/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            auth,
            config,
            http,
            my_user_id: None,
            cached_my_user_id: Arc::new(OnceLock::new()),
            resolve_user_mutex: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn with_my_user_id(mut self, user_id: impl Into<String>) -> Self {
        let uid = user_id.into();
        let lock = OnceLock::new();
        let _ = lock.set(uid.clone());
        self.cached_my_user_id = Arc::new(lock);
        self.my_user_id = Some(uid);
        self
    }

    pub fn my_user_id(&self) -> Option<&str> {
        self.my_user_id
            .as_deref()
            .or_else(|| self.cached_my_user_id.get().map(String::as_str))
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}
