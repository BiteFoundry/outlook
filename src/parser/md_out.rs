//! Outgoing Markdown + inline `cid:` images + quoted reply -> email-client-safe HTML compiler.

use std::collections::{HashMap, HashSet};

use bitefoundry_html::pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};

use crate::{models::InlineImageUpload, parser::html_in::normalize_content_id};

const TABLE_STYLE: &str = "border-collapse:collapse;margin:8px 0;";
const TH_STYLE: &str =
    "border:1px solid #d0d7de;padding:4px 8px;background:#f6f8fa;font-weight:600;";
const TD_STYLE: &str = "border:1px solid #d0d7de;padding:4px 8px;";
const BLOCKQUOTE_STYLE: &str =
    "border-left:3px solid #d0d7de;margin:8px 0;padding:4px 12px;color:#57606a;";
const PRE_STYLE: &str =
    "background:#f6f8fa;padding:8px 12px;border-radius:4px;overflow-x:auto;";

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
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM;

    let mut out = String::with_capacity(markdown.len().saturating_mul(2));
    let mut referenced_cids: HashSet<String> = HashSet::new();
    let mut link_open_stack: Vec<bool> = Vec::new();
    let mut image_stack: Vec<(String, String, String)> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();
    let mut table_alignments: Vec<Alignment> = Vec::new();
    let mut in_table_head = false;
    let mut cell_col: usize = 0;

    for event in Parser::new_ext(markdown, options) {
        if !image_stack.is_empty() {
            match event {
                Event::Text(t) | Event::Code(t) => {
                    if let Some((_, _, alt)) = image_stack.last_mut() {
                        alt.push_str(&t);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some((_, _, alt)) = image_stack.last_mut() {
                        alt.push(' ');
                    }
                }
                Event::End(TagEnd::Image) => {
                    if let Some((dest_url, title, alt)) = image_stack.pop() {
                        if !image_stack.is_empty() {
                            if let Some((_, _, parent_alt)) = image_stack.last_mut() {
                                parent_alt.push_str(&alt);
                            }
                        } else if is_safe_image_url(&dest_url) {
                            let resolved_src =
                                resolve_image_src(&dest_url, cid_aliases, &mut referenced_cids);
                            out.push_str("<img src=\"");
                            out.push_str(&escape_html_attr(&resolved_src));
                            out.push_str("\" alt=\"");
                            out.push_str(&escape_html_attr(&alt));
                            out.push('"');
                            if !title.is_empty() {
                                out.push_str(" title=\"");
                                out.push_str(&escape_html_attr(&title));
                                out.push('"');
                            }
                            out.push_str(" style=\"max-width:100%;height:auto;\">");
                        } else {
                            out.push_str(&escape_html(&alt));
                        }
                    }
                }
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    image_stack.push((dest_url.to_string(), title.to_string(), String::new()));
                }
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    out.push_str(&format!("<h{}>", heading_level_u8(level)));
                }
                Tag::BlockQuote(kind) => {
                    out.push_str(&format!("<blockquote style=\"{BLOCKQUOTE_STYLE}\">"));
                    if let Some(alert) = kind {
                        let label = match alert {
                            BlockQuoteKind::Note => "Note",
                            BlockQuoteKind::Tip => "Tip",
                            BlockQuoteKind::Important => "Important",
                            BlockQuoteKind::Warning => "Warning",
                            BlockQuoteKind::Caution => "Caution",
                        };
                        out.push_str(&format!("<p><strong>{label}</strong></p>"));
                    }
                }
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    code_buf.clear();
                    code_lang = match kind {
                        CodeBlockKind::Fenced(info) => {
                            let lang = info.split_whitespace().next().unwrap_or("").trim();
                            (!lang.is_empty()).then(|| lang.to_string())
                        }
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::List(None) => out.push_str("<ul>"),
                Tag::List(Some(1)) => out.push_str("<ol>"),
                Tag::List(Some(start)) => out.push_str(&format!("<ol start=\"{start}\">")),
                Tag::Item => out.push_str("<li>"),
                Tag::Table(alignments) => {
                    table_alignments = alignments;
                    in_table_head = false;
                    cell_col = 0;
                    out.push_str(&format!("<table style=\"{TABLE_STYLE}\">"));
                }
                Tag::TableHead => {
                    in_table_head = true;
                    cell_col = 0;
                    out.push_str("<thead><tr>");
                }
                Tag::TableRow => {
                    cell_col = 0;
                    out.push_str("<tr>");
                }
                Tag::TableCell => {
                    let tag_name = if in_table_head { "th" } else { "td" };
                    let base_style = if in_table_head { TH_STYLE } else { TD_STYLE };
                    let align_css = match table_alignments.get(cell_col) {
                        Some(Alignment::Left) => "text-align:left;",
                        Some(Alignment::Center) => "text-align:center;",
                        Some(Alignment::Right) => "text-align:right;",
                        _ => "",
                    };
                    out.push_str(&format!(
                        "<{tag_name} style=\"{base_style}{align_css}\">"
                    ));
                }
                Tag::Strong => out.push_str("<strong>"),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link {
                    dest_url, title, ..
                } => {
                    if is_safe_url(&dest_url) {
                        out.push_str("<a href=\"");
                        out.push_str(&escape_html_attr(dest_url.trim()));
                        out.push('"');
                        if !title.is_empty() {
                            out.push_str(" title=\"");
                            out.push_str(&escape_html_attr(&title));
                            out.push('"');
                        }
                        out.push('>');
                        link_open_stack.push(true);
                    } else {
                        link_open_stack.push(false);
                    }
                }
                Tag::Image {
                    dest_url, title, ..
                } => {
                    image_stack.push((dest_url.to_string(), title.to_string(), String::new()));
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(level) => {
                    out.push_str(&format!("</h{}>", heading_level_u8(level)));
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    let trimmed = code_buf.trim_end_matches('\n');
                    let escaped = escape_html(trimmed);
                    if let Some(lang) = code_lang.take() {
                        out.push_str(&format!(
                            "<pre style=\"{PRE_STYLE}\"><code class=\"language-{}\">{escaped}</code></pre>",
                            escape_html_attr(&lang)
                        ));
                    } else {
                        out.push_str(&format!(
                            "<pre style=\"{PRE_STYLE}\"><code>{escaped}</code></pre>"
                        ));
                    }
                    code_buf.clear();
                }
                TagEnd::List(ordered) => {
                    out.push_str(if ordered { "</ol>" } else { "</ul>" });
                }
                TagEnd::Item => out.push_str("</li>"),
                TagEnd::TableHead => {
                    in_table_head = false;
                    out.push_str("</tr></thead><tbody>");
                }
                TagEnd::TableRow => out.push_str("</tr>"),
                TagEnd::TableCell => {
                    out.push_str(if in_table_head { "</th>" } else { "</td>" });
                    cell_col += 1;
                }
                TagEnd::Table => {
                    out.push_str("</tbody></table>");
                    table_alignments.clear();
                }
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link => {
                    if link_open_stack.pop().unwrap_or(false) {
                        out.push_str("</a>");
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else {
                    push_escaped_str(&mut out, &text);
                }
            }
            Event::Code(code) => {
                out.push_str("<code>");
                push_escaped_str(&mut out, &code);
                out.push_str("</code>");
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                if in_code_block {
                    code_buf.push_str(&raw);
                } else {
                    push_escaped_str(&mut out, &raw);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else {
                    out.push_str("<br>");
                }
            }
            Event::Rule => out.push_str("<hr>"),
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }

    (out, referenced_cids)
}

fn heading_level_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn resolve_image_src(
    src: &str,
    cid_aliases: &HashMap<String, String>,
    referenced_cids: &mut HashSet<String>,
) -> String {
    let trimmed = src.trim();
    if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("cid:") {
        let raw_cid = normalize_content_id(&trimmed[4..]);
        if !raw_cid.is_empty() {
            let resolved = cid_aliases
                .get(&raw_cid.to_ascii_lowercase())
                .cloned()
                .unwrap_or(raw_cid);
            referenced_cids.insert(resolved.to_ascii_lowercase());
            return format!("cid:{resolved}");
        }
    }
    trimmed.to_string()
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

fn push_escaped_str(out: &mut String, input: &str) {
    for ch in input.chars() {
        push_escaped_char(out, ch);
    }
}

pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    push_escaped_str(&mut out, input);
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
        assert!(html.contains("<pre style="));
        assert!(html.contains("<code class=\"language-rust\">let x = 1 &lt; 2;</code></pre>"));
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

    #[test]
    fn md_out_gfm_table_tasklist() {
        let md = "| Service | Status |\n| :--- | ---: |\n| Mail | **Healthy** |\n| Sync | `OK` |\n\n- [x] Done\n- [ ] Pending\n\n> Quoted _summary_";
        let html = compile_outgoing_mail_html(md, &[], None);

        assert!(
            html.contains("<table ") && html.contains("</table>"),
            "expected styled <table> in outgoing HTML:\n{html}"
        );
        assert!(html.contains("<thead><tr><th "));
        assert!(html.contains("<strong>Healthy</strong>"));
        assert!(html.contains("<code>OK</code>"));
        assert!(
            html.contains("☑ Done"),
            "expected checked tasklist glyph in outgoing HTML:\n{html}"
        );
        assert!(
            html.contains("☐ Pending"),
            "expected unchecked tasklist glyph in outgoing HTML:\n{html}"
        );
        assert!(
            html.contains("<blockquote style=") && html.contains("<em>summary</em>"),
            "expected styled <blockquote> with italic in outgoing HTML:\n{html}"
        );
    }
}
