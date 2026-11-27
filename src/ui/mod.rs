//! Root UI module: 3-tier responsive 3-pane layout, rendering pipeline, and shared helpers.

pub mod action;
pub mod app;
pub mod calendar_view;
pub mod composer;
pub mod event;
pub mod hitmap;
pub mod keymap;
pub mod mail_view;
pub mod nav_pane;
pub mod notification;
pub mod overlays;
pub mod theme;
pub mod toast;
pub mod todo_view;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::app::{AppState, Focus, NarrowPane, UiState};
use self::hitmap::{ButtonId, HitMap, HitTarget};
use self::nav_pane::{TAB_CALENDAR, TAB_MAIL, TAB_TASKS};
use self::toast::ToastKind;

/// 3-tier responsive terminal width modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// >= 120 columns: Left Nav + Middle List + Right Reading/Detail Pane.
    Wide,
    /// 80..=119 columns: Left Nav + stacked List (40%) / Reader (60%) or Reader fullscreen.
    Medium,
    /// < 80 columns: Compact top switcher + single active pane (`NarrowPane`).
    Narrow,
}

pub fn layout_mode(width: u16) -> LayoutMode {
    if width >= 120 {
        LayoutMode::Wide
    } else if width >= 80 {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutPlan {
    pub mode: Option<LayoutMode>,
    pub compact_bar: Option<Rect>,
    pub nav: Option<Rect>,
    pub list: Option<Rect>,
    pub reader: Option<Rect>,
    pub status: Rect,
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn compute_layout(
    area: Rect,
    show_nav_rail: bool,
    medium_reader_fullscreen: bool,
    narrow_pane: NarrowPane,
) -> LayoutPlan {
    if area.width < 10 || area.height < 4 {
        return LayoutPlan {
            status: Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            ..Default::default()
        };
    }

    let mode = layout_mode(area.width);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let status = rows[1];

    match mode {
        LayoutMode::Wide => {
            let nav_w = if show_nav_rail { 24 } else { 0 };
            let nav_sep = if nav_w > 0 { 1 } else { 0 };
            let remaining = area.width.saturating_sub(nav_w + nav_sep);
            let list_w = remaining.saturating_sub(34).clamp(28, 42);

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(nav_w),
                    Constraint::Length(nav_sep),
                    Constraint::Length(list_w),
                    Constraint::Length(1),
                    Constraint::Min(24),
                ])
                .split(body);

            LayoutPlan {
                mode: Some(mode),
                compact_bar: None,
                nav: (nav_w > 0).then_some(cols[0]),
                list: Some(cols[2]),
                reader: Some(cols[4]),
                status,
            }
        }
        LayoutMode::Medium => {
            let nav_w = if show_nav_rail { 22 } else { 0 };
            let nav_sep = if nav_w > 0 { 1 } else { 0 };
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(nav_w),
                    Constraint::Length(nav_sep),
                    Constraint::Min(30),
                ])
                .split(body);

            let right = cols[2];
            let (list, reader) = if medium_reader_fullscreen || right.height < 8 {
                (None, Some(right))
            } else {
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Percentage(40),
                        Constraint::Length(1),
                        Constraint::Min(4),
                    ])
                    .split(right);
                (Some(split[0]), Some(split[2]))
            };

            LayoutPlan {
                mode: Some(mode),
                compact_bar: None,
                nav: (nav_w > 0).then_some(cols[0]),
                list,
                reader,
                status,
            }
        }
        LayoutMode::Narrow => {
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(body);
            let compact_bar = Some(split[0]);
            let content = split[1];

            match narrow_pane {
                NarrowPane::Folders => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    nav: Some(content),
                    list: None,
                    reader: None,
                    status,
                },
                NarrowPane::List => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    nav: None,
                    list: Some(content),
                    reader: None,
                    status,
                },
                NarrowPane::Read => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    nav: None,
                    list: None,
                    reader: Some(content),
                    status,
                },
            }
        }
    }
}

