//! Bidirectional parser between Outlook HTML emails, Plain Text / Markdown, and Rich Terminal AST.

pub mod html_in;
pub mod md_out;

use std::borrow::Cow;

use crate::models::{MailAttachment, MailMessage, RichBlock};

pub use html_in::{
    blocks_to_markdown, blocks_to_plain, parse_message_blocks, parse_message_blocks_arc,
};
pub use md_out::compile_outgoing_mail_html;

/// Returns effective attachments for an email, combining explicit `MailAttachment`s with any
/// inline `<img src="cid:...">` or `<img src="https://...">` images found in `msg.body_html`
/// so TUI thumbnails and HD `ImagePreview` can resolve all images uniformly.
pub fn effective_mail_attachments(msg: &MailMessage) -> Cow<'_, [MailAttachment]> {
    if !msg
        .body_html
        .as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"<img"))
    {
        return Cow::Borrowed(&msg.attachments);
    }

    let blocks = parse_message_blocks_arc(&msg.body_html, &msg.attachments);
    effective_mail_attachments_from_blocks(msg, &blocks)
}

pub(crate) fn effective_mail_attachments_from_blocks<'a>(
    msg: &'a MailMessage,
    blocks: &[RichBlock],
) -> Cow<'a, [MailAttachment]> {
    let mut extra: Vec<MailAttachment> = Vec::new();
    collect_inline_image_attachments(blocks, &msg.attachments, &mut extra);

    if extra.is_empty() {
        Cow::Borrowed(&msg.attachments)
    } else {
        let mut combined = Vec::with_capacity(msg.attachments.len() + extra.len());
        combined.extend_from_slice(&msg.attachments);
        combined.extend(extra);
        Cow::Owned(combined)
    }
}

fn collect_inline_image_attachments(
    blocks: &[RichBlock],
    existing: &[MailAttachment],
    out: &mut Vec<MailAttachment>,
) {
    for block in blocks {
        match block {
            RichBlock::InlineImageChip {
                src,
                content_id,
                index,
            } => {
                let norm_cid = content_id
                    .as_deref()
                    .map(html_in::normalize_content_id)
                    .filter(|s| !s.is_empty());

                let already_present = |att: &MailAttachment| {
                    if let Some(ref cid) = norm_cid {
                        if att
                            .content_id
                            .as_deref()
                            .map(html_in::normalize_content_id)
                            .is_some_and(|existing_cid| existing_cid.eq_ignore_ascii_case(cid))
                        {
                            return true;
                        }
                    }
                    att.id == *src
                };

                if existing.iter().any(already_present) || out.iter().any(already_present) {
                    continue;
                }

                let (id, name, content_type) = if let Some(ref cid) = norm_cid {
                    let fname = cid
                        .split('@')
                        .next()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("inline-image-{}.png", index + 1));
                    let mime = guess_image_mime(&fname);
                    (format!("cid:{cid}"), fname, mime)
                } else {
                    let lower_src = src.trim().to_ascii_lowercase();
                    let fname = filename_from_src(src)
                        .unwrap_or_else(|| format!("inline-image-{}.png", index + 1));
                    let mime = if let Some(rest) = lower_src.strip_prefix("data:") {
                        let data_mime = rest
                            .split([';', ','])
                            .next()
                            .map(str::trim)
                            .unwrap_or("");
                        if !data_mime.is_empty() {
                            data_mime.to_string()
                        } else {
                            guess_image_mime(&fname)
                        }
                    } else {
                        guess_image_mime(&fname)
                    };
                    (src.clone(), fname, mime)
                };

                out.push(MailAttachment {
                    id,
                    name,
                    content_type,
                    size: 0,
                    is_inline: true,
                    content_id: norm_cid,
                    content_bytes_b64: None,
                });
            }
            RichBlock::QuotedHistory { blocks: nested, .. } => {
                collect_inline_image_attachments(nested, existing, out);
            }
            _ => {}
        }
    }
}

fn filename_from_src(src: &str) -> Option<String> {
    let trimmed = src.trim();
    if trimmed.starts_with("data:") {
        return None;
    }
    let path_part = trimmed.split(['?', '#']).next()?;
    let seg = path_part.rsplit('/').next()?.trim();
    if seg.is_empty() || seg == "$value" {
        None
    } else {
        Some(seg.to_string())
    }
}

fn guess_image_mime(name: &str) -> String {
    let lower = name
        .split(['?', '#'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".svg") || lower.ends_with(".svgz") {
        "image/svg+xml".to_string()
    } else if lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml") {
        "text/html".to_string()
    } else {
        "image/png".to_string()
    }
}

/// Strips HTML tags and formats email content into clean plain text for previews & FTS5 indexing.
pub fn html_to_plain(html: &str, attachments: &[MailAttachment]) -> String {
    let blocks = html_in::parse_message_blocks_no_cache(html, attachments);
    html_in::blocks_to_plain(&blocks)
}

