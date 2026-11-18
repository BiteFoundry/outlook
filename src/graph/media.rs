//! Microsoft Graph v1.0 file upload (OneDrive / SharePoint + chunked upload sessions)
//! and attachment / hosted image download helpers.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::GraphClient;
use crate::models::{
    AttachmentInfo, ConversationTarget, DownloadedFile, MessageItem, OutgoingMessage,
    UploadedReferenceAttachment,
};

const SMALL_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024; // 4 MiB
const UPLOAD_CHUNK_SIZE: usize = 3_276_800; // 3.2 MiB (10 * 320 KiB)
const MAX_IN_MEMORY_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

#[derive(Debug, Deserialize)]
struct DriveParentReference {
    #[serde(rename = "driveId", default)]
    drive_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DriveItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(rename = "eTag", default)]
    e_tag: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "webDavUrl", default)]
    web_dav_url: Option<String>,
    #[serde(rename = "parentReference", default)]
    parent_reference: Option<DriveParentReference>,
}

#[derive(Debug, Deserialize)]
struct UploadSessionResponse {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct SharingLinkInfo {
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateLinkResponse {
    #[serde(default)]
    link: Option<SharingLinkInfo>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
}

impl GraphClient {
    /// Uploads a local file to the appropriate backing store:
    /// - `Chat` -> `/me/drive/root:/Microsoft Teams Chat Files/{name}:/content` (or upload session > 4 MiB) + sharing link
    /// - `Channel` -> `/teams/{id}/channels/{id}/filesFolder` -> `/drives/{drive_id}/items/{folder_id}:/{name}:/content`
    pub async fn upload_reference_file(
        &self,
        target: &ConversationTarget,
        local_path: &Path,
    ) -> Result<UploadedReferenceAttachment> {
        let meta = tokio::fs::metadata(local_path)
            .await
            .with_context(|| format!("File to upload not found: {}", local_path.display()))?;
        if !meta.is_file() {
            bail!("Only regular files can be uploaded: {}", local_path.display());
        }
        let size = meta.len();
        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Invalid file name: {}", local_path.display()))?
            .to_string();
        let encoded_name = urlencoding::encode(&file_name);

        match target {
            ConversationTarget::Chat { .. } => {
                let content_path = format!(
                    "/me/drive/root:/Microsoft%20Teams%20Chat%20Files/{encoded_name}:/content"
                );
                let session_path = format!(
                    "/me/drive/root:/Microsoft%20Teams%20Chat%20Files/{encoded_name}:/createUploadSession"
                );
                let drive_item = self
                    .upload_drive_item(local_path, &file_name, size, &content_path, &session_path)
                    .await?;

                let att_id = extract_etag_guid(drive_item.e_tag.as_deref());
                let link_path = format!(
                    "/me/drive/items/{}/createLink",
                    urlencoding::encode(&drive_item.id)
                );
                let link_body = serde_json::json!({
                    "type": "view",
                    "scope": "organization"
                });
                let shared_url = match self.post_json::<_, CreateLinkResponse>(&link_path, &link_body).await {
                    Ok(resp) => resp
                        .link
                        .and_then(|l| l.web_url)
                        .or(resp.web_url)
                        .filter(|s| !s.trim().is_empty()),
                    Err(_) => None,
                };

                let content_url = shared_url
                    .or(drive_item.web_dav_url)
                    .or(drive_item.web_url)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Failed to obtain share URL for uploaded file."))?;

                Ok(UploadedReferenceAttachment {
                    id: att_id,
                    name: drive_item.name.unwrap_or(file_name),
                    content_url,
                    size: drive_item.size.unwrap_or(size),
                    drive_item_id: Some(drive_item.id),
                })
            }
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => {
                let folder_path = format!(
                    "/teams/{}/channels/{}/filesFolder",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id)
                );
                let folder: DriveItem = self.get_json(&folder_path).await?;
                let drive_id = folder
                    .parent_reference
                    .and_then(|p| p.drive_id)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing driveId on channel filesFolder."))?;

                let content_path = format!(
                    "/drives/{}/items/{}:/{encoded_name}:/content",
                    urlencoding::encode(&drive_id),
                    urlencoding::encode(&folder.id)
                );
                let session_path = format!(
                    "/drives/{}/items/{}:/{encoded_name}:/createUploadSession",
                    urlencoding::encode(&drive_id),
                    urlencoding::encode(&folder.id)
                );
                let drive_item = self
                    .upload_drive_item(local_path, &file_name, size, &content_path, &session_path)
                    .await?;

                let att_id = extract_etag_guid(drive_item.e_tag.as_deref());
                let content_url = drive_item
                    .web_dav_url
                    .or(drive_item.web_url)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing webDavUrl/webUrl for uploaded channel file."))?;

                Ok(UploadedReferenceAttachment {
                    id: att_id,
                    name: drive_item.name.unwrap_or(file_name),
                    content_url,
                    size: drive_item.size.unwrap_or(size),
                    drive_item_id: Some(drive_item.id),
                })
            }
        }
    }

