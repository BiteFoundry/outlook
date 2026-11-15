//! Left navigation pane:
//! - Tab 0 (`Ctrl+1`): 채팅 (1:1 · 그룹 · 회의)
//! - Tab 1 (`Ctrl+2`): 팀/채널
//! - Tab 2 (`Ctrl+3`): 활동 (`@멘션` 모아보기)
//! - Tab 3 (`Ctrl+4`): 메일 (Outlook Mail folders & inbox list)
//! - Tab 4 (`Ctrl+5`): 캘린더 (Outlook Calendar agenda)
//! - Tab 5 (`Ctrl+6`): 연락처 (Contacts + Entra directory)
//! - Tab 6 (`Ctrl+7`): 할 일 (Microsoft To Do lists & tasks)
//! - Tab 7 (`Ctrl+8`): 노트 (Microsoft OneNote notebooks, section groups & sections)

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    models::{ConversationKind, OnenoteNotebook, OnenoteSection},
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
pub const TAB_NOTES: usize = 7;
pub const TAB_COUNT: usize = 8;

/// Signature OneNote Purple accent (`#8A3FFC` bright highlight & `#7719AA` brand purple).
pub const ONENOTE_PURPLE: Color = Color::Rgb(138, 63, 252);
pub const ONENOTE_BRAND_BG: Color = Color::Rgb(119, 25, 170);

/// Crisp 6-color OneNote palette: `[Purple, Cobalt, Emerald, Teal, Amber, Coral]`.
pub const ONENOTE_PALETTE: [Color; 6] = [
    Color::Rgb(138, 63, 252), // Purple
    Color::Rgb(66, 153, 255), // Cobalt
    Color::Rgb(52, 199, 123), // Emerald
    Color::Rgb(34, 197, 194), // Teal
    Color::Rgb(245, 176, 65), // Amber
    Color::Rgb(242, 107, 107), // Coral
];

pub const TAB_LABELS: [&str; TAB_COUNT] = [
    "채팅",
    "팀/채널",
    "활동",
    "메일",
    "캘린더",
    "연락처",
    "할 일",
    "노트",
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
    Notes = 7,
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
            TAB_NOTES => Self::Notes,
            _ => Self::Chats,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn is_outlook(self) -> bool {
        matches!(
            self,
            Self::Mail | Self::Calendar | Self::Contacts | Self::Todo
        )
    }

    pub fn is_onenote(self) -> bool {
        self == Self::Notes
    }
}

/// Resolves a OneNote notebook/section badge color from an optional hex/named color or deterministic hash.
pub fn onenote_badge_color(color_hint: Option<&str>, fallback_key: &str) -> Color {
    if let Some(raw) = color_hint.map(str::trim).filter(|s| !s.is_empty()) {
        let hex = raw.strip_prefix('#').unwrap_or(raw);
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return Color::Rgb(r, g, b);
            }
        }
        let lower = raw.to_ascii_lowercase();
        if lower.contains("purple") || lower.contains("violet") || lower.contains("lilac") {
            return ONENOTE_PALETTE[0];
        }
        if lower.contains("blue") || lower.contains("cobalt") || lower.contains("cyan") {
            return ONENOTE_PALETTE[1];
        }
        if lower.contains("green") || lower.contains("emerald") || lower.contains("lime") {
            return ONENOTE_PALETTE[2];
        }
        if lower.contains("teal") || lower.contains("aqua") || lower.contains("turquoise") {
            return ONENOTE_PALETTE[3];
        }
        if lower.contains("yellow")
            || lower.contains("amber")
            || lower.contains("orange")
            || lower.contains("gold")
            || lower.contains("mango")
        {
            return ONENOTE_PALETTE[4];
        }
        if lower.contains("red")
            || lower.contains("coral")
            || lower.contains("pink")
            || lower.contains("magenta")
            || lower.contains("rose")
        {
            return ONENOTE_PALETTE[5];
        }
    }
    let mut hash: usize = 5381;
    for b in fallback_key.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as usize);
    }
    ONENOTE_PALETTE[hash % ONENOTE_PALETTE.len()]
}

