//! Single-line quick input modal (`n` to create a To Do list or quick-add a To Do task).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickInputKind {
    NewTodoList,
    NewTodoTask,
}

#[derive(Debug, Clone)]
pub struct QuickInputOverlay {
    pub kind: QuickInputKind,
    pub value: String,
    pub list_id: Option<String>,
    pub error: Option<String>,
}

impl QuickInputOverlay {
    pub fn new_todo_list() -> Self {
        Self {
            kind: QuickInputKind::NewTodoList,
            value: String::new(),
            list_id: None,
            error: None,
        }
    }

    pub fn new_todo_task(list_id: String) -> Self {
        Self {
            kind: QuickInputKind::NewTodoTask,
            value: String::new(),
            list_id: Some(list_id),
            error: None,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.value.push(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.value.push_str(&text.replace(['\r', '\n'], " "));
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.value.pop();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let trimmed = self.value.trim().to_string();
                if trimmed.is_empty() {
                    self.error = Some("Enter a title".to_string());
                    return OverlayOutcome::Handled;
                }
                match self.kind {
                    QuickInputKind::NewTodoList => {
                        OverlayOutcome::CloseAndDispatch(Action::CreateTodoList {
                            display_name: trimmed,
                        })
                    }
                    QuickInputKind::NewTodoTask => {
                        let list_id = self.list_id.clone().unwrap_or_default();
                        OverlayOutcome::CloseAndDispatch(Action::CreateTodoTask {
                            list_id,
                            title: trimmed,
                        })
                    }
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(64),
            area.height.saturating_sub(4).min(7),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = match self.kind {
            QuickInputKind::NewTodoList => " New To Do List · Enter Create · Esc Cancel ",
            QuickInputKind::NewTodoTask => " New Task · Enter Add · Esc Cancel ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let prompt = match self.kind {
            QuickInputKind::NewTodoList => " List name:",
            QuickInputKind::NewTodoTask => " Task title:",
        };
        let mut lines = vec![
            Line::from(Span::styled(prompt, theme.muted_style())),
            Line::from(vec![
                Span::styled(" > ", theme.accent_style()),
                Span::raw(self.value.clone()),
                Span::styled("▏", theme.accent_style()),
            ]),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!(" {err}"),
                Style::default().fg(theme.error),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
