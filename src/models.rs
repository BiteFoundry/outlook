//! Shared domain models and wire-independent contracts across Graph, Storage, Parser, Sync, TUI, CLI, HTTP, and MCP.
//!
//! IMPORTANT: Do NOT break existing field names or signatures in this file when working in a module,
//! as all sibling modules compile against these exact types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Kind of Teams conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationKind {
    OneOnOne,
    Group,
    Meeting,
    Channel,
}

impl ConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneOnOne => "oneOnOne",
            Self::Group => "group",
            Self::Meeting => "meeting",
            Self::Channel => "channel",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "oneOnOne" | "one_on_one" => Self::OneOnOne,
            "group" => Self::Group,
            "meeting" => Self::Meeting,
            "channel" => Self::Channel,
            _ => Self::Group,
        }
    }

    pub fn is_channel(self) -> bool {
        matches!(self, Self::Channel)
    }
}

/// Strongly typed address for either a 1:1/group/meeting Chat or a Team Channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "target_type", rename_all = "snake_case")]
pub enum ConversationTarget {
    Chat {
        chat_id: String,
    },
    Channel {
        team_id: String,
        channel_id: String,
    },
}

impl ConversationTarget {
    /// Canonical primary key stored in SQLite and used in URLs (`chat:<chat_id>` or `chan:<team_id>:<channel_id>`).
    pub fn conv_key(&self) -> String {
        match self {
            Self::Chat { chat_id } => format!("chat:{chat_id}"),
            Self::Channel {
                team_id,
                channel_id,
            } => format!("chan:{team_id}:{channel_id}"),
        }
    }

    pub fn from_conv_key(key: &str) -> Option<Self> {
        if let Some(chat_id) = key.strip_prefix("chat:") {
            if !chat_id.is_empty() {
                return Some(Self::Chat {
                    chat_id: chat_id.to_string(),
                });
            }
        }
        if let Some(rest) = key.strip_prefix("chan:") {
            let (team_id, channel_id) = rest.split_once(':')?;
            if !team_id.is_empty() && !channel_id.is_empty() {
                return Some(Self::Channel {
                    team_id: team_id.to_string(),
                    channel_id: channel_id.to_string(),
                });
            }
        }
        None
    }

    pub fn is_channel(&self) -> bool {
        matches!(self, Self::Channel { .. })
    }
}

/// Summary row for a chat or channel in navigation lists, CLI `chats`/`channels`, HTTP, and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub conv_key: String,
    pub target: ConversationTarget,
    pub kind: ConversationKind,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub title: String,
    pub topic: Option<String>,
    pub unread_count: i32,
    pub last_read_at: Option<String>,
    pub last_msg_id: Option<String>,
    pub last_msg_author: Option<String>,
    pub last_msg_preview: Option<String>,
    pub last_updated_at: Option<String>,
    pub pinned: bool,
    /// For 1:1 chats, the other participant's Entra user ID (used for live presence dots).
    pub peer_user_id: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub id: String,
    pub team_id: String,
    pub team_name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub membership_type: String,
    pub web_url: Option<String>,
}

impl ChannelSummary {
    pub fn target(&self) -> ConversationTarget {
        ConversationTarget::Channel {
            team_id: self.team_id.clone(),
            channel_id: self.id.clone(),
        }
    }

    pub fn conv_key(&self) -> String {
        self.target().conv_key()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSummary {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub channels: Vec<ChannelSummary>,
}

/// File, reference, inline hosted image, or Adaptive Card attached to a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub id: String,
    pub content_type: String,
    pub content_url: Option<String>,
    pub name: Option<String>,
    /// JSON payload when `content_type` is `application/vnd.microsoft.card.adaptive` or similar.
    pub content_json: Option<String>,
    pub thumbnail_url: Option<String>,
}

/// `@mention` attached to a Teams message (`<at id="{id}">{display_name}</at>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionInfo {
    pub id: i32,
    pub user_id: String,
    pub display_name: String,
}

/// Aggregated reaction badge on a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionSummary {
    /// Graph reactionType (`like`, `angry`, `sad`, `laugh`, `heart`, `surprised`, or unicode emoji).
    pub reaction_type: String,
    pub count: usize,
    pub me_reacted: bool,
    pub users: Vec<String>,
}