pub fn truncate_width(s: &str, max_w: usize) -> String {
    if max_w == 0 {
        return String::new();
    }
    if s.width() <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let budget = max_w.saturating_sub(1);
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub fn draw(frame: &mut Frame, app: &mut AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    let base_bg = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_canvas)
            .fg(ui.theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(base_bg), area);

    let plan = compute_layout(
        area,
        ui.show_nav_rail,
        ui.medium_reader_fullscreen,
        ui.narrow_pane,
    );
    ui.nav_area = plan.nav.unwrap_or_default();
    ui.mail_list_scroll.area = Rect::default();
    ui.mail_read_scroll.area = Rect::default();
    ui.cal_list_scroll.area = Rect::default();
    ui.cal_detail_scroll.area = Rect::default();
    ui.todo_list_scroll.area = Rect::default();
    ui.todo_detail_scroll.area = Rect::default();

    let mut hitmap = HitMap::default();
    let mut cursor_pos = None;

    if let Some(bar_area) = plan.compact_bar {
        draw_compact_bar(frame, bar_area, app, ui, &mut hitmap);
    }

    if let Some(nav_area) = plan.nav {
        hitmap.add(nav_area, HitTarget::Pane(Focus::Nav));
        nav_pane::draw(frame, nav_area, app, ui, &mut hitmap);
    }

    if let Some(list_area) = plan.list {
        hitmap.add(list_area, HitTarget::Pane(Focus::List));
    }
    if let Some(reader_area) = plan.reader {
        hitmap.add(reader_area, HitTarget::Pane(Focus::Reader));
    }

    match ui.active_nav_tab {
        TAB_MAIL => {
            if let Some(list_area) = plan.list {
                mail_view::draw_list(frame, list_area, app, ui, &mut hitmap);
            }
            if let Some(reader_area) = plan.reader {
                mail_view::draw_reader(frame, reader_area, app, ui, &mut hitmap);
            }
        }
        TAB_CALENDAR => {
            if let Some(list_area) = plan.list {
                calendar_view::draw_list(frame, list_area, app, ui, &mut hitmap);
            }
            if let Some(reader_area) = plan.reader {
                calendar_view::draw_detail(frame, reader_area, app, ui, &mut hitmap);
            }
        }
        TAB_TASKS => {
            if let Some(list_area) = plan.list {
                todo_view::draw_list(frame, list_area, app, ui, &mut hitmap);
            }
            if let Some(reader_area) = plan.reader {
                todo_view::draw_detail(frame, reader_area, app, ui, &mut hitmap);
            }
        }
        _ => {}
    }

    // 1-cell vertical hairline separators between adjacent panes
    let focus = ui.focus();
    if let Some(nav_r) = plan.nav {
        if let Some(right_r) = plan.list.or(plan.reader) {
            let sep_x = nav_r.right();
            if sep_x < right_r.x && sep_x < area.right() {
                let sep_focused = matches!(focus, Focus::Nav | Focus::List)
                    || (focus == Focus::Reader && plan.list.is_none());
                let sep_style = if sep_focused {
                    ui.theme.accent_style()
                } else {
                    ui.theme.subtle_style()
                };
                draw_vertical_hairline(frame, sep_x, nav_r.y, nav_r.height, sep_style);
            }
        }
    }

    if let (Some(list_r), Some(reader_r)) = (plan.list, plan.reader) {
        if plan.mode == Some(LayoutMode::Wide) {
            let sep_x = list_r.right();
            if sep_x < reader_r.x && sep_x < area.right() {
                let sep_focused = matches!(focus, Focus::List | Focus::Reader);
                let sep_style = if sep_focused {
                    ui.theme.accent_style()
                } else {
                    ui.theme.subtle_style()
                };
                draw_vertical_hairline(frame, sep_x, list_r.y, list_r.height, sep_style);
            }
        } else if plan.mode == Some(LayoutMode::Medium) {
            let sep_y = list_r.bottom();
            if sep_y < reader_r.y && sep_y < area.bottom() {
                let sep_focused = matches!(focus, Focus::List | Focus::Reader);
                let sep_style = if sep_focused {
                    ui.theme.accent_style()
                } else {
                    ui.theme.subtle_style()
                };
                draw_horizontal_hairline(frame, list_r.x, sep_y, list_r.width, sep_style);
            }
        }
    }

    draw_status_bar(frame, plan.status, app, ui, &mut hitmap);
    draw_toasts(frame, area, ui);

    ui.pending_pixel_graphics.clear();
    let mut overlays = std::mem::take(&mut ui.overlays);
    let last_idx = overlays.len().saturating_sub(1);
    for (idx, overlay) in overlays.iter_mut().enumerate() {
        if idx == last_idx {
            hitmap.clear();
        }
        match overlay {
            overlays::Overlay::ImagePreview(preview) => {
                ui.pending_pixel_graphics.clear();
                preview.draw_sync(frame, area, app, ui, &mut hitmap);
                if idx == last_idx {
                    cursor_pos = None;
                }
            }
            overlays::Overlay::HtmlPreview(preview) => {
                ui.pending_pixel_graphics.clear();
                preview.draw(frame, area, ui, &mut hitmap);
                if idx == last_idx {
                    cursor_pos = None;
                }
            }
            other => {
                ui.pending_pixel_graphics.clear();
                let overlay_cursor = other.draw(frame, area, app, ui, &mut hitmap);
                if idx == last_idx {
                    cursor_pos = overlay_cursor;
                }
            }
        }
    }
    ui.overlays = overlays;

    ui.hitmap = hitmap;
    cursor_pos
}

fn draw_vertical_hairline(
    frame: &mut Frame,
    x: u16,
    y: u16,
    height: u16,
    style: Style,
) {
    if height == 0 {
        return;
    }
    let rect = Rect::new(x, y, 1, height);
    let lines: Vec<Line<'static>> = (0..height)
        .map(|_| Line::from(Span::styled("│", style)))
        .collect();
    frame.render_widget(Paragraph::new(lines), rect);
}

fn draw_horizontal_hairline(
    frame: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    style: Style,
) {
    if width == 0 {
        return;
    }
    let rect = Rect::new(x, y, width, 1);
    let rule = "─".repeat(width as usize);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(rule, style))), rect);
}

fn draw_compact_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bar_style = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_header)
            .fg(ui.theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(bar_style), area);

    let tab_label = match ui.active_nav_tab {
        TAB_CALENDAR => "Cal",
        TAB_TASKS => "Tasks",
        _ => "Mail",
    };

    let mut segments: Vec<(String, Style, Option<HitTarget>)> = Vec::new();
    let pill = |active: bool| -> Style {
        if active {
            if ui.theme.no_color {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
                    .bg(ui.theme.accent)
                    .fg(ui.theme.bg_canvas)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            bar_style.fg(ui.theme.fg_secondary)
        }
    };

    segments.push((
        format!(" 1-3:{tab_label} "),
        bar_style.fg(ui.theme.accent_soft).add_modifier(Modifier::BOLD),
        None,
    ));
    segments.push((
        " Folders ".to_string(),
        pill(ui.narrow_pane == NarrowPane::Folders),
        Some(HitTarget::Button(ButtonId::NarrowFolders)),
    ));
    segments.push((" ".to_string(), bar_style, None));
    segments.push((
        " List ".to_string(),
        pill(ui.narrow_pane == NarrowPane::List),
        Some(HitTarget::Button(ButtonId::NarrowList)),
    ));
    segments.push((" ".to_string(), bar_style, None));
    segments.push((
        " Read ".to_string(),
        pill(ui.narrow_pane == NarrowPane::Read),
        Some(HitTarget::Button(ButtonId::NarrowRead)),
    ));

    let unread = app.unread_inbox_count();
    if unread > 0 && area.width >= 48 {
        segments.push((
            format!(" · {unread}u"),
            bar_style.fg(ui.theme.unread),
            None,
        ));
    }

    let mut x = area.x;
    let mut spans = Vec::new();
    for (text, style, target) in segments {
        let w = text.width() as u16;
        if x + w > area.right() {
            break;
        }
        if let Some(hit) = target {
            hitmap.add_line(x, area.y, w, hit);
        }
        spans.push(Span::styled(text, style));
        x = x.saturating_add(w);
    }

    // Right-aligned Search, Jump & New buttons if room remains
    let right_buttons: &[(&str, Option<ButtonId>)] = &[
        ("[/]", Some(ButtonId::Search)),
        (" ", None),
        ("[Ctrl+K]", Some(ButtonId::CommandPalette)),
        (" ", None),
        (
            "[+New]",
            Some(match ui.active_nav_tab {
                TAB_CALENDAR => ButtonId::NewEvent,
                TAB_TASKS => ButtonId::NewTodoTask,
                _ => ButtonId::NewMail,
            }),
        ),
    ];
    let right_w: u16 = right_buttons.iter().map(|(s, _)| s.width() as u16).sum();
    if x + 1 + right_w <= area.right() {
        let pad = (area.right() - x - right_w) as usize;
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), bar_style));
            x = x.saturating_add(pad as u16);
        }
        let btn_style = if ui.theme.no_color {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .fg(ui.theme.accent_soft)
                .bg(ui.theme.bg_header)
        };
        for (label, btn_opt) in right_buttons {
            let w = label.width() as u16;
            spans.push(Span::styled((*label).to_string(), btn_style));
            if let Some(btn) = btn_opt {
                hitmap.add_line(x, area.y, w, HitTarget::Button(*btn));
            }
            x = x.saturating_add(w);
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), area);
}

