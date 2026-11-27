//! Sync-engine `CoreEvent` handler and desktop notification dispatch.

use crate::models::{CoreEvent, NotificationCandidate, POLL_RETRY_SUMMARY};
use crate::sync::MAIL_HISTORY_PAGE_SIZE;

use super::super::nav_pane::{VIRTUAL_FLAGGED_FOLDER, VIRTUAL_UNREAD_FOLDER};
use super::super::notification;
use super::state::INITIAL_MAIL_HISTORY_LIMIT;
use super::{AppState, UiState};

impl AppState {
    pub(super) fn emit_scope_warning_once(&mut self, ui: &mut UiState, warning: Option<String>) {
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
                    self.mail_oldest_reached = false;
                    if let Some((received_at, id)) = &self.mail_history_boundary {
                        if let Ok(loaded) = self.storage.list_mail_summaries_through(
                            &self.active_folder_id,
                            received_at,
                            id,
                        ) {
                            self.replace_mail_list(ui, loaded);
                        }
                    } else {
                        let limit = self.mail_messages.len().max(messages.len()).max(150);
                        let is_virtual = self.active_folder_id == VIRTUAL_UNREAD_FOLDER
                            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER;
                        if let Ok(loaded) =
                            self.load_folder_summaries(&self.active_folder_id, limit)
                        {
                            self.replace_mail_list(ui, loaded);
                        } else if !messages.is_empty() && !is_virtual {
                            self.replace_mail_list(ui, messages);
                        }
                    }
                }
                if let Some(candidate) = notify_candidate {
                    self.maybe_notify(ui, &candidate);
                }
            }
            CoreEvent::MailHistoryPageFinished {
                folder_id,
                has_more,
                seq,
            } => {
                if !self.loading_older_mail
                    || seq != self.mail_history_seq
                    || !self.folder_matches_active(&folder_id)
                {
                    return;
                }
                self.loading_older_mail = false;
                let Some(has_more) = has_more else { return };
                let limit = self
                    .mail_messages
                    .len()
                    .saturating_add(MAIL_HISTORY_PAGE_SIZE)
                    .max(INITIAL_MAIL_HISTORY_LIMIT);
                if let Ok(loaded) = self
                    .storage
                    .list_mail_summaries(Some(&self.active_folder_id), limit)
                {
                    self.mail_oldest_reached = !has_more && loaded.len() < limit;
                    self.mail_history_boundary = loaded
                        .last()
                        .map(|mail| (mail.received_at.clone(), mail.id.clone()));
                    self.replace_mail_list(ui, loaded);
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
                self.reconcile_calendar_selection(ui);
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
                    self.reconcile_todo_selection(ui);
                }
            }
        }
    }

    pub(super) fn maybe_notify(&self, ui: &UiState, candidate: &NotificationCandidate) {
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
}
