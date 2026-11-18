//! Adaptive 3-tier background synchronization engine (active focus, chat preview diff + $batch, channel delta + presence).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{
        split_onenote_entity_key, ConversationKind, ConversationSummary, ConversationTarget,
        CoreEvent, MailMessage, MessageItem, NotificationCandidate, OnenoteNotebook,
        OnenoteSection, SyncCursor,
    },
    storage::Storage,
};

const DOMAIN_TEAMS: u8 = 1 << 0;
const DOMAIN_OUTLOOK: u8 = 1 << 1;
const DOMAIN_ONENOTE: u8 = 1 << 2;

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const SLOW_OUTLOOK_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveDomains {
    pub teams: bool,
    pub outlook: bool,
    pub onenote: bool,
}

impl Default for ActiveDomains {
    fn default() -> Self {
        Self {
            teams: true,
            outlook: true,
            onenote: true,
        }
    }
}

impl ActiveDomains {
    pub(crate) fn effective(self, background_all_personas: bool) -> Self {
        if background_all_personas {
            Self {
                teams: true,
                outlook: true,
                onenote: self.onenote,
            }
        } else {
            self
        }
    }

    pub(crate) fn to_bits(self) -> u8 {
        let mut bits = 0u8;
        if self.teams {
            bits |= DOMAIN_TEAMS;
        }
        if self.outlook {
            bits |= DOMAIN_OUTLOOK;
        }
        if self.onenote {
            bits |= DOMAIN_ONENOTE;
        }
        bits
    }

    pub(crate) fn from_bits(bits: u8) -> Self {
        Self {
            teams: (bits & DOMAIN_TEAMS) != 0,
            outlook: (bits & DOMAIN_OUTLOOK) != 0,
            onenote: (bits & DOMAIN_ONENOTE) != 0,
        }
    }
}

pub(crate) struct SyncSharedState {
    epoch: Instant,
    domains_bits: AtomicU8,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
    background_all_personas: bool,
}

impl SyncSharedState {
    pub(crate) fn new(background_all_personas: bool) -> Self {
        Self::with_epoch(Instant::now(), background_all_personas)
    }

    pub(crate) fn with_epoch(epoch: Instant, background_all_personas: bool) -> Self {
        Self {
            epoch,
            domains_bits: AtomicU8::new(ActiveDomains::default().to_bits()),
            last_activity_ms: AtomicU64::new(0),
            activity_seq: AtomicU64::new(0),
            background_all_personas,
        }
    }

    pub(crate) fn set_active_domains(&self, domains: ActiveDomains) {
        self.domains_bits
            .store(domains.to_bits(), Ordering::Release);
    }

    pub(crate) fn effective_domains(&self) -> ActiveDomains {
        let raw = ActiveDomains::from_bits(self.domains_bits.load(Ordering::Acquire));
        raw.effective(self.background_all_personas)
    }

    pub(crate) fn record_activity(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Release);
        self.activity_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn idle_snapshot(&self, now: Instant) -> (Duration, u64) {
        let seq = self.activity_seq.load(Ordering::Acquire);
        let last_ms = self.last_activity_ms.load(Ordering::Acquire);
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        (Duration::from_millis(now_ms.saturating_sub(last_ms)), seq)
    }
}

pub(crate) fn idle_multiplier(idle_duration: Duration) -> u64 {
    let secs = idle_duration.as_secs();
    if secs >= DEEP_IDLE_THRESHOLD_SECS {
        10
    } else if secs >= IDLE_THRESHOLD_SECS {
        4
    } else {
        1
    }
}

#[derive(Debug, Default)]
pub(crate) struct PeriodicBackoffTracker {
    tick_count: u64,
    last_activity_seq: u64,
}

impl PeriodicBackoffTracker {
    pub(crate) fn on_tick(&mut self, idle_duration: Duration, activity_seq: u64) -> bool {
        if activity_seq != self.last_activity_seq {
            self.last_activity_seq = activity_seq;
            self.tick_count = 0;
        }
        let mult = idle_multiplier(idle_duration);
        if mult <= 1 {
            self.tick_count = 0;
            return true;
        }
        self.tick_count = self.tick_count.saturating_add(1);
        self.tick_count % mult == 0
    }
}

