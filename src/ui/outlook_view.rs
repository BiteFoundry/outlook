//! Center pane renderers for Outlook Unified Workspace tabs:
//! - Tab 3 (`메일`): Mail folder / message metadata, attachments, inline image thumbnails & HTML body
//! - Tab 4 (`캘린더`): Event agenda details, Teams meeting join link & RSVP actions
//! - Tab 5 (`연락처`): Personal contacts & Entra directory card + Mail/Chat quick actions
//! - Tab 6 (`할 일`): Microsoft To Do task details, completion toggle & quick-add input bar

use std::cell::RefCell;
use std::sync::Arc;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{self, RenderedTerminalImage},
    models::{CalendarEvent, ContactItem, MailMessage, RichBlock, RichSpan, TodoList, TodoTask},
    parser,
    ui::{
        app::{AppState, Focus, UiState},
        chat_view::{self, hash_theme_sig, DualHasher, LineSliceCache, RenderedLine},
        hitmap::{ButtonId, HitMap, HitTarget},
        nav_pane::{TAB_CALENDAR, TAB_CONTACTS, TAB_MAIL, TAB_TODO},
        theme::Theme,
    },
};

thread_local! {
    static MAIL_LINES_CACHE: RefCell<LineSliceCache> = RefCell::new(LineSliceCache::new(4));
}

fn compute_mail_sig(
    mail: &MailMessage,
    cached_thumbs_len: usize,
    inline_images: bool,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) -> (u64, u64) {
    let mut h = DualHasher::new(0x4d41_494c_5f4c_494e);
    h.write_str(&mail.id);
    h.write_str(&mail.subject);
    h.write_hash(&mail.is_read);
    h.write_hash(&mail.flagged);
    h.write_hash(&mail.body_html.len());
    h.write_hash(&mail.body_plain.len());
    h.write_hash(&mail.attachments.len());
    h.write_hash(&cached_thumbs_len);
    h.write_hash(&inline_images);
    h.write_hash(&width.max(16));
    h.write_hash(&hyperlinks);
    hash_theme_sig(&mut h, theme);
    h.finish()
}

pub fn pane_title(tab: usize) -> &'static str {
    match tab {
        TAB_MAIL => " 메일 보기 ",
        TAB_CALENDAR => " 일정 상세 ",
        TAB_CONTACTS => " 연락처 정보 ",
        TAB_TODO => " 할 일 (To Do) ",
        _ => " Outlook ",
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    let theme = ui.theme.clone();
    let focused = ui.focus == Focus::Chat;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            pane_title(ui.active_nav_tab),
            theme.title(focused),
        )))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match ui.active_nav_tab {
        TAB_MAIL => draw_mail_pane(frame, inner, app, ui, &theme, hitmap),
        TAB_CALENDAR => draw_calendar_pane(frame, inner, app, ui, &theme, hitmap),
        TAB_CONTACTS => draw_contacts_pane(frame, inner, app, ui, &theme, hitmap),
        TAB_TODO => draw_todo_pane(frame, inner, app, ui, &theme, hitmap),
        _ => {}
    }
}

