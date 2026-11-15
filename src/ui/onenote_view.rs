//! Native Microsoft OneNote TUI Renderer (`src/ui/onenote_view.rs`):
//! - Row 0: Colored Horizontal Section Ribbon Bar (`[ ▌섹션1 ] [ ▌섹션2 ] ... [+섹션 S]`)
//! - Row 1: Clickable OneNote Action Toolbar (`HitMap`)
//! - Split Body:
//!   - Wide (`inner.width >= 76`): Left Page List sub-column (`26..=32` cols) with subpage indentation
//!     (`level 0` / `level 1` / `level >= 2`) + Right Rich Interactive Note Canvas
//!   - Narrow (`inner.width < 76`): Compact active-page selector bar + Rich Interactive Note Canvas
//! - Rich Block Rendering:
//!   - Interactive `☐`/`☑` Todo checkboxes (`HitTarget::OnenoteTodoToggle`) & colored tags (`★`, `❓`, `❗`, Highlight)
//!   - Headings (`h1`..`h6`) with subtle underline on `h1`/`h2`
//!   - Nested ordered/unordered lists & quotes
//!   - Crisp Unicode box-drawn tables (`┌──┬──┐` / `│  │  │` / `├──┼──┤` / `└──┴──┘`)
//!   - Bordered code blocks with language badge
//!   - Inline image chips + half-block terminal thumbnails (`img_preview`) & `<object>` attachment chips

use std::collections::HashMap;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    img_preview::RenderedTerminalImage,
    models::{
        OnenoteBlock, OnenoteBlockKind, OnenotePageDetail, OnenotePageSummary, OnenoteSection,
        OnenoteTagKind, RichSpan,
    },
    ui::{
        app::{AppState, Focus, UiState},
        chat_view::{self, RenderedLine},
        hitmap::{ButtonId, HitMap, HitTarget},
        nav_pane::{onenote_badge_color, selection_offset, ONENOTE_BRAND_BG, ONENOTE_PURPLE},
        theme::Theme,
    },
};

const SPLIT_MIN_WIDTH: u16 = 76;

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    let theme = ui.theme.clone();
    let focused = ui.focus == Focus::Chat;

    let nb_name = app
        .active_onenote_notebook()
        .map(|nb| nb.display_name.as_str())
        .unwrap_or("Microsoft OneNote");
    let sec_name = app
        .active_onenote_section()
        .map(|s| s.display_name.as_str())
        .unwrap_or("섹션 미선택");
    let pane_title = format!(" 📓 {nb_name} › {sec_name} ");

    let title_style = if focused {
        Style::default()
            .fg(ONENOTE_PURPLE)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.title(false)
    };
    let border_style = if focused {
        Style::default().fg(ONENOTE_PURPLE)
    } else {
        theme.border_style(false)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            crate::ui::truncate_width(&pane_title, area.width.saturating_sub(2) as usize),
            title_style,
        )))
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Row 0: Colored Horizontal Section Ribbon Bar
    let ribbon_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let sections = app.sections_for_active_notebook();
    draw_section_ribbon(
        frame,
        ribbon_area,
        &sections,
        app.active_section_key.as_deref(),
        &theme,
        hitmap,
    );
    if inner.height == 1 {
        return;
    }

    // Row 1: Clickable Action Toolbar
    let toolbar_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    draw_action_toolbar(frame, toolbar_area, &theme, hitmap);
    if inner.height <= 2 {
        return;
    }

    // Split Body (Row 2..)
    let body_area = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(2),
    );

    if body_area.width >= SPLIT_MIN_WIDTH {
        let page_col_w = ((body_area.width as usize * 30) / 100).clamp(26, 32) as u16;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(page_col_w), Constraint::Min(30)])
            .split(body_area);

        draw_page_list_column(frame, cols[0], app, ui, &theme, hitmap);
        draw_note_canvas(frame, cols[1], app, ui, &theme, hitmap);
    } else {
        // Narrow mode (< 76 cols): compact active-page selector bar + full-width Note Canvas
        if body_area.height >= 3 {
            let selector_rect = Rect::new(body_area.x, body_area.y, body_area.width, 1);
            let canvas_rect = Rect::new(
                body_area.x,
                body_area.y + 1,
                body_area.width,
                body_area.height - 1,
            );
            draw_compact_page_bar(frame, selector_rect, app, &theme, hitmap);
            draw_note_canvas(frame, canvas_rect, app, ui, &theme, hitmap);
        } else {
            draw_note_canvas(frame, body_area, app, ui, &theme, hitmap);
        }
    }
}

