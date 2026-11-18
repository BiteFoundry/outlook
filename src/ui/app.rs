//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use chrono::Utc;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{self, AppConfig, Persona};
use crate::graph::GraphClient;
use crate::img_preview::{self, RenderedTerminalImage};
use crate::models::{
    AttachmentInfo, CalendarEvent, ChannelSummary, ContactItem, ConversationKind,
    ConversationSummary, ConversationTarget, CoreEvent, DownloadedFile, InlineImageUpload,
    MailAttachment, MailFolder, MailMessage, MailSearchHit, MemberInfo, MessageItem,
    NotificationCandidate, OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteResource,
    OnenoteScope, OnenoteSearchHit, OnenoteSection, OutgoingContact, OutgoingEvent, OutgoingMail,
    OutgoingMessage, OutgoingOnenotePage, OutgoingTodoTask, PresenceStatus, QuotedReply,
    ReactionSummary, SearchHit, TeamSummary, ThreadView, TodoList, TodoTask, UpdateCheckResult,
    UserProfile,
};
use crate::parser;
use crate::storage::Storage;
use crate::sync::{self, SyncCommand, SyncController};

use super::composer::{ComposerState, ReplyPreview};
use super::hitmap::{HitMap, MessageScope};
use super::keymap::Keymap;
use super::nav_pane::{self, NavRow};
use super::notification;
use super::overlays::image_preview::PreviewTarget;
use super::overlays::mail_compose::MailComposeMode;
use super::overlays::onenote_form::OnenoteFormOverlay;
use super::overlays::{Overlay, OverlayKind};
use super::theme::Theme;
use super::toast::{TaskBoard, ToastKind, ToastQueue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    Chat,
    Thread,
    Composer,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NarrowPane {
    #[default]
    List,
    Chat,
    Thread,
}

#[derive(Debug, Clone)]
pub struct ScrollState {
    pub follow_tail: bool,
    pub scroll_up: u16,
    pub selected_idx: Option<usize>,
    pub area: Rect,
    pub total_lines: usize,
    pub offset: usize,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            follow_tail: true,
            scroll_up: 0,
            selected_idx: None,
            area: Rect::default(),
            total_lines: 0,
            offset: 0,
        }
    }
}

impl ScrollState {
    pub fn scroll_up(&mut self, lines: u16) {
        self.follow_tail = false;
        self.scroll_up = self.scroll_up.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        if lines >= self.scroll_up {
            self.scroll_up = 0;
            self.follow_tail = true;
        } else {
            self.scroll_up -= lines;
        }
    }

    pub fn bottom(&mut self) {
        self.scroll_up = 0;
        self.follow_tail = true;
    }

    pub fn top(&mut self) {
        self.follow_tail = false;
        self.scroll_up = self.total_lines.min(u16::MAX as usize) as u16;
    }

    pub fn offset(&self, height: usize) -> usize {
        if self.total_lines <= height {
            return 0;
        }
        let max_off = self.total_lines - height;
        if self.follow_tail {
            max_off
        } else {
            max_off.saturating_sub(self.scroll_up as usize)
        }
    }
}

pub enum UiMsg {
    BootstrapDone {
        me: Option<UserProfile>,
        presence: Option<PresenceStatus>,
        conversations: Vec<ConversationSummary>,
        teams: Vec<TeamSummary>,
        mentions: Vec<MessageItem>,
        presences: HashMap<String, PresenceStatus>,
    },
    MessagesLoaded {
        conv_key: String,
        messages: Vec<MessageItem>,
        members: Vec<MemberInfo>,
    },
    OlderMessagesLoaded {
        conv_key: String,
        messages: Vec<MessageItem>,
        reached_end: bool,
    },
    ThreadLoaded {
        conv_key: String,
        root_msg_id: String,
        thread: ThreadView,
    },
    MessageSent {
        conv_key: String,
        parent_msg_id: Option<String>,
        optimistic_id: Option<String>,
        sent: MessageItem,
    },
    MessageSendFailed {
        conv_key: String,
        parent_msg_id: Option<String>,
        optimistic_id: String,
        text: String,
    },
    MessageDeleted {
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    },
    ReactionUpdated {
        conv_key: String,
        msg_id: String,
        reaction_type: String,
        added: bool,
    },
    AttachmentDownloaded(DownloadedFile),
    AttachmentSent {
        conv_key: String,
        sent: MessageItem,
    },
    SearchResults {
        query: String,
        hits: Vec<SearchHit>,
    },
    UserSearchResults {
        query: String,
        users: Vec<UserProfile>,
    },
    ChatCreated(ConversationSummary),
    PresenceUpdated(PresenceStatus),
    OutlookBootstrapDone {
        mail_folders: Vec<MailFolder>,
        mails: Vec<MailMessage>,
        calendar_events: Vec<CalendarEvent>,
        contacts: Vec<ContactItem>,
        todo_lists: Vec<TodoList>,
        todo_tasks: Vec<TodoTask>,
        active_todo_list_id: Option<String>,
    },
    MailFolderLoaded {
        folder_id: String,
        mails: Vec<MailMessage>,
    },
    MailDetailLoaded {
        mail: MailMessage,
    },
    MailDetailFailed {
        mail_id: String,
    },
    MailPrefetchFailed {
        mail_id: String,
    },
    MailReadRollback {
        mail_id: String,
        prev_read: bool,
        folder_prev_unread: Option<(String, i32)>,
    },
    MailFlagRollback {
        mail_id: String,
        prev_flagged: bool,
    },
    MailRemoveRollback {
        folder_id: String,
        index: Option<usize>,
        mail: MailMessage,
    },
    OutlookCachedSecondaryLoaded {
        calendar_events: Vec<CalendarEvent>,
        contacts: Vec<ContactItem>,
        todo_lists: Vec<TodoList>,
        todo_tasks: Vec<TodoTask>,
        active_todo_list_id: Option<String>,
    },
    MailUpdated {
        mail: MailMessage,
    },
    MailRemoved {
        mail_id: String,
    },
    TodoStatusRollback {
        task_id: String,
        prev_status: String,
    },
    CalendarLoaded {
        events: Vec<CalendarEvent>,
    },
    CalendarEventCreated {
        event: CalendarEvent,
    },
    CalendarEventDeleted {
        event_id: String,
    },
    CalendarEventResponded {
        event_id: String,
        response: String,
    },
    ContactsLoaded {
        contacts: Vec<ContactItem>,
    },
    ContactCreated {
        contact: ContactItem,
    },
    ContactDeleted {
        contact_id: String,
    },
    TodoListsLoaded {
        lists: Vec<TodoList>,
    },
    TodoTasksLoaded {
        list_id: String,
        tasks: Vec<TodoTask>,
    },
    TodoTaskUpserted {
        task: TodoTask,
    },
    TodoTaskDeleted {
        task_id: String,
    },
    MailSearchResults {
        query: String,
        hits: Vec<MailSearchHit>,
    },
    ContactSearchResults {
        query: String,
        contacts: Vec<ContactItem>,
    },
    ImageDecoded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    ImageDecodeFailed {
        cache_key: String,
        owner_id: String,
        att_id: String,
        error: String,
    },
    OnenoteImageDecoded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    OnenoteImageDecodeFailed {
        cache_key: String,
        error: String,
    },
    UpdateChecked(UpdateCheckResult),
    DaemonAttached {
        pid: Option<u32>,
        sync_busy: bool,
    },
    DaemonDetached,
    StartLocalBootstrap,
    ProfileLoaded {
        me: Option<UserProfile>,
        presence: Option<PresenceStatus>,
    },
    TaskDone {
        label: String,
        error: Option<String>,
        toast: Option<(String, ToastKind)>,
    },
}

