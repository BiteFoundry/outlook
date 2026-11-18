//! Adaptive two-lane background synchronization engine for Microsoft Outlook
//! (Lane 1: Fast Interactive Lane + Lane 2: Quiet Background Poll Lane).

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, watch},
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{
        CoreEvent, MailFolder, MailMessage, NotificationCandidate, SyncCursor, TodoList,
        POLL_RETRY_SUMMARY,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
const HISTORY_EOF_MARKER: &str = "__eof__";
const INBOX_PREFETCH_UNREAD_LIMIT: usize = 5;

pub(crate) struct SyncSharedState {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
    auth_pause_until_ms: AtomicU64,
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
            auth_pause_until_ms: AtomicU64::new(0),
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

    pub(crate) fn pause_auth(&self, now: Instant, duration: Duration) {
        let until_ms = (now.saturating_duration_since(self.epoch) + duration).as_millis() as u64;
        self.auth_pause_until_ms
            .store(until_ms.max(1), Ordering::Release);
    }

    pub(crate) fn clear_auth_pause(&self) {
        self.auth_pause_until_ms.store(0, Ordering::Release);
    }

    pub(crate) fn is_auth_paused(&self, now: Instant) -> bool {
        let until_ms = self.auth_pause_until_ms.load(Ordering::Acquire);
        if until_ms == 0 {
            return false;
        }
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        now_ms < until_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncErrorKind {
    Forbidden403,
    Unauthorized401,
    Transient,
}

fn extract_error_detail_for_status(lower: &str) -> &str {
    if let Some(idx) = lower.find("graph api error (") {
        let rest = &lower[idx..];
        if let Some((_, tail)) = rest.split_once("): ") {
            return tail;
        }
        return rest;
    }
    lower
}

pub(crate) fn classify_sync_error(err: &anyhow::Error) -> SyncErrorKind {
    let lower = format!("{err:#}").to_ascii_lowercase();
    let detail = extract_error_detail_for_status(&lower);

    if detail.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient")
        || lower.contains("erroraccessdenied")
        || lower.contains("aclcheckfailed")
    {
        return SyncErrorKind::Forbidden403;
    }

    if detail.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("token expired")
    {
        return SyncErrorKind::Unauthorized401;
    }

    SyncErrorKind::Transient
}

pub(crate) fn is_expired_delta_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    let detail = extract_error_detail_for_status(&lower);
    detail.contains("410")
        || detail.contains("400")
        || lower.contains("resyncrequired")
        || lower.contains("syncstatenotfound")
        || lower.contains("invalidskiptoken")
        || lower.contains("skiptoken")
}

#[derive(Debug)]
pub(crate) struct ErrorBackoffState {
    base_ms: u64,
    consecutive_failures: u32,
    next_retry_at: Option<Instant>,
    degraded: bool,
}

impl ErrorBackoffState {
    pub(crate) fn new(base_ms: u64) -> Self {
        Self {
            base_ms: base_ms.max(1),
            consecutive_failures: 0,
            next_retry_at: None,
            degraded: false,
        }
    }

    pub(crate) fn compute_backoff_ms(base_ms: u64, failures: u32, jitter_factor: f64) -> u64 {
        let shift = failures.min(6);
        let raw_ms = base_ms
            .saturating_mul(1u64 << shift)
            .min(MAX_ERROR_BACKOFF_MS);
        let jittered = (raw_ms as f64 * jitter_factor).round() as u64;
        jittered.clamp(1, MAX_ERROR_BACKOFF_MS)
    }

    /// Records a transient failure and returns `true` iff this transitions into degraded state.
    pub(crate) fn record_transient_failure(&mut self, now: Instant) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let jitter = rand::thread_rng().gen_range(0.8_f64..=1.2_f64);
        let delay_ms = Self::compute_backoff_ms(self.base_ms, self.consecutive_failures, jitter);
        self.next_retry_at = Some(now + Duration::from_millis(delay_ms));
        if !self.degraded {
            self.degraded = true;
            true
        } else {
            false
        }
    }

    /// Resets failure counter and returns `true` iff this transitions out of degraded state.
    pub(crate) fn record_success(&mut self) -> bool {
        self.consecutive_failures = 0;
        self.next_retry_at = None;
        if self.degraded {
            self.degraded = false;
            true
        } else {
            false
        }
    }

    pub(crate) fn reset_window(&mut self) {
        self.consecutive_failures = 0;
        self.next_retry_at = None;
    }

    pub(crate) fn is_backed_off(&self, now: Instant) -> bool {
        self.next_retry_at.is_some_and(|until| now < until)
    }

    pub(crate) fn is_degraded(&self) -> bool {
        self.degraded
    }
}

/// Per-capability `403 Forbidden` circuit-breakers keyed by token fingerprint so missing
/// optional scopes (`Calendars.*`, `Tasks.*`, `Contacts.*`) stop hitting Graph silently.
#[derive(Debug, Default, Clone)]
pub(crate) struct CapabilityCircuitBreakers {
    pub calendar_disabled: Option<String>,
    pub todo_disabled: Option<String>,
    pub contacts_disabled: Option<String>,
}

impl CapabilityCircuitBreakers {
    #[cfg(test)]
    pub(crate) fn is_calendar_disabled(&self, token_fp: &str) -> bool {
        self.calendar_disabled.as_deref() == Some(token_fp)
    }

    #[cfg(test)]
    pub(crate) fn is_todo_disabled(&self, token_fp: &str) -> bool {
        self.todo_disabled.as_deref() == Some(token_fp)
    }

    pub(crate) fn reset_all(&mut self) {
        self.calendar_disabled = None;
        self.todo_disabled = None;
        self.contacts_disabled = None;
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub active_folder_id: Option<String>,
    pub active_message_id: Option<String>,
    pub active_todo_list_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncCommand {
    Bootstrap,
    RefreshAll,
    SyncFolder { folder_id: String },
    LoadOlderMail { folder_id: String, explicit: bool },
    HydrateMessage { message_id: String },
    SyncCalendar,
    SyncTodo { list_id: Option<String> },
    SyncContacts,
    NotifyUserActivity,
    Shutdown,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    shared: Arc<SyncSharedState>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    ) -> Self {
        Self {
            focus_tx,
            nudge_tx,
            shared: Arc::new(SyncSharedState::new()),
            shutdown_tx: None,
        }
    }

    pub fn set_focus(&self, focus: FocusTarget) {
        self.shared.record_activity(Instant::now());
        let _ = self.focus_tx.send(focus);
    }

    pub fn request(&self, cmd: SyncCommand) {
        if matches!(cmd, SyncCommand::Shutdown) {
            if let Some(tx) = &self.shutdown_tx {
                let _ = tx.send(true);
            }
        }
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

async fn run_storage<F, T>(storage: &Storage, f: F) -> Result<T>
where
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let st = storage.clone();
    tokio::task::spawn_blocking(move || f(&st))
        .await
        .map_err(|e| anyhow::anyhow!("Storage blocking task failed: {e}"))?
}

async fn current_token_fingerprint(graph: &GraphClient) -> String {
    match graph.auth().current_tokens().await {
        Some(tokens) => {
            let mut hasher = Sha256::new();
            hasher.update(tokens.access_token.as_bytes());
            hex::encode(hasher.finalize())
        }
        None => String::new(),
    }
}

pub(crate) fn default_calendar_window() -> (String, String) {
    let now = chrono::Utc::now();
    let start = (now - chrono::Duration::days(7)).to_rfc3339();
    let end = (now + chrono::Duration::days(30)).to_rfc3339();
    (start, end)
}

fn pick_default_todo_list(lists: &[TodoList]) -> Option<&TodoList> {
    lists
        .iter()
        .find(|l| {
            l.wellknown_list_name
                .as_deref()
                .is_some_and(|w| w.eq_ignore_ascii_case("defaultList"))
        })
        .or_else(|| lists.first())
}

/// Performs initial Outlook bootstrap sync (profile, mail folders, inbox messages,
/// calendar window `now - 7d .. now + 30d`, contacts, and To Do lists/tasks) into SQLite.
/// Best-effort per optional capability so missing scopes never abort bootstrap.
pub async fn bootstrap_outlook(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let (start_iso, end_iso) = default_calendar_window();
    let (me_res, folders_res, inbox_res, events_res, contacts_res, lists_res) = tokio::join!(
        graph.get_me(),
        graph.list_mail_folders(),
        graph.list_mail_messages("inbox", 50, true),
        graph.list_calendar_view(&start_iso, &end_iso, 100),
        graph.list_contacts(100),
        graph.list_todo_lists(),
    );

    let _ = me_res;

    if let Ok(folders) = folders_res {
        run_storage(storage, move |st| st.upsert_mail_folders(&folders)).await?;
    }

    let inbox_msgs = inbox_res?;
    if !inbox_msgs.is_empty() {
        run_storage(storage, move |st| {
            st.upsert_mail_messages(&inbox_msgs)?;
            Ok(())
        })
        .await?;
    }

    if let Ok(events) = events_res {
        let _ = run_storage(storage, move |st| st.upsert_calendar_events(&events)).await;
    }

    if let Ok(contacts) = contacts_res {
        let _ = run_storage(storage, move |st| st.upsert_contacts(&contacts)).await;
    }

    if let Ok(lists) = lists_res {
        let default_list_id = pick_default_todo_list(&lists).map(|l| l.id.clone());
        let _ = run_storage(storage, move |st| st.upsert_todo_lists(&lists)).await;
        if let Some(list_id) = default_list_id {
            if let Ok(tasks) = graph.list_todo_tasks(&list_id, true, 100).await {
                let _ = run_storage(storage, move |st| st.upsert_todo_tasks(&tasks)).await;
            }
        }
    }

    let _ = prefetch_unread_bodies("inbox", INBOX_PREFETCH_UNREAD_LIMIT, graph, storage, None).await;
    Ok(())
}

/// Spawns the quiet two-lane background sync engine (Fast Interactive Lane + Background Poll Lane)
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
    let mail_ms = config.mail_poll_ms.max(500);
    let calendar_ms = config.calendar_poll_ms.max(1_000);
    let shared = Arc::new(SyncSharedState::new());

    let ctrl_shutdown_tx = shutdown_tx.clone();
    // Lane 1: Fast Interactive Lane (message hydration, active folder delta, folder pagination)
    let fast_graph = graph.clone();
    let fast_storage = storage.clone();
    let fast_event_tx = event_tx.clone();
    let mut fast_shutdown_rx = shutdown_rx.clone();
    let fast_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut active_tick = time::interval(Duration::from_millis(active_ms));
        active_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut active_backoff = PeriodicBackoffTracker::default();
        let mut active_err_backoff = ErrorBackoffState::new(active_ms);
        let mut restricted_folders: HashSet<String> = HashSet::new();
        let mut prev_focus = FocusTarget::default();

        // Consume initial immediate tick so startup waits for explicit bootstrap/focus.
        active_tick.tick().await;

        loop {
            if *fast_shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                biased;
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
                            cmd @ (SyncCommand::Bootstrap
                            | SyncCommand::RefreshAll
                            | SyncCommand::SyncCalendar
                            | SyncCommand::SyncTodo { .. }
                            | SyncCommand::SyncContacts),
                        ) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            if matches!(cmd, SyncCommand::RefreshAll) {
                                restricted_folders.clear();
                                active_err_backoff.reset_window();
                            }
                            let _ = bg_tx.send(cmd);
                        }
                        Some(SyncCommand::HydrateMessage { message_id }) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            match hydrate_message_if_needed(&message_id, true, &fast_graph, &fast_storage, &fast_event_tx).await {
                                Ok(()) => emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx),
                                Err(err) => {
                                    handle_quiet_mail_error(
                                        &err,
                                        None,
                                        &fast_shared,
                                        &mut restricted_folders,
                                        &mut active_err_backoff,
                                        &fast_event_tx,
                                    );
                                }
                            }
                        }
                        Some(SyncCommand::SyncFolder { folder_id }) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            restricted_folders.remove(&folder_id);
                            active_err_backoff.reset_window();
                            match sync_mail_folder(&folder_id, &fast_graph, &fast_storage, &fast_event_tx, false).await {
                                Ok(()) => {
                                    emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                    let _ = prefetch_unread_bodies(
                                        &folder_id,
                                        INBOX_PREFETCH_UNREAD_LIMIT,
                                        &fast_graph,
                                        &fast_storage,
                                        Some(&fast_event_tx),
                                    )
                                    .await;
                                }
                                Err(err) => {
                                    handle_quiet_mail_error(
                                        &err,
                                        Some(&folder_id),
                                        &fast_shared,
                                        &mut restricted_folders,
                                        &mut active_err_backoff,
                                        &fast_event_tx,
                                    );
                                }
                            }
                            active_tick.reset();
                        }
                        Some(SyncCommand::LoadOlderMail { folder_id, explicit }) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            if !restricted_folders.contains(&folder_id) {
                                match load_older_mail_page(&folder_id, explicit, &fast_graph, &fast_storage, &fast_event_tx).await {
                                    Ok(()) => emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx),
                                    Err(err) => {
                                        handle_quiet_mail_error(
                                            &err,
                                            Some(&folder_id),
                                            &fast_shared,
                                            &mut restricted_folders,
                                            &mut active_err_backoff,
                                            &fast_event_tx,
                                        );
                                    }
                                }
                            }
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
                    let now = Instant::now();
                    fast_shared.record_activity(now);
                    let focus = focus_rx.borrow_and_update().clone();
                    let folder_changed = focus.active_folder_id != prev_focus.active_folder_id;
                    let msg_changed = focus.active_message_id != prev_focus.active_message_id;
                    let todo_changed = focus.active_todo_list_id != prev_focus.active_todo_list_id;
                    prev_focus = focus.clone();

                    if !fast_shared.is_auth_paused(now) {
                        if let (true, Some(mid)) = (msg_changed, focus.active_message_id.as_deref()) {
                            let _ = hydrate_message_if_needed(
                                mid,
                                false,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                            )
                            .await;
                        }
                        if let (true, Some(fid)) = (folder_changed, focus.active_folder_id.as_deref()) {
                            if !restricted_folders.contains(fid) {
                                match sync_mail_folder(fid, &fast_graph, &fast_storage, &fast_event_tx, false).await {
                                    Ok(()) => {
                                        emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                        let _ = prefetch_unread_bodies(
                                            fid,
                                            INBOX_PREFETCH_UNREAD_LIMIT,
                                            &fast_graph,
                                            &fast_storage,
                                            Some(&fast_event_tx),
                                        )
                                        .await;
                                    }
                                    Err(err) => {
                                        handle_quiet_mail_error(
                                            &err,
                                            Some(fid),
                                            &fast_shared,
                                            &mut restricted_folders,
                                            &mut active_err_backoff,
                                            &fast_event_tx,
                                        );
                                    }
                                }
                            }
                        }
                        if let (true, Some(lid)) = (todo_changed, focus.active_todo_list_id.as_deref()) {
                            let _ = bg_tx.send(SyncCommand::SyncTodo {
                                list_id: Some(lid.to_string()),
                            });
                        }
                    }
                    active_tick.reset();
                }
                _ = active_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = fast_shared.idle_snapshot(now);
                    if active_backoff.on_tick(idle, seq)
                        && !fast_shared.is_auth_paused(now)
                        && !active_err_backoff.is_backed_off(now)
                    {
                        let focus = focus_rx.borrow().clone();
                        if let Some(fid) = focus.active_folder_id.as_deref() {
                            if !restricted_folders.contains(fid) {
                                match sync_mail_folder(fid, &fast_graph, &fast_storage, &fast_event_tx, true).await {
                                    Ok(()) => {
                                        emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                        let _ = prefetch_unread_bodies(
                                            fid,
                                            INBOX_PREFETCH_UNREAD_LIMIT,
                                            &fast_graph,
                                            &fast_storage,
                                            Some(&fast_event_tx),
                                        )
                                        .await;
                                    }
                                    Err(err) => {
                                        handle_quiet_mail_error(
                                            &err,
                                            Some(fid),
                                            &fast_shared,
                                            &mut restricted_folders,
                                            &mut active_err_backoff,
                                            &fast_event_tx,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    // Lane 2: Quiet Background Poll Lane (folders + Inbox every 15s; Calendar + To Do + Contacts every 60s)
    let bg_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut mail_tick = time::interval(Duration::from_millis(mail_ms));
        mail_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut slow_tick = time::interval(Duration::from_millis(calendar_ms));
        slow_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut mail_backoff = PeriodicBackoffTracker::default();
        let mut slow_backoff = PeriodicBackoffTracker::default();
        let mut mail_err_backoff = ErrorBackoffState::new(mail_ms);
        let mut slow_err_backoff = ErrorBackoffState::new(calendar_ms);
        let mut breakers = CapabilityCircuitBreakers::default();
        let mut last_folders: Option<Vec<MailFolder>> = None;
        let mut was_idle = false;

        mail_tick.tick().await;
        slow_tick.tick().await;

        let mut shutdown_rx = shutdown_rx;
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                cmd = bg_rx.recv() => {
                    match cmd {
                        None => break,
                        Some(SyncCommand::Bootstrap) => {
                            bg_shared.clear_auth_pause();
                            breakers.reset_all();
                            mail_err_backoff.reset_window();
                            slow_err_backoff.reset_window();
                            run_bootstrap_and_emit(
                                &graph,
                                &storage,
                                &event_tx,
                                &bg_shared,
                                &mut mail_err_backoff,
                                &mut breakers,
                                &mut last_folders,
                            )
                            .await;
                        }
                        Some(SyncCommand::RefreshAll) => {
                            bg_shared.clear_auth_pause();
                            breakers.reset_all();
                            mail_err_backoff.reset_window();
                            slow_err_backoff.reset_window();
                            run_refresh_all_and_emit(
                                &graph,
                                &storage,
                                &event_tx,
                                &bg_shared,
                                &mut mail_err_backoff,
                                &mut slow_err_backoff,
                                &mut breakers,
                                &mut last_folders,
                            )
                            .await;
                        }
                        Some(SyncCommand::SyncCalendar) => {
                            let token_fp = current_token_fingerprint(&graph).await;
                            breakers.calendar_disabled = None;
                            poll_calendar_quiet(
                                &graph,
                                &storage,
                                &event_tx,
                                &bg_shared,
                                &mut slow_err_backoff,
                                &mut breakers,
                                &token_fp,
                            )
                            .await;
                        }
                        Some(SyncCommand::SyncTodo { list_id }) => {
                            let token_fp = current_token_fingerprint(&graph).await;
                            breakers.todo_disabled = None;
                            poll_todo_quiet(
                                list_id.as_deref(),
                                &graph,
                                &storage,
                                &event_tx,
                                &bg_shared,
                                &mut slow_err_backoff,
                                &mut breakers,
                                &token_fp,
                            )
                            .await;
                        }
                        Some(SyncCommand::SyncContacts) => {
                            let token_fp = current_token_fingerprint(&graph).await;
                            breakers.contacts_disabled = None;
                            poll_contacts_quiet(
                                &graph,
                                &storage,
                                &event_tx,
                                &bg_shared,
                                &mut slow_err_backoff,
                                &mut breakers,
                                &token_fp,
                            )
                            .await;
                        }
                        Some(_) => {}
                    }
                }
                _ = mail_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = bg_shared.idle_snapshot(now);
                    maybe_shrink_on_idle_async(idle, &mut was_idle, &storage);
                    if mail_backoff.on_tick(idle, seq)
                        && !bg_shared.is_auth_paused(now)
                        && !mail_err_backoff.is_backed_off(now)
                    {
                        poll_mail_lane2_quiet(
                            &graph,
                            &storage,
                            &event_tx,
                            &bg_shared,
                            &mut mail_err_backoff,
                            &slow_err_backoff,
                            &mut last_folders,
                        )
                        .await;
                    }
                }
                _ = slow_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = bg_shared.idle_snapshot(now);
                    maybe_shrink_on_idle_async(idle, &mut was_idle, &storage);
                    if idle < Duration::from_secs(IDLE_THRESHOLD_SECS)
                        && slow_backoff.on_tick(idle, seq)
                        && !bg_shared.is_auth_paused(now)
                        && !slow_err_backoff.is_backed_off(now)
                    {
                        let token_fp = current_token_fingerprint(&graph).await;
                        poll_calendar_quiet(
                            &graph,
                            &storage,
                            &event_tx,
                            &bg_shared,
                            &mut slow_err_backoff,
                            &mut breakers,
                            &token_fp,
                        )
                        .await;
                        poll_todo_quiet(
                            None,
                            &graph,
                            &storage,
                            &event_tx,
                            &bg_shared,
                            &mut slow_err_backoff,
                            &mut breakers,
                            &token_fp,
                        )
                        .await;
                        poll_contacts_quiet(
                            &graph,
                            &storage,
                            &event_tx,
                            &bg_shared,
                            &mut slow_err_backoff,
                            &mut breakers,
                            &token_fp,
                        )
                        .await;
                    }
                }
            }
        }
    });

    SyncController {
        focus_tx,
        nudge_tx,
        shared,
        shutdown_tx: Some(ctrl_shutdown_tx),
    }
}

