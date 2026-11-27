//! Central `Action` enum and single `dispatch(action, app, ui) -> bool` reducer.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::NaiveDate;

use crate::img_preview;
use crate::models::{EventRsvpAction, OutgoingEvent};
use crate::parser;
use crate::sync::SyncCommand;

use super::app::{AppState, CalendarRangeFilter, Focus, NarrowPane, UiMsg, UiState};
use super::composer;
use super::hitmap::{ButtonId, HitTarget};
use super::mail_view::retain_latest_reader_layout;
use super::nav_pane::{NavRow, TAB_CALENDAR, TAB_COUNT, TAB_MAIL, TAB_TASKS};
use super::overlays::context_menu::ContextTarget;
use super::overlays::html_preview::{self, HtmlPreviewOverlay};
use super::overlays::image_preview::ImagePreviewOverlay;
use super::overlays::links::LinkExtractorOverlay;
use super::overlays::mail_compose::{MailComposeMode, MailComposeOverlay};
use super::overlays::quick_input::QuickInputOverlay;
use super::overlays::search::SearchOverlay;
use super::overlays::{Overlay, OverlayKind, OverlayOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    QuitConfirm,
    Noop,

    // Focus & layout
    Focus(Focus),
    NextFocus,
    PrevFocus,
    ToggleNavRail,
    SelectNarrowPane(NarrowPane),
    ToggleNotify,
    ToggleDensity,
    CycleTheme,
    ShowDownloadDir,

    // Navigation & tabs
    SelectNavTab(usize),
    SelectNavRow(usize),
    SelectCalendarDate(NaiveDate),
    /// `←`/`h` in the folder pane: collapse the selected folder, else select its parent.
    NavCollapseFolder,
    /// `→`/`l` in the folder pane: expand the selected collapsed folder, else open the row.
    NavExpandFolder,
    /// `Space` in the folder pane: toggle the selected folder's subfolders.
    NavToggleFolder,

    // Movement, scrolling & context
    MoveUp,
    MoveDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    MouseScroll {
        delta: i32,
        column: u16,
        row: u16,
    },
    Activate,
    SpaceAction,
    ContextNew,

    // Mail actions
    SelectMailFolder(String),
    OpenMailCompose,
    StartNewMailCompose,
    ComposeTo(String),
    MailReply {
        reply_all: bool,
    },
    MailForward,
    MailArchive,
    DeleteItem,
    MailToggleRead,
    MailToggleFlag,
    OpenFolderPicker,
    MoveMailToFolder {
        message_ids: Vec<String>,
        destination_folder_id: String,
        destination_name: String,
    },
    ToggleGroupThreads,
    /// Expand (or, when all are open, collapse) the older messages of the active grouped thread.
    ToggleThreadMessages,
    ToggleFocusedOnly,
    ToggleUnreadFilter,
    ToggleQuotedHistory,
    DownloadLatestAttachment,
    DownloadMailAttachment {
        msg_id: String,
        attach_index: usize,
    },
    OpenActiveLink,
    OpenLinkExtractor,
    ToggleShowLinkUrls,
    OpenUrl(String),
    CopyUrlToClipboard(String),
    LoadOlderMail,

    // Mail compose & overlay text editing
    SubmitMailCompose,
    SaveDraft,
    SaveMailDraft,
    StashMailDraft(Box<MailComposeOverlay>),
    OpenAttachPicker,
    OpenFilePicker,
    AttachComposeFile(PathBuf),
    OpenExternalEditor,
    PasteClipboard,
    ClearPendingImages,
    Input(char),
    Paste(String),
    Backspace,
    DeleteWordBackward,
    DeleteForward,
    InsertNewline,
    Tab,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorHome,
    CursorEnd,
    SelectAll,
    Undo,
    Redo,

    // Calendar actions
    OpenEventForm,
    SelectCalendarToday,
    CreateCalendarEvent(OutgoingEvent),
    DeleteCalendarEvent,
    CalendarRsvp(EventRsvpAction),
    JumpToCalendarEvent(String),

    // Todo actions
    OpenNewTodoList,
    OpenNewTodoTask,
    ToggleTodoHideCompleted,
    CreateTodoList {
        display_name: String,
    },
    CreateTodoTask {
        list_id: String,
        title: String,
    },
    ToggleTodoCompleted,
    DeleteTodoTask,

    // Search & sync
    OpenSearch,
    OpenSearchWithQuery(String),
    SearchLocalMail(String),
    SearchRemoteMail(String),
    JumpToSearchHit(usize),
    RefreshAll,

    // Overlays & mouse
    OpenOverlay(OverlayKind),
    CloseOverlay,
    OverlaySubmit,
    OpenContextMenuAt {
        column: u16,
        row: u16,
    },
    Click {
        column: u16,
        row: u16,
    },
    ShowToast(String),

    // Inline images, HTML preview & updater
    ToggleInlineImages,
    OpenImagePreview,
    OpenMailImagePreview {
        msg_id: String,
        attach_index: Option<usize>,
    },
    LoadPreviewImage,
    CycleImageProtocol,
    DownloadPreviewImage,
    OpenHtmlPreview,
    OpenMailHtmlAttachment {
        msg_id: String,
        attach_index: usize,
    },
    ServeHtmlPreview,
    CheckForUpdates,
    ApplySelfUpdate,
}

/// Dispatches an `Action`, mutating `app` and `ui`.
/// Returns `true` when the application should exit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if matches!(action, Action::Quit) {
        return quit_with_draft(app, ui);
    }

    if let Action::Click { column, row } = action {
        return handle_click(column, row, app, ui);
    }

    if let Action::OpenContextMenuAt { column, row } = action {
        if ui.overlays.is_empty() {
            open_context_menu_at(column, row, app, ui);
        } else if ui.overlays.len() == 1
            && matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_)))
        {
            ui.overlays.pop();
            open_context_menu_at(column, row, app, ui);
        }
        return false;
    }

    // Top overlay handles actions first.
    if let Some(mut top) = ui.overlays.pop() {
        let outcome = top.handle(&action, app, ui);
        if let Overlay::MailCompose(ref mut compose) = top {
            if matches!(outcome, OverlayOutcome::Handled) {
                app.sync_compose_contacts_if_needed(compose);
            }
        }
        match outcome {
            OverlayOutcome::Handled => {
                ui.overlays.push(top);
                return false;
            }
            OverlayOutcome::Close => {
                return false;
            }
            OverlayOutcome::Dispatch(next) => {
                ui.overlays.push(top);
                return dispatch_core(next, app, ui);
            }
            OverlayOutcome::CloseAndDispatch(next) => {
                return dispatch_core(next, app, ui);
            }
            OverlayOutcome::NotHandled => {
                ui.overlays.push(top);
            }
        }
    }

    dispatch_core(action, app, ui)
}