fn draw_toolbar_buttons(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    hitmap: &mut HitMap,
    buttons: &[(&str, ButtonId)],
) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for &(label, btn_id) in buttons {
        let w = UnicodeWidthStr::width(label) as u16;
        if x.saturating_sub(area.x) + w > area.width {
            break;
        }
        spans.push(Span::styled(
            label,
            theme.accent_style().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        hitmap.add_line(x, area.y, w, HitTarget::Button(btn_id));
        x = x.saturating_add(w + 1);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_mail_pane(
    frame: &mut Frame,
    inner: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let has_img = app.active_mail().is_some_and(|m| {
        m.attachments
            .iter()
            .any(|a| img_preview::is_image_mime_or_name(&a.content_type, Some(&a.name)))
    });
    let mut buttons: Vec<(&str, ButtonId)> = vec![
        ("[새메일]", ButtonId::NewMail),
        ("[답장]", ButtonId::MailReply),
        ("[전체답장]", ButtonId::MailReplyAll),
        ("[전달]", ButtonId::MailForward),
        ("[읽음]", ButtonId::MailToggleRead),
        ("[★]", ButtonId::MailToggleFlag),
        ("[보관]", ButtonId::MailArchive),
        ("[삭제]", ButtonId::MailDelete),
    ];
    if has_img {
        buttons.push(("[🖼미리보기]", ButtonId::ImagePreview));
    }
    draw_toolbar_buttons(frame, header_area, theme, hitmap, &buttons);

    let body_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    ui.mail_scroll.area = body_area;
    if body_area.height == 0 {
        return;
    }

    let Some(mail) = app.active_mail().cloned() else {
        let empty = if app.loading_mail {
            "메일을 불러오는 중…"
        } else {
            "왼쪽 목록에서 메일을 선택하세요 (c: 새 메일)"
        };
        frame.render_widget(
            Paragraph::new(empty).style(theme.muted_style()),
            body_area,
        );
        return;
    };

    let width = body_area.width as usize;
    let thumb_cols = width.saturating_sub(4).clamp(16, 40) as u16;
    let thumb_rows = 10u16;
    let hyperlinks = chat_view::hyperlinks_enabled();

    let thumb_count = if ui.inline_images {
        mail.attachments
            .iter()
            .filter(|att| {
                img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name))
                    && app.image_cache.contains_key(&AppState::image_cache_key(
                        &mail.id,
                        &att.id,
                        thumb_cols,
                        thumb_rows,
                    ))
            })
            .count()
    } else {
        0
    };

    let sig = compute_mail_sig(&mail, thumb_count, ui.inline_images, theme, width, hyperlinks);
    let lines = if let Some(cached) = MAIL_LINES_CACHE.with(|cache| cache.borrow_mut().get(sig)) {
        cached
    } else {
        // Request lazy inline image previews for any image attachments if inline_images is enabled.
        let mut cached_thumbs: Vec<(String, RenderedTerminalImage)> = Vec::new();
        if ui.inline_images {
            for att in &mail.attachments {
                if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                    let key = AppState::image_cache_key(&mail.id, &att.id, thumb_cols, thumb_rows);
                    if let Some(rendered) = app.image_cache.get(&key) {
                        cached_thumbs.push((att.id.clone(), rendered.clone()));
                    } else {
                        app.request_mail_image(mail.id.clone(), att.clone(), thumb_cols, thumb_rows);
                    }
                }
            }
        }

        let built: Arc<[RenderedLine]> = Arc::from(build_mail_lines(
            &mail,
            &cached_thumbs,
            ui.inline_images,
            theme,
            width,
            hyperlinks,
        ));
        MAIL_LINES_CACHE.with(|cache| cache.borrow_mut().insert(sig, Arc::clone(&built)));
        built
    };

    let height = body_area.height as usize;
    ui.mail_scroll.total_lines = lines.len();
    let offset = ui.mail_scroll.offset(height);
    ui.mail_scroll.offset = offset;
    chat_view::render_scrolled_lines(frame, body_area, &lines, offset, height, hitmap);
}

fn draw_calendar_pane(
    frame: &mut Frame,
    inner: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let has_join = app
        .active_event()
        .and_then(AppState::event_join_url)
        .is_some();
    let mut buttons = vec![("[+ 새 일정]", ButtonId::NewEvent)];
    if has_join {
        buttons.push(("[📹 회의참가]", ButtonId::JoinMeeting));
    }
    buttons.extend_from_slice(&[
        ("[✓수락]", ButtonId::RsvpAccept),
        ("[?미정]", ButtonId::RsvpTentative),
        ("[✕거절]", ButtonId::RsvpDecline),
        ("[삭제]", ButtonId::DeleteEvent),
    ]);
    draw_toolbar_buttons(frame, header_area, theme, hitmap, &buttons);

    let body_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    ui.mail_scroll.area = body_area;
    if body_area.height == 0 {
        return;
    }

    let Some(ev) = app.active_event().cloned() else {
        frame.render_widget(
            Paragraph::new("왼쪽 목록에서 일정을 선택하세요 (n: 새 일정)")
                .style(theme.muted_style()),
            body_area,
        );
        return;
    };

    let lines = build_calendar_lines(
        &ev,
        theme,
        body_area.width as usize,
        chat_view::hyperlinks_enabled(),
    );
    let height = body_area.height as usize;
    ui.mail_scroll.total_lines = lines.len();
    let offset = ui.mail_scroll.offset(height);
    ui.mail_scroll.offset = offset;
    chat_view::render_scrolled_lines(frame, body_area, &lines, offset, height, hitmap);
}

