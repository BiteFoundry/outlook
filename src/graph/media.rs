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
            .with_context(|| format!("업로드할 파일을 찾을 수 없습니다: {}", local_path.display()))?;
        if !meta.is_file() {
            bail!("일반 파일만 업로드할 수 있습니다: {}", local_path.display());
        }
        let size = meta.len();
        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("유효한 파일 이름이 아닙니다: {}", local_path.display()))?
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
                    .ok_or_else(|| anyhow!("업로드된 파일의 공유 URL을 가져오지 못했습니다."))?;

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
                    .ok_or_else(|| anyhow!("채널 filesFolder의 driveId가 없습니다."))?;

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
                    .ok_or_else(|| anyhow!("업로드된 채널 파일의 webDavUrl/webUrl이 없습니다."))?;

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
                .with_context(|| format!("파일 읽기 실패: {}", local_path.display()))?;
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
            .with_context(|| format!("대용량 파일 열기 실패: {}", local_path.display()))?;
        let mut offset: u64 = 0;

        loop {
            let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];
            let mut filled = 0usize;
            while filled < UPLOAD_CHUNK_SIZE {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                bail!("업로드 세션이 완료 응답 없이 EOF에 도달했습니다.");
            }
            buf.truncate(filled);

            let end_inclusive = offset + (filled as u64) - 1;
            let (_status, val) = self
                .put_upload_chunk(&session.upload_url, buf, offset, end_inclusive, size)
                .await?;
            offset += filled as u64;

            if val.get("id").is_some() {
                let item: DriveItem = serde_json::from_value(val)
                    .context("업로드 세션 완료 DriveItem 역직렬화 실패")?;
                return Ok(item);
            }
            if offset >= size {
                bail!("업로드 완료 후 DriveItem 메타데이터를 받지 못했습니다.");
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
            .ok_or_else(|| anyhow!("첨부파일에 다운로드할 content_url이 없습니다."))?;

        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("다운로드 디렉터리 생성 실패: {}", dest_dir.display()))?;

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

        let bytes = if is_hosted {
            self.get_bytes(content_url).await?
        } else {
            let share_token = encode_graph_share_token(content_url);
            let share_path = format!("/shares/{share_token}/driveItem/content");
            match self.get_bytes(&share_path).await {
                Ok(b) => b,
                Err(_) => self.get_bytes(content_url).await?,
            }
        };

        tokio::fs::write(&dest_path, &bytes)
            .await
            .with_context(|| format!("파일 저장 실패: {}", dest_path.display()))?;

        Ok(DownloadedFile {
            path: dest_path,
            file_name: final_name,
            bytes_written: bytes.len() as u64,
        })
    }

    pub async fn download_hosted_bytes(&self, hosted_url: &str) -> Result<Vec<u8>> {
        self.get_bytes(hosted_url).await
    }
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
}
