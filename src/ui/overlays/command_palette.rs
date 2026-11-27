//! Unified Omni-Switcher & Command Palette (`Ctrl+K`).
//!
//! Prefix modes:
//! - (none): Mail folders, recent contacts, upcoming meetings, and commands
//! - `>`: commands only (`>Compose`, `>Sync`, `>Cycle Theme`, ...)
//! - `#`: mail folders only (`#Inbox`, `#Sent`, ...)
//! - `@`: contacts (`@alice` -> starts compose to Alice!)
//! - `/`: hands off query to `SearchOverlay` on Enter

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    search_alias: String,
    pub shortcut: String,
    pub action: Action,
    base_priority: i32,
}

fn palette_commands() -> Vec<PaletteItem> {
    let cmds = [
        ("Compose New Email", "c", Action::OpenMailCompose),
        ("Sync / Refresh All", "Ctrl+R / F5", Action::RefreshAll),
        ("Cycle Color Theme", "", Action::CycleTheme),
        ("Switch to Mail Tab", "1 / Alt+1", Action::SelectNavTab(0)),
        ("Switch to Calendar Tab", "2 / Alt+2", Action::SelectNavTab(1)),
        ("Switch to Tasks Tab", "3 / Alt+3", Action::SelectNavTab(2)),
        ("New Calendar Event", "n", Action::OpenEventForm),
        ("New To Do Task", "", Action::OpenNewTodoTask),
        ("Search Mail", "/ / Ctrl+F", Action::OpenSearch),
        ("Toggle Thread Grouping", "t", Action::ToggleGroupThreads),
        (
            "Expand / Collapse Older Thread Messages",
            "E",
            Action::ToggleThreadMessages,
        ),
        ("Toggle Focused / All Mail", "F", Action::ToggleFocusedOnly),
        ("Toggle Unread Filter", "U", Action::ToggleUnreadFilter),
        (
            "Toggle Message Density (Cozy / Compact)",
            "Ctrl+D",
            Action::ToggleDensity,
        ),
        ("Preview Image Attachment", "v / Alt+v", Action::OpenImagePreview),
        ("Open Interactive HTML Preview", "H", Action::OpenHtmlPreview),
        ("Extract Links from Item", "u", Action::OpenLinkExtractor),
        ("Toggle Inline Link URLs (href)", "h", Action::ToggleShowLinkUrls),
        ("Attach File (File Picker)", "Ctrl+F", Action::OpenFilePicker),
        ("View Attachments", "A", Action::OpenOverlay(OverlayKind::Attachments)),
        ("Move Email to Folder", "m", Action::OpenFolderPicker),
        ("Toggle Left Sidebar", "Ctrl+B", Action::ToggleNavRail),
        ("Toggle Notifications", "", Action::ToggleNotify),
        ("Toggle Inline Image Previews", "", Action::ToggleInlineImages),
        ("Load Older Mail", "o", Action::LoadOlderMail),
        ("Check for Updates", "", Action::CheckForUpdates),
        ("Install Self-Update", "", Action::ApplySelfUpdate),
        ("Settings", "Ctrl+,", Action::OpenOverlay(OverlayKind::Settings)),
        (
            "Keyboard Shortcuts Help",
            "?",
            Action::OpenOverlay(OverlayKind::Help),
        ),
        ("Quit Outlook", "Ctrl+Q", Action::QuitConfirm),
    ];

    cmds.into_iter()
        .map(|(label, shortcut, action)| PaletteItem {
            label: label.to_string(),
            search_alias: String::new(),
            shortcut: shortcut.to_string(),
            action,
            base_priority: 0,
        })
        .collect()
}

fn folder_items(app: &AppState) -> Vec<PaletteItem> {
    let mut items = vec![
        PaletteItem {
            label: "Folder: Unread".to_string(),
            search_alias: "unread".to_string(),
            shortcut: "Mail".to_string(),
            action: Action::SelectMailFolder("__virtual_unread__".to_string()),
            base_priority: 120,
        },
        PaletteItem {
            label: "Folder: Flagged".to_string(),
            search_alias: "flagged".to_string(),
            shortcut: "Mail".to_string(),
            action: Action::SelectMailFolder("__virtual_flagged__".to_string()),
            base_priority: 115,
        },
    ];

    let paths = crate::ui::nav_pane::folder_display_paths(&app.mail_folders);
    for folder in &app.mail_folders {
        let unread_bonus = if folder.unread_item_count > 0 { 180 } else { 80 };
        let shortcut = if folder.unread_item_count > 0 {
            format!("{} unread", folder.unread_item_count)
        } else {
            "Folder".to_string()
        };
        let path = paths
            .get(&folder.id)
            .map(String::as_str)
            .unwrap_or(folder.display_name.as_str());
        let search_alias = match folder.well_known_name.as_deref() {
            Some(wk) if !wk.eq_ignore_ascii_case(&folder.display_name) => {
                format!("{wk} {}", folder.display_name)
            }
            _ => folder.display_name.clone(),
        };
        items.push(PaletteItem {
            label: format!("Folder: {path}"),
            search_alias,
            shortcut,
            action: Action::SelectMailFolder(folder.id.clone()),
            base_priority: unread_bonus,
        });
    }
    items
}