fn draw_contacts_pane(
    frame: &mut Frame,
    inner: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let mut buttons = vec![
        ("[+ 새 연락처]", ButtonId::NewContact),
        ("[✉ 메일보내기]", ButtonId::ContactMail),
        ("[💬 채팅시작]", ButtonId::ContactChat),
    ];
    if app.active_contact().is_some_and(|c| c.source == "contact") {
        buttons.push(("[삭제]", ButtonId::DeleteContact));
    }
    draw_toolbar_buttons(frame, header_area, theme, hitmap, &buttons);

    let body_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    ui.mail_scroll.area = body_area;
    if body_area.height == 0 {
        return;
    }

    let Some(contact) = app.active_contact().cloned() else {
        frame.render_widget(
            Paragraph::new("왼쪽 목록에서 연락처를 선택하세요 (n: 새 연락처 · /: 검색)")
                .style(theme.muted_style()),
            body_area,
        );
        return;
    };

    let lines = build_contact_lines(&contact, theme, body_area.width as usize);
    let height = body_area.height as usize;
    ui.mail_scroll.total_lines = lines.len();
    let offset = ui.mail_scroll.offset(height);
    ui.mail_scroll.offset = offset;
    chat_view::render_scrolled_lines(frame, body_area, &lines, offset, height, hitmap);
}

fn draw_todo_pane(
    frame: &mut Frame,
    inner: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let buttons = vec![
        ("[☑ 완료전환]", ButtonId::TodoToggle),
        ("[+ 새 할일]", ButtonId::NewTodo),
        ("[삭제]", ButtonId::DeleteTodo),
    ];
    draw_toolbar_buttons(frame, header_area, theme, hitmap, &buttons);

    let has_input_bar = inner.height >= 4;
    let body_h = if has_input_bar {
        inner.height.saturating_sub(3)
    } else {
        inner.height.saturating_sub(1)
    };
    let body_area = Rect::new(inner.x, inner.y.saturating_add(1), inner.width, body_h);
    ui.mail_scroll.area = body_area;

    let lines = build_todo_lines(
        app.active_todo_list(ui),
        app.active_todo_task(),
        &app.todo_tasks,
        ui.todo_include_completed,
        theme,
        body_area.width as usize,
    );

    if body_area.height > 0 {
        let height = body_area.height as usize;
        ui.mail_scroll.total_lines = lines.len();
        let offset = ui.mail_scroll.offset(height);
        ui.mail_scroll.offset = offset;
        chat_view::render_scrolled_lines(frame, body_area, &lines, offset, height, hitmap);
    }

    if has_input_bar {
        let input_area = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(2),
            inner.width,
            2,
        );
        let input_focused = ui.focus == Focus::Chat && ui.todo_input_active;
        let prompt = if input_focused {
            format!("새 할 일 > {}_", ui.todo_input)
        } else if ui.todo_input.is_empty() {
            "n: 빠른 할 일 추가 · Space: 완료 전환 · c: 완료 항목 표시/숨김 · [/]: 목록 전환"
                .to_string()
        } else {
            format!("새 할 일 > {}", ui.todo_input)
        };
        let style = if input_focused {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style()
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "─".repeat(input_area.width as usize),
                    theme.muted_style(),
                )),
                Line::from(Span::styled(
                    crate::ui::truncate_width(&prompt, input_area.width as usize),
                    style,
                )),
            ]),
            input_area,
        );
        hitmap.add(input_area, HitTarget::Button(ButtonId::TodoInputFocus));
    }
}