fn dispatch_core(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    match action {
        Action::Quit => {
            return quit_with_draft(app, ui);
        }
        Action::QuitConfirm => {
            if let Some(Overlay::MailCompose(c)) = ui.overlays.last() {
                if c.has_unsent_content() {
                    ui.close_overlays_of(OverlayKind::Confirm);
                    ui.push_overlay(Overlay::confirm(
                        "Stash draft and quit Outlook?",
                        Some(Action::Quit),
                    ));
                    return false;
                }
            }
            return quit_with_draft(app, ui);
        }
        Action::Noop => {}

        // Focus & layout
        Action::Focus(focus) => {
            ui.set_focus(focus);
        }
        Action::NextFocus => cycle_focus(ui, true),
        Action::PrevFocus => cycle_focus(ui, false),
        Action::ToggleNavRail => {
            ui.show_nav_rail = !ui.show_nav_rail;
            app.config.show_nav_rail = ui.show_nav_rail;
            let _ = app.config.save();
        }
        Action::SelectNarrowPane(pane) => {
            ui.narrow_pane = pane;
            match pane {
                NarrowPane::Folders => ui.set_focus(Focus::Nav),
                NarrowPane::List => ui.set_focus(Focus::List),
                NarrowPane::Read => ui.set_focus(Focus::Reader),
            }
        }
        Action::ToggleNotify => {
            ui.notify = !ui.notify;
            app.config.notify = ui.notify;
            let _ = app.config.save();
            ui.info(if ui.notify {
                "Desktop notifications: On"
            } else {
                "Desktop notifications: Off"
            });
        }
        Action::ToggleDensity => {
            ui.density = ui.density.toggle();
            app.config.density = ui.density;
            let _ = app.config.save();
            ui.info(format!("List density: {}", ui.density.label()));
        }
        Action::CycleTheme => {
            ui.theme_preset = ui.theme_preset.next();
            ui.theme = super::theme::Theme::detect(ui.theme_preset);
            app.config.theme = ui.theme_preset;
            let _ = app.config.save();
            app.clear_rendered_image_cache();
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                load_current_preview_image(app, ui);
            }
            ui.info(format!("Theme: {}", ui.theme_preset.label()));
        }
        Action::ShowDownloadDir => {
            ui.info(format!("Downloads folder: {}", app.download_dir.display()));
        }

        // Tabs & navigation
        Action::SelectNavTab(tab) => {
            ui.show_nav_rail = true;
            if ui.active_nav_tab == TAB_MAIL && tab != TAB_MAIL {
                retain_latest_reader_layout();
            }
            ui.active_nav_tab = tab.min(TAB_COUNT - 1);
            ui.medium_reader_fullscreen = false;
            ui.nav_selected = 1;
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::List);
            if ui.active_nav_tab == TAB_CALENDAR && app.active_event_id.is_none() {
                let vis = app.visible_calendar_events(ui);
                app.active_event_id = vis.first().map(|e| e.id.clone());
                if !vis.is_empty() {
                    ui.cal_list_scroll.selected_idx = Some(0);
                }
            } else if ui.active_nav_tab == TAB_TASKS && app.active_todo_task_id.is_none() {
                let vis = app.visible_todo_tasks(ui);
                app.active_todo_task_id = vis.first().map(|t| t.id.clone());
                if !vis.is_empty() {
                    ui.todo_list_scroll.selected_idx = Some(0);
                }
            }
            app.request_sync_for_active_view(ui.active_nav_tab);
        }
        Action::SelectNavRow(idx) => {
            activate_nav_row(app, ui, idx);
        }
        Action::NavCollapseFolder => nav_collapse_or_select_parent(app, ui),
        Action::NavExpandFolder => nav_expand_or_activate(app, ui),
        Action::NavToggleFolder => nav_toggle_folder(app, ui),
        Action::SelectCalendarDate(date) => {
            ui.cal_range = CalendarRangeFilter::SpecificDate(date);
            ui.cal_month_cursor = date;
            let vis = app.visible_calendar_events(ui);
            app.active_event_id = vis.first().map(|e| e.id.clone());
            ui.cal_list_scroll.selected_idx = (!vis.is_empty()).then_some(0);
            ui.cal_list_scroll.offset = 0;
            ui.cal_detail_scroll.offset = 0;
            ui.set_focus(Focus::List);
        }

        // Movement & scrolling
        Action::MoveUp => move_selection(app, ui, -1),
        Action::MoveDown => move_selection(app, ui, 1),
        Action::ScrollPageUp => scroll_reader(ui, -(reader_half_page(ui) as isize)),
        Action::ScrollPageDown => scroll_reader(ui, reader_half_page(ui) as isize),
        Action::ScrollTop => match ui.focus() {
            Focus::Nav => ui.nav_selected = 1,
            Focus::List => move_list_edge(app, ui, true),
            Focus::Reader => active_detail_scroll_mut(ui).top(),
            Focus::Overlay => {}
        },
        Action::ScrollBottom => match ui.focus() {
            Focus::Nav => {
                let count = super::nav_pane::nav_row_count(app, ui);
                if count > 0 {
                    ui.nav_selected = count - 1;
                }
            }
            Focus::List => move_list_edge(app, ui, false),
            Focus::Reader => {
                let h = active_detail_scroll_mut(ui).area.height as usize;
                active_detail_scroll_mut(ui).bottom(h);
            }
            Focus::Overlay => {}
        },
        Action::MouseScroll { delta, column, row } => {
            handle_mouse_scroll(app, ui, delta, column, row);
        }
        Action::Activate => handle_activate(app, ui),
        Action::SpaceAction => {
            if ui.active_nav_tab == TAB_TASKS {
                app.toggle_todo_completed(ui);
            } else {
                scroll_reader(ui, reader_half_page(ui) as isize);
            }
        }
        Action::ContextNew => match ui.active_nav_tab {
            TAB_CALENDAR => {
                let _ = dispatch(Action::OpenEventForm, app, ui);
            }
            TAB_TASKS => {
                if ui.focus() == Focus::Nav {
                    let _ = dispatch(Action::OpenNewTodoList, app, ui);
                } else {
                    let _ = dispatch(Action::OpenNewTodoTask, app, ui);
                }
            }
            _ => {
                let _ = dispatch(Action::OpenMailCompose, app, ui);
            }
        },

        // Mail actions
        Action::SelectMailFolder(folder_id) => {
            ui.active_nav_tab = TAB_MAIL;
            app.select_mail_folder(ui, folder_id);
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::List);
        }
        Action::OpenMailCompose => {
            let mode = MailComposeMode::New;
            let mut compose = match app.draft_store.get_mail(&mode.draft_key()) {
                Some(entry) => MailComposeOverlay::from_draft_entry(mode, entry),
                None => MailComposeOverlay::new(),
            };
            app.sync_compose_contacts_if_needed(&mut compose);
            ui.close_overlays_of(OverlayKind::MailCompose);
            ui.push_overlay(Overlay::mail_compose(compose));
        }
        Action::StartNewMailCompose => {
            if let Err(err) = stash_open_compose(app, ui) {
                ui.error(format!("Cannot save draft locally: {err:#}"));
                return false;
            }
            let mut compose = MailComposeOverlay::new();
            app.sync_compose_contacts_if_needed(&mut compose);
            ui.close_overlays_of(OverlayKind::MailCompose);
            ui.push_overlay(Overlay::mail_compose(compose));
        }
        Action::ComposeTo(email) => {
            let compose = MailComposeOverlay::new_to(email);
            ui.close_overlays_of(OverlayKind::MailCompose);
            ui.push_overlay(Overlay::mail_compose(compose));
        }
        Action::MailReply { reply_all } => {
            if let Some(mail) = app.active_mail().cloned() {
                let mut compose = MailComposeOverlay::reply(&mail, reply_all, &app.my_email);
                if let Some(saved) = app.draft_store.get_mail(&compose.mode.draft_key()) {
                    compose = compose.resume_thread_draft(saved);
                }
                app.sync_compose_contacts_if_needed(&mut compose);
                ui.close_overlays_of(OverlayKind::MailCompose);
                ui.push_overlay(Overlay::mail_compose(compose));
            }
        }
        Action::MailForward => {
            if let Some(mail) = app.active_mail().cloned() {
                let mut compose = MailComposeOverlay::forward(&mail);
                if let Some(saved) = app.draft_store.get_mail(&compose.mode.draft_key()) {
                    compose = compose.resume_thread_draft(saved);
                }
                app.sync_compose_contacts_if_needed(&mut compose);
                ui.close_overlays_of(OverlayKind::MailCompose);
                ui.push_overlay(Overlay::mail_compose(compose));
            }
        }
        Action::MailArchive => {
            if ui.active_nav_tab == TAB_MAIL {
                app.archive_selected_mail(ui);
            }
        }
        Action::DeleteItem => match ui.active_nav_tab {
            TAB_MAIL => app.delete_selected_mail(ui),
            TAB_CALENDAR => app.delete_selected_calendar_event(ui),
            TAB_TASKS => app.delete_selected_todo_task(ui),
            _ => {}
        },
        Action::MailToggleRead => {
            if ui.active_nav_tab == TAB_MAIL {
                app.toggle_mail_read(ui);
            }
        }
        Action::MailToggleFlag => {
            if ui.active_nav_tab == TAB_MAIL {
                app.toggle_mail_flag(ui);
            }
        }
        Action::OpenFolderPicker => {
            if ui.active_nav_tab == TAB_MAIL {
                if let Some(id) = app.active_mail_id.clone() {
                    ui.close_overlays_of(OverlayKind::FolderPicker);
                    ui.push_overlay(Overlay::folder_picker(vec![id]));
                }
            }
        }
        Action::MoveMailToFolder {
            message_ids,
            destination_folder_id,
            destination_name,
        } => {
            app.move_mail_to_folder(ui, message_ids, destination_folder_id, destination_name);
        }
        Action::ToggleGroupThreads => {
            ui.group_threads = !ui.group_threads;
            app.config.group_threads = ui.group_threads;
            let _ = app.config.save();
            app.sync_displayed_mail_details(ui.group_threads, true);
            ui.info(if ui.group_threads {
                "Thread grouping: On"
            } else {
                "Thread grouping: Off"
            });
        }
        Action::ToggleThreadMessages => toggle_thread_messages(app, ui),
        Action::ToggleFocusedOnly => {
            ui.focused_only = !ui.focused_only;
            app.config.focused_only = ui.focused_only;
            let _ = app.config.save();
            app.select_first_visible_mail(ui);
            ui.info(if ui.focused_only {
                "Filter: Focused Inbox"
            } else {
                "Filter: All Mail"
            });
        }
        Action::ToggleUnreadFilter => {
            ui.mail_unread_only = !ui.mail_unread_only;
            app.select_first_visible_mail(ui);
            ui.info(if ui.mail_unread_only {
                "Filter: Unread only"
            } else {
                "Filter: All messages"
            });
        }
        Action::ToggleQuotedHistory => {
            if let Some(id) = app.active_mail_id.clone() {
                if !app.expanded_quoted_ids.remove(&id) {
                    app.expanded_quoted_ids.insert(id);
                }
                app.bump_mail_gen();
            }
        }
        Action::DownloadLatestAttachment => {
            if let Some(msg) = app.active_mail().cloned() {
                if !parser::effective_mail_attachments(&msg).is_empty() {
                    app.download_mail_attachment(ui, msg.id, 0);
                } else {
                    ui.info("No attachments in selected email");
                }
            }
        }
        Action::DownloadMailAttachment {
            msg_id,
            attach_index,
        } => {
            app.download_mail_attachment(ui, msg_id, attach_index);
        }
        Action::OpenActiveLink => {
            let url = match ui.active_nav_tab {
                TAB_MAIL => app.active_mail().and_then(|m| {
                    m.meeting_request
                        .as_ref()
                        .and_then(|mr| mr.online_meeting_url.clone())
                        .or_else(|| m.web_link.clone())
                }),
                TAB_CALENDAR => app.active_event().and_then(|e| {
                    e.online_meeting_url
                        .clone()
                        .or_else(|| e.web_link.clone())
                }),
                _ => None,
            };
            if let Some(link) = url {
                match open::that(&link) {
                    Ok(()) => ui.info(format!("Opened link: {link}")),
                    Err(e) => ui.error(format!("Failed to open link: {e}")),
                }
            } else {
                ui.info("No meeting or web link on selected item");
            }
        }
        Action::OpenLinkExtractor => {
            let overlay = match ui.active_nav_tab {
                TAB_MAIL => app.active_mail().map(LinkExtractorOverlay::from_mail),
                TAB_CALENDAR => app.active_event().map(LinkExtractorOverlay::from_event),
                _ => None,
            };
            match overlay {
                Some(ov) if !ov.links.is_empty() => {
                    ui.close_overlays_of(OverlayKind::Links);
                    ui.push_overlay(Overlay::links(ov));
                }
                Some(_) => ui.info("No URLs found in selected item"),
                None => ui.info("Select an email or calendar event to extract links"),
            }
        }
        Action::ToggleShowLinkUrls => {
            ui.show_link_urls = !ui.show_link_urls;
            ui.info(if ui.show_link_urls {
                "Inline link URLs (href): Shown"
            } else {
                "Inline link URLs (href): Hidden"
            });
        }
        Action::OpenUrl(url) => match open::that(&url) {
            Ok(()) => ui.info(format!("Opened URL: {url}")),
            Err(e) => ui.error(format!("Failed to open URL: {e}")),
        },
        Action::CopyUrlToClipboard(url) => match arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(url.clone()))
        {
            Ok(()) => ui.success(format!("Copied URL: {url}")),
            Err(e) => ui.error(format!("Clipboard copy failed: {e}")),
        },
        Action::LoadOlderMail => {
            app.maybe_autoload_older_mail();
        }

        // Compose submission & draft stashing
        Action::SubmitMailCompose => {
            if let Some(Overlay::MailCompose(compose)) = ui.overlays.last() {
                let compose = compose.clone();
                if app.submit_mail_compose(ui, &compose) {
                    ui.close_overlays_of(OverlayKind::MailCompose);
                }
            }
        }
        Action::SaveMailDraft => {
            if let Some(Overlay::MailCompose(compose)) = ui.overlays.last() {
                let compose = compose.clone();
                app.save_mail_draft(ui, &compose);
            }
        }
        Action::StashMailDraft(compose) => {
            if let Err(err) = app.stash_compose_draft(&compose) {
                let mut compose = *compose;
                compose.error = Some(format!("Cannot save draft locally: {err:#}"));
                ui.push_overlay(Overlay::mail_compose(compose));
            }
        }
        Action::AttachComposeFile(path) => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();
            if let Some(Overlay::MailCompose(ref mut compose)) = ui.overlays.last_mut() {
                compose.attach_file(path);
                ui.info(format!("Attached file: {name}"));
            } else {
                let mut compose = MailComposeOverlay::new();
                compose.attach_file(path);
                ui.push_overlay(Overlay::mail_compose(compose));
                ui.info(format!("Attached file to new draft: {name}"));
            }
        }
        Action::OpenFilePicker => {
            ui.close_overlays_of(OverlayKind::FilePicker);
            ui.push_overlay(Overlay::file_picker());
        }
        Action::OpenExternalEditor => {
            if matches!(ui.overlays.last(), Some(Overlay::MailCompose(_))) {
                ui.editor_requested = true;
            }
        }
        Action::PasteClipboard => {
            paste_clipboard(app, ui);
        }

        // Calendar
        Action::OpenEventForm => {
            ui.close_overlays_of(OverlayKind::EventForm);
            ui.push_overlay(Overlay::event_form());
        }
        Action::SelectCalendarToday => {
            ui.cal_range = CalendarRangeFilter::Today;
            ui.cal_month_cursor = chrono::Local::now().date_naive();
            app.reconcile_calendar_selection(ui);
            ui.cal_list_scroll.offset = 0;
            ui.cal_detail_scroll.offset = 0;
            ui.set_focus(Focus::List);
        }
        Action::CreateCalendarEvent(ev) => {
            app.create_calendar_event(ui, ev);
        }
        Action::DeleteCalendarEvent => {
            app.delete_selected_calendar_event(ui);
        }
        Action::CalendarRsvp(rsvp) => {
            let target_id = match ui.active_nav_tab {
                TAB_MAIL => app
                    .active_mail()
                    .and_then(|m| m.meeting_request.as_ref())
                    .and_then(|mr| mr.event_id.clone()),
                TAB_CALENDAR => app.active_event_id.clone(),
                _ => None,
            };
            if let Some(eid) = target_id {
                app.respond_calendar_event(ui, eid, rsvp);
            } else {
                ui.info("No calendar event ID available for RSVP");
            }
        }
        Action::JumpToCalendarEvent(event_id) => {
            if ui.active_nav_tab == TAB_MAIL {
                retain_latest_reader_layout();
            }
            ui.active_nav_tab = TAB_CALENDAR;
            ui.cal_range = CalendarRangeFilter::AllUpcoming;
            app.active_event_id = Some(event_id.clone());
            let vis = app.visible_calendar_events(ui);
            ui.cal_list_scroll.selected_idx = vis.iter().position(|e| e.id == event_id);
            ui.set_focus(Focus::List);
            app.request_sync_for_active_view(TAB_CALENDAR);
        }

        // Todo
        Action::OpenNewTodoList => {
            ui.close_overlays_of(OverlayKind::QuickInput);
            ui.push_overlay(Overlay::quick_input(QuickInputOverlay::new_todo_list()));
        }
        Action::OpenNewTodoTask => {
            let list_id = app
                .active_todo_list_id
                .clone()
                .unwrap_or_else(|| "Tasks".to_string());
            ui.close_overlays_of(OverlayKind::QuickInput);
            ui.push_overlay(Overlay::quick_input(QuickInputOverlay::new_todo_task(
                list_id,
            )));
        }
        Action::ToggleTodoHideCompleted => {
            ui.todo_hide_completed = !ui.todo_hide_completed;
            app.reconcile_todo_selection(ui);
            ui.todo_list_scroll.offset = 0;
            ui.todo_detail_scroll.offset = 0;
            ui.info(if ui.todo_hide_completed {
                "Tasks: Open only"
            } else {
                "Tasks: All tasks"
            });
        }
        Action::CreateTodoList { display_name } => {
            app.create_todo_list(ui, display_name);
        }
        Action::CreateTodoTask { list_id, title } => {
            app.create_todo_task(ui, list_id, title);
        }
        Action::ToggleTodoCompleted => {
            app.toggle_todo_completed(ui);
        }
        Action::DeleteTodoTask => {
            app.delete_selected_todo_task(ui);
        }

        // Search & sync
        Action::OpenSearch => {
            ui.close_overlays_of(OverlayKind::Search);
            ui.push_overlay(Overlay::search());
        }
        Action::OpenSearchWithQuery(query) => {
            ui.close_overlays_of(OverlayKind::Search);
            let search = SearchOverlay {
                query: query.clone(),
                ..SearchOverlay::default()
            };
            ui.push_overlay(Overlay::Search(search));
            if !query.trim().is_empty() {
                app.search_local_mail(ui, query);
            }
        }
        Action::SearchLocalMail(query) => {
            app.search_local_mail(ui, query);
        }
        Action::SearchRemoteMail(query) => {
            app.search_remote_mail(ui, query);
        }
        Action::JumpToSearchHit(idx) => {
            if let Some(hit) = app.search_results.get(idx).cloned() {
                ui.active_nav_tab = TAB_MAIL;
                let stored_msg = app.storage.get_mail_message(&hit.id).ok().flatten();
                let target_folder = hit
                    .folder_id
                    .clone()
                    .or_else(|| stored_msg.as_ref().and_then(|m| m.folder_id.clone()));
                if let Some(fid) = target_folder {
                    app.select_mail_folder(ui, fid);
                } else {
                    app.pinned_mail = None;
                    app.bump_mail_gen();
                }
                let mut needs_hydrate = true;
                if let Some(msg) = stored_msg {
                    needs_hydrate = !msg.body_loaded;
                    if ui.focused_only && !msg.is_focused() {
                        ui.focused_only = false;
                    }
                    app.pinned_mail = Some(msg.clone());
                    if !app.mail_messages.iter().any(|m| m.id == msg.id) {
                        app.mail_messages.insert(0, AppState::mail_summary_from(&msg));
                    }
                    app.cache_loaded_mail_detail(msg);
                }
                app.active_mail_id = Some(hit.id.clone());
                app.sync_displayed_mail_details(ui.group_threads, false);
                app.push_focus_to_sync();
                if needs_hydrate {
                    app.sync_ctrl.request(SyncCommand::HydrateMessage {
                        message_id: hit.id.clone(),
                    });
                }
                app.sync_mail_list_selection_to_active(ui);
                ui.mail_read_scroll.offset = 0;
                ui.set_focus(Focus::Reader);
            }
        }
        Action::RefreshAll => {
            app.syncing = true;
            app.sync_summary = "Syncing...".to_string();
            app.sync_ctrl.request(SyncCommand::RefreshAll);
        }

        // Overlays
        Action::OpenOverlay(kind) => {
            ui.close_overlays_of(kind);
            match kind {
                OverlayKind::CommandPalette => ui.push_overlay(Overlay::command_palette()),
                OverlayKind::Settings => ui.push_overlay(Overlay::settings()),
                OverlayKind::Search => ui.push_overlay(Overlay::search()),
                OverlayKind::Attachments => ui.push_overlay(Overlay::attachments()),
                OverlayKind::PathInput => ui.push_overlay(Overlay::path_input()),
                OverlayKind::FilePicker => ui.push_overlay(Overlay::file_picker()),
                OverlayKind::Links => {
                    let _ = dispatch(Action::OpenLinkExtractor, app, ui);
                }
                OverlayKind::MailCompose => {
                    let _ = dispatch(Action::OpenMailCompose, app, ui);
                }
                OverlayKind::EventForm => ui.push_overlay(Overlay::event_form()),
                OverlayKind::Help => ui.push_overlay(Overlay::help()),
                OverlayKind::ImagePreview => {
                    let _ = dispatch(Action::OpenImagePreview, app, ui);
                }
                OverlayKind::HtmlPreview => {
                    let _ = dispatch(Action::OpenHtmlPreview, app, ui);
                }
                _ => {}
            }
        }
        Action::CloseOverlay => {
            if ui.medium_reader_fullscreen {
                ui.medium_reader_fullscreen = false;
                ui.set_focus(Focus::List);
            } else if ui.focus() == Focus::Reader {
                ui.set_focus(Focus::List);
                ui.narrow_pane = NarrowPane::List;
            }
        }
        Action::ShowToast(msg) => ui.info(msg),

        // Inline images & preview
        Action::ToggleInlineImages => {
            ui.inline_images = !ui.inline_images;
            app.config.inline_images = ui.inline_images;
            let _ = app.config.save();
            ui.info(if ui.inline_images {
                "Inline image previews: On"
            } else {
                "Inline image previews: Off"
            });
        }
        Action::OpenImagePreview => {
            if let Some(msg) = app.active_mail() {
                if let Some(overlay) = ImagePreviewOverlay::from_mail_message(msg, None) {
                    ui.close_overlays_of(OverlayKind::ImagePreview);
                    ui.push_overlay(Overlay::ImagePreview(overlay));
                    load_current_preview_image(app, ui);
                } else {
                    ui.info("No image attachments in selected email");
                }
            }
        }
        Action::OpenMailImagePreview {
            msg_id,
            attach_index,
        } => {
            if let Some(msg) = app.resolve_mail_for_action(&msg_id) {
                if let Some(overlay) = ImagePreviewOverlay::from_mail_message(&msg, attach_index) {
                    ui.close_overlays_of(OverlayKind::ImagePreview);
                    ui.push_overlay(Overlay::ImagePreview(overlay));
                    load_current_preview_image(app, ui);
                }
            }
        }
        Action::LoadPreviewImage => {
            load_current_preview_image(app, ui);
        }
        Action::CycleImageProtocol => {
            ui.graphics_caps.protocol = ui.graphics_caps.protocol.next_manual();
            app.config.image_protocol = match ui.graphics_caps.protocol {
                img_preview::TerminalImageProtocol::Kitty => crate::config::ImageProtocolPreference::Kitty,
                img_preview::TerminalImageProtocol::ITerm2 => crate::config::ImageProtocolPreference::Iterm2,
                img_preview::TerminalImageProtocol::Sixel => crate::config::ImageProtocolPreference::Sixel,
                img_preview::TerminalImageProtocol::Halfblock => crate::config::ImageProtocolPreference::Halfblock,
            };
            let _ = app.config.save();
            ui.info(format!(
                "Image protocol: {}",
                ui.graphics_caps.protocol.label()
            ));
            load_current_preview_image(app, ui);
        }
        Action::DownloadPreviewImage => {
            if let Some(Overlay::ImagePreview(ref preview)) = ui.overlays.last() {
                if let Some(cur) = preview.current() {
                    let msg_id = cur.owner_id().to_string();
                    let att_id = cur.att_id().to_string();
                    if let Some(msg) = app.resolve_mail_for_action(&msg_id) {
                        if let Some(idx) = parser::effective_mail_attachments(&msg)
                            .iter()
                            .position(|a| a.id == att_id)
                        {
                            app.download_mail_attachment(ui, msg_id, idx);
                        }
                    }
                }
            }
        }
        Action::OpenHtmlPreview => {
            if let Some(msg) = app.active_mail().cloned() {
                let title = msg.effective_subject().to_string();
                let html = if !msg.body_html.trim().is_empty() {
                    Some(msg.body_html)
                } else if !msg.body_plain.trim().is_empty() {
                    Some(format!("<pre>{}</pre>", escape_html_text(&msg.body_plain)))
                } else if !msg.preview.trim().is_empty() {
                    Some(format!("<pre>{}</pre>", escape_html_text(&msg.preview)))
                } else {
                    None
                };
                if let Some(html) = html {
                    ui.close_overlays_of(OverlayKind::HtmlPreview);
                    let mut overlay = HtmlPreviewOverlay::new(Some(title), html);
                    overlay.show_link_urls = ui.show_link_urls;
                    ui.push_overlay(Overlay::html_preview(overlay));
                } else {
                    ui.info("Selected email has no HTML content");
                }
            } else {
                ui.info("Select an email to open HTML preview");
            }
        }
        Action::OpenMailHtmlAttachment {
            msg_id,
            attach_index,
        } => {
            app.open_mail_html_attachment(ui, msg_id, attach_index);
        }
        Action::ServeHtmlPreview => {
            if let Some(Overlay::HtmlPreview(preview)) = ui.overlays.last() {
                let title = preview.title_or_default();
                let html = preview.session.serve_html();
                app.serve_html_in_browser(ui, title, html);
            }
        }
        Action::CheckForUpdates => app.check_for_updates(ui, true, false),
        Action::ApplySelfUpdate => app.check_for_updates(ui, false, false),
        Action::OpenContextMenuAt { column, row } => {
            if ui.overlays.is_empty() {
                open_context_menu_at(column, row, app, ui);
            }
        }
        Action::Click { column, row } => {
            return handle_click(column, row, app, ui);
        }

        // Handled inside overlays; ignore outside
        Action::SaveDraft
        | Action::OpenAttachPicker
        | Action::ClearPendingImages
        | Action::Input(_)
        | Action::Paste(_)
        | Action::Backspace
        | Action::DeleteWordBackward
        | Action::DeleteForward
        | Action::InsertNewline
        | Action::Tab
        | Action::CursorLeft
        | Action::CursorRight
        | Action::CursorUp
        | Action::CursorDown
        | Action::CursorHome
        | Action::CursorEnd
        | Action::SelectAll
        | Action::Undo
        | Action::Redo
        | Action::OverlaySubmit => {}
    }

    false
}

fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn quit_with_draft(app: &mut AppState, ui: &mut UiState) -> bool {
    if app.has_pending_draft_write() {
        ui.info("Outlook is saving a draft. Quit after the save completes.");
        return false;
    }
    if let Err(err) = stash_open_compose(app, ui) {
        ui.error(format!("Cannot save draft locally: {err:#}"));
        return false;
    }
    true
}

fn stash_open_compose(app: &mut AppState, ui: &UiState) -> anyhow::Result<()> {
    for ov in &ui.overlays {
        if let Overlay::MailCompose(compose) = ov {
            app.stash_compose_draft(compose)?;
        }
    }
    Ok(())
}

fn cycle_focus(ui: &mut UiState, forward: bool) {
    let next = match (ui.focus(), forward) {
        (Focus::Nav, true) => {
            ui.narrow_pane = NarrowPane::List;
            Focus::List
        }
        (Focus::List, true) => {
            ui.narrow_pane = NarrowPane::Read;
            Focus::Reader
        }
        (Focus::Reader, true) => {
            ui.show_nav_rail = true;
            ui.narrow_pane = NarrowPane::Folders;
            Focus::Nav
        }
        (Focus::Nav, false) => {
            ui.narrow_pane = NarrowPane::Read;
            Focus::Reader
        }
        (Focus::List, false) => {
            ui.show_nav_rail = true;
            ui.narrow_pane = NarrowPane::Folders;
            Focus::Nav
        }
        (Focus::Reader, false) => {
            ui.narrow_pane = NarrowPane::List;
            Focus::List
        }
        (Focus::Overlay, _) => Focus::Overlay,
    };
    ui.set_focus(next);
}