fn emit_recovered_if_needed(
    err_backoff: &mut ErrorBackoffState,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    if err_backoff.record_success() {
        let _ = event_tx.send(CoreEvent::SyncStateChanged {
            busy: false,
            summary: "Synced".to_string(),
        });
    }
}

fn handle_quiet_mail_error(
    err: &anyhow::Error,
    folder_id: Option<&str>,
    shared: &SyncSharedState,
    restricted_folders: &mut HashSet<String>,
    err_backoff: &mut ErrorBackoffState,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    match classify_sync_error(err) {
        SyncErrorKind::Forbidden403 => {
            if let Some(fid) = folder_id {
                restricted_folders.insert(fid.to_string());
            }
        }
        SyncErrorKind::Unauthorized401 => {
            shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
        }
        SyncErrorKind::Transient => {
            if err_backoff.record_transient_failure(Instant::now()) {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: POLL_RETRY_SUMMARY.to_string(),
                });
            }
        }
    }
}

async fn hydrate_message_if_needed(
    message_id: &str,
    force: bool,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let trimmed = message_id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if !force {
        let id_owned = trimmed.to_string();
        if let Ok(Some(cached)) = run_storage(storage, move |st| st.get_mail_message(&id_owned)).await {
            if cached.body_loaded {
                return Ok(());
            }
        }
    }

    let mut full = graph.get_mail_message(trimmed).await?;
    full.body_loaded = true;
    let for_db = full.clone();
    run_storage(storage, move |st| {
        st.upsert_mail_messages(&[for_db])?;
        Ok(())
    })
    .await?;
    let _ = event_tx.send(CoreEvent::MailMessageHydrated(Box::new(full)));
    Ok(())
}

