//! Tab 1 (`Mail`): Message List Pane (Cozy / Compact, thread grouping, Focused/Unread filters)
//! + Rich HTML Email Reading Pane (`Table`, `CodeBlock`, `QuotedHistory` folding, Meeting Invite
//! RSVP banner, and viewport-driven Halfblock inline thumbnails clickable for HD Sixel preview).

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Local};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    config::DensityMode,
    img_preview::{self, RenderedTerminalImage},
    models::{
        EventRsvpAction, MailAttachment, MailMessage, MeetingRequestInfo, RichBlock, RichSpan,
    },
    parser,
    ui::{
        app::{AppState, Focus, MailListRow, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        theme::Theme,
    },
};

// ---------------------------------------------------------------------------
// Shared DualHasher, LineSliceCache, and RenderedLine (reused across views)
// ---------------------------------------------------------------------------

pub struct DualHasher {
    h1: DefaultHasher,
    h2: u64,
}

impl DualHasher {
    pub fn new(seed: u64) -> Self {
        let mut h1 = DefaultHasher::new();
        seed.hash(&mut h1);
        Self {
            h1,
            h2: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }

    pub fn write_hash<T: Hash>(&mut self, val: &T) {
        val.hash(&mut self.h1);
        let mut sub = DefaultHasher::new();
        val.hash(&mut sub);
        let v = sub.finish();
        self.h2 = self
            .h2
            .wrapping_mul(0x517c_c1b7_2722_0a95)
            .wrapping_add(v ^ (v >> 31));
    }

    pub fn write_str(&mut self, s: &str) {
        s.len().hash(&mut self.h1);
        s.hash(&mut self.h1);
        for b in s.as_bytes() {
            self.h2 = self
                .h2
                .wrapping_mul(0x100_0000_01b3)
                .wrapping_add(*b as u64);
        }
    }

    pub fn finish(&self) -> (u64, u64) {
        (self.h1.finish(), self.h2)
    }
}

pub fn hash_theme_sig(h: &mut DualHasher, theme: &Theme) {
    h.write_hash(&theme.no_color);
    h.write_hash(&theme.ansi16_only);
    h.write_hash(&theme.bg_canvas);
    h.write_hash(&theme.bg_selected);
    h.write_hash(&theme.fg_primary);
    h.write_hash(&theme.fg_secondary);
    h.write_hash(&theme.fg_muted);
    h.write_hash(&theme.accent);
    h.write_hash(&theme.accent_soft);
    h.write_hash(&theme.unread);
    h.write_hash(&theme.warning);
    h.write_hash(&theme.error);
    h.write_hash(&theme.code_fg);
}

#[derive(Debug, Clone)]
pub struct RenderedLine {
    pub line: Line<'static>,
    pub target: Option<HitTarget>,
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

pub struct LineSliceCache {
    entries: Vec<((u64, u64), Arc<[RenderedLine]>)>,
    capacity: usize,
}

impl LineSliceCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn get(&mut self, sig: (u64, u64)) -> Option<Arc<[RenderedLine]>> {
        let pos = self.entries.iter().position(|(k, _)| *k == sig)?;
        if pos + 1 < self.entries.len() {
            let entry = self.entries.remove(pos);
            self.entries.push(entry);
        }
        self.entries.last().map(|(_, v)| Arc::clone(v))
    }

    pub fn insert(&mut self, sig: (u64, u64), val: Arc<[RenderedLine]>) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == sig) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.capacity {
            self.entries.remove(0);
        }
        self.entries.push((sig, val));
    }

    fn retain_latest(&mut self) {
        self.entries.drain(..self.entries.len().saturating_sub(1));
    }
}

thread_local! {
    static MAIL_READER_CACHE: RefCell<LineSliceCache> = RefCell::new(LineSliceCache::new(6));
}

pub(crate) fn retain_latest_reader_layout() {
    MAIL_READER_CACHE.with(|cache| cache.borrow_mut().retain_latest());
}

pub(crate) struct ReaderMailContent {
    fingerprint: (u64, u64, usize),
    blocks: Arc<[RichBlock]>,
    has_attachments: bool,
    has_images: bool,
    has_quoted: bool,
}

impl ReaderMailContent {
    fn refresh(slot: &mut Option<Self>, mail: Option<&MailMessage>) {
        let Some(mail) = mail else {
            *slot = None;
            return;
        };
        let fingerprint =
            parser::html_in::compute_parse_fingerprint(&mail.body_html, &mail.attachments);
        if slot
            .as_ref()
            .is_some_and(|content| content.fingerprint == fingerprint)
        {
            return;
        }
        let blocks = parser::parse_message_blocks_arc(&mail.body_html, &mail.attachments);
        let attachments = parser::effective_mail_attachments_from_blocks(mail, &blocks);
        *slot = Some(Self {
            fingerprint,
            has_attachments: !attachments.is_empty(),
            has_images: attachments.iter().any(|att| {
                img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name))
            }),
            has_quoted: blocks
                .iter()
                .any(|block| matches!(block, RichBlock::QuotedHistory { .. })),
            blocks,
        });
    }
}

pub fn render_scrolled_lines(
    frame: &mut Frame,
    area: Rect,
    lines: &[RenderedLine],
    offset: usize,
    height: usize,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 || height == 0 {
        return;
    }
    let slice: Vec<Line<'static>> = lines
        .iter()
        .skip(offset)
        .take(height)
        .map(|r| r.line.clone())
        .collect();
    frame.render_widget(Paragraph::new(slice), area);

    for (row_idx, r) in lines.iter().skip(offset).take(height).enumerate() {
        let y = area.y.saturating_add(row_idx as u16);
        if let Some(t) = r.target {
            hitmap.add_line(area.x, y, area.width, t);
        }
        for &(col, w, t) in &r.inline_targets {
            let x = area.x.saturating_add(col);
            if x < area.right() {
                let clipped_w = w.min(area.right().saturating_sub(x));
                hitmap.add_line(x, y, clipped_w, t);
            }
        }
    }
}

pub fn hyperlinks_enabled() -> bool {
    match std::env::var("OUTLOOK_HYPERLINKS") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Middle Pane: Mail Message List (Cozy / Compact + Thread Grouping)
// ---------------------------------------------------------------------------

pub fn draw_list(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if ui.theme.bg_canvas != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(ui.theme.bg_canvas)),
            area,
        );
    }

    draw_list_header(frame, area, app, ui, hitmap);

    let list_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.mail_list_scroll.area = list_area;
    if list_area.height == 0 {
        return;
    }

    let rows = app.visible_mail_rows(ui);
    let theme = &ui.theme;
    if rows.is_empty() {
        ui.mail_list_scroll.total_lines = 0;
        let msg = if app.loading_mail {
            " Loading messages…"
        } else if ui.mail_unread_only {
            " No unread messages in this view"
        } else if ui.focused_only {
            " No Focused messages (press F for All)"
        } else {
            " Folder is empty (c to compose)"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate(msg, list_area.width as usize),
                theme.muted_style(),
            )),
            list_area,
        );
        return;
    }

    let row_height: usize = if ui.density == DensityMode::Cozy { 2 } else { 1 };
    let total_rows = rows.len();
    let selected_idx = ui
        .mail_list_scroll
        .selected_idx
        .unwrap_or(0)
        .min(total_rows.saturating_sub(1));
    ui.mail_list_scroll.selected_idx = Some(selected_idx);

    let vis_rows = (list_area.height as usize / row_height).max(1);
    let mut skip = ui.mail_list_scroll.offset.min(total_rows.saturating_sub(1));
    if selected_idx < skip {
        skip = selected_idx;
    } else if selected_idx >= skip + vis_rows {
        skip = selected_idx + 1 - vis_rows;
    }
    ui.mail_list_scroll.offset = skip;
    ui.mail_list_scroll.total_lines = total_rows;

    let list_focused = ui.focus() == Focus::List;
    let width = list_area.width as usize;
    let is_sent_or_drafts = is_sent_or_drafts_folder(app);

    let mut y = list_area.y;
    for (row_idx, row) in rows.iter().enumerate().skip(skip).take(vis_rows) {
        if y >= list_area.bottom() {
            break;
        }
        let Some(msg) = app.mail_messages.get(row.primary_msg_idx) else {
            continue;
        };
        let is_selected = row_idx == selected_idx;
        let is_active = app.active_mail_id.as_deref() == Some(msg.id.as_str());
        let bg_style = if is_selected && list_focused {
            theme.selection()
        } else if is_selected || is_active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(theme.bg_elevated)
            }
        } else {
            Style::default()
        };

        if row_height == 2 && y + 1 < list_area.bottom() {
            let l1 = format_cozy_row_line1(
                msg,
                row,
                width,
                theme,
                bg_style,
                is_selected && list_focused,
                is_sent_or_drafts,
            );
            let l2 = format_cozy_row_line2(msg, row, width, theme, bg_style);
            let rect = Rect::new(list_area.x, y, list_area.width, 2);
            frame.render_widget(Paragraph::new(vec![l1, l2]).style(bg_style), rect);
            hitmap.add(rect, HitTarget::ListItem(row_idx));
            y = y.saturating_add(2);
        } else {
            let l1 = format_compact_row(
                msg,
                row,
                width,
                theme,
                bg_style,
                is_selected && list_focused,
                is_sent_or_drafts,
            );
            let rect = Rect::new(list_area.x, y, list_area.width, 1);
            frame.render_widget(Paragraph::new(l1).style(bg_style), rect);
            hitmap.add(rect, HitTarget::ListItem(row_idx));
            y = y.saturating_add(1);
        }
    }

    if app.loading_older_mail && y < list_area.bottom() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate(" Loading older mail…", width),
                theme.muted_style(),
            )),
            Rect::new(list_area.x, list_area.bottom() - 1, list_area.width, 1),
        );
    }
}

