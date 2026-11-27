//! Shared CLI/HTTP/MCP pagination, date/enum normalization, error formatting, and attachment helpers.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use serde_json::json;

use crate::{
    auth::explain_graph_permission_error,
    graph::GraphClient,
    img_preview,
    models::{MailAttachment, MailMessage},
    safe_text::sanitize_terminal_inline,
};

use super::args::DELETE_JSON_REQUIRES_YES;

pub(crate) const FILTER_SCAN_MAX_PAGES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CursorPos {
    Cache { received_at: String, id: String },
    Graph { skip: usize, url: String },
    Offset(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageCursor {
    pub scope: String,
    pub pos: CursorPos,
}

/// 8-hex-character FNV-1a 32-bit hash of the scope parts joined with U+001F.
pub(crate) fn cursor_scope(parts: &[&str]) -> String {
    let mut h: u32 = 0x811c9dc5;
    for b in parts.join("\u{1f}").bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

pub(crate) fn mail_cursor_scope(
    folder_id: &str,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
) -> String {
    let flags = format!(
        "{}{}{}",
        u8::from(unread_only),
        u8::from(flagged_only),
        u8::from(focused_only)
    );
    cursor_scope(&["mail", folder_id, &flags])
}

impl PageCursor {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let bad = |why: &str| anyhow::anyhow!("invalid cursor: {why}");
        let (kind, rest) = raw.split_once(':').ok_or_else(|| bad("malformed"))?;
        if !matches!(kind, "c" | "g" | "o") {
            return Err(bad("unknown prefix"));
        }
        let (scope, body) = rest.split_once(':').ok_or_else(|| bad("malformed"))?;
        if scope.len() != 8 || !scope.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("malformed"));
        }
        let pos = match kind {
            "c" => {
                let (ts, id) = body.split_once('|').ok_or_else(|| bad("malformed"))?;
                if ts.is_empty() || id.is_empty() {
                    return Err(bad("malformed"));
                }
                CursorPos::Cache {
                    received_at: ts.into(),
                    id: id.into(),
                }
            }
            "g" => {
                let (skip, url) = body.split_once('|').ok_or_else(|| bad("malformed"))?;
                let skip = skip.parse().map_err(|_| bad("malformed"))?;
                if url.is_empty() {
                    return Err(bad("malformed"));
                }
                CursorPos::Graph {
                    skip,
                    url: url.into(),
                }
            }
            _ => CursorPos::Offset(body.parse().map_err(|_| bad("malformed"))?),
        };
        Ok(Self {
            scope: scope.into(),
            pos,
        })
    }

    /// Rejects the cursor if its scope differs from the current call. Call before performing I/O.
    pub(crate) fn require_scope(&self, expected: &str) -> Result<()> {
        if self.scope.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "invalid cursor: issued for a different command, folder, query or filter"
            ))
        }
    }
}

pub(crate) fn offset_page<T: Clone>(
    all: &[T],
    offset: usize,
    limit: usize,
    scope: &str,
) -> (Vec<T>, bool, Option<String>) {
    let start = offset.min(all.len());
    let end = start.saturating_add(limit).min(all.len());
    let has_more = end < all.len();
    (
        all[start..end].to_vec(),
        has_more,
        has_more.then(|| format!("o:{scope}:{end}")),
    )
}

pub(crate) fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 1_000)
}

pub(crate) fn is_forbidden_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
        || lower.contains("missing scope")
        || lower.contains("aclcheckfailed")
        || lower.contains("insufficient graph permissions")
        || lower.contains("insufficient permissions")
}

pub(crate) fn format_graph_error(err: anyhow::Error, access_token: Option<&str>) -> anyhow::Error {
    let raw = format!("{err:#}");
    if let Some(explained) = explain_graph_permission_error(&raw, access_token) {
        anyhow::anyhow!("{explained}\nRaw error: {raw}")
    } else if raw.contains("ErrorInvalidRecipients") {
        anyhow::anyhow!(
            "{raw}\nHint: recipients must be SMTP addresses (user@example.com or \"Name <user@example.com>\"). Find an address with `outlook contacts --query <name>` (MCP: search_contacts)."
        )
    } else {
        err
    }
}

pub(crate) async fn wrap_graph_permission_error(
    graph: &GraphClient,
    err: anyhow::Error,
) -> anyhow::Error {
    let token = graph.auth().current_tokens().await.map(|t| t.access_token);
    format_graph_error(err, token.as_deref())
}

pub(crate) fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("No confirmation input received. Item was not deleted.")?;
    if !answer.trim().eq_ignore_ascii_case("yes") {
        bail!("Cancelled. Item was not deleted.");
    }
    Ok(())
}

pub(super) fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(&['\r', '\n'][..]).to_string())
}

pub(crate) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

pub(crate) fn resolved_abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn resolve_attachment_mime_type(content_type: &str, file_name: &str) -> String {
    let ct = content_type.trim();
    if !ct.is_empty() && !ct.eq_ignore_ascii_case("application/octet-stream") {
        return ct.to_string();
    }
    if let Some(inferred) = image_mime_for_path(Path::new(file_name)) {
        return inferred.to_string();
    }
    if Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    {
        return "application/pdf".to_string();
    }
    if ct.is_empty() {
        "application/octet-stream".to_string()
    } else {
        ct.to_string()
    }
}