    async fn upload_drive_item(
        &self,
        local_path: &Path,
        file_name: &str,
        size: u64,
        content_path: &str,
        session_path: &str,
    ) -> Result<DriveItem> {
        if size <= SMALL_UPLOAD_LIMIT {
            let bytes = tokio::fs::read(local_path)
                .await
                .with_context(|| format!("Failed to read file: {}", local_path.display()))?;
            return self
                .put_bytes(content_path, bytes, Some("application/octet-stream"))
                .await;
        }

        let session_req = serde_json::json!({
            "item": {
                "@microsoft.graph.conflictBehavior": "rename",
                "name": file_name
            }
        });
        let session: UploadSessionResponse = self.post_json(session_path, &session_req).await?;

        let mut file = tokio::fs::File::open(local_path)
            .await
            .with_context(|| format!("Failed to open large file: {}", local_path.display()))?;
        let mut offset: u64 = 0;
        let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];

        loop {
            let mut filled = 0usize;
            while filled < UPLOAD_CHUNK_SIZE {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                bail!("Upload session reached EOF without a completion response.");
            }

            let end_inclusive = offset + (filled as u64) - 1;
            let (_status, val) = self
                .put_upload_chunk(&session.upload_url, &buf[..filled], offset, end_inclusive, size)
                .await?;
            offset += filled as u64;

            if val.get("id").is_some() {
                let item: DriveItem = serde_json::from_value(val)
                    .context("Failed to deserialize DriveItem on upload session completion")?;
                return Ok(item);
            }
            if offset >= size {
                bail!("Did not receive DriveItem metadata after upload completed.");
            }
        }
    }

    /// High-level helper: uploads `local_path` and sends it with an optional caption (and optional `thread_root_id` in channels).
    pub async fn send_file(
        &self,
        target: &ConversationTarget,
        local_path: &Path,
        caption_html: Option<&str>,
        thread_root_id: Option<&str>,
    ) -> Result<MessageItem> {
        let att = self.upload_reference_file(target, local_path).await?;
        let body_html = match caption_html.map(str::trim).filter(|s| !s.is_empty()) {
            Some(caption) => format!("{caption}<br><attachment id=\"{}\"></attachment>", att.id),
            None => format!("<attachment id=\"{}\"></attachment>", att.id),
        };
        let outgoing = OutgoingMessage {
            body_html,
            attachments: vec![att],
            ..Default::default()
        };
        if let Some(root_id) = thread_root_id.map(str::trim).filter(|s| !s.is_empty()) {
            self.reply_message(target, root_id, &outgoing).await
        } else {
            self.send_message(target, &outgoing).await
        }
    }

    /// Downloads a reference attachment (via Graph `/shares/u!.../driveItem/content` or direct stream)
    /// or hosted inline content into `dest_dir`.
    pub async fn download_attachment(
        &self,
        attachment: &AttachmentInfo,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        let content_url = attachment
            .content_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Attachment has no content_url to download."))?;

        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("Failed to create download directory: {}", dest_dir.display()))?;

