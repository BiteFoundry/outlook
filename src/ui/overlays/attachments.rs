//! Attachments picker overlay (`A` in Mail view) for downloading or previewing attachments.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview,
    models::MailMessage,
    parser,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailAttachmentRow {
    pub msg_id: String,
    pub attach_index: usize,
    pub attachment_id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub is_image: bool,
}

impl MailAttachmentRow {
    pub fn icon(&self) -> &'static str {
        if self.is_image {
            "[IMG]"
        } else {
            "[FILE]"
        }
    }

    pub fn label(&self) -> String {
        let size_str = if self.size == 0 {
            String::new()
        } else if self.size < 1024 {
            format!(" ({} B)", self.size)
        } else if self.size < 1024 * 1024 {
            format!(" ({:.1} KB)", self.size as f64 / 1024.0)
        } else {
            format!(" ({:.1} MB)", self.size as f64 / (1024.0 * 1024.0))
        };
        format!("{} {}{}", self.icon(), self.name, size_str)
    }
}

pub fn rows_from_mail(msg: &MailMessage) -> Vec<MailAttachmentRow> {
    parser::effective_mail_attachments(msg)
        .iter()
        .enumerate()
        .map(|(attach_index, att)| {
            let name = if att.name.trim().is_empty() {
                format!("attachment-{}", attach_index + 1)
            } else {
                att.name.clone()
            };
            let is_image =
                img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name));
            MailAttachmentRow {
                msg_id: msg.id.clone(),
                attach_index,
                attachment_id: att.id.clone(),
                name,
                content_type: att.content_type.clone(),
                size: att.size,
                is_image,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentsState {
    pub selected: usize,
}

impl AttachmentsState {
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let items = app
            .active_mail()
            .map(rows_from_mail)
            .unwrap_or_default();
        match action {
            Action::MoveUp => {
                self.move_up();
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.move_down(items.len());
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Input('d') => {
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(row) => OverlayOutcome::Dispatch(Action::DownloadMailAttachment {
                        msg_id: row.msg_id.clone(),
                        attach_index: row.attach_index,
                    }),
                    None => OverlayOutcome::Handled,
                }
            }
            Action::Input('v') | Action::OpenImagePreview => {
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(row) if row.is_image => {
                        OverlayOutcome::CloseAndDispatch(Action::OpenMailImagePreview {
                            msg_id: row.msg_id.clone(),
                            attach_index: Some(row.attach_index),
                        })
                    }
                    _ => OverlayOutcome::Handled,
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
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
            area.width.saturating_sub(8).min(78),
            area.height.saturating_sub(4).min(20),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Attachments · Enter/d Download · v Preview Image · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let items = app
            .active_mail()
            .map(rows_from_mail)
            .unwrap_or_default();
        let total = items.len();
        let header = if total == 0 {
            "No attachments in selected email".to_string()
        } else {
            format!(
                "{total} attachment{} · {}/{total}",
                if total == 1 { "" } else { "s" },
                (self.selected + 1).min(total),
            )
        };
        frame.render_widget(
            Paragraph::new(Span::styled(header, theme.muted_style())),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let hint_y = inner.y + inner.height.saturating_sub(1);
        let list_y = inner.y.saturating_add(2);
        let visible = hint_y.saturating_sub(list_y) as usize;
        if visible > 0 && total > 0 {
            let active_sel = self.selected.min(total - 1);
            let skip = active_sel
                .saturating_sub(visible.saturating_sub(1))
                .min(total.saturating_sub(visible));
            for slot in 0..visible {
                let index = skip + slot;
                if index >= total {
                    break;
                }
                let y = list_y + slot as u16;
                let style = if index == active_sel {
                    theme.selection()
                } else {
                    Style::default()
                };
                let label = format!(" {}", items[index].label());
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&label, inner.width as usize),
                        style,
                    )),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
            }
        }

        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(
                    &format!(" Downloads: {}", app.download_dir.display()),
                    inner.width as usize,
                ),
                theme.muted_style(),
            )),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
    }
}
