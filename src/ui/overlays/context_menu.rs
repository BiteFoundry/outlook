//! Right-click / `m` context menu.

use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::ConversationKind,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget, MessageScope},
        overlays::{OverlayKind, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    NavRow(usize),
    Message {
        scope: MessageScope,
        msg_index: usize,
    },
    Composer,
    Global,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub action: Action,
}

impl MenuItem {
    fn new(label: impl Into<String>, shortcut: Option<&str>, action: Action) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            action,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub context: ContextTarget,
    pub anchor: (u16, u16),
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ContextMenu {
    pub fn build(target: ContextTarget, anchor: (u16, u16), app: &AppState, ui: &UiState) -> Self {
        let items = match target {
            ContextTarget::NavRow(index) => vec![
                MenuItem::new("열기", Some("Enter"), Action::SelectNavRow(index)),
                MenuItem::new("목록 새로고침", Some("F5"), Action::RefreshAll),
                MenuItem::new(
                    "새 채팅",
                    Some("Ctrl+N"),
                    Action::OpenOverlay(OverlayKind::NewChat),
                ),
            ],
            ContextTarget::Message { scope, msg_index } => {
                let msg = app.message_at(scope, msg_index);
                let is_channel_root = scope == MessageScope::Chat
                    && app
                        .active_conversation()
                        .is_some_and(|c| c.kind == ConversationKind::Channel)
                    && msg.is_some_and(|m| m.parent_msg_id.is_none());
                let has_attach = msg.is_some_and(|m| !m.attachments.is_empty());
                let is_me = msg.is_some_and(|m| m.is_me);

                let mut items = Vec::new();
                if is_channel_root {
                    items.push(MenuItem::new(
                        "스레드 열기",
                        Some("t"),
                        Action::OpenThread { msg_index },
                    ));
                }
                items.push(MenuItem::new(
                    "답장 / 인용",
                    Some("r"),
                    Action::ReplyTo { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "반응 추가",
                    Some("+"),
                    Action::OpenReactions { scope, msg_index },
                ));
                items.push(MenuItem::new(
                    "메시지 복사",
                    Some("y"),
                    Action::CopyMessage { scope, msg_index },
                ));
                if has_attach {
                    items.push(MenuItem::new(
                        "첨부 저장",
                        Some("d"),
                        Action::DownloadAttachment {
                            scope,
                            msg_index,
                            attach_index: ui.attach_index,
                        },
                    ));
                }
                if is_me {
                    items.push(MenuItem::new(
                        "메시지 삭제",
                        Some("Del"),
                        Action::RequestDeleteMessage { scope, msg_index },
                    ));
                }
                items
            }
            ContextTarget::Composer => vec![
                MenuItem::new("전송", Some("Enter"), Action::SendMessage),
                MenuItem::new("제목 필드 토글", Some("Ctrl+S"), Action::ToggleSubject),
                MenuItem::new("외부 편집기", Some("Ctrl+E"), Action::OpenExternalEditor),
                MenuItem::new("답장 취소", Some("Esc"), Action::CancelReply),
                MenuItem::new("초안/첨부 지우기", None, Action::ClearComposer),
            ],
            ContextTarget::Global => vec![
                MenuItem::new("검색", Some("Ctrl+F"), Action::OpenSearch),
                MenuItem::new(
                    "새 채팅",
                    Some("Ctrl+N"),
                    Action::OpenOverlay(OverlayKind::NewChat),
                ),
                MenuItem::new(
                    "내 상태 변경",
                    Some("Ctrl+P"),
                    Action::OpenOverlay(OverlayKind::Presence),
                ),
                MenuItem::new("새로고침", Some("F5"), Action::RefreshAll),
                MenuItem::new(
                    "설정",
                    Some("Ctrl+,"),
                    Action::OpenOverlay(OverlayKind::Settings),
                ),
                MenuItem::new(
                    "도움말",
                    Some("?"),
                    Action::OpenOverlay(OverlayKind::Help),
                ),
                MenuItem::new("종료", Some("q"), Action::Quit),
            ],
        };

        Self {
            context: target,
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    pub fn activate(&self) -> OverlayOutcome {
        match self.items.get(self.selected) {
            Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
            None => OverlayOutcome::Close,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let label_width = self
            .items
            .iter()
            .map(|item| {
                unicode_width::UnicodeWidthStr::width(item.label.as_str())
                    + item
                        .shortcut
                        .as_deref()
                        .map_or(0, |shortcut| shortcut.len() + 2)
            })
            .max()
            .unwrap_or(18);
        let width = (label_width as u16 + 4).clamp(22, 48);
        let height = (self.items.len() as u16 + 2).min(area.height);
        let rect = anchored_rect(area, self.anchor, width, height);

        let title = match self.context {
            ContextTarget::NavRow(_) => " 대화 메뉴 ",
            ContextTarget::Message { .. } => " 메시지 메뉴 ",
            ContextTarget::Composer => " 입력 메뉴 ",
            ContextTarget::Global => " 메뉴 ",
        };
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        for (index, item) in self.items.iter().enumerate() {
            if index as u16 >= inner.height {
                break;
            }
            let row = inner.y + index as u16;
            let selected = index == self.selected;
            let label_style = if selected {
                theme.selection().remove_modifier(Modifier::BOLD)
            } else {
                ratatui::style::Style::default()
            };
            let hint_style = if selected {
                theme.selection()
            } else {
                theme.muted_style()
            };
            let used = unicode_width::UnicodeWidthStr::width(item.label.as_str());
            let hint = item.shortcut.clone().unwrap_or_default();
            let pad = inner
                .width
                .saturating_sub(used as u16)
                .saturating_sub(unicode_width::UnicodeWidthStr::width(hint.as_str()) as u16)
                .saturating_sub(2) as usize;
            let line = Line::from(vec![
                Span::styled(format!(" {:<used$}", item.label), label_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(hint, hint_style),
                Span::styled(" ", label_style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, row, inner.width, 1),
            );
            hitmap.add_line(inner.x, row, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }
}

fn anchored_rect(area: Rect, anchor: (u16, u16), width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = anchor.0.min(area.x + area.width.saturating_sub(width));
    let y = anchor.1.min(area.y + area.height.saturating_sub(height));
    Rect::new(x, y, width, height)
}