pub(crate) fn format_downloaded_attachment_json(
    message_id: &str,
    selected: &MailAttachment,
    downloaded: &crate::models::DownloadedFile,
) -> serde_json::Value {
    let abs_path = resolved_abs_path(&downloaded.path);
    let mime_type = resolve_attachment_mime_type(&selected.content_type, &downloaded.file_name);
    let is_image = img_preview::is_image_mime_or_name(&mime_type, Some(&downloaded.file_name));
    json!({
        "ok": true,
        "message_id": message_id,
        "attachment_id": selected.id,
        "path": abs_path.to_string_lossy(),
        "file_name": downloaded.file_name,
        "mime_type": mime_type,
        "size_bytes": downloaded.bytes_written,
        "is_image": is_image,
    })
}

pub(crate) fn select_mail_attachment<'a>(
    mail: &'a MailMessage,
    index: Option<usize>,
    selector: Option<&str>,
) -> Result<&'a MailAttachment> {
    if mail.attachments.is_empty() {
        bail!("Message has no attachments: {}", mail.id);
    }
    if let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(idx) = sel.parse::<usize>() {
            return mail.attachments.get(idx).with_context(|| {
                format!(
                    "Attachment index {idx} is out of range (total {})",
                    mail.attachments.len()
                )
            });
        }
        if let Some(found) = mail
            .attachments
            .iter()
            .find(|a| a.id == sel || a.name.eq_ignore_ascii_case(sel))
        {
            return Ok(found);
        }
        bail!("Matching attachment not found: {sel}");
    }
    if let Some(idx) = index {
        return mail.attachments.get(idx).with_context(|| {
            format!(
                "Attachment index {idx} is out of range (total {})",
                mail.attachments.len()
            )
        });
    }
    Ok(mail
        .attachments
        .iter()
        .find(|a| img_preview::is_image_mime_or_name(&a.content_type, Some(&a.name)))
        .unwrap_or(&mail.attachments[0]))
}

pub(crate) async fn load_mail_attachment_bytes(
    graph: &GraphClient,
    mail_id: &str,
    att: &MailAttachment,
    offline: bool,
) -> Result<(String, String, Vec<u8>)> {
    if let Some(b64) = att
        .content_bytes_b64
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .with_context(|| format!("Failed to decode base64 attachment: {}", att.name))?;
        return Ok((att.name.clone(), att.content_type.clone(), bytes));
    }
    if offline {
        bail!(
            "Cannot fetch uncached attachment bytes in offline mode: {}",
            att.name
        );
    }
    graph.fetch_mail_attachment_bytes(mail_id, &att.id).await
}

pub(crate) fn normalize_mail_importance(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low"),
        "normal" => Ok("normal"),
        "high" => Ok("high"),
        other => bail!("Unsupported mail importance: '{other}' (low | normal | high)"),
    }
}

pub(crate) fn normalize_datetime_iso(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Timestamp string is empty.");
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d_%H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            let dt = Local
                .from_local_datetime(&naive)
                .earliest()
                .or_else(|| Local.from_local_datetime(&naive).latest())
                .context("Failed to convert local timestamp")?;
            return Ok(dt.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, true));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).context("Invalid date")?;
        let dt = Local
            .from_local_datetime(&naive)
            .earliest()
            .or_else(|| Local.from_local_datetime(&naive).latest())
            .context("Failed to convert local date")?;
        return Ok(dt.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, true));
    }
    bail!("Invalid timestamp '{trimmed}' (expected ISO-8601 or YYYY-MM-DD HH:MM)")
}

pub(crate) fn default_calendar_window(
    from: Option<&str>,
    to: Option<&str>,
    days: i64,
) -> Result<(String, String)> {
    let clamped_days = days.clamp(1, 365);
    let start_iso = match from.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => normalize_datetime_iso(s)?,
        None => {
            let today = Local::now().date_naive();
            let naive = today.and_hms_opt(0, 0, 0).context("Invalid start of day")?;
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap_or_else(Local::now)
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        }
    };
    let end_iso = match to.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => normalize_datetime_iso(e)?,
        None => {
            let start_dt = DateTime::parse_from_rfc3339(&start_iso)?;
            (start_dt + chrono::Duration::days(clamped_days))
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        }
    };
    Ok((start_iso, end_iso))
}

pub(crate) fn normalize_rsvp_response(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "accept" | "accepted" => Ok("accept"),
        "tentative" | "tentativelyaccept" | "tentatively_accept" | "tentativelyaccepted" => {
            Ok("tentativelyAccept")
        }
        "decline" | "declined" => Ok("decline"),
        other => bail!("Unsupported RSVP response: '{other}' (accept | tentative | decline)"),
    }
}

pub(crate) fn normalize_todo_due_iso(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Due date is empty.");
    }
    if NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok() {
        return Ok(format!("{trimmed}T00:00:00Z"));
    }
    normalize_datetime_iso(trimmed)
}

pub(crate) fn normalize_todo_importance(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low"),
        "normal" => Ok("normal"),
        "high" => Ok("high"),
        other => bail!("Unsupported To Do importance: '{other}' (low | normal | high)"),
    }
}

pub(super) fn format_unix_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

pub(super) fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let sanitized = sanitize_terminal_inline(input);
    let trimmed = sanitized.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}
