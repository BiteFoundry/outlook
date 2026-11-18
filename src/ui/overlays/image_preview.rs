//! Full-screen inline terminal image preview modal (`v` / `Alt+v` or clicking `[IMG ...]`).
//! Supports Outlook email attachments and inline `cid:` / `https:` images:
//! prev/next cycling (`[`/`]`, `←`/`→`), zoom (`+`/`-`/`0`/scroll), protocol cycle (`p`),
//! download (`d`), and close (`Esc`/`q`/`v`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview,
    models::{MailAttachment, MailMessage},
    parser,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewTarget {
    Mail {
        msg_id: String,
        attachment: MailAttachment,
    },
}

impl PreviewTarget {
    pub fn owner_id(&self) -> &str {
        match self {
            PreviewTarget::Mail { msg_id, .. } => msg_id,
        }
    }

    pub fn att_id(&self) -> &str {
        match self {
            PreviewTarget::Mail { attachment, .. } => &attachment.id,
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            PreviewTarget::Mail { attachment, .. } => {
                if attachment.name.trim().is_empty() {
                    format!("image_{}", attachment.id)
                } else {
                    attachment.name.clone()
                }
            }
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
    pub fn from_mail_message(msg: &MailMessage, preferred_att_idx: Option<usize>) -> Option<Self> {
        let mut items = Vec::new();
        let mut start = 0usize;
        for (raw_idx, att) in parser::effective_mail_attachments(msg).iter().enumerate() {
            if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                if preferred_att_idx == Some(raw_idx) {
                    start = items.len();
                }
                items.push(PreviewTarget::Mail {
                    msg_id: msg.id.clone(),
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
        let inner_w = rect.width.saturating_sub(2).max(12);
        let inner_h = rect.height.saturating_sub(3).max(6);
        let base_cols = rect.width.saturating_sub(4).max(12);
        let base_rows = rect.height.saturating_sub(4).max(6);
        let max_cols = inner_w.max(base_cols);
        let max_rows = inner_h.max(base_rows);
        let scaled_cols =
            ((base_cols as u32 * self.zoom_pct as u32) / 100).clamp(8, max_cols as u32) as u16;
        let scaled_rows =
            ((base_rows as u32 * self.zoom_pct as u32) / 100).clamp(4, max_rows as u32) as u16;
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

    pub fn reset_zoom(&mut self) {
        self.zoom_pct = 100;
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v') => OverlayOutcome::Close,
            Action::Quit
            | Action::QuitConfirm
            | Action::LoadPreviewImage
            | Action::DownloadPreviewImage
            | Action::CycleImageProtocol => OverlayOutcome::NotHandled,
            Action::Input('[') | Action::CursorLeft => {
                self.prev_image();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input(']') | Action::CursorRight => {
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
            Action::Input('0') => {
                self.reset_zoom();
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('p') => OverlayOutcome::Dispatch(Action::CycleImageProtocol),
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.zoom_in();
                    OverlayOutcome::Dispatch(Action::LoadPreviewImage)
                } else if *delta > 0 {
                    self.zoom_out();
                    OverlayOutcome::Dispatch(Action::LoadPreviewImage)
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input('d') | Action::DownloadLatestAttachment => {
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
        hitmap: &mut HitMap,
    ) {
        self.draw_inner(frame, area, app, ui, None, hitmap);
    }

    pub fn draw_sync(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let mut pending = None;
        self.draw_inner(frame, area, app, ui, Some(&mut pending), hitmap);
        ui.pending_pixel_graphic = pending;
    }

    fn draw_inner(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        pending_out: Option<&mut Option<PendingPixelGraphic>>,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let (cols, rows) = self.target_dimensions(area.width, area.height);
        let Some(cur) = self.current() else {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
                .title(Line::from(Span::styled(" Image Preview ", theme.title(true))))
                .border_style(theme.border_style(true));
            frame.render_widget(Paragraph::new("No image to display").block(block), rect);
            return;
        };

        let cache_key = AppState::preview_image_cache_key(
            cur.owner_id(),
            cur.att_id(),
            cols,
            rows,
            ui.graphics_caps.protocol,
        );
        let cached = app.image_cache.get(&cache_key);

        let dim_info = match cached {
            Some(img) => format!(" · {}x{} {}", img.width_px, img.height_px, img.format_label),
            None => String::new(),
        };
        let proto_label = ui.graphics_caps.protocol.label();
        let title = format!(
            " 🖼 {} ({}/{}){} · {}% [{}] ",
            cur.display_name(),
            self.index + 1,
            self.items.len(),
            dim_info,
            self.zoom_pct,
            proto_label,
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
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

        let err_opt = self
            .error
            .as_deref()
            .or_else(|| app.image_errors.get(&cache_key).map(String::as_str));

        if let Some(rendered) = cached {
            let target_w = rendered.cols.min(img_area.width);
            let target_h = rendered.rows.min(img_area.height);
            let x_pad = img_area.width.saturating_sub(target_w) / 2;
            let y_pad = img_area.height.saturating_sub(target_h) / 2;
            let target_rect = Rect::new(
                img_area.x.saturating_add(x_pad),
                img_area.y.saturating_add(y_pad),
                target_w,
                target_h,
            );
            if rendered.protocol.is_pixel() && rendered.pixel_payload.is_some() {
                if let Some(out) = pending_out {
                    *out = Some(PendingPixelGraphic {
                        cache_key: cache_key.clone(),
                        rect: target_rect,
                        protocol: rendered.protocol,
                    });
                }
                if ui.active_pixel_graphic.as_ref()
                    == Some(&(cache_key, target_rect, rendered.protocol))
                    && target_rect.width > 0
                    && target_rect.height > 0
                {
                    let buf = frame.buffer_mut();
                    for y in target_rect.top()..target_rect.bottom() {
                        for x in target_rect.left()..target_rect.right() {
                            if let Some(cell) = buf.cell_mut((x, y)) {
                                cell.set_skip(true);
                            }
                        }
                    }
                }
            } else {
                frame.render_widget(Paragraph::new(rendered.lines.clone()), target_rect);
            }
        } else if let Some(err) = err_opt {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" Failed to load/decode image: {err}"),
                    Style::default().fg(theme.error),
                )),
                img_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(" Loading image…", theme.muted_style())),
                img_area,
            );
        }

        let footer = " [/]: Prev/Next   +/- (0): Zoom   p: Protocol   d: Save   Esc/q/v: Close ";
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
    use crate::models::EmailRecipient;

    fn mail_with_attachments() -> MailMessage {
        MailMessage {
            id: "msg-9".to_string(),
            internet_message_id: None,
            conversation_id: Some("conv-9".to_string()),
            folder_id: Some("inbox".to_string()),
            subject: "Quarterly Charts".to_string(),
            from: Some(EmailRecipient {
                name: Some("Alice".to_string()),
                address: "alice@contoso.com".to_string(),
            }),
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-19T00:00:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: None,
            has_attachments: true,
            body_loaded: true,
            preview: String::new(),
            body_html: "<p>See <img src=\"cid:chart01@outlook\"></p>".to_string(),
            body_plain: String::new(),
            attachments: vec![
                MailAttachment {
                    id: "att-pdf".to_string(),
                    name: "doc.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    size: 1000,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: None,
                },
                MailAttachment {
                    id: "att-png".to_string(),
                    name: "chart.png".to_string(),
                    content_type: "image/png".to_string(),
                    size: 2048,
                    is_inline: true,
                    content_id: Some("<chart01@outlook>".to_string()),
                    content_bytes_b64: Some("AQID".to_string()),
                },
                MailAttachment {
                    id: "att-jpg".to_string(),
                    name: "photo2.jpg".to_string(),
                    content_type: "image/jpeg".to_string(),
                    size: 4096,
                    is_inline: false,
                    content_id: None,
                    content_bytes_b64: None,
                },
            ],
            meeting_request: None,
            web_link: None,
        }
    }

    #[test]
    fn filters_only_images_and_cycles_and_zooms() {
        let msg = mail_with_attachments();
        let mut overlay = ImagePreviewOverlay::from_mail_message(&msg, Some(2)).unwrap();
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
        let (cols_125, rows_125) = overlay.target_dimensions(120, 40);
        overlay.handle(&Action::Input('-'));
        let (cols_100, rows_100) = overlay.target_dimensions(120, 40);
        assert!(cols_125 > cols_100);
        assert!(rows_125 > rows_100);
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

    fn make_test_app_and_ui() -> (AppState, UiState) {
        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    #[test]
    fn zoom_reset_protocol_cycle_and_scroll_wheel_controls() {
        let msg = mail_with_attachments();
        let mut overlay = ImagePreviewOverlay::from_mail_message(&msg, Some(1)).unwrap();
        assert_eq!(overlay.zoom_pct, 100);

        assert_eq!(
            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 10,
                row: 10,
            }),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.zoom_pct, 125);

        assert_eq!(
            overlay.handle(&Action::Input('0')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.zoom_pct, 100);

        assert_eq!(
            overlay.handle(&Action::MouseScroll {
                delta: 3,
                column: 10,
                row: 10,
            }),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.zoom_pct, 75);

        assert_eq!(
            overlay.handle(&Action::Input('p')),
            OverlayOutcome::Dispatch(Action::CycleImageProtocol)
        );
        assert_eq!(
            overlay.handle(&Action::CycleImageProtocol),
            OverlayOutcome::NotHandled
        );
        assert_eq!(
            overlay.handle(&Action::LoadPreviewImage),
            OverlayOutcome::NotHandled
        );
        assert_eq!(
            overlay.handle(&Action::DownloadPreviewImage),
            OverlayOutcome::NotHandled
        );
    }

    #[tokio::test]
    async fn draw_sync_renders_sixel_badge_for_cid_inline_image_and_skips_active_cells() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_test_app_and_ui();
        let mut msg = mail_with_attachments();
        msg.attachments.clear();
        msg.body_html =
            "<p>Screenshot:</p><img src=\"cid:screenshot.png@outlook\">".to_string();

        let overlay = ImagePreviewOverlay::from_mail_message(&msg, None)
            .expect("cid inline img must produce preview overlay");
        assert_eq!(overlay.items.len(), 1);
        assert_eq!(overlay.current().unwrap().att_id(), "cid:screenshot.png@outlook");

        let area = Rect::new(0, 0, 100, 30);
        let (cols, rows) = overlay.target_dimensions(area.width, area.height);

        for (proto, badge) in [
            (img_preview::TerminalImageProtocol::Sixel, "[Sixel]"),
            (img_preview::TerminalImageProtocol::Kitty, "[Kitty]"),
            (img_preview::TerminalImageProtocol::ITerm2, "[iTerm2]"),
            (img_preview::TerminalImageProtocol::Halfblock, "[Halfblock]"),
        ] {
            ui.graphics_caps.protocol = proto;
            ui.active_pixel_graphic = None;
            ui.pending_pixel_graphic = None;
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hitmap = HitMap::default();
            terminal
                .draw(|frame| {
                    overlay.draw_sync(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let top_row: String = (0..area.width)
                .map(|x| buf[(x, 1)].symbol())
                .collect();
            assert!(
                top_row.contains(badge),
                "expected {badge} in title row: {top_row}"
            );
        }

        let proto = img_preview::TerminalImageProtocol::Sixel;
        ui.graphics_caps.protocol = proto;
        let cur = overlay.current().unwrap();
        let cache_key =
            AppState::preview_image_cache_key(cur.owner_id(), cur.att_id(), cols, rows, proto);
        app.insert_image_cache(
            cache_key.clone(),
            img_preview::RenderedTerminalImage {
                width_px: 320,
                height_px: 200,
                cols: 40,
                rows: 12,
                format_label: "PNG".to_string(),
                lines: Vec::new(),
                protocol: proto,
                pixel_payload: Some("\x1bPq#0;2;100;0;0#0!\x1b\\".to_string()),
            },
        );

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw_sync(frame, area, &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let pending = ui
            .pending_pixel_graphic
            .clone()
            .expect("pending_pixel_graphic must be set for Sixel");
        assert_eq!(pending.cache_key, cache_key);
        assert_eq!(pending.protocol, proto);

        ui.active_pixel_graphic = Some((pending.cache_key.clone(), pending.rect, pending.protocol));

        let mut skipped_inside = 0usize;
        terminal
            .draw(|frame| {
                overlay.draw_sync(frame, area, &app, &mut ui, &mut hitmap);
                let buf = frame.buffer_mut();
                for y in pending.rect.top()..pending.rect.bottom() {
                    for x in pending.rect.left()..pending.rect.right() {
                        if buf[(x, y)].skip {
                            skipped_inside += 1;
                        }
                    }
                }
            })
            .unwrap();
        assert_eq!(
            skipped_inside,
            (pending.rect.width as usize) * (pending.rect.height as usize)
        );

        app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
    }
}