async fn prefetch_unread_bodies(
    folder_id: &str,
    max_count: usize,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<usize> {
    let fid = folder_id.to_string();
    let unread_ids: Vec<String> = run_storage(storage, move |st| {
        let msgs = st.list_mail_messages(Some(&fid), true, false, false, max_count.saturating_mul(4).max(10))?;
        Ok(msgs
            .into_iter()
            .filter(|m| !m.body_loaded)
            .take(max_count)
            .map(|m| m.id)
            .collect())
    })
    .await?;

    if unread_ids.is_empty() {
        return Ok(0);
    }

    let mut hydrated = graph.batch_get_mail_messages(&unread_ids).await?;
    for msg in &mut hydrated {
        msg.body_loaded = true;
    }
    if hydrated.is_empty() {
        return Ok(0);
    }

    let to_store = hydrated.clone();
    run_storage(storage, move |st| {
        st.upsert_mail_messages(&to_store)?;
        Ok(())
    })
    .await?;

    if let Some(tx) = event_tx {
        for msg in hydrated.iter() {
            let _ = tx.send(CoreEvent::MailMessageHydrated(Box::new(msg.clone())));
        }
    }
    Ok(hydrated.len())
}

async fn load_older_mail_page(
    folder_id: &str,
    explicit: bool,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let scope_key = format!("mail_history:{folder_id}");
    let scope_clone = scope_key.clone();
    let cursor = run_storage(storage, move |st| st.get_sync_cursor(&scope_clone)).await?;
    let existing_next = cursor.and_then(|c| c.history_next_link);

    if existing_next.as_deref() == Some(HISTORY_EOF_MARKER) && !explicit {
        return Ok(());
    }

    let mut next_arg: Option<String> = existing_next
        .as_deref()
        .filter(|s| *s != HISTORY_EOF_MARKER)
        .map(str::to_string);

    let mut total_changed = 0usize;
    let mut final_next_link: Option<String> = None;

    for _ in 0..5 {
        let (messages, next_link) = match graph
            .list_mail_messages_page(folder_id, 50, next_arg.as_deref(), true)
            .await
        {
            Ok(res) => res,
            Err(err) if next_arg.is_some() && is_expired_delta_error(&err) => {
                graph
                    .list_mail_messages_page(folder_id, 50, None, true)
                    .await?
            }
            Err(err) => return Err(err),
        };

        let msgs_for_db = messages;
        let changed = run_storage(storage, move |st| st.upsert_mail_messages(&msgs_for_db)).await?;
        total_changed += changed;
        final_next_link = next_link.clone();
        if changed > 0 || next_link.is_none() {
            break;
        }
        next_arg = next_link;
    }

    let fid_owned = folder_id.to_string();
    let stored_list = run_storage(storage, move |st| {
        st.upsert_sync_cursor(&SyncCursor {
            scope_key,
            delta_link: None,
            history_next_link: final_next_link.or_else(|| Some(HISTORY_EOF_MARKER.to_string())),
            last_synced_at: chrono::Utc::now().to_rfc3339(),
        })?;
        st.list_mail_messages(Some(&fid_owned), false, false, false, 500)
    })
    .await?;

    if total_changed > 0 || explicit {
        let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
            folder_id: folder_id.to_string(),
            messages: stored_list,
            notify_candidate: None,
        });
    }
    Ok(())
}

