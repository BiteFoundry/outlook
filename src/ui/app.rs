//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use chrono::{Datelike, Duration, Local, NaiveDate};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::{self, AuthManager};
use crate::config::{AppConfig, DensityMode, DraftStore, MailDraftEntry, ThemePreset};
use crate::graph::GraphClient;
use crate::img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::{
    CalendarEvent, ContactItem, CoreEvent, DownloadedFile, EventRsvpAction, InlineImageUpload,
    MailAttachment, MailFolder, MailMessage, MailSearchHit, NotificationCandidate, OutgoingEvent,
    OutgoingMail, OutgoingTodoTask, TodoList, TodoTask, UserProfile, POLL_RETRY_SUMMARY,
};
use crate::parser;
use crate::storage::Storage;
use crate::sync::{ActiveView, FocusTarget, SyncCommand, SyncController};

use super::hitmap::{HitMap, HitTarget};
use super::keymap::Keymap;
use super::nav_pane::{
    find_folder_by_wellknown, TAB_CALENDAR, TAB_MAIL, TAB_TASKS, VIRTUAL_FLAGGED_FOLDER,
    VIRTUAL_UNREAD_FOLDER,
};
use super::notification;
use super::overlays::html_preview::HtmlPreviewOverlay;
use super::overlays::image_preview::PreviewTarget;
use super::overlays::mail_compose::{MailComposeMode, MailComposeOverlay};
use super::overlays::{Overlay, OverlayKind};
use super::theme::Theme;
use super::toast::{TaskBoard, ToastKind, ToastQueue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    List,
    Reader,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NarrowPane {
    Folders,
    #[default]
    List,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CalendarRangeFilter {
    Today,
    Tomorrow,
    Next7Days,
    ThisMonth,
    #[default]
    AllUpcoming,
    SpecificDate(NaiveDate),
}

impl CalendarRangeFilter {
    pub fn label(self) -> String {
        match self {
            Self::Today => "Today".to_string(),
            Self::Tomorrow => "Tomorrow".to_string(),
            Self::Next7Days => "Next 7 Days".to_string(),
            Self::ThisMonth => "This Month".to_string(),
            Self::AllUpcoming => "Upcoming".to_string(),
            Self::SpecificDate(d) => d.format("%b %d").to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MailListRow {
    pub primary_msg_idx: usize,
    pub thread_msg_indices: Vec<usize>,
    pub unread_count: usize,
    pub any_flagged: bool,
    pub any_attachments: bool,
    pub count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ScrollState {
    pub selected_idx: Option<usize>,
    pub area: Rect,
    pub total_lines: usize,
    pub offset: usize,
}

impl ScrollState {
    pub fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize, viewport_height: usize) {
        let max_off = self.total_lines.saturating_sub(viewport_height);
        self.offset = self.offset.saturating_add(lines).min(max_off);
    }

    pub fn top(&mut self) {
        self.offset = 0;
    }

    pub fn bottom(&mut self, viewport_height: usize) {
        self.offset = self.total_lines.saturating_sub(viewport_height);
    }
}

pub enum UiMsg {
    BootstrapDone {
        me: Option<UserProfile>,
        mail_folders: Vec<MailFolder>,
        mail_messages: Vec<MailMessage>,
        calendar_events: Vec<CalendarEvent>,
        contacts: Vec<ContactItem>,
        todo_lists: Vec<TodoList>,
        todo_tasks: Vec<TodoTask>,
        scope_warning: Option<String>,
    },
    FolderMessagesLoaded {
        folder_id: String,
        messages: Vec<MailMessage>,
    },
    MailSent {
        draft_key: String,
    },
    MailDraftSaved {
        draft_key: String,
        saved: MailMessage,
    },
    MailMoved {
        moves: Vec<(String, MailMessage)>,
    },
    MailDeleted {
        message_ids: Vec<String>,
    },
    AttachmentDownloaded(DownloadedFile),
    SearchResults {
        query: String,
        hits: Vec<MailSearchHit>,
    },
    RemoteSearchResults {
        query: String,
        messages: Vec<MailMessage>,
    },
    RemoteSearchFailed {
        query: String,
        error: String,
    },
    CalendarEventCreated(CalendarEvent),
    CalendarEventDeleted(String),
    CalendarRsvpDone {
        event_id: String,
        response_status: String,
    },
    TodoListCreated(TodoList),
    TodoTasksLoaded {
        list_id: String,
        tasks: Vec<TodoTask>,
    },
    TodoTaskCreated(TodoTask),
    TodoTaskUpdated(TodoTask),
    TodoTaskDeleted(String),
    ClipboardImageEncoded {
        unix_ms: u128,
        path: PathBuf,
        width: u32,
        height: u32,
        preview: Option<RenderedTerminalImage>,
    },
    ImageDecoded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    ImageBytesFetched {
        raw_key: String,
        bytes: Arc<[u8]>,
    },
    ImageDecodeFailed {
        cache_key: String,
        owner_id: String,
        att_id: String,
        error: String,
    },
    ImageFetchReturnedHtml {
        cache_key: String,
        owner_id: String,
        att_id: String,
        title: String,
        html: String,
        from_preview: bool,
    },
    HtmlAttachmentLoaded {
        title: String,
        html: String,
    },
    UpdateChecked(crate::models::UpdateCheckResult),
    DaemonAttached {
        pid: Option<u32>,
        sync_busy: bool,
    },
    DaemonDetached,
    StartLocalBootstrap,
    ProfileLoaded {
        me: Option<UserProfile>,
        scope_warning: Option<String>,
    },
    TaskDone {
        label: String,
        error: Option<String>,
        toast: Option<(String, ToastKind)>,
    },
}

pub const MAX_RAW_IMAGE_CACHE_ENTRIES: usize = 8;
pub const MAX_RAW_IMAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
}

pub struct UiState {
    pub focus: Focus,
    pub overlays: Vec<Overlay>,
    pub hitmap: HitMap,
    pub toasts: ToastQueue,
    pub tasks: TaskBoard,
    pub keymap: Keymap,
    pub theme: Theme,
    pub active_nav_tab: usize,
    pub nav_selected: usize,
    pub nav_area: Rect,
    pub mail_list_scroll: ScrollState,
    pub mail_read_scroll: ScrollState,
    pub cal_list_scroll: ScrollState,
    pub cal_detail_scroll: ScrollState,
    pub todo_list_scroll: ScrollState,
    pub todo_detail_scroll: ScrollState,
    pub show_nav_rail: bool,
    pub group_threads: bool,
    pub focused_only: bool,
    pub mail_unread_only: bool,
    pub inline_images: bool,
    pub show_link_urls: bool,
    pub medium_reader_fullscreen: bool,
    pub narrow_pane: NarrowPane,
    pub editor_requested: bool,
    pub notify: bool,
    pub term_width: u16,
    pub term_height: u16,
    pub density: DensityMode,
    pub theme_preset: ThemePreset,
    pub kitty_keyboard_supported: bool,
    pub cal_range: CalendarRangeFilter,
    pub cal_month_cursor: NaiveDate,
    pub last_click: Option<(Instant, u16, u16, HitTarget)>,
    pub poll_retrying: bool,
    pub spinner_tick: u8,
    pub graphics_caps: TerminalGraphicsCaps,
    pub pending_pixel_graphic: Option<PendingPixelGraphic>,
    pub active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        let today = Local::now().date_naive();
        Self {
            focus: Focus::List,
            overlays: Vec::new(),
            hitmap: HitMap::default(),
            toasts: ToastQueue::new(),
            tasks: TaskBoard::default(),
            keymap: Keymap::default_outlook(),
            theme: Theme::default(),
            active_nav_tab: TAB_MAIL,
            nav_selected: 1,
            nav_area: Rect::default(),
            mail_list_scroll: ScrollState::default(),
            mail_read_scroll: ScrollState::default(),
            cal_list_scroll: ScrollState::default(),
            cal_detail_scroll: ScrollState::default(),
            todo_list_scroll: ScrollState::default(),
            todo_detail_scroll: ScrollState::default(),
            show_nav_rail: true,
            group_threads: true,
            focused_only: false,
            mail_unread_only: false,
            inline_images: true,
            show_link_urls: false,
            medium_reader_fullscreen: false,
            narrow_pane: NarrowPane::List,
            editor_requested: false,
            notify: true,
            term_width: 120,
            term_height: 40,
            density: DensityMode::default(),
            theme_preset: ThemePreset::default(),
            kitty_keyboard_supported: false,
            cal_range: CalendarRangeFilter::AllUpcoming,
            cal_month_cursor: today,
            last_click: None,
            poll_retrying: false,
            spinner_tick: 0,
            graphics_caps: TerminalGraphicsCaps::default(),
            pending_pixel_graphic: None,
            active_pixel_graphic: None,
        }
    }

    pub fn from_config(config: &AppConfig) -> Self {
        let mut s = Self::new();
        s.show_nav_rail = config.show_nav_rail;
        s.group_threads = config.group_threads;
        s.focused_only = config.focused_only;
        s.notify = config.notify;
        s.inline_images = config.inline_images;
        s.density = config.density;
        s.theme_preset = config.theme;
        s.theme = Theme::detect(config.theme);
        s.graphics_caps = img_preview::detect_graphics_caps(config.image_protocol);
        s
    }

    pub fn focus(&self) -> Focus {
        if !self.overlays.is_empty() {
            Focus::Overlay
        } else {
            self.focus
        }
    }

    pub fn accepts_text_input(&self) -> bool {
        self.overlays.last().is_some_and(Overlay::accepts_text)
    }

    pub fn push_overlay(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
    }

    pub fn close_top_overlay(&mut self) -> Option<Overlay> {
        self.overlays.pop()
    }

    pub fn close_overlays_of(&mut self, kind: OverlayKind) {
        self.overlays.retain(|o| o.kind() != kind);
    }

    pub fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toasts.info(msg);
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.toasts.success(msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toasts.error(msg);
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub auth: AuthManager,
    pub graph: GraphClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub my_profile: Option<UserProfile>,
    pub my_email: String,
    pub my_display_name: String,
    pub active_view: ActiveView,
    pub mail_folders: Vec<MailFolder>,
    pub active_folder_id: String,
    pub mail_messages: Vec<MailMessage>,
    pub mail_detail_cache: HashMap<String, MailMessage>,
    mail_detail_missing: HashSet<String>,
    pub active_mail_id: Option<String>,
    pub expanded_thread_ids: HashSet<String>,
    pub expanded_quoted_ids: HashSet<String>,
    pub calendar_events: Vec<CalendarEvent>,
    pub active_event_id: Option<String>,
    pub contacts: Vec<ContactItem>,
    pub todo_lists: Vec<TodoList>,
    pub active_todo_list_id: Option<String>,
    pub todo_tasks: Vec<TodoTask>,
    pub active_todo_task_id: Option<String>,
    pub draft_store: DraftStore,
    pub draft_save_lock: Arc<Mutex<()>>,
    pub download_dir: PathBuf,
    pub loading_mail: bool,
    pub loading_older_mail: bool,
    pub mail_oldest_reached: bool,
    pub syncing: bool,
    pub sync_summary: String,
    pub daemon_attached: bool,
    pub daemon_pid: Option<u32>,
    pub searching: bool,
    pub search_query: String,
    pub search_results: Vec<MailSearchHit>,
    pub search_generation: u64,
    pub search_gen_atomic: Arc<AtomicU64>,
    pub search_remote_inflight: bool,
    pub search_remote_done: bool,
    pub search_error: Option<String>,
    pub pinned_mail: Option<MailMessage>,
    pub scope_warned: bool,
    pub image_cache: HashMap<String, RenderedTerminalImage>,
    pub image_cache_order: VecDeque<String>,
    pub image_cache_bytes: usize,
    pub image_cache_gen: u64,
    pub image_pending: HashSet<String>,
    pub image_errors: HashMap<String, String>,
    pub image_errors_order: VecDeque<String>,
    pub image_raw_cache: HashMap<String, Arc<[u8]>>,
    pub image_raw_order: VecDeque<String>,
    pub image_raw_bytes: usize,
    pub html_server: bitefoundry_html::NativeHtmlServer,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        auth: AuthManager,
        graph: GraphClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let download_dir = config.resolved_download_dir();
        let draft_store = DraftStore::load();
        Self {
            config,
            auth,
            graph,
            storage,
            sync_ctrl,
            my_profile: None,
            my_email: String::new(),
            my_display_name: String::new(),
            active_view: ActiveView::Mail,
            mail_folders: Vec::new(),
            active_folder_id: "inbox".to_string(),
            mail_messages: Vec::new(),
            mail_detail_cache: HashMap::new(),
            mail_detail_missing: HashSet::new(),
            active_mail_id: None,
            expanded_thread_ids: HashSet::new(),
            expanded_quoted_ids: HashSet::new(),
            calendar_events: Vec::new(),
            active_event_id: None,
            contacts: Vec::new(),
            todo_lists: Vec::new(),
            active_todo_list_id: None,
            todo_tasks: Vec::new(),
            active_todo_task_id: None,
            draft_store,
            draft_save_lock: Arc::new(Mutex::new(())),
            download_dir,
            loading_mail: false,
            loading_older_mail: false,
            mail_oldest_reached: false,
            syncing: false,
            sync_summary: "Synced".to_string(),
            daemon_attached: false,
            daemon_pid: None,
            searching: false,
            search_query: String::new(),
            search_results: Vec::new(),
            search_generation: 0,
            search_gen_atomic: Arc::new(AtomicU64::new(0)),
            search_remote_inflight: false,
            search_remote_done: false,
            search_error: None,
            pinned_mail: None,
            scope_warned: false,
            image_cache: HashMap::new(),
            image_cache_order: VecDeque::new(),
            image_cache_bytes: 0,
            image_cache_gen: 0,
            image_pending: HashSet::new(),
            image_errors: HashMap::new(),
            image_errors_order: VecDeque::new(),
            image_raw_cache: HashMap::new(),
            image_raw_order: VecDeque::new(),
            image_raw_bytes: 0,
            html_server: bitefoundry_html::NativeHtmlServer::new(),
            ui_tx,
        }
    }

    pub fn load_cached_initial(&mut self) {
        if let Ok(folders) = self.storage.list_mail_folders() {
            self.mail_folders = folders;
            if let Some(inbox) =
                find_folder_by_wellknown(&self.mail_folders, "inbox", &["Inbox", "받은 편지함"])
            {
                self.active_folder_id = inbox.id.clone();
            }
        }
        if let Ok(msgs) = self.storage.list_mail_summaries_filtered(
            Some(&self.active_folder_id),
            false,
            false,
            false,
            150,
        ) {
            self.mail_messages = msgs;
            self.clear_mail_detail_cache();
            if let Some(first_id) = self.mail_messages.first().map(|m| m.id.clone()) {
                self.active_mail_id = Some(first_id.clone());
                self.load_mail_detail(&first_id, true);
            } else {
                self.active_mail_id = None;
            }
        }
        if let Ok(events) = self.storage.list_calendar_events(None, None, 200) {
            self.calendar_events = events;
            if let Some(first) = self.calendar_events.first() {
                self.active_event_id = Some(first.id.clone());
            }
        }
        if let Ok(contacts) = self.storage.list_contacts(None, 200) {
            self.contacts = contacts;
        }
        if let Ok(lists) = self.storage.list_todo_lists() {
            self.todo_lists = lists;
            if let Some(def) = self
                .todo_lists
                .iter()
                .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
                .or_else(|| self.todo_lists.first())
            {
                let lid = def.id.clone();
                self.active_todo_list_id = Some(lid.clone());
                if let Ok(tasks) = self.storage.list_todo_tasks(Some(&lid), true) {
                    self.todo_tasks = tasks;
                    if let Some(first) = self.todo_tasks.first() {
                        self.active_todo_task_id = Some(first.id.clone());
                    }
                }
            }
        }
    }

    pub fn unread_inbox_count(&self) -> i32 {
        find_folder_by_wellknown(&self.mail_folders, "inbox", &["Inbox", "받은 편지함"])
            .map(|f| f.unread_item_count.max(0))
            .unwrap_or_else(|| {
                self.mail_messages
                    .iter()
                    .filter(|m| !m.is_read)
                    .count() as i32
            })
    }

    pub fn active_folder_display_name(&self) -> String {
        if self.active_folder_id == VIRTUAL_UNREAD_FOLDER {
            return "Unread".to_string();
        }
        if self.active_folder_id == VIRTUAL_FLAGGED_FOLDER {
            return "Flagged".to_string();
        }
        self.mail_folders
            .iter()
            .find(|f| f.id == self.active_folder_id)
            .map(|f| f.display_name.clone())
            .unwrap_or_else(|| self.active_folder_id.clone())
    }

    pub fn visible_mail_rows(&self, ui: &UiState) -> Vec<MailListRow> {
        let mut indices: Vec<usize> = self
            .mail_messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                if self.active_folder_id == VIRTUAL_UNREAD_FOLDER && m.is_read {
                    return false;
                }
                if self.active_folder_id == VIRTUAL_FLAGGED_FOLDER && !m.flagged {
                    return false;
                }
                if ui.mail_unread_only && m.is_read {
                    return false;
                }
                if ui.focused_only && !m.is_focused() {
                    return false;
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        // Ensure newest first
        indices.sort_by(|&a, &b| {
            self.mail_messages[b]
                .received_at
                .cmp(&self.mail_messages[a].received_at)
        });

        if !ui.group_threads {
            return indices
                .into_iter()
                .map(|idx| {
                    let m = &self.mail_messages[idx];
                    MailListRow {
                        primary_msg_idx: idx,
                        thread_msg_indices: vec![idx],
                        unread_count: usize::from(!m.is_read),
                        any_flagged: m.flagged,
                        any_attachments: m.has_attachments || !m.attachments.is_empty(),
                        count: 1,
                    }
                })
                .collect();
        }

        let mut rows: Vec<MailListRow> = Vec::new();
        let mut conv_to_row: HashMap<String, usize> = HashMap::new();
        for idx in indices {
            let m = &self.mail_messages[idx];
            let conv_key = m
                .conversation_id
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| m.id.clone());
            if let Some(&pos) = conv_to_row.get(&conv_key) {
                let row = &mut rows[pos];
                row.thread_msg_indices.push(idx);
                row.unread_count += usize::from(!m.is_read);
                row.any_flagged |= m.flagged;
                row.any_attachments |= m.has_attachments || !m.attachments.is_empty();
                row.count += 1;
            } else {
                let pos = rows.len();
                conv_to_row.insert(conv_key, pos);
                rows.push(MailListRow {
                    primary_msg_idx: idx,
                    thread_msg_indices: vec![idx],
                    unread_count: usize::from(!m.is_read),
                    any_flagged: m.flagged,
                    any_attachments: m.has_attachments || !m.attachments.is_empty(),
                    count: 1,
                });
            }
        }
        rows
    }

    pub(crate) fn mail_summary_from(msg: &MailMessage) -> MailMessage {
        MailMessage {
            id: msg.id.clone(),
            internet_message_id: msg.internet_message_id.clone(),
            conversation_id: msg.conversation_id.clone(),
            folder_id: msg.folder_id.clone(),
            subject: msg.subject.clone(),
            from: msg.from.clone(),
            to_recipients: msg.to_recipients.first().cloned().into_iter().collect(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: msg.received_at.clone(),
            sent_at: msg.sent_at.clone(),
            last_modified_at: msg.last_modified_at.clone(),
            is_read: msg.is_read,
            is_draft: msg.is_draft,
            flagged: msg.flagged,
            importance: msg.importance.clone(),
            inference_classification: msg.inference_classification.clone(),
            has_attachments: msg.has_attachments || !msg.attachments.is_empty(),
            body_loaded: msg.body_loaded,
            preview: msg.preview.clone(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: Vec::new(),
            meeting_request: msg.meeting_request.clone(),
            web_link: msg.web_link.clone(),
        }
    }

    pub fn mail_detail(&self, id: &str) -> Option<&MailMessage> {
        self.mail_detail_cache
            .get(id)
            .or_else(|| self.pinned_mail.as_ref().filter(|m| m.id == id))
            .or_else(|| self.mail_messages.iter().find(|m| m.id == id))
    }

    pub fn active_mail(&self) -> Option<&MailMessage> {
        let id = self.active_mail_id.as_deref()?;
        self.mail_detail(id)
    }

    pub fn active_mail_mut(&mut self) -> Option<&mut MailMessage> {
        let id = self.active_mail_id.as_deref()?.to_string();
        if self.mail_detail_cache.contains_key(&id) {
            return self.mail_detail_cache.get_mut(&id);
        }
        if self.pinned_mail.as_ref().is_some_and(|m| m.id == id) {
            return self.pinned_mail.as_mut();
        }
        if let Some(idx) = self.mail_messages.iter().position(|m| m.id == id) {
            return Some(&mut self.mail_messages[idx]);
        }
        None
    }

    pub(crate) fn clear_mail_detail_cache(&mut self) {
        self.mail_detail_cache.clear();
        self.mail_detail_missing.clear();
    }

    pub(crate) fn cache_loaded_mail_detail(&mut self, msg: MailMessage) {
        self.mail_detail_missing.remove(&msg.id);
        self.mail_detail_cache.insert(msg.id.clone(), msg);
    }

    pub(crate) fn resolve_mail_for_action(&mut self, id: &str) -> Option<MailMessage> {
        if let Some(cached) = self.mail_detail_cache.get(id) {
            return Some(cached.clone());
        }
        if let Some(pinned) = self.pinned_mail.as_ref().filter(|m| m.id == id) {
            return Some(pinned.clone());
        }
        if !self.mail_detail_missing.contains(id) {
            if let Ok(Some(full)) = self.storage.get_mail_message(id) {
                return Some(full);
            }
        }
        self.mail_messages.iter().find(|m| m.id == id).cloned()
    }

    fn displayed_mail_ids(&self, group_threads: bool) -> Vec<String> {
        let Some(active_id) = self.active_mail_id.as_deref() else {
            return Vec::new();
        };
        let mut ids = vec![active_id.to_string()];
        if !group_threads {
            return ids;
        }
        let conv_id = self
            .mail_messages
            .iter()
            .find(|m| m.id == active_id)
            .or_else(|| self.pinned_mail.as_ref().filter(|m| m.id == active_id))
            .or_else(|| self.mail_detail_cache.get(active_id))
            .and_then(|m| m.conversation_id.as_deref())
            .filter(|s| !s.is_empty());
        let Some(cid) = conv_id else {
            return ids;
        };
        let mut thread_refs: Vec<&MailMessage> = self
            .mail_messages
            .iter()
            .filter(|m| m.conversation_id.as_deref() == Some(cid))
            .collect();
        thread_refs.sort_by(|a, b| a.received_at.cmp(&b.received_at));
        if let Some(latest) = thread_refs.last() {
            if !ids.iter().any(|id| id == &latest.id) {
                ids.push(latest.id.clone());
            }
        }
        for msg in thread_refs {
            if self.expanded_thread_ids.contains(&msg.id)
                && !ids.iter().any(|id| id == &msg.id)
            {
                ids.push(msg.id.clone());
            }
        }
        ids
    }

    fn load_mail_detail(&mut self, id: &str, request_hydrate: bool) {
        if !self.mail_detail_cache.contains_key(id) && !self.mail_detail_missing.contains(id) {
            match self.storage.get_mail_message(id) {
                Ok(Some(full)) => {
                    self.mail_detail_cache.insert(id.to_string(), full);
                }
                Ok(None) => {
                    self.mail_detail_missing.insert(id.to_string());
                }
                Err(_) => {}
            }
        }
        if request_hydrate {
            if let Some(msg) = self.mail_detail(id) {
                if !msg.body_loaded {
                    self.sync_ctrl.request(SyncCommand::HydrateMessage {
                        message_id: id.to_string(),
                    });
                }
            }
        }
    }

    pub(crate) fn sync_displayed_mail_details(
        &mut self,
        group_threads: bool,
        request_hydrate: bool,
    ) {
        let needed = self.displayed_mail_ids(group_threads);
        self.mail_detail_cache
            .retain(|id, _| needed.iter().any(|k| k == id));
        self.mail_detail_missing
            .retain(|id| needed.iter().any(|k| k == id));
        for id in needed {
            self.load_mail_detail(&id, request_hydrate);
        }
    }

    pub(crate) fn ensure_displayed_mail_details_local(&mut self, ui: &UiState) {
        let needed = self.displayed_mail_ids(ui.group_threads);
        for id in needed {
            self.load_mail_detail(&id, false);
        }
    }

    fn find_inbox_folder_id(&self) -> Option<String> {
        find_folder_by_wellknown(&self.mail_folders, "inbox", &["Inbox", "받은 편지함"])
            .map(|f| f.id.clone())
    }

    fn folder_matches_active(&self, event_folder_id: &str) -> bool {
        if self.active_folder_id == event_folder_id {
            return true;
        }
        if self.active_folder_id == VIRTUAL_UNREAD_FOLDER
            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
        {
            return true;
        }
        if event_folder_id.eq_ignore_ascii_case("inbox") {
            if let Some(inbox_id) = self.find_inbox_folder_id() {
                if self.active_folder_id == inbox_id {
                    return true;
                }
            }
        }
        false
    }

    fn preserve_pinned_mail_in_list(&mut self) {
        if let Some(pinned_id) = self.pinned_mail.as_ref().map(|p| p.id.clone()) {
            if let Ok(Some(fresh)) = self.storage.get_mail_message(&pinned_id) {
                self.pinned_mail = Some(fresh);
            }
        }
        if let Some(ref pinned) = self.pinned_mail {
            if self.active_mail_id.as_deref() == Some(pinned.id.as_str())
                && !self.mail_messages.iter().any(|m| m.id == pinned.id)
            {
                self.mail_messages.push(Self::mail_summary_from(pinned));
            }
        }
    }

    fn hydrate_active_mail_if_needed(&mut self, group_threads: bool) {
        self.sync_displayed_mail_details(group_threads, true);
    }

    pub(crate) fn sync_mail_list_selection_to_active(&self, ui: &mut UiState) {
        let Some(ref active_id) = self.active_mail_id else {
            return;
        };
        let rows = self.visible_mail_rows(ui);
        if let Some(pos) = rows.iter().position(|r| {
            r.thread_msg_indices
                .iter()
                .any(|&i| self.mail_messages[i].id == *active_id)
        }) {
            ui.mail_list_scroll.selected_idx = Some(pos);
        }
    }

    pub fn visible_calendar_events(&self, ui: &UiState) -> Vec<CalendarEvent> {
        let today = Local::now().date_naive();
        let mut list: Vec<CalendarEvent> = self
            .calendar_events
            .iter()
            .filter(|ev| {
                let Some(d) = super::calendar_view::event_local_date(&ev.start_iso) else {
                    return matches!(ui.cal_range, CalendarRangeFilter::AllUpcoming);
                };
                match ui.cal_range {
                    CalendarRangeFilter::Today => d == today,
                    CalendarRangeFilter::Tomorrow => d == today + Duration::days(1),
                    CalendarRangeFilter::Next7Days => d >= today && d <= today + Duration::days(7),
                    CalendarRangeFilter::ThisMonth => {
                        d.year() == today.year() && d.month() == today.month()
                    }
                    CalendarRangeFilter::AllUpcoming => true,
                    CalendarRangeFilter::SpecificDate(target) => d == target,
                }
            })
            .cloned()
            .collect();
        list.sort_by(|a, b| a.start_iso.cmp(&b.start_iso));
        list
    }

    pub fn active_event(&self) -> Option<&CalendarEvent> {
        let id = self.active_event_id.as_deref()?;
        self.calendar_events.iter().find(|e| e.id == id)
    }

    pub fn active_todo_task(&self) -> Option<&TodoTask> {
        let id = self.active_todo_task_id.as_deref()?;
        self.todo_tasks.iter().find(|t| t.id == id)
    }

    pub fn spawn_bootstrap(&mut self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Syncing Outlook".to_string();
        ui.tasks.begin(task_label.clone());
        self.syncing = true;
        self.sync_summary = "Syncing...".to_string();

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let _ = crate::sync::bootstrap_outlook(&graph, &storage).await;
            let (tok, me_res) = tokio::join!(auth.current_tokens(), graph.get_me());
            let scope_warning = tok.as_ref().and_then(|t| {
                auth::diagnose_access_token_scopes(&t.access_token)
                    .warnings
                    .into_iter()
                    .next()
            });
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.email.clone())
                    .await;
            }

            let read_st = storage.clone();
            let (folders, msgs, events, contacts, lists, tasks) = tokio::task::spawn_blocking(move || {
                let folders = read_st.list_mail_folders().unwrap_or_default();
                let inbox_id =
                    find_folder_by_wellknown(&folders, "inbox", &["Inbox", "받은 편지함"])
                        .map(|f| f.id.clone())
                        .unwrap_or_else(|| "inbox".to_string());
                let msgs = read_st
                    .list_mail_summaries_filtered(Some(&inbox_id), false, false, false, 150)
                    .unwrap_or_default();
                let events = read_st
                    .list_calendar_events(None, None, 200)
                    .unwrap_or_default();
                let contacts = read_st.list_contacts(None, 200).unwrap_or_default();
                let lists = read_st.list_todo_lists().unwrap_or_default();
                let def_list_id = lists
                    .iter()
                    .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
                    .or_else(|| lists.first())
                    .map(|l| l.id.clone());
                let tasks = match def_list_id {
                    Some(ref lid) => read_st.list_todo_tasks(Some(lid), true).unwrap_or_default(),
                    None => Vec::new(),
                };
                (folders, msgs, events, contacts, lists, tasks)
            })
            .await
            .unwrap_or_default();

            let _ = tx.send(UiMsg::BootstrapDone {
                me,
                mail_folders: folders,
                mail_messages: msgs,
                calendar_events: events,
                contacts,
                todo_lists: lists,
                todo_tasks: tasks,
                scope_warning,
            });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn spawn_me_profile_if_needed(&self) {
        if self.my_profile.is_some() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (tok, me_res) = tokio::join!(auth.current_tokens(), graph.get_me());
            let scope_warning = tok.as_ref().and_then(|t| {
                auth::diagnose_access_token_scopes(&t.access_token)
                    .warnings
                    .into_iter()
                    .next()
            });
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.email.clone())
                    .await;
            }
            let _ = tx.send(UiMsg::ProfileLoaded { me, scope_warning });
        });
    }

    pub fn active_view_for_tab(tab: usize) -> ActiveView {
        match tab {
            TAB_CALENDAR => ActiveView::Calendar,
            TAB_TASKS => ActiveView::Tasks,
            _ => ActiveView::Mail,
        }
    }

    pub fn push_focus_to_sync(&self) {
        let folder_id = if self.active_folder_id == VIRTUAL_UNREAD_FOLDER
            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
        {
            None
        } else {
            Some(self.active_folder_id.clone())
        };
        self.sync_ctrl.set_focus(FocusTarget {
            active_view: self.active_view,
            active_folder_id: folder_id,
            active_message_id: self.active_mail_id.clone(),
            active_todo_list_id: self.active_todo_list_id.clone(),
        });
    }

    /// Synchronizes the active tab's view immediately (used on tab switch and terminal FocusGained).
    /// Virtual mail folders (`Unread`, `Flagged`) reuse the existing Inbox folder delta scope.
    pub fn request_sync_for_active_view(&mut self, tab: usize) {
        let view = Self::active_view_for_tab(tab);
        self.active_view = view;
        self.push_focus_to_sync();
        match view {
            ActiveView::Mail => {
                let folder_id = if self.active_folder_id == VIRTUAL_UNREAD_FOLDER
                    || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
                {
                    self.find_inbox_folder_id()
                        .unwrap_or_else(|| "inbox".to_string())
                } else {
                    self.active_folder_id.clone()
                };
                self.sync_ctrl.request(SyncCommand::SyncFolder { folder_id });
            }
            ActiveView::Calendar => {
                self.sync_ctrl.request(SyncCommand::SyncCalendar);
            }
            ActiveView::Tasks => {
                self.sync_ctrl.request(SyncCommand::SyncTodo {
                    list_id: self.active_todo_list_id.clone(),
                });
            }
        }
    }

    pub fn request_contacts_sync(&self) {
        self.sync_ctrl.request(SyncCommand::SyncContacts);
    }

    pub fn select_mail_folder(&mut self, ui: &mut UiState, folder_id: String) {
        self.active_view = ActiveView::Mail;
        self.active_folder_id = folder_id.clone();
        self.pinned_mail = None;
        self.clear_mail_detail_cache();
        self.mail_oldest_reached = false;
        self.loading_older_mail = false;
        ui.mail_list_scroll = ScrollState::default();
        ui.mail_read_scroll = ScrollState::default();

        let (filter_folder, unread_only, flagged_only) = if folder_id == VIRTUAL_UNREAD_FOLDER {
            (None, true, false)
        } else if folder_id == VIRTUAL_FLAGGED_FOLDER {
            (None, false, true)
        } else {
            (Some(folder_id.as_str()), false, false)
        };

        self.mail_messages = self
            .storage
            .list_mail_summaries_filtered(filter_folder, unread_only, flagged_only, false, 150)
            .unwrap_or_default();

        self.select_first_visible_mail(ui);
        self.push_focus_to_sync();

        if folder_id != VIRTUAL_UNREAD_FOLDER && folder_id != VIRTUAL_FLAGGED_FOLDER {
            if self.mail_messages.is_empty() {
                self.loading_mail = true;
            }
            self.sync_ctrl.request(SyncCommand::SyncFolder { folder_id });
        }
    }

    pub fn select_first_visible_mail(&mut self, ui: &mut UiState) {
        let rows = self.visible_mail_rows(ui);
        if let Some(first) = rows.first() {
            ui.mail_list_scroll.selected_idx = Some(0);
            if let Some(msg_id) = self
                .mail_messages
                .get(first.primary_msg_idx)
                .map(|m| m.id.clone())
            {
                self.active_mail_id = Some(msg_id);
                self.sync_displayed_mail_details(ui.group_threads, true);
            }
        } else {
            ui.mail_list_scroll.selected_idx = None;
            self.active_mail_id = None;
            self.clear_mail_detail_cache();
        }
    }

    pub fn select_mail_row(&mut self, ui: &mut UiState, row_idx: usize) {
        let rows = self.visible_mail_rows(ui);
        if rows.is_empty() {
            ui.mail_list_scroll.selected_idx = None;
            self.active_mail_id = None;
            self.clear_mail_detail_cache();
            return;
        }
        let clamped = row_idx.min(rows.len() - 1);
        ui.mail_list_scroll.selected_idx = Some(clamped);
        ui.mail_read_scroll.offset = 0;

        if let Some(msg_id) = self
            .mail_messages
            .get(rows[clamped].primary_msg_idx)
            .map(|m| m.id.clone())
        {
            if self
                .pinned_mail
                .as_ref()
                .is_some_and(|p| p.id != msg_id)
            {
                self.pinned_mail = None;
            }
            self.active_mail_id = Some(msg_id);
            self.sync_displayed_mail_details(ui.group_threads, true);
            self.push_focus_to_sync();
        }

        // Auto-load older mail when reaching near bottom of folder list
        if clamped + 2 >= rows.len() {
            self.maybe_autoload_older_mail();
        }
    }

    pub fn maybe_autoload_older_mail(&mut self) {
        if self.loading_older_mail
            || self.mail_oldest_reached
            || self.active_folder_id == VIRTUAL_UNREAD_FOLDER
            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
        {
            return;
        }
        self.loading_older_mail = true;
        self.sync_ctrl.request(SyncCommand::LoadOlderMail {
            folder_id: self.active_folder_id.clone(),
            explicit: false,
        });
    }

    pub fn toggle_mail_read(&mut self, ui: &mut UiState) {
        let Some((id, new_read)) = self.active_mail().map(|m| (m.id.clone(), !m.is_read)) else {
            return;
        };
        if let Some(m) = self.mail_messages.iter_mut().find(|m| m.id == id) {
            m.is_read = new_read;
        }
        if let Some(m) = self.mail_detail_cache.get_mut(&id) {
            m.is_read = new_read;
        }
        if let Some(ref mut m) = self.pinned_mail {
            if m.id == id {
                m.is_read = new_read;
            }
        }
        let _ = self.storage.set_mail_read_local(&id, new_read);

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = graph.set_mail_read(&id, new_read).await {
                let _ = tx.send(UiMsg::TaskDone {
                    label: "Mark read".to_string(),
                    error: Some(err.to_string()),
                    toast: Some((format!("Failed to update read state: {err}"), ToastKind::Error)),
                });
            }
        });
        let _ = ui;
    }

    pub fn toggle_mail_flag(&mut self, ui: &mut UiState) {
        let Some((id, new_flag)) = self.active_mail().map(|m| (m.id.clone(), !m.flagged)) else {
            return;
        };
        if let Some(m) = self.mail_messages.iter_mut().find(|m| m.id == id) {
            m.flagged = new_flag;
        }
        if let Some(m) = self.mail_detail_cache.get_mut(&id) {
            m.flagged = new_flag;
        }
        if let Some(ref mut m) = self.pinned_mail {
            if m.id == id {
                m.flagged = new_flag;
            }
        }
        let _ = self.storage.set_mail_flagged_local(&id, new_flag);
        ui.info(if new_flag { "Flagged" } else { "Unflagged" });

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = graph.set_mail_flag(&id, new_flag).await {
                let _ = tx.send(UiMsg::TaskDone {
                    label: "Flag email".to_string(),
                    error: Some(err.to_string()),
                    toast: Some((format!("Failed to update flag: {err}"), ToastKind::Error)),
                });
            }
        });
    }

    pub fn archive_selected_mail(&mut self, ui: &mut UiState) {
        let Some(id) = self.active_mail_id.clone() else {
            return;
        };
        let archive_id = find_folder_by_wellknown(&self.mail_folders, "archive", &["Archive", "보관함"])
            .map(|f| f.id.clone())
            .unwrap_or_else(|| "archive".to_string());

        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        if self.active_folder_id != archive_id {
            self.mail_messages.retain(|m| m.id != id);
            self.mail_detail_cache.remove(&id);
            self.mail_detail_missing.remove(&id);
            self.expanded_thread_ids.remove(&id);
            if self.pinned_mail.as_ref().is_some_and(|p| p.id == id) {
                self.pinned_mail = None;
            }
        }
        let _ = self.storage.move_mail_folder_local(&id, &archive_id);
        self.reselect_after_removal(ui, prev_sel);
        ui.success("Archived");

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.move_mail_message(&id, &archive_id).await {
                Ok(moved) => {
                    let old_id = id.clone();
                    let moved_for_db = moved.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        storage.replace_moved_mail_local(&old_id, &moved_for_db)
                    })
                    .await;
                    let _ = tx.send(UiMsg::MailMoved {
                        moves: vec![(id, moved)],
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: "Archive".to_string(),
                        error: Some(err.to_string()),
                        toast: Some((format!("Archive failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_selected_mail(&mut self, ui: &mut UiState) {
        let Some(id) = self.active_mail_id.clone() else {
            return;
        };
        let trash_id = find_folder_by_wellknown(
            &self.mail_folders,
            "deleteditems",
            &["Deleted Items", "Trash", "지운 편지함"],
        )
        .map(|f| f.id.clone())
        .unwrap_or_else(|| "deleteditems".to_string());
        let in_trash = self.active_folder_id == trash_id
            || self.active_folder_id.eq_ignore_ascii_case("deleteditems");

        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        self.mail_messages.retain(|m| m.id != id);
        self.mail_detail_cache.remove(&id);
        self.mail_detail_missing.remove(&id);
        self.expanded_thread_ids.remove(&id);
        if self.pinned_mail.as_ref().is_some_and(|p| p.id == id) {
            self.pinned_mail = None;
        }
        if in_trash {
            let _ = self.storage.delete_mail_messages_by_ids(std::slice::from_ref(&id));
        } else {
            let _ = self.storage.move_mail_folder_local(&id, &trash_id);
        }
        self.reselect_after_removal(ui, prev_sel);
        ui.info(if in_trash { "Deleted permanently" } else { "Moved to Trash" });

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if in_trash {
                if let Err(err) = graph.delete_mail_message(&id).await {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: "Delete email".to_string(),
                        error: Some(err.to_string()),
                        toast: Some((format!("Delete failed: {err}"), ToastKind::Error)),
                    });
                }
            } else {
                match graph.move_mail_message(&id, &trash_id).await {
                    Ok(moved) => {
                        let old_id = id.clone();
                        let moved_for_db = moved.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            storage.replace_moved_mail_local(&old_id, &moved_for_db)
                        })
                        .await;
                        let _ = tx.send(UiMsg::MailMoved {
                            moves: vec![(id, moved)],
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskDone {
                            label: "Delete email".to_string(),
                            error: Some(err.to_string()),
                            toast: Some((format!("Delete failed: {err}"), ToastKind::Error)),
                        });
                    }
                }
            }
        });
    }

    pub fn move_mail_to_folder(
        &mut self,
        ui: &mut UiState,
        message_ids: Vec<String>,
        destination_folder_id: String,
        destination_name: String,
    ) {
        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        if self.active_folder_id != destination_folder_id {
            let id_set: HashSet<&str> = message_ids.iter().map(String::as_str).collect();
            self.mail_messages
                .retain(|m| !id_set.contains(m.id.as_str()));
            self.mail_detail_cache
                .retain(|id, _| !id_set.contains(id.as_str()));
            self.mail_detail_missing
                .retain(|id| !id_set.contains(id.as_str()));
            self.expanded_thread_ids
                .retain(|id| !id_set.contains(id.as_str()));
            if self
                .pinned_mail
                .as_ref()
                .is_some_and(|p| id_set.contains(p.id.as_str()))
            {
                self.pinned_mail = None;
            }
        }
        for id in &message_ids {
            let _ = self
                .storage
                .move_mail_folder_local(id, &destination_folder_id);
        }
        self.reselect_after_removal(ui, prev_sel);
        ui.success(format!("Moved to {destination_name}"));

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let mut moves = Vec::with_capacity(message_ids.len());
            for id in &message_ids {
                match graph.move_mail_message(id, &destination_folder_id).await {
                    Ok(moved) => {
                        let st = storage.clone();
                        let old_id = id.clone();
                        let moved_for_db = moved.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st.replace_moved_mail_local(&old_id, &moved_for_db)
                        })
                        .await;
                        moves.push((id.clone(), moved));
                    }
                    Err(err) => {
                        if !moves.is_empty() {
                            let _ = tx.send(UiMsg::MailMoved { moves });
                        }
                        let _ = tx.send(UiMsg::TaskDone {
                            label: "Move email".to_string(),
                            error: Some(err.to_string()),
                            toast: Some((format!("Move failed: {err}"), ToastKind::Error)),
                        });
                        return;
                    }
                }
            }
            let _ = tx.send(UiMsg::MailMoved { moves });
        });
    }

    fn reselect_after_removal(&mut self, ui: &mut UiState, prev_sel: usize) {
        let rows = self.visible_mail_rows(ui);
        if rows.is_empty() {
            ui.mail_list_scroll.selected_idx = None;
            self.active_mail_id = None;
            self.clear_mail_detail_cache();
        } else {
            let next_idx = prev_sel.min(rows.len() - 1);
            self.select_mail_row(ui, next_idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_mail_compose(
        &mut self,
        ui: &mut UiState,
        mode: MailComposeMode,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body_markdown: String,
        attachments: Vec<PathBuf>,
        inline_images: Vec<InlineImageUpload>,
    ) {
        let draft_key = mode.draft_key();
        let (reply_to_id, forward_of_id) = match &mode {
            MailComposeMode::Reply { message_id } | MailComposeMode::ReplyAll { message_id } => {
                (Some(message_id.clone()), None)
            }
            MailComposeMode::Forward { message_id } => (None, Some(message_id.clone())),
            MailComposeMode::New => (None, None),
        };
        // Persist the outgoing draft BEFORE sending so send failures never lose user input;
        // the draft is removed only after Graph confirms send in `UiMsg::MailSent`.
        self.draft_store.set_mail(
            draft_key.clone(),
            MailDraftEntry {
                to: to.join(", "),
                cc: cc.join(", "),
                bcc: bcc.join(", "),
                subject: subject.clone(),
                body: body_markdown.clone(),
                reply_to_id,
                forward_of_id,
            },
        );
        self.persist_draft_store();

        let body_html = parser::compile_outgoing_mail_html(&body_markdown, &inline_images, None);
        let task_label = "Send email".to_string();
        ui.tasks.begin(task_label.clone());

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let has_extras =
            !cc.is_empty() || !bcc.is_empty() || !attachments.is_empty() || !inline_images.is_empty();

        tokio::spawn(async move {
            let res = match mode {
                MailComposeMode::Reply { message_id } if !has_extras => {
                    graph.reply_mail(&message_id, &body_html, false).await
                }
                MailComposeMode::ReplyAll { message_id } if !has_extras => {
                    graph.reply_mail(&message_id, &body_html, true).await
                }
                MailComposeMode::Forward { message_id } if !has_extras => {
                    graph.forward_mail(&message_id, &to, &body_html).await
                }
                _ => {
                    let outgoing = OutgoingMail {
                        subject,
                        body_html,
                        body_markdown: Some(body_markdown),
                        to,
                        cc,
                        bcc,
                        importance: None,
                        attachments,
                        inline_images,
                        in_reply_to_id: None,
                        reply_all: false,
                        forward_of_id: None,
                        draft_id: None,
                        save_to_sent_items: true,
                    };
                    graph.send_mail(&outgoing).await
                }
            };

            match res {
                Ok(()) => {
                    let _ = tx.send(UiMsg::MailSent { draft_key });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Email sent".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((
                            format!("Send failed (draft preserved): {err}"),
                            ToastKind::Error,
                        )),
                    });
                }
            }
        });
    }

    pub fn stash_compose_draft(&mut self, compose: &MailComposeOverlay) {
        let key = compose.mode.draft_key();
        if compose.has_unsent_content() {
            self.draft_store.set_mail(key, compose.to_draft_entry());
        } else {
            self.draft_store.remove_mail(&key);
        }
        self.persist_draft_store();
    }

    pub fn save_mail_draft(&mut self, ui: &mut UiState, compose: &MailComposeOverlay) {
        let key = compose.mode.draft_key();
        self.draft_store
            .set_mail(key.clone(), compose.to_draft_entry());
        self.persist_draft_store();
        ui.success("Draft saved locally");

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let body_html = parser::compile_outgoing_mail_html(
            compose.body.text(),
            &compose.body.pending_images,
            None,
        );
        let outgoing = OutgoingMail {
            subject: compose.subject.trim().to_string(),
            body_html,
            body_markdown: Some(compose.body.text().to_string()),
            to: compose.parsed_to(),
            cc: compose.parsed_cc(),
            bcc: compose.parsed_bcc(),
            importance: None,
            attachments: compose.attachments.clone(),
            inline_images: compose.body.pending_images.clone(),
            in_reply_to_id: None,
            reply_all: false,
            forward_of_id: None,
            draft_id: None,
            save_to_sent_items: false,
        };
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Ok(saved) = graph.save_draft(&outgoing).await {
                let _ = tx.send(UiMsg::MailDraftSaved {
                    draft_key: key,
                    saved,
                });
            }
        });
    }

    fn persist_draft_store(&self) {
        if tokio::runtime::Handle::try_current().is_ok() {
            let store = self.draft_store.clone();
            let lock = Arc::clone(&self.draft_save_lock);
            tokio::task::spawn_blocking(move || {
                let _guard = lock.lock();
                let _ = store.save();
            });
        } else {
            let _guard = self.draft_save_lock.lock();
            let _ = self.draft_store.save();
        }
    }

    pub fn download_mail_attachment(
        &mut self,
        ui: &mut UiState,
        msg_id: String,
        attach_index: usize,
    ) {
        let Some(msg) = self.resolve_mail_for_action(&msg_id) else {
            ui.info("Email not found");
            return;
        };
        let eff = parser::effective_mail_attachments(&msg);
        let Some(att) = eff.get(attach_index).cloned() else {
            ui.info("Attachment not found");
            return;
        };
        let att_name = if att.name.trim().is_empty() {
            "attachment".to_string()
        } else {
            att.name.clone()
        };
        let task_label = format!("Download {att_name}");
        ui.tasks.begin(task_label.clone());

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let dest = self.download_dir.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.download_mail_attachment(&msg_id, &att, &dest).await {
                Ok(file) => {
                    let msg = format!("Saved: {}", file.path.display());
                    let _ = tx.send(UiMsg::AttachmentDownloaded(file));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((msg, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Download failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn search_local_mail(&mut self, ui: &mut UiState, query: String) {
        self.search_query = query.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_gen_atomic.store(generation, Ordering::SeqCst);
        self.search_remote_inflight = false;
        self.search_remote_done = false;
        self.search_error = None;

        let trimmed_len = query.trim().chars().count();
        if trimmed_len == 0 {
            self.search_results.clear();
            self.searching = false;
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            self.search_results = self
                .storage
                .search_mail(&query, None, 100)
                .unwrap_or_default();
            self.searching = false;
            return;
        }

        self.searching = true;
        self.search_remote_inflight = trimmed_len >= 2;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let local_query = query.clone();
        let task_label = "Search mail".to_string();
        ui.tasks.begin(task_label.clone());
        tokio::spawn(async move {
            let q = local_query.clone();
            let hits = tokio::task::spawn_blocking(move || {
                storage.search_mail(&q, None, 100).unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let _ = tx.send(UiMsg::SearchResults {
                query: local_query,
                hits,
            });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });

        if trimmed_len >= 2 {
            let graph = self.graph.clone();
            let storage = self.storage.clone();
            let tx = self.ui_tx.clone();
            let gen_atomic = Arc::clone(&self.search_gen_atomic);
            let remote_query = query;
            let remote_label = "Remote mail search".to_string();
            ui.tasks.begin(remote_label.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(350)).await;
                if gen_atomic.load(Ordering::SeqCst) != generation {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: remote_label,
                        error: None,
                        toast: None,
                    });
                    return;
                }
                match graph.search_mail_remote(&remote_query, 50).await {
                    Ok(messages) => {
                        let st = storage.clone();
                        let to_store = messages.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = st.upsert_mail_messages(&to_store);
                        })
                        .await;
                        if gen_atomic.load(Ordering::SeqCst) == generation {
                            let _ = tx.send(UiMsg::RemoteSearchResults {
                                query: remote_query,
                                messages,
                            });
                        }
                        let _ = tx.send(UiMsg::TaskDone {
                            label: remote_label,
                            error: None,
                            toast: None,
                        });
                    }
                    Err(err) => {
                        let err_msg = err.to_string();
                        if gen_atomic.load(Ordering::SeqCst) == generation {
                            let _ = tx.send(UiMsg::RemoteSearchFailed {
                                query: remote_query,
                                error: err_msg.clone(),
                            });
                        }
                        let _ = tx.send(UiMsg::TaskDone {
                            label: remote_label,
                            error: Some(err_msg),
                            toast: None,
                        });
                    }
                }
            });
        }
    }

    pub fn search_remote_mail(&mut self, ui: &mut UiState, query: String) {
        self.search_query = query.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_gen_atomic.store(generation, Ordering::SeqCst);
        self.search_remote_done = false;
        self.search_error = None;

        if query.trim().is_empty() || tokio::runtime::Handle::try_current().is_err() {
            self.searching = false;
            self.search_remote_inflight = false;
            return;
        }
        self.searching = true;
        self.search_remote_inflight = true;
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let gen_atomic = Arc::clone(&self.search_gen_atomic);
        let task_label = "Remote mail search".to_string();
        ui.tasks.begin(task_label.clone());
        tokio::spawn(async move {
            match graph.search_mail_remote(&query, 50).await {
                Ok(messages) => {
                    let st = storage.clone();
                    let to_store = messages.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = st.upsert_mail_messages(&to_store);
                    })
                    .await;
                    if gen_atomic.load(Ordering::SeqCst) == generation {
                        let _ = tx.send(UiMsg::RemoteSearchResults { query, messages });
                    }
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let err_msg = err.to_string();
                    if gen_atomic.load(Ordering::SeqCst) == generation {
                        let _ = tx.send(UiMsg::RemoteSearchFailed {
                            query,
                            error: err_msg.clone(),
                        });
                    }
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err_msg.clone()),
                        toast: Some((format!("Search failed: {err_msg}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn respond_calendar_event(
        &mut self,
        ui: &mut UiState,
        event_id: String,
        action: EventRsvpAction,
    ) {
        let status_str = match action {
            EventRsvpAction::Accept => "accepted",
            EventRsvpAction::TentativelyAccept => "tentativelyAccepted",
            EventRsvpAction::Decline => "declined",
        };
        if let Some(ev) = self.calendar_events.iter_mut().find(|e| e.id == event_id) {
            ev.response_status = status_str.to_string();
        }
        for m in &mut self.mail_messages {
            if let Some(ref mut mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str()) {
                    mr.response_status = Some(status_str.to_string());
                }
            }
        }
        for m in self.mail_detail_cache.values_mut() {
            if let Some(ref mut mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str()) {
                    mr.response_status = Some(status_str.to_string());
                }
            }
        }
        if let Some(ref mut m) = self.pinned_mail {
            if let Some(ref mut mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str()) {
                    mr.response_status = Some(status_str.to_string());
                }
            }
        }
        let _ = self
            .storage
            .update_event_response_local(&event_id, status_str);
        ui.success(format!("RSVP: {status_str}"));

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Calendar RSVP".to_string();
        ui.tasks.begin(task_label.clone());
        tokio::spawn(async move {
            match graph.respond_calendar_event(&event_id, action, None).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::CalendarRsvpDone {
                        event_id,
                        response_status: status_str.to_string(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("RSVP failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_calendar_event(&mut self, ui: &mut UiState, ev: OutgoingEvent) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Create event".to_string();
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.create_calendar_event(&ev).await {
                Ok(created) => {
                    let st = storage.clone();
                    let c2 = created.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = st.upsert_calendar_events(std::slice::from_ref(&c2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::CalendarEventCreated(created));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Event created".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Event creation failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_selected_calendar_event(&mut self, ui: &mut UiState) {
        let Some(id) = self.active_event_id.clone() else {
            return;
        };
        self.calendar_events.retain(|e| e.id != id);
        let _ = self.storage.delete_calendar_event(&id);
        let visible = self.visible_calendar_events(ui);
        self.active_event_id = visible.first().map(|e| e.id.clone());
        ui.cal_list_scroll.selected_idx = (!visible.is_empty()).then_some(0);
        ui.info("Event deleted");

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.delete_calendar_event(&id).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::CalendarEventDeleted(id));
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: "Delete event".to_string(),
                        error: Some(err.to_string()),
                        toast: Some((format!("Delete failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn select_todo_list(&mut self, ui: &mut UiState, list_id: String) {
        self.active_view = ActiveView::Tasks;
        self.active_todo_list_id = Some(list_id.clone());
        ui.todo_list_scroll = ScrollState::default();
        ui.todo_detail_scroll = ScrollState::default();
        self.todo_tasks = self
            .storage
            .list_todo_tasks(Some(&list_id), true)
            .unwrap_or_default();
        self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
        if !self.todo_tasks.is_empty() {
            ui.todo_list_scroll.selected_idx = Some(0);
        }
        self.push_focus_to_sync();
        self.sync_ctrl.request(SyncCommand::SyncTodo {
            list_id: Some(list_id),
        });
    }

    pub fn toggle_todo_completed(&mut self, ui: &mut UiState) {
        let Some(task_id) = self.active_todo_task_id.clone() else {
            return;
        };
        let Some(task) = self.todo_tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        let completed = !task.is_completed();
        task.status = if completed {
            "completed".to_string()
        } else {
            "notStarted".to_string()
        };
        let list_id = task.list_id.clone();
        let _ = self.storage.set_todo_task_completed_local(&task_id, completed);

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph
                .set_todo_task_completed(&list_id, &task_id, completed)
                .await
            {
                Ok(updated) => {
                    let u2 = updated.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_todo_tasks(std::slice::from_ref(&u2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoTaskUpdated(updated));
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: "Update task".to_string(),
                        error: Some(err.to_string()),
                        toast: Some((format!("Task update failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
        let _ = ui;
    }

    pub fn create_todo_task(&mut self, ui: &mut UiState, list_id: String, title: String) {
        let target_list = if list_id.trim().is_empty() {
            self.active_todo_list_id
                .clone()
                .unwrap_or_else(|| "Tasks".to_string())
        } else {
            list_id
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Create task".to_string();
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let outgoing = OutgoingTodoTask {
                title,
                body_plain: None,
                due_date_iso: None,
                importance: None,
            };
            match graph.create_todo_task(&target_list, &outgoing).await {
                Ok(created) => {
                    let c2 = created.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_todo_tasks(std::slice::from_ref(&c2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoTaskCreated(created));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Task added".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Add task failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_todo_list(&mut self, ui: &mut UiState, display_name: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Create task list".to_string();
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.create_todo_list(&display_name).await {
                Ok(created) => {
                    let c2 = created.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_todo_lists(std::slice::from_ref(&c2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoListCreated(created));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Task list created".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Create list failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_selected_todo_task(&mut self, ui: &mut UiState) {
        let Some(task_id) = self.active_todo_task_id.clone() else {
            return;
        };
        let list_id = self
            .todo_tasks
            .iter()
            .find(|t| t.id == task_id)
            .map(|t| t.list_id.clone())
            .or_else(|| self.active_todo_list_id.clone())
            .unwrap_or_default();

        self.todo_tasks.retain(|t| t.id != task_id);
        let _ = self.storage.delete_todo_task(&task_id);
        self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
        ui.todo_list_scroll.selected_idx = (!self.todo_tasks.is_empty()).then_some(0);
        ui.info("Task deleted");

        if tokio::runtime::Handle::try_current().is_err() || list_id.is_empty() {
            return;
        }
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.delete_todo_task(&list_id, &task_id).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::TodoTaskDeleted(task_id));
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: "Delete task".to_string(),
                        error: Some(err.to_string()),
                        toast: Some((format!("Delete failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Image Cache & Async Decode (inline halfblock + HD pixel preview)
    // ------------------------------------------------------------------

    pub fn image_cache_key(owner_id: &str, att_id: &str, cols: u16, rows: u16) -> String {
        format!("{owner_id}:{att_id}:{cols}x{rows}")
    }

    pub fn preview_image_cache_key(
        owner_id: &str,
        att_id: &str,
        cols: u16,
        rows: u16,
        protocol: TerminalImageProtocol,
    ) -> String {
        format!("{owner_id}:{att_id}:{cols}x{rows}:{}", protocol.as_str())
    }

    pub fn raw_image_cache_key(owner_id: &str, att_id: &str) -> String {
        format!("{owner_id}:{att_id}")
    }

    pub fn insert_raw_image_cache(&mut self, raw_key: String, bytes: Arc<[u8]>) {
        let entry_bytes = bytes.len();
        if entry_bytes > MAX_RAW_IMAGE_CACHE_BYTES {
            return;
        }
        if let Some(prev) = self.image_raw_cache.remove(&raw_key) {
            self.image_raw_bytes = self.image_raw_bytes.saturating_sub(prev.len());
            self.image_raw_order.retain(|k| k != &raw_key);
        }
        self.image_raw_cache.insert(raw_key.clone(), bytes);
        self.image_raw_order.push_back(raw_key);
        self.image_raw_bytes = self.image_raw_bytes.saturating_add(entry_bytes);

        while self.image_raw_order.len() > MAX_RAW_IMAGE_CACHE_ENTRIES {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
        while self.image_raw_bytes > MAX_RAW_IMAGE_CACHE_BYTES && self.image_raw_order.len() > 1 {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
    }

    fn touch_raw_image_cache(&mut self, raw_key: &str) -> Option<Arc<[u8]>> {
        let bytes = self.image_raw_cache.get(raw_key).cloned()?;
        self.image_raw_order.retain(|k| k != raw_key);
        self.image_raw_order.push_back(raw_key.to_string());
        Some(bytes)
    }

    fn rendered_image_bytes(rendered: &RenderedTerminalImage) -> usize {
        rendered.approx_heap_bytes().max(64)
    }

    pub fn insert_image_cache(&mut self, cache_key: String, rendered: RenderedTerminalImage) {
        const MAX_IMAGE_CACHE: usize = 32;
        self.image_pending.remove(&cache_key);
        self.clear_image_error(&cache_key);

        if let Some(prev) = self.image_cache.remove(&cache_key) {
            self.image_cache_bytes = self
                .image_cache_bytes
                .saturating_sub(Self::rendered_image_bytes(&prev));
            self.image_cache_order.retain(|k| k != &cache_key);
        }
        let entry_bytes = Self::rendered_image_bytes(&rendered);
        self.image_cache.insert(cache_key.clone(), rendered);
        self.image_cache_order.push_back(cache_key);
        self.image_cache_bytes = self.image_cache_bytes.saturating_add(entry_bytes);

        while self.image_cache_order.len() > MAX_IMAGE_CACHE {
            let Some(evicted) = self.image_cache_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_cache.remove(&evicted) {
                self.image_cache_bytes = self
                    .image_cache_bytes
                    .saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }
        while self.image_cache_bytes > img_preview::MAX_IMAGE_CACHE_BYTES
            && self.image_cache_order.len() > 1
        {
            let Some(evicted) = self.image_cache_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_cache.remove(&evicted) {
                self.image_cache_bytes = self
                    .image_cache_bytes
                    .saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn insert_image_error(&mut self, cache_key: String, error: String) {
        const MAX_IMAGE_ERRORS: usize = 64;
        if self.image_errors.contains_key(&cache_key) {
            self.image_errors_order.retain(|k| k != &cache_key);
        }
        self.image_errors.insert(cache_key.clone(), error);
        self.image_errors_order.push_back(cache_key);
        while self.image_errors_order.len() > MAX_IMAGE_ERRORS {
            if let Some(evicted) = self.image_errors_order.pop_front() {
                self.image_errors.remove(&evicted);
            }
        }
    }

    pub fn clear_image_error(&mut self, cache_key: &str) {
        if self.image_errors.remove(cache_key).is_some() {
            self.image_errors_order.retain(|k| k != cache_key);
        }
    }

    pub fn clear_image_caches(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
        self.image_errors.clear();
        self.image_errors_order.clear();
        self.image_raw_cache.clear();
        self.image_raw_order.clear();
        self.image_raw_bytes = 0;
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_mail_image_decode(
        &mut self,
        key: String,
        msg_id: String,
        att: MailAttachment,
        cols: u16,
        rows: u16,
        bg: (u8, u8, u8),
        caps: Option<TerminalGraphicsCaps>,
    ) {
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.image_pending.insert(key.clone());
        let raw_key = Self::raw_image_cache_key(&msg_id, &att.id);
        let cached_raw = self.touch_raw_image_cache(&raw_key);
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let att_id = att.id.clone();

        tokio::spawn(async move {
            let from_preview = caps.is_some();
            let att_title = if att.name.trim().is_empty() {
                att_id.clone()
            } else {
                att.name.clone()
            };
            let fetched: anyhow::Result<(Arc<[u8]>, Option<String>, bool)> = match cached_raw {
                Some(bytes) => {
                    let mime = (!att.content_type.trim().is_empty())
                        .then(|| att.content_type.clone());
                    Ok((bytes, mime, false))
                }
                None => {
                    if let Some(b64) = att
                        .content_bytes_b64
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                        match B64_STANDARD.decode(cleaned) {
                            Ok(bytes) => {
                                let mime = (!att.content_type.trim().is_empty())
                                    .then(|| att.content_type.clone());
                                Ok((Arc::from(bytes), mime, true))
                            }
                            Err(e) => Err(anyhow::anyhow!("Invalid base64 image: {e}")),
                        }
                    } else if att.id.starts_with("https://") || att.id.starts_with("http://") {
                        match graph.http().get(&att.id).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                let resp_ct = resp
                                    .headers()
                                    .get(reqwest::header::CONTENT_TYPE)
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .split(';')
                                    .next()
                                    .unwrap_or("")
                                    .trim()
                                    .to_ascii_lowercase();
                                if resp_ct == "image/svg+xml" || resp_ct.contains("svg") {
                                    Err(anyhow::anyhow!(
                                        "SVG vector images ({resp_ct}) are not supported for raster terminal preview"
                                    ))
                                } else {
                                    match resp.bytes().await {
                                        Ok(b) => {
                                            let mime = (!resp_ct.is_empty()).then_some(resp_ct);
                                            Ok((Arc::from(b.to_vec()), mime, true))
                                        }
                                        Err(e) => Err(anyhow::anyhow!(e)),
                                    }
                                }
                            }
                            Ok(resp) => Err(anyhow::anyhow!("HTTP {}", resp.status())),
                            Err(e) => Err(anyhow::anyhow!(e)),
                        }
                    } else {
                        match graph.fetch_mail_attachment_bytes(&msg_id, &att.id).await {
                            Ok((_, ct, bytes)) => {
                                let mime = (!ct.trim().is_empty())
                                    .then_some(ct)
                                    .or_else(|| {
                                        (!att.content_type.trim().is_empty())
                                            .then(|| att.content_type.clone())
                                    });
                                Ok((Arc::from(bytes), mime, true))
                            }
                            Err(err) => Err(err),
                        }
                    }
                }
            };

            match fetched {
                Ok((bytes, mime_hint, is_fresh)) => {
                    let kind = bitefoundry_html::sniff_content_kind(
                        mime_hint.as_deref(),
                        Some(&att.name),
                        Some(&bytes),
                    );
                    if kind == bitefoundry_html::ContentKind::Html {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = tx.send(UiMsg::ImageFetchReturnedHtml {
                            cache_key: key,
                            owner_id: msg_id,
                            att_id,
                            title: att_title,
                            html,
                            from_preview,
                        });
                        return;
                    }
                    if kind == bitefoundry_html::ContentKind::Svg {
                        let _ = tx.send(UiMsg::ImageDecodeFailed {
                            cache_key: key,
                            owner_id: msg_id,
                            att_id,
                            error: "SVG vector images are not supported for raster terminal preview"
                                .to_string(),
                        });
                        return;
                    }
                    if is_fresh {
                        let _ = tx.send(UiMsg::ImageBytesFetched {
                            raw_key,
                            bytes: Arc::clone(&bytes),
                        });
                    }
                    let decode_res = tokio::task::spawn_blocking(move || match caps {
                        Some(c) => img_preview::decode_and_render_for_protocol(&bytes, cols, rows, bg, c),
                        None => img_preview::decode_and_render_halfblocks(&bytes, cols, rows, bg),
                    })
                    .await;
                    match decode_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::ImageDecoded {
                                cache_key: key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageDecodeFailed {
                        cache_key: key,
                        owner_id: msg_id,
                        att_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_mail_image(
        &mut self,
        msg_id: String,
        att: MailAttachment,
        cols: u16,
        rows: u16,
    ) {
        let key = Self::image_cache_key(&msg_id, &att.id, cols, rows);
        self.spawn_mail_image_decode(key, msg_id, att, cols, rows, (20, 22, 26), None);
    }

    pub fn request_preview_image(
        &mut self,
        target: &PreviewTarget,
        cols: u16,
        rows: u16,
        caps: TerminalGraphicsCaps,
        bg_rgb: (u8, u8, u8),
    ) {
        match target {
            PreviewTarget::Mail { msg_id, attachment } => {
                let key = Self::preview_image_cache_key(
                    msg_id,
                    &attachment.id,
                    cols,
                    rows,
                    caps.protocol,
                );
                self.spawn_mail_image_decode(
                    key,
                    msg_id.clone(),
                    attachment.clone(),
                    cols,
                    rows,
                    bg_rgb,
                    Some(caps),
                );
            }
        }
    }

    pub fn open_mail_html_attachment(
        &mut self,
        ui: &mut UiState,
        msg_id: String,
        attach_index: usize,
    ) {
        let Some(msg) = self.resolve_mail_for_action(&msg_id) else {
            ui.info("Email not found");
            return;
        };
        let eff = parser::effective_mail_attachments(&msg);
        let Some(att) = eff.get(attach_index).cloned() else {
            ui.info("Attachment not found");
            return;
        };
        let title = if att.name.trim().is_empty() {
            "attachment.html".to_string()
        } else {
            att.name.clone()
        };

        if let Some(b64) = att
            .content_bytes_b64
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            match B64_STANDARD.decode(cleaned) {
                Ok(bytes) => {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    ui.close_overlays_of(OverlayKind::HtmlPreview);
                    ui.push_overlay(Overlay::html_preview(HtmlPreviewOverlay::new(
                        Some(title),
                        html,
                    )));
                    return;
                }
                Err(err) => {
                    ui.error(format!("Invalid HTML attachment encoding: {err}"));
                    return;
                }
            }
        }

        let raw_key = Self::raw_image_cache_key(&msg_id, &att.id);
        if let Some(bytes) = self.touch_raw_image_cache(&raw_key) {
            let html = String::from_utf8_lossy(&bytes).into_owned();
            ui.close_overlays_of(OverlayKind::HtmlPreview);
            ui.push_overlay(Overlay::html_preview(HtmlPreviewOverlay::new(
                Some(title),
                html,
            )));
            return;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot fetch HTML attachment without async runtime");
            return;
        }

        let task_label = format!("Load HTML {title}");
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res: anyhow::Result<Vec<u8>> =
                if att.id.starts_with("https://") || att.id.starts_with("http://") {
                    match graph.http().get(&att.id).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            resp.bytes().await.map(|b| b.to_vec()).map_err(Into::into)
                        }
                        Ok(resp) => Err(anyhow::anyhow!("HTTP {}", resp.status())),
                        Err(err) => Err(err.into()),
                    }
                } else {
                    graph
                        .fetch_mail_attachment_bytes(&msg_id, &att.id)
                        .await
                        .map(|(_, _, b)| b)
                };

            match res {
                Ok(bytes) => {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = tx.send(UiMsg::HtmlAttachmentLoaded { title, html });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((
                            format!("Failed to load HTML attachment: {err}"),
                            ToastKind::Error,
                        )),
                    });
                }
            }
        });
    }

    /// Serves an HTML document on the lazy localhost Axum server (`0 MB` idle) and returns its URL.
    pub async fn serve_html_document(&self, title: &str, html: &str) -> anyhow::Result<String> {
        self.html_server.serve_html(title, html).await
    }

    /// Serves `html` via `NativeHtmlServer::serve_html` and opens the localhost URL in the default browser.
    pub fn serve_html_in_browser(&self, ui: &mut UiState, title: String, html: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot start native HTML server without async runtime");
            return;
        }
        let task_label = "Serve HTML in browser".to_string();
        ui.tasks.begin(task_label.clone());
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match server.serve_html(&title, &html).await {
                Ok(url) => match open::that(&url) {
                    Ok(()) => {
                        let _ = tx.send(UiMsg::TaskDone {
                            label: task_label,
                            error: None,
                            toast: Some((
                                format!("Opened in browser: {url}"),
                                ToastKind::Success,
                            )),
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskDone {
                            label: task_label,
                            error: Some(err.to_string()),
                            toast: Some((
                                format!("Served at {url} (open failed: {err})"),
                                ToastKind::Error,
                            )),
                        });
                    }
                },
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((
                            format!("Failed to serve HTML: {err}"),
                            ToastKind::Error,
                        )),
                    });
                }
            }
        });
    }

    pub fn check_for_updates(&mut self, ui: &mut UiState, check_only: bool, force: bool) {
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        let task_label = if check_only {
            "Check updates".to_string()
        } else {
            "Install update".to_string()
        };
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match crate::updater::check_or_apply_update(&repo, check_only, force).await {
                Ok(res) => {
                    let toast_msg = if res.updated {
                        format!(
                            "Updated: {} -> {} (restart recommended)",
                            res.current_version, res.latest_version
                        )
                    } else if res.update_available {
                        format!(
                            "Update available: {} (current {})",
                            res.latest_version, res.current_version
                        )
                    } else {
                        format!("Up to date ({})", res.current_version)
                    };
                    let _ = tx.send(UiMsg::UpdateChecked(res));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((toast_msg, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Update check failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    fn emit_scope_warning_once(&mut self, ui: &mut UiState, warning: Option<String>) {
        if self.scope_warned {
            return;
        }
        if let Some(msg) = warning.filter(|s| !s.trim().is_empty()) {
            self.scope_warned = true;
            ui.info(msg);
        }
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, event: CoreEvent) {
        match event {
            CoreEvent::SyncStateChanged { busy, summary } => {
                if summary == POLL_RETRY_SUMMARY {
                    ui.poll_retrying = true;
                    self.syncing = false;
                } else {
                    if !busy && summary == "Synced" {
                        ui.poll_retrying = false;
                    }
                    self.syncing = busy;
                    self.sync_summary = summary;
                }
            }
            CoreEvent::SyncError(err) => {
                ui.error(format!("Sync error: {err}"));
            }
            CoreEvent::ProfileLoaded(profile) => {
                self.my_email = profile.email.clone().unwrap_or_default();
                self.my_display_name = profile.display_name.clone();
                self.my_profile = Some(profile);
            }
            CoreEvent::MailFoldersUpdated(folders) => {
                ui.poll_retrying = false;
                self.mail_folders = folders;
                if self.active_folder_id.eq_ignore_ascii_case("inbox") {
                    if let Some(inbox_id) = self.find_inbox_folder_id() {
                        if inbox_id != self.active_folder_id {
                            self.active_folder_id = inbox_id.clone();
                            if let Ok(loaded) = self.storage.list_mail_summaries_filtered(
                                Some(&inbox_id),
                                false,
                                false,
                                false,
                                150,
                            ) {
                                if !loaded.is_empty() {
                                    self.mail_messages = loaded;
                                    self.clear_mail_detail_cache();
                                    self.preserve_pinned_mail_in_list();
                                }
                            }
                            if self.active_mail().is_none() {
                                self.select_first_visible_mail(ui);
                            } else {
                                self.hydrate_active_mail_if_needed(ui.group_threads);
                            }
                            self.sync_mail_list_selection_to_active(ui);
                            self.push_focus_to_sync();
                        }
                    }
                }
            }
            CoreEvent::MailMessagesUpdated {
                folder_id,
                messages,
                notify_candidate,
            } => {
                ui.poll_retrying = false;
                if self.folder_matches_active(&folder_id) {
                    self.loading_mail = false;
                    self.loading_older_mail = false;
                    let limit = self.mail_messages.len().max(messages.len()).max(150);
                    let (filter_folder, unread_only, flagged_only) =
                        if self.active_folder_id == VIRTUAL_UNREAD_FOLDER {
                            (None, true, false)
                        } else if self.active_folder_id == VIRTUAL_FLAGGED_FOLDER {
                            (None, false, true)
                        } else {
                            (Some(self.active_folder_id.as_str()), false, false)
                        };
                    if let Ok(loaded) = self.storage.list_mail_summaries_filtered(
                        filter_folder,
                        unread_only,
                        flagged_only,
                        false,
                        limit,
                    ) {
                        self.mail_messages = loaded;
                    } else if !messages.is_empty()
                        && self.active_folder_id != VIRTUAL_UNREAD_FOLDER
                        && self.active_folder_id != VIRTUAL_FLAGGED_FOLDER
                    {
                        self.mail_messages = messages;
                    }
                    self.clear_mail_detail_cache();
                    self.preserve_pinned_mail_in_list();
                    if self.active_mail().is_none() {
                        self.select_first_visible_mail(ui);
                    } else {
                        self.hydrate_active_mail_if_needed(ui.group_threads);
                    }
                    self.sync_mail_list_selection_to_active(ui);
                }
                if let Some(candidate) = notify_candidate {
                    self.maybe_notify(ui, &candidate);
                }
            }
            CoreEvent::MailMessageHydrated(hydrated) => {
                let hydrated = *hydrated;
                let summary = Self::mail_summary_from(&hydrated);
                let mut found_in_list = false;
                if let Some(existing) = self
                    .mail_messages
                    .iter_mut()
                    .find(|m| m.id == hydrated.id)
                {
                    *existing = summary.clone();
                    found_in_list = true;
                }
                if let Some(ref mut pinned) = self.pinned_mail {
                    if pinned.id == hydrated.id {
                        *pinned = hydrated.clone();
                    }
                }
                let is_displayed = self
                    .displayed_mail_ids(ui.group_threads)
                    .iter()
                    .any(|id| id == &hydrated.id)
                    || self.mail_detail_cache.contains_key(&hydrated.id);
                if !found_in_list && self.active_mail_id.as_deref() == Some(hydrated.id.as_str()) {
                    self.mail_messages.insert(0, summary);
                    self.cache_loaded_mail_detail(hydrated);
                } else if is_displayed {
                    self.cache_loaded_mail_detail(hydrated);
                }
            }
            CoreEvent::CalendarEventsUpdated(events) => {
                ui.poll_retrying = false;
                self.calendar_events = events;
                let vis = self.visible_calendar_events(ui);
                if self.active_event().is_none() {
                    self.active_event_id = vis.first().map(|e| e.id.clone());
                    ui.cal_list_scroll.selected_idx = (!vis.is_empty()).then_some(0);
                } else if let Some(ref active_id) = self.active_event_id {
                    ui.cal_list_scroll.selected_idx = vis.iter().position(|e| e.id == *active_id);
                }
            }
            CoreEvent::ContactsUpdated(contacts) => {
                self.contacts = contacts;
            }
            CoreEvent::TodoListsUpdated(lists) => {
                self.todo_lists = lists;
                if self.active_todo_list_id.is_none() {
                    self.active_todo_list_id = self
                        .todo_lists
                        .iter()
                        .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
                        .or_else(|| self.todo_lists.first())
                        .map(|l| l.id.clone());
                }
            }
            CoreEvent::TodoTasksUpdated { list_id, tasks } => {
                if self.active_todo_list_id.is_none() {
                    self.active_todo_list_id = Some(list_id.clone());
                }
                if self.active_todo_list_id.as_deref() == Some(list_id.as_str()) {
                    self.todo_tasks = tasks;
                    if self.active_todo_task().is_none() {
                        self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
                        ui.todo_list_scroll.selected_idx =
                            (!self.todo_tasks.is_empty()).then_some(0);
                    } else if let Some(ref active_id) = self.active_todo_task_id {
                        ui.todo_list_scroll.selected_idx =
                            self.todo_tasks.iter().position(|t| t.id == *active_id);
                    }
                }
            }
        }
    }

    fn maybe_notify(&self, ui: &UiState, candidate: &NotificationCandidate) {
        let is_self = !self.my_email.is_empty()
            && candidate
                .sender
                .to_ascii_lowercase()
                .contains(&self.my_email.to_ascii_lowercase());
        if !notification::should_notify(ui.notify, false, is_self) {
            return;
        }
        let body = notification::chat_body(&candidate.subject, &candidate.preview);
        notification::show(&candidate.sender, &body);
    }

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BootstrapDone {
                me,
                mail_folders,
                mail_messages,
                calendar_events,
                contacts,
                todo_lists,
                todo_tasks,
                scope_warning,
            } => {
                if let Some(u) = me {
                    self.my_email = u.email.clone().unwrap_or_default();
                    self.my_display_name = u.display_name.clone();
                    self.my_profile = Some(u);
                }
                if !mail_folders.is_empty() {
                    self.mail_folders = mail_folders;
                    if self.active_folder_id.eq_ignore_ascii_case("inbox") {
                        if let Some(inbox_id) = self.find_inbox_folder_id() {
                            self.active_folder_id = inbox_id;
                        }
                    }
                }
                if !mail_messages.is_empty() {
                    self.mail_messages = mail_messages;
                    self.clear_mail_detail_cache();
                } else if self.mail_messages.is_empty()
                    && !self.active_folder_id.eq_ignore_ascii_case("inbox")
                {
                    if let Ok(loaded) = self.storage.list_mail_summaries_filtered(
                        Some(&self.active_folder_id),
                        false,
                        false,
                        false,
                        150,
                    ) {
                        if !loaded.is_empty() {
                            self.mail_messages = loaded;
                            self.clear_mail_detail_cache();
                        }
                    }
                }
                self.preserve_pinned_mail_in_list();
                if !calendar_events.is_empty() {
                    self.calendar_events = calendar_events;
                    if self.active_event_id.is_none() {
                        self.active_event_id = self.calendar_events.first().map(|e| e.id.clone());
                    }
                }
                if !contacts.is_empty() {
                    self.contacts = contacts;
                }
                if !todo_lists.is_empty() {
                    self.todo_lists = todo_lists;
                    if self.active_todo_list_id.is_none() {
                        self.active_todo_list_id = self
                            .todo_lists
                            .iter()
                            .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
                            .or_else(|| self.todo_lists.first())
                            .map(|l| l.id.clone());
                    }
                }
                if !todo_tasks.is_empty() {
                    self.todo_tasks = todo_tasks;
                    if self.active_todo_task_id.is_none() {
                        self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
                    }
                }
                self.syncing = false;
                self.sync_summary = "Synced".to_string();
                self.emit_scope_warning_once(ui, scope_warning);
                if self.active_mail().is_none() {
                    self.select_first_visible_mail(ui);
                } else {
                    self.hydrate_active_mail_if_needed(ui.group_threads);
                }
                self.sync_mail_list_selection_to_active(ui);
                self.push_focus_to_sync();
            }
            UiMsg::FolderMessagesLoaded {
                folder_id,
                messages,
            } => {
                if self.active_folder_id == folder_id {
                    self.loading_mail = false;
                    self.mail_messages = messages;
                    self.clear_mail_detail_cache();
                    self.preserve_pinned_mail_in_list();
                    if self.active_mail().is_none() {
                        self.select_first_visible_mail(ui);
                    } else {
                        self.hydrate_active_mail_if_needed(ui.group_threads);
                    }
                    self.sync_mail_list_selection_to_active(ui);
                }
            }
            UiMsg::MailDraftSaved { saved, .. } => {
                let _ = self
                    .storage
                    .upsert_mail_messages(std::slice::from_ref(&saved));
                self.mail_detail_cache.remove(&saved.id);
                self.mail_detail_missing.remove(&saved.id);
            }
            UiMsg::MailSent { draft_key } => {
                self.draft_store.remove_mail(&draft_key);
                self.persist_draft_store();
            }
            UiMsg::MailMoved { moves } => {
                if let Ok(folders) = self.storage.list_mail_folders() {
                    self.mail_folders = folders;
                }
                for (old_id, moved) in moves {
                    let moved_id = moved.id.clone();
                    if self.active_mail_id.as_deref() == Some(old_id.as_str()) {
                        self.active_mail_id = Some(moved_id.clone());
                    }
                    self.mail_detail_cache.remove(&old_id);
                    self.mail_detail_missing.remove(&old_id);
                    self.mail_detail_missing.remove(&moved_id);
                    if self.expanded_thread_ids.remove(&old_id) {
                        self.expanded_thread_ids.insert(moved_id.clone());
                    }
                    if self.pinned_mail.as_ref().is_some_and(|p| p.id == old_id) {
                        self.pinned_mail = Some(moved.clone());
                    }
                    let moved_in_active_folder = moved
                        .folder_id
                        .as_deref()
                        .is_some_and(|fid| self.folder_matches_active(fid));
                    if moved_in_active_folder {
                        let summary = Self::mail_summary_from(&moved);
                        if let Some(pos) = self.mail_messages.iter().position(|m| m.id == old_id) {
                            self.mail_messages[pos] = summary;
                            // Deduplicate if moved_id was already present
                            let mut seen_new = false;
                            self.mail_messages.retain(|m| {
                                if m.id == moved_id {
                                    if seen_new {
                                        return false;
                                    }
                                    seen_new = true;
                                }
                                true
                            });
                        } else if !self.mail_messages.iter().any(|m| m.id == moved_id) {
                            self.mail_messages.insert(0, summary);
                        }
                        if self.active_mail_id.as_deref() == Some(moved_id.as_str()) {
                            self.cache_loaded_mail_detail(moved);
                        }
                    } else {
                        self.mail_messages.retain(|m| m.id != old_id);
                    }
                }
            }
            UiMsg::MailDeleted { .. } | UiMsg::AttachmentDownloaded(_) => {}
            UiMsg::SearchResults { query, hits } => {
                if self.search_query.trim() == query.trim() {
                    if !self.search_remote_inflight {
                        self.searching = false;
                    }
                    if !self.search_remote_done {
                        self.search_results = hits;
                    } else {
                        let mut seen: HashSet<String> =
                            self.search_results.iter().map(|h| h.id.clone()).collect();
                        for hit in hits {
                            if seen.insert(hit.id.clone()) {
                                self.search_results.push(hit);
                            }
                        }
                        self.search_results.sort_by(|a, b| {
                            b.received_at
                                .cmp(&a.received_at)
                                .then_with(|| a.id.cmp(&b.id))
                        });
                    }
                }
            }
            UiMsg::RemoteSearchResults { query, messages } => {
                if self.search_query.trim() == query.trim() {
                    self.searching = false;
                    self.search_remote_inflight = false;
                    self.search_remote_done = true;
                    self.search_error = None;
                    let mut seen: HashSet<String> =
                        self.search_results.iter().map(|h| h.id.clone()).collect();
                    for m in messages {
                        if seen.insert(m.id.clone()) {
                            self.search_results.push(MailSearchHit {
                                id: m.id,
                                conversation_id: m.conversation_id,
                                folder_id: m.folder_id,
                                subject: m.subject,
                                from_display: m
                                    .from
                                    .as_ref()
                                    .map(|f| f.display())
                                    .unwrap_or_else(|| "(unknown)".to_string()),
                                received_at: m.received_at,
                                snippet: m.preview,
                                has_attachments: m.has_attachments,
                                is_read: m.is_read,
                                flagged: m.flagged,
                            });
                        }
                    }
                    self.search_results.sort_by(|a, b| {
                        b.received_at
                            .cmp(&a.received_at)
                            .then_with(|| a.id.cmp(&b.id))
                    });
                }
            }
            UiMsg::RemoteSearchFailed { query, error } => {
                if self.search_query.trim() == query.trim() {
                    self.searching = false;
                    self.search_remote_inflight = false;
                    self.search_remote_done = false;
                    self.search_error = Some(error);
                }
            }
            UiMsg::CalendarEventCreated(created) => {
                self.active_event_id = Some(created.id.clone());
                self.calendar_events.push(created);
                self.calendar_events
                    .sort_by(|a, b| a.start_iso.cmp(&b.start_iso));
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::SyncCalendar);
                }
            }
            UiMsg::CalendarEventDeleted(id) => {
                self.calendar_events.retain(|e| e.id != id);
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::SyncCalendar);
                }
            }
            UiMsg::CalendarRsvpDone {
                event_id,
                response_status,
            } => {
                if let Some(ev) = self.calendar_events.iter_mut().find(|e| e.id == event_id) {
                    ev.response_status = response_status;
                }
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::SyncCalendar);
                }
            }
            UiMsg::TodoListCreated(list) => {
                let lid = list.id.clone();
                self.todo_lists.push(list);
                self.select_todo_list(ui, lid);
            }
            UiMsg::TodoTasksLoaded { list_id, tasks } => {
                if self.active_todo_list_id.as_deref() == Some(list_id.as_str()) {
                    self.todo_tasks = tasks;
                }
            }
            UiMsg::TodoTaskCreated(task) => {
                if self.active_todo_list_id.as_deref() == Some(task.list_id.as_str()) {
                    self.active_todo_task_id = Some(task.id.clone());
                    self.todo_tasks.insert(0, task);
                    ui.todo_list_scroll.selected_idx = Some(0);
                }
            }
            UiMsg::TodoTaskUpdated(task) => {
                if let Some(existing) = self.todo_tasks.iter_mut().find(|t| t.id == task.id) {
                    *existing = task;
                }
            }
            UiMsg::TodoTaskDeleted(task_id) => {
                self.todo_tasks.retain(|t| t.id != task_id);
            }
            UiMsg::ClipboardImageEncoded {
                unix_ms,
                path,
                width,
                height,
                preview,
            } => {
                if let Some(Overlay::MailCompose(ref mut compose)) = ui.overlays.last_mut() {
                    let file_name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("outlook-pasted.png")
                        .to_string();
                    compose.attach_file(path);
                    compose.body.pending_preview = preview;
                    let prefix = if compose.body.has_no_text()
                        || compose.body.text().ends_with(['\n', ' '])
                    {
                        ""
                    } else {
                        " "
                    };
                    compose
                        .body
                        .insert_str(&format!("{prefix}[Image {unix_ms}]"));
                    compose.field = 4;
                    ui.success(format!(
                        "Attached clipboard image: {file_name} ({width}x{height})"
                    ));
                }
            }
            UiMsg::ImageDecoded {
                cache_key,
                rendered,
            } => {
                self.insert_image_cache(cache_key, rendered);
            }
            UiMsg::ImageBytesFetched { raw_key, bytes } => {
                self.insert_raw_image_cache(raw_key, bytes);
            }
            UiMsg::ImageDecodeFailed {
                cache_key,
                owner_id,
                att_id,
                error,
            } => {
                self.image_pending.remove(&cache_key);
                self.insert_image_error(cache_key, error.clone());
                if let Some(Overlay::ImagePreview(preview)) = ui.overlays.last_mut() {
                    if let Some(cur) = preview.current() {
                        if cur.owner_id() == owner_id && cur.att_id() == att_id {
                            preview.error = Some(error);
                        }
                    }
                }
            }
            UiMsg::ImageFetchReturnedHtml {
                cache_key,
                owner_id,
                att_id,
                title,
                html,
                from_preview,
            } => {
                self.image_pending.remove(&cache_key);
                self.insert_image_error(
                    cache_key,
                    "HTML content auto-routed to HTML Preview".to_string(),
                );
                let matching_preview = matches!(
                    ui.overlays.last(),
                    Some(Overlay::ImagePreview(preview))
                        if preview
                            .current()
                            .is_some_and(|c| c.owner_id() == owner_id && c.att_id() == att_id)
                );
                if matching_preview
                    || (from_preview
                        && matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_)) | None))
                {
                    ui.close_overlays_of(OverlayKind::ImagePreview);
                    ui.close_overlays_of(OverlayKind::HtmlPreview);
                    ui.push_overlay(Overlay::html_preview(HtmlPreviewOverlay::new(
                        Some(title),
                        html,
                    )));
                    ui.info("Received HTML content — opened in interactive HTML Preview");
                }
            }
            UiMsg::HtmlAttachmentLoaded { title, html } => {
                ui.close_overlays_of(OverlayKind::HtmlPreview);
                ui.push_overlay(Overlay::html_preview(HtmlPreviewOverlay::new(
                    Some(title),
                    html,
                )));
            }
            UiMsg::UpdateChecked(_) => {}
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.daemon_attached = true;
                self.daemon_pid = pid;
                self.syncing = sync_busy;
                self.sync_summary = "Synced".to_string();
                self.push_focus_to_sync();
                if self.my_profile.is_none() {
                    self.spawn_me_profile_if_needed();
                }
            }
            UiMsg::DaemonDetached => {
                self.daemon_attached = false;
                self.daemon_pid = None;
                self.push_focus_to_sync();
            }
            UiMsg::StartLocalBootstrap => {
                self.daemon_attached = false;
                self.daemon_pid = None;
                self.spawn_bootstrap(ui);
            }
            UiMsg::ProfileLoaded { me, scope_warning } => {
                if let Some(u) = me {
                    self.my_email = u.email.clone().unwrap_or_default();
                    self.my_display_name = u.display_name.clone();
                    self.my_profile = Some(u);
                }
                self.emit_scope_warning_once(ui, scope_warning);
            }
            UiMsg::TaskDone {
                label,
                error,
                toast,
            } => {
                ui.tasks.finish(&label, error);
                if let Some((text, kind)) = toast {
                    match kind {
                        ToastKind::Info => ui.toasts.info(text),
                        ToastKind::Success => ui.toasts.success(text),
                        ToastKind::Error => ui.toasts.error(text),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EmailRecipient;

    fn make_mail(id: &str, conv_id: &str, ts: &str, read: bool, flagged: bool) -> MailMessage {
        MailMessage {
            id: id.into(),
            internet_message_id: None,
            conversation_id: Some(conv_id.into()),
            folder_id: Some("inbox".into()),
            subject: format!("Subject {id}"),
            from: Some(EmailRecipient {
                name: Some("Alice".into()),
                address: "alice@contoso.com".into(),
            }),
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: ts.into(),
            sent_at: None,
            last_modified_at: None,
            is_read: read,
            is_draft: false,
            flagged,
            importance: "normal".into(),
            inference_classification: Some("focused".into()),
            has_attachments: false,
            body_loaded: true,
            preview: "Hello".into(),
            body_html: "<p>Hello</p>".into(),
            body_plain: "Hello".into(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        }
    }

    fn make_test_app() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    #[tokio::test]
    async fn mail_thread_grouping_and_optimistic_triage() {
        let (mut app, mut ui) = make_test_app();
        let m1 = make_mail("m1", "conv-A", "2026-11-19T08:00:00Z", true, false);
        let m2 = make_mail("m2", "conv-A", "2026-11-19T10:00:00Z", false, false);
        let m3 = make_mail("m3", "conv-B", "2026-11-19T09:00:00Z", false, true);
        app.mail_messages = vec![m1, m2, m3];
        app.storage.upsert_mail_messages(&app.mail_messages).unwrap();

        // Thread grouping ON -> 2 rows (conv-A with count 2, conv-B with count 1)
        ui.group_threads = true;
        let grouped = app.visible_mail_rows(&ui);
        assert_eq!(grouped.len(), 2);
        assert_eq!(app.mail_messages[grouped[0].primary_msg_idx].id, "m2");
        assert_eq!(grouped[0].count, 2);
        assert_eq!(grouped[0].unread_count, 1);

        // Thread grouping OFF -> 3 flat rows
        ui.group_threads = false;
        let flat = app.visible_mail_rows(&ui);
        assert_eq!(flat.len(), 3);

        // Optimistic read + flag + archive state
        app.active_mail_id = Some("m2".into());
        ui.mail_list_scroll.selected_idx = Some(0);
        app.toggle_mail_read(&mut ui);
        assert!(app.active_mail().unwrap().is_read);
        app.toggle_mail_flag(&mut ui);
        assert!(app.active_mail().unwrap().flagged);
        app.archive_selected_mail(&mut ui);
        assert!(!app.mail_messages.iter().any(|m| m.id == "m2"));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn calendar_rsvp_and_todo_completion_toggle_optimistic() {
        let (mut app, mut ui) = make_test_app();
        let ev = CalendarEvent {
            id: "ev-1".into(),
            subject: "Sync".into(),
            start_iso: "2026-11-19T14:00:00Z".into(),
            end_iso: "2026-11-19T15:00:00Z".into(),
            time_zone: "UTC".into(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: vec![],
            response_status: "notResponded".into(),
            is_online_meeting: false,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        };
        app.calendar_events = vec![ev.clone()];
        app.storage.upsert_calendar_events(&[ev]).unwrap();
        app.respond_calendar_event(&mut ui, "ev-1".into(), EventRsvpAction::Accept);
        assert_eq!(app.calendar_events[0].response_status, "accepted");

        let task = TodoTask {
            id: "t-1".into(),
            list_id: "list-1".into(),
            title: "Ship Outlook TUI".into(),
            status: "notStarted".into(),
            importance: "high".into(),
            body_plain: None,
            due_date_iso: None,
            reminder_iso: None,
            completed_at: None,
            created_at: None,
            updated_at: None,
        };
        app.todo_tasks = vec![task.clone()];
        app.active_todo_list_id = Some("list-1".into());
        app.active_todo_task_id = Some("t-1".into());
        app.storage.upsert_todo_tasks(&[task]).unwrap();
        app.toggle_todo_completed(&mut ui);
        assert!(app.todo_tasks[0].is_completed());
        app.toggle_todo_completed(&mut ui);
        assert!(!app.todo_tasks[0].is_completed());

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[test]
    fn jump_to_search_hit_preserves_pinned_mail_across_updates_and_triggers_hydration() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        ui.focused_only = true;

        let recent = make_mail("m-recent", "conv-r", "2026-11-19T10:00:00Z", true, false);
        let mut older = make_mail("m-old", "conv-old", "2024-06-01T00:00:00Z", false, false);
        older.folder_id = Some("f-inbox".into());
        older.inference_classification = Some("other".into());
        older.body_loaded = false;
        older.body_plain.clear();
        older.body_html.clear();

        app.storage
            .upsert_mail_messages(&[recent.clone(), older.clone()])
            .unwrap();
        app.search_query = "old".into();
        app.search_results = vec![MailSearchHit {
            id: "m-old".into(),
            conversation_id: Some("conv-old".into()),
            folder_id: Some("f-inbox".into()),
            subject: older.subject.clone(),
            from_display: "Alice".into(),
            received_at: older.received_at.clone(),
            snippet: "Hello".into(),
            has_attachments: false,
            is_read: false,
            flagged: false,
        }];

        let _ = crate::ui::action::dispatch(
            crate::ui::action::Action::JumpToSearchHit(0),
            &mut app,
            &mut ui,
        );

        assert!(!ui.focused_only, "JumpToSearchHit must unhide Other mail");
        assert_eq!(app.active_mail_id.as_deref(), Some("m-old"));
        assert_eq!(
            app.pinned_mail.as_ref().map(|m| m.id.as_str()),
            Some("m-old")
        );

        let mut saw_hydrate_old = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m-old")
            {
                saw_hydrate_old = true;
            }
        }
        assert!(saw_hydrate_old, "JumpToSearchHit must request hydration");

        // Simulate folder sync replacing list without older hit -> pinned_mail must survive!
        app.handle_ui_msg(
            &mut ui,
            UiMsg::FolderMessagesLoaded {
                folder_id: "f-inbox".into(),
                messages: vec![recent.clone()],
            },
        );
        assert_eq!(app.active_mail().map(|m| m.id.as_str()), Some("m-old"));
        assert!(app.mail_messages.iter().any(|m| m.id == "m-old"));

        // Simulate MailMessagesUpdated after older is deleted from top-N storage -> still preserved via pinned_mail!
        app.storage
            .delete_mail_messages_by_ids(&["m-old".into()])
            .unwrap();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailMessagesUpdated {
                folder_id: "f-inbox".into(),
                messages: vec![recent],
                notify_candidate: None,
            },
        );
        assert_eq!(app.active_mail().map(|m| m.id.as_str()), Some("m-old"));
        assert!(app.mail_messages.iter().any(|m| m.id == "m-old"));

        // Hydrating the pinned older message updates both mail_messages and pinned_mail.
        let mut hydrated = older.clone();
        hydrated.body_loaded = true;
        hydrated.body_plain = "Full older body".into();
        app.handle_core_event(&mut ui, CoreEvent::MailMessageHydrated(Box::new(hydrated)));
        assert_eq!(
            app.active_mail().map(|m| (m.body_loaded, m.body_plain.as_str())),
            Some((true, "Full older body"))
        );
        assert_eq!(
            app.pinned_mail
                .as_ref()
                .map(|m| (m.body_loaded, m.body_plain.as_str())),
            Some((true, "Full older body"))
        );
    }

    #[test]
    fn bootstrap_and_folders_updated_resolve_inbox_and_hydrate_cached_active_mail() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        let mut cached = make_mail("m-cached", "conv-c", "2026-11-19T09:00:00Z", false, false);
        cached.folder_id = Some("f-inbox-real".into());
        cached.body_loaded = false;
        app.mail_messages = vec![cached.clone()];
        app.active_mail_id = Some("m-cached".into());
        app.storage.upsert_mail_messages(&[cached.clone()]).unwrap();

        let inbox_folder = MailFolder {
            id: "f-inbox-real".into(),
            display_name: "받은 편지함".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 1,
        };
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailFoldersUpdated(vec![inbox_folder]),
        );

        assert_eq!(app.active_folder_id, "f-inbox-real");
        let mut hydrated_req = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m-cached")
            {
                hydrated_req = true;
            }
        }
        assert!(
            hydrated_req,
            "unhydrated active_mail_id must be hydrated when Inbox resolves"
        );
    }

    #[test]
    fn remote_search_results_merge_sort_and_failed_clears_searching() {
        let (mut app, mut ui) = {
            let cfg = AppConfig::default();
            let auth = AuthManager::new(cfg.clone()).unwrap();
            let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
            let storage = Storage::open_in_memory().unwrap();
            let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
            let (cmd_tx, _) = mpsc::unbounded_channel();
            let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
            let (ui_tx, _) = mpsc::unbounded_channel();
            (
                AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx),
                UiState::from_config(&cfg),
            )
        };

        app.search_query = "invoice".into();
        app.searching = true;
        app.search_results = vec![MailSearchHit {
            id: "m1".into(),
            conversation_id: None,
            folder_id: Some("f1".into()),
            subject: "Local invoice".into(),
            from_display: "Bob".into(),
            received_at: "2026-11-10T00:00:00Z".into(),
            snippet: "local".into(),
            has_attachments: false,
            is_read: true,
            flagged: false,
        }];

        let dup_local = make_mail("m1", "c1", "2026-11-10T00:00:00Z", true, false);
        let newer_remote = make_mail("m2", "c2", "2026-11-19T00:00:00Z", false, true);
        app.handle_ui_msg(
            &mut ui,
            UiMsg::RemoteSearchResults {
                query: "invoice".into(),
                messages: vec![dup_local, newer_remote],
            },
        );
        assert!(!app.searching);
        assert!(app.search_remote_done);
        assert!(app.search_error.is_none());
        assert_eq!(app.search_results.len(), 2);
        assert_eq!(app.search_results[0].id, "m2");
        assert_eq!(app.search_results[1].id, "m1");

        app.searching = true;
        app.handle_ui_msg(
            &mut ui,
            UiMsg::RemoteSearchFailed {
                query: "invoice".into(),
                error: "timeout".into(),
            },
        );
        assert!(!app.searching);
        assert!(!app.search_remote_done);
        assert_eq!(app.search_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn summary_list_lazy_loads_displayed_conversation_and_expanded_messages() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        app.storage
            .upsert_mail_folders(&[
                MailFolder {
                    id: "f-inbox".into(),
                    display_name: "Inbox".into(),
                    well_known_name: Some("inbox".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 1,
                    total_item_count: 4,
                },
                MailFolder {
                    id: "f-sent".into(),
                    display_name: "Sent Items".into(),
                    well_known_name: Some("sentitems".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 1,
                },
            ])
            .unwrap();

        let mut t1_unhydrated =
            make_mail("m-t1", "conv-thread", "2026-11-19T08:00:00Z", true, false);
        t1_unhydrated.folder_id = Some("f-inbox".into());
        t1_unhydrated.body_loaded = false;
        t1_unhydrated.body_html.clear();
        t1_unhydrated.body_plain.clear();

        let mut t2_local =
            make_mail("m-t2", "conv-thread", "2026-11-19T09:00:00Z", true, false);
        t2_local.folder_id = Some("f-inbox".into());
        t2_local.body_html = "<p>Local middle body</p>".into();
        t2_local.body_plain = "Local middle body".into();

        let mut t3_latest =
            make_mail("m-t3", "conv-thread", "2026-11-19T11:00:00Z", false, true);
        t3_latest.folder_id = Some("f-inbox".into());
        t3_latest.body_html = "<p>Latest thread body</p>".into();
        t3_latest.body_plain = "Latest thread body".into();

        let mut other = make_mail("m-other", "conv-other", "2026-11-19T10:00:00Z", true, false);
        other.folder_id = Some("f-inbox".into());
        other.body_html = "<p>Other conversation body</p>".into();
        other.body_plain = "Other conversation body".into();

        let mut sent = make_mail("m-sent", "conv-sent", "2026-11-19T12:00:00Z", true, false);
        sent.folder_id = Some("f-sent".into());
        sent.to_recipients = vec![
            EmailRecipient {
                name: Some("Bob".into()),
                address: "bob@contoso.com".into(),
            },
            EmailRecipient {
                name: Some("Carol".into()),
                address: "carol@contoso.com".into(),
            },
        ];
        sent.body_html = "<p>Sent body</p>".into();
        sent.body_plain = "Sent body".into();

        app.storage
            .upsert_mail_messages(&[t1_unhydrated, t2_local, t3_latest, other, sent])
            .unwrap();

        // Initial load keeps summaries in mail_messages and lazily loads ONLY the active message
        app.load_cached_initial();
        assert_eq!(app.mail_messages.len(), 4);
        assert!(app.mail_messages.iter().all(|m| m.body_html.is_empty()));
        assert_eq!(app.active_mail_id.as_deref(), Some("m-t3"));
        assert_eq!(app.mail_detail_cache.len(), 1);
        assert_eq!(
            app.active_mail().map(|m| m.body_html.as_str()),
            Some("<p>Latest thread body</p>")
        );
        // Local body_loaded == true must NOT request Graph hydration
        assert!(cmd_rx.try_recv().is_err());

        // Expanding a locally cached thread message loads it by ID without Graph hydration
        app.expanded_thread_ids.insert("m-t2".into());
        app.sync_displayed_mail_details(true, true);
        assert_eq!(app.mail_detail_cache.len(), 2);
        assert_eq!(
            app.mail_detail("m-t2").map(|m| m.body_html.as_str()),
            Some("<p>Local middle body</p>")
        );
        assert!(cmd_rx.try_recv().is_err());

        // Expanding an unhydrated thread message triggers Graph HydrateMessage
        app.expanded_thread_ids.insert("m-t1".into());
        app.sync_displayed_mail_details(true, true);
        let mut requested_t1 = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m-t1")
            {
                requested_t1 = true;
            }
        }
        assert!(requested_t1, "unhydrated expanded message must request Graph hydration");

        // Hydrating m-t1 caches full body in mail_detail_cache while mail_messages stays summary-only
        let mut t1_full = make_mail("m-t1", "conv-thread", "2026-11-19T08:00:00Z", true, false);
        t1_full.folder_id = Some("f-inbox".into());
        t1_full.body_html = "<p>Hydrated t1 body</p>".into();
        t1_full.body_plain = "Hydrated t1 body".into();
        t1_full.web_link = Some("https://outlook.office.com/mail/m-t1".into());
        app.handle_core_event(&mut ui, CoreEvent::MailMessageHydrated(Box::new(t1_full)));
        assert_eq!(
            app.mail_detail("m-t1").map(|m| m.body_html.as_str()),
            Some("<p>Hydrated t1 body</p>")
        );
        assert!(
            app.mail_messages
                .iter()
                .find(|m| m.id == "m-t1")
                .is_some_and(|m| m.body_loaded
                    && m.body_html.is_empty()
                    && m.web_link.as_deref() == Some("https://outlook.office.com/mail/m-t1")),
            "mail_messages must keep summary projection with web_link after hydration"
        );

        // Background hydration of an undisplayed message must NOT retain its body in RAM
        let mut other_full =
            make_mail("m-other", "conv-other", "2026-11-19T10:00:00Z", true, false);
        other_full.folder_id = Some("f-inbox".into());
        other_full.body_html = "<p>Prefetched undisplayed body</p>".into();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailMessageHydrated(Box::new(other_full)),
        );
        assert!(!app.mail_detail_cache.contains_key("m-other"));
        assert!(app
            .mail_messages
            .iter()
            .find(|m| m.id == "m-other")
            .unwrap()
            .body_html
            .is_empty());

        // Selecting the next conversation evicts old details and loads only m-other
        app.select_mail_row(&mut ui, 1);
        assert_eq!(app.active_mail_id.as_deref(), Some("m-other"));
        assert_eq!(app.mail_detail_cache.len(), 1);
        assert_eq!(
            app.active_mail().map(|m| m.body_html.as_str()),
            Some("<p>Other conversation body</p>")
        );

        // Switching to Sent retains first to_recipient on summaries and loads active full body
        app.select_mail_folder(&mut ui, "f-sent".into());
        assert_eq!(app.mail_messages.len(), 1);
        assert!(app.mail_messages[0].body_html.is_empty());
        assert_eq!(app.mail_messages[0].to_recipients.len(), 1);
        assert_eq!(app.mail_messages[0].to_recipients[0].short_display(), "Bob");
        assert_eq!(app.active_mail().unwrap().to_recipients.len(), 2);
        assert_eq!(app.active_mail().unwrap().body_html, "<p>Sent body</p>");

        // Virtual Unread & Flagged folders filter via SQLite summary projection
        app.select_mail_folder(&mut ui, VIRTUAL_UNREAD_FOLDER.into());
        assert_eq!(app.mail_messages.len(), 1);
        assert_eq!(app.mail_messages[0].id, "m-t3");
        assert!(app.mail_messages[0].body_html.is_empty());
        assert_eq!(app.active_mail().unwrap().body_html, "<p>Latest thread body</p>");

        app.select_mail_folder(&mut ui, VIRTUAL_FLAGGED_FOLDER.into());
        assert_eq!(app.mail_messages.len(), 1);
        assert_eq!(app.mail_messages[0].id, "m-t3");
    }

    #[test]
    fn summary_detail_cache_invalidates_and_stays_consistent_on_edits_and_sync() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        app.storage
            .upsert_mail_folders(&[MailFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 2,
                total_item_count: 2,
            }])
            .unwrap();

        let mut m1 = make_mail("m1", "c1", "2026-11-19T09:00:00Z", false, false);
        m1.folder_id = Some("f-inbox".into());
        m1.body_html = "<p>Body v1</p>".into();
        let mut m2 = make_mail("m2", "c2", "2026-11-19T10:00:00Z", false, false);
        m2.folder_id = Some("f-inbox".into());
        m2.body_html = "<p>Body m2</p>".into();
        app.storage
            .upsert_mail_messages(&[m1.clone(), m2.clone()])
            .unwrap();

        app.select_mail_folder(&mut ui, "f-inbox".into());
        assert_eq!(app.active_mail_id.as_deref(), Some("m2"));

        // Read & flag toggles update summary list, detail cache, and SQLite together
        app.toggle_mail_read(&mut ui);
        assert!(app.mail_messages.iter().find(|m| m.id == "m2").unwrap().is_read);
        assert!(app.active_mail().unwrap().is_read);
        assert!(app.storage.get_mail_message("m2").unwrap().unwrap().is_read);

        app.toggle_mail_flag(&mut ui);
        assert!(app.mail_messages.iter().find(|m| m.id == "m2").unwrap().flagged);
        assert!(app.active_mail().unwrap().flagged);
        assert!(app.storage.get_mail_message("m2").unwrap().unwrap().flagged);

        // Sync update invalidates cached detail and refreshes updated body from SQLite
        let mut m2_updated = m2.clone();
        m2_updated.is_read = true;
        m2_updated.flagged = true;
        m2_updated.body_html = "<p>Body m2 edited</p>".into();
        app.storage.upsert_mail_messages(&[m2_updated]).unwrap();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailMessagesUpdated {
                folder_id: "f-inbox".into(),
                messages: vec![],
                notify_candidate: None,
            },
        );
        assert!(app.mail_messages.iter().all(|m| m.body_html.is_empty()));
        assert_eq!(app.active_mail().unwrap().body_html, "<p>Body m2 edited</p>");

        // Archiving removes old detail and lazily loads next message
        app.archive_selected_mail(&mut ui);
        assert!(!app.mail_detail_cache.contains_key("m2"));
        assert_eq!(app.active_mail_id.as_deref(), Some("m1"));
        assert_eq!(app.active_mail().unwrap().body_html, "<p>Body v1</p>");

        // Deleting last message clears active selection and detail cache
        app.delete_selected_mail(&mut ui);
        assert!(app.active_mail_id.is_none());
        assert!(app.mail_detail_cache.is_empty());
    }

    #[tokio::test]
    async fn submit_mail_compose_preserves_draft_on_send_failure_and_clears_on_success() {
        use axum::{http::StatusCode, routing::post, Json, Router};
        use std::sync::atomic::{AtomicBool, Ordering};

        let should_fail = Arc::new(AtomicBool::new(true));
        let fail_c = Arc::clone(&should_fail);

        let app_router = Router::new().route(
            "/v1.0/me/sendMail",
            post(move || {
                let fail = fail_c.load(Ordering::SeqCst);
                async move {
                    if fail {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": { "code": "ErrorInternalServerError", "message": "SMTP failed" }
                            })),
                        )
                    } else {
                        (StatusCode::ACCEPTED, Json(serde_json::json!({})))
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app_router).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "synthetic-tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _) = mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        // 1. Send fails with 500 -> draft MUST remain preserved in draft_store
        app.submit_mail_compose(
            &mut ui,
            MailComposeMode::New,
            vec!["bob@example.com".into()],
            vec![],
            vec![],
            "Important Proposal".into(),
            "Do not lose this body text.".into(),
            vec![],
            vec![],
        );
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("received task failure")
            .unwrap();
        app.handle_ui_msg(&mut ui, msg);
        let saved_draft = app
            .draft_store
            .get_mail("compose:new")
            .expect("draft must be preserved after send failure");
        assert_eq!(saved_draft.subject, "Important Proposal");
        assert_eq!(saved_draft.body, "Do not lose this body text.");
        assert_eq!(saved_draft.to, "bob@example.com");

        // 2. Retry succeeds -> UiMsg::MailSent clears draft
        should_fail.store(false, Ordering::SeqCst);
        app.submit_mail_compose(
            &mut ui,
            MailComposeMode::New,
            vec!["bob@example.com".into()],
            vec![],
            vec![],
            "Important Proposal".into(),
            "Do not lose this body text.".into(),
            vec![],
            vec![],
        );
        for _ in 0..2 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
                .await
                .expect("received send completion")
                .unwrap();
            app.handle_ui_msg(&mut ui, msg);
        }
        assert!(
            app.draft_store.get_mail("compose:new").is_none(),
            "draft must be cleared once Graph send succeeds"
        );
        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn tui_move_replaces_old_id_and_real_folder_and_empty_updates_work() {
        use axum::{routing::post, Json, Router};

        let app_router = Router::new().route(
            "/v1.0/me/messages/m1/move",
            post(|| async {
                Json(serde_json::json!({
                    "id": "m1-moved",
                    "conversationId": "conv-1",
                    "parentFolderId": "f-archive",
                    "subject": "Move Target",
                    "receivedDateTime": "2026-11-19T09:00:00Z",
                    "isRead": false,
                    "bodyPreview": "Move body",
                    "body": { "contentType": "html", "content": "<p>Move body</p>" }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app_router).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "synthetic-tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _) = tokio::sync::watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        app.mail_folders = vec![
            MailFolder {
                id: "f-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 1,
            },
            MailFolder {
                id: "f-archive".into(),
                display_name: "Archive".into(),
                well_known_name: Some("archive".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 0,
            },
        ];
        app.storage.upsert_mail_folders(&app.mail_folders).unwrap();
        let mut m1 = make_mail("m1", "conv-1", "2026-11-19T09:00:00Z", false, false);
        m1.folder_id = Some("f-inbox".into());
        m1.subject = "Move Target".into();
        m1.body_plain = "Move body".into();
        app.storage.upsert_mail_messages(&[m1]).unwrap();
        app.select_mail_folder(&mut ui, "f-inbox".into());
        while nudge_rx.try_recv().is_ok() {}

        // Archive replaces old ID m1 -> m1-moved in SQLite + FTS without duplicates
        app.archive_selected_mail(&mut ui);
        let moved_msg = tokio::time::timeout(std::time::Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("received MailMoved")
            .unwrap();
        app.handle_ui_msg(&mut ui, moved_msg);
        assert!(app.storage.get_mail_message("m1").unwrap().is_none());
        let moved_row = app.storage.get_mail_message("m1-moved").unwrap().unwrap();
        assert_eq!(moved_row.folder_id.as_deref(), Some("f-archive"));
        let fts_hits = app.storage.search_mail("Move Target", None, 10).unwrap();
        assert_eq!(fts_hits.len(), 1);
        assert_eq!(fts_hits[0].id, "m1-moved");

        // MailMessagesUpdated with alias "inbox" still matches active real folder "f-inbox"
        // and updates list even when folder becomes empty!
        let mut m2 = make_mail("m2", "conv-2", "2026-11-19T10:00:00Z", false, false);
        m2.folder_id = Some("f-inbox".into());
        m2.body_loaded = false;
        app.storage.upsert_mail_messages(&[m2]).unwrap();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailMessagesUpdated {
                folder_id: "inbox".into(),
                messages: vec![],
                notify_candidate: None,
            },
        );
        assert_eq!(app.mail_messages.len(), 1);
        assert_eq!(app.mail_messages[0].id, "m2");
        // Since m2 has body_loaded == false, HydrateMessage was requested
        let mut saw_hydrate = false;
        while let Ok(cmd) = nudge_rx.try_recv() {
            if matches!(cmd, SyncCommand::HydrateMessage { ref message_id } if message_id == "m2") {
                saw_hydrate = true;
            }
        }
        assert!(saw_hydrate, "invalidated message must trigger HydrateMessage");

        // Deleting last message remotely empties TUI list on MailMessagesUpdated
        app.storage
            .delete_mail_messages_by_ids(&["m2".into()])
            .unwrap();
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailMessagesUpdated {
                folder_id: "f-inbox".into(),
                messages: vec![],
                notify_candidate: None,
            },
        );
        assert!(app.mail_messages.is_empty());
        assert!(app.active_mail_id.is_none());
    }

    #[test]
    fn tab_switch_virtual_folders_compose_contacts_and_open_only_hydration() {
        use crate::ui::action::{self, Action};
        use crate::ui::nav_pane::{TAB_CALENDAR, TAB_MAIL, TAB_TASKS};

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        app.mail_folders = vec![MailFolder {
            id: "f-inbox".into(),
            display_name: "Inbox".into(),
            well_known_name: Some("inbox".into()),
            parent_folder_id: None,
            child_folder_count: 0,
            unread_item_count: 1,
            total_item_count: 2,
        }];
        app.storage.upsert_mail_folders(&app.mail_folders).unwrap();
        app.active_folder_id = "f-inbox".into();
        app.active_todo_list_id = Some("todo-work".into());

        // Switching to Calendar sends SyncCalendar and sets ActiveView::Calendar
        action::dispatch(Action::SelectNavTab(TAB_CALENDAR), &mut app, &mut ui);
        assert_eq!(focus_rx.borrow().active_view, ActiveView::Calendar);
        assert!(matches!(nudge_rx.try_recv(), Ok(SyncCommand::SyncCalendar)));

        // Switching to Tasks sends SyncTodo for selected list and sets ActiveView::Tasks
        action::dispatch(Action::SelectNavTab(TAB_TASKS), &mut app, &mut ui);
        assert_eq!(focus_rx.borrow().active_view, ActiveView::Tasks);
        assert!(matches!(
            nudge_rx.try_recv(),
            Ok(SyncCommand::SyncTodo { list_id: Some(ref id) }) if id == "todo-work"
        ));

        // Switching back to Mail on a virtual folder (Unread) uses existing Inbox scope,
        // never requesting SyncFolder with "__virtual_unread__".
        app.active_folder_id = VIRTUAL_UNREAD_FOLDER.into();
        action::dispatch(Action::SelectNavTab(TAB_MAIL), &mut app, &mut ui);
        assert_eq!(focus_rx.borrow().active_view, ActiveView::Mail);
        assert_eq!(focus_rx.borrow().active_folder_id, None);
        assert!(matches!(
            nudge_rx.try_recv(),
            Ok(SyncCommand::SyncFolder { ref folder_id }) if folder_id == "f-inbox"
        ));

        // Opening Mail Compose triggers SyncContacts once; typing in To: does not spam extra SyncContacts
        app.draft_store = DraftStore::default();
        action::dispatch(Action::OpenMailCompose, &mut app, &mut ui);
        assert!(matches!(nudge_rx.try_recv(), Ok(SyncCommand::SyncContacts)));
        action::dispatch(Action::Input('b'), &mut app, &mut ui);
        action::dispatch(Action::Input('o'), &mut app, &mut ui);
        assert!(nudge_rx.try_recv().is_err());
        action::dispatch(Action::CloseOverlay, &mut app, &mut ui);

        // When two messages have body_loaded=false after delta, ONLY the open mail requests HydrateMessage
        app.active_folder_id = "f-inbox".into();
        let mut m_open = make_mail("m-open", "c-open", "2026-11-20T10:00:00Z", false, false);
        m_open.folder_id = Some("f-inbox".into());
        m_open.body_loaded = false;
        let mut m_closed = make_mail("m-closed", "c-closed", "2026-11-20T09:00:00Z", false, false);
        m_closed.folder_id = Some("f-inbox".into());
        m_closed.body_loaded = false;
        app.storage
            .upsert_mail_messages(&[m_open, m_closed])
            .unwrap();
        app.active_mail_id = Some("m-open".into());
        while nudge_rx.try_recv().is_ok() {}
        app.handle_core_event(
            &mut ui,
            CoreEvent::MailMessagesUpdated {
                folder_id: "f-inbox".into(),
                messages: vec![],
                notify_candidate: None,
            },
        );
        let mut hydrated_reqs = Vec::new();
        while let Ok(cmd) = nudge_rx.try_recv() {
            if let SyncCommand::HydrateMessage { message_id } = cmd {
                hydrated_reqs.push(message_id);
            }
        }
        assert_eq!(hydrated_reqs, vec!["m-open".to_string()]);
    }
}
