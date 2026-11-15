//! Left navigation pane:
//! - Tab 0 (`Ctrl+1`): 채팅 (1:1 · 그룹 · 회의)
//! - Tab 1 (`Ctrl+2`): 팀/채널
//! - Tab 2 (`Ctrl+3`): 활동 (`@멘션` 모아보기)
//! - Tab 3 (`Ctrl+4`): 메일 (Outlook Mail folders & inbox list)
//! - Tab 4 (`Ctrl+5`): 캘린더 (Outlook Calendar agenda)
//! - Tab 5 (`Ctrl+6`): 연락처 (Contacts + Entra directory)
//! - Tab 6 (`Ctrl+7`): 할 일 (Microsoft To Do lists & tasks)

use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    models::ConversationKind,
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
    },
};

pub const TAB_CHATS: usize = 0;
pub const TAB_TEAMS: usize = 1;
pub const TAB_ACTIVITY: usize = 2;
pub const TAB_MAIL: usize = 3;
pub const TAB_CALENDAR: usize = 4;
pub const TAB_CONTACTS: usize = 5;
pub const TAB_TODO: usize = 6;
pub const TAB_COUNT: usize = 7;

pub const TAB_LABELS: [&str; TAB_COUNT] = [
    "채팅",
    "팀/채널",
    "활동",
    "메일",
    "캘린더",
    "연락처",
    "할 일",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    Chats = 0,
    Teams = 1,
    Activity = 2,
    Mail = 3,
    Calendar = 4,
    Contacts = 5,
    Todo = 6,
}

