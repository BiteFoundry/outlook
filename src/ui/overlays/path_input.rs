//! Local file path + caption input overlay (`Ctrl+O` or `[파일전송]`).

use std::path::{Path, PathBuf};

use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    overlays::{centered_rect, OverlayOutcome},
};

pub fn expand_path_with_home(input: &str, home: &Path) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

pub fn expand_path(input: &str) -> PathBuf {
    expand_path_with_home(input, &home_dir())
}

fn home_dir() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Clone, Default)]
pub struct PathInputState {
    pub path: String,
    pub caption: String,
    pub field: usize,
    pub error: Option<String>,
}

impl PathInputState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_field(&mut self, field: usize) {
        self.field = field % 2;
        self.error = None;
    }

    pub fn next_field(&mut self) {
        self.set_field(self.field + 1);
    }

    pub fn prev_field(&mut self) {
        self.set_field(self.field + 1);
    }

    fn active_mut(&mut self) -> &mut String {
        if self.field == 0 {
            &mut self.path
        } else {
            &mut self.caption
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.active_mut().push(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.active_mut().push_str(text);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.active_mut().pop();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Tab => {
                self.next_field();
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::PrevFocus => {
                self.prev_field();
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let path = self.path.trim().to_string();
                if path.is_empty() {
                    self.field = 0;
                    self.error = Some("경로를 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                let expanded = expand_path(&path);
                if !expanded.exists() {
                    self.field = 0;
                    self.error = Some(format!("파일이 없습니다: {}", expanded.display()));
                    return OverlayOutcome::Handled;
                }
                if !expanded.is_file() {
                    self.field = 0;
                    self.error = Some(format!("파일이 아닙니다: {}", expanded.display()));
                    return OverlayOutcome::Handled;
                }
                let caption = self.caption.trim().to_string();
                OverlayOutcome::CloseAndDispatch(Action::SendAttachmentPath {
                    path: expanded.to_string_lossy().into_owned(),
                    caption,
                })
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(78),
            area.height.saturating_sub(4).min(11),
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 파일 보내기 · Tab 필드 · Enter 전송 · Esc 취소 ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let path_style = if self.field == 0 {
            theme.accent_style()
        } else {
            theme.muted_style()
        };
        let caption_style = if self.field == 1 {
            theme.accent_style()
        } else {
            theme.muted_style()
        };

        let mut lines = vec![
            Line::from(Span::styled(
                " 로컬 파일 경로 (~ 는 홈 디렉터리로 확장)",
                theme.muted_style(),
            )),
            Line::from(vec![
                Span::styled(" > ", path_style),
                Span::raw(self.path.clone()),
                Span::styled("▏", path_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                " 함께 보낼 메시지/캡션 (선택 · Tab 이동)",
                theme.muted_style(),
            )),
            Line::from(vec![
                Span::styled(" > ", caption_style),
                Span::raw(self.caption.clone()),
                Span::styled("▏", caption_style),
            ]),
        ];
        match &self.error {
            Some(error) => lines.push(Line::from(Span::styled(
                format!(" 오류: {error}"),
                ratatui::style::Style::default().fg(theme.error),
            ))),
            None => lines.push(Line::from(Span::styled(
                " OneDrive/SharePoint 업로드 후 현재 채팅/채널/스레드로 전송됩니다",
                theme.muted_style(),
            ))),
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_leading_tilde() {
        let home = PathBuf::from("/home/user");
        assert_eq!(expand_path_with_home("~", &home), home);
        assert_eq!(expand_path_with_home("~/a/b.png", &home), home.join("a/b.png"));
        assert_eq!(expand_path_with_home(r"~\a\b.png", &home), home.join(r"a\b.png"));
        assert_eq!(expand_path_with_home(" x/~/y ", &home), PathBuf::from("x/~/y"));
    }

    #[test]
    fn empty_or_missing_path_shows_error() {
        let mut state = PathInputState::new();
        assert!(matches!(state.handle(&Action::Activate), OverlayOutcome::Handled));
        assert!(state.error.is_some());
    }
}
