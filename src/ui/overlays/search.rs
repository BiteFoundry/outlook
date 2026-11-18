//! Mail Search overlay (`/` or `Ctrl+F`):
//! Queries local SQLite FTS5 (`storage.search_mail_messages`) live as you type,
//! and `Enter` either jumps to the selected hit OR augments with remote Graph search
//! (`graph.search_mail_remote`).

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
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    pub query: String,
    pub selected: usize,
}

impl SearchOverlay {
    pub fn reset_selection(&mut self) {
        self.selected = 0;
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::SearchLocalMail(self.query.clone()))
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::SearchLocalMail(self.query.clone()))
            }
            Action::Backspace => {
                let _ = self.query.pop();
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::SearchLocalMail(self.query.clone()))
            }
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down(app.search_results.len());
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let query = self.query.trim().to_string();
                if query.is_empty() {
                    return OverlayOutcome::Handled;
                }
                let len = app.search_results.len();
                if query == app.search_query.trim() && len > 0 && self.selected < len {
                    return OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(
                        self.selected,
                    ));
                }
                OverlayOutcome::Dispatch(Action::SearchRemoteMail(query))
            }
            Action::OverlaySubmit => {
                let query = self.query.trim().to_string();
                if query.is_empty() {
                    OverlayOutcome::Handled
                } else {
                    OverlayOutcome::Dispatch(Action::SearchRemoteMail(query))
                }
            }
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
            area.width.saturating_sub(8).min(92),
            area.height.saturating_sub(4).min(22),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = " Search Mail · Enter Jump/Remote Search · Ctrl+Enter Remote · Esc Close ";
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled("Query: ", theme.title(true)),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let len = app.search_results.len();
        let status_y = inner.y.saturating_add(1);
        let status = if app.searching {
            "Searching mailbox…".to_string()
        } else if app.search_query.is_empty() {
            "Type to search cached mail (FTS5) · Enter to search server".to_string()
        } else {
            format!(
                "'{}': {} hit{} · {}/{} (Ctrl+Enter for remote Graph search)",
                app.search_query,
                len,
                if len == 1 { "" } else { "s" },
                if len == 0 { 0 } else { self.selected + 1 },
                len
            )
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(&status, inner.width as usize),
                theme.muted_style(),
            )),
            Rect::new(inner.x, status_y, inner.width, 1),
        );

        let list_y = inner.y.saturating_add(3);
        let visible = inner.height.saturating_sub(3) as usize;
        if visible == 0 {
            return;
        }
        let skip = self.selected.saturating_sub(visible.saturating_sub(1));
        for row in 0..visible {
            let index = skip + row;
            if index >= len {
                break;
            }
            let y = list_y + row as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let selected = index == self.selected;
            let style = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let hit = &app.search_results[index];
            let unread_dot = if !hit.is_read { "● " } else { "  " };
            let flag_tag = if hit.flagged { "⚑ " } else { "" };
            let date_short = hit.received_at.get(..10).unwrap_or("");
            let text = format!(
                "{unread_dot}{flag_tag}{date_short} · {} · {} — {}",
                hit.from_display,
                hit.subject,
                hit.snippet.replace(['\n', '\r'], " ")
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    crate::ui::truncate_width(&text, inner.width as usize),
                    style,
                ))),
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
    fn selection_moves_within_results() {
        let mut search = SearchOverlay::default();
        search.move_down(3);
        search.move_down(3);
        assert_eq!(search.selected, 2);
        search.move_down(3);
        assert_eq!(search.selected, 2);
        search.move_up();
        assert_eq!(search.selected, 1);
        search.move_down(0);
        assert_eq!(search.selected, 0);
    }

    #[test]
    fn activate_jumps_when_query_matches_results_and_triggers_remote_otherwise() {
        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = crate::sync::SyncController::from_channels(
            tokio::sync::watch::channel(crate::sync::FocusTarget::default()).0,
            event_tx,
        );
        let (ui_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);
        app.search_query = "budget".to_string();
        app.search_results = vec![crate::models::MailSearchHit {
            id: "m1".into(),
            conversation_id: Some("conv-1".into()),
            folder_id: Some("inbox".into()),
            subject: "Budget Q4".into(),
            from_display: "Alice".into(),
            received_at: "2026-11-18T00:00:00Z".into(),
            snippet: "budget review".into(),
            has_attachments: false,
            is_read: false,
            flagged: false,
        }];
        let mut search = SearchOverlay {
            query: "budget ".to_string(),
            selected: 0,
        };
        assert_eq!(
            search.handle(&Action::Activate, &app),
            OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(0))
        );

        let mut remote_search = SearchOverlay {
            query: "invoice".to_string(),
            selected: 0,
        };
        assert_eq!(
            remote_search.handle(&Action::Activate, &app),
            OverlayOutcome::Dispatch(Action::SearchRemoteMail("invoice".to_string()))
        );
    }
}
