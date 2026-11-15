//! Confirmation dialog for destructive actions (e.g., message deletion).

use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub message: String,
    pub on_confirm: Option<Action>,
}

impl ConfirmDialog {
    pub fn new(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Self {
            message: message.into(),
            on_confirm,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Activate | Action::Input('y') => {
                let action = self.on_confirm.clone().unwrap_or(Action::Noop);
                OverlayOutcome::CloseAndDispatch(action)
            }
            Action::CloseOverlay | Action::Input('n') => OverlayOutcome::Close,
            Action::MoveUp | Action::MoveDown => OverlayOutcome::Handled,
            Action::Input(_) | Action::Backspace | Action::InsertNewline | Action::Paste(_) => {
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::NotHandled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 56, 9);
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(" 확인 ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(self.message.clone(), theme.muted_style())),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Enter/y ", theme.selection()),
                Span::raw(" 확인    "),
                Span::styled(" Esc/n ", theme.muted_style()),
                Span::raw(" 취소"),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn confirm_dispatches_and_cancel_closes() {
        let mut dialog = ConfirmDialog::new("삭제할까요?", Some(Action::Quit));
        assert!(matches!(
            dialog.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::Quit)
        ));
        assert!(matches!(
            dialog.handle(&Action::CloseOverlay),
            OverlayOutcome::Close
        ));
    }

    #[test]
    fn renders_in_test_backend() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let ui = UiState::new();
        let dialog = ConfirmDialog::new("Delete?", None);
        terminal
            .draw(|frame| dialog.draw(frame, frame.area(), &ui))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Enter/y"));
        assert!(text.contains("Esc/n"));
    }
}