pub fn build_mail_lines(
    mail: &MailMessage,
    cached_thumbs: &[(String, RenderedTerminalImage)],
    inline_images: bool,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) -> Vec<RenderedLine> {
    let width = width.max(16);
    let mut out = Vec::new();
    let subj = if mail.subject.trim().is_empty() {
        "(제목 없음)"
    } else {
        mail.subject.as_str()
    };

    let mut subj_spans = vec![Span::styled(
        crate::ui::truncate_width(subj, width.saturating_sub(8)),
        Style::default()
            .fg(theme.fg)
            .add_modifier(Modifier::BOLD),
    )];
    if !mail.is_read {
        subj_spans.push(Span::styled(
            " [안읽음]",
            Style::default()
                .fg(theme.unread)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if mail.flagged {
        subj_spans.push(Span::styled(
            " ★",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ));
    }
    out.push(RenderedLine::plain(Line::from(subj_spans)));

    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled("보낸사람: ", theme.muted_style()),
        Span::styled(mail.from_display(), theme.accent_style()),
    ])));

    if !mail.to_recipients.is_empty() {
        let to_list = mail
            .to_recipients
            .iter()
            .map(|r| r.display())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("받는사람: ", theme.muted_style()),
            Span::raw(crate::ui::truncate_width(&to_list, width.saturating_sub(10))),
        ])));
    }
    if !mail.cc_recipients.is_empty() {
        let cc_list = mail
            .cc_recipients
            .iter()
            .map(|r| r.display())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("참    조: ", theme.muted_style()),
            Span::raw(crate::ui::truncate_width(&cc_list, width.saturating_sub(10))),
        ])));
    }
    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled("수신시각: ", theme.muted_style()),
        Span::raw(format_iso_pretty(&mail.received_at)),
    ])));
    out.push(RenderedLine::plain(Line::from(Span::styled(
        "─".repeat(width.min(64)),
        theme.muted_style(),
    ))));

    let mut blocks = parser::parse_message_blocks(&mail.body_html, &[], &[]);
    if blocks.is_empty() && !mail.body_plain.trim().is_empty() {
        blocks = mail
            .body_plain
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| RichBlock::Paragraph(vec![RichSpan::plain(l)]))
            .collect();
    }
    if blocks.is_empty() && !mail.preview.trim().is_empty() {
        blocks.push(RichBlock::Paragraph(vec![RichSpan::plain(&mail.preview)]));
    }

    chat_view::render_rich_blocks(&mut out, &blocks, "", false, None, theme, width, hyperlinks);

    if !mail.attachments.is_empty() {
        out.push(RenderedLine::plain(Line::from("")));
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("── 첨부파일 ({}) ──", mail.attachments.len()),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ))));
        for (idx, att) in mail.attachments.iter().enumerate() {
            let is_img = img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name));
            let icon = if is_img { "🖼" } else { "📎" };
            let chip = format!("  [{icon} {} ({})]", att.name, format_bytes(att.size));
            let chip_w = UnicodeWidthStr::width(chip.trim_start()) as u16;
            out.push(RenderedLine::with_inline(
                Line::from(Span::styled(
                    crate::ui::truncate_width(&chip, width),
                    theme.accent_style(),
                )),
                None,
                vec![(
                    2,
                    chip_w,
                    HitTarget::Button(if is_img {
                        ButtonId::ImagePreview
                    } else {
                        ButtonId::Attachments
                    }),
                )],
            ));

            if is_img && inline_images {
                if let Some((_, rendered)) = cached_thumbs.iter().find(|(id, _)| id == &att.id) {
                    for thumb_line in &rendered.lines {
                        let mut full = vec![Span::raw("  ")];
                        full.extend(thumb_line.spans.clone());
                        out.push(RenderedLine::plain(Line::from(full)));
                    }
                    out.push(RenderedLine::plain(Line::from(Span::styled(
                        format!(
                            "  ↳ {}x{} ({}) · v 키로 크게보기 (#{})",
                            rendered.width_px,
                            rendered.height_px,
                            rendered.format_label,
                            idx + 1
                        ),
                        theme.muted_style(),
                    ))));
                }
            }
        }
    }

    out
}

