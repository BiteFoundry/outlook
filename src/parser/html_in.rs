//! Outlook HTML + attachment parser into `Vec<RichBlock>`, plain text, and GFM Markdown.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use bitefoundry_html::pulldown_cmark::{
    CodeBlockKind, Event as MdEvent, HeadingLevel, Options as MdOptions, Parser as MdParser,
    Tag as MdTag, TagEnd as MdTagEnd,
};
use scraper::{ElementRef, Html, Node};

use crate::models::{MailAttachment, RichBlock, RichSpan, TextStyleFlags};

const HTML_CACHE_MAX_ENTRIES: usize = 1024;
const HTML_CACHE_MAX_HTML_BYTES: usize = 256 * 1024;

pub(crate) type HtmlCacheKey = (u64, usize);

struct CacheNode {
    key: HtmlCacheKey,
    value: Arc<[RichBlock]>,
    prev: Option<usize>,
    next: Option<usize>,
}

#[derive(Default)]
struct HtmlParseCache {
    index: HashMap<HtmlCacheKey, usize>,
    nodes: Vec<CacheNode>,
    head: Option<usize>,
    tail: Option<usize>,
}

impl HtmlParseCache {
    fn move_to_front(&mut self, idx: usize) {
        if self.head == Some(idx) {
            return;
        }
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        if let Some(p) = prev {
            self.nodes[p].next = next;
        }
        if let Some(n) = next {
            self.nodes[n].prev = prev;
        } else {
            self.tail = prev;
        }
        let old_head = self.head;
        self.nodes[idx].prev = None;
        self.nodes[idx].next = old_head;
        if let Some(h) = old_head {
            self.nodes[h].prev = Some(idx);
        } else {
            self.tail = Some(idx);
        }
        self.head = Some(idx);
    }

    fn get(&mut self, key: &HtmlCacheKey) -> Option<Arc<[RichBlock]>> {
        let &idx = self.index.get(key)?;
        self.move_to_front(idx);
        Some(Arc::clone(&self.nodes[idx].value))
    }

    fn insert(&mut self, key: HtmlCacheKey, value: Arc<[RichBlock]>) {
        if let Some(&idx) = self.index.get(&key) {
            self.nodes[idx].value = value;
            self.move_to_front(idx);
            return;
        }
        if self.nodes.len() < HTML_CACHE_MAX_ENTRIES {
            let idx = self.nodes.len();
            let old_head = self.head;
            self.nodes.push(CacheNode {
                key,
                value,
                prev: None,
                next: old_head,
            });
            if let Some(h) = old_head {
                self.nodes[h].prev = Some(idx);
            } else {
                self.tail = Some(idx);
            }
            self.head = Some(idx);
            self.index.insert(key, idx);
            return;
        }
        if let Some(evict_idx) = self.tail {
            let old_key = self.nodes[evict_idx].key;
            self.index.remove(&old_key);
            self.nodes[evict_idx].key = key;
            self.nodes[evict_idx].value = value;
            self.index.insert(key, evict_idx);
            self.move_to_front(evict_idx);
        }
    }
}

static HTML_PARSE_CACHE: OnceLock<Mutex<HtmlParseCache>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    pub(crate) static DOM_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Fast single-pass 64-bit word-chunked hasher with splitmix64 finalizer.
struct FastHash64(u64);

