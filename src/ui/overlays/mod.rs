//! Overlay stack & HelpOverlay (`?`).

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod event_form;
pub mod file_picker;
pub mod folder_picker;
pub mod html_preview;
pub mod image_preview;
pub mod links;
pub mod mail_compose;
pub mod path_input;
pub mod quick_input;
pub mod search;
pub mod settings;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{
        attachments::AttachmentsState, command_palette::CommandPalette, confirm::ConfirmDialog,
        context_menu::ContextMenu, event_form::EventFormOverlay, file_picker::FilePickerOverlay,
        folder_picker::FolderPickerOverlay, html_preview::HtmlPreviewOverlay,
        image_preview::ImagePreviewOverlay,
        links::LinkExtractorOverlay, mail_compose::MailComposeOverlay, path_input::PathInputState,
        quick_input::QuickInputOverlay, search::SearchOverlay, settings::SettingsState,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    ContextMenu,
    CommandPalette,
    Settings,
    Search,
    Attachments,
    PathInput,
    FilePicker,
    Links,
    MailCompose,
    EventForm,
    FolderPicker,
    QuickInput,
    ImagePreview,
    HtmlPreview,
    Help,
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
    NotHandled,
}

#[derive(Debug)]
pub enum Overlay {
    ContextMenu(ContextMenu),
    CommandPalette(CommandPalette),
    Settings(SettingsState),
    Search(SearchOverlay),
    Attachments(AttachmentsState),
    PathInput(PathInputState),
    FilePicker(FilePickerOverlay),
    Links(LinkExtractorOverlay),
    MailCompose(MailComposeOverlay),
    EventForm(EventFormOverlay),
    FolderPicker(FolderPickerOverlay),
    QuickInput(QuickInputOverlay),
    ImagePreview(ImagePreviewOverlay),
    HtmlPreview(HtmlPreviewOverlay),
    Help(HelpOverlay),
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::Search(_) => OverlayKind::Search,
            Overlay::Attachments(_) => OverlayKind::Attachments,
            Overlay::PathInput(_) => OverlayKind::PathInput,
            Overlay::FilePicker(_) => OverlayKind::FilePicker,
            Overlay::Links(_) => OverlayKind::Links,
            Overlay::MailCompose(_) => OverlayKind::MailCompose,
            Overlay::EventForm(_) => OverlayKind::EventForm,
            Overlay::FolderPicker(_) => OverlayKind::FolderPicker,
            Overlay::QuickInput(_) => OverlayKind::QuickInput,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::HtmlPreview(_) => OverlayKind::HtmlPreview,
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn accepts_text(&self) -> bool {
        match self {
            Overlay::Search(_)
            | Overlay::CommandPalette(_)
            | Overlay::PathInput(_)
            | Overlay::FilePicker(_)
            | Overlay::FolderPicker(_)
            | Overlay::QuickInput(_) => true,
            Overlay::MailCompose(c) => c.field <= 4,
            Overlay::EventForm(e) => e.field <= 4 || e.field == 6,
            Overlay::HtmlPreview(h) => h.is_editing_input(),
            _ => false,
        }
    }

