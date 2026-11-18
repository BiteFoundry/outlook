//! Center main conversation view & shared rich message renderer.
//!
//! Pure line builders (`ChatRender`, `build_lines_with_hyperlinks`, `render_message_item`)
//! are decoupled from `GraphClient` / `Storage` so they can be unit tested directly.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local};
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
    models::{
        AttachmentInfo, ConversationKind, ConversationSummary, MessageItem, PresenceStatus,
        RichBlock, RichSpan,
    },
    parser,
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget, MessageScope},
        theme::Theme,
    },
};

/// One rendered visual line plus optional full-line and inline click targets.
#[derive(Debug, Clone)]
pub struct RenderedLine {
    pub line: Line<'static>,
    pub target: Option<HitTarget>,
    /// `(col_offset, width, target)` registered after `target` so inline chips win hit-tests.
    pub inline_targets: Vec<(u16, u16, HitTarget)>,
}

impl RenderedLine {
    pub fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            target: None,
            inline_targets: Vec::new(),
        }
    }

    pub fn clickable(line: Line<'static>, target: HitTarget) -> Self {
        Self {
            line,
            target: Some(target),
            inline_targets: Vec::new(),
        }
    }

    pub fn with_inline(
        line: Line<'static>,
        target: Option<HitTarget>,
        inline_targets: Vec<(u16, u16, HitTarget)>,
    ) -> Self {
        Self {
            line,
            target,
            inline_targets,
        }
    }
}

/// Pure view struct for testing and rendering without requiring `AppState`.
pub struct ChatRender<'a> {
    pub messages: &'a [MessageItem],
    pub is_channel: bool,
    pub selected_index: Option<usize>,
}

impl<'a> ChatRender<'a> {
    pub fn from_app(app: &'a AppState, ui: &'a UiState) -> Self {
        let is_channel = app
            .active_conversation()
            .is_some_and(|c| c.kind == ConversationKind::Channel);
        Self {
            messages: &app.messages,
            is_channel,
            selected_index: ui.chat_scroll.selected_idx,
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Chat;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            pane_title(ui.chat_scroll.follow_tail),
            theme.title(focused),
        )))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    draw_header(frame, inner, app, ui, hitmap);

    let message_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    ui.chat_scroll.area = message_area;

    if message_area.height == 0 {
        return;
    }

    if app.active_conv_key.is_none() {
        frame.render_widget(
            Paragraph::new("왼쪽 목록에서 채팅 또는 팀 채널을 선택하세요")
                .style(theme.muted_style()),
            message_area,
        );
        return;
    }

    if app.loading_messages && app.messages.is_empty() {
        frame.render_widget(
            Paragraph::new("메시지를 불러오는 중…").style(theme.muted_style()),
            message_area,
        );
        return;
    }
    if app.messages.is_empty() {
        let mut empty_lines = Vec::new();
        if let Some(banner) = build_history_banner_line(
            app.loading_older,
            app.oldest_reached,
            app.history_error.as_deref(),
            false,
            theme,
            message_area.width as usize,
        ) {
            empty_lines.push(banner);
        }
        empty_lines.push(RenderedLine::plain(Line::from(Span::styled(
            "메시지가 없습니다",
            theme.muted_style(),
        ))));
        let height = message_area.height as usize;
        ui.chat_scroll.total_lines = empty_lines.len();
        ui.chat_scroll.offset = 0;
        ui.pending_prepend_anchor = None;
        render_scrolled_lines(frame, message_area, &empty_lines, 0, height, hitmap);
        return;
    }

    let lines = build_lines_with_images_cached(app, ui, message_area.width as usize);
    if lines.is_empty() {
        return;
    }

    let height = message_area.height as usize;
    apply_prepend_anchor(ui, lines.len(), height);
    ui.chat_scroll.total_lines = lines.len();
    let offset = ui.chat_scroll.offset(height);
    ui.chat_scroll.offset = offset;

    render_scrolled_lines(frame, message_area, &lines, offset, height, hitmap);
}

pub(crate) fn apply_prepend_anchor(ui: &mut UiState, new_total_lines: usize, height: usize) {
    if let Some((old_total_lines, old_offset)) = ui.pending_prepend_anchor.take() {
        if !ui.chat_scroll.follow_tail && old_total_lines > 0 && new_total_lines > old_total_lines
        {
            let delta = new_total_lines - old_total_lines;
            let new_max_off = new_total_lines.saturating_sub(height);
            let anchored_offset = old_offset.saturating_add(delta).min(new_max_off);
            ui.chat_scroll.scroll_up = new_max_off
                .saturating_sub(anchored_offset)
                .min(u16::MAX as usize) as u16;
        }
    }
}

pub(crate) fn build_history_banner_line(
    loading_older: bool,
    oldest_reached: bool,
    history_error: Option<&str>,
    has_messages: bool,
    theme: &Theme,
    width: usize,
) -> Option<RenderedLine> {
    let width = width.max(12);
    if loading_older {
        return Some(RenderedLine::plain(Line::from(Span::styled(
            truncate("  ⟳ 이전 메시지를 서버에서 불러오는 중...", width),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ))));
    }
    if let Some(err) = history_error {
        let text = format!("  ⚠ {err}  [o / 클릭하여 재시도]");
        return Some(RenderedLine::clickable(
            Line::from(Span::styled(
                truncate(&text, width),
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )),
            HitTarget::LoadOlderMessages,
        ));
    }
    if oldest_reached {
        if has_messages {
            let label = "── 대화의 첫 메시지입니다 ──";
            let label_w = UnicodeWidthStr::width(label);
            let left_pad = width.saturating_sub(label_w) / 2;
            let centered = format!("{}{}", " ".repeat(left_pad), label);
            return Some(RenderedLine::plain(Line::from(Span::styled(
                truncate(&centered, width),
                theme.muted_style(),
            ))));
        }
        return None;
    }
    let pill = "  [ ↑ 위로 스크롤하거나 o · H 키로 이전 메시지 더 불러오기 ]";
    Some(RenderedLine::clickable(
        Line::from(Span::styled(truncate(pill, width), theme.muted_style())),
        HitTarget::LoadOlderMessages,
    ))
}

