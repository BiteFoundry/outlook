//! Core TUI state structs (`AppState`, `UiState`), focus/scroll types, and `UiMsg`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{atomic::AtomicU64, Arc};
use std::time::Instant;

use chrono::{Local, NaiveDate};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{
    drafts_path_for, AppConfig, DensityMode, DraftStore, MailDraftFields, ThemePreset,
};
use crate::graph::mail::DraftSaveChange;
use crate::graph::GraphClient;
use crate::img_preview::{
    self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
};
use crate::models::{
    CalendarEvent, ContactItem, DownloadedFile, MailFolder, MailMessage, MailSearchHit, TodoList,
    TodoTask, UserProfile,
};
use crate::storage::Storage;
use crate::sync::{ActiveView, SyncController};

use super::super::hitmap::{HitMap, HitTarget};
use super::super::keymap::Keymap;
use super::super::nav_pane::TAB_MAIL;
use super::super::overlays::mail_compose::MailComposeOverlay;
use super::super::overlays::{Overlay, OverlayKind};
use super::super::theme::Theme;
use super::super::toast::{TaskBoard, ToastKind, ToastQueue};

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
        compose_id: String,
        draft_id: Option<String>,
    },
    MailDraftProgress {
        draft_key: String,
        compose_id: String,
        change: DraftSaveChange,
    },
    MailDraftSaved {
        draft_key: String,
        compose_id: String,
        result: anyhow::Result<Option<MailMessage>>,
        send: bool,
        sent_draft_id: Option<String>,
    },
    MailMoved {
        moves: Vec<(String, MailMessage)>,
    },
    MailDeleteFailed {
        original_id: String,
        original_message: Option<MailMessage>,
        error: String,
    },
    /// Graph rejected a move (archive, move to folder) after the optimistic local move. `restored`
    /// holds the pre-action snapshots; their cache rows are already back in the original folder.
    MailMoveFailed {
        restored: Vec<MailMessage>,
        label: String,
        error: String,
    },
    /// Graph rejected a read/flag update; the optimistic toggle goes back to the given values.
    MailFlagReverted {
        id: String,
        is_read: Option<bool>,
        flagged: Option<bool>,
        label: String,
        error: String,
    },
    MailDeleted {
        message_ids: Vec<String>,
    },
    AttachmentDownloaded(DownloadedFile),
    SearchResults {
        query: String,
        generation: u64,
        hits: Vec<MailSearchHit>,
    },
    RemoteSearchResults {
        query: String,
        generation: u64,
        messages: Vec<MailMessage>,
    },
    RemoteSearchFailed {
        query: String,
        generation: u64,
        error: String,
    },
    CalendarEventCreated(CalendarEvent),
    CalendarEventDeleted(String),
    CalendarEventDeleteFailed {
        original_event: Option<CalendarEvent>,
        error: String,
    },
    CalendarRsvpDone {
        event_id: String,
        response_status: String,
    },
    CalendarRsvpFailed {
        event_id: String,
        prev_status: Option<String>,
        prev_mail_statuses: Vec<(String, Option<String>)>,
        error: String,
    },
    TodoListCreated(TodoList),
    TodoTasksLoaded {
        list_id: String,
        tasks: Vec<TodoTask>,
    },
    TodoTaskCreated(TodoTask),
    TodoTaskUpdated(TodoTask),
    TodoTaskToggleFailed {
        original_task: TodoTask,
        error: String,
    },
    TodoTaskDeleted(String),
    TodoTaskDeleteFailed {
        original_task: Option<TodoTask>,
        error: String,
    },
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
        content_type: Option<String>,
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
// Preserve the initial history jump when the cache already holds several pages.
pub(super) const INITIAL_MAIL_HISTORY_LIMIT: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
    /// Optional pre-encoded terminal escape sequence (used by `HtmlPreviewOverlay` graphics
    /// cached on the overlay rather than in `AppState::image_cache`).
    pub payload: Option<String>,
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
    /// Mail folders whose subfolders are hidden in the folder pane (empty = fully expanded).
    pub collapsed_folder_ids: HashSet<String>,
    pub mail_list_scroll: ScrollState,
    pub mail_read_scroll: ScrollState,
    pub(crate) mail_reader_content: Option<super::super::mail_view::ReaderMailContent>,
    pub cal_list_scroll: ScrollState,
    pub cal_detail_scroll: ScrollState,
    pub todo_list_scroll: ScrollState,
    pub todo_detail_scroll: ScrollState,
    pub show_nav_rail: bool,
    pub group_threads: bool,
    pub focused_only: bool,
    pub mail_unread_only: bool,
    pub todo_hide_completed: bool,
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
    pub cell_px_refresh_pending: bool,
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub active_pixel_graphics: Vec<(String, Rect, TerminalImageProtocol)>,
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
            collapsed_folder_ids: HashSet::new(),
            mail_list_scroll: ScrollState::default(),
            mail_read_scroll: ScrollState::default(),
            mail_reader_content: None,
            cal_list_scroll: ScrollState::default(),
            cal_detail_scroll: ScrollState::default(),
            todo_list_scroll: ScrollState::default(),
            todo_detail_scroll: ScrollState::default(),
            show_nav_rail: true,
            group_threads: true,
            focused_only: false,
            mail_unread_only: false,
            todo_hide_completed: false,
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
            cell_px_refresh_pending: false,
            pending_pixel_graphics: Vec::new(),
            active_pixel_graphics: Vec::new(),
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

pub(super) struct PendingDraftWrite {
    pub(super) fields: MailDraftFields,
    pub(super) files: Vec<PathBuf>,
    pub(super) images: Vec<String>,
    pub(super) queued: Option<MailComposeOverlay>,
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
    pub(super) mail_detail_missing: HashSet<String>,
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
    pub(super) draft_store_path: PathBuf,
    pub(super) draft_writes: HashMap<String, PendingDraftWrite>,
    pub download_dir: PathBuf,
    pub loading_mail: bool,
    pub loading_older_mail: bool,
    pub mail_oldest_reached: bool,
    pub mail_history_seq: u64,
    pub(super) mail_history_boundary: Option<(String, String)>,
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
        // Drafts are per account. The one-time move of the legacy shared file happens in `tui::run`,
        // not here, so constructing an `AppState` never renames files.
        let draft_store_path = drafts_path_for(graph.my_user_id());
        let mut draft_store = DraftStore::load_from(&draft_store_path).unwrap_or_default();
        for entry in draft_store.mail.values_mut() {
            if entry.compose_id.is_empty() {
                entry.compose_id = crate::new_uuid_v4();
            }
        }
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
            draft_store_path,
            draft_writes: HashMap::new(),
            download_dir,
            loading_mail: false,
            loading_older_mail: false,
            mail_oldest_reached: false,
            mail_history_seq: 0,
            mail_history_boundary: None,
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
}