async fn sync_mail_folder(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    is_periodic: bool,
) -> Result<()> {
    let scope_key = format!("mail_delta:{folder_id}");
    let scope_for_read = scope_key.clone();
    let fid_for_read = folder_id.to_string();
    let (prev_cursor, existing_count) = run_storage(storage, move |st| {
        let cursor = st.get_sync_cursor(&scope_for_read)?;
        let summaries = st.list_mail_summaries(Some(&fid_for_read), 1)?;
        Ok((cursor, summaries.len()))
    })
    .await?;

    let had_initial = prev_cursor.is_some() || existing_count > 0;
    let delta_res = graph
        .delta_mail_messages(
            folder_id,
            prev_cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
            50,
        )
        .await;

    let (incoming, deleted_ids, new_delta_link) = match delta_res {
        Ok(res) => res,
        Err(err) if is_expired_delta_error(&err) => {
            // Clear stale delta token and fall back to page listing cleanly.
            let stale_scope = scope_key.clone();
            let hist_link = prev_cursor
                .as_ref()
                .and_then(|c| c.history_next_link.clone());
            let _ = run_storage(storage, move |st| {
                st.upsert_sync_cursor(&SyncCursor {
                    scope_key: stale_scope,
                    delta_link: None,
                    history_next_link: hist_link,
                    last_synced_at: chrono::Utc::now().to_rfc3339(),
                })
            })
            .await;

            let (page_msgs, next_link) = graph
                .list_mail_messages_page(folder_id, 50, None, true)
                .await?;
            let fid_clone = folder_id.to_string();
            let hist_scope = format!("mail_history:{folder_id}");
            let (changed_rows, notify_candidate, stored_msgs) = run_storage(storage, move |st| {
                let notify = if had_initial {
                    find_new_unread_mail_candidate(&fid_clone, st, &page_msgs)
                } else {
                    None
                };
                let changed = st.upsert_mail_messages(&page_msgs)?;
                if st.get_sync_cursor(&hist_scope)?.is_none() {
                    st.upsert_sync_cursor(&SyncCursor {
                        scope_key: hist_scope,
                        delta_link: None,
                        history_next_link: next_link.or_else(|| Some(HISTORY_EOF_MARKER.to_string())),
                        last_synced_at: chrono::Utc::now().to_rfc3339(),
                    })?;
                }
                let list = st.list_mail_messages(Some(&fid_clone), false, false, false, 100)?;
                Ok((changed, notify, list))
            })
            .await?;

            if !is_periodic || changed_rows > 0 {
                let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
                    folder_id: folder_id.to_string(),
                    messages: stored_msgs,
                    notify_candidate,
                });
            }
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    let fid_clone = folder_id.to_string();
    let effective_delta = new_delta_link
        .or_else(|| prev_cursor.as_ref().and_then(|c| c.delta_link.clone()));
    let hist_link = prev_cursor.and_then(|c| c.history_next_link);
    let had_deleted = !deleted_ids.is_empty();

    let (changed_rows, notify_candidate, stored_msgs) = run_storage(storage, move |st| {
        let notify = if had_initial {
            find_new_unread_mail_candidate(&fid_clone, st, &incoming)
        } else {
            None
        };
        if !deleted_ids.is_empty() {
            st.delete_mail_messages_by_ids(&deleted_ids)?;
        }
        let changed = st.upsert_mail_messages(&incoming)?;
        st.upsert_sync_cursor(&SyncCursor {
            scope_key,
            delta_link: effective_delta,
            history_next_link: hist_link,
            last_synced_at: chrono::Utc::now().to_rfc3339(),
        })?;
        let list = if changed > 0 || had_deleted || !is_periodic {
            st.list_mail_messages(Some(&fid_clone), false, false, false, 100)?
        } else {
            Vec::new()
        };
        Ok((changed, notify, list))
    })
    .await?;

    if !is_periodic || changed_rows > 0 || had_deleted {
        let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
            folder_id: folder_id.to_string(),
            messages: stored_msgs,
            notify_candidate,
        });
    }
    Ok(())
}

