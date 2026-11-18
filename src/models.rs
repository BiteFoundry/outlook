//! Shared domain models and wire-independent contracts across Graph, Storage, Parser, Sync, TUI, CLI, HTTP, and MCP.
//!
//! IMPORTANT: Do NOT break existing field names or signatures in this file when working in a module,
//! as all sibling modules compile against these exact types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Sentinel summary emitted via `CoreEvent::SyncStateChanged { busy: false, summary }`
/// while background polling is silently retrying transient errors.
pub const POLL_RETRY_SUMMARY: &str = "__poll_retrying__";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub job_title: Option<String>,
    pub department: Option<String>,
}

// ---------------------------------------------------------------------------
// Mail Domain
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailRecipient {
    pub name: Option<String>,
    pub address: String,
}

impl EmailRecipient {
    pub fn display(&self) -> String {
        match self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(n) if !n.eq_ignore_ascii_case(&self.address) => {
                format!("{n} <{}>", self.address)
            }
            _ => self.address.clone(),
        }
    }

    pub fn short_display(&self) -> String {
        match self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(n) => n.to_string(),
            None => self.address.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailFolder {
    pub id: String,
    pub display_name: String,
    pub well_known_name: Option<String>,
    pub parent_folder_id: Option<String>,
    pub child_folder_count: i32,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingRequestInfo {
    pub meeting_message_type: Option<String>,
    pub event_id: Option<String>,
    pub start_iso: Option<String>,
    pub end_iso: Option<String>,
    pub location: Option<String>,
    pub online_meeting_url: Option<String>,
    pub response_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailMessage {
    pub id: String,
    pub internet_message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub folder_id: Option<String>,
    pub subject: String,
    pub from: Option<EmailRecipient>,
    pub to_recipients: Vec<EmailRecipient>,
    pub cc_recipients: Vec<EmailRecipient>,
    pub bcc_recipients: Vec<EmailRecipient>,
    pub reply_to: Vec<EmailRecipient>,
    pub received_at: String,
    pub sent_at: Option<String>,
    pub is_read: bool,
    pub is_draft: bool,
    pub flagged: bool,
    pub importance: String,
    pub inference_classification: Option<String>,
    pub has_attachments: bool,
    pub body_loaded: bool,
    pub preview: String,
    pub body_html: String,
    pub body_plain: String,
    pub attachments: Vec<MailAttachment>,
    pub meeting_request: Option<MeetingRequestInfo>,
    pub web_link: Option<String>,
}

impl MailMessage {
    pub fn from_display(&self) -> String {
        self.from
            .as_ref()
            .map(EmailRecipient::display)
            .unwrap_or_else(|| "(unknown sender)".to_string())
    }

    pub fn from_short(&self) -> String {
        self.from
            .as_ref()
            .map(EmailRecipient::short_display)
            .unwrap_or_else(|| "(unknown sender)".to_string())
    }

    pub fn effective_subject(&self) -> &str {
        let trimmed = self.subject.trim();
        if trimmed.is_empty() {
            "(No subject)"
        } else {
            &self.subject
        }
    }

    pub fn is_focused(&self) -> bool {
        self.inference_classification
            .as_deref()
            .map(|s| !s.eq_ignore_ascii_case("other"))
            .unwrap_or(true)
    }
}

/// Outgoing inline image (`fileAttachment` with `isInline = true` and `contentId`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImageUpload {
    pub temp_id: String,
    pub content_id: String,
    pub file_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub subject: String,
    pub body_html: String,
    pub body_markdown: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub importance: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub inline_images: Vec<InlineImageUpload>,
    pub in_reply_to_id: Option<String>,
    pub reply_all: bool,
    pub forward_of_id: Option<String>,
    pub draft_id: Option<String>,
    pub save_to_sent_items: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSearchHit {
    pub id: String,
    pub conversation_id: Option<String>,
    pub folder_id: Option<String>,
    pub subject: String,
    pub from_display: String,
    pub received_at: String,
    pub snippet: String,
    pub has_attachments: bool,
    pub is_read: bool,
    pub flagged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub file_name: String,
    pub bytes_written: u64,
}

// ---------------------------------------------------------------------------
// Calendar Domain
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAttendee {
    pub name: Option<String>,
    pub address: String,
    /// `required`, `optional`, `resource`.
    pub attendee_type: String,
    /// `none`, `organizer`, `tentativelyAccepted`, `accepted`, `declined`, `notResponded`.
    pub response_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventRsvpAction {
    Accept,
    TentativelyAccept,
    Decline,
}

impl EventRsvpAction {
    pub fn as_graph_endpoint(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::TentativelyAccept => "tentativelyAccept",
            Self::Decline => "decline",
        }
    }
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

// ---------------------------------------------------------------------------
// Contacts / People Domain
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tasks (Microsoft To Do) Domain
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Sync & Notification Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub scope_key: String,
    pub delta_link: Option<String>,
    pub history_next_link: Option<String>,
    pub last_synced_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationCandidate {
    pub item_id: String,
    pub folder_name: String,
    pub sender: String,
    pub subject: String,
    pub preview: String,
}

/// Asynchronous events pushed from `SyncEngine` to TUI or live subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum CoreEvent {
    ProfileLoaded(UserProfile),
    MailFoldersUpdated(Vec<MailFolder>),
    MailMessagesUpdated {
        folder_id: String,
        messages: Vec<MailMessage>,
        notify_candidate: Option<NotificationCandidate>,
    },
    MailMessageHydrated(Box<MailMessage>),
    CalendarEventsUpdated(Vec<CalendarEvent>),
    ContactsUpdated(Vec<ContactItem>),
    TodoListsUpdated(Vec<TodoList>),
    TodoTasksUpdated {
        list_id: String,
        tasks: Vec<TodoTask>,
    },
    SyncStateChanged {
        busy: bool,
        summary: String,
    },
    SyncError(String),
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
    Table {
        headers: Vec<Vec<RichSpan>>,
        rows: Vec<Vec<Vec<RichSpan>>>,
    },
    Divider,
    AttachmentChip {
        id: String,
        name: Option<String>,
    },
    InlineImageChip {
        src: String,
        content_id: Option<String>,
        index: usize,
    },
    QuotedHistory {
        line_count: usize,
        blocks: Vec<RichBlock>,
    },
}

// ---------------------------------------------------------------------------
// Self-Updater Model
// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mail(
        from: Option<EmailRecipient>,
        subject: &str,
        inference: Option<&str>,
    ) -> MailMessage {
        MailMessage {
            id: "msg-1".to_string(),
            internet_message_id: Some("<msg-1@example.com>".to_string()),
            conversation_id: Some("conv-1".to_string()),
            folder_id: Some("inbox".to_string()),
            subject: subject.to_string(),
            from,
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: "2026-11-18T00:00:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: inference.map(str::to_string),
            has_attachments: false,
            body_loaded: true,
            preview: "Hello".to_string(),
            body_html: "<p>Hello</p>".to_string(),
            body_plain: "Hello".to_string(),
            attachments: Vec::new(),
            meeting_request: None,
            web_link: None,
        }
    }

    #[test]
    fn test_email_recipient_display_and_short_display() {
        let named = EmailRecipient {
            name: Some("Alice Smith".to_string()),
            address: "alice@example.com".to_string(),
        };
        assert_eq!(named.display(), "Alice Smith <alice@example.com>");
        assert_eq!(named.short_display(), "Alice Smith");

        let same = EmailRecipient {
            name: Some("alice@example.com".to_string()),
            address: "alice@example.com".to_string(),
        };
        assert_eq!(same.display(), "alice@example.com");
        assert_eq!(same.short_display(), "alice@example.com");

        let blank_name = EmailRecipient {
            name: Some("   ".to_string()),
            address: "bob@example.com".to_string(),
        };
        assert_eq!(blank_name.display(), "bob@example.com");
        assert_eq!(blank_name.short_display(), "bob@example.com");
    }

    #[test]
    fn test_mail_message_helpers_and_rsvp_endpoint() {
        let msg = sample_mail(
            Some(EmailRecipient {
                name: Some("Alice".to_string()),
                address: "alice@example.com".to_string(),
            }),
            "Quarterly Review",
            Some("focused"),
        );
        assert_eq!(msg.from_display(), "Alice <alice@example.com>");
        assert_eq!(msg.from_short(), "Alice");
        assert_eq!(msg.effective_subject(), "Quarterly Review");
        assert!(msg.is_focused());

        let unknown = sample_mail(None, "   ", Some("Other"));
        assert_eq!(unknown.from_display(), "(unknown sender)");
        assert_eq!(unknown.from_short(), "(unknown sender)");
        assert_eq!(unknown.effective_subject(), "(No subject)");
        assert!(!unknown.is_focused());

        let default_focused = sample_mail(None, "Hi", None);
        assert!(default_focused.is_focused());

        assert_eq!(EventRsvpAction::Accept.as_graph_endpoint(), "accept");
        assert_eq!(
            EventRsvpAction::TentativelyAccept.as_graph_endpoint(),
            "tentativelyAccept"
        );
        assert_eq!(EventRsvpAction::Decline.as_graph_endpoint(), "decline");
    }

    #[test]
    fn test_core_event_tagged_serialization() {
        let evt = CoreEvent::SyncStateChanged {
            busy: false,
            summary: POLL_RETRY_SUMMARY.to_string(),
        };
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["event"], "sync_state_changed");
        assert_eq!(json["data"]["busy"], false);
        assert_eq!(json["data"]["summary"], POLL_RETRY_SUMMARY);
    }
}