fn draw_list_header(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bar_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_primary)
    };
    frame.render_widget(
        Paragraph::new("").style(bar_style),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let folder_title = app.active_folder_display_name();
    let focused_tag = if ui.focused_only { "[Focused]" } else { "[All]" };
    let thread_tag = if ui.group_threads { "[Threads]" } else { "[Flat]" };
    let compose_tag = "[+Compose]";

    let right_pills = [
        (focused_tag, ButtonId::FocusedToggle, ui.focused_only),
        (thread_tag, ButtonId::ThreadGroupToggle, ui.group_threads),
        (compose_tag, ButtonId::NewMail, false),
    ];
    let right_w: u16 = right_pills
        .iter()
        .map(|(l, _, _)| UnicodeWidthStr::width(*l) as u16 + 1)
        .sum();
    let title_budget = area.width.saturating_sub(right_w + 2) as usize;
    let title_text = truncate(&format!(" {folder_title}"), title_budget);
    frame.render_widget(
        Paragraph::new(Span::styled(
            title_text,
            bar_style.add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x, area.y, area.width.saturating_sub(right_w), 1),
    );

    let mut rx = area.right().saturating_sub(right_w);
    for (label, btn_id, active) in right_pills {
        let w = UnicodeWidthStr::width(label) as u16;
        if rx.saturating_add(w) > area.right() {
            break;
        }
        let st = if active {
            bar_style.fg(theme.accent).add_modifier(Modifier::BOLD)
        } else if btn_id == ButtonId::NewMail {
            bar_style.fg(theme.accent)
        } else {
            bar_style.fg(theme.fg_muted)
        };
        let r = Rect::new(rx, area.y, w, 1);
        frame.render_widget(Paragraph::new(Span::styled(label, st)), r);
        hitmap.add(r, HitTarget::Button(btn_id));
        rx = rx.saturating_add(w + 1);
    }
}

fn is_sent_or_drafts_folder(app: &AppState) -> bool {
    if app.active_folder_id.eq_ignore_ascii_case("sentitems")
        || app.active_folder_id.eq_ignore_ascii_case("drafts")
    {
        return true;
    }
    app.mail_folders
        .iter()
        .find(|f| f.id == app.active_folder_id)
        .and_then(|f| f.well_known_name.as_deref())
        .is_some_and(|w| {
            w.eq_ignore_ascii_case("sentitems") || w.eq_ignore_ascii_case("drafts")
        })
}

fn format_row_correspondent(msg: &MailMessage, is_sent_or_drafts: bool) -> String {
    if is_sent_or_drafts || msg.is_draft {
        if let Some(first) = msg.to_recipients.first() {
            let recip = first.short_display();
            if !recip.trim().is_empty() {
                return format!("To: {recip}");
            }
        }
    }
    msg.from_short()
}

fn format_cozy_row_line1(
    msg: &MailMessage,
    row: &MailListRow,
    width: usize,
    theme: &Theme,
    bg_style: Style,
    is_sel_focused: bool,
    is_sent_or_drafts: bool,
) -> Line<'static> {
    let gutter = if is_sel_focused { "▎" } else { " " };
    let dot = if row.unread_count > 0 { "● " } else { "  " };
    let dot_style = if theme.no_color {
        bg_style
    } else if row.unread_count > 0 {
        bg_style.fg(theme.unread).add_modifier(Modifier::BOLD)
    } else {
        bg_style.fg(theme.fg_subtle)
    };

    let time = format_short_time(&msg.received_at);
    let time_w = UnicodeWidthStr::width(time.as_str());
    let sender = format_row_correspondent(msg, is_sent_or_drafts);
    let max_sender = width.saturating_sub(3 + time_w + 2);
    let sender_trunc = truncate(&sender, max_sender);
    let sender_w = UnicodeWidthStr::width(sender_trunc.as_str());
    let pad = width.saturating_sub(3 + sender_w + time_w + 1);

    let sender_style = if row.unread_count > 0 {
        bg_style.fg(theme.fg_primary).add_modifier(Modifier::BOLD)
    } else {
        bg_style.fg(theme.fg_secondary)
    };

    Line::from(vec![
        Span::styled(gutter, bg_style.fg(theme.accent)),
        Span::styled(dot, dot_style),
        Span::styled(sender_trunc, sender_style),
        Span::styled(" ".repeat(pad), bg_style),
        Span::styled(format!("{time} "), bg_style.fg(theme.fg_muted)),
    ])
}

fn format_cozy_row_line2(
    msg: &MailMessage,
    row: &MailListRow,
    width: usize,
    theme: &Theme,
    bg_style: Style,
) -> Line<'static> {
    let mut badges = String::new();
    if row.any_attachments {
        badges.push_str(" 📎");
    }
    if row.any_flagged {
        badges.push_str(" ⚑");
    }
    if msg.importance.eq_ignore_ascii_case("high") {
        badges.push_str(" ❗");
    }
    if msg.meeting_request.is_some() {
        badges.push_str(" 📅");
    }
    if row.count > 1 {
        badges.push_str(&format!(" [{}]", row.count));
    }
    let badge_w = UnicodeWidthStr::width(badges.as_str());
    let subj_max = width.saturating_sub(3 + badge_w + 1);
    let subj = truncate(msg.effective_subject(), subj_max);
    let subj_w = UnicodeWidthStr::width(subj.as_str());
    let pad = width.saturating_sub(3 + subj_w + badge_w + 1);

    let subj_style = if row.unread_count > 0 {
        bg_style.fg(theme.fg_primary)
    } else {
        bg_style.fg(theme.fg_muted)
    };
    let badge_style = if row.any_flagged {
        bg_style.fg(theme.warning)
    } else {
        bg_style.fg(theme.accent)
    };

    Line::from(vec![
        Span::styled("   ", bg_style),
        Span::styled(subj, subj_style),
        Span::styled(" ".repeat(pad), bg_style),
        Span::styled(format!("{badges} "), badge_style),
    ])
}

fn format_compact_row(
    msg: &MailMessage,
    row: &MailListRow,
    width: usize,
    theme: &Theme,
    bg_style: Style,
    is_sel_focused: bool,
    is_sent_or_drafts: bool,
) -> Line<'static> {
    let gutter = if is_sel_focused { "▎" } else { " " };
    let dot = if row.unread_count > 0 { "● " } else { "  " };
    let dot_style = if row.unread_count > 0 {
        bg_style.fg(theme.unread)
    } else {
        bg_style.fg(theme.fg_subtle)
    };
    let time = format_short_time(&msg.received_at);
    let thread_badge = if row.count > 1 {
        format!(" [{}]", row.count)
    } else {
        String::new()
    };
    let flag_badge = if row.any_flagged { "⚑" } else { "" };
    let right = format!("{flag_badge}{thread_badge} {time} ");
    let right_w = UnicodeWidthStr::width(right.as_str());

    let correspondent = format_row_correspondent(msg, is_sent_or_drafts);
    let sender = truncate(&correspondent, 12);
    let sender_w = UnicodeWidthStr::width(sender.as_str());
    let sender_padded = format!("{sender}{}", " ".repeat(12usize.saturating_sub(sender_w)));
    let subj_max = width.saturating_sub(3 + 12 + 3 + right_w);
    let subj = truncate(msg.effective_subject(), subj_max);
    let used = 3 + UnicodeWidthStr::width(sender_padded.as_str()) + 3 + UnicodeWidthStr::width(subj.as_str()) + right_w;
    let pad = width.saturating_sub(used);

    Line::from(vec![
        Span::styled(gutter, bg_style.fg(theme.accent)),
        Span::styled(dot, dot_style),
        Span::styled(sender_padded, bg_style.fg(theme.fg_secondary)),
        Span::styled(" │ ", bg_style.fg(theme.fg_subtle)),
        Span::styled(subj, bg_style.fg(theme.fg_primary)),
        Span::styled(" ".repeat(pad), bg_style),
        Span::styled(right, bg_style.fg(theme.fg_muted)),
    ])
}

// ---------------------------------------------------------------------------
// Right Pane: Rich HTML Email Reading Pane
// ---------------------------------------------------------------------------

