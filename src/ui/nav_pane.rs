//! Unified Left Sidebar (`ui/nav_pane.rs`):
//! - Top tab header: `1 Mail · 2 Cal · 3 Tasks` (clickable + `1`/`2`/`3` & `Alt+1..3`)
//! - Action strip: `[Ctrl+K Jump]` + `[Filter]` + `[+New]`
//! - Tab 0 (`Mail`): `FAVORITES` (Inbox, Unread, Flagged, Drafts, Sent, Archive, Trash + user favorites) + `FOLDERS`
//! - Tab 1 (`Calendar`): Compact Mini Month Calendar Grid (today + `•` event days) + Quick Range Jumpers
//! - Tab 2 (`Tasks`): Microsoft To Do lists + `n` to create list

use std::collections::HashSet;

use chrono::{Datelike, Duration, Local, NaiveDate};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::{
    app::{AppState, CalendarRangeFilter, Focus, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
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
        TAB_MAIL => build_mail_nav_rows(app),
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

fn find_folder_by_wellknown<'a>(
    app: &'a AppState,
    well_known: &str,
    fallback_names: &[&str],
) -> Option<&'a crate::models::MailFolder> {
    app.mail_folders
        .iter()
        .find(|f| {
            f.well_known_name
                .as_deref()
                .is_some_and(|wk| wk.eq_ignore_ascii_case(well_known))
        })
        .or_else(|| {
            app.mail_folders.iter().find(|f| {
                fallback_names
                    .iter()
                    .any(|n| f.display_name.eq_ignore_ascii_case(n))
            })
        })
}

fn build_mail_nav_rows(app: &AppState) -> Vec<NavRow> {
    let mut rows = Vec::new();
    let mut shown_ids: HashSet<String> = HashSet::new();

    rows.push(NavRow::SectionHeader { label: "FAVORITES" });

    // 1. Inbox
    if let Some(inbox) = find_folder_by_wellknown(app, "inbox", &["Inbox"]) {
        shown_ids.insert(inbox.id.clone());
        rows.push(NavRow::MailFolder {
            folder_id: inbox.id.clone(),
            display_name: "Inbox".to_string(),
            unread: inbox.unread_item_count,
            is_virtual: false,
        });
    } else {
        shown_ids.insert("inbox".to_string());
        rows.push(NavRow::MailFolder {
            folder_id: "inbox".to_string(),
            display_name: "Inbox".to_string(),
            unread: app.unread_inbox_count(),
            is_virtual: false,
        });
    }

    // 2. Virtual smart views
    let unread_total = app
        .mail_messages
        .iter()
        .filter(|m| !m.is_read)
        .count() as i32;
    rows.push(NavRow::MailFolder {
        folder_id: VIRTUAL_UNREAD_FOLDER.to_string(),
        display_name: "Unread".to_string(),
        unread: unread_total,
        is_virtual: true,
    });
    let flagged_total = app
        .mail_messages
        .iter()
        .filter(|m| m.flagged)
        .count() as i32;
    rows.push(NavRow::MailFolder {
        folder_id: VIRTUAL_FLAGGED_FOLDER.to_string(),
        display_name: "Flagged".to_string(),
        unread: flagged_total,
        is_virtual: true,
    });

    // 3. Well-known folders
    let well_known_specs: [(&str, &str, &[&str]); 4] = [
        ("drafts", "Drafts", &["Drafts"]),
        ("sentitems", "Sent", &["Sent Items", "Sent"]),
        ("archive", "Archive", &["Archive"]),
        ("deleteditems", "Trash", &["Deleted Items", "Trash"]),
    ];
    for (wk_id, label, aliases) in well_known_specs {
        if let Some(folder) = find_folder_by_wellknown(app, wk_id, aliases) {
            shown_ids.insert(folder.id.clone());
            rows.push(NavRow::MailFolder {
                folder_id: folder.id.clone(),
                display_name: label.to_string(),
                unread: folder.unread_item_count,
                is_virtual: false,
            });
        } else {
            shown_ids.insert(wk_id.to_string());
            rows.push(NavRow::MailFolder {
                folder_id: wk_id.to_string(),
                display_name: label.to_string(),
                unread: 0,
                is_virtual: false,
            });
        }
    }

    // 4. User favorite_folders from config
    for fav_id in &app.config.favorite_folders {
        if let Some(folder) = app.mail_folders.iter().find(|f| &f.id == fav_id) {
            if shown_ids.insert(folder.id.clone()) {
                rows.push(NavRow::MailFolder {
                    folder_id: folder.id.clone(),
                    display_name: folder.display_name.clone(),
                    unread: folder.unread_item_count,
                    is_virtual: false,
                });
            }
        }
    }

    // 5. Custom folders
    let mut custom: Vec<&crate::models::MailFolder> = app
        .mail_folders
        .iter()
        .filter(|f| !shown_ids.contains(&f.id))
        .collect();
    custom.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
    });
    if !custom.is_empty() {
        rows.push(NavRow::SectionHeader { label: "FOLDERS" });
        for f in custom {
            rows.push(NavRow::MailFolder {
                folder_id: f.id.clone(),
                display_name: f.display_name.clone(),
                unread: f.unread_item_count,
                is_virtual: false,
            });
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

    let (filter_label, new_label, new_btn) = match ui.active_nav_tab {
        TAB_MAIL => (
            if ui.mail_unread_only {
                "[Unread]"
            } else if ui.focused_only {
                "[Focused]"
            } else {
                "[All]"
            },
            "[+New]",
            ButtonId::NewMail,
        ),
        TAB_CALENDAR => ("[Today]", "[+Event]", ButtonId::NewEvent),
        _ => ("[Tasks]", "[+Task]", ButtonId::NewTodoTask),
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

        let filter_active =
            ui.active_nav_tab == TAB_MAIL && (ui.mail_unread_only || ui.focused_only);
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
            HitTarget::Button(ButtonId::FilterToggle),
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
        .filter_map(|e| parse_event_local_date(&e.start_iso))
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

fn parse_event_local_date(iso: &str) -> Option<NaiveDate> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return Some(dt.with_timezone(&Local).date_naive());
    }
    NaiveDate::parse_from_str(iso.get(..10)?, "%Y-%m-%d").ok()
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
        } => {
            let icon = if folder_id.eq_ignore_ascii_case("inbox") {
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
                "▸ "
            };
            let badge = if *unread > 0 {
                format!(" {unread}")
            } else {
                String::new()
            };
            let prefix = if is_active { "▎" } else { " " };
            let badge_w = UnicodeWidthStr::width(badge.as_str());
            let icon_w = UnicodeWidthStr::width(icon);
            let label_max = width.saturating_sub(1 + icon_w + badge_w + 1);
            let title = truncate(display_name, label_max);
            let title_w = UnicodeWidthStr::width(title.as_str());
            let pad = width.saturating_sub(1 + icon_w + title_w + badge_w);

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
                Span::styled(format!("{prefix}{icon}{title}{}", " ".repeat(pad)), row_style),
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
