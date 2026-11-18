//! Outlook HTML + attachment parser into `Vec<RichBlock>`, plain text, and GFM Markdown.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use scraper::{ElementRef, Html, Node};

use crate::models::{MailAttachment, RichBlock, RichSpan, TextStyleFlags};

const HTML_CACHE_MAX_ENTRIES: usize = 1024;
const HTML_CACHE_MAX_HTML_BYTES: usize = 256 * 1024;

type HtmlCacheKey = (u64, u64, usize);

#[derive(Default)]
struct HtmlParseCache {
    entries: HashMap<HtmlCacheKey, Arc<[RichBlock]>>,
    order: VecDeque<HtmlCacheKey>,
}

impl HtmlParseCache {
    fn get(&mut self, key: &HtmlCacheKey) -> Option<Arc<[RichBlock]>> {
        let val = self.entries.get(key).cloned()?;
        if self.order.back() != Some(key) {
            if let Some(pos) = self.order.iter().rposition(|k| k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(*key);
        }
        Some(val)
    }

    fn insert(&mut self, key: HtmlCacheKey, value: Arc<[RichBlock]>) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key, value);
            if self.order.back() != Some(&key) {
                if let Some(pos) = self.order.iter().rposition(|k| *k == key) {
                    self.order.remove(pos);
                }
                self.order.push_back(key);
            }
            return;
        }
        while self.entries.len() >= HTML_CACHE_MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        self.entries.insert(key, value);
        self.order.push_back(key);
    }
}

static HTML_PARSE_CACHE: OnceLock<Mutex<HtmlParseCache>> = OnceLock::new();

struct Fnv1a64(u64);

