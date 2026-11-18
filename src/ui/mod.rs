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

    let mut hitmap = HitMap::default();
    let mut cursor_pos = None;

    if let Some(bar_area) = plan.compact_bar {
        draw_compact_bar(frame, bar_area, app, ui, &mut hitmap);
    }

    if let Some(nav_area) = plan.nav {
        nav_pane::draw(frame, nav_area, app, ui, &mut hitmap);
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

    let mut overlays = std::mem::take(&mut ui.overlays);
    let last_idx = overlays.len().saturating_sub(1);
    for (idx, overlay) in overlays.iter_mut().enumerate() {
        match overlay {
            overlays::Overlay::ImagePreview(preview) => {
                preview.draw_sync(frame, area, app, ui, &mut hitmap);
                if idx == last_idx {
                    cursor_pos = None;
                }
            }
            other => {
                ui.pending_pixel_graphic = None;
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

    // Right-aligned Jump & New buttons if room remains
    let right_buttons: &[(&str, ButtonId)] = &[
        ("[Ctrl+K]", ButtonId::CommandPalette),
        (" ", ButtonId::CommandPalette),
        ("[+New]", match ui.active_nav_tab {
            TAB_CALENDAR => ButtonId::NewEvent,
            TAB_TASKS => ButtonId::NewTodoTask,
            _ => ButtonId::NewMail,
        }),
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
        for (idx, (label, btn)) in right_buttons.iter().enumerate() {
            let w = label.width() as u16;
            spans.push(Span::styled((*label).to_string(), btn_style));
            if idx != 1 {
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
            ("1-3 Tab · j/k Event · n New · a/t/d RSVP · J Join · ", None),
            (density_hint.as_str(), Some(ButtonId::DensityToggle)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        (_, TAB_TASKS) => vec![
            ("1-3 Tab · Space Done · n Task · N List · x Del · ", None),
            (density_hint.as_str(), Some(ButtonId::DensityToggle)),
            (" · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
        _ => vec![
            ("1-3 Tab · j/k Move · r Reply · c Compose · a Archive · u Read · ", None),
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
            (" · ", None),
            ("?", Some(ButtonId::Help)),
            (" ", None),
        ],
    };

    let compact_segments: Vec<(&str, Option<ButtonId>)> = match focus {
        Focus::Overlay => vec![("Esc Close ", None)],
        _ => vec![
            ("Ctrl+K Jump", Some(ButtonId::CommandPalette)),
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

    let chosen_right: &[(&str, Option<ButtonId>)] =
        if full_left_w + 2 + full_right_w <= area.width {
            &full_segments
        } else if badge_w + 14 + full_right_w <= area.width {
            &full_segments
        } else if badge_w + 10 + compact_right_w <= area.width {
            &compact_segments
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
}
