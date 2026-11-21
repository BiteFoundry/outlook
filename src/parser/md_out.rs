//! Outgoing Markdown + inline `cid:` images + quoted reply -> email-client-safe HTML compiler.

use std::collections::{HashMap, HashSet};

use crate::{models::InlineImageUpload, parser::html_in::normalize_content_id};

/// Compiles GitHub-Flavored Markdown, inline `cid:` images, and an optional quoted reply HTML block
/// into clean, email-client-safe HTML.
pub fn compile_outgoing_mail_html(
    markdown: &str,
    inline_images: &[InlineImageUpload],
    quoted_reply_html: Option<&str>,
) -> String {
    let mut cid_aliases: HashMap<String, String> = HashMap::new();
    for img in inline_images {
        let cid = normalize_content_id(&img.content_id);
        let temp_id = normalize_content_id(&img.temp_id);
        let canonical = if cid.is_empty() {
            temp_id.clone()
        } else {
            cid
        };
        if canonical.is_empty() {
            continue;
        }
        cid_aliases.insert(canonical.to_ascii_lowercase(), canonical.clone());
        if !temp_id.is_empty() {
            cid_aliases
                .entry(temp_id.to_ascii_lowercase())
                .or_insert_with(|| canonical.clone());
        }
    }

    let (body_html, referenced_cids) = render_markdown_to_html(markdown, &cid_aliases);
    let mut out = String::with_capacity(body_html.len() + 256);
    out.push_str(&body_html);

    for img in inline_images {
        let cid = normalize_content_id(&img.content_id);
        let temp_id = normalize_content_id(&img.temp_id);
        let fallback_id = if cid.is_empty() { temp_id.clone() } else { cid };
        if fallback_id.is_empty()
            || referenced_cids.contains(&fallback_id.to_ascii_lowercase())
            || (!temp_id.is_empty() && referenced_cids.contains(&temp_id.to_ascii_lowercase()))
        {
            continue;
        }
        let alt = if img.file_name.trim().is_empty() {
            "inline image"
        } else {
            img.file_name.trim()
        };
        out.push_str(&format!(
            "<p><img src=\"cid:{}\" alt=\"{}\" style=\"max-width:100%;height:auto;\"></p>",
            escape_html_attr(&fallback_id),
            escape_html_attr(alt),
        ));
    }

    if let Some(quoted) = quoted_reply_html.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("<div id=\"appendonsend\"></div><hr tabindex=\"-1\"><div id=\"divRplyFwdMsg\">");
        out.push_str(quoted);
        out.push_str("</div>");
    }

    out
}

fn render_markdown_to_html(
    markdown: &str,
    cid_aliases: &HashMap<String, String>,
) -> (String, HashSet<String>) {
    let normalized: std::borrow::Cow<'_, str> = if markdown.contains('\r') {
        std::borrow::Cow::Owned(markdown.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(markdown)
    };
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut referenced_cids: HashSet<String> = HashSet::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

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
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Horizontal rule
        if is_horizontal_rule(trimmed) {
            blocks.push("<hr>".to_string());
            i += 1;
            continue;
        }

        // ATX Heading: # .. ######
        if let Some((level, heading_text)) = parse_atx_heading(line) {
            blocks.push(format!(
                "<h{level}>{}</h{level}>",
                render_inline(heading_text, cid_aliases, &mut referenced_cids)
            ));
            i += 1;
            continue;
        }

        // Blockquote lines starting with ">"
        if line.starts_with("> ") || line == ">" {
            let mut quote_lines = Vec::new();
            while i < lines.len() && (lines[i].starts_with("> ") || lines[i] == ">") {
                let content = lines[i]
                    .strip_prefix("> ")
                    .or_else(|| lines[i].strip_prefix('>'))
                    .unwrap_or("");
                quote_lines.push(render_inline(content, cid_aliases, &mut referenced_cids));
                i += 1;
            }
            blocks.push(format!("<blockquote>{}</blockquote>", quote_lines.join("<br>")));
            continue;
        }

        // Unordered list (- item or * item)
        if parse_unordered_list_item(line).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                if let Some(item_text) = parse_unordered_list_item(lines[i]) {
                    items.push(format!(
                        "<li>{}</li>",
                        render_inline(item_text, cid_aliases, &mut referenced_cids)
                    ));
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(format!("<ul>{}</ul>", items.join("")));
            continue;
        }

        // Ordered list (1. item)
        if parse_ordered_list_item(line).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                if let Some(item_text) = parse_ordered_list_item(lines[i]) {
                    items.push(format!(
                        "<li>{}</li>",
                        render_inline(item_text, cid_aliases, &mut referenced_cids)
                    ));
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(format!("<ol>{}</ol>", items.join("")));
            continue;
        }

        // Paragraph until blank line or another block opener
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let cur = lines[i];
            let cur_trim = cur.trim();
            if cur_trim.is_empty()
                || cur.trim_start().starts_with("```")
                || is_horizontal_rule(cur_trim)
                || parse_atx_heading(cur).is_some()
                || cur.starts_with("> ")
                || cur == ">"
                || parse_unordered_list_item(cur).is_some()
                || parse_ordered_list_item(cur).is_some()
            {
                break;
            }
            para_lines.push(render_inline(cur, cid_aliases, &mut referenced_cids));
            i += 1;
        }
        if !para_lines.is_empty() {
            blocks.push(format!("<p>{}</p>", para_lines.join("<br>")));
        }
    }

    (blocks.join(""), referenced_cids)
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let mut chars = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 1usize;
    for ch in chars {
        if ch != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = trimmed[hashes..].strip_prefix(' ')?;
    let content = rest.trim();
    if content.is_empty() {
        None
    } else {
        Some((hashes as u8, content))
    }
}