pub fn section_belongs_to_notebook(sec: &OnenoteSection, nb: &OnenoteNotebook) -> bool {
    if sec.scope.scope_key() != nb.scope.scope_key() {
        return false;
    }
    match sec.notebook_id.as_deref() {
        Some(nb_id) => nb_id == nb.id || nb_id == nb.notebook_key,
        None => false,
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
    OnenoteGroupHeader {
        label: String,
        personal: bool,
        count: usize,
    },
    OnenoteNotebook {
        notebook_key: String,
        display_name: String,
        color: Option<String>,
        collapsed: bool,
        section_count: usize,
        owner_hint: Option<String>,
    },
    OnenoteSectionGroup {
        notebook_key: String,
        display_name: String,
    },
    OnenoteSection {
        section_key: String,
        notebook_key: Option<String>,
        display_name: String,
        color: Option<String>,
        in_section_group: bool,
    },
}

fn push_notebook_tree_rows(
    rows: &mut Vec<NavRow>,
    nb: &OnenoteNotebook,
    all_sections: &[OnenoteSection],
    collapsed_keys: &std::collections::HashSet<String>,
    include_owner_hint: bool,
) {
    let nb_sections: Vec<&OnenoteSection> = all_sections
        .iter()
        .filter(|sec| section_belongs_to_notebook(sec, nb))
        .collect();
    let collapsed = collapsed_keys.contains(&nb.notebook_key);
    let owner_hint = if include_owner_hint {
        let lbl = nb.scope.owner_label();
        if !lbl.is_empty() && lbl != nb.display_name {
            Some(lbl)
        } else {
            None
        }
    } else {
        None
    };

    rows.push(NavRow::OnenoteNotebook {
        notebook_key: nb.notebook_key.clone(),
        display_name: nb.display_name.clone(),
        color: nb.color.clone(),
        collapsed,
        section_count: nb_sections.len(),
        owner_hint,
    });

    if !collapsed {
        let mut last_group: Option<&str> = None;
        for sec in nb_sections {
            if let Some(grp) = sec
                .parent_section_group_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if last_group != Some(grp) {
                    rows.push(NavRow::OnenoteSectionGroup {
                        notebook_key: nb.notebook_key.clone(),
                        display_name: grp.to_string(),
                    });
                    last_group = Some(grp);
                }
                rows.push(NavRow::OnenoteSection {
                    section_key: sec.section_key.clone(),
                    notebook_key: Some(nb.notebook_key.clone()),
                    display_name: sec.display_name.clone(),
                    color: sec.color.clone(),
                    in_section_group: true,
                });
            } else {
                last_group = None;
                rows.push(NavRow::OnenoteSection {
                    section_key: sec.section_key.clone(),
                    notebook_key: Some(nb.notebook_key.clone()),
                    display_name: sec.display_name.clone(),
                    color: sec.color.clone(),
                    in_section_group: false,
                });
            }
        }
    }
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
        TAB_NOTES => {
            let mut rows = Vec::new();
            let personal_nbs: Vec<&OnenoteNotebook> = app
                .onenote_notebooks
                .iter()
                .filter(|nb| nb.scope.is_personal() && !nb.is_shared)
                .collect();
            let shared_nbs: Vec<&OnenoteNotebook> = app
                .onenote_notebooks
                .iter()
                .filter(|nb| !nb.scope.is_personal() || nb.is_shared)
                .collect();

            if !personal_nbs.is_empty() {
                rows.push(NavRow::OnenoteGroupHeader {
                    label: "📓 내 필기장".to_string(),
                    personal: true,
                    count: personal_nbs.len(),
                });
                for nb in &personal_nbs {
                    push_notebook_tree_rows(
                        &mut rows,
                        nb,
                        &app.onenote_sections,
                        &ui.collapsed_notebook_keys,
                        false,
                    );
                }
            }

            if !shared_nbs.is_empty() {
                rows.push(NavRow::OnenoteGroupHeader {
                    label: "👥 팀·공유 필기장".to_string(),
                    personal: false,
                    count: shared_nbs.len(),
                });
                for nb in &shared_nbs {
                    push_notebook_tree_rows(
                        &mut rows,
                        nb,
                        &app.onenote_sections,
                        &ui.collapsed_notebook_keys,
                        true,
                    );
                }
            }

            // Fallback: include any orphan sections not tied to a known notebook
            for sec in &app.onenote_sections {
                let matched = app
                    .onenote_notebooks
                    .iter()
                    .any(|nb| section_belongs_to_notebook(sec, nb));
                if !matched {
                    rows.push(NavRow::OnenoteSection {
                        section_key: sec.section_key.clone(),
                        notebook_key: None,
                        display_name: sec.display_name.clone(),
                        color: sec.color.clone(),
                        in_section_group: false,
                    });
                }
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
        app.onenote_notebooks.len(),
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
        TAB_NOTES => app.onenote_notebooks.len(),
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
    } else if ui.active_nav_tab == TAB_NOTES {
        format!(" 📓 필기장 · {count_badge} ")
    } else {
        format!(" {label} {count_badge} ")
    };

    let is_onenote = ui.active_nav_tab == TAB_NOTES;
    let title_style = if is_onenote && focused {
        Style::default()
            .fg(ONENOTE_PURPLE)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.title(focused)
    };
    let border_style = if is_onenote && focused {
        Style::default().fg(ONENOTE_PURPLE)
    } else {
        theme.border_style(focused)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            truncate(&title, area.width.saturating_sub(2) as usize),
            title_style,
        )))
        .border_style(border_style);
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
            TAB_NOTES => "필기장이 없습니다 (B 새 필기장 · R 새로고침)",
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
                NavRow::OnenoteNotebook { notebook_key, .. } => {
                    app.active_notebook_key.as_deref() == Some(notebook_key.as_str())
                }
                NavRow::OnenoteSection { section_key, .. } => {
                    app.active_section_key.as_deref() == Some(section_key.as_str())
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
        TAB_NOTES => ("[+ 새 필기장]", " B · 섹션 S", ButtonId::OnenoteNewNotebook),
        _ => ("[+ 새 채팅]", " Ctrl+N", ButtonId::NewChat),
    };
    let btn_w = UnicodeWidthStr::width(btn) as u16;
    let btn_style = if ui.active_nav_tab == TAB_NOTES {
        Style::default()
            .fg(ONENOTE_PURPLE)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.accent_style().add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![Span::styled(btn, btn_style)];
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
        NavRow::OnenoteGroupHeader {
            label,
            personal,
            count,
        } => {
            let suffix = format!(" ({count})");
            let fixed = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(suffix.as_str());
            let body = truncate(label, width.saturating_sub(fixed).max(4));
            let header_style = if selected {
                theme.selection()
            } else if *personal {
                Style::default()
                    .fg(ONENOTE_PURPLE)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.accent_style().add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), header_style),
                Span::styled(body, header_style),
                Span::styled(
                    suffix,
                    if selected {
                        header_style
                    } else {
                        theme.muted_style()
                    },
                ),
            ])
        }
        NavRow::OnenoteNotebook {
            notebook_key,
            display_name,
            color,
            collapsed,
            section_count,
            owner_hint,
        } => {
            let arrow = if *collapsed { "▸ " } else { "▾ " };
            let bar = "▌";
            let badge_color = onenote_badge_color(color.as_deref(), notebook_key);
            let full_name = match owner_hint.as_deref() {
                Some(owner) if !owner.is_empty() => format!("{display_name} · {owner}"),
                _ => display_name.clone(),
            };
            let suffix = format!(" ({section_count})");
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(arrow)
                + UnicodeWidthStr::width(bar)
                + UnicodeWidthStr::width(suffix.as_str());
            let body = truncate(&full_name, width.saturating_sub(fixed).max(4));
            let name_style = if selected {
                theme.selection()
            } else if active {
                Style::default()
                    .fg(ONENOTE_PURPLE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)
            };
            let bar_style = if selected {
                theme.selection().fg(badge_color)
            } else {
                Style::default().fg(badge_color)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), name_style),
                Span::styled(arrow, name_style),
                Span::styled(bar, bar_style),
                Span::styled(body, name_style),
                Span::styled(
                    suffix,
                    if selected {
                        name_style
                    } else {
                        theme.muted_style()
                    },
                ),
            ])
        }
        NavRow::OnenoteSectionGroup { display_name, .. } => {
            let prefix = "  ▾ 📁 ";
            let fixed = UnicodeWidthStr::width(marker) + UnicodeWidthStr::width(prefix);
            let body = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else {
                theme.muted_style().add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(prefix, style),
                Span::styled(body, style),
            ])
        }
        NavRow::OnenoteSection {
            section_key,
            display_name,
            color,
            in_section_group,
            ..
        } => {
            let indent = if *in_section_group { "    " } else { "  " };
            let bar = "▌";
            let badge_color = onenote_badge_color(color.as_deref(), section_key);
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(indent)
                + UnicodeWidthStr::width(bar);
            let body = truncate(display_name, width.saturating_sub(fixed).max(4));
            let pad = width
                .saturating_sub(fixed + UnicodeWidthStr::width(body.as_str()))
                .max(0);
            let text_style = if selected {
                theme.selection()
            } else if active {
                Style::default()
                    .fg(ONENOTE_PURPLE)
                    .add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let bar_style = if selected {
                theme.selection().fg(badge_color)
            } else {
                Style::default().fg(badge_color)
            };
            Line::from(vec![
                Span::styled(marker.to_string(), text_style),
                Span::styled(indent, text_style),
                Span::styled(bar, bar_style),
                Span::styled(body, text_style),
                Span::styled(" ".repeat(pad), text_style),
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
    fn eight_tabs_map_roundtrip() {
        assert_eq!(TAB_COUNT, 8);
        assert_eq!(TAB_NOTES, 7);
        assert_eq!(TAB_LABELS.len(), TAB_COUNT);
        assert_eq!(TAB_LABELS[TAB_NOTES], "노트");
        for i in 0..TAB_COUNT {
            let tab = NavTab::from_index(i);
            assert_eq!(tab.index(), i);
            assert_eq!(tab.is_outlook(), (TAB_MAIL..=TAB_TODO).contains(&i));
            assert_eq!(tab.is_onenote(), i == TAB_NOTES);
        }
    }

    #[test]
    fn onenote_badge_color_parses_hex_names_and_deterministic_fallback() {
        assert_eq!(
            onenote_badge_color(Some("#7719AA"), "me|nb1"),
            Color::Rgb(0x77, 0x19, 0xAA)
        );
        assert_eq!(
            onenote_badge_color(Some("teal"), "me|nb2"),
            ONENOTE_PALETTE[3]
        );
        let c1 = onenote_badge_color(None, "me|notebook-alpha");
        let c2 = onenote_badge_color(None, "me|notebook-alpha");
        assert_eq!(c1, c2);
        assert!(ONENOTE_PALETTE.contains(&c1));
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
