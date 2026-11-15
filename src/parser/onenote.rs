//! Microsoft OneNote XHTML <-> Rich AST / GFM Markdown parser and surgical Graph `PATCH` compiler.

use std::{borrow::Cow, collections::HashSet};

use chrono::Utc;
use scraper::{ElementRef, Html, Node, Selector};
use uuid::Uuid;

use crate::models::{
    InlineImageUpload, OnenoteBlock, OnenoteBlockKind, OnenotePageDetail, OnenotePageSummary,
    OnenotePatchCommand, OnenoteResource, OnenoteTagKind, OutgoingOnenotePage, RichSpan,
    TextStyleFlags,
};

// ---------------------------------------------------------------------------
// 1. OneNote XHTML -> OnenotePageDetail (Rich AST + Markdown + Plain Text)
// ---------------------------------------------------------------------------

/// Parses Microsoft Graph OneNote XHTML (`GET .../pages/{id}/content?includeIDs=true`)
/// into structured `OnenoteBlock`s, `OnenoteResource`s, GFM Markdown, and plain text.
pub fn parse_onenote_page_xhtml(
    mut summary: OnenotePageSummary,
    raw_xhtml: &str,
) -> OnenotePageDetail {
    let normalized_xhtml = normalize_xhtml_for_html5ever(raw_xhtml);
    let doc = Html::parse_document(&normalized_xhtml);

    // Populate title from <head><title> if summary.title is blank or placeholder.
    if let Ok(title_sel) = Selector::parse("head > title, title") {
        if let Some(title_el) = doc.select(&title_sel).next() {
            let extracted_title = collapse_html_whitespace(&collect_raw_text(title_el))
                .trim()
                .to_string();
            if !extracted_title.is_empty()
                && (summary.title.trim().is_empty() || summary.title.trim() == "(제목 없음)")
            {
                summary.title = extracted_title;
            }
        }
    }

    let body_el = Selector::parse("body")
        .ok()
        .and_then(|sel| doc.select(&sel).next());

    let primary_div_id = body_el.and_then(|body| {
        body.children()
            .filter_map(ElementRef::wrap)
            .filter(|el| el.value().name() == "div")
            .find_map(|el| non_empty_attr(el, "id"))
    });

    let mut parser = OnenoteXhtmlParser {
        blocks: Vec::new(),
        resources: Vec::new(),
        loose_spans: Vec::new(),
    };

    if !raw_xhtml.trim().is_empty() {
        if let Some(body) = body_el {
            parser.walk_block_container(body, &TextStyleFlags::default());
            parser.flush_loose_paragraph(None, None);
        } else {
            parser.walk_block_container(doc.root_element(), &TextStyleFlags::default());
            parser.flush_loose_paragraph(None, None);
        }
    }

    let blocks = parser.blocks;
    let resources = parser.resources;
    let body_markdown = onenote_blocks_to_markdown(&blocks);
    let body_plain = onenote_blocks_to_plain(&blocks);

    summary.has_todo = blocks
        .iter()
        .any(|b| b.tag.as_ref().is_some_and(OnenoteTagKind::is_todo));

    let needs_preview = summary
        .preview_text
        .as_deref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if needs_preview {
        let compact: String = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
        if !compact.is_empty() {
            let preview: String = compact.chars().take(140).collect();
            summary.preview_text = Some(preview);
        }
    }

    OnenotePageDetail {
        summary,
        raw_xhtml: raw_xhtml.to_string(),
        body_markdown,
        body_plain,
        blocks,
        resources,
        primary_div_id,
        synced_at: Some(Utc::now().to_rfc3339()),
    }
}