pub fn draw_reader(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if ui.theme.bg_canvas != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(ui.theme.bg_canvas)),
            area,
        );
    }

    app.ensure_displayed_mail_details_local(ui);
    ReaderMailContent::refresh(&mut ui.mail_reader_content, app.active_mail());

    draw_reader_toolbar(frame, area, app, ui, hitmap);

    let body_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.mail_read_scroll.area = body_area;
    if body_area.height == 0 {
        return;
    }

    let Some(selected_mail) = app.active_mail().cloned() else {
        ui.mail_read_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new(" Select an email to read (c: compose · Ctrl+K: jump)")
                .style(ui.theme.muted_style()),
            body_area,
        );
        return;
    };

    // Build thread messages list (either whole conversation thread or just selected message)
    let thread_msgs: Vec<MailMessage> = if ui.group_threads {
        let conv_id = selected_mail
            .conversation_id
            .as_deref()
            .filter(|s| !s.is_empty());
        if let Some(cid) = conv_id {
            let mut list: Vec<MailMessage> = app
                .mail_messages
                .iter()
                .filter(|m| m.conversation_id.as_deref() == Some(cid))
                .cloned()
                .collect();
            list.sort_by(|a, b| a.received_at.cmp(&b.received_at));
            let total = list.len();
            for (pos, msg) in list.iter_mut().enumerate() {
                let is_expanded = pos + 1 == total || app.expanded_thread_ids.contains(&msg.id);
                if is_expanded {
                    if let Some(full) = app.mail_detail_cache.get(&msg.id) {
                        *msg = full.clone();
                    } else if let Some(pinned) = app.pinned_mail.as_ref().filter(|p| p.id == msg.id)
                    {
                        *msg = pinned.clone();
                    }
                }
            }
            if list.is_empty() {
                vec![selected_mail]
            } else {
                list
            }
        } else {
            vec![selected_mail]
        }
    } else {
        vec![selected_mail]
    };

    let width = (body_area.width as usize).max(16);
    let thumb_cols = width.saturating_sub(6).clamp(16, 40) as u16;
    let thumb_rows = 10u16;

    // Request inline image decode for visible/expanded messages in thread
    if ui.inline_images {
        for (t_pos, msg) in thread_msgs.iter().enumerate() {
            let is_last = t_pos + 1 == thread_msgs.len();
            if !is_last && !app.expanded_thread_ids.contains(&msg.id) {
                continue;
            }
            for att in parser::effective_mail_attachments(msg).iter() {
                if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                    app.request_mail_image(msg.id.clone(), att.clone(), thumb_cols, thumb_rows);
                }
            }
        }
    }

    let sig = compute_reader_sig(
        &thread_msgs,
        &app.expanded_thread_ids,
        &app.expanded_quoted_ids,
        ui.inline_images,
        ui.show_link_urls,
        app.image_cache_gen,
        width,
        &ui.theme,
    );

    let lines: Arc<[RenderedLine]> =
        if let Some(cached) = MAIL_READER_CACHE.with(|c| c.borrow_mut().get(sig)) {
            cached
        } else {
            let thumbs = if ui.inline_images {
                Some(&app.image_cache)
            } else {
                None
            };
            let selected_blocks = app.active_mail_id.as_deref().zip(
                ui.mail_reader_content
                    .as_ref()
                    .map(|content| content.blocks.as_ref()),
            );
            let built: Arc<[RenderedLine]> = Arc::from(build_reader_lines_with_selected(
                &thread_msgs,
                &app.expanded_thread_ids,
                &app.expanded_quoted_ids,
                &ui.theme,
                width,
                thumbs,
                (thumb_cols, thumb_rows),
                ui.show_link_urls,
                selected_blocks,
            ));
            MAIL_READER_CACHE.with(|c| c.borrow_mut().insert(sig, Arc::clone(&built)));
            built
        };

    let height = body_area.height as usize;
    ui.mail_read_scroll.total_lines = lines.len();
    let max_off = lines.len().saturating_sub(height);
    ui.mail_read_scroll.offset = ui.mail_read_scroll.offset.min(max_off);

    render_scrolled_lines(
        frame,
        body_area,
        &lines,
        ui.mail_read_scroll.offset,
        height,
        hitmap,
    );
}

