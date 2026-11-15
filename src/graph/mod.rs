//! Microsoft Graph v1.0 client for Teams Chats, Teams/Channels/Threads, Media (OneDrive/SharePoint/HostedContents), and Presence.

use std::path::Path;

use anyhow::{bail, Result};

use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{
        AttachmentInfo, ChannelSummary, ConversationSummary, ConversationTarget, DownloadedFile,
        MemberInfo, MessageItem, OutgoingMessage, PresenceStatus, TeamSummary, ThreadView,
        UploadedReferenceAttachment, UserProfile,
    },
};

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    my_user_id: Option<String>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("teams-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            auth,
            config,
            http,
            my_user_id: None,
        })
    }

    pub fn with_my_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.my_user_id = Some(user_id.into());
        self
    }

    pub fn my_user_id(&self) -> Option<&str> {
        self.my_user_id.as_deref()
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

    // --- Profile & Users ---
    pub async fn get_me(&self) -> Result<UserProfile> {
        bail!("graph::get_me not yet implemented")
    }

    pub async fn search_users(&self, _query: &str, _limit: usize) -> Result<Vec<UserProfile>> {
        bail!("graph::search_users not yet implemented")
    }

    // --- 1:1, Group, and Meeting Chats ---
    pub async fn list_chats(&self, _limit: usize) -> Result<Vec<ConversationSummary>> {
        bail!("graph::list_chats not yet implemented")
    }

    /// Creates a `oneOnOne` (1 peer user ID/email) or `group` (2+ peers, optional topic) chat.
    pub async fn create_chat(
        &self,
        _member_ids_or_emails: &[String],
        _topic: Option<&str>,
    ) -> Result<ConversationSummary> {
        bail!("graph::create_chat not yet implemented")
    }

    pub async fn list_chat_members(&self, _chat_id: &str) -> Result<Vec<MemberInfo>> {
        bail!("graph::list_chat_members not yet implemented")
    }

    pub async fn list_chat_messages(&self, _chat_id: &str, _limit: usize) -> Result<Vec<MessageItem>> {
        bail!("graph::list_chat_messages not yet implemented")
    }

    pub async fn mark_chat_read(&self, _chat_id: &str) -> Result<()> {
        bail!("graph::mark_chat_read not yet implemented")
    }

    // --- Teams, Channels, and Threads ---
    pub async fn list_joined_teams_with_channels(&self) -> Result<Vec<TeamSummary>> {
        bail!("graph::list_joined_teams_with_channels not yet implemented")
    }

    pub async fn list_team_channels(
        &self,
        _team_id: &str,
        _team_name: &str,
    ) -> Result<Vec<ChannelSummary>> {
        bail!("graph::list_team_channels not yet implemented")
    }

    pub async fn list_channel_members(
        &self,
        _team_id: &str,
        _channel_id: &str,
    ) -> Result<Vec<MemberInfo>> {
        bail!("graph::list_channel_members not yet implemented")
    }

    pub async fn list_channel_messages(
        &self,
        _team_id: &str,
        _channel_id: &str,
        _limit: usize,
    ) -> Result<Vec<MessageItem>> {
        bail!("graph::list_channel_messages not yet implemented")
    }

    /// Incremental channel sync using `/teams/{id}/channels/{id}/messages/delta`.
    /// Returns `(changed_messages, new_delta_link)`.
    pub async fn sync_channel_messages_delta(
        &self,
        _team_id: &str,
        _channel_id: &str,
        _delta_link: Option<&str>,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        bail!("graph::sync_channel_messages_delta not yet implemented")
    }

    pub async fn list_thread_replies(
        &self,
        _team_id: &str,
        _channel_id: &str,
        _root_msg_id: &str,
        _limit: usize,
    ) -> Result<Vec<MessageItem>> {
        bail!("graph::list_thread_replies not yet implemented")
    }

    pub async fn get_thread(
        &self,
        _team_id: &str,
        _channel_id: &str,
        _root_msg_id: &str,
        _limit: usize,
    ) -> Result<ThreadView> {
        bail!("graph::get_thread not yet implemented")
    }

    // --- Unified Messaging Operations (Chats + Channel Posts + Thread Replies) ---
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

    /// Sends a root message to a Chat or Team Channel.
    pub async fn send_message(
        &self,
        _target: &ConversationTarget,
        _outgoing: &OutgoingMessage,
    ) -> Result<MessageItem> {
        bail!("graph::send_message not yet implemented")
    }

    /// Replies to a message:
    /// - For a Team Channel, posts a native thread reply under `root_or_target_msg_id` (`/messages/{id}/replies`).
    /// - For a 1:1 / Group Chat, sends a message quoting `root_or_target_msg_id`.
    pub async fn reply_message(
        &self,
        _target: &ConversationTarget,
        _root_or_target_msg_id: &str,
        _outgoing: &OutgoingMessage,
    ) -> Result<MessageItem> {
        bail!("graph::reply_message not yet implemented")
    }

    pub async fn edit_message(
        &self,
        _target: &ConversationTarget,
        _msg_id: &str,
        _parent_msg_id: Option<&str>,
        _new_body_html: &str,
    ) -> Result<()> {
        bail!("graph::edit_message not yet implemented")
    }

    pub async fn delete_message(
        &self,
        _target: &ConversationTarget,
        _msg_id: &str,
        _parent_msg_id: Option<&str>,
    ) -> Result<()> {
        bail!("graph::delete_message not yet implemented")
    }

    pub async fn set_reaction(
        &self,
        _target: &ConversationTarget,
        _msg_id: &str,
        _parent_msg_id: Option<&str>,
        _reaction_type: &str,
        _enable: bool,
    ) -> Result<()> {
        bail!("graph::set_reaction not yet implemented")
    }

    // --- File Attachments (OneDrive / SharePoint + Reference Attachment + Download) ---
    /// Uploads a local file to the appropriate backing store:
    /// - `Chat` -> `/me/drive/root:/Microsoft Teams Chat Files/{name}:/content` (or upload session > 4 MiB) + sharing link
    /// - `Channel` -> `/teams/{id}/channels/{id}/filesFolder` -> `/drives/{drive_id}/items/{folder_id}:/{name}:/content`
    pub async fn upload_reference_file(
        &self,
        _target: &ConversationTarget,
        _local_path: &Path,
    ) -> Result<UploadedReferenceAttachment> {
        bail!("graph::upload_reference_file not yet implemented")
    }

    /// High-level helper: uploads `local_path` and sends it with an optional caption (and optional `thread_root_id` in channels).
    pub async fn send_file(
        &self,
        _target: &ConversationTarget,
        _local_path: &Path,
        _caption_html: Option<&str>,
        _thread_root_id: Option<&str>,
    ) -> Result<MessageItem> {
        bail!("graph::send_file not yet implemented")
    }

    /// Downloads a reference attachment (via Graph `/shares/u!.../driveItem/content` or direct stream)
    /// or hosted inline content into `dest_dir`.
    pub async fn download_attachment(
        &self,
        _attachment: &AttachmentInfo,
        _dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        bail!("graph::download_attachment not yet implemented")
    }

    pub async fn download_hosted_bytes(&self, _hosted_url: &str) -> Result<Vec<u8>> {
        bail!("graph::download_hosted_bytes not yet implemented")
    }

    // --- Batch & Presence ---
    /// Fetches multiple chat message lists in a single `POST /$batch` call (chunked by 20).
    pub async fn batch_fetch_chat_messages(
        &self,
        _chat_ids: &[String],
        _limit_per_chat: usize,
    ) -> Result<Vec<(String, Vec<MessageItem>)>> {
        bail!("graph::batch_fetch_chat_messages not yet implemented")
    }

    pub async fn get_my_presence(&self) -> Result<PresenceStatus> {
        bail!("graph::get_my_presence not yet implemented")
    }

    pub async fn get_presences_by_user_ids(
        &self,
        _user_ids: &[String],
    ) -> Result<Vec<PresenceStatus>> {
        bail!("graph::get_presences_by_user_ids not yet implemented")
    }

    pub async fn set_my_presence(
        &self,
        _availability: &str,
        _activity: &str,
        _expiration_mins: Option<u32>,
    ) -> Result<()> {
        bail!("graph::set_my_presence not yet implemented")
    }
}
