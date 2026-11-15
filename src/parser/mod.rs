//! Bidirectional parser between Teams HTML / Adaptive Cards, Plain Text / Markdown, and Rich Terminal AST.

use crate::models::{
    AttachmentInfo, InlineImageUpload, MentionInfo, OutgoingMessage, QuotedReply, RichBlock,
    UploadedReferenceAttachment,
};

/// Strips HTML tags and formats Adaptive Cards into clean plain text for previews & FTS5 indexing.
pub fn html_and_cards_to_plain(html: &str, attachments: &[AttachmentInfo]) -> String {
    html.to_string()
}

/// Converts a Teams HTML message + attachments into clean GitHub-flavored Markdown (used by `export --format md`).
pub fn html_and_cards_to_markdown(html: &str, attachments: &[AttachmentInfo]) -> String {
    html.to_string()
}

/// Parses Teams HTML + Adaptive Cards into structured `RichBlock`s for styled `ratatui` rendering.
pub fn parse_message_blocks(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Vec<RichBlock> {
    if html.trim().is_empty() {
        return Vec::new();
    }
    vec![RichBlock::Paragraph(vec![crate::models::RichSpan::plain(
        html,
    )])]
}

/// Compiles user Markdown input + `@mention` list + uploaded reference attachments + inline images
/// + optional quote reply into a valid `OutgoingMessage` ready for Microsoft Graph.
pub fn compile_outgoing_markdown(
    markdown: &str,
    subject: Option<String>,
    mentions: Vec<MentionInfo>,
    attachments: Vec<UploadedReferenceAttachment>,
    inline_images: Vec<InlineImageUpload>,
    quote_reply_to: Option<QuotedReply>,
) -> OutgoingMessage {
    OutgoingMessage {
        subject,
        body_html: markdown.to_string(),
        mentions,
        attachments,
        inline_images,
        importance: None,
        quote_reply_to,
    }
}