/// Normalized message item across 1:1 chats, group chats, channel root posts, and channel thread replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageItem {
    /// Primary key `"{conv_key}|{msg_id}"`.
    pub msg_key: String,
    pub conv_key: String,
    pub msg_id: String,
    /// `Some(root_msg_id)` when this message is a reply inside a Team Channel thread; `None` for root posts & chats.
    pub parent_msg_id: Option<String>,
    /// Quoted message ID for 1:1 / group chat replies (`messageReference` / `blockquote`).
    pub reply_to_id: Option<String>,
    pub reply_to_author: Option<String>,
    pub reply_to_preview: Option<String>,
    /// Optional subject line on Team Channel root posts.
    pub subject: Option<String>,
    pub author_id: Option<String>,
    pub author_name: String,
    pub is_me: bool,
    pub created_at: String,
    pub last_modified_at: Option<String>,
    pub deleted: bool,
    pub importance: String,
    pub message_type: String,
    pub body_html: String,
    pub body_plain: String,
    pub attachments: Vec<AttachmentInfo>,
    pub mentions: Vec<MentionInfo>,
    pub reactions: Vec<ReactionSummary>,
    /// Number of thread replies under a channel root post.
    pub reply_count: usize,
    pub web_url: Option<String>,
}

impl MessageItem {
    pub fn make_msg_key(conv_key: &str, msg_id: &str) -> String {
        format!("{conv_key}|{msg_id}")
    }
}

/// Thread bundle: root channel post + ordered replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadView {
    pub conv_key: String,
    pub root: MessageItem,
    pub replies: Vec<MessageItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceStatus {
    pub user_id: String,
    /// `Available`, `Busy`, `DoNotDisturb`, `Away`, `BeRightBack`, `Offline`, `PresenceUnknown`.
    pub availability: String,
    pub activity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub display_name: String,
    pub mail: Option<String>,
    pub user_principal_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub user_id: String,
    pub member_id: Option<String>,
    pub display_name: String,
    pub email: Option<String>,
    pub roles: Vec<String>,
    pub presence: Option<PresenceStatus>,
}

/// Outgoing reference attachment after uploading to OneDrive / SharePoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedReferenceAttachment {
    /// GUID extracted from DriveItem `eTag` (or generated UUID fallback if absent).
    pub id: String,
    pub name: String,
    /// `webDavUrl` or sharing `webUrl` for Graph `reference` attachment.
    pub content_url: String,
    pub size: u64,
    pub drive_item_id: Option<String>,
}

/// Outgoing inline image (`chatMessageHostedContent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImageUpload {
    /// Temporary integer/string identifier referenced as `../hostedContents/{temp_id}/$value`.
    pub temp_id: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
}

/// Compiled outgoing payload ready to POST/PATCH to Microsoft Graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub subject: Option<String>,
    pub body_html: String,
    pub mentions: Vec<MentionInfo>,
    pub attachments: Vec<UploadedReferenceAttachment>,
    pub inline_images: Vec<InlineImageUpload>,
    pub importance: Option<String>,
    /// Optional quoted message for 1:1/group chat replies.
    pub quote_reply_to: Option<QuotedReply>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotedReply {
    pub msg_id: String,
    pub author_name: String,
    pub preview_plain: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub file_name: String,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub conv_key: String,
    pub conv_title: String,
    pub kind: ConversationKind,
    pub msg_id: String,
    pub parent_msg_id: Option<String>,
    pub author_name: String,
    pub created_at: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub scope_key: String,
    pub delta_link: Option<String>,
    pub oldest_msg_ts: Option<String>,
    pub synced_at: String,
}

