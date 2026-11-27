//! Bootstrap, sync focus coordination, folder/row/list navigation, and adjacent prefetching.

use crate::auth;
use crate::sync::{ActiveView, FocusTarget, SyncCommand};

use super::super::nav_pane::{
    find_folder_by_wellknown, TAB_CALENDAR, TAB_TASKS, VIRTUAL_FLAGGED_FOLDER,
    VIRTUAL_UNREAD_FOLDER,
};
use super::{AppState, MailListRow, ScrollState, UiMsg, UiState};

impl AppState {
    pub fn spawn_bootstrap(&mut self, ui: &mut UiState) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Syncing Outlook".to_string();
        ui.tasks.begin(task_label.clone());
        self.syncing = true;
        self.sync_summary = "Syncing...".to_string();

        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let (tok, (_bootstrap_res, me)) = tokio::join!(
                auth.current_tokens(),
                crate::sync::bootstrap_outlook_with_profile(&graph, &storage),
            );
            let scope_warning = tok.as_ref().and_then(|t| {
                auth::diagnose_access_token_scopes(&t.access_token)
                    .warnings
                    .into_iter()
                    .next()
            });
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.email.clone())
                    .await;
            }

            let read_st = storage.clone();
            let (folders, msgs, events, contacts, lists, tasks) = tokio::task::spawn_blocking(move || {
                let folders = read_st.list_mail_folders().unwrap_or_default();
                let inbox_id =
                    find_folder_by_wellknown(&folders, "inbox", &["Inbox", "받은 편지함"])
                        .map(|f| f.id.clone())
                        .unwrap_or_else(|| "inbox".to_string());
                let msgs = read_st
                    .list_mail_summaries_filtered(Some(&inbox_id), false, false, false, 150)
                    .unwrap_or_default();
                let events = read_st
                    .list_calendar_events(
                        Some(&crate::sync::calendar_cache_window_start()),
                        None,
                        200,
                    )
                    .unwrap_or_default();
                let contacts = read_st.list_contacts(None, 200).unwrap_or_default();
                let lists = read_st.list_todo_lists().unwrap_or_default();
                let def_list_id = lists
                    .iter()
                    .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
                    .or_else(|| lists.first())
                    .map(|l| l.id.clone());
                let tasks = match def_list_id {
                    Some(ref lid) => read_st.list_todo_tasks(Some(lid), true).unwrap_or_default(),
                    None => Vec::new(),
                };
                (folders, msgs, events, contacts, lists, tasks)
            })
            .await
            .unwrap_or_default();

            let _ = tx.send(UiMsg::BootstrapDone {
                me,
                mail_folders: folders,
                mail_messages: msgs,
                calendar_events: events,
                contacts,
                todo_lists: lists,
                todo_tasks: tasks,
                scope_warning,
            });
            let _ = tx.send(UiMsg::TaskDone {
                label: task_label,
                error: None,
                toast: None,
            });
        });
    }

    pub fn spawn_me_profile_if_needed(&self) {
        if self.my_profile.is_some() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let auth = self.auth.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let (tok, me_res) = tokio::join!(auth.current_tokens(), graph.get_me());
            let scope_warning = tok.as_ref().and_then(|t| {
                auth::diagnose_access_token_scopes(&t.access_token)
                    .warnings
                    .into_iter()
                    .next()
            });
            let me = me_res.ok();
            if let Some(ref u) = me {
                let _ = auth
                    .update_identity(u.id.clone(), u.display_name.clone(), u.email.clone())
                    .await;
            }
            let _ = tx.send(UiMsg::ProfileLoaded { me, scope_warning });
        });
    }

    pub fn active_view_for_tab(tab: usize) -> ActiveView {
        match tab {
            TAB_CALENDAR => ActiveView::Calendar,
            TAB_TASKS => ActiveView::Tasks,
            _ => ActiveView::Mail,
        }
    }

    pub fn push_focus_to_sync(&self) {
        let folder_id = if self.active_folder_id == VIRTUAL_UNREAD_FOLDER
            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
        {
            None
        } else {
            Some(self.active_folder_id.clone())
        };
        self.sync_ctrl.set_focus(FocusTarget {
            active_view: self.active_view,
            active_folder_id: folder_id,
            active_message_id: self.active_mail_id.clone(),
            active_todo_list_id: self.active_todo_list_id.clone(),
        });
    }

    /// Synchronizes the active tab's view immediately (used on tab switch and terminal FocusGained).
    /// Virtual mail folders (`Unread`, `Flagged`) reuse the existing Inbox folder delta scope.
    pub fn request_sync_for_active_view(&mut self, tab: usize) {
        let view = Self::active_view_for_tab(tab);
        self.active_view = view;
        self.push_focus_to_sync();
        match view {
            ActiveView::Mail => {
                let folder_id = if self.active_folder_id == VIRTUAL_UNREAD_FOLDER
                    || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
                {
                    self.find_inbox_folder_id()
                        .unwrap_or_else(|| "inbox".to_string())
                } else {
                    self.active_folder_id.clone()
                };
                self.sync_ctrl.request(SyncCommand::SyncFolder { folder_id });
            }
            ActiveView::Calendar => {
                self.sync_ctrl.request(SyncCommand::SyncCalendar);
            }
            ActiveView::Tasks => {
                self.sync_ctrl.request(SyncCommand::SyncTodo {
                    list_id: self.active_todo_list_id.clone(),
                });
            }
        }
    }

    pub fn request_contacts_sync(&self) {
        self.sync_ctrl.request(SyncCommand::SyncContacts);
    }

    pub fn select_mail_folder(&mut self, ui: &mut UiState, folder_id: String) {
        self.active_view = ActiveView::Mail;
        self.active_folder_id = folder_id.clone();
        self.pinned_mail = None;
        self.clear_mail_detail_cache();
        self.mail_oldest_reached = false;
        self.loading_older_mail = false;
        self.mail_history_seq = self.mail_history_seq.wrapping_add(1);
        self.mail_history_boundary = None;
        ui.mail_list_scroll = ScrollState::default();
        ui.mail_read_scroll = ScrollState::default();

        self.mail_messages = self
            .load_folder_summaries(&folder_id, 150)
            .unwrap_or_default();

        self.select_first_visible_mail(ui);
        let rows = self.visible_mail_rows_cached(ui);
        if let Some(sel) = ui.mail_list_scroll.selected_idx {
            self.queue_adjacent_mail_prefetch(&rows, sel);
        }
        self.push_focus_to_sync();

        if folder_id != VIRTUAL_UNREAD_FOLDER && folder_id != VIRTUAL_FLAGGED_FOLDER {
            if self.mail_messages.is_empty() {
                self.loading_mail = true;
            }
            self.sync_ctrl.request(SyncCommand::SyncFolder { folder_id });
        }
    }

    /// Opportunistically prefetches uncached message bodies for rows adjacent to `selected_row_idx`
    /// (next 2 + previous 1) in a single bounded `$batch` call on interactive navigation only.
    pub(super) fn queue_adjacent_mail_prefetch(&self, rows: &[MailListRow], selected_row_idx: usize) {
        if rows.len() <= 1 {
            return;
        }
        let mut candidate_rows = Vec::with_capacity(3);
        if selected_row_idx + 1 < rows.len() {
            candidate_rows.push(selected_row_idx + 1);
        }
        if selected_row_idx + 2 < rows.len() {
            candidate_rows.push(selected_row_idx + 2);
        }
        if let Some(prev) = selected_row_idx.checked_sub(1) {
            candidate_rows.push(prev);
        }

        let active_id = self.active_mail_id.as_deref();
        let mut message_ids = Vec::with_capacity(candidate_rows.len());
        for idx in candidate_rows {
            let Some(msg) = self.mail_messages.get(rows[idx].primary_msg_idx) else {
                continue;
            };
            if active_id == Some(msg.id.as_str())
                || msg.body_loaded
                || self
                    .mail_detail_cache
                    .get(&msg.id)
                    .is_some_and(|cached| cached.body_loaded)
                || message_ids.iter().any(|id| id == &msg.id)
            {
                continue;
            }
            message_ids.push(msg.id.clone());
        }
        if !message_ids.is_empty() {
            self.sync_ctrl
                .request(SyncCommand::PrefetchMessages { message_ids });
        }
    }

    pub fn select_first_visible_mail(&mut self, ui: &mut UiState) {
        let rows = self.visible_mail_rows_cached(ui);
        if let Some(first) = rows.first() {
            ui.mail_list_scroll.selected_idx = Some(0);
            if let Some(msg_id) = self
                .mail_messages
                .get(first.primary_msg_idx)
                .map(|m| m.id.clone())
            {
                self.active_mail_id = Some(msg_id);
                self.sync_displayed_mail_details(ui.group_threads, true);
            }
        } else {
            ui.mail_list_scroll.selected_idx = None;
            self.active_mail_id = None;
            self.clear_mail_detail_cache();
        }
    }

    pub fn select_mail_row(&mut self, ui: &mut UiState, row_idx: usize) {
        let rows = self.visible_mail_rows_cached(ui);
        if rows.is_empty() {
            ui.mail_list_scroll.selected_idx = None;
            self.active_mail_id = None;
            self.clear_mail_detail_cache();
            return;
        }
        let clamped = row_idx.min(rows.len() - 1);
        ui.mail_list_scroll.selected_idx = Some(clamped);
        ui.mail_read_scroll.offset = 0;

        if let Some(msg_id) = self
            .mail_messages
            .get(rows[clamped].primary_msg_idx)
            .map(|m| m.id.clone())
        {
            if self
                .pinned_mail
                .as_ref()
                .is_some_and(|p| p.id != msg_id)
            {
                self.pinned_mail = None;
                self.bump_mail_gen();
            }
            self.active_mail_id = Some(msg_id);
            self.sync_displayed_mail_details(ui.group_threads, true);
            self.queue_adjacent_mail_prefetch(&rows, clamped);
            self.push_focus_to_sync();
        }

        // Auto-load older mail when reaching near bottom of folder list
        if clamped + 2 >= rows.len() {
            self.maybe_autoload_older_mail();
        }
    }

    pub fn maybe_autoload_older_mail(&mut self) {
        if self.loading_older_mail
            || self.mail_oldest_reached
            || self.active_folder_id == VIRTUAL_UNREAD_FOLDER
            || self.active_folder_id == VIRTUAL_FLAGGED_FOLDER
        {
            return;
        }
        self.mail_history_seq = self.mail_history_seq.wrapping_add(1);
        let seq = self.mail_history_seq;
        self.loading_older_mail = true;
        self.sync_ctrl.request(SyncCommand::LoadOlderMail {
            folder_id: self.active_folder_id.clone(),
            explicit: false,
            seq,
        });
    }

    pub(super) fn reselect_after_removal(&mut self, ui: &mut UiState, prev_sel: usize) {
        let rows = self.visible_mail_rows_cached(ui);
        if rows.is_empty() {
            ui.mail_list_scroll.selected_idx = None;
            self.active_mail_id = None;
            self.clear_mail_detail_cache();
        } else {
            let next_idx = prev_sel.min(rows.len() - 1);
            self.select_mail_row(ui, next_idx);
        }
    }

    pub fn select_todo_list(&mut self, ui: &mut UiState, list_id: String) {
        self.active_view = ActiveView::Tasks;
        self.active_todo_list_id = Some(list_id.clone());
        ui.todo_list_scroll = ScrollState::default();
        ui.todo_detail_scroll = ScrollState::default();
        self.todo_tasks = self
            .storage
            .list_todo_tasks(Some(&list_id), true)
            .unwrap_or_default();
        self.reconcile_todo_selection(ui);
        self.push_focus_to_sync();
        self.sync_ctrl.request(SyncCommand::SyncTodo {
            list_id: Some(list_id),
        });
    }
}
