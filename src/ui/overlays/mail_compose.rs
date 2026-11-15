//! Mail compose / reply / reply-all / forward overlay (`c`, `r`, `R`, `f`).

use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::{ContactItem, MailMessage},
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, path_input, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailComposeMode {
    New,
    Reply { message_id: String },
    ReplyAll { message_id: String },
    Forward { message_id: String },
}

#[derive(Debug, Clone)]
pub struct MailComposeOverlay {
    pub mode: MailComposeMode,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub attachments: String,
    pub body: String,
    /// `0` = To, `1` = Cc, `2` = Subject, `3` = Attachments, `4` = Body, `5` = Send/Cancel buttons.
    pub field: usize,
    /// `0` = Send, `1` = Cancel.
    pub button_idx: usize,
    pub suggestion_idx: usize,
    pub error: Option<String>,
}

impl Default for MailComposeOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl MailComposeOverlay {
    pub fn new() -> Self {
        Self {
            mode: MailComposeMode::New,
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            attachments: String::new(),
            body: String::new(),
            field: 0,
            button_idx: 0,
            suggestion_idx: 0,
            error: None,
        }
    }

    pub fn new_to(recipient: impl Into<String>) -> Self {
        let mut s = Self::new();
        s.to = recipient.into();
        s.field = 2; // focus Subject directly when recipient is prefilled
        s
    }

