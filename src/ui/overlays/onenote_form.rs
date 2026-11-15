//! Microsoft OneNote modal forms (`src/ui/overlays/onenote_form.rs`):
//! - `B`: 새 필기장 만들기 (`CreateNotebook`)
//! - `S`: 새 섹션 만들기 (`CreateSection`)
//! - `n` / `N`: 새 페이지 / 하위 페이지 만들기 (`CreatePage`, supports Markdown + `Ctrl+V` clipboard image paste)
//! - `a`: 페이지 끝에 Markdown 블록 추가 (`AppendMarkdown`, supports `- [ ]` tasks + `Ctrl+V` clipboard image paste)
//! - `e`: 선택 블록 외과적 수정 (`EditBlock`, pre-filled with selected block's Markdown)

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{self, RenderedTerminalImage},
    models::InlineImageUpload,
    ui::{
        action::Action,
        app::UiState,
        composer,
        hitmap::{HitMap, HitTarget},
        nav_pane::ONENOTE_PURPLE,
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnenoteFormMode {
    CreateNotebook,
    CreateSection {
        notebook_key: String,
        notebook_name: String,
    },
    CreatePage {
        section_key: String,
        section_name: String,
        level: i32,
    },
    AppendMarkdown {
        page_key: String,
        page_title: String,
    },
    EditBlock {
        page_key: String,
        page_title: String,
        element_id: String,
        block_index: usize,
    },
}

#[derive(Debug, Clone)]
pub struct OnenoteFormOverlay {
    pub mode: OnenoteFormMode,
    pub title: String,
    pub body: String,
    pub pending_images: Vec<InlineImageUpload>,
    pub pending_preview: Option<RenderedTerminalImage>,
    pub field: usize,
    pub button_idx: usize,
    pub error: Option<String>,
}

impl OnenoteFormOverlay {
    pub fn create_notebook() -> Self {
        Self {
            mode: OnenoteFormMode::CreateNotebook,
            title: String::new(),
            body: String::new(),
            pending_images: Vec::new(),
            pending_preview: None,
            field: 0,
            button_idx: 0,
            error: None,
        }
    }

    pub fn create_section(notebook_key: String, notebook_name: String) -> Self {
        Self {
            mode: OnenoteFormMode::CreateSection {
                notebook_key,
                notebook_name,
            },
            title: String::new(),
            body: String::new(),
            pending_images: Vec::new(),
            pending_preview: None,
            field: 0,
            button_idx: 0,
            error: None,
        }
    }

    pub fn create_page(section_key: String, section_name: String, level: i32) -> Self {
        Self {
            mode: OnenoteFormMode::CreatePage {
                section_key,
                section_name,
                level,
            },
            title: String::new(),
            body: String::new(),
            pending_images: Vec::new(),
            pending_preview: None,
            field: 0,
            button_idx: 0,
            error: None,
        }
    }

    pub fn append_markdown(page_key: String, page_title: String) -> Self {
        Self {
            mode: OnenoteFormMode::AppendMarkdown {
                page_key,
                page_title,
            },
            title: String::new(),
            body: String::new(),
            pending_images: Vec::new(),
            pending_preview: None,
            field: 0,
            button_idx: 0,
            error: None,
        }
    }

    pub fn edit_block(
        page_key: String,
        page_title: String,
        element_id: String,
        block_index: usize,
        initial_markdown: String,
    ) -> Self {
        Self {
            mode: OnenoteFormMode::EditBlock {
                page_key,
                page_title,
                element_id,
                block_index,
            },
            title: String::new(),
            body: initial_markdown,
            pending_images: Vec::new(),
            pending_preview: None,
            field: 0,
            button_idx: 0,
            error: None,
        }
    }

    pub fn field_count(&self) -> usize {
        match self.mode {
            // 0 = name, 1 = buttons
            OnenoteFormMode::CreateNotebook | OnenoteFormMode::CreateSection { .. } => 2,
            // 0 = title, 1 = body, 2 = buttons
            OnenoteFormMode::CreatePage { .. } => 3,
            // 0 = body, 1 = buttons
            OnenoteFormMode::AppendMarkdown { .. } | OnenoteFormMode::EditBlock { .. } => 2,
        }
    }

    fn button_field_index(&self) -> usize {
        self.field_count() - 1
    }

    fn is_multiline_body_field(&self) -> bool {
        match self.mode {
            OnenoteFormMode::CreatePage { .. } => self.field == 1,
            OnenoteFormMode::AppendMarkdown { .. } | OnenoteFormMode::EditBlock { .. } => {
                self.field == 0
            }
            _ => false,
        }
    }

    pub fn supports_images(&self) -> bool {
        matches!(
            self.mode,
            OnenoteFormMode::CreatePage { .. } | OnenoteFormMode::AppendMarkdown { .. }
        )
    }

    pub fn select(&mut self, index: usize) {
        if index < self.field_count() {
            self.field = index;
        }
    }

    pub fn stage_inline_image(
        &mut self,
        bytes: Vec<u8>,
        mime_type: &str,
        file_name: Option<String>,
    ) {
        self.pending_preview =
            img_preview::decode_and_render_halfblocks(&bytes, 22, 4, (20, 22, 26)).ok();
        let temp_id = (self.pending_images.len() + 1).to_string();
        self.pending_images.push(InlineImageUpload {
            temp_id,
            mime_type: mime_type.to_string(),
            bytes,
            file_name,
        });
    }

    fn active_text_mut(&mut self) -> Option<&mut String> {
        match self.mode {
            OnenoteFormMode::CreateNotebook | OnenoteFormMode::CreateSection { .. } => {
                (self.field == 0).then_some(&mut self.title)
            }
            OnenoteFormMode::CreatePage { .. } => match self.field {
                0 => Some(&mut self.title),
                1 => Some(&mut self.body),
                _ => None,
            },
            OnenoteFormMode::AppendMarkdown { .. } | OnenoteFormMode::EditBlock { .. } => {
                (self.field == 0).then_some(&mut self.body)
            }
        }
    }

    fn paste_from_system_clipboard(&mut self) {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            self.error = Some("클립보드를 열 수 없습니다".to_string());
            return;
        };

        if let Ok(text) = cb.get_text() {
            if !text.is_empty() {
                let is_body = self.is_multiline_body_field();
                if let Some(buf) = self.active_text_mut() {
                    if is_body {
                        buf.push_str(&composer::normalize(&text));
                    } else {
                        buf.push_str(&text.replace(['\r', '\n'], " "));
                    }
                }
                return;
            }
        }

        if self.supports_images() {
            if let Ok(img) = cb.get_image() {
                let width = img.width as u32;
                let height = img.height as u32;
                match composer::encode_rgba_to_png(width, height, img.bytes.into_owned()) {
                    Ok(png_bytes) => {
                        let n = self.pending_images.len() + 1;
                        self.stage_inline_image(
                            png_bytes,
                            "image/png",
                            Some(format!("clipboard-{n}.png")),
                        );
                        self.error = None;
                    }
                    Err(err) => {
                        self.error = Some(format!("클립보드 이미지 인코딩 실패: {err}"));
                    }
                }
            }
        }
    }

    pub fn submit(&mut self) -> OverlayOutcome {
        match &self.mode {
            OnenoteFormMode::CreateNotebook => {
                let name = self.title.trim().to_string();
                if name.is_empty() {
                    self.field = 0;
                    self.error = Some("필기장 이름을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::OnenoteCreateNotebook {
                    display_name: name,
                })
            }
            OnenoteFormMode::CreateSection { notebook_key, .. } => {
                let name = self.title.trim().to_string();
                if name.is_empty() {
                    self.field = 0;
                    self.error = Some("섹션 이름을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::OnenoteCreateSection {
                    notebook_key: notebook_key.clone(),
                    display_name: name,
                })
            }
            OnenoteFormMode::CreatePage {
                section_key, level, ..
            } => {
                let title = self.title.trim().to_string();
                if title.is_empty() {
                    self.field = 0;
                    self.error = Some("페이지 제목을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::OnenoteCreatePage {
                    section_key: section_key.clone(),
                    title,
                    markdown_body: self.body.clone(),
                    inline_images: self.pending_images.clone(),
                    level: (*level > 0).then_some(*level),
                })
            }
            OnenoteFormMode::AppendMarkdown { page_key, .. } => {
                if self.body.trim().is_empty() && self.pending_images.is_empty() {
                    self.field = 0;
                    self.error =
                        Some("추가할 Markdown 내용 또는 클립보드 이미지를 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::OnenoteAppendMarkdown {
                    page_key: page_key.clone(),
                    markdown: self.body.clone(),
                    inline_images: self.pending_images.clone(),
                })
            }
            OnenoteFormMode::EditBlock {
                page_key,
                element_id,
                ..
            } => {
                if self.body.trim().is_empty() {
                    self.field = 0;
                    self.error = Some("수정할 블록 Markdown 내용을 입력하세요".to_string());
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::OnenoteEditBlockSubmit {
                    page_key: page_key.clone(),
                    element_id: element_id.clone(),
                    markdown: self.body.clone(),
                })
            }
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        let total_fields = self.field_count();
        let btn_field = self.button_field_index();

        match action {
            Action::Input(ch) => {
                self.error = None;
                if self.field == btn_field {
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
                let is_body = self.is_multiline_body_field();
                if let Some(buf) = self.active_text_mut() {
                    if is_body {
                        buf.push_str(&composer::normalize(text));
                    } else {
                        buf.push_str(&text.replace(['\r', '\n'], " "));
                    }
                }
                OverlayOutcome::Handled
            }
            Action::PasteClipboard => {
                self.error = None;
                self.paste_from_system_clipboard();
                OverlayOutcome::Handled
            }
            Action::ClearPendingImages => {
                self.pending_images.clear();
                self.pending_preview = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.error = None;
                if let Some(buf) = self.active_text_mut() {
                    buf.pop();
                }
                OverlayOutcome::Handled
            }
            Action::InsertNewline => {
                if self.is_multiline_body_field() {
                    if let Some(buf) = self.active_text_mut() {
                        buf.push('\n');
                    }
                    OverlayOutcome::Handled
                } else {
                    self.submit()
                }
            }
            Action::OverlaySubmit => self.submit(),
            Action::Tab => {
                self.field = (self.field + 1) % total_fields;
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.field = (self.field + total_fields - 1) % total_fields;
                OverlayOutcome::Handled
            }
            Action::MoveDown if !self.is_multiline_body_field() => {
                self.field = (self.field + 1) % total_fields;
                OverlayOutcome::Handled
            }
            Action::MoveUp if !self.is_multiline_body_field() => {
                self.field = (self.field + total_fields - 1) % total_fields;
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::CursorRight if self.field == btn_field => {
                self.button_idx = 1 - self.button_idx;
                OverlayOutcome::Handled
            }
            Action::Activate => {
                if self.field == btn_field {
                    return if self.button_idx == 0 {
                        self.submit()
                    } else {
                        OverlayOutcome::Close
                    };
                }
                if self.is_multiline_body_field() {
                    if let Some(buf) = self.active_text_mut() {
                        buf.push('\n');
                    }
                    return OverlayOutcome::Handled;
                }
                if matches!(
                    self.mode,
                    OnenoteFormMode::CreateNotebook | OnenoteFormMode::CreateSection { .. }
                ) {
                    return self.submit();
                }
                self.field = (self.field + 1) % total_fields;
                OverlayOutcome::Handled
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    fn header_title(&self) -> String {
        match &self.mode {
            OnenoteFormMode::CreateNotebook => {
                " 📓 새 필기장 만들기 · Enter 생성 · Esc 닫기 ".to_string()
            }
            OnenoteFormMode::CreateSection { notebook_name, .. } => {
                format!(" 📓 새 섹션 추가 ({notebook_name}) · Enter 생성 · Esc 닫기 ")
            }
            OnenoteFormMode::CreatePage {
                section_name,
                level,
                ..
            } => {
                let kind = if *level > 0 { "새 하위 페이지" } else { "새 페이지" };
                format!(
                    " 📓 {kind} ({section_name}) · Tab 이동 · Ctrl+S 저장 · Ctrl+V 이미지 · Esc 닫기 "
                )
            }
            OnenoteFormMode::AppendMarkdown { page_title, .. } => {
                format!(
                    " 📓 블록 추가 ({page_title}) · Ctrl+S/Ctrl+Enter 추가 · Ctrl+V 이미지 · Esc 닫기 "
                )
            }
            OnenoteFormMode::EditBlock {
                page_title,
                block_index,
                ..
            } => {
                format!(
                    " 📓 블록 #{} 수정 ({page_title}) · Ctrl+S/Ctrl+Enter 저장 · Esc 닫기 ",
                    block_index + 1
                )
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let is_compact = matches!(
            self.mode,
            OnenoteFormMode::CreateNotebook | OnenoteFormMode::CreateSection { .. }
        );
        let modal_w = if is_compact {
            area.width.saturating_sub(8).min(64)
        } else {
            area.width.saturating_sub(6).min(86)
        };
        let modal_h = if is_compact {
            8
        } else {
            area.height.saturating_sub(4).min(24)
        };

        let rect = centered_rect(area, modal_w, modal_h);
        frame.render_widget(Clear, rect);

        let title = self.header_title();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                Style::default()
                    .fg(ONENOTE_PURPLE)
                    .add_modifier(Modifier::BOLD),
            )))
            .border_style(Style::default().fg(ONENOTE_PURPLE));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 4 {
            return;
        }

        let active_label_style = Style::default()
            .fg(ONENOTE_PURPLE)
            .add_modifier(Modifier::BOLD);
        let btn_field = self.button_field_index();
        let mut y = inner.y;

        match &self.mode {
            OnenoteFormMode::CreateNotebook | OnenoteFormMode::CreateSection { .. } => {
                let prompt = if matches!(self.mode, OnenoteFormMode::CreateNotebook) {
                    "필기장 이름: "
                } else {
                    "섹션 이름:   "
                };
                let cursor = if self.field == 0 { "▏" } else { "" };
                let line = Line::from(vec![
                    Span::styled(
                        prompt,
                        if self.field == 0 {
                            active_label_style
                        } else {
                            theme.muted_style()
                        },
                    ),
                    Span::raw(self.title.clone()),
                    Span::styled(cursor, active_label_style),
                ]);
                frame.render_widget(
                    Paragraph::new(line),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: 0 });
                y = y.saturating_add(2);
            }
            OnenoteFormMode::CreatePage { level, .. } => {
                let badge = if *level > 0 {
                    "하위 페이지 제목: "
                } else {
                    "페이지 제목: "
                };
                let cursor = if self.field == 0 { "▏" } else { "" };
                let line = Line::from(vec![
                    Span::styled(
                        badge,
                        if self.field == 0 {
                            active_label_style
                        } else {
                            theme.muted_style()
                        },
                    ),
                    Span::raw(self.title.clone()),
                    Span::styled(cursor, active_label_style),
                ]);
                frame.render_widget(
                    Paragraph::new(line),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: 0 });
                y = y.saturating_add(1);

                self.draw_markdown_editor_box(frame, inner, &mut y, 1, ui, hitmap);
            }
            OnenoteFormMode::AppendMarkdown { .. } | OnenoteFormMode::EditBlock { .. } => {
                self.draw_markdown_editor_box(frame, inner, &mut y, 0, ui, hitmap);
            }
        }

        // Error or helper hint line above buttons
        let footer_y = inner.y + inner.height.saturating_sub(1);
        if footer_y > y {
            let status_y = footer_y.saturating_sub(1);
            if let Some(err) = &self.error {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(err, inner.width as usize),
                        Style::default().fg(theme.error),
                    )),
                    Rect::new(inner.x, status_y, inner.width, 1),
                );
            } else if !self.pending_images.is_empty() {
                let img_note = format!(
                    "🖼 클립보드 이미지 {}개 첨부됨 (전송 시 페이지에 인라인 업로드)",
                    self.pending_images.len()
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&img_note, inner.width as usize),
                        Style::default().fg(ONENOTE_PURPLE),
                    )),
                    Rect::new(inner.x, status_y, inner.width, 1),
                );
            }
        }

        // Submit / Cancel buttons
        let submit_label = match self.mode {
            OnenoteFormMode::CreateNotebook | OnenoteFormMode::CreateSection { .. } => " [ 생성 ] ",
            OnenoteFormMode::CreatePage { .. } => " [ 페이지 생성 (Ctrl+S) ] ",
            OnenoteFormMode::AppendMarkdown { .. } => " [ 블록 추가 (Ctrl+S) ] ",
            OnenoteFormMode::EditBlock { .. } => " [ 수정 저장 (Ctrl+S) ] ",
        };
        let cancel_label = " [ 취소 (Esc) ] ";
        let submit_selected = self.field == btn_field && self.button_idx == 0;
        let cancel_selected = self.field == btn_field && self.button_idx == 1;

        let submit_style = if submit_selected {
            Style::default()
                .fg(Color::White)
                .bg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD)
        };
        let cancel_style = if cancel_selected {
            theme.selection()
        } else {
            theme.muted_style()
        };

        let btn_line = Line::from(vec![
            Span::styled(submit_label, submit_style),
            Span::raw("  "),
            Span::styled(cancel_label, cancel_style),
        ]);
        frame.render_widget(
            Paragraph::new(btn_line),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
        hitmap.add_line(
            inner.x,
            footer_y,
            UnicodeWidthStr::width(submit_label) as u16,
            HitTarget::OverlayItem { index: btn_field },
        );
    }

    fn draw_markdown_editor_box(
        &self,
        frame: &mut Frame,
        inner: Rect,
        y: &mut u16,
        body_field_idx: usize,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let focused = self.field == body_field_idx;
        let hint = if self.supports_images() {
            "Markdown 본문 (- [ ] 할일, ## 제목, | 표 |, Ctrl+V 클립보드 이미지):"
        } else {
            "블록 Markdown 편집 (- [ ] 할일, ## 제목, 굵게/코드 등):"
        };
        let label_style = if focused {
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style()
        };
        if *y < inner.y + inner.height.saturating_sub(2) {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(hint, inner.width as usize),
                    label_style,
                )),
                Rect::new(inner.x, *y, inner.width, 1),
            );
            *y = y.saturating_add(1);
        }

        let preview_h = self
            .pending_preview
            .as_ref()
            .map(|p| p.rows.min(4))
            .unwrap_or(0);
        let avail_h = (inner.y + inner.height)
            .saturating_sub(*y)
            .saturating_sub(2 + preview_h);
        if avail_h == 0 {
            return;
        }

        let box_rect = Rect::new(inner.x, *y, inner.width, avail_h);
        let raw_lines: Vec<&str> = if self.body.is_empty() {
            vec![""]
        } else {
            self.body.split('\n').collect()
        };
        let visible = avail_h as usize;
        let start = raw_lines.len().saturating_sub(visible);
        let mut rendered_lines = Vec::with_capacity(visible);
        for (i, line_str) in raw_lines.iter().enumerate().skip(start) {
            let is_last = i + 1 == raw_lines.len();
            if focused && is_last {
                rendered_lines.push(Line::from(vec![
                    Span::raw((*line_str).to_string()),
                    Span::styled("▏", Style::default().fg(ONENOTE_PURPLE)),
                ]));
            } else {
                rendered_lines.push(Line::from(Span::raw((*line_str).to_string())));
            }
        }
        frame.render_widget(Paragraph::new(rendered_lines), box_rect);
        for row_off in 0..avail_h {
            hitmap.add_line(
                box_rect.x,
                box_rect.y + row_off,
                box_rect.width,
                HitTarget::OverlayItem {
                    index: body_field_idx,
                },
            );
        }
        *y = y.saturating_add(avail_h);

        if let Some(preview) = &self.pending_preview {
            let ph = preview.rows.min(preview_h);
            if ph > 0 {
                let img_rect = Rect::new(inner.x, *y, preview.cols.min(inner.width), ph);
                frame.render_widget(Paragraph::new(preview.lines.clone()), img_rect);
                *y = y.saturating_add(ph);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_notebook_and_section_validate_and_dispatch() {
        let mut nb_form = OnenoteFormOverlay::create_notebook();
        assert_eq!(nb_form.handle(&Action::OverlaySubmit), OverlayOutcome::Handled);
        assert!(nb_form.error.is_some());

        nb_form.handle(&Action::Paste("아키텍처 노트".to_string()));
        assert_eq!(
            nb_form.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OnenoteCreateNotebook {
                display_name: "아키텍처 노트".to_string(),
            })
        );

        let mut sec_form =
            OnenoteFormOverlay::create_section("me|nb-1".to_string(), "내 필기장".to_string());
        sec_form.handle(&Action::Paste("회의록".to_string()));
        assert_eq!(
            sec_form.handle(&Action::OverlaySubmit),
            OverlayOutcome::CloseAndDispatch(Action::OnenoteCreateSection {
                notebook_key: "me|nb-1".to_string(),
                display_name: "회의록".to_string(),
            })
        );
    }

    #[test]
    fn create_subpage_and_append_markdown_with_inline_image() {
        let mut page_form =
            OnenoteFormOverlay::create_page("me|sec-1".to_string(), "회의록".to_string(), 1);
        page_form.handle(&Action::Paste("스프린트 회고".to_string()));
        page_form.handle(&Action::Tab);
        page_form.handle(&Action::Paste("- [ ] 액션 아이템 점검".to_string()));
        page_form.handle(&Action::Activate); // inserts newline in multiline body
        page_form.handle(&Action::Paste("- [x] 릴리스 완료".to_string()));

        let png = composer::encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        page_form.stage_inline_image(png.clone(), "image/png", Some("clip.png".to_string()));

        assert_eq!(
            page_form.handle(&Action::OverlaySubmit),
            OverlayOutcome::CloseAndDispatch(Action::OnenoteCreatePage {
                section_key: "me|sec-1".to_string(),
                title: "스프린트 회고".to_string(),
                markdown_body: "- [ ] 액션 아이템 점검\n- [x] 릴리스 완료".to_string(),
                inline_images: vec![InlineImageUpload {
                    temp_id: "1".to_string(),
                    mime_type: "image/png".to_string(),
                    bytes: png,
                    file_name: Some("clip.png".to_string()),
                }],
                level: Some(1),
            })
        );
    }

    #[test]
    fn edit_block_prefills_and_submits_surgical_patch() {
        let mut edit = OnenoteFormOverlay::edit_block(
            "me|page-1".to_string(),
            "주간 업무".to_string(),
            "p:{123}{40}".to_string(),
            2,
            "- [ ] 기존 할 일".to_string(),
        );
        edit.body = "- [ ] 업데이트된 할 일".to_string();
        assert_eq!(
            edit.handle(&Action::OverlaySubmit),
            OverlayOutcome::CloseAndDispatch(Action::OnenoteEditBlockSubmit {
                page_key: "me|page-1".to_string(),
                element_id: "p:{123}{40}".to_string(),
                markdown: "- [ ] 업데이트된 할 일".to_string(),
            })
        );
    }
}
