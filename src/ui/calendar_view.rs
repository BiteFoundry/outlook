//! Tab 2 (`Calendar`):
//! - Middle pane: Agenda timeline grouped by day (`TODAY · Thu Nov 19`, `TOMORROW · Fri Nov 20`, ...)
//!   with time range, title, response badge (`✓` / `?` / `✗` / `•`), conflict warning `⚠`, and `🎥 Online`.
//! - Right pane: Full event time/timezone, location, organizer, attendee RSVP list, description,
//!   and 1-key actions (`n` New Event, `a` Accept, `y` Tentative, `x` Decline, `o` Join, `Delete`).

use chrono::{DateTime, Duration, Local, NaiveDate};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::CalendarEvent,
    parser,
    ui::{
        app::{AppState, CalendarRangeFilter, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        mail_view::{render_scrolled_lines, truncate, RenderedLine},
    },
};

pub fn draw_list(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if ui.theme.bg_canvas != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(ui.theme.bg_canvas)),
            area,
        );
    }

    draw_agenda_header(frame, area, ui, hitmap);

    let list_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.cal_list_scroll.area = list_area;
    if list_area.height == 0 {
        return;
    }

    let events = app.visible_calendar_events(ui);
    let theme = &ui.theme;
    if events.is_empty() {
        ui.cal_list_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate(" No events in selected range (n to create)", list_area.width as usize),
                theme.muted_style(),
            )),
            list_area,
        );
        return;
    }

    let selected_idx = ui
        .cal_list_scroll
        .selected_idx
        .unwrap_or(0)
        .min(events.len().saturating_sub(1));
    ui.cal_list_scroll.selected_idx = Some(selected_idx);

    let conflicts = detect_conflicts(&events);
    let list_focused = ui.focus() == Focus::List;
    let width = list_area.width as usize;

    let mut lines: Vec<RenderedLine> = Vec::new();
    let mut item_line_starts: Vec<usize> = Vec::with_capacity(events.len());
    let mut last_day: Option<NaiveDate> = None;

    for (idx, ev) in events.iter().enumerate() {
        let day = event_local_date(ev);
        if day != last_day {
            if !lines.is_empty() {
                lines.push(RenderedLine::plain(Line::raw("")));
            }
            let day_label = format_day_group_header(day, &ev.start_iso);
            lines.push(RenderedLine::plain(Line::from(Span::styled(
                truncate(&format!(" {day_label}"), width),
                theme.muted_style().add_modifier(Modifier::BOLD),
            ))));
            last_day = day;
        }

        item_line_starts.push(lines.len());
        let is_selected = idx == selected_idx;
        let is_active = app.active_event_id.as_deref() == Some(ev.id.as_str());
        let bg = if is_selected && list_focused {
            theme.selection()
        } else if is_selected || is_active {
            if theme.no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(theme.bg_elevated)
            }
        } else {
            Style::default()
        };

        let gutter = if is_selected && list_focused { "▎" } else { " " };
        let (rsvp_glyph, rsvp_color) = rsvp_badge(&ev.response_status, theme);
        let time_str = if ev.is_all_day {
            "All Day".to_string()
        } else {
            format!(
                "{}–{}",
                format_hhmm(&ev.start_iso),
                format_hhmm(&ev.end_iso)
            )
        };
        let mut suffix = String::new();
        if conflicts[idx] {
            suffix.push_str(" ⚠");
        }
        if ev.is_online_meeting {
            suffix.push_str(" 🎥");
        }
        let suffix_w = UnicodeWidthStr::width(suffix.as_str());
        let prefix_w = 2 + 2 + time_str.len() + 1;
        let title_max = width.saturating_sub(prefix_w + suffix_w + 1);
        let title = if ev.is_cancelled {
            truncate(&format!("[Cancelled] {}", ev.subject), title_max)
        } else {
            truncate(&ev.subject, title_max)
        };
        let used = prefix_w + UnicodeWidthStr::width(title.as_str()) + suffix_w;
        let pad = width.saturating_sub(used + 1);

        let title_style = if ev.is_cancelled {
            bg.fg(theme.fg_muted).add_modifier(Modifier::CROSSED_OUT)
        } else {
            bg.fg(theme.fg_primary)
        };
        let line = Line::from(vec![
            Span::styled(gutter, bg.fg(theme.accent)),
            Span::styled(format!("{rsvp_glyph} "), bg.fg(rsvp_color)),
            Span::styled(format!("{time_str} "), bg.fg(theme.fg_muted)),
            Span::styled(title, title_style),
            Span::styled(" ".repeat(pad), bg),
            Span::styled(format!("{suffix} "), bg.fg(if conflicts[idx] { theme.warning } else { theme.accent_soft })),
        ]);
        lines.push(RenderedLine::clickable(line, HitTarget::ListItem(idx)));
    }

    let height = list_area.height as usize;
    ui.cal_list_scroll.total_lines = lines.len();
    let sel_line = item_line_starts.get(selected_idx).copied().unwrap_or(0);
    let mut off = ui.cal_list_scroll.offset.min(lines.len().saturating_sub(height));
    if sel_line < off {
        off = sel_line.saturating_sub(1);
    } else if sel_line >= off + height {
        off = sel_line + 1 - height;
    }
    ui.cal_list_scroll.offset = off;

    render_scrolled_lines(frame, list_area, &lines, off, height, hitmap);
}

