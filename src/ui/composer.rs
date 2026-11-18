//! Reusable multiline soft-wrapped Markdown editor: UTF-8 char-boundary
//! cursor/selection/undo/redo, clipboard image staging (`cid:` inline images),
//! and external `$EDITOR` editing.

use std::{ops::Range, process::Command};

use image::{codecs::png::PngEncoder, ExtendedColorType, ImageBuffer, ImageEncoder, Rgba};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthChar;

use crate::{
    img_preview::{self, RenderedTerminalImage},
    models::InlineImageUpload,
    ui::theme::Theme,
};

const MAX_UNDO: usize = 200;

#[derive(Debug, Clone, Default)]
struct Snapshot {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone)]
pub struct ComposerState {
    text: String,
    cursor: usize,
    pub pending_images: Vec<InlineImageUpload>,
    pub pending_preview: Option<RenderedTerminalImage>,
    pub last_inner_width: u16,
    anchor: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl PartialEq for ComposerState {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.cursor == other.cursor
            && self.pending_images.len() == other.pending_images.len()
    }
}

impl Eq for ComposerState {}

impl Default for ComposerState {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            pending_images: Vec::new(),
            pending_preview: None,
            last_inner_width: 72,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

impl ComposerState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.pending_images.is_empty()
    }

    pub fn has_no_text(&self) -> bool {
        self.text.is_empty()
    }

    // ------------------------------------------------------------------
    // Editing
    // ------------------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\r' {
            return;
        }
        let mut buf = [0u8; 4];
        self.insert_replacing_selection(ch.encode_utf8(&mut buf));
    }

    pub fn insert_str(&mut self, value: &str) {
        let value = normalize(value);
        if value.is_empty() {
            return;
        }
        self.insert_replacing_selection(&value);
    }

    fn insert_replacing_selection(&mut self, value: &str) {
        if let Some((start, end)) = self.selection_range() {
            self.record();
            self.text.replace_range(start..end, value);
            self.cursor = start + value.len();
            self.anchor = None;
        } else {
            self.insert_at(self.cursor, value);
        }
    }

    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let Some(prev) = prev_boundary(&self.text, self.cursor) else {
            return;
        };
        self.delete_range(prev..self.cursor);
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.delete_range(self.cursor..next);
        }
    }

    pub fn delete_word_backward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor.min(self.text.len());
        if pos == 0 {
            return;
        }
        let head = &self.text[..pos];
        let mut idx = head.len();
        while let Some((p, ch)) = head[..idx].char_indices().next_back() {
            if ch.is_whitespace() && ch != '\n' {
                idx = p;
            } else {
                break;
            }
        }
        if idx == head.len() {
            if let Some((p, '\n')) = head.char_indices().next_back() {
                self.delete_range(p..pos);
                return;
            }
        }
        while let Some((p, ch)) = head[..idx].char_indices().next_back() {
            if !ch.is_whitespace() {
                idx = p;
            } else {
                break;
            }
        }
        if idx < pos {
            self.delete_range(idx..pos);
        }
    }

    pub fn clear(&mut self) {
        if self.text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        self.record();
        self.text.clear();
        self.cursor = 0;
        self.anchor = None;
        self.pending_images.clear();
        self.pending_preview = None;
    }

    pub fn stage_inline_image(
        &mut self,
        bytes: Vec<u8>,
        mime_type: &str,
        file_name: Option<String>,
    ) -> String {
        let preview = img_preview::decode_and_render_halfblocks(&bytes, 24, 5, (20, 22, 26)).ok();
        self.stage_inline_image_with_preview(bytes, mime_type, file_name, preview)
    }

    pub fn stage_inline_image_with_preview(
        &mut self,
        bytes: Vec<u8>,
        mime_type: &str,
        file_name: Option<String>,
        preview: Option<RenderedTerminalImage>,
    ) -> String {
        self.pending_preview = preview;
        let idx = self.pending_images.len() + 1;
        let temp_id = idx.to_string();
        let content_id = format!("inline-img-{idx}@outlook-cli");
        let name = file_name.unwrap_or_else(|| format!("screenshot-{idx}.png"));
        self.pending_images.push(InlineImageUpload {
            temp_id,
            content_id: content_id.clone(),
            file_name: name,
            content_type: mime_type.to_string(),
            bytes,
        });
        content_id
    }

    pub fn clear_pending_images(&mut self) {
        self.pending_images.clear();
        self.pending_preview = None;
    }

    pub fn load_text(&mut self, value: &str) {
        self.text = normalize(value);
        self.cursor = self.text.len();
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
    }

    pub fn load_text_cursor_start(&mut self, value: &str) {
        self.text = normalize(value);
        self.cursor = 0;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
    }

    fn insert_at(&mut self, pos: usize, value: &str) {
        if value.is_empty() || pos > self.text.len() || !self.text.is_char_boundary(pos) {
            return;
        }
        self.record();
        self.text.insert_str(pos, value);
        self.cursor = pos + value.len();
        self.anchor = None;
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        self.record();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = None;
    }

    // ------------------------------------------------------------------
    // Selection / Cursor
    // ------------------------------------------------------------------

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub fn selected_text(&self) -> Option<&str> {
        let (start, end) = self.selection_range()?;
        self.text.get(start..end)
    }

    pub fn select_all(&mut self) {
        if self.text.is_empty() {
            self.anchor = None;
            return;
        }
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    pub fn delete_selection(&mut self) -> bool {
        match self.selection_range() {
            Some((start, end)) => {
                self.delete_range(start..end);
                true
            }
            None => false,
        }
    }

    pub fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some((start, _)) = self.selection_range() {
                self.anchor = None;
                self.cursor = start;
                return;
            }
        }
        if let Some(prev) = prev_boundary(&self.text, self.cursor) {
            self.move_to(prev, extend);
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some((_, end)) = self.selection_range() {
                self.anchor = None;
                self.cursor = end;
                return;
            }
        }
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.move_to(next, extend);
        }
    }

    pub fn move_up(&mut self, extend: bool) {
        let (line, col) = line_col(&self.text, self.cursor);
        let ranges = line_ranges(&self.text);
        if line == 0 {
            self.move_to(0, extend);
            return;
        }
        let (start, end) = ranges[line - 1];
        let target = byte_at_col(&self.text, start, end, col);
        self.move_to(target, extend);
    }

    pub fn move_down(&mut self, extend: bool) {
        let (line, col) = line_col(&self.text, self.cursor);
        let ranges = line_ranges(&self.text);
        if line + 1 >= ranges.len() {
            self.move_to(self.text.len(), extend);
            return;
        }
        let (start, end) = ranges[line + 1];
        let target = byte_at_col(&self.text, start, end, col);
        self.move_to(target, extend);
    }

    pub fn move_home(&mut self, extend: bool) {
        let (line, _) = line_col(&self.text, self.cursor);
        let (start, _) = line_ranges(&self.text)[line];
        self.move_to(start, extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        let (line, _) = line_col(&self.text, self.cursor);
        let (_, end) = line_ranges(&self.text)[line];
        self.move_to(end, extend);
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        self.move_to(self.text.len(), extend);
    }

    fn move_to(&mut self, pos: usize, extend: bool) {
        let pos = pos.min(self.text.len());
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
    }

    // ------------------------------------------------------------------
    // Undo / Redo
    // ------------------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        }
    }

    fn record(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(next);
        true
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
        self.anchor = None;
    }

    pub fn text_to_send(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(self.text.trim_end().to_string())
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }

    /// Renders the multiline soft-wrapped editor into `area`, returning the terminal cursor `(x, y)` when `focused`.
    pub fn draw_editor(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        focused: bool,
        placeholder: &str,
        surface_style: Style,
    ) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let body_width = (area.width as usize).max(1);
        self.last_inner_width = body_width.min(u16::MAX as usize) as u16;
        let selection = self.selection_range();
        let (visual, cursor_row, cursor_col) = text_layout(&self.text, body_width, self.cursor);

        let height = area.height as usize;
        let mut offset = if cursor_row >= height {
            cursor_row + 1 - height
        } else {
            0
        };
        offset = offset.min(visual.len().saturating_sub(height));

        let mut lines: Vec<Line> = Vec::with_capacity(height);
        if self.text.is_empty() {
            lines.push(Line::from(Span::styled(
                truncate_display(placeholder, body_width),
                theme.muted_style().add_modifier(Modifier::ITALIC),
            )));
        } else {
            for &(start, end) in visual.iter().skip(offset).take(height) {
                push_line_spans(
                    &mut lines,
                    &self.text,
                    start,
                    end,
                    selection,
                    theme.selection(),
                );
            }
        }

        frame.render_widget(Paragraph::new(lines).style(surface_style), area);

        if !focused {
            return None;
        }
        let cursor_y = area
            .y
            .saturating_add(cursor_row.saturating_sub(offset) as u16)
            .min(area.bottom().saturating_sub(1));
        let cursor_x = area
            .x
            .saturating_add(cursor_col as u16)
            .min(area.right().saturating_sub(1));
        Some((cursor_x, cursor_y))
    }
}