fn draw_reader_toolbar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bar_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_primary)
    };
    frame.render_widget(
        Paragraph::new("").style(bar_style),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let mail_opt = app.active_mail();
    let content = ui.mail_reader_content.as_ref();
    let has_attachments = mail_opt.is_some_and(|m| m.has_attachments)
        || content.is_some_and(|parsed| parsed.has_attachments);
    let has_img = content.is_some_and(|parsed| parsed.has_images);
    let has_quoted = content.is_some_and(|parsed| parsed.has_quoted);
    let mut buttons: Vec<(&str, &str, ButtonId)> = vec![
        ("[r Reply]", "[r]", ButtonId::MailReply),
        ("[R All]", "[R]", ButtonId::MailReplyAll),
        ("[f Fwd]", "[f]", ButtonId::MailForward),
        ("[e Archive]", "[e]", ButtonId::MailArchive),
        ("[u Read]", "[u]", ButtonId::MailToggleRead),
        ("[g Flag]", "[g]", ButtonId::MailToggleFlag),
        ("[m Move]", "[m]", ButtonId::MailMove),
        ("[Del]", "[Del]", ButtonId::MailDelete),
    ];
    if mail_opt.is_some() {
        buttons.push(("[H: HTML]", "[H: HTML]", ButtonId::HtmlPreview));
    }
    if has_attachments {
        buttons.push(("[A Attach]", "[A]", ButtonId::Attachments));
    }
    if has_img {
        buttons.push(("[v Image]", "[v]", ButtonId::ImagePreview));
    }
    if has_quoted {
        buttons.push(("[z Quote]", "[z]", ButtonId::ExpandQuoted));
    }

    let full_w: u16 = buttons
        .iter()
        .map(|(full, _, _)| UnicodeWidthStr::width(*full) as u16 + 1)
        .sum();
    let use_compact = full_w > area.width.saturating_sub(1);

    let mut x = area.x + 1;
    for (full, compact, btn_id) in buttons {
        let label = if use_compact { compact } else { full };
        let w = UnicodeWidthStr::width(label) as u16;
        if x.saturating_add(w) > area.right() {
            break;
        }
        let st = if ui.focus() == Focus::Reader {
            bar_style.fg(theme.accent)
        } else {
            bar_style.fg(theme.fg_secondary)
        };
        let r = Rect::new(x, area.y, w, 1);
        frame.render_widget(Paragraph::new(Span::styled(label, st)), r);
        hitmap.add(r, HitTarget::Button(btn_id));
        x = x.saturating_add(w + 1);
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_reader_sig(
    thread_msgs: &[MailMessage],
    expanded_threads: &std::collections::HashSet<String>,
    expanded_quoted: &std::collections::HashSet<String>,
    inline_images: bool,
    show_link_urls: bool,
    image_cache_gen: u64,
    width: usize,
    theme: &Theme,
) -> (u64, u64) {
    let mut h = DualHasher::new(0x4d41_494c_5245_4144);
    h.write_hash(&thread_msgs.len());
    for m in thread_msgs {
        h.write_str(&m.id);
        h.write_str(&m.subject);
        h.write_hash(&m.is_read);
        h.write_hash(&m.flagged);
        h.write_hash(&m.body_loaded);
        h.write_str(&m.body_html);
        h.write_str(&m.body_plain);
        h.write_str(&m.preview);
        h.write_hash(&m.attachments.len());
        h.write_hash(&expanded_threads.contains(&m.id));
        h.write_hash(&expanded_quoted.contains(&m.id));
        if let Some(ref mr) = m.meeting_request {
            h.write_hash(&1u8);
            h.write_hash(&mr.response_status);
        }
    }
    h.write_hash(&inline_images);
    h.write_hash(&show_link_urls);
    h.write_hash(&image_cache_gen);
    h.write_hash(&width);
    hash_theme_sig(&mut h, theme);
    h.finish()
}

pub fn build_reader_lines(
    thread_msgs: &[MailMessage],
    expanded_threads: &std::collections::HashSet<String>,
    expanded_quoted: &std::collections::HashSet<String>,
    theme: &Theme,
    width: usize,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
) -> Vec<RenderedLine> {
    build_reader_lines_with_opts(
        thread_msgs,
        expanded_threads,
        expanded_quoted,
        theme,
        width,
        image_cache,
        thumb_dims,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_reader_lines_with_opts(
    thread_msgs: &[MailMessage],
    expanded_threads: &std::collections::HashSet<String>,
    expanded_quoted: &std::collections::HashSet<String>,
    theme: &Theme,
    width: usize,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
    show_link_urls: bool,
) -> Vec<RenderedLine> {
    build_reader_lines_with_selected(
        thread_msgs,
        expanded_threads,
        expanded_quoted,
        theme,
        width,
        image_cache,
        thumb_dims,
        show_link_urls,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_reader_lines_with_selected(
    thread_msgs: &[MailMessage],
    expanded_threads: &std::collections::HashSet<String>,
    expanded_quoted: &std::collections::HashSet<String>,
    theme: &Theme,
    width: usize,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
    show_link_urls: bool,
    selected_blocks: Option<(&str, &[RichBlock])>,
) -> Vec<RenderedLine> {
    let mut out = Vec::new();
    let total = thread_msgs.len();
    let latest = &thread_msgs[total - 1];

    // Subject header
    let mut subj_spans = vec![
        Span::raw("  "),
        Span::styled(
            truncate(latest.effective_subject(), width.saturating_sub(6)),
            theme.subject_style().add_modifier(Modifier::BOLD),
        ),
    ];
    if latest.flagged {
        subj_spans.push(Span::styled("  ⚑", Style::default().fg(theme.warning)));
    }
    if latest.importance.eq_ignore_ascii_case("high") {
        subj_spans.push(Span::styled(
            "  ❗High",
            Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
        ));
    }
    if total > 1 {
        subj_spans.push(Span::styled(
            format!("  [{total} messages]"),
            theme.muted_style(),
        ));
    }
    out.push(RenderedLine::plain(Line::from(subj_spans)));

    for (t_pos, msg) in thread_msgs.iter().enumerate() {
        let is_last = t_pos + 1 == total;
        let is_expanded = is_last || expanded_threads.contains(&msg.id);

        if t_pos > 0 {
            out.push(RenderedLine::plain(Line::from(Span::styled(
                format!("  {}", "─".repeat(width.saturating_sub(4).max(4))),
                theme.subtle_style(),
            ))));
        }

        if !is_expanded {
            let date = format_full_time(&msg.received_at);
            let snippet = msg.preview.replace(['\n', '\r'], " ");
            let header = format!("  ▸ {} · {} — {}", msg.from_short(), date, snippet);
            out.push(RenderedLine::clickable(
                Line::from(Span::styled(truncate(&header, width), theme.muted_style())),
                HitTarget::ThreadItem(t_pos),
            ));
            continue;
        }

        // Expanded message header block
        let from_line = format!("  From: {}", msg.from_display());
        let date_str = format_full_time(&msg.received_at);
        let mut from_spans = vec![Span::styled(
            truncate(&from_line, width.saturating_sub(date_str.len() + 3)),
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        )];
        let used = UnicodeWidthStr::width(from_spans[0].content.as_ref());
        let pad = width.saturating_sub(used + date_str.len() + 2);
        from_spans.push(Span::raw(" ".repeat(pad)));
        from_spans.push(Span::styled(date_str, theme.muted_style()));
        let from_target = if total > 1 && !is_last {
            Some(HitTarget::ThreadItem(t_pos))
        } else {
            None
        };
        out.push(RenderedLine {
            line: Line::from(from_spans),
            target: from_target,
            inline_targets: Vec::new(),
        });

        if !msg.to_recipients.is_empty() {
            let to_str = msg
                .to_recipients
                .iter()
                .map(|r| r.display())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(RenderedLine::plain(Line::from(Span::styled(
                truncate(&format!("  To:   {to_str}"), width),
                theme.muted_style(),
            ))));
        }

        if !msg.cc_recipients.is_empty() {
            let cc_str = msg
                .cc_recipients
                .iter()
                .map(|r| r.display())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(RenderedLine::plain(Line::from(Span::styled(
                truncate(&format!("  Cc:   {cc_str}"), width),
                theme.muted_style(),
            ))));
        }

        // Meeting Request Invite Banner
        if let Some(ref mr) = msg.meeting_request {
            render_meeting_invite_banner(&mut out, mr, width, theme);
        }

        // Attachment chips bar
        let cached_blocks = selected_blocks
            .filter(|(id, _)| *id == msg.id)
            .map(|(_, blocks)| blocks);
        let eff_attachments = if let Some(blocks) = cached_blocks {
            parser::effective_mail_attachments_from_blocks(msg, blocks)
        } else {
            parser::effective_mail_attachments(msg)
        };
        if !eff_attachments.is_empty() {
            let mut spans = vec![Span::raw("  ")];
            let mut inline_targets = Vec::new();
            let mut col = 2u16;
            for (att_idx, att) in eff_attachments.iter().enumerate() {
                if att_idx > 0 {
                    spans.push(Span::raw(" "));
                    col += 1;
                }
                let is_img =
                    img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name));
                let is_html = !is_img
                    && crate::ui::overlays::html_preview::is_html_mime_or_name(
                        &att.content_type,
                        Some(&att.name),
                    );
                let tag = if is_img {
                    "IMG"
                } else if is_html {
                    "HTML"
                } else {
                    "FILE"
                };
                let name = if att.name.trim().is_empty() {
                    "attachment"
                } else {
                    att.name.as_str()
                };
                let chip = format!("[{tag} {name}]");
                let cw = UnicodeWidthStr::width(chip.as_str()) as u16;
                if (col as usize) + (cw as usize) > width {
                    break;
                }
                spans.push(Span::styled(
                    chip,
                    theme.accent_style().add_modifier(Modifier::BOLD),
                ));
                inline_targets.push((
                    col,
                    cw,
                    HitTarget::Attachment {
                        thread_pos: t_pos,
                        attach_index: att_idx,
                    },
                ));
                col += cw;
            }
            out.push(RenderedLine::with_inline(
                Line::from(spans),
                None,
                inline_targets,
            ));
        }

        // Thin hairline below header
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("  {}", "─".repeat(width.saturating_sub(4).max(4))),
            theme.subtle_style(),
        ))));

        // Body content
        let body_wrap_w = width.saturating_sub(4).max(1);
        if !msg.body_loaded && msg.body_html.trim().is_empty() {
            let preview_text = if !msg.preview.trim().is_empty() {
                msg.preview.as_str()
            } else {
                msg.body_plain.as_str()
            };
            if !preview_text.trim().is_empty() {
                render_wrapped_plain_lines(&mut out, preview_text, body_wrap_w);
                out.push(RenderedLine::plain(Line::default()));
            }
            out.push(RenderedLine::plain(Line::from(Span::styled(
                "  Loading full message…",
                theme.muted_style().add_modifier(Modifier::ITALIC),
            ))));
        } else if !msg.body_html.trim().is_empty() {
            let parsed;
            let blocks = if let Some(blocks) = cached_blocks {
                blocks
            } else {
                parsed = parser::parse_message_blocks_arc(&msg.body_html, &msg.attachments);
                &parsed
            };
            let quote_expanded = expanded_quoted.contains(&msg.id);
            let mut body_lines = Vec::new();
            render_email_blocks(
                &mut body_lines,
                blocks,
                msg,
                t_pos,
                eff_attachments.as_ref(),
                quote_expanded,
                "  ",
                width,
                theme,
                image_cache,
                thumb_dims,
                show_link_urls,
            );
            let has_visible_body = !blocks.is_empty()
                && body_lines.iter().any(|l| {
                    l.line
                        .spans
                        .iter()
                        .any(|s| !s.content.trim().is_empty())
                });
            if has_visible_body {
                out.extend(body_lines);
            } else {
                let fallback = if msg.body_plain.trim().is_empty() {
                    msg.preview.as_str()
                } else {
                    msg.body_plain.as_str()
                };
                render_wrapped_plain_lines(&mut out, fallback, body_wrap_w);
            }
        } else {
            let plain = if msg.body_plain.trim().is_empty() {
                msg.preview.as_str()
            } else {
                msg.body_plain.as_str()
            };
            render_wrapped_plain_lines(&mut out, plain, body_wrap_w);
        }
    }

    out
}

