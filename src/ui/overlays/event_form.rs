//! Calendar event creation modal (`n` or `[+ 새 일정 만들기]` on Calendar tab).

use chrono::{Duration, Local, NaiveDateTime, Timelike};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::OutgoingEvent,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct EventCreateOverlay {
    pub subject: String,
    pub start: String,
    pub end: String,
    pub attendees: String,
    pub location: String,
    pub is_online_meeting: bool,
    pub notes: String,
    /// `0`=Subject, `1`=Start, `2`=End, `3`=Attendees, `4`=Location, `5`=OnlineMeeting toggle, `6`=Notes, `7`=Submit/Cancel buttons.
    pub field: usize,
    pub button_idx: usize,
    pub error: Option<String>,
}

impl Default for EventCreateOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl EventCreateOverlay {
    pub fn new() -> Self {
        let now = Local::now();
        let start_dt = (now + Duration::hours(1))
            .with_minute(0)
            .and_then(|d| d.with_second(0))
            .unwrap_or(now + Duration::hours(1));
        let end_dt = start_dt + Duration::hours(1);
        Self {
            subject: String::new(),
            start: start_dt.format("%Y-%m-%d %H:%M").to_string(),
            end: end_dt.format("%Y-%m-%d %H:%M").to_string(),
            attendees: String::new(),
            location: String::new(),
            is_online_meeting: true,
            notes: String::new(),
            field: 0,
            button_idx: 0,
            error: None,
        }
    }

    pub fn select(&mut self, index: usize) {
        if index <= 7 {
            self.field = index;
        }
    }

    fn active_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.subject),
            1 => Some(&mut self.start),
            2 => Some(&mut self.end),
            3 => Some(&mut self.attendees),
            4 => Some(&mut self.location),
            6 => Some(&mut self.notes),
            _ => None,
        }
    }

    pub fn submit(&mut self) -> OverlayOutcome {
        let subject = self.subject.trim().to_string();
        if subject.is_empty() {
            self.field = 0;
            self.error = Some("일정 제목을 입력하세요".to_string());
            return OverlayOutcome::Handled;
        }
        let Some(start_iso) = parse_local_datetime_to_iso(&self.start) else {
            self.field = 1;
            self.error = Some("시작 일시는 YYYY-MM-DD HH:MM 형식이어야 합니다".to_string());
            return OverlayOutcome::Handled;
        };
        let Some(end_iso) = parse_local_datetime_to_iso(&self.end) else {
            self.field = 2;
            self.error = Some("종료 일시는 YYYY-MM-DD HH:MM 형식이어야 합니다".to_string());
            return OverlayOutcome::Handled;
        };
        let attendees: Vec<String> = self
            .attendees
            .split([',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let location = {
            let trimmed = self.location.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        let body_html = {
            let trimmed = self.notes.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(
                    crate::parser::compile_outgoing_markdown(
                        trimmed,
                        None,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        None,
                    )
                    .body_html,
                )
            }
        };
        OverlayOutcome::CloseAndDispatch(Action::CreateCalendarEvent(OutgoingEvent {
            subject,
            start_iso,
            end_iso,
            time_zone: Some("UTC".to_string()),
            is_all_day: false,
            location,
            body_html,
            attendees,
            is_online_meeting: self.is_online_meeting,
        }))
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.error = None;
                if self.field == 5 {
                    if *ch == ' ' {
                        self.is_online_meeting = !self.is_online_meeting;
                    }
                    return OverlayOutcome::Handled;
                }
                if self.field == 7 {
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
                self.field = (self.field + 1) % 8;
                OverlayOutcome::Handled
            }
            Action::PrevFocus | Action::MoveUp => {
                self.field = (self.field + 7) % 8;
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::CursorRight if self.field == 7 => {
                self.button_idx = 1 - self.button_idx;
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if self.field == 5 {
                    self.is_online_meeting = !self.is_online_meeting;
                    return OverlayOutcome::Handled;
                }
                if self.field == 7 {
                    return if self.button_idx == 0 {
                        self.submit()
                    } else {
                        OverlayOutcome::Close
                    };
                }
                self.field = (self.field + 1) % 8;
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
            area.width.saturating_sub(6).min(82),
            area.height.saturating_sub(4).min(17),
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                " 새 일정 만들기 · Tab 이동 · Ctrl+S 저장 · Esc 닫기 ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 6 {
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

        let rows: [(usize, &str, String); 7] = [
            (0, "제목:        ", self.subject.clone()),
            (1, "시작(YYYY-MM-DD HH:MM): ", self.start.clone()),
            (2, "종료(YYYY-MM-DD HH:MM): ", self.end.clone()),
            (3, "참석자(,):   ", self.attendees.clone()),
            (4, "장소:        ", self.location.clone()),
            (
                5,
                "Teams 회의:  ",
                if self.is_online_meeting {
                    "[x] Teams 온라인 회의 생성 (Space 토글)".to_string()
                } else {
                    "[ ] Teams 온라인 회의 생성 (Space 토글)".to_string()
                },
            ),
            (6, "메모:        ", self.notes.clone()),
        ];

        let mut y = inner.y;
        for (idx, label, value) in rows {
            if y >= inner.y + inner.height.saturating_sub(2) {
                break;
            }
            let max_v = (inner.width as usize).saturating_sub(UnicodeWidthStr::width(label) + 3);
            let shown = crate::ui::truncate_width(&value, max_v);
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
            let save_st = if self.field == 7 && self.button_idx == 0 {
                theme.selection()
            } else {
                theme.accent_style()
            };
            let cancel_st = if self.field == 7 && self.button_idx == 1 {
                theme.selection()
            } else {
                theme.muted_style()
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(" [일정 만들기 (Ctrl+S)] ", save_st),
                    Span::raw("  "),
                    Span::styled(" [취소 (Esc)] ", cancel_st),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: 7 });
        }
    }
}

pub fn parse_local_datetime_to_iso(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
    }
    let naive = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    let local_dt = naive
        .and_local_timezone(Local)
        .earliest()
        .or_else(|| naive.and_local_timezone(Local).latest())?;
    Some(local_dt.with_timezone(&chrono::Utc).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_submits_event_with_online_meeting_toggle() {
        let mut form = EventCreateOverlay::new();
        assert!(matches!(form.submit(), OverlayOutcome::Handled));
        assert!(form.error.is_some());

        form.subject = "스프린트 리뷰".to_string();
        form.start = "2026-11-16 14:00".to_string();
        form.end = "2026-11-16 15:00".to_string();
        form.attendees = "a@contoso.com, b@contoso.com".to_string();
        form.field = 5;
        form.handle(&Action::Input(' '));
        assert!(!form.is_online_meeting);
        form.handle(&Action::Activate);
        assert!(form.is_online_meeting);

        match form.submit() {
            OverlayOutcome::CloseAndDispatch(Action::CreateCalendarEvent(ev)) => {
                assert_eq!(ev.subject, "스프린트 리뷰");
                assert!(ev.is_online_meeting);
                assert_eq!(ev.attendees, vec!["a@contoso.com", "b@contoso.com"]);
                assert!(ev.start_iso.ends_with('Z') || ev.start_iso.contains('+'));
            }
            other => panic!("expected CreateCalendarEvent, got {other:?}"),
        }
    }
}
