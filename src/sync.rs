//! Adaptive 3-tier background synchronization engine (active focus, chat preview diff + $batch, channel delta + presence).

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use anyhow::Result;
use tokio::{
    sync::{mpsc, watch},
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{
        ConversationKind, ConversationSummary, ConversationTarget, CoreEvent, MailMessage,
        MessageItem, NotificationCandidate, SyncCursor,
    },
    storage::Storage,
};

#[derive(Debug, Clone, Default)]
pub struct FocusTarget {
    pub target: Option<ConversationTarget>,
    pub thread_root_id: Option<String>,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
}

#[derive(Debug, Clone)]
pub enum SyncCommand {
    RefreshConversations,
    RefreshOutlook,
    SyncMailFolder {
        folder_id: String,
    },
    SyncTodoList {
        list_id: String,
    },
    SyncActiveNow,
    SyncChannelHistory {
        team_id: String,
        channel_id: String,
    },
    Shutdown,
}

impl SyncController {
    pub fn set_focus(&self, target: Option<ConversationTarget>, thread_root_id: Option<String>) {
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
}

/// Performs an initial bootstrap sync (chats + joined teams/channels) into SQLite.
pub async fn bootstrap_conversations(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let chats = graph.list_chats(50).await?;
    storage.upsert_conversations(&chats)?;
    if let Ok(teams) = graph.list_joined_teams_with_channels().await {
        storage.upsert_teams(&teams)?;
    }
    Ok(())
}

/// Performs an initial bootstrap sync of Outlook Mail folders/inbox, Calendar, Contacts, and To Do into SQLite.
/// Best-effort per service so Teams-only tokens without Outlook scopes never fail bootstrap.
pub async fn bootstrap_outlook(graph: &GraphClient, storage: &Storage) -> Result<()> {
    if let Ok(folders) = graph.list_mail_folders().await {
        storage.upsert_mail_folders(&folders)?;
    }

    if let Ok(messages) = graph.list_mail_messages("inbox", 40).await {
        storage.upsert_mail_messages(&messages)?;
    }

    let (start_iso, end_iso) = default_calendar_window();
    if let Ok(events) = graph.list_calendar_view(&start_iso, &end_iso, 50).await {
        storage.upsert_calendar_events(&events)?;
    }

    if let Ok(contacts) = graph.list_contacts(100).await {
        storage.upsert_contacts(&contacts)?;
    }

    if let Ok(lists) = graph.list_todo_lists().await {
        storage.upsert_todo_lists(&lists)?;
        if let Some(default_list) = pick_default_todo_list(&lists) {
            if let Ok(tasks) = graph.list_todo_tasks(&default_list.id, true, 50).await {
                storage.upsert_todo_tasks(&tasks)?;
            }
        }
    }

    Ok(())
}

/// Spawns the background sync loop and returns a `SyncController` handle.
pub fn spawn_sync_engine(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();

    let active_ms = config.active_poll_ms.max(250);
    let chat_ms = config.chat_poll_ms.max(500);
    let channel_ms = config.channel_poll_ms.max(1_000);

    tokio::spawn(async move {
        let mut active_tick = time::interval(Duration::from_millis(active_ms));
        active_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut chat_tick = time::interval(Duration::from_millis(chat_ms));
        chat_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut channel_tick = time::interval(Duration::from_millis(channel_ms));
        channel_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Consume initial immediate ticks so startup isn't flooded simultaneously before focus/commands arrive.
        active_tick.tick().await;
        chat_tick.tick().await;
        channel_tick.tick().await;

        loop {
            tokio::select! {
                cmd = nudge_rx.recv() => {
                    match cmd {
                        None | Some(SyncCommand::Shutdown) => break,
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
                            refresh_outlook_best_effort(&graph, &storage, &event_tx).await;
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "Outlook 동기화 완료".to_string(),
                            });
                        }
                        Some(SyncCommand::SyncMailFolder { folder_id }) => {
                            if let Err(err) = sync_mail_folder(&folder_id, &graph, &storage, &event_tx).await {
                                let _ = event_tx.send(CoreEvent::SyncError(format!("메일 폴더 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncTodoList { list_id }) => {
                            if let Err(err) = sync_todo_list(&list_id, &graph, &storage, &event_tx).await {
                                let _ = event_tx.send(CoreEvent::SyncError(format!("To Do 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncActiveNow) => {
                            let focus = focus_rx.borrow().clone();
                            if let Err(err) = sync_active_focus(&focus, &graph, &storage, &event_tx).await {
                                let _ = event_tx.send(CoreEvent::SyncError(format!("활성 대화 동기화 실패: {err}")));
                            }
                        }
                        Some(SyncCommand::SyncChannelHistory { team_id, channel_id }) => {
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "채널 히스토리 동기화 중...".to_string(),
                            });
                            if let Err(err) = sync_channel_history(&team_id, &channel_id, &graph, &storage, &event_tx).await {
                                let _ = event_tx.send(CoreEvent::SyncError(format!("채널 동기화 실패: {err}")));
                            }
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "동기화 완료".to_string(),
                            });
                        }
                    }
                }
                changed = focus_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let focus = focus_rx.borrow_and_update().clone();
                    if let Err(err) = sync_active_focus(&focus, &graph, &storage, &event_tx).await {
                        let _ = event_tx.send(CoreEvent::SyncError(format!("포커스 대화 동기화 실패: {err}")));
                    }
                }
                _ = active_tick.tick() => {
                    let focus = focus_rx.borrow().clone();
                    if let Err(err) = sync_active_focus(&focus, &graph, &storage, &event_tx).await {
                        let _ = event_tx.send(CoreEvent::SyncError(format!("활성 폴링 오류: {err}")));
                    }
                }
                _ = chat_tick.tick() => {
                    if let Err(err) = poll_chats_tier2(&graph, &storage, &event_tx).await {
                        let _ = event_tx.send(CoreEvent::SyncError(format!("채팅 폴링 오류: {err}")));
                    }
                }
                _ = channel_tick.tick() => {
                    if let Err(err) = poll_presences_tier3(&graph, &storage, &event_tx).await {
                        let _ = event_tx.send(CoreEvent::SyncError(format!("상태 폴링 오류: {err}")));
                    }
                    refresh_outlook_best_effort(&graph, &storage, &event_tx).await;
                }
            }
        }
    });

    SyncController { focus_tx, nudge_tx }
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
            let delta_res = graph
                .sync_channel_messages_delta(
                    team_id,
                    channel_id,
                    cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
                )
                .await;

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

            if let Some(root_id) = &focus.thread_root_id {
                let replies = graph
                    .list_thread_replies(team_id, channel_id, root_id, 50)
                    .await?;
                storage.upsert_messages(&replies)?;
                let _ = event_tx.send(CoreEvent::MessagesUpdated {
                    conv_key,
                    parent_msg_id: Some(root_id.clone()),
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
    poll_chats_tier2(graph, storage, event_tx).await?;
    if let Ok(teams) = graph.list_joined_teams_with_channels().await {
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

async fn refresh_outlook_best_effort(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    if let Ok(folders) = graph.list_mail_folders().await {
        if storage.upsert_mail_folders(&folders).is_ok() {
            let _ = event_tx.send(CoreEvent::MailFoldersUpdated(folders));
        }
    }

    if let Ok(messages) = graph.list_mail_messages("inbox", 40).await {
        let had_initial = storage
            .list_mail_messages(Some("inbox"), 1)
            .map(|existing| !existing.is_empty())
            .unwrap_or(false);
        let notify_candidate = if had_initial {
            find_new_unread_mail_candidate("inbox", storage, &messages)
        } else {
            None
        };
        if storage.upsert_mail_messages(&messages).is_ok() {
            let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: "inbox".to_string(),
                messages,
                notify_candidate,
            });
        }
    }

    let (start_iso, end_iso) = default_calendar_window();
    if let Ok(events) = graph.list_calendar_view(&start_iso, &end_iso, 50).await {
        if storage.upsert_calendar_events(&events).is_ok() {
            let _ = event_tx.send(CoreEvent::CalendarEventsUpdated(events));
        }
    }

    if let Ok(contacts) = graph.list_contacts(100).await {
        if storage.upsert_contacts(&contacts).is_ok() {
            let _ = event_tx.send(CoreEvent::ContactsUpdated(contacts));
        }
    }

    if let Ok(lists) = graph.list_todo_lists().await {
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

async fn sync_mail_folder(
    folder_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let scope_key = format!("mail_delta:{folder_id}");
    let prev_cursor = storage.get_sync_cursor(&scope_key)?;
    let had_initial = prev_cursor.is_some()
        || storage
            .list_mail_messages(Some(folder_id), 1)
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
            let messages = storage.list_mail_messages(Some(folder_id), 50)?;
            let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: folder_id.to_string(),
                messages,
                notify_candidate,
            });
        }
        Err(_) => {
            let messages = graph.list_mail_messages(folder_id, 50).await?;
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
            let _ = event_tx.send(CoreEvent::MailMessagesUpdated {
                folder_id: folder_id.to_string(),
                messages,
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
    async fn test_bootstrap_outlook_is_best_effort_on_scope_or_graph_errors() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        bootstrap_outlook(&graph, &storage).await.unwrap();
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
}
