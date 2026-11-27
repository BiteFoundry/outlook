//! Tab 3 (`Tasks` / Microsoft To Do):
//! - Middle pane: Task list with `[ ]` / `[x]` checkboxes (`Space` / `Enter` toggles completion
//!   optimistically + `graph.set_todo_task_completed`), `★` high-importance indicator, due date badge.
//! - Right pane: Task details & notes, `n` quick-add task input, `Delete` remove task.

use bitefoundry_html::layout::{wrap_styled_spans, WrapPrefix};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
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
            Paragraph::new(msg)
                .style(theme.muted_style())
                .wrap(Wrap { trim: false }),
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
    if area.width > bw {
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
                .style(ui.theme.muted_style())
                .wrap(Wrap { trim: false }),
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

pub fn build_todo_detail_lines(
    task: &TodoTask,
    theme: &crate::ui::theme::Theme,
    width: usize,
) -> Vec<RenderedLine> {
    let mut out = Vec::new();
    let p2 = [Span::raw("  ")];
    let p4 = [Span::raw("    ")];
    let hanging = WrapPrefix {
        first: &p2,
        cont: &p4,
    };
    let plain_indent = WrapPrefix {
        first: &p2,
        cont: &p2,
    };

    let check = if task.is_completed() { "[x]" } else { "[ ]" };
    let check_style = Style::default()
        .fg(if task.is_completed() {
            theme.success
        } else {
            theme.accent
        })
        .add_modifier(Modifier::BOLD);
    let title_first = [Span::raw("  "), Span::styled(format!("{check} "), check_style)];
    let title_cont = [Span::raw("      ")];
    for line in wrap_styled_spans(
        &[Span::styled(
            task.title.clone(),
            theme.subject_style().add_modifier(Modifier::BOLD),
        )],
        width,
        WrapPrefix {
            first: &title_first,
            cont: &title_cont,
        },
    ) {
        out.push(RenderedLine::plain(line));
    }

    for line in wrap_styled_spans(
        &[Span::styled(
            format!("Status:     {}", task.status),
            theme.muted_style(),
        )],
        width,
        hanging,
    ) {
        out.push(RenderedLine::plain(line));
    }

    let importance_style = if task.importance.eq_ignore_ascii_case("high") {
        Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.muted_style()
    };
    for line in wrap_styled_spans(
        &[Span::styled(
            format!("Importance: {}", task.importance),
            importance_style,
        )],
        width,
        hanging,
    ) {
        out.push(RenderedLine::plain(line));
    }

    if let Some(ref due) = task.due_date_iso.as_deref().and_then(|d| d.get(..10)) {
        for line in wrap_styled_spans(
            &[Span::styled(format!("Due Date:   {due}"), theme.accent_style())],
            width,
            hanging,
        ) {
            out.push(RenderedLine::plain(line));
        }
    }
    if let Some(ref rem) = task.reminder_iso.as_deref().and_then(|d| d.get(..16)) {
        for line in wrap_styled_spans(
            &[Span::styled(format!("Reminder:   {rem}"), theme.muted_style())],
            width,
            hanging,
        ) {
            out.push(RenderedLine::plain(line));
        }
    }

    let rule = if width > 4 {
        format!("  {}", "─".repeat(width - 4))
    } else {
        "─".repeat(width)
    };
    out.push(RenderedLine::plain(Line::from(Span::styled(
        rule,
        theme.subtle_style(),
    ))));

    if let Some(notes) = task.body_plain.as_deref().filter(|s| !s.trim().is_empty()) {
        let body = notes.trim_end_matches(['\r', '\n']).to_string();
        for line in wrap_styled_spans(&[Span::raw(body)], width, plain_indent) {
            out.push(RenderedLine::plain(line));
        }
    } else {
        for line in wrap_styled_spans(
            &[Span::styled("(No task notes)", theme.muted_style())],
            width,
            plain_indent,
        ) {
            out.push(RenderedLine::plain(line));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_detail_wraps_150_char_title_and_notes_within_42_cols() {
        let title = "분기말 릴리스 전 전체 크레이트 텍스트 래핑 불변식 점검 및 통합 회귀 테스트 완료 후 배포 승인 절차 확인하기 — 한글과 영문 혼합 제목으로 백오십 글자 이상이 42컬럼 상세 패널에서도 절대 잘리지 않고 행잉 인덴트로 래핑되는지 검증하는 태스크 제목입니다. 추가 확인 항목까지 전부 포함합니다.";
        let notes = "첫 번째 체크 항목: 캘린더 일정 상세와 투두 노트에서 단일행 절단 호출을 모두 제거한다.\n두 번째 체크 항목: 메일 본문의 코드 블록과 다열 HTML 테이블, 첨부 칩 바 및 미팅 초대 RSVP 버튼이 좁은 화면에서도 무손실로 래핑되고 모든 텍스트가 온전히 보존되는지 확인한다! 마지막 문장까지 누락 없이 출력되어야 합니다.";
        assert!(title.chars().count() >= 150);
        assert!(notes.chars().count() >= 150);

        let task = TodoTask {
            id: "t1".into(),
            list_id: "l1".into(),
            title: title.to_string(),
            status: "completed".into(),
            importance: "high".into(),
            due_date_iso: Some("2026-11-30T09:00:00Z".into()),
            reminder_iso: Some("2026-11-29T18:00:00Z".into()),
            body_plain: Some(notes.to_string()),
            completed_at: None,
            created_at: None,
            updated_at: None,
        };

        let theme = crate::ui::theme::Theme::default();
        let width = 42usize;
        let lines = build_todo_detail_lines(&task, &theme, width);

        for (idx, rl) in lines.iter().enumerate() {
            assert!(
                rl.line.width() <= width,
                "todo detail line {idx} exceeded width {width}: width={} text={:?}",
                rl.line.width(),
                rl.line.to_string()
            );
        }

        // First line starts with `"  [x] "` and continuation title lines use 6-space hanging indent.
        let first_text = lines[0].line.to_string();
        assert!(first_text.starts_with("  [x] "), "got: {first_text:?}");
        let title_lines: Vec<String> = lines
            .iter()
            .take_while(|rl| !rl.line.to_string().contains("Status:"))
            .map(|rl| rl.line.to_string())
            .collect();
        assert!(title_lines.len() >= 3);
        for cont in &title_lines[1..] {
            assert!(
                cont.starts_with("      "),
                "expected 6-space hanging indent on title continuation, got: {cont:?}"
            );
        }

        let norm = |s: &str| s.split_whitespace().collect::<String>();
        let rendered_title = title_lines
            .iter()
            .enumerate()
            .map(|(i, l)| if i == 0 { l.trim_start_matches("  [x] ") } else { l.trim() })
            .collect::<String>();
        assert_eq!(norm(&rendered_title), norm(title));

        let note_lines: Vec<String> = lines
            .iter()
            .skip_while(|rl| !rl.line.to_string().contains('─'))
            .skip(1)
            .map(|rl| rl.line.to_string())
            .collect();
        assert!(
            note_lines.iter().any(|l| l.contains("첫 번째 체크 항목"))
                && note_lines.iter().any(|l| l.contains("두 번째 체크 항목")),
            "expected explicit newline in notes to keep both items on distinct lines: {note_lines:?}"
        );
        let rendered_notes: String = note_lines.iter().map(|l| l.trim()).collect();
        assert_eq!(norm(&rendered_notes), norm(notes));
    }
}
