//! Async Calendar and Microsoft To Do mutation requests.

use crate::models::{EventRsvpAction, MailMessage, OutgoingEvent, OutgoingTodoTask};

use super::super::toast::ToastKind;
use super::{AppState, UiMsg, UiState};

impl AppState {
    pub fn respond_calendar_event(
        &mut self,
        ui: &mut UiState,
        event_id: String,
        action: EventRsvpAction,
    ) {
        let status_str = match action {
            EventRsvpAction::Accept => "accepted",
            EventRsvpAction::TentativelyAccept => "tentativelyAccepted",
            EventRsvpAction::Decline => "declined",
        };
        let prev_status = self
            .calendar_events
            .iter()
            .find(|e| e.id == event_id)
            .map(|e| e.response_status.clone())
            .or_else(|| {
                self.storage
                    .get_calendar_event(&event_id)
                    .ok()
                    .flatten()
                    .map(|e| e.response_status)
            });
        let mut prev_mail_statuses: Vec<(String, Option<String>)> = Vec::new();
        let mut record_mail_prev = |m: &MailMessage| {
            if let Some(ref mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str())
                    && !prev_mail_statuses.iter().any(|(mid, _)| mid == &m.id)
                {
                    prev_mail_statuses.push((m.id.clone(), mr.response_status.clone()));
                }
            }
        };
        for m in &self.mail_messages {
            record_mail_prev(m);
        }
        for m in self.mail_detail_cache.values() {
            record_mail_prev(m);
        }
        if let Some(ref m) = self.pinned_mail {
            record_mail_prev(m);
        }

