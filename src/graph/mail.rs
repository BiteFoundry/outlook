//! Microsoft Graph v1.0 Outlook Mail operations (`/me/mailFolders`, `/me/messages`, `/me/sendMail`,
//! drafts, inline `cid:` images, attachments, remote search, and delta sync).

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use scraper::{Html, Node};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use super::{
    client::{BatchSubRequest, GraphDeltaPage, GraphListPage},
    media::{
        deduplicate_dest_path, extension_for_content_type, guess_mime_type,
        is_auth_or_forbidden_error, maybe_refine_image_extension, sanitize_filename,
        MAX_IN_MEMORY_ATTACHMENT_BYTES,
    },
    GraphClient,
};
use crate::models::{
    DownloadedFile, EmailRecipient, InlineImageUpload, MailAttachment, MailFolder, MailMessage,
    MeetingRequestInfo, OutgoingMail,
};

const MAIL_SMALL_ATTACHMENT_LIMIT: u64 = 3 * 1024 * 1024; // 3 MiB
const MAIL_UPLOAD_CHUNK_SIZE: usize = 3_276_800; // 3.2 MiB

const MAIL_HEADER_SELECT_FIELDS: &str = "id,internetMessageId,conversationId,parentFolderId,subject,from,toRecipients,ccRecipients,bccRecipients,replyTo,receivedDateTime,sentDateTime,isRead,isDraft,flag,importance,inferenceClassification,hasAttachments,bodyPreview,webLink";
const MAIL_FULL_SELECT_FIELDS: &str = "id,internetMessageId,conversationId,parentFolderId,subject,from,toRecipients,ccRecipients,bccRecipients,replyTo,receivedDateTime,sentDateTime,isRead,isDraft,flag,importance,inferenceClassification,hasAttachments,bodyPreview,body,webLink";

// ---------------------------------------------------------------------------
// Shared Graph Outlook Wire DTOs & Normalization Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphEmailAddress {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphRecipient {
    #[serde(rename = "emailAddress", default)]
    pub email_address: Option<GraphEmailAddress>,
}