/// Renders Row 0: `[ ▌섹션1 ] [ ▌섹션2 ] ... [+섹션 S]`.
pub fn draw_section_ribbon(
    frame: &mut Frame,
    area: Rect,
    sections: &[OnenoteSection],
    active_section_key: Option<&str>,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    if area.width == 0 {
        return;
    }

    let add_label = "[+섹션 S]";
    let add_w = UnicodeWidthStr::width(add_label) as u16;

    if sections.is_empty() {
        let hint = " 섹션이 없습니다 ";
        let hint_w = UnicodeWidthStr::width(hint) as u16;
        let mut spans = vec![Span::styled(hint, theme.muted_style())];
        if area.width >= hint_w + add_w + 1 {
            spans.push(Span::styled(
                add_label,
                Style::default()
                    .fg(ONENOTE_PURPLE)
                    .add_modifier(Modifier::BOLD),
            ));
            hitmap.add_line(
                area.x + hint_w,
                area.y,
                add_w,
                HitTarget::Button(ButtonId::OnenoteNewSection),
            );
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let active_idx = active_section_key
        .and_then(|k| sections.iter().position(|s| s.section_key == k))
        .unwrap_or(0);

    // Ensure the active section tab stays visible even when there are many sections
    let budget_for_tabs = area.width.saturating_sub(add_w + 2) as usize;
    let mut start_idx = 0usize;
    if active_idx > 0 {
        let mut needed = 0usize;
        for s in sections.iter().take(active_idx + 1).rev() {
            let tab_txt = format!(" ▌{} ", truncate_clean(&s.display_name, 16));
            let w = UnicodeWidthStr::width(tab_txt.as_str()) + 3;
            if needed + w > budget_for_tabs && needed > 0 {
                break;
            }
            needed += w;
            start_idx = sections
                .iter()
                .position(|cand| cand.section_key == s.section_key)
                .unwrap_or(0);
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut x = area.x;

    for (idx, sec) in sections.iter().enumerate().skip(start_idx) {
        let is_active = active_section_key == Some(sec.section_key.as_str());
        let short_name = truncate_clean(&sec.display_name, 16);
        let tab_w = (UnicodeWidthStr::width(short_name.as_str()) + 6) as u16; // `[ ▌name ]`
        if x.saturating_sub(area.x) + tab_w + add_w + 1 > area.width && idx > start_idx {
            break;
        }
        if x.saturating_sub(area.x) + tab_w > area.width {
            break;
        }

        let badge_color = onenote_badge_color(sec.color.as_deref(), &sec.section_key);
        if is_active {
            let active_bg = ONENOTE_BRAND_BG;
            spans.push(Span::styled(
                "[ ",
                Style::default()
                    .fg(Color::White)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "▌",
                Style::default()
                    .fg(badge_color)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("{short_name} ]"),
                Style::default()
                    .fg(Color::White)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("[ ", theme.muted_style()));
            spans.push(Span::styled("▌", Style::default().fg(badge_color)));
            spans.push(Span::styled(
                short_name,
                Style::default().fg(theme.fg),
            ));
            spans.push(Span::styled(" ]", theme.muted_style()));
        }
        spans.push(Span::raw(" "));
        hitmap.add_line(x, area.y, tab_w, HitTarget::OnenoteSectionTab(idx));
        x = x.saturating_add(tab_w + 1);
    }

    if x.saturating_sub(area.x) + add_w <= area.width {
        spans.push(Span::styled(
            add_label,
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ));
        hitmap.add_line(
            x,
            area.y,
            add_w,
            HitTarget::Button(ButtonId::OnenoteNewSection),
        );
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Renders Row 1: Action Toolbar.
fn draw_action_toolbar(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let buttons: [(&str, ButtonId); 10] = [
        ("[+새페이지 n]", ButtonId::OnenoteNewPage),
        ("[+하위페이지 N]", ButtonId::OnenoteNewSubpage),
        ("[블록추가 a]", ButtonId::OnenoteAppendBlock),
        ("[블록편집 e]", ButtonId::OnenoteEditBlock),
        ("[전체편집 E]", ButtonId::OnenoteEditFullPage),
        ("[체크토글 Space]", ButtonId::OnenoteToggleTodo),
        ("[🖼미리보기 v]", ButtonId::ImagePreview),
        ("[웹열기 o]", ButtonId::OnenoteOpenWeb),
        ("[새로고침 r]", ButtonId::OnenoteRefresh),
        ("[삭제 d]", ButtonId::OnenoteDeletePage),
    ];

    let mut spans = Vec::new();
    let mut x = area.x;
    let btn_style = Style::default()
        .fg(ONENOTE_PURPLE)
        .add_modifier(Modifier::BOLD);

    for &(label, btn_id) in &buttons {
        let w = UnicodeWidthStr::width(label) as u16;
        if x.saturating_sub(area.x) + w > area.width {
            break;
        }
        let style = if btn_id == ButtonId::OnenoteDeletePage {
            Style::default().fg(theme.error)
        } else {
            btn_style
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
        hitmap.add_line(x, area.y, w, HitTarget::Button(btn_id));
        x = x.saturating_add(w + 1);
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Formats a page row label with authentic OneNote subpage indentation (`level 0`, `1`, `2+`).
pub fn format_page_row_label(page: &OnenotePageSummary) -> (&'static str, String) {
    let title = if page.title.trim().is_empty() {
        "(제목 없는 페이지)".to_string()
    } else {
        page.title.trim().to_string()
    };
    match page.level {
        lvl if lvl <= 0 => {
            let prefix = if page.has_todo { "☐ " } else { "• " };
            (prefix, title)
        }
        1 => {
            let prefix = if page.has_todo { "  └─ ☐ " } else { "  └─ " };
            (prefix, title)
        }
        _ => {
            let prefix = if page.has_todo {
                "    └─ ☐ "
            } else {
                "    └─ "
            };
            (prefix, title)
        }
    }
}

fn draw_page_list_column(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    _ui: &UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let title = format!(" 📄 페이지 ({}) ", app.onenote_pages.len());
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .title(Line::from(Span::styled(
            crate::ui::truncate_width(&title, area.width.saturating_sub(1) as usize),
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        )))
        .border_style(theme.border_style(false));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.onenote_pages.is_empty() {
        let msg = if app.loading_onenote_pages {
            "페이지 목록 로드 중…"
        } else {
            "페이지 없음 (n 추가)"
        };
        frame.render_widget(
            Paragraph::new(crate::ui::truncate_width(msg, inner.width as usize))
                .style(theme.muted_style()),
            inner,
        );
        return;
    }

    let active_idx = app
        .active_page_key
        .as_deref()
        .and_then(|k| app.onenote_pages.iter().position(|p| p.page_key == k))
        .unwrap_or(0);
    let height = inner.height as usize;
    let offset = selection_offset(active_idx, height, app.onenote_pages.len());
    let width = inner.width as usize;

    let mut lines = Vec::with_capacity(height);
    for (idx, page) in app
        .onenote_pages
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
    {
        let is_active = app.active_page_key.as_deref() == Some(page.page_key.as_str());
        let (prefix, clean_title) = format_page_row_label(page);
        let avail_w = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(3);
        let body = truncate_clean(&clean_title, avail_w);
        let pad = width
            .saturating_sub(UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(body.as_str()));

        let row_style = if is_active {
            Style::default()
                .fg(Color::White)
                .bg(ONENOTE_BRAND_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let prefix_style = if is_active {
            row_style
        } else if page.has_todo {
            Style::default().fg(ONENOTE_PURPLE)
        } else {
            theme.muted_style()
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, prefix_style),
            Span::styled(body, row_style),
            Span::styled(" ".repeat(pad), row_style),
        ]));
        hitmap.add_line(
            inner.x,
            inner.y + (lines.len() as u16 - 1),
            inner.width,
            HitTarget::OnenotePage(idx),
        );
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_compact_page_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    if app.onenote_pages.is_empty() {
        frame.render_widget(
            Paragraph::new(" 페이지 없음 (n: 새 페이지)").style(theme.muted_style()),
            area,
        );
        return;
    }

    let total = app.onenote_pages.len();
    let idx = app
        .active_page_key
        .as_deref()
        .and_then(|k| app.onenote_pages.iter().position(|p| p.page_key == k))
        .unwrap_or(0);
    let prev_idx = (idx + total - 1) % total;
    let next_idx = (idx + 1) % total;

    let prev_btn = "[◀ K]";
    let next_btn = "[J ▶]";
    let prev_w = UnicodeWidthStr::width(prev_btn) as u16;
    let next_w = UnicodeWidthStr::width(next_btn) as u16;
    let mid_budget = area
        .width
        .saturating_sub(prev_w + next_w + 2)
        .max(6) as usize;
    let page_title = app
        .onenote_pages
        .get(idx)
        .map(|p| p.title.as_str())
        .unwrap_or("(제목 없음)");
    let mid_label = format!(" 📄 {}/{}: {} ", idx + 1, total, page_title);
    let mid_trunc = crate::ui::truncate_width(&mid_label, mid_budget);

    let spans = vec![
        Span::styled(
            prev_btn,
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(mid_trunc.clone(), Style::default().fg(theme.fg)),
        Span::styled(
            next_btn,
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    hitmap.add_line(area.x, area.y, prev_w, HitTarget::OnenotePage(prev_idx));
    let next_x = area.x
        + prev_w
        + UnicodeWidthStr::width(mid_trunc.as_str()) as u16;
    if next_x < area.x + area.width {
        hitmap.add_line(
            next_x,
            area.y,
            next_w.min(area.x + area.width - next_x),
            HitTarget::OnenotePage(next_idx),
        );
    }
}

fn draw_note_canvas(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    ui.onenote_scroll.area = inner;
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let Some(detail) = app.active_page_detail.clone() else {
        let msg = if app.loading_onenote_detail {
            "노트 본문을 불러오는 중…"
        } else if app.onenote_notebooks.is_empty() {
            "왼쪽 탐색 창에서 필기장을 선택하거나 B 키로 새 필기장을 만드세요"
        } else if app.onenote_pages.is_empty() {
            "이 섹션에 페이지가 없습니다 (n: 새 페이지 만들기)"
        } else {
            "페이지를 선택하세요 (J/K 또는 클릭)"
        };
        frame.render_widget(Paragraph::new(msg).style(theme.muted_style()), inner);
        return;
    };

    if !detail.blocks.is_empty() && ui.onenote_selected_block >= detail.blocks.len() {
        ui.onenote_selected_block = detail.blocks.len() - 1;
    }

    // Lazily fetch/decode inline terminal images when inline_images is enabled
    let width = inner.width as usize;
    let thumb_cols = width.saturating_sub(4).clamp(16, 48) as u16;
    let thumb_rows = 12u16;
    let mut cached_thumbs: HashMap<usize, RenderedTerminalImage> = HashMap::new();

    if app.config.inline_images && ui.inline_images {
        for block in &detail.blocks {
            if let OnenoteBlockKind::Image { resource_index, .. } = &block.kind {
                if let Some(res) = detail.resources.get(*resource_index) {
                    let key = AppState::onenote_image_key(
                        &detail.summary.page_key,
                        *resource_index,
                        thumb_cols,
                        thumb_rows,
                    );
                    if let Some(img) = render_terminal_image(&app.onenote_image_cache, &key) {
                        cached_thumbs.insert(*resource_index, img);
                    } else {
                        app.request_onenote_image(
                            detail.summary.page_key.clone(),
                            *resource_index,
                            res.clone(),
                            thumb_cols,
                            thumb_rows,
                        );
                    }
                }
            }
        }
    }

    let (lines, block_line_ranges) = build_page_canvas_lines(
        &detail,
        Some(ui.onenote_selected_block),
        &cached_thumbs,
        theme,
        width,
    );

    let height = inner.height as usize;
    ui.onenote_scroll.total_lines = lines.len();

    // Auto-scroll to keep the selected block visible when navigated by keyboard
    if ui.onenote_follow_selection {
        if let Some(&(start_line, end_line)) = block_line_ranges.get(&ui.onenote_selected_block) {
            ensure_range_visible(&mut ui.onenote_scroll, start_line, end_line, height);
        }
        ui.onenote_follow_selection = false;
    }

    let offset = ui.onenote_scroll.offset(height);
    ui.onenote_scroll.offset = offset;

    chat_view::render_scrolled_lines(frame, inner, &lines, offset, height, hitmap);
}

/// Helper wrapping terminal image lookup from `app.onenote_image_cache`.
pub fn render_terminal_image(
    cache: &HashMap<String, RenderedTerminalImage>,
    cache_key: &str,
) -> Option<RenderedTerminalImage> {
    cache.get(cache_key).cloned()
}

fn ensure_range_visible(
    scroll: &mut crate::ui::app::ScrollState,
    start_line: usize,
    end_line: usize,
    viewport_h: usize,
) {
    if viewport_h == 0 || scroll.total_lines <= viewport_h {
        scroll.scroll_up = 0;
        scroll.follow_tail = false;
        return;
    }
    let max_off = scroll.total_lines - viewport_h;
    let mut cur_off = scroll.offset(viewport_h);
    if start_line < cur_off {
        cur_off = start_line;
    } else if end_line > cur_off + viewport_h {
        cur_off = end_line.saturating_sub(viewport_h).min(start_line);
    }
    cur_off = cur_off.min(max_off);
    scroll.follow_tail = false;
    scroll.scroll_up = max_off.saturating_sub(cur_off).min(u16::MAX as usize) as u16;
}

/// Pure builder returning `(lines, block_index -> (start_line, end_line))` for rendering & unit testing.
pub fn build_page_canvas_lines(
    detail: &OnenotePageDetail,
    selected_block: Option<usize>,
    cached_thumbs: &HashMap<usize, RenderedTerminalImage>,
    theme: &Theme,
    width: usize,
) -> (Vec<RenderedLine>, HashMap<usize, (usize, usize)>) {
    let width = width.max(20);
    let mut lines: Vec<RenderedLine> = Vec::new();
    let mut ranges: HashMap<usize, (usize, usize)> = HashMap::new();

    // Page Title Header
    let page_title = if detail.summary.title.trim().is_empty() {
        "(제목 없는 페이지)"
    } else {
        detail.summary.title.trim()
    };
    lines.push(RenderedLine::plain(Line::from(vec![
        Span::styled(
            "# ",
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            page_title.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ])));

    // Metadata line: `{notebook_name} › {section_name} · 수정 {last_modified} · 블록 {blocks.len()}개`
    let nb = detail
        .summary
        .notebook_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("내 필기장");
    let sec = detail
        .summary
        .section_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("섹션");
    let modified = detail
        .summary
        .last_modified_at
        .as_deref()
        .map(short_timestamp)
        .unwrap_or_else(|| "-".to_string());
    let meta = format!(
        "{nb} › {sec} · 수정 {modified} · 블록 {}개",
        detail.blocks.len()
    );
    lines.push(RenderedLine::plain(Line::from(Span::styled(
        crate::ui::truncate_width(&meta, width),
        theme.muted_style(),
    ))));
    lines.push(RenderedLine::plain(Line::from(Span::styled(
        "─".repeat(width.min(72)),
        Style::default().fg(ONENOTE_BRAND_BG),
    ))));

    if detail.blocks.is_empty() {
        if !detail.body_plain.trim().is_empty() {
            for raw in detail.body_plain.lines() {
                for wrapped in wrap_text_to_width(raw, width.saturating_sub(2)) {
                    lines.push(RenderedLine::plain(Line::from(Span::raw(format!(
                        "  {wrapped}"
                    )))));
                }
            }
        } else {
            lines.push(RenderedLine::plain(Line::from(Span::styled(
                "  비어 있는 페이지입니다. `a` 키로 Markdown 블록을 추가하거나 `E` 키로 외부 편집기에서 작성하세요.",
                theme.muted_style(),
            ))));
        }
        return (lines, ranges);
    }

    for (block_idx, block) in detail.blocks.iter().enumerate() {
        let start = lines.len();
        let is_selected = selected_block == Some(block_idx);
        render_block_into(
            &mut lines,
            block_idx,
            block,
            is_selected,
            cached_thumbs,
            theme,
            width,
        );
        // Subtle spacing line between major blocks
        lines.push(RenderedLine::plain(Line::from("")));
        let end = lines.len();
        ranges.insert(block_idx, (start, end));
    }

    (lines, ranges)
}

fn tag_badge(tag: Option<&OnenoteTagKind>, theme: &Theme) -> Option<(&'static str, Style, bool)> {
    match tag? {
        OnenoteTagKind::Todo { completed: false } => Some((
            "☐ ",
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
            true,
        )),
        OnenoteTagKind::Todo { completed: true } => Some((
            "☑ ",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
            true,
        )),
        OnenoteTagKind::Important => Some((
            "★ ",
            Style::default()
                .fg(Color::Rgb(245, 194, 66))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Question => Some((
            "❓ ",
            Style::default()
                .fg(Color::Rgb(86, 212, 221))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Critical => Some((
            "❗ ",
            Style::default()
                .fg(Color::Rgb(242, 95, 92))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Highlight => Some((
            "🖍 ",
            Style::default()
                .fg(Color::Rgb(255, 226, 102))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Idea => Some((
            "💡 ",
            Style::default()
                .fg(Color::Rgb(245, 194, 66))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Custom(_) => Some(("🏷 ", theme.accent_style(), false)),
    }
}

fn render_block_into(
    out: &mut Vec<RenderedLine>,
    block_idx: usize,
    block: &OnenoteBlock,
    selected: bool,
    cached_thumbs: &HashMap<usize, RenderedTerminalImage>,
    theme: &Theme,
    width: usize,
) {
    let sel_marker = if selected { "▸ " } else { "  " };
    let sel_marker_style = if selected {
        Style::default()
            .fg(ONENOTE_PURPLE)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let row_bg = if matches!(block.tag, Some(OnenoteTagKind::Highlight)) {
        Some(Color::Rgb(72, 60, 16))
    } else if selected {
        Some(theme.selection_bg)
    } else {
        None
    };

    let todo_completed = matches!(block.tag, Some(OnenoteTagKind::Todo { completed: true }));
    let badge_info = tag_badge(block.tag.as_ref(), theme);
    let block_hit = HitTarget::OnenoteBlock(block_idx);

    match &block.kind {
        OnenoteBlockKind::Heading { level, spans } => {
            let heading_color = match *level {
                1 => ONENOTE_PURPLE,
                2 => Color::Rgb(110, 168, 255),
                3 => Color::Rgb(86, 212, 221),
                _ => theme.fg,
            };
            let prefix_hashes = format!("{} ", "#".repeat((*level).clamp(1, 6) as usize));
            let mut base_style = Style::default()
                .fg(heading_color)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                base_style = base_style.bg(bg);
            }
            if todo_completed {
                base_style = base_style.add_modifier(Modifier::DIM | Modifier::CROSSED_OUT);
            }

            let mut line_spans = vec![Span::styled(sel_marker, sel_marker_style)];
            let mut inline_hits = Vec::new();
            let mut col_off = UnicodeWidthStr::width(sel_marker) as u16;

            if let Some((b_txt, mut b_st, is_todo)) = badge_info {
                if let Some(bg) = row_bg {
                    b_st = b_st.bg(bg);
                }
                let bw = UnicodeWidthStr::width(b_txt) as u16;
                if is_todo {
                    inline_hits.push((col_off, bw.max(2), HitTarget::OnenoteTodoToggle(block_idx)));
                }
                line_spans.push(Span::styled(b_txt, b_st));
                col_off = col_off.saturating_add(bw);
            }

            line_spans.push(Span::styled(prefix_hashes, base_style));
            let _ = col_off;
            append_rich_spans(&mut line_spans, spans, base_style, theme);
            out.push(RenderedLine::with_inline(
                Line::from(line_spans),
                Some(block_hit),
                inline_hits,
            ));

            if *level <= 2 {
                let text_w = spans_plain_text(spans).width() + (*level as usize) + 1;
                let rule_char = if *level == 1 { "═" } else { "─" };
                let rule = rule_char.repeat(text_w.clamp(6, width.saturating_sub(4)));
                out.push(RenderedLine::clickable(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(rule, Style::default().fg(heading_color)),
                    ]),
                    block_hit,
                ));
            }
        }
        OnenoteBlockKind::Paragraph { spans } => {
            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                "",
                Style::default(),
                badge_info,
                spans,
                todo_completed,
                row_bg,
                theme,
                width,
            );
        }
        OnenoteBlockKind::ListItem {
            ordered,
            index,
            depth,
            spans,
        } => {
            let indent = "  ".repeat((*depth).min(6) as usize);
            let bullet = if *ordered {
                format!("{indent}{}. ", index.unwrap_or(1))
            } else {
                format!("{indent}• ")
            };
            let mut bullet_style = Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                bullet_style = bullet_style.bg(bg);
            }
            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                &bullet,
                bullet_style,
                badge_info,
                spans,
                todo_completed,
                row_bg,
                theme,
                width,
            );
        }
        OnenoteBlockKind::Quote { spans } => {
            let mut bar_style = Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                bar_style = bar_style.bg(bg);
            }
            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                "│ ",
                bar_style,
                badge_info,
                spans,
                todo_completed,
                row_bg,
                theme,
                width,
            );
        }
        OnenoteBlockKind::Table { headers, rows } => {
            for line in render_unicode_table(headers, rows, selected, theme, width.saturating_sub(2))
            {
                let mut prefixed = vec![Span::styled(sel_marker, sel_marker_style)];
                prefixed.extend(line.spans);
                out.push(RenderedLine::clickable(Line::from(prefixed), block_hit));
            }
        }
        OnenoteBlockKind::CodeBlock { lang, code } => {
            let box_w = width.saturating_sub(4).max(16);
            let lang_label = lang
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("code");
            let top_label = format!("─ {lang_label} ");
            let top_rem = box_w.saturating_sub(UnicodeWidthStr::width(top_label.as_str()) + 2);
            let border_st = if selected {
                Style::default().fg(ONENOTE_PURPLE)
            } else {
                theme.muted_style()
            };
            let code_bg = Color::Rgb(28, 30, 38);
            let code_st = Style::default().fg(Color::Rgb(220, 228, 240)).bg(code_bg);

            out.push(RenderedLine::clickable(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(format!("┌{top_label}{}┐", "─".repeat(top_rem)), border_st),
                ]),
                block_hit,
            ));

            let inner_code_w = box_w.saturating_sub(4).max(8);
            let code_lines: Vec<&str> = if code.is_empty() {
                vec![""]
            } else {
                code.lines().collect()
            };
            for raw in code_lines {
                let expanded = raw.replace('\t', "    ");
                let wrapped = wrap_text_to_width(&expanded, inner_code_w);
                for seg in wrapped {
                    let pad = inner_code_w.saturating_sub(UnicodeWidthStr::width(seg.as_str()));
                    out.push(RenderedLine::clickable(
                        Line::from(vec![
                            Span::styled(sel_marker, sel_marker_style),
                            Span::styled("│ ", border_st.bg(code_bg)),
                            Span::styled(format!("{seg}{}", " ".repeat(pad)), code_st),
                            Span::styled(" │", border_st.bg(code_bg)),
                        ]),
                        block_hit,
                    ));
                }
            }

            out.push(RenderedLine::clickable(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(
                        format!("└{}┘", "─".repeat(box_w.saturating_sub(2))),
                        border_st,
                    ),
                ]),
                block_hit,
            ));
        }
        OnenoteBlockKind::Image {
            resource_index,
            alt,
            width: img_w,
            height: img_h,
            ..
        } => {
            let dim_tag = match (img_w, img_h) {
                (Some(w), Some(h)) if *w > 0 && *h > 0 => format!(" ({w}x{h})"),
                _ => String::new(),
            };
            let alt_tag = alt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|a| format!(" \"{a}\""))
                .unwrap_or_default();
            let chip = format!(
                "[🖼 이미지 #{}{dim_tag}{alt_tag} — v 미리보기]",
                resource_index + 1
            );
            let chip_w = UnicodeWidthStr::width(chip.as_str()) as u16;
            let mut chip_style = Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                chip_style = chip_style.bg(bg);
            }
            let off = UnicodeWidthStr::width(sel_marker) as u16;
            out.push(RenderedLine::with_inline(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(chip, chip_style),
                ]),
                Some(block_hit),
                vec![(
                    off,
                    chip_w,
                    HitTarget::OnenoteResource {
                        resource_index: *resource_index,
                        is_image: true,
                    },
                )],
            ));

            if let Some(thumb) = cached_thumbs.get(resource_index) {
                for img_line in &thumb.lines {
                    let mut spans = vec![Span::styled("    ", Style::default())];
                    spans.extend(img_line.spans.clone());
                    out.push(RenderedLine::with_inline(
                        Line::from(spans),
                        Some(block_hit),
                        vec![(
                            4,
                            thumb.cols,
                            HitTarget::OnenoteResource {
                                resource_index: *resource_index,
                                is_image: true,
                            },
                        )],
                    ));
                }
            }
        }
        OnenoteBlockKind::Attachment {
            resource_index,
            name,
            mime_type,
            ..
        } => {
            let chip = format!("[📎 {name} ({mime_type}) — D 다운로드]");
            let chip_w = UnicodeWidthStr::width(chip.as_str()) as u16;
            let mut chip_style = theme.accent_style().add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                chip_style = chip_style.bg(bg);
            }
            let off = UnicodeWidthStr::width(sel_marker) as u16;
            out.push(RenderedLine::with_inline(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(chip, chip_style),
                ]),
                Some(block_hit),
                vec![(
                    off,
                    chip_w,
                    HitTarget::OnenoteResource {
                        resource_index: *resource_index,
                        is_image: false,
                    },
                )],
            ));
        }
        OnenoteBlockKind::Divider => {
            out.push(RenderedLine::clickable(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(
                        "─".repeat(width.saturating_sub(4).min(64)),
                        theme.muted_style(),
                    ),
                ]),
                block_hit,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_rich_text_lines(
    out: &mut Vec<RenderedLine>,
    block_idx: usize,
    sel_marker: &'static str,
    sel_marker_style: Style,
    lead_prefix: &str,
    lead_style: Style,
    badge_info: Option<(&'static str, Style, bool)>,
    spans: &[RichSpan],
    todo_completed: bool,
    row_bg: Option<Color>,
    theme: &Theme,
    width: usize,
) {
    let block_hit = HitTarget::OnenoteBlock(block_idx);
    let mut base_style = Style::default().fg(theme.fg);
    if let Some(bg) = row_bg {
        base_style = base_style.bg(bg);
    }
    if todo_completed {
        base_style = Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::DIM | Modifier::CROSSED_OUT);
        if let Some(bg) = row_bg {
            base_style = base_style.bg(bg);
        }
    }

    let badge_w = badge_info
        .as_ref()
        .map(|(txt, _, _)| UnicodeWidthStr::width(*txt))
        .unwrap_or(0);
    let first_prefix_w = UnicodeWidthStr::width(sel_marker)
        + UnicodeWidthStr::width(lead_prefix)
        + badge_w;
    let body_budget = width.saturating_sub(first_prefix_w).max(10);

    let wrapped_span_lines = wrap_rich_spans(spans, body_budget, base_style, theme);
    let cont_pad = " ".repeat(UnicodeWidthStr::width(lead_prefix) + badge_w);

    for (line_idx, seg_spans) in wrapped_span_lines.into_iter().enumerate() {
        let mut row_spans = vec![Span::styled(sel_marker, sel_marker_style)];
        let mut inline_hits = Vec::new();
        let mut col_off = UnicodeWidthStr::width(sel_marker) as u16;

        if line_idx == 0 {
            if !lead_prefix.is_empty() {
                let lw = UnicodeWidthStr::width(lead_prefix) as u16;
                row_spans.push(Span::styled(lead_prefix.to_string(), lead_style));
                col_off = col_off.saturating_add(lw);
            }
            if let Some((b_txt, mut b_st, is_todo)) = badge_info {
                if let Some(bg) = row_bg {
                    b_st = b_st.bg(bg);
                }
                let bw = UnicodeWidthStr::width(b_txt) as u16;
                if is_todo {
                    inline_hits.push((col_off, bw.max(2), HitTarget::OnenoteTodoToggle(block_idx)));
                }
                row_spans.push(Span::styled(b_txt, b_st));
            }
        } else if !cont_pad.is_empty() {
            let mut pad_st = Style::default();
            if let Some(bg) = row_bg {
                pad_st = pad_st.bg(bg);
            }
            row_spans.push(Span::styled(cont_pad.clone(), pad_st));
        }

        row_spans.extend(seg_spans);
        out.push(RenderedLine::with_inline(
            Line::from(row_spans),
            Some(block_hit),
            inline_hits,
        ));
    }
}

fn style_for_rich_span(span: &RichSpan, base_style: Style, theme: &Theme) -> Style {
    let mut st = base_style;
    if span.style.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if span.style.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if span.style.strike {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    if span.style.underline || span.style.link_url.is_some() {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    if span.style.link_url.is_some() {
        st = st.fg(theme.accent);
    }
    if span.style.code {
        st = st.fg(Color::Rgb(245, 194, 107)).bg(Color::Rgb(36, 38, 48));
    }
    st
}

fn append_rich_spans(
    out: &mut Vec<Span<'static>>,
    spans: &[RichSpan],
    base_style: Style,
    theme: &Theme,
) {
    for sp in spans {
        let st = style_for_rich_span(sp, base_style, theme);
        out.push(Span::styled(sp.text.replace('\n', " "), st));
    }
}

fn wrap_rich_spans(
    spans: &[RichSpan],
    max_width: usize,
    base_style: Style,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    if spans.is_empty() {
        return vec![vec![Span::styled(String::new(), base_style)]];
    }

    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut cur_w = 0usize;

    for sp in spans {
        let st = style_for_rich_span(sp, base_style, theme);
        let mut chunk = String::new();

        for ch in sp.text.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                if !chunk.is_empty() {
                    lines
                        .last_mut()
                        .unwrap()
                        .push(Span::styled(std::mem::take(&mut chunk), st));
                }
                lines.push(Vec::new());
                cur_w = 0;
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > max_width && cur_w > 0 {
                if !chunk.is_empty() {
                    lines
                        .last_mut()
                        .unwrap()
                        .push(Span::styled(std::mem::take(&mut chunk), st));
                }
                lines.push(Vec::new());
                cur_w = 0;
            }
            chunk.push(ch);
            cur_w += cw;
        }

        if !chunk.is_empty() {
            lines.last_mut().unwrap().push(Span::styled(chunk, st));
        }
    }

    if lines.is_empty() {
        vec![vec![Span::styled(String::new(), base_style)]]
    } else {
        lines
    }
}

/// Renders a crisp Unicode box-drawn table (`┌──┬──┐` / `│  │  │` / `├──┼──┤` / `└──┴──┘`) fit to `max_width`.
pub fn render_unicode_table(
    headers: &[Vec<RichSpan>],
    rows: &[Vec<Vec<RichSpan>>],
    selected: bool,
    theme: &Theme,
    max_width: usize,
) -> Vec<Line<'static>> {
    let col_count = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if col_count == 0 {
        return Vec::new();
    }

    let header_texts: Vec<String> = (0..col_count)
        .map(|c| {
            headers
                .get(c)
                .map(|s| spans_plain_text(s))
                .unwrap_or_default()
        })
        .collect();
    let body_texts: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            (0..col_count)
                .map(|c| r.get(c).map(|s| spans_plain_text(s)).unwrap_or_default())
                .collect()
        })
        .collect();

    let col_widths = compute_table_col_widths(&header_texts, &body_texts, col_count, max_width);
    let border_style = if selected {
        Style::default().fg(ONENOTE_PURPLE)
    } else {
        theme.muted_style()
    };
    let header_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(theme.fg);

    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        build_table_rule(&col_widths, '┌', '┬', '┐'),
        border_style,
    )));

    let has_non_empty_header = header_texts.iter().any(|h| !h.trim().is_empty());
    if has_non_empty_header {
        render_table_row(
            &mut out,
            &header_texts,
            &col_widths,
            border_style,
            header_style,
        );
        if !body_texts.is_empty() {
            out.push(Line::from(Span::styled(
                build_table_rule(&col_widths, '├', '┼', '┤'),
                border_style,
            )));
        }
    }

    for (r_idx, row_cells) in body_texts.iter().enumerate() {
        render_table_row(&mut out, row_cells, &col_widths, border_style, cell_style);
        if r_idx + 1 < body_texts.len() {
            out.push(Line::from(Span::styled(
                build_table_rule(&col_widths, '├', '┼', '┤'),
                border_style,
            )));
        }
    }

    out.push(Line::from(Span::styled(
        build_table_rule(&col_widths, '└', '┴', '┘'),
        border_style,
    )));
    out
}

fn compute_table_col_widths(
    headers: &[String],
    rows: &[Vec<String>],
    col_count: usize,
    max_width: usize,
) -> Vec<usize> {
    // Each column consumes `col_w + 3` chars (`│ cell `), plus final `│` = `3 * col_count + 1` overhead
    let overhead = 3 * col_count + 1;
    let min_col = 4usize;
    let available = max_width.saturating_sub(overhead).max(col_count * min_col);

    let mut natural = vec![min_col; col_count];
    for (c, h) in headers.iter().enumerate() {
        natural[c] = natural[c].max(UnicodeWidthStr::width(h.trim()).min(36));
    }
    for r in rows {
        for (c, cell) in r.iter().enumerate() {
            let max_line = cell
                .lines()
                .map(|l| UnicodeWidthStr::width(l.trim()))
                .max()
                .unwrap_or(0);
            natural[c] = natural[c].max(max_line.min(40));
        }
    }

    let sum_nat: usize = natural.iter().sum();
    if sum_nat <= available {
        return natural;
    }

    let mut scaled: Vec<usize> = natural
        .iter()
        .map(|&w| ((w * available) / sum_nat.max(1)).max(min_col))
        .collect();
    let mut cur_sum: usize = scaled.iter().sum();
    while cur_sum > available {
        if let Some((idx, _)) = scaled
            .iter()
            .enumerate()
            .filter(|(_, &w)| w > min_col)
            .max_by_key(|(_, &w)| w)
        {
            scaled[idx] -= 1;
            cur_sum -= 1;
        } else {
            break;
        }
    }
    scaled
}

fn build_table_rule(col_widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, &w) in col_widths.iter().enumerate() {
        s.push_str(&"─".repeat(w + 2));
        if i + 1 < col_widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    s
}

fn render_table_row(
    out: &mut Vec<Line<'static>>,
    cells: &[String],
    col_widths: &[usize],
    border_style: Style,
    text_style: Style,
) {
    let wrapped_cols: Vec<Vec<String>> = col_widths
        .iter()
        .enumerate()
        .map(|(c, &w)| {
            let raw = cells.get(c).map(String::as_str).unwrap_or("");
            wrap_text_to_width(raw.trim(), w).into_iter().take(4).collect()
        })
        .collect();
    let row_height = wrapped_cols.iter().map(Vec::len).max().unwrap_or(1).max(1);

    for sub in 0..row_height {
        let mut spans = vec![Span::styled("│", border_style)];
        for (c, &w) in col_widths.iter().enumerate() {
            let seg = wrapped_cols
                .get(c)
                .and_then(|lines| lines.get(sub))
                .map(String::as_str)
                .unwrap_or("");
            let used = UnicodeWidthStr::width(seg);
            let pad = w.saturating_sub(used);
            spans.push(Span::styled(format!(" {seg}{} ", " ".repeat(pad)), text_style));
            spans.push(Span::styled("│", border_style));
        }
        out.push(Line::from(spans));
    }
}

/// Converts a single `OnenoteBlock` back into clean Markdown for surgical block editing (`e`).
pub fn block_to_markdown(block: &OnenoteBlock) -> String {
    let todo_prefix = match &block.tag {
        Some(OnenoteTagKind::Todo { completed: false }) => "- [ ] ",
        Some(OnenoteTagKind::Todo { completed: true }) => "- [x] ",
        _ => "",
    };

    match &block.kind {
        OnenoteBlockKind::Heading { level, spans } => {
            format!(
                "{} {}",
                "#".repeat((*level).clamp(1, 6) as usize),
                spans_to_markdown(spans)
            )
        }
        OnenoteBlockKind::Paragraph { spans } => {
            format!("{todo_prefix}{}", spans_to_markdown(spans))
        }
        OnenoteBlockKind::ListItem {
            ordered,
            index,
            depth,
            spans,
        } => {
            let indent = "  ".repeat(*depth as usize);
            if !todo_prefix.is_empty() {
                format!("{indent}{todo_prefix}{}", spans_to_markdown(spans))
            } else if *ordered {
                format!("{indent}{}. {}", index.unwrap_or(1), spans_to_markdown(spans))
            } else {
                format!("{indent}- {}", spans_to_markdown(spans))
            }
        }
        OnenoteBlockKind::Quote { spans } => format!("> {}", spans_to_markdown(spans)),
        OnenoteBlockKind::CodeBlock { lang, code } => {
            let l = lang.as_deref().unwrap_or("");
            format!("```{l}\n{code}\n```")
        }
        OnenoteBlockKind::Table { headers, rows } => {
            let cols = headers
                .len()
                .max(rows.iter().map(Vec::len).max().unwrap_or(0));
            if cols == 0 {
                return String::new();
            }
            let mut md = Vec::new();
            let h_cells: Vec<String> = (0..cols)
                .map(|c| headers.get(c).map(|s| spans_to_markdown(s)).unwrap_or_default())
                .collect();
            md.push(format!("| {} |", h_cells.join(" | ")));
            md.push(format!("| {} |", vec!["---"; cols].join(" | ")));
            for r in rows {
                let r_cells: Vec<String> = (0..cols)
                    .map(|c| r.get(c).map(|s| spans_to_markdown(s)).unwrap_or_default())
                    .collect();
                md.push(format!("| {} |", r_cells.join(" | ")));
            }
            md.join("\n")
        }
        OnenoteBlockKind::Image { alt, src, .. } => {
            format!("![{}]({src})", alt.as_deref().unwrap_or("image"))
        }
        OnenoteBlockKind::Attachment { name, src, .. } => format!("[{name}]({src})"),
        OnenoteBlockKind::Divider => "---".to_string(),
    }
}

fn spans_plain_text(spans: &[RichSpan]) -> String {
    let mut out = String::new();
    for s in spans {
        out.push_str(&s.text);
    }
    out
}

fn spans_to_markdown(spans: &[RichSpan]) -> String {
    let mut out = String::new();
    for sp in spans {
        let t = &sp.text;
        if t.is_empty() {
            continue;
        }
        let mut formatted = if sp.style.code {
            format!("`{t}`")
        } else {
            t.clone()
        };
        if sp.style.bold {
            formatted = format!("**{formatted}**");
        }
        if sp.style.italic {
            formatted = format!("*{formatted}*");
        }
        if sp.style.strike {
            formatted = format!("~~{formatted}~~");
        }
        if let Some(url) = &sp.style.link_url {
            formatted = format!("[{formatted}]({url})");
        }
        out.push_str(&formatted);
    }
    out
}

fn wrap_text_to_width(text: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(2);
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        let clean = raw_line.trim_end_matches('\r');
        if clean.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut used = 0usize;
        for ch in clean.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > max_width && used > 0 {
                out.push(std::mem::take(&mut cur));
                used = 0;
            }
            cur.push(ch);
            used += cw;
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn truncate_clean(text: &str, max_width: usize) -> String {
    crate::ui::truncate_width(text, max_width)
}

fn short_timestamp(iso: &str) -> String {
    if let (Some(date), Some(time)) = (iso.get(0..10), iso.get(11..16)) {
        format!("{date} {time}")
    } else {
        iso.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OnenoteResource, OnenoteScope};

    fn sample_summary(title: &str, level: i32, has_todo: bool) -> OnenotePageSummary {
        OnenotePageSummary {
            page_key: format!("me|p-{title}"),
            id: format!("p-{title}"),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("회의록".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("엔지니어링".to_string()),
            title: title.to_string(),
            level,
            order: 0,
            created_at: Some("2026-11-15T09:00:00Z".to_string()),
            last_modified_at: Some("2026-11-15T14:20:00Z".to_string()),
            content_url: None,
            web_url: Some("https://onedrive.live.com/onenote".to_string()),
            preview_text: None,
            has_todo,
        }
    }

    #[test]
    fn subpage_indentation_formats_levels_authentically() {
        let p0 = sample_summary("루트 페이지", 0, false);
        let p0_todo = sample_summary("할일 페이지", 0, true);
        let p1 = sample_summary("하위 페이지", 1, false);
        let p2 = sample_summary("손자 페이지", 2, false);

        assert_eq!(format_page_row_label(&p0), ("• ", "루트 페이지".to_string()));
        assert_eq!(
            format_page_row_label(&p0_todo),
            ("☐ ", "할일 페이지".to_string())
        );
        assert_eq!(
            format_page_row_label(&p1),
            ("  └─ ", "하위 페이지".to_string())
        );
        assert_eq!(
            format_page_row_label(&p2),
            ("    └─ ", "손자 페이지".to_string())
        );
    }

    #[test]
    fn unicode_box_table_renders_headers_and_rows_cleanly() {
        let headers = vec![
            vec![RichSpan::plain("서비스")],
            vec![RichSpan::plain("상태")],
        ];
        let rows = vec![
            vec![
                vec![RichSpan::plain("Teams")],
                vec![RichSpan::plain("정상")],
            ],
            vec![
                vec![RichSpan::plain("OneNote")],
                vec![RichSpan::plain("배포 완료")],
            ],
        ];
        let rendered = render_unicode_table(&headers, &rows, false, &Theme::default(), 60);
        let plain: String = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(plain.contains('┌') && plain.contains('┬') && plain.contains('┐'));
        assert!(plain.contains("서비스") && plain.contains("OneNote") && plain.contains("배포 완료"));
        assert!(plain.contains('├') && plain.contains('┼') && plain.contains('┤'));
        assert!(plain.contains('└') && plain.contains('┴') && plain.contains('┘'));
    }

    #[test]
    fn note_canvas_renders_tags_todo_hit_targets_code_and_resources() {
        let detail = OnenotePageDetail {
            summary: sample_summary("v0.3.0 출시 노트", 0, true),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: vec![
                OnenoteBlock {
                    element_id: Some("h1:1".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Heading {
                        level: 1,
                        spans: vec![RichSpan::plain("핵심 목표")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("p:2".to_string()),
                    tag: Some(OnenoteTagKind::Todo { completed: false }),
                    raw_html: None,
                    kind: OnenoteBlockKind::Paragraph {
                        spans: vec![RichSpan::plain("OneNote TUI 완성")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("p:3".to_string()),
                    tag: Some(OnenoteTagKind::Todo { completed: true }),
                    raw_html: None,
                    kind: OnenoteBlockKind::Paragraph {
                        spans: vec![RichSpan::plain("XHTML 파서 검증")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("pre:4".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::CodeBlock {
                        lang: Some("rust".to_string()),
                        code: "let ok = true;".to_string(),
                    },
                },
                OnenoteBlock {
                    element_id: Some("img:5".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Image {
                        resource_index: 0,
                        src: "https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content"
                            .to_string(),
                        alt: Some("아키텍처".to_string()),
                        width: Some(800),
                        height: Some(600),
                    },
                },
            ],
            resources: vec![OnenoteResource {
                element_id: Some("img:5".to_string()),
                resource_url:
                    "https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content".to_string(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("arch.png".to_string()),
                is_image: true,
                alt_text: Some("아키텍처".to_string()),
            }],
            primary_div_id: Some("div:main".to_string()),
            synced_at: None,
        };

        let (lines, ranges) = build_page_canvas_lines(
            &detail,
            Some(1),
            &HashMap::new(),
            &Theme::default(),
            80,
        );
        assert_eq!(ranges.len(), 5);

        let all_text: String = lines
            .iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("v0.3.0 출시 노트"));
        assert!(all_text.contains("☐ OneNote TUI 완성"));
        assert!(all_text.contains("☑ XHTML 파서 검증"));
        assert!(all_text.contains("─ rust "));
        assert!(all_text.contains("[🖼 이미지 #1"));

        // Verify block #1 has inline HitTarget::OnenoteTodoToggle(1)
        let has_todo_hit = lines.iter().any(|r| {
            r.inline_targets
                .iter()
                .any(|&(_, _, t)| t == HitTarget::OnenoteTodoToggle(1))
        });
        assert!(has_todo_hit);

        assert_eq!(
            block_to_markdown(&detail.blocks[1]),
            "- [ ] OneNote TUI 완성"
        );
        assert_eq!(
            block_to_markdown(&detail.blocks[2]),
            "- [x] XHTML 파서 검증"
        );
    }
}
