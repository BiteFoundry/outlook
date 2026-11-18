//! Left navigation pane:
//! - Tab 0 (`Ctrl+1`): 채팅 (1:1 · 그룹 · 회의)
//! - Tab 1 (`Ctrl+2`): 팀/채널
//! - Tab 2 (`Ctrl+3`): 활동 (`@멘션` 모아보기)

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
    models::ConversationKind,
    ui::{
        app::{AppState, Focus, UiState},
        chat_view::DualHasher,
        hitmap::{ButtonId, HitMap, HitTarget},
    },
};

pub const TAB_CHATS: usize = 0;
pub const TAB_TEAMS: usize = 1;
pub const TAB_ACTIVITY: usize = 2;
pub const TAB_COUNT: usize = 3;

pub const TAB_LABELS: [&str; TAB_COUNT] = ["채팅", "팀/채널", "활동"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    Chats = 0,
    Teams = 1,
    Activity = 2,
}

impl NavTab {
    pub fn from_index(idx: usize) -> Self {
        match idx {
            TAB_TEAMS => Self::Teams,
            TAB_ACTIVITY => Self::Activity,
            _ => Self::Chats,
        }
    }

    pub fn index(self) -> usize {
        self as usize
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

/// Returns the number of visible rows in the active nav tab without allocating rows.
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
    [chat_count, team_channel_count, app.activity_mentions.len()]
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Nav;
    let rows = nav_rows_cached(app, ui);
    let label = TAB_LABELS.get(ui.active_nav_tab).copied().unwrap_or("채팅");
    let count_badge = rows.len();
    let title = if ui.active_nav_tab == TAB_CHATS && app.loading_older_chats {
        format!(" {label} {count_badge} ⟳ ")
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
        ui.nav_area = Rect::default();
        return;
    }

    let show_footer = inner.height > 1;
    let list_area = if show_footer {
        Rect::new(inner.x, inner.y, inner.width, inner.height - 1)
    } else {
        inner
    };
    ui.nav_area = list_area;

    if rows.is_empty() {
        let empty_msg = match ui.active_nav_tab {
            TAB_CHATS => "채팅이 없습니다 (Ctrl+N 새 채팅)",
            TAB_TEAMS => "가입된 팀/채널이 없습니다",
            TAB_ACTIVITY => "최근 @멘션 활동이 없습니다",
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
        draw_footer(frame, inner, app, ui, hitmap);
    }
}

fn draw_footer(
    frame: &mut Frame,
    inner: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let y = inner.y + inner.height - 1;
    let btn = "[+ 새 채팅]";
    let hint = if ui.active_nav_tab == TAB_CHATS && app.loading_older_chats {
        " 이전 채팅 로드 중…"
    } else {
        " Ctrl+N"
    };
    let btn_w = UnicodeWidthStr::width(btn) as u16;
    let btn_style = theme.accent_style().add_modifier(Modifier::BOLD);
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
        HitTarget::Button(ButtonId::NewChat),
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
        Style::default().fg(theme.fg)
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
                Style::default().fg(glyph_color)
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
                    Style::default()
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
                    Style::default()
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
    fn three_tabs_map_roundtrip() {
        assert_eq!(TAB_COUNT, 3);
        assert_eq!(TAB_LABELS.len(), TAB_COUNT);
        assert_eq!(TAB_LABELS[TAB_CHATS], "채팅");
        assert_eq!(TAB_LABELS[TAB_TEAMS], "팀/채널");
        assert_eq!(TAB_LABELS[TAB_ACTIVITY], "활동");
        for i in 0..TAB_COUNT {
            let tab = NavTab::from_index(i);
            assert_eq!(tab.index(), i);
        }
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
            ChannelSummary, ConversationSummary, ConversationTarget, MessageItem, TeamSummary,
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

        (app, ui, sync_ctrl)
    }

    #[tokio::test]
    async fn nav_row_count_and_nav_row_at_match_build_nav_rows_across_all_tabs() {
        let (app, mut ui, sync_ctrl) = make_populated_test_state();

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

        sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn nav_rows_cached_reuses_arc_and_invalidates_on_relevant_mutations() {
        let (mut app, mut ui, sync_ctrl) = make_populated_test_state();

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
