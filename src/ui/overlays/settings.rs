//! Settings overlay (`Ctrl+,` or `[설정]`).
//! Reads & persists `crate::config::AppConfig` (`config.toml`).

use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    config,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayKind, OverlayOutcome},
    },
};

const ITEMS: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub selected: usize,
}

impl SettingsState {
    pub fn select(&mut self, index: usize) {
        if index < ITEMS {
            self.selected = index;
        }
    }

    pub fn rows(app: &AppState, ui: &UiState) -> [(&'static str, String); ITEMS] {
        [
            ("알림", toggle(ui.notify)),
            ("좌측 레일", toggle(ui.show_nav_rail)),
            ("자동 스레드창", toggle(ui.auto_thread_pane)),
            ("인라인 이미지 미리보기", toggle(ui.inline_images)),
            (
                "다운로드 경로",
                app.download_dir.to_string_lossy().to_string(),
            ),
            ("키보드 도움말", "도움말 보기".to_string()),
            ("Graph URL", app.config.graph_base_url.clone()),
            ("설정 파일", config::config_file_path().to_string_lossy().to_string()),
        ]
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                self.selected = (self.selected + ITEMS - 1) % ITEMS;
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.selected = (self.selected + 1) % ITEMS;
                OverlayOutcome::Handled
            }
            Action::Activate => match self.selected {
                0 => OverlayOutcome::Dispatch(Action::ToggleNotify),
                1 => OverlayOutcome::Dispatch(Action::ToggleNavRail),
                2 => OverlayOutcome::Dispatch(Action::ToggleAutoThreadPane),
                3 => OverlayOutcome::Dispatch(Action::ToggleInlineImages),
                4 => OverlayOutcome::Dispatch(Action::ShowDownloadDir),
                5 => OverlayOutcome::CloseAndDispatch(Action::OpenOverlay(OverlayKind::Help)),
                6 => OverlayOutcome::CloseAndDispatch(Action::OpenOverlay(OverlayKind::Presence)),
                _ => OverlayOutcome::Dispatch(Action::ShowToast(
                    config::config_file_path().display().to_string(),
                )),
            },
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
        let rect = centered_rect(area, 72, (ITEMS as u16 + 4).min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 설정 · Enter 변경/열기 · Esc 닫기 ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let rows = SettingsState::rows(app, ui);
        for (index, (label, value)) in rows.iter().enumerate() {
            if index as u16 >= inner.height.saturating_sub(1) {
                break;
            }
            let y = inner.y + index as u16;
            let selected = index == self.selected;
            let style = if selected {
                theme.selection()
            } else {
                ratatui::style::Style::default()
            };
            let used = unicode_width::UnicodeWidthStr::width(*label) + 4;
            let value =
                crate::ui::truncate_width(value, (inner.width as usize).saturating_sub(used + 2));
            let line = Line::from(vec![
                Span::styled(format!(" {:<16}", label), style),
                Span::styled(value, theme.muted_style()),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }

        let hint_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " 변경 사항은 config.toml 에 자동 저장됩니다",
                theme.muted_style(),
            )),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
    }
}

fn toggle(on: bool) -> String {
    if on {
        "켜짐".to_string()
    } else {
        "꺼짐".to_string()
    }
}
