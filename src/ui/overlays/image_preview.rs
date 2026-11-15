//! Full-screen inline terminal image preview modal (`v`).
//! Supports Teams message attachments and Outlook Mail image attachments,
//! prev/next cycling (`[`/`]`, `←`/`→`), zoom (`+`/`-`), download (`d`), and close (`Esc`/`q`/`v`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview,
    models::{AttachmentInfo, MailAttachment, MailMessage, MessageItem},
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::HitMap,
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewTarget {
    Teams {
        msg_id: String,
        attachment: AttachmentInfo,
    },
    Mail {
        mail_id: String,
        attachment: MailAttachment,
    },
}

impl PreviewTarget {
    pub fn owner_id(&self) -> &str {
        match self {
            PreviewTarget::Teams { msg_id, .. } => msg_id,
            PreviewTarget::Mail { mail_id, .. } => mail_id,
        }
    }

    pub fn att_id(&self) -> &str {
        match self {
            PreviewTarget::Teams { attachment, .. } => &attachment.id,
            PreviewTarget::Mail { attachment, .. } => &attachment.id,
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            PreviewTarget::Teams { attachment, .. } => attachment
                .name
                .clone()
                .unwrap_or_else(|| format!("image_{}", attachment.id)),
            PreviewTarget::Mail { attachment, .. } => attachment.name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub items: Vec<PreviewTarget>,
    pub index: usize,
    /// Zoom percentage (`25..=200`, default `100`).
    pub zoom_pct: u16,
    pub error: Option<String>,
}

impl ImagePreviewOverlay {
    pub fn from_teams_message(msg: &MessageItem, preferred_att_idx: Option<usize>) -> Option<Self> {
        let mut items = Vec::new();
        let mut start = 0usize;
        for (raw_idx, att) in msg.attachments.iter().enumerate() {
            if img_preview::is_image_mime_or_name(&att.content_type, att.name.as_deref()) {
                if preferred_att_idx == Some(raw_idx) {
                    start = items.len();
                }
                items.push(PreviewTarget::Teams {
                    msg_id: msg.msg_id.clone(),
                    attachment: att.clone(),
                });
            }
        }
        if items.is_empty() {
            return None;
        }
        Some(Self {
            items,
            index: start,
            zoom_pct: 100,
            error: None,
        })
    }

    pub fn from_mail_message(mail: &MailMessage, preferred_att_idx: Option<usize>) -> Option<Self> {
        let mut items = Vec::new();
        let mut start = 0usize;
        for (raw_idx, att) in mail.attachments.iter().enumerate() {
            if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                if preferred_att_idx == Some(raw_idx) {
                    start = items.len();
                }
                items.push(PreviewTarget::Mail {
                    mail_id: mail.id.clone(),
                    attachment: att.clone(),
                });
            }
        }
        if items.is_empty() {
            return None;
        }
        Some(Self {
            items,
            index: start,
            zoom_pct: 100,
            error: None,
        })
    }

    pub fn current(&self) -> Option<&PreviewTarget> {
        self.items.get(self.index)
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let base_cols = rect.width.saturating_sub(4).max(12);
        let base_rows = rect.height.saturating_sub(4).max(6);
        let scaled_cols = ((base_cols as u32 * self.zoom_pct as u32) / 100).clamp(8, base_cols as u32) as u16;
        let scaled_rows = ((base_rows as u32 * self.zoom_pct as u32) / 100).clamp(4, base_rows as u32) as u16;
        (scaled_cols, scaled_rows)
    }

    pub fn next_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + 1) % self.items.len();
            self.error = None;
        }
    }

    pub fn prev_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + self.items.len() - 1) % self.items.len();
            self.error = None;
        }
    }

    pub fn zoom_in(&mut self) {
        self.zoom_pct = (self.zoom_pct + 25).min(200);
    }

    pub fn zoom_out(&mut self) {
        self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v')
            | Action::Quit => OverlayOutcome::Close,
            Action::Input('[') | Action::AttachmentPrev | Action::CursorLeft => {
                self.prev_image();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input(']') | Action::AttachmentNext | Action::CursorRight => {
                self.next_image();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_in();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('-') => {
                self.zoom_out();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('d') | Action::DownloadLatest | Action::MailDownloadAttachment => {
                OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        _hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);

        let (cols, rows) = self.target_dimensions(area.width, area.height);
        let Some(cur) = self.current() else {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(Line::from(Span::styled(" 이미지 미리보기 ", theme.title(true))))
                .border_style(theme.border_style(true));
            frame.render_widget(Paragraph::new("표시할 이미지가 없습니다").block(block), rect);
            return;
        };

        let cache_key = AppState::image_cache_key(cur.owner_id(), cur.att_id(), cols, rows);
        let cached = app.image_cache.get(&cache_key);

        let dim_info = match cached {
            Some(img) => format!(" · {}x{} {}", img.width_px, img.height_px, img.format_label),
            None => String::new(),
        };
        let title = format!(
            " 🖼 {} ({}/{}){} · {}% ",
            cur.display_name(),
            self.index + 1,
            self.items.len(),
            dim_info,
            self.zoom_pct
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let img_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
        let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);

        if let Some(rendered) = cached {
            let x_pad = img_area.width.saturating_sub(rendered.cols) / 2;
            let y_pad = img_area.height.saturating_sub(rendered.rows) / 2;
            let target_rect = Rect::new(
                img_area.x.saturating_add(x_pad),
                img_area.y.saturating_add(y_pad),
                rendered.cols.min(img_area.width),
                rendered.rows.min(img_area.height),
            );
            frame.render_widget(Paragraph::new(rendered.lines.clone()), target_rect);
        } else if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" 이미지 로드/디코드 실패: {err}"),
                    Style::default().fg(theme.error),
                )),
                img_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " 이미지를 불러오고 있습니다…",
                    theme.muted_style(),
                )),
                img_area,
            );
        }

        let footer = " [/]: 이전·다음 이미지   +/-: 확대·축소   d: 다운로드   Esc/q/v: 닫기 ";
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(footer, footer_area.width as usize),
                theme.muted_style(),
            )),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail_with_attachments() -> MailMessage {
        MailMessage {
            id: "mail-9".to_string(),
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: "스크린샷".to_string(),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            received_at: "2026-11-15T10:00:00Z".to_string(),
            sent_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            has_attachments: true,
            preview: String::new(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: vec![
                MailAttachment {
                    id: "att-pdf".to_string(),
                    name: "doc.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    size: 100,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: None,
                },
                MailAttachment {
                    id: "att-png".to_string(),
                    name: "shot1.png".to_string(),
                    content_type: "image/png".to_string(),
                    size: 200,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: None,
                },
                MailAttachment {
                    id: "att-jpg".to_string(),
                    name: "photo2.jpg".to_string(),
                    content_type: "image/jpeg".to_string(),
                    size: 300,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: None,
                },
            ],
            web_link: None,
        }
    }

    #[test]
    fn filters_only_images_and_cycles_and_zooms() {
        let mail = mail_with_attachments();
        let mut overlay = ImagePreviewOverlay::from_mail_message(&mail, Some(2)).unwrap();
        assert_eq!(overlay.items.len(), 2);
        assert_eq!(overlay.index, 1);
        assert_eq!(overlay.current().unwrap().att_id(), "att-jpg");

        assert!(matches!(
            overlay.handle(&Action::Input('[')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        ));
        assert_eq!(overlay.index, 0);
        assert_eq!(overlay.current().unwrap().att_id(), "att-png");

        overlay.handle(&Action::Input('+'));
        assert_eq!(overlay.zoom_pct, 125);
        overlay.handle(&Action::Input('-'));
        overlay.handle(&Action::Input('-'));
        assert_eq!(overlay.zoom_pct, 75);

        assert!(matches!(
            overlay.handle(&Action::Input('d')),
            OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
        ));
        assert!(matches!(
            overlay.handle(&Action::Input('v')),
            OverlayOutcome::Close
        ));
    }
}