async fn poll_mail_folders_quiet(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_folders: &mut Option<Vec<MailFolder>>,
) -> Result<()> {
    let folders = graph.list_mail_folders().await?;
    let changed = last_folders.as_ref() != Some(&folders);
    if changed {
        let to_store = folders.clone();
        run_storage(storage, move |st| st.upsert_mail_folders(&to_store)).await?;
        *last_folders = Some(folders.clone());
        let _ = event_tx.send(CoreEvent::MailFoldersUpdated(folders));
    }
    Ok(())
}

async fn poll_mail_lane2_quiet(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    mail_err_backoff: &mut ErrorBackoffState,
    slow_err_backoff: &ErrorBackoffState,
    last_folders: &mut Option<Vec<MailFolder>>,
) {
    let (folders_res, inbox_res) = tokio::join!(
        poll_mail_folders_quiet(graph, storage, event_tx, last_folders),
        sync_mail_folder("inbox", graph, storage, event_tx, true),
    );

    if let Err(err) = folders_res.and(inbox_res) {
        match classify_sync_error(&err) {
            SyncErrorKind::Unauthorized401 => {
                shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            }
            SyncErrorKind::Forbidden403 => {
                mail_err_backoff.record_transient_failure(Instant::now());
            }
            SyncErrorKind::Transient => {
                let was_degraded =
                    mail_err_backoff.is_degraded() || slow_err_backoff.is_degraded();
                mail_err_backoff.record_transient_failure(Instant::now());
                let now_degraded =
                    mail_err_backoff.is_degraded() || slow_err_backoff.is_degraded();
                if !was_degraded && now_degraded {
                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                        busy: false,
                        summary: POLL_RETRY_SUMMARY.to_string(),
                    });
                }
            }
        }
    } else {
        let was_degraded = mail_err_backoff.is_degraded() || slow_err_backoff.is_degraded();
        mail_err_backoff.record_success();
        let now_degraded = mail_err_backoff.is_degraded() || slow_err_backoff.is_degraded();
        if was_degraded && !now_degraded {
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Synced".to_string(),
            });
        }
        let _ = prefetch_unread_bodies(
            "inbox",
            INBOX_PREFETCH_UNREAD_LIMIT,
            graph,
            storage,
            Some(event_tx),
        )
        .await;
    }
}

