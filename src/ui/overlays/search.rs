//! Search overlay (`Ctrl+F`, `/`): queries `storage.search_messages(...)` live
//! and jumps directly to the conversation (opening thread if `parent_msg_id.is_some()`).

use ratatui::{
    layout::Rect,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchScope {
    #[default]
    TeamsMessages,
    Mail,
    Contacts,
    Notes,
}

impl SearchScope {
    pub fn next(self) -> Self {
        match self {
            Self::TeamsMessages => Self::Mail,
            Self::Mail => Self::Contacts,
            Self::Contacts => Self::Notes,
            Self::Notes => Self::TeamsMessages,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TeamsMessages => "Teams 메시지",
            Self::Mail => "메일",
            Self::Contacts => "연락처",
            Self::Notes => "OneNote 노트",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    pub scope: SearchScope,
    pub query: String,
    pub selected: usize,
}

impl SearchOverlay {
    pub fn with_scope(scope: SearchScope) -> Self {
        Self {
            scope,
            query: String::new(),
            selected: 0,
        }
    }

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

    fn dispatch_search_action(&self, q: String) -> Action {
        match self.scope {
            SearchScope::TeamsMessages => Action::Search(q),
            SearchScope::Mail => Action::SearchMail(q),
            SearchScope::Contacts => Action::SearchContacts(q),
            SearchScope::Notes => Action::SearchNotes(q),
        }
    }

    fn result_len(&self, app: &AppState) -> usize {
        match self.scope {
            SearchScope::TeamsMessages => app.search_results.len(),
            SearchScope::Mail => app.mail_search_results.len(),
            SearchScope::Contacts => app.contact_search_results.len(),
            SearchScope::Notes => app.onenote_search_results.len(),
        }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Dispatch(self.dispatch_search_action(self.query.clone()))
            }
            Action::Paste(text) => {
                self.query.push_str(text);
                self.selected = 0;
                OverlayOutcome::Dispatch(self.dispatch_search_action(self.query.clone()))
            }
            Action::Backspace => {
                let _ = self.query.pop();
                self.selected = 0;
                OverlayOutcome::Dispatch(self.dispatch_search_action(self.query.clone()))
            }
            Action::Tab => {
                self.scope = self.scope.next();
                self.selected = 0;
                OverlayOutcome::Dispatch(self.dispatch_search_action(self.query.clone()))
            }
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down(self.result_len(app));
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let query = self.query.trim().to_string();
                if query.is_empty() {
                    return OverlayOutcome::Handled;
                }
                let len = self.result_len(app);
                if query == app.search_query && len > 0 && self.selected < len {
                    let jump = match self.scope {
                        SearchScope::TeamsMessages => Action::JumpToSearchHit(self.selected),
                        SearchScope::Mail => Action::JumpToMailSearchHit(self.selected),
                        SearchScope::Contacts => Action::JumpToContactSearchHit(self.selected),
                        SearchScope::Notes => Action::JumpToNoteSearchHit(self.selected),
                    };
                    return OverlayOutcome::CloseAndDispatch(jump);
                }
                OverlayOutcome::Dispatch(self.dispatch_search_action(query))
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
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(88),
            area.height.saturating_sub(4).min(22),
        );
        frame.render_widget(Clear, rect);
        let title = format!(
            " {} 검색 · Tab 범위전환 · Enter 이동 · Esc 닫기 ",
            self.scope.label()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled("검색어: ", theme.title(true)),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let len = self.result_len(app);
        let status_y = inner.y.saturating_add(1);
        let status = if app.searching {
            "검색 중…".to_string()
        } else if app.search_query.is_empty() {
            "검색어를 입력하면 로컬 캐시/Graph에서 실시간 검색합니다".to_string()
        } else {
            format!(
                "'{}': {}건 · {}/{}",
                app.search_query,
                len,
                if len == 0 { 0 } else { self.selected + 1 },
                len
            )
        };
        frame.render_widget(
            Paragraph::new(Span::styled(status, theme.muted_style())),
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
                ratatui::style::Style::default()
            };
            let text = match self.scope {
                SearchScope::TeamsMessages => {
                    let hit = &app.search_results[index];
                    let thread_tag = if hit.parent_msg_id.is_some() {
                        "[스레드] "
                    } else {
                        ""
                    };
                    format!(
                        "[{0}] {thread_tag}{1}: {2}",
                        hit.conv_title,
                        hit.author_name,
                        hit.snippet.replace(['\n', '\r'], " ")
                    )
                }
                SearchScope::Mail => {
                    let hit = &app.mail_search_results[index];
                    format!(
                        "[메일] {} · {} — {}",
                        hit.from_display,
                        hit.subject,
                        hit.snippet.replace(['\n', '\r'], " ")
                    )
                }
                SearchScope::Contacts => {
                    let c = &app.contact_search_results[index];
                    format!(
                        "[{}] {} ({})",
                        if c.source == "directory" { "조직" } else { "개인" },
                        c.display_name,
                        c.primary_email().unwrap_or("-")
                    )
                }
                SearchScope::Notes => {
                    let hit = &app.onenote_search_results[index];
                    let nb = hit.notebook_name.as_deref().unwrap_or("필기장");
                    let sec = hit.section_name.as_deref().unwrap_or("섹션");
                    format!(
                        "[노트] {nb} › {sec} · {} — {}",
                        hit.title,
                        hit.snippet.replace(['\n', '\r'], " ")
                    )
                }
            };
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
    fn search_scope_cycles_through_notes() {
        let mut scope = SearchScope::TeamsMessages;
        scope = scope.next();
        assert_eq!(scope, SearchScope::Mail);
        scope = scope.next();
        assert_eq!(scope, SearchScope::Contacts);
        scope = scope.next();
        assert_eq!(scope, SearchScope::Notes);
        assert_eq!(scope.label(), "OneNote 노트");
        scope = scope.next();
        assert_eq!(scope, SearchScope::TeamsMessages);
    }
}
