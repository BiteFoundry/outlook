//! Adaptive 2-lane background synchronization engine for Microsoft Teams (Fast Interactive Lane + Background Poll Lane).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
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
        ConversationKind, ConversationSummary, ConversationTarget, CoreEvent, MessageItem,
        NotificationCandidate, SyncCursor,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;

pub(crate) struct SyncSharedState {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
}

impl SyncSharedState {
    pub(crate) fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    pub(crate) fn with_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            last_activity_ms: AtomicU64::new(0),
            activity_seq: AtomicU64::new(0),
        }
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
    SyncActiveNow,
    SyncChannelHistory {
        team_id: String,
        channel_id: String,
    },
    NotifyUserActivity,
    Shutdown,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        nudge_tx: mpsc::UnboundedSender<SyncCommand>,
        _legacy_background_all: bool,
    ) -> Self {
        Self {
            focus_tx,
            nudge_tx,
            shared: Arc::new(SyncSharedState::new()),
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

    pub fn notify_activity(&self) {
        self.shared.record_activity(Instant::now());
        self.request(SyncCommand::NotifyUserActivity);
    }
}

/// Performs an initial bootstrap sync (chats + joined teams/channels) into SQLite.
pub async fn bootstrap_conversations(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let (chats_res, teams_res) = tokio::join!(
        graph.list_chats_page(50, None),
        graph.list_joined_teams_with_channels(),
    );
    let (chats, next_link) = chats_res?;
    storage.upsert_conversations(&chats)?;
    save_initial_chats_list_cursor_if_absent(storage, next_link)?;
    if let Ok(teams) = teams_res {
        storage.upsert_teams(&teams)?;
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
    let shared = Arc::new(SyncSharedState::new());

    // Lane 1: Fast Interactive Lane (active conversation/thread polling + immediate commands)
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
                        Some(cmd @ SyncCommand::RefreshConversations) => {
                            fast_shared.record_activity(Instant::now());
                            let _ = bg_tx.send(cmd);
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
                    if active_backoff.on_tick(idle, seq) {
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

    // Lane 2: Background Poll Lane (chat preview diff + $batch, presence polling, bulk refreshes)
    let bg_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut chat_tick = time::interval(Duration::from_millis(chat_ms));
        chat_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut channel_tick = time::interval(Duration::from_millis(channel_ms));
        channel_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut chat_backoff = PeriodicBackoffTracker::default();
        let mut channel_backoff = PeriodicBackoffTracker::default();
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
                        Some(_) => {}
                    }
                }
                _ = chat_tick.tick() => {
                    let (idle, seq) = bg_shared.idle_snapshot(Instant::now());
                    maybe_shrink_on_idle(idle, &mut was_idle, &storage);
                    if chat_backoff.on_tick(idle, seq) {
                        if let Err(err) = poll_chats_tier2(&graph, &storage, &event_tx).await {
                            let _ = event_tx.send(CoreEvent::SyncError(format!("채팅 폴링 오류: {err}")));
                        }
                    }
                }
                _ = channel_tick.tick() => {
                    let (idle, seq) = bg_shared.idle_snapshot(Instant::now());
                    maybe_shrink_on_idle(idle, &mut was_idle, &storage);
                    if channel_backoff.on_tick(idle, seq) {
                        if let Err(err) = poll_presences_tier3(&graph, &storage, &event_tx).await {
                            let _ = event_tx.send(CoreEvent::SyncError(format!("상태 폴링 오류: {err}")));
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

fn save_initial_history_cursor_if_absent(
    storage: &Storage,
    conv_key: &str,
    msgs: &[MessageItem],
    next_link: Option<String>,
) -> Result<()> {
    let scope = SyncCursor::history_scope(conv_key);
    if storage.get_sync_cursor(&scope)?.is_none() {
        let oldest_msg_ts = if next_link.is_none() {
            Some(SyncCursor::HISTORY_EOF.to_string())
        } else {
            msgs.iter()
                .map(|m| m.created_at.as_str())
                .min()
                .map(str::to_string)
        };
        storage.save_sync_cursor(&SyncCursor {
            scope_key: scope,
            delta_link: next_link,
            oldest_msg_ts,
            synced_at: chrono::Utc::now().to_rfc3339(),
        })?;
    }
    Ok(())
}

fn save_initial_chats_list_cursor_if_absent(
    storage: &Storage,
    next_link: Option<String>,
) -> Result<()> {
    if storage
        .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)?
        .is_none()
    {
        let oldest_msg_ts = if next_link.is_none() {
            Some(SyncCursor::HISTORY_EOF.to_string())
        } else {
            None
        };
        storage.save_sync_cursor(&SyncCursor {
            scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
            delta_link: next_link,
            oldest_msg_ts,
            synced_at: chrono::Utc::now().to_rfc3339(),
        })?;
    }
    Ok(())
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
        ConversationTarget::Chat { .. } => {
            let (messages, next_link) = graph.list_messages_page(target, 50, None).await?;
            storage.upsert_messages(&messages)?;
            save_initial_history_cursor_if_absent(storage, &conv_key, &messages, next_link)?;
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
                    let (messages, next_link) = graph.list_messages_page(target, 50, None).await?;
                    storage.upsert_messages(&messages)?;
                    save_initial_history_cursor_if_absent(
                        storage,
                        &conv_key,
                        &messages,
                        next_link,
                    )?;
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
    let (chats, next_link) = graph.list_chats_page(50, None).await?;
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
    save_initial_chats_list_cursor_if_absent(storage, next_link)?;

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
    let (messages, next_link) = graph.list_messages_page(&target, 50, None).await?;
    storage.upsert_messages(&messages)?;
    save_initial_history_cursor_if_absent(storage, &conv_key, &messages, next_link)?;
    let _ = event_tx.send(CoreEvent::MessagesUpdated {
        conv_key,
        parent_msg_id: None,
        messages,
        notify_candidate: None,
    });
    Ok(())
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
    use crate::auth::{AuthManager, StoredTokens};

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

    fn sample_msg(conv_key: &str, msg_id: &str, created_at: &str) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u2".to_string()),
            author_name: "홍길동".to_string(),
            is_me: false,
            created_at: created_at.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: "<p>메시지</p>".to_string(),
            body_plain: "메시지".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
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

        let mut msg_other = sample_msg("chat:c1", "m2", "2026-11-15T10:05:00Z");
        msg_other.body_plain = "새\n메시지".to_string();
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
        ctrl.request(SyncCommand::RefreshConversations);
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

        // Wait briefly for Background Poll Lane to emit SyncStateChanged and Fast Lane to process
        let mut saw_conv_refresh_state = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await {
                Ok(Some(CoreEvent::SyncStateChanged { summary, .. }))
                    if summary.contains("대화 목록") =>
                {
                    saw_conv_refresh_state = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_conv_refresh_state);

        // Dropping controller closes nudge_tx + focus_tx and shuts down both lanes cleanly
        drop(ctrl);
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "both sync lanes should terminate when controller is dropped");
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
        let state = SyncSharedState::with_epoch(epoch);
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

    #[tokio::test]
    async fn test_sync_controller_activity_and_shutdown_cleanly() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);
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

    #[test]
    fn test_initial_history_and_chats_list_cursor_preservation() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chat:c1";
        let msgs = vec![
            sample_msg(conv_key, "m1", "2026-11-15T09:00:00Z"),
            sample_msg(conv_key, "m2", "2026-11-15T10:00:00Z"),
        ];

        // 1) Initial save with nextLink records delta_link + oldest_msg_ts
        let next_url = "https://graph.microsoft.com/v1.0/chats/c1/messages?$skiptoken=page2";
        save_initial_history_cursor_if_absent(
            &storage,
            conv_key,
            &msgs,
            Some(next_url.to_string()),
        )
        .unwrap();
        let saved = storage
            .get_sync_cursor(&SyncCursor::history_scope(conv_key))
            .unwrap()
            .unwrap();
        assert_eq!(saved.delta_link.as_deref(), Some(next_url));
        assert_eq!(
            saved.oldest_msg_ts.as_deref(),
            Some("2026-11-15T09:00:00Z")
        );
        assert!(!saved.is_history_eof());

        // 2) Subsequent sync must NOT overwrite an existing deeper history cursor
        let deeper_cursor = SyncCursor {
            scope_key: SyncCursor::history_scope(conv_key),
            delta_link: Some("https://graph.microsoft.com/v1.0/chats/c1/messages?$skiptoken=page3".to_string()),
            oldest_msg_ts: Some("2026-11-10T00:00:00Z".to_string()),
            synced_at: "2026-11-15T10:05:00Z".to_string(),
        };
        storage.save_sync_cursor(&deeper_cursor).unwrap();
        save_initial_history_cursor_if_absent(&storage, conv_key, &msgs, None).unwrap();
        let preserved = storage
            .get_sync_cursor(&SyncCursor::history_scope(conv_key))
            .unwrap()
            .unwrap();
        assert_eq!(preserved.delta_link, deeper_cursor.delta_link);
        assert_eq!(preserved.oldest_msg_ts, deeper_cursor.oldest_msg_ts);

        // 3) Conversation with no nextLink records HISTORY_EOF immediately
        let eof_key = "chat:c-eof";
        save_initial_history_cursor_if_absent(&storage, eof_key, &msgs, None).unwrap();
        let eof_cursor = storage
            .get_sync_cursor(&SyncCursor::history_scope(eof_key))
            .unwrap()
            .unwrap();
        assert!(eof_cursor.is_history_eof());
        assert_eq!(
            eof_cursor.oldest_msg_ts.as_deref(),
            Some(SyncCursor::HISTORY_EOF)
        );

        // 4) CHATS_LIST_SCOPE saves initial next_link and does not overwrite existing cursor
        let chats_next = "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=chats2";
        save_initial_chats_list_cursor_if_absent(&storage, Some(chats_next.to_string())).unwrap();
        let chats_cursor = storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .unwrap()
            .unwrap();
        assert_eq!(chats_cursor.delta_link.as_deref(), Some(chats_next));
        assert!(!chats_cursor.is_history_eof());

        save_initial_chats_list_cursor_if_absent(&storage, None).unwrap();
        let chats_preserved = storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .unwrap()
            .unwrap();
        assert_eq!(chats_preserved.delta_link.as_deref(), Some(chats_next));
    }

    #[tokio::test]
    async fn test_sync_active_focus_and_channel_history_record_history_cursor() {
        use axum::{routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");

        let app = Router::new()
            .route(
                "/v1.0/chats/c1/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "m1",
                            "createdDateTime": "2026-11-16T09:00:00Z",
                            "messageType": "message",
                            "importance": "normal",
                            "from": { "user": { "id": "u2", "displayName": "홍길동" } },
                            "body": { "contentType": "text", "content": "안녕" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "cm1",
                            "createdDateTime": "2026-11-16T09:05:00Z",
                            "messageType": "message",
                            "importance": "normal",
                            "from": { "user": { "id": "u2", "displayName": "홍길동" } },
                            "body": { "contentType": "text", "content": "채널 글" }
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
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        let focus = FocusTarget {
            target: Some(ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            }),
            thread_root_id: None,
        };
        sync_active_focus(&focus, &graph, &storage, &event_tx)
            .await
            .unwrap();
        assert!(storage
            .get_sync_cursor(&SyncCursor::history_scope("chat:c1"))
            .unwrap()
            .is_some());

        sync_channel_history("t1", "ch1", &graph, &storage, &event_tx)
            .await
            .unwrap();
        assert!(storage
            .get_sync_cursor(&SyncCursor::history_scope("chan:t1:ch1"))
            .unwrap()
            .is_some());

        server.abort();
    }
}
