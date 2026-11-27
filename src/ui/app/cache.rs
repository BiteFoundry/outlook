//! Local cache loading, folder/thread projections, and selection reconciliation.

use std::collections::HashMap;

use chrono::{Datelike, Duration, Local};

use crate::models::{CalendarEvent, MailMessage, TodoTask};
use crate::sync::SyncCommand;

use super::super::nav_pane::{
    find_folder_by_wellknown, VIRTUAL_FLAGGED_FOLDER, VIRTUAL_UNREAD_FOLDER,
};
use super::{AppState, CalendarRangeFilter, MailListRow, UiState};

impl AppState {
    /// Start-up load: the cached Inbox, with the selection on the first message the active list
    /// filters actually show (the newest message can be hidden by the Focused filter).
    pub fn load_cached_startup(&mut self, ui: &mut UiState) {
        self.load_cached_initial();
        if ui.focused_only {
            self.select_first_visible_mail(ui);
        }
    }

    pub fn load_cached_initial(&mut self) {
        if let Ok(folders) = self.storage.list_mail_folders() {
            self.mail_folders = folders;
            if let Some(inbox) =
                find_folder_by_wellknown(&self.mail_folders, "inbox", &["Inbox", "받은 편지함"])
            {
                self.active_folder_id = inbox.id.clone();
            }
        }
        if let Ok(msgs) = self.storage.list_mail_summaries_filtered(
            Some(&self.active_folder_id),
            false,
            false,
            false,
            150,
        ) {
            self.mail_messages = msgs;
            self.clear_mail_detail_cache();
            if let Some(first_id) = self.mail_messages.first().map(|m| m.id.clone()) {
                self.active_mail_id = Some(first_id.clone());
                self.load_mail_detail(&first_id, true);
            } else {
                self.active_mail_id = None;
            }
        }
        let calendar_start = crate::sync::calendar_cache_window_start();
        if let Ok(events) = self
            .storage
            .list_calendar_events(Some(&calendar_start), None, 200)
        {
            self.calendar_events = events;
            if let Some(first) = self.calendar_events.first() {
                self.active_event_id = Some(first.id.clone());
            }
        }
        if let Ok(contacts) = self.storage.list_contacts(None, 200) {
            self.contacts = contacts;
        }
        if let Ok(lists) = self.storage.list_todo_lists() {
            self.todo_lists = lists;
            if let Some(def) = self
                .todo_lists
                .iter()
                .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
                .or_else(|| self.todo_lists.first())
            {
                let lid = def.id.clone();
                self.active_todo_list_id = Some(lid.clone());
                if let Ok(tasks) = self.storage.list_todo_tasks(Some(&lid), true) {
                    self.todo_tasks = tasks;
                    if let Some(first) = self.todo_tasks.first() {
                        self.active_todo_task_id = Some(first.id.clone());
                    }
                }
            }
        }
    }

    pub fn unread_inbox_count(&self) -> i32 {
        find_folder_by_wellknown(&self.mail_folders, "inbox", &["Inbox", "받은 편지함"])
            .map(|f| f.unread_item_count.max(0))
            .unwrap_or_else(|| {
                self.mail_messages
                    .iter()
                    .filter(|m| !m.is_read)
                    .count() as i32
            })
    }

    /// Mailbox-wide unread count across all non-Deleted-Items folders (never limited to the loaded slice).
    pub fn smart_unread_count(&self) -> i32 {
        if !self.mail_folders.is_empty() {
            let folder_sum: i32 = self
                .mail_folders
                .iter()
                .filter(|f| {
                    !self.is_deleted_items_folder(&f.id)
                        && !f
                            .well_known_name
                            .as_deref()
                            .is_some_and(|w| w.eq_ignore_ascii_case("deleteditems"))
                })
                .map(|f| f.unread_item_count.max(0))
                .sum();
            if folder_sum > 0 {
                return folder_sum;
            }
        }
        let trash_id = self.find_trash_folder_id();
        let db_count = self
            .storage
            .count_smart_folder_messages(true, false, trash_id.as_deref())
            .unwrap_or(0);
        if db_count > 0 {
            return db_count;
        }
        if self.mail_folders.is_empty()
            && self
                .storage
                .list_mail_summaries(None, 1)
                .map(|v| v.is_empty())
                .unwrap_or(false)
        {
            self.mail_messages.iter().filter(|m| !m.is_read).count() as i32
        } else {
            0
        }
    }