pub(crate) async fn poll_calendar_quiet(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    err_backoff: &mut ErrorBackoffState,
    breakers: &mut CapabilityCircuitBreakers,
    token_fp: &str,
) {
    poll_calendar_with_flag(
        graph,
        storage,
        event_tx,
        shared,
        err_backoff,
        &mut breakers.calendar_disabled,
        token_fp,
    )
    .await;
}

async fn poll_calendar_with_flag(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    err_backoff: &mut ErrorBackoffState,
    calendar_disabled: &mut Option<String>,
    token_fp: &str,
) {
    if calendar_disabled.as_deref() == Some(token_fp) {
        return;
    }

    let (start_iso, end_iso) = default_calendar_window();
    let cursor = run_storage(storage, |st| st.get_sync_cursor("calendar_delta")).await.ok().flatten();

    let delta_res = graph
        .delta_calendar_view(
            &start_iso,
            &end_iso,
            cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
        )
        .await;

    let res = match delta_res {
        Ok((events, removed_ids, new_delta_link)) => {
            let effective_delta = new_delta_link
                .or_else(|| cursor.as_ref().and_then(|c| c.delta_link.clone()));
            let has_changes = !events.is_empty() || !removed_ids.is_empty();
            let stored = run_storage(storage, move |st| {
                if !removed_ids.is_empty() {
                    st.delete_calendar_events_by_ids(&removed_ids)?;
                }
                if !events.is_empty() {
                    st.upsert_calendar_events(&events)?;
                }
                st.upsert_sync_cursor(&SyncCursor {
                    scope_key: "calendar_delta".to_string(),
                    delta_link: effective_delta,
                    history_next_link: None,
                    last_synced_at: chrono::Utc::now().to_rfc3339(),
                })?;
                if has_changes {
                    st.list_calendar_events(Some(&start_iso), Some(&end_iso), 200)
                } else {
                    Ok(Vec::new())
                }
            })
            .await;
            if let Ok(list) = stored {
                if has_changes {
                    let _ = event_tx.send(CoreEvent::CalendarEventsUpdated(list));
                }
                Ok(())
            } else {
                stored.map(|_| ())
            }
        }
        Err(err) if is_expired_delta_error(&err) => {
            match graph.list_calendar_view(&start_iso, &end_iso, 100).await {
                Ok(events) => {
                    let to_store = events.clone();
                    let _ = run_storage(storage, move |st| {
                        st.upsert_sync_cursor(&SyncCursor {
                            scope_key: "calendar_delta".to_string(),
                            delta_link: None,
                            history_next_link: None,
                            last_synced_at: chrono::Utc::now().to_rfc3339(),
                        })?;
                        st.upsert_calendar_events(&to_store)
                    })
                    .await;
                    let _ = event_tx.send(CoreEvent::CalendarEventsUpdated(events));
                    Ok(())
                }
                Err(fallback_err) => Err(fallback_err),
            }
        }
        Err(err) => Err(err),
    };

    if let Err(err) = res {
        match classify_sync_error(&err) {
            SyncErrorKind::Forbidden403 => {
                *calendar_disabled = Some(token_fp.to_string());
            }
            SyncErrorKind::Unauthorized401 => {
                shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            }
            SyncErrorKind::Transient => {
                if err_backoff.record_transient_failure(Instant::now()) {
                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                        busy: false,
                        summary: POLL_RETRY_SUMMARY.to_string(),
                    });
                }
            }
        }
    } else {
        emit_recovered_if_needed(err_backoff, event_tx);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn poll_todo_quiet(
    target_list_id: Option<&str>,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    err_backoff: &mut ErrorBackoffState,
    breakers: &mut CapabilityCircuitBreakers,
    token_fp: &str,
) {
    poll_todo_with_flag(
        target_list_id,
        graph,
        storage,
        event_tx,
        shared,
        err_backoff,
        &mut breakers.todo_disabled,
        token_fp,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn poll_todo_with_flag(
    target_list_id: Option<&str>,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    err_backoff: &mut ErrorBackoffState,
    todo_disabled: &mut Option<String>,
    token_fp: &str,
) {
    if todo_disabled.as_deref() == Some(token_fp) {
        return;
    }

    let lists = match graph.list_todo_lists().await {
        Ok(l) => l,
        Err(err) => {
            match classify_sync_error(&err) {
                SyncErrorKind::Forbidden403 => {
                    *todo_disabled = Some(token_fp.to_string());
                }
                SyncErrorKind::Unauthorized401 => {
                    shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
                }
                SyncErrorKind::Transient => {
                    if err_backoff.record_transient_failure(Instant::now()) {
                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                            busy: false,
                            summary: POLL_RETRY_SUMMARY.to_string(),
                        });
                    }
                }
            }
            return;
        }
    };

    let chosen_list_id = target_list_id
        .map(str::to_string)
        .or_else(|| pick_default_todo_list(&lists).map(|l| l.id.clone()));

    let lists_for_db = lists.clone();
    if run_storage(storage, move |st| st.upsert_todo_lists(&lists_for_db))
        .await
        .is_ok()
    {
        let _ = event_tx.send(CoreEvent::TodoListsUpdated(lists));
    }

    if let Some(lid) = chosen_list_id {
        match graph.list_todo_tasks(&lid, true, 100).await {
            Ok(tasks) => {
                let tasks_for_db = tasks.clone();
                let _ = run_storage(storage, move |st| st.upsert_todo_tasks(&tasks_for_db)).await;
                let _ = event_tx.send(CoreEvent::TodoTasksUpdated {
                    list_id: lid,
                    tasks,
                });
                emit_recovered_if_needed(err_backoff, event_tx);
            }
            Err(err) => match classify_sync_error(&err) {
                SyncErrorKind::Forbidden403 => *todo_disabled = Some(token_fp.to_string()),
                SyncErrorKind::Unauthorized401 => {
                    shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
                }
                SyncErrorKind::Transient => {
                    if err_backoff.record_transient_failure(Instant::now()) {
                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                            busy: false,
                            summary: POLL_RETRY_SUMMARY.to_string(),
                        });
                    }
                }
            },
        }
    } else {
        emit_recovered_if_needed(err_backoff, event_tx);
    }
}