fn draw_agenda_header(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let bar_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_primary)
    };
    frame.render_widget(
        Paragraph::new("").style(bar_style),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let range_label = match ui.cal_range {
        CalendarRangeFilter::Today => "Agenda · Today",
        CalendarRangeFilter::Tomorrow => "Agenda · Tomorrow",
        CalendarRangeFilter::Next7Days => "Agenda · Next 7 Days",
        CalendarRangeFilter::ThisMonth => "Agenda · This Month",
        CalendarRangeFilter::AllUpcoming => "Agenda · Upcoming",
        CalendarRangeFilter::SpecificDate(d) => {
            let s = d.format("%b %d").to_string();
            let full = format!(" Agenda · {s}");
            let btn = "[+ New Event]";
            let bw = UnicodeWidthStr::width(btn) as u16;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate(&full, area.width.saturating_sub(bw + 2) as usize),
                    bar_style.add_modifier(Modifier::BOLD),
                )),
                Rect::new(area.x, area.y, area.width.saturating_sub(bw + 1), 1),
            );
            if area.width >= bw + 1 {
                let r = Rect::new(area.right().saturating_sub(bw + 1), area.y, bw, 1);
                frame.render_widget(
                    Paragraph::new(Span::styled(btn, bar_style.fg(theme.accent))),
                    r,
                );
                hitmap.add(r, HitTarget::Button(ButtonId::NewEvent));
            }
            return;
        }
    };

    let btn = "[+ New Event]";
    let bw = UnicodeWidthStr::width(btn) as u16;
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate(
                &format!(" {range_label}"),
                area.width.saturating_sub(bw + 2) as usize,
            ),
            bar_style.add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x, area.y, area.width.saturating_sub(bw + 1), 1),
    );
    if area.width >= bw + 1 {
        let r = Rect::new(area.right().saturating_sub(bw + 1), area.y, bw, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(btn, bar_style.fg(theme.accent))),
            r,
        );
        hitmap.add(r, HitTarget::Button(ButtonId::NewEvent));
    }
}