impl NavTab {
    pub fn from_index(idx: usize) -> Self {
        match idx {
            TAB_TEAMS => Self::Teams,
            TAB_ACTIVITY => Self::Activity,
            TAB_MAIL => Self::Mail,
            TAB_CALENDAR => Self::Calendar,
            TAB_CONTACTS => Self::Contacts,
            TAB_TODO => Self::Todo,
            _ => Self::Chats,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn is_outlook(self) -> bool {
        self.index() >= TAB_MAIL
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    Chat {
        conv_key: String,
        title: String,
        kind: ConversationKind,
        peer_user_id: Option<String>,
        unread: i32,
    },
    TeamHeader {
        team_id: String,
        display_name: String,
        collapsed: bool,
        channel_count: usize,
    },
    TeamChannel {
        conv_key: String,
        display_name: String,
        is_private: bool,
        unread: i32,
    },
    ActivityMention {
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
        author_name: String,
        preview: String,
        created_at: String,
    },
    MailFolderItem {
        folder_id: String,
        display_name: String,
        unread: i32,
        active: bool,
    },
    MailItem {
        mail_id: String,
        subject: String,
        from_display: String,
        is_read: bool,
        flagged: bool,
        has_attachments: bool,
        received_at: String,
    },
    CalendarItem {
        event_id: String,
        subject: String,
        start_iso: String,
        end_iso: String,
        is_all_day: bool,
        is_online: bool,
        response_status: String,
        is_cancelled: bool,
    },
    ContactRow {
        contact_id: String,
        display_name: String,
        email: Option<String>,
        source: String,
    },
    TodoListChip {
        list_id: String,
        display_name: String,
        active: bool,
    },
    TodoTaskRow {
        task_id: String,
        list_id: String,
        title: String,
        completed: bool,
        important: bool,
        due_date_iso: Option<String>,
    },
}

pub fn build_nav_rows(app: &AppState, ui: &UiState) -> Vec<NavRow> {
    match ui.active_nav_tab {
        TAB_CHATS => app
            .conversations
            .iter()
            .filter(|c| c.kind != ConversationKind::Channel)
            .map(|c| NavRow::Chat {
                conv_key: c.conv_key.clone(),
                title: c.title.clone(),
                kind: c.kind,
                peer_user_id: c.peer_user_id.clone(),
                unread: c.unread_count,
            })
            .collect(),
        TAB_TEAMS => {
            let mut rows = Vec::new();
            for team in &app.teams {
                let collapsed = ui.collapsed_team_ids.contains(&team.id);
                rows.push(NavRow::TeamHeader {
                    team_id: team.id.clone(),
                    display_name: team.display_name.clone(),
                    collapsed,
                    channel_count: team.channels.len(),
                });
                if !collapsed {
                    for ch in &team.channels {
                        let conv_key = ch.conv_key();
                        let unread = app
                            .conversations
                            .iter()
                            .find(|c| c.conv_key == conv_key)
                            .map(|c| c.unread_count)
                            .unwrap_or(0);
                        let is_private = !ch.membership_type.eq_ignore_ascii_case("standard");
                        rows.push(NavRow::TeamChannel {
                            conv_key,
                            display_name: ch.display_name.clone(),
                            is_private,
                            unread,
                        });
                    }
                }
            }
            rows
        }
        TAB_ACTIVITY => app
            .activity_mentions
            .iter()
            .map(|m| NavRow::ActivityMention {
                conv_key: m.conv_key.clone(),
                msg_id: m.msg_id.clone(),
                parent_msg_id: m.parent_msg_id.clone(),
                author_name: m.author_name.clone(),
                preview: m.body_plain.replace(['\n', '\r'], " "),
                created_at: m.created_at.clone(),
            })
            .collect(),
        TAB_MAIL => {
            let mut rows = Vec::new();
            for folder in app.mail_folders.iter().take(6) {
                let active = ui.active_mail_folder_id == folder.id
                    || folder
                        .well_known_name
                        .as_deref()
                        .is_some_and(|w| w.eq_ignore_ascii_case(&ui.active_mail_folder_id));
                rows.push(NavRow::MailFolderItem {
                    folder_id: folder.id.clone(),
                    display_name: folder.display_name.clone(),
                    unread: folder.unread_item_count,
                    active,
                });
            }
            for mail in &app.mails {
                rows.push(NavRow::MailItem {
                    mail_id: mail.id.clone(),
                    subject: mail.subject.clone(),
                    from_display: mail.from_display(),
                    is_read: mail.is_read,
                    flagged: mail.flagged,
                    has_attachments: mail.has_attachments,
                    received_at: mail.received_at.clone(),
                });
            }
            rows
        }
        TAB_CALENDAR => app
            .calendar_events
            .iter()
            .map(|ev| NavRow::CalendarItem {
                event_id: ev.id.clone(),
                subject: ev.subject.clone(),
                start_iso: ev.start_iso.clone(),
                end_iso: ev.end_iso.clone(),
                is_all_day: ev.is_all_day,
                is_online: ev.is_online_meeting || ev.online_meeting_url.is_some(),
                response_status: ev.response_status.clone(),
                is_cancelled: ev.is_cancelled,
            })
            .collect(),
        TAB_CONTACTS => app
            .contacts
            .iter()
            .map(|c| NavRow::ContactRow {
                contact_id: c.id.clone(),
                display_name: c.display_name.clone(),
                email: c.primary_email().map(str::to_string),
                source: c.source.clone(),
            })
            .collect(),
        TAB_TODO => {
            let mut rows = Vec::new();
            if app.todo_lists.len() > 1 {
                for list in app.todo_lists.iter().take(6) {
                    rows.push(NavRow::TodoListChip {
                        list_id: list.id.clone(),
                        display_name: list.display_name.clone(),
                        active: ui.active_todo_list_id.as_deref() == Some(list.id.as_str()),
                    });
                }
            }
            for task in &app.todo_tasks {
                rows.push(NavRow::TodoTaskRow {
                    task_id: task.id.clone(),
                    list_id: task.list_id.clone(),
                    title: task.title.clone(),
                    completed: task.is_completed(),
                    important: task.importance.eq_ignore_ascii_case("high"),
                    due_date_iso: task.due_date_iso.clone(),
                });
            }
            rows
        }
        _ => Vec::new(),
    }
}

pub fn tab_counts(app: &AppState) -> [usize; TAB_COUNT] {
    let chat_count = app
        .conversations
        .iter()
        .filter(|c| c.kind != ConversationKind::Channel)
        .count();
    let team_channel_count: usize = app.teams.iter().map(|t| t.channels.len()).sum();
    [
        chat_count,
        team_channel_count,
        app.activity_mentions.len(),
        app.mails.len(),
        app.calendar_events.len(),
        app.contacts.len(),
        app.todo_tasks.len(),
    ]
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Nav;
    let rows = build_nav_rows(app, ui);
    let label = TAB_LABELS.get(ui.active_nav_tab).copied().unwrap_or("채팅");

    let count_badge = match ui.active_nav_tab {
        TAB_MAIL => app.mails.len(),
        TAB_TODO => app.todo_tasks.len(),
        _ => rows.len(),
    };
    let title = if ui.active_nav_tab == TAB_MAIL {
        format!(" {} · {count_badge} ", ui.active_mail_folder_name)
    } else if ui.active_nav_tab == TAB_TODO {
        let list_name = app
            .active_todo_list(ui)
            .map(|l| l.display_name.as_str())
            .unwrap_or(label);
        format!(" {list_name} · {count_badge} ")
    } else {
        format!(" {label} {count_badge} ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            truncate(&title, area.width.saturating_sub(2) as usize),
            theme.title(focused),
        )))
        .border_style(theme.border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let show_footer = inner.height > 1;
    let list_area = if show_footer {
        Rect::new(inner.x, inner.y, inner.width, inner.height - 1)
    } else {
        inner
    };

    if rows.is_empty() {
        let empty_msg = match ui.active_nav_tab {
            TAB_CHATS => "채팅이 없습니다 (Ctrl+N 새 채팅)",
            TAB_TEAMS => "가입된 팀/채널이 없습니다",
            TAB_ACTIVITY => "최근 @멘션 활동이 없습니다",
            TAB_MAIL => "표시할 메일이 없습니다 (c 새 메일)",
            TAB_CALENDAR => "예정된 일정이 없습니다 (n 새 일정)",
            TAB_CONTACTS => "연락처가 없습니다 (n 추가 · / 검색)",
            TAB_TODO => "할 일이 없습니다 (n 추가)",
            _ => "항목 없음",
        };
        frame.render_widget(
            Paragraph::new(truncate(empty_msg, list_area.width as usize)).style(theme.muted_style()),
            list_area,
        );
    } else if list_area.width > 0 && list_area.height > 0 {
        let selected = ui.nav_selected.min(rows.len().saturating_sub(1));
        let height = list_area.height as usize;
        let offset = selection_offset(selected, height, rows.len());
        let width = list_area.width as usize;

        let mut lines = Vec::with_capacity(height);
        for (idx, row) in rows.iter().enumerate().skip(offset).take(height) {
            let is_selected = idx == selected;
            let is_active = match row {
                NavRow::Chat { conv_key, .. } | NavRow::TeamChannel { conv_key, .. } => {
                    app.active_conv_key.as_deref() == Some(conv_key.as_str())
                }
                NavRow::MailFolderItem { active, .. } | NavRow::TodoListChip { active, .. } => {
                    *active
                }
                NavRow::MailItem { mail_id, .. } => {
                    app.active_mail_id.as_deref() == Some(mail_id.as_str())
                }
                NavRow::CalendarItem { event_id, .. } => {
                    app.active_event_id.as_deref() == Some(event_id.as_str())
                }
                NavRow::ContactRow { contact_id, .. } => {
                    app.active_contact_id.as_deref() == Some(contact_id.as_str())
                }
                NavRow::TodoTaskRow { task_id, .. } => {
                    app.active_todo_task_id.as_deref() == Some(task_id.as_str())
                }
                _ => false,
            };
            lines.push(render_nav_row(row, width, is_selected, is_active, app, ui));
            hitmap.add_line(
                list_area.x,
                list_area.y + lines.len() as u16 - 1,
                list_area.width,
                HitTarget::NavRow(idx),
            );
        }
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    if show_footer {
        draw_footer(frame, inner, ui, hitmap);
    }
}

fn draw_footer(frame: &mut Frame, inner: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let y = inner.y + inner.height - 1;
    let (btn, hint, button_id) = match ui.active_nav_tab {
        TAB_MAIL => ("[+ 새 메일]", " c", ButtonId::NewMail),
        TAB_CALENDAR => ("[+ 새 일정]", " n", ButtonId::NewEvent),
        TAB_CONTACTS => ("[+ 새 연락처]", " n", ButtonId::NewContact),
        TAB_TODO => ("[+ 할 일 추가]", " n", ButtonId::NewTodo),
        _ => ("[+ 새 채팅]", " Ctrl+N", ButtonId::NewChat),
    };
    let btn_w = UnicodeWidthStr::width(btn) as u16;
    let mut spans = vec![Span::styled(
        btn,
        theme.accent_style().add_modifier(Modifier::BOLD),
    )];
    if inner.width > btn_w + UnicodeWidthStr::width(hint) as u16 + 1 {
        spans.push(Span::styled(hint, theme.muted_style()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    hitmap.add_line(
        inner.x,
        y,
        btn_w.min(inner.width),
        HitTarget::Button(button_id),
    );
}

fn render_nav_row(
    row: &NavRow,
    width: usize,
    selected: bool,
    active: bool,
    app: &AppState,
    ui: &UiState,
) -> Line<'static> {
    let theme = &ui.theme;
    let marker = if selected {
        "▸ "
    } else if active {
        "• "
    } else {
        "  "
    };

    let base_style = if selected {
        theme.selection()
    } else if active {
        theme.accent_style().add_modifier(Modifier::BOLD)
    } else {
        ratatui::style::Style::default().fg(theme.fg)
    };

    match row {
        NavRow::Chat {
            title,
            kind,
            peer_user_id,
            unread,
            ..
        } => {
            let (prefix_glyph, glyph_color) = match kind {
                ConversationKind::OneOnOne => {
                    let avail = peer_user_id
                        .as_deref()
                        .and_then(|id| app.presences.get(id))
                        .map(|p| p.availability.as_str());
                    let (dot, color) = theme.presence_indicator(avail);
                    (format!("{dot} "), color)
                }
                ConversationKind::Group => ("[그룹] ".to_string(), theme.accent),
                ConversationKind::Meeting => ("[회의] ".to_string(), theme.reply),
                ConversationKind::Channel => ("# ".to_string(), theme.accent),
            };
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let used_fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(prefix_glyph.as_str())
                + UnicodeWidthStr::width(badge.as_str());
            let name_w = width.saturating_sub(used_fixed).max(4);
            let name = truncate(title, name_w);
            let pad = width
                .saturating_sub(used_fixed + UnicodeWidthStr::width(name.as_str()))
                .max(0);

            let glyph_style = if selected {
                theme.selection()
            } else {
                ratatui::style::Style::default().fg(glyph_color)
            };
            let name_style = if !selected && *unread > 0 {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), base_style),
                Span::styled(prefix_glyph, glyph_style),
                Span::styled(name, name_style),
                Span::styled(" ".repeat(pad), base_style),
            ];
            if !badge.is_empty() {
                let badge_style = if selected {
                    theme.selection()
                } else {
                    ratatui::style::Style::default()
                        .fg(theme.unread)
                        .add_modifier(Modifier::BOLD)
                };
                spans.push(Span::styled(badge, badge_style));
            }
            Line::from(spans)
        }
        NavRow::TeamHeader {
            display_name,
            collapsed,
            channel_count,
            ..
        } => {
            let arrow = if *collapsed { "▸ " } else { "▾ " };
            let suffix = format!(" ({channel_count})");
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(arrow)
                + UnicodeWidthStr::width(suffix.as_str());
            let name = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else {
                theme.accent_style().add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(arrow, style),
                Span::styled(name, style),
                Span::styled(suffix, if selected { style } else { theme.muted_style() }),
            ])
        }
        NavRow::TeamChannel {
            display_name,
            is_private,
            unread,
            ..
        } => {
            let icon = if *is_private { "  🔒 " } else { "  # " };
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(icon)
                + UnicodeWidthStr::width(badge.as_str());
            let name = truncate(display_name, width.saturating_sub(fixed).max(4));
            let pad = width
                .saturating_sub(fixed + UnicodeWidthStr::width(name.as_str()))
                .max(0);
            let name_style = if !selected && *unread > 0 {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), base_style),
                Span::styled(icon, if selected { base_style } else { theme.muted_style() }),
                Span::styled(name, name_style),
                Span::styled(" ".repeat(pad), base_style),
            ];
            if !badge.is_empty() {
                let badge_style = if selected {
                    theme.selection()
                } else {
                    ratatui::style::Style::default()
                        .fg(theme.unread)
                        .add_modifier(Modifier::BOLD)
                };
                spans.push(Span::styled(badge, badge_style));
            }
            Line::from(spans)
        }
        NavRow::ActivityMention {
            author_name,
            preview,
            ..
        } => {
            let label = format!("@{author_name}: {preview}");
            let body = truncate(&label, width.saturating_sub(UnicodeWidthStr::width(marker)));
            Line::from(vec![
                Span::styled(marker.to_string(), base_style),
                Span::styled(body, base_style),
            ])
        }
        NavRow::MailFolderItem {
            display_name,
            unread,
            active: folder_active,
            ..
        } => {
            let icon = if *folder_active { "📂 " } else { "📁 " };
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(icon)
                + UnicodeWidthStr::width(badge.as_str());
            let name = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else if *folder_active {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), style),
                Span::styled(icon, style),
                Span::styled(name, style),
            ];
            if !badge.is_empty() {
                spans.push(Span::styled(
                    badge,
                    if selected {
                        theme.selection()
                    } else {
                        ratatui::style::Style::default().fg(theme.unread)
                    },
                ));
            }
            Line::from(spans)
        }
        NavRow::MailItem {
            subject,
            from_display,
            is_read,
            flagged,
            has_attachments,
            ..
        } => {
            let status = match (*is_read, *flagged) {
                (false, true) => "●★ ",
                (false, false) => "● ",
                (true, true) => " ★ ",
                (true, false) => "  ",
            };
            let attach_tag = if *has_attachments { " 📎" } else { "" };
            let sender_short = from_display
                .split('<')
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(from_display.as_str());
            let subj_display = if subject.trim().is_empty() {
                "(제목 없음)"
            } else {
                subject.as_str()
            };
            let combined = format!("{sender_short}: {subj_display}");
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(status)
                + UnicodeWidthStr::width(attach_tag);
            let body = truncate(&combined, width.saturating_sub(fixed).max(4));
            let name_style = if !selected && !*is_read {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let status_style = if selected {
                theme.selection()
            } else if !*is_read {
                ratatui::style::Style::default()
                    .fg(theme.unread)
                    .add_modifier(Modifier::BOLD)
            } else if *flagged {
                ratatui::style::Style::default().fg(theme.warning)
            } else {
                theme.muted_style()
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), base_style),
                Span::styled(status, status_style),
                Span::styled(body, name_style),
            ];
            if !attach_tag.is_empty() {
                spans.push(Span::styled(
                    attach_tag,
                    if selected {
                        theme.selection()
                    } else {
                        theme.muted_style()
                    },
                ));
            }
            Line::from(spans)
        }
        NavRow::CalendarItem {
            subject,
            start_iso,
            is_all_day,
            is_online,
            response_status,
            is_cancelled,
            ..
        } => {
            let when = format_event_nav_time(start_iso, *is_all_day);
            let icon = if *is_cancelled {
                "✖ "
            } else if *is_online {
                "📹 "
            } else if response_status.eq_ignore_ascii_case("declined") {
                "⊘ "
            } else {
                "📅 "
            };
            let text = format!("{when} {subject}");
            let fixed = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(icon);
            let body = truncate(&text, width.saturating_sub(fixed).max(4));
            let style = if *is_cancelled && !selected {
                theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
            } else {
                base_style
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(
                    icon,
                    if selected {
                        style
                    } else {
                        ratatui::style::Style::default().fg(theme.accent)
                    },
                ),
                Span::styled(body, style),
            ])
        }
        NavRow::ContactRow {
            display_name,
            email,
            source,
            ..
        } => {
            let icon = if source == "directory" { "🏢 " } else { "👤 " };
            let detail = match email {
                Some(e) if !e.is_empty() => format!("{display_name} ({e})"),
                _ => display_name.clone(),
            };
            let fixed = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(icon);
            let body = truncate(&detail, width.saturating_sub(fixed).max(4));
            Line::from(vec![
                Span::styled(marker.to_string(), base_style),
                Span::styled(
                    icon,
                    if selected {
                        base_style
                    } else {
                        ratatui::style::Style::default().fg(theme.accent)
                    },
                ),
                Span::styled(body, base_style),
            ])
        }
        NavRow::TodoListChip {
            display_name,
            active: list_active,
            ..
        } => {
            let icon = if *list_active { "▾ 📋 " } else { "▸ 📋 " };
            let fixed = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(icon);
            let body = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else if *list_active {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(icon, style),
                Span::styled(body, style),
            ])
        }
        NavRow::TodoTaskRow {
            title,
            completed,
            important,
            due_date_iso,
            ..
        } => {
            let box_icon = if *completed { "[x] " } else { "[ ] " };
            let star = if *important { "★ " } else { "" };
            let due = due_date_iso
                .as_deref()
                .and_then(|s| s.get(..10))
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            let full = format!("{star}{title}{due}");
            let fixed = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(box_icon);
            let body = truncate(&full, width.saturating_sub(fixed).max(4));
            let style = if *completed && !selected {
                theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
            } else if *important && !selected {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(
                    box_icon,
                    if selected {
                        style
                    } else if *completed {
                        ratatui::style::Style::default().fg(theme.success)
                    } else {
                        ratatui::style::Style::default().fg(theme.accent)
                    },
                ),
                Span::styled(body, style),
            ])
        }
    }
}

