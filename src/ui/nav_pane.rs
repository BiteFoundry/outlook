//! Unified Left Sidebar (`ui/nav_pane.rs`):
//! - Top tab header: `1 Mail · 2 Cal · 3 Tasks` (clickable + `1`/`2`/`3` & `Alt+1..3`)
//! - Action strip: `[Ctrl+K Jump]` + `[Filter]` + `[+New]`
//! - Tab 0 (`Mail`): `FAVORITES` (Inbox, Unread, Flagged, Drafts, Sent, Archive, Trash + user favorites) + `FOLDERS`
//! - Tab 1 (`Calendar`): Compact Mini Month Calendar Grid (today + `•` event days) + Quick Range Jumpers
//! - Tab 2 (`Tasks`): Microsoft To Do lists + `n` to create list

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Duration, Local, NaiveDate};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    models::MailFolder,
    ui::{
        app::{AppState, CalendarRangeFilter, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
    },
};

pub const TAB_MAIL: usize = 0;
pub const TAB_CALENDAR: usize = 1;
pub const TAB_TASKS: usize = 2;
pub const TAB_COUNT: usize = 3;

pub const VIRTUAL_UNREAD_FOLDER: &str = "__virtual_unread__";
pub const VIRTUAL_FLAGGED_FOLDER: &str = "__virtual_flagged__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    SectionHeader {
        label: &'static str,
    },
    MailFolder {
        folder_id: String,
        display_name: String,
        unread: i32,
        is_virtual: bool,
        depth: u8,
        /// The folder has subfolders (cached children or a non-zero `childFolderCount`).
        has_children: bool,
        /// Its subfolders are listed beneath it (`false` when collapsed or childless).
        expanded: bool,
    },
    CalendarRange {
        label: &'static str,
        range: CalendarRangeFilter,
    },
    TodoList {
        list_id: String,
        display_name: String,
        wellknown: Option<String>,
    },
    ActionButton {
        label: &'static str,
        action_id: ButtonId,
    },
}

impl NavRow {
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::SectionHeader { .. })
    }
}

pub fn build_nav_rows(app: &AppState, ui: &UiState) -> Vec<NavRow> {
    match ui.active_nav_tab {
        TAB_MAIL => build_mail_nav_rows(app, &ui.collapsed_folder_ids),
        TAB_CALENDAR => build_calendar_nav_rows(),
        TAB_TASKS => build_todo_nav_rows(app),
        _ => Vec::new(),
    }
}

pub fn nav_row_count(app: &AppState, ui: &UiState) -> usize {
    build_nav_rows(app, ui).len()
}

pub fn nav_row_at(app: &AppState, ui: &UiState, index: usize) -> Option<NavRow> {
    build_nav_rows(app, ui).into_iter().nth(index)
}

pub(crate) fn find_folder_by_wellknown<'a>(
    folders: &'a [MailFolder],
    well_known: &str,
    fallback_names: &[&str],
) -> Option<&'a MailFolder> {
    folders
        .iter()
        .find(|f| {
            f.well_known_name
                .as_deref()
                .is_some_and(|wk| wk.eq_ignore_ascii_case(well_known))
        })
        .or_else(|| {
            folders.iter().find(|f| {
                fallback_names
                    .iter()
                    .any(|n| f.display_name.eq_ignore_ascii_case(n))
            })
        })
}

/// Builds a map of `folder_id -> "Parent / Child / Leaf"` full hierarchical display paths.
pub(crate) fn folder_display_paths(folders: &[MailFolder]) -> HashMap<String, String> {
    let by_id: HashMap<&str, &MailFolder> = folders.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut out = HashMap::with_capacity(folders.len());
    for folder in folders {
        let mut parts = vec![folder.display_name.as_str()];
        let mut visited = HashSet::new();
        visited.insert(folder.id.as_str());
        let mut cur_parent = folder.parent_folder_id.as_deref();
        for _ in 0..32 {
            let Some(pid) = cur_parent.map(str::trim).filter(|s| !s.is_empty()) else {
                break;
            };
            if !visited.insert(pid) {
                break;
            }
            let Some(parent) = by_id.get(pid) else {
                break;
            };
            parts.push(parent.display_name.as_str());
            cur_parent = parent.parent_folder_id.as_deref();
        }
        parts.reverse();
        out.insert(folder.id.clone(), parts.join(" / "));
    }
    out
}

/// Parent -> children index of the cached folder list, plus the user's collapsed folders.
struct FolderTree<'a> {
    children: HashMap<&'a str, Vec<&'a MailFolder>>,
    collapsed: &'a HashSet<String>,
}