fn render_meeting_invite_banner(
    out: &mut Vec<RenderedLine>,
    mr: &MeetingRequestInfo,
    width: usize,
    theme: &Theme,
) {
    let time_range = match (mr.start_iso.as_deref(), mr.end_iso.as_deref()) {
        (Some(s), Some(e)) => format!("{} – {}", format_full_time(s), format_short_time(e)),
        (Some(s), None) => format_full_time(s),
        _ => "Scheduled time".to_string(),
    };
    let loc = mr
        .location
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|l| format!(" · {l}"))
        .unwrap_or_default();

    let mut line1_spans = vec![
        Span::raw("  "),
        Span::styled(
            truncate(
                &format!("📅 Meeting Invite · {time_range}{loc}"),
                width.saturating_sub(18),
            ),
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let mut line1_targets = Vec::new();
    if mr.online_meeting_url.is_some() {
        let col = line_display_width(&Line::from(line1_spans.clone())) as u16 + 2;
        let pill = "[Join Online]";
        let pw = UnicodeWidthStr::width(pill) as u16;
        line1_spans.push(Span::raw("  "));
        line1_spans.push(Span::styled(
            pill,
            theme.accent_style().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
        line1_targets.push((col, pw, HitTarget::JoinMeetingUrl));
    }
    out.push(RenderedLine::with_inline(
        Line::from(line1_spans),
        None,
        line1_targets,
    ));

    // RSVP action chips
    let resp = mr
        .response_status
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let rsvp_btns = [
        (
            "[Accept (a)]",
            EventRsvpAction::Accept,
            resp == "accepted",
        ),
        (
            "[Tentative (y)]",
            EventRsvpAction::TentativelyAccept,
            resp.contains("tentative"),
        ),
        (
            "[Decline (x)]",
            EventRsvpAction::Decline,
            resp == "declined",
        ),
    ];
    let mut spans = vec![Span::raw("  ")];
    let mut targets = Vec::new();
    let mut col = 2u16;
    for (idx, (label, action, active)) in rsvp_btns.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
            col += 2;
        }
        let w = UnicodeWidthStr::width(*label) as u16;
        let st = if *active {
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            theme.accent_style().add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(*label, st));
        targets.push((col, w, HitTarget::MeetingRsvp(*action)));
        col += w;
    }
    out.push(RenderedLine::with_inline(Line::from(spans), None, targets));
}

#[allow(clippy::too_many_arguments)]
fn render_email_blocks(
    out: &mut Vec<RenderedLine>,
    blocks: &[RichBlock],
    msg: &MailMessage,
    thread_pos: usize,
    eff_attachments: &[MailAttachment],
    quote_expanded: bool,
    indent: &str,
    width: usize,
    theme: &Theme,
    image_cache: Option<&HashMap<String, RenderedTerminalImage>>,
    thumb_dims: (u16, u16),
    show_link_urls: bool,
) {
    let indent_w = UnicodeWidthStr::width(indent);
    let body_w = width.saturating_sub(indent_w + 1).max(8);

    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                for line_spans in wrap_rich_spans(spans, body_w, theme, show_link_urls) {
                    let mut full = vec![Span::raw(indent.to_string())];
                    full.extend(line_spans);
                    out.push(RenderedLine::plain(Line::from(full)));
                }
            }
            RichBlock::Heading { level, spans } => {
                let prefix = match level {
                    1 => "# ",
                    2 => "## ",
                    _ => "### ",
                };
                for (i, line_spans) in wrap_rich_spans(
                    spans,
                    body_w.saturating_sub(prefix.len()),
                    theme,
                    show_link_urls,
                )
                .into_iter()
                .enumerate()
                {
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
                    out.push(RenderedLine::plain(Line::from(full)));
                }
            }
            RichBlock::Quote(spans) => {
                for line_spans in
                    wrap_rich_spans(spans, body_w.saturating_sub(2), theme, show_link_urls)
                {
                    let mut full = vec![
                        Span::raw(indent.to_string()),
                        Span::styled("│ ", theme.quote_style()),
                    ];
                    for s in line_spans {
                        full.push(Span::styled(s.content.into_owned(), theme.quote_style()));
                    }
                    out.push(RenderedLine::plain(Line::from(full)));
                }
            }
            RichBlock::BulletList(items) => {
                for item in items {
                    for (i, line_spans) in
                        wrap_rich_spans(item, body_w.saturating_sub(2), theme, show_link_urls)
                            .into_iter()
                            .enumerate()
                    {
                        let mut full = vec![Span::raw(indent.to_string())];
                        if i == 0 {
                            full.push(Span::styled("• ", theme.accent_style()));
                        } else {
                            full.push(Span::raw("  "));
                        }
                        full.extend(line_spans);
                        out.push(RenderedLine::plain(Line::from(full)));
                    }
                }
            }
            RichBlock::CodeBlock { lang, code } => {
                let code_bg = if theme.no_color || theme.bg_code == Color::Reset {
                    None
                } else {
                    Some(theme.bg_code)
                };
                let header_text = match lang.as_deref().filter(|l| !l.is_empty()) {
                    Some(l) => format!("┌─ {l}"),
                    None => "┌─ code".to_string(),
                };
                let header_trunc = truncate(&header_text, body_w);
                let header_style = match code_bg {
                    Some(bg) => Style::default().fg(theme.fg_muted).bg(bg),
                    None => theme.muted_style(),
                };
                let mut top = vec![
                    Span::raw(indent.to_string()),
                    Span::styled(header_trunc.clone(), header_style),
                ];
                if let Some(bg) = code_bg {
                    let used = UnicodeWidthStr::width(header_trunc.as_str());
                    if used < body_w {
                        top.push(Span::styled(" ".repeat(body_w - used), Style::default().bg(bg)));
                    }
                }
                out.push(RenderedLine::plain(Line::from(top)));

                for raw_line in code.lines() {
                    let expanded = raw_line.replace('\t', "    ");
                    let content = truncate(&expanded, body_w.saturating_sub(2));
                    let content_w = UnicodeWidthStr::width(content.as_str());
                    let mut spans = vec![
                        Span::raw(indent.to_string()),
                        Span::styled("│ ", header_style),
                        Span::styled(content, theme.code_style()),
                    ];
                    if let Some(bg) = code_bg {
                        let used = 2 + content_w;
                        if used < body_w {
                            spans.push(Span::styled(
                                " ".repeat(body_w - used),
                                Style::default().bg(bg),
                            ));
                        }
                    }
                    out.push(RenderedLine::plain(Line::from(spans)));
                }

                let mut bot = vec![
                    Span::raw(indent.to_string()),
                    Span::styled("└─", header_style),
                ];
                if let Some(bg) = code_bg {
                    if body_w > 2 {
                        bot.push(Span::styled(" ".repeat(body_w - 2), Style::default().bg(bg)));
                    }
                }
                out.push(RenderedLine::plain(Line::from(bot)));
            }
            RichBlock::Table { headers, rows } => {
                render_table_block(out, headers, rows, indent, body_w, theme, show_link_urls);
            }
            RichBlock::Divider => {
                out.push(RenderedLine::plain(Line::from(Span::styled(
                    format!("{indent}{}", "─".repeat(body_w.min(48))),
                    theme.subtle_style(),
                ))));
            }
            RichBlock::AttachmentChip { id, name } => {
                let att_idx = eff_attachments
                    .iter()
                    .position(|a| &a.id == id)
                    .unwrap_or(0);
                let label = name.as_deref().unwrap_or(id.as_str());
                let chip = format!("[FILE {label}]");
                let w = UnicodeWidthStr::width(chip.as_str()) as u16;
                out.push(RenderedLine::with_inline(
                    Line::from(vec![
                        Span::raw(indent.to_string()),
                        Span::styled(chip, theme.accent_style().add_modifier(Modifier::BOLD)),
                    ]),
                    None,
                    vec![(
                        indent_w as u16,
                        w,
                        HitTarget::Attachment {
                            thread_pos,
                            attach_index: att_idx,
                        },
                    )],
                ));
            }
            RichBlock::InlineImageChip {
                src,
                content_id,
                index,
            } => {
                let att_idx = resolve_mail_attachment_index(
                    eff_attachments,
                    src,
                    content_id.as_deref(),
                    *index,
                );
                let att_name = eff_attachments
                    .get(att_idx)
                    .map(|a| a.name.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("inline-image");
                let chip = format!("[IMG {att_name}]");
                let cw = UnicodeWidthStr::width(chip.as_str()) as u16;
                let target = HitTarget::Attachment {
                    thread_pos,
                    attach_index: att_idx,
                };
                out.push(RenderedLine::with_inline(
                    Line::from(vec![
                        Span::raw(indent.to_string()),
                        Span::styled(chip, theme.accent_style().add_modifier(Modifier::BOLD)),
                    ]),
                    Some(target),
                    vec![(indent_w as u16, cw, target)],
                ));

                if let (Some(cache), Some(att)) = (image_cache, eff_attachments.get(att_idx)) {
                    let key =
                        AppState::image_cache_key(&msg.id, &att.id, thumb_dims.0, thumb_dims.1);
                    if let Some(rendered) = cache.get(&key) {
                        for thumb_line in &rendered.lines {
                            let mut full = vec![Span::raw(indent.to_string())];
                            full.extend(thumb_line.spans.clone());
                            out.push(RenderedLine::clickable(Line::from(full), target));
                        }
                        let meta = format!(
                            "↳ {}x{} ({}) · click or Alt+v for HD preview",
                            rendered.width_px, rendered.height_px, rendered.format_label
                        );
                        out.push(RenderedLine::clickable(
                            Line::from(vec![
                                Span::raw(indent.to_string()),
                                Span::styled(truncate(&meta, body_w), theme.muted_style()),
                            ]),
                            target,
                        ));
                    }
                }
            }
            RichBlock::QuotedHistory { line_count, blocks } => {
                let target = HitTarget::QuotedToggle(thread_pos);
                let toggle_label = if quote_expanded {
                    format!("▾ Quoted reply ({line_count} lines) — press z to collapse")
                } else {
                    format!("▸ Quoted reply ({line_count} lines) — press z to expand")
                };
                out.push(RenderedLine::clickable(
                    Line::from(vec![
                        Span::raw(indent.to_string()),
                        Span::styled(
                            truncate(&toggle_label, body_w),
                            theme.accent_style().add_modifier(Modifier::ITALIC),
                        ),
                    ]),
                    target,
                ));
                if quote_expanded {
                    let nested_indent = format!("{indent}│ ");
                    render_email_blocks(
                        out,
                        blocks,
                        msg,
                        thread_pos,
                        eff_attachments,
                        true,
                        &nested_indent,
                        width,
                        theme,
                        None,
                        thumb_dims,
                        show_link_urls,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_table_block(
    out: &mut Vec<RenderedLine>,
    headers: &[Vec<RichSpan>],
    rows: &[Vec<Vec<RichSpan>>],
    indent: &str,
    body_w: usize,
    theme: &Theme,
    show_link_urls: bool,
) {
    let cols = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return;
    }

    let cell_text = |spans: &[RichSpan]| -> String {
        let mut s = String::new();
        for (i, sp) in spans.iter().enumerate() {
            s.push_str(&sp.text);
            if show_link_urls {
                if let Some(ref url) = sp.style.link_url {
                    let next_same = spans
                        .get(i + 1)
                        .and_then(|n| n.style.link_url.as_deref())
                        == Some(url.as_str());
                    if !next_same && should_append_inline_href(&sp.text, url) {
                        s.push_str(&format!(" ({url})"));
                    }
                }
            }
        }
        s.replace(['\n', '\r'], " ").trim().to_string()
    };

    let mut col_widths = vec![3usize; cols];
    for (ci, h) in headers.iter().enumerate() {
        col_widths[ci] = col_widths[ci].max(UnicodeWidthStr::width(cell_text(h).as_str()));
    }
    for row in rows {
        for (ci, cell) in row.iter().enumerate() {
            if ci < cols {
                col_widths[ci] =
                    col_widths[ci].max(UnicodeWidthStr::width(cell_text(cell).as_str()));
            }
        }
    }

    // Clamp column widths to fit inside `body_w`
    let border_overhead = cols + 1 + cols * 2;
    let avail_content = body_w.saturating_sub(border_overhead).max(cols * 3);
    let total_content: usize = col_widths.iter().sum();
    if total_content > avail_content {
        for w in &mut col_widths {
            *w = ((*w * avail_content) / total_content.max(1)).max(3);
        }
    }

    let border_line = |left: &str, mid: &str, right: &str| -> String {
        let mut s = String::from(left);
        for (ci, w) in col_widths.iter().enumerate() {
            if ci > 0 {
                s.push_str(mid);
            }
            s.push_str(&"─".repeat(w + 2));
        }
        s.push_str(right);
        s
    };

    let format_row = |cells: &[Vec<RichSpan>], is_header: bool| -> Line<'static> {
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled("│", theme.subtle_style()),
        ];
        for (ci, cw) in col_widths.iter().enumerate() {
            let text = cells.get(ci).map(|c| cell_text(c)).unwrap_or_default();
            let trunc = truncate(&text, *cw);
            let used = UnicodeWidthStr::width(trunc.as_str());
            let pad = cw.saturating_sub(used);
            let style = if is_header {
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg_secondary)
            };
            spans.push(Span::styled(format!(" {trunc}{} ", " ".repeat(pad)), style));
            spans.push(Span::styled("│", theme.subtle_style()));
        }
        Line::from(spans)
    };

    out.push(RenderedLine::plain(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(border_line("┌", "┬", "┐"), theme.subtle_style()),
    ])));

    if !headers.is_empty() {
        out.push(RenderedLine::plain(format_row(headers, true)));
        out.push(RenderedLine::plain(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(border_line("├", "┼", "┤"), theme.subtle_style()),
        ])));
    }

    for row in rows {
        out.push(RenderedLine::plain(format_row(row, false)));
    }

    out.push(RenderedLine::plain(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(border_line("└", "┴", "┘"), theme.subtle_style()),
    ])));
}