pub fn draw_detail(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if ui.theme.bg_canvas != Color::Reset {
        frame.render_widget(
            Block::default().style(Style::default().bg(ui.theme.bg_canvas)),
            area,
        );
    }

    draw_detail_toolbar(frame, area, app, ui, hitmap);

    let body_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    ui.cal_detail_scroll.area = body_area;
    if body_area.height == 0 {
        return;
    }

    let Some(ev) = app.active_event() else {
        ui.cal_detail_scroll.total_lines = 0;
        frame.render_widget(
            Paragraph::new(" Select a calendar event (n: new event · a/y/x: RSVP)")
                .style(ui.theme.muted_style()),
            body_area,
        );
        return;
    };

    let width = body_area.width as usize;
    let lines = build_event_detail_lines(ev, &ui.theme, width);
    let height = body_area.height as usize;
    ui.cal_detail_scroll.total_lines = lines.len();
    let max_off = lines.len().saturating_sub(height);
    ui.cal_detail_scroll.offset = ui.cal_detail_scroll.offset.min(max_off);

    render_scrolled_lines(
        frame,
        body_area,
        &lines,
        ui.cal_detail_scroll.offset,
        height,
        hitmap,
    );
}

fn draw_detail_toolbar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bar_style = if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_primary)
    };
    frame.render_widget(
        Paragraph::new("").style(bar_style),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let has_join = app
        .active_event()
        .and_then(|e| e.online_meeting_url.as_ref())
        .is_some();
    let mut buttons = vec![
        ("[n New]", ButtonId::NewEvent),
        ("[a Accept]", ButtonId::RsvpAccept),
        ("[y Tentative]", ButtonId::RsvpTentative),
        ("[x Decline]", ButtonId::RsvpDecline),
    ];
    if has_join {
        buttons.push(("[o Join Online]", ButtonId::JoinMeeting));
    }
    buttons.push(("[Del]", ButtonId::DeleteEvent));

    let mut x = area.x + 1;
    for (label, btn_id) in buttons {
        let w = UnicodeWidthStr::width(label) as u16;
        if x.saturating_add(w) > area.right() {
            break;
        }
        let st = if ui.focus() == Focus::Reader {
            bar_style.fg(theme.accent)
        } else {
            bar_style.fg(theme.fg_secondary)
        };
        let r = Rect::new(x, area.y, w, 1);
        frame.render_widget(Paragraph::new(Span::styled(label, st)), r);
        hitmap.add(r, HitTarget::Button(btn_id));
        x = x.saturating_add(w + 1);
    }
}

