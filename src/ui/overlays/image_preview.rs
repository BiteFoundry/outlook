//! Full-screen inline terminal image preview modal (`v` / `Alt+v` or clicking `[IMG ...]`).
//! Supports Outlook email attachments and inline `cid:` / `https:` images:
//! tall-image viewport scrolling (`j`/`k`/`↑`/`↓`/`PgUp`/`PgDn`/`g`/`G`/wheel),
//! prev/next cycling (`[`/`]`, `←`/`→`), zoom (`+`/`-`/`0`), protocol cycle (`p`),
//! download (`d`), and close (`Esc`/`q`/`v` or `[✕ Esc]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{self, FitBox, TerminalImageProtocol},
    models::{MailAttachment, MailMessage},
    parser,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
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
    /// Zoom percentage (`25..=300`, default `100`).
    pub zoom_pct: u16,
    pub scroll_y: u16,
    pub scroll_x: u16,
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
            scroll_y: 0,
            scroll_x: 0,
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

    pub fn viewport_dimensions(term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let img_w = rect.width.saturating_sub(2).max(1);
        let img_h = rect.height.saturating_sub(3).max(1);
        (img_w, img_h)
    }

    pub fn fit_box(&self, term_width: u16, term_height: u16) -> FitBox {
        let (view_cols, view_rows) = Self::viewport_dimensions(term_width, term_height);
        FitBox::scrollable(view_cols, view_rows, self.zoom_pct)
    }

    pub fn next_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + 1) % self.items.len();
            self.scroll_y = 0;
            self.scroll_x = 0;
            self.error = None;
        }
    }

    pub fn prev_image(&mut self) {
        if !self.items.is_empty() {
            self.index = (self.index + self.items.len() - 1) % self.items.len();
            self.scroll_y = 0;
            self.scroll_x = 0;
            self.error = None;
        }
    }

    pub fn zoom_in(&mut self) {
        self.zoom_pct = (self.zoom_pct + 25).min(300);
    }

    pub fn zoom_out(&mut self) {
        self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
        self.scroll_y = 0;
        self.scroll_x = 0;
    }

    pub fn reset_zoom(&mut self) {
        self.zoom_pct = 100;
        self.scroll_y = 0;
        self.scroll_x = 0;
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
            Action::MoveUp | Action::Input('k') => {
                self.scroll_y = self.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.scroll_y = self.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::ScrollPageUp => {
                self.scroll_y = self.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::ScrollPageDown => {
                self.scroll_y = self.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::Input('h') => {
                self.scroll_x = self.scroll_x.saturating_sub(4);
                OverlayOutcome::Handled
            }
            Action::Input('l') => {
                self.scroll_x = self.scroll_x.saturating_add(4);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.scroll_y = self.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    self.scroll_y = self.scroll_y.saturating_add(*delta as u16);
                }
                OverlayOutcome::Handled
            }
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
            Action::Input('d') | Action::DownloadLatestAttachment => {
                OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        self.draw_inner(frame, area, app, ui, None, hitmap);
    }

    pub fn draw_sync(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let mut pending = Vec::new();
        self.draw_inner(frame, area, app, ui, Some(&mut pending), hitmap);
        ui.pending_pixel_graphics = pending;
    }

    fn draw_inner(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        pending_out: Option<&mut Vec<PendingPixelGraphic>>,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let close_label = "[✕ Esc]";
        let close_w = close_label.width() as u16;
        let show_close_pill = rect.width > close_w + 4;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let (view_cols, view_rows) = Self::viewport_dimensions(area.width, area.height);
        let Some(cur) = self.current().cloned() else {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
                .title(Line::from(Span::styled(" Image Preview ", theme.title(true))))
                .border_style(theme.border_style(true));
            frame.render_widget(Paragraph::new("No image to display").block(block), rect);
            return;
        };

        let proto = ui.graphics_caps.protocol;
        let cache_key = AppState::preview_image_cache_key(
            cur.owner_id(),
            cur.att_id(),
            view_cols,
            view_rows,
            self.zoom_pct,
            ui.graphics_caps.cell_px,
            proto,
        );
        let cached = app.image_cache.get(&cache_key);

        let dim_info = match cached {
            Some(img) => format!(" · {}x{} {}", img.width_px, img.height_px, img.format_label),
            None => String::new(),
        };
        let proto_label = proto.label();
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
                crate::ui::truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_close_pill {
            let close_x = rect.right().saturating_sub(close_w + 1);
            let close_rect = Rect::new(close_x, rect.y, close_w, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    close_label,
                    Style::default().fg(theme.fg).bg(theme.bg_elevated),
                )),
                close_rect,
            );
            hitmap.add_line(
                close_x,
                rect.y,
                close_w,
                HitTarget::Button(ButtonId::ConfirmCancel),
            );
        }

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
            let is_pixel = rendered.protocol.is_pixel() && rendered.pixel_payload.is_some();
            let total_rows = if is_pixel {
                rendered.rows
            } else {
                rendered.lines.len() as u16
            };
            let total_cols = rendered.cols;

            let avail_h = if is_pixel
                && !ui.graphics_caps.cell_px_exact
                && rendered.protocol == TerminalImageProtocol::Sixel
            {
                img_area.height.saturating_sub(1).max(1)
            } else {
                img_area.height
            };
            let show_scrollbar = total_rows > avail_h && img_area.width > 2;
            let avail_w = if show_scrollbar {
                img_area.width.saturating_sub(1)
            } else {
                img_area.width
            };

            let max_scroll_y = total_rows.saturating_sub(avail_h);
            let max_scroll_x = total_cols.saturating_sub(avail_w);
            self.scroll_y = self.scroll_y.min(max_scroll_y);
            self.scroll_x = self.scroll_x.min(max_scroll_x);
            let sy = self.scroll_y;
            let sx = self.scroll_x;

            let draw_w = total_cols.saturating_sub(sx).min(avail_w);
            let draw_h = total_rows.saturating_sub(sy).min(avail_h);
            let x_pad = avail_w.saturating_sub(draw_w) / 2;
            let y_pad = avail_h.saturating_sub(draw_h) / 2;
            let target_rect = Rect::new(
                img_area.x.saturating_add(x_pad),
                img_area.y.saturating_add(y_pad),
                draw_w,
                draw_h,
            );

            if is_pixel {
                let cropped_key = format!("{cache_key}#crop:{sx},{sy},{avail_w},{avail_h}");
                if let Some(out) = pending_out {
                    out.push(PendingPixelGraphic {
                        cache_key: cropped_key.clone(),
                        rect: target_rect,
                        protocol: rendered.protocol,
                        payload: None,
                    });
                }
                if ui
                    .active_pixel_graphics
                    .iter()
                    .any(|entry| entry == &(cropped_key.clone(), target_rect, rendered.protocol))
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
                let start = (sy as usize).min(rendered.lines.len());
                let end = start
                    .saturating_add(target_rect.height as usize)
                    .min(rendered.lines.len());
                let visible_lines: Vec<Line<'static>> = rendered.lines[start..end].to_vec();
                frame.render_widget(
                    Paragraph::new(visible_lines).scroll((0, sx)),
                    target_rect,
                );
            }

            if show_scrollbar {
                let sb_area =
                    Rect::new(img_area.right().saturating_sub(1), img_area.y, 1, img_area.height);
                let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                    .position(sy as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
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

        let footer =
            " ↑↓/jk/Wheel: Scroll   [/]: Prev/Next   +/- (0): Zoom   p: Protocol   d: Save   Esc/q/v: Close ";
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
            last_modified_at: None,
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
            ..Default::default()
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
        let fit_125 = overlay.fit_box(120, 40);
        overlay.handle(&Action::Input('-'));
        let fit_100 = overlay.fit_box(120, 40);
        assert_eq!(fit_125.view_cols, fit_100.view_cols);
        assert_eq!(fit_125.view_rows, fit_100.view_rows);
        assert_eq!(fit_125.zoom_pct, 125);
        assert_eq!(fit_100.zoom_pct, 100);
        assert!(fit_125.scrollable);
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
        assert_eq!(overlay.scroll_y, 0);

        // Mouse wheel scrolls vertically
        assert_eq!(
            overlay.handle(&Action::MouseScroll {
                delta: 3,
                column: 10,
                row: 10,
            }),
            OverlayOutcome::Handled
        );
        assert_eq!(overlay.scroll_y, 3);

        assert_eq!(
            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 10,
                row: 10,
            }),
            OverlayOutcome::Handled
        );
        assert_eq!(overlay.scroll_y, 0);

        // j/k and PageDown/PageUp scroll vertically
        assert_eq!(overlay.handle(&Action::Input('j')), OverlayOutcome::Handled);
        assert_eq!(overlay.scroll_y, 2);
        assert_eq!(overlay.handle(&Action::ScrollPageDown), OverlayOutcome::Handled);
        assert_eq!(overlay.scroll_y, 12);
        assert_eq!(overlay.handle(&Action::Input('k')), OverlayOutcome::Handled);
        assert_eq!(overlay.scroll_y, 10);
        assert_eq!(overlay.handle(&Action::ScrollPageUp), OverlayOutcome::Handled);
        assert_eq!(overlay.scroll_y, 0);

        // Zoom + / 0 / - still dispatch LoadPreviewImage
        assert_eq!(
            overlay.handle(&Action::Input('+')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.zoom_pct, 125);

        assert_eq!(
            overlay.handle(&Action::Input('0')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.zoom_pct, 100);

        assert_eq!(
            overlay.handle(&Action::Input('-')),
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

    fn make_tall_png(w: u32, h: u32) -> Vec<u8> {
        use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
        use std::io::Cursor;

        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([
                ((x * 255) / w.max(1)) as u8,
                ((y * 255) / h.max(1)) as u8,
                160,
                255,
            ])
        });
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn draw_sync_renders_sixel_badge_for_cid_inline_image_and_skips_active_cells() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = make_test_app_and_ui();
        let mut msg = mail_with_attachments();
        msg.attachments.clear();
        msg.body_html =
            "<p>Screenshot:</p><img src=\"cid:screenshot.png@outlook\">".to_string();

        let mut overlay = ImagePreviewOverlay::from_mail_message(&msg, None)
            .expect("cid inline img must produce preview overlay");
        assert_eq!(overlay.items.len(), 1);
        assert_eq!(overlay.current().unwrap().att_id(), "cid:screenshot.png@outlook");

        let area = Rect::new(0, 0, 100, 30);
        let fit = overlay.fit_box(area.width, area.height);

        for (proto, badge) in [
            (img_preview::TerminalImageProtocol::Sixel, "[Sixel]"),
            (img_preview::TerminalImageProtocol::Kitty, "[Kitty]"),
            (img_preview::TerminalImageProtocol::ITerm2, "[iTerm2]"),
            (img_preview::TerminalImageProtocol::Halfblock, "[Halfblock]"),
        ] {
            ui.graphics_caps.protocol = proto;
            ui.active_pixel_graphics.clear();
            ui.pending_pixel_graphics.clear();
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
        ui.graphics_caps = img_preview::TerminalGraphicsCaps {
            protocol: proto,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let cur = overlay.current().unwrap();
        let cache_key = AppState::preview_image_cache_key(
            cur.owner_id(),
            cur.att_id(),
            fit.view_cols,
            fit.view_rows,
            fit.zoom_pct,
            ui.graphics_caps.cell_px,
            proto,
        );
        let sample_png = make_tall_png(320, 200);
        let rendered = img_preview::decode_and_render_for_protocol(
            &sample_png,
            fit,
            (28, 32, 46),
            ui.graphics_caps,
        )
        .unwrap();
        app.insert_image_cache(cache_key.clone(), rendered);

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw_sync(frame, area, &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let pending = ui
            .pending_pixel_graphics
            .first()
            .cloned()
            .expect("pending_pixel_graphics must be populated for Sixel");
        assert!(pending.cache_key.starts_with(&format!("{cache_key}#crop:")));
        assert_eq!(pending.protocol, proto);

        ui.active_pixel_graphics = vec![(pending.cache_key.clone(), pending.rect, pending.protocol)];

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

    #[tokio::test]
    async fn tall_image_scroll_clamps_and_renders_scrollbar_for_halfblock_and_sixel() {
        use ratatui::{backend::TestBackend, Terminal};

        let png = make_tall_png(400, 2000);
        let area = Rect::new(0, 0, 100, 30);

        for proto in [
            TerminalImageProtocol::Halfblock,
            TerminalImageProtocol::Sixel,
        ] {
            let (mut app, mut ui) = make_test_app_and_ui();
            ui.term_width = area.width;
            ui.term_height = area.height;
            ui.graphics_caps = img_preview::TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };

            let msg = mail_with_attachments();
            let mut overlay = ImagePreviewOverlay::from_mail_message(&msg, Some(1)).unwrap();
            let fit = overlay.fit_box(area.width, area.height);
            let rendered = img_preview::decode_and_render_for_protocol(
                &png,
                fit,
                (25, 25, 25),
                ui.graphics_caps,
            )
            .unwrap();
            let total_rows = if proto == TerminalImageProtocol::Halfblock {
                rendered.lines.len() as u16
            } else {
                rendered.rows
            };
            let rect = ImagePreviewOverlay::modal_rect(area);
            let img_h = rect.height.saturating_sub(3);
            let expected_max_scroll = total_rows.saturating_sub(img_h);
            assert!(
                expected_max_scroll > 0,
                "expected scrollable tall image for {proto:?}, total_rows={total_rows}, img_h={img_h}"
            );

            let cur = overlay.current().unwrap();
            let base_key = AppState::preview_image_cache_key(
                cur.owner_id(),
                cur.att_id(),
                fit.view_cols,
                fit.view_rows,
                fit.zoom_pct,
                ui.graphics_caps.cell_px,
                proto,
            );
            app.insert_image_cache(base_key.clone(), rendered);

            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hitmap = HitMap::new();

            // Initial draw at scroll_y = 0
            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphics.clear();
                    hitmap.clear();
                    overlay.draw_sync(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            // Clickable [✕ Esc] close pill registered in top-right border
            let close_region = hitmap
                .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
                .expect("expected [✕ Esc] close pill hit target");
            assert_eq!(close_region.y, rect.y);
            assert!(close_region.right() <= rect.right());

            // Scrollbar rendered on right-most inner column of img_area
            let sb_x = rect.right() - 2;
            let buf = terminal.backend().buffer();
            let has_sb_cell = (rect.y + 1..rect.bottom() - 2).any(|y| {
                let sym = buf[(sb_x, y)].symbol();
                !sym.trim().is_empty()
            });
            assert!(
                has_sb_cell,
                "expected scrollbar glyph in right inner column for {proto:?}"
            );

            if proto == TerminalImageProtocol::Sixel {
                let pg0 = ui
                    .pending_pixel_graphics
                    .first()
                    .cloned()
                    .expect("expected pending pixel graphic for Sixel");
                assert!(
                    pg0.cache_key.starts_with(&format!("{base_key}#crop:0,0,")),
                    "unexpected initial crop key: {}",
                    pg0.cache_key
                );
            }

            // ScrollBottom clamps in-place on draw
            assert_eq!(overlay.handle(&Action::ScrollBottom), OverlayOutcome::Handled);
            assert_eq!(overlay.scroll_y, u16::MAX);
            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphics.clear();
                    hitmap.clear();
                    overlay.draw_sync(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();
            assert_eq!(overlay.scroll_y, expected_max_scroll);

            if proto == TerminalImageProtocol::Sixel {
                let pg_bottom = ui
                    .pending_pixel_graphics
                    .first()
                    .cloned()
                    .expect("expected pending pixel graphic at bottom for Sixel");
                assert!(
                    pg_bottom
                        .cache_key
                        .starts_with(&format!("{base_key}#crop:0,{expected_max_scroll},")),
                    "unexpected bottom crop key: {}",
                    pg_bottom.cache_key
                );
            }

            // Wheel up responds immediately without runaway delay
            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 50,
                row: 15,
            });
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(3));

            app.sync_ctrl.request(crate::sync::SyncCommand::Shutdown);
        }
    }

    #[test]
    fn test_preview_cache_key_includes_cell_px() {
        let k1 = AppState::preview_image_cache_key(
            "msg-1",
            "att-1",
            100,
            30,
            100,
            (10, 20),
            TerminalImageProtocol::Sixel,
        );
        let k2 = AppState::preview_image_cache_key(
            "msg-1",
            "att-1",
            100,
            30,
            100,
            (16, 32),
            TerminalImageProtocol::Sixel,
        );
        let k3 = AppState::preview_image_cache_key(
            "msg-1",
            "att-1",
            100,
            30,
            150,
            (10, 20),
            TerminalImageProtocol::Sixel,
        );
        assert_ne!(k1, k2, "different cell_px must produce different cache keys");
        assert_ne!(k1, k3, "different zoom_pct must produce different cache keys");
        assert!(k1.contains("10x20"));
        assert!(k2.contains("16x32"));
        assert!(k3.contains("150%"));
    }
}