impl GraphRecipient {
    pub fn into_email_recipient(self) -> Option<EmailRecipient> {
        let ea = self.email_address?;
        let address = ea.address?.trim().to_string();
        if address.is_empty() {
            return None;
        }
        let name = ea.name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Some(EmailRecipient { name, address })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphOutlookBody {
    #[serde(rename = "contentType", default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphDateTimeZone {
    #[serde(rename = "dateTime", default)]
    pub date_time: Option<String>,
    #[serde(rename = "timeZone", default)]
    pub _time_zone: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphLocationInfo {
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphEventResponseStatus {
    #[serde(default)]
    response: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnlineMeetingRef {
    #[serde(rename = "joinUrl", default)]
    join_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphEmbeddedEvent {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    start: Option<GraphDateTimeZone>,
    #[serde(default)]
    end: Option<GraphDateTimeZone>,
    #[serde(default)]
    location: Option<GraphLocationInfo>,
    #[serde(rename = "onlineMeetingUrl", default)]
    online_meeting_url: Option<String>,
    #[serde(rename = "onlineMeeting", default)]
    online_meeting: Option<GraphOnlineMeetingRef>,
    #[serde(rename = "responseStatus", default)]
    response_status: Option<GraphEventResponseStatus>,
}

pub(crate) fn extract_teams_meetup_url(text: &str) -> Option<String> {
    const PREFIX: &str = "https://teams.microsoft.com/l/meetup-join/";
    let start = text.find(PREFIX)?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
        .unwrap_or(rest.len());
    let candidate = &rest[..end];
    if candidate.len() <= PREFIX.len() {
        return None;
    }
    Some(candidate.replace("&amp;", "&"))
}

pub(crate) fn escape_html_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn html_to_plain_text(html: &str) -> String {
    enum Step<N> {
        Visit(N),
        CloseBlock,
    }

    let frag = Html::parse_fragment(html);
    let mut out = String::new();
    let mut stack = Vec::new();
    let root = frag.root_element();
    for child in root.children().collect::<Vec<_>>().into_iter().rev() {
        stack.push(Step::Visit(child));
    }

    while let Some(step) = stack.pop() {
        match step {
            Step::CloseBlock => {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Step::Visit(node) => match node.value() {
                Node::Text(t) => out.push_str(&t.text),
                Node::Element(el) => {
                    let tag = el.name().to_ascii_lowercase();
                    if matches!(tag.as_str(), "style" | "script" | "head" | "title") {
                        continue;
                    }
                    if tag == "br" {
                        out.push('\n');
                        continue;
                    }
                    let is_block = matches!(
                        tag.as_str(),
                        "p" | "div"
                            | "li"
                            | "tr"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "blockquote"
                            | "pre"
                            | "hr"
                            | "section"
                            | "article"
                    );
                    if is_block && !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    if is_block {
                        stack.push(Step::CloseBlock);
                    }
                    for child in node.children().collect::<Vec<_>>().into_iter().rev() {
                        stack.push(Step::Visit(child));
                    }
                }
                _ => {
                    for child in node.children().collect::<Vec<_>>().into_iter().rev() {
                        stack.push(Step::Visit(child));
                    }
                }
            },
        }
    }

    // Normalize excessive blank lines while keeping paragraph separation
    let mut lines: Vec<&str> = Vec::new();
    let mut prev_blank = true;
    for raw_line in out.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            if !prev_blank {
                lines.push("");
                prev_blank = true;
            }
        } else {
            lines.push(trimmed);
            prev_blank = false;
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

pub(crate) fn normalize_outlook_body(
    body: Option<&GraphOutlookBody>,
    preview: &str,
) -> (String, String) {
    let raw_content = body
        .and_then(|b| b.content.as_deref())
        .unwrap_or("");
    let content_type = body
        .and_then(|b| b.content_type.as_deref())
        .unwrap_or("html");

    if raw_content.trim().is_empty() {
        return (String::new(), preview.to_string());
    }

    if content_type.eq_ignore_ascii_case("text") {
        let escaped = escape_html_text(raw_content);
        return (escaped, raw_content.to_string());
    }

    let body_html = raw_content.to_string();
    let plain = html_to_plain_text(&body_html);
    let body_plain = if plain.trim().is_empty() {
        preview.to_string()
    } else {
        plain
    };
    (body_html, body_plain)
}

fn clean_content_id(raw: Option<String>) -> Option<String> {
    raw.map(|s| {
        s.trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim()
            .to_string()
    })
    .filter(|s| !s.is_empty())
}

#[derive(Debug, Deserialize)]
struct GraphMailFolder {
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "wellKnownName", default)]
    well_known_name: Option<String>,
    #[serde(rename = "parentFolderId", default)]
    parent_folder_id: Option<String>,
    #[serde(rename = "childFolderCount", default)]
    child_folder_count: i32,
    #[serde(rename = "unreadItemCount", default)]
    unread_item_count: i32,
    #[serde(rename = "totalItemCount", default)]
    total_item_count: i32,
}

#[derive(Debug, Default, Deserialize)]
struct GraphMailFlag {
    #[serde(rename = "flagStatus", default)]
    flag_status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphMailAttachment {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "contentType", default)]
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(rename = "isInline", default)]
    is_inline: bool,
    #[serde(rename = "contentId", default)]
    content_id: Option<String>,
    #[serde(rename = "contentBytes", default)]
    content_bytes: Option<String>,
}

impl From<GraphMailAttachment> for MailAttachment {
    fn from(att: GraphMailAttachment) -> Self {
        Self {
            name: att
                .name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("attachment_{}", att.id)),
            id: att.id,
            content_type: att
                .content_type
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            size: att.size,
            is_inline: att.is_inline,
            content_id: clean_content_id(att.content_id),
            content_bytes_b64: att.content_bytes.filter(|s| !s.trim().is_empty()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphMailMessage {
    #[serde(rename = "@odata.type", default)]
    odata_type: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(rename = "internetMessageId", default)]
    internet_message_id: Option<String>,
    #[serde(rename = "conversationId", default)]
    conversation_id: Option<String>,
    #[serde(rename = "parentFolderId", default)]
    parent_folder_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<GraphRecipient>,
    #[serde(rename = "toRecipients", default)]
    to_recipients: Vec<GraphRecipient>,
    #[serde(rename = "ccRecipients", default)]
    cc_recipients: Vec<GraphRecipient>,
    #[serde(rename = "bccRecipients", default)]
    bcc_recipients: Vec<GraphRecipient>,
    #[serde(rename = "replyTo", default)]
    reply_to: Vec<GraphRecipient>,
    #[serde(rename = "receivedDateTime", default)]
    received_date_time: Option<String>,
    #[serde(rename = "sentDateTime", default)]
    sent_date_time: Option<String>,
    #[serde(rename = "isRead", default)]
    is_read: Option<bool>,
    #[serde(rename = "isDraft", default)]
    is_draft: Option<bool>,
    #[serde(default)]
    flag: Option<GraphMailFlag>,
    #[serde(default)]
    importance: Option<String>,
    #[serde(rename = "inferenceClassification", default)]
    inference_classification: Option<String>,
    #[serde(rename = "hasAttachments", default)]
    has_attachments: Option<bool>,
    #[serde(rename = "bodyPreview", default)]
    body_preview: Option<String>,
    #[serde(default)]
    body: Option<GraphOutlookBody>,
    #[serde(default)]
    attachments: Vec<GraphMailAttachment>,
    #[serde(rename = "webLink", default)]
    web_link: Option<String>,
    #[serde(rename = "meetingMessageType", default)]
    meeting_message_type: Option<String>,
    #[serde(rename = "startDateTime", default)]
    start_date_time: Option<GraphDateTimeZone>,
    #[serde(rename = "endDateTime", default)]
    end_date_time: Option<GraphDateTimeZone>,
    #[serde(default)]
    location: Option<GraphLocationInfo>,
    #[serde(rename = "onlineMeetingUrl", default)]
    online_meeting_url: Option<String>,
    #[serde(rename = "responseType", default)]
    response_type: Option<String>,
    #[serde(default)]
    event: Option<GraphEmbeddedEvent>,
}

impl GraphMailMessage {
    fn into_mail_message(self, body_loaded: bool) -> MailMessage {
        let preview = self.body_preview.unwrap_or_default();
        let (body_html, body_plain) = normalize_outlook_body(self.body.as_ref(), &preview);
        let flagged = self
            .flag
            .and_then(|f| f.flag_status)
            .map(|s| s.eq_ignore_ascii_case("flagged"))
            .unwrap_or(false);

        let is_event_message = self
            .odata_type
            .as_deref()
            .map(|t| t.to_ascii_lowercase().contains("eventmessage"))
            .unwrap_or(false)
            || self
                .meeting_message_type
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);

        let meeting_request = if is_event_message {
            let ev = self.event.as_ref();
            let start_iso = self
                .start_date_time
                .as_ref()
                .and_then(|d| d.date_time.clone())
                .or_else(|| ev.and_then(|e| e.start.as_ref()?.date_time.clone()))
                .filter(|s| !s.trim().is_empty());
            let end_iso = self
                .end_date_time
                .as_ref()
                .and_then(|d| d.date_time.clone())
                .or_else(|| ev.and_then(|e| e.end.as_ref()?.date_time.clone()))
                .filter(|s| !s.trim().is_empty());
            let location = self
                .location
                .as_ref()
                .and_then(|l| l.display_name.clone())
                .or_else(|| ev.and_then(|e| e.location.as_ref()?.display_name.clone()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let online_meeting_url = ev
                .and_then(|e| e.online_meeting.as_ref()?.join_url.clone())
                .or_else(|| ev.and_then(|e| e.online_meeting_url.clone()))
                .or_else(|| self.online_meeting_url.clone())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| extract_teams_meetup_url(&body_html))
                .or_else(|| extract_teams_meetup_url(&body_plain));
            let response_status = ev
                .and_then(|e| e.response_status.as_ref()?.response.clone())
                .or_else(|| self.response_type.clone())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            Some(MeetingRequestInfo {
                meeting_message_type: self
                    .meeting_message_type
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                event_id: ev
                    .and_then(|e| e.id.clone())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                start_iso,
                end_iso,
                location,
                online_meeting_url,
                response_status,
            })
        } else {
            None
        };

        MailMessage {
            id: self.id,
            internet_message_id: self
                .internet_message_id
                .filter(|s| !s.trim().is_empty()),
            conversation_id: self.conversation_id.filter(|s| !s.trim().is_empty()),
            folder_id: self.parent_folder_id.filter(|s| !s.trim().is_empty()),
            subject: self.subject.unwrap_or_default(),
            from: self.from.and_then(GraphRecipient::into_email_recipient),
            to_recipients: self
                .to_recipients
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            cc_recipients: self
                .cc_recipients
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            bcc_recipients: self
                .bcc_recipients
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            reply_to: self
                .reply_to
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            received_at: self.received_date_time.unwrap_or_default(),
            sent_at: self.sent_date_time.filter(|s| !s.trim().is_empty()),
            is_read: self.is_read.unwrap_or(false),
            is_draft: self.is_draft.unwrap_or(false),
            flagged,
            importance: self
                .importance
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "normal".to_string()),
            inference_classification: self
                .inference_classification
                .filter(|s| !s.trim().is_empty()),
            has_attachments: self.has_attachments.unwrap_or(false),
            body_loaded,
            preview,
            body_html,
            body_plain,
            attachments: self.attachments.into_iter().map(Into::into).collect(),
            meeting_request,
            web_link: self.web_link.filter(|s| !s.trim().is_empty()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MailUploadSessionResponse {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct CreatedDraftResponse {
    id: String,
}

struct StagedFile<'a> {
    path: &'a Path,
    name: String,
    mime: &'static str,
    size: u64,
}

impl GraphClient {
    /// Lists visible Outlook mail folders (`/me/mailFolders?$top=100&includeHiddenFolders=false`),
    /// following `@odata.nextLink`.
    pub async fn list_mail_folders(&self) -> Result<Vec<MailFolder>> {
        let mut next_url =
            Some("/me/mailFolders?$top=100&includeHiddenFolders=false".to_string());
        let mut folders = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphMailFolder> = self.get_json(&url).await?;
            for raw in page.value {
                let display_name = raw
                    .display_name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| raw.id.clone());
                let well_known_name = raw
                    .well_known_name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .or_else(|| derive_well_known_folder_name(&display_name))
                    .or_else(|| derive_well_known_folder_name(&raw.id));
                folders.push(MailFolder {
                    id: raw.id,
                    display_name,
                    well_known_name,
                    parent_folder_id: raw.parent_folder_id.filter(|s| !s.trim().is_empty()),
                    child_folder_count: raw.child_folder_count,
                    unread_item_count: raw.unread_item_count,
                    total_item_count: raw.total_item_count,
                });
            }
            next_url = page.next_link;
        }

        Ok(folders)
    }

    /// Lists up to `top` mail messages in `folder_id` (or `"all"` for `/me/messages`).
    pub async fn list_mail_messages(
        &self,
        folder_id: &str,
        top: usize,
        header_only: bool,
    ) -> Result<Vec<MailMessage>> {
        if top == 0 {
            return Ok(Vec::new());
        }
        let mut messages = Vec::new();
        let mut next_link: Option<String> = None;

        loop {
            let remaining = top.saturating_sub(messages.len());
            if remaining == 0 {
                break;
            }
            let page_top = remaining.min(100);
            let (page_msgs, next) = self
                .list_mail_messages_page(folder_id, page_top, next_link.as_deref(), header_only)
                .await?;
            if page_msgs.is_empty() {
                break;
            }
            messages.extend(page_msgs);
            if messages.len() >= top || next.is_none() {
                break;
            }
            next_link = next;
        }

        if messages.len() > top {
            messages.truncate(top);
        }
        Ok(messages)
    }

    /// Fetches a single page of mail messages with `@odata.nextLink` pagination support.
    pub async fn list_mail_messages_page(
        &self,
        folder_id: &str,
        top: usize,
        next_link: Option<&str>,
        header_only: bool,
    ) -> Result<(Vec<MailMessage>, Option<String>)> {
        let trimmed_next = next_link.map(str::trim).filter(|s| !s.is_empty());
        if top == 0 && trimmed_next.is_none() {
            return Ok((Vec::new(), None));
        }

        let url = match trimmed_next {
            Some(link) => link.to_string(),
            None => {
                let page_top = top.clamp(1, 100);
                let select_fields = if header_only {
                    MAIL_HEADER_SELECT_FIELDS
                } else {
                    MAIL_FULL_SELECT_FIELDS
                };
                let base_path = match normalize_folder_target(folder_id) {
                    None => "/me/messages".to_string(),
                    Some(folder) => {
                        format!("/me/mailFolders/{}/messages", urlencoding::encode(&folder))
                    }
                };
                format!(
                    "{base_path}?$top={page_top}&$orderby=receivedDateTime desc&$select={select_fields}"
                )
            }
        };

        let page: GraphListPage<GraphMailMessage> = self.get_json(&url).await?;
        let messages = page
            .value
            .into_iter()
            .map(|raw| raw.into_mail_message(!header_only))
            .collect();
        Ok((messages, page.next_link))
    }

    /// Synchronizes mail message changes via `/messages/delta`, returning
    /// `(upserted_messages, removed_ids, new_delta_link)`.
    pub async fn delta_mail_messages(
        &self,
        folder_id: &str,
        delta_or_next_link: Option<&str>,
        top: usize,
    ) -> Result<(Vec<MailMessage>, Vec<String>, Option<String>)> {
        let initial_url = match delta_or_next_link.map(str::trim).filter(|s| !s.is_empty()) {
            Some(link) => link.to_string(),
            None => {
                let base_path = match normalize_folder_target(folder_id) {
                    None => "/me/messages/delta".to_string(),
                    Some(folder) => {
                        format!(
                            "/me/mailFolders/{}/messages/delta",
                            urlencoding::encode(&folder)
                        )
                    }
                };
                format!("{base_path}?$select={MAIL_HEADER_SELECT_FIELDS}")
            }
        };

        let page_size = if top == 0 { 50 } else { top.min(100) };
        let prefer_val = format!("odata.maxpagesize={page_size}");
        let headers = [("Prefer", prefer_val.as_str())];

        let mut next_url = Some(initial_url);
        let mut upserted = Vec::new();
        let mut removed_ids = Vec::new();
        let mut new_delta_link = None;

        while let Some(url) = next_url {
            let page: GraphDeltaPage<serde_json::Value> =
                self.get_json_with_headers(&url, &headers).await?;
            for val in page.value {
                if val.get("@removed").is_some() {
                    if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                        removed_ids.push(id.to_string());
                    }
                } else {
                    let raw: GraphMailMessage = serde_json::from_value(val)
                        .context("Failed to deserialize Graph mail delta item")?;
                    upserted.push(raw.into_mail_message(false));
                }
            }
            if page.delta_link.is_some() {
                new_delta_link = page.delta_link;
            }
            next_url = page.next_link;
        }

        Ok((upserted, removed_ids, new_delta_link))
    }

    /// Fetches a full mail message with `$expand=attachments`, setting `body_loaded = true`.
    pub async fn get_mail_message(&self, message_id: &str) -> Result<MailMessage> {
        let path = format!(
            "/me/messages/{}?$expand=attachments",
            urlencoding::encode(message_id.trim())
        );
        let raw: GraphMailMessage = self.get_json(&path).await?;
        Ok(raw.into_mail_message(true))
    }

    /// Fetches full bodies and expanded attachments for multiple messages via `POST /$batch` (chunked by 20).
    pub async fn batch_get_mail_messages(&self, message_ids: &[String]) -> Result<Vec<MailMessage>> {
        let mut seen = std::collections::HashSet::new();
        let mut unique_ids = Vec::new();
        for id in message_ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                unique_ids.push(trimmed.to_string());
            }
        }
        if unique_ids.is_empty() {
            return Ok(Vec::new());
        }

        let requests: Vec<BatchSubRequest> = unique_ids
            .iter()
            .enumerate()
            .map(|(idx, id)| BatchSubRequest {
                id: idx.to_string(),
                method: "GET".to_string(),
                url: format!(
                    "/me/messages/{}?$select={MAIL_FULL_SELECT_FIELDS}&$expand=attachments",
                    urlencoding::encode(id)
                ),
                body: None,
                headers: None,
            })
            .collect();

        let responses = self.batch(&requests).await?;
        let mut by_idx: std::collections::BTreeMap<usize, MailMessage> =
            std::collections::BTreeMap::new();
        for resp in responses {
            let Ok(idx) = resp.id.parse::<usize>() else {
                continue;
            };
            if !(200..300).contains(&resp.status) {
                continue;
            }
            if let Ok(raw) = serde_json::from_value::<GraphMailMessage>(resp.body) {
                by_idx.insert(idx, raw.into_mail_message(true));
            }
        }

        Ok(by_idx.into_values().collect())
    }

    /// Sends an email via `/me/sendMail` (or draft + chunked upload session when total attachments exceed 3 MiB
    /// or `draft_id` is set), supporting both file attachments and inline `cid:` images.
    pub async fn send_mail(&self, mail: &OutgoingMail) -> Result<()> {
        let (staged, total_size, any_over_limit) = stage_file_attachments(&mail.attachments).await?;
        let inline_total: u64 = mail
            .inline_images
            .iter()
            .map(|img| img.bytes.len() as u64)
            .sum();
        let combined_size = total_size.saturating_add(inline_total);
        let any_inline_over = mail
            .inline_images
            .iter()
            .any(|img| img.bytes.len() as u64 > MAIL_SMALL_ATTACHMENT_LIMIT);
        let has_existing_draft = mail
            .draft_id
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);

        let use_draft_flow = has_existing_draft
            || any_over_limit
            || any_inline_over
            || ((!staged.is_empty() || !mail.inline_images.is_empty())
                && combined_size > MAIL_SMALL_ATTACHMENT_LIMIT);

        let mut message = build_outgoing_message_json(mail);

        if !use_draft_flow {
            let mut atts = build_inline_image_attachments_json(&mail.inline_images);
            for f in staged {
                let bytes = tokio::fs::read(f.path)
                    .await
                    .with_context(|| format!("Failed to read attachment: {}", f.path.display()))?;
                atts.push(serde_json::json!({
                    "@odata.type": "#microsoft.graph.fileAttachment",
                    "name": f.name,
                    "contentType": f.mime,
                    "contentBytes": BASE64_STANDARD.encode(&bytes),
                }));
            }
            if !atts.is_empty() {
                message["attachments"] = serde_json::Value::Array(atts);
            }

            let payload = serde_json::json!({
                "message": message,
                "saveToSentItems": mail.save_to_sent_items,
            });
            return self.post_json_empty("/me/sendMail", &payload).await;
        }

        message["isDraft"] = serde_json::Value::Bool(true);
        let draft_id = match mail.draft_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(existing_id) => {
                let patch_path = format!("/me/messages/{}", urlencoding::encode(existing_id));
                self.patch_json(&patch_path, &message).await?;
                existing_id.to_string()
            }
            None => {
                let draft: CreatedDraftResponse = self.post_json("/me/messages", &message).await?;
                draft.id
            }
        };
        let draft_id_enc = urlencoding::encode(&draft_id);

        for att_payload in build_inline_image_attachments_json(&mail.inline_images) {
            self.post_json_empty(
                &format!("/me/messages/{draft_id_enc}/attachments"),
                &att_payload,
            )
            .await?;
        }

        for f in staged {
            self.upload_draft_file_attachment(&draft_id_enc, &f).await?;
        }

        self.post_empty(&format!("/me/messages/{draft_id_enc}/send"))
            .await
    }

    /// Creates a new draft (`POST /me/messages`) or updates an existing draft (`PATCH /me/messages/{draft_id}`).
    pub async fn save_draft(&self, mail: &OutgoingMail) -> Result<MailMessage> {
        let (staged, total_size, any_over_limit) = stage_file_attachments(&mail.attachments).await?;
        let mut message = build_outgoing_message_json(mail);
        message["isDraft"] = serde_json::Value::Bool(true);

        if let Some(existing_id) = mail
            .draft_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let patch_path = format!("/me/messages/{}", urlencoding::encode(existing_id));
            let updated: Option<GraphMailMessage> =
                self.patch_json_optional(&patch_path, &message).await?;
            let draft_id_enc = urlencoding::encode(existing_id);

            for att_payload in build_inline_image_attachments_json(&mail.inline_images) {
                self.post_json_empty(
                    &format!("/me/messages/{draft_id_enc}/attachments"),
                    &att_payload,
                )
                .await?;
            }
            for f in staged {
                self.upload_draft_file_attachment(&draft_id_enc, &f).await?;
            }

            if !mail.inline_images.is_empty() || !mail.attachments.is_empty() || updated.is_none() {
                return self.get_mail_message(existing_id).await;
            }
            return Ok(updated.unwrap().into_mail_message(true));
        }

        let inline_total: u64 = mail
            .inline_images
            .iter()
            .map(|img| img.bytes.len() as u64)
            .sum();
        let combined_size = total_size.saturating_add(inline_total);
        let inline_small = !any_over_limit && combined_size <= MAIL_SMALL_ATTACHMENT_LIMIT;

        if inline_small {
            let mut atts = build_inline_image_attachments_json(&mail.inline_images);
            for f in staged {
                let bytes = tokio::fs::read(f.path)
                    .await
                    .with_context(|| format!("Failed to read attachment: {}", f.path.display()))?;
                atts.push(serde_json::json!({
                    "@odata.type": "#microsoft.graph.fileAttachment",
                    "name": f.name,
                    "contentType": f.mime,
                    "contentBytes": BASE64_STANDARD.encode(&bytes),
                }));
            }
            if !atts.is_empty() {
                message["attachments"] = serde_json::Value::Array(atts);
            }
            let created: GraphMailMessage = self.post_json("/me/messages", &message).await?;
            return Ok(created.into_mail_message(true));
        }

        let created: GraphMailMessage = self.post_json("/me/messages", &message).await?;
        let draft_id = created.id.clone();
        let draft_id_enc = urlencoding::encode(&draft_id);
        for att_payload in build_inline_image_attachments_json(&mail.inline_images) {
            self.post_json_empty(
                &format!("/me/messages/{draft_id_enc}/attachments"),
                &att_payload,
            )
            .await?;
        }
        for f in staged {
            self.upload_draft_file_attachment(&draft_id_enc, &f).await?;
        }
        self.get_mail_message(&draft_id).await
    }

    async fn upload_draft_file_attachment(
        &self,
        draft_id_enc: &str,
        f: &StagedFile<'_>,
    ) -> Result<()> {
        if f.size <= MAIL_SMALL_ATTACHMENT_LIMIT {
            let bytes = tokio::fs::read(f.path)
                .await
                .with_context(|| format!("Failed to read attachment: {}", f.path.display()))?;
            let att_payload = serde_json::json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": f.name,
                "contentType": f.mime,
                "contentBytes": BASE64_STANDARD.encode(&bytes),
            });
            self.post_json_empty(
                &format!("/me/messages/{draft_id_enc}/attachments"),
                &att_payload,
            )
            .await
        } else {
            let session_req = serde_json::json!({
                "AttachmentItem": {
                    "attachmentType": "file",
                    "name": f.name,
                    "size": f.size,
                }
            });
            let session: MailUploadSessionResponse = self
                .post_json(
                    &format!("/me/messages/{draft_id_enc}/attachments/createUploadSession"),
                    &session_req,
                )
                .await?;

            let mut file = tokio::fs::File::open(f.path).await.with_context(|| {
                format!("Failed to open large attachment: {}", f.path.display())
            })?;
            let mut offset: u64 = 0;

            while offset < f.size {
                let mut buf = vec![0u8; MAIL_UPLOAD_CHUNK_SIZE];
                let mut filled = 0usize;
                while filled < MAIL_UPLOAD_CHUNK_SIZE {
                    let n = file.read(&mut buf[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    bail!("Unexpected EOF while uploading mail attachment.");
                }
                buf.truncate(filled);
                let end_inclusive = offset + (filled as u64) - 1;
                self.put_upload_chunk(&session.upload_url, &buf, offset, end_inclusive, f.size)
                    .await?;
                offset += filled as u64;
            }
            Ok(())
        }
    }

    pub async fn reply_mail(
        &self,
        message_id: &str,
        comment_html: &str,
        reply_all: bool,
    ) -> Result<()> {
        let action = if reply_all { "replyAll" } else { "reply" };
        let path = format!(
            "/me/messages/{}/{action}",
            urlencoding::encode(message_id.trim())
        );
        let body = serde_json::json!({ "comment": comment_html });
        self.post_json_empty(&path, &body).await
    }

    pub async fn forward_mail(
        &self,
        message_id: &str,
        to: &[String],
        comment_html: &str,
    ) -> Result<()> {
        let recipients = recipients_to_json(to);
        if recipients.is_empty() {
            bail!("At least one recipient email address is required to forward a message.");
        }
        let path = format!(
            "/me/messages/{}/forward",
            urlencoding::encode(message_id.trim())
        );
        let body = serde_json::json!({
            "comment": comment_html,
            "toRecipients": recipients,
        });
        self.post_json_empty(&path, &body).await
    }

    pub async fn set_mail_read(&self, message_id: &str, is_read: bool) -> Result<()> {
        let path = format!("/me/messages/{}", urlencoding::encode(message_id.trim()));
        self.patch_json(&path, &serde_json::json!({ "isRead": is_read }))
            .await
    }

    pub async fn set_mail_flag(&self, message_id: &str, flagged: bool) -> Result<()> {
        let path = format!("/me/messages/{}", urlencoding::encode(message_id.trim()));
        let status = if flagged { "flagged" } else { "notFlagged" };
        self.patch_json(
            &path,
            &serde_json::json!({ "flag": { "flagStatus": status } }),
        )
        .await
    }

    /// Moves a mail message to `destination_folder_id` (supports well-known names such as
    /// `"archive"`, `"deleteditems"`, `"inbox"`, `"junkemail"`).
    pub async fn move_mail_message(
        &self,
        message_id: &str,
        destination_folder_id: &str,
    ) -> Result<MailMessage> {
        let dest = normalize_folder_target(destination_folder_id)
            .ok_or_else(|| anyhow!("Destination folder ID is required to move a mail message."))?;
        let path = format!(
            "/me/messages/{}/move",
            urlencoding::encode(message_id.trim())
        );
        let moved: GraphMailMessage = self
            .post_json(&path, &serde_json::json!({ "destinationId": dest }))
            .await?;
        let has_body = moved.body.is_some();
        Ok(moved.into_mail_message(has_body))
    }

    pub async fn delete_mail_message(&self, message_id: &str) -> Result<()> {
        let path = format!("/me/messages/{}", urlencoding::encode(message_id.trim()));
        self.delete_req(&path).await
    }

    /// Fetches attachment metadata and raw bytes (decoding `contentBytes` or streaming `/$value`).
    pub async fn fetch_mail_attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let msg_enc = urlencoding::encode(message_id.trim());
        let trimmed_att_id = attachment_id.trim();

        let meta: GraphMailAttachment = if let Some(cid_raw) = trimmed_att_id
            .strip_prefix("cid:")
            .or_else(|| trimmed_att_id.strip_prefix("CID:"))
        {
            let target_cid = clean_content_id(Some(cid_raw.to_string()))
                .ok_or_else(|| anyhow!("Empty cid attachment identifier"))?;
            let list_path = format!("/me/messages/{msg_enc}/attachments");
            let page: GraphListPage<GraphMailAttachment> = self.get_json(&list_path).await?;
            page.value
                .into_iter()
                .find(|a| {
                    clean_content_id(a.content_id.clone())
                        .is_some_and(|cid| cid.eq_ignore_ascii_case(&target_cid))
                })
                .ok_or_else(|| {
                    anyhow!("Inline cid attachment '{trimmed_att_id}' not found on message {message_id}")
                })?
        } else {
            let att_enc = urlencoding::encode(trimmed_att_id);
            let meta_path = format!("/me/messages/{msg_enc}/attachments/{att_enc}");
            self.get_json(&meta_path).await?
        };

        let resolved_id = if meta.id.trim().is_empty() {
            trimmed_att_id.to_string()
        } else {
            meta.id.clone()
        };
        let name = meta
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("attachment_{resolved_id}"));
        let content_type = meta
            .content_type
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let bytes = if let Some(b64) = meta.content_bytes.filter(|s| !s.trim().is_empty()) {
            decode_b64_whitespace_tolerant(&b64)?
        } else {
            let att_enc = urlencoding::encode(resolved_id.trim());
            let value_path = format!("/me/messages/{msg_enc}/attachments/{att_enc}/$value");
            self.get_bytes_with_limit(&value_path, true, MAX_IN_MEMORY_ATTACHMENT_BYTES)
                .await?
        };