pub(crate) fn should_refresh_slow_outlook(
    last_sync: Option<Instant>,
    now: Instant,
    ttl: Duration,
) -> bool {
    match last_sync {
        None => true,
        Some(prev) => now.saturating_duration_since(prev) >= ttl,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FocusTarget {
    pub target: Option<ConversationTarget>,
    pub thread_root_id: Option<String>,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    shared: Arc<SyncSharedState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncCommand {
    RefreshConversations,
    RefreshOutlook,
    RefreshOneNote,
    SyncMailFolder {
        folder_id: String,
    },
    SyncTodoList {
        list_id: String,
    },
    SyncOneNoteSection {
        section_key: String,
    },
    SyncOneNotePage {
        page_key: String,
    },
    SyncActiveNow,
    SyncChannelHistory {
        team_id: String,
        channel_id: String,
    },
    SetActiveDomains {
        teams: bool,
        outlook: bool,
        onenote: bool,
    },
    NotifyUserActivity,
    Shutdown,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        nudge_tx: mpsc::UnboundedSender<SyncCommand>,
        background_all_personas: bool,
    ) -> Self {
        Self {
            focus_tx,
            nudge_tx,
            shared: Arc::new(SyncSharedState::new(background_all_personas)),
        }
    }

    pub fn set_focus(&self, target: Option<ConversationTarget>, thread_root_id: Option<String>) {
        self.shared.record_activity(Instant::now());
        let _ = self.focus_tx.send(FocusTarget {
            target,
            thread_root_id,
        });
    }

    pub fn request(&self, cmd: SyncCommand) {
        let _ = self.nudge_tx.send(cmd);
    }

    pub fn send(&self, cmd: SyncCommand) {
        self.request(cmd);
    }

    pub fn set_active_domains(&self, teams: bool, outlook: bool, onenote: bool) {
        self.shared.set_active_domains(ActiveDomains {
            teams,
            outlook,
            onenote,
        });
        self.shared.record_activity(Instant::now());
        self.request(SyncCommand::SetActiveDomains {
            teams,
            outlook,
            onenote,
        });
    }

    pub fn notify_activity(&self) {
        self.shared.record_activity(Instant::now());
        self.request(SyncCommand::NotifyUserActivity);
    }
}

/// Performs an initial bootstrap sync (chats + joined teams/channels) into SQLite.
pub async fn bootstrap_conversations(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let (chats_res, teams_res) = tokio::join!(
        graph.list_chats(50),
        graph.list_joined_teams_with_channels(),
    );
    let chats = chats_res?;
    storage.upsert_conversations(&chats)?;
    if let Ok(teams) = teams_res {
        storage.upsert_teams(&teams)?;
    }
    Ok(())
}

/// Performs an initial bootstrap sync of Outlook Mail folders/inbox, Calendar, Contacts, and To Do into SQLite.
/// Best-effort per service so Teams-only tokens without Outlook scopes never fail bootstrap.
pub async fn bootstrap_outlook(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let (start_iso, end_iso) = default_calendar_window();
    let (folders_res, messages_res, events_res, contacts_res, lists_res) = tokio::join!(
        graph.list_mail_folders(),
        graph.list_mail_messages("inbox", 40),
        graph.list_calendar_view(&start_iso, &end_iso, 50),
        graph.list_contacts(100),
        graph.list_todo_lists(),
    );

    if let Ok(folders) = folders_res {
        storage.upsert_mail_folders(&folders)?;
    }

    if let Ok(messages) = messages_res {
        storage.upsert_mail_messages(&messages)?;
    }

    if let Ok(events) = events_res {
        storage.upsert_calendar_events(&events)?;
    }

    if let Ok(contacts) = contacts_res {
        storage.upsert_contacts(&contacts)?;
    }

    if let Ok(lists) = lists_res {
        storage.upsert_todo_lists(&lists)?;
        if let Some(default_list) = pick_default_todo_list(&lists) {
            if let Ok(tasks) = graph.list_todo_tasks(&default_list.id, true, 50).await {
                storage.upsert_todo_tasks(&tasks)?;
            }
        }
    }

    Ok(())
}

/// Performs an initial bootstrap sync of Microsoft OneNote (personal + joined team notebooks,
/// section groups, sections, and up to 30 pages of the default/first section) into SQLite.
/// Best-effort so tokens without OneNote scopes never fail bootstrap.
pub async fn bootstrap_onenote(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let notebooks = match graph.list_all_accessible_notebooks(true).await {
        Ok(nbs) => {
            storage.upsert_onenote_notebooks(&nbs)?;
            nbs
        }
        Err(_) => return Ok(()),
    };

    if let Ok((section_groups, sections)) = graph.list_all_accessible_sections(&notebooks).await {
        storage.upsert_onenote_section_groups(&section_groups)?;
        storage.upsert_onenote_sections(&sections)?;
        if let Some(default_sec) = pick_default_onenote_section(&notebooks, &sections) {
            if let Ok(pages) = graph
                .list_onenote_pages(&default_sec.scope, &default_sec.id, 30)
                .await
            {
                storage.upsert_onenote_pages(&pages)?;
            }
        }
    }

    Ok(())
}

/// Spawns the two-lane background sync engine (Fast Interactive Lane + Background Poll Lane)
/// and returns a `SyncController` handle.
pub fn spawn_sync_engine(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<SyncCommand>();

    let active_ms = config.active_poll_ms.max(250);
    let chat_ms = config.chat_poll_ms.max(500);
    let channel_ms = config.channel_poll_ms.max(1_000);
    let shared = Arc::new(SyncSharedState::new(config.background_all_personas));

    // Lane 1: Fast Interactive Lane (never blocked by Outlook/OneNote/tier-2/tier-3 background polls)
    let fast_graph = graph.clone();
    let fast_storage = storage.clone();
    let fast_event_tx = event_tx.clone();
    let mut fast_shutdown_rx = shutdown_rx.clone();
    let fast_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut active_tick = time::interval(Duration::from_millis(active_ms));
        active_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut active_backoff = PeriodicBackoffTracker::default();

        // Consume initial immediate tick so startup isn't flooded simultaneously before focus/commands arrive.
        active_tick.tick().await;

        loop {
            if *fast_shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                _ = fast_shutdown_rx.changed() => {
                    if *fast_shutdown_rx.borrow() {
                        break;
                    }
                }
                cmd = nudge_rx.recv() => {
                    match cmd {
                        None | Some(SyncCommand::Shutdown) => {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                        Some(
                            cmd @ (SyncCommand::RefreshConversations
                            | SyncCommand::RefreshOutlook
                            | SyncCommand::RefreshOneNote),
                        ) => {
                            fast_shared.record_activity(Instant::now());
                            let _ = bg_tx.send(cmd);
                        }
                        Some(SyncCommand::SyncMailFolder { folder_id }) => {
                            fast_shared.record_activity(Instant::now());
                            if let Err(err) = sync_mail_folder(&folder_id, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("메일 폴더 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncTodoList { list_id }) => {
                            fast_shared.record_activity(Instant::now());
                            if let Err(err) = sync_todo_list(&list_id, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("To Do 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncOneNoteSection { section_key }) => {
                            fast_shared.record_activity(Instant::now());
                            if let Err(err) = sync_onenote_section(&section_key, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("OneNote 섹션 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncOneNotePage { page_key }) => {
                            fast_shared.record_activity(Instant::now());
                            if let Err(err) = sync_onenote_page(&page_key, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("OneNote 페이지 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncActiveNow) => {
                            fast_shared.record_activity(Instant::now());
                            let focus = focus_rx.borrow_and_update().clone();
                            if let Err(err) = sync_active_focus(&focus, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("활성 대화 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncChannelHistory { team_id, channel_id }) => {
                            fast_shared.record_activity(Instant::now());
                            let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "채널 히스토리 동기화 중...".to_string(),
                            });
                            if let Err(err) = sync_channel_history(&team_id, &channel_id, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("채널 동기화 실패: {err}")));
                            }
                            let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "동기화 완료".to_string(),
                            });
                        }
                        Some(SyncCommand::SetActiveDomains { teams, outlook, onenote }) => {
                            fast_shared.set_active_domains(ActiveDomains {
                                teams,
                                outlook,
                                onenote,
                            });
                            fast_shared.record_activity(Instant::now());
                        }
                        Some(SyncCommand::NotifyUserActivity) => {
                            fast_shared.record_activity(Instant::now());
                        }
                    }
                }
                changed = focus_rx.changed() => {
                    if changed.is_err() {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    fast_shared.record_activity(Instant::now());
                    let focus = focus_rx.borrow_and_update().clone();
                    if let Err(err) = sync_active_focus(&focus, &fast_graph, &fast_storage, &fast_event_tx).await {
                        let _ = fast_event_tx.send(CoreEvent::SyncError(format!("포커스 대화 동기화 실패: {err}")));
                    }
                }
                _ = active_tick.tick() => {
                    let (idle, seq) = fast_shared.idle_snapshot(Instant::now());
                    if active_backoff.on_tick(idle, seq) && fast_shared.effective_domains().teams {
                        let focus = focus_rx.borrow().clone();
                        if focus.target.is_some() {
                            if let Err(err) = sync_active_focus(&focus, &fast_graph, &fast_storage, &fast_event_tx).await {
                                let _ = fast_event_tx.send(CoreEvent::SyncError(format!("활성 폴링 오류: {err}")));
                            }
                        }
                    }
                }
            }
        }
    });

    // Lane 2: Background Poll Lane (tier-2 chat poll, tier-3 presence + Outlook poll, bulk refreshes)
    let bg_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut chat_tick = time::interval(Duration::from_millis(chat_ms));
        chat_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut channel_tick = time::interval(Duration::from_millis(channel_ms));
        channel_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut chat_backoff = PeriodicBackoffTracker::default();
        let mut channel_backoff = PeriodicBackoffTracker::default();
        let mut last_slow_outlook_sync: Option<Instant> = None;
        let mut was_idle = false;

        // Consume initial immediate ticks so startup isn't flooded simultaneously before focus/commands arrive.
        chat_tick.tick().await;
        channel_tick.tick().await;

        let mut shutdown_rx = shutdown_rx;
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                cmd = bg_rx.recv() => {
                    match cmd {
                        None => break,
                        Some(SyncCommand::RefreshConversations) => {
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "대화 목록 동기화 중...".to_string(),
                            });
                            if let Err(err) = refresh_all_conversations(&graph, &storage, &event_tx).await {
                                let _ = event_tx.send(CoreEvent::SyncError(format!("대화 목록 동기화 실패: {err}")));
                            }
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "동기화 완료".to_string(),
                            });
                        }
                        Some(SyncCommand::RefreshOutlook) => {
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "Outlook 동기화 중...".to_string(),
                            });
                            refresh_outlook_full(&graph, &storage, &event_tx, &mut last_slow_outlook_sync).await;
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "Outlook 동기화 완료".to_string(),
                            });
                        }
                        Some(SyncCommand::RefreshOneNote) => {
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "OneNote 동기화 중...".to_string(),
                            });
                            if let Err(err) = refresh_onenote(&graph, &storage, &event_tx).await {
                                let _ = event_tx.send(CoreEvent::SyncError(format!("OneNote 동기화 실패: {err}")));
                            }
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "OneNote 동기화 완료".to_string(),
                            });
                        }
                        Some(_) => {}
                    }
                }
                _ = chat_tick.tick() => {
                    let (idle, seq) = bg_shared.idle_snapshot(Instant::now());
                    maybe_shrink_on_idle(idle, &mut was_idle, &storage);
                    if chat_backoff.on_tick(idle, seq) && bg_shared.effective_domains().teams {
                        if let Err(err) = poll_chats_tier2(&graph, &storage, &event_tx).await {
                            let _ = event_tx.send(CoreEvent::SyncError(format!("채팅 폴링 오류: {err}")));
                        }
                    }
                }
                _ = channel_tick.tick() => {
                    let (idle, seq) = bg_shared.idle_snapshot(Instant::now());
                    maybe_shrink_on_idle(idle, &mut was_idle, &storage);
                    if channel_backoff.on_tick(idle, seq) {
                        let domains = bg_shared.effective_domains();
                        match (domains.teams, domains.outlook) {
                            (true, true) => {
                                let (presence_res, ()) = tokio::join!(
                                    poll_presences_tier3(&graph, &storage, &event_tx),
                                    poll_outlook_incremental(
                                        &graph,
                                        &storage,
                                        &event_tx,
                                        &mut last_slow_outlook_sync,
                                    ),
                                );
                                if let Err(err) = presence_res {
                                    let _ = event_tx.send(CoreEvent::SyncError(format!("상태 폴링 오류: {err}")));
                                }
                            }
                            (true, false) => {
                                if let Err(err) = poll_presences_tier3(&graph, &storage, &event_tx).await {
                                    let _ = event_tx.send(CoreEvent::SyncError(format!("상태 폴링 오류: {err}")));
                                }
                            }
                            (false, true) => {
                                poll_outlook_incremental(
                                    &graph,
                                    &storage,
                                    &event_tx,
                                    &mut last_slow_outlook_sync,
                                )
                                .await;
                            }
                            (false, false) => {}
                        }
                    }
                }
            }
        }
    });

    SyncController {
        focus_tx,
        nudge_tx,
        shared,
    }
}

