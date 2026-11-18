//! Room / Channel Info & Members overlay (`Ctrl+U` or `[Info]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{attachments, centered_rect, OverlayKind, OverlayOutcome},
};

pub const BUTTONS: [&str; 4] = ["Refresh", "Attachments", "Send File", "Close"];

#[derive(Debug, Clone, Default)]
pub struct RoomInfoState {
    pub selected: usize,
}

impl RoomInfoState {
    pub fn select(&mut self, index: usize) {
        if index < BUTTONS.len() {
            self.selected = index;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = (self.selected + BUTTONS.len() - 1) % BUTTONS.len();
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1) % BUTTONS.len();
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down();
                OverlayOutcome::Handled
            }
            Action::Activate => match self.selected {
                0 => OverlayOutcome::Dispatch(Action::RefreshAll),
                1 => {
                    OverlayOutcome::CloseAndDispatch(Action::OpenOverlay(OverlayKind::Attachments))
                }
                2 => OverlayOutcome::CloseAndDispatch(Action::OpenSendFile),
                _ => OverlayOutcome::Close,
            },
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(76),
            area.height.saturating_sub(4).min(28),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Room Info & Members · ↑↓ Select · Enter Run · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let mut lines: Vec<Line> = Vec::new();
        match app.active_conversation() {
            Some(conv) => {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {} ", conv.title), theme.title(true)),
                    Span::styled(conv.kind.as_str().to_string(), theme.muted_style()),
                    Span::styled(format!("  unread {}", conv.unread_count), theme.muted_style()),
                ]));
                if let Some(team) = &conv.team_name {
                    lines.push(Line::from(Span::styled(
                        format!(" Team: {team}"),
                        theme.muted_style(),
                    )));
                }
                if let Some(topic) = &conv.topic {
                    if !topic.trim().is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!(" Topic: {topic}"),
                            theme.muted_style(),
                        )));
                    }
                }
            }
            None => lines.push(Line::from(Span::styled(
                " No conversation selected",
                theme.muted_style(),
            ))),
        }

        let attachment_rows = attachments::rows(app);
        lines.push(Line::from(Span::styled(
            format!(" {} attachments", attachment_rows.len()),
            theme.accent_style(),
        )));
        lines.push(Line::from(""));

        let buttons_y = inner.y + inner.height.saturating_sub(2);
        let member_room = buttons_y.saturating_sub(inner.y + lines.len() as u16) as usize;
        lines.push(Line::from(Span::styled(
            format!(" {} members", app.members.len()),
            theme.accent_style(),
        )));
        let member_space = member_room.saturating_sub(1);
        if app.members.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No members loaded or unavailable",
                theme.muted_style(),
            )));
        } else {
            for member in app.members.iter().take(member_space) {
                let avail = app
                    .presences
                    .get(&member.user_id)
                    .or(member.presence.as_ref())
                    .map(|p| p.availability.as_str());
                let (dot, dot_color) = theme.presence_indicator(avail);
                let me_tag = if member.user_id == app.my_user_id {
                    " (you)"
                } else {
                    ""
                };
                let email = member
                    .email
                    .as_deref()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default();
                let avail_str = avail.unwrap_or("Offline");
                lines.push(Line::from(vec![
                    Span::styled(format!("  {dot} "), Style::default().fg(dot_color)),
                    Span::raw(format!("{}{me_tag}{email}", member.display_name)),
                    Span::styled(format!(" [{avail_str}]"), theme.muted_style()),
                ]));
            }
            if app.members.len() > member_space && member_space > 0 {
                lines.push(Line::from(Span::styled(
                    format!("  …and {} more", app.members.len() - member_space),
                    theme.muted_style(),
                )));
            }
        }

        let body_height = buttons_y.saturating_sub(inner.y);
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(inner.x, inner.y, inner.width, body_height),
        );

        let mut x = inner.x;
        let mut y = buttons_y;
        for (index, label) in BUTTONS.iter().enumerate() {
            let text = format!(" [{label}] ");
            let width = unicode_width::UnicodeWidthStr::width(text.as_str()) as u16;
            if x + width > inner.x + inner.width {
                x = inner.x;
                y = y.saturating_add(1);
                if y >= inner.y + inner.height {
                    break;
                }
            }
            let style = if index == self.selected {
                theme.selection()
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(Span::styled(text, style)),
                Rect::new(x, y, width, 1),
            );
            hitmap.add_line(x, y, width, HitTarget::OverlayItem { index });
            x += width + 1;
        }
    }
}