impl<'a> FolderTree<'a> {
    fn new(folders: &'a [MailFolder], collapsed: &'a HashSet<String>) -> Self {
        let mut children: HashMap<&str, Vec<&MailFolder>> = HashMap::new();
        for folder in folders {
            if let Some(pid) = folder.parent_folder_id.as_deref().map(str::trim) {
                if !pid.is_empty() && pid != folder.id {
                    children.entry(pid).or_default().push(folder);
                }
            }
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| {
                a.display_name
                    .to_ascii_lowercase()
                    .cmp(&b.display_name.to_ascii_lowercase())
                    .then_with(|| a.display_name.cmp(&b.display_name))
            });
        }
        Self {
            children,
            collapsed,
        }
    }

    fn has_children(&self, folder: &MailFolder) -> bool {
        folder.child_folder_count > 0 || self.children.contains_key(folder.id.as_str())
    }

    /// Pushes `folder` (already claimed in `shown_ids` by the caller) and, unless it is collapsed,
    /// its subfolders nested beneath it.
    fn push_folder(
        &self,
        folder: &MailFolder,
        display_name: String,
        depth: u8,
        shown_ids: &mut HashSet<String>,
        rows: &mut Vec<NavRow>,
    ) {
        let has_children = self.has_children(folder);
        let expanded = has_children && !self.collapsed.contains(&folder.id);
        rows.push(NavRow::MailFolder {
            folder_id: folder.id.clone(),
            display_name,
            unread: folder.unread_item_count,
            is_virtual: false,
            depth,
            has_children,
            expanded,
        });
        self.append_descendants(&folder.id, depth.saturating_add(1), expanded, shown_ids, rows);
    }

    /// Claims every descendant of `parent_id` in `shown_ids` (so none resurfaces as a depth-0 root
    /// under `FOLDERS`) and lists them when `visible`; a collapsed subtree is claimed but hidden.
    fn append_descendants(
        &self,
        parent_id: &str,
        depth: u8,
        visible: bool,
        shown_ids: &mut HashSet<String>,
        rows: &mut Vec<NavRow>,
    ) {
        let Some(children) = self.children.get(parent_id) else {
            return;
        };
        for child in children {
            if !shown_ids.insert(child.id.clone()) {
                continue;
            }
            if visible {
                self.push_folder(child, child.display_name.clone(), depth, shown_ids, rows);
            } else {
                self.append_descendants(&child.id, depth.saturating_add(1), false, shown_ids, rows);
            }
        }
    }
}

fn placeholder_folder_row(folder_id: &str, label: &str, unread: i32) -> NavRow {
    NavRow::MailFolder {
        folder_id: folder_id.to_string(),
        display_name: label.to_string(),
        unread,
        is_virtual: false,
        depth: 0,
        has_children: false,
        expanded: false,
    }
}

fn virtual_folder_row(folder_id: &str, label: &str, count: i32) -> NavRow {
    NavRow::MailFolder {
        folder_id: folder_id.to_string(),
        display_name: label.to_string(),
        unread: count,
        is_virtual: true,
        depth: 0,
        has_children: false,
        expanded: false,
    }
}