fn parse_unordered_list_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .map(str::trim)
}

fn parse_ordered_list_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    trimmed[digits..].strip_prefix(". ").map(str::trim)
}

fn render_inline(
    input: &str,
    cid_aliases: &HashMap<String, String>,
    referenced_cids: &mut HashSet<String>,
) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut no_closing_backtick = false;
    let mut no_closing_image = false;
    let mut no_closing_bracket = false;
    let mut no_closing_double_star = false;
    let mut no_closing_tilde = false;
    let mut no_closing_star = false;

    while i < input.len() {
        let rest = &input[i..];

        // Inline code: `...`
        if !no_closing_backtick {
            if let Some(after_tick) = rest.strip_prefix('`') {
                if let Some(end_idx) = after_tick.find('`') {
                    let code = &after_tick[..end_idx];
                    out.push_str("<code>");
                    out.push_str(&escape_html(code));
                    out.push_str("</code>");
                    i += 1 + end_idx + 1;
                    continue;
                } else {
                    no_closing_backtick = true;
                }
            }
        }

        // Inline image: ![alt](src)
        if !no_closing_image && rest.starts_with("![") {
            match parse_markdown_link(&rest[1..]) {
                Some((alt, src, consumed)) => {
                    if is_safe_image_url(src) {
                        let normalized_src = if src.len() >= 4 && src[..4].eq_ignore_ascii_case("cid:") {
                            let raw_cid = normalize_content_id(&src[4..]);
                            if !raw_cid.is_empty() {
                                let resolved = cid_aliases
                                    .get(&raw_cid.to_ascii_lowercase())
                                    .cloned()
                                    .unwrap_or(raw_cid);
                                referenced_cids.insert(resolved.to_ascii_lowercase());
                                format!("cid:{resolved}")
                            } else {
                                src.to_string()
                            }
                        } else {
                            src.to_string()
                        };
                        out.push_str(&format!(
                            "<img src=\"{}\" alt=\"{}\" style=\"max-width:100%;height:auto;\">",
                            escape_html_attr(&normalized_src),
                            escape_html_attr(alt)
                        ));
                        i += 1 + consumed;
                        continue;
                    }
                }
                None => {
                    no_closing_image = true;
                }
            }
        }

        // Markdown link: [label](url)
        if !no_closing_bracket && rest.starts_with('[') {
            match parse_markdown_link(rest) {
                Some((label, url, consumed)) => {
                    if is_safe_url(url) {
                        out.push_str(&format!(
                            "<a href=\"{}\">{}</a>",
                            escape_html_attr(url),
                            render_inline(label, cid_aliases, referenced_cids)
                        ));
                        i += consumed;
                        continue;
                    }
                }
                None => {
                    no_closing_bracket = true;
                }
            }
        }

        // Bold: **...**
        if !no_closing_double_star {
            if let Some(after_stars) = rest.strip_prefix("**") {
                match after_stars.find("**") {
                    Some(end_idx) => {
                        if end_idx > 0 {
                            let inner = &after_stars[..end_idx];
                            out.push_str("<strong>");
                            out.push_str(&render_inline(inner, cid_aliases, referenced_cids));
                            out.push_str("</strong>");
                            i += 2 + end_idx + 2;
                            continue;
                        }
                    }
                    None => {
                        no_closing_double_star = true;
                    }
                }
            }
        }

        // Strikethrough: ~~...~~
        if !no_closing_tilde {
            if let Some(after_tilde) = rest.strip_prefix("~~") {
                match after_tilde.find("~~") {
                    Some(end_idx) => {
                        if end_idx > 0 {
                            let inner = &after_tilde[..end_idx];
                            out.push_str("<s>");
                            out.push_str(&render_inline(inner, cid_aliases, referenced_cids));
                            out.push_str("</s>");
                            i += 2 + end_idx + 2;
                            continue;
                        }
                    }
                    None => {
                        no_closing_tilde = true;
                    }
                }
            }
        }

        // Italic: *...* (single asterisk, not **)
        if !no_closing_star && rest.starts_with('*') && !rest.starts_with("**") {
            match find_closing_single_asterisk(&rest[1..]) {
                Some(end_idx) => {
                    if end_idx > 0 {
                        let inner = &rest[1..1 + end_idx];
                        out.push_str("<em>");
                        out.push_str(&render_inline(inner, cid_aliases, referenced_cids));
                        out.push_str("</em>");
                        i += 1 + end_idx + 1;
                        continue;
                    }
                }
                None => {
                    no_closing_star = true;
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

fn is_safe_image_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("cid:")
        || lower.starts_with("https://")
        || lower.starts_with("http://")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compile_outgoing_mail_html_features_and_escaping() {
        let md = "# Weekly Update\n\n**Bold** *italic* ~~old~~ `code` [link](https://example.com?a=1&b=2) <script>bad</script>\n\n- First\n- Second\n\n1. Step one\n2. Step two\n\n```rust\nlet x = 1 < 2;\n```\n\n![chart](cid:chart@01)";
        let imgs = vec![
            InlineImageUpload {
                temp_id: "1".to_string(),
                content_id: "chart@01".to_string(),
                file_name: "chart.png".to_string(),
                content_type: "image/png".to_string(),
                bytes: vec![1, 2],
            },
            InlineImageUpload {
                temp_id: "2".to_string(),
                content_id: "<logo@02>".to_string(),
                file_name: "logo.png".to_string(),
                content_type: "image/png".to_string(),
                bytes: vec![3, 4],
            },
        ];

        let html = compile_outgoing_mail_html(md, &imgs, Some("<p>Original mail</p>"));
        assert!(html.contains("<h1>Weekly Update</h1>"));
        assert!(html.contains("<strong>Bold</strong>"));
        assert!(html.contains("<em>italic</em>"));
        assert!(html.contains("<s>old</s>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<a href=\"https://example.com?a=1&amp;b=2\">link</a>"));
        assert!(html.contains("&lt;script&gt;bad&lt;/script&gt;"));
        assert!(html.contains("<ul><li>First</li><li>Second</li></ul>"));
        assert!(html.contains("<ol><li>Step one</li><li>Step two</li></ol>"));
        assert!(html.contains("<pre><code class=\"language-rust\">let x = 1 &lt; 2;</code></pre>"));
        assert!(html.contains("<img src=\"cid:chart@01\" alt=\"chart\""));
        // Unreferenced inline image is appended once, while chart@01 is not duplicated
        assert_eq!(html.matches("cid:chart@01").count(), 1);
        assert!(html.contains("<img src=\"cid:logo@02\" alt=\"logo.png\""));
        assert!(html.contains("<div id=\"divRplyFwdMsg\"><p>Original mail</p></div>"));
    }

    #[test]
    fn test_render_inline_linear_on_unmatched_delimiters_and_unsafe_links() {
        let many_ticks = "` ".repeat(2000);
        let many_stars = "* ".repeat(2000);
        let many_double = "** ".repeat(2000);
        let many_tilde = "~~ ".repeat(2000);
        let many_brackets = "[ ".repeat(2000);

        let payload = format!(
            "{many_ticks}\n\n{many_stars}\n\n{many_double}\n\n{many_tilde}\n\n{many_brackets}\n\n[bad](javascript:alert(1)) [ok](https://example.com)"
        );
        let (html, _) = render_markdown_to_html(&payload, &HashMap::new());
        assert!(html.contains("<a href=\"https://example.com\">ok</a>"));
        assert!(!html.contains("<a href=\"javascript:"));
    }
}