fn maybe_shrink_on_idle(idle_duration: Duration, was_idle: &mut bool, storage: &Storage) {
    if idle_duration >= Duration::from_secs(IDLE_THRESHOLD_SECS) {
        if !*was_idle {
            *was_idle = true;
            storage.shrink_memory();
        }
    } else {
        *was_idle = false;
    }
}

async fn sync_active_focus(
    focus: &FocusTarget,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let Some(target) = &focus.target else {
        return Ok(());
    };
    let conv_key = target.conv_key();

    match target {
        ConversationTarget::Chat { chat_id } => {
            let messages = graph.list_chat_messages(chat_id, 30).await?;
            storage.upsert_messages(&messages)?;
            let _ = event_tx.send(CoreEvent::MessagesUpdated {
                conv_key,
                parent_msg_id: None,
                messages,
                notify_candidate: None,
            });
        }
        ConversationTarget::Channel {
            team_id,
            channel_id,
        } => {
            let cursor = storage.get_sync_cursor(&conv_key)?;
            let delta_fut = graph.sync_channel_messages_delta(
                team_id,
                channel_id,
                cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
            );
            let replies_fut = async {
                match &focus.thread_root_id {
                    Some(root_id) => Some((
                        root_id.clone(),
                        graph
                            .list_thread_replies(team_id, channel_id, root_id, 50)
                            .await,
                    )),
                    None => None,
                }
            };
            let (delta_res, replies_opt) = tokio::join!(delta_fut, replies_fut);

            match delta_res {
                Ok((messages, new_delta_link)) => {
                    let oldest_ts = messages
                        .iter()
                        .map(|m| m.created_at.as_str())
                        .min()
                        .map(str::to_string);
                    let new_cursor = SyncCursor {
                        scope_key: conv_key.clone(),
                        delta_link: new_delta_link,
                        oldest_msg_ts: oldest_ts,
                        synced_at: chrono::Utc::now().to_rfc3339(),
                    };
                    storage.save_sync_cursor(&new_cursor)?;
                    if !messages.is_empty() {
                        storage.upsert_messages(&messages)?;
                        let _ = event_tx.send(CoreEvent::MessagesUpdated {
                            conv_key: conv_key.clone(),
                            parent_msg_id: None,
                            messages,
                            notify_candidate: None,
                        });
                    }
                }
                Err(_) => {
                    let messages = graph.list_channel_messages(team_id, channel_id, 30).await?;
                    storage.upsert_messages(&messages)?;
                    let _ = event_tx.send(CoreEvent::MessagesUpdated {
                        conv_key: conv_key.clone(),
                        parent_msg_id: None,
                        messages,
                        notify_candidate: None,
                    });
                }
            }

            if let Some((root_id, replies_res)) = replies_opt {
                let replies = replies_res?;
                storage.upsert_messages(&replies)?;
                let _ = event_tx.send(CoreEvent::MessagesUpdated {
                    conv_key,
                    parent_msg_id: Some(root_id),
                    messages: replies,
                    notify_candidate: None,
                });
            }
        }
    }

    Ok(())
}