fn activate_nav_row(app: &mut AppState, ui: &mut UiState, idx: usize) {
    let rows = super::nav_pane::build_nav_rows_cached(app, ui);
    let Some(row) = rows.get(idx).cloned() else {
        return;
    };
    ui.nav_selected = idx;
    match row {
        NavRow::SectionHeader { .. } => {}
        NavRow::MailFolder { folder_id, .. } => {
            app.select_mail_folder(ui, folder_id);
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::List);
        }
        NavRow::CalendarRange { range, .. } => {
            ui.cal_range = range;
            let vis = app.visible_calendar_events(ui);
            app.active_event_id = vis.first().map(|e| e.id.clone());
            ui.cal_list_scroll.selected_idx = (!vis.is_empty()).then_some(0);
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::List);
        }
        NavRow::TodoList { list_id, .. } => {
            app.select_todo_list(ui, list_id);
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::List);
        }
        NavRow::ActionButton { action_id, .. } => {
            if action_id == ButtonId::NewTodoList {
                let _ = dispatch(Action::OpenNewTodoList, app, ui);
            }
        }
    }
}

/// The folder-pane rows and the index of the selected one (clamped), when there are rows.
fn selected_nav_row(app: &AppState, ui: &UiState) -> Option<(std::sync::Arc<[NavRow]>, usize)> {
    let rows = super::nav_pane::build_nav_rows_cached(app, ui);
    let idx = ui.nav_selected.min(rows.len().checked_sub(1)?);
    Some((rows, idx))
}