    /// Mailbox-wide flagged count from SQLite (excluding Deleted Items).
    pub fn smart_flagged_count(&self) -> i32 {
        let trash_id = self.find_trash_folder_id();
        let db_count = self
            .storage
            .count_smart_folder_messages(false, true, trash_id.as_deref())
            .unwrap_or(0);
        if db_count > 0 {
            return db_count;
        }
        if self
            .storage
            .list_mail_summaries(None, 1)
            .map(|v| v.is_empty())
            .unwrap_or(false)
        {
            self.mail_messages.iter().filter(|m| m.flagged).count() as i32
        } else {
            0
        }
    }

    pub fn active_folder_display_name(&self) -> String {
        if self.active_folder_id == VIRTUAL_UNREAD_FOLDER {
            return "Unread".to_string();
        }
        if self.active_folder_id == VIRTUAL_FLAGGED_FOLDER {
            return "Flagged".to_string();
        }
        self.mail_folders
            .iter()
            .find(|f| f.id == self.active_folder_id)
            .map(|f| f.display_name.clone())
            .unwrap_or_else(|| self.active_folder_id.clone())
    }

    pub fn visible_mail_rows(&self, ui: &UiState) -> Vec<MailListRow> {
        let mut indices: Vec<usize> = self
            .mail_messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                if self.active_folder_id == VIRTUAL_UNREAD_FOLDER && m.is_read {
                    return false;
                }
                if self.active_folder_id == VIRTUAL_FLAGGED_FOLDER && !m.flagged {
                    return false;
                }
                if ui.mail_unread_only && m.is_read {
                    return false;
                }
                if ui.focused_only && !m.is_focused() {
                    return false;
                }
                true
            })
            .map(|(i, _)| i)
            .collect();

        // Ensure newest first
        indices.sort_by(|&a, &b| {
            self.mail_messages[b]
                .received_at
                .cmp(&self.mail_messages[a].received_at)
        });

        if !ui.group_threads {
            return indices
                .into_iter()
                .map(|idx| {
                    let m = &self.mail_messages[idx];
                    MailListRow {
                        primary_msg_idx: idx,
                        thread_msg_indices: vec![idx],
                        unread_count: usize::from(!m.is_read),
                        any_flagged: m.flagged,
                        any_attachments: m.has_attachments || !m.attachments.is_empty(),
                        count: 1,
                    }
                })
                .collect();
        }

        let mut rows: Vec<MailListRow> = Vec::new();
        let mut conv_to_row: HashMap<String, usize> = HashMap::new();
        for idx in indices {
            let m = &self.mail_messages[idx];
            let conv_key = m
                .conversation_id
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| m.id.clone());
            if let Some(&pos) = conv_to_row.get(&conv_key) {
                let row = &mut rows[pos];
                row.thread_msg_indices.push(idx);
                row.unread_count += usize::from(!m.is_read);
                row.any_flagged |= m.flagged;
                row.any_attachments |= m.has_attachments || !m.attachments.is_empty();
                row.count += 1;
            } else {
                let pos = rows.len();
                conv_to_row.insert(conv_key, pos);
                rows.push(MailListRow {
                    primary_msg_idx: idx,
                    thread_msg_indices: vec![idx],
                    unread_count: usize::from(!m.is_read),
                    any_flagged: m.flagged,
                    any_attachments: m.has_attachments || !m.attachments.is_empty(),
                    count: 1,
                });
            }
        }
        rows
    }

    pub(crate) fn mail_summary_from(msg: &MailMessage) -> MailMessage {
        MailMessage {
            id: msg.id.clone(),
            internet_message_id: msg.internet_message_id.clone(),
            conversation_id: msg.conversation_id.clone(),
            folder_id: msg.folder_id.clone(),
            subject: msg.subject.clone(),
            from: msg.from.clone(),
            sender: msg.sender.clone(),
            to_recipients: msg.to_recipients.first().cloned().into_iter().collect(),
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: msg.received_at.clone(),
            sent_at: msg.sent_at.clone(),
            last_modified_at: msg.last_modified_at.clone(),
            is_read: msg.is_read,
            is_draft: msg.is_draft,
            flagged: msg.flagged,
            importance: msg.importance.clone(),
            inference_classification: msg.inference_classification.clone(),
            categories: msg.categories.clone(),
            has_attachments: msg.has_attachments || !msg.attachments.is_empty(),
            body_loaded: msg.body_loaded,
            preview: msg.preview.clone(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: Vec::new(),
            meeting_request: msg.meeting_request.clone(),
            web_link: msg.web_link.clone(),
        }
    }

    pub fn mail_detail(&self, id: &str) -> Option<&MailMessage> {
        self.mail_detail_cache
            .get(id)
            .or_else(|| self.pinned_mail.as_ref().filter(|m| m.id == id))
            .or_else(|| self.mail_messages.iter().find(|m| m.id == id))
    }

    pub fn active_mail(&self) -> Option<&MailMessage> {
        let id = self.active_mail_id.as_deref()?;
        self.mail_detail(id)
    }

    pub fn active_mail_mut(&mut self) -> Option<&mut MailMessage> {
        let id = self.active_mail_id.as_deref()?.to_string();
        if self.mail_detail_cache.contains_key(&id) {
            return self.mail_detail_cache.get_mut(&id);
        }
        if self.pinned_mail.as_ref().is_some_and(|m| m.id == id) {
            return self.pinned_mail.as_mut();
        }
        if let Some(idx) = self.mail_messages.iter().position(|m| m.id == id) {
            return Some(&mut self.mail_messages[idx]);
        }
        None
    }

    pub(crate) fn clear_mail_detail_cache(&mut self) {
        self.mail_detail_cache.clear();
        self.mail_detail_missing.clear();
    }

    pub(crate) fn cache_loaded_mail_detail(&mut self, msg: MailMessage) {
        self.mail_detail_missing.remove(&msg.id);
        self.mail_detail_cache.insert(msg.id.clone(), msg);
    }

    pub(crate) fn resolve_mail_for_action(&mut self, id: &str) -> Option<MailMessage> {
        if let Some(cached) = self.mail_detail_cache.get(id) {
            return Some(cached.clone());
        }
        if let Some(pinned) = self.pinned_mail.as_ref().filter(|m| m.id == id) {
            return Some(pinned.clone());
        }
        if !self.mail_detail_missing.contains(id) {
            if let Ok(Some(full)) = self.storage.get_mail_message(id) {
                return Some(full);
            }
        }
        self.mail_messages.iter().find(|m| m.id == id).cloned()
    }

    pub(super) fn displayed_mail_ids(&self, group_threads: bool) -> Vec<String> {
        let Some(active_id) = self.active_mail_id.as_deref() else {
            return Vec::new();
        };
        let mut ids = vec![active_id.to_string()];
        if !group_threads {
            return ids;
        }
        let conv_id = self
            .mail_messages
            .iter()
            .find(|m| m.id == active_id)
            .or_else(|| self.pinned_mail.as_ref().filter(|m| m.id == active_id))
            .or_else(|| self.mail_detail_cache.get(active_id))
            .and_then(|m| m.conversation_id.as_deref())
            .filter(|s| !s.is_empty());
        let Some(cid) = conv_id else {
            return ids;
        };
        let mut thread_refs: Vec<&MailMessage> = self
            .mail_messages
            .iter()
            .filter(|m| m.conversation_id.as_deref() == Some(cid))
            .collect();
        thread_refs.sort_by(|a, b| a.received_at.cmp(&b.received_at));
        if let Some(latest) = thread_refs.last() {
            if !ids.iter().any(|id| id == &latest.id) {
                ids.push(latest.id.clone());
            }
        }
        for msg in thread_refs {
            if self.expanded_thread_ids.contains(&msg.id)
                && !ids.iter().any(|id| id == &msg.id)
            {
                ids.push(msg.id.clone());
            }
        }
        ids
    }

    pub(super) fn load_mail_detail(&mut self, id: &str, request_hydrate: bool) {
        if !self.mail_detail_cache.contains_key(id) && !self.mail_detail_missing.contains(id) {
            match self.storage.get_mail_message(id) {
                Ok(Some(full)) => {
                    self.mail_detail_cache.insert(id.to_string(), full);
                }
                Ok(None) => {
                    self.mail_detail_missing.insert(id.to_string());
                }
                Err(_) => {}
            }
        }
        if request_hydrate {
            if let Some(msg) = self.mail_detail(id) {
                if !msg.body_loaded {
                    self.sync_ctrl.request(SyncCommand::HydrateMessage {
                        message_id: id.to_string(),
                    });
                }
            }
        }
    }

    pub(crate) fn sync_displayed_mail_details(
        &mut self,
        group_threads: bool,
        request_hydrate: bool,
    ) {
        let needed = self.displayed_mail_ids(group_threads);
        self.mail_detail_cache
            .retain(|id, _| needed.iter().any(|k| k == id));
        self.mail_detail_missing
            .retain(|id| needed.iter().any(|k| k == id));
        for id in needed {
            self.load_mail_detail(&id, request_hydrate);
        }
    }

    pub(crate) fn ensure_displayed_mail_details_local(&mut self, ui: &UiState) {
        let needed = self.displayed_mail_ids(ui.group_threads);
        for id in needed {
            self.load_mail_detail(&id, false);
        }
    }

    pub(super) fn find_inbox_folder_id(&self) -> Option<String> {
        find_folder_by_wellknown(&self.mail_folders, "inbox", &["Inbox", "받은 편지함"])
            .map(|f| f.id.clone())
    }

    pub(super) fn find_trash_folder_id(&self) -> Option<String> {
        find_folder_by_wellknown(
            &self.mail_folders,
            "deleteditems",
            &["Deleted Items", "Trash", "지운 편지함"],
        )
        .map(|f| f.id.clone())
    }

    pub(super) fn is_deleted_items_folder(&self, folder_id: &str) -> bool {
        let trimmed = folder_id.trim();
        if trimmed.eq_ignore_ascii_case("deleteditems")
            || trimmed.eq_ignore_ascii_case("trash")
            || trimmed.eq_ignore_ascii_case("deleted")
        {
            return true;
        }
        self.find_trash_folder_id()
            .is_some_and(|trash_id| trash_id == trimmed)
    }

    pub(super) fn folder_matches_active(&self, event_folder_id: &str) -> bool {
        if self.active_folder_id == event_folder_id {
            return true;
        }
        if self.active_folder_id == VIRTUAL_UNREAD_FOLDER
            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
        {
            return !self.is_deleted_items_folder(event_folder_id);
        }
        if event_folder_id.eq_ignore_ascii_case("inbox") {
            if let Some(inbox_id) = self.find_inbox_folder_id() {
                if self.active_folder_id == inbox_id {
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn message_matches_active_folder(&self, msg: &MailMessage) -> bool {
        let Some(fid) = msg.folder_id.as_deref() else {
            return false;
        };
        if !self.folder_matches_active(fid) {
            return false;
        }
        if self.active_folder_id == VIRTUAL_UNREAD_FOLDER {
            return !msg.is_read;
        }
        if self.active_folder_id == VIRTUAL_FLAGGED_FOLDER {
            return msg.flagged;
        }
        true
    }

    pub(super) fn load_folder_summaries(
        &self,
        folder_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MailMessage>> {
        if folder_id == VIRTUAL_UNREAD_FOLDER {
            let trash_id = self.find_trash_folder_id();
            self.storage
                .list_smart_folder_summaries(true, false, trash_id.as_deref(), limit)
        } else if folder_id == VIRTUAL_FLAGGED_FOLDER {
            let trash_id = self.find_trash_folder_id();
            self.storage
                .list_smart_folder_summaries(false, true, trash_id.as_deref(), limit)
        } else {
            self.storage
                .list_mail_summaries_filtered(Some(folder_id), false, false, false, limit)
        }
    }

    pub(super) fn preserve_pinned_mail_in_list(&mut self) {
        if let Some(pinned_id) = self.pinned_mail.as_ref().map(|p| p.id.clone()) {
            if let Ok(Some(fresh)) = self.storage.get_mail_message(&pinned_id) {
                self.pinned_mail = Some(fresh);
            }
        }
        if let Some(ref pinned) = self.pinned_mail {
            if self.active_mail_id.as_deref() == Some(pinned.id.as_str())
                && !self.mail_messages.iter().any(|m| m.id == pinned.id)
            {
                self.mail_messages.push(Self::mail_summary_from(pinned));
            }
        }
    }

    pub(super) fn hydrate_active_mail_if_needed(&mut self, group_threads: bool) {
        self.sync_displayed_mail_details(group_threads, true);
    }

    pub(super) fn refresh_mail_folders_from_storage(&mut self) {
        if let Ok(folders) = self.storage.list_mail_folders() {
            if !folders.is_empty() {
                self.mail_folders = folders;
            }
        }
    }

    pub(super) fn active_mail_is_visible(&self, ui: &UiState) -> bool {
        let Some(ref active_id) = self.active_mail_id else {
            return false;
        };
        let rows = self.visible_mail_rows(ui);
        rows.iter().any(|r| {
            r.thread_msg_indices
                .iter()
                .any(|&i| self.mail_messages[i].id == *active_id)
        })
    }

    pub(super) fn replace_mail_list(&mut self, ui: &mut UiState, messages: Vec<MailMessage>) {
        self.mail_messages = messages;
        self.clear_mail_detail_cache();
        self.preserve_pinned_mail_in_list();
        if self.active_mail().is_none() || !self.active_mail_is_visible(ui) {
            self.select_first_visible_mail(ui);
        } else {
            self.hydrate_active_mail_if_needed(ui.group_threads);
        }
        self.sync_mail_list_selection_to_active(ui);
    }

    pub(crate) fn sync_mail_list_selection_to_active(&self, ui: &mut UiState) {
        let Some(ref active_id) = self.active_mail_id else {
            return;
        };
        let rows = self.visible_mail_rows(ui);
        if let Some(pos) = rows.iter().position(|r| {
            r.thread_msg_indices
                .iter()
                .any(|&i| self.mail_messages[i].id == *active_id)
        }) {
            ui.mail_list_scroll.selected_idx = Some(pos);
        }
    }

    pub fn visible_calendar_events(&self, ui: &UiState) -> Vec<CalendarEvent> {
        let today = Local::now().date_naive();
        let mut list: Vec<CalendarEvent> = self
            .calendar_events
            .iter()
            .filter(|ev| {
                let Some(d) = super::super::calendar_view::event_local_date(ev) else {
                    return matches!(ui.cal_range, CalendarRangeFilter::AllUpcoming);
                };
                match ui.cal_range {
                    CalendarRangeFilter::Today => d == today,
                    CalendarRangeFilter::Tomorrow => d == today + Duration::days(1),
                    CalendarRangeFilter::Next7Days => d >= today && d <= today + Duration::days(7),
                    CalendarRangeFilter::ThisMonth => {
                        d.year() == today.year() && d.month() == today.month()
                    }
                    CalendarRangeFilter::AllUpcoming => true,
                    CalendarRangeFilter::SpecificDate(target) => d == target,
                }
            })
            .cloned()
            .collect();
        list.sort_by(|a, b| a.start_iso.cmp(&b.start_iso));
        list
    }

    pub fn active_event(&self) -> Option<&CalendarEvent> {
        let id = self.active_event_id.as_deref()?;
        self.calendar_events.iter().find(|e| e.id == id)
    }

    pub fn active_todo_task(&self) -> Option<&TodoTask> {
        let id = self.active_todo_task_id.as_deref()?;
        self.todo_tasks.iter().find(|t| t.id == id)
    }

    pub fn visible_todo_tasks(&self, ui: &UiState) -> Vec<TodoTask> {
        if !ui.todo_hide_completed {
            return self.todo_tasks.clone();
        }
        self.todo_tasks
            .iter()
            .filter(|t| !t.is_completed())
            .cloned()
            .collect()
    }

    pub(crate) fn reconcile_calendar_selection(&mut self, ui: &mut UiState) {
        let vis = self.visible_calendar_events(ui);
        if let Some(ref active_id) = self.active_event_id {
            if let Some(pos) = vis.iter().position(|e| e.id == *active_id) {
                ui.cal_list_scroll.selected_idx = Some(pos);
                return;
            }
        }
        self.active_event_id = vis.first().map(|e| e.id.clone());
        ui.cal_list_scroll.selected_idx = (!vis.is_empty()).then_some(0);
    }

    pub(crate) fn reconcile_todo_selection(&mut self, ui: &mut UiState) {
        let vis = self.visible_todo_tasks(ui);
        if let Some(ref active_id) = self.active_todo_task_id {
            if let Some(pos) = vis.iter().position(|t| t.id == *active_id) {
                ui.todo_list_scroll.selected_idx = Some(pos);
                return;
            }
        }
        self.active_todo_task_id = vis.first().map(|t| t.id.clone());
        ui.todo_list_scroll.selected_idx = (!vis.is_empty()).then_some(0);
    }

    /// Builds a context menu for `target` without forcing `overlays::context_menu` to depend on `AppState`.
    pub fn build_context_menu(
        &self,
        target: super::super::overlays::context_menu::ContextTarget,
        anchor: (u16, u16),
    ) -> super::super::overlays::context_menu::ContextMenu {
        let has_attachments = self
            .active_mail()
            .is_some_and(|m| m.has_attachments || !m.attachments.is_empty());
        let has_online_meeting = self
            .active_event()
            .and_then(|e| e.online_meeting_url.as_ref())
            .is_some();
        super::super::overlays::context_menu::ContextMenu::build(
            target,
            anchor,
            has_attachments,
            has_online_meeting,
        )
    }

    /// Triggers a one-shot contacts sync when `compose` is focused on a recipient header field.
    pub fn sync_compose_contacts_if_needed(
        &self,
        compose: &mut super::super::overlays::mail_compose::MailComposeOverlay,
    ) {
        if compose.claim_contacts_sync() {
            self.request_contacts_sync();
        }
    }
}
