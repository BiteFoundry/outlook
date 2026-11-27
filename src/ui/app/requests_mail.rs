//! Async mail mutations (read/flag/archive/delete/move), draft autosave, compose, attachments, and search.

use std::collections::HashSet;
use std::sync::{atomic::Ordering, Arc};

use sha2::{Digest, Sha256};

use crate::config::{MailDraftEntry, MailDraftFields};
use crate::graph::mail::{rewrite_inline_image_cids, DraftSaveChange};
use crate::models::{MailMessage, OutgoingMail};
use crate::parser;
use crate::storage::Storage;

use super::super::nav_pane::find_folder_by_wellknown;
use super::super::overlays::mail_compose::{MailComposeMode, MailComposeOverlay};
use super::super::overlays::Overlay;
use super::super::toast::ToastKind;
use super::state::PendingDraftWrite;
use super::{AppState, UiMsg, UiState};

impl AppState {
    pub fn toggle_mail_read(&mut self, ui: &mut UiState) {
        let Some((id, new_read)) = self.active_mail().map(|m| (m.id.clone(), !m.is_read)) else {
            return;
        };
        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        if let Some(m) = self.mail_messages.iter_mut().find(|m| m.id == id) {
            m.is_read = new_read;
        }
        if let Some(m) = self.mail_detail_cache.get_mut(&id) {
            m.is_read = new_read;
        }
        if let Some(ref mut m) = self.pinned_mail {
            if m.id == id {
                m.is_read = new_read;
            }
        }
        self.bump_mail_gen();
        let _ = self.storage.set_mail_read_local(&id, new_read);
        self.refresh_mail_folders_from_storage();
        if !self.active_mail_is_visible(ui) {
            self.reselect_after_removal(ui, prev_sel);
        }

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = graph.set_mail_read(&id, new_read).await {
                let (st, restore_id) = (storage.clone(), id.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    st.set_mail_read_local(&restore_id, !new_read)
                })
                .await;
                let _ = tx.send(UiMsg::MailFlagReverted {
                    id,
                    is_read: Some(!new_read),
                    flagged: None,
                    label: "Mark read".to_string(),
                    error: format!("Failed to update read state: {err}"),
                });
            }
        });
    }

    pub fn toggle_mail_flag(&mut self, ui: &mut UiState) {
        let Some((id, new_flag)) = self.active_mail().map(|m| (m.id.clone(), !m.flagged)) else {
            return;
        };
        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        if let Some(m) = self.mail_messages.iter_mut().find(|m| m.id == id) {
            m.flagged = new_flag;
        }
        if let Some(m) = self.mail_detail_cache.get_mut(&id) {
            m.flagged = new_flag;
        }
        if let Some(ref mut m) = self.pinned_mail {
            if m.id == id {
                m.flagged = new_flag;
            }
        }
        self.bump_mail_gen();
        let _ = self.storage.set_mail_flagged_local(&id, new_flag);
        if !self.active_mail_is_visible(ui) {
            self.reselect_after_removal(ui, prev_sel);
        }
        ui.info(if new_flag { "Flagged" } else { "Unflagged" });

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = graph.set_mail_flag(&id, new_flag).await {
                let (st, restore_id) = (storage.clone(), id.clone());
                let _ = tokio::task::spawn_blocking(move || {
                    st.set_mail_flagged_local(&restore_id, !new_flag)
                })
                .await;
                let _ = tx.send(UiMsg::MailFlagReverted {
                    id,
                    is_read: None,
                    flagged: Some(!new_flag),
                    label: "Flag email".to_string(),
                    error: format!("Failed to update flag: {err}"),
                });
            }
        });
    }

    pub fn archive_selected_mail(&mut self, ui: &mut UiState) {
        let Some(id) = self.active_mail_id.clone() else {
            return;
        };
        let archive_id = find_folder_by_wellknown(&self.mail_folders, "archive", &["Archive", "보관함"])
            .map(|f| f.id.clone())
            .unwrap_or_else(|| "archive".to_string());

        let prev_msg = self.mail_snapshot(&id);
        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        if self.active_folder_id != archive_id {
            self.mail_messages.retain(|m| m.id != id);
            self.mail_detail_cache.remove(&id);
            self.mail_detail_missing.remove(&id);
            self.expanded_thread_ids.remove(&id);
            if self.pinned_mail.as_ref().is_some_and(|p| p.id == id) {
                self.pinned_mail = None;
            }
            self.bump_mail_gen();
        }
        let _ = self.storage.move_mail_folder_local(&id, &archive_id);
        self.refresh_mail_folders_from_storage();
        self.reselect_after_removal(ui, prev_sel);
        ui.success("Archived");

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.move_mail_message_with_meta(&id, &archive_id).await {
                Ok((moved, attachments_expanded)) => {
                    let old_id = id.clone();
                    let moved_for_db = moved.clone();
                    let persisted = tokio::task::spawn_blocking(move || {
                        storage.replace_moved_mail_local_ex(
                            &old_id,
                            &moved_for_db,
                            attachments_expanded,
                        )
                    })
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(moved);
                    let _ = tx.send(UiMsg::MailMoved {
                        moves: vec![(id, persisted)],
                    });
                }
                Err(err) => {
                    let restored: Vec<MailMessage> = prev_msg.into_iter().collect();
                    restore_mail_folders_local(&storage, &restored).await;
                    let _ = tx.send(UiMsg::MailMoveFailed {
                        restored,
                        label: "Archive".to_string(),
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    /// The cached row for `id` (falling back to the list row) as it is before an optimistic change,
    /// so a failed Graph call can put it back.
    pub(super) fn mail_snapshot(&self, id: &str) -> Option<MailMessage> {
        self.storage
            .get_mail_message(id)
            .ok()
            .flatten()
            .or_else(|| self.mail_messages.iter().find(|m| m.id == id).cloned())
    }

    pub fn delete_selected_mail(&mut self, ui: &mut UiState) {
        let Some(id) = self.active_mail_id.clone() else {
            return;
        };
        let trash_id = self
            .find_trash_folder_id()
            .unwrap_or_else(|| "deleteditems".to_string());
        let in_trash = self.active_folder_id == trash_id
            || self.active_folder_id.eq_ignore_ascii_case("deleteditems");

        let prev_msg = self
            .storage
            .get_mail_message(&id)
            .ok()
            .flatten()
            .or_else(|| self.mail_messages.iter().find(|m| m.id == id).cloned());
        let prev_folder_id = prev_msg.as_ref().and_then(|m| m.folder_id.clone());

        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        self.mail_messages.retain(|m| m.id != id);
        self.mail_detail_cache.remove(&id);
        self.mail_detail_missing.remove(&id);
        self.expanded_thread_ids.remove(&id);
        if self.pinned_mail.as_ref().is_some_and(|p| p.id == id) {
            self.pinned_mail = None;
        }
        self.bump_mail_gen();
        if in_trash {
            let _ = self.storage.delete_mail_messages_by_ids(std::slice::from_ref(&id));
        } else {
            let _ = self.storage.move_mail_folder_local(&id, &trash_id);
        }
        if let Ok(folders) = self.storage.list_mail_folders() {
            self.mail_folders = folders;
            self.bump_folders_gen();
        }
        self.reselect_after_removal(ui, prev_sel);
        ui.info(if in_trash { "Deleted permanently" } else { "Moved to Trash" });

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            if in_trash {
                if let Err(err) = graph.delete_mail_message(&id).await {
                    if let Some(ref restore_msg) = prev_msg {
                        let st = storage.clone();
                        let restore_clone = restore_msg.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st.upsert_mail_messages(std::slice::from_ref(&restore_clone))
                        })
                        .await;
                    }
                    let _ = tx.send(UiMsg::MailDeleteFailed {
                        original_id: id,
                        original_message: prev_msg,
                        error: err.to_string(),
                    });
                }
            } else {
                match graph.move_mail_message_with_meta(&id, &trash_id).await {
                    Ok((moved, attachments_expanded)) => {
                        let old_id = id.clone();
                        let moved_for_db = moved.clone();
                        let persisted = tokio::task::spawn_blocking(move || {
                            storage.replace_moved_mail_local_ex(
                                &old_id,
                                &moved_for_db,
                                attachments_expanded,
                            )
                        })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(moved);
                        let _ = tx.send(UiMsg::MailMoved {
                            moves: vec![(id, persisted)],
                        });
                    }
                    Err(err) => {
                        if let Some(orig_fid) = prev_folder_id {
                            let st = storage.clone();
                            let restore_id = id.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                st.move_mail_folder_local(&restore_id, &orig_fid)
                            })
                            .await;
                        }
                        let _ = tx.send(UiMsg::MailDeleteFailed {
                            original_id: id,
                            original_message: prev_msg,
                            error: err.to_string(),
                        });
                    }
                }
            }
        });
    }

    pub fn move_mail_to_folder(
        &mut self,
        ui: &mut UiState,
        message_ids: Vec<String>,
        destination_folder_id: String,
        destination_name: String,
    ) {
        let snapshots: Vec<MailMessage> = message_ids
            .iter()
            .filter_map(|id| self.mail_snapshot(id))
            .collect();
        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        if self.active_folder_id != destination_folder_id {
            let id_set: HashSet<&str> = message_ids.iter().map(String::as_str).collect();
            self.mail_messages
                .retain(|m| !id_set.contains(m.id.as_str()));
            self.mail_detail_cache
                .retain(|id, _| !id_set.contains(id.as_str()));
            self.mail_detail_missing
                .retain(|id| !id_set.contains(id.as_str()));
            self.expanded_thread_ids
                .retain(|id| !id_set.contains(id.as_str()));
            if self
                .pinned_mail
                .as_ref()
                .is_some_and(|p| id_set.contains(p.id.as_str()))
            {
                self.pinned_mail = None;
            }
            self.bump_mail_gen();
        }
        for id in &message_ids {
            let _ = self
                .storage
                .move_mail_folder_local(id, &destination_folder_id);
        }
        self.refresh_mail_folders_from_storage();
        self.reselect_after_removal(ui, prev_sel);
        ui.success(format!("Moved to {destination_name}"));

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let mut moves = Vec::with_capacity(message_ids.len());
            for (attempt, id) in message_ids.iter().enumerate() {
                match graph
                    .move_mail_message_with_meta(id, &destination_folder_id)
                    .await
                {
                    Ok((moved, attachments_expanded)) => {
                        let st = storage.clone();
                        let old_id = id.clone();
                        let moved_for_db = moved.clone();
                        let persisted = tokio::task::spawn_blocking(move || {
                            st.replace_moved_mail_local_ex(
                                &old_id,
                                &moved_for_db,
                                attachments_expanded,
                            )
                        })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or(moved);
                        moves.push((id.clone(), persisted));
                    }
                    Err(err) => {
                        if !moves.is_empty() {
                            let _ = tx.send(UiMsg::MailMoved { moves });
                        }
                        // The failed message and every one not attempted yet go back where they were.
                        let not_moved: HashSet<&str> =
                            message_ids[attempt..].iter().map(String::as_str).collect();
                        let restored: Vec<MailMessage> = snapshots
                            .into_iter()
                            .filter(|m| not_moved.contains(m.id.as_str()))
                            .collect();
                        restore_mail_folders_local(&storage, &restored).await;
                        let _ = tx.send(UiMsg::MailMoveFailed {
                            restored,
                            label: "Move email".to_string(),
                            error: err.to_string(),
                        });
                        return;
                    }
                }
            }
            let _ = tx.send(UiMsg::MailMoved { moves });
        });
    }

    pub fn submit_mail_compose(&mut self, ui: &mut UiState, compose: &MailComposeOverlay) -> bool {
        if tokio::runtime::Handle::try_current().is_err() {
            return false;
        }
        if self.draft_writes.contains_key(&compose.compose_id) {
            ui.info("Wait for the draft to finish saving, then send it.");
            return false;
        }
        let entry = match self.store_compose(compose) {
            Ok(entry) => entry,
            Err(err) => {
                Self::show_draft_error(
                    ui,
                    &compose.compose_id,
                    format!("Cannot save draft locally: {err:#}"),
                );
                return false;
            }
        };
        if let Some(path) = entry
            .attachments
            .iter()
            .find(|path| !entry.synced.files.contains(path) && !path.is_file())
        {
            Self::show_draft_error(
                ui,
                &compose.compose_id,
                format!("Attachment file not found: {}", path.display()),
            );
            return false;
        }
        if entry.draft_id.is_some() || !matches!(compose.mode, MailComposeMode::New) {
            return self.start_draft_write(ui, compose, entry, true);
        }

        let mut outgoing = Self::outgoing_draft(compose, &entry, true);
        outgoing.save_to_sent_items = true;
        let draft_key = compose.mode.draft_key();
        let compose_id = compose.compose_id.clone();
        let task_label = "Send email".to_string();
        ui.tasks.begin(task_label.clone());

        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let res = graph.send_mail(&outgoing).await;

            if res.is_ok() {
                let _ = tx.send(UiMsg::MailSent {
                    draft_key,
                    compose_id,
                    draft_id: None,
                });
            }
            let (error, toast) = match res {
                Ok(()) => (None, ("Email sent".to_string(), ToastKind::Success)),
                Err(err) => (
                    Some(err.to_string()),
                    (
                        format!("Send failed (draft preserved): {err}"),
                        ToastKind::Error,
                    ),
                ),
            };
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error,
                toast: Some(toast),
            });
        });
        true
    }

    pub fn stash_compose_draft(&mut self, compose: &MailComposeOverlay) -> anyhow::Result<()> {
        if compose.has_unsent_content() {
            self.store_compose(compose)?;
        } else {
            let key = compose.mode.draft_key();
            if self.draft_store.get_mail(&key).is_some_and(|d| {
                d.compose_id == compose.compose_id
                    && !d.synced.thread_creation_pending
                    && !d.synced.send_pending
            }) && !self.draft_writes.contains_key(&compose.compose_id)
            {
                self.draft_store.remove_mail(&key);
                self.persist_draft_store()?;
            }
        }
        Ok(())
    }

    pub fn has_pending_draft_write(&self) -> bool {
        !self.draft_writes.is_empty()
    }

    pub(super) fn stored_compose_key(&self, key: &str, compose_id: &str) -> Option<String> {
        if self
            .draft_store
            .get_mail(key)
            .is_some_and(|draft| draft.compose_id == compose_id)
        {
            return Some(key.to_string());
        }
        let previous_key = format!("{key}:{compose_id}");
        self.draft_store
            .get_mail(&previous_key)
            .map(|_| previous_key)
    }

    pub(super) fn store_compose(&mut self, compose: &MailComposeOverlay) -> anyhow::Result<MailDraftEntry> {
        let key = compose.mode.draft_key();
        let mut entry = compose.to_draft_entry();
        if let Some(stored_key) = self.stored_compose_key(&key, &compose.compose_id) {
            if let Some(previous) = self.draft_store.get_mail(&stored_key) {
                entry.draft_id.clone_from(&previous.draft_id);
                entry.synced.clone_from(&previous.synced);
            }
            if !matches!(compose.mode, MailComposeMode::New)
                && entry.draft_id.is_some()
                && entry.synced.thread_quote_html.is_none()
            {
                // Earlier versions bound replies to top-level drafts.
                entry.draft_id = None;
                entry.synced = Default::default();
            }
            self.draft_store.mail.insert(stored_key, entry.clone());
        } else {
            if let Some(mut previous) = self.draft_store.remove_mail(&key) {
                if previous.compose_id.is_empty() {
                    previous.compose_id = crate::new_uuid_v4();
                }
                self.draft_store
                    .set_mail(format!("{key}:{}", previous.compose_id), previous);
            }
            self.draft_store.set_mail(key, entry.clone());
        }
        self.persist_draft_store()?;
        Ok(entry)
    }

    pub fn save_mail_draft(&mut self, ui: &mut UiState, compose: &MailComposeOverlay) {
        if compose.to_draft_entry().is_empty()
            && !self.draft_writes.contains_key(&compose.compose_id)
        {
            ui.info("Enter draft content before saving.");
            return;
        }
        let entry = match self.store_compose(compose) {
            Ok(entry) => entry,
            Err(err) => {
                Self::show_draft_error(
                    ui,
                    &compose.compose_id,
                    format!("Cannot save draft locally: {err:#}"),
                );
                return;
            }
        };
        if let Some(in_flight) = self.draft_writes.get_mut(&compose.compose_id) {
            in_flight.queued = Some(compose.clone());
        } else if tokio::runtime::Handle::try_current().is_ok() {
            if !self.start_draft_write(ui, compose, entry, false) {
                return;
            }
        } else {
            ui.success("Draft saved locally");
            return;
        }
        ui.info("Draft saved locally. Saving to Outlook…");
    }

    pub(super) fn outgoing_draft(
        compose: &MailComposeOverlay,
        entry: &MailDraftEntry,
        include_body: bool,
    ) -> OutgoingMail {
        let body_html = if include_body {
            let html = parser::compile_outgoing_mail_html(&entry.body, &entry.inline_images, None);
            rewrite_inline_image_cids(&html, &entry.inline_images)
        } else {
            String::new()
        };
        OutgoingMail {
            subject: entry.subject.trim().to_string(),
            body_html,
            to: compose.parsed_to(),
            cc: compose.parsed_cc(),
            bcc: compose.parsed_bcc(),
            attachments: entry
                .attachments
                .iter()
                .filter(|p| !entry.synced.files.contains(p))
                .cloned()
                .collect(),
            inline_images: entry
                .inline_images
                .iter()
                .filter(|img| !entry.synced.image_ids.contains(&img.content_id))
                .cloned()
                .collect(),
            draft_id: entry.draft_id.clone(),
            in_reply_to_id: entry.reply_to_id.clone(),
            reply_all: matches!(compose.mode, MailComposeMode::ReplyAll { .. }),
            forward_of_id: entry.forward_of_id.clone(),
            ..Default::default()
        }
    }

    pub(super) fn start_draft_write(
        &mut self,
        ui: &mut UiState,
        compose: &MailComposeOverlay,
        entry: MailDraftEntry,
        send: bool,
    ) -> bool {
        let draft_key = compose.mode.draft_key();
        let compose_id = compose.compose_id.clone();
        let creating_thread = !matches!(compose.mode, MailComposeMode::New) && entry.draft_id.is_none();
        if entry.synced.thread_creation_pending || entry.synced.send_pending {
            let error = if entry.synced.thread_creation_pending {
                "Outlook did not confirm draft creation. Check Outlook Drafts for this reply or forward."
            } else {
                "Outlook did not confirm the send. Check Outlook Drafts and Sent Items."
            };
            Self::show_draft_error(ui, &compose_id, error.into());
            return false;
        }
        if creating_thread || send {
            let Some(key) = self.stored_compose_key(&draft_key, &compose_id) else {
                return false;
            };
            let Some(local) = self.draft_store.mail.get_mut(&key) else { return false };
            local.synced.thread_creation_pending = creating_thread;
            local.synced.send_pending = send;
            if let Err(err) = self.persist_draft_store() {
                if let Some(local) = self.draft_store.mail.get_mut(&key) {
                    local.synced.thread_creation_pending = false;
                    local.synced.send_pending = false;
                }
                Self::show_draft_error(
                    ui,
                    &compose_id,
                    format!("Cannot save draft locally: {err:#}"),
                );
                return false;
            }
        }
        let image_refs: Vec<_> = entry
            .inline_images
            .iter()
            .map(|img| (&img.temp_id, &img.content_id, &img.file_name))
            .collect();
        let body_hash = crate::hex_encode(Sha256::digest(
            serde_json::to_vec(&(&entry.body, image_refs)).expect("draft body consists of strings"),
        ));
        let fields = MailDraftFields {
            subject: entry.subject.trim().to_string(),
            to: compose.parsed_to(),
            cc: compose.parsed_cc(),
            bcc: compose.parsed_bcc(),
            body_hash,
        };
        let mut changed = Vec::new();
        if let Some(prior) = entry.synced.fields.as_ref() {
            for (field, differs) in [
                ("subject", prior.subject != fields.subject),
                ("toRecipients", prior.to != fields.to),
                ("ccRecipients", prior.cc != fields.cc),
                ("bccRecipients", prior.bcc != fields.bcc),
                ("body", prior.body_hash != fields.body_hash),
            ] {
                if differs {
                    changed.push(field);
                }
            }
        } else if let Some(defaults) = entry.thread_headers.as_ref() {
            changed.push("body");
            for (field, differs) in [
                ("subject", defaults.subject != fields.subject),
                ("toRecipients", defaults.to != fields.to),
                ("ccRecipients", defaults.cc != fields.cc),
                ("bccRecipients", defaults.bcc != fields.bcc),
            ] {
                if differs {
                    changed.push(field);
                }
            }
        } else {
            changed.extend([
                "subject",
                "toRecipients",
                "ccRecipients",
                "bccRecipients",
                "body",
            ]);
        }
        let outgoing = Self::outgoing_draft(compose, &entry, changed.contains(&"body"));
        self.draft_writes.insert(
            compose_id.clone(),
            PendingDraftWrite {
                fields,
                files: outgoing.attachments.clone(),
                images: outgoing
                    .inline_images
                    .iter()
                    .map(|img| img.content_id.clone())
                    .collect(),
                queued: None,
            },
        );

        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let quote = entry.synced.thread_quote_html;
        tokio::spawn(async move {
            let captured_draft_id: Arc<std::sync::Mutex<Option<String>>> =
                Arc::new(std::sync::Mutex::new(outgoing.draft_id.clone()));
            let captured_for_cb = Arc::clone(&captured_draft_id);
            let mut result = graph
                .save_draft_with_progress(
                    &outgoing,
                    Some(&changed),
                    quote.as_deref(),
                    !send,
                    |change| {
                        match &change {
                            DraftSaveChange::ThreadCreated { id, .. }
                            | DraftSaveChange::Fields { id, .. }
                                if !id.trim().is_empty() =>
                            {
                                *captured_for_cb.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(id.clone());
                            }
                            _ => {}
                        }
                        let _ = tx.send(UiMsg::MailDraftProgress {
                            draft_key: draft_key.clone(),
                            compose_id: compose_id.clone(),
                            change,
                        });
                    },
                )
                .await;
            let mut sent_draft_id: Option<String> = None;
            if let Ok(saved) = &result {
                if send {
                    let id = outgoing
                        .draft_id
                        .as_deref()
                        .or_else(|| saved.as_ref().map(|msg| msg.id.as_str()))
                        .map(str::to_string)
                        .or_else(|| {
                            captured_draft_id
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        });
                    match id {
                        Some(id) => {
                            if let Err(err) = graph.send_saved_draft(&id).await {
                                result = Err(err);
                            } else {
                                sent_draft_id = Some(id);
                            }
                        }
                        None => result = Err(anyhow::anyhow!("Outlook did not return a draft ID")),
                    }
                }
            }
            let _ = tx.send(UiMsg::MailDraftSaved {
                draft_key,
                compose_id,
                result,
                send,
                sent_draft_id,
            });
        });
        true
    }

    pub(super) fn remove_sent_draft_locally(&mut self, ui: &mut UiState, draft_id: &str) {
        let id = draft_id.trim();
        if id.is_empty() {
            return;
        }
        let id_owned = id.to_string();
        let _ = self
            .storage
            .delete_mail_messages_by_ids(std::slice::from_ref(&id_owned));
        if let Ok(folders) = self.storage.list_mail_folders() {
            self.mail_folders = folders;
            self.bump_folders_gen();
        }

        let prev_sel = ui.mail_list_scroll.selected_idx.unwrap_or(0);
        let was_in_list = self.mail_messages.iter().any(|m| m.id == id);
        let was_active = self.active_mail_id.as_deref() == Some(id);
        self.mail_messages.retain(|m| m.id != id);
        self.mail_detail_cache.remove(id);
        self.mail_detail_missing.remove(id);
        self.expanded_thread_ids.remove(id);
        if self.pinned_mail.as_ref().is_some_and(|p| p.id == id) {
            self.pinned_mail = None;
        }
        self.bump_mail_gen();
        self.search_results.retain(|h| h.id != id);
        if was_in_list || was_active {
            self.reselect_after_removal(ui, prev_sel);
        }
    }

    pub(super) fn persist_draft_store(&self) -> anyhow::Result<()> {
        self.draft_store.save_to(&self.draft_store_path)
    }

    pub(super) fn show_draft_error(ui: &mut UiState, compose_id: &str, error: String) {
        for overlay in &mut ui.overlays {
            if let Overlay::MailCompose(compose) = overlay {
                if compose.compose_id == compose_id {
                    compose.error = Some(error.clone());
                }
            }
        }
        ui.error(error);
    }

    pub fn download_mail_attachment(
        &mut self,
        ui: &mut UiState,
        msg_id: String,
        attach_index: usize,
    ) {
        let Some(msg) = self.resolve_mail_for_action(&msg_id) else {
            ui.info("Email not found");
            return;
        };
        let eff = parser::effective_mail_attachments(&msg);
        let Some(att) = eff.get(attach_index).cloned() else {
            ui.info("Attachment not found");
            return;
        };
        let att_name = if att.name.trim().is_empty() {
            "attachment".to_string()
        } else {
            att.name.clone()
        };
        let task_label = format!("Download {att_name}");
        ui.tasks.begin(task_label.clone());

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let dest = self.download_dir.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.download_mail_attachment(&msg_id, &att, &dest).await {
                Ok(file) => {
                    let msg = format!("Saved: {}", file.path.display());
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
                        toast: Some((format!("Download failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn search_local_mail(&mut self, ui: &mut UiState, query: String) {
        self.search_query = query.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_gen_atomic.store(generation, Ordering::SeqCst);
        self.search_remote_inflight = false;
        self.search_remote_done = false;
        self.search_error = None;

        let trimmed_len = query.trim().chars().count();
        if trimmed_len == 0 {
            self.search_results.clear();
            self.searching = false;
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            self.search_results = self
                .storage
                .search_mail(&query, None, 100)
                .unwrap_or_default();
            self.searching = false;
            return;
        }

        self.searching = true;
        self.search_remote_inflight = trimmed_len >= 2;
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let local_gen_atomic = Arc::clone(&self.search_gen_atomic);
        let local_query = query.clone();
        let task_label = "Search mail".to_string();
        ui.tasks.begin(task_label.clone());
        tokio::spawn(async move {
            let q = local_query.clone();
            let hits = tokio::task::spawn_blocking(move || {
                storage.search_mail(&q, None, 100).unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            if local_gen_atomic.load(Ordering::SeqCst) == generation {
                let _ = tx.send(UiMsg::SearchResults {
                    query: local_query,
                    generation,
                    hits,
                });
            }
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });

        if trimmed_len >= 2 {
            let graph = self.graph.clone();
            let storage = self.storage.clone();
            let tx = self.ui_tx.clone();
            let gen_atomic = Arc::clone(&self.search_gen_atomic);
            let remote_query = query;
            let remote_label = "Remote mail search".to_string();
            ui.tasks.begin(remote_label.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(350)).await;
                if gen_atomic.load(Ordering::SeqCst) != generation {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: remote_label,
                        error: None,
                        toast: None,
                    });
                    return;
                }
                match graph.search_mail_remote(&remote_query, 50).await {
                    Ok(messages) => {
                        let st = storage.clone();
                        let to_store = messages.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = st.upsert_mail_messages(&to_store);
                        })
                        .await;
                        if gen_atomic.load(Ordering::SeqCst) == generation {
                            let _ = tx.send(UiMsg::RemoteSearchResults {
                                query: remote_query,
                                generation,
                                messages,
                            });
                        }
                        let _ = tx.send(UiMsg::TaskDone {
                            label: remote_label,
                            error: None,
                            toast: None,
                        });
                    }
                    Err(err) => {
                        let err_msg = err.to_string();
                        if gen_atomic.load(Ordering::SeqCst) == generation {
                            let _ = tx.send(UiMsg::RemoteSearchFailed {
                                query: remote_query,
                                generation,
                                error: err_msg.clone(),
                            });
                        }
                        let _ = tx.send(UiMsg::TaskDone {
                            label: remote_label,
                            error: Some(err_msg),
                            toast: None,
                        });
                    }
                }
            });
        }
    }

    pub fn search_remote_mail(&mut self, ui: &mut UiState, query: String) {
        self.search_query = query.clone();
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_gen_atomic.store(generation, Ordering::SeqCst);
        self.search_remote_done = false;
        self.search_error = None;

        if query.trim().is_empty() || tokio::runtime::Handle::try_current().is_err() {
            self.searching = false;
            self.search_remote_inflight = false;
            return;
        }
        self.searching = true;
        self.search_remote_inflight = true;
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let gen_atomic = Arc::clone(&self.search_gen_atomic);
        let task_label = "Remote mail search".to_string();
        ui.tasks.begin(task_label.clone());
        tokio::spawn(async move {
            match graph.search_mail_remote(&query, 50).await {
                Ok(messages) => {
                    let st = storage.clone();
                    let to_store = messages.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = st.upsert_mail_messages(&to_store);
                    })
                    .await;
                    if gen_atomic.load(Ordering::SeqCst) == generation {
                        let _ = tx.send(UiMsg::RemoteSearchResults {
                            query,
                            generation,
                            messages,
                        });
                    }
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let err_msg = err.to_string();
                    if gen_atomic.load(Ordering::SeqCst) == generation {
                        let _ = tx.send(UiMsg::RemoteSearchFailed {
                            query,
                            generation,
                            error: err_msg.clone(),
                        });
                    }
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err_msg.clone()),
                        toast: Some((format!("Search failed: {err_msg}"), ToastKind::Error)),
                    });
                }
            }
        });
    }
}

/// Puts the cache rows of `snapshots` back into the folder each one was in before an optimistic move.
pub(super) async fn restore_mail_folders_local(storage: &Storage, snapshots: &[MailMessage]) {
    let targets: Vec<(String, String)> = snapshots
        .iter()
        .filter_map(|m| Some((m.id.clone(), m.folder_id.clone()?)))
        .collect();
    if targets.is_empty() {
        return;
    }
    let st = storage.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for (id, folder_id) in &targets {
            let _ = st.move_mail_folder_local(id, folder_id);
        }
    })
    .await;
}