async fn poll_chats_tier2(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let chats = graph.list_chats(50).await?;
    let mut cached_by_key: HashMap<String, ConversationSummary> = HashMap::new();
    for chat in &chats {
        if let Some(existing) = storage.get_conversation(&chat.conv_key)? {
            cached_by_key.insert(chat.conv_key.clone(), existing);
        }
    }

    let dirty_ids = collect_dirty_chat_ids(&chats, &cached_by_key);
    let chat_by_id: HashMap<String, &ConversationSummary> = chats
        .iter()
        .filter_map(|c| match &c.target {
            ConversationTarget::Chat { chat_id } => Some((chat_id.clone(), c)),
            _ => None,
        })
        .collect();

    storage.upsert_conversations(&chats)?;

    if !dirty_ids.is_empty() {
        let fetched = graph.batch_fetch_chat_messages(&dirty_ids, 20).await?;
        for (chat_id, messages) in fetched {
            storage.upsert_messages(&messages)?;
            let conv_key = ConversationTarget::Chat {
                chat_id: chat_id.clone(),
            }
            .conv_key();
            let conv_title = chat_by_id
                .get(&chat_id)
                .map(|c| c.title.clone())
                .unwrap_or_else(|| conv_key.clone());
            let notify_candidate =
                build_notification_candidate(&conv_key, &conv_title, &messages);

            let _ = event_tx.send(CoreEvent::MessagesUpdated {
                conv_key,
                parent_msg_id: None,
                messages,
                notify_candidate,
            });
        }
    }

    let all_convs = storage.list_conversations()?;
    let _ = event_tx.send(CoreEvent::ConversationsUpdated(all_convs));
    Ok(())
}

async fn poll_presences_tier3(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let convs = storage.list_conversations()?;
    let mut seen = HashSet::new();
    let mut user_ids = Vec::new();
    for c in convs {
        if c.kind == ConversationKind::OneOnOne {
            if let Some(uid) = c.peer_user_id {
                if seen.insert(uid.clone()) {
                    user_ids.push(uid);
                }
            }
        }
    }
    if user_ids.is_empty() {
        return Ok(());
    }

    let presences = graph.get_presences_by_user_ids(&user_ids).await?;
    storage.upsert_presences(&presences)?;
    let _ = event_tx.send(CoreEvent::PresenceUpdated(presences));
    Ok(())
}

async fn refresh_all_conversations(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let (chats_res, teams_res) = tokio::join!(
        poll_chats_tier2(graph, storage, event_tx),
        graph.list_joined_teams_with_channels(),
    );
    chats_res?;
    if let Ok(teams) = teams_res {
        storage.upsert_teams(&teams)?;
        let _ = event_tx.send(CoreEvent::TeamsUpdated(teams));
        let all_convs = storage.list_conversations()?;
        let _ = event_tx.send(CoreEvent::ConversationsUpdated(all_convs));
    }
    Ok(())
}

async fn sync_channel_history(
    team_id: &str,
    channel_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let target = ConversationTarget::Channel {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
    };
    let conv_key = target.conv_key();
    let messages = graph.list_channel_messages(team_id, channel_id, 50).await?;
    storage.upsert_messages(&messages)?;
    let _ = event_tx.send(CoreEvent::MessagesUpdated {
        conv_key,
        parent_msg_id: None,
        messages,
        notify_candidate: None,
    });
    Ok(())
}

fn default_calendar_window() -> (String, String) {
    let now = chrono::Utc::now();
    let start = (now - chrono::Duration::days(1)).to_rfc3339();
    let end = (now + chrono::Duration::days(14)).to_rfc3339();
    (start, end)
}

fn pick_default_todo_list(lists: &[crate::models::TodoList]) -> Option<&crate::models::TodoList> {
    lists
        .iter()
        .find(|l| {
            l.wellknown_list_name
                .as_deref()
                .map(|w| w.eq_ignore_ascii_case("defaultList"))
                .unwrap_or(false)
        })
        .or_else(|| lists.first())
}

async fn refresh_mail_folders_best_effort(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    if let Ok(folders) = graph.list_mail_folders().await {
        if storage.upsert_mail_folders(&folders).is_ok() {
            let _ = event_tx.send(CoreEvent::MailFoldersUpdated(folders));
        }
    }
}

async fn refresh_slow_outlook_resources(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    let (start_iso, end_iso) = default_calendar_window();
    let (events_res, contacts_res, lists_res) = tokio::join!(
        graph.list_calendar_view(&start_iso, &end_iso, 50),
        graph.list_contacts(100),
        graph.list_todo_lists(),
    );

    if let Ok(events) = events_res {
        if storage.upsert_calendar_events(&events).is_ok() {
            let _ = event_tx.send(CoreEvent::CalendarEventsUpdated(events));
        }
    }

    if let Ok(contacts) = contacts_res {
        if storage.upsert_contacts(&contacts).is_ok() {
            let _ = event_tx.send(CoreEvent::ContactsUpdated(contacts));
        }
    }

    if let Ok(lists) = lists_res {
        if storage.upsert_todo_lists(&lists).is_ok() {
            let (list_id, tasks) = if let Some(default_list) = pick_default_todo_list(&lists) {
                match graph.list_todo_tasks(&default_list.id, true, 50).await {
                    Ok(tasks) => {
                        let _ = storage.upsert_todo_tasks(&tasks);
                        (Some(default_list.id.clone()), tasks)
                    }
                    Err(_) => (Some(default_list.id.clone()), Vec::new()),
                }
            } else {
                (None, Vec::new())
            };
            let _ = event_tx.send(CoreEvent::TodoUpdated {
                lists,
                list_id,
                tasks,
            });
        }
    }
}

async fn refresh_outlook_full(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_slow_outlook_sync: &mut Option<Instant>,
) {
    let (_, inbox_res, _) = tokio::join!(
        refresh_mail_folders_best_effort(graph, storage, event_tx),
        sync_mail_folder_with_limit("inbox", 40, graph, storage, event_tx),
        refresh_slow_outlook_resources(graph, storage, event_tx),
    );
    let _ = inbox_res;
    *last_slow_outlook_sync = Some(Instant::now());
}

async fn poll_outlook_incremental(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_slow_outlook_sync: &mut Option<Instant>,
) {
    let now = Instant::now();
    if should_refresh_slow_outlook(*last_slow_outlook_sync, now, SLOW_OUTLOOK_TTL) {
        let (_, inbox_res, _) = tokio::join!(
            refresh_mail_folders_best_effort(graph, storage, event_tx),
            sync_mail_folder("inbox", graph, storage, event_tx),
            refresh_slow_outlook_resources(graph, storage, event_tx),
        );
        let _ = inbox_res;
        *last_slow_outlook_sync = Some(Instant::now());
    } else {
        let (_, inbox_res) = tokio::join!(
            refresh_mail_folders_best_effort(graph, storage, event_tx),
            sync_mail_folder("inbox", graph, storage, event_tx),
        );
        let _ = inbox_res;
    }
}

async fn sync_mail_folder(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    sync_mail_folder_with_limit(folder_id, 50, graph, storage, event_tx).await
}