fn nav_collapse_or_select_parent(app: &mut AppState, ui: &mut UiState) {
    let Some((rows, idx)) = selected_nav_row(app, ui) else {
        return;
    };
    let NavRow::MailFolder {
        folder_id,
        depth,
        expanded,
        ..
    } = &rows[idx]
    else {
        return;
    };
    if *expanded {
        ui.collapsed_folder_ids.insert(folder_id.clone());
        ui.bump_nav_fold_gen();
        ui.nav_selected = idx;
    } else if *depth > 0 {
        // Rows are a pre-order listing, so the nearest row one level up is the parent.
        if let Some(parent) = rows[..idx].iter().rposition(
            |r| matches!(r, NavRow::MailFolder { depth: d, .. } if d.saturating_add(1) == *depth),
        ) {
            ui.nav_selected = parent;
        }
    }
}

fn nav_expand_or_activate(app: &mut AppState, ui: &mut UiState) {
    if let Some((rows, idx)) = selected_nav_row(app, ui) {
        if let NavRow::MailFolder {
            folder_id,
            has_children: true,
            expanded: false,
            ..
        } = &rows[idx]
        {
            ui.collapsed_folder_ids.remove(folder_id);
            ui.bump_nav_fold_gen();
            ui.nav_selected = idx;
            return;
        }
    }
    handle_activate(app, ui);
}

fn nav_toggle_folder(app: &mut AppState, ui: &mut UiState) {
    let Some((rows, idx)) = selected_nav_row(app, ui) else {
        return;
    };
    if let NavRow::MailFolder {
        folder_id,
        has_children: true,
        expanded,
        ..
    } = &rows[idx]
    {
        if *expanded {
            ui.collapsed_folder_ids.insert(folder_id.clone());
        } else {
            ui.collapsed_folder_ids.remove(folder_id);
        }
        ui.bump_nav_fold_gen();
        ui.nav_selected = idx;
    }
}

/// Ids of the messages above the newest one in the active grouped conversation, oldest first,
/// in the same order the reader lists them.
fn active_thread_older_ids(app: &AppState, ui: &UiState) -> Vec<String> {
    if !ui.group_threads {
        return Vec::new();
    }
    let Some(cid) = app
        .active_mail()
        .and_then(|m| m.conversation_id.as_deref())
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let mut thread: Vec<&crate::models::MailMessage> = app
        .mail_messages
        .iter()
        .filter(|m| m.conversation_id.as_deref() == Some(cid))
        .collect();
    thread.sort_by(|a, b| a.received_at.cmp(&b.received_at));
    thread.pop();
    thread.into_iter().map(|m| m.id.clone()).collect()
}

fn toggle_thread_messages(app: &mut AppState, ui: &mut UiState) {
    if ui.active_nav_tab != TAB_MAIL {
        return;
    }
    let older = active_thread_older_ids(app, ui);
    if older.is_empty() {
        ui.info("No older messages in this thread");
        return;
    }
    let active = app.active_mail_id.as_deref();
    let any_collapsed = older
        .iter()
        .any(|id| !app.expanded_thread_ids.contains(id) && active != Some(id.as_str()));
    if any_collapsed {
        let count = older.len();
        app.expanded_thread_ids.extend(older);
        app.bump_mail_gen();
        app.sync_displayed_mail_details(ui.group_threads, true);
        ui.info(format!(
            "Thread: expanded {count} older message{}",
            if count == 1 { "" } else { "s" }
        ));
    } else {
        for id in &older {
            app.expanded_thread_ids.remove(id);
        }
        app.bump_mail_gen();
        app.sync_displayed_mail_details(ui.group_threads, false);
        ui.info("Thread: collapsed older messages");
    }
}

