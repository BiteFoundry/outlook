//! Tab 3 (`Tasks` / Microsoft To Do):
//! - Middle pane: Task list with `[ ]` / `[x]` checkboxes (`Space` / `Enter` toggles completion
//!   optimistically + `graph.set_todo_task_completed`), `★` high-importance indicator, due date badge.
//! - Right pane: Task details & notes, `n` quick-add task input, `Delete` remove task.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::TodoTask,
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        mail_view::{render_scrolled_lines, truncate, RenderedLine},
    },
};

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
    ui.todo_list_scroll.area = list_area;
    if list_area.height == 0 {
        return;
    }

    let tasks = app.visible_todo_tasks(ui);
    let theme = &ui.theme;
    if tasks.is_empty() {
        ui.todo_list_scroll.total_lines = 0;
        let msg = if ui.todo_hide_completed && !app.todo_tasks.is_empty() {
            " All tasks completed ([Open] filter active)"
        } else {
            " No tasks in this list (n to add task)"
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

    let total = tasks.len();
    let selected_idx = ui
        .todo_list_scroll
        .selected_idx
        .unwrap_or(0)
        .min(total.saturating_sub(1));
    ui.todo_list_scroll.selected_idx = Some(selected_idx);

    let height = list_area.height as usize;
    let mut skip = ui.todo_list_scroll.offset.min(total.saturating_sub(1));
    if selected_idx < skip {
        skip = selected_idx;
    } else if selected_idx >= skip + height {
        skip = selected_idx + 1 - height;
    }
    ui.todo_list_scroll.offset = skip;
    ui.todo_list_scroll.total_lines = total;

    let list_focused = ui.focus() == Focus::List;
    let width = list_area.width as usize;

    for (slot, (idx, task)) in tasks.iter().enumerate().skip(skip).take(height).enumerate() {
        let y = list_area.y + slot as u16;
        let is_selected = idx == selected_idx;
        let is_active = app.active_todo_task_id.as_deref() == Some(task.id.as_str());
        let bg = if is_selected && list_focused {
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

        let gutter = if is_selected && list_focused { "▎" } else { " " };
        let completed = task.is_completed();
        let checkbox = if completed { "[x]" } else { "[ ]" };
        let cb_style = if completed {
            bg.fg(theme.success).add_modifier(Modifier::BOLD)
        } else {
            bg.fg(theme.accent)
        };
        let star = if task.importance.eq_ignore_ascii_case("high") {
            "★ "
        } else {
            ""
        };
        let due_badge = task
            .due_date_iso
            .as_deref()
            .and_then(|d| d.get(..10))
            .map(|d| format!(" Due {d}"))
            .unwrap_or_default();
        let due_w = UnicodeWidthStr::width(due_badge.as_str());
        let star_w = UnicodeWidthStr::width(star);
        let title_max = width.saturating_sub(2 + 4 + star_w + due_w + 1);
        let title = truncate(&task.title, title_max);
        let used = 2 + 4 + star_w + UnicodeWidthStr::width(title.as_str()) + due_w;
        let pad = width.saturating_sub(used + 1);

        let title_style = if completed {
            bg.fg(theme.fg_muted).add_modifier(Modifier::CROSSED_OUT)
        } else {
            bg.fg(theme.fg_primary)
        };

        let line = Line::from(vec![
            Span::styled(gutter, bg.fg(theme.accent)),
            Span::styled(format!("{checkbox} "), cb_style),
            Span::styled(star, bg.fg(theme.warning)),
            Span::styled(title, title_style),
            Span::styled(" ".repeat(pad), bg),
            Span::styled(format!("{due_badge} "), bg.fg(theme.fg_muted)),
        ]);
        let row_rect = Rect::new(list_area.x, y, list_area.width, 1);
        frame.render_widget(Paragraph::new(line).style(bg), row_rect);
        hitmap.add(row_rect, HitTarget::ListItem(idx));
        hitmap.add_line(list_area.x + 1, y, 3, HitTarget::TodoCheckbox(idx));
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

    let list_name = app
        .active_todo_list_id
        .as_deref()
        .and_then(|id| app.todo_lists.iter().find(|l| l.id == id))
        .map(|l| l.display_name.as_str())
        .unwrap_or("Tasks");
    let open_count = app.todo_tasks.iter().filter(|t| !t.is_completed()).count();
    let title = format!(" {list_name} ({open_count} open)");

    let btn = "[+ New Task]";
    let bw = UnicodeWidthStr::width(btn) as u16;
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate(&title, area.width.saturating_sub(bw + 2) as usize),
            bar_style.add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x, area.y, area.width.saturating_sub(bw + 1), 1),
    );
    if area.width >= bw + 1 {
        let r = Rect::new(area.right().saturating_sub(bw + 1), area.y, bw, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(btn, bar_style.fg(theme.accent))),
            r,
        );
        hitmap.add(r, HitTarget::Button(ButtonId::NewTodoTask));
    }
}

