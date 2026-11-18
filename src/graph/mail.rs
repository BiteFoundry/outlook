//! Microsoft Graph v1.0 Outlook Mail operations (`/me/mailFolders`, `/me/messages`, `/me/sendMail`, attachments, delta).

use std::{path::Path, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use super::{
    client::{parse_retry_after_secs, BatchSubRequest, GraphDeltaPage, GraphListPage},
    media::{deduplicate_dest_path, sanitize_filename},
    GraphClient,
};
use crate::models::{
    DownloadedFile, EmailRecipient, MailAttachment, MailFolder, MailMessage, OutgoingMail,
};

const MAIL_SMALL_ATTACHMENT_LIMIT: u64 = 3 * 1024 * 1024; // 3 MiB
const MAIL_UPLOAD_CHUNK_SIZE: usize = 3_276_800; // 3.2 MiB
const MAX_THROTTLE_RETRIES: usize = 3;

const MAIL_HEADER_SELECT_FIELDS: &str = "id,conversationId,parentFolderId,subject,from,toRecipients,ccRecipients,bccRecipients,receivedDateTime,sentDateTime,isRead,isDraft,flag,importance,hasAttachments,bodyPreview,webLink";
const MAIL_FULL_SELECT_FIELDS: &str = "id,conversationId,parentFolderId,subject,from,toRecipients,ccRecipients,bccRecipients,receivedDateTime,sentDateTime,isRead,isDraft,flag,importance,hasAttachments,bodyPreview,body,webLink";

// ---------------------------------------------------------------------------
// Shared Graph Outlook Wire DTOs & Request Helpers
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
        let escaped = crate::parser::md_out::escape_html(raw_content);
        return (escaped, raw_content.to_string());
    }

    let body_html = raw_content.to_string();
    let plain = crate::parser::html_and_cards_to_plain(&body_html, &[]);
    let body_plain = if plain.trim().is_empty() {
        preview.to_string()
    } else {
        plain
    };
    (body_html, body_plain)
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
            content_id: att.content_id.filter(|s| !s.trim().is_empty()),
            content_bytes_b64: att.content_bytes.filter(|s| !s.trim().is_empty()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphMailMessage {
    #[serde(default)]
    id: String,
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
}

impl From<GraphMailMessage> for MailMessage {
    fn from(raw: GraphMailMessage) -> Self {
        let preview = raw.body_preview.unwrap_or_default();
        let (body_html, body_plain) = normalize_outlook_body(raw.body.as_ref(), &preview);
        let flagged = raw
            .flag
            .and_then(|f| f.flag_status)
            .map(|s| s.eq_ignore_ascii_case("flagged"))
            .unwrap_or(false);

        Self {
            id: raw.id,
            conversation_id: raw.conversation_id.filter(|s| !s.trim().is_empty()),
            folder_id: raw.parent_folder_id.filter(|s| !s.trim().is_empty()),
            subject: raw.subject.unwrap_or_default(),
            from: raw.from.and_then(GraphRecipient::into_email_recipient),
            to_recipients: raw
                .to_recipients
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            cc_recipients: raw
                .cc_recipients
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            bcc_recipients: raw
                .bcc_recipients
                .into_iter()
                .filter_map(GraphRecipient::into_email_recipient)
                .collect(),
            received_at: raw.received_date_time.unwrap_or_default(),
            sent_at: raw.sent_date_time.filter(|s| !s.trim().is_empty()),
            is_read: raw.is_read.unwrap_or(false),
            is_draft: raw.is_draft.unwrap_or(false),
            flagged,
            importance: raw
                .importance
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "normal".to_string()),
            has_attachments: raw.has_attachments.unwrap_or(false),
            preview,
            body_html,
            body_plain,
            attachments: raw.attachments.into_iter().map(Into::into).collect(),
            web_link: raw.web_link.filter(|s| !s.trim().is_empty()),
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

impl GraphClient {
    /// Executes a Graph GET request with extra headers, including 401 refresh-retry and 429/503 Retry-After backoff.
    pub(crate) async fn get_json_with_headers<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<T> {
        let resp = self
            .execute_custom_with_retry(Method::GET, path_or_url, None, extra_headers)
            .await?;
        let url = self.resolve_url(path_or_url);
        let bytes = resp.bytes().await.context("Graph 응답 바디 읽기 실패")?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Graph JSON 역직렬화 실패 ({url})"))
    }

    /// Executes a Graph PATCH request with JSON body and deserializes an optional JSON response (returning `None` on empty/204 body).
    pub(crate) async fn patch_json_optional<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<Option<T>> {
        let json_val = serde_json::to_value(body)?;
        let resp = self
            .execute_custom_with_retry(Method::PATCH, path_or_url, Some(json_val), &[])
            .await?;
        let url = self.resolve_url(path_or_url);
        let bytes = resp.bytes().await.context("Graph PATCH 응답 바디 읽기 실패")?;
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(None);
        }
        let parsed = serde_json::from_slice(&bytes)
            .with_context(|| format!("Graph PATCH JSON 역직렬화 실패 ({url})"))?;
        Ok(Some(parsed))
    }

    async fn execute_custom_with_retry(
        &self,
        method: Method,
        path_or_url: &str,
        json_body: Option<serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let url = self.resolve_url(path_or_url);
        let mut unauthorized_retried = false;
        let mut throttle_attempt = 0usize;

        loop {
            let token = self.auth().valid_access_token().await?;
            let mut req = self.http().request(method.clone(), &url).bearer_auth(&token);

            for (name, val) in extra_headers {
                req = req.header(*name, *val);
            }
            if let Some(ref val) = json_body {
                req = req.json(val);
            }

            let resp = req
                .send()
                .await
                .with_context(|| format!("Graph HTTP 요청 실패 ({method} {url})"))?;
            let status = resp.status();

            if status == StatusCode::UNAUTHORIZED
                && !unauthorized_retried
                && self.auth().is_refreshable().await
            {
                unauthorized_retried = true;
                self.auth().refresh_for_failed_token(&token).await?;
                continue;
            }

            if (status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE)
                && throttle_attempt < MAX_THROTTLE_RETRIES
            {
                let delay_secs = parse_retry_after_secs(resp.headers(), throttle_attempt);
                throttle_attempt += 1;
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                continue;
            }

            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                let detail = format_custom_graph_error(status, &err_body);
                bail!("Graph API 오류 ({method} {url}): {detail}");
            }

            return Ok(resp);
        }
    }

    pub async fn list_mail_folders(&self) -> Result<Vec<MailFolder>> {
        let mut next_url = Some(
            "/me/mailFolders?$top=100&$select=id,displayName,parentFolderId,unreadItemCount,totalItemCount"
                .to_string(),
        );
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
                    unread_item_count: raw.unread_item_count,
                    total_item_count: raw.total_item_count,
                });
            }
            next_url = page.next_link;
        }

        Ok(folders)
    }

    pub async fn list_mail_messages(
        &self,
        folder_id_or_wellknown: &str,
        limit: usize,
    ) -> Result<Vec<MailMessage>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let top = limit.min(100);
        let base_path = match normalize_folder_target(folder_id_or_wellknown) {
            None => "/me/messages".to_string(),
            Some(folder) => format!("/me/mailFolders/{}/messages", urlencoding::encode(&folder)),
        };
        let mut next_url = Some(format!(
            "{base_path}?$top={top}&$orderby=receivedDateTime desc&$select={MAIL_HEADER_SELECT_FIELDS}"
        ));
        let mut messages = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphMailMessage> = self.get_json(&url).await?;
            for raw in page.value {
                messages.push(MailMessage::from(raw));
                if messages.len() >= limit {
                    return Ok(messages);
                }
            }
            next_url = page.next_link;
        }

        Ok(messages)
    }

    pub async fn delta_mail_messages(
        &self,
        folder_id_or_wellknown: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<MailMessage>, Vec<String>, Option<String>)> {
        let initial_url = match delta_link.map(str::trim).filter(|s| !s.is_empty()) {
            Some(link) => link.to_string(),
            None => {
                let base_path = match normalize_folder_target(folder_id_or_wellknown) {
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

        let mut next_url = Some(initial_url);
        let mut upserted = Vec::new();
        let mut removed_ids = Vec::new();
        let mut new_delta_link = None;

        while let Some(url) = next_url {
            let page: GraphDeltaPage<serde_json::Value> = self.get_json(&url).await?;
            for val in page.value {
                if val.get("@removed").is_some() {
                    if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                        removed_ids.push(id.to_string());
                    }
                } else {
                    let raw: GraphMailMessage = serde_json::from_value(val)
                        .context("Graph 메일 delta 항목 역직렬화 실패")?;
                    upserted.push(MailMessage::from(raw));
                }
            }
            if page.delta_link.is_some() {
                new_delta_link = page.delta_link;
            }
            next_url = page.next_link;
        }

        Ok((upserted, removed_ids, new_delta_link))
    }

    pub async fn get_mail_message(&self, message_id: &str) -> Result<MailMessage> {
        let path = format!(
            "/me/messages/{}?$expand=attachments",
            urlencoding::encode(message_id.trim())
        );
        let raw: GraphMailMessage = self.get_json(&path).await?;
        Ok(MailMessage::from(raw))
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
                by_idx.insert(idx, MailMessage::from(raw));
            }
        }

        Ok(by_idx.into_values().collect())
    }

    pub async fn send_mail(&self, mail: &OutgoingMail) -> Result<()> {
        struct StagedFile<'a> {
            path: &'a Path,
            name: String,
            mime: &'static str,
            size: u64,
        }

        let mut staged = Vec::with_capacity(mail.attachments.len());
        let mut total_size: u64 = 0;
        let mut any_over_limit = false;

        for path in &mail.attachments {
            let meta = tokio::fs::metadata(path)
                .await
                .with_context(|| format!("첨부파일을 찾을 수 없습니다: {}", path.display()))?;
            if !meta.is_file() {
                bail!("일반 파일만 첨부할 수 있습니다: {}", path.display());
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
                .ok_or_else(|| anyhow!("유효한 파일 이름이 아닙니다: {}", path.display()))?
                .to_string();
            let mime = guess_mime_type(path);
            staged.push(StagedFile {
                path,
                name,
                mime,
                size,
            });
        }

        let use_draft_flow = !staged.is_empty()
            && (any_over_limit || total_size > MAIL_SMALL_ATTACHMENT_LIMIT);

        let mut message = build_outgoing_message_json(mail);

        if !use_draft_flow {
            if !staged.is_empty() {
                let mut atts = Vec::with_capacity(staged.len());
                for f in staged {
                    let bytes = tokio::fs::read(f.path)
                        .await
                        .with_context(|| format!("첨부파일 읽기 실패: {}", f.path.display()))?;
                    atts.push(serde_json::json!({
                        "@odata.type": "#microsoft.graph.fileAttachment",
                        "name": f.name,
                        "contentType": f.mime,
                        "contentBytes": BASE64_STANDARD.encode(&bytes),
                    }));
                }
                message["attachments"] = serde_json::Value::Array(atts);
            }

            let payload = serde_json::json!({
                "message": message,
                "saveToSentItems": mail.save_to_sent_items,
            });
            return self.post_json_empty("/me/sendMail", &payload).await;
        }

        message["isDraft"] = serde_json::Value::Bool(true);
        let draft: CreatedDraftResponse = self.post_json("/me/messages", &message).await?;
        let draft_id_enc = urlencoding::encode(&draft.id);

        for f in staged {
            if f.size <= MAIL_SMALL_ATTACHMENT_LIMIT {
                let bytes = tokio::fs::read(f.path)
                    .await
                    .with_context(|| format!("첨부파일 읽기 실패: {}", f.path.display()))?;
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
                .await?;
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
                    format!("대용량 첨부파일 열기 실패: {}", f.path.display())
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
                        bail!("메일 첨부파일 업로드 중 예상치 못한 EOF가 발생했습니다.");
                    }
                    buf.truncate(filled);
                    let end_inclusive = offset + (filled as u64) - 1;
                    self.put_upload_chunk(&session.upload_url, buf, offset, end_inclusive, f.size)
                        .await?;
                    offset += filled as u64;
                }
            }
        }

        self.post_empty(&format!("/me/messages/{draft_id_enc}/send"))
            .await
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
        to_addresses: &[String],
        comment_html: &str,
    ) -> Result<()> {
        let recipients = recipients_to_json(to_addresses);
        if recipients.is_empty() {
            bail!("전달할 수신자 이메일 주소를 1개 이상 지정해야 합니다.");
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

    pub async fn move_mail_message(
        &self,
        message_id: &str,
        destination_folder: &str,
    ) -> Result<MailMessage> {
        let dest = normalize_folder_target(destination_folder)
            .ok_or_else(|| anyhow!("이동할 대상 폴더 ID를 지정해야 합니다."))?;
        let path = format!(
            "/me/messages/{}/move",
            urlencoding::encode(message_id.trim())
        );
        let moved: GraphMailMessage = self
            .post_json(&path, &serde_json::json!({ "destinationId": dest }))
            .await?;
        Ok(MailMessage::from(moved))
    }

    pub async fn delete_mail_message(&self, message_id: &str) -> Result<()> {
        let path = format!("/me/messages/{}", urlencoding::encode(message_id.trim()));
        self.delete_req(&path).await
    }

    pub async fn fetch_mail_attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let msg_enc = urlencoding::encode(message_id.trim());
        let att_enc = urlencoding::encode(attachment_id.trim());
        let meta_path = format!("/me/messages/{msg_enc}/attachments/{att_enc}");
        let meta: GraphMailAttachment = self.get_json(&meta_path).await?;
        let name = meta
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("attachment_{attachment_id}"));
        let content_type = meta
            .content_type
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let bytes = if let Some(b64) = meta.content_bytes.filter(|s| !s.trim().is_empty()) {
            let compact: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            BASE64_STANDARD
                .decode(compact.as_bytes())
                .context("메일 첨부파일 base64 디코딩 실패")?
        } else {
            let value_path = format!("/me/messages/{msg_enc}/attachments/{att_enc}/$value");
            self.get_bytes(&value_path).await?
        };

        Ok((name, content_type, bytes))
    }

    pub async fn download_mail_attachment(
        &self,
        message_id: &str,
        attachment_id: &str,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        let (raw_name, _content_type, bytes) = self
            .fetch_mail_attachment_bytes(message_id, attachment_id)
            .await?;

        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("다운로드 디렉터리 생성 실패: {}", dest_dir.display()))?;

        let safe_name = sanitize_filename(&raw_name);
        let dest_path = deduplicate_dest_path(dest_dir, &safe_name);
        let final_name = dest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        tokio::fs::write(&dest_path, &bytes)
            .await
            .with_context(|| format!("메일 첨부파일 저장 실패: {}", dest_path.display()))?;

        Ok(DownloadedFile {
            path: dest_path,
            file_name: final_name,
            bytes_written: bytes.len() as u64,
        })
    }

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
                messages.push(MailMessage::from(raw));
                if messages.len() >= limit {
                    return Ok(messages);
                }
            }
            next_url = page.next_link;
        }

        Ok(messages)
    }
}

