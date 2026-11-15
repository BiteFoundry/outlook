//! Fuzzy Command Palette (`Ctrl+K`).

use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone)]
struct PaletteItem {
    label: &'static str,
    shortcut: &'static str,
    action: Action,
}

fn palette_items() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            label: "채팅 탭으로 이동",
            shortcut: "Ctrl+1",
            action: Action::SelectNavTab(0),
        },
        PaletteItem {
            label: "팀/채널 탭으로 이동",
            shortcut: "Ctrl+2",
            action: Action::SelectNavTab(1),
        },
        PaletteItem {
            label: "활동 / @멘션 탭으로 이동",
            shortcut: "Ctrl+3",
            action: Action::SelectNavTab(2),
        },
        PaletteItem {
            label: "메일(Outlook) 탭으로 이동",
            shortcut: "Ctrl+4",
            action: Action::SelectNavTab(3),
        },
        PaletteItem {
            label: "캘린더 탭으로 이동",
            shortcut: "Ctrl+5",
            action: Action::SelectNavTab(4),
        },
        PaletteItem {
            label: "연락처 탭으로 이동",
            shortcut: "Ctrl+6",
            action: Action::SelectNavTab(5),
        },
        PaletteItem {
            label: "할 일(To Do) 탭으로 이동",
            shortcut: "Ctrl+7",
            action: Action::SelectNavTab(6),
        },
        PaletteItem {
            label: "노트(OneNote) 탭으로 이동",
            shortcut: "Ctrl+8",
            action: Action::SelectNavTab(7),
        },
        PaletteItem {
            label: "Teams 모드로 전환",
            shortcut: "Alt+T",
            action: Action::SwitchPersona(crate::config::Persona::Teams),
        },
        PaletteItem {
            label: "Outlook 모드로 전환",
            shortcut: "Alt+O",
            action: Action::SwitchPersona(crate::config::Persona::Outlook),
        },
        PaletteItem {
            label: "OneNote 모드로 전환",
            shortcut: "Alt+N",
            action: Action::SwitchPersona(crate::config::Persona::OneNote),
        },
        PaletteItem {
            label: "새 채팅 만들기",
            shortcut: "Ctrl+N",
            action: Action::OpenOverlay(OverlayKind::NewChat),
        },
        PaletteItem {
            label: "새 메일 작성",
            shortcut: "c (메일탭)",
            action: Action::OpenMailComposeNew,
        },
        PaletteItem {
            label: "새 일정 만들기",
            shortcut: "n (일정탭)",
            action: Action::OpenOverlay(OverlayKind::EventCreate),
        },
        PaletteItem {
            label: "새 연락처 추가",
            shortcut: "n (연락처)",
            action: Action::OpenOverlay(OverlayKind::ContactCreate),
        },
        PaletteItem {
            label: "새 노트 페이지 만들기",
            shortcut: "n (노트탭)",
            action: Action::OnenoteOpenCreatePage { subpage: false },
        },
        PaletteItem {
            label: "새 하위 페이지 만들기",
            shortcut: "N (노트탭)",
            action: Action::OnenoteOpenCreatePage { subpage: true },
        },
        PaletteItem {
            label: "새 노트 섹션 만들기",
            shortcut: "S (노트탭)",
            action: Action::OnenoteOpenCreateSection,
        },
        PaletteItem {
            label: "새 필기장 만들기",
            shortcut: "B (노트탭)",
            action: Action::OnenoteOpenCreateNotebook,
        },
        PaletteItem {
            label: "이미지 첨부 크게보기",
            shortcut: "v",
            action: Action::OpenImagePreview,
        },
        PaletteItem {
            label: "내 상태(Presence) 변경",
            shortcut: "Ctrl+P",
            action: Action::OpenOverlay(OverlayKind::Presence),
        },
        PaletteItem {
            label: "메시지 검색",
            shortcut: "Ctrl+F",
            action: Action::OpenSearch,
        },
        PaletteItem {
            label: "파일 보내기",
            shortcut: "Ctrl+O",
            action: Action::OpenSendFile,
        },
        PaletteItem {
            label: "첨부 파일 목록",
            shortcut: "Ctrl+T",
            action: Action::OpenOverlay(OverlayKind::Attachments),
        },
        PaletteItem {
            label: "멤버 / 방 정보",
            shortcut: "Ctrl+U",
            action: Action::OpenOverlay(OverlayKind::RoomInfo),
        },
        PaletteItem {
            label: "선택 메시지에 반응 추가",
            shortcut: "+",
            action: Action::OpenReactionsForSelected,
        },
        PaletteItem {
            label: "선택 게시물 스레드 열기",
            shortcut: "t",
            action: Action::OpenThreadForSelected,
        },
        PaletteItem {
            label: "입력 대상 전환 (스레드 ↔ 채널 새 글)",
            shortcut: "클릭",
            action: Action::ToggleComposerTarget,
        },
        PaletteItem {
            label: "채널 게시물 제목 입력 토글",
            shortcut: "Ctrl+S",
            action: Action::ToggleSubject,
        },
        PaletteItem {
            label: "목록/메시지 새로고침",
            shortcut: "F5 / R",
            action: Action::RefreshAll,
        },
        PaletteItem {
            label: "전체 동기화 요청",
            shortcut: "s",
            action: Action::SyncHistory,
        },
        PaletteItem {
            label: "이전 메시지 불러오기",
            shortcut: "u",
            action: Action::LoadOlder,
        },
        PaletteItem {
            label: "최신 첨부 다운로드",
            shortcut: "d",
            action: Action::DownloadLatest,
        },
        PaletteItem {
            label: "입력창으로 이동",
            shortcut: "i",
            action: Action::OpenComposer,
        },
        PaletteItem {
            label: "현재 대화 읽음 처리",
            shortcut: "",
            action: Action::MarkActiveRead,
        },
        PaletteItem {
            label: "좌측 레일 접기/펼치기",
            shortcut: "Ctrl+B",
            action: Action::ToggleNavRail,
        },
        PaletteItem {
            label: "스레드 창 닫기",
            shortcut: "Esc",
            action: Action::CloseThreadDrawer,
        },
        PaletteItem {
            label: "알림 토글",
            shortcut: "설정",
            action: Action::ToggleNotify,
        },
        PaletteItem {
            label: "인라인 이미지 미리보기 토글",
            shortcut: "설정",
            action: Action::ToggleInlineImages,
        },
        PaletteItem {
            label: "새 버전(업데이트) 확인",
            shortcut: "",
            action: Action::CheckForUpdates,
        },
        PaletteItem {
            label: "자체 업데이트 설치",
            shortcut: "",
            action: Action::ApplySelfUpdate,
        },
        PaletteItem {
            label: "설정",
            shortcut: "Ctrl+,",
            action: Action::OpenOverlay(OverlayKind::Settings),
        },
        PaletteItem {
            label: "키보드 도움말",
            shortcut: "?",
            action: Action::OpenOverlay(OverlayKind::Help),
        },
        PaletteItem {
            label: "종료",
            shortcut: "q",
            action: Action::Quit,
        },
    ]
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    fn filtered(&self) -> Vec<PaletteItem> {
        let query = self.query.trim().to_lowercase();
        let mut items: Vec<(i32, PaletteItem)> = palette_items()
            .into_iter()
            .filter_map(|item| {
                if query.is_empty() {
                    return Some((0, item));
                }
                let label_l = item.label.to_lowercase();
                let short_l = item.shortcut.to_lowercase();
                if label_l.contains(&query) || short_l.contains(&query) {
                    Some((2, item))
                } else if fuzzy_matches(&label_l, &query) || fuzzy_matches(&short_l, &query) {
                    Some((1, item))
                } else {
                    None
                }
            })
            .collect();
        items.sort_by(|a, b| b.0.cmp(&a.0));
        items.into_iter().map(|(_, i)| i).collect()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.query.push_str(text);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::MoveUp => {
                let len = self.filtered().len();
                if len > 0 {
                    self.selected = (self.selected + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered().len();
                if len > 0 {
                    self.selected = (self.selected + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => match self.filtered().get(self.selected) {
                Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
                None => OverlayOutcome::Handled,
            },
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 60, (area.height.saturating_sub(4)).min(18));
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 명령 팔레트 · Enter 실행 · Esc 닫기 ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if inner.height == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled("> ", theme.title(true)),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let items = self.filtered();
        let list_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        let skip = self.selected.saturating_sub(visible.saturating_sub(1));
        for (row, (index, item)) in items
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
            let is_selected = index == self.selected;
            let style = if is_selected {
                theme.selection()
            } else {
                ratatui::style::Style::default()
            };
            let label_w = (inner.width as usize).saturating_sub(14).max(10);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {:<label_w$}", crate::ui::truncate_width(item.label, label_w)),
                    style,
                ),
                Span::styled(item.shortcut.to_string(), theme.muted_style()),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.filtered().len() {
            self.selected = index;
        }
    }
}

fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
    let mut chars = needle.chars();
    let Some(mut target) = chars.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == target {
            match chars.next() {
                Some(next) => target = next,
                None => return true,
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_fuzzy_matches() {
        let mut palette = CommandPalette::new();
        palette.query = "검색".to_string();
        assert!(!palette.filtered().is_empty());

        palette.query = "새채".to_string();
        assert!(palette
            .filtered()
            .iter()
            .any(|i| i.label.contains("새 채팅")));
    }

    #[test]
    fn activate_dispatches_quit() {
        let mut palette = CommandPalette::new();
        palette.query = "종료".to_string();
        let outcome = palette.handle(&Action::Activate);
        assert!(matches!(
            outcome,
            OverlayOutcome::CloseAndDispatch(Action::Quit)
        ));
    }
}