/// Encodes raw RGBA8 pixel buffer into PNG bytes.
pub fn encode_rgba_to_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("Clipboard has 0x0 image data".to_string());
    }
    let img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)
        .ok_or_else(|| "Clipboard image RGBA buffer size mismatch".to_string())?;
    let mut out = Vec::new();
    let encoder = PngEncoder::new(&mut out);
    encoder
        .write_image(img.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| format!("Failed to encode clipboard image as PNG: {err}"))?;
    Ok(out)
}

fn push_line_spans(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    start: usize,
    end: usize,
    selection: Option<(usize, usize)>,
    selection_style: Style,
) {
    let mut spans: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_selected = false;

    let slice = text.get(start..end).unwrap_or("");
    for (offset, ch) in slice.char_indices() {
        let pos = start + offset;
        let selected = selection
            .map(|(from, to)| pos >= from && pos < to)
            .unwrap_or(false);
        if !run.is_empty() && selected != run_selected {
            let style = if run_selected {
                selection_style
            } else {
                Style::default()
            };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_selected = selected;
        if ch == '\t' {
            run.push_str("    ");
        } else {
            run.push(ch);
        }
    }
    if !run.is_empty() {
        let style = if run_selected {
            selection_style
        } else {
            Style::default()
        };
        spans.push(Span::styled(run, style));
    }
    if spans.is_empty() {
        lines.push(Line::from(""));
    } else {
        lines.push(Line::from(spans));
    }
}

// ---------------------------------------------------------------------------
// Text layout helpers
// ---------------------------------------------------------------------------

pub fn text_layout(text: &str, width: usize, cursor: usize) -> (Vec<(usize, usize)>, usize, usize) {
    let width = width.max(1);
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut row = 0usize;
    let mut col = 0usize;
    let mut line_start = 0usize;
    let mut cursor_pos: Option<(usize, usize)> = None;

    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            if index == cursor {
                cursor_pos = Some((row, col));
            }
            lines.push((line_start, index));
            row += 1;
            col = 0;
            line_start = index + 1;
            continue;
        }
        let char_width = if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if col > 0 && col + char_width > width {
            lines.push((line_start, index));
            row += 1;
            col = 0;
            line_start = index;
        }
        if index == cursor {
            cursor_pos = Some((row, col));
        }
        col += char_width;
    }
    if cursor >= text.len() {
        cursor_pos = Some((row, col));
    }
    lines.push((line_start, text.len()));

    let (cursor_row, cursor_col) = cursor_pos.unwrap_or((row, col));
    (lines, cursor_row, cursor_col)
}

fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            ranges.push((start, index));
            start = index + 1;
        }
    }
    ranges.push((start, text.len()));
    ranges
}

fn line_col(text: &str, cursor: usize) -> (usize, usize) {
    let ranges = line_ranges(text);
    let cursor = cursor.min(text.len());
    let mut line = 0;
    for (index, &(start, end)) in ranges.iter().enumerate() {
        if cursor <= end && cursor >= start {
            line = index;
            break;
        }
    }
    let (start, _) = ranges[line];
    (line, text[start..cursor.min(text.len())].chars().count())
}

fn byte_at_col(text: &str, start: usize, end: usize, col: usize) -> usize {
    let mut pos = start;
    for (count, ch) in text[start..end].chars().enumerate() {
        if count >= col {
            break;
        }
        pos += ch.len_utf8();
    }
    pos
}

fn prev_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 || cursor > text.len() {
        return None;
    }
    let mut prev = cursor - 1;
    while !text.is_char_boundary(prev) {
        if prev == 0 {
            return None;
        }
        prev -= 1;
    }
    Some(prev)
}

fn next_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor >= text.len() {
        return None;
    }
    let ch = text[cursor..].chars().next()?;
    Some(cursor + ch.len_utf8())
}

pub fn normalize(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn truncate_display(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if used + w > width {
            break;
        }
        if ch == '\t' {
            out.push_str("    ");
        } else {
            out.push(ch);
        }
        used += w;
    }
    out
}