async fn sync_mail_folder_with_limit(
    folder_id: &str,
    fallback_limit: usize,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let scope_key = format!("mail_delta:{folder_id}");
    let prev_cursor = storage.get_sync_cursor(&scope_key)?;
    let had_initial = prev_cursor.is_some()
        || storage
            .list_mail_summaries(Some(folder_id), 1)
            .map(|existing| !existing.is_empty())
            .unwrap_or(false);

    match graph
        .delta_mail_messages(
            folder_id,
            prev_cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
        )
        .await
    {
        Ok((delta_msgs, deleted_ids, new_delta_link)) => {
            let notify_candidate = if had_initial {
                find_new_unread_mail_candidate(folder_id, storage, &delta_msgs)
            } else {
                None
            };
            if !deleted_ids.is_empty() {
                storage.delete_mail_messages_by_ids(&deleted_ids)?;
            }
            if !delta_msgs.is_empty() {
                storage.upsert_mail_messages(&delta_msgs)?;
            }
            let oldest_ts = delta_msgs
                .iter()
                .map(|m| m.received_at.as_str())
                .min()
                .map(str::to_string);
            storage.save_sync_cursor(&SyncCursor {
                scope_key,
                delta_link: new_delta_link,
                oldest_msg_ts: oldest_ts,
                synced_at: chrono::Utc::now().to_rfc3339(),
            })?;
            let messages = storage.list_mail_summaries(Some(folder_id), 50)?;
            let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: folder_id.to_string(),
                messages,
                notify_candidate,
            });
        }
        Err(_) => {
            let messages = graph.list_mail_messages(folder_id, fallback_limit).await?;
            let notify_candidate = if had_initial {
                find_new_unread_mail_candidate(folder_id, storage, &messages)
            } else {
                None
            };
            storage.upsert_mail_messages(&messages)?;
            let oldest_ts = messages
                .iter()
                .map(|m| m.received_at.as_str())
                .min()
                .map(str::to_string);
            storage.save_sync_cursor(&SyncCursor {
                scope_key,
                delta_link: prev_cursor.and_then(|c| c.delta_link),
                oldest_msg_ts: oldest_ts,
                synced_at: chrono::Utc::now().to_rfc3339(),
            })?;
            let summaries = storage.list_mail_summaries(Some(folder_id), 50)?;
            let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: folder_id.to_string(),
                messages: summaries,
                notify_candidate,
            });
        }
    }
    Ok(())
}

async fn sync_todo_list(
    list_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let tasks = graph.list_todo_tasks(list_id, true, 100).await?;
    storage.upsert_todo_tasks(&tasks)?;
    let lists = storage.list_todo_lists()?;
    let _ = event_tx.send(CoreEvent::TodoUpdated {
        lists,
        list_id: Some(list_id.to_string()),
        tasks,
    });
    Ok(())
}

fn pick_default_onenote_section<'a>(
    notebooks: &[OnenoteNotebook],
    sections: &'a [OnenoteSection],
) -> Option<&'a OnenoteSection> {
    if sections.is_empty() {
        return None;
    }

    let default_nb_keys: HashSet<String> = notebooks
        .iter()
        .filter(|nb| nb.is_default)
        .map(|nb| nb.notebook_key.clone())
        .collect();

    sections
        .iter()
        .find(|s| s.scope.is_personal() && s.is_default)
        .or_else(|| {
            sections.iter().find(|s| {
                s.notebook_id
                    .as_deref()
                    .map(|nb_id| default_nb_keys.contains(&s.scope.make_entity_key(nb_id)))
                    .unwrap_or(false)
            })
        })
        .or_else(|| sections.iter().find(|s| s.is_default))
        .or_else(|| sections.iter().find(|s| s.scope.is_personal()))
        .or_else(|| sections.first())
}

async fn refresh_onenote(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let fetched_notebooks = graph.list_all_accessible_notebooks(true).await?;
    storage.upsert_onenote_notebooks(&fetched_notebooks)?;

    let (fetched_sgs, fetched_secs) = graph
        .list_all_accessible_sections(&fetched_notebooks)
        .await?;
    storage.upsert_onenote_section_groups(&fetched_sgs)?;
    storage.upsert_onenote_sections(&fetched_secs)?;

    let notebooks = storage.list_onenote_notebooks()?;
    let section_groups = storage.list_onenote_section_groups()?;
    let sections = storage.list_onenote_sections(None)?;

    let _ = event_tx.send(CoreEvent::OnenoteHierarchyUpdated {
        notebooks: notebooks.clone(),
        section_groups,
        sections: sections.clone(),
    });

    if let Some(default_sec) = pick_default_onenote_section(&notebooks, &sections) {
        if let Ok(pages) = graph
            .list_onenote_pages(&default_sec.scope, &default_sec.id, 50)
            .await
        {
            storage.upsert_onenote_pages(&pages)?;
            let stored_pages = storage.list_onenote_pages(&default_sec.section_key)?;
            let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                section_key: default_sec.section_key.clone(),
                pages: stored_pages,
            });
        }
    }

    Ok(())
}

async fn sync_onenote_section(
    section_key: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let trimmed = section_key.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let existing = storage
        .list_onenote_sections(None)?
        .into_iter()
        .find(|s| s.section_key == trimmed || s.id == trimmed);

    let (scope, sec_id, canonical_key) = match existing {
        Some(sec) => (sec.scope, sec.id, sec.section_key),
        None => {
            let (scope, raw_id) = split_onenote_entity_key(trimmed);
            let canonical = scope.make_entity_key(&raw_id);
            (scope, raw_id, canonical)
        }
    };

    let pages = graph.list_onenote_pages(&scope, &sec_id, 100).await?;
    storage.upsert_onenote_pages(&pages)?;
    let stored_pages = storage.list_onenote_pages(&canonical_key)?;
    let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
        section_key: canonical_key,
        pages: stored_pages,
    });
    Ok(())
}

async fn sync_onenote_page(
    page_key: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let trimmed = page_key.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let cached_summary = storage.get_onenote_page_summary(trimmed)?;
    let (scope, page_id) = match &cached_summary {
        Some(sum) => (sum.scope.clone(), sum.id.clone()),
        None => split_onenote_entity_key(trimmed),
    };

    let detail = graph
        .get_onenote_page_detail(&scope, &page_id, cached_summary)
        .await?;
    storage.upsert_onenote_page_detail(&detail)?;
    let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
    Ok(())
}

