//! Multiline Markdown composer: UTF-8 char-boundary cursor/selection/undo/redo,
//! optional Channel Subject field (`Ctrl+S`), `@mention` autocomplete, quoted reply,
//! clipboard image paste (RGBA -> PNG `InlineImageUpload`), and external `$EDITOR`.

use std::{ops::Range, process::Command};

use image::{codecs::png::PngEncoder, ExtendedColorType, ImageBuffer, ImageEncoder, Rgba};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    img_preview::{self, RenderedTerminalImage},
    models::{ConversationKind, InlineImageUpload, MemberInfo, MentionInfo},
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::mentions::MentionPopup,
    },
};

const MAX_UNDO: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyPreview {
    pub msg_id: String,
    pub author: String,
    pub text: String,
}

impl ReplyPreview {
    pub fn summary(&self, width: usize) -> String {
        truncate_display(&self.text.replace('\n', " "), width)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionRef {
    pub user_id: String,
    pub display_name: String,
    pub start: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    text: String,
    cursor: usize,
    mentions: Vec<MentionRef>,
}

#[derive(Debug, Clone, Default)]
pub struct ComposerState {
    text: String,
    cursor: usize,
    pub subject: String,
    pub subject_cursor: usize,
    pub subject_focused: bool,
    pub reply: Option<ReplyPreview>,
    mentions: Vec<MentionRef>,
    pub pending_images: Vec<InlineImageUpload>,
    pub pending_preview: Option<RenderedTerminalImage>,
    anchor: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    pub popup: MentionPopup,
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

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.pending_images.is_empty()
    }

    pub fn reply(&self) -> Option<&ReplyPreview> {
        self.reply.as_ref()
    }

    pub fn mentions(&self) -> &[MentionRef] {
        &self.mentions
    }

    pub fn compiled_mentions(&self) -> Vec<MentionInfo> {
        self.mentions
            .iter()
            .enumerate()
            .map(|(idx, m)| MentionInfo {
                id: idx as i32,
                user_id: m.user_id.clone(),
                display_name: m.display_name.clone(),
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Editing
    // ------------------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\r' {
            return;
        }
        if self.subject_focused {
            if ch != '\n' {
                let pos = self.subject_cursor.min(self.subject.len());
                self.subject.insert(pos, ch);
                self.subject_cursor = pos + ch.len_utf8();
            }
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
        if self.subject_focused {
            let single = value.replace('\n', " ");
            let pos = self.subject_cursor.min(self.subject.len());
            self.subject.insert_str(pos, &single);
            self.subject_cursor = pos + single.len();
            return;
        }
        self.insert_replacing_selection(&value);
    }

    fn insert_replacing_selection(&mut self, value: &str) {
        if let Some((start, end)) = self.selection_range() {
            self.record();
            let start_char = self.text[..start].chars().count();
            let removed_chars = self.text[start..end].chars().count();
            let inserted_chars = value.chars().count();
            self.text.replace_range(start..end, value);
            self.cursor = start + value.len();
            self.anchor = None;
            self.adjust_mentions(start_char, removed_chars, inserted_chars);
        } else {
            self.insert_at(self.cursor, value);
        }
    }

    pub fn newline(&mut self) {
        if self.subject_focused {
            self.subject_focused = false;
            return;
        }
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.subject_focused {
            if self.subject_cursor == 0 {
                return;
            }
            if let Some(prev) = prev_boundary(&self.subject, self.subject_cursor) {
                self.subject.replace_range(prev..self.subject_cursor, "");
                self.subject_cursor = prev;
            }
            return;
        }
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
        if self.subject_focused {
            if let Some(next) = next_boundary(&self.subject, self.subject_cursor) {
                self.subject.replace_range(self.subject_cursor..next, "");
            }
            return;
        }
        if self.delete_selection() {
            return;
        }
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.delete_range(self.cursor..next);
        }
    }

    pub fn clear(&mut self) {
        if self.text.is_empty()
            && self.subject.is_empty()
            && self.mentions.is_empty()
            && self.pending_images.is_empty()
        {
            return;
        }
        self.record();
        self.text.clear();
        self.cursor = 0;
        self.subject.clear();
        self.subject_cursor = 0;
        self.subject_focused = false;
        self.anchor = None;
        self.mentions.clear();
        self.pending_images.clear();
        self.pending_preview = None;
        self.popup.close();
    }

    pub fn stage_inline_image(&mut self, bytes: Vec<u8>, mime_type: &str, file_name: Option<String>) {
        self.pending_preview = img_preview::decode_and_render_halfblocks(&bytes, 24, 5, (20, 22, 26)).ok();
        let temp_id = (self.pending_images.len() + 1).to_string();
        self.pending_images.push(InlineImageUpload {
            temp_id,
            mime_type: mime_type.to_string(),
            bytes,
            file_name,
        });
    }

    pub fn clear_pending_images(&mut self) {
        self.pending_images.clear();
        self.pending_preview = None;
    }

    pub fn reset_after_send(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.subject.clear();
        self.subject_cursor = 0;
        self.subject_focused = false;
        self.anchor = None;
        self.mentions.clear();
        self.pending_images.clear();
        self.pending_preview = None;
        self.reply = None;
        self.popup.close();
        self.undo.clear();
        self.redo.clear();
    }

    pub fn load_text(&mut self, value: &str) {
        self.text = normalize(value);
        self.cursor = self.text.len();
        self.subject.clear();
        self.subject_cursor = 0;
        self.subject_focused = false;
        self.anchor = None;
        self.mentions.clear();
        self.pending_images.clear();
        self.pending_preview = None;
        self.popup.close();
        self.undo.clear();
        self.redo.clear();
    }

    fn insert_at(&mut self, pos: usize, value: &str) {
        if value.is_empty() || pos > self.text.len() || !self.text.is_char_boundary(pos) {
            return;
        }
        self.record();
        let start_char = self.text[..pos].chars().count();
        let inserted_chars = value.chars().count();
        self.text.insert_str(pos, value);
        self.cursor = pos + value.len();
        self.anchor = None;
        self.adjust_mentions(start_char, 0, inserted_chars);
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        self.record();
        let start_char = self.text[..range.start].chars().count();
        let removed_chars = self.text[range.clone()].chars().count();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = None;
        self.adjust_mentions(start_char, removed_chars, 0);
    }

    fn adjust_mentions(&mut self, start_char: usize, removed: usize, inserted: usize) {
        if removed == 0 && inserted == 0 {
            return;
        }
        let end_char = start_char + removed;
        let mut kept = Vec::with_capacity(self.mentions.len());

        for mention in &self.mentions {
            let m_start = mention.start.saturating_sub(1);
            let m_end = m_start + mention.len;

            if removed == 0 {
                let mut next = mention.clone();
                if start_char <= m_start {
                    next.start += inserted;
                    kept.push(next);
                } else if start_char < m_end {
                    continue;
                } else {
                    kept.push(next);
                }
                continue;
            }

            if m_start < end_char && m_end > start_char {
                continue;
            }
            let mut next = mention.clone();
            if m_start >= end_char {
                let shifted = next.start as isize - removed as isize + inserted as isize;
                next.start = shifted.max(1) as usize;
            } else if start_char < m_start {
                let shifted = next.start as isize + inserted as isize;
                next.start = shifted.max(1) as usize;
            }
            kept.push(next);
        }

        self.mentions = kept;
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
        if self.subject_focused {
            if let Some(prev) = prev_boundary(&self.subject, self.subject_cursor) {
                self.subject_cursor = prev;
            }
            return;
        }
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
        if self.subject_focused {
            if let Some(next) = next_boundary(&self.subject, self.subject_cursor) {
                self.subject_cursor = next;
            }
            return;
        }
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
        if self.subject_focused {
            return;
        }
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
        if self.subject_focused {
            self.subject_focused = false;
            return;
        }
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
        if self.subject_focused {
            self.subject_cursor = 0;
            return;
        }
        let (line, _) = line_col(&self.text, self.cursor);
        let (start, _) = line_ranges(&self.text)[line];
        self.move_to(start, extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        if self.subject_focused {
            self.subject_cursor = self.subject.len();
            return;
        }
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
            mentions: self.mentions.clone(),
        }
    }

    fn record(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.popup.close();
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
        self.mentions = snapshot.mentions;
        self.anchor = None;
        self.popup.close();
    }

    // ------------------------------------------------------------------
    // Reply & Mentions
    // ------------------------------------------------------------------

    pub fn clear_reply(&mut self) {
        self.reply = None;
    }

    pub fn set_reply(&mut self, reply: ReplyPreview) {
        self.reply = Some(reply);
    }

    pub fn reference_mentions(&mut self, members: &[MemberInfo]) {
        if self.subject_focused {
            self.popup.close();
            return;
        }
        self.popup.refresh(&self.text, self.cursor, members);
    }

    pub fn mention_active(&self) -> bool {
        self.popup.active
    }

    pub fn mention_next(&mut self) {
        self.popup.next();
    }

    pub fn mention_prev(&mut self) {
        self.popup.prev();
    }

    pub fn close_mentions(&mut self) {
        self.popup.close();
    }

    pub fn accept_mention(&mut self) -> bool {
        if !self.popup.active {
            return false;
        }
        let Some(candidate) = self.popup.current().cloned() else {
            self.popup.close();
            return false;
        };
        let start = self.popup.start;
        let end = self.cursor;
        if start > end
            || end > self.text.len()
            || !self.text.is_char_boundary(start)
            || !self.text.is_char_boundary(end)
        {
            self.popup.close();
            return false;
        }

        let insert = format!("@{} ", candidate.display_name);
        let start_char = self.text[..start].chars().count();
        let removed_chars = self.text[start..end].chars().count();

        self.record();
        self.text.replace_range(start..end, &insert);
        self.cursor = start + insert.len();
        self.anchor = None;
        self.adjust_mentions(start_char, removed_chars, insert.chars().count());
        self.mentions.push(MentionRef {
            user_id: candidate.user_id,
            display_name: candidate.display_name.clone(),
            start: start_char + 1,
            len: candidate.display_name.chars().count(),
        });
        self.popup.close();
        true
    }

    // ------------------------------------------------------------------
    // Send / Display
    // ------------------------------------------------------------------

    pub fn text_to_send(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(self.text.trim_end().to_string())
    }

    pub fn subject_to_send(&self) -> Option<String> {
        let s = self.subject.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    pub fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }

    pub fn desired_height(&self, show_subject: bool) -> u16 {
        let mut extra = 0u16;
        if show_subject {
            extra += 1;
        }
        if self.reply.is_some() {
            extra += 1;
        }
        if !self.pending_images.is_empty() {
            extra += 1;
            if let Some(ref preview) = self.pending_preview {
                extra = extra.saturating_add(preview.rows.min(4));
            }
        }
        ((self.line_count() as u16) + 2 + extra).clamp(3, 14)
    }
}

/// Encodes raw RGBA8 pixel buffer into PNG bytes.
pub fn encode_rgba_to_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("이미지 크기가 0입니다".to_string());
    }
    let img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)
        .ok_or_else(|| "클립보드 이미지 버퍼 크기가 일치하지 않습니다".to_string())?;
    let mut out = Vec::new();
    let encoder = PngEncoder::new(&mut out);
    encoder
        .write_image(img.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| format!("PNG 인코딩 실패: {err}"))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) -> Option<(u16, u16)> {
    let focused = ui.focus == Focus::Composer && ui.overlays.is_empty();
    let theme = ui.theme.clone();

    let title = composer_title(ui, app);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(title, theme.title(focused))))
        .border_style(theme.border_style(focused));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    hitmap.add(area, HitTarget::Composer);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let mut y = inner.y;
    let is_channel_root = is_composing_channel_root(app, ui);
    let show_subject = is_channel_root && ui.show_subject;

    if show_subject && y < inner.y + inner.height {
        let prefix = "제목: ";
        let prefix_w = UnicodeWidthStr::width(prefix);
        let subj_style = if focused && ui.composer.subject_focused {
            theme.subject_style()
        } else {
            theme.accent_style()
        };
        let body_text = if ui.composer.subject.is_empty() {
            "(채널 게시물 제목 입력 · Ctrl+S 숨김)".to_string()
        } else {
            ui.composer.subject.clone()
        };
        let body_style = if ui.composer.subject.is_empty() {
            theme.muted_style().add_modifier(Modifier::ITALIC)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let max_body = (inner.width as usize).saturating_sub(prefix_w);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, subj_style),
                Span::styled(truncate_display(&body_text, max_body), body_style),
            ])),
            Rect::new(inner.x, y, inner.width, 1),
        );
        hitmap.add_line(inner.x, y, inner.width, HitTarget::SubjectInput);
        y = y.saturating_add(1);
    }

    if let Some(reply) = ui.composer.reply() {
        if y < inner.y + inner.height {
            let text = format!(
                "↩ [답장: {}] {}",
                reply.author,
                reply.summary(inner.width.saturating_sub(14) as usize)
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_display(&text, inner.width as usize),
                    theme.quote_style(),
                ))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }
    }

    if !ui.composer.pending_images.is_empty() && y < inner.y + inner.height {
        let count = ui.composer.pending_images.len();
        let chip = if let Some(ref preview) = ui.composer.pending_preview {
            format!(
                "[📋 클립보드 이미지 {}개 ({}x{}) — 전송 시 인라인 업로드]",
                count, preview.width_px, preview.height_px
            )
        } else if count == 1 {
            "[📋 클립보드 이미지 첨부됨 — 전송 시 인라인 업로드]".to_string()
        } else {
            format!("[📋 클립보드 이미지 {count}개 첨부됨 — 전송 시 인라인 업로드]")
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_display(&chip, inner.width as usize),
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y = y.saturating_add(1);

        if ui.inline_images {
            if let Some(ref preview) = ui.composer.pending_preview {
                for thumb_line in preview.lines.iter().take(4) {
                    if y >= inner.y + inner.height.saturating_sub(1) {
                        break;
                    }
                    frame.render_widget(
                        Paragraph::new(thumb_line.clone()),
                        Rect::new(inner.x, y, inner.width, 1),
                    );
                    y = y.saturating_add(1);
                }
            }
        }
    }

    let text_height = (inner.y + inner.height).saturating_sub(y);
    if text_height == 0 {
        return None;
    }
    let text_area = Rect::new(inner.x, y, inner.width, text_height);

    let width = inner.width as usize;
    let text = ui.composer.text();
    let selection = ui.composer.selection_range();
    let (visual, cursor_row, cursor_col) = text_layout(text, width, ui.composer.cursor());

    let height = text_height as usize;
    let mut offset = if cursor_row >= height {
        cursor_row + 1 - height
    } else {
        0
    };
    offset = offset.min(visual.len().saturating_sub(height));

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    if text.is_empty() {
        let hint = if focused {
            if is_channel_root {
                "Markdown 입력 (Enter 전송 · Shift+Enter 줄바꿈 · Ctrl+S 제목 · Ctrl+V 이미지 · @ 멘션)"
            } else {
                "Markdown 입력 (Enter 전송 · Shift+Enter 줄바꿈 · Ctrl+V 이미지 · @ 멘션 · Ctrl+E 편집)"
            }
        } else {
            "i/Enter: 입력 · ↑↓: 탐색 · t: 스레드 · +: 반응 · Ctrl+K: 명령팔레트"
        };
        lines.push(Line::from(Span::styled(
            truncate_display(hint, width),
            theme.muted_style().add_modifier(Modifier::ITALIC),
        )));
    } else {
        for &(start, end) in visual.iter().skip(offset).take(height) {
            push_line_spans(&mut lines, text, start, end, selection, theme.selection());
        }
    }

    frame.render_widget(Paragraph::new(lines), text_area);

    if focused && !ui.composer.subject_focused {
        ui.composer.popup.draw(frame, area, &theme);
    }
    if !focused {
        return None;
    }

    if show_subject && ui.composer.subject_focused {
        let prefix_w = UnicodeWidthStr::width("제목: ") as u16;
        let subj_col = UnicodeWidthStr::width(
            &ui.composer.subject[..ui.composer.subject_cursor.min(ui.composer.subject.len())],
        ) as u16;
        let cx = inner
            .x
            .saturating_add(prefix_w)
            .saturating_add(subj_col)
            .min(inner.x + inner.width.saturating_sub(1));
        return Some((cx, inner.y));
    }

    let cursor_y = text_area
        .y
        .saturating_add(cursor_row.saturating_sub(offset) as u16)
        .min(text_area.y + text_area.height.saturating_sub(1));
    let cursor_x = text_area
        .x
        .saturating_add(cursor_col as u16)
        .min(text_area.x + text_area.width.saturating_sub(1));

    Some((cursor_x, cursor_y))
}