        Ok((name, content_type, bytes))
    }

    /// Downloads a `MailAttachment` to `dest_dir`, using inline `content_bytes_b64` when already
    /// expanded or streaming `/$value` directly to disk, and sniffing image extensions when missing.
    pub async fn download_mail_attachment(
        &self,
        message_id: &str,
        attachment: &MailAttachment,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("Failed to create download directory: {}", dest_dir.display()))?;

        let raw_name = if attachment.name.trim().is_empty() {
            format!("attachment_{}", attachment.id)
        } else {
            attachment.name.clone()
        };
        let safe_name = sanitize_filename(&raw_name);
        let mut dest_path = deduplicate_dest_path(dest_dir, &safe_name);
        let mut final_name = dest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        let bytes_written = if let Some(b64) = attachment
            .content_bytes_b64
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let bytes = decode_b64_whitespace_tolerant(b64)?;
            tokio::fs::write(&dest_path, &bytes)
                .await
                .with_context(|| format!("Failed to save mail attachment: {}", dest_path.display()))?;
            bytes.len() as u64
        } else {
            let msg_enc = urlencoding::encode(message_id.trim());
            let att_enc = urlencoding::encode(attachment.id.trim());
            let value_path = format!("/me/messages/{msg_enc}/attachments/{att_enc}/$value");
            match self.get_to_file(&value_path, &dest_path, true).await {
                Ok(n) => n,
                Err(err) if is_auth_or_forbidden_error(&err) => return Err(err),
                Err(_) => {
                    let (_name, _ct, bytes) = self
                        .fetch_mail_attachment_bytes(message_id, &attachment.id)
                        .await?;
                    tokio::fs::write(&dest_path, &bytes).await.with_context(|| {
                        format!("Failed to save mail attachment: {}", dest_path.display())
                    })?;
                    bytes.len() as u64
                }
            }
        };

        if let Some((new_path, new_name)) =
            maybe_refine_image_extension(&dest_path, dest_dir, &safe_name, &final_name).await
        {
            dest_path = new_path;
            final_name = new_name;
        } else if Path::new(&final_name).extension().is_none() {
            if let Some(ext) = extension_for_content_type(&attachment.content_type) {
                let candidate = format!("{final_name}.{ext}");
                let target_path = deduplicate_dest_path(dest_dir, &candidate);
                if tokio::fs::rename(&dest_path, &target_path).await.is_ok() {
                    if let Some(name) = target_path.file_name().and_then(|s| s.to_str()) {
                        final_name = name.to_string();
                        dest_path = target_path;
                    }
                }
            }
        }

        Ok(DownloadedFile {
            path: dest_path,
            file_name: final_name,
            bytes_written,
        })
    }

    /// Convenience helper when only `attachment_id` is known.
    pub async fn download_mail_attachment_by_id(
        &self,
        message_id: &str,
        attachment_id: &str,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        let (name, content_type, bytes) = self
            .fetch_mail_attachment_bytes(message_id, attachment_id)
            .await?;
        let att = MailAttachment {
            id: attachment_id.to_string(),
            name,
            content_type,
            size: bytes.len() as u64,
            is_inline: false,
            content_id: None,
            content_bytes_b64: Some(BASE64_STANDARD.encode(&bytes)),
        };
        self.download_mail_attachment(message_id, &att, dest_dir).await
    }

    /// Searches remote mailbox via `/me/messages?$search="..."` with `ConsistencyLevel: eventual`.
    pub async fn search_mail_remote(&self, query: &str, limit: usize) -> Result<Vec<MailMessage>> {
        let q = query.trim();
        if limit == 0 || q.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = q.replace('\\', "\\\\").replace('"', "\\\"");
        let search_val = format!("\"{escaped}\"");
        let top = limit.min(100);
        let mut next_url = Some(format!(
            "/me/messages?$search={}&$top={top}&$select={MAIL_HEADER_SELECT_FIELDS}",
            urlencoding::encode(&search_val)
        ));
        let mut messages = Vec::new();
        let headers = [("ConsistencyLevel", "eventual")];

        while let Some(url) = next_url {
            let page: GraphListPage<GraphMailMessage> =
                self.get_json_with_headers(&url, &headers).await?;
            for raw in page.value {
                messages.push(raw.into_mail_message(false));
                if messages.len() >= limit {
                    return Ok(messages);
                }
            }
            next_url = page.next_link;
        }

        Ok(messages)
    }
}