fn format_event_nav_time(start_iso: &str, is_all_day: bool) -> String {
    let date_part = start_iso.get(5..10).unwrap_or("");
    if is_all_day {
        if date_part.is_empty() {
            "종일".to_string()
        } else {
            format!("{date_part} 종일")
        }
    } else if let Some(time_part) = start_iso.get(11..16) {
        if date_part.is_empty() {
            time_part.to_string()
        } else {
            format!("{date_part} {time_part}")
        }
    } else {
        date_part.to_string()
    }
}

pub fn selection_offset(selected: usize, height: usize, total: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        0
    } else {
        (selected + 1)
            .saturating_sub(height)
            .min(total.saturating_sub(height))
    }
}

fn truncate(text: &str, width: usize) -> String {
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

    #[test]
    fn seven_tabs_map_roundtrip() {
        assert_eq!(TAB_LABELS.len(), TAB_COUNT);
        for i in 0..TAB_COUNT {
            let tab = NavTab::from_index(i);
            assert_eq!(tab.index(), i);
            assert_eq!(tab.is_outlook(), i >= TAB_MAIL);
        }
    }

    #[test]
    fn calendar_time_formats_concisely() {
        assert_eq!(
            format_event_nav_time("2026-11-15T14:30:00Z", false),
            "11-15 14:30"
        );
        assert_eq!(
            format_event_nav_time("2026-11-15T00:00:00Z", true),
            "11-15 종일"
        );
    }
}
