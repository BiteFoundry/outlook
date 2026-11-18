//! Left navigation pane:
//! - Tab 0 (`Ctrl+1`): 채팅 (1:1 · 그룹 · 회의)
//! - Tab 1 (`Ctrl+2`): 팀/채널
//! - Tab 2 (`Ctrl+3`): 활동 (`@멘션` 모아보기)
//! - Tab 3 (`Ctrl+4`): 메일 (Outlook Mail folders & inbox list)
//! - Tab 4 (`Ctrl+5`): 캘린더 (Outlook Calendar agenda)
//! - Tab 5 (`Ctrl+6`): 연락처 (Contacts + Entra directory)
//! - Tab 6 (`Ctrl+7`): 할 일 (Microsoft To Do lists & tasks)
//! - Tab 7 (`Ctrl+8`): 노트 (Microsoft OneNote notebooks, section groups & sections)

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

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
        chat_view::DualHasher,
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

const NAV_CACHE_CAPACITY: usize = 16;

struct NavRowsCache {
    entries: Vec<((u64, u64), Arc<[NavRow]>)>,
    max_entries: usize,
}

impl NavRowsCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries: max_entries.max(1),
        }
    }

    fn get(&mut self, key: (u64, u64)) -> Option<Arc<[NavRow]>> {
        let pos = self.entries.iter().position(|(k, _)| *k == key)?;
        if pos + 1 < self.entries.len() {
            let entry = self.entries.remove(pos);
            self.entries.push(entry);
        }
        self.entries.last().map(|(_, val)| Arc::clone(val))
    }

    fn insert(&mut self, key: (u64, u64), val: Arc<[NavRow]>) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push((key, val));
    }
}

thread_local! {
    static NAV_ROWS_CACHE: RefCell<NavRowsCache> = RefCell::new(NavRowsCache::new(NAV_CACHE_CAPACITY));
}

#[inline]
fn write_opt_str(h: &mut DualHasher, s: Option<&str>) {
    match s {
        Some(v) => {
            h.write_hash(&1u8);
            h.write_str(v);
        }
        None => h.write_hash(&0u8),
    }
}

