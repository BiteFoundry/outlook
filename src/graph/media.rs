//! Shared attachment safety helpers (filename sanitization, deduplication, Bearer origin guard,
//! image format sniffing) and re-exported Outlook mail attachment download helpers.

use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::io::AsyncReadExt;

use super::GraphClient;
use crate::models::{DownloadedFile, MailAttachment};

pub(crate) const MAX_IN_MEMORY_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

impl GraphClient {
    /// Convenience alias delegating to `GraphClient::download_mail_attachment`.
    pub async fn download_attachment(
        &self,
        message_id: &str,
        attachment: &MailAttachment,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        self.download_mail_attachment(message_id, attachment, dest_dir)
            .await
    }

    /// Convenience alias delegating to `GraphClient::fetch_mail_attachment_bytes`.
    pub async fn fetch_attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        self.fetch_mail_attachment_bytes(message_id, attachment_id)
            .await
    }
}

pub fn should_send_bearer(graph_base_url: &str, target_url: &str) -> bool {
    let trimmed = target_url.trim();
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        return true;
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return !trimmed.starts_with("//") && !trimmed.contains("://") && !trimmed.is_empty();
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

pub(crate) fn is_auth_or_forbidden_error(err: &anyhow::Error) -> bool {
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
        || lower.contains("erroraccessdenied")
        || lower.contains("insufficient privileges")
}

pub fn sanitize_filename(raw: &str) -> String {
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

pub fn deduplicate_dest_path(dest_dir: &Path, safe_name: &str) -> PathBuf {
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
    dest_dir.join(format!("{stem}_{}", crate::new_uuid_v4()))
}

pub fn guess_mime_type(path: &Path) -> &'static str {
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

pub fn sniff_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 3 {
        return None;
    }
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some("png"),
        image::ImageFormat::Jpeg => Some("jpg"),
        image::ImageFormat::Gif => Some("gif"),
        image::ImageFormat::WebP => Some("webp"),
        _ => None,
    }
}

pub(crate) fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/html" => Some("html"),
        "application/json" => Some("json"),
        _ => None,
    }
}

pub(crate) async fn maybe_refine_image_extension(
    dest_path: &Path,
    dest_dir: &Path,
    safe_name: &str,
    final_name: &str,
) -> Option<(PathBuf, String)> {
    let cur_ext = Path::new(safe_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let is_generic = cur_ext.is_empty()
        || safe_name.starts_with("inline-image-")
        || safe_name.starts_with("attachment_");
    if !is_generic {
        return None;
    }

    let mut header = [0u8; 16];
    let n = {
        let mut f = tokio::fs::File::open(dest_path).await.ok()?;
        let mut read = 0usize;
        while read < header.len() {
            let chunk = f.read(&mut header[read..]).await.ok()?;
            if chunk == 0 {
                break;
            }
            read += chunk;
        }
        read
    };
    let detected_ext = sniff_image_extension(&header[..n])?;

    if cur_ext.eq_ignore_ascii_case(detected_ext)
        || (cur_ext.eq_ignore_ascii_case("jpeg") && detected_ext == "jpg")
    {
        return None;
    }

    let candidate_path = Path::new(final_name).with_extension(detected_ext);
    let candidate_name = candidate_path.file_name()?.to_str()?;
    let target_path = deduplicate_dest_path(dest_dir, candidate_name);
    tokio::fs::rename(dest_path, &target_path).await.ok()?;
    let target_name = target_path
        .file_name()
        .and_then(|s| s.to_str())?
        .to_string();
    Some((target_path, target_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        assert!(should_send_bearer(base, "/me/messages/1/attachments/a/$value"));
        assert!(should_send_bearer(
            base,
            "https://graph.microsoft.com/v1.0/me/messages"
        ));
        assert!(should_send_bearer(base, "https://cdn.microsoft.com/file.bin"));
        assert!(!should_send_bearer(base, "https://evil.example.com/steal"));
        assert!(!should_send_bearer(base, "//evil.example.com/x.png"));
        assert!(!should_send_bearer(base, "http://graph.microsoft.com/v1.0/me"));

        let mock_base = "http://127.0.0.1:4567/v1.0";
        assert!(should_send_bearer(mock_base, "http://127.0.0.1:4567/files/a.bin"));
        assert!(should_send_bearer(mock_base, "http://localhost:4567/files/a.bin"));
        assert!(!should_send_bearer(mock_base, "http://127.0.0.1:9999/files/a.bin"));
    }

    #[test]
    fn test_sniff_image_extension_and_guess_mime_type() {
        let jpeg_hdr = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01];
        assert_eq!(sniff_image_extension(&jpeg_hdr), Some("jpg"));
        let png_hdr = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(sniff_image_extension(&png_hdr), Some("png"));
        assert_eq!(sniff_image_extension(b"plain text"), None);
        assert_eq!(guess_mime_type(Path::new("deck.pptx")), "application/vnd.openxmlformats-officedocument.presentationml.presentation");
        assert_eq!(guess_mime_type(Path::new("photo.PNG")), "image/png");
    }
}