impl FastHash64 {
    #[inline]
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    #[inline]
    fn mix_word(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    #[inline]
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.mix_word(bytes.len() as u64);
        let (chunks, rem) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.mix_word(u64::from_le_bytes(*chunk));
        }
        if !rem.is_empty() {
            let mut tail = 0u64;
            for (i, &b) in rem.iter().enumerate() {
                tail |= (b as u64) << (i * 8);
            }
            self.mix_word(tail);
        }
    }

    #[inline]
    fn write_opt_str(&mut self, opt: Option<&str>) {
        match opt {
            Some(s) => {
                self.mix_word(1);
                self.write_bytes(s.as_bytes());
            }
            None => self.mix_word(0),
        }
    }

    #[inline]
    fn finish(self) -> u64 {
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

pub(crate) fn compute_parse_fingerprint(html: &str, attachments: &[MailAttachment]) -> HtmlCacheKey {
    let mut h = FastHash64::new(0x4854_4d4c_9e37_79b9);
    h.write_bytes(html.as_bytes());
    h.mix_word(attachments.len() as u64);
    for att in attachments {
        h.write_bytes(att.id.as_bytes());
        h.write_bytes(att.name.as_bytes());
        h.write_bytes(att.content_type.as_bytes());
        h.mix_word(att.size);
        h.mix_word(u64::from(att.is_inline));
        h.write_opt_str(att.content_id.as_deref());
    }
    (h.finish(), html.len())
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

fn has_plain_markdown_syntax(text: &str) -> bool {
    if text.contains('\n') || text.contains('\r') {
        return true;
    }
    if text.bytes().any(|b| {
        matches!(
            b,
            b'*' | b'_' | b'`' | b'[' | b']' | b'~' | b'#' | b'\\' | b'|' | b'>'
        )
    }) {
        return true;
    }
    let trimmed = text.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("+ ") || trimmed.starts_with("---") {
        return true;
    }
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && trimmed[digits..].starts_with(". ")
}

fn try_fast_path_plain_or_empty(
    html: &str,
    attachments: &[MailAttachment],
) -> Option<Arc<[RichBlock]>> {
    if !attachments.is_empty() {
        return None;
    }
    let trimmed = html.trim();
    if trimmed.is_empty() {
        return Some(Arc::from([]));
    }
    if !html.contains('<')
        && !html.contains('&')
        && !html.contains('\0')
        && !has_plain_quote_or_reply_marker(html)
        && !has_plain_markdown_syntax(html)
    {
        return Some(Arc::from([RichBlock::Paragraph(vec![RichSpan::plain(
            trimmed,
        )])]));
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
            #[cfg(test)]
            DOM_PARSE_COUNT.with(|count| count.set(count.get() + 1));
            let fragment = Html::parse_fragment(html);
            ctx.walk_children(fragment.root_element(), &TextStyleFlags::default());
            ctx.flush_paragraph();
        }
    }

    ctx.finalize_quoted_history();
    unwrap_sole_quoted_history(&mut ctx.blocks);

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
        let mut in_fence: Option<char> = None;

        let flush_normal = |ctx: &mut Self, buf: &mut Vec<&str>| {
            if !buf.is_empty() {
                let joined = buf.join("\n");
                if !joined.trim().is_empty() {
                    ctx.parse_plain_markdown_segment(&joined);
                }
                buf.clear();
            }
        };
        let flush_quote = |ctx: &mut Self, buf: &mut Vec<String>| {
            if !buf.is_empty() {
                let joined = buf.join("\n");
                if !joined.trim().is_empty() {
                    ctx.parse_plain_quote_segment(&joined);
                }
                buf.clear();
            }
        };

        for line in text.lines() {
            let trimmed_start = line.trim_start();
            if let Some(fence_ch) = in_fence {
                normal_lines.push(line.trim_end());
                if trimmed_start.starts_with(if fence_ch == '`' { "```" } else { "~~~" }) {
                    in_fence = None;
                }
                continue;
            }

            if let Some(after_gt) = trimmed_start.strip_prefix('>') {
                flush_normal(self, &mut normal_lines);
                quote_lines.push(
                    after_gt
                        .strip_prefix(' ')
                        .unwrap_or(after_gt)
                        .trim_end()
                        .to_string(),
                );
            } else if trimmed_start.is_empty() {
                if !quote_lines.is_empty() {
                    flush_quote(self, &mut quote_lines);
                } else {
                    normal_lines.push("");
                }
            } else {
                flush_quote(self, &mut quote_lines);
                if trimmed_start.starts_with("```") {
                    in_fence = Some('`');
                } else if trimmed_start.starts_with("~~~") {
                    in_fence = Some('~');
                }
                normal_lines.push(line.trim_end());
            }
        }
        flush_normal(self, &mut normal_lines);
        flush_quote(self, &mut quote_lines);
    }

    fn parse_plain_quote_segment(&mut self, segment: &str) {
        let spans = parse_markdown_inline_spans_block(segment);
        if !spans_are_blank(&spans) {
            self.push_block(RichBlock::Quote(trim_spans(spans)));
        }
    }

    fn parse_plain_markdown_segment(&mut self, segment: &str) {
        let opts = plain_markdown_options();
        let mut style_stack: Vec<TextStyleFlags> = vec![TextStyleFlags::default()];
        let mut cur_spans: Vec<RichSpan> = Vec::new();
        let mut heading_level: Option<u8> = None;
        let mut in_code_block = false;
        let mut code_lang: Option<String> = None;
        let mut code_buf = String::new();
        let mut list_items: Option<Vec<Vec<RichSpan>>> = None;
        let mut list_depth: usize = 0;
        let mut in_list_item = false;
        let mut ordered_counters: Vec<Option<u64>> = Vec::new();
        let mut blockquote_depth: usize = 0;
        let mut quote_spans: Vec<RichSpan> = Vec::new();
        let mut in_table_head = false;
        let mut table_headers: Vec<Vec<RichSpan>> = Vec::new();
        let mut table_rows: Vec<Vec<Vec<RichSpan>>> = Vec::new();
        let mut table_cur_row: Vec<Vec<RichSpan>> = Vec::new();
        let mut in_table_cell = false;

        let flush_paragraph = |ctx: &mut Self, spans: &mut Vec<RichSpan>| {
            let taken = std::mem::take(spans);
            if !spans_are_blank(&taken) {
                ctx.push_block(RichBlock::Paragraph(trim_spans(taken)));
            }
        };

        let flush_list_item = |spans: &mut Vec<RichSpan>,
                               items: &mut Option<Vec<Vec<RichSpan>>>,
                               depth: usize| {
            let taken = std::mem::take(spans);
            if !spans_are_blank(&taken) {
                let cleaned = if depth > 1 {
                    trim_spans_end(taken)
                } else {
                    trim_spans(taken)
                };
                if let Some(list) = items.as_mut() {
                    list.push(cleaned);
                }
            }
        };

        for event in MdParser::new_ext(segment, opts) {
            match event {
                MdEvent::Start(tag) => match tag {
                    MdTag::Paragraph => {
                        if blockquote_depth > 0 {
                            if !quote_spans.is_empty() {
                                push_rich_span(&mut quote_spans, "\n", &TextStyleFlags::default());
                            }
                        } else if in_list_item && !cur_spans.is_empty() {
                            push_rich_span(&mut cur_spans, "\n", &TextStyleFlags::default());
                        }
                    }
                    MdTag::Heading { level, .. } => {
                        if blockquote_depth == 0 && list_depth == 0 {
                            flush_paragraph(self, &mut cur_spans);
                        }
                        heading_level = Some(md_heading_level_u8(level));
                    }
                    MdTag::BlockQuote(_) => {
                        if blockquote_depth == 0 && list_depth == 0 {
                            flush_paragraph(self, &mut cur_spans);
                        }
                        blockquote_depth += 1;
                    }
                    MdTag::CodeBlock(kind) => {
                        if blockquote_depth == 0 && list_depth == 0 {
                            flush_paragraph(self, &mut cur_spans);
                        }
                        in_code_block = true;
                        code_buf.clear();
                        code_lang = match kind {
                            CodeBlockKind::Fenced(info) => {
                                non_empty(info.split_whitespace().next().unwrap_or(""))
                            }
                            CodeBlockKind::Indented => None,
                        };
                    }
                    MdTag::List(start_num) => {
                        if list_depth == 0 {
                            if blockquote_depth == 0 {
                                flush_paragraph(self, &mut cur_spans);
                            }
                            list_items = Some(Vec::new());
                        } else if in_list_item {
                            flush_list_item(&mut cur_spans, &mut list_items, list_depth);
                        }
                        list_depth += 1;
                        ordered_counters.push(start_num);
                    }
                    MdTag::Item => {
                        in_list_item = true;
                        let target = if blockquote_depth > 0 {
                            &mut quote_spans
                        } else {
                            &mut cur_spans
                        };
                        if blockquote_depth > 0 && !target.is_empty() {
                            push_rich_span(target, "\n", &TextStyleFlags::default());
                        }
                        if list_depth > 1 {
                            let indent = "  ".repeat(list_depth - 1);
                            push_rich_span(target, &indent, &TextStyleFlags::default());
                        }
                        if let Some(Some(n)) = ordered_counters.last_mut() {
                            let prefix = format!("{n}. ");
                            *n += 1;
                            push_rich_span(target, &prefix, &TextStyleFlags::default());
                        } else if blockquote_depth > 0 {
                            push_rich_span(target, "- ", &TextStyleFlags::default());
                        }
                    }
                    MdTag::Table(_) => {
                        if blockquote_depth == 0 && list_depth == 0 {
                            flush_paragraph(self, &mut cur_spans);
                        }
                        table_headers.clear();
                        table_rows.clear();
                        table_cur_row.clear();
                        in_table_head = false;
                        in_table_cell = false;
                    }
                    MdTag::TableHead => {
                        in_table_head = true;
                        table_cur_row.clear();
                    }
                    MdTag::TableRow => {
                        table_cur_row.clear();
                    }
                    MdTag::TableCell => {
                        in_table_cell = true;
                        cur_spans.clear();
                    }
                    MdTag::Strong => {
                        let mut next = style_stack.last().cloned().unwrap_or_default();
                        next.bold = true;
                        style_stack.push(next);
                    }
                    MdTag::Emphasis => {
                        let mut next = style_stack.last().cloned().unwrap_or_default();
                        next.italic = true;
                        style_stack.push(next);
                    }
                    MdTag::Strikethrough => {
                        let mut next = style_stack.last().cloned().unwrap_or_default();
                        next.strike = true;
                        style_stack.push(next);
                    }
                    MdTag::Link { dest_url, .. } | MdTag::Image { dest_url, .. } => {
                        let mut next = style_stack.last().cloned().unwrap_or_default();
                        if let Some(url) = non_empty(&dest_url) {
                            next.link_url = Some(url);
                        }
                        style_stack.push(next);
                    }
                    _ => {}
                },
                MdEvent::End(tag_end) => match tag_end {
                    MdTagEnd::Paragraph => {
                        if blockquote_depth == 0 && !in_list_item && !in_table_cell {
                            flush_paragraph(self, &mut cur_spans);
                        }
                    }
                    MdTagEnd::Heading(_) => {
                        let level = heading_level.take().unwrap_or(1);
                        if blockquote_depth == 0 && !in_list_item {
                            let spans = std::mem::take(&mut cur_spans);
                            if !spans_are_blank(&spans) {
                                self.push_block(RichBlock::Heading {
                                    level,
                                    spans: trim_spans(spans),
                                });
                            }
                        }
                    }
                    MdTagEnd::BlockQuote(_) => {
                        blockquote_depth = blockquote_depth.saturating_sub(1);
                        if blockquote_depth == 0 {
                            let spans = std::mem::take(&mut quote_spans);
                            if !spans_are_blank(&spans) {
                                self.push_block(RichBlock::Quote(trim_spans(spans)));
                            }
                        }
                    }
                    MdTagEnd::CodeBlock => {
                        in_code_block = false;
                        let trimmed = code_buf.trim_matches(|c| c == '\r' || c == '\n');
                        if !trimmed.is_empty() {
                            if blockquote_depth > 0 {
                                if !quote_spans.is_empty() {
                                    push_rich_span(
                                        &mut quote_spans,
                                        "\n",
                                        &TextStyleFlags::default(),
                                    );
                                }
                                let code_style = TextStyleFlags {
                                    code: true,
                                    ..TextStyleFlags::default()
                                };
                                push_rich_span(&mut quote_spans, trimmed, &code_style);
                            } else {
                                self.push_block(RichBlock::CodeBlock {
                                    lang: code_lang.take(),
                                    code: trimmed.to_string(),
                                });
                            }
                        }
                        code_lang = None;
                        code_buf.clear();
                    }
                    MdTagEnd::Item => {
                        if blockquote_depth == 0 {
                            flush_list_item(&mut cur_spans, &mut list_items, list_depth);
                        }
                        in_list_item = false;
                    }
                    MdTagEnd::List(_) => {
                        ordered_counters.pop();
                        list_depth = list_depth.saturating_sub(1);
                        if list_depth == 0 {
                            if let Some(items) = list_items.take() {
                                if !items.is_empty() && blockquote_depth == 0 {
                                    self.push_block(RichBlock::BulletList(items));
                                }
                            }
                        }
                    }
                    MdTagEnd::TableHead => {
                        in_table_head = false;
                        if !table_cur_row.is_empty() {
                            table_headers = std::mem::take(&mut table_cur_row);
                        }
                    }
                    MdTagEnd::TableRow => {
                        if !table_cur_row.is_empty() {
                            table_rows.push(std::mem::take(&mut table_cur_row));
                        }
                    }
                    MdTagEnd::TableCell => {
                        in_table_cell = false;
                        table_cur_row.push(trim_spans(std::mem::take(&mut cur_spans)));
                    }
                    MdTagEnd::Table => {
                        if !table_headers.is_empty() || !table_rows.is_empty() {
                            self.push_block(RichBlock::Table {
                                headers: std::mem::take(&mut table_headers),
                                rows: std::mem::take(&mut table_rows),
                            });
                        }
                        let _ = in_table_head;
                    }
                    MdTagEnd::Strong
                    | MdTagEnd::Emphasis
                    | MdTagEnd::Strikethrough
                    | MdTagEnd::Link
                    | MdTagEnd::Image
                        if style_stack.len() > 1 =>
                    {
                        style_stack.pop();
                    }
                    _ => {}
                },
                MdEvent::Text(text) => {
                    if in_code_block {
                        code_buf.push_str(&text);
                    } else {
                        let style = style_stack.last().cloned().unwrap_or_default();
                        let target = if blockquote_depth > 0 {
                            &mut quote_spans
                        } else {
                            &mut cur_spans
                        };
                        push_rich_span(target, &text, &style);
                    }
                }
                MdEvent::Code(code) => {
                    if in_code_block {
                        code_buf.push_str(&code);
                    } else {
                        let mut style = style_stack.last().cloned().unwrap_or_default();
                        style.code = true;
                        let target = if blockquote_depth > 0 {
                            &mut quote_spans
                        } else {
                            &mut cur_spans
                        };
                        push_rich_span(target, &code, &style);
                    }
                }
                MdEvent::SoftBreak | MdEvent::HardBreak => {
                    if in_code_block {
                        code_buf.push('\n');
                    } else {
                        let target = if blockquote_depth > 0 {
                            &mut quote_spans
                        } else {
                            &mut cur_spans
                        };
                        push_rich_span(target, "\n", &TextStyleFlags::default());
                    }
                }
                MdEvent::TaskListMarker(checked) => {
                    let target = if blockquote_depth > 0 {
                        &mut quote_spans
                    } else {
                        &mut cur_spans
                    };
                    push_rich_span(
                        target,
                        if checked { "☑ " } else { "☐ " },
                        &TextStyleFlags::default(),
                    );
                }
                MdEvent::Rule if blockquote_depth == 0 && list_depth == 0 => {
                    flush_paragraph(self, &mut cur_spans);
                    self.push_block(RichBlock::Divider);
                }
                _ => {}
            }
        }

        if !spans_are_blank(&cur_spans) {
            flush_paragraph(self, &mut cur_spans);
        }
        if !spans_are_blank(&quote_spans) {
            self.push_block(RichBlock::Quote(trim_spans(quote_spans)));
        }
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
        for child in table_el.children() {
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(_) => {
                    let Some(child_el) = ElementRef::wrap(child) else {
                        continue;
                    };
                    let el = child_el.value();
                    if is_ignored_noise_tag(el.name()) || has_display_none(el) {
                        continue;
                    }
                    if !self.in_quoted_history && is_quoted_history_boundary(child_el) {
                        self.flush_paragraph();
                        self.in_quoted_history = true;
                    }
                    match el.name() {
                        "thead" | "tbody" | "tfoot" => {
                            for row_node in child_el.children() {
                                match row_node.value() {
                                    Node::Text(text) => {
                                        self.push_inline_text(text.text.as_ref(), style);
                                    }
                                    Node::Element(_) => {
                                        if let Some(row_el) = ElementRef::wrap(row_node) {
                                            let rel = row_el.value();
                                            if is_ignored_noise_tag(rel.name())
                                                || has_display_none(rel)
                                            {
                                                continue;
                                            }
                                            if !self.in_quoted_history
                                                && is_quoted_history_boundary(row_el)
                                            {
                                                self.flush_paragraph();
                                                self.in_quoted_history = true;
                                            }
                                            if rel.name() == "tr" {
                                                self.linearize_table_row(row_el, style);
                                            } else {
                                                self.handle_element(row_el, style);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "tr" => {
                            self.linearize_table_row(child_el, style);
                        }
                        "caption" => {
                            self.flush_paragraph();
                            self.walk_children(child_el, style);
                            self.flush_paragraph();
                        }
                        _ => {
                            self.handle_element(child_el, style);
                        }
                    }
                }
                _ => {}
            }
        }
        self.flush_paragraph();
    }

    fn linearize_table_row(&mut self, row_el: ElementRef<'_>, style: &TextStyleFlags) {
        for cell_node in row_el.children() {
            match cell_node.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(_) => {
                    let Some(cell_el) = ElementRef::wrap(cell_node) else {
                        continue;
                    };
                    let cel = cell_el.value();
                    if is_ignored_noise_tag(cel.name()) || has_display_none(cel) {
                        continue;
                    }
                    if !self.in_quoted_history && is_quoted_history_boundary(cell_el) {
                        self.flush_paragraph();
                        self.in_quoted_history = true;
                    }
                    if matches!(cel.name(), "td" | "th") {
                        self.flush_paragraph();
                        self.walk_children(cell_el, style);
                        self.flush_paragraph();
                    } else {
                        self.handle_element(cell_el, style);
                    }
                }
                _ => {}
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
                    let cell_spans = trim_spans(self.collect_quote_spans(cell_el, style));
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
            if has_visible_content(&self.blocks) {
                let line_count = count_blocks_lines(&quoted);
                self.blocks.push(RichBlock::QuotedHistory {
                    line_count,
                    blocks: quoted,
                });
            } else {
                self.blocks.extend(quoted);
            }
            return;
        }

        // Check for trailing quote chains or plain-text Outlook/Gmail reply headers at the end of `self.blocks`.
        if let Some(split_idx) = find_trailing_quoted_split_index(&self.blocks) {
            if has_visible_content(&self.blocks[..split_idx]) {
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

fn block_has_visible_content(block: &RichBlock) -> bool {
    match block {
        RichBlock::Paragraph(spans)
        | RichBlock::Heading { spans, .. }
        | RichBlock::Quote(spans) => !spans_are_blank(spans),
        RichBlock::CodeBlock { code, .. } => !code.trim().is_empty(),
        RichBlock::BulletList(items) => items.iter().any(|item| !spans_are_blank(item)),
        RichBlock::Table { headers, rows } => !headers.is_empty() || !rows.is_empty(),
        RichBlock::AttachmentChip { .. } | RichBlock::InlineImageChip { .. } => true,
        RichBlock::Divider | RichBlock::QuotedHistory { .. } => false,
    }
}

fn has_visible_content(blocks: &[RichBlock]) -> bool {
    blocks.iter().any(block_has_visible_content)
}

fn unwrap_sole_quoted_history(blocks: &mut Vec<RichBlock>) {
    while !has_visible_content(blocks)
        && blocks
            .iter()
            .any(|b| matches!(b, RichBlock::QuotedHistory { .. }))
    {
        let prev = std::mem::take(blocks);
        for b in prev {
            match b {
                RichBlock::QuotedHistory { blocks: inner, .. } => {
                    blocks.extend(inner);
                }
                other => blocks.push(other),
            }
        }
    }
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
            let is_en_hdr = trimmed.starts_with("-----Original Message-----")
                || (trimmed.contains("From:")
                    && (trimmed.contains("Sent:") || trimmed.contains("Date:")));
            let is_ko_hdr = trimmed.contains("보낸 사람:")
                && (trimmed.contains("보낸 날짜:") || trimmed.contains("제목:"));
            if (is_en_hdr || is_ko_hdr) && has_visible_content(&blocks[..idx]) {
                return Some(idx);
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
        let has_en_hdr = tail_text.contains("From:")
            && (tail_text.contains("Sent:")
                || tail_text.contains("Date:")
                || tail_text.contains("Subject:"));
        let has_ko_hdr = tail_text.contains("보낸 사람:")
            && (tail_text.contains("보낸 날짜:") || tail_text.contains("제목:"));
        if has_en_hdr || has_ko_hdr {
            return true;
        }
    }
    false
}

fn is_ignored_noise_tag(tag: &str) -> bool {
    matches!(
        tag,
        "head"
            | "style"
            | "script"
            | "meta"
            | "title"
            | "xml"
            | "o:p"
            | "noscript"
            | "template"
            | "svg"
            | "canvas"
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

fn row_has_th_or_block_td(row_el: ElementRef<'_>, has_th: &mut bool, has_block_td: &mut bool) {
    for cell in row_el.children().filter_map(ElementRef::wrap) {
        match cell.value().name() {
            "th" => *has_th = true,
            "td" if cell.descendants().skip(1).any(|desc| {
                ElementRef::wrap(desc).is_some_and(|el| {
                    matches!(
                        el.value().name(),
                        "div"
                            | "p"
                            | "ul"
                            | "ol"
                            | "blockquote"
                            | "pre"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "hr"
                            | "table"
                    )
                })
            }) =>
            {
                *has_block_td = true;
            }
            _ => {}
        }
    }
}

fn should_linearize_table(table_el: ElementRef<'_>) -> bool {
    if is_presentation_role(table_el.value()) || has_nested_table(table_el) {
        return true;
    }

    let mut max_cols = 0usize;
    let mut has_th_or_thead = false;
    let mut has_block_td = false;
    for child in table_el.children() {
        let Some(child_el) = ElementRef::wrap(child) else {
            continue;
        };
        if is_presentation_role(child_el.value()) {
            return true;
        }
        match child_el.value().name() {
            "thead" | "tbody" | "tfoot" => {
                if child_el.value().name() == "thead" {
                    has_th_or_thead = true;
                }
                for row_el in child_el.children().filter_map(ElementRef::wrap) {
                    if row_el.value().name() == "tr" {
                        if is_presentation_role(row_el.value()) {
                            return true;
                        }
                        max_cols = max_cols.max(count_row_columns(row_el));
                        row_has_th_or_block_td(row_el, &mut has_th_or_thead, &mut has_block_td);
                    }
                }
            }
            "tr" => {
                max_cols = max_cols.max(count_row_columns(child_el));
                row_has_th_or_block_td(child_el, &mut has_th_or_thead, &mut has_block_td);
            }
            _ => {}
        }
    }

    max_cols <= 1 || (!has_th_or_thead && has_block_td)
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

fn plain_markdown_options() -> MdOptions {
    MdOptions::ENABLE_TABLES
        | MdOptions::ENABLE_TASKLISTS
        | MdOptions::ENABLE_STRIKETHROUGH
        | MdOptions::ENABLE_GFM
}

fn md_heading_level_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn push_rich_span(spans: &mut Vec<RichSpan>, text: &str, style: &TextStyleFlags) {
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

fn trim_spans_end(mut spans: Vec<RichSpan>) -> Vec<RichSpan> {
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

fn parse_markdown_inline_spans_block(segment: &str) -> Vec<RichSpan> {
    let mut style_stack: Vec<TextStyleFlags> = vec![TextStyleFlags::default()];
    let mut spans: Vec<RichSpan> = Vec::new();
    let mut in_code_block = false;
    let mut ordered_counters: Vec<Option<u64>> = Vec::new();

    for event in MdParser::new_ext(segment, plain_markdown_options()) {
        match event {
            MdEvent::Start(tag) => match tag {
                MdTag::Paragraph | MdTag::Heading { .. } => {
                    if !spans.is_empty() {
                        push_rich_span(&mut spans, "\n", &TextStyleFlags::default());
                    }
                    if matches!(tag, MdTag::Heading { .. }) {
                        let mut next = style_stack.last().cloned().unwrap_or_default();
                        next.bold = true;
                        style_stack.push(next);
                    }
                }
                MdTag::CodeBlock(_) => {
                    if !spans.is_empty() {
                        push_rich_span(&mut spans, "\n", &TextStyleFlags::default());
                    }
                    in_code_block = true;
                }
                MdTag::List(start_num) => {
                    ordered_counters.push(start_num);
                }
                MdTag::Item => {
                    if !spans.is_empty() {
                        push_rich_span(&mut spans, "\n", &TextStyleFlags::default());
                    }
                    if let Some(Some(n)) = ordered_counters.last_mut() {
                        let prefix = format!("{n}. ");
                        *n += 1;
                        push_rich_span(&mut spans, &prefix, &TextStyleFlags::default());
                    } else {
                        push_rich_span(&mut spans, "- ", &TextStyleFlags::default());
                    }
                }
                MdTag::Strong => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.bold = true;
                    style_stack.push(next);
                }
                MdTag::Emphasis => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.italic = true;
                    style_stack.push(next);
                }
                MdTag::Strikethrough => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.strike = true;
                    style_stack.push(next);
                }
                MdTag::Link { dest_url, .. } | MdTag::Image { dest_url, .. } => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    if let Some(url) = non_empty(&dest_url) {
                        next.link_url = Some(url);
                    }
                    style_stack.push(next);
                }
                _ => {}
            },
            MdEvent::End(tag_end) => match tag_end {
                MdTagEnd::CodeBlock => {
                    in_code_block = false;
                }
                MdTagEnd::List(_) => {
                    ordered_counters.pop();
                }
                MdTagEnd::Heading(_)
                | MdTagEnd::Strong
                | MdTagEnd::Emphasis
                | MdTagEnd::Strikethrough
                | MdTagEnd::Link
                | MdTagEnd::Image
                    if style_stack.len() > 1 =>
                {
                    style_stack.pop();
                }
                _ => {}
            },
            MdEvent::Text(text) => {
                if in_code_block {
                    let style = TextStyleFlags {
                        code: true,
                        ..TextStyleFlags::default()
                    };
                    push_rich_span(&mut spans, text.trim_end_matches('\n'), &style);
                } else {
                    let style = style_stack.last().cloned().unwrap_or_default();
                    push_rich_span(&mut spans, &text, &style);
                }
            }
            MdEvent::Code(code) => {
                let mut style = style_stack.last().cloned().unwrap_or_default();
                style.code = true;
                push_rich_span(&mut spans, &code, &style);
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                push_rich_span(&mut spans, "\n", &TextStyleFlags::default());
            }
            MdEvent::TaskListMarker(checked) => {
                push_rich_span(
                    &mut spans,
                    if checked { "☑ " } else { "☐ " },
                    &TextStyleFlags::default(),
                );
            }
            _ => {}
        }
    }

    spans
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
            cache.insert((i as u64, i), Arc::from(Vec::new()));
        }
        assert!(cache.get(&(0, 0)).is_some());
        cache.insert((99999, 99999), Arc::from(Vec::new()));
        assert!(cache.get(&(0, 0)).is_some());
        assert!(cache.get(&(1, 1)).is_none());
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

    #[test]
    fn unwraps_quoted_history_when_no_visible_body_precedes_it() {
        let fwd_div_only = r#"
            <div id="divRplyFwdMsg">
                <b>From:</b> Alice &lt;alice@example.com&gt;<br>
                <b>Sent:</b> Monday, Nov 18, 2026<br>
            </div>
            <div><p>Forwarded corporate message body</p></div>
        "#;
        let blocks = parse_message_blocks(fwd_div_only, &[]);
        assert!(
            !blocks
                .iter()
                .any(|b| matches!(b, RichBlock::QuotedHistory { .. })),
            "expected sole QuotedHistory to unwrap, got {blocks:?}"
        );
        let plain = blocks_to_plain(&blocks);
        assert!(plain.contains("From: Alice"));
        assert!(plain.contains("Forwarded corporate message body"));

        let ko_fwd = r#"
            <hr tabindex="-1">
            <p>보낸 사람: 김철수<br>보낸 날짜: 2026-11-18<br>제목: 공유 드립니다</p>
            <p>본문 내용이 숨겨지면 안 됩니다.</p>
        "#;
        let ko_blocks = parse_message_blocks(ko_fwd, &[]);
        assert!(
            !ko_blocks
                .iter()
                .any(|b| matches!(b, RichBlock::QuotedHistory { .. })),
            "expected Korean forwarded body not to collapse into QuotedHistory: {ko_blocks:?}"
        );
        let ko_plain = blocks_to_plain(&ko_blocks);
        assert!(ko_plain.contains("보낸 사람: 김철수"));
        assert!(ko_plain.contains("본문 내용이 숨겨지면 안 됩니다."));

        let gmail_only = r#"<blockquote class="gmail_quote"><p>Entire reply wrapped in blockquote</p></blockquote>"#;
        let gblocks = parse_message_blocks(gmail_only, &[]);
        assert!(
            !gblocks
                .iter()
                .any(|b| matches!(b, RichBlock::QuotedHistory { .. })),
            "expected blockquote-only body to be visible: {gblocks:?}"
        );
        assert!(blocks_to_plain(&gblocks).contains("Entire reply wrapped in blockquote"));
    }

    #[test]
    fn deeply_nested_corporate_layout_tables_preserve_all_text() {
        let html = r#"
            <table width="100%">
                <tr><td>
                    <table width="600">
                        <tr>
                            <td width="20"> </td>
                            <td>
                                <div><h3>Notice</h3></div>
                                <table>
                                    <tr><td>
                                        <p>Deeply nested corporate content</p>
                                    </td></tr>
                                </table>
                            </td>
                        </tr>
                    </table>
                </td></tr>
            </table>
        "#;
        let blocks = parse_message_blocks(html, &[]);
        let plain = blocks_to_plain(&blocks);
        assert!(plain.contains("Notice"));
        assert!(plain.contains("Deeply nested corporate content"));
    }

    #[test]
    fn strips_inline_svg_canvas_style_and_display_none_without_leaking_noise() {
        let html = r#"
            <style>@media (max-width: 600px) { .banner { color: red; } } .x { display: none; }</style>
            <div style="display:none">hidden-preheader-secret</div>
            <p>Visible top text</p>
            <svg width="100" height="50">
                <desc>secret</desc>
                <title>svg-hidden-title</title>
                <rect width="10" height="10"/>
                <path d="M0 0 L10 10"/>
            </svg>
            <canvas width="200" height="100">canvas-fallback-noise</canvas>
            <blockquote>
                <style>.q { font-size: 12px; }</style>
                <span style="display:none">hidden-in-quote</span>
                <svg><desc>quote-svg-secret</desc><rect width="5" height="5"/></svg>
                <p>Quoted visible sentence</p>
            </blockquote>
            <p>Visible bottom text</p>
        "#;

        let blocks = parse_message_blocks(html, &[]);
        let plain = blocks_to_plain(&blocks);
        let md = blocks_to_markdown(&blocks);

        for out in [&plain, &md] {
            assert!(out.contains("Visible top text"), "missing top text: {out}");
            assert!(out.contains("Visible bottom text"), "missing bottom text: {out}");
            assert!(
                out.contains("Quoted visible sentence"),
                "missing quoted text: {out}"
            );
            for leaked in [
                "secret",
                "svg-hidden-title",
                "quote-svg-secret",
                "canvas-fallback-noise",
                "hidden-preheader-secret",
                "hidden-in-quote",
                "color: red",
                "@media",
                "font-size",
                "<svg",
                "<rect",
                "<path",
            ] {
                assert!(
                    !out.contains(leaked),
                    "HTML parser leaked raw SVG/CSS/hidden noise `{leaked}` into output:\n{out}"
                );
            }
        }
    }

    #[test]
    fn plain_text_mail_preserves_newlines_and_md() {
        let plain_md = "# Release Notes\n\nLine 1\nLine 2\n\n- **Item** with `code` and [link](https://example.com)\n- *Second* item\n\n> Quoted **important** note";
        let blocks = parse_message_blocks(plain_md, &[]);

        assert!(
            matches!(&blocks[0], RichBlock::Heading { level: 1, spans } if spans_to_plain(spans) == "Release Notes"),
            "expected H1 heading, got: {blocks:?}"
        );
        match &blocks[1] {
            RichBlock::Paragraph(spans) => {
                assert_eq!(spans_to_plain(spans), "Line 1\nLine 2");
            }
            other => panic!("expected Paragraph preserving newline, got: {other:?}"),
        }
        match &blocks[2] {
            RichBlock::BulletList(items) => {
                assert_eq!(items.len(), 2);
                assert!(
                    items[0].iter().any(|s| s.text == "Item" && s.style.bold),
                    "expected bold span for **Item**, got: {:?}",
                    items[0]
                );
                assert!(
                    items[0].iter().any(|s| s.text == "code" && s.style.code),
                    "expected inline code span, got: {:?}",
                    items[0]
                );
                assert!(
                    items[0]
                        .iter()
                        .any(|s| s.text == "link"
                            && s.style.link_url.as_deref() == Some("https://example.com")),
                    "expected link span, got: {:?}",
                    items[0]
                );
                assert!(
                    items[1].iter().any(|s| s.text == "Second" && s.style.italic),
                    "expected italic span for *Second*, got: {:?}",
                    items[1]
                );
            }
            other => panic!("expected BulletList, got: {other:?}"),
        }

        let plain_out = blocks_to_plain(&blocks);
        assert!(
            !plain_out.contains("**"),
            "raw markdown asterisks must not leak into plain output:\n{plain_out}"
        );
        assert!(plain_out.contains("Line 1\nLine 2"));

        // Focused requirement fixture: `Line 1\n\n- **Item**`
        let short_blocks = parse_message_blocks("Line 1\n\n- **Item**", &[]);
        assert_eq!(short_blocks.len(), 2, "got: {short_blocks:?}");
        assert!(matches!(&short_blocks[0], RichBlock::Paragraph(s) if spans_to_plain(s) == "Line 1"));
        match &short_blocks[1] {
            RichBlock::BulletList(items) => {
                assert_eq!(items.len(), 1);
                assert!(items[0].iter().any(|s| s.text == "Item" && s.style.bold));
            }
            other => panic!("expected BulletList, got: {other:?}"),
        }
        assert_eq!(blocks_to_plain(&short_blocks), "Line 1\n- Item");
    }
}