pub fn build_calendar_lines(
    ev: &CalendarEvent,
    theme: &Theme,
    width: usize,
    hyperlinks: bool,
) -> Vec<RenderedLine> {
    let width = width.max(16);
    let mut out = Vec::new();
    let title = if ev.is_cancelled {
        format!("[취소됨] {}", ev.subject)
    } else {
        ev.subject.clone()
    };
    out.push(RenderedLine::plain(Line::from(Span::styled(
        crate::ui::truncate_width(&title, width),
        Style::default()
            .fg(theme.fg)
            .add_modifier(Modifier::BOLD),
    ))));
    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled("시    간: ", theme.muted_style()),
        Span::raw(if ev.is_all_day {
            format!("{} (종일)", format_iso_pretty(&ev.start_iso))
        } else {
            format!(
                "{} ~ {}",
                format_iso_pretty(&ev.start_iso),
                format_iso_pretty(&ev.end_iso)
            )
        }),
    ])));
    if let Some(loc) = ev.location.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("장    소: ", theme.muted_style()),
            Span::raw(loc.to_string()),
        ])));
    }
    if let Some(org) = ev.organizer_name.as_deref().or(ev.organizer_email.as_deref()) {
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("주 최 자: ", theme.muted_style()),
            Span::raw(org.to_string()),
        ])));
    }
    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled("내 응 답: ", theme.muted_style()),
        Span::styled(
            response_label(&ev.response_status),
            theme.accent_style(),
        ),
    ])));

    if let Some(join) = AppState::event_join_url(ev) {
        out.push(RenderedLine::with_inline(
            Line::from(vec![
                Span::styled("온라인회의: ", theme.muted_style()),
                Span::styled(
                    "[📹 회의 참가 (j / Enter)]",
                    theme.accent_style().add_modifier(Modifier::BOLD),
                ),
            ]),
            None,
            vec![(11, 23, HitTarget::Button(ButtonId::JoinMeeting))],
        ));
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("  {}", crate::ui::truncate_width(join, width.saturating_sub(2))),
            theme.muted_style(),
        ))));
    }

    if !ev.attendees.is_empty() {
        out.push(RenderedLine::plain(Line::from("")));
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("── 참석자 ({}) ──", ev.attendees.len()),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ))));
        for att in &ev.attendees {
            let mark = match att.response_status.as_str() {
                "accepted" => "✓",
                "tentativelyAccepted" => "?",
                "declined" => "✕",
                "organizer" => "★",
                _ => "·",
            };
            let who = att
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|n| format!("{n} <{}>", att.address))
                .unwrap_or_else(|| att.address.clone());
            out.push(RenderedLine::plain(Line::from(vec![
                Span::styled(format!("  {mark} "), theme.accent_style()),
                Span::raw(crate::ui::truncate_width(&who, width.saturating_sub(4))),
            ])));
        }
    }

    let mut blocks = parser::parse_message_blocks(&ev.body_html, &[], &[]);
    if blocks.is_empty() && !ev.body_plain.trim().is_empty() {
        blocks = ev
            .body_plain
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| RichBlock::Paragraph(vec![RichSpan::plain(l)]))
            .collect();
    }
    if !blocks.is_empty() {
        out.push(RenderedLine::plain(Line::from("")));
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "── 내용 ──",
            theme.muted_style(),
        ))));
        chat_view::render_rich_blocks(&mut out, &blocks, "", false, None, theme, width, hyperlinks);
    }

    out
}