fn derive_well_known_folder_name(name: &str) -> Option<String> {
    let normalized = name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "inbox" | "받은 편지함" | "받은편지함" => Some("inbox".to_string()),
        "sentitems" | "sent items" | "sent" | "보낸 편지함" | "보낸편지함" => {
            Some("sentitems".to_string())
        }
        "drafts" | "임시 보관함" | "임시보관함" => Some("drafts".to_string()),
        "archive" | "보관처리" | "보관" | "보관함" => Some("archive".to_string()),
        "deleteditems" | "deleted items" | "지운 편지함" | "지운편지함" | "휴지통" => {
            Some("deleteditems".to_string())
        }
        "junkemail" | "junk email" | "junk" | "정크 메일" | "정크메일" => {
            Some("junkemail".to_string())
        }
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
    let mut message = serde_json::json!({
        "subject": mail.subject,
        "body": {
            "contentType": "HTML",
            "content": mail.body_html,
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

fn guess_mime_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("log") => "text/plain",
        Some("csv") => "text/csv",
        Some("html") | Some("htm") => "text/html",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn format_custom_graph_error(status: StatusCode, body: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err) = val.get("error") {
            let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if !code.is_empty() || !msg.is_empty() {
                return format!("{status} [{code}] {msg}");
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        status.to_string()
    } else {
        format!("{status} {trimmed}")
    }
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
    async fn test_list_mail_folders_pagination_and_korean_well_known_names() {
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
                                { "id": "f4", "displayName": "보관", "unreadItemCount": 0, "totalItemCount": 5 },
                                { "id": "f5", "displayName": "휴지통", "unreadItemCount": 1, "totalItemCount": 3 },
                                { "id": "f6", "displayName": "정크 메일", "unreadItemCount": 2, "totalItemCount": 2 },
                                { "id": "f7", "displayName": "프로젝트", "wellKnownName": "custom", "unreadItemCount": 0, "totalItemCount": 1 }
                            ]
                        }))
                    } else {
                        let base = st.base_url.lock().unwrap().clone();
                        Json(serde_json::json!({
                            "value": [
                                { "id": "f1", "displayName": "받은 편지함", "unreadItemCount": 4, "totalItemCount": 20 },
                                { "id": "f2", "displayName": "보낸편지함", "unreadItemCount": 0, "totalItemCount": 10 },
                                { "id": "f3", "displayName": "임시 보관함", "unreadItemCount": 1, "totalItemCount": 2 }
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
        assert_eq!(folders[1].well_known_name.as_deref(), Some("sentitems"));
        assert_eq!(folders[2].well_known_name.as_deref(), Some("drafts"));
        assert_eq!(folders[3].well_known_name.as_deref(), Some("archive"));
        assert_eq!(folders[4].well_known_name.as_deref(), Some("deleteditems"));
        assert_eq!(folders[5].well_known_name.as_deref(), Some("junkemail"));
        assert_eq!(folders[6].well_known_name.as_deref(), Some("custom"));
    }

    #[tokio::test]
    async fn test_list_delta_and_get_mail_messages() {
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
                get(|AxumPath(folder): AxumPath<String>| async move {
                    assert_eq!(folder, "sentitems");
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "m1",
                                "subject": "HTML 메일",
                                "from": { "emailAddress": { "name": "김철수", "address": "kim@example.com" } },
                                "toRecipients": [{ "emailAddress": { "address": "lee@example.com" } }],
                                "receivedDateTime": "2026-11-15T10:00:00Z",
                                "isRead": true,
                                "flag": { "flagStatus": "flagged" },
                                "importance": "high",
                                "hasAttachments": false,
                                "bodyPreview": "미리보기",
                                "body": { "contentType": "html", "content": "<p>안녕 <b>세계</b></p>" }
                            },
                            {
                                "id": "m2",
                                "subject": "텍스트 메일",
                                "receivedDateTime": "2026-11-15T09:00:00Z",
                                "bodyPreview": "preview2",
                                "body": { "contentType": "text", "content": "a < b & c" }
                            },
                            {
                                "id": "m3",
                                "subject": "빈 본문",
                                "receivedDateTime": "2026-11-15T08:00:00Z",
                                "bodyPreview": "프리뷰 폴백",
                                "body": { "contentType": "html", "content": "" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "m-all", "subject": "전체" }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/mailFolders/{folder}/messages/delta",
                get(|AxumPath(folder): AxumPath<String>, State(st): State<ServerState>, Query(params): Query<HashMap<String, String>>| async move {
                    assert_eq!(folder, "inbox");
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
                                { "id": "m-up", "subject": "신규 메일", "bodyPreview": "hi", "body": { "contentType": "text", "content": "hi" } }
                            ],
                            "@odata.nextLink": format!("{base}/v1.0/me/mailFolders/inbox/messages/delta?page=2")
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/messages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    assert_eq!(id, "m1");
                    Json(serde_json::json!({
                        "id": "m1",
                        "subject": "첨부 포함",
                        "hasAttachments": true,
                        "attachments": [
                            {
                                "id": "att1",
                                "name": "doc.txt",
                                "contentType": "text/plain",
                                "size": 5,
                                "isInline": false,
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

        let sent = client.list_mail_messages("sent", 10).await.unwrap();
        assert_eq!(sent.len(), 3);
        assert!(sent[0].flagged);
        assert!(sent[0].body_plain.contains("안녕 세계"));
        assert_eq!(sent[1].body_plain, "a < b & c");
        assert_eq!(sent[1].body_html, "a &lt; b &amp; c");
        assert_eq!(sent[2].body_plain, "프리뷰 폴백");

        let all = client.list_mail_messages("all", 5).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "m-all");

        let (upserted, removed, dlink) = client.delta_mail_messages("inbox", None).await.unwrap();
        assert_eq!(upserted.len(), 1);
        assert_eq!(upserted[0].id, "m-up");
        assert_eq!(removed, vec!["m-del"]);
        assert!(dlink.unwrap().contains("$deltatoken=tok2"));

        let detail = client.get_mail_message("m1").await.unwrap();
        assert_eq!(detail.attachments.len(), 1);
        assert_eq!(detail.attachments[0].name, "doc.txt");
        assert_eq!(
            detail.attachments[0].content_bytes_b64.as_deref(),
            Some("aGVsbG8=")
        );
    }

    #[tokio::test]
    async fn test_send_mail_small_large_and_actions() {
        #[derive(Clone, Default)]
        struct Recorded {
            send_mail_payloads: Arc<Mutex<Vec<serde_json::Value>>>,
            draft_created: Arc<Mutex<bool>>,
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
                    Json(serde_json::json!({ "id": "draft-1" }))
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

        client
            .send_mail(&OutgoingMail {
                subject: "Small".into(),
                body_html: "<p>hi</p>".into(),
                to: vec!["a@example.com".into()],
                cc: vec![],
                bcc: vec![],
                importance: Some("high".into()),
                attachments: vec![small_path.clone()],
                save_to_sent_items: true,
            })
            .await
            .unwrap();

        assert_eq!(rec_check.send_mail_payloads.lock().unwrap().len(), 1);
        let p0 = &rec_check.send_mail_payloads.lock().unwrap()[0];
        assert_eq!(
            p0["message"]["attachments"][0]["contentType"],
            "text/plain"
        );

        let large_path = dir.path().join("big.bin");
        let big_bytes = vec![0xABu8; 3_600_000];
        std::fs::write(&large_path, &big_bytes).unwrap();

        client
            .send_mail(&OutgoingMail {
                subject: "Large".into(),
                body_html: "<p>big</p>".into(),
                to: vec!["b@example.com".into()],
                cc: vec![],
                bcc: vec![],
                importance: None,
                attachments: vec![small_path, large_path],
                save_to_sent_items: false,
            })
            .await
            .unwrap();

        assert!(*rec_check.draft_created.lock().unwrap());
        assert!(*rec_check.small_att_uploaded.lock().unwrap());
        assert!(*rec_check.draft_sent.lock().unwrap());
        let ranges = rec_check.chunk_ranges.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], ("bytes 0-3276799/3600000".to_string(), false));
        assert_eq!(
            ranges[1],
            ("bytes 3276800-3599999/3600000".to_string(), false)
        );

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
    async fn test_mail_attachments_download_and_remote_search() {
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
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let dir = tempdir().unwrap();

        let dl1 = client
            .download_mail_attachment("m1", "a1", dir.path())
            .await
            .unwrap();
        assert_eq!(dl1.file_name, "re_po_rt.txt");
        assert_eq!(std::fs::read(&dl1.path).unwrap(), b"hello");

        let dl2 = client
            .download_mail_attachment("m1", "a1", dir.path())
            .await
            .unwrap();
        assert_eq!(dl2.file_name, "re_po_rt (1).txt");

        let (name2, ct2, bytes2) = client.fetch_mail_attachment_bytes("m1", "a2").await.unwrap();
        assert_eq!(name2, "raw.bin");
        assert_eq!(ct2, "application/octet-stream");
        assert_eq!(bytes2, vec![1, 2, 3]);

        let hits = client.search_mail_remote("invoice \"2026\"", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m-hit");
    }

    #[test]
    fn test_mail_select_fields_split() {
        let header_fields: Vec<&str> = MAIL_HEADER_SELECT_FIELDS.split(',').collect();
        let full_fields: Vec<&str> = MAIL_FULL_SELECT_FIELDS.split(',').collect();
        assert!(!header_fields.contains(&"body"));
        assert!(header_fields.contains(&"bodyPreview"));
        assert!(full_fields.contains(&"body"));
        assert!(full_fields.contains(&"bodyPreview"));
    }

    #[tokio::test]
    async fn test_header_first_list_delta_and_batch_get_mail_messages() {
        #[derive(Clone, Default)]
        struct BatchState {
            list_selects: Arc<Mutex<Vec<String>>>,
            delta_selects: Arc<Mutex<Vec<String>>>,
            batch_calls: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
        }
        let st = BatchState::default();
        let st_check = st.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders/{folder}/messages",
                get(
                    |State(s): State<BatchState>, Query(params): Query<HashMap<String, String>>| async move {
                        s.list_selects
                            .lock()
                            .unwrap()
                            .push(params.get("$select").cloned().unwrap_or_default());
                        Json(serde_json::json!({
                            "value": [{
                                "id": "m-hdr-1",
                                "subject": "Header Only",
                                "bodyPreview": "Short preview text"
                            }]
                        }))
                    },
                ),
            )
            .route(
                "/v1.0/me/mailFolders/{folder}/messages/delta",
                get(
                    |State(s): State<BatchState>, Query(params): Query<HashMap<String, String>>| async move {
                        s.delta_selects
                            .lock()
                            .unwrap()
                            .push(params.get("$select").cloned().unwrap_or_default());
                        Json(serde_json::json!({
                            "value": [{
                                "id": "m-hdr-2",
                                "subject": "Delta Header",
                                "bodyPreview": "Delta preview"
                            }],
                            "@odata.deltaLink": "http://example.invalid/delta?tok=1"
                        }))
                    },
                ),
            )
            .route(
                "/v1.0/$batch",
                post(
                    |State(s): State<BatchState>, Json(payload): Json<serde_json::Value>| async move {
                        let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                        s.batch_calls.lock().unwrap().push(reqs.clone());
                        let responses: Vec<serde_json::Value> = reqs
                            .into_iter()
                            .map(|r| {
                                let req_id = r["id"].as_str().unwrap_or_default().to_string();
                                let url = r["url"].as_str().unwrap_or_default().to_string();
                                if url.contains("/me/messages/missing-404?") {
                                    serde_json::json!({
                                        "id": req_id,
                                        "status": 404,
                                        "body": { "error": { "code": "ErrorItemNotFound" } }
                                    })
                                } else {
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
                                            "bodyPreview": "Preview",
                                            "body": {
                                                "contentType": "html",
                                                "content": format!("<p>Body for {msg_id}</p>")
                                            },
                                            "attachments": [{
                                                "id": "att-1",
                                                "name": "info.txt",
                                                "contentType": "text/plain",
                                                "size": 4
                                            }]
                                        }
                                    })
                                }
                            })
                            .collect();
                        Json(serde_json::json!({ "responses": responses }))
                    },
                ),
            )
            .with_state(st);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));

        let listed = client.list_mail_messages("inbox", 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].body_html, "");
        assert_eq!(listed[0].body_plain, "Short preview text");

        let (delta_up, _, _) = client.delta_mail_messages("inbox", None).await.unwrap();
        assert_eq!(delta_up.len(), 1);
        assert_eq!(delta_up[0].body_html, "");
        assert_eq!(delta_up[0].body_plain, "Delta preview");

        let list_sels = st_check.list_selects.lock().unwrap().clone();
        let delta_sels = st_check.delta_selects.lock().unwrap().clone();
        assert_eq!(list_sels, vec![MAIL_HEADER_SELECT_FIELDS]);
        assert_eq!(delta_sels, vec![MAIL_HEADER_SELECT_FIELDS]);

        // Empty batch returns empty without HTTP call
        assert!(client.batch_get_mail_messages(&[]).await.unwrap().is_empty());

        // 22 unique IDs (plus duplicates, blanks, and one 404) -> 2 $batch calls (20 + 2)
        let mut ids: Vec<String> = (0..21).map(|i| format!("msg-{i}")).collect();
        ids.push("  msg-0  ".to_string()); // duplicate after trim
        ids.push("   ".to_string()); // blank
        ids.push("missing-404".to_string());

        let fetched = client.batch_get_mail_messages(&ids).await.unwrap();
        assert_eq!(fetched.len(), 21);
        assert_eq!(fetched[0].id, "msg-0");
        assert_eq!(fetched[0].body_html, "<p>Body for msg-0</p>");
        assert_eq!(fetched[0].body_plain, "Body for msg-0");
        assert_eq!(fetched[0].attachments.len(), 1);
        assert_eq!(fetched[20].id, "msg-20");

        let calls = st_check.batch_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].len(), 20);
        assert_eq!(calls[1].len(), 2);
        let first_url = calls[0][0]["url"].as_str().unwrap();
        assert!(first_url.contains(&format!("$select={MAIL_FULL_SELECT_FIELDS}")));
        assert!(first_url.contains("$expand=attachments"));
    }
}