        let raw_name = attachment
            .name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| filename_from_url(content_url))
            .unwrap_or_else(|| format!("attachment_{}", attachment.id));

        let safe_name = sanitize_filename(&raw_name);
        let dest_path = deduplicate_dest_path(dest_dir, &safe_name);
        let final_name = dest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        let is_hosted = content_url.contains("/hostedContents/")
            || attachment.content_type.eq_ignore_ascii_case("hostedContent");
        let fallback_bearer = should_send_bearer(&self.config.graph_base_url, content_url);

        let bytes_written = if is_hosted {
            self.get_to_file(content_url, &dest_path, fallback_bearer)
                .await?
        } else {
            let share_token = encode_graph_share_token(content_url);
            let share_path = format!("/shares/{share_token}/driveItem/content");
            match self.get_to_file(&share_path, &dest_path, true).await {
                Ok(n) => n,
                Err(err) if is_auth_or_forbidden_error(&err) => return Err(err),
                Err(_) => {
                    self.get_to_file(content_url, &dest_path, fallback_bearer)
                        .await?
                }
            }
        };

        Ok(DownloadedFile {
            path: dest_path,
            file_name: final_name,
            bytes_written,
        })
    }

    pub async fn download_hosted_bytes(&self, hosted_url: &str) -> Result<Vec<u8>> {
        let use_bearer = should_send_bearer(&self.config.graph_base_url, hosted_url);
        self.get_bytes_with_limit(hosted_url, use_bearer, MAX_IN_MEMORY_ATTACHMENT_BYTES)
            .await
    }

    /// Downloads raw attachment bytes in memory (for inline terminal image preview) along with `(file_name, content_type, bytes)`.
    pub async fn fetch_attachment_bytes(
        &self,
        attachment: &AttachmentInfo,
    ) -> Result<(String, String, Vec<u8>)> {
        let content_url = attachment
            .content_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Attachment has no content_url to download."))?;
        let raw_name = attachment
            .name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| filename_from_url(content_url))
            .unwrap_or_else(|| format!("attachment_{}", attachment.id));
        let is_hosted = content_url.contains("/hostedContents/")
            || attachment.content_type.eq_ignore_ascii_case("hostedContent");
        let fallback_bearer = should_send_bearer(&self.config.graph_base_url, content_url);
        let bytes = if is_hosted {
            self.get_bytes_with_limit(content_url, fallback_bearer, MAX_IN_MEMORY_ATTACHMENT_BYTES)
                .await?
        } else {
            let share_token = encode_graph_share_token(content_url);
            let share_path = format!("/shares/{share_token}/driveItem/content");
            match self
                .get_bytes_with_limit(&share_path, true, MAX_IN_MEMORY_ATTACHMENT_BYTES)
                .await
            {
                Ok(b) => b,
                Err(err) if is_auth_or_forbidden_error(&err) => return Err(err),
                Err(_) => {
                    self.get_bytes_with_limit(
                        content_url,
                        fallback_bearer,
                        MAX_IN_MEMORY_ATTACHMENT_BYTES,
                    )
                    .await?
                }
            }
        };
        Ok((raw_name, attachment.content_type.clone(), bytes))
    }
}

pub(crate) fn should_send_bearer(graph_base_url: &str, target_url: &str) -> bool {
    let trimmed = target_url.trim();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return true;
    }
    let Ok(target) = url::Url::parse(trimmed) else {
        return false;
    };
    let Some(target_host) = target.host_str() else {
        return false;
    };
    if let Ok(base) = url::Url::parse(graph_base_url.trim()) {
        if let Some(base_host) = base.host_str() {
            if target.scheme() == base.scheme()
                && target.port_or_known_default() == base.port_or_known_default()
            {
                if target_host.eq_ignore_ascii_case(base_host) {
                    return true;
                }
                let is_loopback = |h: &str| {
                    h.eq_ignore_ascii_case("127.0.0.1") || h.eq_ignore_ascii_case("localhost")
                };
                if is_loopback(base_host) && is_loopback(target_host) {
                    return true;
                }
            }
        }
    }
    if target.scheme() == "https" {
        let lower = target_host.to_ascii_lowercase();
        if lower == "microsoft.com" || lower.ends_with(".microsoft.com") {
            return true;
        }
    }
    false
}