fn build_mail_nav_rows(app: &AppState, collapsed: &HashSet<String>) -> Vec<NavRow> {
    let mut rows = Vec::new();
    let mut shown_ids: HashSet<String> = HashSet::new();
    let tree = FolderTree::new(&app.mail_folders, collapsed);

    rows.push(NavRow::SectionHeader { label: "FAVORITES" });

    // 1. Inbox + recursive subfolders
    let mut fixed_ids: HashSet<String> = HashSet::new();
    if let Some(inbox) =
        find_folder_by_wellknown(&app.mail_folders, "inbox", &["Inbox", "받은 편지함"])
    {
        shown_ids.insert(inbox.id.clone());
        fixed_ids.insert(inbox.id.clone());
        tree.push_folder(inbox, "Inbox".to_string(), 0, &mut shown_ids, &mut rows);
    } else {
        shown_ids.insert("inbox".to_string());
        rows.push(placeholder_folder_row("inbox", "Inbox", app.unread_inbox_count()));
    }

    // 2. Virtual smart views (mailbox-wide counts from folders/SQLite, never just loaded slice)
    rows.push(virtual_folder_row(
        VIRTUAL_UNREAD_FOLDER,
        "Unread",
        app.smart_unread_count(),
    ));
    rows.push(virtual_folder_row(
        VIRTUAL_FLAGGED_FOLDER,
        "Flagged",
        app.smart_flagged_count(),
    ));

    // 3. Well-known folders + their subfolders nested beneath them
    let well_known_specs: [(&str, &str, &[&str]); 4] = [
        ("drafts", "Drafts", &["Drafts", "임시 보관함"]),
        ("sentitems", "Sent", &["Sent Items", "Sent", "보낸 편지함"]),
        ("archive", "Archive", &["Archive", "보관함"]),
        ("deleteditems", "Trash", &["Deleted Items", "Trash", "지운 편지함"]),
    ];
    for (wk_id, label, aliases) in well_known_specs {
        if let Some(folder) = find_folder_by_wellknown(&app.mail_folders, wk_id, aliases) {
            shown_ids.insert(folder.id.clone());
            fixed_ids.insert(folder.id.clone());
            tree.push_folder(folder, label.to_string(), 0, &mut shown_ids, &mut rows);
        } else {
            shown_ids.insert(wk_id.to_string());
            rows.push(placeholder_folder_row(wk_id, label, 0));
        }
    }

    // 4. User favorite_folders from config. A favorite is a shortcut, so it is listed even when its
    // folder already sits in another displayed subtree (e.g. an Inbox subfolder); there it stays a
    // flat row instead of duplicating that subtree. Otherwise its subfolders nest beneath it.
    let mut favorite_ids: HashSet<&str> = HashSet::new();
    for fav_id in &app.config.favorite_folders {
        let Some(folder) = app.mail_folders.iter().find(|f| &f.id == fav_id) else {
            continue;
        };
        if fixed_ids.contains(&folder.id) || !favorite_ids.insert(folder.id.as_str()) {
            continue;
        }
        if shown_ids.insert(folder.id.clone()) {
            tree.push_folder(folder, folder.display_name.clone(), 0, &mut shown_ids, &mut rows);
        } else {
            rows.push(NavRow::MailFolder {
                folder_id: folder.id.clone(),
                display_name: folder.display_name.clone(),
                unread: folder.unread_item_count,
                is_virtual: false,
                depth: 0,
                has_children: false,
                expanded: false,
            });
        }
    }

    // 5. Custom folders (hierarchical tree)
    let unshown: Vec<&MailFolder> = app
        .mail_folders
        .iter()
        .filter(|f| !shown_ids.contains(&f.id))
        .collect();
    if !unshown.is_empty() {
        rows.push(NavRow::SectionHeader { label: "FOLDERS" });
        let unshown_ids: HashSet<&str> = unshown.iter().map(|f| f.id.as_str()).collect();
        let mut roots: Vec<&MailFolder> = unshown
            .iter()
            .copied()
            .filter(|f| {
                let Some(pid) = f.parent_folder_id.as_deref().map(str::trim) else {
                    return true;
                };
                pid.is_empty() || pid == f.id || !unshown_ids.contains(pid)
            })
            .collect();
        roots.sort_by(|a, b| {
            a.display_name
                .to_ascii_lowercase()
                .cmp(&b.display_name.to_ascii_lowercase())
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
        for root in roots {
            if !shown_ids.insert(root.id.clone()) {
                continue;
            }
            tree.push_folder(root, root.display_name.clone(), 0, &mut shown_ids, &mut rows);
        }
        // Safety sweep for any disconnected cyclic folders
        let mut remaining: Vec<&MailFolder> = unshown
            .into_iter()
            .filter(|f| !shown_ids.contains(&f.id))
            .collect();
        remaining.sort_by(|a, b| {
            a.display_name
                .to_ascii_lowercase()
                .cmp(&b.display_name.to_ascii_lowercase())
        });
        for f in remaining {
            if shown_ids.insert(f.id.clone()) {
                tree.push_folder(f, f.display_name.clone(), 0, &mut shown_ids, &mut rows);
            }
        }
    }

    rows
}

fn build_calendar_nav_rows() -> Vec<NavRow> {
    vec![
        NavRow::SectionHeader {
            label: "QUICK RANGES",
        },
        NavRow::CalendarRange {
            label: "Today",
            range: CalendarRangeFilter::Today,
        },
        NavRow::CalendarRange {
            label: "Tomorrow",
            range: CalendarRangeFilter::Tomorrow,
        },
        NavRow::CalendarRange {
            label: "Next 7 Days",
            range: CalendarRangeFilter::Next7Days,
        },
        NavRow::CalendarRange {
            label: "This Month",
            range: CalendarRangeFilter::ThisMonth,
        },
        NavRow::CalendarRange {
            label: "All Upcoming",
            range: CalendarRangeFilter::AllUpcoming,
        },
    ]
}

fn build_todo_nav_rows(app: &AppState) -> Vec<NavRow> {
    let mut rows = vec![NavRow::SectionHeader {
        label: "TASK LISTS",
    }];
    let mut lists = app.todo_lists.clone();
    lists.sort_by(|a, b| {
        let rank = |l: &crate::models::TodoList| match l.wellknown_list_name.as_deref() {
            Some("defaultList") => 0,
            Some("flaggedEmails") => 1,
            _ => 2,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    for list in lists {
        rows.push(NavRow::TodoList {
            list_id: list.id,
            display_name: list.display_name,
            wellknown: list.wellknown_list_name,
        });
    }
    rows.push(NavRow::ActionButton {
        label: "+ New List (n)",
        action_id: ButtonId::NewTodoList,
    });
    rows
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    if area.width == 0 || area.height == 0 {
        ui.nav_area = Rect::default();
        return;
    }

    let theme = &ui.theme;
    let sidebar_style = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(sidebar_style), area);

    draw_tab_strip(frame, area, app, ui, hitmap);

    if area.height < 2 {
        ui.nav_area = Rect::default();
        return;
    }

    draw_filter_strip(frame, area, ui, hitmap);

    let mut content_y = area.y.saturating_add(2);

    // Tab 1 (Calendar) renders a compact Mini Month Calendar Grid above the quick range jumpers!
    if ui.active_nav_tab == TAB_CALENDAR && area.bottom() > content_y + 8 {
        let cal_h = 8u16;
        let cal_rect = Rect::new(area.x, content_y, area.width, cal_h);
        draw_mini_calendar(frame, cal_rect, app, ui, hitmap);
        content_y = content_y.saturating_add(cal_h + 1);
    }

    let list_h = area.bottom().saturating_sub(content_y);
    let list_area = Rect::new(area.x, content_y, area.width, list_h);
    ui.nav_area = list_area;

    if list_area.width == 0 || list_area.height == 0 {
        return;
    }

    let rows = build_nav_rows(app, ui);
    if rows.is_empty() {
        let empty_style = if theme.no_color {
            theme.muted_style()
        } else {
            theme.muted_style().bg(theme.bg_sidebar)
        };
        frame.render_widget(
            Paragraph::new(" No items").style(empty_style),
            list_area,
        );
        return;
    }

    let selected = ui.nav_selected.min(rows.len().saturating_sub(1));
    let height = list_area.height as usize;
    let offset = selection_offset(selected, height, rows.len());
    let width = list_area.width as usize;

    let nav_focused = ui.focus() == Focus::Nav;
    let mut lines = Vec::with_capacity(height);

    for (idx, row) in rows.iter().enumerate().skip(offset).take(height) {
        let is_selected = nav_focused && idx == selected && row.is_selectable();
        let is_active = match row {
            NavRow::MailFolder { folder_id, .. } => {
                app.active_folder_id.eq_ignore_ascii_case(folder_id)
            }
            NavRow::CalendarRange { range, .. } => ui.cal_range == *range,
            NavRow::TodoList { list_id, .. } => {
                app.active_todo_list_id.as_deref() == Some(list_id.as_str())
            }
            _ => false,
        };
        lines.push(render_nav_row(row, width, is_selected, is_active, ui));
        if row.is_selectable() {
            let row_y = list_area.y + lines.len() as u16 - 1;
            hitmap.add_line(list_area.x, row_y, list_area.width, HitTarget::NavRow(idx));
        }
    }
    frame.render_widget(Paragraph::new(lines).style(sidebar_style), list_area);
}

fn draw_tab_strip(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let header_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_secondary)
    };
    frame.render_widget(
        Paragraph::new("").style(header_bg),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let unread_inbox = app.unread_inbox_count();
    let mail_label = if unread_inbox > 0 {
        format!("1 Mail ({unread_inbox})")
    } else {
        "1 Mail".to_string()
    };
    let labels = [mail_label, "2 Cal".to_string(), "3 Tasks".to_string()];

    let mut x = area.x;
    for (idx, label) in labels.iter().enumerate() {
        let pill = if idx == 0 {
            format!(" {label} ")
        } else {
            format!("· {label} ")
        };
        let avail = area.right().saturating_sub(x) as usize;
        if avail == 0 {
            break;
        }
        let clipped = truncate(&pill, avail);
        let w = UnicodeWidthStr::width(clipped.as_str()) as u16;
        if w == 0 {
            break;
        }
        let active = ui.active_nav_tab == idx;
        let style = if active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_header)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            }
        } else {
            header_bg
        };
        frame.render_widget(
            Paragraph::new(Span::styled(clipped, style)),
            Rect::new(x, area.y, w, 1),
        );
        hitmap.add_line(x, area.y, w, HitTarget::NavTab(idx));
        x = x.saturating_add(w);
    }
}

