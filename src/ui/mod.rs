//! Root UI module: 3-tier responsive layout, rendering pipeline, and shared helpers.

pub mod action;
pub mod app;
pub mod chat_view;
pub mod composer;
pub mod event;
pub mod hitmap;
pub mod keymap;
pub mod nav_pane;
pub mod notification;
pub mod overlays;
pub mod theme;
pub mod thread_view;
pub mod toast;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::app::{AppState, NarrowPane, UiState};
use self::hitmap::{ButtonId, HitTarget};
use self::toast::ToastKind;

/// 3-tier responsive terminal width modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// >= 120 columns: Nav rail + Nav pane + Chat + optional Right Thread Drawer.
    Wide,
    /// 80..=119 columns: Nav pane + Main (Chat or Thread drawer split/swap).
    Medium,
    /// < 80 columns: Single active pane (`NarrowPane`) with compact top bar.
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

#[derive(Debug, Clone, Default)]
pub struct LayoutPlan {
    pub mode: Option<LayoutMode>,
    pub compact_bar: Option<Rect>,
    pub rail: Option<Rect>,
    pub nav: Option<Rect>,
    pub chat: Option<Rect>,
    pub thread: Option<Rect>,
    pub composer: Option<Rect>,
    pub status: Rect,
}

pub fn compute_layout(
    area: Rect,
    show_nav_rail: bool,
    thread_open: bool,
    auto_thread_pane: bool,
    narrow_pane: NarrowPane,
    composer_height: u16,
) -> LayoutPlan {
    let mode = layout_mode(area.width);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let status = rows[1];

    let comp_h = composer_height.clamp(3, (body.height / 2).max(3));

    match mode {
        LayoutMode::Wide => {
            let rail_w = if show_nav_rail { 6 } else { 0 };
            let nav_w = 28u16;
            let show_thread = thread_open && auto_thread_pane || thread_open;
            let thread_w = if show_thread {
                ((body.width.saturating_sub(rail_w + nav_w)) * 42 / 100).clamp(36, 52)
            } else {
                0
            };

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(rail_w),
                    Constraint::Length(nav_w),
                    Constraint::Min(30),
                    Constraint::Length(thread_w),
                ])
                .split(body);

            let center_rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                .split(cols[2]);

            LayoutPlan {
                mode: Some(mode),
                compact_bar: None,
                rail: (rail_w > 0).then_some(cols[0]),
                nav: Some(cols[1]),
                chat: Some(center_rows[0]),
                thread: (thread_w > 0).then_some(cols[3]),
                composer: Some(center_rows[1]),
                status,
            }
        }
        LayoutMode::Medium => {
            let rail_w = if show_nav_rail && area.width >= 96 { 6 } else { 0 };
            let nav_w = 24u16;
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(rail_w),
                    Constraint::Length(nav_w),
                    Constraint::Min(28),
                ])
                .split(body);

            let main_rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                .split(cols[2]);

            let (chat, thread) = if thread_open {
                (None, Some(main_rows[0]))
            } else {
                (Some(main_rows[0]), None)
            };

            LayoutPlan {
                mode: Some(mode),
                compact_bar: None,
                rail: (rail_w > 0).then_some(cols[0]),
                nav: Some(cols[1]),
                chat,
                thread,
                composer: Some(main_rows[1]),
                status,
            }
        }
        LayoutMode::Narrow => {
            let v = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(body);
            let compact_bar = Some(v[0]);
            let content = v[1];

            match narrow_pane {
                NarrowPane::List => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    rail: None,
                    nav: Some(content),
                    chat: None,
                    thread: None,
                    composer: None,
                    status,
                },
                NarrowPane::Chat => {
                    let split = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                        .split(content);
                    LayoutPlan {
                        mode: Some(mode),
                        compact_bar,
                        rail: None,
                        nav: None,
                        chat: Some(split[0]),
                        thread: None,
                        composer: Some(split[1]),
                        status,
                    }
                }
                NarrowPane::Thread => {
                    let split = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(4), Constraint::Length(comp_h)])
                        .split(content);
                    LayoutPlan {
                        mode: Some(mode),
                        compact_bar,
                        rail: None,
                        nav: None,
                        chat: None,
                        thread: Some(split[0]),
                        composer: Some(split[1]),
                        status,
                    }
                }
            }
        }
    }
}