/// Renders structured `OnenoteBlock`s into clean GitHub-flavored Markdown.
pub fn onenote_blocks_to_markdown(blocks: &[OnenoteBlock]) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SectionSpacing {
        Normal,
        TightList,
        TightTodo,
    }

    let mut out = String::new();
    let mut prev_spacing: Option<SectionSpacing> = None;

    for block in blocks {
        let (rendered, spacing) = match &block.kind {
            OnenoteBlockKind::Heading { level, spans } => {
                let hashes = "#".repeat((*level).clamp(1, 6) as usize);
                let tag_pfx = block
                    .tag
                    .as_ref()
                    .map(tag_inline_prefix_md)
                    .unwrap_or_default();
                let text = spans_to_markdown(spans);
                let trimmed = text.trim();
                if trimmed.is_empty() && tag_pfx.is_empty() {
                    continue;
                }
                (
                    format!("{hashes} {tag_pfx}{trimmed}"),
                    SectionSpacing::Normal,
                )
            }
            OnenoteBlockKind::Paragraph { spans } => {
                let text = spans_to_markdown(spans);
                let trimmed = text.trim();
                match &block.tag {
                    Some(OnenoteTagKind::Todo { completed }) => {
                        let box_pfx = if *completed { "- [x]" } else { "- [ ]" };
                        let line = if trimmed.is_empty() {
                            box_pfx.to_string()
                        } else {
                            format!("{box_pfx} {trimmed}")
                        };
                        (line, SectionSpacing::TightTodo)
                    }
                    Some(other_tag) => {
                        let pfx = tag_inline_prefix_md(other_tag);
                        let line = format!("{pfx}{trimmed}").trim_end().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        (line, SectionSpacing::Normal)
                    }
                    None => {
                        if trimmed.is_empty() {
                            continue;
                        }
                        (trimmed.to_string(), SectionSpacing::Normal)
                    }
                }
            }
            OnenoteBlockKind::ListItem {
                ordered,
                index,
                depth,
                spans,
            } => {
                let indent = "  ".repeat(*depth as usize);
                let marker = if *ordered {
                    format!("{}.", index.unwrap_or(1))
                } else {
                    "-".to_string()
                };
                let tag_pfx = match &block.tag {
                    Some(OnenoteTagKind::Todo { completed: true }) => "[x] ".to_string(),
                    Some(OnenoteTagKind::Todo { completed: false }) => "[ ] ".to_string(),
                    Some(other) => tag_inline_prefix_md(other),
                    None => String::new(),
                };
                let text = spans_to_markdown(spans);
                let trimmed = text.trim();
                let body = format!("{tag_pfx}{trimmed}");
                let line = if body.is_empty() {
                    format!("{indent}{marker}")
                } else {
                    format!("{indent}{marker} {body}")
                };
                (line, SectionSpacing::TightList)
            }
            OnenoteBlockKind::Quote { spans } => {
                let text = spans_to_markdown(spans);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let quoted = trimmed
                    .lines()
                    .map(|l| {
                        let lt = l.trim_end();
                        if lt.is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {lt}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (quoted, SectionSpacing::Normal)
            }
            OnenoteBlockKind::CodeBlock { lang, code } => {
                let lang_str = lang.as_deref().unwrap_or("");
                let clean = code.trim_end_matches('\n');
                (
                    format!("```{lang_str}\n{clean}\n```"),
                    SectionSpacing::Normal,
                )
            }
            OnenoteBlockKind::Table { headers, rows } => {
                let table_md = render_table_to_markdown(headers, rows);
                if table_md.is_empty() {
                    continue;
                }
                (table_md, SectionSpacing::Normal)
            }
            OnenoteBlockKind::Image {
                resource_index,
                src,
                alt,
                ..
            } => {
                let label = alt
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("image-{}", resource_index + 1));
                (format!("![{label}]({src})"), SectionSpacing::Normal)
            }
            OnenoteBlockKind::Attachment { name, src, .. } => {
                let line = if src.trim().is_empty() {
                    format!("[첨부: {name}]")
                } else {
                    format!("[📎 {name}]({src})")
                };
                (line, SectionSpacing::Normal)
            }
            OnenoteBlockKind::Divider => ("---".to_string(), SectionSpacing::Normal),
        };

        if let Some(prev) = prev_spacing {
            if (prev == SectionSpacing::TightList && spacing == SectionSpacing::TightList)
                || (prev == SectionSpacing::TightTodo && spacing == SectionSpacing::TightTodo)
            {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
        out.push_str(&rendered);
        prev_spacing = Some(spacing);
    }

    out
}

/// Renders structured `OnenoteBlock`s into clean plain text for previews and FTS5 indexing.
pub fn onenote_blocks_to_plain(blocks: &[OnenoteBlock]) -> String {
    let mut parts = Vec::new();

    for block in blocks {
        match &block.kind {
            OnenoteBlockKind::Heading { spans, .. } => {
                let pfx = block
                    .tag
                    .as_ref()
                    .map(tag_inline_prefix_plain)
                    .unwrap_or_default();
                let s = spans_to_plain(spans);
                let line = format!("{pfx}{}", s.trim());
                if !line.trim().is_empty() {
                    parts.push(line.trim().to_string());
                }
            }
            OnenoteBlockKind::Paragraph { spans } => {
                let pfx = block
                    .tag
                    .as_ref()
                    .map(tag_inline_prefix_plain)
                    .unwrap_or_default();
                let s = spans_to_plain(spans);
                let line = format!("{pfx}{}", s.trim());
                if !line.trim().is_empty() {
                    parts.push(line.trim().to_string());
                }
            }
            OnenoteBlockKind::ListItem {
                ordered,
                index,
                depth,
                spans,
            } => {
                let indent = "  ".repeat(*depth as usize);
                let marker = if *ordered {
                    format!("{}.", index.unwrap_or(1))
                } else {
                    "-".to_string()
                };
                let pfx = block
                    .tag
                    .as_ref()
                    .map(tag_inline_prefix_plain)
                    .unwrap_or_default();
                let s = spans_to_plain(spans);
                let body = format!("{pfx}{}", s.trim());
                if !body.trim().is_empty() {
                    parts.push(format!("{indent}{marker} {}", body.trim()));
                }
            }
            OnenoteBlockKind::Quote { spans } => {
                let s = spans_to_plain(spans);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    let quoted = trimmed
                        .lines()
                        .map(|l| format!("> {}", l.trim()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    parts.push(quoted);
                }
            }
            OnenoteBlockKind::CodeBlock { code, .. } => {
                let trimmed = code.trim_end();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            OnenoteBlockKind::Table { headers, rows } => {
                let mut table_lines = Vec::new();
                if !headers.is_empty() {
                    let h = headers
                        .iter()
                        .map(|c| spans_to_plain(c).trim().to_string())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    if !h.trim().is_empty() {
                        table_lines.push(h);
                    }
                }
                for row in rows {
                    let r = row
                        .iter()
                        .map(|c| spans_to_plain(c).trim().to_string())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    if !r.trim().is_empty() {
                        table_lines.push(r);
                    }
                }
                if !table_lines.is_empty() {
                    parts.push(table_lines.join("\n"));
                }
            }
            OnenoteBlockKind::Image {
                resource_index,
                alt,
                ..
            } => match alt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(a) => parts.push(format!("[이미지 #{}: {a}]", resource_index + 1)),
                None => parts.push(format!("[이미지 #{}]", resource_index + 1)),
            },
            OnenoteBlockKind::Attachment { name, .. } => {
                parts.push(format!("[첨부: {name}]"));
            }
            OnenoteBlockKind::Divider => {
                parts.push("---".to_string());
            }
        }
    }

    parts.join("\n")
}

struct InlineCollectResult {
    spans: Vec<RichSpan>,
    media_blocks: Vec<OnenoteBlock>,
}

struct OnenoteXhtmlParser {
    blocks: Vec<OnenoteBlock>,
    resources: Vec<OnenoteResource>,
    loose_spans: Vec<RichSpan>,
}

impl OnenoteXhtmlParser {
    fn walk_block_container(&mut self, parent: ElementRef<'_>, inherited_style: &TextStyleFlags) {
        for child in parent.children() {
            match child.value() {
                Node::Text(t) => {
                    push_inline_text(&mut self.loose_spans, t.text.as_ref(), inherited_style);
                }
                Node::Element(_) => {
                    if let Some(el_ref) = ElementRef::wrap(child) {
                        self.handle_container_element(el_ref, inherited_style);
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_container_element(&mut self, el_ref: ElementRef<'_>, style: &TextStyleFlags) {
        let el = el_ref.value();
        let tag_name = el.name();
        let next_style = apply_css_style(style, el.attr("style"));

        match tag_name {
            "head" | "title" | "meta" | "script" | "style" | "link" => {}
            "html" | "body" => {
                self.flush_loose_paragraph(None, None);
                self.walk_block_container(el_ref, &next_style);
                self.flush_loose_paragraph(None, None);
            }
            "div" => {
                self.flush_loose_paragraph(None, None);
                let div_id = non_empty_attr(el_ref, "id");
                let div_tag = parse_data_tag(el.attr("data-tag"));
                let start_blocks_len = self.blocks.len();
                self.walk_block_container(el_ref, &next_style);
                if self.blocks.len() == start_blocks_len {
                    self.flush_loose_paragraph(div_id, div_tag);
                } else {
                    self.flush_loose_paragraph(None, None);
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_loose_paragraph(None, None);
                let level = tag_name[1..].parse::<u8>().unwrap_or(1).clamp(1, 6);
                let element_id = non_empty_attr(el_ref, "id");
                let block_tag = parse_data_tag(el.attr("data-tag"));
                let raw_html = Some(el_ref.html());
                let res = self.collect_inline(el_ref, &next_style, false);
                if !spans_are_blank(&res.spans) || block_tag.is_some() {
                    self.blocks.push(OnenoteBlock {
                        element_id,
                        tag: block_tag,
                        raw_html,
                        kind: OnenoteBlockKind::Heading {
                            level,
                            spans: trim_spans(res.spans),
                        },
                    });
                }
                self.blocks.extend(res.media_blocks);
            }
            "p" => {
                self.flush_loose_paragraph(None, None);
                let element_id = non_empty_attr(el_ref, "id");
                let mut block_tag = parse_data_tag(el.attr("data-tag"));

                // Multi-line monospace <p style="font-family:Consolas..."> -> CodeBlock
                if block_tag.is_none() && is_monospace_style(el.attr("style")) {
                    let raw_code = collect_raw_text(el_ref);
                    let trimmed_code = raw_code.trim_matches(|c| c == '\r' || c == '\n');
                    if trimmed_code.contains('\n') && !trimmed_code.trim().is_empty() {
                        self.blocks.push(OnenoteBlock {
                            element_id,
                            tag: None,
                            raw_html: Some(el_ref.html()),
                            kind: OnenoteBlockKind::CodeBlock {
                                lang: detect_code_lang(el_ref),
                                code: trimmed_code.to_string(),
                            },
                        });
                        return;
                    }
                }

                let raw_html = Some(el_ref.html());
                let mut res = self.collect_inline(el_ref, &next_style, false);

                if spans_are_blank(&res.spans) && res.media_blocks.len() == 1 && block_tag.is_none()
                {
                    if res.media_blocks[0].element_id.is_none() {
                        res.media_blocks[0].element_id = element_id;
                    }
                    self.blocks.extend(res.media_blocks);
                    return;
                }

                if !spans_are_blank(&res.spans) || block_tag.is_some() {
                    self.blocks.push(OnenoteBlock {
                        element_id,
                        tag: block_tag.take(),
                        raw_html,
                        kind: OnenoteBlockKind::Paragraph {
                            spans: trim_spans(res.spans),
                        },
                    });
                }
                self.blocks.extend(res.media_blocks);
                self.walk_nested_blocks_in_inline_wrapper(el_ref, &next_style, 0);
            }
            "ul" | "ol" => {
                self.flush_loose_paragraph(None, None);
                let ordered = tag_name == "ol";
                self.walk_list(el_ref, ordered, 0, &next_style);
            }
            "li" => {
                // Standalone <li> outside <ul>/<ol>
                self.flush_loose_paragraph(None, None);
                self.handle_list_item(el_ref, false, None, 0, &next_style);
            }
            "blockquote" => {
                self.flush_loose_paragraph(None, None);
                let element_id = non_empty_attr(el_ref, "id");
                let block_tag = parse_data_tag(el.attr("data-tag"));
                let res = self.collect_inline(el_ref, &next_style, false);
                if !spans_are_blank(&res.spans) || block_tag.is_some() {
                    self.blocks.push(OnenoteBlock {
                        element_id,
                        tag: block_tag,
                        raw_html: None,
                        kind: OnenoteBlockKind::Quote {
                            spans: trim_spans(res.spans),
                        },
                    });
                }
                self.blocks.extend(res.media_blocks);
            }
            "pre" => {
                self.flush_loose_paragraph(None, None);
                let element_id = non_empty_attr(el_ref, "id");
                let block_tag = parse_data_tag(el.attr("data-tag"));
                let lang = detect_code_lang(el_ref);
                let raw_code = collect_raw_text(el_ref);
                let trimmed = raw_code.trim_matches(|c| c == '\r' || c == '\n');
                if !trimmed.is_empty() || block_tag.is_some() {
                    self.blocks.push(OnenoteBlock {
                        element_id,
                        tag: block_tag,
                        raw_html: None,
                        kind: OnenoteBlockKind::CodeBlock {
                            lang,
                            code: trimmed.to_string(),
                        },
                    });
                }
            }
            "table" => {
                self.flush_loose_paragraph(None, None);
                if let Some(table_block) = self.parse_table_block(el_ref, &next_style) {
                    self.blocks.push(table_block);
                }
            }
            "img" => {
                self.flush_loose_paragraph(None, None);
                if let Some(img_block) = self.build_image_block(el_ref) {
                    self.blocks.push(img_block);
                }
            }
            "object" => {
                self.flush_loose_paragraph(None, None);
                if let Some(att_block) = self.build_attachment_block(el_ref) {
                    self.blocks.push(att_block);
                }
            }
            "hr" => {
                self.flush_loose_paragraph(None, None);
                self.blocks.push(OnenoteBlock {
                    element_id: non_empty_attr(el_ref, "id"),
                    tag: parse_data_tag(el.attr("data-tag")),
                    raw_html: None,
                    kind: OnenoteBlockKind::Divider,
                });
            }
            "br" => {
                push_raw_span(&mut self.loose_spans, "\n", &TextStyleFlags::default());
            }
            _ => {
                // Inline element directly inside body/div container (e.g. <b>, <span>, <a>)
                let mut media = Vec::new();
                let mut loose = std::mem::take(&mut self.loose_spans);
                self.walk_inline_element(el_ref, style, false, &mut loose, &mut media);
                self.loose_spans = loose;
                if !media.is_empty() {
                    self.flush_loose_paragraph(None, None);
                    self.blocks.extend(media);
                }
            }
        }
    }

    fn walk_list(
        &mut self,
        list_el: ElementRef<'_>,
        ordered: bool,
        depth: u8,
        style: &TextStyleFlags,
    ) {
        let mut next_num = list_el
            .value()
            .attr("start")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1);

        for child in list_el.children() {
            let Some(child_el) = ElementRef::wrap(child) else {
                continue;
            };
            match child_el.value().name() {
                "li" => {
                    if let Some(explicit_val) = child_el
                        .value()
                        .attr("value")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                    {
                        next_num = explicit_val;
                    }
                    let index = if ordered { Some(next_num) } else { None };
                    if ordered {
                        next_num += 1;
                    }
                    self.handle_list_item(child_el, ordered, index, depth, style);
                }
                "ul" | "ol" => {
                    let sub_ordered = child_el.value().name() == "ol";
                    self.walk_list(child_el, sub_ordered, depth.saturating_add(1), style);
                }
                _ => {}
            }
        }
    }

    fn handle_list_item(
        &mut self,
        li_el: ElementRef<'_>,
        ordered: bool,
        index: Option<usize>,
        depth: u8,
        style: &TextStyleFlags,
    ) {
        let li_style = apply_css_style(style, li_el.value().attr("style"));
        let element_id = non_empty_attr(li_el, "id");
        let block_tag = parse_data_tag(li_el.value().attr("data-tag"));
        let raw_html = Some(li_el.html());
        let res = self.collect_inline(li_el, &li_style, true);

        if !spans_are_blank(&res.spans) || block_tag.is_some() {
            self.blocks.push(OnenoteBlock {
                element_id,
                tag: block_tag,
                raw_html,
                kind: OnenoteBlockKind::ListItem {
                    ordered,
                    index,
                    depth,
                    spans: trim_spans(res.spans),
                },
            });
        }
        self.blocks.extend(res.media_blocks);
        self.walk_nested_blocks_in_inline_wrapper(li_el, &li_style, depth.saturating_add(1));
    }

    fn walk_nested_blocks_in_inline_wrapper(
        &mut self,
        wrapper_el: ElementRef<'_>,
        style: &TextStyleFlags,
        list_depth: u8,
    ) {
        for child in wrapper_el.children() {
            let Some(child_el) = ElementRef::wrap(child) else {
                continue;
            };
            match child_el.value().name() {
                "ul" | "ol" => {
                    let sub_ordered = child_el.value().name() == "ol";
                    self.walk_list(child_el, sub_ordered, list_depth, style);
                }
                "table" => {
                    if let Some(tbl) = self.parse_table_block(child_el, style) {
                        self.blocks.push(tbl);
                    }
                }
                "p" | "div" => {
                    self.walk_nested_blocks_in_inline_wrapper(child_el, style, list_depth);
                }
                _ => {}
            }
        }
    }

    fn parse_table_block(
        &mut self,
        table_el: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Option<OnenoteBlock> {
        let element_id = non_empty_attr(table_el, "id");
        let block_tag = parse_data_tag(table_el.value().attr("data-tag"));
        let table_style = apply_css_style(style, table_el.value().attr("style"));

        let mut thead_rows: Vec<(Vec<Vec<RichSpan>>, bool)> = Vec::new();
        let mut body_rows: Vec<(Vec<Vec<RichSpan>>, bool)> = Vec::new();

        for child in table_el.children() {
            let Some(child_el) = ElementRef::wrap(child) else {
                continue;
            };
            match child_el.value().name() {
                "thead" => {
                    for tr_node in child_el.children() {
                        if let Some(tr_el) = ElementRef::wrap(tr_node) {
                            if tr_el.value().name() == "tr" {
                                if let Some(parsed) = self.parse_table_row(tr_el, &table_style) {
                                    thead_rows.push(parsed);
                                }
                            }
                        }
                    }
                }
                "tbody" | "tfoot" => {
                    for tr_node in child_el.children() {
                        if let Some(tr_el) = ElementRef::wrap(tr_node) {
                            if tr_el.value().name() == "tr" {
                                if let Some(parsed) = self.parse_table_row(tr_el, &table_style) {
                                    body_rows.push(parsed);
                                }
                            }
                        }
                    }
                }
                "tr" => {
                    if let Some(parsed) = self.parse_table_row(child_el, &table_style) {
                        body_rows.push(parsed);
                    }
                }
                _ => {}
            }
        }

        if thead_rows.is_empty() && body_rows.is_empty() && block_tag.is_none() {
            return None;
        }

        let (headers, rows) = if !thead_rows.is_empty() {
            let (first_head, _) = thead_rows.remove(0);
            let mut rest_rows: Vec<Vec<Vec<RichSpan>>> =
                thead_rows.into_iter().map(|(r, _)| r).collect();
            rest_rows.extend(body_rows.into_iter().map(|(r, _)| r));
            (first_head, rest_rows)
        } else if !body_rows.is_empty() {
            let (_, first_uses_th) = &body_rows[0];
            let promote_bold_header =
                body_rows.len() >= 2 && is_all_bold_header_row(&body_rows[0].0);
            if *first_uses_th || promote_bold_header {
                let (first_row, _) = body_rows.remove(0);
                let rest: Vec<Vec<Vec<RichSpan>>> = body_rows.into_iter().map(|(r, _)| r).collect();
                (first_row, rest)
            } else {
                let all: Vec<Vec<Vec<RichSpan>>> = body_rows.into_iter().map(|(r, _)| r).collect();
                (Vec::new(), all)
            }
        } else {
            (Vec::new(), Vec::new())
        };

        Some(OnenoteBlock {
            element_id,
            tag: block_tag,
            raw_html: None,
            kind: OnenoteBlockKind::Table { headers, rows },
        })
    }

    fn parse_table_row(
        &mut self,
        tr_el: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Option<(Vec<Vec<RichSpan>>, bool)> {
        let tr_style = apply_css_style(style, tr_el.value().attr("style"));
        let mut cells = Vec::new();
        let mut has_th = false;
        let mut all_th = true;

        for child in tr_el.children() {
            let Some(cell_el) = ElementRef::wrap(child) else {
                continue;
            };
            let cell_name = cell_el.value().name();
            if cell_name != "td" && cell_name != "th" {
                continue;
            }
            if cell_name == "th" {
                has_th = true;
            } else {
                all_th = false;
            }
            let mut cell_style = apply_css_style(&tr_style, cell_el.value().attr("style"));
            if cell_name == "th" {
                cell_style.bold = true;
            }
            cells.push(self.parse_table_cell(cell_el, &cell_style));
        }

        if cells.is_empty() {
            None
        } else {
            Some((cells, has_th && all_th))
        }
    }

    fn parse_table_cell(
        &mut self,
        cell_el: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Vec<RichSpan> {
        let mut segments: Vec<Vec<RichSpan>> = Vec::new();
        let mut inline_buf: Vec<RichSpan> = Vec::new();
        let mut media_blocks: Vec<OnenoteBlock> = Vec::new();

        for child in cell_el.children() {
            match child.value() {
                Node::Text(t) => {
                    push_inline_text(&mut inline_buf, t.text.as_ref(), style);
                }
                Node::Element(el) => {
                    let Some(child_el) = ElementRef::wrap(child) else {
                        continue;
                    };
                    match el.name() {
                        "p" | "div" | "li" | "ul" | "ol" => {
                            if !spans_are_blank(&inline_buf) {
                                segments.push(trim_spans(std::mem::take(&mut inline_buf)));
                            } else {
                                inline_buf.clear();
                            }
                            if matches!(el.name(), "ul" | "ol") {
                                for li_node in child_el.children() {
                                    if let Some(li_el) = ElementRef::wrap(li_node) {
                                        if li_el.value().name() == "li" {
                                            let li_style =
                                                apply_css_style(style, li_el.value().attr("style"));
                                            let res = self.collect_inline(li_el, &li_style, false);
                                            if !spans_are_blank(&res.spans) {
                                                segments.push(trim_spans(res.spans));
                                            }
                                            media_blocks.extend(res.media_blocks);
                                        }
                                    }
                                }
                            } else {
                                let p_style = apply_css_style(style, el.attr("style"));
                                let res = self.collect_inline(child_el, &p_style, false);
                                if !spans_are_blank(&res.spans) {
                                    segments.push(trim_spans(res.spans));
                                }
                                media_blocks.extend(res.media_blocks);
                            }
                        }
                        _ => {
                            self.walk_inline_element(
                                child_el,
                                style,
                                false,
                                &mut inline_buf,
                                &mut media_blocks,
                            );
                        }
                    }
                }
                _ => {}
            }
        }

        if !spans_are_blank(&inline_buf) {
            segments.push(trim_spans(inline_buf));
        }

        // Preserve any images/attachments embedded inside table cells in resources & blocks.
        self.blocks.extend(media_blocks);

        let mut joined: Vec<RichSpan> = Vec::new();
        for (idx, seg) in segments.into_iter().enumerate() {
            if idx > 0 {
                push_raw_span(&mut joined, " / ", &TextStyleFlags::default());
            }
            for mut sp in seg {
                if sp.text.contains('\n') {
                    sp.text = sp.text.replace('\n', " / ");
                }
                push_raw_span(&mut joined, &sp.text, &sp.style);
            }
        }
        trim_spans(joined)
    }

    fn build_image_block(&mut self, el_ref: ElementRef<'_>) -> Option<OnenoteBlock> {
        let el = el_ref.value();
        let src_attr = non_empty_attr(el_ref, "src");
        let fullres_url = non_empty_attr(el_ref, "data-fullres-src");

        // Ignore OneNote tag icon or spacer images with empty src (unless fullres is explicitly given).
        if src_attr.is_none() && fullres_url.is_none() {
            return None;
        }
        if src_attr.is_none() && el.attr("src").is_some() && fullres_url.is_none() {
            return None;
        }

        let width = parse_dimension_px(el.attr("width"));
        let height = parse_dimension_px(el.attr("height"));
        if width == Some(0) || height == Some(0) || (width == Some(1) && height == Some(1)) {
            return None;
        }

        let effective_src = fullres_url.clone().or(src_attr)?;
        let mime_type =
            non_empty_attr(el_ref, "data-src-type").unwrap_or_else(|| "image/png".to_string());
        let alt = non_empty_attr(el_ref, "alt");
        let element_id = non_empty_attr(el_ref, "id");
        let tag = parse_data_tag(el.attr("data-tag"));

        let resource_index = self.resources.len();
        self.resources.push(OnenoteResource {
            element_id: element_id.clone(),
            resource_url: effective_src.clone(),
            fullres_url,
            mime_type,
            file_name: None,
            is_image: true,
            alt_text: alt.clone(),
        });

        Some(OnenoteBlock {
            element_id,
            tag,
            raw_html: None,
            kind: OnenoteBlockKind::Image {
                resource_index,
                src: effective_src,
                alt,
                width,
                height,
            },
        })
    }

    fn build_attachment_block(&mut self, el_ref: ElementRef<'_>) -> Option<OnenoteBlock> {
        let el = el_ref.value();
        let src = non_empty_attr(el_ref, "data").or_else(|| non_empty_attr(el_ref, "src"));
        let name_attr =
            non_empty_attr(el_ref, "data-attachment").or_else(|| non_empty_attr(el_ref, "name"));

        if src.is_none() && name_attr.is_none() {
            return None;
        }

        let src = src.unwrap_or_default();
        let name = name_attr.unwrap_or_else(|| "attachment".to_string());
        let mime_type = non_empty_attr(el_ref, "type")
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let element_id = non_empty_attr(el_ref, "id");
        let tag = parse_data_tag(el.attr("data-tag"));

        let resource_index = self.resources.len();
        self.resources.push(OnenoteResource {
            element_id: element_id.clone(),
            resource_url: src.clone(),
            fullres_url: None,
            mime_type: mime_type.clone(),
            file_name: Some(name.clone()),
            is_image: false,
            alt_text: None,
        });

        Some(OnenoteBlock {
            element_id,
            tag,
            raw_html: None,
            kind: OnenoteBlockKind::Attachment {
                resource_index,
                name,
                src,
                mime_type,
            },
        })
    }

    fn collect_inline(
        &mut self,
        parent: ElementRef<'_>,
        style: &TextStyleFlags,
        skip_nested_lists: bool,
    ) -> InlineCollectResult {
        let mut spans = Vec::new();
        let mut media_blocks = Vec::new();
        self.walk_inline_children(
            parent,
            style,
            skip_nested_lists,
            &mut spans,
            &mut media_blocks,
        );
        InlineCollectResult {
            spans,
            media_blocks,
        }
    }

    fn walk_inline_children(
        &mut self,
        parent: ElementRef<'_>,
        style: &TextStyleFlags,
        skip_nested_lists: bool,
        spans: &mut Vec<RichSpan>,
        media_blocks: &mut Vec<OnenoteBlock>,
    ) {
        for child in parent.children() {
            match child.value() {
                Node::Text(t) => {
                    push_inline_text(spans, t.text.as_ref(), style);
                }
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        self.walk_inline_element(
                            child_el,
                            style,
                            skip_nested_lists,
                            spans,
                            media_blocks,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_inline_element(
        &mut self,
        el_ref: ElementRef<'_>,
        style: &TextStyleFlags,
        skip_nested_lists: bool,
        spans: &mut Vec<RichSpan>,
        media_blocks: &mut Vec<OnenoteBlock>,
    ) {
        let el = el_ref.value();
        let tag_name = el.name();

        if skip_nested_lists && matches!(tag_name, "ul" | "ol") {
            return;
        }
        if matches!(
            tag_name,
            "script" | "style" | "head" | "meta" | "title" | "table"
        ) {
            return;
        }

        let mut next = apply_css_style(style, el.attr("style"));

        match tag_name {
            "br" => {
                push_raw_span(spans, "\n", &TextStyleFlags::default());
            }
            "b" | "strong" => {
                next.bold = true;
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
            "i" | "em" => {
                next.italic = true;
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
            "u" => {
                next.underline = true;
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
            "s" | "del" | "strike" => {
                next.strike = true;
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
            "code" => {
                next.code = true;
                let raw = collect_raw_text(el_ref);
                if !raw.is_empty() {
                    push_raw_span(spans, &raw, &next);
                }
            }
            "a" => {
                if let Some(href) = non_empty_attr(el_ref, "href") {
                    next.link_url = Some(href);
                }
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
            "img" => {
                if let Some(img_block) = self.build_image_block(el_ref) {
                    media_blocks.push(img_block);
                }
            }
            "object" => {
                if let Some(att_block) = self.build_attachment_block(el_ref) {
                    media_blocks.push(att_block);
                }
            }
            "p" | "div" => {
                if !spans_are_blank(spans)
                    && !spans
                        .last()
                        .map(|s| s.text.ends_with('\n'))
                        .unwrap_or(false)
                {
                    push_raw_span(spans, "\n", &TextStyleFlags::default());
                }
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
            _ => {
                self.walk_inline_children(el_ref, &next, skip_nested_lists, spans, media_blocks);
            }
        }
    }

    fn flush_loose_paragraph(
        &mut self,
        element_id: Option<String>,
        block_tag: Option<OnenoteTagKind>,
    ) {
        let spans = std::mem::take(&mut self.loose_spans);
        if !spans_are_blank(&spans) || block_tag.is_some() {
            self.blocks.push(OnenoteBlock {
                element_id,
                tag: block_tag,
                raw_html: None,
                kind: OnenoteBlockKind::Paragraph {
                    spans: trim_spans(spans),
                },
            });
        }
    }
}

fn normalize_xhtml_for_html5ever(raw: &str) -> Cow<'_, str> {
    let bytes = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut last_copied = 0;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if raw[i..].starts_with("<!--") {
            if let Some(end_c) = raw[i + 4..].find("-->") {
                i += 4 + end_c + 3;
                continue;
            } else {
                break;
            }
        }
        if raw[i..].starts_with("<![CDATA[") {
            if let Some(end_cd) = raw[i + 9..].find("]]>") {
                i += 9 + end_cd + 3;
                continue;
            } else {
                break;
            }
        }
        if i + 1 < bytes.len() && matches!(bytes[i + 1], b'/' | b'!' | b'?') {
            i += 2;
            continue;
        }

        let name_start = i + 1;
        if name_start >= bytes.len() || !bytes[name_start].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let mut p = name_start;
        while p < bytes.len()
            && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'-' || bytes[p] == b':')
        {
            p += 1;
        }
        let tag_name = &raw[name_start..p];

        while p < bytes.len() {
            if bytes[p] == b'"' || bytes[p] == b'\'' {
                let q = bytes[p];
                p += 1;
                while p < bytes.len() && bytes[p] != q {
                    p += 1;
                }
                if p < bytes.len() {
                    p += 1;
                }
            } else if bytes[p] == b'/' && p + 1 < bytes.len() && bytes[p + 1] == b'>' {
                if !is_html5_void_tag(tag_name) {
                    if out.is_empty() {
                        out.reserve(raw.len() + 64);
                    }
                    let head = raw[last_copied..p].trim_end_matches(' ');
                    out.push_str(head);
                    out.push('>');
                    out.push_str("</");
                    out.push_str(tag_name);
                    out.push('>');
                    last_copied = p + 2;
                }
                p += 2;
                break;
            } else if bytes[p] == b'>' {
                p += 1;
                break;
            } else if bytes[p] == b'<' {
                break;
            } else {
                p += 1;
            }
        }
        i = p;
    }

    if last_copied == 0 {
        Cow::Borrowed(raw)
    } else {
        out.push_str(&raw[last_copied..]);
        Cow::Owned(out)
    }
}

fn is_html5_void_tag(tag_name: &str) -> bool {
    matches!(
        tag_name.to_ascii_lowercase().as_str(),
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn parse_data_tag(attr: Option<&str>) -> Option<OnenoteTagKind> {
    let raw = attr?.trim();
    if raw.is_empty() {
        return None;
    }

    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }

    // Give `to-do` priority when comma-separated tags exist so checkbox toggling works reliably.
    for part in &parts {
        if part.eq_ignore_ascii_case("to-do:completed") {
            return Some(OnenoteTagKind::Todo { completed: true });
        }
        if part.eq_ignore_ascii_case("to-do") {
            return Some(OnenoteTagKind::Todo { completed: false });
        }
    }

    let first = parts[0];
    if first.eq_ignore_ascii_case("important") {
        Some(OnenoteTagKind::Important)
    } else if first.eq_ignore_ascii_case("question") {
        Some(OnenoteTagKind::Question)
    } else if first.eq_ignore_ascii_case("critical") {
        Some(OnenoteTagKind::Critical)
    } else if first.eq_ignore_ascii_case("highlight") {
        Some(OnenoteTagKind::Highlight)
    } else if first.eq_ignore_ascii_case("idea") {
        Some(OnenoteTagKind::Idea)
    } else {
        Some(OnenoteTagKind::Custom(first.to_string()))
    }
}

fn apply_css_style(base: &TextStyleFlags, style_attr: Option<&str>) -> TextStyleFlags {
    let mut next = base.clone();
    let Some(raw) = style_attr else {
        return next;
    };

    for decl in raw.split(';') {
        let Some((k, v)) = decl.split_once(':') else {
            continue;
        };
        let prop = k.trim().to_ascii_lowercase();
        let val = v.trim().to_ascii_lowercase();
        match prop.as_str() {
            "font-weight" => {
                if val.contains("bold") || matches!(val.as_str(), "700" | "800" | "900") {
                    next.bold = true;
                } else if matches!(val.as_str(), "normal" | "400") {
                    next.bold = false;
                }
            }
            "font-style" => {
                if val.contains("italic") || val.contains("oblique") {
                    next.italic = true;
                } else if val == "normal" {
                    next.italic = false;
                }
            }
            "text-decoration" | "text-decoration-line" => {
                if val == "none" {
                    next.underline = false;
                    next.strike = false;
                } else {
                    if val.contains("underline") {
                        next.underline = true;
                    }
                    if val.contains("line-through") {
                        next.strike = true;
                    }
                }
            }
            "font-family" => {
                if is_monospace_font_value(&val) {
                    next.code = true;
                }
            }
            _ => {}
        }
    }

    next
}

fn is_monospace_style(style_attr: Option<&str>) -> bool {
    let Some(raw) = style_attr else {
        return false;
    };
    for decl in raw.split(';') {
        if let Some((k, v)) = decl.split_once(':') {
            if k.trim().eq_ignore_ascii_case("font-family")
                && is_monospace_font_value(&v.trim().to_ascii_lowercase())
            {
                return true;
            }
        }
    }
    false
}

fn is_monospace_font_value(lower_val: &str) -> bool {
    const MONO_FAMILIES: &[&str] = &[
        "consolas",
        "monospace",
        "courier",
        "menlo",
        "monaco",
        "fira code",
        "jetbrains mono",
        "source code pro",
        "cascadia",
    ];
    MONO_FAMILIES.iter().any(|f| lower_val.contains(f))
}

fn is_all_bold_header_row(cells: &[Vec<RichSpan>]) -> bool {
    if cells.is_empty() {
        return false;
    }
    let mut has_non_empty = false;
    for cell in cells {
        for sp in cell {
            if !sp.text.trim().is_empty() {
                has_non_empty = true;
                if !sp.style.bold {
                    return false;
                }
            }
        }
    }
    has_non_empty
}

fn detect_code_lang(el_ref: ElementRef<'_>) -> Option<String> {
    let el = el_ref.value();
    if let Some(lang) = el
        .attr("data-lang")
        .or_else(|| el.attr("lang"))
        .and_then(non_empty_str)
    {
        return Some(lang);
    }
    if let Some(lang) = lang_from_class(el.attr("class")) {
        return Some(lang);
    }
    for child in el_ref.children() {
        if let Node::Element(child_el) = child.value() {
            if child_el.name() == "code" {
                if let Some(lang) = child_el
                    .attr("data-lang")
                    .or_else(|| child_el.attr("lang"))
                    .and_then(non_empty_str)
                {
                    return Some(lang);
                }
                if let Some(lang) = lang_from_class(child_el.attr("class")) {
                    return Some(lang);
                }
            }
        }
    }
    None
}

fn lang_from_class(class_attr: Option<&str>) -> Option<String> {
    let classes = class_attr?;
    for cls in classes.split_whitespace() {
        if let Some(rest) = cls
            .strip_prefix("language-")
            .or_else(|| cls.strip_prefix("lang-"))
        {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn parse_dimension_px(attr: Option<&str>) -> Option<u32> {
    let s = attr?.trim();
    if s.is_empty() {
        return None;
    }
    let stripped = s.strip_suffix("px").unwrap_or(s).trim();
    if let Ok(v) = stripped.parse::<u32>() {
        return Some(v);
    }
    if let Ok(f) = stripped.parse::<f64>() {
        if f >= 0.0 {
            return Some(f.round() as u32);
        }
    }
    None
}

fn non_empty_attr(el_ref: ElementRef<'_>, name: &str) -> Option<String> {
    el_ref.value().attr(name).and_then(non_empty_str)
}

fn non_empty_str(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn collect_raw_text(el_ref: ElementRef<'_>) -> String {
    let mut out = String::new();
    collect_raw_text_inner(el_ref, &mut out);
    out
}

fn collect_raw_text_inner(el_ref: ElementRef<'_>, out: &mut String) {
    for child in el_ref.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t.text.as_ref()),
            Node::Element(el) => {
                if el.name() == "br" {
                    out.push('\n');
                } else if let Some(child_el) = ElementRef::wrap(child) {
                    collect_raw_text_inner(child_el, out);
                    if matches!(el.name(), "p" | "div") && !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
}

fn collapse_html_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn push_inline_text(spans: &mut Vec<RichSpan>, raw: &str, style: &TextStyleFlags) {
    let collapsed = collapse_html_whitespace(raw);
    if collapsed.is_empty() {
        return;
    }
    let prev_ends_ws = spans.is_empty()
        || spans
            .last()
            .map(|s| s.text.ends_with('\n') || s.text.ends_with(' '))
            .unwrap_or(false);

    let to_push = if collapsed == " " && prev_ends_ws {
        return;
    } else if prev_ends_ws {
        collapsed.trim_start().to_string()
    } else {
        collapsed
    };

    if !to_push.is_empty() {
        push_raw_span(spans, &to_push, style);
    }
}

fn push_raw_span(spans: &mut Vec<RichSpan>, text: &str, style: &TextStyleFlags) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if &last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(RichSpan {
        text: text.to_string(),
        style: style.clone(),
    });
}

fn spans_are_blank(spans: &[RichSpan]) -> bool {
    spans.iter().all(|s| s.text.trim().is_empty())
}

fn trim_spans(mut spans: Vec<RichSpan>) -> Vec<RichSpan> {
    while let Some(first) = spans.first_mut() {
        let trimmed = first.text.trim_start_matches([' ', '\t', '\n', '\r']);
        if trimmed.is_empty() {
            spans.remove(0);
        } else {
            first.text = trimmed.to_string();
            break;
        }
    }
    while let Some(last) = spans.last_mut() {
        let trimmed = last.text.trim_end_matches([' ', '\t', '\n', '\r']);
        if trimmed.is_empty() {
            spans.pop();
        } else {
            last.text = trimmed.to_string();
            break;
        }
    }
    spans
}

fn spans_to_plain(spans: &[RichSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn spans_to_markdown(spans: &[RichSpan]) -> String {
    let mut out = String::new();
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let mut formatted = if span.style.code {
            if span.text.contains('`') {
                format!("`` {} ``", span.text)
            } else {
                format!("`{}`", span.text)
            }
        } else {
            span.text.clone()
        };

        if span.style.bold && !span.style.code {
            formatted = format!("**{formatted}**");
        }
        if span.style.italic && !span.style.code {
            formatted = format!("*{formatted}*");
        }
        if span.style.strike && !span.style.code {
            formatted = format!("~~{formatted}~~");
        }
        if span.style.underline && !span.style.code {
            formatted = format!("<u>{formatted}</u>");
        }
        if let Some(url) = &span.style.link_url {
            formatted = format!("[{formatted}]({url})");
        }
        out.push_str(&formatted);
    }
    out
}

fn spans_to_onenote_xhtml(spans: &[RichSpan]) -> String {
    let mut out = String::new();
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let inner = escape_xml(&span.text).replace('\n', "<br />");
        let mut chunk = if span.style.code {
            format!("<code>{inner}</code>")
        } else {
            inner
        };
        if span.style.bold && !span.style.code {
            chunk = format!("<b>{chunk}</b>");
        }
        if span.style.italic && !span.style.code {
            chunk = format!("<i>{chunk}</i>");
        }
        if span.style.underline && !span.style.code {
            chunk = format!("<u>{chunk}</u>");
        }
        if span.style.strike && !span.style.code {
            chunk = format!("<s>{chunk}</s>");
        }
        if let Some(url) = &span.style.link_url {
            chunk = format!("<a href=\"{}\">{chunk}</a>", escape_xml_attr(url));
        }
        out.push_str(&chunk);
    }
    out
}

fn render_table_to_markdown(headers: &[Vec<RichSpan>], rows: &[Vec<Vec<RichSpan>>]) -> String {
    let col_count = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if col_count == 0 {
        return String::new();
    }

    let format_row = |cells: &[Vec<RichSpan>], strip_outer_bold: bool| -> String {
        let mut cols = Vec::with_capacity(col_count);
        for idx in 0..col_count {
            let cell_md = cells
                .get(idx)
                .map(|spans| {
                    if strip_outer_bold {
                        let plain_headers: Vec<RichSpan> = spans
                            .iter()
                            .map(|sp| {
                                let mut c = sp.clone();
                                c.style.bold = false;
                                c
                            })
                            .collect();
                        spans_to_markdown(&plain_headers)
                    } else {
                        spans_to_markdown(spans)
                    }
                })
                .unwrap_or_default();
            let escaped = cell_md.trim().replace('|', "\\|").replace('\n', "<br />");
            cols.push(escaped);
        }
        format!("| {} |", cols.join(" | "))
    };

    let sep_row = format!("| {} |", vec!["---"; col_count].join(" | "));
    let mut lines = Vec::new();

    if !headers.is_empty() {
        lines.push(format_row(headers, true));
        lines.push(sep_row);
        for r in rows {
            lines.push(format_row(r, false));
        }
    } else if let Some((first, rest)) = rows.split_first() {
        lines.push(format_row(first, false));
        lines.push(sep_row);
        for r in rest {
            lines.push(format_row(r, false));
        }
    }

    lines.join("\n")
}

fn tag_inline_prefix_md(tag: &OnenoteTagKind) -> String {
    match tag {
        OnenoteTagKind::Todo { completed: false } => "[ ] ".to_string(),
        OnenoteTagKind::Todo { completed: true } => "[x] ".to_string(),
        OnenoteTagKind::Important => "[★] ".to_string(),
        OnenoteTagKind::Question => "[?] ".to_string(),
        OnenoteTagKind::Critical => "[!] ".to_string(),
        OnenoteTagKind::Highlight => "[==] ".to_string(),
        OnenoteTagKind::Idea => "[💡] ".to_string(),
        OnenoteTagKind::Custom(c) => format!("[{c}] "),
    }
}

fn tag_inline_prefix_plain(tag: &OnenoteTagKind) -> String {
    tag_inline_prefix_md(tag)
}

// ---------------------------------------------------------------------------
// 2. GFM Markdown -> Valid OneNote XHTML Fragment Compiler
// ---------------------------------------------------------------------------

/// Converts GitHub-flavored Markdown + inline image references into a clean,
/// valid OneNote XHTML fragment (with self-closing `<br />`, `<hr />`, `<img ... />`).
pub fn markdown_to_onenote_html_fragment(
    markdown: &str,
    inline_images: &[InlineImageUpload],
) -> String {
    let normalized = markdown.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed_start = line.trim_start();
        let trimmed = line.trim();

        // Blank line
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block: ```lang ... ```
        if let Some(fence_rest) = trimmed_start.strip_prefix("```") {
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
            let escaped_code = escape_xml(&code_lines.join("\n"));
            if lang.is_empty() {
                blocks.push(format!("<pre><code>{escaped_code}</code></pre>"));
            } else {
                blocks.push(format!(
                    "<pre><code class=\"language-{}\">{escaped_code}</code></pre>",
                    escape_xml_attr(lang)
                ));
            }
            continue;
        }

        // GFM Pipe Table: | A | B | \n | --- | --- |
        if i + 1 < lines.len() && is_gfm_table_start(line, lines[i + 1]) {
            let header_cells = split_gfm_table_cells(line);
            let col_count = header_cells.len();
            i += 2; // consume header + separator line

            let mut table_html = String::from("<table border=\"1\"><tr>");
            for hc in &header_cells {
                table_html.push_str("<th>");
                table_html.push_str(&render_onenote_inline(hc));
                table_html.push_str("</th>");
            }
            table_html.push_str("</tr>");

            while i < lines.len() {
                let row_line = lines[i];
                if row_line.trim().is_empty()
                    || !row_line.contains('|')
                    || row_line.trim_start().starts_with("```")
                {
                    break;
                }
                let row_cells = split_gfm_table_cells(row_line);
                table_html.push_str("<tr>");
                for col_idx in 0..col_count {
                    table_html.push_str("<td>");
                    if let Some(cell_text) = row_cells.get(col_idx) {
                        table_html.push_str(&render_onenote_inline(cell_text));
                    }
                    table_html.push_str("</td>");
                }
                table_html.push_str("</tr>");
                i += 1;
            }
            table_html.push_str("</table>");
            blocks.push(table_html);
            continue;
        }

        // Horizontal rule: ---, ***, ___
        if is_horizontal_rule(trimmed) {
            blocks.push("<hr />".to_string());
            i += 1;
            continue;
        }

        // ATX Heading: # .. ######
        if let Some((level, heading_body)) = parse_atx_heading(trimmed_start) {
            let (tag_attr, clean_body) = match parse_tag_prefix(heading_body) {
                Some((data_tag, rest)) => (format!(" data-tag=\"{data_tag}\""), rest),
                None => (String::new(), heading_body),
            };
            blocks.push(format!(
                "<h{level}{tag_attr}>{}</h{level}>",
                render_onenote_inline(clean_body)
            ));
            i += 1;
            continue;
        }

        // Top-level task list item: - [ ] text / * [ ] text / - [x] text / - [X] text
        if leading_indent_spaces(line) < 2 {
            if let Some((completed, task_text)) = parse_task_list_line(trimmed_start) {
                let tag_val = if completed {
                    "to-do:completed"
                } else {
                    "to-do"
                };
                blocks.push(format!(
                    "<p data-tag=\"{tag_val}\">{}</p>",
                    render_onenote_inline(task_text)
                ));
                i += 1;
                continue;
            }
        }

        // Tag prefix paragraph: [★] / [!IMPORTANT] / [?] / etc.
        if let Some((data_tag, rest_text)) = parse_tag_prefix(trimmed_start) {
            blocks.push(format!(
                "<p data-tag=\"{data_tag}\">{}</p>",
                render_onenote_inline(rest_text)
            ));
            i += 1;
            continue;
        }

        // Blockquote: > ...
        if trimmed_start.starts_with("> ") || trimmed_start == ">" {
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let qt = lines[i].trim_start();
                if qt.starts_with("> ") || qt == ">" {
                    let content = qt
                        .strip_prefix("> ")
                        .or_else(|| qt.strip_prefix('>'))
                        .unwrap_or("");
                    quote_lines.push(render_onenote_inline(content));
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(format!(
                "<blockquote>{}</blockquote>",
                quote_lines.join("<br />")
            ));
            continue;
        }

        // Unordered or ordered list (including nested indented items)
        if parse_list_item_line(line, false).is_some() {
            let mut list_items = Vec::new();
            while i < lines.len() {
                let allow_top_task = false;
                if let Some(item) = parse_list_item_line(lines[i], allow_top_task) {
                    list_items.push(item);
                    i += 1;
                } else {
                    break;
                }
            }
            let mut cursor = 0;
            while cursor < list_items.len() {
                let base_indent = list_items[cursor].indent;
                blocks.push(build_nested_list_xhtml(&list_items, &mut cursor, base_indent));
            }
            continue;
        }

        // Standard paragraph until next block boundary
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let cur = lines[i];
            let cur_ts = cur.trim_start();
            let cur_t = cur.trim();
            if cur_t.is_empty()
                || cur_ts.starts_with("```")
                || is_horizontal_rule(cur_t)
                || parse_atx_heading(cur_ts).is_some()
                || (leading_indent_spaces(cur) < 2 && parse_task_list_line(cur_ts).is_some())
                || parse_tag_prefix(cur_ts).is_some()
                || cur_ts.starts_with("> ")
                || cur_ts == ">"
                || parse_list_item_line(cur, false).is_some()
                || (i + 1 < lines.len() && is_gfm_table_start(cur, lines[i + 1]))
            {
                break;
            }
            para_lines.push(render_onenote_inline(cur.trim_end()));
            i += 1;
        }
        if !para_lines.is_empty() {
            blocks.push(format!("<p>{}</p>", para_lines.join("<br />")));
        }
    }

    let mut fragment = blocks.join("");

    // Append any inline_images not already referenced via `name:{temp_id}`
    for img in inline_images {
        let escaped_id = escape_xml_attr(img.temp_id.trim());
        if escaped_id.is_empty() {
            continue;
        }
        let needle = format!("src=\"name:{escaped_id}\"");
        if !fragment.contains(&needle) {
            let alt = img
                .file_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(img.temp_id.trim());
            fragment.push_str(&format!(
                "<p><img src=\"name:{escaped_id}\" alt=\"{}\" /></p>",
                escape_xml_attr(alt)
            ));
        }
    }

    fragment
}

#[derive(Debug)]
struct ParsedListLine<'a> {
    indent: usize,
    ordered: bool,
    task_tag: Option<&'static str>,
    content: &'a str,
}

fn leading_indent_spaces(line: &str) -> usize {
    let mut count = 0;
    for ch in line.chars() {
        match ch {
            ' ' => count += 1,
            '\t' => count += 2,
            _ => break,
        }
    }
    count
}

fn parse_task_list_line(trimmed_start: &str) -> Option<(bool, &str)> {
    let after_bullet = trimmed_start
        .strip_prefix("- ")
        .or_else(|| trimmed_start.strip_prefix("* "))
        .or_else(|| trimmed_start.strip_prefix("+ "))?;
    let rest_trim = after_bullet.trim_start();
    if let Some(after_box) = rest_trim.strip_prefix("[ ]") {
        if after_box.is_empty() || after_box.starts_with(char::is_whitespace) {
            return Some((false, after_box.trim_start()));
        }
    }
    if let Some(after_box) = rest_trim
        .strip_prefix("[x]")
        .or_else(|| rest_trim.strip_prefix("[X]"))
    {
        if after_box.is_empty() || after_box.starts_with(char::is_whitespace) {
            return Some((true, after_box.trim_start()));
        }
    }
    None
}

fn parse_tag_prefix(trimmed_start: &str) -> Option<(&'static str, &str)> {
    const PREFIXES: &[(&str, &str, bool)] = &[
        ("[★]", "important", false),
        ("[!IMPORTANT]", "important", true),
        ("[?]", "question", false),
        ("[!QUESTION]", "question", true),
        ("[!]", "critical", false),
        ("[!CRITICAL]", "critical", true),
        ("[==]", "highlight", false),
        ("[!HIGHLIGHT]", "highlight", true),
        ("[💡]", "idea", false),
        ("[!IDEA]", "idea", true),
    ];

    for &(pfx, tag_name, ascii_ci) in PREFIXES {
        let matched = if ascii_ci {
            trimmed_start
                .get(..pfx.len())
                .map(|head| head.eq_ignore_ascii_case(pfx))
                .unwrap_or(false)
        } else {
            trimmed_start.starts_with(pfx)
        };
        if matched {
            let after = &trimmed_start[pfx.len()..];
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                return Some((tag_name, after.trim_start()));
            }
        }
    }
    None
}

fn parse_atx_heading(trimmed_start: &str) -> Option<(u8, &str)> {
    let hashes = trimmed_start.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &trimmed_start[hashes..];
    if after.is_empty() {
        return Some((hashes as u8, ""));
    }
    if after.starts_with(char::is_whitespace) {
        let content = after.trim().trim_end_matches('#').trim_end();
        return Some((hashes as u8, content));
    }
    None
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if matches!(ch, '-' | '*' | '_') {
            if let Some(m) = marker {
                if m != ch {
                    return false;
                }
            } else {
                marker = Some(ch);
            }
            count += 1;
        } else {
            return false;
        }
    }
    count >= 3
}

fn parse_list_item_line(line: &str, allow_top_level_task: bool) -> Option<ParsedListLine<'_>> {
    let indent = leading_indent_spaces(line);
    let trimmed = line.trim_start();

    // Unordered bullet
    if let Some(after_bullet) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        if let Some((completed, task_body)) = parse_checkbox_prefix(after_bullet) {
            if indent < 2 && !allow_top_level_task {
                return None;
            }
            return Some(ParsedListLine {
                indent,
                ordered: false,
                task_tag: Some(if completed { "to-do:completed" } else { "to-do" }),
                content: task_body,
            });
        }
        return Some(ParsedListLine {
            indent,
            ordered: false,
            task_tag: None,
            content: after_bullet.trim_end(),
        });
    }

    // Ordered bullet: `1. text` or `1) text`
    let digit_count = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if (1..=9).contains(&digit_count) {
        let after_digits = &trimmed[digit_count..];
        if let Some(after_marker) = after_digits
            .strip_prefix(". ")
            .or_else(|| after_digits.strip_prefix(") "))
        {
            if let Some((completed, task_body)) = parse_checkbox_prefix(after_marker) {
                return Some(ParsedListLine {
                    indent,
                    ordered: true,
                    task_tag: Some(if completed { "to-do:completed" } else { "to-do" }),
                    content: task_body,
                });
            }
            return Some(ParsedListLine {
                indent,
                ordered: true,
                task_tag: None,
                content: after_marker.trim_end(),
            });
        }
    }

    None
}

fn parse_checkbox_prefix(s: &str) -> Option<(bool, &str)> {
    let ts = s.trim_start();
    if let Some(rest) = ts.strip_prefix("[ ]") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some((false, rest.trim_start().trim_end()));
        }
    }
    if let Some(rest) = ts.strip_prefix("[x]").or_else(|| ts.strip_prefix("[X]")) {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some((true, rest.trim_start().trim_end()));
        }
    }
    None
}

fn build_nested_list_xhtml(
    items: &[ParsedListLine<'_>],
    idx: &mut usize,
    base_indent: usize,
) -> String {
    let list_ordered = items[*idx].ordered;
    let list_tag = if list_ordered { "ol" } else { "ul" };
    let mut out = format!("<{list_tag}>");

    while *idx < items.len() {
        let cur = &items[*idx];
        if cur.indent < base_indent {
            break;
        }
        if cur.indent == base_indent && cur.ordered != list_ordered {
            break;
        }

        let item_indent = cur.indent;
        let tag_attr = match cur.task_tag {
            Some(t) => format!(" data-tag=\"{t}\""),
            None => String::new(),
        };
        let rendered_body = render_onenote_inline(cur.content);
        *idx += 1;

        out.push_str(&format!("<li{tag_attr}>{rendered_body}"));
        while *idx < items.len() && items[*idx].indent > item_indent {
            let child_indent = items[*idx].indent;
            out.push_str(&build_nested_list_xhtml(items, idx, child_indent));
        }
        out.push_str("</li>");
    }

    out.push_str(&format!("</{list_tag}>"));
    out
}

fn is_gfm_table_start(header_line: &str, sep_line: &str) -> bool {
    if !header_line.contains('|') || !sep_line.contains('|') {
        return false;
    }
    let headers = split_gfm_table_cells(header_line);
    let seps = split_gfm_table_cells(sep_line);
    if headers.is_empty() || headers.len() != seps.len() {
        return false;
    }
    seps.iter().all(|cell| {
        let t = cell.trim();
        let without_left = t.strip_prefix(':').unwrap_or(t);
        let core = without_left.strip_suffix(':').unwrap_or(without_left);
        !core.is_empty() && core.chars().all(|c| c == '-')
    })
}

fn split_gfm_table_cells(line: &str) -> Vec<String> {
    let mut s = line.trim();
    if let Some(stripped) = s.strip_prefix('|') {
        s = stripped;
    }
    if s.ends_with('|') && !s.ends_with("\\|") {
        s = &s[..s.len() - 1];
    }

    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_code = false;

    while let Some(ch) = chars.next() {
        if ch == '`' {
            in_code = !in_code;
            cur.push(ch);
        } else if ch == '\\' && !in_code && chars.peek() == Some(&'|') {
            chars.next();
            cur.push('|');
        } else if ch == '|' && !in_code {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn render_onenote_inline(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        let rest = &input[i..];

        // Double-backtick inline code: ``code``
        if let Some(after_ticks) = rest.strip_prefix("``") {
            if let Some(end_idx) = after_ticks.find("``") {
                let raw_code = &after_ticks[..end_idx];
                let trimmed = raw_code
                    .strip_prefix(' ')
                    .and_then(|s| s.strip_suffix(' '))
                    .unwrap_or(raw_code);
                out.push_str("<code>");
                out.push_str(&escape_xml(trimmed));
                out.push_str("</code>");
                i += 2 + end_idx + 2;
                continue;
            }
        }

        // Single-backtick inline code: `code`
        if let Some(after_tick) = rest.strip_prefix('`') {
            if let Some(end_idx) = after_tick.find('`') {
                let raw_code = &after_tick[..end_idx];
                out.push_str("<code>");
                out.push_str(&escape_xml(raw_code));
                out.push_str("</code>");
                i += 1 + end_idx + 1;
                continue;
            }
        }

        // Explicit inline <br>, <br/>, <br /> (useful inside GFM table cells)
        if let Some(consumed_br) = match_inline_br_tag(rest) {
            out.push_str("<br />");
            i += consumed_br;
            continue;
        }

        // Explicit safe <u>...</u> underline round-trip
        if let Some(after_u) = rest.strip_prefix("<u>") {
            if let Some(end_idx) = after_u.find("</u>") {
                if end_idx > 0 {
                    let inner = &after_u[..end_idx];
                    out.push_str("<u>");
                    out.push_str(&render_onenote_inline(inner));
                    out.push_str("</u>");
                    i += 3 + end_idx + 4;
                    continue;
                }
            }
        }

        // Markdown inline image: ![alt](url)
        if let Some(after_bang) = rest.strip_prefix('!') {
            if after_bang.starts_with('[') {
                if let Some((alt, url, consumed)) = parse_markdown_link_or_image(after_bang, true) {
                    if is_safe_onenote_image_src(url) {
                        out.push_str(&format!(
                            "<img src=\"{}\" alt=\"{}\" />",
                            escape_xml_attr(url),
                            escape_xml_attr(alt)
                        ));
                        i += 1 + consumed;
                        continue;
                    }
                }
            }
        }

        // Markdown link: [label](url)
        if rest.starts_with('[') {
            if let Some((label, url, consumed)) = parse_markdown_link_or_image(rest, false) {
                if is_safe_onenote_link_url(url) {
                    out.push_str(&format!(
                        "<a href=\"{}\">{}</a>",
                        escape_xml_attr(url),
                        render_onenote_inline(label)
                    ));
                    i += consumed;
                    continue;
                }
            }
        }

        // Bold + Italic: ***...***
        if let Some(after_3) = rest.strip_prefix("***") {
            if let Some(end_idx) = after_3.find("***") {
                if end_idx > 0 {
                    let inner = &after_3[..end_idx];
                    out.push_str("<b><i>");
                    out.push_str(&render_onenote_inline(inner));
                    out.push_str("</i></b>");
                    i += 3 + end_idx + 3;
                    continue;
                }
            }
        }

        // Bold: **...**
        if let Some(after_stars) = rest.strip_prefix("**") {
            if let Some(end_idx) = after_stars.find("**") {
                if end_idx > 0 {
                    let inner = &after_stars[..end_idx];
                    out.push_str("<b>");
                    out.push_str(&render_onenote_inline(inner));
                    out.push_str("</b>");
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
                    out.push_str(&render_onenote_inline(inner));
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
                    out.push_str("<i>");
                    out.push_str(&render_onenote_inline(inner));
                    out.push_str("</i>");
                    i += 1 + end_idx + 1;
                    continue;
                }
            }
        }

        let ch = rest.chars().next().unwrap();
        push_xml_escaped_char(&mut out, ch);
        i += ch.len_utf8();
    }

    out
}

fn match_inline_br_tag(rest: &str) -> Option<usize> {
    const BR_FORMS: &[&str] = &["<br />", "<br/>", "<br>"];
    for form in BR_FORMS {
        if rest
            .get(..form.len())
            .map(|h| h.eq_ignore_ascii_case(form))
            .unwrap_or(false)
        {
            return Some(form.len());
        }
    }
    None
}

fn parse_markdown_link_or_image(
    rest: &str,
    allow_empty_label: bool,
) -> Option<(&str, &str, usize)> {
    let after_open = rest.strip_prefix('[')?;
    let close_bracket = after_open.find("](")?;
    let label = &after_open[..close_bracket];
    if !allow_empty_label && label.is_empty() {
        return None;
    }
    let after_mid = &after_open[close_bracket + 2..];
    let close_paren = after_mid.find(')')?;
    let url = after_mid[..close_paren].trim();
    if url.is_empty() {
        return None;
    }
    let consumed = 1 + close_bracket + 2 + close_paren + 1;
    Some((label, url, consumed))
}

fn is_safe_onenote_link_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
        || lower.starts_with("onenote:")
}

fn is_safe_onenote_image_src(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("name:")
        || lower.starts_with("data:image/")
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

fn push_xml_escaped_char(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#39;"),
        // Strip disallowed XML 1.0 control characters while keeping \t, \n, \r.
        c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
        c => out.push(c),
    }
}

fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        push_xml_escaped_char(&mut out, ch);
    }
    out
}

fn escape_xml_attr(input: &str) -> String {
    escape_xml(input)
}

// ---------------------------------------------------------------------------
// 3. New Page XHTML / Multipart Payload Compiler (`POST .../sections/{id}/pages`)
// ---------------------------------------------------------------------------

/// Compiles an `OutgoingOnenotePage` into `(body_bytes, content_type_header)` ready for
/// `POST /{scope}/onenote/sections/{id}/pages`.
pub fn compile_new_page_payload(outgoing: &OutgoingOnenotePage) -> (Vec<u8>, String) {
    let escaped_title = escape_xml(outgoing.title.trim());
    let created_iso = Utc::now().to_rfc3339();
    let body_fragment =
        markdown_to_onenote_html_fragment(&outgoing.markdown_body, &outgoing.inline_images);

    let xhtml = format!(
        "<!DOCTYPE html>\n<html>\n  <head>\n    <title>{escaped_title}</title>\n    <meta name=\"created\" content=\"{created_iso}\" />\n  </head>\n  <body>\n    {body_fragment}\n  </body>\n</html>"
    );

    if outgoing.inline_images.is_empty() {
        (
            xhtml.into_bytes(),
            "application/xhtml+xml; charset=utf-8".to_string(),
        )
    } else {
        build_multipart_payload(
            "Presentation",
            "application/xhtml+xml; charset=utf-8",
            xhtml.as_bytes(),
            &outgoing.inline_images,
        )
    }
}

// ---------------------------------------------------------------------------
// 4. Surgical `PATCH` Builders (Todo Toggle, Set Tag, Append, Replace, Edit Page)
// ---------------------------------------------------------------------------

/// Toggles an existing `OnenoteTagKind::Todo` block between unchecked (`to-do`) and
/// completed (`to-do:completed`), preserving all inner links/styles in `raw_html`.
/// Returns `None` if `block.element_id` is absent or `block.tag` is not a `Todo`.
pub fn build_toggle_todo_patch(block: &OnenoteBlock) -> Option<(OnenotePatchCommand, bool)> {
    let target_id = block.element_id.as_deref()?.trim();
    if target_id.is_empty() {
        return None;
    }

    let new_completed = match &block.tag {
        Some(OnenoteTagKind::Todo { completed }) => !completed,
        _ => return None,
    };

    let updated_html = block
        .raw_html
        .as_deref()
        .and_then(|raw| {
            rewrite_opening_tag_data_tag(
                raw,
                TagRewriteMode::ToggleTodo {
                    completed: new_completed,
                },
            )
        })
        .unwrap_or_else(|| {
            reconstruct_block_xhtml_with_tag(
                block,
                target_id,
                Some(&OnenoteTagKind::Todo {
                    completed: new_completed,
                }),
            )
        });

    let cmd = OnenotePatchCommand {
        target: target_id.to_string(),
        action: "replace".to_string(),
        position: None,
        content: updated_html,
    };
    Some((cmd, new_completed))
}

/// Sets or clears any `OnenoteTagKind` on a block that has a valid `element_id`,
/// surgically updating `data-tag` on `raw_html` when available.
pub fn build_set_tag_patch(
    block: &OnenoteBlock,
    new_tag: Option<OnenoteTagKind>,
) -> Option<OnenotePatchCommand> {
    let target_id = block.element_id.as_deref()?.trim();
    if target_id.is_empty() {
        return None;
    }

    let updated_html = block
        .raw_html
        .as_deref()
        .and_then(|raw| rewrite_opening_tag_data_tag(raw, TagRewriteMode::Set(new_tag.as_ref())))
        .unwrap_or_else(|| reconstruct_block_xhtml_with_tag(block, target_id, new_tag.as_ref()));

    Some(OnenotePatchCommand {
        target: target_id.to_string(),
        action: "replace".to_string(),
        position: None,
        content: updated_html,
    })
}

/// Builds `(body_bytes, content_type)` to append Markdown (+ optional inline images)
/// to `primary_div_id` (falling back to `"body"` when absent) via `PATCH .../pages/{id}/content`.
pub fn build_append_markdown_payload(
    primary_div_id: Option<&str>,
    markdown: &str,
    inline_images: &[InlineImageUpload],
) -> (Vec<u8>, String) {
    let target = primary_div_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("body");
    let html = markdown_to_onenote_html_fragment(markdown, inline_images);
    let content = if html.is_empty() {
        "<p></p>".to_string()
    } else {
        html
    };

    let cmd = OnenotePatchCommand {
        target: target.to_string(),
        action: "append".to_string(),
        position: None,
        content,
    };
    let json_bytes = serde_json::to_vec(&vec![cmd]).unwrap_or_default();

    if inline_images.is_empty() {
        (json_bytes, "application/json".to_string())
    } else {
        build_multipart_payload("Commands", "application/json", &json_bytes, inline_images)
    }
}

/// Builds a surgical `replace` `OnenotePatchCommand` for a single `target_element_id`.
pub fn build_replace_block_patch(target_element_id: &str, markdown: &str) -> OnenotePatchCommand {
    let html = markdown_to_onenote_html_fragment(markdown, &[]);
    let content = if html.is_empty() {
        "<p></p>".to_string()
    } else {
        html
    };
    OnenotePatchCommand {
        target: target_element_id.trim().to_string(),
        action: "replace".to_string(),
        position: None,
        content,
    }
}

/// Builds surgical `PATCH` commands to update page title (if changed) and replace the page body.
pub fn build_replace_page_body_commands(
    detail: &OnenotePageDetail,
    new_title: Option<&str>,
    new_markdown: &str,
) -> Vec<OnenotePatchCommand> {
    let mut cmds = Vec::new();

    if let Some(t) = new_title {
        let trimmed_title = t.trim();
        if !trimmed_title.is_empty() && trimmed_title != detail.summary.title {
            cmds.push(OnenotePatchCommand {
                target: "title".to_string(),
                action: "replace".to_string(),
                position: None,
                content: escape_xml(trimmed_title),
            });
        }
    }

    let fragment = markdown_to_onenote_html_fragment(new_markdown, &[]);

    if let Some(div_id) = detail
        .primary_div_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cmds.push(OnenotePatchCommand {
            target: div_id.to_string(),
            action: "replace".to_string(),
            position: None,
            content: format!("<div>{fragment}</div>"),
        });
    } else {
        let mut seen = HashSet::new();
        let mut block_ids = Vec::new();
        for b in &detail.blocks {
            if let Some(id) = b
                .element_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if seen.insert(id) {
                    block_ids.push(id);
                }
            }
        }

        if let Some((first_id, rest_ids)) = block_ids.split_first() {
            cmds.push(OnenotePatchCommand {
                target: (*first_id).to_string(),
                action: "replace".to_string(),
                position: None,
                content: if fragment.is_empty() {
                    "<p></p>".to_string()
                } else {
                    fragment
                },
            });
            for rem_id in rest_ids {
                cmds.push(OnenotePatchCommand {
                    target: (*rem_id).to_string(),
                    action: "replace".to_string(),
                    position: None,
                    content: "<p></p>".to_string(),
                });
            }
        } else {
            cmds.push(OnenotePatchCommand {
                target: "body".to_string(),
                action: "append".to_string(),
                position: None,
                content: if fragment.is_empty() {
                    "<p></p>".to_string()
                } else {
                    fragment
                },
            });
        }
    }

    cmds
}

// ---------------------------------------------------------------------------
// Internal Multipart & Opening-Tag Rewriter Helpers
// ---------------------------------------------------------------------------

fn build_multipart_payload(
    first_part_name: &str,
    first_part_content_type: &str,
    first_part_bytes: &[u8],
    inline_images: &[InlineImageUpload],
) -> (Vec<u8>, String) {
    let boundary = format!("onenote-part-{}", Uuid::new_v4().simple());
    let mut body = Vec::with_capacity(first_part_bytes.len() + inline_images.len() * 512 + 256);

    // Part 1: Presentation (XHTML) or Commands (JSON)
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{first_part_name}\"\r\n").as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {first_part_content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(first_part_bytes);
    body.extend_from_slice(b"\r\n");

    // Subsequent parts: binary image attachments
    for img in inline_images {
        let part_name = sanitize_mime_token(&img.temp_id);
        if part_name.is_empty() {
            continue;
        }
        let mime_type = {
            let m = sanitize_mime_token(&img.mime_type);
            if m.is_empty() {
                "image/png".to_string()
            } else {
                m
            }
        };
        let default_name = format!("{part_name}.{}", default_ext_for_mime(&mime_type));
        let file_name = img
            .file_name
            .as_deref()
            .map(sanitize_mime_token)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_name);

        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{part_name}\"; filename=\"{file_name}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(&img.bytes);
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

fn sanitize_mime_token(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|&c| !matches!(c, '\r' | '\n' | '"'))
        .collect()
}

fn default_ext_for_mime(mime: &str) -> &'static str {
    let lower = mime.to_ascii_lowercase();
    if lower.contains("jpeg") || lower.contains("jpg") {
        "jpg"
    } else if lower.contains("gif") {
        "gif"
    } else if lower.contains("webp") {
        "webp"
    } else {
        "png"
    }
}

enum TagRewriteMode<'a> {
    ToggleTodo { completed: bool },
    Set(Option<&'a OnenoteTagKind>),
}

struct OpeningAttrSpan {
    ws_start: usize,
    name_start: usize,
    name_end: usize,
    attr_end: usize,
    raw_value: Option<String>,
}

fn rewrite_opening_tag_data_tag(raw: &str, mode: TagRewriteMode<'_>) -> Option<String> {
    let lt_idx = raw.find('<')?;
    if !raw[..lt_idx].trim().is_empty() {
        return None;
    }

    let bytes = raw.as_bytes();
    let mut pos = lt_idx + 1;
    if pos >= bytes.len() || !bytes[pos].is_ascii_alphabetic() {
        return None;
    }
    while pos < bytes.len()
        && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'-' || bytes[pos] == b':')
    {
        pos += 1;
    }

    let mut attrs = Vec::new();
    let close_bracket_start;

    loop {
        let ws_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            return None;
        }
        if bytes[pos] == b'>' {
            close_bracket_start = pos;
            break;
        }
        if bytes[pos] == b'/' && pos + 1 < bytes.len() && bytes[pos + 1] == b'>' {
            close_bracket_start = pos;
            break;
        }

        let name_start = pos;
        while pos < bytes.len()
            && !bytes[pos].is_ascii_whitespace()
            && !matches!(bytes[pos], b'=' | b'>' | b'/')
        {
            pos += 1;
        }
        if pos == name_start {
            return None;
        }
        let name_end = pos;

        let mut temp = pos;
        while temp < bytes.len() && bytes[temp].is_ascii_whitespace() {
            temp += 1;
        }

        let (attr_end, raw_value) = if temp < bytes.len() && bytes[temp] == b'=' {
            pos = temp + 1;
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos >= bytes.len() {
                return None;
            }
            if bytes[pos] == b'"' || bytes[pos] == b'\'' {
                let quote = bytes[pos];
                pos += 1;
                let val_start = pos;
                while pos < bytes.len() && bytes[pos] != quote {
                    pos += 1;
                }
                if pos >= bytes.len() {
                    return None;
                }
                let val = unescape_basic_xml_entities(&raw[val_start..pos]);
                pos += 1;
                (pos, Some(val))
            } else {
                let val_start = pos;
                while pos < bytes.len()
                    && !bytes[pos].is_ascii_whitespace()
                    && bytes[pos] != b'>'
                    && !(bytes[pos] == b'/' && pos + 1 < bytes.len() && bytes[pos + 1] == b'>')
                {
                    pos += 1;
                }
                let val = unescape_basic_xml_entities(&raw[val_start..pos]);
                (pos, Some(val))
            }
        } else {
            (name_end, None)
        };

        attrs.push(OpeningAttrSpan {
            ws_start,
            name_start,
            name_end,
            attr_end,
            raw_value,
        });
    }

    let existing = attrs
        .iter()
        .find(|a| raw[a.name_start..a.name_end].eq_ignore_ascii_case("data-tag"));

    let desired_val: Option<String> = match mode {
        TagRewriteMode::ToggleTodo { completed } => {
            let todo_token = if completed {
                "to-do:completed"
            } else {
                "to-do"
            };
            if let Some(attr) = existing {
                let prev = attr.raw_value.as_deref().unwrap_or("");
                let mut updated_parts = Vec::new();
                let mut replaced_todo = false;
                for part in prev.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if part.eq_ignore_ascii_case("to-do")
                        || part.eq_ignore_ascii_case("to-do:completed")
                    {
                        if !replaced_todo {
                            updated_parts.push(todo_token.to_string());
                            replaced_todo = true;
                        }
                    } else {
                        updated_parts.push(part.to_string());
                    }
                }
                if !replaced_todo {
                    updated_parts.insert(0, todo_token.to_string());
                }
                Some(updated_parts.join(","))
            } else {
                Some(todo_token.to_string())
            }
        }
        TagRewriteMode::Set(Some(kind)) => Some(kind.data_tag_value()),
        TagRewriteMode::Set(None) => None,
    };

    match (existing, desired_val) {
        (Some(attr), Some(val)) => Some(format!(
            "{}=\"{}\"{}",
            &raw[..attr.name_end],
            escape_xml_attr(&val),
            &raw[attr.attr_end..]
        )),
        (Some(attr), None) => Some(format!("{}{}", &raw[..attr.ws_start], &raw[attr.attr_end..])),
        (None, Some(val)) => {
            if close_bracket_start > 0 && bytes[close_bracket_start - 1].is_ascii_whitespace() {
                Some(format!(
                    "{}data-tag=\"{}\" {}",
                    &raw[..close_bracket_start],
                    escape_xml_attr(&val),
                    &raw[close_bracket_start..]
                ))
            } else {
                Some(format!(
                    "{} data-tag=\"{}\"{}",
                    &raw[..close_bracket_start],
                    escape_xml_attr(&val),
                    &raw[close_bracket_start..]
                ))
            }
        }
        (None, None) => Some(raw.to_string()),
    }
}

fn unescape_basic_xml_entities(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn reconstruct_block_xhtml_with_tag(
    block: &OnenoteBlock,
    target_id: &str,
    tag: Option<&OnenoteTagKind>,
) -> String {
    let id_attr = escape_xml_attr(target_id);
    let tag_attr = match tag {
        Some(k) => format!(" data-tag=\"{}\"", escape_xml_attr(&k.data_tag_value())),
        None => String::new(),
    };

    match &block.kind {
        OnenoteBlockKind::Heading { level, spans } => {
            let lvl = (*level).clamp(1, 6);
            format!(
                "<h{lvl} id=\"{id_attr}\"{tag_attr}>{}</h{lvl}>",
                spans_to_onenote_xhtml(spans)
            )
        }
        OnenoteBlockKind::ListItem { spans, .. } => {
            format!(
                "<li id=\"{id_attr}\"{tag_attr}>{}</li>",
                spans_to_onenote_xhtml(spans)
            )
        }
        OnenoteBlockKind::Quote { spans } => {
            format!(
                "<blockquote id=\"{id_attr}\"{tag_attr}>{}</blockquote>",
                spans_to_onenote_xhtml(spans)
            )
        }
        OnenoteBlockKind::Paragraph { spans } => {
            format!(
                "<p id=\"{id_attr}\"{tag_attr}>{}</p>",
                spans_to_onenote_xhtml(spans)
            )
        }
        OnenoteBlockKind::CodeBlock { code, .. } => {
            format!(
                "<pre id=\"{id_attr}\"{tag_attr}><code>{}</code></pre>",
                escape_xml(code)
            )
        }
        _ => format!("<p id=\"{id_attr}\"{tag_attr}></p>"),
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::OnenoteScope;

    fn sample_summary(title: &str) -> OnenotePageSummary {
        OnenotePageSummary {
            page_key: "me|page-1".to_string(),
            id: "page-1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("스프린트".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("엔지니어링".to_string()),
            title: title.to_string(),
            level: 0,
            order: 0,
            created_at: Some("2026-11-15T09:00:00Z".to_string()),
            last_modified_at: Some("2026-11-15T09:30:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        }
    }

    #[test]
    fn test_parse_real_world_onenote_xhtml() {
        let raw_xhtml = r#"<!DOCTYPE html>
<html lang="ko-KR">
  <head>
    <title>v0.3.0 아키텍처 회의록</title>
    <meta name="created" content="2026-11-15T09:00:00Z" />
  </head>
  <body data-absolute-enabled="true" style="font-family:Calibri;font-size:11pt">
    <div id="div:{33f8a242-aaaa-bbbb-cccc-000000000001}{32}" style="position:absolute;left:48px;top:115px;width:624px">
      <h1 id="h1:{guid}{10}" data-tag="important">핵심 마일스톤</h1>
      <p id="p:{guid}{11}" data-tag="to-do">XHTML 파서 작성 <a href="https://learn.microsoft.com/graph"><b>Graph 문서</b></a></p>
      <p id="p:{guid}{12}" data-tag="to-do:completed,important"><span style="font-style:italic;text-decoration:underline">모델 정의</span> 확정</p>
      <ul id="ul:{guid}{20}">
        <li id="li:{guid}{21}">상위 항목
          <ol id="ol:{guid}{22}">
            <li id="li:{guid}{23}" data-tag="to-do">중첩 할일 1</li>
            <li id="li:{guid}{24}">중첩 항목 2</li>
          </ol>
        </li>
      </ul>
      <table id="table:{guid}{30}" border="1">
        <tr>
          <th>모듈</th>
          <th>담당</th>
        </tr>
        <tr>
          <td><p>parser</p><p>onenote.rs</p></td>
          <td><p><span style="font-family:Consolas">Track 2</span></p></td>
        </tr>
      </table>
      <pre id="pre:{guid}{40}"><code class="language-rust">fn main() {
    println!("hello onenote");
}</code></pre>
      <p id="p:{guid}{50}"><img id="img:{guid}{51}" src="https://graph.microsoft.com/v1.0/me/onenote/resources/thumb1/$value" data-fullres-src="https://graph.microsoft.com/v1.0/me/onenote/resources/full1/$value" data-src-type="image/png" width="800" height="600" alt="시스템 다이어그램" /></p>
      <img src="" width="1" height="1" />
      <object id="obj:{guid}{60}" data="https://graph.microsoft.com/v1.0/me/onenote/resources/pdf1/$value" data-attachment="spec.pdf" type="application/pdf" />
      <hr id="hr:{guid}{70}" />
    </div>
  </body>
</html>"#;

        let detail = parse_onenote_page_xhtml(sample_summary("(제목 없음)"), raw_xhtml);

        assert_eq!(detail.summary.title, "v0.3.0 아키텍처 회의록");
        assert!(detail.summary.has_todo);
        assert!(detail
            .summary
            .preview_text
            .as_deref()
            .is_some_and(|p| p.contains("핵심 마일스톤") && p.chars().count() <= 140));
        assert_eq!(
            detail.primary_div_id.as_deref(),
            Some("div:{33f8a242-aaaa-bbbb-cccc-000000000001}{32}")
        );

        // Verify blocks in exact document order:
        // 0: H1, 1: Todo(false), 2: Todo(true), 3: LI(depth 0), 4: LI(depth 1, Todo), 5: LI(depth 1),
        // 6: Table, 7: CodeBlock, 8: Image, 9: Attachment, 10: Divider
        assert_eq!(detail.blocks.len(), 11);

        assert_eq!(detail.blocks[0].element_id.as_deref(), Some("h1:{guid}{10}"));
        assert_eq!(detail.blocks[0].tag, Some(OnenoteTagKind::Important));
        assert!(matches!(
            detail.blocks[0].kind,
            OnenoteBlockKind::Heading { level: 1, .. }
        ));

        assert_eq!(detail.blocks[1].element_id.as_deref(), Some("p:{guid}{11}"));
        assert_eq!(
            detail.blocks[1].tag,
            Some(OnenoteTagKind::Todo { completed: false })
        );
        assert!(detail.blocks[1].raw_html.is_some());

        // Comma-separated `to-do:completed,important` prioritizes Todo { completed: true }
        assert_eq!(
            detail.blocks[2].tag,
            Some(OnenoteTagKind::Todo { completed: true })
        );
        if let OnenoteBlockKind::Paragraph { spans } = &detail.blocks[2].kind {
            assert!(spans[0].style.italic);
            assert!(spans[0].style.underline);
        } else {
            panic!("expected Paragraph for block 2");
        }

        // Nested list items
        assert!(matches!(
            &detail.blocks[3].kind,
            OnenoteBlockKind::ListItem {
                ordered: false,
                index: None,
                depth: 0,
                ..
            }
        ));
        if let OnenoteBlockKind::ListItem { spans, .. } = &detail.blocks[3].kind {
            assert_eq!(spans_to_plain(spans), "상위 항목");
        }
        assert_eq!(detail.blocks[4].element_id.as_deref(), Some("li:{guid}{23}"));
        assert_eq!(
            detail.blocks[4].tag,
            Some(OnenoteTagKind::Todo { completed: false })
        );
        assert!(matches!(
            &detail.blocks[4].kind,
            OnenoteBlockKind::ListItem {
                ordered: true,
                index: Some(1),
                depth: 1,
                ..
            }
        ));
        assert!(matches!(
            &detail.blocks[5].kind,
            OnenoteBlockKind::ListItem {
                ordered: true,
                index: Some(2),
                depth: 1,
                ..
            }
        ));

        // Table with multi-<p> cell and Consolas inline code
        if let OnenoteBlockKind::Table { headers, rows } = &detail.blocks[6].kind {
            assert_eq!(headers.len(), 2);
            assert_eq!(spans_to_plain(&headers[0]), "모듈");
            assert_eq!(rows.len(), 1);
            assert_eq!(spans_to_plain(&rows[0][0]), "parser / onenote.rs");
            assert!(rows[0][1][0].style.code);
        } else {
            panic!("expected Table for block 6");
        }

        // Code block
        if let OnenoteBlockKind::CodeBlock { lang, code } = &detail.blocks[7].kind {
            assert_eq!(lang.as_deref(), Some("rust"));
            assert!(code.contains("println!(\"hello onenote\");"));
        } else {
            panic!("expected CodeBlock for block 7");
        }

        // Image + Attachment resources (spacer 1x1 empty-src img ignored)
        assert_eq!(detail.resources.len(), 2);
        assert!(detail.resources[0].is_image);
        assert_eq!(
            detail.resources[0].resource_url,
            "https://graph.microsoft.com/v1.0/me/onenote/resources/full1/$value"
        );
        assert!(!detail.resources[1].is_image);
        assert_eq!(detail.resources[1].file_name.as_deref(), Some("spec.pdf"));

        if let OnenoteBlockKind::Image {
            resource_index,
            src,
            alt,
            width,
            height,
        } = &detail.blocks[8].kind
        {
            assert_eq!(*resource_index, 0);
            assert!(src.ends_with("/full1/$value"));
            assert_eq!(alt.as_deref(), Some("시스템 다이어그램"));
            assert_eq!(*width, Some(800));
            assert_eq!(*height, Some(600));
        } else {
            panic!("expected Image for block 8");
        }

        if let OnenoteBlockKind::Attachment {
            resource_index,
            name,
            mime_type,
            ..
        } = &detail.blocks[9].kind
        {
            assert_eq!(*resource_index, 1);
            assert_eq!(name, "spec.pdf");
            assert_eq!(mime_type, "application/pdf");
        } else {
            panic!("expected Attachment for block 9");
        }

        assert!(matches!(detail.blocks[10].kind, OnenoteBlockKind::Divider));
        assert!(detail.body_markdown.contains("- [ ] XHTML 파서 작성"));
        assert!(detail.body_markdown.contains("- [x] <u>*모델 정의*</u> 확정"));
        assert!(detail.body_plain.contains("[첨부: spec.pdf]"));
    }

    #[test]
    fn test_toggle_todo_and_set_tag_preserve_inner_html() {
        let block = OnenoteBlock {
            element_id: Some("p:{33f8a242-1234}{40}".to_string()),
            tag: Some(OnenoteTagKind::Todo { completed: false }),
            raw_html: Some(
                r#"<p id="p:{33f8a242-1234}{40}" data-custom-data-tag="keep" data-tag="to-do,important">배포 전 <a href="https://example.com?a=1&amp;b=2"><b>체크리스트</b></a> 확인</p>"#
                    .to_string(),
            ),
            kind: OnenoteBlockKind::Paragraph {
                spans: vec![RichSpan::plain("배포 전 체크리스트 확인")],
            },
        };

        // Unchecked -> Completed
        let (cmd1, completed1) = build_toggle_todo_patch(&block).expect("should toggle todo");
        assert!(completed1);
        assert_eq!(cmd1.target, "p:{33f8a242-1234}{40}");
        assert_eq!(cmd1.action, "replace");
        assert!(cmd1.content.contains(r#"data-custom-data-tag="keep""#));
        assert!(cmd1.content.contains(r#"data-tag="to-do:completed,important""#));
        assert!(cmd1
            .content
            .contains(r#"<a href="https://example.com?a=1&amp;b=2"><b>체크리스트</b></a>"#));

        // Completed -> Unchecked
        let mut completed_block = block.clone();
        completed_block.tag = Some(OnenoteTagKind::Todo { completed: true });
        completed_block.raw_html = Some(cmd1.content);
        let (cmd2, completed2) =
            build_toggle_todo_patch(&completed_block).expect("should toggle back");
        assert!(!completed2);
        assert!(cmd2.content.contains(r#"data-tag="to-do,important""#));
        assert!(cmd2
            .content
            .contains(r#"<a href="https://example.com?a=1&amp;b=2"><b>체크리스트</b></a>"#));

        // Non-todo block returns None on Space toggle, while build_set_tag_patch works
        let heading_block = OnenoteBlock {
            element_id: Some("h1:{1}{2}".to_string()),
            tag: None,
            raw_html: Some(r#"<h1 id="h1:{1}{2}">제목</h1>"#.to_string()),
            kind: OnenoteBlockKind::Heading {
                level: 1,
                spans: vec![RichSpan::plain("제목")],
            },
        };
        assert!(build_toggle_todo_patch(&heading_block).is_none());

        let set_cmd =
            build_set_tag_patch(&heading_block, Some(OnenoteTagKind::Question)).unwrap();
        assert_eq!(
            set_cmd.content,
            r#"<h1 id="h1:{1}{2}" data-tag="question">제목</h1>"#
        );

        let clear_cmd = build_set_tag_patch(
            &OnenoteBlock {
                raw_html: Some(set_cmd.content),
                ..heading_block
            },
            None,
        )
        .unwrap();
        assert_eq!(clear_cmd.content, r#"<h1 id="h1:{1}{2}">제목</h1>"#);
    }

    #[test]
    fn test_markdown_to_onenote_xhtml_roundtrip_and_self_closing_tags() {
        let md = "\
# 스프린트 계획

- [ ] API 스펙 확정
- [x] **DB 스키마** 리뷰 *완료* ~~보류~~ `v0.3` [링크](https://example.com?x=1&y=2)

[★] 배포 일정 엄수
[?] 권한 스코프 추가 필요?

- 백엔드
  1. 파서 구현
  2. 스토리지 연동

> 첫째 인용
> 둘째 인용

| 항목 | 상태 |
| --- | --- |
| 파서 | 완료 |
| 테스트 | 진행중 |

```rust
let ok = 1 < 2 && 3 > 1;
```

첫 줄
둘째 줄 `<unsafe>` <raw-xss>

---";

        let img = InlineImageUpload {
            temp_id: "shot1".to_string(),
            mime_type: "image/png".to_string(),
            bytes: vec![137, 80, 78, 71],
            file_name: Some("preview.png".to_string()),
        };

        let xhtml_frag = markdown_to_onenote_html_fragment(md, std::slice::from_ref(&img));

        // Verify valid self-closing XHTML void tags and inline tags
        assert!(xhtml_frag.contains("<h1>스프린트 계획</h1>"));
        assert!(xhtml_frag.contains(r#"<p data-tag="to-do">API 스펙 확정</p>"#));
        assert!(xhtml_frag.contains(
            r#"<p data-tag="to-do:completed"><b>DB 스키마</b> 리뷰 <i>완료</i> <s>보류</s> <code>v0.3</code> <a href="https://example.com?x=1&amp;y=2">링크</a></p>"#
        ));
        assert!(xhtml_frag.contains(r#"<p data-tag="important">배포 일정 엄수</p>"#));
        assert!(xhtml_frag.contains(r#"<p data-tag="question">권한 스코프 추가 필요?</p>"#));
        assert!(xhtml_frag
            .contains("<ul><li>백엔드<ol><li>파서 구현</li><li>스토리지 연동</li></ol></li></ul>"));
        assert!(xhtml_frag.contains("<blockquote>첫째 인용<br />둘째 인용</blockquote>"));
        assert!(xhtml_frag.contains(
            r#"<table border="1"><tr><th>항목</th><th>상태</th></tr><tr><td>파서</td><td>완료</td></tr><tr><td>테스트</td><td>진행중</td></tr></table>"#
        ));
        assert!(xhtml_frag.contains(
            r#"<pre><code class="language-rust">let ok = 1 &lt; 2 &amp;&amp; 3 &gt; 1;</code></pre>"#
        ));
        assert!(xhtml_frag.contains("첫 줄<br />둘째 줄 <code>&lt;unsafe&gt;</code> &lt;raw-xss&gt;"));
        assert!(!xhtml_frag.contains("<raw-xss>"));
        assert!(xhtml_frag.contains("<hr />"));
        assert!(xhtml_frag.contains(r#"<p><img src="name:shot1" alt="preview.png" /></p>"#));

        // Parse the generated XHTML back into OnenotePageDetail!
        let wrapped = format!(
            "<!DOCTYPE html><html><head><title>스프린트 계획</title></head><body><div id=\"div:1\">{xhtml_frag}</div></body></html>"
        );
        let reparsed = parse_onenote_page_xhtml(sample_summary(""), &wrapped);
        assert_eq!(reparsed.summary.title, "스프린트 계획");
        assert!(reparsed.summary.has_todo);
        assert!(reparsed.body_markdown.contains("- [ ] API 스펙 확정"));
        assert!(reparsed.body_markdown.contains("- [x] **DB 스키마**"));
        assert!(reparsed.body_markdown.contains("| 항목 | 상태 |"));
        assert!(reparsed.body_markdown.contains("```rust"));
        assert_eq!(reparsed.resources.len(), 1);
        assert_eq!(reparsed.resources[0].resource_url, "name:shot1");
    }

    #[test]
    fn test_multipart_new_page_and_append_payloads_and_page_replace() {
        // 1. Single-part new page (no images)
        let simple_page = OutgoingOnenotePage {
            title: "단일 페이지 & <테스트>".to_string(),
            markdown_body: "본문 내용".to_string(),
            inline_images: Vec::new(),
            level: None,
        };
        let (bytes_single, ct_single) = compile_new_page_payload(&simple_page);
        assert_eq!(ct_single, "application/xhtml+xml; charset=utf-8");
        let xhtml_str = String::from_utf8(bytes_single).unwrap();
        assert!(xhtml_str.contains("<title>단일 페이지 &amp; &lt;테스트&gt;</title>"));
        assert!(xhtml_str.contains("<meta name=\"created\" content=\""));
        assert!(xhtml_str.contains("<p>본문 내용</p>"));

        // 2. Multipart new page (with inline image)
        let img = InlineImageUpload {
            temp_id: "part1".to_string(),
            mime_type: "image/png".to_string(),
            bytes: vec![0x89, b'P', b'N', b'G'],
            file_name: Some("diagram.png".to_string()),
        };
        let multi_page = OutgoingOnenotePage {
            title: "이미지 페이지".to_string(),
            markdown_body: "설명 텍스트".to_string(),
            inline_images: vec![img.clone()],
            level: Some(1),
        };
        let (bytes_multi, ct_multi) = compile_new_page_payload(&multi_page);
        let boundary = ct_multi
            .strip_prefix("multipart/form-data; boundary=")
            .expect("expected multipart content-type");
        let multi_lossy = String::from_utf8_lossy(&bytes_multi);
        assert!(multi_lossy.contains(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"Presentation\"")));
        assert!(multi_lossy.contains("Content-Type: application/xhtml+xml; charset=utf-8"));
        assert!(multi_lossy.contains("src=\"name:part1\""));
        assert!(multi_lossy.contains(
            "Content-Disposition: form-data; name=\"part1\"; filename=\"diagram.png\""
        ));
        assert!(multi_lossy.ends_with(&format!("--{boundary}--\r\n")));

        // 3. Append payload: JSON without images, Multipart with images
        let (append_json, append_ct) =
            build_append_markdown_payload(Some("div:{abc}{32}"), "- [ ] 추가 할일", &[]);
        assert_eq!(append_ct, "application/json");
        let parsed_cmds: Vec<OnenotePatchCommand> = serde_json::from_slice(&append_json).unwrap();
        assert_eq!(parsed_cmds.len(), 1);
        assert_eq!(parsed_cmds[0].target, "div:{abc}{32}");
        assert_eq!(parsed_cmds[0].action, "append");
        assert_eq!(parsed_cmds[0].content, r#"<p data-tag="to-do">추가 할일</p>"#);

        let (append_mp, append_mp_ct) =
            build_append_markdown_payload(None, "캡처 첨부", std::slice::from_ref(&img));
        assert!(append_mp_ct.starts_with("multipart/form-data; boundary="));
        let append_mp_str = String::from_utf8_lossy(&append_mp);
        assert!(append_mp_str.contains("Content-Disposition: form-data; name=\"Commands\""));
        assert!(append_mp_str.contains("\"target\":\"body\""));
        assert!(append_mp_str.contains("name=\"part1\"; filename=\"diagram.png\""));

        // 4. Replace block & Replace full page body
        let block_patch = build_replace_block_patch("p:{guid}{99}", "**수정됨**");
        assert_eq!(block_patch.target, "p:{guid}{99}");
        assert_eq!(block_patch.action, "replace");
        assert_eq!(block_patch.content, "<p><b>수정됨</b></p>");

        let detail = parse_onenote_page_xhtml(
            sample_summary("기존 제목"),
            r#"<html><head><title>기존 제목</title></head><body><div id="div:{main}{1}"><p id="p:1">예전 본문</p></div></body></html>"#,
        );
        let page_cmds =
            build_replace_page_body_commands(&detail, Some("새 제목 & v2"), "## 새 섹션\n\n내용");
        assert_eq!(page_cmds.len(), 2);
        assert_eq!(page_cmds[0].target, "title");
        assert_eq!(page_cmds[0].content, "새 제목 &amp; v2");
        assert_eq!(page_cmds[1].target, "div:{main}{1}");
        assert_eq!(page_cmds[1].action, "replace");
        assert_eq!(
            page_cmds[1].content,
            "<div><h2>새 섹션</h2><p>내용</p></div>"
        );
    }
}