pub fn build_contact_lines(
    contact: &ContactItem,
    theme: &Theme,
    width: usize,
) -> Vec<RenderedLine> {
    let width = width.max(16);
    let mut out = Vec::new();
    let badge = if contact.source == "directory" {
        "[조직 디렉터리]"
    } else {
        "[개인 연락처]"
    };
    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled(
            crate::ui::truncate_width(&contact.display_name, width.saturating_sub(16)),
            Style::default()
                .fg(theme.fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(badge, theme.accent_style()),
    ])));
    out.push(RenderedLine::plain(Line::from(Span::styled(
        "─".repeat(width.min(52)),
        theme.muted_style(),
    ))));

    if contact.email_addresses.is_empty() {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "이메일: (없음)",
            theme.muted_style(),
        ))));
    } else {
        for (i, email) in contact.email_addresses.iter().enumerate() {
            let label = if i == 0 { "이 메 일: " } else { "          " };
            out.push(RenderedLine::plain(Line::from(vec![
                Span::styled(label, theme.muted_style()),
                Span::styled(email.clone(), theme.accent_style()),
            ])));
        }
    }

    if let Some(mobile) = contact.mobile_phone.as_deref().filter(|s| !s.is_empty()) {
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("휴 대 폰: ", theme.muted_style()),
            Span::raw(mobile.to_string()),
        ])));
    }
    for phone in &contact.business_phones {
        if !phone.trim().is_empty() {
            out.push(RenderedLine::plain(Line::from(vec![
                Span::styled("회사전화: ", theme.muted_style()),
                Span::raw(phone.clone()),
            ])));
        }
    }
    if let Some(comp) = contact.company_name.as_deref().filter(|s| !s.is_empty()) {
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("회    사: ", theme.muted_style()),
            Span::raw(comp.to_string()),
        ])));
    }
    if let Some(dept) = contact.department.as_deref().filter(|s| !s.is_empty()) {
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("부    서: ", theme.muted_style()),
            Span::raw(dept.to_string()),
        ])));
    }
    if let Some(title) = contact.job_title.as_deref().filter(|s| !s.is_empty()) {
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("직    책: ", theme.muted_style()),
            Span::raw(title.to_string()),
        ])));
    }
    if let Some(notes) = contact.notes.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(RenderedLine::plain(Line::from("")));
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "── 메모 ──",
            theme.muted_style(),
        ))));
        for line in notes.lines() {
            out.push(RenderedLine::plain(Line::from(Span::raw(
                crate::ui::truncate_width(line, width),
            ))));
        }
    }
    out
}

pub fn build_todo_lines(
    list: Option<&TodoList>,
    active_task: Option<&TodoTask>,
    tasks: &[TodoTask],
    include_completed: bool,
    theme: &Theme,
    width: usize,
) -> Vec<RenderedLine> {
    let width = width.max(16);
    let mut out = Vec::new();
    let list_name = list.map(|l| l.display_name.as_str()).unwrap_or("기본 목록");
    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled(
            format!("📋 {list_name}"),
            theme.accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  (항목 {}개 · 완료됨: {})",
                tasks.len(),
                if include_completed { "표시" } else { "숨김" }
            ),
            theme.muted_style(),
        ),
    ])));
    out.push(RenderedLine::plain(Line::from(Span::styled(
        "─".repeat(width.min(56)),
        theme.muted_style(),
    ))));

    if let Some(task) = active_task {
        let box_icon = if task.is_completed() { "[x] " } else { "[ ] " };
        let star = if task.importance.eq_ignore_ascii_case("high") {
            "★ "
        } else {
            ""
        };
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled(box_icon, theme.accent_style()),
            Span::styled(
                format!("{star}{}", crate::ui::truncate_width(&task.title, width.saturating_sub(6))),
                Style::default()
                    .fg(theme.fg)
                    .add_modifier(Modifier::BOLD),
            ),
        ])));
        out.push(RenderedLine::plain(Line::from(vec![
            Span::styled("상    태: ", theme.muted_style()),
            Span::raw(if task.is_completed() {
                "완료됨"
            } else {
                task.status.as_str()
            }
            .to_string()),
        ])));
        if let Some(due) = task.due_date_iso.as_deref() {
            out.push(RenderedLine::plain(Line::from(vec![
                Span::styled("마 감 일: ", theme.muted_style()),
                Span::raw(format_iso_pretty(due)),
            ])));
        }
        if let Some(body) = task.body_plain.as_deref().filter(|s| !s.trim().is_empty()) {
            out.push(RenderedLine::plain(Line::from("")));
            for line in body.lines() {
                out.push(RenderedLine::plain(Line::from(Span::raw(
                    crate::ui::truncate_width(line, width),
                ))));
            }
        }
    } else if tasks.is_empty() {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "등록된 할 일이 없습니다. 아래 입력창(n 키)에서 바로 추가할 수 있습니다.",
            theme.muted_style(),
        ))));
    } else {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "왼쪽 목록에서 할 일을 선택하세요 (Space: 완료/미완료 전환)",
            theme.muted_style(),
        ))));
    }

    out
}

