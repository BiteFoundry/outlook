//! Overlay stack & HelpOverlay (`?`).

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod mentions;
pub mod new_chat;
pub mod path_input;
pub mod presence;
pub mod reactions;
pub mod room_info;
pub mod search;
pub mod settings;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, MessageScope},
    overlays::{
        attachments::AttachmentsState, command_palette::CommandPalette, confirm::ConfirmDialog,
        context_menu::ContextMenu, new_chat::NewChatState, path_input::PathInputState,
        presence::PresenceOverlay, reactions::ReactionsOverlay, room_info::RoomInfoState,
        search::SearchOverlay, settings::SettingsState,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    ContextMenu,
    CommandPalette,
    Settings,
    Search,
    Presence,
    Reactions,
    Attachments,
    PathInput,
    NewChat,
    RoomInfo,
    Help,
    Confirm,
}

impl OverlayKind {
    pub fn is_singleton(self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
    NotHandled,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    ContextMenu(ContextMenu),
    CommandPalette(CommandPalette),
    Settings(SettingsState),
    Search(SearchOverlay),
    Presence(PresenceOverlay),
    Reactions(ReactionsOverlay),
    Attachments(AttachmentsState),
    PathInput(PathInputState),
    NewChat(NewChatState),
    RoomInfo(RoomInfoState),
    Help(HelpOverlay),
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::Search(_) => OverlayKind::Search,
            Overlay::Presence(_) => OverlayKind::Presence,
            Overlay::Reactions(_) => OverlayKind::Reactions,
            Overlay::Attachments(_) => OverlayKind::Attachments,
            Overlay::PathInput(_) => OverlayKind::PathInput,
            Overlay::NewChat(_) => OverlayKind::NewChat,
            Overlay::RoomInfo(_) => OverlayKind::RoomInfo,
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn accepts_text(&self) -> bool {
        matches!(
            self,
            Overlay::Search(_)
                | Overlay::CommandPalette(_)
                | Overlay::PathInput(_)
                | Overlay::NewChat(_)
        )
    }

    pub fn context_menu(menu: ContextMenu) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsState::default())
    }

    pub fn search() -> Self {
        Overlay::Search(SearchOverlay::default())
    }

    pub fn presence() -> Self {
        Overlay::Presence(PresenceOverlay::default())
    }

    pub fn reactions(scope: MessageScope, msg_index: usize) -> Self {
        Overlay::Reactions(ReactionsOverlay::new(scope, msg_index))
    }

    pub fn attachments() -> Self {
        Overlay::Attachments(AttachmentsState::default())
    }

    pub fn path_input() -> Self {
        Overlay::PathInput(PathInputState::new())
    }

    pub fn new_chat() -> Self {
        Overlay::NewChat(NewChatState::new())
    }

    pub fn room_info() -> Self {
        Overlay::RoomInfo(RoomInfoState::default())
    }

    pub fn help() -> Self {
        Overlay::Help(HelpOverlay)
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, _ui: &UiState) -> OverlayOutcome {
        match self {
            Overlay::ContextMenu(menu) => match action {
                Action::MoveUp => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::Activate => menu.activate(),
                Action::CloseOverlay => OverlayOutcome::Close,
                Action::Input(_) | Action::Backspace | Action::InsertNewline | Action::Paste(_) => {
                    OverlayOutcome::Handled
                }
                _ => OverlayOutcome::NotHandled,
            },
            Overlay::CommandPalette(palette) => palette.handle(action),
            Overlay::Settings(settings) => settings.handle(action),
            Overlay::Search(search) => search.handle(action, app),
            Overlay::Presence(presence) => presence.handle(action),
            Overlay::Reactions(reactions) => reactions.handle(action),
            Overlay::Attachments(tray) => tray.handle(action, app),
            Overlay::PathInput(input) => input.handle(action),
            Overlay::NewChat(new_chat) => new_chat.handle(action, app),
            Overlay::RoomInfo(info) => info.handle(action),
            Overlay::Help(_) => match action {
                Action::CloseOverlay | Action::Activate => OverlayOutcome::Close,
                Action::Quit => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::Confirm(dialog) => dialog.handle(action),
        }
    }

    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::ContextMenu(menu) => menu.select(index),
            Overlay::CommandPalette(palette) => palette.select(index),
            Overlay::Settings(settings) => settings.select(index),
            Overlay::Search(search) => search.select(index),
            Overlay::Presence(presence) => presence.select(index),
            Overlay::Reactions(reactions) => reactions.select(index),
            Overlay::Attachments(tray) => tray.select(index),
            Overlay::NewChat(new_chat) => new_chat.select(index),
            Overlay::RoomInfo(info) => info.select(index),
            Overlay::PathInput(_) | Overlay::Help(_) | Overlay::Confirm(_) => {}
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
        match self {
            Overlay::ContextMenu(menu) => menu.draw(frame, area, ui, hitmap),
            Overlay::CommandPalette(palette) => palette.draw(frame, area, ui, hitmap),
            Overlay::Settings(settings) => settings.draw(frame, area, app, ui, hitmap),
            Overlay::Search(search) => search.draw(frame, area, app, ui, hitmap),
            Overlay::Presence(presence) => presence.draw(frame, area, app, ui, hitmap),
            Overlay::Reactions(reactions) => reactions.draw(frame, area, app, ui, hitmap),
            Overlay::Attachments(tray) => tray.draw(frame, area, app, ui, hitmap),
            Overlay::PathInput(input) => input.draw(frame, area, ui),
            Overlay::NewChat(new_chat) => new_chat.draw(frame, area, app, ui, hitmap),
            Overlay::RoomInfo(info) => info.draw(frame, area, app, ui, hitmap),
            Overlay::Help(_) => draw_help(frame, area, ui),
            Overlay::Confirm(dialog) => dialog.draw(frame, area, ui),
        }
    }
}