pub fn build_event_detail_lines(
    ev: &CalendarEvent,
    theme: &crate::ui::theme::Theme,
    width: usize,
) -> Vec<RenderedLine> {
    let mut out = Vec::new();
    let subj = if ev.is_cancelled {
        format!("[Cancelled] {}", ev.subject)
    } else {
        ev.subject.clone()
    };
    out.push(RenderedLine::plain(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            truncate(&subj, width.saturating_sub(4)),
            theme.subject_style().add_modifier(Modifier::BOLD),
        ),
    ])));

    let when = if ev.is_all_day {
        format!("  When:      {} (All Day · {})", format_date_only(ev), ev.time_zone)
    } else {
        // Hours are shown in the machine's zone, so label them with its offset, not the event's storage zone.
        format!(
            "  When:      {} {} – {} ({})",
            format_date_only(ev),
            format_hhmm(&ev.start_iso),
            format_hhmm(&ev.end_iso),
            Local::now().format("%:z")
        )
    };
    out.push(RenderedLine::plain(Line::from(Span::styled(
        truncate(&when, width),
        Style::default().fg(theme.fg_primary),
    ))));

    if let Some(ref loc) = ev.location.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            truncate(&format!("  Location:  {loc}"), width),
            theme.muted_style(),
        ))));
    }

    if let Some(org) = ev
        .organizer_name
        .as_deref()
        .or(ev.organizer_email.as_deref())
    {
        let org_full = match (&ev.organizer_name, &ev.organizer_email) {
            (Some(n), Some(e)) if !n.eq_ignore_ascii_case(e) => format!("{n} <{e}>"),
            _ => org.to_string(),
        };
        out.push(RenderedLine::plain(Line::from(Span::styled(
            truncate(&format!("  Organizer: {org_full}"), width),
            theme.muted_style(),
        ))));
    }

    let (glyph, color) = rsvp_badge(&ev.response_status, theme);
    out.push(RenderedLine::plain(Line::from(vec![
        Span::styled("  Your RSVP: ", theme.muted_style()),
        Span::styled(
            format!("{glyph} {}", ev.response_status),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])));

    if let Some(ref join_url) = ev.online_meeting_url {
        let label = format!("  🎥 Join Online Meeting (o): {join_url}");
        out.push(RenderedLine::clickable(
            Line::from(Span::styled(
                truncate(&label, width),
                theme.accent_style().add_modifier(Modifier::UNDERLINED),
            )),
            HitTarget::JoinMeetingUrl,
        ));
    }

    if !ev.attendees.is_empty() {
        out.push(RenderedLine::plain(Line::raw("")));
        out.push(RenderedLine::plain(Line::from(Span::styled(
            format!("  Attendees ({}):", ev.attendees.len()),
            theme.muted_style().add_modifier(Modifier::BOLD),
        ))));
        for att in &ev.attendees {
            let (ag, ac) = rsvp_badge(&att.response_status, theme);
            let name = att.name.as_deref().unwrap_or(&att.address);
            let text = if name.eq_ignore_ascii_case(&att.address) {
                format!("{name} ({})", att.attendee_type)
            } else {
                format!("{name} <{}> ({})", att.address, att.attendee_type)
            };
            out.push(RenderedLine::plain(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{ag} "), Style::default().fg(ac)),
                Span::styled(truncate(&text, width.saturating_sub(8)), Style::default().fg(theme.fg_secondary)),
            ])));
        }
    }

    out.push(RenderedLine::plain(Line::from(Span::styled(
        format!("  {}", "─".repeat(width.saturating_sub(4).max(4))),
        theme.subtle_style(),
    ))));

    let plain = if !ev.body_plain.trim().is_empty() {
        ev.body_plain.clone()
    } else if !ev.body_html.trim().is_empty() {
        parser::html_to_plain(&ev.body_html, &[])
    } else {
        String::new()
    };
    if plain.trim().is_empty() {
        out.push(RenderedLine::plain(Line::from(Span::styled(
            "  (No description)",
            theme.muted_style(),
        ))));
    } else {
        for line_str in plain.lines() {
            out.push(RenderedLine::plain(Line::from(vec![
                Span::raw("  "),
                Span::raw(truncate(line_str, width.saturating_sub(4))),
            ])));
        }
    }

    out
}

fn detect_conflicts(events: &[CalendarEvent]) -> Vec<bool> {
    let mut flags = vec![false; events.len()];
    let parsed: Vec<Option<(DateTime<chrono::FixedOffset>, DateTime<chrono::FixedOffset>)>> =
        events
            .iter()
            .map(|e| {
                if e.is_cancelled || e.is_all_day {
                    return None;
                }
                let s = DateTime::parse_from_rfc3339(&e.start_iso).ok()?;
                let end = DateTime::parse_from_rfc3339(&e.end_iso).ok()?;
                Some((s, end))
            })
            .collect();

    for i in 0..events.len() {
        let Some((si, ei)) = parsed[i] else { continue };
        for j in (i + 1)..events.len() {
            let Some((sj, ej)) = parsed[j] else { continue };
            if si < ej && sj < ei {
                flags[i] = true;
                flags[j] = true;
            }
        }
    }
    flags
}

fn rsvp_badge(status: &str, theme: &crate::ui::theme::Theme) -> (&'static str, Color) {
    match status.trim().to_ascii_lowercase().as_str() {
        "accepted" | "organizer" => ("✓", theme.success),
        "tentativelyaccepted" | "tentative" => ("?", theme.warning),
        "declined" => ("✗", theme.error),
        _ => ("•", theme.accent),
    }
}

