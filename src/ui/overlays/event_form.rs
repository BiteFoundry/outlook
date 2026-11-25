//! Calendar event creation modal (`n` or `[+ New Event]` on Calendar tab).

use chrono::{Duration, Local, NaiveDateTime, Timelike};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::OutgoingEvent,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct EventFormOverlay {
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

impl Default for EventFormOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl EventFormOverlay {
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
            self.error = Some("Enter an event subject".to_string());
            return OverlayOutcome::Handled;
        }
        let Some(start_iso) = parse_local_datetime_to_iso(&self.start) else {
            self.field = 1;
            self.error = Some("Start must be YYYY-MM-DD HH:MM".to_string());
            return OverlayOutcome::Handled;
        };
        let Some(end_iso) = parse_local_datetime_to_iso(&self.end) else {
            self.field = 2;
            self.error = Some("End must be YYYY-MM-DD HH:MM".to_string());
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
                Some(crate::parser::compile_outgoing_mail_html(trimmed, &[], None))
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
            Action::OverlaySubmit | Action::SaveDraft => self.submit(),
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
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(82),
            area.height.saturating_sub(4).min(15),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " New Calendar Event · Tab Next · Ctrl+S Save · Esc Cancel ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
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
            (0, "Subject:       ", self.subject.clone()),
            (1, "Start (local): ", self.start.clone()),
            (2, "End (local):   ", self.end.clone()),
            (3, "Attendees (,): ", self.attendees.clone()),
            (4, "Location:      ", self.location.clone()),
            (
                5,
                "Teams Meeting: ",
                if self.is_online_meeting {
                    "[x] Include online meeting link (Space toggle)".to_string()
                } else {
                    "[ ] Include online meeting link (Space toggle)".to_string()
                },
            ),
            (6, "Notes:         ", self.notes.clone()),
        ];

        let footer_rows: u16 = if self.error.is_some() && inner.height >= 3 {
            2
        } else {
            1
        };
        let field_viewport_h = inner.height.saturating_sub(footer_rows) as usize;
        let focused_field = self.field.min(rows.len().saturating_sub(1));
        let start = if field_viewport_h == 0 {
            0
        } else if focused_field >= field_viewport_h {
            (focused_field + 1 - field_viewport_h).min(rows.len().saturating_sub(field_viewport_h))
        } else {
            0
        };

        let mut y = inner.y;
        for (idx, label, value) in rows.into_iter().skip(start).take(field_viewport_h) {
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
            if inner.height >= 3 {
                let err_y = inner.bottom().saturating_sub(2);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&format!(" Error: {err}"), inner.width as usize),
                        Style::default().fg(theme.error),
                    )),
                    Rect::new(inner.x, err_y, inner.width, 1),
                );
            }
        }

        let btn_y = inner.bottom().saturating_sub(1);
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
        let save_label = " [Create Event (Ctrl+S)] ";
        let cancel_label = " [Cancel (Esc)] ";
        let save_w = UnicodeWidthStr::width(save_label) as u16;
        let cancel_w = UnicodeWidthStr::width(cancel_label) as u16;
        let bx = inner.x.saturating_add(1);
        let save_rect = Rect::new(bx, btn_y, save_w.min(inner.right().saturating_sub(bx)), 1);
        if save_rect.width > 0 {
            frame.render_widget(Paragraph::new(Span::styled(save_label, save_st)), save_rect);
            hitmap.add(save_rect, HitTarget::Button(ButtonId::EventFormSubmit));
        }
        let cx = bx.saturating_add(save_w).saturating_add(2);
        if cx < inner.right() {
            let cancel_rect =
                Rect::new(cx, btn_y, cancel_w.min(inner.right().saturating_sub(cx)), 1);
            if cancel_rect.width > 0 {
                frame.render_widget(
                    Paragraph::new(Span::styled(cancel_label, cancel_st)),
                    cancel_rect,
                );
                hitmap.add(cancel_rect, HitTarget::Button(ButtonId::EventFormCancel));
            }
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
        let mut form = EventFormOverlay::new();
        assert!(matches!(form.submit(), OverlayOutcome::Handled));
        assert!(form.error.is_some());

        form.subject = "Sprint Review".to_string();
        form.start = "2026-11-19 14:00".to_string();
        form.end = "2026-11-19 15:00".to_string();
        form.attendees = "a@contoso.com, b@contoso.com".to_string();
        form.field = 5;
        form.handle(&Action::Input(' '));
        assert!(!form.is_online_meeting);
        form.handle(&Action::Activate);
        assert!(form.is_online_meeting);

        match form.submit() {
            OverlayOutcome::CloseAndDispatch(Action::CreateCalendarEvent(ev)) => {
                assert_eq!(ev.subject, "Sprint Review");
                assert!(ev.is_online_meeting);
                assert_eq!(ev.attendees, vec!["a@contoso.com", "b@contoso.com"]);
                assert!(ev.start_iso.ends_with('Z') || ev.start_iso.contains('+'));
            }
            other => panic!("expected CreateCalendarEvent, got {other:?}"),
        }
    }
}
