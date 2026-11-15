//! Left navigation pane:
//! - Tab 0 (`Ctrl+1`): 채팅 (1:1 · 그룹 · 회의) with live presence dots & `[+ 새 채팅]`
//! - Tab 1 (`Ctrl+2`): 팀/채널 (collapsible tree: `▾ 개발팀` -> `  # 일반`, `  🔒 보안`)
//! - Tab 2 (`Ctrl+3`): 활동 (`@멘션` 모아보기)

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

pub const TAB_LABELS: [&str; 3] = ["채팅", "팀/채널", "활동"];

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
        _ => Vec::new(),
    }
}

pub fn tab_counts(app: &AppState) -> [usize; 3] {
    let chat_count = app
        .conversations
        .iter()
        .filter(|c| c.kind != ConversationKind::Channel)
        .count();
    let team_channel_count: usize = app.teams.iter().map(|t| t.channels.len()).sum();
    [chat_count, team_channel_count, app.activity_mentions.len()]
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Nav;
    let rows = build_nav_rows(app, ui);
    let label = TAB_LABELS.get(ui.active_nav_tab).copied().unwrap_or("채팅");

    let title = format!(" {label} {} ", rows.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(title, theme.title(focused))))
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
        draw_footer(frame, inner, ui, hitmap);
    }
}

fn draw_footer(frame: &mut Frame, inner: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let y = inner.y + inner.height - 1;
    let btn = "[+ 새 채팅]";
    let btn_w = UnicodeWidthStr::width(btn) as u16;
    let hint = " Ctrl+N";
    let mut spans = vec![Span::styled(
        btn,
        theme.accent_style().add_modifier(Modifier::BOLD),
    )];
    if inner.width > btn_w + 8 {
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