pub fn slice_display_window(text: &str, cursor: usize, max_width: usize) -> (String, usize) {
    if max_width == 0 {
        return (String::new(), 0);
    }
    let mut safe_cursor = cursor.min(text.len());
    while safe_cursor > 0 && !text.is_char_boundary(safe_cursor) {
        safe_cursor -= 1;
    }
    let cursor_total_col: usize = text[..safe_cursor]
        .chars()
        .map(|ch| {
            if ch == '\t' {
                1
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            }
        })
        .sum();
    let scroll_col = if cursor_total_col < max_width {
        0
    } else {
        cursor_total_col.saturating_sub(max_width.saturating_sub(1))
    };

    let mut out = String::new();
    let mut col = 0usize;
    let mut start_col: Option<usize> = None;
    let mut vis_w = 0usize;
    for ch in text.chars() {
        let display_ch = if ch == '\t' { ' ' } else { ch };
        let cw = UnicodeWidthChar::width(display_ch).unwrap_or(0);
        if col < scroll_col {
            col += cw;
            continue;
        }
        if start_col.is_none() {
            start_col = Some(col);
        }
        if vis_w + cw > max_width {
            break;
        }
        out.push(display_ch);
        vis_w += cw;
        col += cw;
    }
    let origin = start_col.unwrap_or(col);
    let cursor_col = cursor_total_col
        .saturating_sub(origin)
        .min(max_width.saturating_sub(1));
    (out, cursor_col)
}

// ---------------------------------------------------------------------------
// External Editor
// ---------------------------------------------------------------------------

pub fn external_edit(current: &str) -> std::io::Result<String> {
    let path = std::env::temp_dir().join(format!(
        "outlook-compose-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ));

    std::fs::write(&path, current)?;

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(default_editor);

    let parts = parse_editor_command(&editor);
    let program = parts.first().map(String::as_str).unwrap_or("vi");
    let args = parts.get(1..).unwrap_or(&[]);

    let status = Command::new(program).args(args).arg(&path).status();

    let contents = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);

    let _ = status?;
    Ok(normalize(&contents?).trim_end().to_string())
}

fn parse_editor_command(raw: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_token = false;

    for ch in raw.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    in_token = true;
                } else if ch.is_whitespace() {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else {
                    current.push(ch);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        args.push(current);
    }
    args
}

fn default_editor() -> String {
    if cfg!(windows) {
        "notepad".to_string()
    } else if cfg!(target_os = "macos") {
        "open -W -t".to_string()
    } else {
        "vi".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer_with(text: &str) -> ComposerState {
        let mut composer = ComposerState::new();
        composer.insert_str(text);
        composer
    }

    #[test]
    fn insert_and_backspace_utf8() {
        let mut composer = ComposerState::new();
        composer.insert_char('é');
        composer.insert_char('a');
        assert_eq!(composer.text(), "éa");
        composer.backspace();
        assert_eq!(composer.text(), "é");
        composer.backspace();
        assert_eq!(composer.text(), "");
        composer.backspace();
    }

    #[test]
    fn cursor_moves_across_utf8_lines() {
        let mut composer = composer_with("ab\néα");
        assert_eq!(composer.cursor(), 7);
        composer.move_left(false);
        assert_eq!(composer.cursor(), 5);
        composer.move_home(false);
        assert_eq!(composer.cursor(), 3);
        composer.move_up(false);
        assert_eq!(composer.cursor(), 0);
        composer.move_end(false);
        assert_eq!(composer.cursor(), 2);
        composer.move_down(false);
        assert_eq!(composer.cursor(), 7);
    }

    #[test]
    fn undo_redo_and_selection_work() {
        let mut composer = composer_with("hello");
        composer.select_all();
        assert_eq!(composer.selected_text(), Some("hello"));
        composer.insert_char('X');
        assert_eq!(composer.text(), "X");
        assert!(composer.undo());
        assert_eq!(composer.text(), "hello");
        assert!(composer.redo());
        assert_eq!(composer.text(), "X");
    }

    #[test]
    fn stage_inline_image_makes_composer_non_empty() {
        let mut composer = ComposerState::new();
        assert!(composer.is_empty());
        let png = encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        let cid = composer.stage_inline_image(png, "image/png", Some("clip.png".into()));
        assert!(!composer.is_empty());
        assert_eq!(composer.pending_images.len(), 1);
        assert!(cid.contains("outlook-cli"));
        assert_eq!(
            encode_rgba_to_png(0, 2, vec![]),
            Err("Clipboard has 0x0 image data".to_string())
        );
    }

    #[test]
    fn text_layout_wrap_boundary_places_cursor_on_next_row() {
        let (lines, row, col) = text_layout("abcdeX", 5, 5);
        assert_eq!(lines, vec![(0, 5), (5, 6)]);
        assert_eq!((row, col), (1, 0));

        let (_, tab_row, tab_col) = text_layout("\ta", 10, 1);
        assert_eq!((tab_row, tab_col), (0, 4));

        let (win, ccol) = slice_display_window("0123456789ABCDEF", 16, 8);
        assert_eq!(win.len(), 7);
        assert!(ccol < 8);

        assert_eq!(
            parse_editor_command(r#""C:\Program Files\Sublime Text\subl.exe" -w"#),
            vec![r"C:\Program Files\Sublime Text\subl.exe", "-w"]
        );
    }
}