fn draw_filter_strip(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let y = area.y.saturating_add(1);
    let row_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_sidebar)
    };
    frame.render_widget(
        Paragraph::new("").style(row_bg),
        Rect::new(area.x, y, area.width, 1),
    );

    let (filter_label, filter_btn, filter_active, new_label, new_btn) = match ui.active_nav_tab {
        TAB_MAIL => (
            if ui.mail_unread_only {
                "[Unread]"
            } else if ui.focused_only {
                "[Focused]"
            } else {
                "[All]"
            },
            ButtonId::FilterToggle,
            ui.mail_unread_only || ui.focused_only,
            "[+New]",
            ButtonId::NewMail,
        ),
        TAB_CALENDAR => (
            "[Today]",
            ButtonId::CalendarToday,
            ui.cal_range == CalendarRangeFilter::Today,
            "[+Event]",
            ButtonId::NewEvent,
        ),
        _ => (
            if ui.todo_hide_completed {
                "[Open]"
            } else {
                "[Tasks]"
            },
            ButtonId::TodoFilterToggle,
            ui.todo_hide_completed,
            "[+Task]",
            ButtonId::NewTodoTask,
        ),
    };

    let filter_w = UnicodeWidthStr::width(filter_label) as u16;
    let new_w = UnicodeWidthStr::width(new_label) as u16;
    let right_w = filter_w + 1 + new_w;

    let left_budget = area.width.saturating_sub(right_w + 1) as usize;
    let jump_label = if left_budget >= 13 {
        " Ctrl+K Jump "
    } else if left_budget >= 8 {
        " Ctrl+K "
    } else {
        ""
    };

    if !jump_label.is_empty() {
        let jw = UnicodeWidthStr::width(jump_label) as u16;
        let jump_style = if theme.no_color {
            theme.muted_style()
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_sidebar)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(jump_label, jump_style)),
            Rect::new(area.x, y, jw, 1),
        );
        hitmap.add_line(area.x, y, jw, HitTarget::Button(ButtonId::CommandPalette));
    }

    if area.width >= right_w {
        let filter_x = area.right().saturating_sub(right_w);
        let new_x = area.right().saturating_sub(new_w);

        let filter_style = if filter_active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_selected)
                    .add_modifier(Modifier::BOLD)
            }
        } else if theme.no_color {
            theme.muted_style()
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_sidebar)
        };

        let new_style = if theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.accent)
                .bg(theme.bg_sidebar)
                .add_modifier(Modifier::BOLD)
        };

        frame.render_widget(
            Paragraph::new(Span::styled(filter_label, filter_style)),
            Rect::new(filter_x, y, filter_w, 1),
        );
        hitmap.add_line(
            filter_x,
            y,
            filter_w,
            HitTarget::Button(filter_btn),
        );

        frame.render_widget(
            Paragraph::new(Span::styled(new_label, new_style)),
            Rect::new(new_x, y, new_w, 1),
        );
        hitmap.add_line(new_x, y, new_w, HitTarget::Button(new_btn));
    }
}