impl Fnv1a64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn finish_mixed(&self) -> u64 {
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

impl Hasher for Fnv1a64 {
    fn finish(&self) -> u64 {
        self.finish_mixed()
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

fn hash_str_field(h1: &mut DefaultHasher, h2: &mut Fnv1a64, s: &str) {
    s.len().hash(h1);
    s.as_bytes().hash(h1);
    s.len().hash(h2);
    s.as_bytes().hash(h2);
}

fn hash_opt_str_field(h1: &mut DefaultHasher, h2: &mut Fnv1a64, opt: Option<&str>) {
    match opt {
        Some(s) => {
            1u8.hash(h1);
            1u8.hash(h2);
            hash_str_field(h1, h2, s);
        }
        None => {
            0u8.hash(h1);
            0u8.hash(h2);
        }
    }
}

fn compute_parse_fingerprint(html: &str, attachments: &[MailAttachment]) -> HtmlCacheKey {
    let mut h1 = DefaultHasher::new();
    let mut h2 = Fnv1a64::new();

    0x4854_4d4c_u32.hash(&mut h1);
    0x9e37_79b9_u32.hash(&mut h2);

    hash_str_field(&mut h1, &mut h2, html);

    attachments.len().hash(&mut h1);
    attachments.len().hash(&mut h2);
    for att in attachments {
        hash_str_field(&mut h1, &mut h2, &att.id);
        hash_str_field(&mut h1, &mut h2, &att.name);
        hash_str_field(&mut h1, &mut h2, &att.content_type);
        att.size.hash(&mut h1);
        att.size.hash(&mut h2);
        att.is_inline.hash(&mut h1);
        att.is_inline.hash(&mut h2);
        hash_opt_str_field(&mut h1, &mut h2, att.content_id.as_deref());
    }

    (h1.finish(), h2.finish(), html.len())
}

fn has_plain_quote_or_reply_marker(text: &str) -> bool {
    text.lines().any(|line| {
        let t = line.trim_start();
        t.starts_with('>')
            || t.starts_with("-----Original Message-----")
            || t.starts_with("From:")
            || t.ends_with("wrote:")
    })
}

fn try_fast_path_plain_or_empty(
    html: &str,
    attachments: &[MailAttachment],
) -> Option<Arc<[RichBlock]>> {
    if !attachments.is_empty() {
        return None;
    }
    if html.trim().is_empty() {
        return Some(Arc::from([]));
    }
    if !html.contains('<')
        && !html.contains('&')
        && !html.contains('\0')
        && !has_plain_quote_or_reply_marker(html)
    {
        let collapsed = collapse_html_whitespace(html);
        let trimmed = collapsed.trim();
        if trimmed.is_empty() {
            return Some(Arc::from([]));
        }
        return Some(Arc::from([RichBlock::Paragraph(vec![RichSpan {
            text: trimmed.to_string(),
            style: TextStyleFlags::default(),
        }])]));
    }
    None
}

pub fn parse_message_blocks_arc(
    html: &str,
    attachments: &[MailAttachment],
) -> Arc<[RichBlock]> {
    if let Some(fast) = try_fast_path_plain_or_empty(html, attachments) {
        return fast;
    }
    if html.len() > HTML_CACHE_MAX_HTML_BYTES {
        return Arc::from(parse_message_blocks_uncached(html, attachments));
    }

    let key = compute_parse_fingerprint(html, attachments);
    let cache = HTML_PARSE_CACHE.get_or_init(|| Mutex::new(HtmlParseCache::default()));
    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.get(&key) {
            return cached;
        }
    }

    let parsed: Arc<[RichBlock]> = Arc::from(parse_message_blocks_uncached(html, attachments));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, Arc::clone(&parsed));
    parsed
}

pub fn parse_message_blocks(html: &str, attachments: &[MailAttachment]) -> Vec<RichBlock> {
    parse_message_blocks_arc(html, attachments).to_vec()
}

pub(crate) fn parse_message_blocks_no_cache(
    html: &str,
    attachments: &[MailAttachment],
) -> Arc<[RichBlock]> {
    if let Some(fast) = try_fast_path_plain_or_empty(html, attachments) {
        return fast;
    }
    Arc::from(parse_message_blocks_uncached(html, attachments))
}

fn parse_message_blocks_uncached(
    html: &str,
    attachments: &[MailAttachment],
) -> Vec<RichBlock> {
    let mut ctx = ParserContext {
        seen_attachments: HashSet::new(),
        seen_inline_srcs: HashSet::new(),
        seen_content_ids: HashSet::new(),
        blocks: Vec::new(),
        quoted_blocks: Vec::new(),
        in_quoted_history: false,
        current_spans: Vec::new(),
        next_image_index: 0,
    };

    if !html.trim().is_empty() {
        if !html.contains('<') && !html.contains('&') {
            ctx.parse_plain_text_lines(html);
        } else {
            let fragment = Html::parse_fragment(html);
            ctx.walk_children(fragment.root_element(), &TextStyleFlags::default());
            ctx.flush_paragraph();
        }
    }

    ctx.finalize_quoted_history();

    ctx.in_quoted_history = false;
    for att in attachments {
        if !ctx.seen_attachments.contains(&att.id) && !ctx.is_seen_inline_attachment(att) {
            ctx.push_attachment_block(att);
        }
    }

    ctx.blocks
}

pub fn blocks_to_plain(blocks: &[RichBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                let s = spans_to_plain(spans);
                if !s.trim().is_empty() {
                    parts.push(s.trim().to_string());
                }
            }
            RichBlock::Heading { spans, .. } => {
                let s = spans_to_plain(spans);
                if !s.trim().is_empty() {
                    parts.push(s.trim().to_string());
                }
            }
            RichBlock::Quote(spans) => {
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
            RichBlock::CodeBlock { code, .. } => {
                let trimmed = code.trim_end();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            RichBlock::BulletList(items) => {
                let mut lines = Vec::new();
                for item in items {
                    let s = spans_to_plain(item);
                    if !s.trim().is_empty() {
                        lines.push(format!("- {}", s.trim()));
                    }
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::Table { headers, rows } => {
                let mut lines = Vec::new();
                if !headers.is_empty() {
                    let h = headers
                        .iter()
                        .map(|cell| spans_to_plain(cell).trim().to_string())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    if !h.trim().is_empty() {
                        lines.push(h);
                    }
                }
                for row in rows {
                    let r = row
                        .iter()
                        .map(|cell| spans_to_plain(cell).trim().to_string())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    if !r.trim().is_empty() {
                        lines.push(r);
                    }
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::Divider => {
                parts.push("---".to_string());
            }
            RichBlock::AttachmentChip { id, name } => {
                let label = name.as_deref().unwrap_or(id.as_str());
                parts.push(format!("[Attachment: {label}]"));
            }
            RichBlock::InlineImageChip { index, .. } => {
                parts.push(format!("[Image #{}]", index + 1));
            }
            RichBlock::QuotedHistory { blocks, .. } => {
                let inner = blocks_to_plain(blocks);
                if !inner.trim().is_empty() {
                    parts.push(inner);
                }
            }
        }
    }
    parts.join("\n")
}

pub fn blocks_to_markdown(blocks: &[RichBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                let s = spans_to_markdown(spans);
                if !s.trim().is_empty() {
                    parts.push(s.trim().to_string());
                }
            }
            RichBlock::Heading { level, spans } => {
                let s = spans_to_markdown(spans);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    let hashes = "#".repeat((*level).clamp(1, 6) as usize);
                    parts.push(format!("{hashes} {trimmed}"));
                }
            }
            RichBlock::Quote(spans) => {
                let s = spans_to_markdown(spans);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    let quoted = trimmed
                        .lines()
                        .map(|l| format!("> {}", l.trim_end()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    parts.push(quoted);
                }
            }
            RichBlock::CodeBlock { lang, code } => {
                let lang_str = lang.as_deref().unwrap_or("");
                parts.push(format!("```{lang_str}\n{}\n```", code.trim_end_matches('\n')));
            }
            RichBlock::BulletList(items) => {
                let mut lines = Vec::new();
                for item in items {
                    let s = spans_to_markdown(item);
                    if !s.trim().is_empty() {
                        lines.push(format!("- {}", s.trim()));
                    }
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::Table { headers, rows } => {
                let col_count = headers
                    .len()
                    .max(rows.iter().map(Vec::len).max().unwrap_or(0));
                if col_count > 0 {
                    let mut lines = Vec::new();
                    let header_cells: Vec<String> = if !headers.is_empty() {
                        (0..col_count)
                            .map(|i| {
                                headers
                                    .get(i)
                                    .map(|c| escape_md_table_cell(&spans_to_markdown(c)))
                                    .unwrap_or_default()
                            })
                            .collect()
                    } else {
                        vec![String::new(); col_count]
                    };
                    lines.push(format!("| {} |", header_cells.join(" | ")));
                    lines.push(format!(
                        "| {} |",
                        vec!["---"; col_count].join(" | ")
                    ));
                    for row in rows {
                        let cells: Vec<String> = (0..col_count)
                            .map(|i| {
                                row.get(i)
                                    .map(|c| escape_md_table_cell(&spans_to_markdown(c)))
                                    .unwrap_or_default()
                            })
                            .collect();
                        lines.push(format!("| {} |", cells.join(" | ")));
                    }
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::Divider => {
                parts.push("---".to_string());
            }
            RichBlock::AttachmentChip { id, name } => {
                let label = name.as_deref().unwrap_or(id.as_str());
                parts.push(format!("[Attachment: {label}]"));
            }
            RichBlock::InlineImageChip {
                src,
                content_id,
                index,
            } => {
                if let Some(cid) = content_id {
                    parts.push(format!("![image-{}](cid:{cid})", index + 1));
                } else {
                    parts.push(format!("![image-{}]({src})", index + 1));
                }
            }
            RichBlock::QuotedHistory { blocks, .. } => {
                let inner = blocks_to_markdown(blocks);
                if !inner.trim().is_empty() {
                    let quoted = inner
                        .lines()
                        .map(|l| {
                            if l.trim().is_empty() {
                                ">".to_string()
                            } else {
                                format!("> {l}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    parts.push(quoted);
                }
            }
        }
    }
    parts.join("\n\n")
}

fn escape_md_table_cell(raw: &str) -> String {
    raw.trim().replace('\n', " ").replace('|', "\\|")
}

pub(crate) fn normalize_content_id(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(rest) = s
        .strip_prefix("cid:")
        .or_else(|| s.strip_prefix("CID:"))
        .or_else(|| {
            if s.len() >= 4 && s[..4].eq_ignore_ascii_case("cid:") {
                Some(&s[4..])
            } else {
                None
            }
        })
    {
        s = rest.trim();
    }
    s.trim_start_matches('<').trim_end_matches('>').trim().to_string()
}

struct ParserContext {
    seen_attachments: HashSet<String>,
    seen_inline_srcs: HashSet<String>,
    seen_content_ids: HashSet<String>,
    blocks: Vec<RichBlock>,
    quoted_blocks: Vec<RichBlock>,
    in_quoted_history: bool,
    current_spans: Vec<RichSpan>,
    next_image_index: usize,
}

impl ParserContext {
    fn push_block(&mut self, block: RichBlock) {
        if self.in_quoted_history {
            self.quoted_blocks.push(block);
        } else {
            self.blocks.push(block);
        }
    }

    fn parse_plain_text_lines(&mut self, text: &str) {
        let mut normal_lines: Vec<&str> = Vec::new();
        let mut quote_lines: Vec<String> = Vec::new();

        let flush_normal = |ctx: &mut Self, buf: &mut Vec<&str>| {
            if !buf.is_empty() {
                let joined = buf.join("\n");
                let trimmed = joined.trim();
                if !trimmed.is_empty() {
                    ctx.push_block(RichBlock::Paragraph(vec![RichSpan::plain(trimmed)]));
                }
                buf.clear();
            }
        };
        let flush_quote = |ctx: &mut Self, buf: &mut Vec<String>| {
            if !buf.is_empty() {
                let joined = buf.join("\n");
                let trimmed = joined.trim();
                if !trimmed.is_empty() {
                    ctx.push_block(RichBlock::Quote(vec![RichSpan::plain(trimmed)]));
                }
                buf.clear();
            }
        };

        for line in text.lines() {
            let trimmed_start = line.trim_start();
            if let Some(after_gt) = trimmed_start.strip_prefix('>') {
                flush_normal(self, &mut normal_lines);
                quote_lines.push(after_gt.strip_prefix(' ').unwrap_or(after_gt).trim_end().to_string());
            } else if trimmed_start.is_empty() {
                flush_normal(self, &mut normal_lines);
                flush_quote(self, &mut quote_lines);
            } else {
                flush_quote(self, &mut quote_lines);
                normal_lines.push(line.trim_end());
            }
        }
        flush_normal(self, &mut normal_lines);
        flush_quote(self, &mut quote_lines);
    }

    fn walk_children(&mut self, parent: ElementRef<'_>, style: &TextStyleFlags) {
        for child in parent.children() {
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if !self.in_quoted_history && is_quoted_history_boundary(child_el) {
                            self.flush_paragraph();
                            self.in_quoted_history = true;
                            if child_el.value().name() == "hr" {
                                continue;
                            }
                        }
                        self.handle_element(child_el, style);
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_element(&mut self, el_ref: ElementRef<'_>, style: &TextStyleFlags) {
        let el = el_ref.value();
        let tag = el.name();

        if is_ignored_noise_tag(tag) || has_display_none(el) {
            return;
        }

        match tag {
            "br" => {
                self.push_raw_span("\n", &TextStyleFlags::default());
            }
            "hr" => {
                self.flush_paragraph();
                self.push_block(RichBlock::Divider);
            }
            "b" | "strong" => {
                let mut next = style.clone();
                next.bold = true;
                self.walk_children(el_ref, &next);
            }
            "i" | "em" => {
                let mut next = style.clone();
                next.italic = true;
                self.walk_children(el_ref, &next);
            }
            "s" | "strike" | "del" => {
                let mut next = style.clone();
                next.strike = true;
                self.walk_children(el_ref, &next);
            }
            "u" => {
                let mut next = style.clone();
                next.underline = true;
                self.walk_children(el_ref, &next);
            }
            "code" => {
                let mut next = style.clone();
                next.code = true;
                let raw = collect_raw_text(el_ref);
                if !raw.is_empty() {
                    self.push_raw_span(&raw, &next);
                }
            }
            "a" => {
                let mut next = style.clone();
                if let Some(href) = el.attr("href").map(str::trim).filter(|s| !s.is_empty()) {
                    next.link_url = Some(href.to_string());
                }
                self.walk_children(el_ref, &next);
            }
            "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "center" => {
                self.flush_paragraph();
                self.walk_children(el_ref, style);
                self.flush_paragraph();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_paragraph();
                let level = tag[1..].parse::<u8>().unwrap_or(1);
                let spans = self.collect_inline_spans(el_ref, style);
                if !spans_are_blank(&spans) {
                    self.push_block(RichBlock::Heading {
                        level,
                        spans: trim_spans(spans),
                    });
                }
            }
            "blockquote" => {
                self.flush_paragraph();
                let spans = self.collect_quote_spans(el_ref, style);
                if !spans_are_blank(&spans) {
                    self.push_block(RichBlock::Quote(trim_spans(spans)));
                }
            }
            "pre" => {
                self.flush_paragraph();
                let lang = detect_code_lang(el_ref);
                let code = collect_raw_text(el_ref);
                let trimmed = code.trim_matches(|c| c == '\r' || c == '\n');
                if !trimmed.is_empty() {
                    self.push_block(RichBlock::CodeBlock {
                        lang,
                        code: trimmed.to_string(),
                    });
                }
            }
            "ul" | "ol" => {
                self.flush_paragraph();
                let mut items = Vec::new();
                for child in el_ref.children() {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if child_el.value().name() == "li" {
                            let spans = self.collect_inline_spans(child_el, style);
                            if !spans_are_blank(&spans) {
                                items.push(trim_spans(spans));
                            }
                        }
                    }
                }
                if !items.is_empty() {
                    self.push_block(RichBlock::BulletList(items));
                }
            }
            "table" => {
                self.flush_paragraph();
                if should_linearize_table(el_ref) {
                    self.linearize_table(el_ref, style);
                } else if let Some(table_block) = self.parse_data_table(el_ref, style) {
                    self.push_block(table_block);
                }
            }
            "emoji" => {
                let emoji_text = non_empty_attr(el, "alt")
                    .or_else(|| non_empty_attr(el, "title"))
                    .map(str::to_string)
                    .or_else(|| {
                        el_ref.descendants().find_map(|desc| {
                            let desc_el = ElementRef::wrap(desc)?;
                            if desc_el.value().name() == "img" {
                                non_empty_attr(desc_el.value(), "alt")
                                    .or_else(|| non_empty_attr(desc_el.value(), "title"))
                                    .map(str::to_string)
                            } else {
                                None
                            }
                        })
                    });
                if let Some(alt) = emoji_text {
                    self.push_raw_span(&alt, style);
                } else {
                    self.walk_children(el_ref, style);
                }
            }
            "img" => {
                if is_tracking_pixel_img(el) {
                    return;
                }
                if is_emoji_sprite_img(el) {
                    if let Some(alt) =
                        non_empty_attr(el, "alt").or_else(|| non_empty_attr(el, "title"))
                    {
                        self.push_raw_span(alt, style);
                    }
                    return;
                }
                if let Some(src) = el.attr("src").map(str::trim).filter(|s| !s.is_empty()) {
                    self.flush_paragraph();
                    self.seen_inline_srcs.insert(src.to_string());
                    let content_id = extract_cid(src);
                    if let Some(ref cid) = content_id {
                        self.seen_content_ids.insert(cid.to_ascii_lowercase());
                    }
                    let index = self.next_image_index;
                    self.next_image_index += 1;
                    self.push_block(RichBlock::InlineImageChip {
                        src: src.to_string(),
                        content_id,
                        index,
                    });
                }
            }
            _ => {
                self.walk_children(el_ref, style);
            }
        }
    }

    fn linearize_table(&mut self, table_el: ElementRef<'_>, style: &TextStyleFlags) {
        let mut walked_any_cell = false;
        for child in table_el.children() {
            let Some(child_el) = ElementRef::wrap(child) else {
                continue;
            };
            match child_el.value().name() {
                "thead" | "tbody" | "tfoot" => {
                    for row_node in child_el.children() {
                        if let Some(row_el) = ElementRef::wrap(row_node) {
                            if row_el.value().name() == "tr" {
                                self.linearize_table_row(row_el, style);
                                walked_any_cell = true;
                            }
                        }
                    }
                }
                "tr" => {
                    self.linearize_table_row(child_el, style);
                    walked_any_cell = true;
                }
                _ => {}
            }
        }
        if !walked_any_cell {
            self.walk_children(table_el, style);
            self.flush_paragraph();
        }
    }

    fn linearize_table_row(&mut self, row_el: ElementRef<'_>, style: &TextStyleFlags) {
        for cell_node in row_el.children() {
            let Some(cell_el) = ElementRef::wrap(cell_node) else {
                continue;
            };
            if matches!(cell_el.value().name(), "td" | "th") && !has_display_none(cell_el.value()) {
                self.flush_paragraph();
                self.walk_children(cell_el, style);
                self.flush_paragraph();
            }
        }
    }

    fn parse_data_table(
        &mut self,
        table_el: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Option<RichBlock> {
        let mut headers: Vec<Vec<RichSpan>> = Vec::new();
        let mut rows: Vec<Vec<Vec<RichSpan>>> = Vec::new();
        let mut saw_thead = false;

        let mut raw_rows: Vec<(bool, ElementRef<'_>)> = Vec::new();
        for child in table_el.children() {
            let Some(child_el) = ElementRef::wrap(child) else {
                continue;
            };
            match child_el.value().name() {
                "thead" => {
                    saw_thead = true;
                    for r in child_el.children().filter_map(ElementRef::wrap) {
                        if r.value().name() == "tr" {
                            raw_rows.push((true, r));
                        }
                    }
                }
                "tbody" | "tfoot" => {
                    for r in child_el.children().filter_map(ElementRef::wrap) {
                        if r.value().name() == "tr" {
                            raw_rows.push((false, r));
                        }
                    }
                }
                "tr" => {
                    raw_rows.push((false, child_el));
                }
                _ => {}
            }
        }

        for (idx, (in_thead, row_el)) in raw_rows.into_iter().enumerate() {
            let mut cells: Vec<Vec<RichSpan>> = Vec::new();
            let mut th_count = 0usize;
            let mut total_cells = 0usize;

            for cell_el in row_el.children().filter_map(ElementRef::wrap) {
                let tag = cell_el.value().name();
                if matches!(tag, "th" | "td") {
                    total_cells += 1;
                    if tag == "th" {
                        th_count += 1;
                    }
                    let cell_spans = trim_spans(self.collect_inline_spans(cell_el, style));
                    cells.push(cell_spans);
                }
            }

            let all_blank = cells.iter().all(|c| spans_are_blank(c));
            if all_blank || cells.is_empty() {
                continue;
            }

            let is_header_row = (in_thead && headers.is_empty())
                || (!saw_thead && idx == 0 && headers.is_empty() && th_count == total_cells);
            if is_header_row {
                headers = cells;
            } else {
                rows.push(cells);
            }
        }

        if headers.is_empty() && rows.is_empty() {
            None
        } else {
            Some(RichBlock::Table { headers, rows })
        }
    }

    fn finalize_quoted_history(&mut self) {
        if !self.quoted_blocks.is_empty() {
            let quoted = std::mem::take(&mut self.quoted_blocks);
            let line_count = count_blocks_lines(&quoted);
            self.blocks.push(RichBlock::QuotedHistory {
                line_count,
                blocks: quoted,
            });
            return;
        }

        // Check for trailing quote chains or plain-text Outlook/Gmail reply headers at the end of `self.blocks`.
        if let Some(split_idx) = find_trailing_quoted_split_index(&self.blocks) {
            let quoted = self.blocks.split_off(split_idx);
            if !quoted.is_empty() {
                let line_count = count_blocks_lines(&quoted);
                self.blocks.push(RichBlock::QuotedHistory {
                    line_count,
                    blocks: quoted,
                });
            }
        }
    }

    fn collect_inline_spans(
        &mut self,
        el_ref: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Vec<RichSpan> {
        let saved = std::mem::take(&mut self.current_spans);
        self.walk_children(el_ref, style);
        std::mem::replace(&mut self.current_spans, saved)
    }

    fn collect_quote_spans(
        &mut self,
        el_ref: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Vec<RichSpan> {
        let saved = std::mem::take(&mut self.current_spans);
        self.walk_quote_children(el_ref, style);
        std::mem::replace(&mut self.current_spans, saved)
    }

    fn walk_quote_children(&mut self, parent: ElementRef<'_>, style: &TextStyleFlags) {
        let mut first_block = true;
        for child in parent.children() {
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(el) => {
                    if is_ignored_noise_tag(el.name()) || has_display_none(el) {
                        continue;
                    }
                    let Some(child_el) = ElementRef::wrap(child) else {
                        continue;
                    };
                    match el.name() {
                        "p" | "div" => {
                            if !first_block && !self.current_spans.is_empty() {
                                self.push_raw_span("\n", &TextStyleFlags::default());
                            }
                            first_block = false;
                            self.walk_quote_children(child_el, style);
                        }
                        "br" => {
                            self.push_raw_span("\n", &TextStyleFlags::default());
                        }
                        "b" | "strong" => {
                            let mut next = style.clone();
                            next.bold = true;
                            self.walk_quote_children(child_el, &next);
                        }
                        "i" | "em" => {
                            let mut next = style.clone();
                            next.italic = true;
                            self.walk_quote_children(child_el, &next);
                        }
                        "a" => {
                            let mut next = style.clone();
                            if let Some(href) =
                                el.attr("href").map(str::trim).filter(|s| !s.is_empty())
                            {
                                next.link_url = Some(href.to_string());
                            }
                            self.walk_quote_children(child_el, &next);
                        }
                        "code" => {
                            let mut next = style.clone();
                            next.code = true;
                            let raw = collect_raw_text(child_el);
                            if !raw.is_empty() {
                                self.push_raw_span(&raw, &next);
                            }
                        }
                        _ => {
                            self.walk_quote_children(child_el, style);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn push_inline_text(&mut self, raw: &str, style: &TextStyleFlags) {
        let collapsed = collapse_html_whitespace(raw);
        if collapsed.is_empty() {
            return;
        }
        let to_push = if collapsed == " "
            && (self.current_spans.is_empty()
                || self
                    .current_spans
                    .last()
                    .map(|s| s.text.ends_with('\n') || s.text.ends_with(' '))
                    .unwrap_or(false))
        {
            return;
        } else if self.current_spans.is_empty() {
            collapsed.trim_start().to_string()
        } else {
            collapsed
        };

        if !to_push.is_empty() {
            self.push_raw_span(&to_push, style);
        }
    }

    fn push_raw_span(&mut self, text: &str, style: &TextStyleFlags) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.current_spans.last_mut() {
            if &last.style == style {
                last.text.push_str(text);
                return;
            }
        }
        self.current_spans.push(RichSpan {
            text: text.to_string(),
            style: style.clone(),
        });
    }

    fn flush_paragraph(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);
        if !spans_are_blank(&spans) {
            let trimmed = trim_spans(spans);
            self.push_block(RichBlock::Paragraph(trimmed));
        }
    }

    fn push_attachment_block(&mut self, att: &MailAttachment) {
        self.blocks.push(RichBlock::AttachmentChip {
            id: att.id.clone(),
            name: if att.name.trim().is_empty() {
                None
            } else {
                Some(att.name.clone())
            },
        });
    }

    fn is_seen_inline_attachment(&self, att: &MailAttachment) -> bool {
        if let Some(cid) = att.content_id.as_deref() {
            let norm = normalize_content_id(cid).to_ascii_lowercase();
            if !norm.is_empty() && self.seen_content_ids.contains(&norm) {
                return true;
            }
        }
        att.is_inline && self.seen_inline_srcs.contains(&att.id)
    }
}

fn count_blocks_lines(blocks: &[RichBlock]) -> usize {
    let plain = blocks_to_plain(blocks);
    plain.lines().count().max(blocks.len()).max(1)
}

fn find_trailing_quoted_split_index(blocks: &[RichBlock]) -> Option<usize> {
    if blocks.is_empty() {
        return None;
    }

    // 1) Explicit Outlook/plain-text reply boundary paragraph (`-----Original Message-----` or `From: ... Sent: ...`)
    for (idx, block) in blocks.iter().enumerate() {
        if let RichBlock::Paragraph(spans) = block {
            let text = spans_to_plain(spans);
            let trimmed = text.trim();
            if trimmed.starts_with("-----Original Message-----")
                || (trimmed.contains("From:") && (trimmed.contains("Sent:") || trimmed.contains("Date:")))
            {
                // Avoid folding if it's the very first block and there's nothing before it?
                // Even if an email starts with a forwarded block, folding after non-empty lead or >0 blocks is good;
                // require either idx > 0 or >= 2 blocks so a 1-line "From: HR" greeting isn't folded.
                if idx > 0 || blocks.len() >= 2 {
                    return Some(idx);
                }
            }
        }
    }

    // 2) Trailing `RichBlock::Quote` blocks at the end of the email (optionally preceded by `On ... wrote:` or `<hr>`).
    let mut quote_start = blocks.len();
    while quote_start > 0 && matches!(blocks[quote_start - 1], RichBlock::Quote(_)) {
        quote_start -= 1;
    }
    if quote_start < blocks.len() {
        if quote_start > 0 {
            if let RichBlock::Paragraph(spans) = &blocks[quote_start - 1] {
                let prev = spans_to_plain(spans);
                let t = prev.trim();
                if t.ends_with("wrote:") || t.ends_with("wrote :") {
                    quote_start -= 1;
                }
            }
        }
        if quote_start > 0 && matches!(blocks[quote_start - 1], RichBlock::Divider) {
            quote_start -= 1;
        }
        if quote_start > 0 {
            return Some(quote_start);
        }
    }

    None
}

fn is_quoted_history_boundary(el_ref: ElementRef<'_>) -> bool {
    let el = el_ref.value();
    if let Some(id) = el.attr("id") {
        let lower_id = id.trim().to_ascii_lowercase();
        if matches!(lower_id.as_str(), "appendonsend" | "divrplyfwdmsg") {
            return true;
        }
    }
    if let Some(cls) = el.attr("class") {
        if cls.split_whitespace().any(|c| {
            let lc = c.to_ascii_lowercase();
            matches!(
                lc.as_str(),
                "gmail_quote" | "gmail_quote_container" | "yahoo_quoted" | "outlookmessageheader"
            )
        }) {
            return true;
        }
    }
    if el.name() == "hr" && el.attr("tabindex").is_some_and(|v| v.trim() == "-1") {
        let mut tail_text = String::new();
        for sib in el_ref.next_siblings().take(6) {
            if let Some(sib_el) = ElementRef::wrap(sib) {
                if sib_el.value().attr("id").is_some_and(|id| {
                    let l = id.trim().to_ascii_lowercase();
                    l == "divrplyfwdmsg" || l == "appendonsend"
                }) {
                    return true;
                }
                collect_raw_text_inner(sib_el, &mut tail_text);
            } else if let Node::Text(t) = sib.value() {
                tail_text.push_str(t.text.as_ref());
            }
        }
        if tail_text.contains("From:")
            && (tail_text.contains("Sent:")
                || tail_text.contains("Date:")
                || tail_text.contains("Subject:"))
        {
            return true;
        }
    }
    false
}

fn is_ignored_noise_tag(tag: &str) -> bool {
    matches!(
        tag,
        "head" | "style" | "script" | "meta" | "title" | "xml" | "o:p" | "noscript" | "template"
    )
}

fn has_display_none(el: &scraper::node::Element) -> bool {
    let Some(style) = el.attr("style") else {
        return false;
    };
    for decl in style.split(';') {
        let Some((prop, val)) = decl.split_once(':') else {
            continue;
        };
        if prop.trim().eq_ignore_ascii_case("display") && val.trim().eq_ignore_ascii_case("none") {
            return true;
        }
    }
    false
}

fn parse_css_dim_px(raw: &str) -> Option<u32> {
    let trimmed = raw.trim().to_ascii_lowercase();
    let num_str = trimmed.strip_suffix("px").unwrap_or(&trimmed).trim();
    num_str.parse::<f32>().ok().map(|v| v.round().max(0.0) as u32)
}

fn is_tracking_pixel_img(el: &scraper::node::Element) -> bool {
    if has_display_none(el) {
        return true;
    }

    let attr_w = el.attr("width").and_then(parse_css_dim_px);
    let attr_h = el.attr("height").and_then(parse_css_dim_px);
    if matches!((attr_w, attr_h), (Some(w), Some(h)) if w <= 1 && h <= 1)
        || attr_w == Some(0)
        || attr_h == Some(0)
    {
        return true;
    }

    if let Some(style) = el.attr("style") {
        let mut css_w: Option<u32> = None;
        let mut css_h: Option<u32> = None;
        for decl in style.split(';') {
            let Some((prop, val)) = decl.split_once(':') else {
                continue;
            };
            let prop_l = prop.trim().to_ascii_lowercase();
            if prop_l == "width" {
                css_w = parse_css_dim_px(val);
            } else if prop_l == "height" {
                css_h = parse_css_dim_px(val);
            }
        }
        if matches!((css_w, css_h), (Some(w), Some(h)) if w <= 1 && h <= 1)
            || css_w == Some(0)
            || css_h == Some(0)
        {
            return true;
        }
    }

    false
}

fn is_presentation_role(el: &scraper::node::Element) -> bool {
    el.attr("role").is_some_and(|r| {
        let trimmed = r.trim();
        trimmed.eq_ignore_ascii_case("presentation") || trimmed.eq_ignore_ascii_case("none")
    })
}

fn has_nested_table(table_el: ElementRef<'_>) -> bool {
    table_el.descendants().skip(1).any(|desc| {
        ElementRef::wrap(desc).is_some_and(|el| el.value().name() == "table")
    })
}

fn should_linearize_table(table_el: ElementRef<'_>) -> bool {
    if is_presentation_role(table_el.value()) || has_nested_table(table_el) {
        return true;
    }

    let mut max_cols = 0usize;
    for child in table_el.children() {
        let Some(child_el) = ElementRef::wrap(child) else {
            continue;
        };
        if is_presentation_role(child_el.value()) {
            return true;
        }
        match child_el.value().name() {
            "thead" | "tbody" | "tfoot" => {
                for row_el in child_el.children().filter_map(ElementRef::wrap) {
                    if row_el.value().name() == "tr" {
                        if is_presentation_role(row_el.value()) {
                            return true;
                        }
                        max_cols = max_cols.max(count_row_columns(row_el));
                    }
                }
            }
            "tr" => {
                max_cols = max_cols.max(count_row_columns(child_el));
            }
            _ => {}
        }
    }

    max_cols <= 1
}

fn count_row_columns(row_el: ElementRef<'_>) -> usize {
    let mut cols = 0usize;
    for cell in row_el.children().filter_map(ElementRef::wrap) {
        if matches!(cell.value().name(), "td" | "th") {
            let span = cell
                .value()
                .attr("colspan")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1);
            cols += span;
        }
    }
    cols
}

fn extract_cid(src: &str) -> Option<String> {
    let trimmed = src.trim();
    if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("cid:") {
        let norm = normalize_content_id(&trimmed[4..]);
        if !norm.is_empty() {
            return Some(norm);
        }
    }
    None
}

fn non_empty_attr<'a>(el: &'a scraper::node::Element, name: &str) -> Option<&'a str> {
    el.attr(name).map(str::trim).filter(|s| !s.is_empty())
}

fn is_emoji_sprite_img(el: &scraper::node::Element) -> bool {
    if el
        .attr("itemtype")
        .is_some_and(|t| t.to_ascii_lowercase().contains("emoji"))
    {
        return true;
    }
    if let Some(src) = el.attr("src") {
        let lower = src.to_ascii_lowercase();
        if lower.contains("statics.teams.cdn.office.net")
            || lower.contains("/personal-expressions/")
            || lower.contains("/emoticons/")
        {
            return true;
        }
    }
    false
}

fn detect_code_lang(pre_el: ElementRef<'_>) -> Option<String> {
    let el = pre_el.value();
    if let Some(lang) = el
        .attr("lang")
        .or_else(|| el.attr("data-lang"))
        .and_then(non_empty)
    {
        return Some(lang);
    }
    if let Some(lang) = lang_from_class(el.attr("class")) {
        return Some(lang);
    }
    for child in pre_el.children() {
        if let Node::Element(child_el) = child.value() {
            if child_el.name() == "code" {
                if let Some(lang) = child_el.attr("lang").and_then(non_empty) {
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

fn non_empty(s: &str) -> Option<String> {
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
                if is_ignored_noise_tag(el.name()) || has_display_none(el) {
                    continue;
                }
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

fn spans_are_blank(spans: &[RichSpan]) -> bool {
    spans.iter().all(|s| s.text.trim().is_empty())
}

fn trim_spans(mut spans: Vec<RichSpan>) -> Vec<RichSpan> {
    let start_idx = spans
        .iter()
        .position(|s| !s.text.trim_start_matches([' ', '\t', '\n', '\r']).is_empty())
        .unwrap_or(spans.len());
    if start_idx > 0 {
        spans.drain(..start_idx);
    }
    if let Some(first) = spans.first_mut() {
        let trimmed_len = first.text.trim_start_matches([' ', '\t', '\n', '\r']).len();
        let lead = first.text.len() - trimmed_len;
        if lead > 0 {
            first.text.drain(..lead);
        }
    }
    while let Some(last) = spans.last_mut() {
        let trimmed_len = last.text.trim_end_matches([' ', '\t', '\n', '\r']).len();
        if trimmed_len == 0 {
            spans.pop();
        } else {
            if trimmed_len < last.text.len() {
                last.text.truncate(trimmed_len);
            }
            break;
        }
    }
    spans
}

fn spans_to_plain(spans: &[RichSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn spans_to_markdown(spans: &[RichSpan]) -> String {
    let total_len: usize = spans.iter().map(|s| s.text.len()).sum();
    let mut out = String::with_capacity(total_len);
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let has_link = span.style.link_url.is_some();
        if has_link {
            out.push('[');
        }
        if !span.style.code {
            if span.style.underline {
                out.push_str("<u>");
            }
            if span.style.strike {
                out.push_str("~~");
            }
            if span.style.italic {
                out.push('*');
            }
            if span.style.bold {
                out.push_str("**");
            }
            out.push_str(&span.text);
            if span.style.bold {
                out.push_str("**");
            }
            if span.style.italic {
                out.push('*');
            }
            if span.style.strike {
                out.push_str("~~");
            }
            if span.style.underline {
                out.push_str("</u>");
            }
        } else if span.text.contains('`') {
            out.push_str("`` ");
            out.push_str(&span.text);
            out.push_str(" ``");
        } else {
            out.push('`');
            out.push_str(&span.text);
            out.push('`');
        }
        if let Some(url) = &span.style.link_url {
            out.push_str("](");
            out.push_str(url);
            out.push(')');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attachment(id: &str, name: &str, content_id: Option<&str>) -> MailAttachment {
        MailAttachment {
            id: id.to_string(),
            name: name.to_string(),
            content_type: "application/octet-stream".to_string(),
            size: 128,
            is_inline: content_id.is_some(),
            content_id: content_id.map(str::to_string),
            content_bytes_b64: None,
        }
    }

    #[test]
    fn parse_message_blocks_cache_hit_and_lru_eviction() {
        let html = "<p>Hello <b>Outlook</b></p>";
        let atts = vec![sample_attachment("a1", "spec.pdf", None)];

        let expected = parse_message_blocks_uncached(html, &atts);
        let first = parse_message_blocks(html, &atts);
        let second = parse_message_blocks(html, &atts);
        assert_eq!(first, expected);
        assert_eq!(second, expected);

        let mut cache = HtmlParseCache::default();
        for i in 0..HTML_CACHE_MAX_ENTRIES {
            cache.insert((i as u64, 1, i), Arc::from(Vec::new()));
        }
        assert!(cache.get(&(0, 1, 0)).is_some());
        cache.insert((99999, 1, 99999), Arc::from(Vec::new()));
        assert!(cache.get(&(0, 1, 0)).is_some());
        assert!(cache.get(&(1, 1, 1)).is_none());
    }

    #[test]
    fn strips_email_noise_and_tracking_pixels() {
        let html = r#"
            <head>
                <title>Hidden Title</title>
                <style>.foo { color: red; }</style>
                <meta charset="utf-8">
            </head>
            <xml><o:DocumentProperties><o:Author>Bot</o:Author></o:DocumentProperties></xml>
            <div style="display:none;max-height:0px;">Preheader preview noise</div>
            <p>Actual email body<o:p></o:p></p>
            <img src="https://track.example.com/open.gif" width="1" height="1">
            <img src="https://track.example.com/pixel.png" style="width: 1px; height: 1px;">
            <img src="https://track.example.com/hidden.png" style="display: none;">
            <script>alert(1)</script>
            <img src="cid:<image001.png@01DB1234>" width="400" height="200">
        "#;
        let blocks = parse_message_blocks(html, &[]);
        let plain = blocks_to_plain(&blocks);
        assert!(!plain.contains("Hidden Title"));
        assert!(!plain.contains("color: red"));
        assert!(!plain.contains("Preheader"));
        assert!(!plain.contains("alert(1)"));
        assert!(plain.contains("Actual email body"));

        let imgs: Vec<&RichBlock> = blocks
            .iter()
            .filter(|b| matches!(b, RichBlock::InlineImageChip { .. }))
            .collect();
        assert_eq!(imgs.len(), 1);
        match imgs[0] {
            RichBlock::InlineImageChip {
                src,
                content_id,
                index,
            } => {
                assert_eq!(src, "cid:<image001.png@01DB1234>");
                assert_eq!(content_id.as_deref(), Some("image001.png@01DB1234"));
                assert_eq!(*index, 0);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn flattens_layout_tables_and_preserves_data_tables() {
        let html = r#"
            <table role="presentation">
                <tr><td><h1>Newsletter Title</h1></td></tr>
                <tr><td>
                    <table>
                        <tr><td>
                            <p>Intro paragraph</p>
                            <table>
                                <thead><tr><th>Service</th><th>Status</th></tr></thead>
                                <tbody>
                                    <tr><td>Mail</td><td>Healthy</td></tr>
                                    <tr><td>Calendar</td><td>Degraded</td></tr>
                                </tbody>
                            </table>
                        </td></tr>
                    </table>
                </td></tr>
            </table>
        "#;
        let blocks = parse_message_blocks(html, &[]);
        assert!(matches!(blocks[0], RichBlock::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], RichBlock::Paragraph(_)));
        match &blocks[2] {
            RichBlock::Table { headers, rows } => {
                assert_eq!(headers.len(), 2);
                assert_eq!(spans_to_plain(&headers[0]), "Service");
                assert_eq!(spans_to_plain(&headers[1]), "Status");
                assert_eq!(rows.len(), 2);
                assert_eq!(spans_to_plain(&rows[0][0]), "Mail");
                assert_eq!(spans_to_plain(&rows[0][1]), "Healthy");
                assert_eq!(spans_to_plain(&rows[1][0]), "Calendar");
                assert_eq!(spans_to_plain(&rows[1][1]), "Degraded");
            }
            other => panic!("expected RichBlock::Table, got {other:?}"),
        }
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn folds_outlook_gmail_and_plain_quoted_history() {
        let outlook_html = r#"
            <div><p>Sounds good, let's ship it.</p></div>
            <div id="appendonsend"></div>
            <hr tabindex="-1">
            <div id="divRplyFwdMsg">
                <b>From:</b> Alice &lt;alice@example.com&gt;<br>
                <b>Sent:</b> Monday, Nov 18, 2026<br>
            </div>
            <div><p>Can we release today?</p><p>Thanks, Alice</p></div>
        "#;
        let blocks = parse_message_blocks(outlook_html, &[]);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks_to_plain(&blocks[..1]), "Sounds good, let's ship it.");
        match &blocks[1] {
            RichBlock::QuotedHistory { line_count, blocks } => {
                assert!(*line_count >= 3);
                let qplain = blocks_to_plain(blocks);
                assert!(qplain.contains("From: Alice"));
                assert!(qplain.contains("Can we release today?"));
            }
            other => panic!("expected QuotedHistory, got {other:?}"),
        }

        let gmail_html = r#"
            <div dir="ltr">I will review this afternoon.</div>
            <div class="gmail_quote">
                <div class="gmail_attr">On Mon, Nov 18, 2026 Alice wrote:</div>
                <blockquote class="gmail_quote">Please check PR #42.</blockquote>
            </div>
        "#;
        let gblocks = parse_message_blocks(gmail_html, &[]);
        assert_eq!(gblocks.len(), 2);
        assert!(matches!(gblocks[0], RichBlock::Paragraph(_)));
        assert!(matches!(gblocks[1], RichBlock::QuotedHistory { .. }));

        let plain_reply = "Yes, approved!\n\nOn Mon, Alice wrote:\n> Line one\n> Line two";
        let pblocks = parse_message_blocks(plain_reply, &[]);
        assert_eq!(pblocks.len(), 2);
        assert!(matches!(pblocks[0], RichBlock::Paragraph(_)));
        match &pblocks[1] {
            RichBlock::QuotedHistory { line_count, .. } => {
                assert!(*line_count >= 2);
            }
            other => panic!("expected QuotedHistory, got {other:?}"),
        }
    }
}