fn decode_b64_whitespace_tolerant(b64: &str) -> Result<Vec<u8>> {
    let compact: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    BASE64_STANDARD
        .decode(compact.as_bytes())
        .context("Failed to decode mail attachment base64 content")
}

async fn stage_file_attachments(paths: &[std::path::PathBuf]) -> Result<(Vec<StagedFile<'_>>, u64, bool)> {
    let mut staged = Vec::with_capacity(paths.len());
    let mut total_size: u64 = 0;
    let mut any_over_limit = false;

    for path in paths {
        let meta = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("Attachment file not found: {}", path.display()))?;
        if !meta.is_file() {
            bail!("Only regular files can be attached: {}", path.display());
        }
        let size = meta.len();
        if size > MAIL_SMALL_ATTACHMENT_LIMIT {
            any_over_limit = true;
        }
        total_size = total_size.saturating_add(size);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Invalid attachment file name: {}", path.display()))?
            .to_string();
        let mime = guess_mime_type(path);
        staged.push(StagedFile {
            path,
            name,
            mime,
            size,
        });
    }
    Ok((staged, total_size, any_over_limit))
}

fn rewrite_inline_image_cids(body_html: &str, inline_images: &[InlineImageUpload]) -> String {
    if inline_images.is_empty() {
        return body_html.to_string();
    }
    let mut html = body_html.to_string();
    for img in inline_images {
        let cid = img
            .content_id
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>');
        if cid.is_empty() {
            continue;
        }
        let cid_uri = format!("cid:{cid}");
        let temp_id = img.temp_id.trim();
        if !temp_id.is_empty() {
            for pattern in [
                format!("../hostedContents/{temp_id}/$value"),
                format!("./hostedContents/{temp_id}/$value"),
                format!("hostedContents/{temp_id}/$value"),
                format!("../hostedContents/{temp_id}"),
                format!("hostedContents/{temp_id}"),
                format!("cid:{temp_id}"),
            ] {
                if html.contains(&pattern) {
                    html = html.replace(&pattern, &cid_uri);
                }
            }
            // Replace bare `src="<temp_id>"` or `src='<temp_id>'`
            let dq = format!("src=\"{temp_id}\"");
            if html.contains(&dq) {
                html = html.replace(&dq, &format!("src=\"{cid_uri}\""));
            }
            let sq = format!("src='{temp_id}'");
            if html.contains(&sq) {
                html = html.replace(&sq, &format!("src='{cid_uri}'"));
            }
        }
    }
    html
}