/// Truncates `text` to fit within `max_width` terminal columns, appending `…` if cut.
pub fn truncate_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw >= max_width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Renders one complete frame, rebuilding `ui.hitmap` cleanly from scratch.
/// Returns the composer terminal cursor position `(x, y)` if focused.
pub fn draw(frame: &mut Frame, app: &mut AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.hitmap.clear();

    let row_count = nav_pane::build_nav_rows(app, ui).len();
    if row_count == 0 {
        ui.nav_selected = 0;
    } else if ui.nav_selected >= row_count {
        ui.nav_selected = row_count - 1;
    }

    let thread_open = ui.thread_drawer_open && app.active_thread.is_some();
    let show_subject = ui.show_subject
        && app
            .active_target
            .as_ref()
            .is_some_and(|t| t.is_channel())
        && !(thread_open && ui.composer_target_thread);
    let comp_h = ui.composer.desired_height(show_subject);
    let plan = compute_layout(
        area,
        ui.show_nav_rail,
        thread_open,
        ui.auto_thread_pane,
        ui.narrow_pane,
        comp_h,
    );

    if let Some(bar_area) = plan.compact_bar {
        draw_compact_bar(frame, bar_area, app, ui);
    }

    if let Some(rail_area) = plan.rail {
        draw_rail(frame, rail_area, app, ui);
    }

    let mut hitmap = std::mem::take(&mut ui.hitmap);

    if let Some(nav_area) = plan.nav {
        nav_pane::draw(frame, nav_area, app, ui, &mut hitmap);
    }

    if let Some(chat_area) = plan.chat {
        chat_view::draw(frame, chat_area, app, ui, &mut hitmap);
    }

    if let Some(thread_area) = plan.thread {
        thread_view::draw(frame, thread_area, app, ui, &mut hitmap);
    }

    let mut cursor_pos = None;
    if let Some(comp_area) = plan.composer {
        cursor_pos = composer::draw(frame, comp_area, app, ui, &mut hitmap);
    }

    draw_status_bar(frame, plan.status, app, ui);
    draw_toasts(frame, area, ui);

    for overlay in &ui.overlays {
        overlay.draw(frame, area, app, ui, &mut hitmap);
    }

    ui.hitmap = hitmap;
    cursor_pos
}

fn draw_rail(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(ui.theme.border_style(false));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height < 4 {
        return;
    }

    let (presence_dot, presence_color) = ui
        .theme
        .presence_indicator(app.my_presence.as_ref().map(|p| p.availability.as_str()));

    let items: [(&str, HitTarget, Option<ratatui::style::Color>); 7] = [
        ("💬", HitTarget::NavTab(0), None),
        ("👥", HitTarget::NavTab(1), None),
        ("🔔", HitTarget::NavTab(2), None),
        ("＋", HitTarget::Button(ButtonId::NewChat), None),
        ("🔍", HitTarget::Button(ButtonId::Search), None),
        (
            presence_dot,
            HitTarget::Button(ButtonId::Presence),
            Some(presence_color),
        ),
        ("⚙", HitTarget::Button(ButtonId::Settings), None),
    ];

    for (idx, (icon, target, custom_fg)) in items.iter().enumerate() {
        if idx as u16 >= inner.height {
            break;
        }
        let row_rect = Rect {
            x: inner.x,
            y: inner.y + idx as u16,
            width: inner.width,
            height: 1,
        };
        let active = matches!(target, HitTarget::NavTab(t) if *t == ui.active_nav_tab);
        let style = if active {
            ui.theme.selection()
        } else if let Some(fg) = custom_fg {
            Style::default().fg(*fg)
        } else {
            Style::default().fg(ui.theme.fg)
        };
        frame.render_widget(Paragraph::new(format!(" {icon} ")).style(style), row_rect);
        ui.hitmap.add(row_rect, *target);
    }
}

