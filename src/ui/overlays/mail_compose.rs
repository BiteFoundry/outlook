//! Structured Mail Compose / Reply / Reply-All / Forward overlay (`c`, `r`, `R`, `f`).
//!
//! Fields: `To:`, `Cc:`, `Bcc:` (with live Contacts/Directory autocomplete dropdown),
//! `Subject:`, multiline soft-wrapped Markdown `Body` (`ComposerState`), file attachments,
//! and clipboard inline `cid:` screenshot staging.

use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::MailDraftEntry,
    models::{ContactItem, InlineImageUpload, MailMessage},
    ui::{
        action::Action,
        app::{AppState, UiState},
        composer::{slice_display_window, truncate_display, ComposerState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayKind, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailComposeMode {
    New,
    Reply { message_id: String },
    ReplyAll { message_id: String },
    Forward { message_id: String },
}

impl MailComposeMode {
    pub fn draft_key(&self) -> String {
        match self {
            Self::New => "compose:new".to_string(),
            Self::Reply { message_id } => format!("compose:reply:{message_id}"),
            Self::ReplyAll { message_id } => format!("compose:reply_all:{message_id}"),
            Self::Forward { message_id } => format!("compose:forward:{message_id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailComposeOverlay {
    pub mode: MailComposeMode,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: ComposerState,
    pub attachments: Vec<PathBuf>,
    /// `0` = To, `1` = Cc, `2` = Bcc, `3` = Subject, `4` = Body, `5` = Action buttons.
    pub field: usize,
    /// `0` = Send, `1` = Attach, `2` = Save Draft, `3` = Cancel.
    pub button_idx: usize,
    pub suggestion_idx: usize,
    pub suggestion_dismissed: bool,
    pub contacts_sync_requested: bool,
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
            bcc: String::new(),
            subject: String::new(),
            body: ComposerState::new(),
            attachments: Vec::new(),
            field: 0,
            button_idx: 0,
            suggestion_idx: 0,
            suggestion_dismissed: false,
            contacts_sync_requested: false,
            error: None,
        }
    }

    pub fn new_to(recipient: impl Into<String>) -> Self {
        let mut s = Self::new();
        s.to = format!("{}, ", recipient.into().trim().trim_end_matches(','));
        s.field = 3; // jump to Subject
        s
    }

    pub fn from_draft_entry(mode: MailComposeMode, draft: &MailDraftEntry) -> Self {
        let mut s = Self::new();
        s.mode = mode;
        s.to = draft.to.clone();
        s.cc = draft.cc.clone();
        s.bcc = draft.bcc.clone();
        s.subject = draft.subject.clone();
        s.body.load_text(&draft.body);
        s.field = if s.to.trim().is_empty() { 0 } else { 4 };
        s
    }

    pub fn to_draft_entry(&self) -> MailDraftEntry {
        let (reply_to_id, forward_of_id) = match &self.mode {
            MailComposeMode::New => (None, None),
            MailComposeMode::Reply { message_id } | MailComposeMode::ReplyAll { message_id } => {
                (Some(message_id.clone()), None)
            }
            MailComposeMode::Forward { message_id } => (None, Some(message_id.clone())),
        };
        MailDraftEntry {
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: self.bcc.clone(),
            subject: self.subject.clone(),
            body: self.body.text().to_string(),
            reply_to_id,
            forward_of_id,
        }
    }

    pub fn has_unsent_content(&self) -> bool {
        !self.body.is_empty()
            || !self.attachments.is_empty()
            || (!self.subject.trim().is_empty() && matches!(self.mode, MailComposeMode::New))
            || (!self.to.trim().is_empty() && matches!(self.mode, MailComposeMode::New))
    }

    pub fn reply(mail: &MailMessage, reply_all: bool, my_email: &str) -> Self {
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
                let addr = r.address.trim();
                if !addr.is_empty()
                    && !addr.eq_ignore_ascii_case(my_email.trim())
                    && !list.iter().any(|x| x.eq_ignore_ascii_case(addr))
                {
                    list.push(addr.to_string());
                }
            }
            list.join(", ")
        } else {
            sender_addr
        };
        let cc = if reply_all {
            mail.cc_recipients
                .iter()
                .map(|r| r.address.trim())
                .filter(|addr| !addr.is_empty() && !addr.eq_ignore_ascii_case(my_email.trim()))
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
        let mut s = Self::new();
        s.mode = if reply_all {
            MailComposeMode::ReplyAll {
                message_id: mail.id.clone(),
            }
        } else {
            MailComposeMode::Reply {
                message_id: mail.id.clone(),
            }
        };
        s.to = to;
        s.cc = cc;
        s.subject = subject;
        s.field = 4; // focus Body directly
        s
    }

    pub fn forward(mail: &MailMessage) -> Self {
        let subject = if mail.subject.to_ascii_lowercase().starts_with("fwd:") {
            mail.subject.clone()
        } else {
            format!("Fwd: {}", mail.subject)
        };
        let plain = mail.body_plain.trim();
        let quoted_body = if plain.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\n\n{}",
                mail.from_display(),
                mail.received_at,
                mail.effective_subject(),
                plain
            )
        };
        let mut s = Self::new();
        s.mode = MailComposeMode::Forward {
            message_id: mail.id.clone(),
        };
        s.subject = subject;
        s.body.load_text_cursor_start(&quoted_body);
        s.field = 0; // focus To:
        s
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            MailComposeMode::New => {
                " New Email · Tab Next · Ctrl+Enter/Alt+Enter Send · Ctrl+S Draft · Esc Close "
            }
            MailComposeMode::Reply { .. } => {
                " Reply · Tab Next · Ctrl+Enter/Alt+Enter Send · Ctrl+S Draft · Esc Close "
            }
            MailComposeMode::ReplyAll { .. } => {
                " Reply All · Tab Next · Ctrl+Enter/Alt+Enter Send · Ctrl+S Draft · Esc Close "
            }
            MailComposeMode::Forward { .. } => {
                " Forward Email · Tab Next · Ctrl+Enter/Alt+Enter Send · Ctrl+S Draft · Esc Close "
            }
        }
    }

    pub fn select_field(&mut self, index: usize) {
        if index <= 5 {
            self.field = index;
            self.suggestion_idx = 0;
            self.suggestion_dismissed = false;
        }
    }

    pub fn parsed_to(&self) -> Vec<String> {
        split_emails(&self.to)
    }

    pub fn parsed_cc(&self) -> Vec<String> {
        split_emails(&self.cc)
    }

    pub fn parsed_bcc(&self) -> Vec<String> {
        split_emails(&self.bcc)
    }

    pub fn matching_contacts<'a>(
        &self,
        contacts: &'a [ContactItem],
    ) -> Vec<(&'a ContactItem, &'a str)> {
        if self.suggestion_dismissed || self.field > 2 {
            return Vec::new();
        }
        let raw = match self.field {
            0 => &self.to,
            1 => &self.cc,
            2 => &self.bcc,
            _ => return Vec::new(),
        };
        let token = raw.rsplit([',', ';']).next().unwrap_or("").trim();
        let query = token.strip_prefix('@').unwrap_or(token).trim();
        if query.is_empty() && !token.starts_with('@') {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut out: Vec<(&'a ContactItem, &'a str)> = contacts
            .iter()
            .filter_map(|c| {
                let email = c.primary_email()?;
                let name_l = c.display_name.to_lowercase();
                let email_l = email.to_lowercase();
                if q.is_empty() || name_l.contains(&q) || email_l.contains(&q) {
                    Some((c, email))
                } else {
                    None
                }
            })
            .collect();
        out.sort_by(|(ca, ea), (cb, eb)| {
            let a_starts = !q.is_empty()
                && (ca.display_name.to_lowercase().starts_with(&q)
                    || ea.to_lowercase().starts_with(&q));
            let b_starts = !q.is_empty()
                && (cb.display_name.to_lowercase().starts_with(&q)
                    || eb.to_lowercase().starts_with(&q));
            b_starts
                .cmp(&a_starts)
                .then_with(|| ca.display_name.cmp(&cb.display_name))
        });
        out.truncate(5);
        out
    }

    pub fn apply_contact_suggestion(&mut self, email: &str) {
        let target = match self.field {
            0 => &mut self.to,
            1 => &mut self.cc,
            2 => &mut self.bcc,
            _ => return,
        };
        if let Some(sep_idx) = target.rfind([',', ';']) {
            let prefix = target[..=sep_idx].trim_end();
            *target = format!("{prefix} {email}, ");
        } else {
            *target = format!("{email}, ");
        }
        self.suggestion_idx = 0;
        self.suggestion_dismissed = false;
    }

    pub fn attach_file(&mut self, path: PathBuf) {
        if !self.attachments.contains(&path) {
            self.attachments.push(path);
        }
        self.error = None;
    }

    fn active_header_mut(&mut self) -> Option<&mut String> {
        match self.field {
            0 => Some(&mut self.to),
            1 => Some(&mut self.cc),
            2 => Some(&mut self.bcc),
            3 => Some(&mut self.subject),
            _ => None,
        }
    }

    pub fn submit(&mut self) -> OverlayOutcome {
        let to = self.parsed_to();
        let cc = self.parsed_cc();
        let bcc = self.parsed_bcc();
        match &self.mode {
            MailComposeMode::New => {
                if to.is_empty() && cc.is_empty() && bcc.is_empty() {
                    self.field = 0;
                    self.error = Some("Enter at least one recipient (To/Cc/Bcc)".to_string());
                    return OverlayOutcome::Handled;
                }
                if self.subject.trim().is_empty() {
                    self.field = 3;
                    self.error = Some("Enter an email subject".to_string());
                    return OverlayOutcome::Handled;
                }
            }
            MailComposeMode::Forward { .. } => {
                if to.is_empty() {
                    self.field = 0;
                    self.error = Some("Enter a recipient to forward to".to_string());
                    return OverlayOutcome::Handled;
                }
            }
            MailComposeMode::Reply { .. } | MailComposeMode::ReplyAll { .. } => {
                if self.body.is_empty() && self.attachments.is_empty() {
                    self.field = 4;
                    self.error = Some("Enter a reply message".to_string());
                    return OverlayOutcome::Handled;
                }
            }
        }
        for path in &self.attachments {
            if !path.is_file() {
                self.error = Some(format!("Attachment file not found: {}", path.display()));
                return OverlayOutcome::Handled;
            }
        }
        let inline_images: Vec<InlineImageUpload> = self.body.pending_images.clone();
        OverlayOutcome::CloseAndDispatch(Action::SubmitMailCompose {
            mode: self.mode.clone(),
            to,
            cc,
            bcc,
            subject: self.subject.trim().to_string(),
            body_markdown: self.body.text().to_string(),
            attachments: self.attachments.clone(),
            inline_images,
        })
    }

    pub fn maybe_request_contacts_sync(&mut self, app: &AppState) {
        if self.field <= 2 && !self.contacts_sync_requested {
            self.contacts_sync_requested = true;
            app.request_contacts_sync();
        }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let suggestions = self.matching_contacts(&app.contacts);
        let suggestions_len = suggestions.len();
        let has_suggestions = self.field <= 2 && suggestions_len > 0;

        let outcome = match action {
            Action::OverlaySubmit => self.submit(),
            Action::SaveDraft => OverlayOutcome::Dispatch(Action::SaveMailDraft),
            Action::OpenAttachPicker | Action::OpenFilePicker => {
                OverlayOutcome::Dispatch(Action::OpenOverlay(OverlayKind::FilePicker))
            }
            Action::OpenExternalEditor => OverlayOutcome::NotHandled,
            Action::PasteClipboard => OverlayOutcome::NotHandled,
            Action::ClearPendingImages => {
                self.body.clear_pending_images();
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.error = None;
                self.suggestion_dismissed = false;
                if self.field == 4 {
                    self.body.insert_char(*ch);
                    return OverlayOutcome::Handled;
                }
                if self.field == 5 {
                    if *ch == ' ' {
                        return self.activate_button();
                    }
                    return OverlayOutcome::Handled;
                }
                if let Some(buf) = self.active_header_mut() {
                    buf.push(*ch);
                    self.suggestion_idx = 0;
                }
                if self.field <= 2 {
                    self.maybe_request_contacts_sync(app);
                }
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.error = None;
                if self.field == 4 {
                    self.body.insert_str(text);
                } else if let Some(buf) = self.active_header_mut() {
                    buf.push_str(&text.replace(['\r', '\n'], " "));
                    self.suggestion_dismissed = false;
                    if self.field <= 2 {
                        self.maybe_request_contacts_sync(app);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.error = None;
                if self.field == 4 {
                    self.body.backspace();
                } else if let Some(buf) = self.active_header_mut() {
                    buf.pop();
                    self.suggestion_idx = 0;
                    self.suggestion_dismissed = false;
                    if self.field <= 2 {
                        self.maybe_request_contacts_sync(app);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::DeleteWordBackward if self.field == 4 => {
                self.body.delete_word_backward();
                OverlayOutcome::Handled
            }
            Action::DeleteForward if self.field == 4 => {
                self.body.delete_forward();
                OverlayOutcome::Handled
            }
            Action::InsertNewline => {
                if self.field == 4 {
                    self.body.newline();
                    OverlayOutcome::Handled
                } else {
                    self.submit()
                }
            }
            Action::Tab => {
                if has_suggestions {
                    let idx = self.suggestion_idx.min(suggestions_len - 1);
                    let email = suggestions[idx].1.to_string();
                    self.apply_contact_suggestion(&email);
                } else {
                    self.field = (self.field + 1) % 6;
                    self.suggestion_idx = 0;
                    self.suggestion_dismissed = false;
                }
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.field = (self.field + 5) % 6;
                self.suggestion_idx = 0;
                self.suggestion_dismissed = false;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown => {
                if has_suggestions {
                    self.suggestion_idx = (self.suggestion_idx + 1) % suggestions_len;
                } else if self.field == 4 {
                    self.body.move_down(false);
                } else {
                    self.field = (self.field + 1) % 6;
                }
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::CursorUp => {
                if has_suggestions {
                    self.suggestion_idx =
                        (self.suggestion_idx + suggestions_len - 1) % suggestions_len;
                } else if self.field == 4 {
                    self.body.move_up(false);
                } else {
                    self.field = (self.field + 5) % 6;
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft => {
                if self.field == 4 {
                    self.body.move_left(false);
                } else if self.field == 5 {
                    self.button_idx = (self.button_idx + 3) % 4;
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight => {
                if self.field == 4 {
                    self.body.move_right(false);
                } else if self.field == 5 {
                    self.button_idx = (self.button_idx + 1) % 4;
                }
                OverlayOutcome::Handled
            }
            Action::CursorHome if self.field == 4 => {
                self.body.move_home(false);
                OverlayOutcome::Handled
            }
            Action::CursorEnd if self.field == 4 => {
                self.body.move_end(false);
                OverlayOutcome::Handled
            }
            Action::SelectAll if self.field == 4 => {
                self.body.select_all();
                OverlayOutcome::Handled
            }
            Action::Undo if self.field == 4 => {
                self.body.undo();
                OverlayOutcome::Handled
            }
            Action::Redo if self.field == 4 => {
                self.body.redo();
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if has_suggestions {
                    let idx = self.suggestion_idx.min(suggestions_len - 1);
                    let email = suggestions[idx].1.to_string();
                    self.apply_contact_suggestion(&email);
                    return OverlayOutcome::Handled;
                }
                if self.field == 4 {
                    self.body.newline();
                    return OverlayOutcome::Handled;
                }
                if self.field == 5 {
                    return self.activate_button();
                }
                self.field = (self.field + 1) % 6;
                OverlayOutcome::Handled
            }
            Action::CloseOverlay => {
                if has_suggestions {
                    self.suggestion_dismissed = true;
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::StashMailDraft(Box::new(self.clone())))
            }
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        };
        if self.field <= 2 && matches!(outcome, OverlayOutcome::Handled) {
            self.maybe_request_contacts_sync(app);
        }
        outcome
    }

    fn activate_button(&mut self) -> OverlayOutcome {
        match self.button_idx {
            0 => self.submit(),
            1 => OverlayOutcome::Dispatch(Action::OpenOverlay(OverlayKind::FilePicker)),
            2 => OverlayOutcome::Dispatch(Action::SaveMailDraft),
            _ => OverlayOutcome::CloseAndDispatch(Action::StashMailDraft(Box::new(self.clone()))),
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) -> Option<(u16, u16)> {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(4).min(96),
            area.height.saturating_sub(2).min(30),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(self.title(), rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width < 12 || inner.height < 8 {
            return None;
        }

        let label_style = |idx: usize| {
            if self.field == idx {
                theme.accent_style().add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            }
        };

        let mut y = inner.y;
        let mut cursor_pos: Option<(u16, u16)> = None;
        let header_fields: [(usize, &str, &str); 4] = [
            (0, "To:      ", &self.to),
            (1, "Cc:      ", &self.cc),
            (2, "Bcc:     ", &self.bcc),
            (3, "Subject: ", &self.subject),
        ];

        for (idx, label, value) in header_fields {
            if y >= inner.bottom().saturating_sub(4) {
                break;
            }
            let lw = UnicodeWidthStr::width(label);
            let max_v = (inner.width as usize).saturating_sub(lw + 2);
            let (shown, ccol) = if self.field == idx {
                slice_display_window(value, value.len(), max_v)
            } else {
                (truncate_display(value, max_v), 0)
            };
            let line = Line::from(vec![
                Span::styled(format!(" {label}"), label_style(idx)),
                Span::raw(shown),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::ComposeField(idx));
            if self.field == idx {
                cursor_pos = Some((
                    inner
                        .x
                        .saturating_add(1 + lw as u16 + ccol as u16)
                        .min(inner.right().saturating_sub(1)),
                    y,
                ));
            }
            y = y.saturating_add(1);
        }

        // Attachments + inline clipboard image chips row
        if !self.attachments.is_empty() || !self.body.pending_images.is_empty() {
            let mut spans = vec![Span::styled(" Attach:  ", theme.muted_style())];
            for path in &self.attachments {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                spans.push(Span::styled(
                    format!("[FILE {name}] "),
                    theme.accent_style().add_modifier(Modifier::BOLD),
                ));
            }
            for img in &self.body.pending_images {
                spans.push(Span::styled(
                    format!("[IMG {} (cid)] ", img.file_name),
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);

            if ui.inline_images {
                if let Some(ref preview) = self.body.pending_preview {
                    for thumb_line in preview.lines.iter().take(3) {
                        if y >= inner.bottom().saturating_sub(4) {
                            break;
                        }
                        frame.render_widget(
                            Paragraph::new(thumb_line.clone()),
                            Rect::new(inner.x + 1, y, inner.width.saturating_sub(1), 1),
                        );
                        y = y.saturating_add(1);
                    }
                }
            }
        }

        // Hairline separator above Body
        if y < inner.bottom().saturating_sub(2) {
            let rule_prefix = if self.field == 4 {
                "─ Body (Markdown · Ctrl+V paste image · Ctrl+F attach · Ctrl+E $EDITOR) "
            } else {
                "─ Body (Markdown) "
            };
            let mut rule = truncate_display(rule_prefix, inner.width as usize);
            let used = UnicodeWidthStr::width(rule.as_str());
            for _ in used..(inner.width as usize) {
                rule.push('─');
            }
            let rule_style = if self.field == 4 {
                theme.accent_style()
            } else {
                theme.subtle_style()
            };
            frame.render_widget(
                Paragraph::new(Span::styled(rule, rule_style)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y = y.saturating_add(1);
        }

        let footer_rows: u16 = if self.error.is_some() { 2 } else { 1 };
        let body_bottom = inner.bottom().saturating_sub(footer_rows);
        if body_bottom > y {
            let body_area = Rect::new(
                inner.x.saturating_add(1),
                y,
                inner.width.saturating_sub(2),
                body_bottom - y,
            );
            let body_bg = if theme.no_color {
                Style::default()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let body_cursor = self.body.draw_editor(
                frame,
                body_area,
                theme,
                self.field == 4,
                "Write email in Markdown…",
                body_bg,
            );
            hitmap.add(body_area, HitTarget::ComposeField(4));
            if self.field == 4 {
                cursor_pos = body_cursor;
            }
            y = body_bottom;
        }

        if let Some(err) = &self.error {
            if y < inner.bottom().saturating_sub(1) {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&format!(" Error: {err}"), inner.width as usize),
                        Style::default().fg(theme.error),
                    )),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y = y.saturating_add(1);
            }
        }

        // Action buttons row
        if y < inner.bottom() {
            let full_specs = [
                (" [Send (Ctrl+Enter)] ", ButtonId::ComposeSend),
                (" [+ Attach (Ctrl+F)] ", ButtonId::ComposeAttach),
                (" [Save Draft (Ctrl+S)] ", ButtonId::ComposeSaveDraft),
                (" [Cancel (Esc)] ", ButtonId::ComposeCancel),
            ];
            let compact_specs = [
                ("[Send]", ButtonId::ComposeSend),
                ("[+Attach]", ButtonId::ComposeAttach),
                ("[Draft]", ButtonId::ComposeSaveDraft),
                ("[Cancel]", ButtonId::ComposeCancel),
            ];
            let full_w: u16 = full_specs
                .iter()
                .map(|(label, _)| UnicodeWidthStr::width(*label) as u16 + 1)
                .sum();
            let btn_specs = if full_w <= inner.width.saturating_sub(1) {
                &full_specs
            } else {
                &compact_specs
            };
            let mut bx = inner.x.saturating_add(1);
            for (idx, (label, btn_id)) in btn_specs.iter().enumerate() {
                let w = UnicodeWidthStr::width(*label) as u16;
                if bx.saturating_add(w) > inner.right() {
                    break;
                }
                let st = if self.field == 5 && self.button_idx == idx {
                    theme.selection()
                } else if idx == 0 {
                    theme.accent_style().add_modifier(Modifier::BOLD)
                } else {
                    theme.muted_style()
                };
                let r = Rect::new(bx, y, w, 1);
                frame.render_widget(Paragraph::new(Span::styled(*label, st)), r);
                hitmap.add(r, HitTarget::Button(*btn_id));
                bx = bx.saturating_add(w + 1);
            }
        }

        // Render live Contacts/Directory autocomplete dropdown OVER the body when typing in To/Cc/Bcc
        let suggestions = self.matching_contacts(&app.contacts);
        if self.field <= 2 && !suggestions.is_empty() {
            let anchor_y = inner.y + (self.field as u16) + 1;
            let drop_h = (suggestions.len() as u16 + 2).min(inner.bottom().saturating_sub(anchor_y));
            if drop_h >= 3 {
                let drop_w = inner.width.saturating_sub(10).clamp(24, 68);
                let drop_rect = Rect::new(inner.x.saturating_add(10), anchor_y, drop_w, drop_h);
                frame.render_widget(Clear, drop_rect);
                hitmap.add(drop_rect, HitTarget::OverlayBody);
                let drop_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg))
                    .title(Line::from(Span::styled(
                        " Contacts · Tab/Enter Insert ",
                        theme.muted_style(),
                    )))
                    .border_style(theme.border_style(true));
                let drop_inner = drop_block.inner(drop_rect);
                frame.render_widget(drop_block, drop_rect);

                let active_sel = self.suggestion_idx.min(suggestions.len() - 1);
                for (i, (contact, email)) in suggestions.iter().enumerate() {
                    if i as u16 >= drop_inner.height {
                        break;
                    }
                    let ry = drop_inner.y + i as u16;
                    let is_sel = i == active_sel;
                    let style = if is_sel {
                        theme.selection()
                    } else {
                        Style::default()
                    };
                    let tag = if contact.source == "directory" {
                        "[Dir]"
                    } else {
                        "[Contact]"
                    };
                    let label = format!(" {} <{}> {tag}", contact.display_name, email);
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            crate::ui::truncate_width(&label, drop_inner.width as usize),
                            style,
                        )),
                        Rect::new(drop_inner.x, ry, drop_inner.width, 1),
                    );
                    hitmap.add_line(
                        drop_inner.x,
                        ry,
                        drop_inner.width,
                        HitTarget::ComposeContactSuggestion(i),
                    );
                }
            }
        }

        cursor_pos
    }
}

pub fn split_emails(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::{AppConfig, DraftStore},
        graph::GraphClient,
        models::EmailRecipient,
        storage::Storage,
        sync::{self, SyncCommand},
    };
    use tokio::sync::mpsc;

    fn sample_contacts() -> Vec<ContactItem> {
        vec![
            ContactItem {
                id: "c1".into(),
                display_name: "Alice Smith".into(),
                given_name: Some("Alice".into()),
                surname: Some("Smith".into()),
                email_addresses: vec!["alice@contoso.com".into()],
                mobile_phone: None,
                business_phones: vec![],
                company_name: Some("Contoso".into()),
                job_title: None,
                department: None,
                notes: None,
                source: "contact".into(),
            },
            ContactItem {
                id: "c2".into(),
                display_name: "Bob Jones".into(),
                given_name: Some("Bob".into()),
                surname: Some("Jones".into()),
                email_addresses: vec!["bob@contoso.com".into()],
                mobile_phone: None,
                business_phones: vec![],
                company_name: None,
                job_title: None,
                department: None,
                notes: None,
                source: "directory".into(),
            },
        ]
    }

    #[test]
    fn recipient_autocomplete_matches_and_inserts_chip() {
        let mut compose = MailComposeOverlay::new();
        compose.to = "al".to_string();
        compose.field = 0;
        let contacts = sample_contacts();
        let matches = compose.matching_contacts(&contacts);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1, "alice@contoso.com");

        compose.apply_contact_suggestion(matches[0].1);
        assert_eq!(compose.to, "alice@contoso.com, ");
        assert_eq!(compose.parsed_to(), vec!["alice@contoso.com"]);

        // Second recipient after comma
        compose.to.push_str("bob");
        let matches2 = compose.matching_contacts(&contacts);
        assert_eq!(matches2.len(), 1);
        assert_eq!(matches2[0].1, "bob@contoso.com");
        compose.apply_contact_suggestion(matches2[0].1);
        assert_eq!(compose.parsed_to(), vec!["alice@contoso.com", "bob@contoso.com"]);
    }

    #[test]
    fn reply_and_draft_stash_roundtrip() {
        let mail = MailMessage {
            id: "msg-42".into(),
            internet_message_id: None,
            conversation_id: Some("conv-42".into()),
            folder_id: Some("inbox".into()),
            subject: "Budget Q4".into(),
            from: Some(EmailRecipient {
                name: Some("Alice".into()),
                address: "alice@contoso.com".into(),
            }),
            to_recipients: vec![EmailRecipient {
                name: Some("Me".into()),
                address: "me@contoso.com".into(),
            }],
            cc_recipients: vec![EmailRecipient {
                name: Some("Carol".into()),
                address: "carol@contoso.com".into(),
            }],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-19T00:00:00Z".into(),
            sent_at: None,
            last_modified_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: None,
            has_attachments: false,
            body_loaded: true,
            preview: "Please review".into(),
            body_html: "<p>Please review</p>".into(),
            body_plain: "Please review".into(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        };

        let mut compose = MailComposeOverlay::reply(&mail, true, "me@contoso.com");
        assert_eq!(compose.subject, "Re: Budget Q4");
        assert_eq!(compose.parsed_to(), vec!["alice@contoso.com"]);
        assert_eq!(compose.parsed_cc(), vec!["carol@contoso.com"]);

        compose.body.insert_str("Looks good to me!");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drafts.json");
        let mut store = DraftStore::default();
        store.set_mail(compose.mode.draft_key(), compose.to_draft_entry());
        store.save_to(&path).unwrap();

        let loaded = DraftStore::load_from(&path).unwrap();
        let entry = loaded.get_mail(&compose.mode.draft_key()).unwrap();
        let restored = MailComposeOverlay::from_draft_entry(compose.mode.clone(), entry);
        assert_eq!(restored.body.text(), "Looks good to me!");
        assert_eq!(restored.subject, "Re: Budget Q4");
    }

    #[tokio::test]
    async fn handle_ctrl_enter_submits_mail_and_esc_stashes() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (tx, _) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);

        let mut compose = MailComposeOverlay::new();
        compose.to = "alice@contoso.com".into();
        compose.subject = "Hello".into();
        compose.field = 4;
        compose.body.insert_str("Body text");

        let esc_outcome = compose.handle(&Action::CloseOverlay, &app);
        assert!(matches!(
            esc_outcome,
            OverlayOutcome::CloseAndDispatch(Action::StashMailDraft(_))
        ));

        let send_outcome = compose.handle(&Action::OverlaySubmit, &app);
        assert!(matches!(
            send_outcome,
            OverlayOutcome::CloseAndDispatch(Action::SubmitMailCompose { .. })
        ));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}