fn contact_items(app: &AppState) -> Vec<PaletteItem> {
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for c in &app.contacts {
        let Some(email) = c.primary_email() else {
            continue;
        };
        if !seen.insert(email.to_ascii_lowercase()) {
            continue;
        }
        items.push(PaletteItem {
            label: format!("Compose to {} <{email}>", c.display_name),
            search_alias: format!("@{} {email}", c.display_name),
            shortcut: if c.source == "directory" {
                "Org".to_string()
            } else {
                "Contact".to_string()
            },
            action: Action::ComposeTo(email.to_string()),
            base_priority: 60,
        });
    }
    // Also add recent mail senders not yet in contacts
    for m in app.mail_messages.iter().take(40) {
        if let Some(ref from) = m.from {
            let addr = from.address.trim();
            if !addr.is_empty() && seen.insert(addr.to_ascii_lowercase()) {
                items.push(PaletteItem {
                    label: format!("Compose to {}", from.display()),
                    search_alias: format!("@{} {addr}", from.short_display()),
                    shortcut: "Recent".to_string(),
                    action: Action::ComposeTo(addr.to_string()),
                    base_priority: 40,
                });
            }
        }
    }
    items
}

fn event_items(app: &AppState) -> Vec<PaletteItem> {
    app.calendar_events
        .iter()
        .take(25)
        .map(|ev| {
            let day = ev.start_iso.get(..10).unwrap_or("");
            let time = ev.start_iso.get(11..16).unwrap_or("");
            let badge = if ev.is_online_meeting {
                format!("{day} {time} · Online")
            } else {
                format!("{day} {time}")
            };
            PaletteItem {
                label: format!("Meeting: {}", ev.subject),
                search_alias: ev.organizer_name.clone().unwrap_or_default(),
                shortcut: badge,
                action: Action::JumpToCalendarEvent(ev.id.clone()),
                base_priority: 50,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn filtered(&self, app: &AppState, _ui: &UiState) -> Vec<PaletteItem> {
        let raw = self.query.trim();
        let (mode, needle) = if let Some(rest) = raw.strip_prefix('>') {
            ('>', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('#') {
            ('#', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('@') {
            ('@', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('/') {
            let search_q = rest.trim().to_string();
            let label = if search_q.is_empty() {
                "Search all emails…".to_string()
            } else {
                format!("Search emails for \"{search_q}\"")
            };
            return vec![PaletteItem {
                label,
                search_alias: String::new(),
                shortcut: "Enter".to_string(),
                action: Action::OpenSearchWithQuery(search_q),
                base_priority: 500,
            }];
        } else {
            (' ', raw.to_lowercase())
        };

        let mut pool = Vec::new();
        match mode {
            '>' => pool.extend(palette_commands()),
            '#' => pool.extend(folder_items(app)),
            '@' => pool.extend(contact_items(app)),
            _ => {
                pool.extend(folder_items(app));
                pool.extend(contact_items(app));
                pool.extend(event_items(app));
                pool.extend(palette_commands());
            }
        }

        let mut scored: Vec<(i32, PaletteItem)> = pool
            .into_iter()
            .filter_map(|item| {
                if needle.is_empty() {
                    return Some((item.base_priority, item));
                }
                let label_l = item.label.to_lowercase();
                let alias_l = item.search_alias.to_lowercase();
                let short_l = item.shortcut.to_lowercase();
                let match_score = if label_l.starts_with(&needle) || alias_l.starts_with(&needle) {
                    400
                } else if label_l.contains(&needle)
                    || alias_l.contains(&needle)
                    || short_l.contains(&needle)
                {
                    200
                } else if fuzzy_matches(&label_l, &needle)
                    || (!alias_l.is_empty() && fuzzy_matches(&alias_l, &needle))
                    || fuzzy_matches(&short_l, &needle)
                {
                    80
                } else {
                    return None;
                };
                Some((match_score + item.base_priority, item))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::MoveUp => {
                let len = self.filtered(app, ui).len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered(app, ui).len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let items = self.filtered(app, ui);
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
                    None => OverlayOutcome::Handled,
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
            area.width.saturating_sub(6).min(74),
            (area.height.saturating_sub(4)).min(20),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Omni-Switcher · > cmd  # folder  @ contact  / search · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled("> ", theme.title(true)),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let items = self.filtered(app, ui);
        let active_sel = if items.is_empty() {
            0
        } else {
            self.selected.min(items.len() - 1)
        };
        let list_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " No matching folders, contacts, meetings, or commands",
                    theme.muted_style(),
                )),
                Rect::new(inner.x, list_y, inner.width, 1),
            );
            return;
        }

        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
        for (row, (index, item)) in items
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
            let y = list_y + row as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let is_selected = index == active_sel;
            let style = if is_selected {
                theme.selection()
            } else {
                Style::default()
            };
            let right_w =
                unicode_width::UnicodeWidthStr::width(item.shortcut.as_str()).clamp(8, 22);
            let label_w = (inner.width as usize).saturating_sub(right_w + 3).max(10);
            let truncated_label = crate::ui::truncate_width(&item.label, label_w);
            let used_w = unicode_width::UnicodeWidthStr::width(truncated_label.as_str());
            let pad = label_w.saturating_sub(used_w);
            let line = Line::from(vec![
                Span::styled(format!(" {truncated_label}{}", " ".repeat(pad)), style),
                Span::styled(
                    crate::ui::truncate_width(&item.shortcut, right_w),
                    theme.muted_style(),
                ),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }
}

fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
    let mut chars = needle.chars();
    let Some(mut target) = chars.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == target {
            match chars.next() {
                Some(next) => target = next,
                None => return true,
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        graph::GraphClient,
        models::{CalendarEvent, ContactItem, MailFolder},
        storage::Storage,
        sync::{self, SyncCommand},
    };
    use tokio::sync::mpsc;

    fn make_palette_app() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        app.mail_folders = vec![MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 3,
            total_item_count: 20,
        }];
        app.contacts = vec![ContactItem {
            id: "c1".into(),
            display_name: "Alice Cooper".into(),
            given_name: None,
            surname: None,
            email_addresses: vec!["alice@contoso.com".into()],
            mobile_phone: None,
            business_phones: vec![],
            company_name: None,
            job_title: None,
            department: None,
            notes: None,
            source: "contact".into(),
        }];
        app.calendar_events = vec![CalendarEvent {
            id: "ev1".into(),
            subject: "Design Sync".into(),
            start_iso: "2026-11-19T14:00:00Z".into(),
            end_iso: "2026-11-19T15:00:00Z".into(),
            time_zone: "UTC".into(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: vec![],
            response_status: "accepted".into(),
            is_online_meeting: true,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
            ..Default::default()
        }];
        (app, ui)
    }

    #[tokio::test]
    async fn filters_folders_contacts_commands_and_search() {
        let (mut app, ui) = make_palette_app();
        let mut palette = CommandPalette::new();

        palette.query = ">theme".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res.iter().all(|i| !matches!(
            i.action,
            Action::SelectMailFolder(_)
                | Action::ComposeTo(_)
                | Action::JumpToCalendarEvent(_)
                | Action::OpenSearchWithQuery(_)
        )));
        assert!(res.iter().any(|i| i.action == Action::CycleTheme));

        palette.query = "#inb".to_string();
        let res = palette.filtered(&app, &ui);
        assert!(res
            .iter()
            .any(|i| matches!(i.action, Action::SelectMailFolder(ref id) if id == "f-inbox")));

        palette.query = "#2026".to_string();
        app.mail_folders.push(MailFolder {
            id: "f-proj".into(),
            display_name: "Projects".into(),
            well_known_name: None,
            parent_folder_id: Some("f-inbox".into()),
            child_folder_count: 1,
            unread_item_count: 0,
            total_item_count: 5,
        });
        app.mail_folders.push(MailFolder {
            id: "f-2026".into(),
            display_name: "2026".into(),
            well_known_name: None,
            parent_folder_id: Some("f-proj".into()),
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 2,
        });
        let sub_res = palette.filtered(&app, &ui);
        assert!(sub_res
            .iter()
            .any(|i| i.label == "Folder: Inbox / Projects / 2026"));

        palette.query = "@alice".to_string();
        let res = palette.filtered(&app, &ui);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].action, Action::ComposeTo("alice@contoso.com".into()));

        palette.query = "/quarterly".to_string();
        let outcome = palette.handle(&Action::Activate, &app, &ui);
        assert_eq!(
            outcome,
            OverlayOutcome::CloseAndDispatch(Action::OpenSearchWithQuery("quarterly".into()))
        );

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}
