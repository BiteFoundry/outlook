//! Outgoing Markdown + mentions + attachments + quote reply -> safe Microsoft Teams HTML compiler.

use crate::models::{
    InlineImageUpload, MentionInfo, OutgoingMessage, QuotedReply, UploadedReferenceAttachment,
};

pub fn compile_outgoing_markdown(
    markdown: &str,
    subject: Option<String>,
    mentions: Vec<MentionInfo>,
    attachments: Vec<UploadedReferenceAttachment>,
    inline_images: Vec<InlineImageUpload>,
    quote_reply_to: Option<QuotedReply>,
) -> OutgoingMessage {
    let mut html = String::new();

    if let Some(q) = &quote_reply_to {
        html.push_str(&format!(
            "<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"{}\"><strong>{}</strong><br>{}</blockquote>",
            escape_html_attr(&q.msg_id),
            escape_html(&q.author_name),
            escape_html(&q.preview_plain)
        ));
    }

    let body = render_markdown_to_html(markdown, &mentions);
    html.push_str(&body);

    for img in &inline_images {
        html.push_str(&format!(
            "<img src=\"../hostedContents/{}/$value\">",
            escape_html_attr(&img.temp_id)
        ));
    }

    for att in &attachments {
        html.push_str(&format!(
            "<attachment id=\"{}\"></attachment>",
            escape_html_attr(&att.id)
        ));
    }

    OutgoingMessage {
        subject,
        body_html: html,
        mentions,
        attachments,
        inline_images,
        importance: None,
        quote_reply_to,
    }
}

fn render_markdown_to_html(markdown: &str, mentions: &[MentionInfo]) -> String {
    let normalized = markdown.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;

    let mut sorted_mentions: Vec<&MentionInfo> = mentions.iter().collect();
    sorted_mentions.sort_by(|a, b| b.display_name.len().cmp(&a.display_name.len()));

    while i < lines.len() {
        let line = lines[i];

        // Fenced code block ```lang ... ```
        if let Some(fence_rest) = line.trim_start().strip_prefix("```") {
            let lang = fence_rest.trim();
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    i += 1;
                    break;
                }
                code_lines.push(lines[i]);
                i += 1;
            }
            let code_escaped = escape_html(&code_lines.join("\n"));
            if lang.is_empty() {
                blocks.push(format!("<pre><code>{code_escaped}</code></pre>"));
            } else {
                blocks.push(format!(
                    "<pre><code class=\"language-{}\">{code_escaped}</code></pre>",
                    escape_html_attr(lang)
                ));
            }
            continue;
        }

        // Blank line separator
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Blockquote lines starting with "> " (or ">")
        if line.starts_with("> ") || line == ">" {
            let mut quote_lines = Vec::new();
            while i < lines.len() && (lines[i].starts_with("> ") || lines[i] == ">") {
                let content = lines[i]
                    .strip_prefix("> ")
                    .or_else(|| lines[i].strip_prefix('>'))
                    .unwrap_or("");
                quote_lines.push(render_inline(content, &sorted_mentions));
                i += 1;
            }
            blocks.push(format!("<blockquote>{}</blockquote>", quote_lines.join("<br>")));
            continue;
        }

        // Paragraph until blank line, fence, or blockquote
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let cur = lines[i];
            if cur.trim().is_empty()
                || cur.trim_start().starts_with("```")
                || cur.starts_with("> ")
                || cur == ">"
            {
                break;
            }
            para_lines.push(render_inline(cur, &sorted_mentions));
            i += 1;
        }
        if !para_lines.is_empty() {
            blocks.push(format!("<p>{}</p>", para_lines.join("<br>")));
        }
    }

    blocks.join("")
}

