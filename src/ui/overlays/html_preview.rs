//! Interactive native HTML viewer modal (`H` on emails, `.html`/`.htm` attachments,
//! or auto-routed when an image preview fetch returns HTML bytes).
//!
//! Backed by `bitefoundry_html::HtmlViewSession` (0 MB idle on static HTML, on-demand
//! sandboxed 8 MiB QuickJS-NG when `<script>` / `on*` handlers exist, dropped on close).

use std::fmt;

use bitefoundry_html::{HtmlEffect, HtmlTheme, HtmlViewSession};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{ButtonId, HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
    theme::Theme,
};

/// Returns `true` when MIME type or filename identifies HTML content.
pub fn is_html_mime_or_name(content_type: &str, file_name: Option<&str>) -> bool {
    let mime = (!content_type.trim().is_empty()).then_some(content_type);
    bitefoundry_html::sniff_content_kind(mime, file_name, None)
        == bitefoundry_html::ContentKind::Html
        || bitefoundry_html::sniff_content_kind(None, file_name, None)
            == bitefoundry_html::ContentKind::Html
}

/// Maps Outlook's active `Theme` palette onto `bitefoundry_html::HtmlTheme`.
pub fn html_theme_from_ui(theme: &Theme) -> HtmlTheme {
    if theme.no_color {
        return HtmlTheme::no_color();
    }
    let focus_fg = if theme.bg_elevated == Color::Reset {
        Color::Black
    } else {
        theme.bg_elevated
    };
    HtmlTheme {
        text: theme.fg_primary,
        muted: theme.fg_muted,
        heading: theme.accent_soft,
        accent: theme.accent,
        link: theme.accent,
        code_fg: theme.code_fg,
        code_bg: theme.bg_code,
        quote_border: theme.fg_subtle,
        border: theme.border,
        control_fg: theme.fg_primary,
        control_bg: theme.bg_selected,
        focus_fg,
        focus_bg: theme.accent,
        raw_tag: theme.accent,
        raw_attr: theme.code_fg,
    }
}

/// Interactive HTML Preview overlay backed by `bitefoundry_html::HtmlViewSession`.
pub struct HtmlPreviewOverlay {
    pub session: HtmlViewSession,
    pub content_area: Rect,
    pub last_viewport_height: u16,
    pub show_link_urls: bool,
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("title", &self.session.title())
            .field("raw_mode", &self.session.raw_mode)
            .field("scroll_y", &self.session.scroll_y)
            .field("focused_control", &self.session.focused_control)
            .field("js_active", &self.session.js_active)
            .finish()
    }
}

