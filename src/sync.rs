//! Adaptive two-lane background synchronization engine for Microsoft Outlook
//! (Lane 1: Fast Interactive Lane + Lane 2: Quiet Background Poll Lane).

use std::{
    collections::{HashMap, HashSet},
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
const ACTIVE_VIEW_SLOW_POLL_MS: u64 = 10_000;
const DEDUP_COALESCE_WINDOW: Duration = Duration::from_millis(400);
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
const HISTORY_EOF_MARKER: &str = "__eof__";
pub(crate) const MAIL_HISTORY_PAGE_SIZE: usize = 50;
const INBOX_PREFETCH_UNREAD_LIMIT: usize = 5;

pub(crate) struct SyncSharedState {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
    auth_pause_until_ms: AtomicU64,
    #[cfg(test)]
    test_idle_offset_ms: AtomicU64,
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
            #[cfg(test)]
            test_idle_offset_ms: AtomicU64::new(0),
        }
    }

    pub(crate) fn record_activity(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Release);
        #[cfg(test)]
        self.test_idle_offset_ms.store(0, Ordering::Release);
        self.activity_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn idle_snapshot(&self, now: Instant) -> (Duration, u64) {
        let seq = self.activity_seq.load(Ordering::Acquire);
        let last_ms = self.last_activity_ms.load(Ordering::Acquire);
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        let base_idle = now_ms.saturating_sub(last_ms);
        #[cfg(test)]
        let base_idle = base_idle.saturating_add(self.test_idle_offset_ms.load(Ordering::Acquire));
        (Duration::from_millis(base_idle), seq)
    }

    #[cfg(test)]
    pub(crate) fn set_test_idle_offset(&self, offset: Duration) {
        self.test_idle_offset_ms
            .store(offset.as_millis() as u64, Ordering::Release);
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

#[derive(Debug, Default)]
pub(crate) struct SyncDeduper {
    in_flight: HashSet<String>,
    last_completed: HashMap<String, Instant>,
}

pub(crate) struct SyncInFlightGuard {
    key: String,
    deduper: Arc<std::sync::Mutex<SyncDeduper>>,
}

impl Drop for SyncInFlightGuard {
    fn drop(&mut self) {
        let mut guard = self.deduper.lock().unwrap_or_else(|e| e.into_inner());
        guard.in_flight.remove(&self.key);
        guard.last_completed.insert(self.key.clone(), Instant::now());
    }
}

pub(crate) fn try_acquire_sync_slot(
    deduper: &Arc<std::sync::Mutex<SyncDeduper>>,
    key: &str,
    min_gap: Option<Duration>,
) -> Option<SyncInFlightGuard> {
    let mut guard = deduper.lock().unwrap_or_else(|e| e.into_inner());
    if guard.in_flight.contains(key) {
        return None;
    }
    if let Some(gap) = min_gap {
        if guard
            .last_completed
            .get(key)
            .is_some_and(|t| t.elapsed() < gap)
        {
            return None;
        }
    }
    guard.in_flight.insert(key.to_string());
    Some(SyncInFlightGuard {
        key: key.to_string(),
        deduper: Arc::clone(deduper),
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveView {
    #[default]
    Mail,
    Calendar,
    Tasks,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    #[serde(default)]
    pub active_view: ActiveView,
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

    #[cfg(test)]
    pub(crate) fn set_idle_for_test(&self, idle: Duration) {
        self.shared.set_test_idle_offset(idle);
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

/// Lower bound for calendar views read from the cache: the same "now - 7 days" the sync engine fetches from.
pub(crate) fn calendar_cache_window_start() -> String {
    default_calendar_window().0
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
    let (me_res, folders_res, events_res, contacts_res, lists_res) = tokio::join!(
        graph.get_me(),
        graph.list_mail_folders(),
        graph.list_calendar_view_complete(&start_iso, &end_iso, 100),
        graph.list_contacts_complete(100),
        graph.list_todo_lists(),
    );

    let _ = me_res;

    if let Ok(folders) = folders_res {
        run_storage(storage, move |st| st.replace_mail_folders(&folders)).await?;
    }

    let (inbox_res, sent_res) = tokio::join!(
        sync_mail_folder_delta("inbox", graph, storage, None, false),
        sync_mail_folder_delta("sentitems", graph, storage, None, false),
    );
    inbox_res?;
    let _ = sent_res;

    if let Ok((events, reached_end)) = events_res {
        let s_iso = start_iso.clone();
        let e_iso = end_iso.clone();
        let _ = run_storage(storage, move |st| {
            if reached_end {
                st.replace_calendar_events_in_window(&s_iso, &e_iso, &events)?;
                Ok(())
            } else {
                st.upsert_calendar_events(&events)
            }
        })
        .await;
    }

    if let Ok((contacts, reached_end)) = contacts_res {
        let _ = run_storage(storage, move |st| {
            if reached_end {
                st.replace_personal_contacts(&contacts)
            } else {
                st.upsert_contacts(&contacts)
            }
        })
        .await;
    }

    if let Ok(lists) = lists_res {
        let default_list_id = pick_default_todo_list(&lists).map(|l| l.id.clone());
        let _ = run_storage(storage, move |st| st.replace_todo_lists(&lists)).await;
        if let Some(list_id) = default_list_id {
            if let Ok((tasks, reached_end)) =
                graph.list_todo_tasks_complete(&list_id, true, 100).await
            {
                let lid = list_id.clone();
                let _ = run_storage(storage, move |st| {
                    if reached_end {
                        st.replace_todo_tasks_for_list(&lid, true, &tasks)
                    } else {
                        st.upsert_todo_tasks(&tasks)
                    }
                })
                .await;
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
    let bg_focus_rx = focus_rx.clone();
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<SyncCommand>();

    let active_ms = config.active_poll_ms.max(250);
    let mail_ms = config.mail_poll_ms.max(500);
    let calendar_ms = config.calendar_poll_ms.max(1_000);
    let active_slow_ms = calendar_ms.min(ACTIVE_VIEW_SLOW_POLL_MS);
    let dedup_window = Duration::from_millis((active_ms / 2).min(DEDUP_COALESCE_WINDOW.as_millis() as u64));
    let shared = Arc::new(SyncSharedState::new());
    let deduper = Arc::new(std::sync::Mutex::new(SyncDeduper::default()));

    let ctrl_shutdown_tx = shutdown_tx.clone();
    // Lane 1: Fast Interactive Lane (message hydration, active folder delta, folder pagination)
    let fast_graph = graph.clone();
    let fast_storage = storage.clone();
    let fast_event_tx = event_tx.clone();
    let mut fast_shutdown_rx = shutdown_rx.clone();
    let fast_shared = Arc::clone(&shared);
    let fast_deduper = Arc::clone(&deduper);
    let in_flight_hydrations: Arc<std::sync::Mutex<HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(HashSet::new()));
    tokio::spawn(async move {
        let mut active_tick = time::interval(Duration::from_millis(active_ms));
        active_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
                            spawn_hydrate_message(
                                message_id,
                                false,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                &fast_shared,
                                &in_flight_hydrations,
                            );
                        }
                        Some(SyncCommand::SyncFolder { folder_id }) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            restricted_folders.remove(&folder_id);
                            active_err_backoff.reset_window();
                            match sync_mail_folder_deduped(
                                &folder_id,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                false,
                                &fast_deduper,
                                Some(dedup_window),
                            )
                            .await
                            {
                                Ok(true) => {
                                    emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                }
                                Ok(false) => {}
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
                            let has_more = if restricted_folders.contains(&folder_id) {
                                None
                            } else {
                                match load_older_mail_page(&folder_id, explicit, &fast_graph, &fast_storage).await {
                                    Ok(has_more) => {
                                        emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                        Some(has_more)
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
                                        None
                                    }
                                }
                            };
                            let _ = fast_event_tx.send(CoreEvent::MailHistoryPageFinished { folder_id, has_more });
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
                            spawn_hydrate_message(
                                mid.to_string(),
                                false,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                &fast_shared,
                                &in_flight_hydrations,
                            );
                        }
                        if focus.active_view == ActiveView::Mail {
                            if let (true, Some(fid)) = (folder_changed, focus.active_folder_id.as_deref()) {
                                if !restricted_folders.contains(fid) {
                                    match sync_mail_folder_deduped(
                                        fid,
                                        &fast_graph,
                                        &fast_storage,
                                        &fast_event_tx,
                                        false,
                                        &fast_deduper,
                                        Some(dedup_window),
                                    )
                                    .await
                                    {
                                        Ok(true) => {
                                            emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                        }
                                        Ok(false) => {}
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
                        if focus.active_view == ActiveView::Tasks {
                            if let (true, Some(lid)) = (todo_changed, focus.active_todo_list_id.as_deref()) {
                                let _ = bg_tx.send(SyncCommand::SyncTodo {
                                    list_id: Some(lid.to_string()),
                                });
                            }
                        }
                    }
                    active_tick.reset();
                }
                _ = active_tick.tick() => {
                    let now = Instant::now();
                    if !fast_shared.is_auth_paused(now)
                        && !active_err_backoff.is_backed_off(now)
                    {
                        let focus = focus_rx.borrow().clone();
                        if focus.active_view == ActiveView::Mail {
                            if let Some(fid) = focus.active_folder_id.as_deref() {
                                if !restricted_folders.contains(fid) {
                                    match sync_mail_folder_deduped(
                                        fid,
                                        &fast_graph,
                                        &fast_storage,
                                        &fast_event_tx,
                                        true,
                                        &fast_deduper,
                                        Some(dedup_window),
                                    )
                                    .await
                                    {
                                        Ok(true) => {
                                            emit_recovered_if_needed(&mut active_err_backoff, &fast_event_tx);
                                        }
                                        Ok(false) => {}
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
        }
    });

    // Lane 2: Quiet Background Poll Lane (folders + non-active Inbox every 15s;
    // visible Calendar/Tasks every 10s; background Calendar + To Do + Contacts every 60s)
    let bg_shared = Arc::clone(&shared);
    let bg_deduper = Arc::clone(&deduper);
    tokio::spawn(async move {
        let mut mail_tick = time::interval(Duration::from_millis(mail_ms));
        mail_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut active_slow_tick = time::interval(Duration::from_millis(active_slow_ms));
        active_slow_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
        active_slow_tick.tick().await;
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
                            let focus = bg_focus_rx.borrow().clone();
                            run_refresh_all_and_emit(
                                &focus,
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
                            if let Some(_slot) = try_acquire_sync_slot(
                                &bg_deduper,
                                "calendar",
                                Some(dedup_window),
                            ) {
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
                                active_slow_tick.reset();
                            }
                        }
                        Some(SyncCommand::SyncTodo { list_id }) => {
                            let token_fp = current_token_fingerprint(&graph).await;
                            breakers.todo_disabled = None;
                            let key = resolve_todo_dedup_key(list_id.as_deref(), &storage).await;
                            if let Some(_slot) = try_acquire_sync_slot(
                                &bg_deduper,
                                &key,
                                Some(dedup_window),
                            ) {
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
                                active_slow_tick.reset();
                            }
                        }
                        Some(SyncCommand::SyncContacts) => {
                            let token_fp = current_token_fingerprint(&graph).await;
                            breakers.contacts_disabled = None;
                            if let Some(_slot) = try_acquire_sync_slot(
                                &bg_deduper,
                                "contacts",
                                Some(dedup_window),
                            ) {
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
                        let focus = bg_focus_rx.borrow().clone();
                        let inbox_is_active = focus.active_view == ActiveView::Mail
                            && is_inbox_folder_active(focus.active_folder_id.as_deref(), &storage)
                                .await;
                        poll_mail_lane2_quiet_with_dedup(
                            &graph,
                            &storage,
                            &event_tx,
                            &bg_shared,
                            &mut mail_err_backoff,
                            &slow_err_backoff,
                            &mut last_folders,
                            !inbox_is_active,
                            Some(&bg_deduper),
                        )
                        .await;
                    }
                }
                _ = active_slow_tick.tick() => {
                    let now = Instant::now();
                    let (idle, _) = bg_shared.idle_snapshot(now);
                    maybe_shrink_on_idle_async(idle, &mut was_idle, &storage);
                    let focus = bg_focus_rx.borrow().clone();
                    if matches!(focus.active_view, ActiveView::Calendar | ActiveView::Tasks)
                        && !bg_shared.is_auth_paused(now)
                        && !slow_err_backoff.is_backed_off(now)
                    {
                        let token_fp = current_token_fingerprint(&graph).await;
                        match focus.active_view {
                            ActiveView::Calendar => {
                                if let Some(_slot) = try_acquire_sync_slot(
                                    &bg_deduper,
                                    "calendar",
                                    Some(dedup_window),
                                ) {
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
                            }
                            ActiveView::Tasks => {
                                let target_lid = focus.active_todo_list_id.as_deref();
                                let key = resolve_todo_dedup_key(target_lid, &storage).await;
                                if let Some(_slot) = try_acquire_sync_slot(
                                    &bg_deduper,
                                    &key,
                                    Some(dedup_window),
                                ) {
                                    poll_todo_quiet(
                                        target_lid,
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
                            ActiveView::Mail => {}
                        }
                    }
                }
                _ = slow_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = bg_shared.idle_snapshot(now);
                    maybe_shrink_on_idle_async(idle, &mut was_idle, &storage);
                    if slow_backoff.on_tick(idle, seq)
                        && !bg_shared.is_auth_paused(now)
                        && !slow_err_backoff.is_backed_off(now)
                    {
                        let focus = bg_focus_rx.borrow().clone();
                        let token_fp = current_token_fingerprint(&graph).await;
                        if focus.active_view != ActiveView::Calendar {
                            if let Some(_slot) = try_acquire_sync_slot(
                                &bg_deduper,
                                "calendar",
                                Some(dedup_window),
                            ) {
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
                        }
                        if focus.active_view != ActiveView::Tasks {
                            let target_lid = focus.active_todo_list_id.as_deref();
                            let key = resolve_todo_dedup_key(target_lid, &storage).await;
                            if let Some(_slot) = try_acquire_sync_slot(
                                &bg_deduper,
                                &key,
                                Some(dedup_window),
                            ) {
                                poll_todo_quiet(
                                    target_lid,
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
                        if let Some(_slot) = try_acquire_sync_slot(
                            &bg_deduper,
                            "contacts",
                            Some(DEDUP_COALESCE_WINDOW),
                        ) {
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

struct InFlightHydrationGuard {
    key: String,
    set: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl Drop for InFlightHydrationGuard {
    fn drop(&mut self) {
        let mut guard = self.set.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(&self.key);
    }
}

fn spawn_hydrate_message(
    message_id: String,
    force: bool,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &Arc<SyncSharedState>,
    in_flight: &Arc<std::sync::Mutex<HashSet<String>>>,
) {
    let trimmed = message_id.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    {
        let mut guard = in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.insert(trimmed.clone()) {
            return;
        }
    }

    let graph = graph.clone();
    let storage = storage.clone();
    let event_tx = event_tx.clone();
    let shared = Arc::clone(shared);
    let in_flight = Arc::clone(in_flight);
    tokio::spawn(async move {
        let _guard = InFlightHydrationGuard {
            key: trimmed.clone(),
            set: in_flight,
        };
        if let Err(err) =
            hydrate_message_if_needed(&trimmed, force, &graph, &storage, &event_tx).await
        {
            if classify_sync_error(&err) == SyncErrorKind::Unauthorized401 {
                shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            }
        }
    });
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
        let msgs = st.list_mail_summaries_filtered(
            Some(&fid),
            true,
            false,
            false,
            max_count.saturating_mul(4).max(10),
        )?;
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
) -> Result<bool> {
    let scope_key = format!("mail_history:{folder_id}");
    let scope_clone = scope_key.clone();
    let cursor = run_storage(storage, move |st| st.get_sync_cursor(&scope_clone)).await?;
    let existing_next = cursor.and_then(|c| c.history_next_link);

    if existing_next.as_deref() == Some(HISTORY_EOF_MARKER) && !explicit {
        return Ok(false);
    }

    let mut next_arg: Option<String> = existing_next
        .as_deref()
        .filter(|s| *s != HISTORY_EOF_MARKER)
        .map(str::to_string);

    let mut final_next_link: Option<String> = None;

    for _ in 0..5 {
        let (messages, next_link) = match graph
            .list_mail_messages_page(folder_id, MAIL_HISTORY_PAGE_SIZE, next_arg.as_deref(), true)
            .await
        {
            Ok(res) => res,
            Err(err) if next_arg.is_some() && is_expired_delta_error(&err) => {
                graph
                    .list_mail_messages_page(folder_id, MAIL_HISTORY_PAGE_SIZE, None, true)
                    .await?
            }
            Err(err) => return Err(err),
        };

        let msgs_for_db = messages;
        let changed = run_storage(storage, move |st| st.upsert_mail_messages(&msgs_for_db)).await?;
        final_next_link = next_link.clone();
        if changed > 0 || next_link.is_none() {
            break;
        }
        next_arg = next_link;
    }

    let has_more = final_next_link.is_some();
    run_storage(storage, move |st| {
        st.upsert_sync_cursor(&SyncCursor {
            scope_key,
            delta_link: None,
            history_next_link: final_next_link.or_else(|| Some(HISTORY_EOF_MARKER.to_string())),
            last_synced_at: chrono::Utc::now().to_rfc3339(),
        })?;
        Ok(())
    })
    .await?;

    Ok(has_more)
}

#[derive(Debug, Clone)]
pub struct MailFolderSyncOutcome {
    pub folder_id: String,
    pub changed_rows: usize,
    pub deleted_rows: usize,
}

async fn resolve_canonical_folder_id(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
) -> (String, Option<String>) {
    let trimmed = folder_id.trim();
    let wk_alias = crate::resolver::canonical_well_known_folder(trimmed).map(str::to_string);

    let q_local = trimmed.to_string();
    if let Ok(Ok(found)) = run_storage(storage, move |st| {
        let folders = st.list_mail_folders()?;
        Ok(crate::resolver::resolve_mail_folder_from_slice(&folders, &q_local))
    })
    .await
    {
        return (
            found.id,
            found.well_known_name.or(wk_alias),
        );
    }

    if let Ok(folders) = graph.list_mail_folders().await {
        let folders_for_db = folders.clone();
        let _ = run_storage(storage, move |st| st.replace_mail_folders(&folders_for_db)).await;
        if let Ok(found) = crate::resolver::resolve_mail_folder_from_slice(&folders, trimmed) {
            return (
                found.id,
                found.well_known_name.or(wk_alias),
            );
        }
    }

    (trimmed.to_string(), wk_alias)
}

fn normalize_incoming_folder_ids(messages: &mut [MailMessage], real_folder_id: &str) {
    for msg in messages {
        if msg
            .folder_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            msg.folder_id = Some(real_folder_id.to_string());
        }
    }
}

async fn sync_mail_folder_first_page(
    real_folder_id: &str,
    had_initial: bool,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
) -> Result<()> {
    let (mut page_msgs, next_link) = graph
        .list_mail_messages_page(real_folder_id, 50, None, true)
        .await?;
    normalize_incoming_folder_ids(&mut page_msgs, real_folder_id);
    let fid_clone = real_folder_id.to_string();
    let hist_scope = format!("mail_history:{real_folder_id}");
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
        let list = st.list_mail_summaries_filtered(Some(&fid_clone), false, false, false, 150)?;
        Ok((changed, notify, list))
    })
    .await?;

    if let Some(tx) = event_tx {
        if changed_rows > 0 || !had_initial {
            let _ = tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: real_folder_id.to_string(),
                messages: stored_msgs,
                notify_candidate,
            });
        }
    }
    Ok(())
}

/// Synchronizes a single mail folder using per-folder Microsoft Graph `/messages/delta`:
/// - Always resolves well-known aliases (`"inbox"`, etc.) to the real Graph folder ID.
/// - Preserves fast first-page display when bootstrapping an uncached folder in interactive mode.
/// - Streams header-only delta pages to SQLite without an arbitrary page cap (guarding against
///   cyclic/repeated `@odata.nextLink`), never drops prior cached rows on partial/error runs,
///   reconciles missing rows strictly for this folder only upon reaching the terminal
///   `@odata.deltaLink`, and commits the terminal page + reconciliation + cursor atomically.
pub async fn sync_mail_folder_delta(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
    is_periodic: bool,
) -> Result<MailFolderSyncOutcome> {
    sync_mail_folder_delta_with_page_size(folder_id, graph, storage, event_tx, is_periodic, 50)
        .await
}

pub(crate) async fn sync_mail_folder_delta_with_page_size(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: Option<&mpsc::UnboundedSender<CoreEvent>>,
    is_periodic: bool,
    page_size: usize,
) -> Result<MailFolderSyncOutcome> {
    let (real_folder_id, folder_alias) =
        resolve_canonical_folder_id(folder_id, graph, storage).await;
    let scope_key = format!("mail_delta:{real_folder_id}");
    let scope_for_read = scope_key.clone();
    let fid_for_read = real_folder_id.clone();
    let alias_for_read = folder_alias.clone();
    let (prev_cursor, existing_count) = run_storage(storage, move |st| {
        let mut cursor = st.get_sync_cursor(&scope_for_read)?;
        if cursor
            .as_ref()
            .and_then(|c| c.delta_link.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            if let Some(ref alias) = alias_for_read {
                if !alias.eq_ignore_ascii_case(&fid_for_read) {
                    if let Some(alias_cur) = st.get_sync_cursor(&format!("mail_delta:{alias}"))? {
                        if alias_cur
                            .delta_link
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .is_some()
                        {
                            cursor = Some(alias_cur);
                        }
                    }
                }
            }
        }
        let summaries = st.list_mail_summaries(Some(&fid_for_read), 1)?;
        Ok((cursor, summaries.len()))
    })
    .await?;

    let had_initial = prev_cursor.is_some() || existing_count > 0;
    let existing_delta_link = prev_cursor
        .as_ref()
        .and_then(|c| c.delta_link.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut is_full_bootstrap = existing_delta_link.is_none();
    if is_full_bootstrap && event_tx.is_some() && !had_initial {
        // Keep fast first-page listing on initial empty interactive load so UI renders promptly
        // before full folder delta bootstrap finishes.
        let _ = sync_mail_folder_first_page(
            &real_folder_id,
            had_initial,
            graph,
            storage,
            event_tx,
        )
        .await;
    }

    let mut next_link: Option<String> = existing_delta_link.clone();
    let mut seen_urls = HashSet::new();
    seen_urls.insert(crate::graph::mail::canonical_delta_url_key(
        &real_folder_id,
        next_link.as_deref(),
    ));
    let mut reconcile_seen: Option<HashSet<String>> = if is_full_bootstrap {
        Some(HashSet::new())
    } else {
        None
    };
    let mut tried_expired_reset = false;
    let mut total_changed = 0usize;
    let mut total_deleted = 0usize;
    let mut notify_candidate: Option<NotificationCandidate> = None;
    let hist_link = prev_cursor.as_ref().and_then(|c| c.history_next_link.clone());

    loop {
        let mut page = match graph
            .delta_mail_messages_page(&real_folder_id, next_link.as_deref(), page_size)
            .await
        {
            Ok(p) => p,
            Err(err)
                if next_link.is_some()
                    && !tried_expired_reset
                    && is_expired_delta_error(&err) =>
            {
                // Persist cleared expired cursor immediately so an interrupted re-bootstrap
                // retries a full delta instead of replaying the dead token.
                tried_expired_reset = true;
                let expired_scope = scope_key.clone();
                let expired_hist = hist_link.clone();
                let _ = run_storage(storage, move |st| {
                    st.upsert_sync_cursor(&SyncCursor {
                        scope_key: expired_scope,
                        delta_link: None,
                        history_next_link: expired_hist,
                        last_synced_at: chrono::Utc::now().to_rfc3339(),
                    })
                })
                .await;
                is_full_bootstrap = true;
                reconcile_seen = Some(HashSet::new());
                next_link = None;
                seen_urls.clear();
                seen_urls.insert(crate::graph::mail::canonical_delta_url_key(
                    &real_folder_id,
                    None,
                ));
                continue;
            }
            Err(err) => return Err(err),
        };

        normalize_incoming_folder_ids(&mut page.upserted, &real_folder_id);
        if let Some(ref mut seen) = reconcile_seen {
            for msg in &page.upserted {
                seen.insert(msg.id.clone());
            }
        }

        let is_terminal = page.delta_link.is_some() || page.next_link.is_none();
        if !is_terminal {
            let next = page.next_link.expect("non-terminal page has next_link");
            let next_key =
                crate::graph::mail::canonical_delta_url_key(&real_folder_id, Some(&next));
            if !seen_urls.insert(next_key) {
                anyhow::bail!(
                    "Cyclic or repeated @odata.nextLink detected during mail delta sync for folder {real_folder_id}: {next}"
                );
            }

            let fid_p = real_folder_id.clone();
            let alias_p = folder_alias.clone();
            let incoming_p = page.upserted;
            let removed_p = page.removed_ids;
            let need_notify = had_initial && notify_candidate.is_none();
            let (changed, deleted, page_notify) = run_storage(storage, move |st| {
                let cand = if need_notify {
                    find_new_unread_mail_candidate(&fid_p, st, &incoming_p)
                } else {
                    None
                };
                let (c, d) = st.apply_mail_folder_delta_page(
                    &fid_p,
                    alias_p.as_deref(),
                    &incoming_p,
                    &removed_p,
                )?;
                Ok((c, d, cand))
            })
            .await?;
            total_changed += changed;
            total_deleted += deleted;
            if notify_candidate.is_none() {
                notify_candidate = page_notify;
            }
            next_link = Some(next);
            continue;
        }

        let delta_complete = page.delta_link.is_some();
        let final_reconcile = if is_full_bootstrap && delta_complete {
            reconcile_seen.take()
        } else {
            None
        };
        // Only advance the delta cursor when delta ran to completion; if malformed/truncated
        // (no deltaLink), preserve the prior valid delta_link (or None during bootstrap).
        let effective_delta = if delta_complete {
            page.delta_link
        } else if is_full_bootstrap {
            None
        } else {
            existing_delta_link.clone()
        };

        let fid_clone = real_folder_id.clone();
        let alias_clone = folder_alias.clone();
        let incoming_final = page.upserted;
        let removed_final = page.removed_ids;
        let need_notify = had_initial && notify_candidate.is_none();
        let cursor = SyncCursor {
            scope_key: scope_key.clone(),
            delta_link: effective_delta,
            history_next_link: hist_link.clone(),
            last_synced_at: chrono::Utc::now().to_rfc3339(),
        };

        let (changed, deleted, page_notify) = run_storage(storage, move |st| {
            let cand = if need_notify {
                find_new_unread_mail_candidate(&fid_clone, st, &incoming_final)
            } else {
                None
            };
            let (c, d) = st.apply_mail_folder_delta(
                &fid_clone,
                alias_clone.as_deref(),
                &incoming_final,
                &removed_final,
                final_reconcile.as_ref(),
                &cursor,
            )?;
            Ok((c, d, cand))
        })
        .await?;
        total_changed += changed;
        total_deleted += deleted;
        if notify_candidate.is_none() {
            notify_candidate = page_notify;
        }
        break;
    }

    if let Some(tx) = event_tx {
        if !is_periodic || total_changed > 0 || total_deleted > 0 {
            let fid_list = real_folder_id.clone();
            let stored_msgs = run_storage(storage, move |st| {
                st.list_mail_summaries_filtered(Some(&fid_list), false, false, false, 150)
            })
            .await?;
            let _ = tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: real_folder_id.clone(),
                messages: stored_msgs,
                notify_candidate,
            });
        }
    }

    Ok(MailFolderSyncOutcome {
        folder_id: real_folder_id,
        changed_rows: total_changed,
        deleted_rows: total_deleted,
    })
}

async fn is_inbox_folder_active(active_folder_id: Option<&str>, storage: &Storage) -> bool {
    let Some(fid) = active_folder_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if fid.eq_ignore_ascii_case("inbox") {
        return true;
    }
    let fid_owned = fid.to_string();
    run_storage(storage, move |st| {
        Ok(st
            .find_well_known_folder_id("inbox")?
            .is_some_and(|inbox_id| inbox_id == fid_owned))
    })
    .await
    .unwrap_or(false)
}

async fn resolve_todo_dedup_key(list_id: Option<&str>, storage: &Storage) -> String {
    if let Some(lid) = list_id.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("todo:{lid}");
    }
    let cached_default = run_storage(storage, |st| {
        let lists = st.list_todo_lists()?;
        Ok(pick_default_todo_list(&lists).map(|l| l.id.clone()))
    })
    .await
    .ok()
    .flatten();
    match cached_default {
        Some(lid) => format!("todo:{lid}"),
        None => "todo:__default__".to_string(),
    }
}

async fn sync_mail_folder_deduped(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    is_periodic: bool,
    deduper: &Arc<std::sync::Mutex<SyncDeduper>>,
    min_gap: Option<Duration>,
) -> Result<bool> {
    let (real_fid, _) = resolve_canonical_folder_id(folder_id, graph, storage).await;
    let key = format!("folder:{real_fid}");
    let Some(_slot) = try_acquire_sync_slot(deduper, &key, min_gap) else {
        return Ok(false);
    };
    sync_mail_folder(&real_fid, graph, storage, event_tx, is_periodic).await?;
    Ok(true)
}

async fn sync_mail_folder(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    is_periodic: bool,
) -> Result<()> {
    sync_mail_folder_delta(folder_id, graph, storage, Some(event_tx), is_periodic).await?;
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
        run_storage(storage, move |st| st.replace_mail_folders(&to_store)).await?;
        *last_folders = Some(folders.clone());
        let _ = event_tx.send(CoreEvent::MailFoldersUpdated(folders));
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
async fn poll_mail_lane2_quiet(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    mail_err_backoff: &mut ErrorBackoffState,
    slow_err_backoff: &ErrorBackoffState,
    last_folders: &mut Option<Vec<MailFolder>>,
) {
    poll_mail_lane2_quiet_with_dedup(
        graph,
        storage,
        event_tx,
        shared,
        mail_err_backoff,
        slow_err_backoff,
        last_folders,
        true,
        None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn poll_mail_lane2_quiet_with_dedup(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &SyncSharedState,
    mail_err_backoff: &mut ErrorBackoffState,
    slow_err_backoff: &ErrorBackoffState,
    last_folders: &mut Option<Vec<MailFolder>>,
    sync_inbox: bool,
    deduper: Option<&Arc<std::sync::Mutex<SyncDeduper>>>,
) {
    let folders_res = poll_mail_folders_quiet(graph, storage, event_tx, last_folders).await;
    let inbox_res = if sync_inbox {
        if let Some(d) = deduper {
            sync_mail_folder_deduped(
                "inbox",
                graph,
                storage,
                event_tx,
                true,
                d,
                Some(DEDUP_COALESCE_WINDOW),
            )
            .await
            .map(|_| ())
        } else {
            sync_mail_folder("inbox", graph, storage, event_tx, true).await
        }
    } else {
        Ok(())
    };

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
    let cursor = run_storage(storage, |st| st.get_sync_cursor("calendar_delta"))
        .await
        .ok()
        .flatten();
    let today_utc = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let stale_day_cursor = cursor.as_ref().is_some_and(|c| {
        c.last_synced_at
            .trim()
            .get(..10)
            .is_some_and(|day| day != today_utc.as_str())
    });
    let effective_cursor_link = if stale_day_cursor {
        None
    } else {
        cursor
            .as_ref()
            .and_then(|c| c.delta_link.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let had_delta_link = effective_cursor_link.is_some();

    let delta_res = graph
        .delta_calendar_view(&start_iso, &end_iso, effective_cursor_link)
        .await;

    let res = match delta_res {
        Ok((events, removed_ids, new_delta_link)) => {
            let delta_complete = new_delta_link
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some();
            let reconcile_window = !had_delta_link && delta_complete;
            let effective_delta = new_delta_link
                .or_else(|| cursor.as_ref().and_then(|c| c.delta_link.clone()));
            let sync_cursor = SyncCursor {
                scope_key: "calendar_delta".to_string(),
                delta_link: effective_delta,
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            };
            let s_iso = start_iso.clone();
            let e_iso = end_iso.clone();
            let stored = run_storage(storage, move |st| {
                let has_changes = st.apply_calendar_delta(
                    &s_iso,
                    &e_iso,
                    &events,
                    &removed_ids,
                    reconcile_window,
                    &sync_cursor,
                )?;
                if has_changes {
                    let list = st.list_calendar_events(Some(&s_iso), Some(&e_iso), 200)?;
                    Ok(Some(list))
                } else {
                    Ok(None)
                }
            })
            .await;
            match stored {
                Ok(Some(list)) => {
                    let _ = event_tx.send(CoreEvent::CalendarEventsUpdated(list));
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(err) => Err(err),
            }
        }
        Err(err) if is_expired_delta_error(&err) => {
            match graph
                .list_calendar_view_complete(&start_iso, &end_iso, 100)
                .await
            {
                Ok((events, reached_end)) => {
                    let to_store = events.clone();
                    let s_iso = start_iso.clone();
                    let e_iso = end_iso.clone();
                    let _ = run_storage(storage, move |st| {
                        st.upsert_sync_cursor(&SyncCursor {
                            scope_key: "calendar_delta".to_string(),
                            delta_link: None,
                            history_next_link: None,
                            last_synced_at: chrono::Utc::now().to_rfc3339(),
                        })?;
                        if reached_end {
                            st.replace_calendar_events_in_window(&s_iso, &e_iso, &to_store)?;
                            Ok(())
                        } else {
                            st.upsert_calendar_events(&to_store)
                        }
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
    if run_storage(storage, move |st| st.replace_todo_lists(&lists_for_db))
        .await
        .is_ok()
    {
        let _ = event_tx.send(CoreEvent::TodoListsUpdated(lists));
    }

    if let Some(lid) = chosen_list_id {
        match graph.list_todo_tasks_complete(&lid, true, 100).await {
            Ok((tasks, reached_end)) => {
                let tasks_for_db = tasks.clone();
                let lid_for_db = lid.clone();
                let _ = run_storage(storage, move |st| {
                    if reached_end {
                        st.replace_todo_tasks_for_list(&lid_for_db, true, &tasks_for_db)
                    } else {
                        st.upsert_todo_tasks(&tasks_for_db)
                    }
                })
                .await;
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

    match graph.list_contacts_complete(100).await {
        Ok((contacts, reached_end)) => {
            let to_store = contacts.clone();
            if run_storage(storage, move |st| {
                if reached_end {
                    st.replace_personal_contacts(&to_store)
                } else {
                    st.upsert_contacts(&to_store)
                }
            })
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
    let folders_res = poll_mail_folders_quiet(graph, storage, event_tx, last_folders).await;
    let inbox_res = sync_mail_folder("inbox", graph, storage, event_tx, false).await;

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
    focus: &FocusTarget,
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

    if let Some(active_fid) = focus
        .active_folder_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (inbox_real, _) = resolve_canonical_folder_id("inbox", graph, storage).await;
        if !active_fid.eq_ignore_ascii_case("inbox") && active_fid != inbox_real {
            let _ = sync_mail_folder(active_fid, graph, storage, event_tx, false).await;
        }
    }

    if let Some(active_mid) = focus
        .active_message_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let _ = hydrate_message_if_needed(active_mid, true, graph, storage, event_tx).await;
    }

    if let Some(active_lid) = focus
        .active_todo_list_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let token_fp = current_token_fingerprint(graph).await;
        poll_todo_quiet(
            Some(active_lid),
            graph,
            storage,
            event_tx,
            shared,
            slow_err_backoff,
            breakers,
            &token_fp,
        )
        .await;
    }

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
        let fail_c3 = Arc::clone(&fail_mode);

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
                "/v1.0/me/mailFolders/{fid}/messages",
                get(move || {
                    let should_fail = fail_c3.load(Ordering::SeqCst);
                    async move {
                        if should_fail {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(serde_json::json!({ "error": { "code": "ServiceUnavailable", "message": "busy" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({ "value": [] })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/{fid}/messages/delta",
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
            active_view: ActiveView::Mail,
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
            last_modified_at: None,
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

    #[tokio::test]
    async fn test_sync_mail_folder_initial_uses_fast_page_and_bootstraps_delta() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let delta_hits = Arc::new(AtomicUsize::new(0));
        let delta_hits_c = Arc::clone(&delta_hits);
        let page_hits = Arc::new(AtomicUsize::new(0));
        let page_hits_c = Arc::clone(&page_hits);

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "fld-inbox",
                            "displayName": "Inbox",
                            "wellKnownName": "inbox",
                            "childFolderCount": 0,
                            "unreadItemCount": 1,
                            "totalItemCount": 2
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/mailFolders/{fid}/messages",
                get(move || {
                    page_hits_c.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m1",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Fast Page 1",
                                    "receivedDateTime": "2026-11-18T10:00:00Z",
                                    "isRead": false,
                                    "bodyPreview": "Preview 1"
                                },
                                {
                                    "id": "m2",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Fast Page 2",
                                    "receivedDateTime": "2026-11-18T09:00:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Preview 2"
                                }
                            ],
                            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/mailFolders/fld-inbox/messages?$skip=50"
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/{fid}/messages/delta",
                get(move || {
                    delta_hits_c.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m1",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Fast Page 1",
                                    "receivedDateTime": "2026-11-18T10:00:00Z",
                                    "isRead": false,
                                    "bodyPreview": "Preview 1"
                                },
                                {
                                    "id": "m2",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Fast Page 2",
                                    "receivedDateTime": "2026-11-18T09:00:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Preview 2"
                                }
                            ],
                            "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1"
                        }))
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

        sync_mail_folder("inbox", &client, &storage, &event_tx, false)
            .await
            .unwrap();

        assert_eq!(page_hits.load(Ordering::SeqCst), 1);
        assert_eq!(delta_hits.load(Ordering::SeqCst), 1);

        let hist_cursor = storage
            .get_sync_cursor("mail_history:fld-inbox")
            .unwrap()
            .expect("history cursor stored under real folder ID");
        assert_eq!(
            hist_cursor.history_next_link.as_deref(),
            Some("https://graph.microsoft.com/v1.0/me/mailFolders/fld-inbox/messages?$skip=50")
        );
        let delta_cursor = storage
            .get_sync_cursor("mail_delta:fld-inbox")
            .unwrap()
            .expect("delta cursor stored under real folder ID");
        assert!(delta_cursor
            .delta_link
            .as_deref()
            .is_some_and(|l| l.contains("deltatoken=d1")));

        let ev = event_rx.try_recv().expect("MailMessagesUpdated emitted");
        match ev {
            CoreEvent::MailMessagesUpdated {
                folder_id,
                messages,
                ..
            } => {
                assert_eq!(folder_id, "fld-inbox");
                assert_eq!(messages.len(), 2);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_concurrent_hydrate_message_emits_body_loaded_and_coalesces() {
        use axum::{extract::Path as AxumPath, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let msg_hits = Arc::new(AtomicUsize::new(0));
        let msg_hits_c = Arc::clone(&msg_hits);

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders/inbox/messages",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    Json(serde_json::json!({ "value": [] }))
                }),
            )
            .route(
                "/v1.0/me/messages/{id}",
                get(move |AxumPath(id): AxumPath<String>| {
                    let hits = Arc::clone(&msg_hits_c);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        Json(serde_json::json!({
                            "id": id,
                            "parentFolderId": "inbox",
                            "subject": "Hydrated Subject",
                            "receivedDateTime": "2026-11-18T11:00:00Z",
                            "bodyPreview": "Preview",
                            "body": { "contentType": "html", "content": "<p>Full concurrent body</p>" }
                        }))
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

        let ctrl = spawn_sync_engine(AppConfig::default(), client, storage.clone(), event_tx);
        ctrl.request(SyncCommand::SyncFolder {
            folder_id: "inbox".to_string(),
        });
        ctrl.request(SyncCommand::HydrateMessage {
            message_id: "msg-1".to_string(),
        });
        ctrl.request(SyncCommand::HydrateMessage {
            message_id: "msg-1".to_string(),
        });
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Mail,
            active_folder_id: None,
            active_message_id: Some("msg-1".to_string()),
            active_todo_list_id: None,
        });

        let hydrated = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(ev) = event_rx.recv().await {
                if let CoreEvent::MailMessageHydrated(msg) = ev {
                    return msg;
                }
            }
            panic!("channel closed without MailMessageHydrated");
        })
        .await
        .expect("timed out waiting for hydration");

        assert_eq!(hydrated.id, "msg-1");
        assert!(hydrated.body_loaded);
        assert!(hydrated.body_plain.contains("Full concurrent body"));
        assert_eq!(msg_hits.load(Ordering::SeqCst), 1);

        ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn test_bootstrap_outlook_fetches_sentitems() {
        use axum::{routing::get, Json, Router};

        let app = Router::new()
            .route(
                "/v1.0/me",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "u1",
                        "displayName": "Tester",
                        "mail": "me@example.com"
                    }))
                }),
            )
            .route(
                "/v1.0/me/mailFolders",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "fld-inbox", "displayName": "받은 편지함", "childFolderCount": 0 },
                            { "id": "fld-sent", "displayName": "보낸 편지함", "childFolderCount": 0 }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/mailFolders/fld-inbox/messages/delta",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "in-1",
                            "parentFolderId": "fld-inbox",
                            "subject": "Inbox Mail",
                            "receivedDateTime": "2026-11-18T10:00:00Z",
                            "isRead": true
                        }],
                        "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=in1"
                    }))
                }),
            )
            .route(
                "/v1.0/me/mailFolders/fld-sent/messages/delta",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "sent-1",
                            "parentFolderId": "fld-sent",
                            "subject": "Sent Mail",
                            "receivedDateTime": "2026-11-18T09:00:00Z",
                            "isRead": true
                        }],
                        "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/mailFolders/fld-sent/messages/delta?$deltatoken=sent1"
                    }))
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(format!("http://{addr}/v1.0"));
        let storage = Storage::open_in_memory().unwrap();
        bootstrap_outlook(&client, &storage).await.unwrap();

        let sent_msgs = storage
            .list_mail_messages(Some("sentitems"), false, false, false, 10)
            .unwrap();
        assert_eq!(sent_msgs.len(), 1);
        assert_eq!(sent_msgs[0].id, "sent-1");
    }

    #[tokio::test]
    async fn test_sync_mail_folder_delta_multipage_interrupted_reconcile_and_expired() {
        use axum::{extract::RawQuery, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
        use std::sync::atomic::{AtomicBool, AtomicUsize};

        let fail_page2 = Arc::new(AtomicBool::new(true));
        let fail_page2_c = Arc::clone(&fail_page2);
        let expire_token = Arc::new(AtomicBool::new(false));
        let expire_token_c = Arc::clone(&expire_token);
        let post_expire_removed = Arc::new(AtomicBool::new(false));
        let post_expire_removed_c = Arc::clone(&post_expire_removed);
        let delta_calls = Arc::new(AtomicUsize::new(0));
        let delta_calls_c = Arc::clone(&delta_calls);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_for_router = format!("http://{addr}/v1.0");
        let base_c = base_for_router.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "fld-inbox", "displayName": "Inbox", "wellKnownName": "inbox", "childFolderCount": 0, "unreadItemCount": 1, "totalItemCount": 6 },
                            { "id": "fld-archive", "displayName": "Archive", "wellKnownName": "archive", "childFolderCount": 0, "unreadItemCount": 0, "totalItemCount": 1 }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/mailFolders/fld-inbox/messages/delta",
                get(move |RawQuery(query): RawQuery| {
                    let q = query.unwrap_or_default();
                    let fail_p2 = Arc::clone(&fail_page2_c);
                    let expire = Arc::clone(&expire_token_c);
                    let after_exp = Arc::clone(&post_expire_removed_c);
                    let calls = Arc::clone(&delta_calls_c);
                    let base = base_c.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if q.contains("deltatoken=d1") {
                            if expire.load(Ordering::SeqCst) {
                                return (
                                    StatusCode::GONE,
                                    Json(serde_json::json!({
                                        "error": { "code": "syncStateNotFound", "message": "Delta token expired" }
                                    })),
                                )
                                    .into_response();
                            }
                            return Json(serde_json::json!({
                                "value": [
                                    { "id": "m-page1", "@removed": { "reason": "deleted" } },
                                    { "id": "m-moved-same-id", "@removed": { "reason": "deleted" } }
                                ],
                                "@odata.deltaLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d2")
                            }))
                            .into_response();
                        }

                        if q.contains("page=2") {
                            if fail_p2.load(Ordering::SeqCst) {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(serde_json::json!({
                                        "error": { "code": "InternalServerError", "message": "Simulated mid-delta failure" }
                                    })),
                                )
                                    .into_response();
                            }
                            return Json(serde_json::json!({
                                "value": [{
                                    "id": "m-page2",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Page 2 Mail",
                                    "receivedDateTime": "2026-11-18T10:02:00Z",
                                    "isRead": true
                                }],
                                "@odata.nextLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?page=3")
                            }))
                            .into_response();
                        }
                        if q.contains("page=3") {
                            return Json(serde_json::json!({
                                "value": [{
                                    "id": "m-page3",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Page 3 Mail",
                                    "receivedDateTime": "2026-11-18T10:03:00Z",
                                    "isRead": true
                                }],
                                "@odata.nextLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?page=4")
                            }))
                            .into_response();
                        }
                        if q.contains("page=4") {
                            return Json(serde_json::json!({
                                "value": [{
                                    "id": "m-page4",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Page 4 Mail",
                                    "receivedDateTime": "2026-11-18T10:04:00Z",
                                    "isRead": true
                                }],
                                "@odata.nextLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?page=5")
                            }))
                            .into_response();
                        }
                        if q.contains("page=5") {
                            let val = if after_exp.load(Ordering::SeqCst) {
                                serde_json::json!([])
                            } else {
                                serde_json::json!([{
                                    "id": "m-page5",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Page 5 Mail",
                                    "receivedDateTime": "2026-11-18T10:05:00Z",
                                    "isRead": false
                                }])
                            };
                            return Json(serde_json::json!({
                                "value": val,
                                "@odata.deltaLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1")
                            }))
                            .into_response();
                        }

                        // Page 1 of initial delta
                        Json(serde_json::json!({
                            "value": [{
                                "id": "m-page1",
                                "parentFolderId": "fld-inbox",
                                "subject": "Page 1 Mail",
                                "receivedDateTime": "2026-11-18T10:01:00Z",
                                "isRead": false
                            }],
                            "@odata.nextLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?page=2")
                        }))
                        .into_response()
                    }
                }),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        // Seed a stale ghost in inbox AND a moved item already in archive (same ID)
        storage
            .upsert_mail_folders(&[
                MailFolder {
                    id: "fld-inbox".into(),
                    display_name: "Inbox".into(),
                    well_known_name: Some("inbox".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 1,
                    total_item_count: 1,
                },
                MailFolder {
                    id: "fld-archive".into(),
                    display_name: "Archive".into(),
                    well_known_name: Some("archive".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 1,
                },
            ])
            .unwrap();
        let make_seed = |id: &str, folder: &str| MailMessage {
            id: id.into(),
            internet_message_id: None,
            conversation_id: None,
            folder_id: Some(folder.into()),
            subject: format!("Seed {id}"),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-18T09:00:00Z".into(),
            sent_at: None,
            last_modified_at: None,
            is_read: true,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: None,
            has_attachments: false,
            body_loaded: false,
            preview: String::new(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        };
        storage
            .upsert_mail_messages(&[
                make_seed("m-stale-ghost", "fld-inbox"),
                make_seed("m-moved-same-id", "fld-archive"),
            ])
            .unwrap();

        // 1. Interrupted initial delta (page 2 returns 500) must NOT prune existing cache or save a cursor
        let err = sync_mail_folder_delta("inbox", &client, &storage, None, false).await;
        assert!(err.is_err());
        assert!(storage.get_mail_message("m-stale-ghost").unwrap().is_some());
        assert!(storage.get_sync_cursor("mail_delta:fld-inbox").unwrap().is_none());

        // 2. Retry succeeds across all 5 pages (> old DELTA_MAX_PAGES=4), reconciles stale ghost,
        // preserves archive item, and stores real-folder-ID delta cursor.
        fail_page2.store(false, Ordering::SeqCst);
        let outcome = sync_mail_folder_delta("inbox", &client, &storage, None, false)
            .await
            .unwrap();
        assert_eq!(outcome.folder_id, "fld-inbox");
        assert!(storage.get_mail_message("m-stale-ghost").unwrap().is_none());
        assert!(storage.get_mail_message("m-page5").unwrap().is_some());
        assert_eq!(
            storage
                .get_mail_message("m-moved-same-id")
                .unwrap()
                .unwrap()
                .folder_id
                .as_deref(),
            Some("fld-archive")
        );
        assert!(storage
            .get_sync_cursor("mail_delta:fld-inbox")
            .unwrap()
            .unwrap()
            .delta_link
            .as_deref()
            .is_some_and(|l| l.contains("deltatoken=d1")));

        // 3. Incremental delta deletes m-page1 in inbox, but late @removed for m-moved-same-id
        // preserves it in fld-archive!
        sync_mail_folder_delta("inbox", &client, &storage, None, true)
            .await
            .unwrap();
        assert!(storage.get_mail_message("m-page1").unwrap().is_none());
        assert_eq!(
            storage
                .get_mail_message("m-moved-same-id")
                .unwrap()
                .unwrap()
                .folder_id
                .as_deref(),
            Some("fld-archive"),
            "late source-folder @removed must preserve item already moved to archive"
        );

        // 4. Expired delta token (410) triggers full re-bootstrap and reconciles m-page5 removed remotely
        storage
            .upsert_sync_cursor(&SyncCursor {
                scope_key: "mail_delta:fld-inbox".into(),
                delta_link: Some(format!(
                    "http://{addr}/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1"
                )),
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();
        expire_token.store(true, Ordering::SeqCst);
        post_expire_removed.store(true, Ordering::SeqCst);
        sync_mail_folder_delta("inbox", &client, &storage, None, false)
            .await
            .unwrap();
        assert!(storage.get_mail_message("m-page5").unwrap().is_none());
        assert!(storage.get_mail_message("m-page2").unwrap().is_some());
        assert!(storage.get_mail_message("m-moved-same-id").unwrap().is_some());
    }

    #[tokio::test]
    async fn test_mail_delta_bootstrap_501_pages_cyclic_and_partial_safety() {
        use axum::{
            extract::Query,
            http::StatusCode,
            response::IntoResponse,
            routing::get,
            Json, Router,
        };
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

        const TOTAL_PAGES: usize = 501;
        // mode: 0 = cyclic nextLink at page 3 -> page 2; 1 = 500 error at page 250; 2 = healthy 501 pages
        let mode = Arc::new(AtomicU8::new(0));
        let delta_hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let base_for_router = base_url.clone();

        let mode_r = Arc::clone(&mode);
        let hits_r = Arc::clone(&delta_hits);
        let app = Router::new().route(
            "/v1.0/me/mailFolders/fld-large/messages/delta",
            get(move |Query(params): Query<HashMap<String, String>>| {
                let mode = Arc::clone(&mode_r);
                let hits = Arc::clone(&hits_r);
                let base = base_url.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if params.contains_key("$deltatoken") {
                        return Json(serde_json::json!({
                            "value": [],
                            "@odata.deltaLink": format!("{base}/me/mailFolders/fld-large/messages/delta?$deltatoken=large-d2")
                        }))
                        .into_response();
                    }

                    let page: usize = params
                        .get("page")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1);
                    let cur_mode = mode.load(Ordering::SeqCst);

                    if cur_mode == 1 && page == 250 {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": { "code": "InternalServerError", "message": "Page 250 transient failure" }
                            })),
                        )
                            .into_response();
                    }

                    let item = serde_json::json!({
                        "id": format!("large-m-{page:04}"),
                        "parentFolderId": "fld-large",
                        "subject": format!("Large Folder Mail {page}"),
                        "receivedDateTime": "2026-11-20T05:00:00Z",
                        "isRead": true
                    });

                    if cur_mode == 0 && page >= 3 {
                        return Json(serde_json::json!({
                            "value": [item],
                            "@odata.nextLink": format!("{base}/me/mailFolders/fld-large/messages/delta?page=2")
                        }))
                        .into_response();
                    }

                    if page < TOTAL_PAGES {
                        Json(serde_json::json!({
                            "value": [item],
                            "@odata.nextLink": format!("{base}/me/mailFolders/fld-large/messages/delta?page={}", page + 1)
                        }))
                        .into_response()
                    } else {
                        Json(serde_json::json!({
                            "value": [item],
                            "@odata.deltaLink": format!("{base}/me/mailFolders/fld-large/messages/delta?$deltatoken=large-d1")
                        }))
                        .into_response()
                    }
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-large".into(),
                display_name: "Large Archive".into(),
                well_known_name: None,
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: TOTAL_PAGES as i32,
            }])
            .unwrap();
        storage
            .upsert_mail_messages(&[MailMessage {
                id: "m-orphan-large".into(),
                internet_message_id: None,
                conversation_id: None,
                folder_id: Some("fld-large".into()),
                subject: "Prior Cached Orphan".into(),
                from: None,
                to_recipients: vec![],
                cc_recipients: vec![],
                bcc_recipients: vec![],
                reply_to: vec![],
                received_at: "2026-11-19T00:00:00Z".into(),
                sent_at: None,
                last_modified_at: None,
                is_read: true,
                is_draft: false,
                flagged: false,
                importance: "normal".into(),
                inference_classification: None,
                has_attachments: false,
                body_loaded: false,
                preview: String::new(),
                body_html: String::new(),
                body_plain: String::new(),
                attachments: vec![],
                meeting_request: None,
                web_link: None,
            }])
            .unwrap();

        // 1. Cyclic nextLink fails fast and preserves prior cached row + empty delta_link
        let cyc_err = sync_mail_folder_delta_with_page_size("fld-large", &client, &storage, None, false, 1)
            .await
            .expect_err("cyclic nextLink must fail");
        assert!(format!("{cyc_err:#}").contains("Cyclic or repeated @odata.nextLink"));
        assert_eq!(delta_hits.load(Ordering::SeqCst), 3);
        assert!(storage.get_mail_message("m-orphan-large").unwrap().is_some());
        assert!(storage.get_sync_cursor("mail_delta:fld-large").unwrap().is_none());

        // 2. Mid-stream 500 error at page 250 preserves prior cached row + empty delta_link
        mode.store(1, Ordering::SeqCst);
        delta_hits.store(0, Ordering::SeqCst);
        let err_250 = sync_mail_folder_delta_with_page_size("fld-large", &client, &storage, None, false, 1)
            .await
            .expect_err("page 250 error must abort bootstrap");
        assert!(format!("{err_250:#}").contains("500") || format!("{err_250:#}").contains("InternalServerError"));
        assert_eq!(delta_hits.load(Ordering::SeqCst), 250);
        assert!(storage.get_mail_message("m-orphan-large").unwrap().is_some());
        assert!(storage.get_sync_cursor("mail_delta:fld-large").unwrap().is_none());

        // 3. Healthy 501-page bootstrap reaches terminal deltaLink, caches all 501 rows (header-only),
        // prunes m-orphan-large, and persists deltaLink so follow-up sync takes 1 incremental call.
        mode.store(2, Ordering::SeqCst);
        delta_hits.store(0, Ordering::SeqCst);
        let outcome = sync_mail_folder_delta_with_page_size("fld-large", &client, &storage, None, false, 1)
            .await
            .expect("501-page bootstrap must succeed");
        assert_eq!(outcome.folder_id, "fld-large");
        assert_eq!(delta_hits.load(Ordering::SeqCst), TOTAL_PAGES);
        assert!(storage.get_mail_message("m-orphan-large").unwrap().is_none());
        let first_msg = storage.get_mail_message("large-m-0001").unwrap().unwrap();
        let last_msg = storage.get_mail_message("large-m-0501").unwrap().unwrap();
        assert!(!first_msg.body_loaded && !last_msg.body_loaded);
        assert_eq!(
            storage
                .list_mail_summaries(Some("fld-large"), 600)
                .unwrap()
                .len(),
            TOTAL_PAGES
        );
        let saved_cur = storage
            .get_sync_cursor("mail_delta:fld-large")
            .unwrap()
            .expect("terminal deltaLink must be persisted");
        assert!(saved_cur
            .delta_link
            .as_deref()
            .is_some_and(|l| l.contains("$deltatoken=large-d1")));

        // 4. Subsequent sync uses saved deltaLink in 1 call (never replays pages 1..500)
        delta_hits.store(0, Ordering::SeqCst);
        sync_mail_folder_delta_with_page_size("fld-large", &client, &storage, None, true, 1)
            .await
            .unwrap();
        assert_eq!(delta_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_move_same_id_then_late_source_tombstone_preserves_destination_row_and_fts() {
        use axum::{routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let base_for_router = base_url.clone();

        let app = Router::new().route(
            "/v1.0/me/mailFolders/fld-inbox/messages/delta",
            get(move || {
                let base = base_url.clone();
                async move {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "m-same-move", "@removed": { "reason": "deleted" } }
                        ],
                        "@odata.deltaLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=after-tombstone")
                    }))
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[
                MailFolder {
                    id: "fld-inbox".into(),
                    display_name: "Inbox".into(),
                    well_known_name: Some("inbox".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 1,
                    total_item_count: 1,
                },
                MailFolder {
                    id: "fld-archive".into(),
                    display_name: "Archive".into(),
                    well_known_name: Some("archive".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 0,
                },
            ])
            .unwrap();

        let orig = MailMessage {
            id: "m-same-move".into(),
            internet_message_id: Some("<same@example.com>".into()),
            conversation_id: Some("conv-same".into()),
            folder_id: Some("fld-inbox".into()),
            subject: "Immutable ID Quarterly Report".into(),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-20T05:00:00Z".into(),
            sent_at: None,
            last_modified_at: Some("2026-11-20T05:00:00Z".into()),
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: None,
            has_attachments: false,
            body_loaded: true,
            preview: "Quarterly report preview".into(),
            body_html: "<p>Quarterly report full body</p>".into(),
            body_plain: "Quarterly report full body".into(),
            attachments: vec![],
            meeting_request: None,
            web_link: None,
        };
        storage.upsert_mail_messages(&[orig.clone()]).unwrap();
        storage
            .upsert_sync_cursor(&SyncCursor {
                scope_key: "mail_delta:fld-inbox".into(),
                delta_link: Some(format!(
                    "http://{addr}/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=before-move"
                )),
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();

        // Move m-same-move to fld-archive keeping the EXACT same ID (ImmutableId / in-place move)
        let mut moved = orig.clone();
        moved.folder_id = Some("fld-archive".into());
        moved.body_loaded = false;
        moved.body_html.clear();
        moved.body_plain.clear();
        storage
            .replace_moved_mail_local("m-same-move", &moved)
            .unwrap();

        // Late source-folder delta emits @removed tombstone for m-same-move
        sync_mail_folder_delta("inbox", &client, &storage, None, true)
            .await
            .unwrap();

        let survived = storage
            .get_mail_message("m-same-move")
            .unwrap()
            .expect("late source-folder tombstone must NOT delete same ID now in archive");
        assert_eq!(survived.folder_id.as_deref(), Some("fld-archive"));
        assert!(survived.body_loaded);
        assert_eq!(survived.body_html, "<p>Quarterly report full body</p>");

        let archive_hits = storage
            .search_mail("Quarterly", Some("archive"), 10)
            .unwrap();
        assert_eq!(archive_hits.len(), 1);
        assert_eq!(archive_hits[0].id, "m-same-move");
        let inbox_hits = storage.search_mail("Quarterly", Some("inbox"), 10).unwrap();
        assert!(inbox_hits.is_empty());
    }

    #[tokio::test]
    async fn test_hydrate_refetch_changed_vs_unchanged_body_after_delta() {
        use axum::{extract::Path as AxumPath, routing::get, Json, Router};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let phase_v2 = Arc::new(AtomicBool::new(false));
        let unchanged_gets = Arc::new(AtomicUsize::new(0));
        let older_gets = Arc::new(AtomicUsize::new(0));
        let combined_gets = Arc::new(AtomicUsize::new(0));
        let changed_content_gets = Arc::new(AtomicUsize::new(0));
        let changed_body_only_gets = Arc::new(AtomicUsize::new(0));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let base_for_router = base_url.clone();

        let p2_msg = Arc::clone(&phase_v2);
        let u_gets = Arc::clone(&unchanged_gets);
        let o_gets = Arc::clone(&older_gets);
        let cm_gets = Arc::clone(&combined_gets);
        let cc_gets = Arc::clone(&changed_content_gets);
        let cbo_gets = Arc::clone(&changed_body_only_gets);

        let app = Router::new()
            .route(
                "/v1.0/me/messages/{id}",
                get(move |AxumPath(id): AxumPath<String>| {
                    let p2 = p2_msg.load(Ordering::SeqCst);
                    let u_gets = Arc::clone(&u_gets);
                    let o_gets = Arc::clone(&o_gets);
                    let cm_gets = Arc::clone(&cm_gets);
                    let cc_gets = Arc::clone(&cc_gets);
                    let cbo_gets = Arc::clone(&cbo_gets);
                    async move {
                        match id.as_str() {
                            "m-unchanged" => {
                                u_gets.fetch_add(1, Ordering::SeqCst);
                                Json(serde_json::json!({
                                    "id": "m-unchanged",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Stable Subject",
                                    "receivedDateTime": "2026-11-20T05:00:00Z",
                                    "lastModifiedDateTime": "2026-11-20T05:00:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Stable preview",
                                    "body": { "contentType": "html", "content": "<p>Unchanged v1 body</p>" }
                                }))
                            }
                            "m-older" => {
                                o_gets.fetch_add(1, Ordering::SeqCst);
                                Json(serde_json::json!({
                                    "id": "m-older",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Newer Cached Subject",
                                    "receivedDateTime": "2026-11-20T05:00:30Z",
                                    "lastModifiedDateTime": "2026-11-20T05:30:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Newer preview",
                                    "body": { "contentType": "html", "content": "<p>Newer cached body</p>" }
                                }))
                            }
                            "m-combined" => {
                                cm_gets.fetch_add(1, Ordering::SeqCst);
                                Json(serde_json::json!({
                                    "id": "m-combined",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Combined Edit Subject",
                                    "receivedDateTime": "2026-11-20T05:00:45Z",
                                    "lastModifiedDateTime": if p2 { "2026-11-20T06:00:00Z" } else { "2026-11-20T05:00:45Z" },
                                    "isRead": !p2,
                                    "bodyPreview": "Same leading preview",
                                    "body": { "contentType": "html", "content": if p2 { "<p>Combined UPDATED body v2</p>" } else { "<p>Combined old body v1</p>" } }
                                }))
                            }
                            "m-changed-content" => {
                                cc_gets.fetch_add(1, Ordering::SeqCst);
                                Json(serde_json::json!({
                                    "id": "m-changed-content",
                                    "parentFolderId": "fld-inbox",
                                    "subject": if p2 { "Edited Subject v2" } else { "Edited Subject v1" },
                                    "receivedDateTime": "2026-11-20T05:01:00Z",
                                    "lastModifiedDateTime": if p2 { "2026-11-20T06:00:00Z" } else { "2026-11-20T05:01:00Z" },
                                    "isRead": true,
                                    "bodyPreview": if p2 { "Edited preview v2" } else { "Edited preview v1" },
                                    "body": { "contentType": "html", "content": if p2 { "<p>Edited body v2</p>" } else { "<p>Edited body v1</p>" } }
                                }))
                            }
                            "m-changed-body-only" => {
                                cbo_gets.fetch_add(1, Ordering::SeqCst);
                                Json(serde_json::json!({
                                    "id": "m-changed-body-only",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Draft With Body Edit",
                                    "receivedDateTime": "2026-11-20T05:02:00Z",
                                    "lastModifiedDateTime": if p2 { "2026-11-20T06:00:00Z" } else { "2026-11-20T05:02:00Z" },
                                    "isRead": true,
                                    "bodyPreview": "Same leading preview",
                                    "body": { "contentType": "html", "content": if p2 { "<p>Same leading preview + UPDATED tail v2</p>" } else { "<p>Same leading preview + old tail v1</p>" } }
                                }))
                            }
                            _ => Json(serde_json::json!({})),
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/fld-inbox/messages/delta",
                get(move || {
                    let base = base_url.clone();
                    async move {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m-unchanged",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Stable Subject",
                                    "receivedDateTime": "2026-11-20T05:00:00Z",
                                    "lastModifiedDateTime": "2026-11-20T05:00:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Stable preview"
                                },
                                {
                                    "id": "m-older",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Stale Older Subject",
                                    "receivedDateTime": "2026-11-20T05:00:30Z",
                                    "lastModifiedDateTime": "2026-11-20T04:00:00Z",
                                    "isRead": false,
                                    "bodyPreview": "Stale preview"
                                },
                                {
                                    "id": "m-combined",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Combined Edit Subject",
                                    "receivedDateTime": "2026-11-20T05:00:45Z",
                                    "lastModifiedDateTime": "2026-11-20T06:00:00Z",
                                    "isRead": false,
                                    "bodyPreview": "Same leading preview"
                                },
                                {
                                    "id": "m-changed-content",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Edited Subject v2",
                                    "receivedDateTime": "2026-11-20T05:01:00Z",
                                    "lastModifiedDateTime": "2026-11-20T06:00:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Edited preview v2"
                                },
                                {
                                    "id": "m-changed-body-only",
                                    "parentFolderId": "fld-inbox",
                                    "subject": "Draft With Body Edit",
                                    "receivedDateTime": "2026-11-20T05:02:00Z",
                                    "lastModifiedDateTime": "2026-11-20T06:00:00Z",
                                    "isRead": true,
                                    "bodyPreview": "Same leading preview"
                                }
                            ],
                            "@odata.deltaLink": format!("{base}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d2")
                        }))
                    }
                }),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 5,
            }])
            .unwrap();
        storage
            .upsert_sync_cursor(&SyncCursor {
                scope_key: "mail_delta:fld-inbox".into(),
                delta_link: Some(format!(
                    "http://{addr}/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1"
                )),
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();

        let all_ids = [
            "m-unchanged",
            "m-older",
            "m-combined",
            "m-changed-content",
            "m-changed-body-only",
        ];

        // Initial hydration of all 5 messages (1 GET each)
        for id in all_ids {
            hydrate_message_if_needed(id, false, &client, &storage, &event_tx)
                .await
                .unwrap();
        }
        assert_eq!(unchanged_gets.load(Ordering::SeqCst), 1);
        assert_eq!(older_gets.load(Ordering::SeqCst), 1);
        assert_eq!(combined_gets.load(Ordering::SeqCst), 1);
        assert_eq!(changed_content_gets.load(Ordering::SeqCst), 1);
        assert_eq!(changed_body_only_gets.load(Ordering::SeqCst), 1);

        // Second non-forced hydration before any changes must be 0 GETs for all 5
        for id in all_ids {
            hydrate_message_if_needed(id, false, &client, &storage, &event_tx)
                .await
                .unwrap();
        }
        assert_eq!(unchanged_gets.load(Ordering::SeqCst), 1);
        assert_eq!(older_gets.load(Ordering::SeqCst), 1);
        assert_eq!(combined_gets.load(Ordering::SeqCst), 1);
        assert_eq!(changed_content_gets.load(Ordering::SeqCst), 1);
        assert_eq!(changed_body_only_gets.load(Ordering::SeqCst), 1);

        // Drain initial events, advance server to v2, and run incremental header delta
        while event_rx.try_recv().is_ok() {}
        phase_v2.store(true, Ordering::SeqCst);
        sync_mail_folder_delta("inbox", &client, &storage, None, true)
            .await
            .unwrap();

        assert!(storage.get_mail_message("m-unchanged").unwrap().unwrap().body_loaded);
        assert!(storage.get_mail_message("m-older").unwrap().unwrap().body_loaded);
        assert!(!storage.get_mail_message("m-combined").unwrap().unwrap().body_loaded);
        assert!(!storage.get_mail_message("m-changed-content").unwrap().unwrap().body_loaded);
        assert!(!storage.get_mail_message("m-changed-body-only").unwrap().unwrap().body_loaded);

        // Non-forced hydration after delta:
        // - m-unchanged & m-older must NOT refetch (still 1 GET)
        // - m-combined, m-changed-content, m-changed-body-only MUST refetch (2 GETs each)
        for id in all_ids {
            hydrate_message_if_needed(id, false, &client, &storage, &event_tx)
                .await
                .unwrap();
        }
        assert_eq!(unchanged_gets.load(Ordering::SeqCst), 1);
        assert_eq!(older_gets.load(Ordering::SeqCst), 1);
        assert_eq!(
            combined_gets.load(Ordering::SeqCst),
            2,
            "combined metadata+body edit with newer lastModifiedDateTime must trigger refetch"
        );
        assert_eq!(changed_content_gets.load(Ordering::SeqCst), 2);
        assert_eq!(changed_body_only_gets.load(Ordering::SeqCst), 2);

        let u = storage.get_mail_message("m-unchanged").unwrap().unwrap();
        assert!(u.is_read && u.body_loaded && u.body_html == "<p>Unchanged v1 body</p>");
        let o = storage.get_mail_message("m-older").unwrap().unwrap();
        assert!(o.is_read && o.body_loaded && o.body_html == "<p>Newer cached body</p>");
        let cm = storage.get_mail_message("m-combined").unwrap().unwrap();
        assert!(!cm.is_read && cm.body_loaded && cm.body_html == "<p>Combined UPDATED body v2</p>");
        let cc = storage.get_mail_message("m-changed-content").unwrap().unwrap();
        assert!(cc.body_loaded && cc.body_html == "<p>Edited body v2</p>");
        let cbo = storage.get_mail_message("m-changed-body-only").unwrap().unwrap();
        assert!(cbo.body_loaded && cbo.body_html == "<p>Same leading preview + UPDATED tail v2</p>");

        let mut hydrated_event_ids = Vec::new();
        while let Ok(ev) = event_rx.try_recv() {
            if let CoreEvent::MailMessageHydrated(m) = ev {
                hydrated_event_ids.push(m.id);
            }
        }
        assert_eq!(
            hydrated_event_ids,
            vec![
                "m-combined".to_string(),
                "m-changed-content".to_string(),
                "m-changed-body-only".to_string()
            ]
        );

        // Re-delivering the exact same delta must NOT invalidate or trigger any additional GETs.
        sync_mail_folder_delta("inbox", &client, &storage, None, true)
            .await
            .unwrap();
        for id in all_ids {
            hydrate_message_if_needed(id, false, &client, &storage, &event_tx)
                .await
                .unwrap();
        }
        assert_eq!(unchanged_gets.load(Ordering::SeqCst), 1);
        assert_eq!(older_gets.load(Ordering::SeqCst), 1);
        assert_eq!(combined_gets.load(Ordering::SeqCst), 2);
        assert_eq!(changed_content_gets.load(Ordering::SeqCst), 2);
        assert_eq!(changed_body_only_gets.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_deduper_coalesces_overlapping_folder_and_calendar_syncs() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let folder_hits = Arc::new(AtomicUsize::new(0));
        let cal_hits = Arc::new(AtomicUsize::new(0));
        let fh = Arc::clone(&folder_hits);
        let ch = Arc::clone(&cal_hits);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let base_for_router = base_url.clone();
        let b1 = base_url.clone();
        let b2 = base_url.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders/fld-inbox/messages/delta",
                get(move || {
                    let fh = Arc::clone(&fh);
                    let b = b1.clone();
                    async move {
                        fh.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Json(serde_json::json!({
                            "value": [],
                            "@odata.deltaLink": format!("{b}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d2")
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/calendarView/delta",
                get(move || {
                    let ch = Arc::clone(&ch);
                    let b = b2.clone();
                    async move {
                        ch.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Json(serde_json::json!({
                            "value": [],
                            "@odata.deltaLink": format!("{b}/me/calendarView/delta?$deltatoken=c2")
                        }))
                    }
                }),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 0,
            }])
            .unwrap();
        storage
            .upsert_sync_cursor(&SyncCursor {
                scope_key: "mail_delta:fld-inbox".into(),
                delta_link: Some(format!(
                    "http://{addr}/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1"
                )),
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut cfg = AppConfig::default();
        cfg.active_poll_ms = 5_000;
        cfg.mail_poll_ms = 15_000;
        cfg.calendar_poll_ms = 60_000;
        let ctrl = spawn_sync_engine(cfg, client, storage, event_tx);

        // Overlapping set_focus + SyncFolder + duplicate SyncFolder must run only 1 delta
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Mail,
            active_folder_id: Some("fld-inbox".into()),
            active_message_id: None,
            active_todo_list_id: None,
        });
        ctrl.request(SyncCommand::SyncFolder {
            folder_id: "fld-inbox".into(),
        });
        ctrl.request(SyncCommand::SyncFolder {
            folder_id: "inbox".into(),
        });

        // Overlapping SyncCalendar bursts must run only 1 calendarView/delta
        ctrl.request(SyncCommand::SyncCalendar);
        ctrl.request(SyncCommand::SyncCalendar);

        tokio::time::sleep(Duration::from_millis(220)).await;
        assert_eq!(folder_hits.load(Ordering::SeqCst), 1);
        assert_eq!(cal_hits.load(Ordering::SeqCst), 1);
        ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn test_lane2_skips_inbox_delta_when_inbox_is_active_mail_folder() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let folders_hits = Arc::new(AtomicUsize::new(0));
        let inbox_delta_hits = Arc::new(AtomicUsize::new(0));
        let fh = Arc::clone(&folders_hits);
        let idh = Arc::clone(&inbox_delta_hits);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let base_for_router = base_url.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(move || {
                    let fh = Arc::clone(&fh);
                    async move {
                        fh.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [{
                                "id": "fld-inbox",
                                "displayName": "Inbox",
                                "wellKnownName": "inbox",
                                "childFolderCount": 0,
                                "unreadItemCount": 0,
                                "totalItemCount": 0
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/fld-inbox/messages/delta",
                get(move || {
                    let idh = Arc::clone(&idh);
                    let b = base_url.clone();
                    async move {
                        idh.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [],
                            "@odata.deltaLink": format!("{b}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1")
                        }))
                    }
                }),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 0,
            }])
            .unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let shared = SyncSharedState::new();
        let mut mail_backoff = ErrorBackoffState::new(15_000);
        let slow_backoff = ErrorBackoffState::new(60_000);
        let mut last_folders = None;

        assert!(is_inbox_folder_active(Some("fld-inbox"), &storage).await);
        assert!(is_inbox_folder_active(Some("inbox"), &storage).await);
        assert!(!is_inbox_folder_active(Some("fld-sent"), &storage).await);
        assert!(!is_inbox_folder_active(None, &storage).await);

        // When Inbox is active, Lane 2 polls folders only (0 duplicate Inbox delta calls).
        poll_mail_lane2_quiet_with_dedup(
            &client,
            &storage,
            &event_tx,
            &shared,
            &mut mail_backoff,
            &slow_backoff,
            &mut last_folders,
            false,
            None,
        )
        .await;
        assert_eq!(folders_hits.load(Ordering::SeqCst), 1);
        assert_eq!(inbox_delta_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_visible_view_retains_full_cadence_past_2m_and_10m_idle_while_background_throttles() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let inbox_hits = Arc::new(AtomicUsize::new(0));
        let folders_hits = Arc::new(AtomicUsize::new(0));
        let cal_hits = Arc::new(AtomicUsize::new(0));
        let todo_hits = Arc::new(AtomicUsize::new(0));
        let inbox_status = Arc::new(AtomicUsize::new(200));
        let cal_status = Arc::new(AtomicUsize::new(200));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let base_for_router = base_url.clone();

        let ih = Arc::clone(&inbox_hits);
        let fh = Arc::clone(&folders_hits);
        let ch = Arc::clone(&cal_hits);
        let th = Arc::clone(&todo_hits);
        let is_code = Arc::clone(&inbox_status);
        let cs_code = Arc::clone(&cal_status);
        let b1 = base_url.clone();
        let b2 = base_url.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/mailFolders",
                get(move || {
                    let fh = Arc::clone(&fh);
                    async move {
                        fh.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [{
                                "id": "fld-inbox",
                                "displayName": "Inbox",
                                "wellKnownName": "inbox",
                                "childFolderCount": 0,
                                "unreadItemCount": 0,
                                "totalItemCount": 0
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/mailFolders/fld-inbox/messages/delta",
                get(move || {
                    let ih = Arc::clone(&ih);
                    let code = is_code.load(Ordering::SeqCst);
                    let b = b1.clone();
                    async move {
                        ih.fetch_add(1, Ordering::SeqCst);
                        if code == 503 {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(serde_json::json!({ "error": { "code": "ServiceUnavailable", "message": "busy" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [],
                                    "@odata.deltaLink": format!("{b}/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d1")
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/calendarView/delta",
                get(move || {
                    let ch = Arc::clone(&ch);
                    let code = cs_code.load(Ordering::SeqCst);
                    let b = b2.clone();
                    async move {
                        ch.fetch_add(1, Ordering::SeqCst);
                        if code == 401 {
                            (
                                StatusCode::UNAUTHORIZED,
                                Json(serde_json::json!({ "error": { "code": "InvalidAuthenticationToken", "message": "Token expired" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [],
                                    "@odata.deltaLink": format!("{b}/me/calendarView/delta?$deltatoken=c1")
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/todo/lists",
                get(move || {
                    let th = Arc::clone(&th);
                    async move {
                        th.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [{
                                "id": "todo-1",
                                "displayName": "Tasks",
                                "wellknownListName": "defaultList",
                                "isOwner": true,
                                "isShared": false
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/todo/lists/todo-1/tasks",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/contacts",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = mock_client(base_for_router);
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 0,
            }])
            .unwrap();
        storage
            .upsert_sync_cursor(&SyncCursor {
                scope_key: "mail_delta:fld-inbox".into(),
                delta_link: Some(format!(
                    "http://{addr}/v1.0/me/mailFolders/fld-inbox/messages/delta?$deltatoken=d0"
                )),
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut cfg = AppConfig::default();
        cfg.active_poll_ms = 250;
        cfg.mail_poll_ms = 500;
        cfg.calendar_poll_ms = 1_000;
        let ctrl = spawn_sync_engine(cfg, client, storage, event_tx);

        // 1. Visible Mail at >2 min (180 s) and >10 min (720 s) idle retains full 250 ms cadence
        //    while background mail_tick (folders) and slow_tick (calendar) are throttled.
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Mail,
            active_folder_id: Some("fld-inbox".into()),
            active_message_id: None,
            active_todo_list_id: None,
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        ctrl.set_idle_for_test(Duration::from_secs(180));
        inbox_hits.store(0, Ordering::SeqCst);
        folders_hits.store(0, Ordering::SeqCst);
        cal_hits.store(0, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(850)).await;
        assert!(
            inbox_hits.load(Ordering::SeqCst) >= 2,
            "visible Mail must stay on active cadence past 2 min idle, got {}",
            inbox_hits.load(Ordering::SeqCst)
        );
        assert_eq!(folders_hits.load(Ordering::SeqCst), 0, "background mail_tick must throttle at >2m idle");
        assert_eq!(cal_hits.load(Ordering::SeqCst), 0, "background slow_tick must throttle at >2m idle");

        ctrl.set_idle_for_test(Duration::from_secs(720));
        inbox_hits.store(0, Ordering::SeqCst);
        folders_hits.store(0, Ordering::SeqCst);
        cal_hits.store(0, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(850)).await;
        assert!(
            inbox_hits.load(Ordering::SeqCst) >= 2,
            "visible Mail must stay on active cadence past 10 min idle, got {}",
            inbox_hits.load(Ordering::SeqCst)
        );
        assert_eq!(folders_hits.load(Ordering::SeqCst), 0, "background mail_tick must throttle at >10m idle");
        assert_eq!(cal_hits.load(Ordering::SeqCst), 0, "background slow_tick must throttle at >10m idle");

        // 2. Visible Calendar at >10 min (720 s) idle retains full active_slow_ms (1000 ms) cadence
        //    while non-visible Mail & Tasks stay throttled.
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Calendar,
            active_folder_id: Some("fld-inbox".into()),
            active_message_id: None,
            active_todo_list_id: Some("todo-1".into()),
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        ctrl.set_idle_for_test(Duration::from_secs(720));
        inbox_hits.store(0, Ordering::SeqCst);
        cal_hits.store(0, Ordering::SeqCst);
        todo_hits.store(0, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(2_350)).await;
        assert!(
            cal_hits.load(Ordering::SeqCst) >= 2,
            "visible Calendar must stay on active cadence past 10 min idle, got {}",
            cal_hits.load(Ordering::SeqCst)
        );
        assert_eq!(inbox_hits.load(Ordering::SeqCst), 0, "non-visible Mail must stay throttled at >10m idle");
        assert_eq!(todo_hits.load(Ordering::SeqCst), 0, "non-visible Tasks must stay throttled at >10m idle");

        // 3. Visible Tasks at >10 min (720 s) idle retains full active_slow_ms (1000 ms) cadence.
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Tasks,
            active_folder_id: Some("fld-inbox".into()),
            active_message_id: None,
            active_todo_list_id: Some("todo-1".into()),
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        ctrl.set_idle_for_test(Duration::from_secs(720));
        cal_hits.store(0, Ordering::SeqCst);
        todo_hits.store(0, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(2_350)).await;
        assert!(
            todo_hits.load(Ordering::SeqCst) >= 2,
            "visible Tasks must stay on active cadence past 10 min idle, got {}",
            todo_hits.load(Ordering::SeqCst)
        );
        assert_eq!(cal_hits.load(Ordering::SeqCst), 0, "non-visible Calendar must stay throttled at >10m idle");

        // 4. Detached / headless FocusTarget::default() at >10 min idle performs 0 active polls.
        ctrl.set_focus(FocusTarget::default());
        tokio::time::sleep(Duration::from_millis(60)).await;
        ctrl.set_idle_for_test(Duration::from_secs(720));
        inbox_hits.store(0, Ordering::SeqCst);
        folders_hits.store(0, Ordering::SeqCst);
        cal_hits.store(0, Ordering::SeqCst);
        todo_hits.store(0, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1_250)).await;
        assert_eq!(inbox_hits.load(Ordering::SeqCst), 0);
        assert_eq!(folders_hits.load(Ordering::SeqCst), 0);
        assert_eq!(cal_hits.load(Ordering::SeqCst), 0);
        assert_eq!(todo_hits.load(Ordering::SeqCst), 0);

        // 5. Transient 503 error backoff & 401 auth pause remain intact on visible views.
        inbox_status.store(503, Ordering::SeqCst);
        inbox_hits.store(0, Ordering::SeqCst);
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Mail,
            active_folder_id: Some("fld-inbox".into()),
            active_message_id: None,
            active_todo_list_id: None,
        });
        tokio::time::sleep(Duration::from_millis(550)).await;
        let err_hits = inbox_hits.load(Ordering::SeqCst);
        assert!(
            err_hits <= 2,
            "503 error backoff must suppress rapid visible Mail retries, got {err_hits}"
        );

        cal_status.store(401, Ordering::SeqCst);
        cal_hits.store(0, Ordering::SeqCst);
        ctrl.set_focus(FocusTarget {
            active_view: ActiveView::Calendar,
            active_folder_id: None,
            active_message_id: None,
            active_todo_list_id: None,
        });
        ctrl.request(SyncCommand::SyncCalendar);
        tokio::time::sleep(Duration::from_millis(1_350)).await;
        assert_eq!(
            cal_hits.load(Ordering::SeqCst),
            1,
            "401 auth pause must suppress subsequent visible Calendar ticks"
        );

        ctrl.request(SyncCommand::Shutdown);
    }
}
