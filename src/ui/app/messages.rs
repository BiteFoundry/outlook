//! Async UI task completion (`UiMsg`) router and domain-specific reducers.

use std::collections::HashSet;

use crate::graph::client::MailActionUnconfirmed;
use crate::graph::mail::DraftSaveChange;
use crate::models::{MailMessage, MailSearchHit};
use crate::sync::SyncCommand;

use super::super::nav_pane::{VIRTUAL_FLAGGED_FOLDER, VIRTUAL_UNREAD_FOLDER};
use super::super::overlays::html_preview::HtmlPreviewOverlay;
use super::super::overlays::{Overlay, OverlayKind};
use super::super::toast::ToastKind;
use super::{AppState, UiMsg, UiState};

impl AppState {
    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::BootstrapDone { .. } | UiMsg::FolderMessagesLoaded { .. } => {
                self.handle_bootstrap_msg(ui, msg);
            }
            UiMsg::MailDraftProgress { .. }
            | UiMsg::MailDraftSaved { .. }
            | UiMsg::MailSent { .. }
            | UiMsg::MailMoved { .. }
            | UiMsg::MailDeleteFailed { .. }
            | UiMsg::MailMoveFailed { .. }
            | UiMsg::MailFlagReverted { .. }
            | UiMsg::MailDeleted { .. }
            | UiMsg::AttachmentDownloaded(_) => {
                self.handle_mail_msg(ui, msg);
            }
            UiMsg::SearchResults { .. }
            | UiMsg::RemoteSearchResults { .. }
            | UiMsg::RemoteSearchFailed { .. } => {
                self.handle_search_msg(msg);
            }
            UiMsg::CalendarEventCreated(_)
            | UiMsg::CalendarEventDeleted(_)
            | UiMsg::CalendarEventDeleteFailed { .. }
            | UiMsg::CalendarRsvpDone { .. }
            | UiMsg::CalendarRsvpFailed { .. } => {
                self.handle_calendar_msg(ui, msg);
            }
            UiMsg::TodoListCreated(_)
            | UiMsg::TodoTasksLoaded { .. }
            | UiMsg::TodoTaskCreated(_)
            | UiMsg::TodoTaskUpdated(_)
            | UiMsg::TodoTaskToggleFailed { .. }
            | UiMsg::TodoTaskDeleted(_)
            | UiMsg::TodoTaskDeleteFailed { .. } => {
                self.handle_todo_msg(ui, msg);
            }
            UiMsg::ClipboardImageEncoded { .. }
            | UiMsg::ImageDecoded { .. }
            | UiMsg::ImageBytesFetched { .. }
            | UiMsg::ImageDecodeFailed { .. }
            | UiMsg::ImageFetchReturnedHtml { .. }
            | UiMsg::HtmlAttachmentLoaded { .. } => {
                self.handle_media_msg(ui, msg);
            }
            UiMsg::UpdateChecked(_)
            | UiMsg::DaemonAttached { .. }
            | UiMsg::DaemonDetached
            | UiMsg::StartLocalBootstrap
            | UiMsg::ProfileLoaded { .. }
            | UiMsg::TaskDone { .. } => {
                self.handle_lifecycle_msg(ui, msg);
            }
        }
    }

    fn handle_bootstrap_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
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
                    self.bump_folders_gen();
                    if self.active_folder_id.eq_ignore_ascii_case("inbox") {
                        if let Some(inbox_id) = self.find_inbox_folder_id() {
                            self.active_folder_id = inbox_id;
                        }
                    }
                }
                // The bootstrap payload is the Inbox: it may only replace the list while the real
                // Inbox is on screen, never virtual smart folders (`Unread` / `Flagged`) or another folder.
                let is_virtual_folder = self.active_folder_id == VIRTUAL_UNREAD_FOLDER
                    || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER;
                if !mail_messages.is_empty()
                    && self.mail_history_boundary.is_none()
                    && !is_virtual_folder
                    && self.folder_matches_active("inbox")
                {
                    self.mail_messages = mail_messages;
                    self.clear_mail_detail_cache();
                } else if is_virtual_folder {
                    let limit = self.mail_messages.len().max(150);
                    if let Ok(loaded) = self.load_folder_summaries(&self.active_folder_id, limit) {
                        if !loaded.is_empty() {
                            self.mail_messages = loaded;
                            self.clear_mail_detail_cache();
                        }
                    }
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
                }
                self.reconcile_calendar_selection(ui);
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
                    let payload_list_id = todo_tasks.first().map(|t| t.list_id.as_str());
                    let matches_active = match (self.active_todo_list_id.as_deref(), payload_list_id)
                    {
                        (Some(active), Some(payload)) => active == payload,
                        (None, Some(payload)) => {
                            self.active_todo_list_id = Some(payload.to_string());
                            true
                        }
                        _ => false,
                    };
                    if matches_active {
                        self.todo_tasks = todo_tasks;
                    }
                }
                self.reconcile_todo_selection(ui);
                self.syncing = false;
                self.sync_summary = "Synced".to_string();
                self.emit_scope_warning_once(ui, scope_warning);
                if self.active_mail().is_none() || !self.active_mail_is_visible(ui) {
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
            _ => unreachable!("unexpected UiMsg routed to handle_bootstrap_msg"),
        }
    }

    fn handle_mail_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::MailDraftProgress {
                draft_key,
                compose_id,
                change,
            } => {
                let Some(write) = self.draft_writes.get(&compose_id) else {
                    return;
                };
                let Some(key) = self.stored_compose_key(&draft_key, &compose_id) else {
                    return;
                };
                let Some(entry) = self.draft_store.mail.get_mut(&key) else {
                    return;
                };
                match change {
                    DraftSaveChange::ThreadCreated { id, quoted_html } => {
                        entry.draft_id = Some(id);
                        entry.synced.thread_quote_html = Some(quoted_html);
                        entry.synced.thread_creation_pending = false;
                    }
                    DraftSaveChange::Fields {
                        id,
                        included_attachments,
                    } => {
                        entry.draft_id = Some(id.clone());
                        entry.synced.fields = Some(write.fields.clone());
                        if included_attachments {
                            entry.synced.files.extend(write.files.iter().cloned());
                            entry.synced.image_ids.extend(write.images.iter().cloned());
                        }
                    }
                    DraftSaveChange::File(path) => entry.synced.files.push(path),
                    DraftSaveChange::Image(id) => entry.synced.image_ids.push(id),
                }
                for overlay in &mut ui.overlays {
                    if let Overlay::MailCompose(compose) = overlay {
                        if compose.compose_id == compose_id {
                            compose.draft_id.clone_from(&entry.draft_id);
                        }
                    }
                }
                if let Err(err) = self.persist_draft_store() {
                    Self::show_draft_error(
                        ui,
                        &compose_id,
                        format!("Cannot record Outlook draft locally: {err:#}"),
                    );
                }
            }
            UiMsg::MailDraftSaved {
                draft_key,
                compose_id,
                result,
                send,
                sent_draft_id,
            } => {
                let queued = self
                    .draft_writes
                    .remove(&compose_id)
                    .and_then(|write| write.queued);
                match result {
                    Ok(saved) => {
                        let saved_id = saved.as_ref().map(|m| m.id.clone());
                        if let Some(mut saved) = saved {
                            self.mail_detail_missing.remove(&saved.id);
                            if !send {
                                saved.body_loaded = true;
                                let _ = self
                                    .storage
                                    .upsert_mail_messages(std::slice::from_ref(&saved));
                                let in_list_pos = self
                                    .mail_messages
                                    .iter()
                                    .position(|m| m.id == saved.id);
                                let is_active_drafts = self.message_matches_active_folder(&saved)
                                    || (saved.is_draft
                                        && self
                                            .mail_folders
                                            .iter()
                                            .find(|f| f.id == self.active_folder_id)
                                            .is_some_and(|f| {
                                                f.well_known_name
                                                    .as_deref()
                                                    .is_some_and(|w| w.eq_ignore_ascii_case("drafts"))
                                                    || f.display_name.eq_ignore_ascii_case("drafts")
                                            }));
                                if let Some(pos) = in_list_pos {
                                    self.mail_messages[pos] = Self::mail_summary_from(&saved);
                                } else if is_active_drafts {
                                    self.mail_messages
                                        .insert(0, Self::mail_summary_from(&saved));
                                    if self.active_mail_id.is_none() {
                                        self.active_mail_id = Some(saved.id.clone());
                                        ui.mail_list_scroll.selected_idx = Some(0);
                                    }
                                }
                                if self.active_mail_id.as_deref() == Some(saved.id.as_str())
                                    || self.mail_detail_cache.contains_key(&saved.id)
                                {
                                    self.mail_detail_cache.insert(saved.id.clone(), saved);
                                }
                                self.bump_mail_gen();
                            } else {
                                self.mail_detail_cache.remove(&saved.id);
                                self.bump_mail_gen();
                            }
                        }
                        if send {
                            self.handle_ui_msg(
                                ui,
                                UiMsg::MailSent {
                                    draft_key,
                                    compose_id,
                                    draft_id: sent_draft_id.or(saved_id),
                                },
                            );
                            ui.success("Email sent");
                        } else if let Err(err) = self.persist_draft_store() {
                            Self::show_draft_error(
                                ui,
                                &compose_id,
                                format!("Cannot record Outlook draft locally: {err:#}"),
                            );
                        } else if let Some(compose) = queued {
                            if let Some(entry) = self
                                .stored_compose_key(&draft_key, &compose_id)
                                .and_then(|key| self.draft_store.get_mail(&key))
                            {
                                let mut next = compose.to_draft_entry();
                                next.draft_id.clone_from(&entry.draft_id);
                                next.synced.clone_from(&entry.synced);
                                self.start_draft_write(ui, &compose, next, false);
                            }
                        } else {
                            for overlay in &mut ui.overlays {
                                if let Overlay::MailCompose(compose) = overlay {
                                    if compose.compose_id == compose_id {
                                        compose.error = None;
                                    }
                                }
                            }
                            ui.success("Draft saved to Outlook");
                        }
                    }
                    Err(err) => {
                        if !err.is::<MailActionUnconfirmed>() {
                            if let Some(entry) = self
                                .stored_compose_key(&draft_key, &compose_id)
                                .and_then(|key| self.draft_store.mail.get_mut(&key))
                            {
                                if entry.synced.thread_creation_pending || entry.synced.send_pending {
                                    entry.synced.thread_creation_pending = false;
                                    entry.synced.send_pending = false;
                                    if let Err(local_err) = self.persist_draft_store() {
                                        ui.error(format!(
                                            "Cannot record Outlook draft locally: {local_err:#}"
                                        ));
                                    }
                                }
                            }
                        }
                        let operation = if send { "Send" } else { "Outlook draft save" };
                        Self::show_draft_error(
                            ui,
                            &compose_id,
                            format!("{operation} failed (draft preserved locally): {err:#}"),
                        );
                    }
                }
            }
            UiMsg::MailSent {
                draft_key,
                compose_id,
                draft_id,
            } => {
                let stored_key = self.stored_compose_key(&draft_key, &compose_id);
                let resolved_draft_id = draft_id.or_else(|| {
                    stored_key
                        .as_deref()
                        .and_then(|key| self.draft_store.get_mail(key))
                        .and_then(|entry| entry.draft_id.clone())
                });
                if let Some(key) = stored_key {
                    self.draft_store.remove_mail(&key);
                    if let Err(err) = self.persist_draft_store() {
                        ui.error(format!("Cannot clear sent draft locally: {err:#}"));
                    }
                }
                if let Some(id) = resolved_draft_id {
                    self.remove_sent_draft_locally(ui, &id);
                }
            }
            UiMsg::MailMoved { moves } => {
                if let Ok(folders) = self.storage.list_mail_folders() {
                    self.mail_folders = folders;
                    self.bump_folders_gen();
                }
                self.bump_mail_gen();
                for (old_id, moved) in moves {
                    let moved_id = moved.id.clone();
                    let moved_in_active_folder = self.message_matches_active_folder(&moved);
                    self.mail_detail_cache.remove(&old_id);
                    self.mail_detail_missing.remove(&old_id);
                    self.mail_detail_missing.remove(&moved_id);
                    if self.expanded_thread_ids.remove(&old_id) && moved_in_active_folder {
                        self.expanded_thread_ids.insert(moved_id.clone());
                    }
                    if moved_in_active_folder {
                        if self.active_mail_id.as_deref() == Some(old_id.as_str()) {
                            self.active_mail_id = Some(moved_id.clone());
                        }
                        if self.pinned_mail.as_ref().is_some_and(|p| p.id == old_id) {
                            self.pinned_mail = Some(moved.clone());
                        }
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
                        self.sync_mail_list_selection_to_active(ui);
                    } else {
                        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
                        let was_present_or_active = self
                            .mail_messages
                            .iter()
                            .any(|m| m.id == old_id || m.id == moved_id)
                            || self.active_mail_id.as_deref() == Some(old_id.as_str())
                            || self.active_mail_id.as_deref() == Some(moved_id.as_str());
                        self.mail_messages
                            .retain(|m| m.id != old_id && m.id != moved_id);
                        if self
                            .pinned_mail
                            .as_ref()
                            .is_some_and(|p| p.id == old_id || p.id == moved_id)
                        {
                            self.pinned_mail = None;
                        }
                        if was_present_or_active {
                            self.reselect_after_removal(ui, prev_sel);
                        }
                    }
                }
            }
            UiMsg::MailDeleteFailed {
                original_id,
                original_message,
                error,
            } => {
                if let Ok(folders) = self.storage.list_mail_folders() {
                    self.mail_folders = folders;
                    self.bump_folders_gen();
                }
                self.bump_mail_gen();
                if let Some(msg) = original_message {
                    if self.message_matches_active_folder(&msg) {
                        if !self.mail_messages.iter().any(|m| m.id == msg.id) {
                            self.mail_messages.push(Self::mail_summary_from(&msg));
                            self.mail_messages
                                .sort_by(|a, b| b.received_at.cmp(&a.received_at));
                        }
                        self.active_mail_id = Some(original_id);
                        self.sync_displayed_mail_details(ui.group_threads, false);
                        self.sync_mail_list_selection_to_active(ui);
                    }
                }
                ui.tasks.finish("Delete email", Some(error.clone()));
                ui.toasts.error(format!("Delete failed: {error}"));
            }
            UiMsg::MailMoveFailed {
                restored,
                label,
                error,
            } => {
                if let Ok(folders) = self.storage.list_mail_folders() {
                    self.mail_folders = folders;
                    self.bump_folders_gen();
                }
                self.bump_mail_gen();
                let mut reinserted: Option<String> = None;
                for msg in &restored {
                    if self.message_matches_active_folder(msg)
                        && !self.mail_messages.iter().any(|m| m.id == msg.id)
                    {
                        self.mail_messages.push(Self::mail_summary_from(msg));
                        reinserted.get_or_insert_with(|| msg.id.clone());
                    }
                }
                if let Some(first_id) = reinserted {
                    // Newest first, the order the list is kept in.
                    self.mail_messages
                        .sort_by(|a, b| b.received_at.cmp(&a.received_at));
                    if self.active_mail_id.is_none() {
                        self.active_mail_id = Some(first_id);
                    }
                    self.sync_displayed_mail_details(ui.group_threads, false);
                    self.sync_mail_list_selection_to_active(ui);
                }
                // "Archive" -> "Archive failed", "Move email" -> "Move failed".
                let action = label.split_whitespace().next().unwrap_or("Move");
                ui.toasts.error(format!("{action} failed: {error}"));
                ui.tasks.finish(&label, Some(error));
            }
            UiMsg::MailFlagReverted {
                id,
                is_read,
                flagged,
                label,
                error,
            } => {
                let revert = |m: &mut MailMessage| {
                    if let Some(read) = is_read {
                        m.is_read = read;
                    }
                    if let Some(flag) = flagged {
                        m.flagged = flag;
                    }
                };
                if let Some(m) = self.mail_messages.iter_mut().find(|m| m.id == id) {
                    revert(m);
                }
                if let Some(m) = self.mail_detail_cache.get_mut(&id) {
                    revert(m);
                }
                if let Some(m) = self.pinned_mail.as_mut().filter(|m| m.id == id) {
                    revert(m);
                }
                self.bump_mail_gen();
                self.refresh_mail_folders_from_storage();
                if self.active_mail().is_none() || !self.active_mail_is_visible(ui) {
                    self.select_first_visible_mail(ui);
                } else {
                    self.sync_mail_list_selection_to_active(ui);
                }
                ui.toasts.error(error.clone());
                ui.tasks.finish(&label, Some(error));
            }
            UiMsg::MailDeleted { .. } | UiMsg::AttachmentDownloaded(_) => {}
            _ => unreachable!("unexpected UiMsg routed to handle_mail_msg"),
        }
    }

    fn handle_search_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::SearchResults {
                query,
                generation,
                hits,
            } => {
                if generation == self.search_generation
                    && self.search_query.trim() == query.trim()
                {
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
            UiMsg::RemoteSearchResults {
                query,
                generation,
                messages,
            } => {
                if generation == self.search_generation
                    && self.search_query.trim() == query.trim()
                {
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
            UiMsg::RemoteSearchFailed {
                query,
                generation,
                error,
            } => {
                if generation == self.search_generation
                    && self.search_query.trim() == query.trim()
                {
                    self.searching = false;
                    self.search_remote_inflight = false;
                    self.search_remote_done = false;
                    self.search_error = Some(error);
                }
            }
            _ => unreachable!("unexpected UiMsg routed to handle_search_msg"),
        }
    }

    fn handle_calendar_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::CalendarEventCreated(created) => {
                let created_id = created.id.clone();
                self.calendar_events.push(created);
                self.calendar_events
                    .sort_by(|a, b| a.start_iso.cmp(&b.start_iso));
                self.active_event_id = Some(created_id.clone());
                let vis = self.visible_calendar_events(ui);
                ui.cal_list_scroll.selected_idx = vis.iter().position(|e| e.id == created_id);
                ui.cal_detail_scroll.offset = 0;
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::SyncCalendar);
                }
            }
            UiMsg::CalendarEventDeleted(id) => {
                self.calendar_events.retain(|e| e.id != id);
                self.reconcile_calendar_selection(ui);
                if self.daemon_attached {
                    self.sync_ctrl.request(SyncCommand::SyncCalendar);
                }
            }
            UiMsg::CalendarEventDeleteFailed {
                original_event,
                error,
            } => {
                if let Some(ev) = original_event {
                    let ev_id = ev.id.clone();
                    if !self.calendar_events.iter().any(|e| e.id == ev_id) {
                        self.calendar_events.push(ev);
                        self.calendar_events
                            .sort_by(|a, b| a.start_iso.cmp(&b.start_iso));
                    }
                    if self.active_event_id.is_none() {
                        self.active_event_id = Some(ev_id);
                    }
                    self.reconcile_calendar_selection(ui);
                }
                ui.tasks.finish("Delete event", Some(error.clone()));
                ui.toasts.error(format!("Delete failed: {error}"));
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
            UiMsg::CalendarRsvpFailed {
                event_id,
                prev_status,
                prev_mail_statuses,
                error,
            } => {
                if let Some(old_status) = prev_status {
                    if let Some(ev) = self.calendar_events.iter_mut().find(|e| e.id == event_id) {
                        ev.response_status = old_status;
                    }
                }
                for (msg_id, old_mail_status) in prev_mail_statuses {
                    if let Some(m) = self.mail_messages.iter_mut().find(|m| m.id == msg_id) {
                        if let Some(ref mut mr) = m.meeting_request {
                            mr.response_status = old_mail_status.clone();
                        }
                    }
                    if let Some(m) = self.mail_detail_cache.get_mut(&msg_id) {
                        if let Some(ref mut mr) = m.meeting_request {
                            mr.response_status = old_mail_status.clone();
                        }
                    }
                    if let Some(m) = self.pinned_mail.as_mut().filter(|m| m.id == msg_id) {
                        if let Some(ref mut mr) = m.meeting_request {
                            mr.response_status = old_mail_status;
                        }
                    }
                }
                self.bump_mail_gen();
                ui.tasks.finish("Calendar RSVP", Some(error.clone()));
                ui.toasts.error(format!("RSVP failed: {error}"));
            }
            _ => unreachable!("unexpected UiMsg routed to handle_calendar_msg"),
        }
    }

    fn handle_todo_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::TodoListCreated(list) => {
                let lid = list.id.clone();
                self.todo_lists.push(list);
                self.select_todo_list(ui, lid);
            }
            UiMsg::TodoTasksLoaded { list_id, tasks } => {
                if self.active_todo_list_id.as_deref() == Some(list_id.as_str()) {
                    self.todo_tasks = tasks;
                    self.reconcile_todo_selection(ui);
                }
            }
            UiMsg::TodoTaskCreated(task) => {
                if self.active_todo_list_id.as_deref() == Some(task.list_id.as_str()) {
                    self.active_todo_task_id = Some(task.id.clone());
                    self.todo_tasks.insert(0, task);
                    self.reconcile_todo_selection(ui);
                }
            }
            UiMsg::TodoTaskUpdated(task) => {
                if let Some(existing) = self.todo_tasks.iter_mut().find(|t| t.id == task.id) {
                    *existing = task;
                }
                self.reconcile_todo_selection(ui);
            }
            UiMsg::TodoTaskToggleFailed {
                original_task,
                error,
            } => {
                if let Some(existing) = self
                    .todo_tasks
                    .iter_mut()
                    .find(|t| t.id == original_task.id)
                {
                    *existing = original_task;
                }
                self.reconcile_todo_selection(ui);
                ui.tasks.finish("Update task", Some(error.clone()));
                ui.toasts.error(format!("Task update failed: {error}"));
            }
            UiMsg::TodoTaskDeleted(task_id) => {
                self.todo_tasks.retain(|t| t.id != task_id);
                self.reconcile_todo_selection(ui);
            }
            UiMsg::TodoTaskDeleteFailed {
                original_task,
                error,
            } => {
                if let Some(task) = original_task {
                    if self.active_todo_list_id.as_deref() == Some(task.list_id.as_str()) {
                        if let Ok(sorted) = self
                            .storage
                            .list_todo_tasks(Some(&task.list_id), true)
                        {
                            if !sorted.is_empty() {
                                self.todo_tasks = sorted;
                            } else if !self.todo_tasks.iter().any(|t| t.id == task.id) {
                                self.todo_tasks.push(task.clone());
                            }
                        } else if !self.todo_tasks.iter().any(|t| t.id == task.id) {
                            self.todo_tasks.push(task.clone());
                        }
                        if self.active_todo_task_id.is_none() {
                            self.active_todo_task_id = Some(task.id);
                        }
                        self.reconcile_todo_selection(ui);
                    }
                }
                ui.tasks.finish("Delete task", Some(error.clone()));
                ui.toasts.error(format!("Delete failed: {error}"));
            }
            _ => unreachable!("unexpected UiMsg routed to handle_todo_msg"),
        }
    }

    fn handle_media_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
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
            UiMsg::HtmlAttachmentLoaded {
                title,
                content_type,
                html,
            } => {
                let kind = super::super::overlays::html_preview::classify_markup_content(
                    content_type.as_deref(),
                    Some(&title),
                    html.as_bytes(),
                );
                let mut overlay = if kind == bitefoundry_html::ContentKind::Markdown {
                    HtmlPreviewOverlay::new_markdown(Some(title), html)
                } else {
                    HtmlPreviewOverlay::new(Some(title), html)
                };
                overlay.show_link_urls = ui.show_link_urls;
                ui.close_overlays_of(OverlayKind::HtmlPreview);
                ui.push_overlay(Overlay::html_preview(overlay));
            }
            _ => unreachable!("unexpected UiMsg routed to handle_media_msg"),
        }
    }

    fn handle_lifecycle_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
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
            _ => unreachable!("unexpected UiMsg routed to handle_lifecycle_msg"),
        }
    }
}