fn draw_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bar_style = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_header)
            .fg(ui.theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(bar_style), area);

    let spinner_glyph = SPINNER_FRAMES[(ui.spinner_tick as usize) % SPINNER_FRAMES.len()];
    let (sync_glyph, sync_glyph_color, sync_tail) = if app.syncing {
        (
            spinner_glyph,
            Some(ui.theme.accent),
            format!(" {}", app.sync_summary),
        )
    } else if ui.poll_retrying {
        // Quiet status-bar design: tiny 1-cell amber spinner without scary error banners.
        (spinner_glyph, Some(ui.theme.warning), String::new())
    } else {
        ("✓", Some(ui.theme.success), format!(" {}", app.sync_summary))
    };

    let focus = ui.focus();
    let mode_badge = match focus {
        Focus::Nav => " [NAV] ",
        Focus::List => " [LIST] ",
        Focus::Reader => " [READ] ",
        Focus::Overlay => " [MODAL] ",
    };

    let context_label = match ui.active_nav_tab {
        TAB_CALENDAR => format!("Cal · {}", ui.cal_range.label()),
        TAB_TASKS => {
            let list_name = app
                .active_todo_list_id
                .as_deref()
                .and_then(|id| app.todo_lists.iter().find(|l| l.id == id))
                .map(|l| l.display_name.as_str())
                .unwrap_or("Tasks");
            format!("Tasks · {list_name}")
        }
        _ => {
            let mut label = format!("Mail · {}", app.active_folder_display_name());
            if ui.mail_unread_only {
                label.push_str(" (Unread)");
            } else if ui.focused_only {
                label.push_str(" (Focused)");
            }
            label
        }
    };

    let account_label = if !app.my_email.is_empty() {
        format!("{} · ", app.my_email)
    } else {
        String::new()
    };

    let task_note = ui
        .tasks
        .first_running()
        .map(|t| format!(" · ⏳ {}", t.display()))
        .unwrap_or_default();

    let density_hint = format!("Ctrl+D {}", ui.density.label());
    let full_segments: Vec<(&str, Option<ButtonId>)> = match (focus, ui.active_nav_tab) {
        (Focus::Overlay, _) => vec![("↑↓ Select · Enter Confirm · Esc Close ", None)],
        (_, TAB_CALENDAR) => vec![
            ("1-3 Tab · n New · a/y/x RSVP · ", None),
            ("/ Search", Some(ButtonId::Search)),
            (" · ", None),
            (density_hint.as_str(), Some(ButtonId::DensityToggle)),
            (" · ", None),
            ("Ctrl+R Sync", Some(ButtonId::Refresh)),
            (" · ", None),
            ("Ctrl+, Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        (_, TAB_TASKS) => vec![
            ("1-3 Tab · Space Done · n Task · ", None),
            ("/ Search", Some(ButtonId::Search)),
            (" · ", None),
            (density_hint.as_str(), Some(ButtonId::DensityToggle)),
            (" · ", None),
            ("Ctrl+R Sync", Some(ButtonId::Refresh)),
            (" · ", None),
            ("Ctrl+, Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        _ => vec![
            ("1-3 Tab · r Reply · c New · ", None),
            ("/ Search", Some(ButtonId::Search)),
            (" · ", None),
            ("Ctrl+R Sync", Some(ButtonId::Refresh)),
            (" · ", None),
            ("Ctrl+, Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
    };

    let compact_segments: Vec<(&str, Option<ButtonId>)> = match focus {
        Focus::Overlay => vec![("Esc Close ", None)],
        _ => vec![
            ("/ Search", Some(ButtonId::Search)),
            (" · ", None),
            ("Sync", Some(ButtonId::Refresh)),
            (" · ", None),
            ("Settings", Some(ButtonId::Settings)),
            (" · ", None),
            ("Ctrl+K", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
    };

    let mini_segments: Vec<(&str, Option<ButtonId>)> = match focus {
        Focus::Overlay => vec![("Esc Close ", None)],
        _ => vec![
            ("Ctrl+K", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
    };

    let badge_w = mode_badge.width() as u16;
    let sync_tag = format!("{sync_glyph}{sync_tail}");
    let left_rest = format!(" {account_label}{context_label} · {sync_tag}{task_note}");
    let full_left_w = badge_w + left_rest.width() as u16;

    let seg_width = |segs: &[(&str, Option<ButtonId>)]| -> u16 {
        segs.iter().map(|(s, _)| s.width() as u16).sum()
    };
    let full_right_w = seg_width(&full_segments);
    let compact_right_w = seg_width(&compact_segments);
    let mini_right_w = seg_width(&mini_segments);

    let chosen_right: &[(&str, Option<ButtonId>)] =
        if full_left_w + 2 + full_right_w <= area.width
            || badge_w + 14 + full_right_w <= area.width
        {
            &full_segments
        } else if badge_w + 10 + compact_right_w <= area.width {
            &compact_segments
        } else if badge_w + 6 + mini_right_w <= area.width {
            &mini_segments
        } else {
            &[]
        };

    let right_w = seg_width(chosen_right);
    let left_budget = area
        .width
        .saturating_sub(badge_w + right_w + 1) as usize;
    let left_trimmed = truncate_width(&left_rest, left_budget);
    let left_total_w = badge_w + left_trimmed.width() as u16;
    let pad = area.width.saturating_sub(left_total_w + right_w) as usize;

    let badge_style = if ui.theme.no_color {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
            .fg(ui.theme.accent)
            .bg(ui.theme.bg_header)
            .add_modifier(Modifier::BOLD)
    };
    let hint_style = if ui.theme.no_color {
        ui.theme.muted_style()
    } else {
        Style::default()
            .fg(ui.theme.fg_secondary)
            .bg(ui.theme.bg_header)
    };

    let mut spans = vec![Span::styled(mode_badge, badge_style)];
    let prefix = format!(" {account_label}{context_label} · ");
    let prefix_w = prefix.width();
    let glyph_w = sync_glyph.width();
    let sync_hit_w = sync_tag
        .width()
        .min(left_budget.saturating_sub(prefix_w)) as u16;
    if left_budget >= prefix_w + glyph_w && sync_hit_w > 0 {
        let sync_x = area.x.saturating_add(badge_w).saturating_add(prefix_w as u16);
        if sync_x + sync_hit_w <= area.right() {
            hitmap.add_line(sync_x, area.y, sync_hit_w, HitTarget::Button(ButtonId::Refresh));
        }
    }
    if let Some(glyph_color) = sync_glyph_color {
        if !ui.theme.no_color && left_budget >= prefix_w + glyph_w {
            let after_glyph = truncate_width(
                &format!("{sync_tail}{task_note}"),
                left_budget.saturating_sub(prefix_w + glyph_w),
            );
            spans.push(Span::styled(prefix, bar_style));
            spans.push(Span::styled(sync_glyph, bar_style.fg(glyph_color)));
            if !after_glyph.is_empty() {
                spans.push(Span::styled(after_glyph, bar_style));
            }
        } else {
            spans.push(Span::styled(left_trimmed, bar_style));
        }
    } else {
        spans.push(Span::styled(left_trimmed, bar_style));
    }
    spans.push(Span::styled(" ".repeat(pad), bar_style));

    let mut rx = area.x.saturating_add(left_total_w + pad as u16);
    for (seg, btn_opt) in chosen_right {
        let sw = seg.width() as u16;
        let st = if btn_opt.is_some() {
            if ui.theme.no_color {
                hint_style.add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(ui.theme.accent_soft)
                    .bg(ui.theme.bg_header)
            }
        } else {
            hint_style
        };
        spans.push(Span::styled((*seg).to_string(), st));
        if let Some(btn) = btn_opt {
            if rx + sw <= area.right() {
                hitmap.add_line(rx, area.y, sw, HitTarget::Button(*btn));
            }
        }
        rx = rx.saturating_add(sw);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), area);
}

fn draw_toasts(frame: &mut Frame, area: Rect, ui: &UiState) {
    let Some(toast) = ui.toasts.current() else {
        return;
    };
    if area.width < 24 || area.height < 5 {
        return;
    }
    let width = (toast.message.width() + 4).clamp(20, (area.width / 2) as usize) as u16;
    let rect = Rect {
        x: area.right().saturating_sub(width + 2),
        y: area.bottom().saturating_sub(4),
        width,
        height: 3,
    };
    let color = match toast.kind {
        ToastKind::Info => ui.theme.toast,
        ToastKind::Success => ui.theme.success,
        ToastKind::Error => ui.theme.error,
    };
    let surface = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_elevated)
            .fg(ui.theme.fg_primary)
    };
    let border_st = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default().fg(color).bg(ui.theme.bg_elevated)
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(truncate_width(
            &toast.message,
            width.saturating_sub(2) as usize,
        ))
        .style(surface)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(surface)
                .border_style(border_st),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_3pane_layout_breakpoints() {
        assert_eq!(layout_mode(160), LayoutMode::Wide);
        assert_eq!(layout_mode(120), LayoutMode::Wide);
        assert_eq!(layout_mode(119), LayoutMode::Medium);
        assert_eq!(layout_mode(80), LayoutMode::Medium);
        assert_eq!(layout_mode(79), LayoutMode::Narrow);
    }

    #[test]
    fn wide_3pane_layout_allocates_nav_list_and_reader_with_hairlines() {
        let area = Rect::new(0, 0, 140, 40);
        let plan = compute_layout(area, true, false, NarrowPane::List);
        assert_eq!(plan.mode, Some(LayoutMode::Wide));
        assert!(plan.compact_bar.is_none());

        let nav = plan.nav.expect("wide mode includes nav pane");
        let list = plan.list.expect("wide mode includes middle list pane");
        let reader = plan.reader.expect("wide mode includes right reading pane");

        assert_eq!(nav.width, 24);
        assert_eq!(nav.right() + 1, list.x, "1-cell vertical hairline between nav and list");
        assert_eq!(list.width, 42);
        assert_eq!(list.right() + 1, reader.x, "1-cell vertical hairline between list and reader");
        assert!(reader.width >= 70);
        assert_eq!(plan.status.y, 39);
        assert_eq!(plan.status.height, 1);

        // Hiding left nav gives full space to list + reader.
        let hidden = compute_layout(area, false, false, NarrowPane::List);
        assert!(hidden.nav.is_none());
        assert!(hidden.list.is_some());
        assert!(hidden.reader.is_some());
        assert_eq!(hidden.list.unwrap().x, 0);
    }

    #[test]
    fn medium_layout_splits_list_and_reader_or_fullscreen() {
        let area = Rect::new(0, 0, 100, 32);
        let stacked = compute_layout(area, true, false, NarrowPane::List);
        assert_eq!(stacked.mode, Some(LayoutMode::Medium));
        let nav = stacked.nav.expect("medium nav visible");
        let list = stacked.list.expect("medium list visible on top");
        let reader = stacked.reader.expect("medium reader visible below");

        assert_eq!(nav.width, 22);
        assert_eq!(nav.right() + 1, list.x);
        assert_eq!(list.x, reader.x);
        assert_eq!(list.bottom() + 1, reader.y, "1-row horizontal hairline between list and reader");
        assert!(reader.height > list.height);

        let full = compute_layout(area, true, true, NarrowPane::Read);
        assert!(full.list.is_none());
        assert!(full.reader.is_some());
        assert_eq!(full.reader.unwrap().height, area.height - 1);
    }

    #[test]
    fn narrow_layout_switches_single_pane_with_compact_bar() {
        let area = Rect::new(0, 0, 72, 24);
        let folders = compute_layout(area, true, false, NarrowPane::Folders);
        assert_eq!(folders.mode, Some(LayoutMode::Narrow));
        assert_eq!(folders.compact_bar.map(|r| r.height), Some(1));
        assert!(folders.nav.is_some());
        assert!(folders.list.is_none());
        assert!(folders.reader.is_none());

        let list = compute_layout(area, true, false, NarrowPane::List);
        assert!(list.nav.is_none());
        assert!(list.list.is_some());
        assert!(list.reader.is_none());

        let read = compute_layout(area, true, false, NarrowPane::Read);
        assert!(read.nav.is_none());
        assert!(read.list.is_none());
        assert!(read.reader.is_some());
    }

    #[test]
    fn truncate_width_handles_multibyte_unicode() {
        assert_eq!(truncate_width("αβγδε", 4), "αβγ…");
        assert_eq!(truncate_width("FullWidth：日本語", 12), "FullWidth：…");
        assert_eq!(truncate_width("abc", 5), "abc");
    }

    #[tokio::test]
    async fn audited_buttons_are_registered_and_dispatched_on_mouse_click() {
        use crate::models::{EmailRecipient, MailAttachment, MailMessage};
        use crate::ui::action::{self, Action};
        use crate::ui::overlays::mail_compose::MailComposeOverlay;
        use crate::ui::overlays::{Overlay, OverlayKind};
        use ratatui::{backend::TestBackend, Terminal};

        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        let msg = MailMessage {
            id: "msg-btn-1".into(),
            internet_message_id: None,
            conversation_id: Some("conv-btn-1".into()),
            folder_id: Some("inbox".into()),
            subject: "Budget Review".into(),
            from: Some(EmailRecipient {
                name: Some("Alice".into()),
                address: "alice@contoso.com".into(),
            }),
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-20T09:00:00Z".into(),
            sent_at: None,
            last_modified_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: Some("focused".into()),
            has_attachments: true,
            body_loaded: true,
            preview: "See attached".into(),
            body_html: r#"<p>Latest update</p><div id="divRplyFwdMsg"><p>Older reply</p></div>"#.into(),
            body_plain: "Latest update".into(),
            attachments: vec![MailAttachment {
                id: "att-1".into(),
                name: "report.pdf".into(),
                content_type: "application/pdf".into(),
                size: 4096,
                is_inline: false,
                content_id: None,
                content_bytes_b64: None,
            }],
            meeting_request: None,
            web_link: None,
            ..Default::default()
        };
        app.mail_messages.push(msg);
        app.active_mail_id = Some("msg-btn-1".into());
        ui.mail_list_scroll.selected_idx = Some(0);

        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal
            .draw(|frame| {
                let _ = draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // Status bar & mail reader toolbar buttons must all be registered
        for required in [
            ButtonId::Search,
            ButtonId::Settings,
            ButtonId::Refresh,
            ButtonId::Attachments,
            ButtonId::ExpandQuoted,
        ] {
            assert!(
                ui.hitmap.region_of(HitTarget::Button(required)).is_some(),
                "expected ButtonId::{required:?} in HitMap"
            );
        }

        // Click Search -> opens Search overlay
        let search_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::Search)).unwrap();
        action::dispatch(Action::Click { column: search_r.x, row: search_r.y }, &mut app, &mut ui);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::Search));
        ui.close_top_overlay();

        // Click Settings -> opens Settings overlay
        let settings_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::Settings)).unwrap();
        action::dispatch(Action::Click { column: settings_r.x, row: settings_r.y }, &mut app, &mut ui);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::Settings));
        ui.close_top_overlay();

        // Click Attachments -> opens Attachments overlay
        let attach_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::Attachments)).unwrap();
        action::dispatch(Action::Click { column: attach_r.x, row: attach_r.y }, &mut app, &mut ui);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::Attachments));
        ui.close_top_overlay();

        // Click ExpandQuoted -> toggles expanded_quoted_ids for active mail
        assert!(!app.expanded_quoted_ids.contains("msg-btn-1"));
        let quote_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::ExpandQuoted)).unwrap();
        action::dispatch(Action::Click { column: quote_r.x, row: quote_r.y }, &mut app, &mut ui);
        assert!(app.expanded_quoted_ids.contains("msg-btn-1"));

        // Click Refresh -> dispatches cleanly without opening an overlay or panicking
        let refresh_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::Refresh)).unwrap();
        action::dispatch(Action::Click { column: refresh_r.x, row: refresh_r.y }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());

        // Compose overlay buttons must ALL be registered & clickable even on an 80-col terminal
        let mut narrow_term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        ui.push_overlay(Overlay::mail_compose(MailComposeOverlay::new()));
        narrow_term
            .draw(|frame| {
                let _ = draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        for compose_btn in [
            ButtonId::ComposeSend,
            ButtonId::ComposeAttach,
            ButtonId::ComposeSaveDraft,
            ButtonId::ComposeCancel,
        ] {
            assert!(
                ui.hitmap.region_of(HitTarget::Button(compose_btn)).is_some(),
                "expected ButtonId::{compose_btn:?} in 80-col MailCompose HitMap"
            );
        }

        // Click ComposeSend with empty To -> triggers validation error cleanly
        let send_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::ComposeSend)).unwrap();
        action::dispatch(Action::Click { column: send_r.x, row: send_r.y }, &mut app, &mut ui);
        if let Some(Overlay::MailCompose(ref c)) = ui.overlays.last() {
            assert!(c.error.is_some());
        } else {
            panic!("expected MailCompose to remain open with validation error");
        }

        // Click ComposeAttach -> opens FilePicker overlay on top of MailCompose
        let c_attach_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::ComposeAttach)).unwrap();
        action::dispatch(Action::Click { column: c_attach_r.x, row: c_attach_r.y }, &mut app, &mut ui);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::FilePicker));
        ui.close_top_overlay();

        // Click ComposeSaveDraft -> dispatches cleanly
        let c_draft_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::ComposeSaveDraft)).unwrap();
        action::dispatch(Action::Click { column: c_draft_r.x, row: c_draft_r.y }, &mut app, &mut ui);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::MailCompose));

        // Click ComposeCancel -> closes MailCompose overlay
        let c_cancel_r = ui.hitmap.region_of(HitTarget::Button(ButtonId::ComposeCancel)).unwrap();
        action::dispatch(Action::Click { column: c_cancel_r.x, row: c_cancel_r.y }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());

        app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    fn make_ui_test_harness() -> (AppState, UiState) {
        use crate::{
            auth::AuthManager,
            config::AppConfig,
            graph::GraphClient,
            storage::Storage,
            sync::{FocusTarget, SyncController},
        };
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (nudge_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    fn sample_mail_msg(id: &str, subj: &str, received_at: &str) -> crate::models::MailMessage {
        crate::models::MailMessage {
            id: id.into(),
            conversation_id: Some(format!("conv-{id}")),
            folder_id: Some("inbox".into()),
            subject: subj.into(),
            received_at: received_at.into(),
            is_read: false,
            body_loaded: true,
            body_plain: format!("Body of {id}\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12"),
            ..Default::default()
        }
    }

    #[test]
    fn right_click_selects_clicked_item_and_blocks_over_modals() {
        use crate::models::{CalendarEvent, TodoTask};
        use crate::ui::action::Action;
        use crate::ui::overlays::context_menu::ContextTarget;
        use crate::ui::overlays::{Overlay, OverlayKind};
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_ui_test_harness();
        ui.group_threads = false;
        app.mail_messages = vec![
            sample_mail_msg("m1", "First", "2026-11-25T10:00:00Z"),
            sample_mail_msg("m2", "Second", "2026-11-25T09:00:00Z"),
        ];
        app.active_mail_id = Some("m1".into());
        ui.mail_list_scroll.selected_idx = Some(0);

        let mut term = Terminal::new(TestBackend::new(140, 32)).unwrap();
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();

        // Right-click second mail item -> selects m2 BEFORE building ContextMenu
        let r_m2 = ui.hitmap.region_of(HitTarget::ListItem(1)).unwrap();
        action::dispatch(
            Action::OpenContextMenuAt {
                column: r_m2.x,
                row: r_m2.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(app.active_mail_id.as_deref(), Some("m2"));
        assert_eq!(ui.mail_list_scroll.selected_idx, Some(1));
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::ContextMenu(m)) if m.context == ContextTarget::MailItem(1)
        ));

        // Right-click while ContextMenu is open replaces it instead of stacking
        action::dispatch(
            Action::OpenContextMenuAt { column: 5, row: 5 },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.overlays.len(), 1);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::ContextMenu));
        ui.close_top_overlay();

        // Right-click while a modal (Settings) is open is blocked (no stacking)
        ui.push_overlay(Overlay::settings());
        action::dispatch(
            Action::OpenContextMenuAt {
                column: r_m2.x,
                row: r_m2.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.overlays.len(), 1);
        assert_eq!(ui.overlays.last().map(Overlay::kind), Some(OverlayKind::Settings));
        ui.close_top_overlay();

        // Calendar right-click selects clicked event
        ui.active_nav_tab = TAB_CALENDAR;
        app.calendar_events = vec![
            CalendarEvent {
                id: "ev1".into(),
                subject: "Standup".into(),
                start_iso: "2026-11-25T09:00:00Z".into(),
                end_iso: "2026-11-25T09:30:00Z".into(),
                ..Default::default()
            },
            CalendarEvent {
                id: "ev2".into(),
                subject: "Design".into(),
                start_iso: "2026-11-25T11:00:00Z".into(),
                end_iso: "2026-11-25T12:00:00Z".into(),
                ..Default::default()
            },
        ];
        app.active_event_id = Some("ev1".into());
        ui.cal_list_scroll.selected_idx = Some(0);
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();
        let r_ev2 = ui.hitmap.region_of(HitTarget::ListItem(1)).unwrap();
        action::dispatch(
            Action::OpenContextMenuAt {
                column: r_ev2.x,
                row: r_ev2.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(app.active_event_id.as_deref(), Some("ev2"));
        assert_eq!(ui.cal_list_scroll.selected_idx, Some(1));
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::ContextMenu(m)) if m.context == ContextTarget::CalendarEvent(1)
        ));
        ui.close_top_overlay();

        // Tasks right-click on TodoCheckbox selects clicked task and opens Task Menu (not Global)
        ui.active_nav_tab = TAB_TASKS;
        app.todo_tasks = vec![
            TodoTask {
                id: "t1".into(),
                list_id: "l1".into(),
                title: "Task 1".into(),
                status: "notStarted".into(),
                importance: "normal".into(),
                body_plain: None,
                due_date_iso: None,
                reminder_iso: None,
                completed_at: None,
                created_at: None,
                updated_at: None,
            },
            TodoTask {
                id: "t2".into(),
                list_id: "l1".into(),
                title: "Task 2".into(),
                status: "notStarted".into(),
                importance: "high".into(),
                body_plain: None,
                due_date_iso: None,
                reminder_iso: None,
                completed_at: None,
                created_at: None,
                updated_at: None,
            },
        ];
        app.active_todo_task_id = Some("t1".into());
        ui.todo_list_scroll.selected_idx = Some(0);
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();
        let r_cb2 = ui.hitmap.region_of(HitTarget::TodoCheckbox(1)).unwrap();
        action::dispatch(
            Action::OpenContextMenuAt {
                column: r_cb2.x,
                row: r_cb2.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(app.active_todo_task_id.as_deref(), Some("t2"));
        assert_eq!(ui.todo_list_scroll.selected_idx, Some(1));
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::ContextMenu(m)) if m.context == ContextTarget::TodoTask(1)
        ));
    }

    #[test]
    fn backdrop_click_on_mail_compose_stashes_draft() {
        use crate::ui::action::Action;
        use crate::ui::overlays::mail_compose::MailComposeOverlay;
        use crate::ui::overlays::Overlay;
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_ui_test_harness();
        let mut compose = MailComposeOverlay::new();
        compose.to = "alice@contoso.com".into();
        compose.subject = "Important Draft".into();
        compose.body.insert_str("Do not lose this body!");
        let draft_key = compose.mode.draft_key();
        ui.push_overlay(Overlay::mail_compose(compose));

        let mut term = Terminal::new(TestBackend::new(120, 36)).unwrap();
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();

        // Click at (0, 0) outside the centered compose modal
        action::dispatch(Action::Click { column: 0, row: 0 }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty(), "backdrop click should close compose");
        let saved = app
            .draft_store
            .get_mail(&draft_key)
            .expect("unsent draft must be stashed on backdrop click");
        assert_eq!(saved.subject, "Important Draft");
        assert_eq!(saved.body, "Do not lose this body!");
    }

    #[test]
    fn event_form_field_clicks_focus_without_advancing_and_cancel_does_not_create() {
        use crate::ui::action::Action;
        use crate::ui::overlays::event_form::EventFormOverlay;
        use crate::ui::overlays::{Overlay, OverlayOutcome};
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_ui_test_harness();
        let mut form = EventFormOverlay::new();
        form.subject = "Quarterly Sync".into();
        form.start = "2026-11-26 10:00".into();
        form.end = "2026-11-26 11:00".into();
        form.field = 2;

        // Ctrl+S (SaveDraft) submits the form cleanly
        let save_outcome = form.clone().handle(&Action::SaveDraft);
        assert!(matches!(
            save_outcome,
            OverlayOutcome::CloseAndDispatch(Action::CreateCalendarEvent(_))
        ));

        ui.push_overlay(Overlay::EventForm(form));
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();

        // Click field 0 (Subject) -> focuses field 0 and does NOT advance to 1
        let f0 = ui.hitmap.region_of(HitTarget::OverlayItem { index: 0 }).unwrap();
        action::dispatch(Action::Click { column: f0.x, row: f0.y }, &mut app, &mut ui);
        if let Some(Overlay::EventForm(ref ef)) = ui.overlays.last() {
            assert_eq!(ef.field, 0, "clicking Subject must keep focus on field 0");
        } else {
            panic!("EventForm should stay open");
        }

        // Click field 4 (Location) -> focuses field 4 and does NOT advance to 5
        let f4 = ui.hitmap.region_of(HitTarget::OverlayItem { index: 4 }).unwrap();
        action::dispatch(Action::Click { column: f4.x, row: f4.y }, &mut app, &mut ui);
        if let Some(Overlay::EventForm(ref ef)) = ui.overlays.last() {
            assert_eq!(ef.field, 4, "clicking Location must keep focus on field 4");
        } else {
            panic!("EventForm should stay open");
        }

        // Submit and Cancel must be two distinct buttons; Cancel closes without creating an event
        let submit_r = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::EventFormSubmit))
            .expect("EventFormSubmit button registered");
        let cancel_r = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::EventFormCancel))
            .expect("EventFormCancel button registered");
        assert!(submit_r.x + submit_r.width <= cancel_r.x);

        action::dispatch(
            Action::Click {
                column: cancel_r.x,
                row: cancel_r.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty(), "clicking Cancel must close EventForm");
        assert!(
            app.calendar_events.is_empty(),
            "clicking Cancel must never create an event"
        );
    }

    #[test]
    fn sidebar_calendar_today_and_tasks_pills_never_toggle_mail_unread() {
        use crate::ui::action::Action;
        use crate::ui::app::CalendarRangeFilter;
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_ui_test_harness();
        let mut term = Terminal::new(TestBackend::new(140, 32)).unwrap();

        // Calendar tab: [Today] pill jumps to Today and does NOT touch mail_unread_only
        ui.active_nav_tab = TAB_CALENDAR;
        ui.cal_range = CalendarRangeFilter::AllUpcoming;
        ui.mail_unread_only = false;
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();
        let today_r = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::CalendarToday))
            .expect("[Today] pill registered as CalendarToday");
        action::dispatch(
            Action::Click {
                column: today_r.x,
                row: today_r.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.cal_range, CalendarRangeFilter::Today);
        assert!(!ui.mail_unread_only);

        // Tasks tab: [Tasks] pill toggles todo_hide_completed and does NOT touch mail_unread_only
        ui.active_nav_tab = TAB_TASKS;
        ui.todo_hide_completed = false;
        term.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();
        let tasks_r = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::TodoFilterToggle))
            .expect("[Tasks] pill registered as TodoFilterToggle");
        action::dispatch(
            Action::Click {
                column: tasks_r.x,
                row: tasks_r.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.todo_hide_completed);
        assert!(!ui.mail_unread_only);
    }

    #[test]
    fn fullscreen_reader_clears_stale_list_area_and_pane_clicks_focus_panes() {
        use crate::ui::action::Action;
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_ui_test_harness();
        app.mail_messages = vec![
            sample_mail_msg("m1", "Long email", "2026-11-25T10:00:00Z"),
            sample_mail_msg("m2", "Second email", "2026-11-25T09:00:00Z"),
        ];
        app.mail_messages[0].body_plain = (0..60)
            .map(|i| format!("Paragraph line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.active_mail_id = Some("m1".into());
        ui.mail_list_scroll.selected_idx = Some(0);

        // First draw Wide (populates mail_list_scroll.area)
        let mut wide = Terminal::new(TestBackend::new(140, 32)).unwrap();
        wide.draw(|f| {
            let _ = draw(f, &mut app, &mut ui);
        })
        .unwrap();
        assert!(ui.mail_list_scroll.area.height > 0);
        let reader_area = ui.mail_read_scroll.area;
        let list_area = ui.mail_list_scroll.area;
        let nav_area = ui.nav_area;

        // Clicking inside Reading pane focuses Focus::Reader; empty List/Nav focus List/Nav
        ui.set_focus(Focus::List);
        action::dispatch(
            Action::Click {
                column: reader_area.x + 2,
                row: reader_area.bottom().saturating_sub(1),
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus(), Focus::Reader);

        action::dispatch(
            Action::Click {
                column: nav_area.x + 1,
                row: nav_area.bottom().saturating_sub(1),
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus(), Focus::Nav);

        action::dispatch(
            Action::Click {
                column: list_area.x + 1,
                row: list_area.bottom().saturating_sub(1),
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus(), Focus::List);

        // Switch to Medium fullscreen -> plan.list is None -> stale mail_list_scroll.area MUST be cleared!
        ui.medium_reader_fullscreen = true;
        let mut medium = Terminal::new(TestBackend::new(100, 24)).unwrap();
        medium
            .draw(|f| {
                let _ = draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert_eq!(ui.mail_list_scroll.area, Rect::default());
        assert!(ui.mail_read_scroll.area.height > 0);

        // Wheel scroll over upper reader area scrolls mail_read_scroll instead of changing mail list selection
        let rx = ui.mail_read_scroll.area.x + 2;
        let ry = ui.mail_read_scroll.area.y + 1;
        action::dispatch(
            Action::MouseScroll {
                delta: 3,
                column: rx,
                row: ry,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(app.active_mail_id.as_deref(), Some("m1"));
        assert!(ui.mail_read_scroll.offset > 0);
    }

    #[test]
    fn overlay_scroll_windowing_for_settings_context_menu_event_form_help_and_compose() {
        use crate::ui::action::Action;
        use crate::ui::overlays::context_menu::{ContextMenu, ContextTarget};
        use crate::ui::overlays::event_form::EventFormOverlay;
        use crate::ui::overlays::mail_compose::MailComposeOverlay;
        use crate::ui::overlays::settings::SettingsState;
        use crate::ui::overlays::Overlay;
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_ui_test_harness();
        let mut short = Terminal::new(TestBackend::new(80, 9)).unwrap();

        // Settings scroll window keeps item 11 visible & clickable in short terminal
        let settings = SettingsState { selected: 11 };
        ui.push_overlay(Overlay::Settings(settings));
        short
            .draw(|f| {
                let _ = draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(ui
            .hitmap
            .region_of(HitTarget::OverlayItem { index: 11 })
            .is_some());
        ui.close_top_overlay();

        // ContextMenu scroll window keeps last item visible & clickable in short terminal
        let mut menu = ContextMenu::build(ContextTarget::Global, (0, 0), &app, &ui);
        let last_idx = menu.items.len() - 1;
        menu.selected = last_idx;
        ui.push_overlay(Overlay::ContextMenu(menu));
        short
            .draw(|f| {
                let _ = draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(ui
            .hitmap
            .region_of(HitTarget::OverlayItem { index: last_idx })
            .is_some());
        ui.close_top_overlay();

        // EventForm scroll window keeps field 6 visible + buttons pinned at bottom in short terminal
        let mut form = EventFormOverlay::new();
        form.field = 6;
        ui.push_overlay(Overlay::EventForm(form));
        short
            .draw(|f| {
                let _ = draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(ui
            .hitmap
            .region_of(HitTarget::OverlayItem { index: 6 })
            .is_some());
        assert!(ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::EventFormCancel))
            .is_some());
        ui.close_top_overlay();

        // HelpOverlay scrolls down with MouseScroll in short terminal
        ui.push_overlay(Overlay::help());
        short
            .draw(|f| {
                let _ = draw(f, &mut app, &mut ui);
            })
            .unwrap();
        action::dispatch(
            Action::MouseScroll {
                delta: 3,
                column: 10,
                row: 4,
            },
            &mut app,
            &mut ui,
        );
        if let Some(Overlay::Help(h)) = ui.overlays.last() {
            assert!(h.max_scroll > 0);
            assert!(h.scroll > 0);
        } else {
            panic!("expected Help overlay");
        }
        ui.close_top_overlay();

        // MailComposeOverlay body scrolls up with MouseScroll
        let mut compose = MailComposeOverlay::new();
        compose.field = 4;
        compose.body.load_text(
            &(0..30)
                .map(|i| format!("Compose line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        ui.push_overlay(Overlay::mail_compose(compose));
        let mut medium = Terminal::new(TestBackend::new(80, 18)).unwrap();
        medium
            .draw(|f| {
                let _ = draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let before_off = match ui.overlays.last() {
            Some(Overlay::MailCompose(c)) => c.body.scroll_offset,
            _ => unreachable!(),
        };
        assert!(before_off > 0);
        action::dispatch(
            Action::MouseScroll {
                delta: -3,
                column: 20,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        let after_off = match ui.overlays.last() {
            Some(Overlay::MailCompose(c)) => c.body.scroll_offset,
            _ => unreachable!(),
        };
        assert!(after_off < before_off, "compose body wheel-up must scroll offset up");
    }
}