pub(crate) async fn poll_contacts_quiet(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    err_backoff: &mut ErrorBackoffState,
    breakers: &mut CapabilityCircuitBreakers,
    token_fp: &str,
) {
    poll_contacts_with_flag(
        graph,
        storage,
        event_tx,
        shared,
        err_backoff,
        &mut breakers.contacts_disabled,
        token_fp,
    )
    .await;
}

async fn poll_contacts_with_flag(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    err_backoff: &mut ErrorBackoffState,
    contacts_disabled: &mut Option<String>,
    token_fp: &str,
) {
    if contacts_disabled.as_deref() == Some(token_fp) {
        return;
    }

    match graph.list_contacts(100).await {
        Ok(contacts) => {
            let to_store = contacts.clone();
            if run_storage(storage, move |st| st.upsert_contacts(&to_store))
                .await
                .is_ok()
            {
                let _ = event_tx.send(CoreEvent::ContactsUpdated(contacts));
            }
            emit_recovered_if_needed(err_backoff, event_tx);
        }
        Err(err) => match classify_sync_error(&err) {
            SyncErrorKind::Forbidden403 => {
                *contacts_disabled = Some(token_fp.to_string());
            }
            SyncErrorKind::Unauthorized401 => {
                shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            }
            SyncErrorKind::Transient => {
                if err_backoff.record_transient_failure(Instant::now()) {
                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                        busy: false,
                        summary: POLL_RETRY_SUMMARY.to_string(),
                    });
                }
            }
        },
    }
}

async fn run_bootstrap_and_emit(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    mail_err_backoff: &mut ErrorBackoffState,
    breakers: &mut CapabilityCircuitBreakers,
    last_folders: &mut Option<Vec<MailFolder>>,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Syncing Outlook...".to_string(),
    });

    if let Ok(profile) = graph.get_me().await {
        let _ = event_tx.send(CoreEvent::ProfileLoaded(profile));
    }

    let token_fp = current_token_fingerprint(graph).await;
    let (folders_res, inbox_res) = tokio::join!(
        poll_mail_folders_quiet(graph, storage, event_tx, last_folders),
        sync_mail_folder("inbox", graph, storage, event_tx, false),
    );

    let mail_ok = match folders_res.and(inbox_res) {
        Ok(()) => {
            mail_err_backoff.record_success();
            let _ = prefetch_unread_bodies(
                "inbox",
                INBOX_PREFETCH_UNREAD_LIMIT,
                graph,
                storage,
                Some(event_tx),
            )
            .await;
            true
        }
        Err(err) => {
            if classify_sync_error(&err) == SyncErrorKind::Unauthorized401 {
                shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            } else {
                mail_err_backoff.record_transient_failure(Instant::now());
            }
            false
        }
    };

    let mut cal_backoff = ErrorBackoffState::new(60_000);
    let mut todo_backoff = ErrorBackoffState::new(60_000);
    let mut contacts_backoff = ErrorBackoffState::new(60_000);
    let mut cal_disabled = breakers.calendar_disabled.take();
    let mut todo_disabled = breakers.todo_disabled.take();
    let mut contacts_disabled = breakers.contacts_disabled.take();

    tokio::join!(
        poll_calendar_with_flag(
            graph,
            storage,
            event_tx,
            shared,
            &mut cal_backoff,
            &mut cal_disabled,
            &token_fp,
        ),
        poll_todo_with_flag(
            None,
            graph,
            storage,
            event_tx,
            shared,
            &mut todo_backoff,
            &mut todo_disabled,
            &token_fp,
        ),
        poll_contacts_with_flag(
            graph,
            storage,
            event_tx,
            shared,
            &mut contacts_backoff,
            &mut contacts_disabled,
            &token_fp,
        ),
    );
    breakers.calendar_disabled = cal_disabled;
    breakers.todo_disabled = todo_disabled;
    breakers.contacts_disabled = contacts_disabled;

    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: false,
        summary: if mail_ok {
            "Synced".to_string()
        } else {
            POLL_RETRY_SUMMARY.to_string()
        },
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_refresh_all_and_emit(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    mail_err_backoff: &mut ErrorBackoffState,
    slow_err_backoff: &mut ErrorBackoffState,
    breakers: &mut CapabilityCircuitBreakers,
    last_folders: &mut Option<Vec<MailFolder>>,
) {
    run_bootstrap_and_emit(
        graph,
        storage,
        event_tx,
        shared,
        mail_err_backoff,
        breakers,
        last_folders,
    )
    .await;
    slow_err_backoff.record_success();
}

#[cfg_attr(not(test), allow(dead_code))]
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

fn maybe_shrink_on_idle_async(idle_duration: Duration, was_idle: &mut bool, storage: &Storage) {
    if idle_duration >= Duration::from_secs(IDLE_THRESHOLD_SECS) {
        if !*was_idle {
            *was_idle = true;
            let st = storage.clone();
            tokio::task::spawn_blocking(move || st.shrink_memory());
        }
    } else {
        *was_idle = false;
    }
}