fn find_new_unread_mail_candidate(
    folder_id: &str,
    storage: &Storage,
    incoming: &[MailMessage],
) -> Option<NotificationCandidate> {
    let newest = incoming
        .iter()
        .filter(|m| !m.is_read && !m.is_draft)
        .filter(|m| matches!(storage.get_mail_message(&m.id), Ok(None)))
        .max_by(|a, b| a.received_at.cmp(&b.received_at).then(a.id.cmp(&b.id)))?;
    let folder_title = storage
        .list_mail_folders()
        .ok()
        .and_then(|folders| {
            folders.into_iter().find(|f| {
                f.id == folder_id
                    || f.well_known_name
                        .as_deref()
                        .map(|w| w.eq_ignore_ascii_case(folder_id))
                        .unwrap_or(false)
            })
        })
        .map(|f| f.display_name)
        .unwrap_or_else(|| folder_id.to_string());
    let preview_body = newest
        .preview
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let preview = if preview_body.is_empty() {
        newest.subject.clone()
    } else if newest.subject.trim().is_empty() {
        preview_body
    } else {
        format!("{} - {}", newest.subject.trim(), preview_body)
    };
    Some(NotificationCandidate {
        conv_key: format!("mail:{folder_id}"),
        conv_title: folder_title,
        sender: newest.from_display(),
        preview,
    })
}

pub(crate) fn collect_dirty_chat_ids(
    fresh_chats: &[ConversationSummary],
    cached_by_key: &HashMap<String, ConversationSummary>,
) -> Vec<String> {
    let mut dirty = Vec::new();
    for chat in fresh_chats {
        let ConversationTarget::Chat { chat_id } = &chat.target else {
            continue;
        };
        let is_dirty = match cached_by_key.get(&chat.conv_key) {
            None => chat.last_msg_id.is_some() || chat.last_updated_at.is_some(),
            Some(cached) => {
                (chat.last_msg_id.is_some() && chat.last_msg_id != cached.last_msg_id)
                    || (chat.last_updated_at.is_some()
                        && chat.last_updated_at != cached.last_updated_at)
            }
        };
        if is_dirty {
            dirty.push(chat_id.clone());
        }
    }
    dirty
}