fn resolve_mail_attachment_index(
    attachments: &[MailAttachment],
    src: &str,
    content_id: Option<&str>,
    fallback_idx: usize,
) -> usize {
    if let Some(cid) = content_id {
        let norm = cid.trim().trim_start_matches('<').trim_end_matches('>');
        if let Some(pos) = attachments.iter().position(|a| {
            a.content_id.as_deref().is_some_and(|existing| {
                existing
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .eq_ignore_ascii_case(norm)
            })
        }) {
            return pos;
        }
    }
    if let Some(pos) = attachments.iter().position(|a| a.id == src) {
        return pos;
    }
    fallback_idx.min(attachments.len().saturating_sub(1))
}

fn should_append_inline_href(label: &str, url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() {
        return false;
    }
    let lower_u = u.to_ascii_lowercase();
    if !(lower_u.starts_with("https://") || lower_u.starts_with("http://")) {
        return false;
    }
    let t = label.trim();
    if t.is_empty() {
        return false;
    }
    let lower_t = t.to_ascii_lowercase();
    !(lower_t == lower_u
        || lower_t.trim_end_matches('/') == lower_u.trim_end_matches('/')
        || lower_t.contains(&lower_u))
}

fn push_wrapped_text(
    text: &str,
    style: Style,
    limit: usize,
    lines: &mut Vec<Vec<Span<'static>>>,
    current: &mut Vec<Span<'static>>,
    used: &mut usize,
) {
    let mut buf = String::new();
    for ch in text.chars() {
        if ch == '\n' {
            if !buf.is_empty() {
                current.push(Span::styled(std::mem::take(&mut buf), style));
            }
            lines.push(std::mem::take(current));
            *used = 0;
            continue;
        }
        if ch == '\t' {
            for _ in 0..4 {
                if *used + 1 > limit && (*used > 0 || !buf.is_empty()) {
                    if !buf.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    lines.push(std::mem::take(current));
                    *used = 0;
                }
                buf.push(' ');
                *used += 1;
            }
            continue;
        }
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if *used + cw > limit && (*used > 0 || !buf.is_empty()) {
            if !buf.is_empty() {
                current.push(Span::styled(std::mem::take(&mut buf), style));
            }
            lines.push(std::mem::take(current));
            *used = 0;
        }
        buf.push(ch);
        *used += cw;
    }
    if !buf.is_empty() {
        current.push(Span::styled(buf, style));
    }
}

fn wrap_rich_spans(
    rich: &[RichSpan],
    width: usize,
    theme: &Theme,
    show_link_urls: bool,
) -> Vec<Vec<Span<'static>>> {
    let limit = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut active_link_label = String::new();

    for (idx, piece) in rich.iter().enumerate() {
        let mut style = Style::default();
        if piece.style.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if piece.style.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if piece.style.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if piece.style.underline {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if piece.style.code {
            style = theme.code_style();
        }
        if piece.style.link_url.is_some() {
            style = style
                .fg(theme.accent)
                .add_modifier(Modifier::UNDERLINED);
            active_link_label.push_str(&piece.text);
        } else {
            active_link_label.clear();
        }

        push_wrapped_text(
            &piece.text,
            style,
            limit,
            &mut lines,
            &mut current,
            &mut used,
        );

        if show_link_urls {
            if let Some(ref url) = piece.style.link_url {
                let next_same = rich
                    .get(idx + 1)
                    .and_then(|n| n.style.link_url.as_deref())
                    == Some(url.as_str());
                if !next_same {
                    if should_append_inline_href(&active_link_label, url) {
                        let suffix = format!(" ({})", url.trim());
                        push_wrapped_text(
                            &suffix,
                            theme.muted_style(),
                            limit,
                            &mut lines,
                            &mut current,
                            &mut used,
                        );
                    }
                    active_link_label.clear();
                }
            }
        }
    }

    lines.push(current);
    lines
}

fn line_display_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

pub fn format_short_time(iso: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        let local = dt.with_timezone(&Local);
        if local.date_naive() == Local::now().date_naive() {
            return local.format("%H:%M").to_string();
        }
        return local.format("%b %d").to_string();
    }
    iso.get(11..16)
        .or_else(|| iso.get(..10))
        .unwrap_or("--:--")
        .to_string()
}

pub fn format_full_time(iso: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return dt
            .with_timezone(&Local)
            .format("%a %b %d, %H:%M")
            .to_string();
    }
    iso.get(..16).unwrap_or(iso).to_string()
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