        if let Some(ev) = self.calendar_events.iter_mut().find(|e| e.id == event_id) {
            ev.response_status = status_str.to_string();
        }
        for m in &mut self.mail_messages {
            if let Some(ref mut mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str()) {
                    mr.response_status = Some(status_str.to_string());
                }
            }
        }
        for m in self.mail_detail_cache.values_mut() {
            if let Some(ref mut mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str()) {
                    mr.response_status = Some(status_str.to_string());
                }
            }
        }
        if let Some(ref mut m) = self.pinned_mail {
            if let Some(ref mut mr) = m.meeting_request {
                if mr.event_id.as_deref() == Some(event_id.as_str()) {
                    mr.response_status = Some(status_str.to_string());
                }
            }
        }
        let _ = self
            .storage
            .update_event_response_local(&event_id, status_str);
        ui.success(format!("RSVP: {status_str}"));

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let task_label = "Calendar RSVP".to_string();
        ui.tasks.begin(task_label.clone());
        tokio::spawn(async move {
            match graph.respond_calendar_event(&event_id, action, None).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::CalendarRsvpDone {
                        event_id,
                        response_status: status_str.to_string(),
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    if let Some(ref old_status) = prev_status {
                        let st = storage.clone();
                        let eid = event_id.clone();
                        let old = old_status.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st.update_event_response_local(&eid, &old)
                        })
                        .await;
                    }
                    let _ = tx.send(UiMsg::CalendarRsvpFailed {
                        event_id,
                        prev_status,
                        prev_mail_statuses,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn create_calendar_event(&mut self, ui: &mut UiState, ev: OutgoingEvent) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Create event".to_string();
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.create_calendar_event(&ev).await {
                Ok(created) => {
                    let st = storage.clone();
                    let c2 = created.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = st.upsert_calendar_events(std::slice::from_ref(&c2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::CalendarEventCreated(created));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Event created".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Event creation failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_selected_calendar_event(&mut self, ui: &mut UiState) {
        let Some(id) = self.active_event_id.clone() else {
            return;
        };
        let prev_event = self
            .storage
            .get_calendar_event(&id)
            .ok()
            .flatten()
            .or_else(|| self.calendar_events.iter().find(|e| e.id == id).cloned());
        let prev_sel = ui.cal_list_scroll.selected_idx.unwrap_or(0);
        self.calendar_events.retain(|e| e.id != id);
        let _ = self.storage.delete_calendar_event(&id);
        let visible = self.visible_calendar_events(ui);
        if visible.is_empty() {
            self.active_event_id = None;
            ui.cal_list_scroll.selected_idx = None;
        } else {
            let next_idx = prev_sel.min(visible.len() - 1);
            self.active_event_id = Some(visible[next_idx].id.clone());
            ui.cal_list_scroll.selected_idx = Some(next_idx);
        }
        ui.cal_detail_scroll.offset = 0;
        ui.info("Event deleted");

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.delete_calendar_event(&id).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::CalendarEventDeleted(id));
                }
                Err(err) => {
                    if let Some(ref ev) = prev_event {
                        let st = storage.clone();
                        let restore = ev.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st.upsert_calendar_events(std::slice::from_ref(&restore))
                        })
                        .await;
                    }
                    let _ = tx.send(UiMsg::CalendarEventDeleteFailed {
                        original_event: prev_event,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn toggle_todo_completed(&mut self, ui: &mut UiState) {
        let Some(task_id) = self.active_todo_task_id.clone() else {
            return;
        };
        let prev_sel = ui.todo_list_scroll.selected_idx.unwrap_or(0);
        let Some(task) = self.todo_tasks.iter_mut().find(|t| t.id == task_id) else {
            return;
        };
        let prev_task = task.clone();
        let completed = !task.is_completed();
        task.status = if completed {
            "completed".to_string()
        } else {
            "notStarted".to_string()
        };
        let list_id = task.list_id.clone();
        let _ = self.storage.set_todo_task_completed_local(&task_id, completed);
        if ui.todo_hide_completed && completed {
            let vis = self.visible_todo_tasks(ui);
            if vis.is_empty() {
                self.active_todo_task_id = None;
                ui.todo_list_scroll.selected_idx = None;
            } else {
                let next_idx = prev_sel.min(vis.len() - 1);
                self.active_todo_task_id = Some(vis[next_idx].id.clone());
                ui.todo_list_scroll.selected_idx = Some(next_idx);
            }
            ui.todo_detail_scroll.offset = 0;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph
                .set_todo_task_completed(&list_id, &task_id, completed)
                .await
            {
                Ok(updated) => {
                    let u2 = updated.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_todo_tasks(std::slice::from_ref(&u2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoTaskUpdated(updated));
                }
                Err(err) => {
                    let st = storage.clone();
                    let restore = prev_task.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        st.upsert_todo_tasks(std::slice::from_ref(&restore))
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoTaskToggleFailed {
                        original_task: prev_task,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn create_todo_task(&mut self, ui: &mut UiState, list_id: String, title: String) {
        let target_list = if list_id.trim().is_empty() {
            self.active_todo_list_id
                .clone()
                .unwrap_or_else(|| "Tasks".to_string())
        } else {
            list_id
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Create task".to_string();
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let outgoing = OutgoingTodoTask {
                title,
                body_plain: None,
                due_date_iso: None,
                importance: None,
            };
            match graph.create_todo_task(&target_list, &outgoing).await {
                Ok(created) => {
                    let c2 = created.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_todo_tasks(std::slice::from_ref(&c2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoTaskCreated(created));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Task added".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Add task failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn create_todo_list(&mut self, ui: &mut UiState, display_name: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let task_label = "Create task list".to_string();
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.create_todo_list(&display_name).await {
                Ok(created) => {
                    let c2 = created.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = storage.upsert_todo_lists(std::slice::from_ref(&c2));
                    })
                    .await;
                    let _ = tx.send(UiMsg::TodoListCreated(created));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some(("Task list created".to_string(), ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Create list failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }

    pub fn delete_selected_todo_task(&mut self, ui: &mut UiState) {
        let Some(task_id) = self.active_todo_task_id.clone() else {
            return;
        };
        let prev_task = self
            .todo_tasks
            .iter()
            .find(|t| t.id == task_id)
            .cloned();
        let list_id = prev_task
            .as_ref()
            .map(|t| t.list_id.clone())
            .or_else(|| self.active_todo_list_id.clone())
            .unwrap_or_default();
        let prev_sel = ui.todo_list_scroll.selected_idx.unwrap_or(0);

        self.todo_tasks.retain(|t| t.id != task_id);
        let _ = self.storage.delete_todo_task(&task_id);
        let vis = self.visible_todo_tasks(ui);
        if vis.is_empty() {
            self.active_todo_task_id = None;
            ui.todo_list_scroll.selected_idx = None;
        } else {
            let next_idx = prev_sel.min(vis.len() - 1);
            self.active_todo_task_id = Some(vis[next_idx].id.clone());
            ui.todo_list_scroll.selected_idx = Some(next_idx);
        }
        ui.todo_detail_scroll.offset = 0;
        ui.info("Task deleted");

        if tokio::runtime::Handle::try_current().is_err() || list_id.is_empty() {
            return;
        }
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.delete_todo_task(&list_id, &task_id).await {
                Ok(()) => {
                    let _ = tx.send(UiMsg::TodoTaskDeleted(task_id));
                }
                Err(err) => {
                    if let Some(ref task) = prev_task {
                        let st = storage.clone();
                        let restore = task.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st.upsert_todo_tasks(std::slice::from_ref(&restore))
                        })
                        .await;
                    }
                    let _ = tx.send(UiMsg::TodoTaskDeleteFailed {
                        original_task: prev_task,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Image Cache & Async Decode (inline halfblock + HD pixel preview)
    // ------------------------------------------------------------------
}