pub(crate) fn build_notification_candidate(
    conv_key: &str,
    conv_title: &str,
    messages: &[MessageItem],
) -> Option<NotificationCandidate> {
    let newest = messages
        .iter()
        .filter(|m| !m.deleted)
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.msg_id.cmp(&b.msg_id)))?;
    if newest.is_me {
        return None;
    }
    let preview = newest
        .body_plain
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(NotificationCandidate {
        conv_key: conv_key.to_string(),
        conv_title: conv_title.to_string(),
        sender: newest.author_name.clone(),
        preview,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;

    fn chat_summary(id: &str, last_msg_id: Option<&str>, updated: Option<&str>) -> ConversationSummary {
        let target = ConversationTarget::Chat {
            chat_id: id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: format!("Chat {id}"),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: last_msg_id.map(str::to_string),
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: updated.map(str::to_string),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }
    }

    #[test]
    fn test_collect_dirty_chat_ids_and_notification_candidate() {
        let fresh = vec![
            chat_summary("c1", Some("m2"), Some("2026-11-15T10:05:00Z")),
            chat_summary("c2", Some("m1"), Some("2026-11-15T09:00:00Z")),
            chat_summary("c3", Some("m9"), Some("2026-11-15T11:00:00Z")),
        ];
        let mut cached = HashMap::new();
        cached.insert(
            "chat:c1".to_string(),
            chat_summary("c1", Some("m1"), Some("2026-11-15T10:00:00Z")),
        );
        cached.insert(
            "chat:c2".to_string(),
            chat_summary("c2", Some("m1"), Some("2026-11-15T09:00:00Z")),
        );

        let dirty = collect_dirty_chat_ids(&fresh, &cached);
        assert_eq!(dirty, vec!["c1".to_string(), "c3".to_string()]);

        let msg_other = MessageItem {
            msg_key: "chat:c1|m2".to_string(),
            conv_key: "chat:c1".to_string(),
            msg_id: "m2".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u2".to_string()),
            author_name: "홍길동".to_string(),
            is_me: false,
            created_at: "2026-11-15T10:05:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: "<p>새 메시지</p>".to_string(),
            body_plain: "새\n메시지".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        };
        let cand = build_notification_candidate("chat:c1", "홍길동", &[msg_other.clone()]).unwrap();
        assert_eq!(cand.sender, "홍길동");
        assert_eq!(cand.preview, "새 메시지");

        let mut msg_me = msg_other;
        msg_me.is_me = true;
        assert!(build_notification_candidate("chat:c1", "홍길동", &[msg_me]).is_none());
    }

    #[tokio::test]
    async fn test_spawn_sync_engine_controller_and_shutdown() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            }),
            None,
        );
        ctrl.request(SyncCommand::Shutdown);
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn test_spawn_sync_engine_two_lane_routing_and_drop_shutdown() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);

        // Coalesce rapid focus switches and verify both lanes process commands without deadlocking
        ctrl.request(SyncCommand::RefreshOutlook);
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-first".to_string(),
            }),
            None,
        );
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-latest".to_string(),
            }),
            None,
        );
        ctrl.request(SyncCommand::SyncActiveNow);

        // Wait briefly for Background Poll Lane to emit Outlook SyncStateChanged and Fast Lane to process
        let mut saw_outlook_state = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await {
                Ok(Some(CoreEvent::SyncStateChanged { summary, .. }))
                    if summary.contains("Outlook") =>
                {
                    saw_outlook_state = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_outlook_state);

        // Dropping controller closes nudge_tx + focus_tx and shuts down both lanes cleanly
        drop(ctrl);
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "both sync lanes should terminate when controller is dropped");
    }

    #[tokio::test]
    async fn test_bootstrap_outlook_is_best_effort_on_scope_or_graph_errors() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        bootstrap_outlook(&graph, &storage).await.unwrap();
        bootstrap_onenote(&graph, &storage).await.unwrap();
    }

    #[test]
    fn test_pick_default_onenote_section_prefers_personal_default() {
        use crate::models::OnenoteScope;

        let group_scope = OnenoteScope::Group {
            group_id: "t1".to_string(),
            group_name: Some("개발팀".to_string()),
        };
        let notebooks = vec![OnenoteNotebook {
            notebook_key: "me|nb-def".to_string(),
            id: "nb-def".to_string(),
            scope: OnenoteScope::Me,
            display_name: "기본 노트".to_string(),
            color: None,
            is_default: true,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        }];
        let sections = vec![
            OnenoteSection {
                section_key: "group:t1|sec-g".to_string(),
                id: "sec-g".to_string(),
                scope: group_scope,
                notebook_id: Some("nb-g".to_string()),
                notebook_name: None,
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "팀 섹션".to_string(),
                color: None,
                is_default: true,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
            OnenoteSection {
                section_key: "me|sec-me".to_string(),
                id: "sec-me".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb-def".to_string()),
                notebook_name: None,
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "빠른 노트".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
        ];

        let picked = pick_default_onenote_section(&notebooks, &sections).unwrap();
        assert_eq!(picked.section_key, "me|sec-me");
    }

    #[test]
    fn test_outlook_mail_notification_candidate_and_default_todo_picker() {
        use crate::models::{EmailRecipient, MailFolder, TodoList};

        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".to_string(),
                display_name: "받은 편지함".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                unread_item_count: 1,
                total_item_count: 1,
            }])
            .unwrap();

        let existing = MailMessage {
            id: "m-old".to_string(),
            conversation_id: None,
            folder_id: Some("fld-inbox".to_string()),
            subject: "기존 메일".to_string(),
            from: Some(EmailRecipient {
                name: Some("김철수".to_string()),
                address: "kim@example.com".to_string(),
            }),
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            received_at: "2026-11-15T09:00:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            has_attachments: false,
            preview: "이미 있음".to_string(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: Vec::new(),
            web_link: None,
        };
        storage.upsert_mail_messages(&[existing.clone()]).unwrap();

        // Already-stored unread message must not re-notify
        assert!(find_new_unread_mail_candidate("inbox", &storage, &[existing]).is_none());

        // Brand-new unread message produces NotificationCandidate with folder display name
        let fresh_unread = MailMessage {
            id: "m-new".to_string(),
            conversation_id: None,
            folder_id: Some("fld-inbox".to_string()),
            subject: "긴급 공지".to_string(),
            from: Some(EmailRecipient {
                name: Some("홍길동".to_string()),
                address: "hong@example.com".to_string(),
            }),
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            received_at: "2026-11-15T11:00:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "high".to_string(),
            has_attachments: false,
            preview: "배포 일정 확인\n부탁드립니다".to_string(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: Vec::new(),
            web_link: None,
        };
        let cand = find_new_unread_mail_candidate("inbox", &storage, &[fresh_unread]).unwrap();
        assert_eq!(cand.conv_key, "mail:inbox");
        assert_eq!(cand.conv_title, "받은 편지함");
        assert_eq!(cand.sender, "홍길동 <hong@example.com>");
        assert_eq!(cand.preview, "긴급 공지 - 배포 일정 확인 부탁드립니다");

        let lists = vec![
            TodoList {
                id: "l1".to_string(),
                display_name: "개인".to_string(),
                wellknown_list_name: None,
                is_owner: true,
                is_shared: false,
            },
            TodoList {
                id: "l2".to_string(),
                display_name: "작업".to_string(),
                wellknown_list_name: Some("defaultList".to_string()),
                is_owner: true,
                is_shared: false,
            },
        ];
        assert_eq!(pick_default_todo_list(&lists).unwrap().id, "l2");
    }

    #[test]
    fn test_idle_backoff_multiplier_and_immediate_activity_reset() {
        assert_eq!(idle_multiplier(Duration::from_secs(0)), 1);
        assert_eq!(idle_multiplier(Duration::from_secs(119)), 1);
        assert_eq!(idle_multiplier(Duration::from_secs(120)), 4);
        assert_eq!(idle_multiplier(Duration::from_secs(599)), 4);
        assert_eq!(idle_multiplier(Duration::from_secs(600)), 10);
        assert_eq!(idle_multiplier(Duration::from_secs(1_800)), 10);

        let epoch = Instant::now();
        let state = SyncSharedState::with_epoch(epoch, false);
        let mut tracker = PeriodicBackoffTracker::default();

        // Active (< 120s): every tick runs (1x)
        let (idle, seq) = state.idle_snapshot(epoch + Duration::from_secs(30));
        assert!(tracker.on_tick(idle, seq));
        assert!(tracker.on_tick(idle, seq));

        // Idle (120s .. 600s): 4x — skips 3 out of 4 ticks
        let (idle_4x, seq_4x) = state.idle_snapshot(epoch + Duration::from_secs(150));
        let results_4x: Vec<bool> = (0..8)
            .map(|_| tracker.on_tick(idle_4x, seq_4x))
            .collect();
        assert_eq!(
            results_4x,
            vec![false, false, false, true, false, false, false, true]
        );

        // Deep idle (>= 600s): 10x — skips 9 out of 10 ticks
        let mut deep_tracker = PeriodicBackoffTracker::default();
        let (idle_10x, seq_10x) = state.idle_snapshot(epoch + Duration::from_secs(600));
        let results_10x: Vec<bool> = (0..20)
            .map(|_| deep_tracker.on_tick(idle_10x, seq_10x))
            .collect();
        let expected_10x: Vec<bool> = (1..=20).map(|i| i % 10 == 0).collect();
        assert_eq!(results_10x, expected_10x);

        // User activity arrives at 610s -> immediate reset to 1x active!
        state.record_activity(epoch + Duration::from_secs(610));
        let (idle_after, seq_after) = state.idle_snapshot(epoch + Duration::from_secs(611));
        assert_eq!(idle_multiplier(idle_after), 1);
        assert!(deep_tracker.on_tick(idle_after, seq_after));
        assert!(tracker.on_tick(idle_after, seq_after));
    }

    #[test]
    fn test_domain_bitmask_gating_and_background_all_personas_override() {
        let all = ActiveDomains {
            teams: true,
            outlook: true,
            onenote: true,
        };
        assert_eq!(ActiveDomains::from_bits(all.to_bits()), all);

        let outlook_only = ActiveDomains {
            teams: false,
            outlook: true,
            onenote: false,
        };
        assert_eq!(
            ActiveDomains::from_bits(outlook_only.to_bits()),
            outlook_only
        );

        // Default before SetActiveDomains: all enabled
        let gated_state = SyncSharedState::new(false);
        assert_eq!(gated_state.effective_domains(), all);

        // Switching to Outlook-only disables Teams polling when background_all_personas = false
        gated_state.set_active_domains(outlook_only);
        assert_eq!(gated_state.effective_domains(), outlook_only);

        // With background_all_personas = true, teams & outlook stay enabled regardless
        let bg_all_state = SyncSharedState::new(true);
        bg_all_state.set_active_domains(ActiveDomains {
            teams: false,
            outlook: false,
            onenote: true,
        });
        assert_eq!(
            bg_all_state.effective_domains(),
            ActiveDomains {
                teams: true,
                outlook: true,
                onenote: true,
            }
        );
    }

    #[test]
    fn test_should_refresh_slow_outlook_ttl() {
        let t0 = Instant::now();
        assert!(should_refresh_slow_outlook(None, t0, SLOW_OUTLOOK_TTL));
        assert!(!should_refresh_slow_outlook(
            Some(t0),
            t0 + Duration::from_secs(599),
            SLOW_OUTLOOK_TTL
        ));
        assert!(should_refresh_slow_outlook(
            Some(t0),
            t0 + Duration::from_secs(600),
            SLOW_OUTLOOK_TTL
        ));
    }

    #[tokio::test]
    async fn test_poll_outlook_incremental_uses_delta_and_slow_resource_ttl() {
        use std::sync::atomic::AtomicUsize;
        use axum::{routing::get, Json, Router};
        use crate::auth::StoredTokens;

        #[derive(Default)]
        struct Counts {
            folders: AtomicUsize,
            delta: AtomicUsize,
            calendar: AtomicUsize,
            contacts: AtomicUsize,
            todo: AtomicUsize,
        }

        let counts = Arc::new(Counts::default());
        let c_folders = Arc::clone(&counts);
        let c_delta = Arc::clone(&counts);
        let c_cal = Arc::clone(&counts);
        let c_contacts = Arc::clone(&counts);
        let c_todo = Arc::clone(&counts);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let delta_next = format!("{base_url}/me/mailFolders/inbox/messages/delta?$deltatoken=tok2");

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(move || {
                    let c = Arc::clone(&c_folders);
                    async move {
                        c.folders.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/inbox/messages/delta",
                get(move || {
                    let c = Arc::clone(&c_delta);
                    let dl = delta_next.clone();
                    async move {
                        c.delta.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [],
                            "@odata.deltaLink": dl
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/calendarView",
                get(move || {
                    let c = Arc::clone(&c_cal);
                    async move {
                        c.calendar.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/me/contacts",
                get(move || {
                    let c = Arc::clone(&c_contacts);
                    async move {
                        c.contacts.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/me/todo/lists",
                get(move || {
                    let c = Arc::clone(&c_todo);
                    async move {
                        c.todo.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: base_url,
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "test-tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut last_slow: Option<Instant> = None;

        // First tick: last_slow is None -> syncs folders + delta + slow resources
        poll_outlook_incremental(&graph, &storage, &event_tx, &mut last_slow).await;
        assert!(last_slow.is_some());
        assert_eq!(counts.folders.load(Ordering::SeqCst), 1);
        assert_eq!(counts.delta.load(Ordering::SeqCst), 1);
        assert_eq!(counts.calendar.load(Ordering::SeqCst), 1);
        assert_eq!(counts.contacts.load(Ordering::SeqCst), 1);
        assert_eq!(counts.todo.load(Ordering::SeqCst), 1);
        assert!(storage
            .get_sync_cursor("mail_delta:inbox")
            .unwrap()
            .and_then(|c| c.delta_link)
            .is_some());

        // Second tick immediately after: only folders + delta run; slow resources skipped!
        poll_outlook_incremental(&graph, &storage, &event_tx, &mut last_slow).await;
        assert_eq!(counts.folders.load(Ordering::SeqCst), 2);
        assert_eq!(counts.delta.load(Ordering::SeqCst), 2);
        assert_eq!(counts.calendar.load(Ordering::SeqCst), 1);
        assert_eq!(counts.contacts.load(Ordering::SeqCst), 1);
        assert_eq!(counts.todo.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn test_sync_controller_domain_and_activity_commands_shutdown_cleanly() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);
        ctrl.set_active_domains(false, true, false);
        ctrl.notify_activity();
        ctrl.request(SyncCommand::Shutdown);

        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok());
    }

    #[test]
    fn test_maybe_shrink_on_idle_transition_state_machine() {
        let storage = Storage::open_in_memory().unwrap();
        let mut was_idle = false;

        // Active duration stays false
        maybe_shrink_on_idle(
            Duration::from_secs(IDLE_THRESHOLD_SECS - 1),
            &mut was_idle,
            &storage,
        );
        assert!(!was_idle);

        // Crossing threshold sets was_idle = true and runs shrink_memory
        maybe_shrink_on_idle(
            Duration::from_secs(IDLE_THRESHOLD_SECS),
            &mut was_idle,
            &storage,
        );
        assert!(was_idle);

        // Staying idle keeps was_idle = true
        maybe_shrink_on_idle(
            Duration::from_secs(DEEP_IDLE_THRESHOLD_SECS),
            &mut was_idle,
            &storage,
        );
        assert!(was_idle);

        // Returning to active resets was_idle = false
        maybe_shrink_on_idle(Duration::from_secs(5), &mut was_idle, &storage);
        assert!(!was_idle);
    }

    #[tokio::test]
    async fn test_sync_mail_folder_emits_summary_only_events_on_delta_and_fallback() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use crate::auth::{AuthManager, StoredTokens};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let delta_mode = Arc::new(AtomicU64::new(0));
        let delta_mode_route = Arc::clone(&delta_mode);

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders/inbox/messages/delta",
                get(move || {
                    let mode = Arc::clone(&delta_mode_route);
                    async move {
                        if mode.load(Ordering::SeqCst) == 0 {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [{
                                        "id": "m-delta-1",
                                        "parentFolderId": "inbox",
                                        "subject": "델타 대용량 메일",
                                        "receivedDateTime": "2026-11-16T09:00:00Z",
                                        "isRead": false,
                                        "isDraft": false,
                                        "importance": "normal",
                                        "hasAttachments": false,
                                        "bodyPreview": "요약 프리뷰",
                                        "body": {
                                            "contentType": "html",
                                            "content": "<html><body><p>대용량 HTML 본문 데이터</p></body></html>"
                                        }
                                    }],
                                    "@odata.deltaLink": "http://example.invalid/delta?token=1"
                                })),
                            )
                        } else {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({ "error": "delta failed" })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/inbox/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "m-fallback-1",
                            "parentFolderId": "inbox",
                            "subject": "폴백 대용량 메일",
                            "receivedDateTime": "2026-11-16T10:00:00Z",
                            "isRead": false,
                            "isDraft": false,
                            "importance": "normal",
                            "hasAttachments": false,
                            "bodyPreview": "폴백 프리뷰",
                            "body": {
                                "contentType": "html",
                                "content": "<html><body><p>폴백 대용량 HTML 본문</p></body></html>"
                            }
                        }]
                    }))
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: base_url,
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "test-tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        // 1) Delta path stores full body in SQLite, emits summary-only CoreEvent
        sync_mail_folder_with_limit("inbox", 50, &graph, &storage, &event_tx)
            .await
            .unwrap();
        let ev1 = event_rx.recv().await.unwrap();
        match ev1 {
            CoreEvent::MailMessagesUpdated {
                folder_id,
                messages,
                ..
            } => {
                assert_eq!(folder_id, "inbox");
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "m-delta-1");
                assert!(messages[0].body_html.is_empty());
                assert!(messages[0].body_plain.is_empty());
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let stored1 = storage.get_mail_message("m-delta-1").unwrap().unwrap();
        assert!(stored1.body_html.contains("대용량 HTML 본문 데이터"));

        // 2) Fallback path on delta error also stores full body in SQLite, emits summary-only CoreEvent
        delta_mode.store(1, Ordering::SeqCst);
        sync_mail_folder_with_limit("inbox", 50, &graph, &storage, &event_tx)
            .await
            .unwrap();
        let ev2 = event_rx.recv().await.unwrap();
        match ev2 {
            CoreEvent::MailMessagesUpdated {
                folder_id,
                messages,
                ..
            } => {
                assert_eq!(folder_id, "inbox");
                assert_eq!(messages.len(), 2);
                assert!(messages.iter().all(|m| m.body_html.is_empty()));
                assert!(messages.iter().all(|m| m.body_plain.is_empty()));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let stored2 = storage.get_mail_message("m-fallback-1").unwrap().unwrap();
        assert!(stored2.body_html.contains("폴백 대용량 HTML 본문"));

        server.abort();
    }
}