fn is_auth_or_forbidden_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    let detail = msg
        .strip_prefix(crate::graph::GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    let lower = detail.to_ascii_lowercase();
    detail.starts_with("401 ")
        || detail.starts_with("403 ")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
}

/// Extracts a lowercase GUID from a DriveItem `eTag` (e.g. `"{153FA47D-18C9-4179-BE08-9879815A9F90},1"`),
/// falling back to a fresh random UUID v4 when absent or unparseable.
pub(crate) fn extract_etag_guid(etag: Option<&str>) -> String {
    if let Some(raw) = etag {
        // Scan candidate 36-char UUID windows inside the eTag string
        let bytes = raw.as_bytes();
        if bytes.len() >= 36 {
            for start in 0..=bytes.len() - 36 {
                let candidate = &raw[start..start + 36];
                if let Ok(parsed) = Uuid::parse_str(candidate) {
                    return parsed.hyphenated().to_string();
                }
            }
        }
    }
    Uuid::new_v4().hyphenated().to_string()
}

/// Encodes a sharing/WebDAV URL into a Microsoft Graph `/shares/{shareId}` token (`"u!" + base64url_no_pad`).
pub(crate) fn encode_graph_share_token(url: &str) -> String {
    format!("u!{}", URL_SAFE_NO_PAD.encode(url.as_bytes()))
}

fn filename_from_url(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    let seg = parsed.path_segments()?.rfind(|s| !s.is_empty())?;
    let decoded = urlencoding::decode(seg).ok()?.into_owned();
    let trimmed = decoded.trim().to_string();
    if trimmed.is_empty() || trimmed == "$value" {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) fn sanitize_filename(raw: &str) -> String {
    let leaf = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);
    let mut cleaned: String = leaf
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "attachment".to_string()
    } else {
        cleaned
    }
}

