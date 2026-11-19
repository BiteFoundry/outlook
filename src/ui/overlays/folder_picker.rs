//! Fuzzy-searchable Move to Folder modal (`m` in Mail view).

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::MailFolder,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        nav_pane::folder_display_paths,
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Default)]
pub struct FolderPickerOverlay {
    pub message_ids: Vec<String>,
    pub query: String,
    pub selected: usize,
}

impl FolderPickerOverlay {
    pub fn new(message_ids: Vec<String>) -> Self {
        Self {
            message_ids,
            query: String::new(),
            selected: 0,
        }
    }

    pub fn filtered<'a>(&self, folders: &'a [MailFolder]) -> Vec<&'a MailFolder> {
        let paths = folder_display_paths(folders);
        self.filtered_with_paths(folders, &paths)
    }

    fn filtered_with_paths<'a>(
        &self,
        folders: &'a [MailFolder],
        paths: &HashMap<String, String>,
    ) -> Vec<&'a MailFolder> {
        let q = self.query.trim().to_lowercase();
        let mut out: Vec<&'a MailFolder> = folders
            .iter()
            .filter(|f| {
                if q.is_empty() {
                    return true;
                }
                let name = f.display_name.to_lowercase();
                let path = paths
                    .get(&f.id)
                    .map(|p| p.to_lowercase())
                    .unwrap_or_else(|| name.clone());
                let wk = f
                    .well_known_name
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase();
                path.contains(&q)
                    || name.contains(&q)
                    || wk.contains(&q)
                    || fuzzy_matches(&path, &q)
                    || fuzzy_matches(&name, &q)
            })
            .collect();
        out.sort_by(|a, b| {
            let a_path = paths
                .get(&a.id)
                .map(String::as_str)
                .unwrap_or(a.display_name.as_str());
            let b_path = paths
                .get(&b.id)
                .map(String::as_str)
                .unwrap_or(b.display_name.as_str());
            let a_exact = !q.is_empty()
                && (a.display_name.to_lowercase().starts_with(&q)
                    || a_path.to_lowercase().starts_with(&q));
            let b_exact = !q.is_empty()
                && (b.display_name.to_lowercase().starts_with(&q)
                    || b_path.to_lowercase().starts_with(&q));
            b_exact
                .cmp(&a_exact)
                .then_with(|| a_path.to_lowercase().cmp(&b_path.to_lowercase()))
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
        out
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let paths = folder_display_paths(&app.mail_folders);
        let matches = self.filtered_with_paths(&app.mail_folders, &paths);
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
                if !matches.is_empty() {
                    let cur = self.selected.min(matches.len() - 1);
                    self.selected = (cur + matches.len() - 1) % matches.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if !matches.is_empty() {
                    let cur = self.selected.min(matches.len() - 1);
                    self.selected = (cur + 1) % matches.len();
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let idx = self.selected.min(matches.len().saturating_sub(1));
                if let Some(folder) = matches.get(idx) {
                    let destination_name = paths
                        .get(&folder.id)
                        .cloned()
                        .unwrap_or_else(|| folder.display_name.clone());
                    OverlayOutcome::CloseAndDispatch(Action::MoveMailToFolder {
                        message_ids: self.message_ids.clone(),
                        destination_folder_id: folder.id.clone(),
                        destination_name,
                    })
                } else {
                    OverlayOutcome::Handled
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
            area.width.saturating_sub(8).min(58),
            area.height.saturating_sub(4).min(18),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = format!(
            " Move {} Message{} to Folder · Esc Cancel ",
            self.message_ids.len().max(1),
            if self.message_ids.len() == 1 { "" } else { "s" }
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", theme.accent_style()),
                Span::raw(self.query.clone()),
                Span::styled("▏", theme.muted_style()),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let list_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }

        let paths = folder_display_paths(&app.mail_folders);
        let matches = self.filtered_with_paths(&app.mail_folders, &paths);
        if matches.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(" No matching folders", theme.muted_style())),
                Rect::new(inner.x, list_y, inner.width, 1),
            );
            return;
        }

        let active_sel = self.selected.min(matches.len() - 1);
        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
        for (slot, (index, folder)) in matches
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
            let y = list_y + slot as u16;
            let is_sel = index == active_sel;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default()
            };
            let badge = if folder.unread_item_count > 0 {
                format!(" ({})", folder.unread_item_count)
            } else {
                String::new()
            };
            let path = paths
                .get(&folder.id)
                .map(String::as_str)
                .unwrap_or(folder.display_name.as_str());
            let label = crate::ui::truncate_width(
                &format!(" {}{}", path, badge),
                inner.width as usize,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(label, style)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
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

    #[test]
    fn filters_and_formats_hierarchical_subfolder_paths() {
        let folders = vec![
            MailFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 1,
                unread_item_count: 2,
                total_item_count: 10,
            },
            MailFolder {
                id: "f-proj".into(),
                display_name: "Projects".into(),
                well_known_name: None,
                parent_folder_id: Some("f-inbox".into()),
                child_folder_count: 1,
                unread_item_count: 0,
                total_item_count: 4,
            },
            MailFolder {
                id: "f-2026".into(),
                display_name: "2026".into(),
                well_known_name: None,
                parent_folder_id: Some("f-proj".into()),
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 1,
            },
        ];

        let picker = FolderPickerOverlay {
            message_ids: vec!["m1".into()],
            query: "inbox / projects".into(),
            selected: 0,
        };
        let filtered = picker.filtered(&folders);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].id, "f-proj");
        assert_eq!(filtered[1].id, "f-2026");
    }
}
