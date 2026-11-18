//! Microsoft Graph v1.0 client for Teams Chats, Teams/Channels/Threads, Media, Presence,
//! Outlook Mail, Calendar, Contacts, Microsoft To Do, & Microsoft OneNote.

pub mod calendar;
pub mod chats;
pub mod client;
pub mod contacts;
pub mod mail;
pub mod media;
pub mod onenote;
pub mod presence;
pub mod teams;
pub mod todo;

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;

use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{ConversationTarget, MemberInfo, MessageItem},
};

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    my_user_id: Option<String>,
    cached_my_user_id: Arc<OnceLock<String>>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("teams-cli/", env!("CARGO_PKG_VERSION")))
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

    // --- Unified Messaging Operations (Chats + Channel Posts) ---
    pub async fn list_members(&self, target: &ConversationTarget) -> Result<Vec<MemberInfo>> {
        match target {
            ConversationTarget::Chat { chat_id } => self.list_chat_members(chat_id).await,
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => self.list_channel_members(team_id, channel_id).await,
        }
    }

    pub async fn list_messages(
        &self,
        target: &ConversationTarget,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        match target {
            ConversationTarget::Chat { chat_id } => self.list_chat_messages(chat_id, limit).await,
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => self.list_channel_messages(team_id, channel_id, limit).await,
        }
    }
}