/// Computes a 128-bit signature of all inputs that affect `NavRow` output for the active tab.
fn compute_nav_sig(app: &AppState, ui: &UiState) -> (u64, u64) {
    let mut h = DualHasher::new(0x4e41_565f_524f_5753);
    h.write_hash(&ui.active_nav_tab);
    match ui.active_nav_tab {
        TAB_CHATS => {
            let mut count = 0usize;
            for c in &app.conversations {
                if c.kind != ConversationKind::Channel {
                    h.write_str(&c.conv_key);
                    h.write_str(&c.title);
                    h.write_hash(&c.kind);
                    write_opt_str(&mut h, c.peer_user_id.as_deref());
                    h.write_hash(&c.unread_count);
                    count += 1;
                }
            }
            h.write_hash(&count);
        }
        TAB_TEAMS => {
            h.write_hash(&app.teams.len());
            for team in &app.teams {
                h.write_str(&team.id);
                h.write_str(&team.display_name);
                let collapsed = ui.collapsed_team_ids.contains(&team.id);
                h.write_hash(&collapsed);
                h.write_hash(&team.channels.len());
                if !collapsed {
                    for ch in &team.channels {
                        h.write_str(&ch.team_id);
                        h.write_str(&ch.id);
                        h.write_str(&ch.display_name);
                        h.write_hash(&(!ch.membership_type.eq_ignore_ascii_case("standard")));
                    }
                }
            }
            let mut chan_conv_count = 0usize;
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel || c.conv_key.starts_with("chan:") {
                    h.write_str(&c.conv_key);
                    h.write_hash(&c.unread_count);
                    chan_conv_count += 1;
                }
            }
            h.write_hash(&chan_conv_count);
        }
        TAB_ACTIVITY => {
            h.write_hash(&app.activity_mentions.len());
            for m in &app.activity_mentions {
                h.write_str(&m.conv_key);
                h.write_str(&m.msg_id);
                write_opt_str(&mut h, m.parent_msg_id.as_deref());
                h.write_str(&m.author_name);
                h.write_str(&m.body_plain);
                h.write_str(&m.created_at);
            }
        }
        TAB_MAIL => {
            h.write_str(&ui.active_mail_folder_id);
            let folder_count = app.mail_folders.len().min(6);
            h.write_hash(&folder_count);
            for folder in app.mail_folders.iter().take(6) {
                h.write_str(&folder.id);
                h.write_str(&folder.display_name);
                write_opt_str(&mut h, folder.well_known_name.as_deref());
                h.write_hash(&folder.unread_item_count);
            }
            h.write_hash(&app.mails.len());
            for mail in &app.mails {
                h.write_str(&mail.id);
                h.write_str(&mail.subject);
                match &mail.from {
                    Some(from) => {
                        h.write_hash(&1u8);
                        write_opt_str(&mut h, from.name.as_deref());
                        h.write_str(&from.address);
                    }
                    None => h.write_hash(&0u8),
                }
                h.write_hash(&mail.is_read);
                h.write_hash(&mail.flagged);
                h.write_hash(&mail.has_attachments);
                h.write_str(&mail.received_at);
            }
        }
        TAB_CALENDAR => {
            h.write_hash(&app.calendar_events.len());
            for ev in &app.calendar_events {
                h.write_str(&ev.id);
                h.write_str(&ev.subject);
                h.write_str(&ev.start_iso);
                h.write_str(&ev.end_iso);
                h.write_hash(&ev.is_all_day);
                h.write_hash(&(ev.is_online_meeting || ev.online_meeting_url.is_some()));
                h.write_str(&ev.response_status);
                h.write_hash(&ev.is_cancelled);
            }
        }
        TAB_CONTACTS => {
            h.write_hash(&app.contacts.len());
            for c in &app.contacts {
                h.write_str(&c.id);
                h.write_str(&c.display_name);
                write_opt_str(&mut h, c.primary_email());
                h.write_str(&c.source);
            }
        }
        TAB_TODO => {
            let show_chips = app.todo_lists.len() > 1;
            h.write_hash(&show_chips);
            if show_chips {
                write_opt_str(&mut h, ui.active_todo_list_id.as_deref());
                let list_count = app.todo_lists.len().min(6);
                h.write_hash(&list_count);
                for list in app.todo_lists.iter().take(6) {
                    h.write_str(&list.id);
                    h.write_str(&list.display_name);
                }
            }
            h.write_hash(&app.todo_tasks.len());
            for task in &app.todo_tasks {
                h.write_str(&task.id);
                h.write_str(&task.list_id);
                h.write_str(&task.title);
                h.write_hash(&task.is_completed());
                h.write_hash(&task.importance.eq_ignore_ascii_case("high"));
                write_opt_str(&mut h, task.due_date_iso.as_deref());
            }
        }
        TAB_NOTES => {
            h.write_hash(&app.onenote_notebooks.len());
            for nb in &app.onenote_notebooks {
                h.write_str(&nb.notebook_key);
                h.write_str(&nb.id);
                h.write_hash(&nb.scope);
                h.write_str(&nb.display_name);
                write_opt_str(&mut h, nb.color.as_deref());
                h.write_hash(&nb.is_shared);
                h.write_hash(&ui.collapsed_notebook_keys.contains(&nb.notebook_key));
            }
            h.write_hash(&app.onenote_sections.len());
            for sec in &app.onenote_sections {
                h.write_str(&sec.section_key);
                h.write_hash(&sec.scope);
                write_opt_str(&mut h, sec.notebook_id.as_deref());
                write_opt_str(&mut h, sec.parent_section_group_name.as_deref());
                h.write_str(&sec.display_name);
                write_opt_str(&mut h, sec.color.as_deref());
            }
        }
        _ => {}
    }
    h.finish()
}

/// Returns the cached `Arc<[NavRow]>` slice for the active tab, rebuilding only on signature miss.
pub fn nav_rows_cached(app: &AppState, ui: &UiState) -> Arc<[NavRow]> {
    let sig = compute_nav_sig(app, ui);
    if let Some(hit) = NAV_ROWS_CACHE.with(|cache| cache.borrow_mut().get(sig)) {
        return hit;
    }
    let rows: Arc<[NavRow]> = build_nav_rows_uncached(app, ui).into();
    NAV_ROWS_CACHE.with(|cache| cache.borrow_mut().insert(sig, Arc::clone(&rows)));
    rows
}