    pub fn context_menu(menu: ContextMenu) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsState::default())
    }

    pub fn search() -> Self {
        Overlay::Search(SearchOverlay::default())
    }

    pub fn attachments() -> Self {
        Overlay::Attachments(AttachmentsState::default())
    }

    pub fn path_input() -> Self {
        Overlay::PathInput(PathInputState::new())
    }

    pub fn file_picker() -> Self {
        Overlay::FilePicker(FilePickerOverlay::new())
    }

    pub fn links(overlay: LinkExtractorOverlay) -> Self {
        Overlay::Links(overlay)
    }

    pub fn mail_compose(compose: MailComposeOverlay) -> Self {
        Overlay::MailCompose(compose)
    }

    pub fn event_form() -> Self {
        Overlay::EventForm(EventFormOverlay::new())
    }

    pub fn folder_picker(message_ids: Vec<String>) -> Self {
        Overlay::FolderPicker(FolderPickerOverlay::new(message_ids))
    }

    pub fn quick_input(quick: QuickInputOverlay) -> Self {
        Overlay::QuickInput(quick)
    }

    pub fn html_preview(preview: HtmlPreviewOverlay) -> Self {
        Overlay::HtmlPreview(preview)
    }

    pub fn help() -> Self {
        Overlay::Help(HelpOverlay)
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        match self {
            Overlay::ContextMenu(menu) => match action {
                Action::MoveUp => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::Activate => menu.activate(),
                Action::CloseOverlay => OverlayOutcome::Close,
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::CommandPalette(palette) => match action {
                Action::Tab => palette.handle(&Action::MoveDown, app, ui),
                Action::PrevFocus => palette.handle(&Action::MoveUp, app, ui),
                other => palette.handle(other, app, ui),
            },
            Overlay::Settings(settings) => match action {
                Action::Tab => settings.handle(&Action::MoveDown, app),
                Action::PrevFocus => settings.handle(&Action::MoveUp, app),
                other => settings.handle(other, app),
            },
            Overlay::Search(search) => search.handle(action, app),
            Overlay::Attachments(tray) => tray.handle(action, app),
            Overlay::PathInput(input) => input.handle(action),
            Overlay::FilePicker(picker) => picker.handle(action),
            Overlay::Links(links) => links.handle(action),
            Overlay::MailCompose(compose) => compose.handle(action, app),
            Overlay::EventForm(form) => form.handle(action),
            Overlay::FolderPicker(picker) => picker.handle(action, app),
            Overlay::QuickInput(input) => input.handle(action),
            Overlay::ImagePreview(preview) => preview.handle(action),
            Overlay::HtmlPreview(preview) => preview.handle(action),
            Overlay::Help(_) => match action {
                Action::CloseOverlay | Action::Activate | Action::Input('q') => {
                    OverlayOutcome::Close
                }
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::Confirm(dialog) => dialog.handle(action),
        }
    }

    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::ContextMenu(menu) => menu.select(index),
            Overlay::CommandPalette(palette) => palette.select(index),
            Overlay::Settings(settings) => settings.select(index),
            Overlay::Search(search) => search.select(index),
            Overlay::Attachments(tray) => tray.select(index),
            Overlay::FilePicker(picker) => picker.select(index),
            Overlay::Links(links) => links.select(index),
            Overlay::MailCompose(compose) => compose.select_field(index),
            Overlay::EventForm(form) => form.select(index),
            Overlay::FolderPicker(picker) => picker.select(index),
            Overlay::QuickInput(_)
            | Overlay::ImagePreview(_)
            | Overlay::HtmlPreview(_)
            | Overlay::PathInput(_)
            | Overlay::Help(_)
            | Overlay::Confirm(_) => {}
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
        match self {
            Overlay::ContextMenu(menu) => {
                menu.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::CommandPalette(palette) => {
                palette.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Settings(settings) => {
                settings.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Search(search) => {
                search.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Attachments(tray) => {
                tray.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::PathInput(input) => {
                input.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::FilePicker(picker) => {
                picker.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::Links(links) => {
                links.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::MailCompose(compose) => compose.draw(frame, area, app, ui, hitmap),
            Overlay::EventForm(form) => {
                form.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::FolderPicker(picker) => {
                picker.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::QuickInput(input) => {
                input.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::ImagePreview(preview) => {
                preview.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::HtmlPreview(preview) => {
                preview.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::Help(_) => {
                draw_help(frame, area, ui, hitmap);
                None
            }
            Overlay::Confirm(dialog) => {
                dialog.draw(frame, area, ui, hitmap);
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Help Overlay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct HelpOverlay;

const HELP_SECTIONS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Navigation & Tabs",
        &[
            ("1 / 2 / 3 · Alt+1..3", "Switch Mail / Calendar / Tasks"),
            ("Ctrl+K", "Omni-Switcher (>cmd #folder @contact /search)"),
            ("/ / Ctrl+F", "Search mail (FTS5 local + Graph remote)"),
            ("Tab / Enter / Esc", "Move focus Folders <-> List <-> Reader"),
            ("j / k · ↑ / ↓", "Move selection / scroll reading pane"),
            ("Space / PgDn / PgUp", "Page-scroll reading pane from list"),
            ("Ctrl+B", "Toggle left sidebar"),
            ("Ctrl+R / F5", "Manual sync / refresh"),
        ],
    ),
    (
        "Mail Triage & Reader",
        &[
            ("c", "Compose new email"),
            ("r / R / f", "Reply / Reply All / Forward"),
            ("e / Del / #", "Archive / Delete email"),
            ("u (List / Read) / g", "Toggle Read/Unread (List) / Links (Read) / Flag"),
            ("H / h", "Interactive HTML preview / toggle inline hrefs"),
            ("m", "Move email to folder"),
            ("t / F / U", "Thread grouping / Focused / Unread filter"),
            ("z / v / Alt+v", "Toggle quoted reply / HD image preview"),
            ("d / A / o", "Download attachment / picker / open link"),
        ],
    ),
    (
        "Mail Compose",
        &[
            ("Ctrl+Enter / Alt+Enter", "Send email"),
            ("Tab / Enter (To/Cc/Bcc)", "Insert autocomplete contact chip"),
            ("Ctrl+V", "Paste text or clipboard image as PNG"),
            ("Ctrl+F / Ctrl+A", "Browse & attach file (z for zoxide jump)"),
            ("Ctrl+E", "Edit body in external $EDITOR"),
            ("Ctrl+S", "Save draft (Esc auto-stashes locally)"),
        ],
    ),
    (
        "Calendar, Tasks & Prefs",
        &[
            ("n (Cal / Tasks)", "New event / new task or To Do list"),
            ("a / y / x", "RSVP Accept / Tentative / Decline"),
            ("o (Cal / Invite)", "Join online meeting URL"),
            ("Space / Enter (Tasks)", "Toggle task completion"),
            ("Ctrl+D", "Toggle Cozy / Compact list density"),
            ("Ctrl+,", "Open settings (theme, protocol, etc.)"),
            ("? / Ctrl+Q", "Help / quit Outlook"),
        ],
    ),
];

fn draw_help(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let rect = centered_rect(
        area,
        area.width.saturating_sub(4).min(100),
        area.height.saturating_sub(3).min(24),
    );
    frame.render_widget(Clear, rect);
    hitmap.add(rect, HitTarget::OverlayBody);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
        .title(Line::from(Span::styled(
            " Microsoft Outlook TUI Help · Esc Close ",
            theme.title(true),
        )))
        .border_style(theme.border_style(true));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    for (index, (title, entries)) in HELP_SECTIONS.iter().enumerate() {
        let target = columns[index % 2];
        if target.width == 0 {
            continue;
        }
        let y_offset = if index < 2 { 0 } else { target.height / 2 };
        let mut lines = Vec::with_capacity(entries.len() + 1);
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            theme.title(true),
        )));
        for (key, desc) in entries.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<23}"), theme.accent_style()),
                Span::raw(desc.to_string()),
            ]));
        }
        let sub = Rect::new(
            target.x,
            target.y + y_offset,
            target.width,
            target.height.saturating_sub(y_offset),
        );
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), sub);
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