/// Calendar date an event starts on. All-day events are floating dates: only the date part is read,
/// with no time-zone conversion, so they stay on the same day in every UTC offset.
pub fn event_local_date(ev: &CalendarEvent) -> Option<NaiveDate> {
    let iso = ev.start_iso.as_str();
    if !ev.is_all_day {
        if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
            return Some(dt.with_timezone(&Local).date_naive());
        }
    }
    NaiveDate::parse_from_str(iso.get(..10)?, "%Y-%m-%d").ok()
}

fn format_day_group_header(day: Option<NaiveDate>, fallback_iso: &str) -> String {
    let today = Local::now().date_naive();
    if let Some(d) = day {
        if d == today {
            return format!("TODAY · {}", d.format("%a %b %d"));
        }
        if d == today + Duration::days(1) {
            return format!("TOMORROW · {}", d.format("%a %b %d"));
        }
        return d.format("%a · %b %d, %Y").to_string().to_uppercase();
    }
    fallback_iso.get(..10).unwrap_or("SCHEDULE").to_string()
}

fn format_hhmm(iso: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return dt.with_timezone(&Local).format("%H:%M").to_string();
    }
    iso.get(11..16).unwrap_or("--:--").to_string()
}

fn format_date_only(ev: &CalendarEvent) -> String {
    match event_local_date(ev) {
        Some(date) => date.format("%a, %b %d %Y").to_string(),
        None => ev.start_iso.get(..10).unwrap_or(&ev.start_iso).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn event(id: &str, start: &str, end: &str, zone: &str, all_day: bool) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            subject: id.into(),
            start_iso: crate::graph::calendar::normalize_graph_event_time(start, zone, all_day),
            end_iso: crate::graph::calendar::normalize_graph_event_time(end, zone, all_day),
            time_zone: zone.into(),
            is_all_day: all_day,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: Vec::new(),
            response_status: "none".into(),
            is_online_meeting: false,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        }
    }

    #[test]
    fn conflicts_are_detected_for_offsetless_graph_utc_times() {
        let events = [
            event("a", "2026-11-23T05:00:00.0000000", "2026-11-23T06:00:00.0000000", "UTC", false),
            event("b", "2026-11-23T05:30:00.0000000", "2026-11-23T06:30:00.0000000", "UTC", false),
            event("c", "2026-11-23T07:00:00.0000000", "2026-11-23T08:00:00.0000000", "UTC", false),
            event("d", "2026-11-23T00:00:00.0000000", "2026-11-24T00:00:00.0000000", "UTC", true),
        ];
        assert_eq!(detect_conflicts(&events), vec![true, true, false, false]);
    }

    #[test]
    fn timed_events_render_in_the_machine_zone() {
        let instant = Utc.with_ymd_and_hms(2026, 11, 23, 5, 0, 0).unwrap();
        assert_eq!(
            format_hhmm("2026-11-23T05:00:00Z"),
            instant.with_timezone(&Local).format("%H:%M").to_string()
        );

        let late = event("late", "2026-11-23T23:30:00.0000000", "2026-11-24T00:30:00.0000000", "UTC", false);
        let expected = Utc
            .with_ymd_and_hms(2026, 11, 23, 23, 30, 0)
            .unwrap()
            .with_timezone(&Local)
            .date_naive();
        assert_eq!(event_local_date(&late), Some(expected));
        assert_eq!(format_date_only(&late), expected.format("%a, %b %d %Y").to_string());
    }

    #[test]
    fn all_day_events_keep_their_floating_date_in_every_offset() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let floating = event("x", "2026-11-23T00:00:00.0000000", "2026-11-24T00:00:00.0000000", "UTC", true);
        assert_eq!(event_local_date(&floating), Some(day));
        assert_eq!(format_date_only(&floating), "Mon, Nov 23 2026");

        let mut with_z = floating.clone();
        with_z.start_iso = "2026-11-23T00:00:00Z".into();
        assert_eq!(event_local_date(&with_z), Some(day));
        let mut negative_offset = floating;
        negative_offset.start_iso = "2026-11-23T00:00:00-08:00".into();
        assert_eq!(event_local_date(&negative_offset), Some(day));
    }
}