fn move_selection(app: &mut AppState, ui: &mut UiState, delta: i32) {
    match ui.focus() {
        Focus::Nav => {
            let rows = super::nav_pane::build_nav_rows_cached(app, ui);
            if rows.is_empty() {
                return;
            }
            let mut idx = ui.nav_selected.min(rows.len() - 1);
            for _ in 0..rows.len() {
                idx = if delta < 0 {
                    if idx == 0 { rows.len() - 1 } else { idx - 1 }
                } else {
                    (idx + 1) % rows.len()
                };
                if rows[idx].is_selectable() {
                    ui.nav_selected = idx;
                    break;
                }
            }
        }
        Focus::List => match ui.active_nav_tab {
            TAB_MAIL => {
                let rows = app.visible_mail_rows_cached(ui);
                if rows.is_empty() {
                    return;
                }
                let cur = ui.mail_list_scroll.selected_idx.unwrap_or(0);
                let next = if delta < 0 {
                    cur.saturating_sub((-delta) as usize)
                } else {
                    (cur + delta as usize).min(rows.len() - 1)
                };
                app.select_mail_row(ui, next);
            }
            TAB_CALENDAR => {
                let vis = app.visible_calendar_events(ui);
                if vis.is_empty() {
                    return;
                }
                let cur = ui.cal_list_scroll.selected_idx.unwrap_or(0);
                let next = if delta < 0 {
                    cur.saturating_sub((-delta) as usize)
                } else {
                    (cur + delta as usize).min(vis.len() - 1)
                };
                ui.cal_list_scroll.selected_idx = Some(next);
                app.active_event_id = Some(vis[next].id.clone());
                ui.cal_detail_scroll.offset = 0;
            }
            TAB_TASKS => {
                let vis = app.visible_todo_tasks(ui);
                if vis.is_empty() {
                    return;
                }
                let cur = ui.todo_list_scroll.selected_idx.unwrap_or(0);
                let next = if delta < 0 {
                    cur.saturating_sub((-delta) as usize)
                } else {
                    (cur + delta as usize).min(vis.len() - 1)
                };
                ui.todo_list_scroll.selected_idx = Some(next);
                app.active_todo_task_id = Some(vis[next].id.clone());
                ui.todo_detail_scroll.offset = 0;
            }
            _ => {}
        },
        Focus::Reader => {
            scroll_reader(ui, delta as isize);
        }
        Focus::Overlay => {}
    }
}

fn move_list_edge(app: &mut AppState, ui: &mut UiState, top: bool) {
    match ui.active_nav_tab {
        TAB_MAIL => {
            let len = app.visible_mail_rows_cached(ui).len();
            if len > 0 {
                app.select_mail_row(ui, if top { 0 } else { len - 1 });
            }
        }
        TAB_CALENDAR => {
            let vis = app.visible_calendar_events(ui);
            if !vis.is_empty() {
                let idx = if top { 0 } else { vis.len() - 1 };
                ui.cal_list_scroll.selected_idx = Some(idx);
                app.active_event_id = Some(vis[idx].id.clone());
            }
        }
        TAB_TASKS => {
            let vis = app.visible_todo_tasks(ui);
            if !vis.is_empty() {
                let idx = if top { 0 } else { vis.len() - 1 };
                ui.todo_list_scroll.selected_idx = Some(idx);
                app.active_todo_task_id = Some(vis[idx].id.clone());
            }
        }
        _ => {}
    }
}

fn active_detail_scroll_mut(ui: &mut UiState) -> &mut super::app::ScrollState {
    match ui.active_nav_tab {
        TAB_CALENDAR => &mut ui.cal_detail_scroll,
        TAB_TASKS => &mut ui.todo_detail_scroll,
        _ => &mut ui.mail_read_scroll,
    }
}

fn reader_half_page(ui: &UiState) -> usize {
    let h = match ui.active_nav_tab {
        TAB_CALENDAR => ui.cal_detail_scroll.area.height,
        TAB_TASKS => ui.todo_detail_scroll.area.height,
        _ => ui.mail_read_scroll.area.height,
    };
    (h as usize / 2).max(4)
}

fn scroll_reader(ui: &mut UiState, delta: isize) {
    let scroll = active_detail_scroll_mut(ui);
    let h = scroll.area.height as usize;
    if delta < 0 {
        scroll.scroll_up((-delta) as usize);
    } else {
        scroll.scroll_down(delta as usize, h);
    }
}

fn handle_activate(app: &mut AppState, ui: &mut UiState) {
    match ui.focus() {
        Focus::Nav => {
            activate_nav_row(app, ui, ui.nav_selected);
        }
        Focus::List => match ui.active_nav_tab {
            TAB_TASKS => app.toggle_todo_completed(ui),
            _ => {
                if ui.term_width >= 80 && ui.term_width < 120 {
                    ui.medium_reader_fullscreen = true;
                }
                ui.narrow_pane = NarrowPane::Read;
                ui.set_focus(Focus::Reader);
            }
        },
        Focus::Reader => {
            let _ = dispatch(Action::OpenActiveLink, app, ui);
        }
        Focus::Overlay => {}
    }
}

fn handle_mouse_scroll(app: &mut AppState, ui: &mut UiState, delta: i32, column: u16, row: u16) {
    if !ui.overlays.is_empty() {
        return;
    }
    let pos = ratatui::layout::Position::new(column, row);
    if ui.nav_area.contains(pos) {
        let step = if delta < 0 { -1 } else { 1 };
        let prev_focus = ui.focus;
        ui.focus = Focus::Nav;
        move_selection(app, ui, step);
        ui.focus = prev_focus;
        return;
    }
    let list_area = match ui.active_nav_tab {
        TAB_CALENDAR => ui.cal_list_scroll.area,
        TAB_TASKS => ui.todo_list_scroll.area,
        _ => ui.mail_list_scroll.area,
    };
    if list_area.contains(pos) {
        let step = if delta < 0 { -1 } else { 1 };
        let prev_focus = ui.focus;
        ui.focus = Focus::List;
        move_selection(app, ui, step);
        ui.focus = prev_focus;
        return;
    }
    let detail_area = match ui.active_nav_tab {
        TAB_CALENDAR => ui.cal_detail_scroll.area,
        TAB_TASKS => ui.todo_detail_scroll.area,
        _ => ui.mail_read_scroll.area,
    };
    if detail_area.contains(pos) {
        scroll_reader(ui, delta as isize);
    }
}