pub(crate) fn find_new_unread_mail_candidate(
    folder_id: &str,
    storage: &Storage,
    incoming: &[MailMessage],
) -> Option<NotificationCandidate> {
    let newest = incoming
        .iter()
        .filter(|m| !m.is_read && !m.is_draft)
        .filter(|m| matches!(storage.get_mail_message(&m.id), Ok(None)))
        .max_by(|a, b| a.received_at.cmp(&b.received_at).then(a.id.cmp(&b.id)))?;

    let folder_name = storage
        .list_mail_folders()
        .ok()
        .and_then(|folders| {
            folders.into_iter().find(|f| {
                f.id == folder_id
                    || f.well_known_name
                        .as_deref()
                        .is_some_and(|w| w.eq_ignore_ascii_case(folder_id))
            })
        })
        .map(|f| f.display_name)
        .unwrap_or_else(|| folder_id.to_string());

    let preview_body = newest
        .preview
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    Some(NotificationCandidate {
        item_id: newest.id.clone(),
        folder_name,
        sender: newest.from_display(),
        subject: newest.effective_subject().to_string(),
        preview: preview_body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthManager, StoredTokens};
    use crate::models::EmailRecipient;

    fn mock_client(base_url: String) -> GraphClient {
        let mut cfg = AppConfig::default();
        cfg.graph_base_url = base_url;
        let tok = StoredTokens {
            access_token: "test-access-token".into(),
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
        GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1")
    }

    #[tokio::test]
    async fn test_403_capability_circuit_breakers_trip_silently() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let cal_hits = Arc::new(AtomicUsize::new(0));
        let todo_hits = Arc::new(AtomicUsize::new(0));
        let cal_c = Arc::clone(&cal_hits);
        let todo_c = Arc::clone(&todo_hits);

        let app = Router::new()
            .route(
                "/v1.0/me/calendarView/delta",
                get(move || {
                    cal_c.fetch_add(1, Ordering::SeqCst);
                    async {
                        (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({
                                "error": { "code": "ErrorAccessDenied", "message": "Access is denied" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/todo/lists",
                get(move || {
                    todo_c.fetch_add(1, Ordering::SeqCst);
                    async {
                        (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({
                                "error": { "code": "Forbidden", "message": "Insufficient privileges" }
                            })),
                        )
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(format!("http://{addr}/v1.0"));
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let shared = SyncSharedState::new();
        let mut backoff = ErrorBackoffState::new(60_000);
        let mut breakers = CapabilityCircuitBreakers::default();
        let fp = current_token_fingerprint(&client).await;

        // First call gets 403 and trips both circuit breakers silently
        poll_calendar_quiet(&client, &storage, &event_tx, &shared, &mut backoff, &mut breakers, &fp).await;
        poll_todo_quiet(None, &client, &storage, &event_tx, &shared, &mut backoff, &mut breakers, &fp).await;
        assert!(breakers.is_calendar_disabled(&fp));
        assert!(breakers.is_todo_disabled(&fp));
        assert_eq!(cal_hits.load(Ordering::SeqCst), 1);
        assert_eq!(todo_hits.load(Ordering::SeqCst), 1);

        // Second call does NOT hit Graph at all
        poll_calendar_quiet(&client, &storage, &event_tx, &shared, &mut backoff, &mut breakers, &fp).await;
        poll_todo_quiet(None, &client, &storage, &event_tx, &shared, &mut backoff, &mut breakers, &fp).await;
        assert_eq!(cal_hits.load(Ordering::SeqCst), 1);
        assert_eq!(todo_hits.load(Ordering::SeqCst), 1);

        // Zero SyncError and zero POLL_RETRY_SUMMARY emitted on 403
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_quiet_poll_retry_summary_and_recovery_without_sync_error() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicBool;

        let fail_mode = Arc::new(AtomicBool::new(true));
        let fail_c1 = Arc::clone(&fail_mode);
        let fail_c2 = Arc::clone(&fail_mode);

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(move || {
                    let should_fail = fail_c1.load(Ordering::SeqCst);
                    async move {
                        if should_fail {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({ "error": { "code": "InternalServerError", "message": "transient" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [{
                                        "id": "fld-inbox",
                                        "displayName": "Inbox",
                                        "wellKnownName": "inbox",
                                        "childFolderCount": 0,
                                        "unreadItemCount": 0,
                                        "totalItemCount": 0
                                    }]
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/inbox/messages/delta",
                get(move || {
                    let should_fail = fail_c2.load(Ordering::SeqCst);
                    async move {
                        if should_fail {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(serde_json::json!({ "error": { "code": "ServiceUnavailable", "message": "busy" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [],
                                    "@odata.deltaLink": "https://example.com/delta?tok=1"
                                })),
                            )
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(format!("http://{addr}/v1.0"));
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let shared = SyncSharedState::new();
        let mut mail_backoff = ErrorBackoffState::new(15_000);
        let slow_backoff = ErrorBackoffState::new(60_000);
        let mut last_folders = None;

        // First transient failure emits POLL_RETRY_SUMMARY (never SyncError!)
        poll_mail_lane2_quiet(
            &client,
            &storage,
            &event_tx,
            &shared,
            &mut mail_backoff,
            &slow_backoff,
            &mut last_folders,
        )
        .await;
        assert!(mail_backoff.is_degraded());

        let ev1 = event_rx.try_recv().unwrap();
        assert!(matches!(
            ev1,
            CoreEvent::SyncStateChanged { busy: false, ref summary } if summary == POLL_RETRY_SUMMARY
        ));
        assert!(event_rx.try_recv().is_err());

        // Recovery transitions back to "Synced"
        fail_mode.store(false, Ordering::SeqCst);
        poll_mail_lane2_quiet(
            &client,
            &storage,
            &event_tx,
            &shared,
            &mut mail_backoff,
            &slow_backoff,
            &mut last_folders,
        )
        .await;
        assert!(!mail_backoff.is_degraded());

        let mut saw_synced = false;
        while let Ok(ev) = event_rx.try_recv() {
            assert!(!matches!(ev, CoreEvent::SyncError(_)));
            if matches!(
                ev,
                CoreEvent::SyncStateChanged { busy: false, ref summary } if summary == "Synced"
            ) {
                saw_synced = true;
            }
        }
        assert!(saw_synced);
    }

    #[tokio::test]
    async fn test_spawn_sync_engine_lifecycle_and_idle_shrink() {
        let config = AppConfig::default();
        let client = mock_client("http://127.0.0.1:1/v1.0".to_string());
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, client, storage.clone(), event_tx);
        ctrl.set_focus(FocusTarget {
            active_folder_id: Some("inbox".to_string()),
            active_message_id: None,
            active_todo_list_id: None,
        });
        ctrl.notify_activity();
        ctrl.request(SyncCommand::Shutdown);

        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok());

        let mut was_idle = false;
        maybe_shrink_on_idle(Duration::from_secs(IDLE_THRESHOLD_SECS), &mut was_idle, &storage);
        assert!(was_idle);

        let incoming = vec![MailMessage {
            id: "m-new".to_string(),
            internet_message_id: None,
            conversation_id: None,
            folder_id: Some("inbox".to_string()),
            subject: "Alert".to_string(),
            from: Some(EmailRecipient {
                name: Some("Alice".to_string()),
                address: "alice@example.com".to_string(),
            }),
            to_recipients: Vec::new(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: "2026-11-18T12:00:00Z".to_string(),
            sent_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: None,
            has_attachments: false,
            body_loaded: false,
            preview: "Server alert".to_string(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: Vec::new(),
            meeting_request: None,
            web_link: None,
        }];
        let cand = find_new_unread_mail_candidate("inbox", &storage, &incoming).unwrap();
        assert_eq!(cand.item_id, "m-new");
        assert_eq!(cand.subject, "Alert");
    }
}