impl HtmlPreviewOverlay {
    pub fn new(title: Option<String>, raw_html: String) -> Self {
        let mut session = HtmlViewSession::new(title, raw_html);
        // Drain initial script side-effects; startup alerts are already reflected in `last_status`.
        let _ = session.take_pending_effects();
        Self {
            session,
            content_area: Rect::default(),
            last_viewport_height: 20,
            show_link_urls: false,
        }
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn is_editing_input(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn title_or_default(&self) -> String {
        self.session
            .title()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("HTML Preview")
            .to_string()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        if matches!(
            action,
            Action::Quit
                | Action::QuitConfirm
                | Action::ServeHtmlPreview
                | Action::OpenUrl(_)
                | Action::ShowToast(_)
        ) {
            return OverlayOutcome::NotHandled;
        }

        if self.session.editing_input.is_some() {
            return match action {
                Action::Input(ch) => {
                    self.session.input_char(*ch);
                    OverlayOutcome::Handled
                }
                Action::Paste(text) => {
                    for ch in text.chars() {
                        if !matches!(ch, '\r' | '\n') {
                            self.session.input_char(ch);
                        }
                    }
                    OverlayOutcome::Handled
                }
                Action::Backspace => {
                    self.session.backspace();
                    OverlayOutcome::Handled
                }
                Action::Activate | Action::OverlaySubmit | Action::CloseOverlay => {
                    let effects = self.session.finish_editing();
                    self.apply_effects(effects)
                }
                Action::Tab => {
                    let effects = self.session.finish_editing();
                    self.session.focus_next_control();
                    self.ensure_focused_visible();
                    self.apply_effects(effects)
                }
                Action::PrevFocus => {
                    let effects = self.session.finish_editing();
                    self.session.focus_prev_control();
                    self.ensure_focused_visible();
                    self.apply_effects(effects)
                }
                Action::MouseScroll { delta, .. } => {
                    if *delta < 0 {
                        self.session.scroll_y =
                            self.session.scroll_y.saturating_sub((-*delta) as u16);
                    } else {
                        self.session.scroll_y =
                            self.session.scroll_y.saturating_add(*delta as u16);
                    }
                    OverlayOutcome::Handled
                }
                _ => OverlayOutcome::Handled,
            };
        }

        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Tab => {
                self.session.focus_next_control();
                self.ensure_focused_visible();
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.session.focus_prev_control();
                self.ensure_focused_visible();
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let effects = self.session.activate_focused();
                self.apply_effects(effects)
            }
            Action::Input(' ') | Action::SpaceAction => {
                if self.session.focused_control.is_some() {
                    let effects = self.session.activate_focused();
                    self.apply_effects(effects)
                } else {
                    let step = (self.last_viewport_height / 2).max(4);
                    self.session.scroll_y = self.session.scroll_y.saturating_add(step);
                    OverlayOutcome::Handled
                }
            }
            Action::Input('m') => {
                self.session.toggle_raw_mode();
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::Input('o') => OverlayOutcome::Dispatch(Action::ServeHtmlPreview),
            Action::Input('h') => {
                self.show_link_urls = !self.show_link_urls;
                OverlayOutcome::Handled
            }
            Action::MoveUp | Action::Input('k') => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(1);
                OverlayOutcome::Handled
            }
            Action::ScrollPageUp => {
                let step = self.last_viewport_height.saturating_sub(2).max(6);
                self.session.scroll_y = self.session.scroll_y.saturating_sub(step);
                OverlayOutcome::Handled
            }
            Action::ScrollPageDown => {
                let step = self.last_viewport_height.saturating_sub(2).max(6);
                self.session.scroll_y = self.session.scroll_y.saturating_add(step);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.session.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.session.scroll_y =
                        self.session.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    self.session.scroll_y =
                        self.session.scroll_y.saturating_add(*delta as u16);
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    /// Translates terminal `(column, row)` coordinates within `self.content_area` to
    /// document `(line_index, col)` and invokes `HtmlViewSession::click_at`.
    pub fn click_at_screen(&mut self, column: u16, row: u16) -> OverlayOutcome {
        if self.content_area.width == 0 || self.content_area.height == 0 {
            return OverlayOutcome::Handled;
        }
        if column < self.content_area.x
            || column >= self.content_area.right()
            || row < self.content_area.y
            || row >= self.content_area.bottom()
        {
            return OverlayOutcome::Handled;
        }
        let line_index =
            (row.saturating_sub(self.content_area.y) as usize) + (self.session.scroll_y as usize);
        let col = column.saturating_sub(self.content_area.x);
        self.click_at_content(line_index, col)
    }

    /// Directly invokes `HtmlViewSession::click_at(line_index, col)` and applies effects.
    pub fn click_at_content(&mut self, line_index: usize, col: u16) -> OverlayOutcome {
        let effects = self.session.click_at(line_index, col);
        self.apply_effects(effects)
    }

    fn apply_effects(&mut self, effects: Vec<HtmlEffect>) -> OverlayOutcome {
        let mut open_url: Option<String> = None;
        let mut toast: Option<String> = None;
        for eff in effects {
            match eff {
                HtmlEffect::OpenUrl(url) => {
                    open_url = Some(url);
                }
                HtmlEffect::Toast(msg) => {
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let target = if action.trim().is_empty() {
                        "local form"
                    } else {
                        action.as_str()
                    };
                    let summary = if fields.is_empty() {
                        format!("Submitted {target}")
                    } else {
                        let pairs = fields
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("Submitted {target}: {pairs}")
                    };
                    self.session.last_status = Some(summary.clone());
                    toast = Some(summary);
                }
                HtmlEffect::DomChanged | HtmlEffect::None => {}
            }
        }
        if let Some(url) = open_url {
            OverlayOutcome::Dispatch(Action::OpenUrl(url))
        } else if let Some(msg) = toast {
            OverlayOutcome::Dispatch(Action::ShowToast(msg))
        } else {
            OverlayOutcome::Handled
        }
    }

    fn ensure_focused_visible(&mut self) {
        let Some(focus_idx) = self.session.focused_control else {
            return;
        };
        let width = self.content_area.width.max(20);
        let theme = HtmlTheme::dark();
        let rendered = self.session.render(width, &theme, self.show_link_urls);
        let Some(ctrl) = rendered.controls.get(focus_idx) else {
            return;
        };
        let ctrl_line = ctrl.line.min(u16::MAX as usize) as u16;
        let vh = self.last_viewport_height.max(1);
        if ctrl_line < self.session.scroll_y {
            self.session.scroll_y = ctrl_line;
        } else if ctrl_line >= self.session.scroll_y.saturating_add(vh) {
            self.session.scroll_y = ctrl_line.saturating_sub(vh.saturating_sub(1));
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let mode_pill = if self.session.raw_mode {
            "[m: Rendered]"
        } else {
            "[m: Raw]"
        };
        let browser_pill = "[o: Browser]";
        let close_pill = "[✕ Esc]";

        let pills: &[(&str, ButtonId)] = &[
            (mode_pill, ButtonId::HtmlToggleRaw),
            (browser_pill, ButtonId::HtmlOpenBrowser),
            (close_pill, ButtonId::ConfirmCancel),
        ];
        let pills_w: u16 = pills
            .iter()
            .map(|(l, _)| l.width() as u16 + 1)
            .sum();
        let show_pills = rect.width > pills_w + 8;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_pills { pills_w + 1 } else { 0 })
            as usize;

        let inner_w = rect.width.saturating_sub(2);
        let inner_h = rect.height.saturating_sub(2);
        let content_w = if inner_w > 2 {
            inner_w.saturating_sub(1)
        } else {
            inner_w
        };
        let html_theme = html_theme_from_ui(theme);
        let show_urls = self.show_link_urls || ui.show_link_urls;

        // Render document before building title so control counts / doc title are fresh.
        let (lines, control_count) = {
            let rendered = self.session.render(content_w.max(12), &html_theme, show_urls);
            (rendered.lines.clone(), rendered.controls.len())
        };

        let mode_badge = if self.session.raw_mode { "Raw" } else { "HTML" };
        let js_badge = if self.session.js_active { " · JS" } else { "" };
        let ctrl_badge = if !self.session.raw_mode && control_count > 0 {
            match self.session.focused_control {
                Some(idx) => format!(" · {}/{control_count}", idx + 1),
                None => format!(" · {control_count} ctrl"),
            }
        } else {
            String::new()
        };
        let title = format!(
            " 🌐 {} [{mode_badge}{js_badge}]{ctrl_badge} ",
            self.title_or_default()
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

        if show_pills {
            let mut px = rect.right().saturating_sub(pills_w + 1);
            for &(label, btn_id) in pills {
                let pw = label.width() as u16;
                if px.saturating_add(pw) >= rect.right() {
                    break;
                }
                let pill_style = if btn_id == ButtonId::ConfirmCancel {
                    Style::default().fg(theme.fg).bg(theme.bg_elevated)
                } else {
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.bg_elevated)
                        .add_modifier(Modifier::BOLD)
                };
                frame.render_widget(
                    Paragraph::new(Span::styled(label, pill_style)),
                    Rect::new(px, rect.y, pw, 1),
                );
                hitmap.add_line(px, rect.y, pw, HitTarget::Button(btn_id));
                px = px.saturating_add(pw + 1);
            }
        }

        if inner.width == 0 || inner.height < 2 {
            self.content_area = Rect::default();
            return;
        }

        let body_h = inner.height.saturating_sub(1);
        let content_area = Rect::new(inner.x, inner.y, content_w.min(inner.width), body_h);
        let footer_area = Rect::new(inner.x, inner.y + body_h, inner.width, 1);

        self.content_area = content_area;
        self.last_viewport_height = body_h;
        hitmap.add(content_area, HitTarget::HtmlPreviewBody);

        let total_lines = lines.len().min(u16::MAX as usize) as u16;
        let max_scroll = total_lines.saturating_sub(body_h);
        self.session.scroll_y = self.session.scroll_y.min(max_scroll);
        let sy = self.session.scroll_y;

        if lines.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(" (Empty HTML document)", theme.muted_style())),
                content_area,
            );
        } else {
            frame.render_widget(Paragraph::new(lines).scroll((sy, 0)), content_area);
        }

        if total_lines > body_h && inner_h > 1 && inner.width > 2 {
            let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, body_h);
            let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1) as usize)
                .position(sy as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        let footer_text = if self.session.editing_input.is_some() {
            " Typing in input · Enter/Esc: Finish · Backspace: Delete ".to_string()
        } else if let Some(ref status) = self.session.last_status {
            format!(
                " {status} │ Tab: Focus · Enter/Space: Activate · m: Raw · o: Browser · Esc/q: Close "
            )
        } else {
            " Tab/⇧Tab: Focus · Enter/Space/Click: Activate · j/k/Wheel: Scroll · m: Raw · o: Browser · Esc/q: Close ".to_string()
        };
        let footer_style = if self.session.editing_input.is_some() {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(&footer_text, footer_area.width as usize),
                footer_style,
            )),
            footer_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn render_overlay_text(overlay: &mut HtmlPreviewOverlay, width: u16, height: u16) -> String {
        let ui = UiState::new();
        let area = Rect::new(0, 0, width, height);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw(frame, area, &ui, &mut hitmap);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn button_onclick_quickjs_and_click_at_mutate_dom() {
        let html = r#"
            <html>
              <head><title>Interactive Mail</title></head>
              <body>
                <p id="status">Pending</p>
                <button id="approve" onclick="document.getElementById('status').textContent = 'Approved!'">Approve</button>
              </body>
            </html>
        "#;
        let mut overlay = HtmlPreviewOverlay::new(None, html.to_string());
        assert!(overlay.session.js_active, "QuickJS should activate for onclick");

        let before = render_overlay_text(&mut overlay, 90, 24);
        assert!(before.contains("Pending"));
        assert!(!before.contains("Approved!"));

        // Locate exact rendered button control coordinates and click via screen coordinates
        let theme = HtmlTheme::dark();
        let ctrl = overlay
            .session
            .render(overlay.content_area.width.max(12), &theme, false)
            .controls[0]
            .clone();
        let click_col = overlay.content_area.x + ctrl.col_start + 1;
        let click_row = overlay.content_area.y + ctrl.line as u16;
        let outcome = overlay.click_at_screen(click_col, click_row);
        assert_eq!(outcome, OverlayOutcome::Handled);

        let after = render_overlay_text(&mut overlay, 90, 24);
        assert!(
            after.contains("Approved!"),
            "Expected onclick handler to update DOM text, got:\n{after}"
        );
    }

    #[test]
    fn tab_focus_space_activate_raw_toggle_and_native_browser_dispatch() {
        let html = r#"
            <div>
              <details><summary>More info</summary><p>Secret details</p></details>
              <input type="text" name="note" value="hi" />
              <a href="https://outlook.office.com/mail">Open Outlook</a>
            </div>
        "#;
        let mut overlay = HtmlPreviewOverlay::new(Some("Newsletter".into()), html.to_string());
        assert!(!overlay.session.js_active, "Static HTML must use 0 MB JS runtime");

        let initial = render_overlay_text(&mut overlay, 96, 24);
        assert!(initial.contains("More info"));
        assert!(!initial.contains("Secret details"));

        // Tab focuses <details><summary>, Space activates it
        assert_eq!(overlay.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(overlay.session.focused_control, Some(0));
        assert_eq!(overlay.handle(&Action::Input(' ')), OverlayOutcome::Handled);
        let expanded = render_overlay_text(&mut overlay, 96, 24);
        assert!(expanded.contains("Secret details"));

        // Tab to text input, Enter enters editing mode, type chars, Enter finishes
        assert_eq!(overlay.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(overlay.handle(&Action::Activate), OverlayOutcome::Handled);
        assert!(overlay.is_editing_input());
        assert_eq!(overlay.handle(&Action::Input('!')), OverlayOutcome::Handled);
        assert_eq!(overlay.handle(&Action::Backspace), OverlayOutcome::Handled);
        assert_eq!(overlay.handle(&Action::Input('?')), OverlayOutcome::Handled);
        assert_eq!(overlay.handle(&Action::Activate), OverlayOutcome::Handled);
        assert!(!overlay.is_editing_input());

        // Tab to link and press Enter -> dispatches OpenUrl
        assert_eq!(overlay.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(
            overlay.handle(&Action::Activate),
            OverlayOutcome::Dispatch(Action::OpenUrl("https://outlook.office.com/mail".into()))
        );

        // Press 'm' -> toggles Rendered <-> Raw HTML
        assert!(!overlay.session.raw_mode);
        assert_eq!(overlay.handle(&Action::Input('m')), OverlayOutcome::Handled);
        assert!(overlay.session.raw_mode);
        let raw_view = render_overlay_text(&mut overlay, 96, 24);
        assert!(raw_view.contains("<details>"));
        assert!(raw_view.contains("[Raw]"));
        assert_eq!(overlay.handle(&Action::Input('m')), OverlayOutcome::Handled);
        assert!(!overlay.session.raw_mode);

        // Press 'o' -> dispatches ServeHtmlPreview for native localhost browser serving
        assert_eq!(
            overlay.handle(&Action::Input('o')),
            OverlayOutcome::Dispatch(Action::ServeHtmlPreview)
        );

        // Press 'q' or Esc -> closes overlay
        assert_eq!(overlay.handle(&Action::Input('q')), OverlayOutcome::Close);
        assert_eq!(overlay.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }

    #[test]
    fn scroll_and_scrollbar_render_on_tall_html() {
        let mut html = String::from("<html><body>");
        for i in 1..=50 {
            html.push_str(&format!("<p>Paragraph line number {i}</p>"));
        }
        html.push_str("</body></html>");

        let mut overlay = HtmlPreviewOverlay::new(Some("Tall Doc".into()), html);
        let top = render_overlay_text(&mut overlay, 80, 16);
        assert!(top.contains("Paragraph line number 1"));

        // Scroll via j / wheel / PgDn
        overlay.handle(&Action::Input('j'));
        assert_eq!(overlay.session.scroll_y, 1);
        overlay.handle(&Action::MouseScroll {
            delta: 3,
            column: 20,
            row: 8,
        });
        assert_eq!(overlay.session.scroll_y, 4);
        overlay.handle(&Action::ScrollPageDown);
        assert!(overlay.session.scroll_y > 4);

        let scrolled = render_overlay_text(&mut overlay, 80, 16);
        assert!(!scrolled.contains("Paragraph line number 1\n"));
        overlay.handle(&Action::Input('g'));
        assert_eq!(overlay.session.scroll_y, 0);

        // Wheel scroll still works while editing an HTML form input.
        overlay.session.editing_input = Some(bitefoundry_html::DomNodeId(0));
        assert_eq!(
            overlay.handle(&Action::MouseScroll {
                delta: 3,
                column: 20,
                row: 8,
            }),
            OverlayOutcome::Handled
        );
        assert_eq!(overlay.session.scroll_y, 3);
        assert_eq!(
            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 20,
                row: 8,
            }),
            OverlayOutcome::Handled
        );
        assert_eq!(overlay.session.scroll_y, 0);
    }

    fn make_test_app_ui() -> (
        crate::ui::app::AppState,
        UiState,
        tokio::sync::mpsc::UnboundedReceiver<crate::ui::app::UiMsg>,
    ) {
        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(crate::sync::FocusTarget::default());
        let (cmd_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = crate::sync::SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = crate::ui::app::AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui, ui_rx)
    }

    fn sample_html_mail(id: &str, body_html: &str, attachments: Vec<crate::models::MailAttachment>) -> crate::models::MailMessage {
        crate::models::MailMessage {
            id: id.to_string(),
            internet_message_id: None,
            conversation_id: Some(format!("conv-{id}")),
            folder_id: Some("inbox".to_string()),
            subject: "Interactive Newsletter".to_string(),
            from: Some(crate::models::EmailRecipient {
                name: Some("Alice".to_string()),
                address: "alice@contoso.com".to_string(),
            }),
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-20T09:00:00Z".to_string(),
            sent_at: None,
            last_modified_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: Some("focused".to_string()),
            has_attachments: !attachments.is_empty(),
            body_loaded: true,
            preview: "Preview".to_string(),
            body_html: body_html.to_string(),
            body_plain: "Plain fallback".to_string(),
            attachments,
            meeting_request: None,
            web_link: None,
        }
    }

    #[tokio::test]
    async fn email_html_pill_click_opens_overlay_and_native_server_serves_html() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut app, mut ui, _ui_rx) = make_test_app_ui();
        let email_html = r#"
            <html><body>
              <p id="msg">Ready</p>
              <button onclick="document.getElementById('msg').textContent = 'Clicked via HitMap'">Run CTA</button>
            </body></html>
        "#;
        let msg = sample_html_mail("m-html", email_html, vec![]);
        app.active_mail_id = Some(msg.id.clone());
        app.mail_messages = vec![msg];

        let area = Rect::new(0, 0, 140, 32);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| {
                crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let pill_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlPreview))
            .expect("[H: HTML] pill must be registered in hitmap");
        let buf = terminal.backend().buffer();
        let pill_text: String = (pill_rect.x..pill_rect.right())
            .map(|x| buf[(x, pill_rect.y)].symbol())
            .collect();
        assert_eq!(pill_text, "[H: HTML]");

        // Clicking [H: HTML] opens Overlay::HtmlPreview
        let _ = crate::ui::action::dispatch(
            Action::Click {
                column: pill_rect.x + 1,
                row: pill_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(
            ui.overlays.last().map(crate::ui::overlays::Overlay::kind),
            Some(crate::ui::overlays::OverlayKind::HtmlPreview)
        );

        // Draw overlay so HitTarget::HtmlPreviewBody and content_area are populated, then click CTA button
        terminal
            .draw(|frame| {
                crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let (click_col, click_row, raw_html, title) = {
            let Some(crate::ui::overlays::Overlay::HtmlPreview(preview)) = ui.overlays.last_mut() else {
                panic!("expected HtmlPreview overlay");
            };
            let theme = HtmlTheme::dark();
            let ctrl = preview
                .session
                .render(preview.content_area.width.max(12), &theme, false)
                .controls[0]
                .clone();
            (
                preview.content_area.x + ctrl.col_start + 1,
                preview.content_area.y + ctrl.line as u16,
                preview.session.raw_html().to_string(),
                preview.title_or_default(),
            )
        };
        let _ = crate::ui::action::dispatch(
            Action::Click {
                column: click_col,
                row: click_row,
            },
            &mut app,
            &mut ui,
        );

        terminal
            .draw(|frame| {
                crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        let screen: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("Clicked via HitMap"),
            "expected mouse click through dispatch to run onclick handler"
        );

        // Verify lazy NativeHtmlServer on AppState serves the email HTML on localhost
        let url = app.serve_html_document(&title, &raw_html).await.unwrap();
        assert!(url.starts_with("http://127.0.0.1:"));
        let (host_port, path_tail) = url.strip_prefix("http://").unwrap().split_once('/').unwrap();
        let mut stream = tokio::net::TcpStream::connect(host_port).await.unwrap();
        let req = format!(
            "GET /{path_tail} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.to_ascii_lowercase().contains("content-type: text/html; charset=utf-8"));
        assert!(resp.contains("Run CTA"));

        // Clicking outside modal closes HtmlPreview (dropping QuickJS runtime)
        let _ = crate::ui::action::dispatch(
            Action::Click { column: 0, row: 0 },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty());
        app.html_server.shutdown();
    }

    #[tokio::test]
    async fn attachments_overlay_routes_html_attachment_to_html_preview() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

        let (mut app, mut ui, _ui_rx) = make_test_app_ui();
        let html_payload = "<!DOCTYPE html><html><body><h1>Invoice #42</h1><button>Pay</button></body></html>";
        let att = crate::models::MailAttachment {
            id: "att-html-1".to_string(),
            name: "invoice.html".to_string(),
            content_type: "text/html".to_string(),
            size: html_payload.len() as u64,
            is_inline: false,
            content_id: None,
            content_bytes_b64: Some(B64.encode(html_payload.as_bytes())),
        };
        let msg = sample_html_mail("m-att", "<p>See attached HTML</p>", vec![att]);
        app.active_mail_id = Some(msg.id.clone());
        app.mail_messages = vec![msg];

        // Open Attachments overlay and press Enter -> opens HtmlPreview instead of downloading
        let _ = crate::ui::action::dispatch(
            Action::OpenOverlay(crate::ui::overlays::OverlayKind::Attachments),
            &mut app,
            &mut ui,
        );
        assert_eq!(
            ui.overlays.last().map(crate::ui::overlays::Overlay::kind),
            Some(crate::ui::overlays::OverlayKind::Attachments)
        );
        let _ = crate::ui::action::dispatch(Action::Activate, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(crate::ui::overlays::Overlay::kind),
            Some(crate::ui::overlays::OverlayKind::HtmlPreview)
        );
        let Some(crate::ui::overlays::Overlay::HtmlPreview(preview)) = ui.overlays.last() else {
            panic!("expected HtmlPreview");
        };
        assert_eq!(preview.title_or_default(), "invoice.html");
        assert!(preview.session.raw_html().contains("Invoice #42"));
    }

    #[tokio::test]
    async fn image_preview_fetch_returning_html_bytes_auto_routes_to_html_preview() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

        let (mut app, mut ui, mut ui_rx) = make_test_app_ui();
        // Attachment masquerading as image/png + chart.png whose actual bytes are HTML
        let fake_img_html = b"<!DOCTYPE html><html><body><h1>Auth Wall</h1><a href=\"https://login.microsoftonline.com\">Sign in</a></body></html>";
        let att = crate::models::MailAttachment {
            id: "att-fake-png".to_string(),
            name: "chart.png".to_string(),
            content_type: "image/png".to_string(),
            size: fake_img_html.len() as u64,
            is_inline: false,
            content_id: None,
            content_bytes_b64: Some(B64.encode(fake_img_html)),
        };
        let msg = sample_html_mail("m-img-html", "<p>Chart</p>", vec![att]);
        app.active_mail_id = Some(msg.id.clone());
        app.mail_messages = vec![msg];

        // Open ImagePreview overlay -> spawns preview decode
        let _ = crate::ui::action::dispatch(Action::OpenImagePreview, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(crate::ui::overlays::Overlay::kind),
            Some(crate::ui::overlays::OverlayKind::ImagePreview)
        );

        // Wait for background task message and process it
        let msg_ev = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for UiMsg")
            .expect("ui channel closed");
        assert!(
            matches!(msg_ev, crate::ui::app::UiMsg::ImageFetchReturnedHtml { .. }),
            "expected ImageFetchReturnedHtml instead of ImageDecodeFailed"
        );
        app.handle_ui_msg(&mut ui, msg_ev);

        // Must have replaced ImagePreview with Overlay::HtmlPreview!
        assert_eq!(
            ui.overlays.last().map(crate::ui::overlays::Overlay::kind),
            Some(crate::ui::overlays::OverlayKind::HtmlPreview)
        );
        let Some(crate::ui::overlays::Overlay::HtmlPreview(preview)) = ui.overlays.last() else {
            panic!("expected HtmlPreview overlay after auto-routing");
        };
        assert!(preview.session.raw_html().contains("Auth Wall"));
    }
}