/// Returns the number of visible rows in the active nav tab without allocating rows on tabs 0..=6.
pub fn nav_row_count(app: &AppState, ui: &UiState) -> usize {
    match ui.active_nav_tab {
        TAB_CHATS => app
            .conversations
            .iter()
            .filter(|c| c.kind != ConversationKind::Channel)
            .count(),
        TAB_TEAMS => app
            .teams
            .iter()
            .map(|team| {
                1 + if ui.collapsed_team_ids.contains(&team.id) {
                    0
                } else {
                    team.channels.len()
                }
            })
            .sum(),
        TAB_ACTIVITY => app.activity_mentions.len(),
        TAB_MAIL => app.mail_folders.len().min(6) + app.mails.len(),
        TAB_CALENDAR => app.calendar_events.len(),
        TAB_CONTACTS => app.contacts.len(),
        TAB_TODO => {
            let list_chips = if app.todo_lists.len() > 1 {
                app.todo_lists.len().min(6)
            } else {
                0
            };
            list_chips + app.todo_tasks.len()
        }
        TAB_NOTES => nav_rows_cached(app, ui).len(),
        _ => 0,
    }
}

pub fn nav_row_at(app: &AppState, ui: &UiState, index: usize) -> Option<NavRow> {
    nav_rows_cached(app, ui).get(index).cloned()
}

pub fn build_nav_rows(app: &AppState, ui: &UiState) -> Vec<NavRow> {
    nav_rows_cached(app, ui).to_vec()
}