pub(crate) fn deduplicate_dest_path(dest_dir: &Path, safe_name: &str) -> PathBuf {
    let candidate = dest_dir.join(safe_name);
    if !candidate.exists() {
        return candidate;
    }

    let p = Path::new(safe_name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str());

    for idx in 1..10_000 {
        let next_name = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({idx}).{e}"),
            _ => format!("{stem} ({idx})"),
        };
        let next_path = dest_dir.join(&next_name);
        if !next_path.exists() {
            return next_path;
        }
    }
    dest_dir.join(format!("{stem}_{}", Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_extract_etag_guid_variants() {
        assert_eq!(
            extract_etag_guid(Some("\"{153FA47D-18C9-4179-BE08-9879815A9F90},1\"")),
            "153fa47d-18c9-4179-be08-9879815a9f90"
        );
        assert_eq!(
            extract_etag_guid(Some("{153FA47D-18C9-4179-BE08-9879815A9F90},42")),
            "153fa47d-18c9-4179-be08-9879815a9f90"
        );
        assert_eq!(
            extract_etag_guid(Some("W/\"153fa47d-18c9-4179-be08-9879815a9f90\"")),
            "153fa47d-18c9-4179-be08-9879815a9f90"
        );
        let fallback = extract_etag_guid(None);
        assert!(Uuid::parse_str(&fallback).is_ok());
    }

    #[test]
    fn test_encode_graph_share_token() {
        let url = "https://contoso.sharepoint.com/sites/eng/Shared Documents/spec.pdf";
        let token = encode_graph_share_token(url);
        assert!(token.starts_with("u!"));
        assert!(!token.contains('='));
        assert!(!token.contains('+'));
        assert!(!token.contains('/'));
        let decoded = URL_SAFE_NO_PAD.decode(&token[2..]).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), url);
    }

    #[test]
    fn test_sanitize_and_deduplicate_filename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("re:po*rt?.pdf"), "re_po_rt_.pdf");
        assert_eq!(sanitize_filename(".."), "attachment");

        let dir = tempdir().unwrap();
        let first = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        std::fs::write(&first, b"1").unwrap();

        let second = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (1).pdf");
        std::fs::write(&second, b"2").unwrap();

        let third = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(third.file_name().unwrap(), "report (2).pdf");
    }

    #[test]
    fn test_should_send_bearer_origin_guard() {
        let base = "https://graph.microsoft.com/v1.0";
        assert!(should_send_bearer(base, "/me/drive/items/1/content"));
        assert!(should_send_bearer(base, "https://graph.microsoft.com/v1.0/shares/123"));
        assert!(should_send_bearer(base, "https://cdn.microsoft.com/file.bin"));
        assert!(!should_send_bearer(base, "https://evil.example.com/steal"));
        assert!(!should_send_bearer(base, "https://contoso.sharepoint.com/sites/a/doc.pdf"));
        assert!(!should_send_bearer(base, "http://graph.microsoft.com/v1.0/me"));

        let mock_base = "http://127.0.0.1:4567/v1.0";
        assert!(should_send_bearer(mock_base, "http://127.0.0.1:4567/files/a.bin"));
        assert!(should_send_bearer(mock_base, "http://localhost:4567/files/a.bin"));
        assert!(!should_send_bearer(mock_base, "http://127.0.0.1:9999/files/a.bin"));
    }

    #[tokio::test]
    async fn test_download_attachment_streams_and_protects_bearer_and_403() {
        use axum::{
            http::{header, HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::{Arc, Mutex};

        let saw_ext_auth = Arc::new(Mutex::new(Option::<String>::None));
        let saw_ext_clone = saw_ext_auth.clone();

        let ext_app = Router::new().route(
            "/external/file.txt",
            get(move |headers: HeaderMap| {
                let saw = saw_ext_clone.clone();
                async move {
                    *saw.lock().unwrap() = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    (StatusCode::OK, b"external-payload-ok".to_vec())
                }
            }),
        );
        let ext_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ext_addr = ext_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(ext_listener, ext_app).await;
        });

        let forbidden_url = format!(
            "http://127.0.0.1:{}/external/file.txt?forbidden=1",
            ext_addr.port()
        );
        let forbidden_share_tok = encode_graph_share_token(&forbidden_url);
        let graph_app = Router::new().route(
            "/v1.0/shares/{token}/driveItem/content",
            get(move |axum::extract::Path(token): axum::extract::Path<String>| {
                let forbidden_tok = forbidden_share_tok.clone();
                async move {
                    if token == forbidden_tok {
                        (StatusCode::FORBIDDEN, "forbidden")
                    } else {
                        (StatusCode::NOT_FOUND, "not found")
                    }
                }
            }),
        );
        let graph_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let graph_addr = graph_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(graph_listener, graph_app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = format!("http://{graph_addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "secret-graph-bearer".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let dir = tempdir().unwrap();

        // 1. 404 on /shares falls back to external content_url WITHOUT leaking Bearer token
        let ext_url = format!("http://127.0.0.1:{}/external/file.txt", ext_addr.port());
        let att = AttachmentInfo {
            id: "a1".into(),
            content_type: "reference".into(),
            content_url: Some(ext_url),
            name: Some("ext.txt".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let dl = client.download_attachment(&att, dir.path()).await.unwrap();
        assert_eq!(std::fs::read(&dl.path).unwrap(), b"external-payload-ok");
        assert_eq!(dl.bytes_written, 19);
        assert_eq!(*saw_ext_auth.lock().unwrap(), None);

        // 2. 403 on /shares must NOT be swallowed or fall back to content_url
        let forbidden_att = AttachmentInfo {
            id: "a2".into(),
            content_type: "reference".into(),
            content_url: Some(forbidden_url),
            name: Some("forbidden.txt".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let err = client
            .download_attachment(&forbidden_att, dir.path())
            .await
            .unwrap_err();
        assert!(is_auth_or_forbidden_error(&err));
        assert!(!dir.path().join("forbidden.txt").exists());
    }

    #[tokio::test]
    async fn test_get_bytes_with_limit_rejects_oversized_response() {
        use axum::{routing::get, Router};

        let app = Router::new().route(
            "/v1.0/big",
            get(|| async { vec![b'x'; 2048] }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let err = client
            .get_bytes_with_limit("/big", true, 1024)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds in-memory limit"));
    }
}