/// Wraps a single plain-text line to at most `max_width` display columns,
/// preferring word boundaries when available and splitting long CJK/URL runs cleanly.
pub fn wrap_plain_text_line(line: &str, max_width: usize) -> Vec<String> {
    let limit = max_width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut last_break: Option<usize> = None;
    let mut saw_non_ws = false;

    for raw_ch in line.chars() {
        if raw_ch == '\r' {
            continue;
        }
        let repeat_count = if raw_ch == '\t' { 4 } else { 1 };
        let ch = if raw_ch == '\t' { ' ' } else { raw_ch };

        for _ in 0..repeat_count {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            let is_ws = ch.is_whitespace();

            if cur_w + cw > limit && !cur.is_empty() {
                if is_ws {
                    if saw_non_ws {
                        out.push(cur.trim_end().to_string());
                        cur.clear();
                        cur_w = 0;
                        last_break = None;
                        saw_non_ws = false;
                        continue;
                    }
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                    last_break = None;
                } else if let Some(break_idx) = last_break {
                    let prefix = cur[..break_idx].trim_end().to_string();
                    let remainder = cur[break_idx..].trim_start().to_string();
                    out.push(prefix);
                    cur = remainder;
                    cur_w = UnicodeWidthStr::width(cur.as_str());
                    last_break = None;
                    saw_non_ws = !cur.is_empty();
                    if !cur.is_empty() && cur_w + cw > limit {
                        out.push(std::mem::take(&mut cur));
                        cur_w = 0;
                        saw_non_ws = false;
                    }
                } else {
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                    last_break = None;
                    saw_non_ws = false;
                }
            }

            if is_ws {
                if saw_non_ws {
                    last_break = Some(cur.len());
                }
            } else {
                saw_non_ws = true;
            }
            cur.push(ch);
            cur_w += cw;
        }
    }

    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn render_wrapped_plain_lines(out: &mut Vec<RenderedLine>, text: &str, max_width: usize) {
    let wrap_w = max_width.max(1);
    for line_str in text.lines() {
        for wrapped in wrap_plain_text_line(line_str, wrap_w) {
            out.push(RenderedLine::plain(Line::from(vec![
                Span::raw("  "),
                Span::raw(wrapped),
            ])));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EmailRecipient, TextStyleFlags};

    fn sample_mail(id: &str, conv_id: &str, body_html: &str) -> MailMessage {
        MailMessage {
            id: id.into(),
            internet_message_id: None,
            conversation_id: Some(conv_id.into()),
            folder_id: Some("inbox".into()),
            subject: "Quarterly Review".into(),
            from: Some(EmailRecipient {
                name: Some("Alice".into()),
                address: "alice@contoso.com".into(),
            }),
            to_recipients: vec![EmailRecipient {
                name: Some("Bob".into()),
                address: "bob@contoso.com".into(),
            }],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-19T09:00:00Z".into(),
            sent_at: None,
            last_modified_at: None,
            is_read: false,
            is_draft: false,
            flagged: true,
            importance: "high".into(),
            inference_classification: Some("focused".into()),
            has_attachments: false,
            body_loaded: true,
            preview: "Agenda".into(),
            body_html: body_html.into(),
            body_plain: String::new(),
            attachments: vec![],
            meeting_request: Some(MeetingRequestInfo {
                meeting_message_type: Some("meetingRequest".into()),
                event_id: Some("ev-1".into()),
                start_iso: Some("2026-11-19T14:00:00Z".into()),
                end_iso: Some("2026-11-19T15:00:00Z".into()),
                location: Some("Room 404".into()),
                online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/123".into()),
                response_status: None,
            }),
            web_link: None,
        }
    }

    #[test]
    fn renders_meeting_banner_table_and_folded_quoted_history() {
        let html = r#"
            <p>Hello team</p>
            <table><tr><th>Metric</th><th>Value</th></tr><tr><td>Users</td><td>42</td></tr></table>
            <div id="divRplyFwdMsg"><p>Previous email line 1</p><p>Previous email line 2</p></div>
        "#;
        let msg = sample_mail("m1", "c1", html);
        let theme = Theme::default();
        let expanded_threads = std::collections::HashSet::new();
        let mut expanded_quoted = std::collections::HashSet::new();

        let folded_lines = build_reader_lines(
            &[msg.clone()],
            &expanded_threads,
            &expanded_quoted,
            &theme,
            80,
            None,
            (24, 8),
        );
        let folded_text: String = folded_lines
            .iter()
            .map(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(folded_text.contains("Meeting Invite"));
        assert!(folded_text.contains("[Accept (a)]"));
        assert!(folded_text.contains("[Join Online]"));
        assert!(folded_text.contains("Metric"));
        assert!(folded_text.contains("42"));
        assert!(folded_text.contains("▸ Quoted reply"));
        assert!(!folded_text.contains("Previous email line 1"));

        // Expanding quoted history reveals nested lines
        expanded_quoted.insert(msg.id.clone());
        let expanded_lines = build_reader_lines(
            &[msg],
            &expanded_threads,
            &expanded_quoted,
            &theme,
            80,
            None,
            (24, 8),
        );
        let expanded_text: String = expanded_lines
            .iter()
            .map(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(expanded_text.contains("▾ Quoted reply"));
        assert!(expanded_text.contains("Previous email line 1"));

        // Ensure RichBlock::Table renders cleanly directly as well
        let mut table_out = Vec::new();
        render_table_block(
            &mut table_out,
            &[vec![RichSpan {
                text: "Col".into(),
                style: TextStyleFlags::default(),
            }]],
            &[vec![vec![RichSpan::plain("Val")]]],
            "  ",
            40,
            &theme,
            false,
        );
        assert!(table_out.len() >= 4);
    }

    fn lines_to_text(lines: &[RenderedLine]) -> String {
        lines
            .iter()
            .map(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn unhydrated_message_wraps_preview_and_shows_loading_indicator() {
        let mut msg = sample_mail("m-unhydrated", "c-unhydrated", "");
        msg.body_loaded = false;
        msg.meeting_request = None;
        msg.preview = "This is a long preview snippet from header-only sync that should wrap cleanly across lines rather than getting truncated.".into();
        msg.body_plain = msg.preview.clone();

        let theme = Theme::default();
        let expanded = std::collections::HashSet::new();
        let lines = build_reader_lines(&[msg], &expanded, &expanded, &theme, 44, None, (24, 8));
        let rendered = lines_to_text(&lines);

        assert!(rendered.contains("This is a long preview snippet"));
        assert!(rendered.contains("rather than getting"));
        assert!(rendered.contains("truncated."));
        assert!(rendered.contains("Loading full message…"));
    }

    #[test]
    fn plain_text_body_wraps_long_ascii_and_cjk_without_truncation() {
        let ascii = "Please review https://example.com/very/long/path/to/the/quarterly/report and share feedback.";
        let wrapped_ascii = wrap_plain_text_line(ascii, 24);
        assert!(wrapped_ascii.len() >= 4);
        assert_eq!(wrapped_ascii[0], "Please review");
        assert!(wrapped_ascii.concat().contains("quarterly/report"));
        assert!(wrapped_ascii.last().unwrap().contains("feedback."));
        for w in &wrapped_ascii {
            assert!(UnicodeWidthStr::width(w.as_str()) <= 24);
        }

        let korean = "안녕하세요반갑습니다오늘오후세시에회의가있으니참석부탁드립니다감사합니다";
        let wrapped_ko = wrap_plain_text_line(korean, 16);
        assert!(wrapped_ko.len() >= 4);
        assert_eq!(wrapped_ko.concat(), korean);
        for w in &wrapped_ko {
            assert!(UnicodeWidthStr::width(w.as_str()) <= 16);
        }

        let mut msg = sample_mail("m-plain", "c-plain", "");
        msg.body_loaded = true;
        msg.meeting_request = None;
        msg.body_plain = format!("{ascii}\n{korean}");
        let theme = Theme::default();
        let expanded = std::collections::HashSet::new();
        let lines = build_reader_lines(&[msg], &expanded, &expanded, &theme, 32, None, (24, 8));
        let rendered = lines_to_text(&lines);

        let body_lines: Vec<&str> = rendered
            .lines()
            .skip_while(|l| !l.contains('─'))
            .skip(1)
            .collect();
        for line in &body_lines {
            assert!(
                UnicodeWidthStr::width(*line) <= 32,
                "rendered body line exceeded width 32: {line:?}"
            );
        }
        let cjk_lines: Vec<&str> = body_lines
            .iter()
            .copied()
            .filter(|l| l.chars().any(|c| ('\u{AC00}'..='\u{D7AF}').contains(&c)))
            .collect();
        assert!(
            cjk_lines.len() >= 2,
            "expected CJK run to wrap across multiple lines, got: {cjk_lines:?}"
        );
        let joined_body: String = body_lines.iter().map(|l| l.trim()).collect();
        let joined_cjk: String = cjk_lines.iter().map(|l| l.trim()).collect();

        assert!(rendered.contains("share feedback."));
        assert!(joined_body.contains("참석부탁드립니다감사합니다"));
        assert_eq!(joined_cjk, korean);
    }

    #[test]
    fn html_body_falls_back_to_plain_when_blocks_are_empty_or_hidden() {
        let mut msg = sample_mail(
            "m-empty-html",
            "c-empty-html",
            r#"<div style="display:none">Hidden preheader</div>"#,
        );
        msg.meeting_request = None;
        msg.body_plain = "Fallback plain body text must still render.".into();

        let theme = Theme::default();
        let expanded = std::collections::HashSet::new();
        let lines = build_reader_lines(&[msg], &expanded, &expanded, &theme, 50, None, (24, 8));
        let rendered = lines_to_text(&lines);

        assert!(rendered.contains("Fallback plain body text must still render."));
    }

    #[test]
    fn sent_and_drafts_rows_display_to_recipient_with_fallback() {
        let msg = sample_mail("m-sent", "c-sent", "<p>Hi</p>");
        let row = MailListRow {
            primary_msg_idx: 0,
            thread_msg_indices: vec![0],
            count: 1,
            unread_count: 0,
            any_flagged: false,
            any_attachments: false,
        };
        let theme = Theme::default();
        let cozy = format_cozy_row_line1(&msg, &row, 60, &theme, Style::default(), false, true);
        let cozy_str: String = cozy.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(cozy_str.contains("To: Bob"), "cozy row was: {cozy_str:?}");

        let compact = format_compact_row(&msg, &row, 60, &theme, Style::default(), false, true);
        let compact_str: String = compact.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(compact_str.contains("To: Bob"), "compact row was: {compact_str:?}");

        // Non-sent folder falls back to sender
        let inbox_cozy =
            format_cozy_row_line1(&msg, &row, 60, &theme, Style::default(), false, false);
        let inbox_str: String = inbox_cozy
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(inbox_str.contains("Alice"));
        assert!(!inbox_str.contains("To: Bob"));
    }

    #[test]
    fn show_link_urls_toggle_appends_href_for_named_links_without_duplicating_bare_urls() {
        let html = r#"<p>See <a href="https://docs.example.com/q4">Q4 Report</a> and <a href="https://bare.example.com">https://bare.example.com</a></p>"#;
        let mut msg = sample_mail("m-href", "c-href", html);
        msg.meeting_request = None;
        let theme = Theme::default();
        let expanded = std::collections::HashSet::new();

        let hidden = build_reader_lines_with_opts(
            &[msg.clone()],
            &expanded,
            &expanded,
            &theme,
            100,
            None,
            (24, 8),
            false,
        );
        let hidden_text = lines_to_text(&hidden);
        assert!(hidden_text.contains("Q4 Report"));
        assert!(!hidden_text.contains("(https://docs.example.com/q4)"));

        let shown = build_reader_lines_with_opts(
            &[msg],
            &expanded,
            &expanded,
            &theme,
            100,
            None,
            (24, 8),
            true,
        );
        let shown_text = lines_to_text(&shown);
        assert!(
            shown_text.contains("Q4 Report (https://docs.example.com/q4)"),
            "expected inline href on named link, got:\n{shown_text}"
        );
        assert!(
            !shown_text.contains("(https://bare.example.com)"),
            "bare URL should not be duplicated in parens, got:\n{shown_text}"
        );
    }

    fn make_reader_test_app() -> (AppState, UiState) {
        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
        let (cmd_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = crate::sync::SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    fn render_reader_text(app: &mut AppState, ui: &mut UiState) -> (String, HitMap) {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|f| draw_reader(f, f.area(), app, ui, &mut hitmap))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        (text, hitmap)
    }

    #[test]
    fn reader_and_list_render_summaries_with_lazy_loaded_expanded_thread() {
        let (mut app, mut ui) = make_reader_test_app();
        ui.group_threads = true;

        app.storage
            .upsert_mail_folders(&[crate::models::MailFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 2,
            }])
            .unwrap();

        let mut older = sample_mail("m-old", "conv-1", "<p>OLDER_LAZY_BODY_TOKEN</p>");
        older.folder_id = Some("f-inbox".into());
        older.received_at = "2026-11-19T08:00:00Z".into();
        older.preview = "Older collapsed preview".into();
        older.meeting_request = None;

        let mut newer = sample_mail("m-new", "conv-1", "<p>NEWER_LAZY_BODY_TOKEN</p>");
        newer.folder_id = Some("f-inbox".into());
        newer.received_at = "2026-11-19T10:00:00Z".into();
        newer.preview = "Newer preview".into();
        newer.meeting_request = None;

        app.storage.upsert_mail_messages(&[older, newer]).unwrap();
        app.select_mail_folder(&mut ui, "f-inbox".into());
        assert!(app.mail_messages.iter().all(|m| m.body_html.is_empty()));

        let (collapsed, _) = render_reader_text(&mut app, &mut ui);
        assert!(collapsed.contains("NEWER_LAZY_BODY_TOKEN"));
        assert!(collapsed.contains("Older collapsed preview"));
        assert!(!collapsed.contains("OLDER_LAZY_BODY_TOKEN"));

        // Expanding older thread item lazily loads and renders its body
        app.expanded_thread_ids.insert("m-old".into());
        let (expanded, _) = render_reader_text(&mut app, &mut ui);
        assert!(expanded.contains("NEWER_LAZY_BODY_TOKEN"));
        assert!(expanded.contains("OLDER_LAZY_BODY_TOKEN"));
    }

    #[test]
    fn leaving_mail_keeps_the_layout_reused_on_return_at_the_same_scroll_offset() {
        use crate::ui::{action::{dispatch, Action}, nav_pane::{TAB_CALENDAR, TAB_MAIL}};

        let (mut app, mut ui) = make_reader_test_app();
        for i in 0..6 {
            let id = format!("mail-{i}");
            let body: String = (0..1200).map(|j| format!("<p>Mail {i}, line {j}</p>")).collect();
            app.mail_messages.push(sample_mail(&id, &id, &body));
            app.select_mail_row(&mut ui, i);
            render_reader_text(&mut app, &mut ui);
        }
        ui.mail_read_scroll.offset = 33;
        let (before, _) = render_reader_text(&mut app, &mut ui);
        let latest = MAIL_READER_CACHE.with(|cache| {
            let cache = cache.borrow();
            assert_eq!(cache.entries.len(), 6);
            Arc::clone(&cache.entries.last().unwrap().1)
        });
        dispatch(Action::SelectNavTab(TAB_CALENDAR), &mut app, &mut ui);
        MAIL_READER_CACHE.with(|cache| assert_eq!(cache.borrow().entries.len(), 1));

        dispatch(Action::SelectNavTab(TAB_MAIL), &mut app, &mut ui);
        let (after, _) = render_reader_text(&mut app, &mut ui);
        assert_eq!(ui.mail_read_scroll.offset, 33);
        assert_eq!(before, after);
        MAIL_READER_CACHE.with(|cache| {
            let cache = cache.borrow();
            assert_eq!(cache.entries.len(), 1);
            assert!(Arc::ptr_eq(&cache.entries[0].1, &latest));
        });
    }

    #[test]
    fn warm_oversized_mail_skips_dom_parsing_and_tracks_same_id_quote_edits() {
        use crate::models::CoreEvent;
        use crate::parser::html_in::DOM_PARSE_COUNT;
        use crate::ui::action::{dispatch, Action};

        let pad = |html: String| {
            let padding = "x".repeat(259 * 1024 - 7 - html.len());
            format!("{html}<!--{padding}-->")
        };
        let body = "<p>Mail body</p>".repeat(60);
        let mut mail = sample_mail("m-quote", "m-quote", &pad(body.clone()));
        mail.meeting_request = None;
        let (mut app, mut ui) = make_reader_test_app();
        ui.inline_images = false;
        app.mail_messages.push(mail.clone());
        app.select_mail_row(&mut ui, 0);
        let quote = HitTarget::Button(ButtonId::ExpandQuoted);
        let before = DOM_PARSE_COUNT.with(|count| count.get());
        let (_, hitmap) = render_reader_text(&mut app, &mut ui);
        assert!(hitmap.region_of(quote).is_none());
        let warm = DOM_PARSE_COUNT.with(|count| count.get());
        assert_eq!(warm - before, 1);
        for offset in 1..=12 {
            ui.mail_read_scroll.offset = offset;
            render_reader_text(&mut app, &mut ui);
        }
        assert_eq!(DOM_PARSE_COUNT.with(|count| count.get()), warm);

        ui.mail_read_scroll.top();
        let quoted = r#"<div id="divRplyFwdMsg"><p>OLDER_QUOTED_TEXT</p></div>"#;
        let added = pad(format!("<p>Edited body</p>{quoted}"));
        mail.body_html = added;
        app.handle_core_event(&mut ui, CoreEvent::MailMessageHydrated(Box::new(mail.clone())));
        let (folded, hitmap) = render_reader_text(&mut app, &mut ui);
        assert!(hitmap.region_of(quote).is_some());
        assert!(!folded.contains("OLDER_QUOTED_TEXT"));
        dispatch(Action::ToggleQuotedHistory, &mut app, &mut ui);
        let (opened, _) = render_reader_text(&mut app, &mut ui);
        assert!(opened.contains("OLDER_QUOTED_TEXT"));
        assert_eq!(DOM_PARSE_COUNT.with(|count| count.get()), warm + 1);

        mail.body_html = pad("<p>Edited again</p>".into());
        app.handle_core_event(&mut ui, CoreEvent::MailMessageHydrated(Box::new(mail)));
        let (removed, hitmap) = render_reader_text(&mut app, &mut ui);
        assert!(hitmap.region_of(quote).is_none());
        assert!(removed.contains("Edited again"));
        assert!(!removed.contains("OLDER_QUOTED_TEXT"));
        assert_eq!(DOM_PARSE_COUNT.with(|count| count.get()), warm + 2);
    }
}