fn build_nav_rows_uncached(app: &AppState, ui: &UiState) -> Vec<NavRow> {
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
            let mut channel_unread: HashMap<&str, i32> =
                HashMap::with_capacity(app.conversations.len());
            for c in &app.conversations {
                if c.kind == ConversationKind::Channel || c.conv_key.starts_with("chan:") {
                    channel_unread
                        .entry(c.conv_key.as_str())
                        .or_insert(c.unread_count);
                }
            }
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
                        let unread = channel_unread.get(conv_key.as_str()).copied().unwrap_or(0);
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
    let rows = nav_rows_cached(app, ui);
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

const MARKER_WIDTH: usize = 2;

const SPACES: &str = "                                                                                                                                ";

#[inline]
fn spaces_cow(n: usize) -> Cow<'static, str> {
    if n <= SPACES.len() {
        Cow::Borrowed(&SPACES[..n])
    } else {
        Cow::Owned(" ".repeat(n))
    }
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
    let marker: &'static str = if selected {
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
            let (prefix_glyph, glyph_color): (Cow<'static, str>, Color) = match kind {
                ConversationKind::OneOnOne => {
                    let avail = peer_user_id
                        .as_deref()
                        .and_then(|id| app.presences.get(id))
                        .map(|p| p.availability.as_str());
                    let (dot, color) = theme.presence_indicator(avail);
                    (Cow::Owned(format!("{dot} ")), color)
                }
                ConversationKind::Group => (Cow::Borrowed("[그룹] "), theme.accent),
                ConversationKind::Meeting => (Cow::Borrowed("[회의] "), theme.reply),
                ConversationKind::Channel => (Cow::Borrowed("# "), theme.accent),
            };
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let used_fixed = MARKER_WIDTH
                + UnicodeWidthStr::width(prefix_glyph.as_ref())
                + badge.len();
            let name_w = width.saturating_sub(used_fixed).max(4);
            let (name, name_used) = truncate_with_width(title, name_w);
            let pad = width.saturating_sub(used_fixed + name_used);

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
            let mut spans = Vec::with_capacity(5);
            spans.push(Span::styled(marker, base_style));
            spans.push(Span::styled(prefix_glyph, glyph_style));
            spans.push(Span::styled(name, name_style));
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), base_style));
            }
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
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(arrow) + suffix.len();
            let name = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else {
                theme.accent_style().add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(marker, style),
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
            let (icon, icon_w) = if *is_private {
                ("  🔒 ", 5usize)
            } else {
                ("  # ", 4usize)
            };
            let badge = if *unread > 0 {
                format!(" ({unread})")
            } else {
                String::new()
            };
            let fixed = MARKER_WIDTH + icon_w + badge.len();
            let (name, name_used) =
                truncate_with_width(display_name, width.saturating_sub(fixed).max(4));
            let pad = width.saturating_sub(fixed + name_used);
            let name_style = if !selected && *unread > 0 {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let mut spans = Vec::with_capacity(5);
            spans.push(Span::styled(marker, base_style));
            spans.push(Span::styled(
                icon,
                if selected {
                    base_style
                } else {
                    theme.muted_style()
                },
            ));
            spans.push(Span::styled(name, name_style));
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), base_style));
            }
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
            let body = truncate(&label, width.saturating_sub(MARKER_WIDTH));
            Line::from(vec![
                Span::styled(marker, base_style),
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
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(icon) + badge.len();
            let name = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else if *folder_active {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            let mut spans = Vec::with_capacity(4);
            spans.push(Span::styled(marker, style));
            spans.push(Span::styled(icon, style));
            spans.push(Span::styled(name, style));
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
            let fixed = MARKER_WIDTH
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
            let mut spans = Vec::with_capacity(4);
            spans.push(Span::styled(marker, base_style));
            spans.push(Span::styled(status, status_style));
            spans.push(Span::styled(body, name_style));
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
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(icon);
            let body = truncate(&text, width.saturating_sub(fixed).max(4));
            let style = if *is_cancelled && !selected {
                theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
            } else {
                base_style
            };
            Line::from(vec![
                Span::styled(marker, style),
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
            let detail: Cow<'_, str> = match email {
                Some(e) if !e.is_empty() => Cow::Owned(format!("{display_name} ({e})")),
                _ => Cow::Borrowed(display_name.as_str()),
            };
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(icon);
            let body = truncate(&detail, width.saturating_sub(fixed).max(4));
            Line::from(vec![
                Span::styled(marker, base_style),
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
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(icon);
            let body = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else if *list_active {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            Line::from(vec![
                Span::styled(marker, style),
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
            let due_slice = due_date_iso.as_deref().and_then(|s| s.get(..10));
            let full: Cow<'_, str> = if !*important && due_slice.is_none() {
                Cow::Borrowed(title.as_str())
            } else {
                let star = if *important { "★ " } else { "" };
                match due_slice {
                    Some(d) => Cow::Owned(format!("{star}{title} ({d})")),
                    None => Cow::Owned(format!("{star}{title}")),
                }
            };
            let fixed = MARKER_WIDTH + 4; // box_icon is 4 ASCII chars
            let body = truncate(&full, width.saturating_sub(fixed).max(4));
            let style = if *completed && !selected {
                theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
            } else if *important && !selected {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            Line::from(vec![
                Span::styled(marker, style),
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
            let fixed = MARKER_WIDTH + suffix.len();
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
                Span::styled(marker, header_style),
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
            let full_name: Cow<'_, str> = match owner_hint.as_deref() {
                Some(owner) if !owner.is_empty() => Cow::Owned(format!("{display_name} · {owner}")),
                _ => Cow::Borrowed(display_name.as_str()),
            };
            let suffix = format!(" ({section_count})");
            let fixed = MARKER_WIDTH
                + UnicodeWidthStr::width(arrow)
                + UnicodeWidthStr::width(bar)
                + suffix.len();
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
                Span::styled(marker, name_style),
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
            let fixed = MARKER_WIDTH + UnicodeWidthStr::width(prefix);
            let body = truncate(display_name, width.saturating_sub(fixed).max(4));
            let style = if selected {
                theme.selection()
            } else {
                theme.muted_style().add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(marker, style),
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
            let (indent, indent_w) = if *in_section_group {
                ("    ", 4usize)
            } else {
                ("  ", 2usize)
            };
            let bar = "▌";
            let badge_color = onenote_badge_color(color.as_deref(), section_key);
            let fixed = MARKER_WIDTH + indent_w + UnicodeWidthStr::width(bar);
            let (body, body_used) =
                truncate_with_width(display_name, width.saturating_sub(fixed).max(4));
            let pad = width.saturating_sub(fixed + body_used);
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
            let mut spans = Vec::with_capacity(5);
            spans.push(Span::styled(marker, text_style));
            spans.push(Span::styled(indent, text_style));
            spans.push(Span::styled(bar, bar_style));
            spans.push(Span::styled(body, text_style));
            if pad > 0 {
                spans.push(Span::styled(spaces_cow(pad), text_style));
            }
            Line::from(spans)
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

fn truncate_with_width(text: &str, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        if text.len() <= width {
            return (text.to_string(), text.len());
        }
        return (text[..width].to_string(), width);
    }
    let mut out = String::with_capacity(text.len().min(width.saturating_mul(3)));
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    (out, used)
}

fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.is_ascii() {
        if text.len() <= width {
            return text.to_string();
        }
        if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return text[..width].to_string();
        }
    }
    truncate_with_width(text, width).0
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

    #[test]
    fn truncate_and_spaces_helpers_handle_ascii_and_wide_chars() {
        assert_eq!(UnicodeWidthStr::width("▸ "), MARKER_WIDTH);
        assert_eq!(UnicodeWidthStr::width("• "), MARKER_WIDTH);
        assert_eq!(UnicodeWidthStr::width("  "), MARKER_WIDTH);

        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("hello", 0), "");

        let (s, w) = truncate_with_width("한글ABC", 4);
        assert_eq!(s, "한글");
        assert_eq!(w, 4);

        let (s2, w2) = truncate_with_width("한글ABC", 5);
        assert_eq!(s2, "한글A");
        assert_eq!(w2, 5);

        assert_eq!(spaces_cow(0).as_ref(), "");
        assert_eq!(spaces_cow(5).as_ref(), "     ");
        assert!(matches!(spaces_cow(128), Cow::Borrowed(_)));
        assert_eq!(spaces_cow(140).len(), 140);
    }

    fn make_populated_test_state() -> (AppState, UiState, crate::sync::SyncController) {
        use crate::auth::AuthManager;
        use crate::config::AppConfig;
        use crate::graph::GraphClient;
        use crate::models::{
            CalendarEvent, ChannelSummary, ContactItem, ConversationSummary, ConversationTarget,
            EmailRecipient, MailFolder, MailMessage, MessageItem, OnenoteScope, TeamSummary,
            TodoList, TodoTask,
        };
        use crate::storage::Storage;
        use crate::sync;

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();

        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl.clone(), ui_tx);
        let ui = UiState::new();

        app.conversations = vec![
            ConversationSummary {
                conv_key: "chat:c1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c1".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "Alice".to_string(),
                topic: None,
                unread_count: 2,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: Some("u1".to_string()),
                web_url: None,
            },
            ConversationSummary {
                conv_key: "chat:c2".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c2".to_string(),
                },
                kind: ConversationKind::Group,
                team_id: None,
                team_name: None,
                title: "Design Sync".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            },
            ConversationSummary {
                conv_key: "chan:t1:ch1".to_string(),
                target: ConversationTarget::Channel {
                    team_id: "t1".to_string(),
                    channel_id: "ch1".to_string(),
                },
                kind: ConversationKind::Channel,
                team_id: Some("t1".to_string()),
                team_name: Some("Core".to_string()),
                title: "General".to_string(),
                topic: None,
                unread_count: 5,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: None,
                pinned: false,
                peer_user_id: None,
                web_url: None,
            },
        ];

        app.teams = vec![
            TeamSummary {
                id: "t1".to_string(),
                display_name: "Core".to_string(),
                description: None,
                channels: vec![
                    ChannelSummary {
                        id: "ch1".to_string(),
                        team_id: "t1".to_string(),
                        team_name: "Core".to_string(),
                        display_name: "General".to_string(),
                        description: None,
                        membership_type: "standard".to_string(),
                        web_url: None,
                    },
                    ChannelSummary {
                        id: "ch2".to_string(),
                        team_id: "t1".to_string(),
                        team_name: "Core".to_string(),
                        display_name: "Secret".to_string(),
                        description: None,
                        membership_type: "private".to_string(),
                        web_url: None,
                    },
                ],
            },
            TeamSummary {
                id: "t2".to_string(),
                display_name: "Infra".to_string(),
                description: None,
                channels: vec![ChannelSummary {
                    id: "ch3".to_string(),
                    team_id: "t2".to_string(),
                    team_name: "Infra".to_string(),
                    display_name: "Alerts".to_string(),
                    description: None,
                    membership_type: "standard".to_string(),
                    web_url: None,
                }],
            },
        ];

        app.activity_mentions = vec![MessageItem {
            msg_key: "chan:t1:ch1|m1".to_string(),
            conv_key: "chan:t1:ch1".to_string(),
            msg_id: "m1".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Bob".to_string(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: String::new(),
            body_plain: "ping\nline2".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }];

        app.mail_folders = vec![
            MailFolder {
                id: "inbox".to_string(),
                display_name: "받은 편지함".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                unread_item_count: 1,
                total_item_count: 10,
            },
            MailFolder {
                id: "sent".to_string(),
                display_name: "보낸 편지함".to_string(),
                well_known_name: Some("sentitems".to_string()),
                parent_folder_id: None,
                unread_item_count: 0,
                total_item_count: 3,
            },
        ];

        app.mails = vec![MailMessage {
            id: "mail-1".to_string(),
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: "Quarterly Review".to_string(),
            from: Some(EmailRecipient {
                name: Some("Alice".to_string()),
                address: "alice@example.com".to_string(),
            }),
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            received_at: "2026-11-18T01:00:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: true,
            importance: "normal".to_string(),
            has_attachments: true,
            preview: "Preview".to_string(),
            body_html: "<p>Initial body</p>".to_string(),
            body_plain: "Initial body".to_string(),
            attachments: Vec::new(),
            web_link: None,
        }];

        app.calendar_events = vec![CalendarEvent {
            id: "ev-1".to_string(),
            subject: "Standup".to_string(),
            start_iso: "2026-11-18T09:00:00Z".to_string(),
            end_iso: "2026-11-18T09:30:00Z".to_string(),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: Vec::new(),
            response_status: "accepted".to_string(),
            is_online_meeting: true,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        }];

        app.contacts = vec![ContactItem {
            id: "ct-1".to_string(),
            display_name: "Charlie".to_string(),
            given_name: None,
            surname: None,
            email_addresses: vec!["charlie@example.com".to_string()],
            mobile_phone: None,
            business_phones: Vec::new(),
            company_name: None,
            job_title: None,
            department: None,
            notes: None,
            source: "contact".to_string(),
        }];

        app.todo_lists = vec![
            TodoList {
                id: "list-1".to_string(),
                display_name: "Tasks".to_string(),
                wellknown_list_name: None,
                is_owner: true,
                is_shared: false,
            },
            TodoList {
                id: "list-2".to_string(),
                display_name: "Work".to_string(),
                wellknown_list_name: None,
                is_owner: true,
                is_shared: false,
            },
        ];
        app.todo_tasks = vec![TodoTask {
            id: "task-1".to_string(),
            list_id: "list-1".to_string(),
            title: "Ship cache".to_string(),
            status: "notStarted".to_string(),
            importance: "high".to_string(),
            body_plain: None,
            due_date_iso: Some("2026-11-20T00:00:00Z".to_string()),
            reminder_iso: None,
            completed_at: None,
            created_at: None,
            updated_at: None,
        }];

        app.onenote_notebooks = vec![
            OnenoteNotebook {
                notebook_key: "me|nb1".to_string(),
                id: "nb1".to_string(),
                scope: OnenoteScope::Me,
                display_name: "Personal".to_string(),
                color: Some("purple".to_string()),
                is_default: true,
                is_shared: false,
                user_role: None,
                created_at: None,
                last_modified_at: None,
                created_by: None,
                web_url: None,
            },
            OnenoteNotebook {
                notebook_key: "group:g1|nb2".to_string(),
                id: "nb2".to_string(),
                scope: OnenoteScope::Group {
                    group_id: "g1".to_string(),
                    group_name: Some("Eng".to_string()),
                },
                display_name: "Shared".to_string(),
                color: Some("teal".to_string()),
                is_default: false,
                is_shared: true,
                user_role: None,
                created_at: None,
                last_modified_at: None,
                created_by: None,
                web_url: None,
            },
        ];
        app.onenote_sections = vec![
            OnenoteSection {
                section_key: "me|sec1".to_string(),
                id: "sec1".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb1".to_string()),
                notebook_name: None,
                parent_section_group_id: Some("sg1".to_string()),
                parent_section_group_name: Some("Projects".to_string()),
                display_name: "Alpha".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
            OnenoteSection {
                section_key: "group:g1|sec2".to_string(),
                id: "sec2".to_string(),
                scope: OnenoteScope::Group {
                    group_id: "g1".to_string(),
                    group_name: Some("Eng".to_string()),
                },
                notebook_id: Some("nb2".to_string()),
                notebook_name: None,
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "Roadmap".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
            OnenoteSection {
                section_key: "me|orphan".to_string(),
                id: "orphan".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("missing-nb".to_string()),
                notebook_name: None,
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "Orphan".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
        ];

        (app, ui, sync_ctrl)
    }

    #[tokio::test]
    async fn nav_row_count_and_nav_row_at_match_build_nav_rows_across_all_tabs() {
        let (mut app, mut ui, sync_ctrl) = make_populated_test_state();

        for tab in 0..TAB_COUNT {
            ui.active_nav_tab = tab;
            let expected = build_nav_rows(&app, &ui);
            assert_eq!(
                nav_row_count(&app, &ui),
                expected.len(),
                "row count mismatch on tab {tab}"
            );
            for (idx, exp_row) in expected.iter().enumerate() {
                assert_eq!(
                    nav_row_at(&app, &ui, idx).as_ref(),
                    Some(exp_row),
                    "nav_row_at mismatch on tab {tab} idx {idx}"
                );
            }
            assert_eq!(nav_row_at(&app, &ui, expected.len()), None);
        }

        // TAB_TEAMS with collapsed team
        ui.active_nav_tab = TAB_TEAMS;
        ui.collapsed_team_ids.insert("t1".to_string());
        let teams_collapsed = build_nav_rows(&app, &ui);
        assert_eq!(nav_row_count(&app, &ui), teams_collapsed.len());
        assert_eq!(teams_collapsed.len(), 3); // t1 header + t2 header + alerts

        // TAB_TODO with single list (no chips rendered)
        ui.active_nav_tab = TAB_TODO;
        app.todo_lists.truncate(1);
        let todo_single = build_nav_rows(&app, &ui);
        assert_eq!(nav_row_count(&app, &ui), todo_single.len());
        assert_eq!(todo_single.len(), 1);

        // TAB_NOTES with collapsed notebook
        ui.active_nav_tab = TAB_NOTES;
        ui.collapsed_notebook_keys.insert("me|nb1".to_string());
        let notes_collapsed = build_nav_rows(&app, &ui);
        assert_eq!(nav_row_count(&app, &ui), notes_collapsed.len());

        sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn nav_rows_cached_reuses_arc_and_invalidates_on_relevant_mutations() {
        let (mut app, mut ui, sync_ctrl) = make_populated_test_state();

        // TAB_MAIL: repeated call returns identical Arc pointer
        ui.active_nav_tab = TAB_MAIL;
        let p1 = nav_rows_cached(&app, &ui);
        let p2 = nav_rows_cached(&app, &ui);
        assert!(Arc::ptr_eq(&p1, &p2));

        // Mutating body_html only must NOT invalidate nav cache
        app.mails[0].body_html = "<p>Updated HTML body</p>".to_string();
        let p_body = nav_rows_cached(&app, &ui);
        assert!(Arc::ptr_eq(&p1, &p_body));

        // Mutating is_read MUST invalidate immediately
        app.mails[0].is_read = true;
        let p_read = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&p1, &p_read));
        assert!(matches!(
            p_read.last(),
            Some(NavRow::MailItem { is_read: true, .. })
        ));

        // Mutating active_mail_folder_id MUST invalidate immediately
        ui.active_mail_folder_id = "sent".to_string();
        let p_folder = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&p_read, &p_folder));

        // TAB_CHATS: unread_count mutation invalidates
        ui.active_nav_tab = TAB_CHATS;
        let c1 = nav_rows_cached(&app, &ui);
        let c2 = nav_rows_cached(&app, &ui);
        assert!(Arc::ptr_eq(&c1, &c2));
        app.conversations[0].unread_count = 9;
        let c3 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&c1, &c3));
        assert!(matches!(c3.first(), Some(NavRow::Chat { unread: 9, .. })));

        // TAB_TEAMS: channel unread_count & collapsed_team_ids invalidate
        ui.active_nav_tab = TAB_TEAMS;
        let t1 = nav_rows_cached(&app, &ui);
        let t2 = nav_rows_cached(&app, &ui);
        assert!(Arc::ptr_eq(&t1, &t2));

        app.conversations[2].unread_count = 12;
        let t3 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&t1, &t3));
        assert!(matches!(
            t3.get(1),
            Some(NavRow::TeamChannel { unread: 12, .. })
        ));

        ui.collapsed_team_ids.insert("t1".to_string());
        let t4 = nav_rows_cached(&app, &ui);
        assert!(!Arc::ptr_eq(&t3, &t4));
        assert_eq!(t4.len(), 3);

        sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }
}