fn draw_mini_calendar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let today = Local::now().date_naive();
    let month_anchor = ui.cal_month_cursor;
    let year = month_anchor.year();
    let month = month_anchor.month();

    let header_title = format!("  {} {year}", month_name(month));
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate(&header_title, area.width as usize),
            theme.accent_style().add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height < 2 {
        return;
    }

    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate("  Mo Tu We Th Fr Sa Su", area.width as usize),
            theme.muted_style(),
        )),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );

    let event_days: HashSet<NaiveDate> = app
        .calendar_events
        .iter()
        .filter(|e| !e.is_cancelled)
        .filter_map(super::calendar_view::event_local_date)
        .collect();

    let first_of_month = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(today);
    let offset_mon = first_of_month.weekday().num_days_from_monday() as i64;
    let grid_start = first_of_month - Duration::days(offset_mon);

    let selected_date = match ui.cal_range {
        CalendarRangeFilter::SpecificDate(d) => Some(d),
        CalendarRangeFilter::Today => Some(today),
        CalendarRangeFilter::Tomorrow => Some(today + Duration::days(1)),
        _ => None,
    };

    for week in 0..6u16 {
        let y = area.y + 2 + week;
        if y >= area.bottom() {
            break;
        }
        let mut spans = vec![Span::raw(" ")];
        for dow in 0..7i64 {
            let day_date = grid_start + Duration::days((week as i64) * 7 + dow);
            let in_month = day_date.month() == month;
            let is_today = day_date == today;
            let is_sel = selected_date == Some(day_date);
            let has_event = event_days.contains(&day_date);

            let num = format!("{:2}", day_date.day());
            let marker = if has_event { "•" } else { " " };
            let cell_text = format!("{num}{marker}");

            let style = if is_sel {
                theme.nav_selection()
            } else if is_today {
                if theme.no_color {
                    Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                }
            } else if !in_month {
                theme.subtle_style()
            } else if has_event {
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg_primary)
            };

            let cell_x = area.x + 1 + (dow as u16) * 3;
            if cell_x + 3 <= area.right() {
                hitmap.add_line(
                    cell_x,
                    y,
                    3,
                    HitTarget::MiniCalDay {
                        year: day_date.year(),
                        month: day_date.month(),
                        day: day_date.day(),
                    },
                );
            }
            spans.push(Span::styled(cell_text, style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

fn month_name(m: u32) -> &'static str {
    match m {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "",
    }
}

fn render_nav_row(
    row: &NavRow,
    width: usize,
    is_selected: bool,
    is_active: bool,
    ui: &UiState,
) -> Line<'static> {
    let theme = &ui.theme;
    match row {
        NavRow::SectionHeader { label } => {
            let text = truncate(&format!(" {label}"), width);
            Line::from(Span::styled(
                text,
                theme.muted_style().add_modifier(Modifier::BOLD),
            ))
        }
        NavRow::MailFolder {
            folder_id,
            display_name,
            unread,
            is_virtual,
            depth,
            has_children,
            expanded,
        } => {
            let indent = if *depth > 0 {
                "  ".repeat(*depth as usize)
            } else {
                String::new()
            };
            let icon = mail_folder_icon(folder_id, display_name, *depth, *has_children, *expanded);
            let badge = if *unread > 0 {
                format!(" {unread}")
            } else {
                String::new()
            };
            let prefix = if is_active { "▎" } else { " " };
            let badge_w = UnicodeWidthStr::width(badge.as_str());
            let indent_w = UnicodeWidthStr::width(indent.as_str());
            let icon_w = UnicodeWidthStr::width(icon);
            let label_max = width.saturating_sub(1 + indent_w + icon_w + badge_w);
            let title = truncate(display_name, label_max);
            let title_w = UnicodeWidthStr::width(title.as_str());
            let pad = width.saturating_sub(1 + indent_w + icon_w + title_w + badge_w);

            let row_style = if is_selected {
                theme.nav_selection()
            } else if is_active {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_selected)
                    .add_modifier(Modifier::BOLD)
            } else if *unread > 0 && !*is_virtual {
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg_secondary)
            };

            let badge_style = if is_selected || is_active {
                row_style
            } else {
                Style::default()
                    .fg(theme.unread)
                    .add_modifier(Modifier::BOLD)
            };

            Line::from(vec![
                Span::styled(
                    format!("{prefix}{indent}{icon}{title}{}", " ".repeat(pad)),
                    row_style,
                ),
                Span::styled(badge, badge_style),
            ])
        }
        NavRow::CalendarRange { label, .. } => {
            let prefix = if is_active { "▎ " } else { "  " };
            let text = truncate(&format!("{prefix}{label}"), width);
            let pad = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
            let style = if is_selected {
                theme.nav_selection()
            } else if is_active {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_selected)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg_secondary)
            };
            Line::from(Span::styled(format!("{text}{}", " ".repeat(pad)), style))
        }
        NavRow::TodoList {
            display_name,
            wellknown,
            ..
        } => {
            let icon = match wellknown.as_deref() {
                Some("defaultList") => "☑ ",
                Some("flaggedEmails") => "⚑ ",
                _ => "☰ ",
            };
            let prefix = if is_active { "▎" } else { " " };
            let text = truncate(&format!("{prefix}{icon}{display_name}"), width);
            let pad = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
            let style = if is_selected {
                theme.nav_selection()
            } else if is_active {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.bg_selected)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg_secondary)
            };
            Line::from(Span::styled(format!("{text}{}", " ".repeat(pad)), style))
        }
        NavRow::ActionButton { label, .. } => {
            let text = truncate(&format!("  {label}"), width);
            let style = if is_selected {
                theme.nav_selection()
            } else {
                theme.accent_style()
            };
            Line::from(Span::styled(text, style))
        }
    }
}