fn handle_click(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) -> bool {
    let hit = ui.hitmap.hit(row, column);
    let now = Instant::now();
    let double_click = match (hit, ui.last_click) {
        (Some(target), Some((prev_t, prev_c, prev_r, prev_target))) => {
            now.duration_since(prev_t) <= Duration::from_millis(450)
                && prev_c.abs_diff(column) <= 1
                && prev_r == row
                && prev_target == target
        }
        _ => false,
    };
    ui.last_click = hit.map(|target| (now, column, row, target));

    if !ui.overlays.is_empty() {
        match hit {
            Some(HitTarget::OverlayBody) => return false,
            Some(HitTarget::HtmlPreviewBody) => {
                let outcome =
                    if let Some(Overlay::HtmlPreview(ref mut preview)) = ui.overlays.last_mut() {
                        preview.click_at_screen(column, row)
                    } else {
                        OverlayOutcome::Handled
                    };
                return match outcome {
                    OverlayOutcome::Dispatch(next) => dispatch_core(next, app, ui),
                    OverlayOutcome::Close => {
                        let _ = ui.close_top_overlay();
                        false
                    }
                    OverlayOutcome::CloseAndDispatch(next) => {
                        let _ = ui.close_top_overlay();
                        dispatch_core(next, app, ui)
                    }
                    OverlayOutcome::Handled | OverlayOutcome::NotHandled => false,
                };
            }
            Some(HitTarget::OverlayItem { index }) => {
                if let Some(Overlay::EventForm(ref mut form)) = ui.overlays.last_mut() {
                    if index <= 6 {
                        form.select(index);
                        return false;
                    }
                }
                if let Some(top) = ui.overlays.last_mut() {
                    top.select(index);
                }
                return dispatch(Action::Activate, app, ui);
            }
            Some(HitTarget::ComposeField(idx)) => {
                if let Some(Overlay::MailCompose(ref mut compose)) = ui.overlays.last_mut() {
                    compose.select_field(idx);
                    app.sync_compose_contacts_if_needed(compose);
                }
                return false;
            }
            Some(HitTarget::ComposeContactSuggestion(idx)) => {
                if let Some(Overlay::MailCompose(ref mut compose)) = ui.overlays.last_mut() {
                    let suggestions = compose.matching_contacts(&app.contacts);
                    if let Some((_, email)) = suggestions.get(idx) {
                        let owned = (*email).to_string();
                        compose.apply_contact_suggestion(&owned);
                    }
                }
                return false;
            }
            Some(HitTarget::Button(btn)) => return handle_button(btn, app, ui),
            None => {
                return dispatch(Action::CloseOverlay, app, ui);
            }
            _ => return false,
        }
    }

    let Some(target) = hit else {
        let pos = ratatui::layout::Position::new(column, row);
        let list_area = match ui.active_nav_tab {
            TAB_CALENDAR => ui.cal_list_scroll.area,
            TAB_TASKS => ui.todo_list_scroll.area,
            _ => ui.mail_list_scroll.area,
        };
        let detail_area = match ui.active_nav_tab {
            TAB_CALENDAR => ui.cal_detail_scroll.area,
            TAB_TASKS => ui.todo_detail_scroll.area,
            _ => ui.mail_read_scroll.area,
        };
        if detail_area.contains(pos) {
            ui.set_focus(Focus::Reader);
        } else if list_area.contains(pos) {
            ui.set_focus(Focus::List);
        } else if ui.nav_area.contains(pos) {
            ui.set_focus(Focus::Nav);
        }
        return false;
    };

    match target {
        HitTarget::Pane(focus) => {
            ui.set_focus(focus);
            false
        }
        HitTarget::NavTab(idx) => dispatch(Action::SelectNavTab(idx), app, ui),
        HitTarget::NavRow(idx) => {
            ui.set_focus(Focus::Nav);
            activate_nav_row(app, ui, idx);
            false
        }
        HitTarget::MiniCalDay { year, month, day } => {
            if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
                return dispatch(Action::SelectCalendarDate(d), app, ui);
            }
            false
        }
        HitTarget::ListItem(idx) => {
            ui.set_focus(Focus::List);
            match ui.active_nav_tab {
                TAB_MAIL => {
                    app.select_mail_row(ui, idx);
                    if double_click {
                        return dispatch(Action::Activate, app, ui);
                    }
                }
                TAB_CALENDAR => {
                    let vis = app.visible_calendar_events(ui);
                    if let Some(ev) = vis.get(idx) {
                        ui.cal_list_scroll.selected_idx = Some(idx);
                        app.active_event_id = Some(ev.id.clone());
                        ui.cal_detail_scroll.offset = 0;
                    }
                }
                TAB_TASKS => {
                    let vis = app.visible_todo_tasks(ui);
                    if let Some(task) = vis.get(idx) {
                        ui.todo_list_scroll.selected_idx = Some(idx);
                        app.active_todo_task_id = Some(task.id.clone());
                        ui.todo_detail_scroll.offset = 0;
                    }
                }
                _ => {}
            }
            false
        }
        HitTarget::TodoCheckbox(idx) => {
            ui.set_focus(Focus::List);
            let vis = app.visible_todo_tasks(ui);
            if let Some(task) = vis.get(idx) {
                ui.todo_list_scroll.selected_idx = Some(idx);
                app.active_todo_task_id = Some(task.id.clone());
                app.toggle_todo_completed(ui);
            }
            false
        }
        HitTarget::ThreadItem(thread_pos) => {
            ui.set_focus(Focus::Reader);
            let toggle_id = app.active_mail().and_then(|selected| {
                let cid = selected.conversation_id.as_deref()?;
                let mut msgs: Vec<_> = app
                    .mail_messages
                    .iter()
                    .filter(|m| m.conversation_id.as_deref() == Some(cid))
                    .collect();
                msgs.sort_by(|a, b| a.received_at.cmp(&b.received_at));
                msgs.get(thread_pos).map(|m| m.id.clone())
            });
            if let Some(id) = toggle_id {
                let expanded = if app.expanded_thread_ids.remove(&id) {
                    false
                } else {
                    app.expanded_thread_ids.insert(id);
                    true
                };
                app.bump_mail_gen();
                app.sync_displayed_mail_details(ui.group_threads, expanded);
            }
            false
        }
        HitTarget::Attachment {
            thread_pos,
            attach_index,
        } => {
            let target_msg = resolve_thread_msg(app, ui, thread_pos);
            if let Some(msg) = target_msg {
                let eff = parser::effective_mail_attachments(&msg);
                if let Some(att) = eff.get(attach_index) {
                    if html_preview::is_markup_mime_or_name(&att.content_type, Some(&att.name)) {
                        return dispatch(
                            Action::OpenMailHtmlAttachment {
                                msg_id: msg.id,
                                attach_index,
                            },
                            app,
                            ui,
                        );
                    }
                    if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                        return dispatch(
                            Action::OpenMailImagePreview {
                                msg_id: msg.id,
                                attach_index: Some(attach_index),
                            },
                            app,
                            ui,
                        );
                    }
                    return dispatch(
                        Action::DownloadMailAttachment {
                            msg_id: msg.id,
                            attach_index,
                        },
                        app,
                        ui,
                    );
                }
            }
            false
        }
        HitTarget::QuotedToggle(thread_pos) => {
            if let Some(msg) = resolve_thread_msg(app, ui, thread_pos) {
                if !app.expanded_quoted_ids.remove(&msg.id) {
                    app.expanded_quoted_ids.insert(msg.id);
                }
                app.bump_mail_gen();
            }
            false
        }
        HitTarget::MeetingRsvp(rsvp) => dispatch(Action::CalendarRsvp(rsvp), app, ui),
        HitTarget::JoinMeetingUrl | HitTarget::WebLinkUrl => {
            dispatch(Action::OpenActiveLink, app, ui)
        }
        HitTarget::LoadOlderMail => dispatch(Action::LoadOlderMail, app, ui),
        HitTarget::Button(btn) => handle_button(btn, app, ui),
        _ => false,
    }
}

fn resolve_thread_msg(
    app: &AppState,
    ui: &UiState,
    thread_pos: usize,
) -> Option<crate::models::MailMessage> {
    let selected = app.active_mail()?;
    if ui.group_threads {
        if let Some(cid) = selected
            .conversation_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            let mut list: Vec<_> = app
                .mail_messages
                .iter()
                .filter(|m| m.conversation_id.as_deref() == Some(cid))
                .cloned()
                .collect();
            list.sort_by(|a, b| a.received_at.cmp(&b.received_at));
            if let Some(found) = list.get(thread_pos) {
                return Some(
                    app.mail_detail(&found.id)
                        .cloned()
                        .unwrap_or_else(|| found.clone()),
                );
            }
        }
    }
    Some(selected.clone())
}