pub fn render_scrolled_lines(
    frame: &mut Frame,
    area: Rect,
    lines: &[RenderedLine],
    offset: usize,
    height: usize,
    hitmap: &mut HitMap,
) {
    let end = offset.saturating_add(height).min(lines.len());
    let slice = lines.get(offset..end).unwrap_or(&[]);
    let visible: Vec<Line> = slice.iter().map(|r| r.line.clone()).collect();
    frame.render_widget(Paragraph::new(visible), area);

    for (row, rendered) in slice.iter().enumerate() {
        let y = area.y + row as u16;
        if let Some(target) = rendered.target {
            hitmap.add_line(area.x, y, area.width, target);
        }
        for &(col_off, width, target) in &rendered.inline_targets {
            let x = area.x.saturating_add(col_off);
            if x < area.x + area.width {
                let clamped = width.min((area.x + area.width).saturating_sub(x));
                hitmap.add_line(x, y, clamped, target);
            }
        }
    }
}

pub fn pane_title(follow_tail: bool) -> &'static str {
    if follow_tail {
        " 대화 "
    } else {
        " 대화 · 새 메시지 ↓ "
    }
}

fn draw_header(frame: &mut Frame, inner: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let (title_text, presence_opt) = header_label(app.active_conversation(), app);

    let buttons: [(&str, ButtonId); 6] = [
        ("[검색]", ButtonId::Search),
        ("[파일전송]", ButtonId::SendFile),
        ("[첨부목록]", ButtonId::Attachments),
        ("[멤버/정보]", ButtonId::RoomInfo),
        ("[새로고침]", ButtonId::Refresh),
        ("[설정]", ButtonId::Settings),
    ];

    let mut visible_buttons = Vec::new();
    let mut used_btn_w = 0u16;
    let min_title_w = 14u16;
    for &(label, id) in &buttons {
        let w = UnicodeWidthStr::width(label) as u16 + 1;
        if inner.width >= min_title_w + used_btn_w + w {
            visible_buttons.push((label, id, w));
            used_btn_w += w;
        }
    }

    let mut prefix_w = 0u16;
    let mut spans = Vec::new();
    if let Some((dot, color, avail)) = presence_opt {
        let badge = format!("{dot} ");
        prefix_w += UnicodeWidthStr::width(badge.as_str()) as u16;
        spans.push(Span::styled(badge, Style::default().fg(color)));
        let title_budget = inner
            .width
            .saturating_sub(used_btn_w + prefix_w + 12)
            .max(6) as usize;
        let truncated = truncate(&title_text, title_budget);
        prefix_w += UnicodeWidthStr::width(truncated.as_str()) as u16;
        spans.push(Span::styled(
            truncated,
            theme
                .title(ui.focus == Focus::Chat)
                .add_modifier(Modifier::BOLD),
        ));
        let status_label = format!(" ({avail})");
        let rem = inner
            .width
            .saturating_sub(used_btn_w + prefix_w) as usize;
        if rem >= UnicodeWidthStr::width(status_label.as_str()) {
            prefix_w += UnicodeWidthStr::width(status_label.as_str()) as u16;
            spans.push(Span::styled(status_label, theme.muted_style()));
        }
    } else {
        let title_budget = inner.width.saturating_sub(used_btn_w + 1).max(4) as usize;
        let truncated = truncate(&title_text, title_budget);
        prefix_w += UnicodeWidthStr::width(truncated.as_str()) as u16;
        spans.push(Span::styled(
            truncated,
            theme
                .title(ui.focus == Focus::Chat)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if !visible_buttons.is_empty() {
        let pad = inner
            .width
            .saturating_sub(prefix_w)
            .saturating_sub(used_btn_w);
        spans.push(Span::raw(" ".repeat(pad as usize)));
        let mut x = inner.x + prefix_w + pad;
        for (label, id, w) in visible_buttons {
            spans.push(Span::styled(label.to_string(), theme.accent_style()));
            spans.push(Span::raw(" "));
            hitmap.add_line(x, inner.y, w.saturating_sub(1), HitTarget::Button(id));
            x += w;
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
}

fn header_label<'a>(
    conv: Option<&'a ConversationSummary>,
    app: &'a AppState,
) -> (String, Option<(&'static str, ratatui::style::Color, &'a str)>) {
    let Some(conv) = conv else {
        return ("대화를 선택하세요".to_string(), None);
    };
    match conv.kind {
        ConversationKind::Channel => {
            let team = conv.team_name.as_deref().unwrap_or("팀");
            (format!("{team} › # {}", conv.title), None)
        }
        ConversationKind::Group => (format!("[그룹] {}", conv.title), None),
        ConversationKind::Meeting => (format!("[회의] {}", conv.title), None),
        ConversationKind::OneOnOne => {
            let presence: Option<&PresenceStatus> = conv
                .peer_user_id
                .as_deref()
                .and_then(|id| app.presences.get(id));
            let avail = presence
                .map(|p| p.availability.as_str())
                .unwrap_or("Offline");
            let (dot, color) = app_theme_presence(avail);
            (conv.title.clone(), Some((dot, color, avail)))
        }
    }
}

fn app_theme_presence(avail: &str) -> (&'static str, ratatui::style::Color) {
    Theme::default().presence_indicator(Some(avail))
}

pub(crate) struct DualHasher {
    h1: DefaultHasher,
    h2: DefaultHasher,
}

impl DualHasher {
    pub(crate) fn new(domain: u64) -> Self {
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        domain.hash(&mut h1);
        (domain ^ 0x9e37_79b9_7f4a_7c15).hash(&mut h2);
        Self { h1, h2 }
    }

    pub(crate) fn write_hash<T: Hash>(&mut self, val: &T) {
        val.hash(&mut self.h1);
        val.hash(&mut self.h2);
    }

    pub(crate) fn write_str(&mut self, s: &str) {
        s.len().hash(&mut self.h1);
        s.as_bytes().hash(&mut self.h1);
        s.len().hash(&mut self.h2);
        s.as_bytes().hash(&mut self.h2);
    }

    pub(crate) fn finish(self) -> (u64, u64) {
        (self.h1.finish(), self.h2.finish())
    }
}

pub(crate) struct LineSliceCache {
    entries: Vec<((u64, u64), Arc<[RenderedLine]>)>,
    max_entries: usize,
}

impl LineSliceCache {
    pub(crate) fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries: max_entries.max(1),
        }
    }

    pub(crate) fn get(&mut self, key: (u64, u64)) -> Option<Arc<[RenderedLine]>> {
        let pos = self.entries.iter().position(|(k, _)| *k == key)?;
        let (_, val) = self.entries.remove(pos);
        self.entries.push((key, Arc::clone(&val)));
        Some(val)
    }

    pub(crate) fn insert(&mut self, key: (u64, u64), val: Arc<[RenderedLine]>) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push((key, val));
    }
}

pub(crate) fn hash_theme_sig(h: &mut DualHasher, theme: &Theme) {
    h.write_hash(&theme.fg);
    h.write_hash(&theme.accent);
    h.write_hash(&theme.own);
    h.write_hash(&theme.date_separator);
    h.write_hash(&theme.unread);
    h.write_hash(&theme.warning);
    h.write_hash(&theme.error);
}

pub(crate) fn hash_message_sig(h: &mut DualHasher, msg: &MessageItem) {
    h.write_str(&msg.msg_id);
    match &msg.last_modified_at {
        Some(ts) => {
            h.write_hash(&1u8);
            h.write_str(ts);
        }
        None => h.write_hash(&0u8),
    }
    h.write_hash(&msg.deleted);
    h.write_hash(&msg.reply_count);
    h.write_hash(&msg.reactions.len());
    for r in &msg.reactions {
        h.write_str(&r.reaction_type);
        h.write_hash(&r.count);
        h.write_hash(&r.me_reacted);
    }
    h.write_hash(&msg.attachments.len());
    h.write_hash(&msg.body_html.len());
    h.write_hash(&msg.body_plain.len());
    h.write_hash(&msg.subject.as_deref().map(str::len));
    h.write_str(&msg.importance);
}

#[allow(clippy::too_many_arguments)]
fn compute_chat_lines_sig(
    conv_key: Option<&str>,
    view: &ChatRender,
    width: usize,
    hyperlinks: bool,
    inline_images: bool,
    image_cache_len: usize,
    loading_older: bool,
    oldest_reached: bool,
    history_error: Option<&str>,
    theme: &Theme,
) -> (u64, u64) {
    let mut h = DualHasher::new(0x4348_4154_5f4c_494e);
    match conv_key {
        Some(k) => {
            h.write_hash(&1u8);
            h.write_str(k);
        }
        None => h.write_hash(&0u8),
    }
    h.write_hash(&view.is_channel);
    h.write_hash(&view.selected_index);
    h.write_hash(&width);
    h.write_hash(&hyperlinks);
    h.write_hash(&inline_images);
    h.write_hash(&image_cache_len);
    h.write_hash(&loading_older);
    h.write_hash(&oldest_reached);
    match history_error {
        Some(err) => {
            h.write_hash(&1u8);
            h.write_str(err);
        }
        None => h.write_hash(&0u8),
    }
    hash_theme_sig(&mut h, theme);
    h.write_hash(&view.messages.len());
    for msg in view.messages {
        hash_message_sig(&mut h, msg);
    }
    h.finish()
}

thread_local! {
    static CHAT_LINES_CACHE: RefCell<LineSliceCache> = RefCell::new(LineSliceCache::new(4));
}

pub fn build_lines(app: &AppState, ui: &UiState, width: usize) -> Vec<RenderedLine> {
    build_lines_with_hyperlinks(
        &ChatRender::from_app(app, ui),
        &ui.theme,
        width,
        hyperlinks_enabled(),
    )
}

pub fn build_lines_with_images(
    app: &mut AppState,
    ui: &UiState,
    width: usize,
) -> Vec<RenderedLine> {
    build_lines_with_images_cached(app, ui, width).to_vec()
}

pub(crate) fn build_lines_with_images_cached(
    app: &mut AppState,
    ui: &UiState,
    width: usize,
) -> Arc<[RenderedLine]> {
    let hyperlinks = hyperlinks_enabled();
    let width = width.max(12);
    let view = ChatRender::from_app(app, ui);
    let sig = compute_chat_lines_sig(
        app.active_conv_key.as_deref(),
        &view,
        width,
        hyperlinks,
        ui.inline_images,
        app.image_cache.len(),
        app.loading_older,
        app.oldest_reached,
        app.history_error.as_deref(),
        &ui.theme,
    );

    if let Some(cached) = CHAT_LINES_CACHE.with(|cache| cache.borrow_mut().get(sig)) {
        return cached;
    }

    let thumb_cols = width.saturating_sub(4).clamp(16, 40) as u16;
    let thumb_rows = 10u16;
    if ui.inline_images {
        let len = app.messages.len();
        let start = len.saturating_sub(15);
        let to_fetch: Vec<(String, AttachmentInfo)> = app.messages[start..]
            .iter()
            .flat_map(|m| {
                m.attachments
                    .iter()
                    .filter(|att| img_preview::is_image_mime_or_name(&att.content_type, att.name.as_deref()))
                    .map(move |att| (m.msg_id.clone(), att.clone()))
            })
            .collect();
        for (msg_id, att) in to_fetch {
            app.request_teams_image(msg_id, att, thumb_cols, thumb_rows);
        }
    }

    let view = ChatRender::from_app(app, ui);
    let mut out = Vec::new();
    if app.active_conv_key.is_some() {
        if let Some(banner) = build_history_banner_line(
            app.loading_older,
            app.oldest_reached,
            app.history_error.as_deref(),
            !view.messages.is_empty(),
            &ui.theme,
            width,
        ) {
            out.push(banner);
        }
    }
    let mut last_date: Option<String> = None;

    for (msg_index, msg) in view.messages.iter().enumerate() {
        let date = format_date(&msg.created_at);
        if last_date.as_deref() != Some(date.as_str()) {
            out.push(RenderedLine::plain(Line::from(Span::styled(
                format!("── {date} ──"),
                Style::default()
                    .fg(theme_date_color(&ui.theme))
                    .add_modifier(Modifier::BOLD),
            ))));
            last_date = Some(date);
        }

        let selected = view.selected_index == Some(msg_index);
        let thumbs: Option<&std::collections::HashMap<String, RenderedTerminalImage>> =
            if ui.inline_images {
                Some(&app.image_cache)
            } else {
                None
            };
        render_message_item_with_thumbs(
            &mut out,
            msg,
            msg_index,
            MessageScope::Chat,
            view.is_channel,
            selected,
            &ui.theme,
            width,
            hyperlinks,
            thumbs,
            (thumb_cols, thumb_rows),
        );
    }

    let arc: Arc<[RenderedLine]> = Arc::from(out);
    CHAT_LINES_CACHE.with(|cache| cache.borrow_mut().insert(sig, Arc::clone(&arc)));
    arc
}

fn theme_date_color(theme: &Theme) -> ratatui::style::Color {
    theme.date_separator
}

pub fn build_lines_with_hyperlinks(
    view: &ChatRender,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) -> Vec<RenderedLine> {
    let width = width.max(12);
    let mut out = Vec::new();
    let mut last_date: Option<String> = None;

    for (msg_index, msg) in view.messages.iter().enumerate() {
        let date = format_date(&msg.created_at);
        if last_date.as_deref() != Some(date.as_str()) {
            out.push(RenderedLine::plain(Line::from(Span::styled(
                format!("── {date} ──"),
                Style::default()
                    .fg(theme.date_separator)
                    .add_modifier(Modifier::BOLD),
            ))));
            last_date = Some(date);
        }

        let selected = view.selected_index == Some(msg_index);
        render_message_item(
            &mut out,
            msg,
            msg_index,
            MessageScope::Chat,
            view.is_channel,
            selected,
            theme,
            width,
            hyperlinks,
        );
    }

    out
}

/// Renders a single `MessageItem` into `RenderedLine`s. Shared by `chat_view` and `thread_view`.
#[allow(clippy::too_many_arguments)]
pub fn render_message_item(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    scope: MessageScope,
    is_channel: bool,
    selected: bool,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) {
    render_message_item_with_thumbs(
        out,
        msg,
        msg_index,
        scope,
        is_channel,
        selected,
        theme,
        width,
        hyperlinks,
        None,
        (0, 0),
    );
}

#[allow(clippy::too_many_arguments)]
pub fn render_message_item_with_thumbs(
    out: &mut Vec<RenderedLine>,
    msg: &MessageItem,
    msg_index: usize,
    scope: MessageScope,
    is_channel: bool,
    selected: bool,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
    image_cache: Option<&std::collections::HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
) {
    let target = HitTarget::Message { scope, msg_index };

    // Quoted reply banner
    if msg.reply_to_author.is_some() || msg.reply_to_preview.is_some() {
        let author = msg.reply_to_author.as_deref().unwrap_or("답장");
        let preview = msg
            .reply_to_preview
            .as_deref()
            .unwrap_or("")
            .replace(['\n', '\r'], " ");
        let banner = format!("  ↩ {author}: {}", truncate(&preview, width.saturating_sub(12)));
        let mut line = Line::from(Span::styled(banner, theme.quote_style()));
        if selected {
            line = line.style(theme.selection());
        }
        out.push(RenderedLine::clickable(line, target));
    }

    // Channel root post subject
    if let Some(subject) = msg.subject.as_deref().filter(|s| !s.trim().is_empty()) {
        let mut line = Line::from(vec![
            Span::styled("  📌 ", theme.subject_style()),
            Span::styled(truncate(subject, width.saturating_sub(6)), theme.subject_style()),
        ]);
        if selected {
            line = line.style(theme.selection());
        }
        out.push(RenderedLine::clickable(line, target));
    }

    // Message header: time + author + importance badge
    let time = format_time(&msg.created_at);
    let author_style = if msg.is_me {
        Style::default().fg(theme.own).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.author(&msg.author_name))
            .add_modifier(Modifier::BOLD)
    };
    let mut header_spans = vec![
        Span::styled(format!("{time} "), theme.muted_style()),
        Span::styled(msg.author_name.clone(), author_style),
    ];
    if msg.importance.eq_ignore_ascii_case("urgent") {
        header_spans.push(Span::styled(
            " [긴급]",
            Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
        ));
    } else if msg.importance.eq_ignore_ascii_case("high") {
        header_spans.push(Span::styled(
            " [중요]",
            Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
        ));
    }
    if msg.last_modified_at.is_some() && !msg.deleted {
        header_spans.push(Span::styled(" (수정됨)", theme.muted_style()));
    }
    let mut header_line = Line::from(header_spans);
    if selected {
        header_line = header_line.style(theme.selection());
    }
    out.push(RenderedLine::clickable(header_line, target));

    if msg.deleted {
        let mut line = Line::from(Span::styled(
            "  (삭제된 메시지)",
            theme.muted_style().add_modifier(Modifier::ITALIC),
        ));
        if selected {
            line = line.style(theme.selection());
        }
        out.push(RenderedLine::clickable(line, target));
        return;
    }

    // Parse RichBlocks via `crate::parser`
    let mut blocks = parser::parse_message_blocks(&msg.body_html, &msg.attachments, &msg.mentions);
    if blocks.is_empty() && !msg.body_plain.trim().is_empty() {
        blocks.push(RichBlock::Paragraph(vec![RichSpan::plain(&msg.body_plain)]));
    }

    let indent = "  ";
    let mut emitted_attach_indices: Vec<usize> = Vec::new();
    let mut emitted_thumb_ids: Vec<String> = Vec::new();

    for block in &blocks {
        match block {
            RichBlock::AttachmentChip { id, name } => {
                let attach_idx = resolve_attachment_index(&msg.attachments, Some(id.as_str()), None)
                    .unwrap_or(0);
                emitted_attach_indices.push(attach_idx);
                let display_name = name
                    .as_deref()
                    .or_else(|| msg.attachments.get(attach_idx).and_then(|a| a.name.as_deref()))
                    .unwrap_or("첨부파일");
                let is_img = msg.attachments.get(attach_idx).is_some_and(|a| {
                    img_preview::is_image_mime_or_name(&a.content_type, a.name.as_deref())
                });
                let icon = if is_img { "🖼" } else { "📎" };
                let chip = format!("  [{icon} {display_name}]");
                let chip_w = UnicodeWidthStr::width(chip.trim_start()) as u16;
                out.push(RenderedLine::with_inline(
                    Line::from(Span::styled(
                        truncate(&chip, width),
                        theme.accent_style().add_modifier(Modifier::BOLD),
                    )),
                    Some(target),
                    vec![(
                        2,
                        chip_w,
                        HitTarget::Attachment {
                            scope,
                            msg_index,
                            attach_index: attach_idx,
                        },
                    )],
                ));
                if let (Some(cache), Some(att)) = (image_cache, msg.attachments.get(attach_idx)) {
                    if is_img && !emitted_thumb_ids.contains(&att.id) {
                        emitted_thumb_ids.push(att.id.clone());
                        emit_inline_thumb(out, &msg.msg_id, att, cache, thumb_dims, theme, target);
                    }
                }
            }
            RichBlock::InlineImageChip { src, index } => {
                let attach_idx =
                    resolve_attachment_index(&msg.attachments, None, Some(src.as_str()))
                        .unwrap_or(*index);
                emitted_attach_indices.push(attach_idx);
                let chip = format!("  [🖼 이미지 #{}]", index + 1);
                let chip_w = UnicodeWidthStr::width(chip.trim_start()) as u16;
                out.push(RenderedLine::with_inline(
                    Line::from(Span::styled(
                        truncate(&chip, width),
                        theme.accent_style(),
                    )),
                    Some(target),
                    vec![(
                        2,
                        chip_w,
                        HitTarget::Attachment {
                            scope,
                            msg_index,
                            attach_index: attach_idx,
                        },
                    )],
                ));
                if let (Some(cache), Some(att)) = (image_cache, msg.attachments.get(attach_idx)) {
                    if !emitted_thumb_ids.contains(&att.id) {
                        emitted_thumb_ids.push(att.id.clone());
                        emit_inline_thumb(out, &msg.msg_id, att, cache, thumb_dims, theme, target);
                    }
                }
            }
            other => {
                render_rich_blocks(
                    out,
                    std::slice::from_ref(other),
                    indent,
                    selected,
                    Some(target),
                    theme,
                    width,
                    hyperlinks,
                );
            }
        }
    }

    // Emit any remaining file/reference attachments not already emitted by parser blocks
    for (idx, att) in msg.attachments.iter().enumerate() {
        if emitted_attach_indices.contains(&idx) {
            continue;
        }
        if att
            .content_type
            .to_ascii_lowercase()
            .contains("card.adaptive")
        {
            continue;
        }
        let name = att.name.as_deref().unwrap_or(&att.id);
        let is_img = img_preview::is_image_mime_or_name(&att.content_type, att.name.as_deref());
        let icon = if is_img { "🖼" } else { "📎" };
        let chip = format!("  [{icon} {name}]");
        let chip_w = UnicodeWidthStr::width(chip.trim_start()) as u16;
        out.push(RenderedLine::with_inline(
            Line::from(Span::styled(
                truncate(&chip, width),
                theme.accent_style().add_modifier(Modifier::BOLD),
            )),
            Some(target),
            vec![(
                2,
                chip_w,
                HitTarget::Attachment {
                    scope,
                    msg_index,
                    attach_index: idx,
                },
            )],
        ));
        if let Some(cache) = image_cache {
            if is_img && !emitted_thumb_ids.contains(&att.id) {
                emitted_thumb_ids.push(att.id.clone());
                emit_inline_thumb(out, &msg.msg_id, att, cache, thumb_dims, theme, target);
            }
        }
    }

    // Reaction badges row (`[👍 2] [❤️ 1]`)
    if !msg.reactions.is_empty() {
        let mut spans = vec![Span::raw("  ")];
        let mut inline_targets = Vec::new();
        let mut col = 2u16;
        for (r_idx, r) in msg.reactions.iter().enumerate() {
            let emoji = reaction_emoji(&r.reaction_type);
            let badge = format!("[{emoji} {}]", r.count);
            let badge_w = UnicodeWidthStr::width(badge.as_str()) as u16;
            if (col as usize) + (badge_w as usize) > width {
                break;
            }
            spans.push(Span::styled(badge, theme.reaction_style(r.me_reacted)));
            spans.push(Span::raw(" "));
            inline_targets.push((
                col,
                badge_w,
                HitTarget::Reaction {
                    scope,
                    msg_index,
                    reaction_index: r_idx,
                },
            ));
            col += badge_w + 1;
        }
        out.push(RenderedLine::with_inline(
            Line::from(spans),
            Some(target),
            inline_targets,
        ));
    }

    // Channel root post thread indicator
    if is_channel && scope == MessageScope::Chat && msg.parent_msg_id.is_none() {
        let label = if msg.reply_count > 0 {
            format!("  [💬 답글 {}개 — Enter/t 로 스레드 열기]", msg.reply_count)
        } else {
            "  [💬 스레드 시작 (t)]".to_string()
        };
        let label_w = UnicodeWidthStr::width(label.trim_start()) as u16;
        out.push(RenderedLine::with_inline(
            Line::from(Span::styled(
                truncate(&label, width),
                theme.accent_style(),
            )),
            Some(target),
            vec![(2, label_w, HitTarget::OpenThread { msg_index })],
        ));
    }
}