// ---------------------------------------------------------------------------
// Help Overlay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct HelpOverlay;

const HELP_SECTIONS: [(&str, &[(&str, &str)]); 4] = [
    (
        "탐색",
        &[
            ("Ctrl+1 / 2 / 3", "채팅 / 팀·채널 / 활동 탭"),
            ("↑/↓, j/k", "목록/메시지 이동"),
            ("Tab / Shift+Tab", "포커스 순환"),
            ("Enter", "열기 / 스레드 / 전송"),
            ("Esc", "오버레이·스레드 닫기"),
        ],
    ),
    (
        "메시지 · 스레드",
        &[
            ("t", "채널 스레드 열기"),
            ("+ / e", "메시지 반응(이모지) 추가"),
            ("r", "메시지 답장(인용)/스레드"),
            ("y / Del", "메시지 복사 / 삭제(확인)"),
            ("d", "최신 첨부 다운로드"),
            ("m / 우클릭", "컨텍스트 메뉴"),
            ("F5 / R", "전체 새로고침"),
        ],
    ),
    (
        "입력 (Markdown)",
        &[
            ("Enter", "전송 / 멘션 확정"),
            ("Shift+Enter / Ctrl+J", "줄바꿈"),
            ("Ctrl+S", "채널 게시물 제목 토글"),
            ("Ctrl+V", "텍스트·클립보드 이미지 붙여넣기"),
            ("@", "멤버 멘션 자동완성"),
            ("Ctrl+E", "외부 에디터 ($EDITOR)"),
            ("Ctrl+Z / Ctrl+Y", "실행 취소 / 다시 실행"),
        ],
    ),
    (
        "기능 · 창",
        &[
            ("Ctrl+K", "명령 팔레트"),
            ("Ctrl+F / /", "로컬 메시지 검색"),
            ("Ctrl+N", "새 1:1 / 그룹 채팅"),
            ("Ctrl+P", "내 상태 (Presence) 변경"),
            ("Ctrl+O", "로컬 파일 업로드/전송"),
            ("Ctrl+T / Ctrl+U", "첨부 목록 / 멤버·정보"),
            ("Ctrl+B / Ctrl+,", "좌측 레일 토글 / 설정"),
            ("? / q", "도움말 / 종료"),
        ],
    ),
];

fn draw_help(frame: &mut Frame, area: Rect, ui: &UiState) {
    let theme = &ui.theme;
    let rect = centered_rect(
        area,
        area.width.saturating_sub(8).min(84),
        area.height.saturating_sub(4).min(24),
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            " Microsoft Teams TUI 도움말 · Esc 닫기 ",
            theme.title(true),
        )))
        .border_style(theme.border_style(true));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    for (index, (title, entries)) in HELP_SECTIONS.iter().enumerate() {
        let target = columns[index % 2];
        if target.width == 0 {
            continue;
        }
        let y_offset = if index < 2 { 0 } else { target.height / 2 };
        let mut lines = Vec::with_capacity(entries.len() + 1);
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            theme.title(true),
        )));
        for (key, desc) in entries.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<20}"), theme.accent_style()),
                Span::raw(desc.to_string()),
            ]));
        }
        let sub = Rect::new(
            target.x,
            target.y + y_offset,
            target.width,
            target.height.saturating_sub(y_offset),
        );
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), sub);
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn draw_stack(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    if let Some(overlay) = ui.overlays.last() {
        overlay.draw(frame, area, app, ui, hitmap);
    }
}