fn render_inline(input: &str, mentions: &[&MentionInfo]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        let rest = &input[i..];

        // Inline code: `...`
        if let Some(after_tick) = rest.strip_prefix('`') {
            if let Some(end_idx) = after_tick.find('`') {
                let code = &after_tick[..end_idx];
                out.push_str("<code>");
                out.push_str(&escape_html(code));
                out.push_str("</code>");
                i += 1 + end_idx + 1;
                continue;
            }
        }

        // Mention: @DisplayName
        if rest.starts_with('@') && is_mention_boundary(input, i) {
            if let Some(m) = mentions.iter().find(|m| {
                !m.display_name.is_empty()
                    && rest[1..].starts_with(&m.display_name)
                    && is_mention_end_boundary(&rest[1 + m.display_name.len()..])
            }) {
                out.push_str(&format!(
                    "<at id=\"{}\">{}</at>",
                    m.id,
                    escape_html(&m.display_name)
                ));
                i += 1 + m.display_name.len();
                continue;
            }
        }

        // Markdown link: [label](url)
        if rest.starts_with('[') {
            if let Some((label, url, consumed)) = parse_markdown_link(rest) {
                if is_safe_url(url) {
                    out.push_str(&format!(
                        "<a href=\"{}\">{}</a>",
                        escape_html_attr(url),
                        render_inline(label, mentions)
                    ));
                    i += consumed;
                    continue;
                }
            }
        }

        // Bold: **...**
        if let Some(after_stars) = rest.strip_prefix("**") {
            if let Some(end_idx) = after_stars.find("**") {
                if end_idx > 0 {
                    let inner = &after_stars[..end_idx];
                    out.push_str("<strong>");
                    out.push_str(&render_inline(inner, mentions));
                    out.push_str("</strong>");
                    i += 2 + end_idx + 2;
                    continue;
                }
            }
        }

        // Strikethrough: ~~...~~
        if let Some(after_tilde) = rest.strip_prefix("~~") {
            if let Some(end_idx) = after_tilde.find("~~") {
                if end_idx > 0 {
                    let inner = &after_tilde[..end_idx];
                    out.push_str("<s>");
                    out.push_str(&render_inline(inner, mentions));
                    out.push_str("</s>");
                    i += 2 + end_idx + 2;
                    continue;
                }
            }
        }

        // Italic: *...* (single asterisk, not **)
        if rest.starts_with('*') && !rest.starts_with("**") {
            if let Some(end_idx) = find_closing_single_asterisk(&rest[1..]) {
                if end_idx > 0 {
                    let inner = &rest[1..1 + end_idx];
                    out.push_str("<em>");
                    out.push_str(&render_inline(inner, mentions));
                    out.push_str("</em>");
                    i += 1 + end_idx + 1;
                    continue;
                }
            }
        }

        // Escape next character literally and safely
        let ch = rest.chars().next().unwrap();
        push_escaped_char(&mut out, ch);
        i += ch.len_utf8();
    }

    out
}

fn is_mention_boundary(full: &str, at_byte_idx: usize) -> bool {
    if at_byte_idx == 0 {
        return true;
    }
    full[..at_byte_idx]
        .chars()
        .next_back()
        .map(|c| !c.is_alphanumeric())
        .unwrap_or(true)
}

fn is_mention_end_boundary(after: &str) -> bool {
    match after.chars().next() {
        None => true,
        Some(c) => !c.is_alphanumeric() && c != '_',
    }
}

fn parse_markdown_link(rest: &str) -> Option<(&str, &str, usize)> {
    let after_open = rest.strip_prefix('[')?;
    let close_bracket = after_open.find("](")?;
    let label = &after_open[..close_bracket];
    let after_mid = &after_open[close_bracket + 2..];
    let close_paren = after_mid.find(')')?;
    let url = after_mid[..close_paren].trim();
    if url.is_empty() {
        return None;
    }
    let consumed = 1 + close_bracket + 2 + close_paren + 1;
    Some((label, url, consumed))
}

fn is_safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
}

fn find_closing_single_asterisk(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'*' {
            let next_is_star = idx + 1 < bytes.len() && bytes[idx + 1] == b'*';
            if !next_is_star {
                return Some(idx);
            }
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}

fn push_escaped_char(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#39;"),
        _ => out.push(ch),
    }
}

pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        push_escaped_char(&mut out, ch);
    }
    out
}

pub fn escape_html_attr(input: &str) -> String {
    escape_html(input)
}
