//! Right Thread Drawer for Team Channel threads (`src/ui/thread_view.rs`).
//!
//! Shows the root post summary, scrollable chronological replies, composer target
//! toggle (`스레드 답글` vs `채널 새 글`), and close button (`Esc` / `[닫기]`).

use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::ThreadView,
    ui::{
        app::{AppState, Focus, UiState},
        chat_view::{hyperlinks_enabled, render_message_item, render_scrolled_lines, truncate, RenderedLine},
        hitmap::{ButtonId, HitMap, HitTarget, MessageScope},
        theme::Theme,
    },
};

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Thread;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(" 스레드 ", theme.title(focused))))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    draw_header(frame, inner, ui, hitmap);

    let body_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    ui.thread_scroll.area = body_area;

    if body_area.height == 0 {
        return;
    }

    let Some(thread) = &app.active_thread else {
        let msg = if app.loading_thread {
            "스레드를 불러오는 중…"
        } else {
            "열린 스레드가 없습니다 (채널 게시물에서 t)"
        };
        frame.render_widget(Paragraph::new(msg).style(theme.muted_style()), body_area);
        return;
    };

    let lines = build_thread_lines(
        thread,
        ui.thread_scroll.selected_idx,
        theme,
        body_area.width as usize,
        hyperlinks_enabled(),
    );

    if lines.is_empty() {
        return;
    }

    let height = body_area.height as usize;
    ui.thread_scroll.total_lines = lines.len();
    let offset = ui.thread_scroll.offset(height);
    ui.thread_scroll.offset = offset;

    render_scrolled_lines(frame, body_area, &lines, offset, height, hitmap);
}

fn draw_header(frame: &mut Frame, inner: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let target_label = if ui.composer_target_thread {
        "[입력:스레드]"
    } else {
        "[입력:채널새글]"
    };
    let close_label = "[닫기]";

    let target_w = UnicodeWidthStr::width(target_label) as u16;
    let close_w = UnicodeWidthStr::width(close_label) as u16;
    let buttons_w = target_w + 1 + close_w;

    let title_budget = inner.width.saturating_sub(buttons_w + 1) as usize;
    let title = truncate("스레드 답글", title_budget);
    let title_w = UnicodeWidthStr::width(title.as_str()) as u16;
    let pad = inner
        .width
        .saturating_sub(title_w)
        .saturating_sub(buttons_w);

    let spans = vec![
        Span::styled(title, theme.title(ui.focus == Focus::Thread)),
        Span::raw(" ".repeat(pad as usize)),
        Span::styled(
            target_label,
            theme.accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(close_label, theme.muted_style()),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let target_x = inner.x + title_w + pad;
    hitmap.add_line(
        target_x,
        inner.y,
        target_w,
        HitTarget::Button(ButtonId::ComposerTargetToggle),
    );
    let close_x = target_x + target_w + 1;
    hitmap.add_line(
        close_x,
        inner.y,
        close_w,
        HitTarget::Button(ButtonId::CloseThread),
    );
}

/// Pure thread line builder: root post summary + divider + chronological replies.
pub fn build_thread_lines(
    thread: &ThreadView,
    selected_reply: Option<usize>,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) -> Vec<RenderedLine> {
    let width = width.max(12);
    let mut out = Vec::new();

    render_message_item(
        &mut out,
        &thread.root,
        0,
        MessageScope::ThreadRoot,
        true,
        false,
        theme,
        width,
        hyperlinks,
    );

    out.push(RenderedLine::plain(Line::from(Span::styled(
        truncate(&format!("── 답글 {}개 ──", thread.replies.len()), width),
        theme.accent_style().add_modifier(Modifier::BOLD),
    ))));

    if thread.replies.is_empty() {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "  아직 답글이 없습니다. 아래 입력창에서 첫 답글을 남겨보세요.",
            theme.muted_style(),
        ))));
    } else {
        for (idx, reply) in thread.replies.iter().enumerate() {
            let selected = selected_reply == Some(idx);
            render_message_item(
                &mut out,
                reply,
                idx,
                MessageScope::ThreadReply,
                true,
                selected,
                theme,
                width,
                hyperlinks,
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MessageItem;

    fn msg(id: &str, author: &str, body: &str, parent: Option<&str>) -> MessageItem {
        MessageItem {
            msg_key: format!("chan:t1:c1|{id}"),
            conv_key: "chan:t1:c1".into(),
            msg_id: id.into(),
            parent_msg_id: parent.map(str::to_string),
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: if parent.is_none() {
                Some("배포 공지".into())
            } else {
                None
            },
            author_id: Some("u1".into()),
            author_name: author.into(),
            is_me: false,
            created_at: "2026-11-15T06:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: body.into(),
            body_plain: body.into(),
            attachments: vec![],
            mentions: vec![],
            reactions: vec![],
            reply_count: 1,
            web_url: None,
        }
    }

    #[test]
    fn thread_lines_include_root_and_replies() {
        let thread = ThreadView {
            conv_key: "chan:t1:c1".into(),
            root: msg("r1", "김팀장", "오늘 5시 배포", None),
            replies: vec![msg("rep1", "이개발", "확인했습니다!", Some("r1"))],
        };
        let lines = build_thread_lines(&thread, Some(0), &Theme::default(), 60, false);
        let joined: String = lines
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

        assert!(joined.contains("배포 공지"));
        assert!(joined.contains("오늘 5시 배포"));
        assert!(joined.contains("답글 1개"));
        assert!(joined.contains("확인했습니다!"));
    }
}
