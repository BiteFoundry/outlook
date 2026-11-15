//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::Utc;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{self, AppConfig};
use crate::graph::GraphClient;
use crate::models::{
    ChannelSummary, ConversationKind, ConversationSummary, ConversationTarget, CoreEvent,
    DownloadedFile, MemberInfo, MessageItem, NotificationCandidate, OutgoingMessage,
    PresenceStatus, QuotedReply, ReactionSummary, SearchHit, TeamSummary, ThreadView, UserProfile,
};
use crate::parser;
use crate::storage::Storage;
use crate::sync::{self, SyncCommand, SyncController};

use super::composer::{ComposerState, ReplyPreview};
use super::hitmap::{HitMap, MessageScope};
use super::keymap::Keymap;
use super::nav_pane::{self, NavRow};
use super::notification;
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
        sent: MessageItem,
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
    TaskDone {
        label: String,
        error: Option<String>,
        toast: Option<(String, ToastKind)>,
    },
}

pub struct UiState {
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
    pub attach_index: usize,
    pub show_nav_rail: bool,
    pub auto_thread_pane: bool,
    pub thread_drawer_open: bool,
    pub narrow_pane: NarrowPane,
    pub editor_requested: bool,
    pub notify: bool,
    pub composer_target_thread: bool,
    pub show_subject: bool,
    pub term_width: u16,
    pub term_height: u16,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self {
            focus: Focus::Nav,
            overlays: Vec::new(),
            composer: ComposerState::new(),
            hitmap: HitMap::default(),
            toasts: ToastQueue::new(),
            tasks: TaskBoard::default(),
            keymap: Keymap::default_teams(),
            theme: Theme::default(),
            active_nav_tab: 0,
            nav_selected: 0,
            collapsed_team_ids: HashSet::new(),
            chat_scroll: ScrollState::default(),
            thread_scroll: ScrollState::default(),
            attach_index: 0,
            show_nav_rail: true,
            auto_thread_pane: true,
            thread_drawer_open: false,
            narrow_pane: NarrowPane::List,
            editor_requested: false,
            notify: true,
            composer_target_thread: false,
            show_subject: false,
            term_width: 120,
            term_height: 40,
        }
    }

    pub fn from_config(config: &AppConfig) -> Self {
        let mut s = Self::new();
        s.show_nav_rail = config.show_nav_rail;
        s.auto_thread_pane = config.auto_thread_pane;
        s.notify = config.notify;
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
    pub searching: bool,
    pub search_query: String,
    pub search_results: Vec<SearchHit>,
    pub user_searching: bool,
    pub user_search_results: Vec<UserProfile>,
    pub oldest_reached: bool,
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
        let drafts = config::load_drafts();
        Self {
            config,
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
            searching: false,
            search_query: String::new(),
            search_results: Vec::new(),
            user_searching: false,
            user_search_results: Vec::new(),
            oldest_reached: false,
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

    pub fn stash_draft(&mut self, ui: &UiState) {
        if let Some(key) = self.current_draft_key(ui) {
            let text = ui.composer.text().trim_end_matches('\n').to_string();
            if text.is_empty() {
                self.drafts.remove(&key);
            } else {
                self.drafts.insert(key, text);
            }
            let _ = config::save_drafts(&self.drafts);
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
            self.drafts.remove(&key);
            let _ = config::save_drafts(&self.drafts);
        }
        ui.composer.reset_after_send();
    }

    pub fn load_cached_initial(&mut self) {
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

    pub fn spawn_bootstrap(&mut self, ui: &mut UiState) {
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        let task_label = "초기 동기화".to_string();
        ui.tasks.begin(task_label.clone());
        self.syncing = true;
        self.sync_summary = "동기화 중...".to_string();

        tokio::spawn(async move {
            let token_uid = auth.current_tokens().await.and_then(|t| t.user_id);
            let me = graph.get_me().await.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.mail.clone())
                    .await;
            }
            let presence = graph.get_my_presence().await.ok();
            let _ = sync::bootstrap_conversations(&graph, &storage).await;
            let conversations = storage.list_conversations().unwrap_or_default();
            let teams = storage.list_teams().unwrap_or_default();
            let my_id = me
                .as_ref()
                .map(|u| u.id.as_str())
                .or(token_uid.as_deref())
                .unwrap_or("");
            let mentions = if !my_id.is_empty() {
                storage.list_mentions_for_user(my_id, 50).unwrap_or_default()
            } else {
                Vec::new()
            };
            let peer_ids: Vec<String> = conversations
                .iter()
                .filter_map(|c| c.peer_user_id.clone())
                .collect();
            let presences = if !peer_ids.is_empty() {
                let fetched = graph.get_presences_by_user_ids(&peer_ids).await.unwrap_or_default();
                let _ = storage.upsert_presences(&fetched);
                storage.get_presences().unwrap_or_else(|_| {
                    fetched
                        .into_iter()
                        .map(|p| (p.user_id.clone(), p))
                        .collect()
                })
            } else {
                storage.get_presences().unwrap_or_default()
            };

            let _ = tx.send(UiMsg::BootstrapDone {
                me,
                presence,
                conversations,
                teams,
                mentions,
                presences,
            });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
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
        let _ = self.storage.mark_conversation_read(&new_key, &now_ts);

        self.sync_ctrl.set_focus(Some(target.clone()), None);
        self.sync_ctrl.request(SyncCommand::SyncActiveNow);

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let conv_key = new_key.clone();
        tokio::spawn(async move {
            let fetched_msgs = graph.list_messages(&target, 60).await.unwrap_or_default();
            if !fetched_msgs.is_empty() {
                let _ = storage.upsert_messages(&fetched_msgs);
            }
            if let Ok(members) = graph.list_members(&target).await {
                if !members.is_empty() {
                    let _ = storage.upsert_members(&conv_key, &members);
                }
            }
            if let ConversationTarget::Chat { ref chat_id } = target {
                let _ = graph.mark_chat_read(chat_id).await;
            }
            let messages = storage
                .list_root_messages(&conv_key, 80, None)
                .unwrap_or(fetched_msgs);
            let members = storage.list_members(&conv_key).unwrap_or_default();
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
        let rows = nav_pane::build_nav_rows(self, ui);
        let Some(row) = rows.get(idx).cloned() else {
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
                let _ = storage.upsert_messages(&all);
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
            inline_images,
            quote.clone(),
        );

        let task_label = "메시지 전송".to_string();
        ui.tasks.begin(task_label.clone());
        self.clear_current_draft(ui);
        ui.show_subject = false;

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
                        sent,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
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
            let older = storage
                .list_root_messages(&conv_key, 50, before.as_deref())
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
            let hits = storage.search_messages(&query, None, 100).unwrap_or_default();
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
                        self.messages = msgs;
                    }
                    if let Some(root_id) =
                        self.active_thread.as_ref().map(|t| t.root.msg_id.clone())
                    {
                        if let Ok(Some(thread)) = self.storage.get_thread(&conv_key, &root_id, 100)
                        {
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

                if self.active_conv_key.is_none() {
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
                    self.messages = messages;
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
                sent,
            } => {
                if self.active_conv_key.as_deref() == Some(&conv_key) {
                    if let Some(pid) = parent_msg_id {
                        if let Some(thread) = self.active_thread.as_mut() {
                            if thread.root.msg_id == pid
                                && !thread.replies.iter().any(|r| r.msg_id == sent.msg_id)
                            {
                                thread.replies.push(sent);
                            }
                        }
                        if let Some(root) = self.messages.iter_mut().find(|m| m.msg_id == pid) {
                            root.reply_count = root.reply_count.saturating_add(1);
                        }
                    } else if !self.messages.iter().any(|m| m.msg_id == sent.msg_id) {
                        self.messages.push(sent);
                        ui.chat_scroll.bottom();
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