fn emit_inline_thumb(
    out: &mut Vec<RenderedLine>,
    msg_id: &str,
    att: &AttachmentInfo,
    cache: &std::collections::HashMap<String, RenderedTerminalImage>,
    thumb_dims: (u16, u16),
    theme: &Theme,
    target: HitTarget,
) {
    let key = AppState::image_cache_key(msg_id, &att.id, thumb_dims.0, thumb_dims.1);
    if let Some(rendered) = cache.get(&key) {
        for thumb_line in &rendered.lines {
            let mut full = vec![Span::raw("  ")];
            full.extend(thumb_line.spans.clone());
            out.push(RenderedLine::clickable(Line::from(full), target));
        }
        out.push(RenderedLine::clickable(
            Line::from(Span::styled(
                format!(
                    "  ↳ {}x{} ({}) · v 키로 크게보기",
                    rendered.width_px, rendered.height_px, rendered.format_label
                ),
                theme.muted_style(),
            )),
            target,
        ));
    }
}

/// Shared `RichBlock` renderer used by Teams chat and thread views.
#[allow(clippy::too_many_arguments)]
pub fn render_rich_blocks(
    out: &mut Vec<RenderedLine>,
    blocks: &[RichBlock],
    indent: &str,
    selected: bool,
    target: Option<HitTarget>,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) {
    let indent_w = UnicodeWidthStr::width(indent);
    let body_w = width.saturating_sub(indent_w).max(6);
    let mut push_line = |line: Line<'static>| {
        if let Some(t) = target {
            out.push(RenderedLine::clickable(line, t));
        } else {
            out.push(RenderedLine::plain(line));
        }
    };

    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                let wrapped = wrap_rich_spans(spans, body_w, theme, hyperlinks);
                for line_spans in wrapped {
                    let mut full = if indent.is_empty() {
                        Vec::new()
                    } else {
                        vec![Span::raw(indent.to_string())]
                    };
                    full.extend(line_spans);
                    let mut line = Line::from(full);
                    if selected {
                        line = line.style(theme.selection());
                    }
                    push_line(line);
                }
            }
            RichBlock::Heading { level, spans } => {
                let prefix = match level {
                    1 => "# ",
                    2 => "## ",
                    _ => "### ",
                };
                let wrapped =
                    wrap_rich_spans(spans, body_w.saturating_sub(prefix.len()), theme, hyperlinks);
                for (i, line_spans) in wrapped.into_iter().enumerate() {
                    let mut full = vec![Span::styled(
                        if i == 0 {
                            format!("{indent}{prefix}")
                        } else {
                            format!("{indent}  ")
                        },
                        theme.accent_style().add_modifier(Modifier::BOLD),
                    )];
                    for s in line_spans {
                        full.push(Span::styled(
                            s.content.into_owned(),
                            s.style.add_modifier(Modifier::BOLD),
                        ));
                    }
                    let mut line = Line::from(full);
                    if selected {
                        line = line.style(theme.selection());
                    }
                    push_line(line);
                }
            }
            RichBlock::Quote(spans) => {
                let wrapped = wrap_rich_spans(spans, body_w.saturating_sub(2), theme, hyperlinks);
                for line_spans in wrapped {
                    let mut full = vec![Span::styled(format!("{indent}│ "), theme.quote_style())];
                    for s in line_spans {
                        full.push(Span::styled(s.content.into_owned(), theme.quote_style()));
                    }
                    let mut line = Line::from(full);
                    if selected {
                        line = line.style(theme.selection());
                    }
                    push_line(line);
                }
            }
            RichBlock::CodeBlock { lang, code } => {
                let top = match lang.as_deref().filter(|l| !l.is_empty()) {
                    Some(l) => format!("{indent}┌─ {l} ─"),
                    None => format!("{indent}┌─ code ─"),
                };
                push_line(Line::from(Span::styled(
                    truncate(&top, width),
                    theme.muted_style(),
                )));
                for raw_line in code.lines() {
                    let content = truncate(raw_line, body_w.saturating_sub(2));
                    push_line(Line::from(vec![
                        Span::styled(format!("{indent}│ "), theme.muted_style()),
                        Span::styled(content, theme.code_style()),
                    ]));
                }
                push_line(Line::from(Span::styled(
                    format!("{indent}└───────"),
                    theme.muted_style(),
                )));
            }
            RichBlock::BulletList(items) => {
                for item in items {
                    let wrapped =
                        wrap_rich_spans(item, body_w.saturating_sub(2), theme, hyperlinks);
                    for (i, line_spans) in wrapped.into_iter().enumerate() {
                        let mut full = vec![Span::styled(
                            if i == 0 {
                                format!("{indent}• ")
                            } else {
                                format!("{indent}  ")
                            },
                            theme.accent_style(),
                        )];
                        full.extend(line_spans);
                        let mut line = Line::from(full);
                        if selected {
                            line = line.style(theme.selection());
                        }
                        push_line(line);
                    }
                }
            }
            RichBlock::Card {
                title,
                subtitle,
                facts,
                body_lines,
                actions,
            } => {
                push_line(Line::from(Span::styled(
                    format!("{indent}┌─ Adaptive Card ─────────"),
                    theme.accent_style(),
                )));
                if let Some(t) = title {
                    push_line(Line::from(vec![
                        Span::styled(format!("{indent}│ "), theme.accent_style()),
                        Span::styled(
                            truncate(t, body_w.saturating_sub(2)),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                if let Some(sub) = subtitle {
                    push_line(Line::from(vec![
                        Span::styled(format!("{indent}│ "), theme.accent_style()),
                        Span::styled(
                            truncate(sub, body_w.saturating_sub(2)),
                            theme.muted_style(),
                        ),
                    ]));
                }
                for (k, v) in facts {
                    let text = format!("{k}: {v}");
                    push_line(Line::from(vec![
                        Span::styled(format!("{indent}│ "), theme.accent_style()),
                        Span::raw(truncate(&text, body_w.saturating_sub(2))),
                    ]));
                }
                for bl in body_lines {
                    push_line(Line::from(vec![
                        Span::styled(format!("{indent}│ "), theme.accent_style()),
                        Span::raw(truncate(bl, body_w.saturating_sub(2))),
                    ]));
                }
                for (label, url) in actions {
                    let link_label = format!("[{label}]");
                    let rendered = if hyperlinks && !url.is_empty() {
                        osc8(url, &link_label)
                    } else if !url.is_empty() {
                        format!("{link_label} ({url})")
                    } else {
                        link_label
                    };
                    push_line(Line::from(vec![
                        Span::styled(format!("{indent}│ "), theme.accent_style()),
                        Span::styled(
                            rendered,
                            theme.accent_style().add_modifier(Modifier::UNDERLINED),
                        ),
                    ]));
                }
                push_line(Line::from(Span::styled(
                    format!("{indent}└─────────────────────────"),
                    theme.accent_style(),
                )));
            }
            RichBlock::AttachmentChip { .. } | RichBlock::InlineImageChip { .. } => {}
        }
    }
}

fn resolve_attachment_index(
    attachments: &[AttachmentInfo],
    id: Option<&str>,
    src: Option<&str>,
) -> Option<usize> {
    if let Some(id) = id {
        if let Some(pos) = attachments.iter().position(|a| a.id == id) {
            return Some(pos);
        }
    }
    if let Some(src) = src {
        if let Some(pos) = attachments
            .iter()
            .position(|a| a.content_url.as_deref() == Some(src))
        {
            return Some(pos);
        }
    }
    None
}

pub fn reaction_emoji(reaction_type: &str) -> &str {
    match reaction_type {
        "like" => "👍",
        "heart" => "❤️",
        "laugh" => "😆",
        "surprised" => "😮",
        "sad" => "😢",
        "angry" => "😡",
        other => other,
    }
}

fn span_style(s: &RichSpan, theme: &Theme) -> Style {
    let mut style = Style::default();
    if s.style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.style.strike {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if s.style.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if s.style.code {
        style = theme.code_style();
    }
    if s.style.mention {
        style = style.fg(theme.unread).add_modifier(Modifier::BOLD);
    }
    if s.style.link_url.is_some() {
        style = style
            .fg(theme.accent)
            .add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn wrap_rich_spans(
    rich: &[RichSpan],
    width: usize,
    theme: &Theme,
    hyperlinks: bool,
) -> Vec<Vec<Span<'static>>> {
    let limit = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for piece in rich {
        let style = span_style(piece, theme);
        let url = piece.style.link_url.as_deref();
        let mut buf = String::new();

        for ch in piece.text.chars() {
            if ch == '\n' {
                if !buf.is_empty() {
                    let content = match (url, hyperlinks) {
                        (Some(u), true) => osc8(u, &buf),
                        _ => std::mem::take(&mut buf),
                    };
                    current.push(Span::styled(content, style));
                    buf.clear();
                }
                lines.push(std::mem::take(&mut current));
                used = 0;
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > limit && (used > 0 || !buf.is_empty()) {
                if !buf.is_empty() {
                    let content = match (url, hyperlinks) {
                        (Some(u), true) => osc8(u, &buf),
                        _ => std::mem::take(&mut buf),
                    };
                    current.push(Span::styled(content, style));
                    buf.clear();
                }
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            buf.push(ch);
            used += cw;
        }
        if !buf.is_empty() {
            let content = match (url, hyperlinks) {
                (Some(u), true) => osc8(u, &buf),
                _ => buf,
            };
            current.push(Span::styled(content, style));
        }
    }

    lines.push(current);
    lines
}

pub fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

pub fn hyperlinks_enabled() -> bool {
    match std::env::var("TEAMS_HYPERLINKS") {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !matches!(value.as_str(), "0" | "false" | "off" | "no")
        }
        Err(_) => true,
    }
}

pub fn format_date(iso: &str) -> String {
    const WEEKDAYS: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        let local = dt.with_timezone(&Local);
        let wd = WEEKDAYS[local.weekday().num_days_from_monday() as usize];
        return format!("{} ({wd})", local.format("%Y-%m-%d"));
    }
    iso.get(..10).unwrap_or(iso).to_string()
}

pub fn format_time(iso: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&Local).format("%H:%M").to_string();
    }
    iso.get(11..16).unwrap_or("--:--").to_string()
}

pub fn truncate(text: &str, width: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ReactionSummary;

    fn sample_msg(id: &str, subject: Option<&str>, reply_count: usize) -> MessageItem {
        MessageItem {
            msg_key: format!("chan:t1:c1|{id}"),
            conv_key: "chan:t1:c1".into(),
            msg_id: id.into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: subject.map(str::to_string),
            author_id: Some("u1".into()),
            author_name: "홍길동".into(),
            is_me: false,
            created_at: "2026-11-15T06:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: "안녕하세요".into(),
            body_plain: "안녕하세요".into(),
            attachments: vec![AttachmentInfo {
                id: "att-1".into(),
                content_type: "reference".into(),
                content_url: Some("https://contoso.sharepoint.com/Budget.docx".into()),
                name: Some("Budget.docx".into()),
                content_json: None,
                thumbnail_url: None,
            }],
            mentions: vec![],
            reactions: vec![ReactionSummary {
                reaction_type: "like".into(),
                count: 2,
                me_reacted: true,
                users: vec!["u1".into(), "u2".into()],
            }],
            reply_count,
            web_url: None,
        }
    }

    #[test]
    fn channel_post_renders_subject_attachment_reactions_and_thread_indicator() {
        let msgs = vec![sample_msg("m1", Some("주간 보고"), 3)];
        let view = ChatRender {
            messages: &msgs,
            is_channel: true,
            selected_index: Some(0),
        };
        let lines = build_lines_with_hyperlinks(&view, &Theme::default(), 80, false);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");

        assert!(joined.contains("주간 보고"));
        assert!(joined.contains("Budget.docx"));
        assert!(joined.contains("👍 2"));
        assert!(joined.contains("답글 3개"));
        assert!(lines.iter().any(|l| l
            .inline_targets
            .iter()
            .any(|(_, _, t)| matches!(t, HitTarget::OpenThread { msg_index: 0 }))));
        assert!(lines.iter().any(|l| l
            .inline_targets
            .iter()
            .any(|(_, _, t)| matches!(t, HitTarget::Reaction { msg_index: 0, reaction_index: 0, .. }))));
        assert!(lines.iter().any(|l| l
            .inline_targets
            .iter()
            .any(|(_, _, t)| matches!(t, HitTarget::Attachment { msg_index: 0, attach_index: 0, .. }))));
    }

    #[test]
    fn chat_render_signature_and_line_cache_behavior() {
        let mut msgs = vec![sample_msg("m1", Some("주간 보고"), 3)];
        let theme = Theme::default();
        let sig_base = {
            let view = ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            };
            compute_chat_lines_sig(
                Some("chan:t1:c1"),
                &view,
                80,
                false,
                true,
                0,
                false,
                false,
                None,
                &theme,
            )
        };

        // Same inputs produce identical signature and cache hit returns identical lines.
        let mut cache = LineSliceCache::new(2);
        let lines: Arc<[RenderedLine]> = Arc::from(build_lines_with_hyperlinks(
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            &theme,
            80,
            false,
        ));
        cache.insert(sig_base, Arc::clone(&lines));
        let hit = cache.get(sig_base).expect("expected cache hit");
        assert_eq!(hit.len(), lines.len());

        // Selection, width, reaction me_reacted, and history state changes invalidate signature.
        let sig_sel = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: None,
            },
            80,
            false,
            true,
            0,
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_sel);

        let sig_width = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            100,
            false,
            true,
            0,
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_width);

        let sig_loading = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            80,
            false,
            true,
            0,
            true,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_loading);

        msgs[0].reactions[0].me_reacted = false;
        let sig_react = compute_chat_lines_sig(
            Some("chan:t1:c1"),
            &ChatRender {
                messages: &msgs,
                is_channel: true,
                selected_index: Some(0),
            },
            80,
            false,
            true,
            0,
            false,
            false,
            None,
            &theme,
        );
        assert_ne!(sig_base, sig_react);

        // Capacity bound evicts oldest entry.
        cache.insert(sig_sel, Arc::clone(&lines));
        cache.insert(sig_width, Arc::clone(&lines));
        assert!(cache.get(sig_base).is_none());
        assert!(cache.get(sig_width).is_some());
    }

    #[test]
    fn prepend_anchor_preserves_viewport_offset_without_jump() {
        let mut ui = UiState::new();
        ui.chat_scroll.follow_tail = false;
        ui.chat_scroll.total_lines = 100;
        ui.chat_scroll.offset = 0;
        ui.chat_scroll.scroll_up = 120; // oversaturated past max_off=80
        ui.pending_prepend_anchor = Some((100, 0));

        // 50 new lines prepended at top -> total_lines=150, viewport height=20
        apply_prepend_anchor(&mut ui, 150, 20);
        ui.chat_scroll.total_lines = 150;
        assert_eq!(ui.chat_scroll.offset(20), 50);
        assert!(ui.pending_prepend_anchor.is_none());
    }

    #[test]
    fn history_banner_renders_all_states_and_registers_click_target() {
        let theme = Theme::default();

        let loading = build_history_banner_line(true, false, None, true, &theme, 80).unwrap();
        let loading_text: String = loading.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(loading_text.contains("불러오는 중"));
        assert_eq!(loading.target, None);

        let err = build_history_banner_line(false, false, Some("권한 부족"), true, &theme, 80).unwrap();
        let err_text: String = err.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(err_text.contains("권한 부족"));
        assert_eq!(err.target, Some(HitTarget::LoadOlderMessages));

        let eof = build_history_banner_line(false, true, None, true, &theme, 80).unwrap();
        let eof_text: String = eof.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(eof_text.contains("첫 메시지입니다"));
        assert_eq!(eof.target, None);

        let more = build_history_banner_line(false, false, None, true, &theme, 80).unwrap();
        let more_text: String = more.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(more_text.contains("o · H"));
        assert_eq!(more.target, Some(HitTarget::LoadOlderMessages));
    }
}