/// Leading glyph of a mail folder row: a disclosure chevron (`▾` expanded / `▸` collapsed) only on
/// folders that have subfolders, otherwise a folder-kind glyph so leaves never look expandable.
fn mail_folder_icon(
    folder_id: &str,
    display_name: &str,
    depth: u8,
    has_children: bool,
    expanded: bool,
) -> &'static str {
    if has_children {
        return if expanded { "▾ " } else { "▸ " };
    }
    if depth > 0 {
        "↳ "
    } else if folder_id.eq_ignore_ascii_case("inbox") || display_name.eq_ignore_ascii_case("Inbox")
    {
        "📥 "
    } else if folder_id == VIRTUAL_UNREAD_FOLDER {
        "● "
    } else if folder_id == VIRTUAL_FLAGGED_FOLDER {
        "⚑ "
    } else if display_name.eq_ignore_ascii_case("Drafts") {
        "✎ "
    } else if display_name.eq_ignore_ascii_case("Sent") {
        "↗ "
    } else if display_name.eq_ignore_ascii_case("Archive") {
        "▣ "
    } else if display_name.eq_ignore_ascii_case("Trash") {
        "⌫ "
    } else {
        "• "
    }
}

fn selection_offset(selected: usize, height: usize, total: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    let half = height / 2;
    selected.saturating_sub(half).min(total - height)
}

fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        graph::GraphClient,
        storage::Storage,
        sync::{FocusTarget, SyncController},
    };
    use tokio::sync::{mpsc, watch};

    fn make_nav_app() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = watch::channel(FocusTarget::default());
        let (cmd_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    fn folder(
        id: &str,
        name: &str,
        wk: Option<&str>,
        parent: Option<&str>,
        unread: i32,
    ) -> MailFolder {
        MailFolder {
            id: id.into(),
            display_name: name.into(),
            well_known_name: wk.map(str::to_string),
            parent_folder_id: parent.map(str::to_string),
            child_folder_count: 0,
            unread_item_count: unread,
            total_item_count: unread + 5,
        }
    }

    #[test]
    fn inbox_subfolders_render_indented_without_duplicating_and_korean_sent_maps() {
        let (mut app, ui) = make_nav_app();
        app.mail_folders = vec![
            folder("f-inbox", "받은 편지함", None, Some("msgfolderroot"), 4),
            folder("f-proj", "Projects", None, Some("f-inbox"), 2),
            folder("f-2026", "2026", None, Some("f-proj"), 1),
            folder("f-sent", "보낸 편지함", None, Some("msgfolderroot"), 0),
            folder("f-drafts", "임시 보관함", None, Some("msgfolderroot"), 0),
            folder("f-arch", "보관함", None, Some("msgfolderroot"), 0),
            folder("f-trash", "지운 편지함", None, Some("msgfolderroot"), 0),
            folder("f-custom", "Clients", None, Some("msgfolderroot"), 3),
            folder("f-custom-child", "Acme", None, Some("f-custom"), 1),
        ];

        let rows = build_mail_nav_rows(&app, &HashSet::new());

        // Find Inbox and verify Projects (depth 1) & 2026 (depth 2) are immediately beneath it.
        let inbox_pos = rows
            .iter()
            .position(|r| matches!(r, NavRow::MailFolder { folder_id, .. } if folder_id == "f-inbox"))
            .expect("Inbox row missing");
        assert_eq!(
            rows[inbox_pos],
            NavRow::MailFolder {
                folder_id: "f-inbox".into(),
                display_name: "Inbox".into(),
                unread: 4,
                is_virtual: false,
                depth: 0,
                has_children: true,
                expanded: true,
            }
        );
        assert_eq!(
            rows[inbox_pos + 1],
            NavRow::MailFolder {
                folder_id: "f-proj".into(),
                display_name: "Projects".into(),
                unread: 2,
                is_virtual: false,
                depth: 1,
                has_children: true,
                expanded: true,
            }
        );
        assert_eq!(
            rows[inbox_pos + 2],
            NavRow::MailFolder {
                folder_id: "f-2026".into(),
                display_name: "2026".into(),
                unread: 1,
                is_virtual: false,
                depth: 2,
                has_children: false,
                expanded: false,
            }
        );

        // Korean "보낸 편지함" maps to Sent in FAVORITES and does not duplicate under FOLDERS.
        assert!(rows.iter().any(|r| matches!(
            r,
            NavRow::MailFolder { folder_id, display_name, depth: 0, .. }
            if folder_id == "f-sent" && display_name == "Sent"
        )));

        let folders_header_pos = rows
            .iter()
            .position(|r| matches!(r, NavRow::SectionHeader { label: "FOLDERS" }))
            .expect("FOLDERS section missing");
        let under_folders: Vec<&NavRow> = rows[folders_header_pos + 1..].iter().collect();
        assert_eq!(under_folders.len(), 2);
        assert!(matches!(
            under_folders[0],
            NavRow::MailFolder { folder_id, depth: 0, .. } if folder_id == "f-custom"
        ));
        assert!(matches!(
            under_folders[1],
            NavRow::MailFolder { folder_id, depth: 1, .. } if folder_id == "f-custom-child"
        ));

        // Verify indented rendering with right-aligned unread count.
        let line = render_nav_row(&rows[inbox_pos + 1], 24, false, false, &ui);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.starts_with("   ▾ Projects"));
        assert!(rendered.ends_with(" 2"));
        assert_eq!(UnicodeWidthStr::width(rendered.as_str()), 24);

        // Verify hierarchical display paths helper.
        let paths = folder_display_paths(&app.mail_folders);
        assert_eq!(paths.get("f-2026").map(String::as_str), Some("받은 편지함 / Projects / 2026"));
    }

    fn row_of<'a>(rows: &'a [NavRow], id: &str) -> Vec<(usize, &'a NavRow)> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, NavRow::MailFolder { folder_id, .. } if folder_id == id))
            .collect()
    }

    fn depth_of(row: &NavRow) -> u8 {
        match row {
            NavRow::MailFolder { depth, .. } => *depth,
            other => panic!("not a folder row: {other:?}"),
        }
    }

    fn rendered_text(row: &NavRow, ui: &UiState) -> String {
        render_nav_row(row, 24, false, false, ui)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    fn well_known_tree() -> Vec<MailFolder> {
        vec![
            folder("f-inbox", "Inbox", Some("inbox"), Some("root"), 3),
            folder("f-proj", "Projects", None, Some("f-inbox"), 1),
            folder("f-drafts", "Drafts", Some("drafts"), Some("root"), 0),
            folder("f-tmpl", "Templates", None, Some("f-drafts"), 0),
            folder("f-sent", "Sent Items", Some("sentitems"), Some("root"), 0),
            folder("f-rcpt", "Receipts", None, Some("f-sent"), 2),
            folder("f-rcpt-26", "2026", None, Some("f-rcpt"), 1),
            folder("f-arch", "Archive", Some("archive"), Some("root"), 0),
            folder("f-old", "Old Projects", None, Some("f-arch"), 0),
            folder("f-trash", "Deleted Items", Some("deleteditems"), Some("root"), 0),
            folder("f-recov", "Recovered", None, Some("f-trash"), 0),
            folder("f-clients", "Clients", None, Some("root"), 0),
            folder("f-acme", "Acme", None, Some("f-clients"), 0),
            folder("f-misc", "Misc", None, Some("root"), 0),
        ]
    }

    #[test]
    fn well_known_and_favorite_subfolders_nest_under_their_parent_not_depth0_folders() {
        let (mut app, _ui) = make_nav_app();
        app.mail_folders = well_known_tree();
        // A custom favorite with a child, and an Inbox subfolder pinned as a favorite.
        app.config.favorite_folders = vec!["f-clients".into(), "f-proj".into()];

        let rows = build_mail_nav_rows(&app, &HashSet::new());

        for (parent, children) in [
            ("f-drafts", vec![("f-tmpl", 1)]),
            ("f-sent", vec![("f-rcpt", 1), ("f-rcpt-26", 2)]),
            ("f-arch", vec![("f-old", 1)]),
            ("f-trash", vec![("f-recov", 1)]),
            ("f-clients", vec![("f-acme", 1)]),
        ] {
            let found = row_of(&rows, parent);
            assert_eq!(found.len(), 1, "{parent} listed once");
            let (pos, parent_row) = found[0];
            assert_eq!(depth_of(parent_row), 0);
            assert!(matches!(
                parent_row,
                NavRow::MailFolder { has_children: true, expanded: true, .. }
            ));
            for (offset, (child, depth)) in children.into_iter().enumerate() {
                let child_rows = row_of(&rows, child);
                assert_eq!(child_rows.len(), 1, "{child} listed exactly once");
                assert_eq!(child_rows[0].0, pos + 1 + offset, "{child} sits right under {parent}");
                assert_eq!(depth_of(child_rows[0].1), depth, "{child} keeps its tree depth");
            }
        }

        // FOLDERS only holds the genuinely custom root; nothing floats there at depth 0.
        let folders_pos = rows
            .iter()
            .position(|r| matches!(r, NavRow::SectionHeader { label: "FOLDERS" }))
            .expect("FOLDERS section");
        let under_folders: Vec<&str> = rows[folders_pos + 1..]
            .iter()
            .filter_map(|r| match r {
                NavRow::MailFolder { folder_id, .. } => Some(folder_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(under_folders, ["f-misc"]);

        // The Inbox subfolder favorite is not swallowed by the Inbox subtree: it is nested under
        // Inbox and also listed as a FAVORITES shortcut.
        let proj = row_of(&rows[..folders_pos], "f-proj");
        assert_eq!(proj.len(), 2);
        assert_eq!(depth_of(proj[0].1), 1, "nested under Inbox");
        assert_eq!(depth_of(proj[1].1), 0, "favorite shortcut row");
    }

    #[test]
    fn folder_chevrons_only_on_parents_and_left_right_space_collapse_expand() {
        use crate::ui::action::dispatch;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let (mut app, mut ui) = make_nav_app();
        app.mail_folders = well_known_tree();
        ui.active_nav_tab = TAB_MAIL;
        ui.set_focus(Focus::Nav);

        let press = |app: &mut AppState, ui: &mut UiState, code: KeyCode| {
            let action = ui
                .keymap
                .resolve_for_tab(Focus::Nav, TAB_MAIL, KeyEvent::new(code, KeyModifiers::NONE))
                .expect("bound in the folder pane");
            dispatch(action, app, ui);
        };
        let select = |app: &AppState, ui: &mut UiState, id: &str| {
            let rows = build_nav_rows(app, ui);
            ui.nav_selected = row_of(&rows, id)[0].0;
        };
        let sent_row = |app: &AppState, ui: &UiState| {
            let rows = build_nav_rows(app, ui);
            row_of(&rows, "f-sent")[0].1.clone()
        };
        let receipts_visible = |app: &AppState, ui: &UiState| {
            !row_of(&build_nav_rows(app, ui), "f-rcpt").is_empty()
        };

        // Leaves never get a chevron; parents show an open one.
        let rows = build_nav_rows(&app, &ui);
        for leaf in ["f-tmpl", "f-rcpt-26", "f-misc", "f-acme"] {
            let text = rendered_text(row_of(&rows, leaf)[0].1, &ui);
            assert!(!text.contains('▸') && !text.contains('▾'), "leaf {leaf}: {text:?}");
        }
        assert!(rendered_text(row_of(&rows, "f-misc")[0].1, &ui).starts_with(" • Misc"));
        assert!(rendered_text(row_of(&rows, "f-tmpl")[0].1, &ui).starts_with("   ↳ Templates"));
        assert!(rendered_text(&sent_row(&app, &ui), &ui).starts_with(" ▾ Sent"));

        // Left collapses the selected parent: its subtree disappears and does not float to FOLDERS.
        select(&app, &mut ui, "f-sent");
        press(&mut app, &mut ui, KeyCode::Left);
        assert!(ui.collapsed_folder_ids.contains("f-sent"));
        assert!(!receipts_visible(&app, &ui));
        assert!(row_of(&build_nav_rows(&app, &ui), "f-rcpt-26").is_empty());
        assert!(rendered_text(&sent_row(&app, &ui), &ui).starts_with(" ▸ Sent"));
        assert_eq!(ui.focus(), Focus::Nav);

        // Right expands a collapsed parent without leaving the folder pane...
        press(&mut app, &mut ui, KeyCode::Right);
        assert!(receipts_visible(&app, &ui));
        assert!(rendered_text(&sent_row(&app, &ui), &ui).starts_with(" ▾ Sent"));
        assert_eq!(ui.focus(), Focus::Nav);

        // ...and Space toggles it both ways.
        press(&mut app, &mut ui, KeyCode::Char(' '));
        assert!(!receipts_visible(&app, &ui));
        press(&mut app, &mut ui, KeyCode::Char(' '));
        assert!(receipts_visible(&app, &ui));

        // Left on a leaf jumps to its parent; Left again collapses that parent, then climbs to Sent.
        select(&app, &mut ui, "f-rcpt-26");
        press(&mut app, &mut ui, KeyCode::Char('h'));
        assert_eq!(ui.nav_selected, row_of(&build_nav_rows(&app, &ui), "f-rcpt")[0].0);
        press(&mut app, &mut ui, KeyCode::Char('h'));
        assert!(ui.collapsed_folder_ids.contains("f-rcpt"));
        let rows = build_nav_rows(&app, &ui);
        assert!(row_of(&rows, "f-rcpt-26").is_empty());
        assert!(rendered_text(row_of(&rows, "f-rcpt")[0].1, &ui).starts_with("   ▸ Receipts"));
        press(&mut app, &mut ui, KeyCode::Char('h'));
        assert_eq!(ui.nav_selected, row_of(&build_nav_rows(&app, &ui), "f-sent")[0].0);

        // Space on a leaf changes nothing; Right on a leaf (or expanded parent) opens it in the list.
        select(&app, &mut ui, "f-misc");
        let before = ui.collapsed_folder_ids.clone();
        press(&mut app, &mut ui, KeyCode::Char(' '));
        assert_eq!(ui.collapsed_folder_ids, before);
        press(&mut app, &mut ui, KeyCode::Char('l'));
        assert_eq!(ui.focus(), Focus::List);
        assert_eq!(app.active_folder_id, "f-misc");
    }
}
