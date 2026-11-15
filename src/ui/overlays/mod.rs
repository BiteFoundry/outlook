//! Overlay stack & HelpOverlay (`?`).

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod contact_form;
pub mod context_menu;
pub mod event_form;
pub mod image_preview;
pub mod mail_compose;
pub mod mentions;
pub mod new_chat;
pub mod onenote_form;
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
        contact_form::ContactCreateOverlay, context_menu::ContextMenu,
        event_form::EventCreateOverlay, image_preview::ImagePreviewOverlay,
        mail_compose::MailComposeOverlay, new_chat::NewChatState, onenote_form::OnenoteFormOverlay,
        path_input::PathInputState, presence::PresenceOverlay, reactions::ReactionsOverlay,
        room_info::RoomInfoState, search::SearchOverlay, settings::SettingsState,
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
    MailCompose,
    ImagePreview,
    EventCreate,
    ContactCreate,
    OnenoteForm,
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
    MailCompose(MailComposeOverlay),
    ImagePreview(ImagePreviewOverlay),
    EventCreate(EventCreateOverlay),
    ContactCreate(ContactCreateOverlay),
    OnenoteForm(OnenoteFormOverlay),
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
            Overlay::MailCompose(_) => OverlayKind::MailCompose,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::EventCreate(_) => OverlayKind::EventCreate,
            Overlay::ContactCreate(_) => OverlayKind::ContactCreate,
            Overlay::OnenoteForm(_) => OverlayKind::OnenoteForm,
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
                | Overlay::MailCompose(_)
                | Overlay::EventCreate(_)
                | Overlay::ContactCreate(_)
                | Overlay::OnenoteForm(_)
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

    pub fn event_create() -> Self {
        Overlay::EventCreate(EventCreateOverlay::new())
    }

    pub fn contact_create() -> Self {
        Overlay::ContactCreate(ContactCreateOverlay::new())
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
            Overlay::CommandPalette(palette) => match action {
                Action::Tab => palette.handle(&Action::MoveDown),
                Action::PrevFocus => palette.handle(&Action::MoveUp),
                other => palette.handle(other),
            },
            Overlay::Settings(settings) => match action {
                Action::Tab => settings.handle(&Action::MoveDown),
                Action::PrevFocus => settings.handle(&Action::MoveUp),
                other => settings.handle(other),
            },
            Overlay::Search(search) => search.handle(action, app),
            Overlay::Presence(presence) => presence.handle(action),
            Overlay::Reactions(reactions) => reactions.handle(action),
            Overlay::Attachments(tray) => tray.handle(action, app),
            Overlay::PathInput(input) => input.handle(action),
            Overlay::NewChat(new_chat) => match action {
                Action::Tab => new_chat.handle(&Action::MoveDown, app),
                Action::PrevFocus => new_chat.handle(&Action::MoveUp, app),
                other => new_chat.handle(other, app),
            },
            Overlay::RoomInfo(info) => info.handle(action),
            Overlay::MailCompose(compose) => compose.handle(action, app),
            Overlay::ImagePreview(preview) => preview.handle(action),
            Overlay::EventCreate(form) => form.handle(action),
            Overlay::ContactCreate(form) => form.handle(action),
            Overlay::OnenoteForm(form) => form.handle(action),
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
            Overlay::MailCompose(compose) => compose.select(index),
            Overlay::EventCreate(form) => form.select(index),
            Overlay::ContactCreate(form) => form.select(index),
            Overlay::OnenoteForm(form) => form.select(index),
            Overlay::ImagePreview(_) | Overlay::PathInput(_) | Overlay::Help(_) | Overlay::Confirm(_) => {}
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
            Overlay::MailCompose(compose) => compose.draw(frame, area, app, ui, hitmap),
            Overlay::ImagePreview(preview) => preview.draw(frame, area, app, ui, hitmap),
            Overlay::EventCreate(form) => form.draw(frame, area, ui, hitmap),
            Overlay::ContactCreate(form) => form.draw(frame, area, ui, hitmap),
            Overlay::OnenoteForm(form) => form.draw(frame, area, ui, hitmap),
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
        "탐색 · 워크스페이스",
        &[
            ("Ctrl+1~3/4~7/8", "Teams / Outlook / OneNote(노트)"),
            ("Alt+T / O / N", "Teams / Outlook / OneNote 전환"),
            ("↑/↓, j/k", "목록·메시지·블록 이동"),
            ("Tab / Shift+Tab", "포커스 순환"),
            ("Enter / Esc", "열기·전송 / 오버레이·스레드 닫기"),
        ],
    ),
    (
        "Teams 메시지 · 이미지",
        &[
            ("t / +", "채널 스레드 열기 / 반응 추가"),
            ("v", "선택 이미지 첨부 크게보기 (모달)"),
            ("r / y / Del", "답장 / 복사 / 삭제(확인)"),
            ("d", "최신 첨부 다운로드"),
            ("m / 우클릭", "컨텍스트 메뉴"),
            ("F5 / R / s", "새로고침 / 전체 동기화"),
        ],
    ),
    (
        "Outlook · OneNote",
        &[
            ("메일: c/r/R/f", "새 메일 / 답장 / 전체답장 / 전달"),
            ("일정/할일", "j/n/y/t/x 일정 · Space/n/[/] 할일"),
            ("노트: Space / x", "선택 체크박스(☐/☑) 즉시 토글"),
            ("노트: n/N/S/B", "새 페이지 / 하위페이지 / 섹션 / 필기장"),
            ("노트: a/e/E", "블록추가 / 블록편집 / $EDITOR 전체편집"),
            ("노트: [/] · J/K", "섹션 탭 전환 / 페이지 이동"),
        ],
    ),
    (
        "입력 · 기능 · 창",
        &[
            ("Enter / Ctrl+Enter", "전송 (모달 본문 Ctrl+Enter)"),
            ("Ctrl+V / Ctrl+S", "이미지 붙여넣기 / 저장·제목 토글"),
            ("Ctrl+K / Ctrl+F", "명령 팔레트 / 통합 검색(Tab 전환)"),
            ("Ctrl+N / Ctrl+O", "새 채팅 / 파일 전송"),
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