fn format_iso_pretty(iso: &str) -> String {
    if iso.len() >= 16 {
        format!("{} {}", &iso[..10], &iso[11..16])
    } else {
        iso.to_string()
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn response_label(status: &str) -> &'static str {
    match status {
        "accepted" => "수락함",
        "tentativelyAccepted" => "미정(임시 수락)",
        "declined" => "거절함",
        "organizer" => "주최자",
        "notResponded" => "미응답",
        _ => "없음",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EmailRecipient, MailAttachment};

    #[test]
    fn mail_lines_include_metadata_and_inline_image_caption() {
        let mail = MailMessage {
            id: "m1".to_string(),
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: "주간 보고".to_string(),
            from: Some(EmailRecipient {
                name: Some("홍길동".to_string()),
                address: "hong@example.com".to_string(),
            }),
            to_recipients: vec![EmailRecipient {
                name: None,
                address: "me@example.com".to_string(),
            }],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            received_at: "2026-11-15T09:30:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: true,
            importance: "normal".to_string(),
            has_attachments: true,
            preview: "미리보기".to_string(),
            body_html: "<p>안녕하세요 <b>Teams</b>!</p>".to_string(),
            body_plain: String::new(),
            attachments: vec![MailAttachment {
                id: "att-1".to_string(),
                name: "chart.png".to_string(),
                content_type: "image/png".to_string(),
                size: 2048,
                is_inline: false,
                content_id: None,
                content_bytes_b64: None,
            }],
            web_link: None,
        };
        let thumb = RenderedTerminalImage {
            width_px: 100,
            height_px: 50,
            cols: 20,
            rows: 5,
            format_label: "PNG".to_string(),
            lines: vec![Line::from("▀")],
        };
        let lines = build_mail_lines(
            &mail,
            &[("att-1".to_string(), thumb)],
            true,
            &Theme::default(),
            60,
            false,
        );
        let text: String = lines
            .iter()
            .map(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("주간 보고"));
        assert!(text.contains("chart.png"));
        assert!(text.contains("100x50 (PNG)"));
    }

    #[test]
    fn calendar_lines_show_meeting_join_chip() {
        let ev = CalendarEvent {
            id: "e1".to_string(),
            subject: "디자인 싱크".to_string(),
            start_iso: "2026-11-15T14:00:00Z".to_string(),
            end_iso: "2026-11-15T15:00:00Z".to_string(),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: Some("Teams".to_string()),
            organizer_name: Some("이순신".to_string()),
            organizer_email: None,
            attendees: vec![],
            response_status: "accepted".to_string(),
            is_online_meeting: true,
            online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/123".to_string()),
            body_html: String::new(),
            body_plain: "아젠다 확인".to_string(),
            is_cancelled: false,
            web_link: None,
        };
        let lines = build_calendar_lines(&ev, &Theme::default(), 70, false);
        let joined: String = lines
            .iter()
            .map(|l| {
                l.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("회의 참가"));
        assert!(joined.contains("아젠다 확인"));
    }
}