/// Compatibility alias for `html_to_plain`.
pub fn html_and_cards_to_plain(html: &str, attachments: &[MailAttachment]) -> String {
    html_to_plain(html, attachments)
}

/// Converts an Outlook HTML email + attachments into clean GitHub-flavored Markdown.
pub fn html_to_markdown(html: &str, attachments: &[MailAttachment]) -> String {
    let blocks = html_in::parse_message_blocks_no_cache(html, attachments);
    html_in::blocks_to_markdown(&blocks)
}

/// Compatibility alias for `html_to_markdown`.
pub fn html_and_cards_to_markdown(html: &str, attachments: &[MailAttachment]) -> String {
    html_to_markdown(html, attachments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EmailRecipient;

    fn sample_mail_with_html(html: &str, attachments: Vec<MailAttachment>) -> MailMessage {
        MailMessage {
            id: "m1".to_string(),
            internet_message_id: None,
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: "Test".to_string(),
            from: Some(EmailRecipient {
                name: Some("Alice".to_string()),
                address: "alice@example.com".to_string(),
            }),
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: "2026-11-18T00:00:00Z".to_string(),
            sent_at: None,
            last_modified_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: None,
            has_attachments: !attachments.is_empty(),
            body_loaded: true,
            preview: String::new(),
            body_html: html.to_string(),
            body_plain: String::new(),
            attachments,
            meeting_request: None,
            web_link: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_effective_mail_attachments_cid_dedup_and_https_synthesis() {
        let existing = vec![MailAttachment {
            id: "att-cid-1".to_string(),
            name: "diagram.png".to_string(),
            content_type: "image/png".to_string(),
            size: 2048,
            is_inline: true,
            content_id: Some("<img001@01DB>".to_string()),
            content_bytes_b64: Some("AQID".to_string()),
        }];

        // Case 1: Only matching cid -> zero-copy Borrowed
        let msg_cid_only = sample_mail_with_html(
            r#"<p>See below:</p><img src="cid:img001@01DB">"#,
            existing.clone(),
        );
        let eff_cid = effective_mail_attachments(&msg_cid_only);
        assert!(matches!(eff_cid, Cow::Borrowed(_)));
        assert_eq!(eff_cid.len(), 1);
        assert_eq!(eff_cid[0].id, "att-cid-1");

        // Case 2: Tracking pixel ignored + external https image synthesized
        let msg_mixed = sample_mail_with_html(
            r#"<p>Hello</p><img src="cid:img001@01DB"><img src="https://track.example.com/1x1.gif" width="1" height="1"><img src="https://cdn.example.com/banner.jpg?w=600">"#,
            existing,
        );
        let eff_mixed = effective_mail_attachments(&msg_mixed);
        assert_eq!(eff_mixed.len(), 2);
        assert_eq!(eff_mixed[1].id, "https://cdn.example.com/banner.jpg?w=600");
        assert_eq!(eff_mixed[1].name, "banner.jpg");
        assert_eq!(eff_mixed[1].content_type, "image/jpeg");
        assert!(eff_mixed[1].is_inline);

        // Case 3: Inline SVG is supported as native image preview, while HTML redirect URLs remain excluded
        let msg_non_raster = sample_mail_with_html(
            r#"<img src="https://cdn.example.com/logo.svg?v=2"><img src="https://track.example.com/pixel.html"><img src="data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=">"#,
            Vec::new(),
        );
        let eff_non_raster = effective_mail_attachments(&msg_non_raster);
        assert_eq!(eff_non_raster.len(), 3);
        assert_eq!(eff_non_raster[0].content_type, "image/svg+xml");
        assert_eq!(eff_non_raster[1].content_type, "text/html");
        assert_eq!(eff_non_raster[2].content_type, "image/svg+xml");
        assert!(crate::img_preview::is_image_mime_or_name(
            &eff_non_raster[0].content_type,
            Some(&eff_non_raster[0].name)
        ));
        assert!(!crate::img_preview::is_image_mime_or_name(
            &eff_non_raster[1].content_type,
            Some(&eff_non_raster[1].name)
        ));
        assert!(crate::img_preview::is_image_mime_or_name(
            &eff_non_raster[2].content_type,
            Some(&eff_non_raster[2].name)
        ));
    }

    #[test]
    fn test_html_to_plain_and_markdown_roundtrip() {
        let html = r#"<style>p{color:black}</style><h2>Agenda</h2><ul><li>Deploy</li></ul><p>Check <b>status</b> at <a href="https://example.com">portal</a>.</p>"#;
        let plain = html_to_plain(html, &[]);
        assert!(plain.contains("Agenda"));
        assert!(plain.contains("- Deploy"));
        assert!(!plain.contains("color:black"));

        let md = html_to_markdown(html, &[]);
        assert!(md.contains("## Agenda"));
        assert!(md.contains("**status**"));
        assert!(md.contains("[portal](https://example.com)"));
    }
}
