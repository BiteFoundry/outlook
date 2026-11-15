//! Personal contact creation modal (`n` or `[+ 새 연락처]` on Contacts tab).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::OutgoingContact,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Default)]
pub struct ContactCreateOverlay {
    pub display_name: String,
    pub email: String,
    pub mobile_phone: String,
    pub company_name: String,
    pub job_title: String,
    pub notes: String,
    /// `0`=Name, `1`=Email, `2`=Mobile, `3`=Company, `4`=Title, `5`=Notes, `6`=Save/Cancel buttons.
    pub field: usize,
    pub button_idx: usize,
    pub error: Option<String>,
}

impl ContactCreateOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select(&mut self, index: usize) {
        if index <= 6 {
            self.field = index;
        }
    }

    fn active_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.display_name),
            1 => Some(&mut self.email),
            2 => Some(&mut self.mobile_phone),
            3 => Some(&mut self.company_name),
            4 => Some(&mut self.job_title),
            5 => Some(&mut self.notes),
            _ => None,
        }
    }

    pub fn submit(&mut self) -> OverlayOutcome {
        let name = self.display_name.trim().to_string();
        if name.is_empty() {
            self.field = 0;
            self.error = Some("이름을 입력하세요".to_string());
            return OverlayOutcome::Handled;
        }
        let emails: Vec<String> = self
            .email
            .split([',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let opt = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        OverlayOutcome::CloseAndDispatch(Action::CreateContact(OutgoingContact {
            display_name: name,
            given_name: None,
            surname: None,
            email_addresses: emails,
            mobile_phone: opt(&self.mobile_phone),
            company_name: opt(&self.company_name),
            job_title: opt(&self.job_title),
            department: None,
            notes: opt(&self.notes),
        }))
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.error = None;
                if self.field == 6 {
                    if *ch == ' ' {
                        return if self.button_idx == 0 {
                            self.submit()
                        } else {
                            OverlayOutcome::Close
                        };
                    }
                    return OverlayOutcome::Handled;
                }
                if let Some(buf) = self.active_text_mut() {
                    buf.push(*ch);
                }
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.error = None;
                if let Some(buf) = self.active_text_mut() {
                    buf.push_str(&text.replace(['\r', '\n'], " "));
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.error = None;
                if let Some(buf) = self.active_text_mut() {
                    buf.pop();
                }
                OverlayOutcome::Handled
            }
            Action::OverlaySubmit => self.submit(),
            Action::Tab | Action::MoveDown => {
                self.field = (self.field + 1) % 7;
                OverlayOutcome::Handled
            }
            Action::PrevFocus | Action::MoveUp => {
                self.field = (self.field + 6) % 7;
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::CursorRight if self.field == 6 => {
                self.button_idx = 1 - self.button_idx;
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if self.field == 6 {
                    return if self.button_idx == 0 {
                        self.submit()
                    } else {
                        OverlayOutcome::Close
                    };
                }
                self.field = (self.field + 1) % 7;
                OverlayOutcome::Handled
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(76),
            area.height.saturating_sub(4).min(15),
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 새 연락처 · Tab 이동 · Ctrl+S 저장 · Esc 닫기 ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 5 {
            return;
        }

        let field_style = |idx: usize| {
            if self.field == idx {
                theme.accent_style()
            } else {
                theme.muted_style()
            }
        };
        let cursor_for = |idx: usize| if self.field == idx { "▏" } else { "" };

        let rows: [(usize, &str, &str); 6] = [
            (0, "이름:       ", &self.display_name),
            (1, "이메일:     ", &self.email),
            (2, "휴대전화:   ", &self.mobile_phone),
            (3, "회사:       ", &self.company_name),
            (4, "직책:       ", &self.job_title),
            (5, "메모:       ", &self.notes),
        ];

        let mut y = inner.y;
        for (idx, label, value) in rows {
            if y >= inner.y + inner.height.saturating_sub(2) {
                break;
            }
            let max_v = (inner.width as usize).saturating_sub(UnicodeWidthStr::width(label) + 3);
            let shown = crate::ui::truncate_width(value, max_v);
            let line = Line::from(vec![
                Span::styled(format!(" {label}"), field_style(idx)),
                Span::raw(shown),
                Span::styled(cursor_for(idx), field_style(idx)),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
            y = y.saturating_add(1);
        }

        if let Some(err) = &self.error {
            if y < inner.y + inner.height.saturating_sub(1) {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&format!(" 오류: {err}"), inner.width as usize),
                        Style::default().fg(theme.error),
                    )),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y = y.saturating_add(1);
            }
        }

        if y < inner.y + inner.height {
            let save_st = if self.field == 6 && self.button_idx == 0 {
                theme.selection()
            } else {
                theme.accent_style()
            };
            let cancel_st = if self.field == 6 && self.button_idx == 1 {
                theme.selection()
            } else {
                theme.muted_style()
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(" [저장 (Ctrl+S)] ", save_st),
                    Span::raw("  "),
                    Span::styled(" [취소 (Esc)] ", cancel_st),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: 6 });
        }
    }
}