fn handle_button(btn: ButtonId, app: &mut AppState, ui: &mut UiState) -> bool {
    match btn {
        ButtonId::Search => dispatch(Action::OpenSearch, app, ui),
        ButtonId::CommandPalette => {
            dispatch(Action::OpenOverlay(OverlayKind::CommandPalette), app, ui)
        }
        ButtonId::DensityToggle => dispatch(Action::ToggleDensity, app, ui),
        ButtonId::Help => dispatch(Action::OpenOverlay(OverlayKind::Help), app, ui),
        ButtonId::FilterToggle => {
            if ui.active_nav_tab == TAB_MAIL {
                dispatch(Action::ToggleUnreadFilter, app, ui)
            } else {
                false
            }
        }
        ButtonId::CalendarToday => dispatch(Action::SelectCalendarToday, app, ui),
        ButtonId::TodoFilterToggle => dispatch(Action::ToggleTodoHideCompleted, app, ui),
        ButtonId::FocusedToggle => dispatch(Action::ToggleFocusedOnly, app, ui),
        ButtonId::ThreadGroupToggle => dispatch(Action::ToggleGroupThreads, app, ui),
        ButtonId::Settings => dispatch(Action::OpenOverlay(OverlayKind::Settings), app, ui),
        ButtonId::Refresh => dispatch(Action::RefreshAll, app, ui),
        ButtonId::NewMail => dispatch(Action::OpenMailCompose, app, ui),
        ButtonId::MailReply => dispatch(Action::MailReply { reply_all: false }, app, ui),
        ButtonId::MailReplyAll => dispatch(Action::MailReply { reply_all: true }, app, ui),
        ButtonId::MailForward => dispatch(Action::MailForward, app, ui),
        ButtonId::MailToggleRead => dispatch(Action::MailToggleRead, app, ui),
        ButtonId::MailToggleFlag => dispatch(Action::MailToggleFlag, app, ui),
        ButtonId::MailArchive => dispatch(Action::MailArchive, app, ui),
        ButtonId::MailDelete => dispatch(Action::DeleteItem, app, ui),
        ButtonId::MailMove => dispatch(Action::OpenFolderPicker, app, ui),
        ButtonId::Attachments => dispatch(Action::OpenOverlay(OverlayKind::Attachments), app, ui),
        ButtonId::ImagePreview => dispatch(Action::OpenImagePreview, app, ui),
        ButtonId::HtmlPreview => dispatch(Action::OpenHtmlPreview, app, ui),
        ButtonId::HtmlToggleRaw => {
            if let Some(Overlay::HtmlPreview(ref mut preview)) = ui.overlays.last_mut() {
                preview.session.toggle_raw_mode();
                preview.session.scroll_y = 0;
            }
            false
        }
        ButtonId::HtmlOpenBrowser => dispatch(Action::ServeHtmlPreview, app, ui),
        ButtonId::ExpandQuoted => dispatch(Action::ToggleQuotedHistory, app, ui),
        ButtonId::NewEvent => dispatch(Action::OpenEventForm, app, ui),
        ButtonId::JoinMeeting => dispatch(Action::OpenActiveLink, app, ui),
        ButtonId::RsvpAccept => dispatch(Action::CalendarRsvp(EventRsvpAction::Accept), app, ui),
        ButtonId::RsvpTentative => {
            dispatch(Action::CalendarRsvp(EventRsvpAction::TentativelyAccept), app, ui)
        }
        ButtonId::RsvpDecline => dispatch(Action::CalendarRsvp(EventRsvpAction::Decline), app, ui),
        ButtonId::DeleteEvent => dispatch(Action::DeleteCalendarEvent, app, ui),
        ButtonId::NewTodoList => dispatch(Action::OpenNewTodoList, app, ui),
        ButtonId::NewTodoTask => dispatch(Action::OpenNewTodoTask, app, ui),
        ButtonId::DeleteTodoTask => dispatch(Action::DeleteTodoTask, app, ui),
        ButtonId::NarrowFolders => dispatch(Action::SelectNarrowPane(NarrowPane::Folders), app, ui),
        ButtonId::NarrowList => dispatch(Action::SelectNarrowPane(NarrowPane::List), app, ui),
        ButtonId::NarrowRead => dispatch(Action::SelectNarrowPane(NarrowPane::Read), app, ui),
        Button::ComposeSend => dispatch(Action::OverlaySubmit, app, ui),
        Button::ComposeAttach => dispatch(Action::OpenAttachPicker, app, ui),
        Button::ComposeSaveDraft => dispatch(Action::SaveDraft, app, ui),
        Button::ComposeCancel => dispatch(Action::CloseOverlay, app, ui),
        ButtonId::EventFormSubmit => {
            if let Some(Overlay::EventForm(ref mut form)) = ui.overlays.last_mut() {
                form.field = 7;
                form.button_idx = 0;
            }
            dispatch(Action::OverlaySubmit, app, ui)
        }
        ButtonId::EventFormCancel => {
            if let Some(Overlay::EventForm(ref mut form)) = ui.overlays.last_mut() {
                form.field = 7;
                form.button_idx = 1;
            }
            dispatch(Action::CloseOverlay, app, ui)
        }
        ButtonId::ConfirmOk => dispatch(Action::Activate, app, ui),
        ButtonId::ConfirmCancel => dispatch(Action::CloseOverlay, app, ui),
    }
}
use ButtonId as Button;

fn open_context_menu_at(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) {
    let target = match ui.hitmap.hit(row, column) {
        Some(HitTarget::ListItem(idx)) => match ui.active_nav_tab {
            TAB_CALENDAR => {
                let vis = app.visible_calendar_events(ui);
                if let Some(ev) = vis.get(idx) {
                    ui.set_focus(Focus::List);
                    ui.cal_list_scroll.selected_idx = Some(idx);
                    app.active_event_id = Some(ev.id.clone());
                    ui.cal_detail_scroll.offset = 0;
                    ContextTarget::CalendarEvent(idx)
                } else {
                    ContextTarget::Global
                }
            }
            TAB_TASKS => {
                let vis = app.visible_todo_tasks(ui);
                if let Some(task) = vis.get(idx) {
                    ui.set_focus(Focus::List);
                    ui.todo_list_scroll.selected_idx = Some(idx);
                    app.active_todo_task_id = Some(task.id.clone());
                    ui.todo_detail_scroll.offset = 0;
                    ContextTarget::TodoTask(idx)
                } else {
                    ContextTarget::Global
                }
            }
            _ => {
                let rows = app.visible_mail_rows_cached(ui);
                if idx < rows.len() {
                    ui.set_focus(Focus::List);
                    app.select_mail_row(ui, idx);
                    ContextTarget::MailItem(idx)
                } else {
                    ContextTarget::Global
                }
            }
        },
        Some(HitTarget::TodoCheckbox(idx)) => {
            let vis = app.visible_todo_tasks(ui);
            if let Some(task) = vis.get(idx) {
                ui.set_focus(Focus::List);
                ui.todo_list_scroll.selected_idx = Some(idx);
                app.active_todo_task_id = Some(task.id.clone());
                ui.todo_detail_scroll.offset = 0;
                ContextTarget::TodoTask(idx)
            } else {
                ContextTarget::Global
            }
        }
        _ => ContextTarget::Global,
    };
    let menu = app.build_context_menu(target, (column, row));
    ui.close_overlays_of(OverlayKind::ContextMenu);
    ui.push_overlay(Overlay::context_menu(menu));
}

pub fn load_current_preview_image(app: &mut AppState, ui: &mut UiState) {
    let Some(Overlay::ImagePreview(ref preview)) = ui.overlays.last() else {
        return;
    };
    let Some(target) = preview.current().cloned() else {
        return;
    };
    let fit = preview.fit_box(ui.term_width, ui.term_height);
    let caps = ui.graphics_caps;
    let bg_rgb = img_preview::color_to_rgb(ui.theme.bg_elevated, (28, 32, 46));
    app.request_preview_image(&target, fit, caps, bg_rgb);
}

fn paste_clipboard(app: &mut AppState, ui: &mut UiState) {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return;
    };

    if matches!(ui.overlays.last(), Some(Overlay::MailCompose(_))) {
        if let Ok(img) = clipboard.get_image() {
            let width = img.width as u32;
            let height = img.height as u32;
            let rgba = img.bytes.into_owned();
            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if tokio::runtime::Handle::try_current().is_ok() {
                let tx = app.ui_tx.clone();
                let temp_dir = std::env::temp_dir();
                let bg_rgb = img_preview::color_to_rgb(ui.theme.bg_elevated, (28, 32, 46));
                let cell_px = ui.graphics_caps.cell_px;
                tokio::task::spawn_blocking(move || {
                    if let Ok((path, png_bytes)) = composer::save_clipboard_rgba_to_temp_png(
                        &temp_dir, unix_ms, width, height, rgba,
                    ) {
                        let preview = img_preview::decode_and_render_halfblocks_with_cell_px(
                            &png_bytes,
                            img_preview::FitBox::fixed(24, 4),
                            bg_rgb,
                            cell_px,
                        )
                        .ok();
                        let _ = tx.send(UiMsg::ClipboardImageEncoded {
                            unix_ms,
                            path,
                            width,
                            height,
                            preview,
                        });
                    }
                });
            }
            return;
        }
    }

    if let Ok(text) = clipboard.get_text() {
        let _ = dispatch(Action::Paste(text), app, ui);
    }
}