pub struct UiState {
    pub persona: Persona,
    pub focus: Focus,
    pub overlays: Vec<Overlay>,
    pub composer: ComposerState,
    pub hitmap: HitMap,
    pub toasts: ToastQueue,
    pub tasks: TaskBoard,
    pub keymap: Keymap,
    pub theme: Theme,
    pub active_nav_tab: usize,
    pub nav_selected: usize,
    pub collapsed_team_ids: HashSet<String>,
    pub chat_scroll: ScrollState,
    pub thread_scroll: ScrollState,
    pub mail_scroll: ScrollState,
    pub attach_index: usize,
    pub mail_attach_index: usize,
    pub show_nav_rail: bool,
    pub auto_thread_pane: bool,
    pub inline_images: bool,
    pub thread_drawer_open: bool,
    pub narrow_pane: NarrowPane,
    pub editor_requested: bool,
    pub notify: bool,
    pub composer_target_thread: bool,
    pub show_subject: bool,
    pub active_mail_folder_id: String,
    pub active_mail_folder_name: String,
    pub active_todo_list_id: Option<String>,
    pub todo_input: String,
    pub todo_input_active: bool,
    pub todo_include_completed: bool,
    pub term_width: u16,
    pub term_height: u16,
    pub collapsed_notebook_keys: HashSet<String>,
    pub onenote_selected_block: usize,
    pub onenote_scroll: ScrollState,
    pub onenote_follow_selection: bool,
    pub onenote_external_edit: Option<(String, String)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self::new_with_persona(Persona::Teams)
    }

    pub fn new_with_persona(persona: Persona) -> Self {
        let active_nav_tab = match persona {
            Persona::Teams => 0,
            Persona::Outlook => 3,
            Persona::OneNote => 7,
        };
        Self {
            persona,
            focus: Focus::Nav,
            overlays: Vec::new(),
            composer: ComposerState::new(),
            hitmap: HitMap::default(),
            toasts: ToastQueue::new(),
            tasks: TaskBoard::default(),
            keymap: Keymap::default_teams(),
            theme: Theme::default(),
            active_nav_tab,
            nav_selected: 0,
            collapsed_team_ids: HashSet::new(),
            chat_scroll: ScrollState::default(),
            thread_scroll: ScrollState::default(),
            mail_scroll: ScrollState::default(),
            attach_index: 0,
            mail_attach_index: 0,
            show_nav_rail: true,
            auto_thread_pane: true,
            inline_images: true,
            thread_drawer_open: false,
            narrow_pane: NarrowPane::List,
            editor_requested: false,
            notify: true,
            composer_target_thread: false,
            show_subject: false,
            active_mail_folder_id: "inbox".to_string(),
            active_mail_folder_name: "받은 편지함".to_string(),
            active_todo_list_id: None,
            todo_input: String::new(),
            todo_input_active: false,
            todo_include_completed: false,
            term_width: 120,
            term_height: 40,
            collapsed_notebook_keys: HashSet::new(),
            onenote_selected_block: 0,
            onenote_scroll: ScrollState::default(),
            onenote_follow_selection: false,
            onenote_external_edit: None,
        }
    }

    pub fn from_config(config: &AppConfig) -> Self {
        Self::from_config_with_persona(config, Persona::Teams)
    }

    pub fn from_config_with_persona(config: &AppConfig, persona: Persona) -> Self {
        let mut s = Self::new_with_persona(persona);
        s.show_nav_rail = config.show_nav_rail;
        s.auto_thread_pane = config.auto_thread_pane;
        s.notify = config.notify;
        s.inline_images = config.inline_images;
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
        matches!(self.focus(), Focus::Composer)
            || (matches!(self.focus(), Focus::Chat)
                && self.active_nav_tab == 6
                && self.todo_input_active)
            || self.overlays.last().is_some_and(Overlay::accepts_text)
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

    pub fn toggle_composer_focus(&mut self, active: bool) {
        if active {
            self.set_focus(Focus::Composer);
        } else if self.thread_drawer_open {
            self.set_focus(Focus::Thread);
        } else {
            self.set_focus(Focus::Chat);
        }
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
    pub persona: Persona,
    pub auth: AuthManager,
    pub graph: GraphClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub my_profile: Option<UserProfile>,
    pub my_user_id: String,
    pub my_presence: Option<PresenceStatus>,
    pub conversations: Vec<ConversationSummary>,
    pub teams: Vec<TeamSummary>,
    pub active_conv_key: Option<String>,
    pub active_target: Option<ConversationTarget>,
    pub messages: Vec<MessageItem>,
    pub active_thread: Option<ThreadView>,
    pub activity_mentions: Vec<MessageItem>,
    pub members: Vec<MemberInfo>,
    pub presences: HashMap<String, PresenceStatus>,
    pub drafts: HashMap<String, String>,
    pub download_dir: PathBuf,
    pub loading_messages: bool,
    pub loading_thread: bool,
    pub syncing: bool,
    pub sync_summary: String,
    pub daemon_attached: bool,
    pub daemon_pid: Option<u32>,
    pub searching: bool,
    pub search_query: String,
    pub search_results: Vec<SearchHit>,
    pub mail_search_results: Vec<MailSearchHit>,
    pub contact_search_results: Vec<ContactItem>,
    pub user_searching: bool,
    pub user_search_results: Vec<UserProfile>,
    pub oldest_reached: bool,
    // Lazy domain activation flags
    pub activated_teams: bool,
    pub activated_outlook: bool,
    pub activated_onenote: bool,
    // Outlook Unified Workspace state
    pub mail_folders: Vec<MailFolder>,
    pub mails: Vec<MailMessage>,
    pub active_mail_id: Option<String>,
    pub active_mail: Option<MailMessage>,
    pub loading_mail: bool,
    pub mail_prefetch_pending: HashSet<String>,
    pub calendar_events: Vec<CalendarEvent>,
    pub active_event_id: Option<String>,
    pub contacts: Vec<ContactItem>,
    pub active_contact_id: Option<String>,
    pub todo_lists: Vec<TodoList>,
    pub todo_tasks: Vec<TodoTask>,
    pub active_todo_task_id: Option<String>,
    // Microsoft OneNote workspace state
    pub onenote_notebooks: Vec<OnenoteNotebook>,
    pub onenote_sections: Vec<OnenoteSection>,
    pub onenote_pages: Vec<OnenotePageSummary>,
    pub onenote_search_results: Vec<OnenoteSearchHit>,
    pub active_notebook_key: Option<String>,
    pub active_section_key: Option<String>,
    pub active_page_key: Option<String>,
    pub active_page_detail: Option<OnenotePageDetail>,
    pub loading_onenote_pages: bool,
    pub loading_onenote_detail: bool,
    pub onenote_image_cache: HashMap<String, RenderedTerminalImage>,
    pub onenote_image_cache_order: VecDeque<String>,
    pub onenote_image_cache_bytes: usize,
    pub onenote_image_pending: HashSet<String>,
    pub onenote_image_errors: HashSet<String>,
    // Terminal inline image preview cache (bounded LRU-style FIFO + 8 MiB byte budget)
    pub image_cache: HashMap<String, RenderedTerminalImage>,
    pub image_cache_order: VecDeque<String>,
    pub image_cache_bytes: usize,
    pub image_pending: HashSet<String>,
    pub image_errors: HashMap<String, String>,
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
        Self::new_with_persona(
            config,
            Persona::Teams,
            auth,
            graph,
            storage,
            sync_ctrl,
            ui_tx,
        )
    }

    pub fn new_with_persona(
        config: AppConfig,
        persona: Persona,
        auth: AuthManager,
        graph: GraphClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let download_dir = config.resolved_download_dir();
        let drafts = config::load_drafts();
        Self {
            config,
            persona,
            auth,
            graph,
            storage,
            sync_ctrl,
            my_profile: None,
            my_user_id: String::new(),
            my_presence: None,
            conversations: Vec::new(),
            teams: Vec::new(),
            active_conv_key: None,
            active_target: None,
            messages: Vec::new(),
            active_thread: None,
            activity_mentions: Vec::new(),
            members: Vec::new(),
            presences: HashMap::new(),
            drafts,
            download_dir,
            loading_messages: false,
            loading_thread: false,
            syncing: false,
            sync_summary: "준비".to_string(),
            daemon_attached: false,
            daemon_pid: None,
            searching: false,
            search_query: String::new(),
            search_results: Vec::new(),
            mail_search_results: Vec::new(),
            contact_search_results: Vec::new(),
            user_searching: false,
            user_search_results: Vec::new(),
            oldest_reached: false,
            activated_teams: false,
            activated_outlook: false,
            activated_onenote: false,
            mail_folders: Vec::new(),
            mails: Vec::new(),
            active_mail_id: None,
            active_mail: None,
            loading_mail: false,
            mail_prefetch_pending: HashSet::new(),
            calendar_events: Vec::new(),
            active_event_id: None,
            contacts: Vec::new(),
            active_contact_id: None,
            todo_lists: Vec::new(),
            todo_tasks: Vec::new(),
            active_todo_task_id: None,
            onenote_notebooks: Vec::new(),
            onenote_sections: Vec::new(),
            onenote_pages: Vec::new(),
            onenote_search_results: Vec::new(),
            active_notebook_key: None,
            active_section_key: None,
            active_page_key: None,
            active_page_detail: None,
            loading_onenote_pages: false,
            loading_onenote_detail: false,
            onenote_image_cache: HashMap::new(),
            onenote_image_cache_order: VecDeque::new(),
            onenote_image_cache_bytes: 0,
            onenote_image_pending: HashSet::new(),
            onenote_image_errors: HashSet::new(),
            image_cache: HashMap::new(),
            image_cache_order: VecDeque::new(),
            image_cache_bytes: 0,
            image_pending: HashSet::new(),
            image_errors: HashMap::new(),
            ui_tx,
        }
    }

    pub fn active_conversation(&self) -> Option<&ConversationSummary> {
        let key = self.active_conv_key.as_deref()?;
        self.conversations.iter().find(|c| c.conv_key == key)
    }

    pub fn active_channel(&self) -> Option<(&TeamSummary, &ChannelSummary)> {
        let target = self.active_target.as_ref()?;
        match target {
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => {
                let team = self.teams.iter().find(|t| &t.id == team_id)?;
                let ch = team.channels.iter().find(|c| &c.id == channel_id)?;
                Some((team, ch))
            }
            ConversationTarget::Chat { .. } => None,
        }
    }

    pub fn active_title(&self) -> String {
        if let Some(conv) = self.active_conversation() {
            return conv.title.clone();
        }
        if let Some((team, ch)) = self.active_channel() {
            return format!("{} › #{}", team.display_name, ch.display_name);
        }
        "대화를 선택하세요".to_string()
    }

    pub fn message_at(&self, scope: MessageScope, msg_index: usize) -> Option<&MessageItem> {
        match scope {
            MessageScope::Chat => self.messages.get(msg_index),
            MessageScope::ThreadRoot => self.active_thread.as_ref().map(|t| &t.root),
            MessageScope::ThreadReply => self
                .active_thread
                .as_ref()
                .and_then(|t| t.replies.get(msg_index)),
        }
    }

    pub fn message_at_mut(
        &mut self,
        scope: MessageScope,
        msg_index: usize,
    ) -> Option<&mut MessageItem> {
        match scope {
            MessageScope::Chat => self.messages.get_mut(msg_index),
            MessageScope::ThreadRoot => self.active_thread.as_mut().map(|t| &mut t.root),
            MessageScope::ThreadReply => self
                .active_thread
                .as_mut()
                .and_then(|t| t.replies.get_mut(msg_index)),
        }
    }

    pub fn current_draft_key(&self, ui: &UiState) -> Option<String> {
        let conv_key = self.active_conv_key.as_deref()?;
        if ui.composer_target_thread {
            if let Some(thread) = &self.active_thread {
                return Some(format!("{conv_key}#thread:{}", thread.root.msg_id));
            }
        }
        Some(conv_key.to_string())
    }

    fn persist_drafts(&self) {
        if tokio::runtime::Handle::try_current().is_ok() {
            let snapshot = self.drafts.clone();
            tokio::task::spawn_blocking(move || {
                let _ = config::save_drafts(&snapshot);
            });
        } else {
            let _ = config::save_drafts(&self.drafts);
        }
    }

    /// Updates the in-memory draft map from the composer and returns `true` if `self.drafts` changed.
    fn stash_draft_in_memory(&mut self, ui: &UiState) -> bool {
        let Some(key) = self.current_draft_key(ui) else {
            return false;
        };
        let text = ui.composer.text().trim_end_matches('\n').to_string();
        if text.is_empty() {
            self.drafts.remove(&key).is_some()
        } else if self.drafts.get(&key) != Some(&text) {
            self.drafts.insert(key, text);
            true
        } else {
            false
        }
    }

    pub fn stash_draft(&mut self, ui: &UiState) {
        if self.stash_draft_in_memory(ui) {
            self.persist_drafts();
        }
    }

    pub fn restore_draft(&self, ui: &mut UiState) {
        let key = self.current_draft_key(ui);
        let text = key
            .as_deref()
            .and_then(|k| self.drafts.get(k))
            .map(String::as_str)
            .unwrap_or("");
        ui.composer.load_text(text);
    }

    pub fn clear_current_draft(&mut self, ui: &mut UiState) {
        if let Some(key) = self.current_draft_key(ui) {
            if self.drafts.remove(&key).is_some() {
                self.persist_drafts();
            }
        }
        ui.composer.reset_after_send();
    }

    fn load_cached_teams(&mut self) {
        if let Ok(convs) = self.storage.list_conversations() {
            self.conversations = convs;
        }
        if let Ok(teams) = self.storage.list_teams() {
            self.teams = teams;
        }
        if let Ok(presences) = self.storage.get_presences() {
            self.presences = presences;
        }
        if !self.my_user_id.is_empty() {
            if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50) {
                self.activity_mentions = mentions;
            }
        }
    }

    fn load_cached_outlook_initial(&mut self) {
        if let Ok(folders) = self.storage.list_mail_folders() {
            self.mail_folders = folders;
        }
        if let Ok(mails) = self.storage.list_mail_summaries(Some("inbox"), 100) {
            self.mails = mails;
        }
    }

    fn load_cached_outlook_full(&mut self) {
        self.load_cached_outlook_initial();
        if let Ok(events) = self.storage.list_calendar_events(None, None, 100) {
            self.calendar_events = events;
        }
        if let Ok(contacts) = self.storage.list_contacts(None, 200) {
            self.contacts = contacts;
        }
        if let Ok(lists) = self.storage.list_todo_lists() {
            if let Some(first) = lists.first() {
                if let Ok(tasks) = self.storage.list_todo_tasks(&first.id, false, 200) {
                    self.todo_tasks = tasks;
                }
            }
            self.todo_lists = lists;
        }
    }

    fn load_cached_onenote(&mut self) {
        if let Ok(notebooks) = self.storage.list_onenote_notebooks() {
            self.onenote_notebooks = notebooks;
        }
        if let Ok(sections) = self.storage.list_onenote_sections(None) {
            self.onenote_sections = sections;
        }
        if self.active_notebook_key.is_none() {
            self.active_notebook_key = self
                .onenote_notebooks
                .first()
                .map(|nb| nb.notebook_key.clone());
        }
        if self.active_section_key.is_none() {
            self.active_section_key = self
                .sections_for_active_notebook()
                .first()
                .map(|sec| sec.section_key.clone())
                .or_else(|| self.onenote_sections.first().map(|sec| sec.section_key.clone()));
        }
        if let Some(sec_key) = self.active_section_key.clone() {
            if let Ok(pages) = self.storage.list_onenote_pages(&sec_key) {
                if let Some(first_page) = pages.first() {
                    self.active_page_key = Some(first_page.page_key.clone());
                    self.active_page_detail = self
                        .storage
                        .get_onenote_page_detail(&first_page.page_key)
                        .unwrap_or(None);
                }
                self.onenote_pages = pages;
            }
        }
    }

    pub fn load_cached_initial(&mut self) {
        self.load_cached_teams();
        self.load_cached_outlook_full();
        self.load_cached_onenote();
    }

    pub fn load_cached_for_persona(&mut self) {
        match self.persona {
            Persona::Teams => {
                self.load_cached_teams();
                self.activated_teams = true;
            }
            Persona::Outlook => {
                self.load_cached_outlook_initial();
                self.activated_outlook = true;
            }
            Persona::OneNote => {
                self.load_cached_onenote();
                self.activated_onenote = true;
            }
        }
    }

    fn send_active_domains(&self) {
        self.sync_ctrl.request(SyncCommand::SetActiveDomains {
            teams: self.activated_teams,
            outlook: self.activated_outlook,
            onenote: self.activated_onenote,
        });
    }

    pub fn ensure_tab_domain_activated(&mut self, ui: &mut UiState, tab: usize) {
        const ONENOTE_TAB_LEAVE_MAX_BYTES: usize = 2 * 1024 * 1024;
        const SHRINK_MEMORY_EVICT_THRESHOLD_BYTES: usize = 1024 * 1024;

        if tab != nav_pane::TAB_NOTES
            && self.onenote_image_cache_bytes > ONENOTE_TAB_LEAVE_MAX_BYTES
        {
            let evicted_bytes = self.trim_onenote_image_cache_to(ONENOTE_TAB_LEAVE_MAX_BYTES);
            if evicted_bytes > SHRINK_MEMORY_EVICT_THRESHOLD_BYTES
                && tokio::runtime::Handle::try_current().is_ok()
            {
                let storage = self.storage.clone();
                tokio::task::spawn_blocking(move || {
                    storage.shrink_memory();
                });
            }
        }

        if tab <= nav_pane::TAB_ACTIVITY {
            if !self.activated_teams {
                self.activated_teams = true;
                self.load_cached_teams();
                self.send_active_domains();
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::RefreshConversations);
                } else {
                    self.spawn_teams_bootstrap(None);
                }
            }
        } else if (nav_pane::TAB_MAIL..=nav_pane::TAB_TODO).contains(&tab) {
            if !self.activated_outlook {
                self.activated_outlook = true;
                self.load_cached_outlook_full();
                if ui.active_todo_list_id.is_none() {
                    ui.active_todo_list_id = self.todo_lists.first().map(|l| l.id.clone());
                }
                self.send_active_domains();
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::RefreshOutlook);
                } else {
                    self.spawn_outlook_bootstrap(None);
                }
            }
        } else if tab == nav_pane::TAB_NOTES && !self.activated_onenote {
            self.activated_onenote = true;
            self.load_cached_onenote();
            self.send_active_domains();
            self.spawn_onenote_bootstrap();
        }
    }

    fn spawn_onenote_bootstrap(&self) {
        self.sync_ctrl.request(SyncCommand::RefreshOneNote);
        if let Some(sec_key) = self.active_section_key.clone() {
            self.sync_ctrl.request(SyncCommand::SyncOneNoteSection {
                section_key: sec_key,
            });
        }
        if let Some(page_key) = self.active_page_key.clone() {
            self.sync_ctrl
                .request(SyncCommand::SyncOneNotePage { page_key });
        }
    }

    fn spawn_outlook_cached_secondary(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let snapshot = tokio::task::spawn_blocking(move || {
                let calendar_events = storage
                    .list_calendar_events(None, None, 100)
                    .unwrap_or_default();
                let contacts = storage.list_contacts(None, 200).unwrap_or_default();
                let todo_lists = storage.list_todo_lists().unwrap_or_default();
                let active_todo_list_id = todo_lists.first().map(|l| l.id.clone());
                let todo_tasks = if let Some(ref lid) = active_todo_list_id {
                    storage.list_todo_tasks(lid, false, 200).unwrap_or_default()
                } else {
                    Vec::new()
                };
                (
                    calendar_events,
                    contacts,
                    todo_lists,
                    todo_tasks,
                    active_todo_list_id,
                )
            })
            .await;

            if let Ok((
                calendar_events,
                contacts,
                todo_lists,
                todo_tasks,
                active_todo_list_id,
            )) = snapshot
            {
                let _ = tx.send(UiMsg::OutlookCachedSecondaryLoaded {
                    calendar_events,
                    contacts,
                    todo_lists,
                    todo_tasks,
                    active_todo_list_id,
                });
            }
        });
    }

    fn spawn_outlook_bootstrap(&self, task_label: Option<String>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let outlook_graph = self.graph.clone();
        let outlook_storage = self.storage.clone();
        let outlook_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let _ = sync::bootstrap_outlook(&outlook_graph, &outlook_storage).await;
            let snapshot = tokio::task::spawn_blocking(move || {
                let mail_folders = outlook_storage.list_mail_folders().unwrap_or_default();
                let mails = outlook_storage
                    .list_mail_summaries(Some("inbox"), 100)
                    .unwrap_or_default();
                let calendar_events = outlook_storage
                    .list_calendar_events(None, None, 100)
                    .unwrap_or_default();
                let contacts = outlook_storage.list_contacts(None, 200).unwrap_or_default();
                let todo_lists = outlook_storage.list_todo_lists().unwrap_or_default();
                let active_todo_list_id = todo_lists.first().map(|l| l.id.clone());
                let todo_tasks = if let Some(ref lid) = active_todo_list_id {
                    outlook_storage
                        .list_todo_tasks(lid, false, 200)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                (
                    mail_folders,
                    mails,
                    calendar_events,
                    contacts,
                    todo_lists,
                    todo_tasks,
                    active_todo_list_id,
                )
            })
            .await;

            if let Ok((
                mail_folders,
                mails,
                calendar_events,
                contacts,
                todo_lists,
                todo_tasks,
                active_todo_list_id,
            )) = snapshot
            {
                let _ = outlook_tx.send(UiMsg::OutlookBootstrapDone {
                    mail_folders,
                    mails,
                    calendar_events,
                    contacts,
                    todo_lists,
                    todo_tasks,
                    active_todo_list_id,
                });
            }
            if let Some(label) = task_label {
                let _ = outlook_tx.send(UiMsg::TaskDone {
                    label,
                    error: None,
                    toast: None,
                });
            }
        });
    }

    fn spawn_teams_bootstrap(&self, task_label: Option<String>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (token_uid, me_res, presence_res, _conv_res) = tokio::join!(
                async { auth.current_tokens().await.and_then(|t| t.user_id) },
                graph.get_me(),
                graph.get_my_presence(),
                sync::bootstrap_conversations(&graph, &storage),
            );
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.mail.clone())
                    .await;
            }
            let presence = presence_res.ok();
            let my_id_owned = me
                .as_ref()
                .map(|u| u.id.clone())
                .or(token_uid)
                .unwrap_or_default();

            let read_storage = storage.clone();
            let (conversations, teams, mentions, cached_presences) =
                tokio::task::spawn_blocking(move || {
                    let conversations = read_storage.list_conversations().unwrap_or_default();
                    let teams = read_storage.list_teams().unwrap_or_default();
                    let mentions = if !my_id_owned.is_empty() {
                        read_storage
                            .list_mentions_for_user(&my_id_owned, 50)
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let cached_presences = read_storage.get_presences().unwrap_or_default();
                    (conversations, teams, mentions, cached_presences)
                })
                .await
                .unwrap_or_default();

            let peer_ids: Vec<String> = conversations
                .iter()
                .filter_map(|c| c.peer_user_id.clone())
                .collect();
            let presences = if !peer_ids.is_empty() {
                let fetched = graph
                    .get_presences_by_user_ids(&peer_ids)
                    .await
                    .unwrap_or_default();
                let write_storage = storage.clone();
                let fetched_for_db = fetched.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = write_storage.upsert_presences(&fetched_for_db);
                    write_storage.get_presences()
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(|| {
                    fetched
                        .into_iter()
                        .map(|p| (p.user_id.clone(), p))
                        .collect()
                })
            } else {
                cached_presences
            };

            let _ = tx.send(UiMsg::BootstrapDone {
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences,
            });
            if let Some(label) = task_label {
                let _ = tx.send(UiMsg::TaskDone {
                    label,
                    error: None,
                    toast: None,
                });
            }
        });
    }

    fn spawn_me_profile_if_needed(&self) {
        if self.my_profile.is_some() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (me_res, presence_res) = tokio::join!(graph.get_me(), graph.get_my_presence());
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.mail.clone())
                    .await;
            }
            let presence = presence_res.ok();
            let _ = tx.send(UiMsg::ProfileLoaded { me, presence });
        });
    }

    pub fn spawn_bootstrap(&mut self, ui: &mut UiState) {
        if ui.active_todo_list_id.is_none() {
            ui.active_todo_list_id = self.todo_lists.first().map(|l| l.id.clone());
        }

        match self.persona {
            Persona::Teams => self.activated_teams = true,
            Persona::Outlook => self.activated_outlook = true,
            Persona::OneNote => self.activated_onenote = true,
        }
        self.send_active_domains();

        let task_label = "초기 동기화".to_string();
        ui.tasks.begin(task_label.clone());
        self.syncing = true;
        self.sync_summary = "동기화 중...".to_string();

        match self.persona {
            Persona::Teams => {
                self.spawn_teams_bootstrap(Some(task_label));
            }
            Persona::Outlook => {
                self.spawn_outlook_cached_secondary();
                self.spawn_outlook_bootstrap(Some(task_label));
            }
            Persona::OneNote => {
                self.spawn_onenote_bootstrap();
            }
        }
    }

    pub fn select_conversation(&mut self, ui: &mut UiState, target: ConversationTarget) {
        let new_key = target.conv_key();
        if self.active_conv_key.as_deref() == Some(&new_key) {
            self.focus_conversation_pane(ui);
            return;
        }

        self.stash_draft(ui);

        self.active_conv_key = Some(new_key.clone());
        self.active_target = Some(target.clone());
        self.active_thread = None;
        self.oldest_reached = false;
        ui.thread_drawer_open = false;
        ui.composer_target_thread = false;
        ui.show_subject = false;
        ui.composer.clear_reply();
        ui.composer.subject.clear();
        ui.composer.close_mentions();
        ui.chat_scroll = ScrollState::default();
        ui.thread_scroll = ScrollState::default();
        self.loading_messages = true;

        self.messages = self
            .storage
            .list_root_messages(&new_key, 80, None)
            .unwrap_or_default();
        self.members = self.storage.list_members(&new_key).unwrap_or_default();
        self.restore_draft(ui);
        self.focus_conversation_pane(ui);

        if let Some(conv) = self.conversations.iter_mut().find(|c| c.conv_key == new_key) {
            conv.unread_count = 0;
        }
        let now_ts = Utc::now().to_rfc3339();

        self.sync_ctrl.set_focus(Some(target.clone()), None);
        self.sync_ctrl.request(SyncCommand::SyncActiveNow);

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let conv_key = new_key.clone();
        tokio::spawn(async move {
            let mark_storage = storage.clone();
            let mark_conv_key = conv_key.clone();
            let mark_read_fut = tokio::task::spawn_blocking(move || {
                let _ = mark_storage.mark_conversation_read(&mark_conv_key, &now_ts);
            });
            let chat_read_fut = async {
                if let ConversationTarget::Chat { ref chat_id } = target {
                    let _ = graph.mark_chat_read(chat_id).await;
                }
            };
            let (msgs_res, members_res, _, _) = tokio::join!(
                graph.list_messages(&target, 60),
                graph.list_members(&target),
                mark_read_fut,
                chat_read_fut,
            );
            let fetched_msgs = msgs_res.unwrap_or_default();
            let fetched_members = members_res.ok();
            let db_conv_key = conv_key.clone();
            let (messages, members) = tokio::task::spawn_blocking(move || {
                if !fetched_msgs.is_empty() {
                    let _ = storage.upsert_messages(&fetched_msgs);
                }
                if let Some(ref members) = fetched_members {
                    if !members.is_empty() {
                        let _ = storage.upsert_members(&db_conv_key, members);
                    }
                }
                let messages = storage
                    .list_root_messages(&db_conv_key, 80, None)
                    .unwrap_or(fetched_msgs);
                let members = storage.list_members(&db_conv_key).unwrap_or_default();
                (messages, members)
            })
            .await
            .unwrap_or_default();

            let _ = tx.send(UiMsg::MessagesLoaded {
                conv_key,
                messages,
                members,
            });
        });
    }

    fn focus_conversation_pane(&self, ui: &mut UiState) {
        ui.narrow_pane = NarrowPane::Chat;
        ui.set_focus(Focus::Chat);
    }

    pub fn select_nav_row(&mut self, ui: &mut UiState, idx: usize) {
        let Some(row) = nav_pane::nav_row_at(self, ui, idx) else {
            return;
        };
        ui.nav_selected = idx;
        match row {
            NavRow::Chat { conv_key, .. } => {
                if let Some(target) = self
                    .conversations
                    .iter()
                    .find(|c| c.conv_key == conv_key)
                    .map(|c| c.target.clone())
                    .or_else(|| ConversationTarget::from_conv_key(&conv_key))
                {
                    self.select_conversation(ui, target);
                }
            }
            NavRow::TeamHeader { team_id, .. } => {
                if !ui.collapsed_team_ids.remove(&team_id) {
                    ui.collapsed_team_ids.insert(team_id);
                }
            }
            NavRow::TeamChannel { conv_key, .. } => {
                if let Some(target) = ConversationTarget::from_conv_key(&conv_key) {
                    self.select_conversation(ui, target);
                }
            }
            NavRow::ActivityMention {
                conv_key,
                msg_id,
                parent_msg_id,
                ..
            } => {
                if let Some(target) = ConversationTarget::from_conv_key(&conv_key) {
                    self.select_conversation(ui, target);
                    if let Some(root) = parent_msg_id {
                        self.open_thread_by_id(ui, root);
                    } else if let Some(m_idx) =
                        self.messages.iter().position(|m| m.msg_id == msg_id)
                    {
                        ui.chat_scroll.selected_idx = Some(m_idx);
                    }
                }
            }
            NavRow::MailFolderItem {
                folder_id,
                display_name,
                ..
            } => {
                self.select_mail_folder(ui, folder_id, display_name);
            }
            NavRow::MailItem { mail_id, .. } => {
                self.select_mail(ui, mail_id);
                self.focus_conversation_pane(ui);
            }
            NavRow::CalendarItem { event_id, .. } => {
                self.select_calendar_event(ui, event_id);
                self.focus_conversation_pane(ui);
            }
            NavRow::ContactRow { contact_id, .. } => {
                self.select_contact(ui, contact_id);
                self.focus_conversation_pane(ui);
            }
            NavRow::TodoListChip { list_id, .. } => {
                self.select_todo_list(ui, list_id);
            }
            NavRow::TodoTaskRow { task_id, .. } => {
                self.select_todo_task(ui, task_id);
                self.focus_conversation_pane(ui);
            }
            NavRow::OnenoteGroupHeader { .. } => {}
            NavRow::OnenoteNotebook { notebook_key, .. } => {
                self.active_notebook_key = Some(notebook_key.clone());
                self.toggle_onenote_notebook(ui, notebook_key);
            }
            NavRow::OnenoteSectionGroup { .. } => {}
            NavRow::OnenoteSection { section_key, .. } => {
                self.open_onenote_section(ui, section_key);
                self.focus_conversation_pane(ui);
            }
        }
    }

    pub fn open_thread(&mut self, ui: &mut UiState, msg_index: usize) {
        let Some(msg) = self.messages.get(msg_index).cloned() else {
            return;
        };
        let is_channel = self
            .active_target
            .as_ref()
            .is_some_and(ConversationTarget::is_channel);
        if !is_channel {
            ui.composer.set_reply(ReplyPreview {
                msg_id: msg.msg_id.clone(),
                author: msg.author_name.clone(),
                text: msg.body_plain.chars().take(80).collect(),
            });
            ui.set_focus(Focus::Composer);
            return;
        }
        let root_id = msg.parent_msg_id.unwrap_or(msg.msg_id);
        self.open_thread_by_id(ui, root_id);
    }

    pub fn open_thread_by_id(&mut self, ui: &mut UiState, root_msg_id: String) {
        let Some(ConversationTarget::Channel {
            team_id,
            channel_id,
        }) = self.active_target.clone()
        else {
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };

        self.stash_draft(ui);
        self.loading_thread = true;
        if let Ok(Some(cached)) = self.storage.get_thread(&conv_key, &root_msg_id, 100) {
            self.active_thread = Some(cached);
        } else if let Some(root) = self
            .messages
            .iter()
            .find(|m| m.msg_id == root_msg_id)
            .cloned()
        {
            self.active_thread = Some(ThreadView {
                conv_key: conv_key.clone(),
                root,
                replies: Vec::new(),
            });
        }
        ui.thread_drawer_open = true;
        ui.composer_target_thread = true;
        ui.narrow_pane = NarrowPane::Thread;
        ui.thread_scroll = ScrollState::default();
        self.restore_draft(ui);
        ui.set_focus(Focus::Thread);

        self.sync_ctrl
            .set_focus(self.active_target.clone(), Some(root_msg_id.clone()));

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Ok(thread) = graph
                .get_thread(&team_id, &channel_id, &root_msg_id, 100)
                .await
            {
                let mut all = Vec::with_capacity(1 + thread.replies.len());
                all.push(thread.root.clone());
                all.extend(thread.replies.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = storage.upsert_messages(&all);
                })
                .await;
                let _ = tx.send(UiMsg::ThreadLoaded {
                    conv_key,
                    root_msg_id,
                    thread,
                });
            }
        });
    }

    pub fn close_thread(&mut self, ui: &mut UiState) {
        self.stash_draft(ui);
        ui.thread_drawer_open = false;
        ui.composer_target_thread = false;
        self.active_thread = None;
        ui.narrow_pane = NarrowPane::Chat;
        self.sync_ctrl.set_focus(self.active_target.clone(), None);
        self.restore_draft(ui);
        ui.set_focus(Focus::Chat);
    }

    pub fn send_composer(&mut self, ui: &mut UiState) {
        let Some(target) = self.active_target.clone() else {
            ui.info("전송할 대화가 선택되지 않았습니다");
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let text = ui.composer.text().trim().to_string();
        if text.is_empty() && ui.composer.pending_images.is_empty() {
            return;
        }

        let is_channel = target.is_channel();
        let thread_root_id = if ui.composer_target_thread {
            self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
        } else {
            None
        };
        let subject = if is_channel && thread_root_id.is_none() && ui.show_subject {
            ui.composer.subject_to_send()
        } else {
            None
        };
        let mentions = ui.composer.compiled_mentions();
        let inline_images = ui.composer.pending_images.clone();
        let quote = if thread_root_id.is_none() {
            ui.composer.reply().map(|r| QuotedReply {
                msg_id: r.msg_id.clone(),
                author_name: r.author.clone(),
                preview_plain: r.text.clone(),
            })
        } else {
            None
        };

        let outgoing: OutgoingMessage = parser::compile_outgoing_markdown(
            &text,
            subject,
            mentions,
            Vec::new(),
            inline_images.clone(),
            quote.clone(),
        );

        let optimistic_id = if inline_images.is_empty() {
            let opt_id = format!("optimistic-{}", uuid::Uuid::new_v4());
            let author_name = self
                .my_profile
                .as_ref()
                .map(|p| p.display_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "나".to_string());
            let optimistic_msg = MessageItem {
                msg_key: format!("{conv_key}|{opt_id}"),
                conv_key: conv_key.clone(),
                msg_id: opt_id.clone(),
                parent_msg_id: thread_root_id.clone(),
                reply_to_id: quote.as_ref().map(|q| q.msg_id.clone()),
                reply_to_author: quote.as_ref().map(|q| q.author_name.clone()),
                reply_to_preview: quote.as_ref().map(|q| q.preview_plain.clone()),
                subject: outgoing.subject.clone(),
                author_id: (!self.my_user_id.is_empty()).then(|| self.my_user_id.clone()),
                author_name,
                is_me: true,
                created_at: Utc::now().to_rfc3339(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: outgoing.body_html.clone(),
                body_plain: text.clone(),
                attachments: Vec::new(),
                mentions: Vec::new(),
                reactions: Vec::new(),
                reply_count: 0,
                web_url: None,
            };
            if let Some(ref pid) = thread_root_id {
                if let Some(thread) = self.active_thread.as_mut() {
                    if &thread.root.msg_id == pid {
                        thread.replies.push(optimistic_msg);
                        ui.thread_scroll.bottom();
                    }
                }
            } else {
                self.messages.push(optimistic_msg);
                ui.chat_scroll.bottom();
            }
            Some(opt_id)
        } else {
            None
        };

        let task_label = "메시지 전송".to_string();
        ui.tasks.begin(task_label.clone());
        self.clear_current_draft(ui);
        ui.show_subject = false;

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let quote_id = quote.map(|q| q.msg_id);

        tokio::spawn(async move {
            let result = if let Some(ref root_id) = thread_root_id {
                graph.reply_message(&target, root_id, &outgoing).await
            } else if let Some(ref reply_id) = quote_id {
                if !target.is_channel() {
                    graph.reply_message(&target, reply_id, &outgoing).await
                } else {
                    graph.send_message(&target, &outgoing).await
                }
            } else {
                graph.send_message(&target, &outgoing).await
            };

            match result {
                Ok(sent) => {
                    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
                    let _ = tx.send(UiMsg::MessageSent {
                        conv_key,
                        parent_msg_id: thread_root_id,
                        optimistic_id,
                        sent,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    if let Some(opt_id) = optimistic_id {
                        let _ = tx.send(UiMsg::MessageSendFailed {
                            conv_key,
                            parent_msg_id: thread_root_id,
                            optimistic_id: opt_id,
                            text,
                        });
                    }
                    let msg = format!("전송 실패: {err}");
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((msg, ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn load_older(&mut self, ui: &mut UiState) {
        if self.oldest_reached || self.loading_messages {
            return;
        }
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let before = self.messages.first().map(|m| m.created_at.clone());
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "이전 메시지 로드".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let db_conv_key = conv_key.clone();
            let older = tokio::task::spawn_blocking(move || {
                storage
                    .list_root_messages(&db_conv_key, 50, before.as_deref())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let reached_end = older.is_empty();
            let _ = tx.send(UiMsg::OlderMessagesLoaded {
                conv_key,
                messages: older,
                reached_end,
            });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn toggle_reaction(
        &mut self,
        ui: &mut UiState,
        scope: MessageScope,
        msg_index: usize,
        reaction_type: String,
    ) {
        let Some(target) = self.active_target.clone() else {
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let Some(msg) = self.message_at(scope, msg_index).cloned() else {
            return;
        };
        let parent_msg_id = match scope {
            MessageScope::ThreadReply => self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
            _ => msg.parent_msg_id.clone(),
        };
        let currently_set = msg
            .reactions
            .iter()
            .any(|r| r.reaction_type == reaction_type && r.me_reacted);
        let enable = !currently_set;

        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let msg_id = msg.msg_id.clone();
        let task_label = "리액션".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .set_reaction(
                    &target,
                    &msg_id,
                    parent_msg_id.as_deref(),
                    &reaction_type,
                    enable,
                )
                .await
            {
                Ok(()) => {
                    let _ = tx.send(UiMsg::ReactionUpdated {
                        conv_key,
                        msg_id,
                        reaction_type,
                        added: enable,
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
                        toast: Some((format!("리액션 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_message(
        &mut self,
        ui: &mut UiState,
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    ) {
        let Some(target) = self.active_target.clone() else {
            return;
        };
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = "메시지 삭제".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .delete_message(&target, &msg_id, parent_msg_id.as_deref())
                .await
            {
                Ok(()) => {
                    let _ = tx.send(UiMsg::MessageDeleted {
                        conv_key,
                        msg_id,
                        parent_msg_id,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("메시지를 삭제했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("삭제 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn download_attachment(
        &mut self,
        ui: &mut UiState,
        scope: MessageScope,
        msg_index: usize,
        attach_index: usize,
    ) {
        let Some(msg) = self.message_at(scope, msg_index) else {
            return;
        };
        let Some(att) = msg.attachments.get(attach_index).cloned() else {
            ui.info("첨부파일을 찾을 수 없습니다");
            return;
        };
        let att_name = att.name.clone().unwrap_or_else(|| "attachment".to_string());
        let graph = self.graph.clone();
        let dest = self.download_dir.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("다운로드: {att_name}");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.download_attachment(&att, &dest).await {
                Ok(file) => {
                    let msg = format!("저장됨: {}", file.path.display());
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
                        toast: Some((format!("다운로드 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn send_file(&mut self, ui: &mut UiState, path: String, caption: String) {
        let Some(target) = self.active_target.clone() else {
            ui.info("전송할 대화가 선택되지 않았습니다");
            return;
        };
        let Some(conv_key) = self.active_conv_key.clone() else {
            return;
        };
        let thread_root_id = if ui.composer_target_thread {
            self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
        } else {
            None
        };
        let caption_html = if caption.trim().is_empty() {
            None
        } else {
            Some(
                parser::compile_outgoing_markdown(
                    caption.trim(),
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    None,
                )
                .body_html,
            )
        };

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "파일 업로드".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .send_file(
                    &target,
                    std::path::Path::new(&path),
                    caption_html.as_deref(),
                    thread_root_id.as_deref(),
                )
                .await
            {
                Ok(sent) => {
                    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
                    let _ = tx.send(UiMsg::AttachmentSent { conv_key, sent });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("파일을 전송했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("파일 전송 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn search_messages(&mut self, ui: &mut UiState, query: String) {
        self.search_query = query.clone();
        if query.trim().is_empty() {
            self.search_results.clear();
            self.searching = false;
            return;
        }
        self.searching = true;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "검색".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let search_query = query.clone();
            let hits = tokio::task::spawn_blocking(move || {
                storage
                    .search_messages(&search_query, None, 100)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let _ = tx.send(UiMsg::SearchResults { query, hits });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn search_users(&mut self, ui: &mut UiState, query: String) {
        if query.trim().is_empty() {
            self.user_search_results.clear();
            self.user_searching = false;
            return;
        }
        self.user_searching = true;
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = "사용자 검색".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let users = graph.search_users(&query, 20).await.unwrap_or_default();
            let _ = tx.send(UiMsg::UserSearchResults { query, users });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn create_chat(&mut self, ui: &mut UiState, peers: Vec<String>, topic: Option<String>) {
        if peers.is_empty() {
            ui.info("대화 상대를 선택하세요");
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "새 채팅 생성".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.create_chat(&peers, topic.as_deref()).await {
                Ok(conv) => {
                    let _ = storage.upsert_conversations(std::slice::from_ref(&conv));
                    let _ = tx.send(UiMsg::ChatCreated(conv));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("새 채팅이 열렸습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("채팅 생성 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn image_cache_key(owner_id: &str, att_id: &str, cols: u16, rows: u16) -> String {
        format!("{owner_id}:{att_id}:{cols}x{rows}")
    }

    fn rendered_image_bytes(rendered: &RenderedTerminalImage) -> usize {
        rendered.approx_heap_bytes().max(64)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_bounded_image_cache(
        cache: &mut HashMap<String, RenderedTerminalImage>,
        order: &mut VecDeque<String>,
        bytes_total: &mut usize,
        other_cache: &mut HashMap<String, RenderedTerminalImage>,
        other_order: &mut VecDeque<String>,
        other_bytes_total: &mut usize,
        max_entries: usize,
        cache_key: String,
        rendered: RenderedTerminalImage,
    ) {
        if let Some(prev) = cache.remove(&cache_key) {
            *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&prev));
            order.retain(|k| k != &cache_key);
        }
        let entry_bytes = Self::rendered_image_bytes(&rendered);
        cache.insert(cache_key.clone(), rendered);
        order.push_back(cache_key);
        *bytes_total = bytes_total.saturating_add(entry_bytes);

        while order.len() > max_entries {
            let Some(evicted) = order.pop_front() else {
                break;
            };
            if let Some(removed) = cache.remove(&evicted) {
                *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }

        // Enforce strict 8 MiB combined heap budget across both image caches,
        // keeping at least the newly inserted image if a single image exceeds budget.
        while bytes_total.saturating_add(*other_bytes_total) > img_preview::MAX_IMAGE_CACHE_BYTES {
            if !other_order.is_empty() {
                if let Some(evicted) = other_order.pop_front() {
                    if let Some(removed) = other_cache.remove(&evicted) {
                        *other_bytes_total = other_bytes_total
                            .saturating_sub(Self::rendered_image_bytes(&removed));
                    }
                }
            } else if order.len() > 1 {
                if let Some(evicted) = order.pop_front() {
                    if let Some(removed) = cache.remove(&evicted) {
                        *bytes_total =
                            bytes_total.saturating_sub(Self::rendered_image_bytes(&removed));
                    }
                }
            } else {
                break;
            }
        }
    }

    pub fn insert_image_cache(&mut self, cache_key: String, rendered: RenderedTerminalImage) {
        const MAX_IMAGE_CACHE: usize = 32;
        self.image_pending.remove(&cache_key);
        self.image_errors.remove(&cache_key);
        Self::insert_bounded_image_cache(
            &mut self.image_cache,
            &mut self.image_cache_order,
            &mut self.image_cache_bytes,
            &mut self.onenote_image_cache,
            &mut self.onenote_image_cache_order,
            &mut self.onenote_image_cache_bytes,
            MAX_IMAGE_CACHE,
            cache_key,
            rendered,
        );
    }

    pub fn insert_onenote_image_cache(
        &mut self,
        cache_key: String,
        rendered: RenderedTerminalImage,
    ) {
        const MAX_ONENOTE_IMAGE_CACHE: usize = 24;
        self.onenote_image_pending.remove(&cache_key);
        self.onenote_image_errors.remove(&cache_key);
        Self::insert_bounded_image_cache(
            &mut self.onenote_image_cache,
            &mut self.onenote_image_cache_order,
            &mut self.onenote_image_cache_bytes,
            &mut self.image_cache,
            &mut self.image_cache_order,
            &mut self.image_cache_bytes,
            MAX_ONENOTE_IMAGE_CACHE,
            cache_key,
            rendered,
        );
    }

    /// Evicts oldest OneNote cached images until `onenote_image_cache_bytes <= max_bytes`,
    /// returning total evicted bytes.
    pub fn trim_onenote_image_cache_to(&mut self, max_bytes: usize) -> usize {
        let mut evicted_bytes = 0usize;
        while self.onenote_image_cache_bytes > max_bytes {
            let Some(evicted) = self.onenote_image_cache_order.pop_front() else {
                self.onenote_image_cache_bytes = 0;
                break;
            };
            if let Some(removed) = self.onenote_image_cache.remove(&evicted) {
                let bytes = Self::rendered_image_bytes(&removed);
                self.onenote_image_cache_bytes =
                    self.onenote_image_cache_bytes.saturating_sub(bytes);
                evicted_bytes = evicted_bytes.saturating_add(bytes);
            }
        }
        evicted_bytes
    }

    pub fn clear_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
    }

    pub fn clear_onenote_image_cache(&mut self) {
        self.onenote_image_cache.clear();
        self.onenote_image_cache_order.clear();
        self.onenote_image_cache_bytes = 0;
    }

    pub fn clear_image_caches(&mut self) {
        self.clear_image_cache();
        self.clear_onenote_image_cache();
    }

    pub fn request_teams_image(
        &mut self,
        msg_id: String,
        att: AttachmentInfo,
        cols: u16,
        rows: u16,
    ) {
        let key = Self::image_cache_key(&msg_id, &att.id, cols, rows);
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        self.image_pending.insert(key.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let bg = (20, 22, 26);
        let att_id = att.id.clone();

        tokio::spawn(async move {
            let fetched = graph.fetch_attachment_bytes(&att).await;
            match fetched {
                Ok((_, _, bytes)) => {
                    let decode_res = tokio::task::spawn_blocking(move || {
                        img_preview::decode_and_render_halfblocks(&bytes, cols, rows, bg)
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
        mail_id: String,
        att: MailAttachment,
        cols: u16,
        rows: u16,
    ) {
        let key = Self::image_cache_key(&mail_id, &att.id, cols, rows);
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        self.image_pending.insert(key.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let bg = (20, 22, 26);
        let att_id = att.id.clone();
        let inline_b64 = att.content_bytes_b64.clone();

        tokio::spawn(async move {
            let bytes_res: anyhow::Result<Vec<u8>> = if let Some(b64) = inline_b64 {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|e| anyhow::anyhow!("base64 디코드 실패: {e}"))
            } else {
                graph
                    .fetch_mail_attachment_bytes(&mail_id, &att_id)
                    .await
                    .map(|(_, _, b)| b)
            };

            match bytes_res {
                Ok(bytes) => {
                    let decode_res = tokio::task::spawn_blocking(move || {
                        img_preview::decode_and_render_halfblocks(&bytes, cols, rows, bg)
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
                                owner_id: mail_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: mail_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageDecodeFailed {
                        cache_key: key,
                        owner_id: mail_id,
                        att_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_preview_image(&mut self, target: &PreviewTarget, cols: u16, rows: u16) {
        match target {
            PreviewTarget::Teams { msg_id, attachment } => {
                self.request_teams_image(msg_id.clone(), attachment.clone(), cols, rows);
            }
            PreviewTarget::Mail {
                mail_id,
                attachment,
            } => {
                self.request_mail_image(mail_id.clone(), attachment.clone(), cols, rows);
            }
            PreviewTarget::OneNote {
                page_key,
                resource_id,
                resource,
                ..
            } => {
                self.request_onenote_preview_image(
                    page_key.clone(),
                    resource_id.clone(),
                    resource.clone(),
                    cols,
                    rows,
                );
            }
        }
    }

    /// Loads a full-size OneNote page image into the shared preview cache
    /// (keyed like every other `PreviewTarget` so `ImagePreviewOverlay` can render it).
    fn request_onenote_preview_image(
        &mut self,
        page_key: String,
        resource_id: String,
        resource: OnenoteResource,
        cols: u16,
        rows: u16,
    ) {
        let key = Self::image_cache_key(&page_key, &resource_id, cols, rows);
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        let url = resource.resource_url.trim().to_string();
        if url.is_empty() {
            self.image_errors.insert(key, "이미지 URL이 없습니다".to_string());
            return;
        }
        self.image_pending.insert(key.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let bg = (20, 22, 26);

        tokio::spawn(async move {
            match graph.download_onenote_resource_bytes(&url).await {
                Ok(bytes) => {
                    let decode_res = tokio::task::spawn_blocking(move || {
                        img_preview::decode_and_render_halfblocks(&bytes, cols, rows, bg)
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
                                owner_id: page_key,
                                att_id: resource_id,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: page_key,
                                att_id: resource_id,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageDecodeFailed {
                        cache_key: key,
                        owner_id: page_key,
                        att_id: resource_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn download_mail_attachment(
        &mut self,
        ui: &mut UiState,
        mail_id: String,
        att_id: String,
        name: String,
    ) {
        let graph = self.graph.clone();
        let dir = self.download_dir.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("첨부 다운로드: {name}");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.download_mail_attachment(&mail_id, &att_id, &dir).await {
                Ok(file) => {
                    let toast = Some((
                        format!("저장됨: {}", file.path.display()),
                        ToastKind::Success,
                    ));
                    let _ = tx.send(UiMsg::AttachmentDownloaded(file));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("다운로드 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Outlook Mail, Calendar, Contacts, To Do & Self-Update Actions
    // ------------------------------------------------------------------

    pub fn active_mail(&self) -> Option<&MailMessage> {
        if let Some(m) = self.active_mail.as_ref() {
            if self.active_mail_id.as_deref() == Some(&m.id) {
                return Some(m);
            }
        }
        let id = self.active_mail_id.as_deref()?;
        self.mails.iter().find(|m| m.id == id)
    }

    pub fn active_event(&self) -> Option<&CalendarEvent> {
        let id = self.active_event_id.as_deref()?;
        self.calendar_events.iter().find(|e| e.id == id)
    }

    pub fn active_contact(&self) -> Option<&ContactItem> {
        let id = self.active_contact_id.as_deref()?;
        self.contacts.iter().find(|c| c.id == id)
    }

    pub fn active_todo_task(&self) -> Option<&TodoTask> {
        let id = self.active_todo_task_id.as_deref()?;
        self.todo_tasks.iter().find(|t| t.id == id)
    }

    pub fn active_todo_list(&self, ui: &UiState) -> Option<&TodoList> {
        let id = ui.active_todo_list_id.as_deref()?;
        self.todo_lists.iter().find(|l| l.id == id)
    }

    pub fn unread_mail_count(&self) -> i32 {
        let folder_unread: i32 = self
            .mail_folders
            .iter()
            .filter(|f| f.well_known_name.as_deref() == Some("inbox"))
            .map(|f| f.unread_item_count.max(0))
            .sum();
        if folder_unread > 0 {
            folder_unread
        } else {
            self.mails.iter().filter(|m| !m.is_read).count() as i32
        }
    }

    pub fn event_join_url(ev: &CalendarEvent) -> Option<&str> {
        ev.online_meeting_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| ev.web_link.as_deref().filter(|s| !s.is_empty()))
    }

    pub fn select_mail_folder(
        &mut self,
        ui: &mut UiState,
        folder_id: String,
        display_name: String,
    ) {
        ui.active_mail_folder_id = folder_id.clone();
        ui.active_mail_folder_name = display_name;
        self.mails = self
            .storage
            .list_mail_summaries(Some(&folder_id), 100)
            .unwrap_or_default();
        self.active_mail_id = None;
        self.active_mail = None;
        self.loading_mail = false;
        if let Some(first_id) = self.mails.first().map(|m| m.id.clone()) {
            self.select_mail(ui, first_id);
        } else {
            ui.mail_scroll = ScrollState::default();
            ui.mail_attach_index = 0;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        self.sync_ctrl.send(SyncCommand::SyncMailFolder {
            folder_id: folder_id.clone(),
        });
        tokio::spawn(async move {
            if let Ok(list) = graph.list_mail_messages(&folder_id, 50).await {
                let db_folder_id = folder_id.clone();
                let cached = tokio::task::spawn_blocking(move || {
                    let _ = storage.upsert_mail_messages(&list);
                    storage
                        .list_mail_summaries(Some(&db_folder_id), 100)
                        .unwrap_or(list)
                })
                .await
                .unwrap_or_default();
                let _ = tx.send(UiMsg::MailFolderLoaded {
                    folder_id,
                    mails: cached,
                });
            }
        });
    }

    fn spawn_mail_prefetch(&self, mail_id: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let db_storage = storage.clone();
            let db_id = mail_id.clone();
            let cached = tokio::task::spawn_blocking(move || {
                db_storage.get_mail_message(&db_id).ok().flatten()
            })
            .await
            .ok()
            .flatten();
            if let Some(mail) = cached {
                if !mail.body_html.is_empty() && (!mail.has_attachments || !mail.attachments.is_empty()) {
                    let _ = tx.send(UiMsg::MailDetailLoaded { mail });
                    return;
                }
            }
            match graph.get_mail_message(&mail_id).await {
                Ok(mail) => {
                    let mail_to_store = mail.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_mail_messages(std::slice::from_ref(&mail_to_store));
                    })
                    .await;
                    let _ = tx.send(UiMsg::MailDetailLoaded { mail });
                }
                Err(_) => {
                    let _ = tx.send(UiMsg::MailPrefetchFailed { mail_id });
                }
            }
        });
    }

    pub fn select_mail(&mut self, ui: &mut UiState, mail_id: String) {
        self.active_mail_id = Some(mail_id.clone());
        let in_mem = self.mails.iter().find(|m| m.id == mail_id).cloned();
        self.active_mail = if in_mem
            .as_ref()
            .is_some_and(|m| !m.body_html.is_empty() && (!m.has_attachments || !m.attachments.is_empty()))
        {
            in_mem
        } else {
            self.storage
                .get_mail_message(&mail_id)
                .ok()
                .flatten()
                .filter(|m| !m.body_html.is_empty() || !m.body_plain.is_empty())
                .or(in_mem)
        };
        ui.mail_scroll = ScrollState::default();
        ui.mail_scroll.top();
        ui.mail_attach_index = 0;

        let needs_detail = self
            .active_mail
            .as_ref()
            .is_none_or(|m| m.body_html.is_empty() || (m.has_attachments && m.attachments.is_empty()));
        self.loading_mail = needs_detail;

        if tokio::runtime::Handle::try_current().is_ok() {
            if needs_detail {
                let graph = self.graph.clone();
                let storage = self.storage.clone();
                let tx = self.ui_tx.clone();
                let req_id = mail_id.clone();
                tokio::spawn(async move {
                    match graph.get_mail_message(&req_id).await {
                        Ok(mail) => {
                            let mail_to_store = mail.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                let _ = storage.upsert_mail_messages(std::slice::from_ref(&mail_to_store));
                            })
                            .await;
                            let _ = tx.send(UiMsg::MailDetailLoaded { mail });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::MailDetailFailed { mail_id: req_id });
                            let _ = tx.send(UiMsg::TaskDone {
                                label: "메일 읽기".to_string(),
                                error: Some(err.to_string()),
                                toast: None,
                            });
                        }
                    }
                });
            }

            // Prefetch up to 2 neighboring mails (idx - 1, idx + 1) when header-only.
            if let Some(idx) = self.mails.iter().position(|m| m.id == mail_id) {
                let mut neighbors = Vec::with_capacity(2);
                if idx > 0 {
                    neighbors.push(idx - 1);
                }
                if idx + 1 < self.mails.len() {
                    neighbors.push(idx + 1);
                }
                for n_idx in neighbors {
                    if let Some(neighbor) = self.mails.get(n_idx) {
                        if neighbor.body_html.is_empty()
                            && !self.mail_prefetch_pending.contains(&neighbor.id)
                        {
                            let n_id = neighbor.id.clone();
                            self.mail_prefetch_pending.insert(n_id.clone());
                            self.spawn_mail_prefetch(n_id);
                        }
                    }
                }
            }
        }
    }

    pub fn submit_mail_compose(
        &mut self,
        ui: &mut UiState,
        mode: MailComposeMode,
        to: Vec<String>,
        cc: Vec<String>,
        subject: String,
        body_markdown: String,
        attachments: Vec<PathBuf>,
    ) {
        let compiled = parser::compile_outgoing_markdown(
            &body_markdown,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
        );
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let sync_ctrl = self.sync_ctrl.clone();
        let task_label = match &mode {
            MailComposeMode::New => "메일 전송".to_string(),
            MailComposeMode::Reply { .. } => "답장 전송".to_string(),
            MailComposeMode::ReplyAll { .. } => "전체 답장 전송".to_string(),
            MailComposeMode::Forward { .. } => "메일 전달".to_string(),
        };
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let res = match mode {
                MailComposeMode::New => {
                    graph
                        .send_mail(&OutgoingMail {
                            subject,
                            body_html: compiled.body_html,
                            to,
                            cc,
                            bcc: Vec::new(),
                            importance: None,
                            attachments,
                            save_to_sent_items: true,
                        })
                        .await
                }
                MailComposeMode::Reply { message_id } => {
                    graph
                        .reply_mail(&message_id, &compiled.body_html, false)
                        .await
                }
                MailComposeMode::ReplyAll { message_id } => {
                    graph
                        .reply_mail(&message_id, &compiled.body_html, true)
                        .await
                }
                MailComposeMode::Forward { message_id } => {
                    graph
                        .forward_mail(&message_id, &to, &compiled.body_html)
                        .await
                }
            };
            match res {
                Ok(()) => {
                    sync_ctrl.send(SyncCommand::RefreshOutlook);
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("메일을 전송했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("메일 전송 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    fn set_mail_read_in_memory(&mut self, mail_id: &str, is_read: bool) {
        if let Some(m) = self.mails.iter_mut().find(|m| m.id == mail_id) {
            m.is_read = is_read;
        }
        if let Some(m) = self.active_mail.as_mut().filter(|m| m.id == mail_id) {
            m.is_read = is_read;
        }
    }

    fn set_mail_flagged_in_memory(&mut self, mail_id: &str, flagged: bool) {
        if let Some(m) = self.mails.iter_mut().find(|m| m.id == mail_id) {
            m.flagged = flagged;
        }
        if let Some(m) = self.active_mail.as_mut().filter(|m| m.id == mail_id) {
            m.flagged = flagged;
        }
    }

    fn optimistic_remove_active_mail(
        &mut self,
        ui: &mut UiState,
    ) -> Option<(String, Option<usize>, MailMessage)> {
        let mail = self.active_mail().cloned()?;
        let folder_id = ui.active_mail_folder_id.clone();
        let idx = self.mails.iter().position(|m| m.id == mail.id);
        if let Some(pos) = idx {
            self.mails.remove(pos);
            let next_id = self
                .mails
                .get(pos)
                .or_else(|| self.mails.last())
                .map(|m| m.id.clone());
            if let Some(id) = next_id {
                self.select_mail(ui, id);
            } else {
                self.active_mail_id = None;
                self.active_mail = None;
                self.loading_mail = false;
            }
        } else if self.active_mail_id.as_deref() == Some(&mail.id) {
            if let Some(id) = self.mails.first().map(|m| m.id.clone()) {
                self.select_mail(ui, id);
            } else {
                self.active_mail_id = None;
                self.active_mail = None;
                self.loading_mail = false;
            }
        }
        Some((folder_id, idx, mail))
    }

    pub fn toggle_active_mail_read(&mut self, ui: &mut UiState) {
        let Some(mail) = self.active_mail().cloned() else {
            return;
        };
        let prev_read = mail.is_read;
        let next_read = !prev_read;
        let target_folder = mail
            .folder_id
            .as_deref()
            .unwrap_or(&ui.active_mail_folder_id);
        let folder_prev_unread = self
            .mail_folders
            .iter_mut()
            .find(|f| {
                f.id == target_folder
                    || f.well_known_name
                        .as_deref()
                        .is_some_and(|w| w.eq_ignore_ascii_case(target_folder))
            })
            .map(|f| {
                let prev = f.unread_item_count;
                let delta = if next_read { -1 } else { 1 };
                f.unread_item_count = (prev + delta).max(0);
                (f.id.clone(), prev)
            });
        self.set_mail_read_in_memory(&mail.id, next_read);

        let task_label = if next_read { "읽음 표시" } else { "안읽음 표시" }.to_string();
        ui.tasks.begin(task_label.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            match graph.set_mail_read(&mail.id, next_read).await {
                Ok(()) => {
                    let mut updated = mail;
                    updated.is_read = next_read;
                    let _ = storage.upsert_mail_messages(std::slice::from_ref(&updated));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label.clone(),
                        error: None,
                        toast: Some((task_label, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::MailReadRollback {
                        mail_id: mail.id,
                        prev_read,
                        folder_prev_unread,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("메일 상태 변경 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn toggle_active_mail_flag(&mut self, ui: &mut UiState) {
        let Some(mail) = self.active_mail().cloned() else {
            return;
        };
        let prev_flagged = mail.flagged;
        let next_flag = !prev_flagged;
        self.set_mail_flagged_in_memory(&mail.id, next_flag);

        let task_label = if next_flag { "중요 표시" } else { "표시 해제" }.to_string();
        ui.tasks.begin(task_label.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            match graph.set_mail_flag(&mail.id, next_flag).await {
                Ok(()) => {
                    let mut updated = mail;
                    updated.flagged = next_flag;
                    let _ = storage.upsert_mail_messages(std::slice::from_ref(&updated));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label.clone(),
                        error: None,
                        toast: Some((task_label, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::MailFlagRollback {
                        mail_id: mail.id,
                        prev_flagged,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("깃발 변경 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn archive_active_mail(&mut self, ui: &mut UiState) {
        let Some((folder_id, index, mail)) = self.optimistic_remove_active_mail(ui) else {
            return;
        };
        let task_label = "메일 보관".to_string();
        ui.tasks.begin(task_label.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            match graph.move_mail_message(&mail.id, "archive").await {
                Ok(moved) => {
                    let _ = storage.delete_mail_messages_by_ids(std::slice::from_ref(&mail.id));
                    let _ = storage.upsert_mail_messages(&[moved]);
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("보관함으로 이동했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::MailRemoveRollback {
                        folder_id,
                        index,
                        mail,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("보관 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_active_mail(&mut self, ui: &mut UiState) {
        let Some((folder_id, index, mail)) = self.optimistic_remove_active_mail(ui) else {
            return;
        };
        let task_label = "메일 삭제".to_string();
        ui.tasks.begin(task_label.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            match graph.delete_mail_message(&mail.id).await {
                Ok(()) => {
                    let _ = storage.delete_mail_messages_by_ids(std::slice::from_ref(&mail.id));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("메일을 삭제했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::MailRemoveRollback {
                        folder_id,
                        index,
                        mail,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("메일 삭제 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn select_calendar_event(&mut self, ui: &mut UiState, event_id: String) {
        self.active_event_id = Some(event_id);
        ui.mail_scroll = ScrollState::default();
        ui.mail_scroll.top();
    }

    pub fn create_calendar_event(&mut self, ui: &mut UiState, event: OutgoingEvent) {
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "일정 생성".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.create_calendar_event(&event).await {
                Ok(created) => {
                    let _ = storage.upsert_calendar_events(std::slice::from_ref(&created));
                    let _ = tx.send(UiMsg::CalendarEventCreated { event: created });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("일정을 생성했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("일정 생성 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn respond_active_event(&mut self, ui: &mut UiState, response: &str) {
        let Some(ev) = self.active_event().cloned() else {
            return;
        };
        let resp_owned = response.to_string();
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("일정 응답({response})");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.respond_calendar_event(&ev.id, &resp_owned, None).await {
                Ok(()) => {
                    let mut updated = ev.clone();
                    updated.response_status = match resp_owned.as_str() {
                        "accept" => "accepted",
                        "tentativelyAccept" => "tentativelyAccepted",
                        "decline" => "declined",
                        other => other,
                    }
                    .to_string();
                    let _ = storage.upsert_calendar_events(&[updated]);
                    let _ = tx.send(UiMsg::CalendarEventResponded {
                        event_id: ev.id,
                        response: resp_owned,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("일정 응답을 보냈습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("일정 응답 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_active_event(&mut self, ui: &mut UiState) {
        let Some(ev) = self.active_event().cloned() else {
            return;
        };
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "일정 삭제".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.delete_calendar_event(&ev.id).await {
                Ok(()) => {
                    let _ = storage.delete_calendar_event(&ev.id);
                    let _ = tx.send(UiMsg::CalendarEventDeleted { event_id: ev.id });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("일정을 삭제했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("일정 삭제 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn open_active_event_meeting(&mut self, ui: &mut UiState) {
        let Some(ev) = self.active_event() else {
            ui.info("선택된 일정이 없습니다");
            return;
        };
        let Some(url) = Self::event_join_url(ev) else {
            ui.info("이 일정에 연결된 회의/웹 링크가 없습니다");
            return;
        };
        match open::that(url) {
            Ok(()) => ui.success("브라우저에서 회의를 열었습니다"),
            Err(err) => ui.error(format!("링크 열기 실패: {err}")),
        }
    }

    pub fn select_contact(&mut self, ui: &mut UiState, contact_id: String) {
        self.active_contact_id = Some(contact_id);
        ui.mail_scroll = ScrollState::default();
        ui.mail_scroll.top();
    }

    pub fn create_contact(&mut self, ui: &mut UiState, contact: OutgoingContact) {
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "연락처 추가".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.create_contact(&contact).await {
                Ok(created) => {
                    let _ = storage.upsert_contacts(std::slice::from_ref(&created));
                    let _ = tx.send(UiMsg::ContactCreated { contact: created });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("연락처를 추가했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("연락처 추가 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_active_contact(&mut self, ui: &mut UiState) {
        let Some(contact) = self.active_contact().cloned() else {
            return;
        };
        if contact.source != "contact" {
            ui.info("조직 디렉터리 사용자는 삭제할 수 없습니다");
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "연락처 삭제".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.delete_contact(&contact.id).await {
                Ok(()) => {
                    let _ = storage.delete_contact(&contact.id);
                    let _ = tx.send(UiMsg::ContactDeleted {
                        contact_id: contact.id,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("연락처를 삭제했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("연락처 삭제 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn start_chat_with_contact(&mut self, ui: &mut UiState) {
        let Some(contact) = self.active_contact().cloned() else {
            return;
        };
        let peer_ref = if contact.source == "directory" && !contact.id.is_empty() {
            contact.id.clone()
        } else if let Some(email) = contact.primary_email() {
            email.to_string()
        } else {
            ui.error("이 연락처에 이메일 주소가 없습니다");
            return;
        };
        let label = format!("1:1 채팅 시작: {}", contact.display_name);
        ui.active_nav_tab = 0; // TAB_CHATS
        self.ensure_tab_domain_activated(ui, 0);
        self.create_chat(ui, vec![peer_ref], None);
        ui.info(label);
    }

    pub fn select_todo_list(&mut self, ui: &mut UiState, list_id: String) {
        ui.active_todo_list_id = Some(list_id.clone());
        let include_completed = ui.todo_include_completed;
        self.todo_tasks = self
            .storage
            .list_todo_tasks(&list_id, include_completed, 200)
            .unwrap_or_default();
        self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        self.sync_ctrl.send(SyncCommand::SyncTodoList {
            list_id: list_id.clone(),
        });
        tokio::spawn(async move {
            if let Ok(tasks) = graph
                .list_todo_tasks(&list_id, include_completed, 100)
                .await
            {
                let tasks_to_store = tasks.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = storage.upsert_todo_tasks(&tasks_to_store);
                })
                .await;
                let _ = tx.send(UiMsg::TodoTasksLoaded { list_id, tasks });
            }
        });
    }

    pub fn select_todo_task(&mut self, _ui: &mut UiState, task_id: String) {
        self.active_todo_task_id = Some(task_id);
    }

    pub fn cycle_todo_list(&mut self, ui: &mut UiState, delta: isize) {
        if self.todo_lists.is_empty() {
            return;
        }
        let cur_idx = ui
            .active_todo_list_id
            .as_deref()
            .and_then(|id| self.todo_lists.iter().position(|l| l.id == id))
            .unwrap_or(0);
        let len = self.todo_lists.len() as isize;
        let next_idx = ((cur_idx as isize + delta).rem_euclid(len)) as usize;
        if let Some(next_list) = self.todo_lists.get(next_idx).cloned() {
            self.select_todo_list(ui, next_list.id);
        }
    }

    pub fn toggle_todo_show_completed(&mut self, ui: &mut UiState) {
        ui.todo_include_completed = !ui.todo_include_completed;
        if let Some(list_id) = ui.active_todo_list_id.clone() {
            self.select_todo_list(ui, list_id);
        }
    }

    pub fn toggle_active_todo_completed(&mut self, ui: &mut UiState) {
        let Some(task) = self.active_todo_task().cloned() else {
            return;
        };
        let prev_status = task.status.clone();
        let next_completed = !task.is_completed();
        let next_status = if next_completed {
            "completed"
        } else {
            "notStarted"
        };
        if let Some(t) = self.todo_tasks.iter_mut().find(|t| t.id == task.id) {
            t.status = next_status.to_string();
        }
        let task_label = if next_completed { "할 일 완료" } else { "완료 취소" }.to_string();
        ui.tasks.begin(task_label.clone());
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            match graph
                .set_todo_task_completed(&task.list_id, &task.id, next_completed)
                .await
            {
                Ok(updated) => {
                    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&updated));
                    let _ = tx.send(UiMsg::TodoTaskUpserted { task: updated });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label.clone(),
                        error: None,
                        toast: Some((task_label, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TodoStatusRollback {
                        task_id: task.id,
                        prev_status,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("상태 변경 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_todo_task(&mut self, ui: &mut UiState, title: String) {
        let title = title.trim().to_string();
        if title.is_empty() {
            return;
        }
        let Some(list_id) = ui
            .active_todo_list_id
            .clone()
            .or_else(|| self.todo_lists.first().map(|l| l.id.clone()))
        else {
            ui.error("활성화된 할 일 목록이 없습니다");
            return;
        };
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "할 일 추가".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .create_todo_task(
                    &list_id,
                    &OutgoingTodoTask {
                        title,
                        body_plain: None,
                        due_date_iso: None,
                        importance: None,
                    },
                )
                .await
            {
                Ok(created) => {
                    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&created));
                    let _ = tx.send(UiMsg::TodoTaskUpserted { task: created });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("할 일을 추가했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("할 일 추가 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_active_todo(&mut self, ui: &mut UiState) {
        let Some(task) = self.active_todo_task().cloned() else {
            return;
        };
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "할 일 삭제".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.delete_todo_task(&task.list_id, &task.id).await {
                Ok(()) => {
                    let _ = storage.delete_todo_task(&task.id);
                    let _ = tx.send(UiMsg::TodoTaskDeleted { task_id: task.id });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("할 일을 삭제했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("할 일 삭제 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Microsoft OneNote Workspace Actions
    // ------------------------------------------------------------------

    pub fn active_onenote_notebook(&self) -> Option<&OnenoteNotebook> {
        if let Some(key) = self.active_notebook_key.as_deref() {
            if let Some(nb) = self
                .onenote_notebooks
                .iter()
                .find(|nb| nb.notebook_key == key)
            {
                return Some(nb);
            }
        }
        let sec = self.active_onenote_section()?;
        let nb_id = sec.notebook_id.as_deref()?;
        self.onenote_notebooks
            .iter()
            .find(|nb| nb.scope.scope_key() == sec.scope.scope_key() && nb.id == nb_id)
    }

    pub fn active_onenote_section(&self) -> Option<&OnenoteSection> {
        let key = self.active_section_key.as_deref()?;
        self.onenote_sections
            .iter()
            .find(|sec| sec.section_key == key)
    }

    /// Sections that belong to the currently active notebook (clone of in-memory state).
    pub fn sections_for_active_notebook(&self) -> Vec<OnenoteSection> {
        let Some(key) = self.active_notebook_key.as_deref() else {
            return Vec::new();
        };
        let Some(nb) = self
            .onenote_notebooks
            .iter()
            .find(|nb| nb.notebook_key == key)
        else {
            return Vec::new();
        };
        self.onenote_sections
            .iter()
            .filter(|sec| nav_pane::section_belongs_to_notebook(sec, nb))
            .cloned()
            .collect()
    }

    pub fn onenote_image_key(page_key: &str, resource_index: usize, cols: u16, rows: u16) -> String {
        format!("{page_key}:{resource_index}:{cols}x{rows}")
    }

    pub fn request_onenote_image(
        &mut self,
        page_key: String,
        resource_index: usize,
        resource: OnenoteResource,
        thumb_cols: u16,
        thumb_rows: u16,
    ) {
        let key = Self::onenote_image_key(&page_key, resource_index, thumb_cols, thumb_rows);
        if self.onenote_image_cache.contains_key(&key)
            || self.onenote_image_pending.contains(&key)
            || self.onenote_image_errors.contains(&key)
        {
            return;
        }
        let url = resource.resource_url.trim().to_string();
        if url.is_empty() {
            self.onenote_image_errors.insert(key);
            return;
        }
        self.onenote_image_pending.insert(key.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let bg = (20, 22, 26);

        tokio::spawn(async move {
            match graph.download_onenote_resource_bytes(&url).await {
                Ok(bytes) => {
                    let decode_res = tokio::task::spawn_blocking(move || {
                        img_preview::decode_and_render_halfblocks(
                            &bytes, thumb_cols, thumb_rows, bg,
                        )
                    })
                    .await;
                    match decode_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::OnenoteImageDecoded {
                                cache_key: key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::OnenoteImageDecodeFailed {
                                cache_key: key,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::OnenoteImageDecodeFailed {
                                cache_key: key,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::OnenoteImageDecodeFailed {
                        cache_key: key,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn download_onenote_attachment(
        &mut self,
        ui: &mut UiState,
        _page_key: &str,
        resource_index: usize,
        resource: OnenoteResource,
    ) {
        let url = resource.resource_url.trim().to_string();
        if url.is_empty() {
            ui.info("이 첨부에는 다운로드 링크가 없습니다");
            return;
        }
        let file_name = resource
            .file_name
            .clone()
            .or_else(|| resource.alt_text.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("onenote_{}.bin", resource_index + 1));
        let safe_name = file_name.replace(['/', '\\', ':', '|', '*', '?', '"', '<', '>'], "_");
        let graph = self.graph.clone();
        let dir = self.download_dir.clone();
        let tx = self.ui_tx.clone();
        let task_label = format!("첨부 다운로드: {safe_name}");
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            let write_res: anyhow::Result<PathBuf> = async {
                let bytes = graph.download_onenote_resource_bytes(&url).await?;
                tokio::fs::create_dir_all(&dir).await?;
                let path = dir.join(&safe_name);
                tokio::fs::write(&path, &bytes).await?;
                Ok(path)
            }
            .await;

            match write_res {
                Ok(path) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((
                            format!("저장됨: {}", path.display()),
                            ToastKind::Success,
                        )),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("다운로드 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_onenote_notebook(&mut self, ui: &mut UiState, display_name: String) {
        let name = display_name.trim().to_string();
        if name.is_empty() {
            ui.info("필기장 이름을 입력하세요");
            return;
        }
        let scope = self
            .active_notebook_key
            .as_deref()
            .and_then(|key| self.onenote_notebooks.iter().find(|nb| nb.notebook_key == key))
            .map(|nb| nb.scope.clone())
            .unwrap_or(OnenoteScope::Me);
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "필기장 만들기".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.create_onenote_notebook(&scope, &name).await {
                Ok(created) => {
                    let _ = storage.upsert_onenote_notebooks(std::slice::from_ref(&created));
                    sync.request(SyncCommand::RefreshOneNote);
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((
                            format!("필기장을 만들었습니다: {}", created.display_name),
                            ToastKind::Success,
                        )),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("필기장 만들기 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_onenote_section(
        &mut self,
        ui: &mut UiState,
        notebook_key: String,
        display_name: String,
    ) {
        let name = display_name.trim().to_string();
        if name.is_empty() {
            ui.info("섹션 이름을 입력하세요");
            return;
        }
        let (scope, notebook_id) = crate::models::split_onenote_entity_key(&notebook_key);
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "섹션 만들기".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .create_onenote_section(&scope, &notebook_id, &name)
                .await
            {
                Ok(created) => {
                    let _ = storage.upsert_onenote_sections(std::slice::from_ref(&created));
                    sync.request(SyncCommand::RefreshOneNote);
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((
                            format!("섹션을 만들었습니다: {}", created.display_name),
                            ToastKind::Success,
                        )),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("섹션 만들기 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_onenote_page(
        &mut self,
        ui: &mut UiState,
        section_key: String,
        title: String,
        markdown_body: String,
        inline_images: Vec<InlineImageUpload>,
        level: Option<i32>,
    ) {
        let title = title.trim().to_string();
        if title.is_empty() {
            ui.info("페이지 제목을 입력하세요");
            return;
        }
        let (scope, section_id) = crate::models::split_onenote_entity_key(&section_key);
        let outgoing = OutgoingOnenotePage {
            title,
            markdown_body,
            inline_images,
            level: level.filter(|l| *l > 0),
        };
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "페이지 만들기".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .create_onenote_page(&scope, &section_id, &outgoing)
                .await
            {
                Ok(created) => {
                    let _ = storage.upsert_onenote_pages(std::slice::from_ref(&created));
                    sync.request(SyncCommand::SyncOneNoteSection { section_key });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("페이지를 만들었습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("페이지 만들기 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn append_onenote_markdown(
        &mut self,
        ui: &mut UiState,
        page_key: String,
        markdown: String,
        inline_images: Vec<InlineImageUpload>,
    ) {
        if markdown.trim().is_empty() && inline_images.is_empty() {
            ui.info("추가할 Markdown 내용을 입력하세요");
            return;
        }
        let primary_div_id = self
            .active_page_detail
            .as_ref()
            .filter(|d| d.summary.page_key == page_key)
            .and_then(|d| d.primary_div_id.clone());
        let (scope, page_id) = crate::models::split_onenote_entity_key(&page_key);
        let graph = self.graph.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "페이지에 내용 추가".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .append_onenote_page_markdown(
                    &scope,
                    &page_id,
                    primary_div_id.as_deref(),
                    &markdown,
                    &inline_images,
                )
                .await
            {
                Ok(()) => {
                    sync.request(SyncCommand::SyncOneNotePage {
                        page_key: page_key.clone(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("페이지에 내용을 추가했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("내용 추가 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn edit_onenote_block(
        &mut self,
        ui: &mut UiState,
        page_key: String,
        element_id: String,
        markdown: String,
    ) {
        if element_id.trim().is_empty() {
            ui.info("수정할 블록 ID가 없습니다");
            return;
        }
        if markdown.trim().is_empty() {
            ui.info("수정할 블록 Markdown 내용을 입력하세요");
            return;
        }
        let command = parser::onenote::build_replace_block_patch(&element_id, &markdown);
        let (scope, page_id) = crate::models::split_onenote_entity_key(&page_key);
        let graph = self.graph.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "블록 수정".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .patch_onenote_page_commands(&scope, &page_id, &[command])
                .await
            {
                Ok(()) => {
                    sync.request(SyncCommand::SyncOneNotePage {
                        page_key: page_key.clone(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("블록을 수정했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("블록 수정 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn search_notes(&mut self, query: String) {
        let trimmed = query.trim().to_string();
        if trimmed.is_empty() {
            self.onenote_search_results.clear();
            return;
        }
        self.onenote_search_results = self
            .storage
            .search_onenote_pages(&trimmed, None, 50)
            .unwrap_or_default();
    }

    pub fn jump_to_note_search_hit(&mut self, ui: &mut UiState, index: usize) {
        let Some(hit) = self.onenote_search_results.get(index).cloned() else {
            return;
        };
        let section_key = hit.scope.make_entity_key(&hit.section_id);
        let notebook_key = self
            .onenote_sections
            .iter()
            .find(|sec| sec.section_key == section_key)
            .and_then(|sec| sec.notebook_id.clone())
            .and_then(|nb_id| {
                self.onenote_notebooks
                    .iter()
                    .find(|nb| nb.scope.scope_key() == hit.scope.scope_key() && nb.id == nb_id)
                    .map(|nb| nb.notebook_key.clone())
            });
        ui.persona = Persona::OneNote;
        ui.active_nav_tab = nav_pane::TAB_NOTES;
        self.ensure_tab_domain_activated(ui, nav_pane::TAB_NOTES);
        if let Some(nb_key) = notebook_key {
            ui.collapsed_notebook_keys.remove(&nb_key);
            self.active_notebook_key = Some(nb_key);
        }
        self.active_section_key = Some(section_key);
        self.open_onenote_page(ui, hit.page_key);
    }

    pub fn open_onenote_notebook(&mut self, ui: &mut UiState, notebook_key: String) {
        ui.collapsed_notebook_keys.remove(&notebook_key);
        if self.active_notebook_key.as_deref() == Some(notebook_key.as_str()) {
            return;
        }
        self.active_notebook_key = Some(notebook_key);
        self.active_section_key = None;
        self.active_page_key = None;
        self.active_page_detail = None;
        self.onenote_pages.clear();
        self.loading_onenote_pages = false;
        self.loading_onenote_detail = false;
        ui.onenote_selected_block = 0;
        ui.onenote_scroll = ScrollState::default();
    }

    pub fn open_onenote_section(&mut self, ui: &mut UiState, section_key: String) {
        if let Some(nb_key) = self
            .onenote_sections
            .iter()
            .find(|sec| sec.section_key == section_key)
            .and_then(|sec| {
                let nb_id = sec.notebook_id.as_deref()?;
                let scope_key = sec.scope.scope_key();
                self.onenote_notebooks
                    .iter()
                    .find(|nb| nb.scope.scope_key() == scope_key && nb.id == nb_id)
                    .map(|nb| nb.notebook_key.clone())
            })
        {
            self.active_notebook_key = Some(nb_key);
        }
        self.active_section_key = Some(section_key.clone());
        self.active_page_key = None;
        self.active_page_detail = None;
        self.onenote_pages = self
            .storage
            .list_onenote_pages(&section_key)
            .unwrap_or_default();
        self.loading_onenote_pages = true;
        self.loading_onenote_detail = false;
        ui.onenote_selected_block = 0;
        ui.onenote_scroll = ScrollState::default();
        self.sync_ctrl
            .request(SyncCommand::SyncOneNoteSection { section_key });
        if let Some(first_page_key) = self.onenote_pages.first().map(|p| p.page_key.clone()) {
            self.open_onenote_page(ui, first_page_key);
        }
    }

    pub fn cycle_onenote_section(&mut self, ui: &mut UiState, delta: isize) {
        let sections = self.sections_for_active_notebook();
        if sections.is_empty() {
            return;
        }
        let cur_idx = self
            .active_section_key
            .as_deref()
            .and_then(|key| sections.iter().position(|s| s.section_key == key))
            .unwrap_or(0);
        let len = sections.len() as isize;
        let next_idx = ((cur_idx as isize + delta) % len + len) as usize % sections.len();
        self.open_onenote_section(ui, sections[next_idx].section_key.clone());
    }

    pub fn cycle_onenote_page(&mut self, ui: &mut UiState, delta: isize) {
        if self.onenote_pages.is_empty() {
            return;
        }
        let cur_idx = self
            .active_page_key
            .as_deref()
            .and_then(|key| self.onenote_pages.iter().position(|p| p.page_key == key))
            .unwrap_or(0);
        let len = self.onenote_pages.len() as isize;
        let next_idx =
            ((cur_idx as isize + delta) % len + len) as usize % self.onenote_pages.len();
        self.open_onenote_page(ui, self.onenote_pages[next_idx].page_key.clone());
    }

    pub fn replace_onenote_page_body(
        &mut self,
        ui: &mut UiState,
        page_key: String,
        markdown: String,
    ) {
        let detail = self
            .active_page_detail
            .as_ref()
            .filter(|d| d.summary.page_key == page_key)
            .cloned()
            .or_else(|| self.storage.get_onenote_page_detail(&page_key).ok().flatten());
        let Some(detail) = detail else {
            ui.info("편집할 페이지 본문 정보를 찾을 수 없습니다");
            return;
        };
        let commands =
            parser::onenote::build_replace_page_body_commands(&detail, None, &markdown);
        if commands.is_empty() {
            return;
        }
        let (scope, page_id) = crate::models::split_onenote_entity_key(&page_key);
        let graph = self.graph.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "페이지 전체 수정".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph
                .patch_onenote_page_commands(&scope, &page_id, &commands)
                .await
            {
                Ok(()) => {
                    sync.request(SyncCommand::SyncOneNotePage {
                        page_key: page_key.clone(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("페이지 본문을 업데이트했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("페이지 수정 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn open_onenote_page(&mut self, ui: &mut UiState, page_key: String) {
        self.active_page_detail = self
            .storage
            .get_onenote_page_detail(&page_key)
            .unwrap_or(None);
        self.active_page_key = Some(page_key.clone());
        self.loading_onenote_detail = true;
        ui.onenote_selected_block = 0;
        ui.onenote_scroll = ScrollState {
            follow_tail: false,
            scroll_up: u16::MAX,
            ..ScrollState::default()
        };
        ui.onenote_follow_selection = false;
        self.sync_ctrl
            .request(SyncCommand::SyncOneNotePage { page_key });
    }

    pub fn select_onenote_block(&mut self, ui: &mut UiState, index: usize) {
        ui.onenote_selected_block = index;
        ui.onenote_follow_selection = true;
    }

    pub fn toggle_onenote_todo(&mut self, ui: &mut UiState, index: usize) {
        let Some(detail) = self.active_page_detail.clone() else {
            ui.info("페이지를 먼저 열어야 체크리스트를 바꿀 수 있습니다");
            return;
        };
        let Some(block) = detail.blocks.get(index).cloned() else {
            ui.info("선택한 블록이 없습니다");
            return;
        };
        let Some((command, completed)) = parser::onenote::build_toggle_todo_patch(&block) else {
            ui.info("이 블록은 체크리스트 항목이 아닙니다");
            return;
        };
        let page_key = detail.summary.page_key.clone();
        let (scope, page_id) = crate::models::split_onenote_entity_key(&page_key);
        let graph = self.graph.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "체크리스트 변경".to_string();
        ui.tasks.begin(task_label.clone());
        let toast_text = if completed {
            "완료로 표시했습니다".to_string()
        } else {
            "미완료로 되돌렸습니다".to_string()
        };

        tokio::spawn(async move {
            match graph
                .patch_onenote_page_commands(&scope, &page_id, &[command])
                .await
            {
                Ok(()) => {
                    sync.request(SyncCommand::SyncOneNotePage {
                        page_key: page_key.clone(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((toast_text, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((
                            format!("체크리스트 변경 실패: {err}"),
                            ToastKind::Error,
                        )),
                    });
                }
            }
        });
    }

    pub fn toggle_onenote_notebook(&mut self, ui: &mut UiState, notebook_key: String) {
        if !ui.collapsed_notebook_keys.remove(&notebook_key) {
            ui.collapsed_notebook_keys.insert(notebook_key);
        }
    }

    pub fn open_onenote_create_notebook(&mut self, ui: &mut UiState) {
        ui.push_overlay(Overlay::OnenoteForm(OnenoteFormOverlay::create_notebook()));
    }

    pub fn open_onenote_create_section(&mut self, ui: &mut UiState) {
        let Some(notebook_key) = self.active_notebook_key.clone() else {
            ui.info("섹션을 만들 필기장을 먼저 선택하세요");
            return;
        };
        let notebook_name = self
            .onenote_notebooks
            .iter()
            .find(|nb| nb.notebook_key == notebook_key)
            .map(|nb| nb.display_name.clone())
            .unwrap_or_else(|| notebook_key.clone());
        ui.push_overlay(Overlay::OnenoteForm(OnenoteFormOverlay::create_section(
            notebook_key,
            notebook_name,
        )));
    }

    pub fn open_onenote_create_page(&mut self, ui: &mut UiState, subpage: bool) {
        let Some(section_key) = self.active_section_key.clone() else {
            ui.info("페이지를 만들 섹션을 먼저 선택하세요");
            return;
        };
        let section_name = self
            .onenote_sections
            .iter()
            .find(|sec| sec.section_key == section_key)
            .map(|sec| sec.display_name.clone())
            .unwrap_or_else(|| section_key.clone());
        let level = if subpage {
            self.active_page_detail
                .as_ref()
                .map(|d| (d.summary.level + 1).clamp(1, 2))
                .unwrap_or(1)
        } else {
            0
        };
        ui.push_overlay(Overlay::OnenoteForm(OnenoteFormOverlay::create_page(
            section_key,
            section_name,
            level,
        )));
    }

    pub fn delete_active_onenote_page(&mut self, ui: &mut UiState) {
        let Some(page_key) = self.active_page_key.clone() else {
            ui.info("삭제할 페이지를 먼저 선택하세요");
            return;
        };
        let (scope, page_id) = crate::models::split_onenote_entity_key(&page_key);
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let sync = self.sync_ctrl.clone();
        let tx = self.ui_tx.clone();
        let task_label = "페이지 삭제".to_string();
        ui.tasks.begin(task_label.clone());

        self.onenote_pages.retain(|p| p.page_key != page_key);
        self.active_page_key = None;
        self.active_page_detail = None;
        ui.onenote_selected_block = 0;
        ui.onenote_scroll = ScrollState::default();

        tokio::spawn(async move {
            match graph.delete_onenote_page(&scope, &page_id).await {
                Ok(()) => {
                    let _ = storage.delete_onenote_page(&page_key);
                    sync.request(SyncCommand::RefreshOneNote);
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("페이지를 삭제했습니다".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("페이지 삭제 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn refresh_onenote(&mut self, ui: &mut UiState) {
        self.loading_onenote_pages = true;
        self.sync_ctrl.request(SyncCommand::RefreshOneNote);
        ui.info("OneNote를 새로고침합니다");
    }

    pub fn open_active_onenote_in_browser(&mut self, ui: &mut UiState) {
        let url = self
            .active_page_detail
            .as_ref()
            .and_then(|d| d.summary.web_url.clone())
            .or_else(|| {
                let key = self.active_page_key.as_deref()?;
                self.onenote_pages
                    .iter()
                    .find(|p| p.page_key == key)
                    .and_then(|p| p.web_url.clone())
            });
        let Some(url) = url else {
            ui.info("이 페이지는 브라우저 링크가 없습니다");
            return;
        };
        match open::that(&url) {
            Ok(()) => ui.success("브라우저에서 페이지를 열었습니다"),
            Err(err) => ui.error(format!("링크 열기 실패: {err}")),
        }
    }

    pub fn search_mail(&mut self, query: String) {
        self.search_query = query.clone();
        self.mail_search_results.clear();
        if query.trim().is_empty() {
            self.searching = false;
            return;
        }
        self.searching = true;
        let storage = self.storage.clone();
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let db_storage = storage.clone();
            let db_query = query.clone();
            let mut hits = tokio::task::spawn_blocking(move || {
                db_storage
                    .search_mail_messages(&db_query, None, 40)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            if hits.is_empty() {
                if let Ok(remote) = graph.search_mail_remote(&query, 25).await {
                    let remote_to_store = remote.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_mail_messages(&remote_to_store);
                    })
                    .await;
                    hits = remote
                        .into_iter()
                        .map(|m| MailSearchHit {
                            from_display: m.from_display(),
                            id: m.id,
                            folder_id: m.folder_id,
                            subject: m.subject,
                            received_at: m.received_at,
                            snippet: m.preview,
                            has_attachments: m.has_attachments,
                            is_read: m.is_read,
                        })
                        .collect();
                }
            }
            let _ = tx.send(UiMsg::MailSearchResults { query, hits });
        });
    }

    pub fn search_contacts(&mut self, query: String) {
        self.search_query = query.clone();
        self.contact_search_results.clear();
        if query.trim().is_empty() {
            self.searching = false;
            return;
        }
        self.searching = true;
        let storage = self.storage.clone();
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let db_storage = storage.clone();
            let db_query = query.clone();
            let mut list = tokio::task::spawn_blocking(move || {
                db_storage
                    .list_contacts(Some(&db_query), 50)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            if list.is_empty() {
                if let Ok(remote) = graph.search_contacts_and_directory(&query, 30).await {
                    let remote_to_store = remote.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_contacts(&remote_to_store);
                    })
                    .await;
                    list = remote;
                }
            }
            let _ = tx.send(UiMsg::ContactSearchResults {
                query,
                contacts: list,
            });
        });
    }

    pub fn check_for_updates(&mut self, ui: &mut UiState, check_only: bool, force: bool) {
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        let task_label = if check_only {
            "업데이트 확인".to_string()
        } else {
            "자체 업데이트 설치".to_string()
        };
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match crate::updater::check_or_apply_update(&repo, check_only, force).await {
                Ok(res) => {
                    let toast_msg = if res.updated {
                        format!(
                            "업데이트 완료: {} -> {} (재시작 권장)",
                            res.current_version, res.latest_version
                        )
                    } else if res.update_available {
                        format!(
                            "새 버전 사용 가능: {} (현재 {})",
                            res.latest_version, res.current_version
                        )
                    } else {
                        format!("최신 버전입니다 ({})", res.current_version)
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
                        toast: Some((format!("업데이트 확인 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn set_presence(&mut self, ui: &mut UiState, availability: String, activity: String) {
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let task_label = "상태 변경".to_string();
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match graph.set_my_presence(&availability, &activity, None).await {
                Ok(()) => {
                    let status = graph.get_my_presence().await.unwrap_or(PresenceStatus {
                        user_id: String::new(),
                        availability: availability.clone(),
                        activity,
                    });
                    let _ = tx.send(UiMsg::PresenceUpdated(status));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((format!("상태 변경: {availability}"), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("상태 변경 실패: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, event: CoreEvent) {
        match event {
            CoreEvent::SyncStateChanged { busy, summary } => {
                self.syncing = busy;
                self.sync_summary = summary;
                if !busy && self.persona == Persona::OneNote {
                    ui.tasks.finish("초기 동기화", None);
                }
            }
            CoreEvent::SyncError(err) => {
                ui.error(format!("동기화 오류: {err}"));
            }
            CoreEvent::ConversationsUpdated(convs) => {
                self.conversations = convs;
            }
            CoreEvent::TeamsUpdated(teams) => {
                self.teams = teams;
            }
            CoreEvent::MessagesUpdated {
                conv_key,
                parent_msg_id: _,
                messages: _,
                notify_candidate,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Ok(msgs) = self.storage.list_root_messages(&conv_key, 80, None) {
                        merge_optimistic_messages(&mut self.messages, msgs);
                    }
                    if let Some(root_id) =
                        self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
                    {
                        if let Ok(Some(mut thread)) =
                            self.storage.get_thread(&conv_key, &root_id, 100)
                        {
                            if let Some(existing) = self.active_thread.as_mut() {
                                merge_optimistic_messages(
                                    &mut existing.replies,
                                    std::mem::take(&mut thread.replies),
                                );
                                thread.replies = std::mem::take(&mut existing.replies);
                            }
                            self.active_thread = Some(thread);
                        }
                    }
                }
                if !self.my_user_id.is_empty() {
                    if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
                    {
                        self.activity_mentions = mentions;
                    }
                }
                if let Some(candidate) = notify_candidate {
                    self.maybe_notify(ui, &candidate);
                }
            }
            CoreEvent::PresenceUpdated(list) => {
                for p in list {
                    if p.user_id == self.my_user_id {
                        self.my_presence = Some(p.clone());
                    }
                    self.presences.insert(p.user_id.clone(), p);
                }
            }
            CoreEvent::MailFoldersUpdated(folders) => {
                self.mail_folders = folders;
            }
            CoreEvent::MailMessagesUpdated {
                folder_id,
                messages,
                notify_candidate,
            } => {
                if ui.active_mail_folder_id == folder_id {
                    self.mails = messages;
                    if let Some(active_id) = self.active_mail_id.clone() {
                        if let Some(updated) = self.mails.iter().find(|m| m.id == active_id).cloned()
                        {
                            let keep_existing_body = self
                                .active_mail
                                .as_ref()
                                .is_some_and(|m| m.id == active_id && !m.body_html.is_empty())
                                && updated.body_html.is_empty();
                            if !keep_existing_body {
                                self.active_mail = Some(updated);
                            }
                        }
                    }
                }
                if let Some(candidate) = notify_candidate {
                    self.maybe_notify(ui, &candidate);
                }
            }
            CoreEvent::CalendarEventsUpdated(events) => {
                self.calendar_events = events;
                if self.active_event_id.is_none() {
                    self.active_event_id = self.calendar_events.first().map(|e| e.id.clone());
                }
            }
            CoreEvent::ContactsUpdated(contacts) => {
                self.contacts = contacts;
                if self.active_contact_id.is_none() {
                    self.active_contact_id = self.contacts.first().map(|c| c.id.clone());
                }
            }
            CoreEvent::TodoUpdated {
                lists,
                list_id,
                tasks,
            } => {
                self.todo_lists = lists;
                if ui.active_todo_list_id.is_none() {
                    ui.active_todo_list_id =
                        list_id.clone().or_else(|| self.todo_lists.first().map(|l| l.id.clone()));
                }
                if ui.active_todo_list_id.as_deref() == list_id.as_deref() {
                    self.todo_tasks = tasks;
                }
            }
            CoreEvent::OnenoteHierarchyUpdated {
                notebooks,
                section_groups: _,
                sections,
            } => {
                self.onenote_notebooks = notebooks;
                self.onenote_sections = sections;
                if self.active_notebook_key.is_none() {
                    self.active_notebook_key = self
                        .onenote_notebooks
                        .first()
                        .map(|nb| nb.notebook_key.clone());
                }
                if self.active_section_key.is_none() {
                    let first_sec = self
                        .sections_for_active_notebook()
                        .first()
                        .map(|sec| sec.section_key.clone())
                        .or_else(|| self.onenote_sections.first().map(|sec| sec.section_key.clone()));
                    if let Some(sec_key) = first_sec {
                        self.open_onenote_section(ui, sec_key);
                        return;
                    }
                }
                self.loading_onenote_pages = false;
                self.loading_onenote_detail = false;
                if self.persona == Persona::OneNote {
                    self.syncing = false;
                    self.sync_summary = "최신 상태".to_string();
                    ui.tasks.finish("초기 동기화", None);
                }
            }
            CoreEvent::OnenotePagesUpdated { section_key, pages } => {
                if self.active_section_key.as_deref() == Some(section_key.as_str()) {
                    self.onenote_pages = pages;
                    self.loading_onenote_pages = false;
                    let active_missing = self
                        .active_page_key
                        .as_deref()
                        .is_none_or(|k| !self.onenote_pages.iter().any(|p| p.page_key == k));
                    if active_missing {
                        if let Some(first_key) =
                            self.onenote_pages.first().map(|p| p.page_key.clone())
                        {
                            self.open_onenote_page(ui, first_key);
                        }
                    }
                }
            }
            CoreEvent::OnenotePageDetailUpdated(detail) => {
                if self.active_page_key.as_deref() == Some(detail.summary.page_key.as_str())
                    || self.active_page_key.is_none()
                {
                    self.active_page_key = Some(detail.summary.page_key.clone());
                    self.active_page_detail = Some(*detail);
                    self.loading_onenote_detail = false;
                }
            }
        }
    }

    fn maybe_notify(&self, ui: &mut UiState, candidate: &NotificationCandidate) {
        let is_active = self.active_conv_key.as_deref() == Some(&candidate.conv_key);
        let dnd = self
            .my_presence
            .as_ref()
            .is_some_and(|p| matches!(p.availability.as_str(), "DoNotDisturb"));
        let is_self = !self.my_user_id.is_empty()
            && self
                .my_profile
                .as_ref()
                .is_some_and(|u| u.display_name == candidate.sender);
        if dnd || !notification::should_notify(ui.notify, is_active, is_self) {
            return;
        }
        let body = notification::chat_body(&candidate.sender, &candidate.preview);
        notification::show(&candidate.conv_title, &body);
    }

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BootstrapDone {
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences,
            } => {
                if let Some(u) = me {
                    self.my_user_id = u.id.clone();
                    self.my_profile = Some(u);
                }
                if let Some(p) = presence {
                    self.my_presence = Some(p);
                }
                self.conversations = conversations;
                self.teams = teams;
                self.activity_mentions = mentions;
                self.presences.extend(presences);
                self.syncing = false;
                self.sync_summary = "최신 상태".to_string();

                if self.active_conv_key.is_none() && ui.active_nav_tab < nav_pane::TAB_MAIL {
                    if let Some(first) = self.conversations.first().cloned() {
                        self.select_conversation(ui, first.target);
                    }
                }
            }
            UiMsg::MessagesLoaded {
                conv_key,
                messages,
                members,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    self.loading_messages = false;
                    merge_optimistic_messages(&mut self.messages, messages);
                    self.members = members;
                }
            }
            UiMsg::OlderMessagesLoaded {
                conv_key,
                mut messages,
                reached_end,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    self.oldest_reached = reached_end;
                    if !messages.is_empty() {
                        messages.extend(std::mem::take(&mut self.messages));
                        self.messages = messages;
                    }
                }
            }
            UiMsg::ThreadLoaded {
                conv_key,
                root_msg_id,
                thread,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key)
                    && self
                        .active_thread
                        .as_ref()
                        .is_some_and(|t| t.root.msg_id == root_msg_id)
                {
                    self.loading_thread = false;
                    self.active_thread = Some(thread);
                }
            }
            UiMsg::MessageSent {
                conv_key,
                parent_msg_id,
                optimistic_id,
                sent,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = parent_msg_id {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid {
                                if let Some(opt_id) = optimistic_id.as_deref() {
                                    if let Some(pos) =
                                        thread.replies.iter().position(|r| r.msg_id == opt_id)
                                    {
                                        if thread.replies.iter().any(|r| r.msg_id == sent.msg_id) {
                                            thread.replies.remove(pos);
                                        } else {
                                            thread.replies[pos] = sent;
                                        }
                                    } else if !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                                    {
                                        thread.replies.push(sent);
                                    }
                                } else if !thread.replies.iter().any(|r| r.msg_id == sent.msg_id) {
                                    thread.replies.push(sent);
                                }
                            }
                        }
                        if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                            root.reply_count = root.reply_count.saturating_add(1);
                        }
                    } else if let Some(opt_id) = optimistic_id.as_deref() {
                        if let Some(pos) = self.messages.iter().position(|m| m.msg_id == opt_id) {
                            if self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                                self.messages.remove(pos);
                            } else {
                                self.messages[pos] = sent;
                            }
                        } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                            self.messages.push(sent);
                            ui.chat_scroll.bottom();
                        }
                    } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                        self.messages.push(sent);
                        ui.chat_scroll.bottom();
                    }
                }
            }
            UiMsg::MessageSendFailed {
                conv_key,
                parent_msg_id,
                optimistic_id,
                text,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = parent_msg_id {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid {
                                thread.replies.retain(|r| r.msg_id != optimistic_id);
                            }
                        }
                    } else {
                        self.messages.retain(|m| m.msg_id != optimistic_id);
                    }
                    if ui.composer.text().trim().is_empty() {
                        ui.composer.load_text(&text);
                    }
                }
            }
            UiMsg::AttachmentSent { conv_key, sent } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = sent.parent_msg_id.clone() {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid
                                && !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                            {
                                thread.replies.push(sent);
                            }
                        }
                    } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                        self.messages.push(sent);
                        ui.chat_scroll.bottom();
                    }
                }
            }
            UiMsg::MessageDeleted {
                conv_key,
                msg_id,
                parent_msg_id,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if parent_msg_id.is_some() {
                        if let Some(thread) = self.active_thread.as_mut() {
                            for r in &mut thread.replies {
                                if r.msg_id == msg_id {
                                    r.deleted = true;
                                }
                            }
                        }
                    } else {
                        for m in &mut self.messages {
                            if m.msg_id == msg_id {
                                m.deleted = true;
                            }
                        }
                    }
                }
            }
            UiMsg::ReactionUpdated {
                conv_key,
                msg_id,
                reaction_type,
                added,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    let apply = |msg: &mut MessageItem| {
                        if let Some(r) = msg
                            .reactions
                            .iter_mut()
                            .find(|r| r.reaction_type == reaction_type)
                        {
                            r.me_reacted = added;
                            if added {
                                r.count = r.count.saturating_add(1);
                            } else {
                                r.count = r.count.saturating_sub(1);
                            }
                            msg.reactions.retain(|r| r.count > 0);
                        } else if added {
                            msg.reactions.push(ReactionSummary {
                                reaction_type: reaction_type.clone(),
                                count: 1,
                                me_reacted: true,
                                users: Vec::new(),
                            });
                        }
                    };
                    if let Some(m) = self.messages.iter_mut().find(|m| m.msg_id == msg_id) {
                        apply(m);
                    }
                    if let Some(thread) = self.active_thread.as_mut() {
                        if thread.root.msg_id == msg_id {
                            apply(&mut thread.root);
                        }
                        if let Some(r) = thread.replies.iter_mut().find(|r| r.msg_id == msg_id) {
                            apply(r);
                        }
                    }
                }
            }
            UiMsg::AttachmentDownloaded(_) => {}
            UiMsg::SearchResults { query, hits } => {
                if self.search_query == query {
                    self.searching = false;
                    self.search_results = hits;
                }
            }
            UiMsg::UserSearchResults { query: _, users } => {
                self.user_searching = false;
                self.user_search_results = users;
            }
            UiMsg::ChatCreated(conv) => {
                if !self.conversations.iter().any(|c| c.conv_key == conv.conv_key) {
                    self.conversations.insert(0, conv.clone());
                }
                self.select_conversation(ui, conv.target);
            }
            UiMsg::PresenceUpdated(status) => {
                let uid = if status.user_id.is_empty() {
                    self.my_user_id.clone()
                } else {
                    status.user_id.clone()
                };
                self.my_presence = Some(PresenceStatus {
                    user_id: uid.clone(),
                    availability: status.availability.clone(),
                    activity: status.activity.clone(),
                });
                if !uid.is_empty() {
                    self.presences.insert(uid, status);
                }
            }
            UiMsg::OutlookCachedSecondaryLoaded {
                calendar_events,
                contacts,
                todo_lists,
                todo_tasks,
                active_todo_list_id,
            } => {
                if self.calendar_events.is_empty() {
                    self.calendar_events = calendar_events;
                }
                if self.active_event_id.is_none() {
                    self.active_event_id = self.calendar_events.first().map(|e| e.id.clone());
                }
                if self.contacts.is_empty() {
                    self.contacts = contacts;
                }
                if self.active_contact_id.is_none() {
                    self.active_contact_id = self.contacts.first().map(|c| c.id.clone());
                }
                if self.todo_lists.is_empty() {
                    self.todo_lists = todo_lists;
                }
                if ui.active_todo_list_id.is_none() {
                    ui.active_todo_list_id = active_todo_list_id;
                }
                if self.todo_tasks.is_empty() {
                    self.todo_tasks = todo_tasks;
                }
                if self.active_todo_task_id.is_none() {
                    self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
                }
            }
            UiMsg::OutlookBootstrapDone {
                mail_folders,
                mails,
                calendar_events,
                contacts,
                todo_lists,
                todo_tasks,
                active_todo_list_id,
            } => {
                self.mail_folders = mail_folders;
                if ui.active_mail_folder_id == "inbox" {
                    self.mails = mails;
                    if self.active_mail_id.is_none() {
                        if let Some(first_id) = self.mails.first().map(|m| m.id.clone()) {
                            self.select_mail(ui, first_id);
                        }
                    }
                }
                self.calendar_events = calendar_events;
                if self.active_event_id.is_none() {
                    self.active_event_id = self.calendar_events.first().map(|e| e.id.clone());
                }
                self.contacts = contacts;
                if self.active_contact_id.is_none() {
                    self.active_contact_id = self.contacts.first().map(|c| c.id.clone());
                }
                self.todo_lists = todo_lists;
                if ui.active_todo_list_id.is_none() {
                    ui.active_todo_list_id = active_todo_list_id;
                }
                if self.todo_tasks.is_empty() {
                    self.todo_tasks = todo_tasks;
                }
                if self.active_todo_task_id.is_none() {
                    self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
                }
                if self.persona == Persona::Outlook {
                    self.syncing = false;
                    self.sync_summary = "최신 상태".to_string();
                }
            }
            UiMsg::MailFolderLoaded { folder_id, mails } => {
                if ui.active_mail_folder_id == folder_id {
                    self.mails = mails;
                    if self.active_mail_id.is_none() {
                        if let Some(first_id) = self.mails.first().map(|m| m.id.clone()) {
                            self.select_mail(ui, first_id);
                        }
                    }
                }
            }
            UiMsg::MailDetailLoaded { mail } => {
                self.mail_prefetch_pending.remove(&mail.id);
                if let Some(existing) = self.mails.iter_mut().find(|m| m.id == mail.id) {
                    *existing = mail.clone();
                }
                if self.active_mail_id.as_deref() == Some(&mail.id) {
                    self.loading_mail = false;
                    self.active_mail = Some(mail);
                }
            }
            UiMsg::MailDetailFailed { mail_id } => {
                self.mail_prefetch_pending.remove(&mail_id);
                if self.active_mail_id.as_deref() == Some(&mail_id) {
                    self.loading_mail = false;
                }
            }
            UiMsg::MailPrefetchFailed { mail_id } => {
                self.mail_prefetch_pending.remove(&mail_id);
            }
            UiMsg::MailReadRollback {
                mail_id,
                prev_read,
                folder_prev_unread,
            } => {
                self.set_mail_read_in_memory(&mail_id, prev_read);
                if let Some((fid, prev_count)) = folder_prev_unread {
                    if let Some(folder) = self.mail_folders.iter_mut().find(|f| f.id == fid) {
                        folder.unread_item_count = prev_count;
                    }
                }
            }
            UiMsg::MailFlagRollback {
                mail_id,
                prev_flagged,
            } => {
                self.set_mail_flagged_in_memory(&mail_id, prev_flagged);
            }
            UiMsg::MailRemoveRollback {
                folder_id,
                index,
                mail,
            } => {
                if ui.active_mail_folder_id == folder_id {
                    if let Some(pos) = index {
                        if !self.mails.iter().any(|m| m.id == mail.id) {
                            let insert_at = pos.min(self.mails.len());
                            self.mails.insert(insert_at, mail.clone());
                        }
                    }
                    if self.active_mail_id.is_none() {
                        self.active_mail_id = Some(mail.id.clone());
                        self.active_mail = Some(mail);
                    }
                }
            }
            UiMsg::TodoStatusRollback {
                task_id,
                prev_status,
            } => {
                if let Some(t) = self.todo_tasks.iter_mut().find(|t| t.id == task_id) {
                    t.status = prev_status;
                }
            }
            UiMsg::MailUpdated { mail } => {
                if let Some(existing) = self.mails.iter_mut().find(|m| m.id == mail.id) {
                    *existing = mail.clone();
                }
                if self.active_mail_id.as_deref() == Some(&mail.id) {
                    self.active_mail = Some(mail);
                }
            }
            UiMsg::MailRemoved { mail_id } => {
                self.mails.retain(|m| m.id != mail_id);
                if self.active_mail_id.as_deref() == Some(&mail_id) {
                    self.active_mail_id = self.mails.first().map(|m| m.id.clone());
                    self.active_mail = self.mails.first().cloned();
                }
            }
            UiMsg::CalendarLoaded { events } => {
                self.calendar_events = events;
                if self.active_event_id.is_none() {
                    self.active_event_id = self.calendar_events.first().map(|e| e.id.clone());
                }
            }
            UiMsg::CalendarEventCreated { event } => {
                if !self.calendar_events.iter().any(|e| e.id == event.id) {
                    self.calendar_events.push(event.clone());
                    self.calendar_events
                        .sort_by(|a, b| a.start_iso.cmp(&b.start_iso));
                }
                self.active_event_id = Some(event.id);
            }
            UiMsg::CalendarEventDeleted { event_id } => {
                self.calendar_events.retain(|e| e.id != event_id);
                if self.active_event_id.as_deref() == Some(&event_id) {
                    self.active_event_id = self.calendar_events.first().map(|e| e.id.clone());
                }
            }
            UiMsg::CalendarEventResponded { event_id, response } => {
                if let Some(ev) = self.calendar_events.iter_mut().find(|e| e.id == event_id) {
                    ev.response_status = match response.as_str() {
                        "accept" => "accepted",
                        "tentativelyAccept" => "tentativelyAccepted",
                        "decline" => "declined",
                        other => other,
                    }
                    .to_string();
                }
            }
            UiMsg::ContactsLoaded { contacts } => {
                self.contacts = contacts;
                if self.active_contact_id.is_none() {
                    self.active_contact_id = self.contacts.first().map(|c| c.id.clone());
                }
            }
            UiMsg::ContactCreated { contact } => {
                if !self.contacts.iter().any(|c| c.id == contact.id) {
                    self.contacts.insert(0, contact.clone());
                }
                self.active_contact_id = Some(contact.id);
            }
            UiMsg::ContactDeleted { contact_id } => {
                self.contacts.retain(|c| c.id != contact_id);
                if self.active_contact_id.as_deref() == Some(&contact_id) {
                    self.active_contact_id = self.contacts.first().map(|c| c.id.clone());
                }
            }
            UiMsg::TodoListsLoaded { lists } => {
                self.todo_lists = lists;
                if ui.active_todo_list_id.is_none() {
                    ui.active_todo_list_id = self.todo_lists.first().map(|l| l.id.clone());
                }
            }
            UiMsg::TodoTasksLoaded { list_id, tasks } => {
                if ui.active_todo_list_id.as_deref() == Some(&list_id) {
                    self.todo_tasks = tasks;
                    if self.active_todo_task_id.is_none() {
                        self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
                    }
                }
            }
            UiMsg::TodoTaskUpserted { task } => {
                if ui.active_todo_list_id.as_deref() == Some(&task.list_id) {
                    if let Some(existing) = self.todo_tasks.iter_mut().find(|t| t.id == task.id) {
                        *existing = task.clone();
                    } else {
                        self.todo_tasks.insert(0, task.clone());
                    }
                    if !ui.todo_include_completed {
                        self.todo_tasks.retain(|t| !t.is_completed());
                    }
                    self.active_todo_task_id = Some(task.id);
                }
            }
            UiMsg::TodoTaskDeleted { task_id } => {
                self.todo_tasks.retain(|t| t.id != task_id);
                if self.active_todo_task_id.as_deref() == Some(&task_id) {
                    self.active_todo_task_id = self.todo_tasks.first().map(|t| t.id.clone());
                }
            }
            UiMsg::MailSearchResults { query, hits } => {
                if self.search_query == query {
                    self.searching = false;
                    self.mail_search_results = hits;
                }
            }
            UiMsg::ContactSearchResults { query, contacts } => {
                if self.search_query == query {
                    self.searching = false;
                    self.contact_search_results = contacts;
                }
            }
            UiMsg::ImageDecoded {
                cache_key,
                rendered,
            } => {
                self.insert_image_cache(cache_key, rendered);
            }
            UiMsg::ImageDecodeFailed {
                cache_key,
                owner_id,
                att_id,
                error,
            } => {
                self.image_pending.remove(&cache_key);
                self.image_errors.insert(cache_key, error.clone());
                if let Some(Overlay::ImagePreview(preview)) = ui.overlays.last_mut() {
                    if let Some(cur) = preview.current() {
                        if cur.owner_id() == owner_id && cur.att_id() == att_id {
                            preview.error = Some(error);
                        }
                    }
                }
            }
            UiMsg::OnenoteImageDecoded {
                cache_key,
                rendered,
            } => {
                self.insert_onenote_image_cache(cache_key, rendered);
            }
            UiMsg::OnenoteImageDecodeFailed { cache_key, .. } => {
                self.onenote_image_pending.remove(&cache_key);
                self.onenote_image_errors.insert(cache_key);
            }
            UiMsg::UpdateChecked(_res) => {}
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.daemon_attached = true;
                self.daemon_pid = pid;
                self.syncing = sync_busy;
                self.sync_summary = "데몬 연결됨".to_string();
                if ui.active_todo_list_id.is_none() {
                    ui.active_todo_list_id = self.todo_lists.first().map(|l| l.id.clone());
                }
                match self.persona {
                    Persona::Teams => self.activated_teams = true,
                    Persona::Outlook => self.activated_outlook = true,
                    Persona::OneNote => self.activated_onenote = true,
                }
                self.send_active_domains();
                self.sync_ctrl.set_focus(
                    self.active_target.clone(),
                    self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
                );
                if self.my_profile.is_none() {
                    self.spawn_me_profile_if_needed();
                }
            }
            UiMsg::DaemonDetached => {
                self.daemon_attached = false;
                self.daemon_pid = None;
                if !self.syncing {
                    self.sync_summary = "로컬 동기화".to_string();
                }
                self.send_active_domains();
                self.sync_ctrl.set_focus(
                    self.active_target.clone(),
                    self.active_thread.as_ref().map(|t| t.root.msg_id.clone()),
                );
            }
            UiMsg::StartLocalBootstrap => {
                self.daemon_attached = false;
                self.daemon_pid = None;
                self.spawn_bootstrap(ui);
            }
            UiMsg::ProfileLoaded { me, presence } => {
                if let Some(u) = me {
                    self.my_user_id = u.id.clone();
                    self.my_profile = Some(u);
                }
                if let Some(p) = presence {
                    self.my_presence = Some(p);
                }
                if !self.my_user_id.is_empty() && self.activity_mentions.is_empty() {
                    if let Ok(mentions) = self.storage.list_mentions_for_user(&self.my_user_id, 50)
                    {
                        self.activity_mentions = mentions;
                    }
                }
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

    pub fn unread_chat_count(&self) -> i32 {
        self.conversations
            .iter()
            .filter(|c| c.kind != ConversationKind::Channel)
            .map(|c| c.unread_count.max(0))
            .sum()
    }

    pub fn unread_channel_count(&self) -> i32 {
        self.conversations
            .iter()
            .filter(|c| c.kind == ConversationKind::Channel)
            .map(|c| c.unread_count.max(0))
            .sum()
    }

    pub fn conversation_kind_label(kind: ConversationKind) -> &'static str {
        match kind {
            ConversationKind::OneOnOne => "1:1",
            ConversationKind::Group => "그룹",
            ConversationKind::Meeting => "모임",
            ConversationKind::Channel => "채널",
        }
    }
}

fn merge_optimistic_messages(target: &mut Vec<MessageItem>, mut incoming: Vec<MessageItem>) {
    let pending: Vec<MessageItem> = target
        .iter()
        .filter(|m| m.msg_id.starts_with("optimistic-"))
        .cloned()
        .collect();
    incoming.extend(pending);
    *target = incoming;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_outlook_defaults_to_mail_tab() {
        let cfg = AppConfig::default();
        let teams_ui = UiState::from_config_with_persona(&cfg, Persona::Teams);
        let outlook_ui = UiState::from_config_with_persona(&cfg, Persona::Outlook);
        let onenote_ui = UiState::from_config_with_persona(&cfg, Persona::OneNote);
        assert_eq!(teams_ui.persona, Persona::Teams);
        assert_eq!(teams_ui.active_nav_tab, 0);
        assert_eq!(outlook_ui.persona, Persona::Outlook);
        assert_eq!(outlook_ui.active_nav_tab, 3);
        assert_eq!(onenote_ui.persona, Persona::OneNote);
        assert_eq!(onenote_ui.active_nav_tab, 7);
    }

    #[test]
    fn image_cache_key_formats_stably() {
        assert_eq!(
            AppState::image_cache_key("msg-1", "att-9", 40, 14),
            "msg-1:att-9:40x14"
        );
    }

    #[tokio::test]
    async fn stash_draft_noop_does_not_mark_dirty_and_ignores_stale_loads() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl.clone(), ui_tx);
        let mut ui = UiState::new();
        app.active_conv_key = Some("chat:room-a".to_string());
        app.drafts.clear();

        // Empty composer -> no-op (must not mark dirty or trigger disk write)
        assert!(!app.stash_draft_in_memory(&ui));
        app.stash_draft(&ui);

        // Non-empty draft -> marks dirty; repeating unchanged text -> no-op
        ui.composer.load_text("hello world");
        assert!(app.stash_draft_in_memory(&ui));
        assert_eq!(app.drafts.get("chat:room-a").map(String::as_str), Some("hello world"));
        assert!(!app.stash_draft_in_memory(&ui));

        // Clearing composer -> marks dirty, removes draft; repeating -> no-op
        ui.composer.load_text("");
        assert!(app.stash_draft_in_memory(&ui));
        assert!(!app.drafts.contains_key("chat:room-a"));
        assert!(!app.stash_draft_in_memory(&ui));

        // Stale MessagesLoaded for another room must NOT overwrite active room's messages
        app.loading_messages = true;
        let stale_msg = MessageItem {
            msg_key: "chat:room-b|m1".to_string(),
            conv_key: "chat:room-b".to_string(),
            msg_id: "m1".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Other".to_string(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: String::new(),
            body_plain: "stale".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        };
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessagesLoaded {
                conv_key: "chat:room-b".to_string(),
                messages: vec![stale_msg.clone()],
                members: Vec::new(),
            },
        );
        assert!(app.messages.is_empty());
        assert!(app.loading_messages);

        // Matching MessagesLoaded updates state
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessagesLoaded {
                conv_key: "chat:room-a".to_string(),
                messages: vec![stale_msg],
                members: Vec::new(),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert!(!app.loading_messages);

        sync_ctrl.request(SyncCommand::Shutdown);
    }

    fn make_test_app_with_persona(persona: Persona) -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let sync_ctrl =
            sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app =
            AppState::new_with_persona(cfg.clone(), persona, auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config_with_persona(&cfg, persona);
        (app, ui)
    }

    fn sample_mail(id: &str, is_read: bool, flagged: bool) -> MailMessage {
        MailMessage {
            id: id.to_string(),
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: format!("Subject {id}"),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            received_at: "2026-11-18T00:00:00Z".to_string(),
            sent_at: None,
            is_read,
            is_draft: false,
            flagged,
            importance: "normal".to_string(),
            has_attachments: false,
            preview: format!("Preview {id}"),
            body_html: format!("<p>{id}</p>"),
            body_plain: id.to_string(),
            attachments: vec![],
            web_link: None,
        }
    }

    #[tokio::test]
    async fn persona_outlook_activates_only_outlook_initially_then_teams_on_tab_switch() {
        let (mut app, mut ui) = make_test_app_with_persona(Persona::Outlook);
        app.load_cached_for_persona();
        assert!(!app.activated_teams);
        assert!(app.activated_outlook);
        assert!(!app.activated_onenote);

        // Switching to Tab 0 (Teams) lazily activates Teams without activating OneNote.
        app.ensure_tab_domain_activated(&mut ui, 0);
        assert!(app.activated_teams);
        assert!(app.activated_outlook);
        assert!(!app.activated_onenote);

        // Switching to Tab 7 (OneNote) activates OneNote.
        app.ensure_tab_domain_activated(&mut ui, nav_pane::TAB_NOTES);
        assert!(app.activated_onenote);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn optimistic_mail_read_flag_delete_and_rollback() {
        let (mut app, mut ui) = make_test_app_with_persona(Persona::Outlook);
        app.mail_folders = vec![MailFolder {
            id: "inbox".to_string(),
            display_name: "Inbox".to_string(),
            well_known_name: Some("inbox".to_string()),
            parent_folder_id: None,
            unread_item_count: 2,
            total_item_count: 2,
        }];
        let m1 = sample_mail("m1", false, false);
        let m2 = sample_mail("m2", true, false);
        app.mails = vec![m1.clone(), m2.clone()];
        app.active_mail_id = Some("m1".to_string());
        app.active_mail = Some(m1);

        // 1. Optimistic Read toggle + rollback
        app.toggle_active_mail_read(&mut ui);
        assert!(app.mails[0].is_read);
        assert!(app.active_mail.as_ref().unwrap().is_read);
        assert_eq!(app.mail_folders[0].unread_item_count, 1);

        app.handle_ui_msg(
            &mut ui,
            UiMsg::MailReadRollback {
                mail_id: "m1".to_string(),
                prev_read: false,
                folder_prev_unread: Some(("inbox".to_string(), 2)),
            },
        );
        assert!(!app.mails[0].is_read);
        assert!(!app.active_mail.as_ref().unwrap().is_read);
        assert_eq!(app.mail_folders[0].unread_item_count, 2);

        // 2. Optimistic Flag toggle + rollback
        app.toggle_active_mail_flag(&mut ui);
        assert!(app.mails[0].flagged);
        assert!(app.active_mail.as_ref().unwrap().flagged);

        app.handle_ui_msg(
            &mut ui,
            UiMsg::MailFlagRollback {
                mail_id: "m1".to_string(),
                prev_flagged: false,
            },
        );
        assert!(!app.mails[0].flagged);
        assert!(!app.active_mail.as_ref().unwrap().flagged);

        // 3. Optimistic Delete advances active_mail_id to m2 + rollback re-inserts m1 at index 0
        let m1_snapshot = app.mails[0].clone();
        app.delete_active_mail(&mut ui);
        assert_eq!(app.mails.len(), 1);
        assert_eq!(app.mails[0].id, "m2");
        assert_eq!(app.active_mail_id.as_deref(), Some("m2"));

        app.handle_ui_msg(
            &mut ui,
            UiMsg::MailRemoveRollback {
                folder_id: "inbox".to_string(),
                index: Some(0),
                mail: m1_snapshot,
            },
        );
        assert_eq!(app.mails.len(), 2);
        assert_eq!(app.mails[0].id, "m1");
        assert_eq!(app.mails[1].id, "m2");

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn optimistic_todo_and_message_send_with_rollback() {
        let (mut app, mut ui) = make_test_app_with_persona(Persona::Teams);
        app.todo_tasks = vec![TodoTask {
            id: "t1".to_string(),
            list_id: "l1".to_string(),
            title: "Task 1".to_string(),
            status: "notStarted".to_string(),
            importance: "normal".to_string(),
            body_plain: None,
            due_date_iso: None,
            reminder_iso: None,
            completed_at: None,
            created_at: None,
            updated_at: None,
        }];
        app.active_todo_task_id = Some("t1".to_string());
        app.toggle_active_todo_completed(&mut ui);
        assert_eq!(app.todo_tasks[0].status, "completed");

        app.handle_ui_msg(
            &mut ui,
            UiMsg::TodoStatusRollback {
                task_id: "t1".to_string(),
                prev_status: "notStarted".to_string(),
            },
        );
        assert_eq!(app.todo_tasks[0].status, "notStarted");

        // Optimistic message send -> replacement -> failure restore
        app.active_conv_key = Some("chat:c1".to_string());
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "c1".to_string(),
        });
        ui.composer.load_text("hello optimistic");
        app.send_composer(&mut ui);
        assert_eq!(app.messages.len(), 1);
        let opt_id = app.messages[0].msg_id.clone();
        assert!(opt_id.starts_with("optimistic-"));
        assert!(ui.composer.text().is_empty());

        let mut real_msg = app.messages[0].clone();
        real_msg.msg_id = "server-1".to_string();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSent {
                conv_key: "chat:c1".to_string(),
                parent_msg_id: None,
                optimistic_id: Some(opt_id),
                sent: real_msg,
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].msg_id, "server-1");

        // Failure rollback restores composer text when empty
        ui.composer.load_text("retry me");
        app.send_composer(&mut ui);
        assert_eq!(app.messages.len(), 2);
        let failed_opt_id = app.messages[1].msg_id.clone();
        assert!(ui.composer.text().is_empty());
        app.handle_ui_msg(
            &mut ui,
            UiMsg::MessageSendFailed {
                conv_key: "chat:c1".to_string(),
                parent_msg_id: None,
                optimistic_id: failed_opt_id,
                text: "retry me".to_string(),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(ui.composer.text(), "retry me");

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn onenote_image_cache_is_bounded_to_24_fifo() {
        let (mut app, _ui) = make_test_app_with_persona(Persona::OneNote);
        for i in 0..30 {
            let key = format!("page:{i}:20x10");
            let img = RenderedTerminalImage {
                width_px: 10,
                height_px: 10,
                cols: 20,
                rows: 10,
                format_label: "PNG".to_string(),
                lines: vec![],
            };
            app.insert_onenote_image_cache(key, img);
        }
        assert_eq!(app.onenote_image_cache.len(), 24);
        assert_eq!(app.onenote_image_cache_order.len(), 24);
        assert!(!app.onenote_image_cache.contains_key("page:0:20x10"));
        assert!(!app.onenote_image_cache.contains_key("page:5:20x10"));
        assert!(app.onenote_image_cache.contains_key("page:6:20x10"));
        assert!(app.onenote_image_cache.contains_key("page:29:20x10"));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    fn make_sized_test_image(label_bytes: usize) -> RenderedTerminalImage {
        RenderedTerminalImage {
            width_px: 10,
            height_px: 10,
            cols: 20,
            rows: 10,
            format_label: "X".repeat(label_bytes),
            lines: vec![],
        }
    }

    #[tokio::test]
    async fn image_cache_byte_budget_evicts_across_both_caches_and_clear_resets_bytes() {
        let (mut app, _ui) = make_test_app_with_persona(Persona::Teams);
        let two_mib = 2 * 1024 * 1024 - 128;

        for i in 0..3 {
            app.insert_image_cache(format!("msg:{i}:20x10"), make_sized_test_image(two_mib));
        }
        assert_eq!(app.image_cache.len(), 3);
        assert!(app.image_cache_bytes <= img_preview::MAX_IMAGE_CACHE_BYTES);

        // Inserting OneNote images pushes combined cache over 8 MiB -> older entries are evicted
        // so combined byte usage stays <= MAX_IMAGE_CACHE_BYTES.
        for i in 0..2 {
            app.insert_onenote_image_cache(
                format!("note:{i}:20x10"),
                make_sized_test_image(two_mib),
            );
        }
        assert!(
            app.image_cache_bytes + app.onenote_image_cache_bytes
                <= img_preview::MAX_IMAGE_CACHE_BYTES
        );
        assert_eq!(app.onenote_image_cache.len(), 2);

        // Replacing an existing key updates byte counter accurately.
        let one_mib = 1024 * 1024 - 128;
        app.insert_onenote_image_cache(
            "note:1:20x10".to_string(),
            make_sized_test_image(one_mib),
        );
        let expected_onenote_bytes: usize = app
            .onenote_image_cache
            .values()
            .map(AppState::rendered_image_bytes)
            .sum();
        assert_eq!(app.onenote_image_cache_bytes, expected_onenote_bytes);

        let expected_teams_bytes: usize = app
            .image_cache
            .values()
            .map(AppState::rendered_image_bytes)
            .sum();
        assert_eq!(app.image_cache_bytes, expected_teams_bytes);

        // Single image larger than MAX_IMAGE_CACHE_BYTES still stays cached as the sole entry.
        let ten_mib = 10 * 1024 * 1024;
        app.insert_image_cache("oversized:0:20x10".to_string(), make_sized_test_image(ten_mib));
        assert_eq!(app.image_cache.len(), 1);
        assert!(app.image_cache.contains_key("oversized:0:20x10"));
        assert_eq!(app.onenote_image_cache.len(), 0);
        assert_eq!(app.onenote_image_cache_bytes, 0);

        // Clearing resets maps, orders, and byte counters to zero.
        app.clear_image_caches();
        assert!(app.image_cache.is_empty());
        assert!(app.image_cache_order.is_empty());
        assert_eq!(app.image_cache_bytes, 0);
        assert!(app.onenote_image_cache.is_empty());
        assert!(app.onenote_image_cache_order.is_empty());
        assert_eq!(app.onenote_image_cache_bytes, 0);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn leaving_onenote_tab_trims_onenote_image_cache_to_2mib() {
        let (mut app, mut ui) = make_test_app_with_persona(Persona::OneNote);
        app.load_cached_for_persona();
        let one_mib = 1024 * 1024 - 128;
        for i in 0..4 {
            app.insert_onenote_image_cache(
                format!("note:{i}:20x10"),
                make_sized_test_image(one_mib),
            );
        }
        assert!(app.onenote_image_cache_bytes > 2 * 1024 * 1024);

        // Leaving TAB_NOTES trims OneNote cache down to <= 2 MiB while preserving newest entries.
        app.ensure_tab_domain_activated(&mut ui, 0);
        assert!(app.onenote_image_cache_bytes <= 2 * 1024 * 1024);
        assert!(!app.onenote_image_cache.contains_key("note:0:20x10"));
        assert!(app.onenote_image_cache.contains_key("note:3:20x10"));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn daemon_attached_skips_duplicate_bootstrap_and_preserves_domain_and_focus_nudges() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx, false);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

        let mut app = AppState::new_with_persona(
            cfg.clone(),
            Persona::Teams,
            auth,
            graph,
            storage,
            sync_ctrl,
            ui_tx,
        );
        let mut ui = UiState::from_config_with_persona(&cfg, Persona::Teams);
        app.my_profile = Some(UserProfile {
            id: "user-1".to_string(),
            display_name: "Alice".to_string(),
            mail: Some("alice@example.com".to_string()),
            user_principal_name: Some("alice@example.com".to_string()),
        });
        app.my_user_id = "user-1".to_string();
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "chat-42".to_string(),
        });

        app.handle_ui_msg(
            &mut ui,
            UiMsg::DaemonAttached {
                pid: Some(4242),
                sync_busy: false,
            },
        );

        assert!(app.daemon_attached);
        assert_eq!(app.daemon_pid, Some(4242));
        assert!(!app.syncing);
        assert_eq!(app.sync_summary, "데몬 연결됨");
        assert!(app.activated_teams);
        assert!(!app.activated_outlook);
        assert!(!app.activated_onenote);
        assert!(ui.tasks.first_running().is_none());

        let cmd = nudge_rx.try_recv().expect("expected SetActiveDomains nudge");
        assert!(matches!(
            cmd,
            SyncCommand::SetActiveDomains {
                teams: true,
                outlook: false,
                onenote: false,
            }
        ));
        assert!(nudge_rx.try_recv().is_err(), "unexpected extra bootstrap command");
        assert_eq!(
            focus_rx.borrow().target,
            Some(ConversationTarget::Chat {
                chat_id: "chat-42".to_string(),
            })
        );

        app.handle_ui_msg(&mut ui, UiMsg::DaemonDetached);
        assert!(!app.daemon_attached);
        assert_eq!(app.daemon_pid, None);
        assert_eq!(app.sync_summary, "로컬 동기화");
        assert!(matches!(
            nudge_rx.try_recv().expect("expected re-activated domains on detach"),
            SyncCommand::SetActiveDomains {
                teams: true,
                outlook: false,
                onenote: false,
            }
        ));
    }
}