/// Asynchronous events pushed from `SyncEngine` to TUI or live subscribers.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    ConversationsUpdated(Vec<ConversationSummary>),
    TeamsUpdated(Vec<TeamSummary>),
    MessagesUpdated {
        conv_key: String,
        parent_msg_id: Option<String>,
        messages: Vec<MessageItem>,
        notify_candidate: Option<NotificationCandidate>,
    },
    PresenceUpdated(Vec<PresenceStatus>),
    MailFoldersUpdated(Vec<MailFolder>),
    MailMessagesUpdated {
        folder_id: String,
        messages: Vec<MailMessage>,
        notify_candidate: Option<NotificationCandidate>,
    },
    CalendarEventsUpdated(Vec<CalendarEvent>),
    ContactsUpdated(Vec<ContactItem>),
    TodoUpdated {
        lists: Vec<TodoList>,
        list_id: Option<String>,
        tasks: Vec<TodoTask>,
    },
    OnenoteHierarchyUpdated {
        notebooks: Vec<OnenoteNotebook>,
        section_groups: Vec<OnenoteSectionGroup>,
        sections: Vec<OnenoteSection>,
    },
    OnenotePagesUpdated {
        section_key: String,
        pages: Vec<OnenotePageSummary>,
    },
    OnenotePageDetailUpdated(Box<OnenotePageDetail>),
    SyncStateChanged {
        busy: bool,
        summary: String,
    },
    SyncError(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationCandidate {
    pub conv_key: String,
    pub conv_title: String,
    pub sender: String,
    pub preview: String,
}

// ---------------------------------------------------------------------------
// Parser Rich AST (shared between `parser` and `ui`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStyleFlags {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub mention: bool,
    pub link_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichSpan {
    pub text: String,
    pub style: TextStyleFlags,
}

impl RichSpan {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: TextStyleFlags::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichBlock {
    Paragraph(Vec<RichSpan>),
    Heading {
        level: u8,
        spans: Vec<RichSpan>,
    },
    Quote(Vec<RichSpan>),
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    BulletList(Vec<Vec<RichSpan>>),
    Card {
        title: Option<String>,
        subtitle: Option<String>,
        facts: Vec<(String, String)>,
        body_lines: Vec<String>,
        actions: Vec<(String, String)>,
    },
    AttachmentChip {
        id: String,
        name: Option<String>,
    },
    InlineImageChip {
        src: String,
        index: usize,
    },
}

// ---------------------------------------------------------------------------
// Outlook Mail, Calendar, Contacts, Microsoft To Do, and Self-Updater Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailRecipient {
    pub name: Option<String>,
    pub address: String,
}

impl EmailRecipient {
    pub fn display(&self) -> String {
        match self.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(n) if !n.eq_ignore_ascii_case(&self.address) => {
                format!("{n} <{}>", self.address)
            }
            _ => self.address.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFolder {
    pub id: String,
    pub display_name: String,
    pub well_known_name: Option<String>,
    pub parent_folder_id: Option<String>,
    pub unread_item_count: i32,
    pub total_item_count: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAttachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub is_inline: bool,
    pub content_id: Option<String>,
    /// Optional inline base64 bytes when expanded from Graph (`fileAttachment`).
    pub content_bytes_b64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMessage {
    pub id: String,
    pub conversation_id: Option<String>,
    pub folder_id: Option<String>,
    pub subject: String,
    pub from: Option<EmailRecipient>,
    pub to_recipients: Vec<EmailRecipient>,
    pub cc_recipients: Vec<EmailRecipient>,
    pub bcc_recipients: Vec<EmailRecipient>,
    pub received_at: String,
    pub sent_at: Option<String>,
    pub is_read: bool,
    pub is_draft: bool,
    pub flagged: bool,
    pub importance: String,
    pub has_attachments: bool,
    pub preview: String,
    pub body_html: String,
    pub body_plain: String,
    pub attachments: Vec<MailAttachment>,
    pub web_link: Option<String>,
}

impl MailMessage {
    pub fn from_display(&self) -> String {
        self.from
            .as_ref()
            .map(EmailRecipient::display)
            .unwrap_or_else(|| "(발신자 없음)".to_string())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub subject: String,
    pub body_html: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub importance: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub save_to_sent_items: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSearchHit {
    pub id: String,
    pub folder_id: Option<String>,
    pub subject: String,
    pub from_display: String,
    pub received_at: String,
    pub snippet: String,
    pub has_attachments: bool,
    pub is_read: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAttendee {
    pub name: Option<String>,
    pub address: String,
    /// `required`, `optional`, `resource`.
    pub attendee_type: String,
    /// `none`, `organizer`, `tentativelyAccepted`, `accepted`, `declined`, `notResponded`.
    pub response_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub subject: String,
    pub start_iso: String,
    pub end_iso: String,
    pub time_zone: String,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub attendees: Vec<EventAttendee>,
    /// Current user's response (`accepted`, `tentativelyAccepted`, `declined`, `notResponded`, `organizer`, `none`).
    pub response_status: String,
    pub is_online_meeting: bool,
    pub online_meeting_url: Option<String>,
    pub body_html: String,
    pub body_plain: String,
    pub is_cancelled: bool,
    pub web_link: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingEvent {
    pub subject: String,
    pub start_iso: String,
    pub end_iso: String,
    pub time_zone: Option<String>,
    pub is_all_day: bool,
    pub location: Option<String>,
    pub body_html: Option<String>,
    pub attendees: Vec<String>,
    pub is_online_meeting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactItem {
    pub id: String,
    pub display_name: String,
    pub given_name: Option<String>,
    pub surname: Option<String>,
    pub email_addresses: Vec<String>,
    pub mobile_phone: Option<String>,
    pub business_phones: Vec<String>,
    pub company_name: Option<String>,
    pub job_title: Option<String>,
    pub department: Option<String>,
    pub notes: Option<String>,
    /// `"contact"` (personal Outlook contact) or `"directory"` (Entra org directory user).
    pub source: String,
}

impl ContactItem {
    pub fn primary_email(&self) -> Option<&str> {
        self.email_addresses.first().map(String::as_str)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingContact {
    pub display_name: String,
    pub given_name: Option<String>,
    pub surname: Option<String>,
    pub email_addresses: Vec<String>,
    pub mobile_phone: Option<String>,
    pub company_name: Option<String>,
    pub job_title: Option<String>,
    pub department: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub id: String,
    pub display_name: String,
    pub wellknown_list_name: Option<String>,
    pub is_owner: bool,
    pub is_shared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoTask {
    pub id: String,
    pub list_id: String,
    pub title: String,
    /// `notStarted`, `inProgress`, `completed`, `waitingOnOthers`, `deferred`.
    pub status: String,
    /// `low`, `normal`, `high`.
    pub importance: String,
    pub body_plain: Option<String>,
    pub due_date_iso: Option<String>,
    pub reminder_iso: Option<String>,
    pub completed_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl TodoTask {
    pub fn is_completed(&self) -> bool {
        self.status.eq_ignore_ascii_case("completed")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingTodoTask {
    pub title: String,
    pub body_plain: Option<String>,
    pub due_date_iso: Option<String>,
    pub importance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Microsoft OneNote Domain & XHTML/PATCH Models (Personal + Group/Site Shared)
// ---------------------------------------------------------------------------

/// Identifies which OneNote container owns a notebook/section/page (`/me`, `/groups/{id}`, or `/sites/{id}`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(tag = "scope_type", rename_all = "snake_case")]
pub enum OnenoteScope {
    #[default]
    Me,
    Group {
        group_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_name: Option<String>,
    },
    Site {
        site_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site_name: Option<String>,
    },
}

impl OnenoteScope {
    /// Canonical stable key prefix (`"me"`, `"group:{id}"`, `"site:{id}"`).
    pub fn scope_key(&self) -> String {
        match self {
            Self::Me => "me".to_string(),
            Self::Group { group_id, .. } => format!("group:{group_id}"),
            Self::Site { site_id, .. } => format!("site:{site_id}"),
        }
    }

    pub fn from_scope_key(key: &str, name_hint: Option<String>) -> Option<Self> {
        let trimmed = key.trim();
        if trimmed.eq_ignore_ascii_case("me") || trimmed.is_empty() {
            return Some(Self::Me);
        }
        if let Some(gid) = trimmed.strip_prefix("group:") {
            if !gid.is_empty() {
                return Some(Self::Group {
                    group_id: gid.to_string(),
                    group_name: name_hint,
                });
            }
        }
        if let Some(sid) = trimmed.strip_prefix("site:") {
            if !sid.is_empty() {
                return Some(Self::Site {
                    site_id: sid.to_string(),
                    site_name: name_hint,
                });
            }
        }
        None
    }

    /// Microsoft Graph v1.0 relative base path (`"/me/onenote"`, `"/groups/{id}/onenote"`, `"/sites/{id}/onenote"`).
    pub fn graph_base_path(&self) -> String {
        match self {
            Self::Me => "/me/onenote".to_string(),
            Self::Group { group_id, .. } => format!("/groups/{group_id}/onenote"),
            Self::Site { site_id, .. } => format!("/sites/{site_id}/onenote"),
        }
    }

    pub fn owner_label(&self) -> String {
        match self {
            Self::Me => "내 필기장".to_string(),
            Self::Group {
                group_id,
                group_name,
            } => group_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(group_id)
                .to_string(),
            Self::Site { site_id, site_name } => site_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(site_id)
                .to_string(),
        }
    }

    pub fn is_personal(&self) -> bool {
        matches!(self, Self::Me)
    }

    pub fn make_entity_key(&self, entity_id: &str) -> String {
        format!("{}|{}", self.scope_key(), entity_id)
    }
}

pub fn split_onenote_entity_key(composite_or_raw: &str) -> (OnenoteScope, String) {
    if let Some((scope_part, raw_id)) = composite_or_raw.split_once('|') {
        if let Some(scope) = OnenoteScope::from_scope_key(scope_part, None) {
            if !raw_id.is_empty() {
                return (scope, raw_id.to_string());
            }
        }
    }
    (OnenoteScope::Me, composite_or_raw.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteNotebook {
    /// Composite key `"{scope_key}|{id}"`.
    pub notebook_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub display_name: String,
    pub color: Option<String>,
    pub is_default: bool,
    pub is_shared: bool,
    pub user_role: Option<String>,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub created_by: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteSectionGroup {
    /// Composite key `"{scope_key}|{id}"`.
    pub section_group_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub notebook_id: Option<String>,
    pub parent_section_group_id: Option<String>,
    pub display_name: String,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteSection {
    /// Composite key `"{scope_key}|{id}"`.
    pub section_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub notebook_id: Option<String>,
    pub notebook_name: Option<String>,
    pub parent_section_group_id: Option<String>,
    pub parent_section_group_name: Option<String>,
    pub display_name: String,
    pub color: Option<String>,
    pub is_default: bool,
    pub pages_url: Option<String>,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenotePageSummary {
    /// Composite key `"{scope_key}|{id}"`.
    pub page_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub section_id: String,
    pub section_name: Option<String>,
    pub notebook_id: Option<String>,
    pub notebook_name: Option<String>,
    pub title: String,
    /// Indentation level in OneNote page list (`0` = root page, `1` = subpage, `2` = sub-subpage).
    pub level: i32,
    pub order: i32,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub content_url: Option<String>,
    pub web_url: Option<String>,
    pub preview_text: Option<String>,
    pub has_todo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnenoteTagKind {
    Todo { completed: bool },
    Important,
    Question,
    Critical,
    Highlight,
    Idea,
    Custom(String),
}

impl OnenoteTagKind {
    pub fn is_todo(&self) -> bool {
        matches!(self, Self::Todo { .. })
    }

    pub fn data_tag_value(&self) -> String {
        match self {
            Self::Todo { completed: false } => "to-do".to_string(),
            Self::Todo { completed: true } => "to-do:completed".to_string(),
            Self::Important => "important".to_string(),
            Self::Question => "question".to_string(),
            Self::Critical => "critical".to_string(),
            Self::Highlight => "highlight".to_string(),
            Self::Idea => "idea".to_string(),
            Self::Custom(raw) => raw.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteResource {
    pub element_id: Option<String>,
    pub resource_url: String,
    pub fullres_url: Option<String>,
    pub mime_type: String,
    pub file_name: Option<String>,
    pub is_image: bool,
    pub alt_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteBlock {
    /// System-generated Graph element ID (`p:{guid}{40}`, `div:{guid}{32}`, `table:{...}`, etc.) from `?includeIDs=true`.
    pub element_id: Option<String>,
    /// Optional OneNote `data-tag` attached to this element.
    pub tag: Option<OnenoteTagKind>,
    /// Original outer HTML snippet of this element so surgical tag toggling preserves inline styles/links.
    pub raw_html: Option<String>,
    pub kind: OnenoteBlockKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "block_type", rename_all = "snake_case")]
pub enum OnenoteBlockKind {
    Heading {
        level: u8,
        spans: Vec<RichSpan>,
    },
    Paragraph {
        spans: Vec<RichSpan>,
    },
    ListItem {
        ordered: bool,
        index: Option<usize>,
        depth: u8,
        spans: Vec<RichSpan>,
    },
    Quote {
        spans: Vec<RichSpan>,
    },
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    Table {
        headers: Vec<Vec<RichSpan>>,
        rows: Vec<Vec<Vec<RichSpan>>>,
    },
    Image {
        resource_index: usize,
        src: String,
        alt: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    },
    Attachment {
        resource_index: usize,
        name: String,
        src: String,
        mime_type: String,
    },
    Divider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenotePageDetail {
    pub summary: OnenotePageSummary,
    pub raw_xhtml: String,
    pub body_markdown: String,
    pub body_plain: String,
    pub blocks: Vec<OnenoteBlock>,
    pub resources: Vec<OnenoteResource>,
    /// Primary `#div:{...}` content container ID on the page (if present) for clean `PATCH` append/replace.
    pub primary_div_id: Option<String>,
    pub synced_at: Option<String>,
}

/// Wire payload item for `PATCH /{scope}/pages/{id}/content` (`application/json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenotePatchCommand {
    pub target: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteSearchHit {
    pub page_key: String,
    pub page_id: String,
    pub scope: OnenoteScope,
    pub notebook_name: Option<String>,
    pub section_id: String,
    pub section_name: Option<String>,
    pub title: String,
    pub last_modified_at: Option<String>,
    pub snippet: String,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingOnenotePage {
    pub title: String,
    pub markdown_body: String,
    pub inline_images: Vec<InlineImageUpload>,
    pub level: Option<i32>,
}