fn is_composing_channel_root(app: &AppState, ui: &UiState) -> bool {
    let is_channel = app
        .active_conversation()
        .is_some_and(|c| c.kind == ConversationKind::Channel);
    if !is_channel {
        return false;
    }
    !(ui.thread_drawer_open && ui.composer_target_thread && app.active_thread.is_some())
}

fn composer_title(ui: &UiState, app: &AppState) -> String {
    let conv_name = app
        .active_conversation()
        .map(|c| c.title.as_str())
        .unwrap_or("대화 없음");

    if ui.thread_drawer_open && ui.composer_target_thread {
        if let Some(thread) = &app.active_thread {
            return format!(
                " 스레드 답글 · ↩ {} ({}) · Enter 전송 · Shift+Enter 줄바꿈 ",
                thread.root.author_name, conv_name
            );
        }
    }
    if ui.composer.reply().is_some() {
        return format!(" 답장 · {conv_name} · Enter 전송 · Shift+Enter 줄바꿈 ");
    }
    if is_composing_channel_root(app, ui) {
        return format!(" 채널 새 글 · {conv_name} · Ctrl+S 제목 · Enter 전송 ");
    }
    format!(" 입력 · {conv_name} · Enter 전송 · Shift+Enter 줄바꿈 ")
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
        run.push(ch);
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
        if index == cursor {
            cursor_pos = Some((row, col));
        }
        if ch == '\n' {
            lines.push((line_start, index));
            row += 1;
            col = 0;
            line_start = index + 1;
            continue;
        }
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col > 0 && col + char_width > width {
            lines.push((line_start, index));
            row += 1;
            col = 0;
            line_start = index;
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

fn truncate_display(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

// ---------------------------------------------------------------------------
// External Editor
// ---------------------------------------------------------------------------

pub fn external_edit(current: &str) -> std::io::Result<String> {
    let path = std::env::temp_dir().join(format!(
        "teams-compose-{}-{}.md",
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

    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");

    let status = Command::new(program).args(parts).arg(&path).status();

    let contents = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);

    let _ = status?;
    Ok(normalize(&contents?).trim_end().to_string())
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

    fn sample_members() -> Vec<MemberInfo> {
        vec![MemberInfo {
            user_id: "user-7".to_string(),
            member_id: None,
            display_name: "kim".to_string(),
            email: Some("kim@contoso.com".to_string()),
            roles: vec![],
            presence: None,
        }]
    }

    #[test]
    fn insert_and_backspace_utf8() {
        let mut composer = ComposerState::new();
        composer.insert_char('가');
        composer.insert_char('a');
        assert_eq!(composer.text(), "가a");
        composer.backspace();
        assert_eq!(composer.text(), "가");
        composer.backspace();
        assert_eq!(composer.text(), "");
        composer.backspace();
    }

    #[test]
    fn cursor_moves_across_utf8_lines() {
        let mut composer = composer_with("ab\n가나");
        assert_eq!(composer.cursor(), 9);
        composer.move_left(false);
        assert_eq!(composer.cursor(), 6);
        composer.move_home(false);
        assert_eq!(composer.cursor(), 3);
        composer.move_up(false);
        assert_eq!(composer.cursor(), 0);
        composer.move_end(false);
        assert_eq!(composer.cursor(), 2);
        composer.move_down(false);
        assert_eq!(composer.cursor(), 9);
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
    fn mention_autocomplete_and_shift() {
        let mut composer = ComposerState::new();
        let members = sample_members();
        composer.insert_str("hi @ki");
        composer.reference_mentions(&members);
        assert!(composer.mention_active());
        assert!(composer.accept_mention());
        assert_eq!(composer.text(), "hi @kim ");
        assert_eq!(composer.mentions().len(), 1);
        assert_eq!(composer.mentions()[0].user_id, "user-7");
        assert_eq!(composer.mentions()[0].start, 4);

        composer.move_doc_start(false);
        composer.insert_str("yo ");
        assert_eq!(composer.mentions()[0].start, 7);
    }

    #[test]
    fn stage_inline_image_makes_composer_non_empty() {
        let mut composer = ComposerState::new();
        assert!(composer.is_empty());
        let png = encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        composer.stage_inline_image(png, "image/png", Some("clip.png".into()));
        assert!(!composer.is_empty());
        assert_eq!(composer.pending_images.len(), 1);
    }
}
