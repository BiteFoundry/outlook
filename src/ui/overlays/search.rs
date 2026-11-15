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
                OverlayOutcome::Dispatch(Action::Search(self.query.clone()))
            }
            Action::Paste(text) => {
                self.query.push_str(text);
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::Search(self.query.clone()))
            }
            Action::Backspace => {
                let _ = self.query.pop();
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::Search(self.query.clone()))
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
                if query == app.search_query && !app.search_results.is_empty() {
                    if self.selected < app.search_results.len() {
                        return OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(
                            self.selected,
                        ));
                    }
                }
                OverlayOutcome::Dispatch(Action::Search(query))
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
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 메시지 검색 · Enter 이동 · Esc 닫기 ",
                theme.title(true),
            )))
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

        let status_y = inner.y.saturating_add(1);
        let status = if app.searching {
            "검색 중…".to_string()
        } else if app.search_query.is_empty() {
            "검색어를 입력하면 로컬 캐시에서 실시간 검색합니다".to_string()
        } else {
            format!(
                "'{}': {}건 · {}/{}",
                app.search_query,
                app.search_results.len(),
                if app.search_results.is_empty() {
                    0
                } else {
                    self.selected + 1
                },
                app.search_results.len()
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
        for (row, (index, hit)) in app
            .search_results
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
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
            let thread_tag = if hit.parent_msg_id.is_some() {
                "[스레드] "
            } else {
                ""
            };
            let text = format!(
                "[{0}] {thread_tag}{1}: {2}",
                hit.conv_title,
                hit.author_name,
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
}