fn draw_compact_bar(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState) {
    let header_style = Style::default().bg(ui.theme.selection_bg).fg(ui.theme.fg);
    frame.render_widget(Paragraph::new("").style(header_style), area);

    let mut x = area.x;
    let buttons: [(&str, HitTarget, bool); 4] = [
        (
            " 목록 ",
            HitTarget::Button(ButtonId::NarrowToggle),
            ui.narrow_pane == NarrowPane::List,
        ),
        (
            " 대화 ",
            HitTarget::Button(ButtonId::NarrowToggle),
            ui.narrow_pane == NarrowPane::Chat,
        ),
        (
            " 스레드 ",
            HitTarget::Button(ButtonId::NarrowToggle),
            ui.narrow_pane == NarrowPane::Thread,
        ),
        (" 검색 ", HitTarget::Button(ButtonId::Search), false),
    ];
    for (label, target, active) in buttons {
        let w = label.width() as u16;
        if x + w > area.right() {
            break;
        }
        let r = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        let st = if active {
            ui.theme
                .accent_style()
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            header_style
        };
        frame.render_widget(Paragraph::new(label).style(st), r);
        ui.hitmap.add(r, target);
        x += w + 1;
    }

    if x + 4 < area.right() {
        let title = truncate_width(&app.active_title(), (area.right() - x) as usize);
        frame.render_widget(
            Paragraph::new(title).style(ui.theme.muted_style()),
            Rect {
                x,
                y: area.y,
                width: area.right() - x,
                height: 1,
            },
        );
    }
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bar_style = Style::default().bg(ui.theme.selection_bg).fg(ui.theme.fg);
    let sync_tag = if app.syncing {
        format!("⟳ {}", app.sync_summary)
    } else {
        format!("✓ {}", app.sync_summary)
    };
    let (dot, dot_color) = ui
        .theme
        .presence_indicator(app.my_presence.as_ref().map(|p| p.availability.as_str()));
    let avail = app
        .my_presence
        .as_ref()
        .map(|p| p.availability.as_str())
        .unwrap_or("Offline");

    let focus_label = match ui.focus() {
        app::Focus::Nav => "NAV",
        app::Focus::Chat => "CHAT",
        app::Focus::Thread => "THREAD",
        app::Focus::Composer => "COMPOSE",
        app::Focus::Overlay => "MODAL",
    };

    let task_note = ui
        .tasks
        .first_running()
        .map(|t| format!(" | ⏳ {}", t.display()))
        .unwrap_or_default();

    let left = format!(" [{focus_label}] {avail} | {sync_tag}{task_note}");
    let right = " Ctrl+K 팔레트  Ctrl+F 검색  ? 도움말 ";

    let right_w = right.width() as u16;
    let left_w = area.width.saturating_sub(right_w + 3) as usize;
    let left_trimmed = truncate_width(&left, left_w);
    let pad = area
        .width
        .saturating_sub(left_trimmed.width() as u16 + right_w + 2);

    let line = Line::from(vec![
        Span::styled(format!(" {dot}"), bar_style.fg(dot_color)),
        Span::styled(left_trimmed, bar_style),
        Span::styled(" ".repeat(pad as usize), bar_style),
        Span::styled(right, bar_style.fg(ui.theme.muted)),
    ]);
    frame.render_widget(Paragraph::new(line).style(bar_style), area);
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
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(truncate_width(&toast.message, width.saturating_sub(2) as usize)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color)),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_layout_breakpoints() {
        assert_eq!(layout_mode(140), LayoutMode::Wide);
        assert_eq!(layout_mode(120), LayoutMode::Wide);
        assert_eq!(layout_mode(119), LayoutMode::Medium);
        assert_eq!(layout_mode(80), LayoutMode::Medium);
        assert_eq!(layout_mode(79), LayoutMode::Narrow);
    }

    #[test]
    fn wide_layout_includes_thread_drawer_when_open() {
        let area = Rect::new(0, 0, 140, 40);
        let closed = compute_layout(area, true, false, true, NarrowPane::Chat, 4);
        assert!(closed.rail.is_some());
        assert!(closed.nav.is_some());
        assert!(closed.chat.is_some());
        assert!(closed.thread.is_none());

        let open = compute_layout(area, true, true, true, NarrowPane::Chat, 4);
        assert!(open.chat.is_some());
        assert!(open.thread.is_some());
        assert!(open.composer.is_some());
    }

    #[test]
    fn narrow_layout_switches_single_pane() {
        let area = Rect::new(0, 0, 72, 24);
        let list = compute_layout(area, true, true, true, NarrowPane::List, 3);
        assert!(list.nav.is_some());
        assert!(list.chat.is_none());
        assert!(list.composer.is_none());

        let chat = compute_layout(area, true, true, true, NarrowPane::Chat, 3);
        assert!(chat.nav.is_none());
        assert!(chat.chat.is_some());
        assert!(chat.composer.is_some());

        let thread = compute_layout(area, true, true, true, NarrowPane::Thread, 3);
        assert!(thread.thread.is_some());
        assert!(thread.composer.is_some());
    }

    #[test]
    fn truncate_width_handles_multibyte_korean() {
        assert_eq!(truncate_width("안녕하세요", 4), "안…");
        assert_eq!(truncate_width("abc", 5), "abc");
    }
}