    pub fn reply(mail: &MailMessage, reply_all: bool) -> Self {
        let sender_addr = mail
            .from
            .as_ref()
            .map(|r| r.address.clone())
            .unwrap_or_default();
        let to = if reply_all {
            let mut list = Vec::new();
            if !sender_addr.is_empty() {
                list.push(sender_addr);
            }
            for r in &mail.to_recipients {
                if !r.address.trim().is_empty()
                    && !list.iter().any(|x| x.eq_ignore_ascii_case(&r.address))
                {
                    list.push(r.address.clone());
                }
            }
            list.join(", ")
        } else {
            sender_addr
        };
        let cc = if reply_all {
            mail.cc_recipients
                .iter()
                .filter(|r| !r.address.trim().is_empty())
                .map(|r| r.address.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            String::new()
        };
        let subject = if mail.subject.to_ascii_lowercase().starts_with("re:") {
            mail.subject.clone()
        } else {
            format!("Re: {}", mail.subject)
        };
        Self {
            mode: if reply_all {
                MailComposeMode::ReplyAll {
                    message_id: mail.id.clone(),
                }
            } else {
                MailComposeMode::Reply {
                    message_id: mail.id.clone(),
                }
            },
            to,
            cc,
            subject,
            attachments: String::new(),
            body: String::new(),
            field: 4, // jump directly to reply body
            button_idx: 0,
            suggestion_idx: 0,
            error: None,
        }
    }

    pub fn forward(mail: &MailMessage) -> Self {
        let subject = if mail.subject.to_ascii_lowercase().starts_with("fwd:") {
            mail.subject.clone()
        } else {
            format!("Fwd: {}", mail.subject)
        };
        let quoted_body = if mail.body_plain.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n---------- 전달된 메시지 ----------\n보낸사람: {}\n제목: {}\n\n{}",
                mail.from_display(),
                mail.subject,
                mail.body_plain.trim()
            )
        };
        Self {
            mode: MailComposeMode::Forward {
                message_id: mail.id.clone(),
            },
            to: String::new(),
            cc: String::new(),
            subject,
            attachments: String::new(),
            body: quoted_body,
            field: 0, // focus To recipient
            button_idx: 0,
            suggestion_idx: 0,
            error: None,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            MailComposeMode::New => " 새 메일 작성 · Tab 이동 · Ctrl+Enter 전송 · Esc 닫기 ",
            MailComposeMode::Reply { .. } => " 답장 작성 · Tab 이동 · Ctrl+Enter 전송 · Esc 닫기 ",
            MailComposeMode::ReplyAll { .. } => {
                " 전체 답장 · Tab 이동 · Ctrl+Enter 전송 · Esc 닫기 "
            }
            MailComposeMode::Forward { .. } => {
                " 메일 전달 · Tab 이동 · Ctrl+Enter 전송 · Esc 닫기 "
            }
        }
    }

    pub fn select(&mut self, index: usize) {
        if index <= 5 {
            self.field = index;
        }
    }

    pub fn parsed_to(&self) -> Vec<String> {
        split_emails(&self.to)
    }

    pub fn parsed_cc(&self) -> Vec<String> {
        split_emails(&self.cc)
    }

    pub fn parsed_attachments(&self) -> Vec<PathBuf> {
        self.attachments
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(path_input::expand_path)
            .collect()
    }

    pub fn matching_contacts<'a>(&self, contacts: &'a [ContactItem]) -> Vec<(&'a ContactItem, &'a str)> {
        if self.field != 0 && self.field != 1 {
            return Vec::new();
        }
        let raw = if self.field == 0 {
            &self.to
        } else {
            &self.cc
        };
        let token = raw.rsplit(',').next().unwrap_or("").trim();
        let query = token.strip_prefix('@').unwrap_or(token).trim();
        if query.is_empty() && !token.starts_with('@') {
            return Vec::new();
        }
        let q = query.to_lowercase();
        contacts
            .iter()
            .filter_map(|c| {
                let email = c.primary_email()?;
                if q.is_empty()
                    || c.display_name.to_lowercase().contains(&q)
                    || email.to_lowercase().contains(&q)
                {
                    Some((c, email))
                } else {
                    None
                }
            })
            .take(5)
            .collect()
    }

    fn apply_contact_suggestion(&mut self, email: &str) {
        let target = if self.field == 0 {
            &mut self.to
        } else {
            &mut self.cc
        };
        if let Some((prefix, _)) = target.rsplit_once(',') {
            *target = format!("{}, {email}, ", prefix.trim_end());
        } else {
            *target = format!("{email}, ");
        }
        self.suggestion_idx = 0;
    }

    fn active_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.to),
            1 => Some(&mut self.cc),
            2 => Some(&mut self.subject),
            3 => Some(&mut self.attachments),
            4 => Some(&mut self.body),
            _ => None,
        }
    }

    pub fn submit(&mut self) -> OverlayOutcome {
        let to = self.parsed_to();
        let cc = self.parsed_cc();
        match &self.mode {
            MailComposeMode::New => {
                if to.is_empty() {
                    self.field = 0;
                    self.error = Some("받는 사람(To) 이메일을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                if self.subject.trim().is_empty() {
                    self.field = 2;
                    self.error = Some("메일 제목을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
            }
            MailComposeMode::Forward { .. } => {
                if to.is_empty() {
                    self.field = 0;
                    self.error = Some("전달할 받는 사람(To) 이메일을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
            }
            MailComposeMode::Reply { .. } | MailComposeMode::ReplyAll { .. } => {
                if self.body.trim().is_empty() {
                    self.field = 4;
                    self.error = Some("답장 내용을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
            }
        }
        let attachments = self.parsed_attachments();
        for path in &attachments {
            if !path.is_file() {
                self.field = 3;
                self.error = Some(format!("첨부 파일을 찾을 수 없습니다: {}", path.display()));
                return OverlayOutcome::Handled;
            }
        }
        OverlayOutcome::CloseAndDispatch(Action::SubmitMailCompose {
            mode: self.mode.clone(),
            to,
            cc,
            subject: self.subject.trim().to_string(),
            body_markdown: self.body.clone(),
            attachments,
        })
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let suggestions_len = self.matching_contacts(&app.contacts).len();
        match action {
            Action::Input(ch) => {
                self.error = None;
                if self.field == 5 {
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
                    self.suggestion_idx = 0;
                }
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.error = None;
                let is_body = self.field == 4;
                if let Some(buf) = self.active_text_mut() {
                    if is_body {
                        buf.push_str(text);
                    } else {
                        buf.push_str(&text.replace(['\r', '\n'], " "));
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.error = None;
                if let Some(buf) = self.active_text_mut() {
                    buf.pop();
                    self.suggestion_idx = 0;
                }
                OverlayOutcome::Handled
            }
            Action::InsertNewline => {
                if self.field == 4 {
                    self.body.push('\n');
                    OverlayOutcome::Handled
                } else {
                    self.submit()
                }
            }
            Action::OverlaySubmit => self.submit(),
            Action::Tab => {
                self.field = (self.field + 1) % 6;
                self.suggestion_idx = 0;
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.field = (self.field + 5) % 6;
                self.suggestion_idx = 0;
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if (self.field == 0 || self.field == 1) && suggestions_len > 0 {
                    self.suggestion_idx = (self.suggestion_idx + 1) % suggestions_len;
                } else {
                    self.field = (self.field + 1) % 6;
                }
                OverlayOutcome::Handled
            }
            Action::MoveUp => {
                if (self.field == 0 || self.field == 1) && suggestions_len > 0 {
                    self.suggestion_idx =
                        (self.suggestion_idx + suggestions_len - 1) % suggestions_len;
                } else {
                    self.field = (self.field + 5) % 6;
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::CursorRight if self.field == 5 => {
                self.button_idx = 1 - self.button_idx;
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if (self.field == 0 || self.field == 1) && suggestions_len > 0 {
                    let chosen = self
                        .matching_contacts(&app.contacts)
                        .get(self.suggestion_idx)
                        .map(|(_, email)| (*email).to_string());
                    if let Some(email) = chosen {
                        self.apply_contact_suggestion(&email);
                        return OverlayOutcome::Handled;
                    }
                }
                if self.field == 4 {
                    self.body.push('\n');
                    return OverlayOutcome::Handled;
                }
                if self.field == 5 {
                    return if self.button_idx == 0 {
                        self.submit()
                    } else {
                        OverlayOutcome::Close
                    };
                }
                self.field = (self.field + 1) % 6;
                OverlayOutcome::Handled
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
            area.width.saturating_sub(6).min(92),
            area.height.saturating_sub(4).min(26),
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(self.title(), theme.title(true))))
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

        let mut y = inner.y;
        let header_fields: [(usize, &str, &str); 4] = [
            (0, "받는사람(To): ", &self.to),
            (1, "참조(Cc):     ", &self.cc),
            (2, "제목:         ", &self.subject),
            (3, "첨부파일(,):  ", &self.attachments),
        ];

        for (idx, label, value) in header_fields {
            if y >= inner.y + inner.height.saturating_sub(4) {
                break;
            }
            let max_v = (inner.width as usize).saturating_sub(UnicodeWidthStr::width(label) + 3);
            let shown = crate::ui::truncate_width(value, max_v);
            let line = Line::from(vec![
                Span::styled(format!(" {label}"), field_style(idx)),
                Span::raw(shown),
                Span::styled(cursor_for(idx), field_style(idx)),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
            y = y.saturating_add(1);
        }

        // Contact suggestions dropdown for To/Cc
        let suggestions = self.matching_contacts(&app.contacts);
        if !suggestions.is_empty() && y < inner.y + inner.height.saturating_sub(4) {
            for (s_idx, (contact, email)) in suggestions.iter().enumerate() {
                if y >= inner.y + inner.height.saturating_sub(4) {
                    break;
                }
                let st = if s_idx == self.suggestion_idx {
                    theme.selection()
                } else {
                    theme.muted_style()
                };
                let text = format!("   ↳ {} <{email}>", contact.display_name);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&text, inner.width as usize),
                        st,
                    )),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y = y.saturating_add(1);
            }
        }

        // Body label + multiline body box
        let footer_rows = if self.error.is_some() { 3 } else { 2 };
        let body_avail = inner
            .y
            .saturating_add(inner.height)
            .saturating_sub(footer_rows)
            .saturating_sub(y);
        if body_avail >= 2 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " 본문 (Markdown · Enter 줄바꿈 · Ctrl+S / Ctrl+Enter 전송):",
                    field_style(4),
                ))),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: 4 });
            y = y.saturating_add(1);

            let text_rows = body_avail.saturating_sub(1) as usize;
            let body_lines: Vec<&str> = if self.body.is_empty() {
                vec![""]
            } else {
                self.body.split('\n').collect()
            };
            let skip = body_lines.len().saturating_sub(text_rows);
            for (row_off, raw_line) in body_lines.iter().skip(skip).take(text_rows).enumerate() {
                let is_last = skip + row_off + 1 == body_lines.len();
                let mut spans = vec![
                    Span::styled(" │ ", field_style(4)),
                    Span::raw(crate::ui::truncate_width(
                        raw_line,
                        (inner.width as usize).saturating_sub(5),
                    )),
                ];
                if is_last && self.field == 4 {
                    spans.push(Span::styled("▏", theme.accent_style()));
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)),
                    Rect::new(inner.x, y + row_off as u16, inner.width, 1),
                );
            }
            y = y.saturating_add(text_rows as u16);
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
            let send_st = if self.field == 5 && self.button_idx == 0 {
                theme.selection()
            } else {
                theme.accent_style()
            };
            let cancel_st = if self.field == 5 && self.button_idx == 1 {
                theme.selection()
            } else {
                theme.muted_style()
            };
            let btn_line = Line::from(vec![
                Span::raw(" "),
                Span::styled(" [전송 (Ctrl+S)] ", send_st),
                Span::raw("  "),
                Span::styled(" [취소 (Esc)] ", cancel_st),
            ]);
            frame.render_widget(
                Paragraph::new(btn_line),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: 5 });
        }
    }
}

