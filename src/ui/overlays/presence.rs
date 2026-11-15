//! Presence overlay (`Ctrl+P`): view & set user's Microsoft Teams availability.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

pub struct PresenceOption {
    pub availability: &'static str,
    pub activity: &'static str,
    pub label: &'static str,
    pub badge: &'static str,
}

pub const PRESENCE_OPTIONS: [PresenceOption; 6] = [
    PresenceOption {
        availability: "Available",
        activity: "Available",
        label: "Available (대화 가능)",
        badge: "●",
    },
    PresenceOption {
        availability: "Busy",
        activity: "Busy",
        label: "Busy (다른 용무 중)",
        badge: "●",
    },
    PresenceOption {
        availability: "DoNotDisturb",
        activity: "DoNotDisturb",
        label: "DoNotDisturb (방해 금지)",
        badge: "⛔",
    },
    PresenceOption {
        availability: "BeRightBack",
        activity: "BeRightBack",
        label: "BeRightBack (곧 돌아오겠음)",
        badge: "●",
    },
    PresenceOption {
        availability: "Away",
        activity: "Away",
        label: "Away (자리 비움)",
        badge: "●",
    },
    PresenceOption {
        availability: "Reset",
        activity: "Reset",
        label: "Reset (자동 상태로 초기화)",
        badge: "○",
    },
];

#[derive(Debug, Clone, Default)]
pub struct PresenceOverlay {
    pub selected: usize,
}

impl PresenceOverlay {
    pub fn select(&mut self, index: usize) {
        if index < PRESENCE_OPTIONS.len() {
            self.selected = index;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = (self.selected + PRESENCE_OPTIONS.len() - 1) % PRESENCE_OPTIONS.len();
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1) % PRESENCE_OPTIONS.len();
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
            Action::Input(ch @ '1'..='6') => {
                let idx = (*ch as usize) - ('1' as usize);
                self.selected = idx;
                let opt = &PRESENCE_OPTIONS[idx];
                OverlayOutcome::CloseAndDispatch(Action::SetPresence {
                    availability: opt.availability.to_string(),
                    activity: opt.activity.to_string(),
                })
            }
            Action::Activate => {
                let opt = &PRESENCE_OPTIONS[self.selected];
                OverlayOutcome::CloseAndDispatch(Action::SetPresence {
                    availability: opt.availability.to_string(),
                    activity: opt.activity.to_string(),
                })
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit => OverlayOutcome::NotHandled,
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
        let rect = centered_rect(area, 52, 12);
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 내 상태 (Presence) · Enter 적용 · Esc 닫기 ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }

        let current_text = match &app.my_presence {
            Some(p) => format!(" 현재 상태: {} ({})", p.availability, p.activity),
            None => " 현재 상태: 알 수 없음".to_string(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(current_text, theme.muted_style())),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let start_y = inner.y.saturating_add(2);
        for (index, opt) in PRESENCE_OPTIONS.iter().enumerate() {
            let y = start_y + index as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let selected = index == self.selected;
            let (_, dot_color) = theme.presence_indicator(Some(opt.availability));
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(format!(" {}. ", index + 1), theme.muted_style()),
                Span::styled(format!("{} ", opt.badge), Style::default().fg(dot_color)),
                Span::styled(opt.label, style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_shortcut_dispatches_presence() {
        let mut p = PresenceOverlay::default();
        assert!(matches!(
            p.handle(&Action::Input('2')),
            OverlayOutcome::CloseAndDispatch(Action::SetPresence { availability, .. }) if availability == "Busy"
        ));
    }
}