pub fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
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

    draw_detail_toolbar(frame, area, ui, hitmap);

    let body_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.todo_detail_scroll.area = body_area;
    if body_area.height == 0 {
        return;
    }

    let Some(task) = app.active_todo_task() else {
        ui.todo_detail_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new(" Select a task (Space/Enter toggle · n add · Del delete)")
                .style(ui.theme.muted_style()),
            body_area,
        );
        return;
    };

    let width = body_area.width as usize;
    let lines = build_todo_detail_lines(task, &ui.theme, width);
    let height = body_area.height as usize;
    ui.todo_detail_scroll.total_lines = lines.len();
    let max_off = lines.len().saturating_sub(height);
    ui.todo_detail_scroll.offset = ui.todo_detail_scroll.offset.min(max_off);

    render_scrolled_lines(
        frame,
        body_area,
        &lines,
        ui.todo_detail_scroll.offset,
        height,
        hitmap,
    );
}

fn draw_detail_toolbar(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
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

    let toolbar = [
        ("[n New Task]", ButtonId::NewTodoTask),
        ("[N New List]", ButtonId::NewTodoList),
        ("[Del Delete]", ButtonId::DeleteTodoTask),
    ];
    let mut x = area.x + 1;
    for (label, btn_id) in toolbar {
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

fn build_todo_detail_lines(
    task: &TodoTask,
    theme: &crate::ui::theme::Theme,
    width: usize,
) -> Vec<RenderedLine> {
    let mut out = Vec::new();
    let check = if task.is_completed() { "[x]" } else { "[ ]" };
    out.push(RenderedLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{check} "),
            Style::default()
                .fg(if task.is_completed() { theme.success } else { theme.accent })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate(&task.title, width.saturating_sub(8)),
            theme.subject_style().add_modifier(Modifier::BOLD),
        ),
    ])));

    out.push(RenderedLine::plain(Line::from(Span::styled(
        format!("  Status:     {}", task.status),
        theme.muted_style(),
    ))));
    out.push(RenderedLine::plain(Line::from(Span::styled(
        format!("  Importance: {}", task.importance),
        if task.importance.eq_ignore_ascii_case("high") {
            Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style()
        },
    ))));

    if let Some(ref due) = task.due_date_iso.as_deref().and_then(|d| d.get(..10)) {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("  Due Date:   {due}"),
            theme.accent_style(),
        ))));
    }
    if let Some(ref rem) = task.reminder_iso.as_deref().and_then(|d| d.get(..16)) {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("  Reminder:   {rem}"),
            theme.muted_style(),
        ))));
    }

    out.push(RenderedLine::plain(Line::from(Span::styled(
        format!("  {}", "─".repeat(width.saturating_sub(4).max(4))),
        theme.subtle_style(),
    ))));

    if let Some(ref notes) = task.body_plain.as_deref().filter(|s| !s.trim().is_empty()) {
        for line_str in notes.lines() {
            out.push(RenderedLine::plain(Line::from(vec![
                Span::raw("  "),
                Span::raw(truncate(line_str, width.saturating_sub(4))),
            ])));
        }
    } else {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "  (No task notes)",
            theme.muted_style(),
        ))));
    }

    out
}