fn build_inline_image_attachments_json(inline_images: &[InlineImageUpload]) -> Vec<serde_json::Value> {
    inline_images
        .iter()
        .map(|img| {
            let cid = img
                .content_id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            serde_json::json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": img.file_name,
                "contentType": img.content_type,
                "isInline": true,
                "contentId": cid,
                "contentBytes": BASE64_STANDARD.encode(&img.bytes),
            })
        })
        .collect()
}

fn derive_well_known_folder_name(name: &str) -> Option<String> {
    let normalized = name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "inbox" => Some("inbox".to_string()),
        "sentitems" | "sent items" | "sent" => Some("sentitems".to_string()),
        "drafts" => Some("drafts".to_string()),
        "archive" => Some("archive".to_string()),
        "deleteditems" | "deleted items" | "trash" => Some("deleteditems".to_string()),
        "junkemail" | "junk email" | "junk" => Some("junkemail".to_string()),
        _ => None,
    }
}

fn normalize_folder_target(folder_id_or_wellknown: &str) -> Option<String> {
    let trimmed = folder_id_or_wellknown.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return None;
    }
    let mapped = match trimmed.to_ascii_lowercase().as_str() {
        "inbox" => "inbox",
        "sentitems" | "sent" => "sentitems",
        "drafts" => "drafts",
        "archive" => "archive",
        "deleteditems" | "trash" => "deleteditems",
        "junk" | "junkemail" => "junkemail",
        _ => trimmed,
    };
    Some(mapped.to_string())
}