fn split_emails(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mail() -> MailMessage {
        MailMessage {
            id: "mail-1".to_string(),
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: "주간 보고".to_string(),
            from: Some(crate::models::EmailRecipient {
                name: Some("홍길동".to_string()),
                address: "hong@example.com".to_string(),
            }),
            to_recipients: vec![crate::models::EmailRecipient {
                name: Some("팀원".to_string()),
                address: "team@example.com".to_string(),
            }],
            cc_recipients: vec![crate::models::EmailRecipient {
                name: None,
                address: "cc@example.com".to_string(),
            }],
            bcc_recipients: vec![],
            received_at: "2026-11-15T10:00:00Z".to_string(),
            sent_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            has_attachments: false,
            preview: "내용".to_string(),
            body_html: "<p>본문</p>".to_string(),
            body_plain: "본문".to_string(),
            attachments: vec![],
            web_link: None,
        }
    }

    #[test]
    fn reply_and_forward_prefill_recipients_and_subject() {
        let mail = sample_mail();
        let reply = MailComposeOverlay::reply(&mail, false);
        assert_eq!(reply.parsed_to(), vec!["hong@example.com"]);
        assert!(reply.parsed_cc().is_empty());
        assert_eq!(reply.subject, "Re: 주간 보고");
        assert_eq!(reply.field, 4);

        let reply_all = MailComposeOverlay::reply(&mail, true);
        assert_eq!(
            reply_all.parsed_to(),
            vec!["hong@example.com", "team@example.com"]
        );
        assert_eq!(reply_all.parsed_cc(), vec!["cc@example.com"]);

        let fwd = MailComposeOverlay::forward(&mail);
        assert!(fwd.parsed_to().is_empty());
        assert_eq!(fwd.subject, "Fwd: 주간 보고");
        assert!(fwd.body.contains("전달된 메시지"));
    }

    #[test]
    fn validation_requires_to_and_subject_for_new_mail() {
        let mut compose = MailComposeOverlay::new();
        assert!(matches!(compose.submit(), OverlayOutcome::Handled));
        assert!(compose.error.is_some());
        assert_eq!(compose.field, 0);

        compose.to = "a@b.com".to_string();
        assert!(matches!(compose.submit(), OverlayOutcome::Handled));
        assert_eq!(compose.field, 2);

        compose.subject = "테스트".to_string();
        compose.body = "안녕".to_string();
        match compose.submit() {
            OverlayOutcome::CloseAndDispatch(Action::SubmitMailCompose {
                to,
                subject,
                body_markdown,
                ..
            }) => {
                assert_eq!(to, vec!["a@b.com"]);
                assert_eq!(subject, "테스트");
                assert_eq!(body_markdown, "안녕");
            }
            other => panic!("expected SubmitMailCompose, got {other:?}"),
        }
    }

    #[test]
    fn contact_suggestions_match_at_or_partial_name() {
        let mut compose = MailComposeOverlay::new();
        compose.to = "@김".to_string();
        let contacts = vec![ContactItem {
            id: "c1".to_string(),
            display_name: "김철수".to_string(),
            given_name: None,
            surname: None,
            email_addresses: vec!["kim@contoso.com".to_string()],
            mobile_phone: None,
            business_phones: vec![],
            company_name: None,
            job_title: None,
            department: None,
            notes: None,
            source: "contact".to_string(),
        }];
        let hits = compose.matching_contacts(&contacts);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "kim@contoso.com");

        compose.apply_contact_suggestion("kim@contoso.com");
        assert_eq!(compose.parsed_to(), vec!["kim@contoso.com"]);
    }
}