fn recipients_to_json(addresses: &[String]) -> Vec<serde_json::Value> {
    addresses
        .iter()
        .filter_map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Some((name_part, rest)) = trimmed.rsplit_once('<') {
                if let Some(addr) = rest.strip_suffix('>') {
                    let addr = addr.trim();
                    let name = name_part.trim().trim_matches('"');
                    if !addr.is_empty() {
                        return Some(if name.is_empty() {
                            serde_json::json!({ "emailAddress": { "address": addr } })
                        } else {
                            serde_json::json!({ "emailAddress": { "name": name, "address": addr } })
                        });
                    }
                }
            }
            Some(serde_json::json!({ "emailAddress": { "address": trimmed } }))
        })
        .collect()
}

fn build_outgoing_message_json(mail: &OutgoingMail) -> serde_json::Value {
    let rewritten_html = rewrite_inline_image_cids(&mail.body_html, &mail.inline_images);
    let mut message = serde_json::json!({
        "subject": mail.subject,
        "body": {
            "contentType": "HTML",
            "content": rewritten_html,
        },
        "toRecipients": recipients_to_json(&mail.to),
        "ccRecipients": recipients_to_json(&mail.cc),
        "bccRecipients": recipients_to_json(&mail.bcc),
    });
    if let Some(imp) = mail
        .importance
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        message["importance"] = serde_json::Value::String(imp.to_string());
    }
    message
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        auth::{AuthManager, StoredTokens},
        config::AppConfig,
    };
    use axum::{
        body::Bytes,
        extract::{Path as AxumPath, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{delete, get, patch, post, put},
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tempfile::tempdir;

    pub(crate) fn test_graph_client(graph_base_url: &str) -> GraphClient {
        let mut cfg = AppConfig::default();
        cfg.graph_base_url = graph_base_url.to_string();
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".into(),
                tenant_id: "tid".into(),
                user_id: Some("me-1".into()),
                user_display_name: Some("Tester".into()),
                user_email: Some("me@example.com".into()),
                auth_mode: "access_token".into(),
            },
        );
        GraphClient::new(cfg, auth).unwrap()
    }

    #[tokio::test]
    async fn test_list_mail_folders_pagination_and_well_known_names() {
        #[derive(Clone)]
        struct ServerState {
            base_url: Arc<Mutex<String>>,
        }
        let state = ServerState {
            base_url: Arc::new(Mutex::new(String::new())),
        };
        let state_clone = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(|State(st): State<ServerState>, Query(params): Query<HashMap<String, String>>| async move {
                    if params.get("page").map(String::as_str) == Some("2") {
                        Json(serde_json::json!({
                            "value": [
                                { "id": "f4", "displayName": "Archive", "childFolderCount": 2, "unreadItemCount": 0, "totalItemCount": 5 },
                                { "id": "f5", "displayName": "Deleted Items", "childFolderCount": 0, "unreadItemCount": 1, "totalItemCount": 3 },
                                { "id": "f6", "displayName": "Junk Email", "childFolderCount": 0, "unreadItemCount": 2, "totalItemCount": 2 },
                                { "id": "f7", "displayName": "Projects", "wellKnownName": "custom", "childFolderCount": 1, "unreadItemCount": 0, "totalItemCount": 1 }
                            ]
                        }))
                    } else {
                        assert_eq!(params.get("includeHiddenFolders").map(String::as_str), Some("false"));
                        let base = st.base_url.lock().unwrap().clone();
                        Json(serde_json::json!({
                            "value": [
                                { "id": "f1", "displayName": "Inbox", "childFolderCount": 3, "unreadItemCount": 4, "totalItemCount": 20 },
                                { "id": "f2", "displayName": "Sent Items", "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 10 },
                                { "id": "f3", "displayName": "Drafts", "childFolderCount": 0, "unreadItemCount": 1, "totalItemCount": 2 }
                            ],
                            "@odata.nextLink": format!("{base}/v1.0/me/mailFolders?page=2")
                        }))
                    }
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *state_clone.base_url.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let folders = client.list_mail_folders().await.unwrap();
        assert_eq!(folders.len(), 7);
        assert_eq!(folders[0].well_known_name.as_deref(), Some("inbox"));
        assert_eq!(folders[0].child_folder_count, 3);
        assert_eq!(folders[1].well_known_name.as_deref(), Some("sentitems"));
        assert_eq!(folders[2].well_known_name.as_deref(), Some("drafts"));
        assert_eq!(folders[3].well_known_name.as_deref(), Some("archive"));
        assert_eq!(folders[3].child_folder_count, 2);
        assert_eq!(folders[4].well_known_name.as_deref(), Some("deleteditems"));
        assert_eq!(folders[5].well_known_name.as_deref(), Some("junkemail"));
        assert_eq!(folders[6].well_known_name.as_deref(), Some("custom"));
    }

    #[tokio::test]
    async fn test_list_page_delta_and_get_mail_message_with_meeting_request() {
        #[derive(Clone)]
        struct ServerState {
            base_url: Arc<Mutex<String>>,
        }
        let state = ServerState {
            base_url: Arc::new(Mutex::new(String::new())),
        };
        let state_clone = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders/{folder}/messages",
                get(|State(st): State<ServerState>, AxumPath(folder): AxumPath<String>, Query(params): Query<HashMap<String, String>>| async move {
                    assert_eq!(folder, "sentitems");
                    let base = st.base_url.lock().unwrap().clone();
                    if params.get("page").map(String::as_str) == Some("2") {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m3",
                                    "subject": "Empty body",
                                    "receivedDateTime": "2026-11-15T08:00:00Z",
                                    "bodyPreview": "Preview fallback",
                                    "body": { "contentType": "html", "content": "" }
                                }
                            ]
                        }))
                    } else {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m1",
                                    "internetMessageId": "<m1@example.com>",
                                    "subject": "HTML Mail",
                                    "from": { "emailAddress": { "name": "Alice", "address": "alice@example.com" } },
                                    "toRecipients": [{ "emailAddress": { "address": "bob@example.com" } }],
                                    "replyTo": [{ "emailAddress": { "address": "reply@example.com" } }],
                                    "receivedDateTime": "2026-11-15T10:00:00Z",
                                    "isRead": true,
                                    "flag": { "flagStatus": "flagged" },
                                    "importance": "high",
                                    "inferenceClassification": "focused",
                                    "hasAttachments": false,
                                    "bodyPreview": "Preview",
                                    "body": { "contentType": "html", "content": "<p>Hello <b>World</b></p>" }
                                },
                                {
                                    "id": "m2",
                                    "subject": "Text Mail",
                                    "inferenceClassification": "other",
                                    "receivedDateTime": "2026-11-15T09:00:00Z",
                                    "bodyPreview": "preview2",
                                    "body": { "contentType": "text", "content": "a < b & c" }
                                }
                            ],
                            "@odata.nextLink": format!("{base}/v1.0/me/mailFolders/sentitems/messages?page=2")
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/{folder}/messages/delta",
                get(|AxumPath(folder): AxumPath<String>, headers: HeaderMap, State(st): State<ServerState>, Query(params): Query<HashMap<String, String>>| async move {
                    assert_eq!(folder, "inbox");
                    assert_eq!(
                        headers.get("prefer").and_then(|v| v.to_str().ok()),
                        Some("odata.maxpagesize=25")
                    );
                    let base = st.base_url.lock().unwrap().clone();
                    if params.get("page").map(String::as_str) == Some("2") {
                        Json(serde_json::json!({
                            "value": [
                                { "id": "m-del", "@removed": { "reason": "deleted" } }
                            ],
                            "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=tok2")
                        }))
                    } else {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m-up",
                                    "subject": "New Mail",
                                    "inferenceClassification": "focused",
                                    "bodyPreview": "hi"
                                }
                            ],
                            "@odata.nextLink": format!("{base}/v1.0/me/mailFolders/inbox/messages/delta?page=2")
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/messages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    assert_eq!(id, "m-invite");
                    Json(serde_json::json!({
                        "@odata.type": "#microsoft.graph.eventMessageRequest",
                        "id": "m-invite",
                        "subject": "Architecture Review",
                        "meetingMessageType": "meetingRequest",
                        "startDateTime": { "dateTime": "2026-11-20T10:00:00Z", "timeZone": "UTC" },
                        "endDateTime": { "dateTime": "2026-11-20T11:00:00Z", "timeZone": "UTC" },
                        "location": { "displayName": "Conf Room B" },
                        "event": {
                            "id": "ev-123",
                            "responseStatus": { "response": "notResponded" }
                        },
                        "hasAttachments": true,
                        "body": {
                            "contentType": "html",
                            "content": "<p>Join <a href=\"https://teams.microsoft.com/l/meetup-join/19%3ameeting/0\">link</a><img src=\"cid:logo-1\"></p>"
                        },
                        "attachments": [
                            {
                                "id": "att1",
                                "name": "logo.png",
                                "contentType": "image/png",
                                "size": 5,
                                "isInline": true,
                                "contentId": "<logo-1>",
                                "contentBytes": "aGVsbG8="
                            }
                        ]
                    }))
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *state_clone.base_url.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));

        let (page1, next1) = client
            .list_mail_messages_page("sent", 2, None, false)
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert!(page1[0].body_loaded);
        assert!(page1[0].flagged);
        assert!(page1[0].is_focused());
        assert_eq!(page1[0].internet_message_id.as_deref(), Some("<m1@example.com>"));
        assert_eq!(page1[0].reply_to[0].address, "reply@example.com");
        assert!(page1[0].body_plain.contains("Hello World"));
        assert!(!page1[1].is_focused());
        assert_eq!(page1[1].body_plain, "a < b & c");
        assert_eq!(page1[1].body_html, "a &lt; b &amp; c");
        assert!(next1.is_some());

        let all_sent = client.list_mail_messages("sent", 10, true).await.unwrap();
        assert_eq!(all_sent.len(), 3);
        assert!(!all_sent[0].body_loaded);
        assert_eq!(all_sent[2].body_plain, "Preview fallback");

        let (upserted, removed, dlink) = client.delta_mail_messages("inbox", None, 25).await.unwrap();
        assert_eq!(upserted.len(), 1);
        assert_eq!(upserted[0].id, "m-up");
        assert!(!upserted[0].body_loaded);
        assert_eq!(removed, vec!["m-del"]);
        assert!(dlink.unwrap().contains("$deltatoken=tok2"));

        let invite = client.get_mail_message("m-invite").await.unwrap();
        assert!(invite.body_loaded);
        assert_eq!(invite.attachments.len(), 1);
        assert!(invite.attachments[0].is_inline);
        assert_eq!(invite.attachments[0].content_id.as_deref(), Some("logo-1"));
        assert_eq!(invite.attachments[0].content_bytes_b64.as_deref(), Some("aGVsbG8="));
        let mr = invite.meeting_request.unwrap();
        assert_eq!(mr.meeting_message_type.as_deref(), Some("meetingRequest"));
        assert_eq!(mr.event_id.as_deref(), Some("ev-123"));
        assert_eq!(mr.location.as_deref(), Some("Conf Room B"));
        assert_eq!(
            mr.online_meeting_url.as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting/0")
        );
        assert_eq!(mr.response_status.as_deref(), Some("notResponded"));
    }

    #[tokio::test]
    async fn test_send_mail_inline_cid_draft_save_and_actions() {
        #[derive(Clone, Default)]
        struct Recorded {
            send_mail_payloads: Arc<Mutex<Vec<serde_json::Value>>>,
            draft_created: Arc<Mutex<bool>>,
            draft_patched: Arc<Mutex<bool>>,
            small_att_uploaded: Arc<Mutex<bool>>,
            chunk_ranges: Arc<Mutex<Vec<(String, bool)>>>,
            draft_sent: Arc<Mutex<bool>>,
            actions: Arc<Mutex<Vec<String>>>,
            base_url: Arc<Mutex<String>>,
        }
        let rec = Recorded::default();
        let rec_check = rec.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/sendMail",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    st.send_mail_payloads.lock().unwrap().push(body);
                    StatusCode::ACCEPTED
                }),
            )
            .route(
                "/v1.0/me/messages",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["isDraft"], true);
                    *st.draft_created.lock().unwrap() = true;
                    Json(serde_json::json!({
                        "id": "draft-1",
                        "subject": body["subject"],
                        "isDraft": true,
                        "body": body["body"]
                    }))
                }),
            )
            .route(
                "/v1.0/me/messages/draft-1",
                patch(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    *st.draft_patched.lock().unwrap() = true;
                    Json(serde_json::json!({
                        "id": "draft-1",
                        "subject": body["subject"],
                        "isDraft": true,
                        "body": body["body"]
                    }))
                }),
            )
            .route(
                "/v1.0/me/messages/draft-1/attachments",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["@odata.type"], "#microsoft.graph.fileAttachment");
                    *st.small_att_uploaded.lock().unwrap() = true;
                    (StatusCode::CREATED, Json(serde_json::json!({ "id": "att-small" })))
                }),
            )
            .route(
                "/v1.0/me/messages/draft-1/attachments/createUploadSession",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["AttachmentItem"]["attachmentType"], "file");
                    let base = st.base_url.lock().unwrap().clone();
                    Json(serde_json::json!({
                        "uploadUrl": format!("{base}/upload/session-1")
                    }))
                }),
            )
            .route(
                "/upload/session-1",
                put(|State(st): State<Recorded>, headers: HeaderMap, body: Bytes| async move {
                    let has_auth = headers.contains_key("authorization");
                    let range = headers
                        .get("content-range")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    assert!(!body.is_empty());
                    st.chunk_ranges.lock().unwrap().push((range, has_auth));
                    (StatusCode::OK, Json(serde_json::json!({ "nextExpectedRanges": [] })))
                }),
            )
            .route(
                "/v1.0/me/messages/draft-1/send",
                post(|State(st): State<Recorded>| async move {
                    *st.draft_sent.lock().unwrap() = true;
                    StatusCode::ACCEPTED
                }),
            )
            .route(
                "/v1.0/me/messages/m1/reply",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    st.actions
                        .lock()
                        .unwrap()
                        .push(format!("reply:{}", body["comment"].as_str().unwrap_or("")));
                    StatusCode::ACCEPTED
                }),
            )
            .route(
                "/v1.0/me/messages/m1/replyAll",
                post(|State(st): State<Recorded>| async move {
                    st.actions.lock().unwrap().push("replyAll".to_string());
                    StatusCode::ACCEPTED
                }),
            )
            .route(
                "/v1.0/me/messages/m1/forward",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["toRecipients"].as_array().unwrap().len(), 1);
                    st.actions.lock().unwrap().push("forward".to_string());
                    StatusCode::ACCEPTED
                }),
            )
            .route(
                "/v1.0/me/messages/m1",
                patch(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    st.actions.lock().unwrap().push(format!("patch:{body}"));
                    StatusCode::OK
                }),
            )
            .route(
                "/v1.0/me/messages/m1/move",
                post(|State(st): State<Recorded>, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["destinationId"], "deleteditems");
                    st.actions.lock().unwrap().push("move".to_string());
                    Json(serde_json::json!({ "id": "m1-moved", "parentFolderId": "deleteditems", "subject": "moved" }))
                }),
            )
            .route(
                "/v1.0/me/messages/m1-del",
                delete(|State(st): State<Recorded>| async move {
                    st.actions.lock().unwrap().push("delete".to_string());
                    StatusCode::NO_CONTENT
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(rec);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *rec_check.base_url.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let dir = tempdir().unwrap();

        let small_path = dir.path().join("note.txt");
        std::fs::write(&small_path, b"hello attachment").unwrap();

        // 1. Small send_mail with inline cid image rewrite + file attachment
        client
            .send_mail(&OutgoingMail {
                subject: "Inline & Small".into(),
                body_html: "<p>See <img src=\"../hostedContents/tmp-img-1/$value\"></p>".into(),
                to: vec!["Alice <a@example.com>".into()],
                importance: Some("high".into()),
                attachments: vec![small_path.clone()],
                inline_images: vec![InlineImageUpload {
                    temp_id: "tmp-img-1".into(),
                    content_id: "img-cid-1@outlook".into(),
                    file_name: "shot.png".into(),
                    content_type: "image/png".into(),
                    bytes: vec![0x89, b'P', b'N', b'G'],
                }],
                save_to_sent_items: true,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(rec_check.send_mail_payloads.lock().unwrap().len(), 1);
        let p0 = &rec_check.send_mail_payloads.lock().unwrap()[0];
        assert_eq!(
            p0["message"]["body"]["content"],
            "<p>See <img src=\"cid:img-cid-1@outlook\"></p>"
        );
        assert_eq!(p0["message"]["attachments"][0]["isInline"], true);
        assert_eq!(p0["message"]["attachments"][0]["contentId"], "img-cid-1@outlook");
        assert_eq!(p0["message"]["attachments"][1]["contentType"], "text/plain");

        // 2. Large attachment triggers draft + upload session
        let large_path = dir.path().join("big.bin");
        let big_bytes = vec![0xABu8; 3_600_000];
        std::fs::write(&large_path, &big_bytes).unwrap();

        client
            .send_mail(&OutgoingMail {
                subject: "Large".into(),
                body_html: "<p>big</p>".into(),
                to: vec!["b@example.com".into()],
                attachments: vec![small_path, large_path],
                save_to_sent_items: false,
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(*rec_check.draft_created.lock().unwrap());
        assert!(*rec_check.small_att_uploaded.lock().unwrap());
        assert!(*rec_check.draft_sent.lock().unwrap());
        let ranges = rec_check.chunk_ranges.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], ("bytes 0-3276799/3600000".to_string(), false));
        assert_eq!(ranges[1], ("bytes 3276800-3599999/3600000".to_string(), false));

        // 3. save_draft creates and patches
        let d_new = client
            .save_draft(&OutgoingMail {
                subject: "Draft v1".into(),
                body_html: "<p>v1</p>".into(),
                to: vec!["c@example.com".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(d_new.id, "draft-1");
        assert!(d_new.is_draft);

        let d_upd = client
            .save_draft(&OutgoingMail {
                draft_id: Some("draft-1".into()),
                subject: "Draft v2".into(),
                body_html: "<p>v2</p>".into(),
                to: vec!["c@example.com".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(d_upd.subject, "Draft v2");
        assert!(*rec_check.draft_patched.lock().unwrap());

        // 4. Reply, forward, read/flag, move, delete
        client.reply_mail("m1", "<p>re</p>", false).await.unwrap();
        client.reply_mail("m1", "<p>all</p>", true).await.unwrap();
        client
            .forward_mail("m1", &["fwd@example.com".into()], "<p>fyi</p>")
            .await
            .unwrap();
        client.set_mail_read("m1", true).await.unwrap();
        client.set_mail_flag("m1", true).await.unwrap();
        let moved = client.move_mail_message("m1", "trash").await.unwrap();
        assert_eq!(moved.id, "m1-moved");
        client.delete_mail_message("m1-del").await.unwrap();

        let acts = rec_check.actions.lock().unwrap().clone();
        assert_eq!(acts.len(), 7);
    }

    #[tokio::test]
    async fn test_mail_attachments_download_sniffing_batch_and_remote_search() {
        let png_bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let png_clone = png_bytes.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/messages/m1/attachments/a1",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "a1",
                        "name": "re:po*rt.txt",
                        "contentType": "text/plain",
                        "size": 5,
                        "contentBytes": "aGVsbG8="
                    }))
                }),
            )
            .route(
                "/v1.0/me/messages/m1/attachments/a2",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "a2",
                        "name": "raw.bin",
                        "contentType": "application/octet-stream",
                        "size": 3
                    }))
                }),
            )
            .route(
                "/v1.0/me/messages/m1/attachments/a2/$value",
                get(|| async { vec![1u8, 2, 3] }),
            )
            .route(
                "/v1.0/me/messages/m1/attachments/a3/$value",
                get(move || {
                    let payload = png_clone.clone();
                    async move { payload }
                }),
            )
            .route(
                "/v1.0/me/messages",
                get(|headers: HeaderMap, Query(params): Query<HashMap<String, String>>| async move {
                    assert_eq!(
                        headers
                            .get("consistencylevel")
                            .and_then(|v| v.to_str().ok()),
                        Some("eventual")
                    );
                    let search = params.get("$search").cloned().unwrap_or_default();
                    assert!(search.contains("invoice"));
                    Json(serde_json::json!({
                        "value": [{ "id": "m-hit", "subject": "Invoice #1" }]
                    }))
                }),
            )
            .route(
                "/v1.0/$batch",
                post(|Json(payload): Json<serde_json::Value>| async move {
                    let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                    let responses: Vec<serde_json::Value> = reqs
                        .into_iter()
                        .map(|r| {
                            let req_id = r["id"].as_str().unwrap_or_default().to_string();
                            let url = r["url"].as_str().unwrap_or_default().to_string();
                            let msg_id = url
                                .trim_start_matches("/me/messages/")
                                .split('?')
                                .next()
                                .unwrap_or_default();
                            serde_json::json!({
                                "id": req_id,
                                "status": 200,
                                "body": {
                                    "id": msg_id,
                                    "subject": format!("Full {msg_id}"),
                                    "body": { "contentType": "text", "content": "hydrated" }
                                }
                            })
                        })
                        .collect();
                    Json(serde_json::json!({ "responses": responses }))
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let dir = tempdir().unwrap();

        // 1. Expanded attachment with inline content_bytes_b64
        let expanded_att = MailAttachment {
            id: "a1".into(),
            name: "re:po*rt.txt".into(),
            content_type: "text/plain".into(),
            size: 5,
            is_inline: false,
            content_id: None,
            content_bytes_b64: Some("aGVsbG8=".into()),
        };
        let dl1 = client
            .download_mail_attachment("m1", &expanded_att, dir.path())
            .await
            .unwrap();
        assert_eq!(dl1.file_name, "re_po_rt.txt");
        assert_eq!(std::fs::read(&dl1.path).unwrap(), b"hello");

        let dl2 = client
            .download_mail_attachment("m1", &expanded_att, dir.path())
            .await
            .unwrap();
        assert_eq!(dl2.file_name, "re_po_rt (1).txt");

        // 2. Attachment without extension sniffs PNG from /$value stream
        let no_ext_img = MailAttachment {
            id: "a3".into(),
            name: "inline-image-1".into(),
            content_type: "application/octet-stream".into(),
            size: 8,
            is_inline: true,
            content_id: Some("img1".into()),
            content_bytes_b64: None,
        };
        let dl_img = client
            .download_mail_attachment("m1", &no_ext_img, dir.path())
            .await
            .unwrap();
        assert_eq!(dl_img.file_name, "inline-image-1.png");
        assert_eq!(std::fs::read(&dl_img.path).unwrap(), png_bytes);

        let (name2, ct2, bytes2) = client.fetch_mail_attachment_bytes("m1", "a2").await.unwrap();
        assert_eq!(name2, "raw.bin");
        assert_eq!(ct2, "application/octet-stream");
        assert_eq!(bytes2, vec![1, 2, 3]);

        let hits = client.search_mail_remote("invoice \"2026\"", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m-hit");

        let batch = client
            .batch_get_mail_messages(&["m1".into(), "m2".into()])
            .await
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch[0].body_loaded);
        assert_eq!(batch[0].body_plain, "hydrated");
    }
}
