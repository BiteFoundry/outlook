//! Microsoft Graph v1.0 Microsoft To Do operations (`/me/todo/lists`, `/me/todo/lists/{id}/tasks`).

use anyhow::{bail, Result};
use serde::Deserialize;

use super::{client::GraphListPage, GraphClient};
use crate::models::{OutgoingTodoTask, TodoList, TodoTask};

#[derive(Debug, Default, Deserialize)]
struct GraphTodoList {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "wellknownListName", default)]
    wellknown_list_name: Option<String>,
    #[serde(rename = "isOwner", default)]
    is_owner: bool,
    #[serde(rename = "isShared", default)]
    is_shared: bool,
}

impl From<GraphTodoList> for TodoList {
    fn from(raw: GraphTodoList) -> Self {
        let display_name = raw
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| raw.id.clone());
        Self {
            id: raw.id,
            display_name,
            wellknown_list_name: raw
                .wellknown_list_name
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            is_owner: raw.is_owner,
            is_shared: raw.is_shared,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphTodoBody {
    #[serde(rename = "contentType", default)]
    content_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphTodoDateTime {
    #[serde(rename = "dateTime", default)]
    date_time: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphTodoTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    importance: Option<String>,
    #[serde(default)]
    body: Option<GraphTodoBody>,
    #[serde(rename = "dueDateTime", default)]
    due_date_time: Option<GraphTodoDateTime>,
    #[serde(rename = "reminderDateTime", default)]
    reminder_date_time: Option<GraphTodoDateTime>,
    #[serde(rename = "completedDateTime", default)]
    completed_date_time: Option<GraphTodoDateTime>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
}

impl GraphTodoTask {
    fn into_todo_task(self, list_id: String) -> TodoTask {
        let body_plain = self.body.and_then(|b| {
            let content = b.content.unwrap_or_default();
            if content.trim().is_empty() {
                return None;
            }
            if b.content_type
                .as_deref()
                .map(|ct| ct.eq_ignore_ascii_case("html"))
                .unwrap_or(false)
            {
                let plain = crate::parser::html_and_cards_to_plain(&content, &[]);
                if plain.trim().is_empty() {
                    None
                } else {
                    Some(plain)
                }
            } else {
                Some(content)
            }
        });

        TodoTask {
            id: self.id,
            list_id,
            title: self.title.unwrap_or_default(),
            status: self
                .status
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "notStarted".to_string()),
            importance: self
                .importance
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "normal".to_string()),
            body_plain,
            due_date_iso: self
                .due_date_time
                .and_then(|d| d.date_time)
                .filter(|s| !s.trim().is_empty()),
            reminder_iso: self
                .reminder_date_time
                .and_then(|d| d.date_time)
                .filter(|s| !s.trim().is_empty()),
            completed_at: self
                .completed_date_time
                .and_then(|d| d.date_time)
                .filter(|s| !s.trim().is_empty()),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            updated_at: self.last_modified_date_time.filter(|s| !s.trim().is_empty()),
        }
    }
}

impl GraphClient {
    async fn resolve_todo_list_id(&self, list_id: &str) -> Result<String> {
        let trimmed = list_id.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("default")
            || trimmed.eq_ignore_ascii_case("tasks")
        {
            let lists = self.list_todo_lists().await?;
            if let Some(default_list) = lists.iter().find(|l| {
                l.wellknown_list_name
                    .as_deref()
                    .map(|w| w.eq_ignore_ascii_case("defaultList"))
                    .unwrap_or(false)
            }) {
                return Ok(default_list.id.clone());
            }
            if let Some(first) = lists.first() {
                return Ok(first.id.clone());
            }
            bail!("Microsoft To Do 기본 목록을 찾을 수 없습니다.");
        }
        Ok(trimmed.to_string())
    }

    pub async fn list_todo_lists(&self) -> Result<Vec<TodoList>> {
        let mut next_url = Some("/me/todo/lists".to_string());
        let mut lists = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphTodoList> = self.get_json(&url).await?;
            lists.extend(page.value.into_iter().map(TodoList::from));
            next_url = page.next_link;
        }

        Ok(lists)
    }

    pub async fn create_todo_list(&self, display_name: &str) -> Result<TodoList> {
        let raw: GraphTodoList = self
            .post_json(
                "/me/todo/lists",
                &serde_json::json!({ "displayName": display_name }),
            )
            .await?;
        Ok(TodoList::from(raw))
    }

    pub async fn list_todo_tasks(
        &self,
        list_id: &str,
        include_completed: bool,
        limit: usize,
    ) -> Result<Vec<TodoTask>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let resolved_id = self.resolve_todo_list_id(list_id).await?;
        let top = limit.min(100);
        let mut next_url = Some(format!(
            "/me/todo/lists/{}/tasks?$top={top}",
            urlencoding::encode(&resolved_id)
        ));
        let mut tasks = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphTodoTask> = self.get_json(&url).await?;
            for raw in page.value {
                let task = raw.into_todo_task(resolved_id.clone());
                if !include_completed && task.is_completed() {
                    continue;
                }
                tasks.push(task);
                if tasks.len() >= limit {
                    return Ok(tasks);
                }
            }
            next_url = page.next_link;
        }

        Ok(tasks)
    }

    pub async fn create_todo_task(
        &self,
        list_id: &str,
        task: &OutgoingTodoTask,
    ) -> Result<TodoTask> {
        let resolved_id = self.resolve_todo_list_id(list_id).await?;
        let mut payload = serde_json::json!({
            "title": task.title,
        });
        if let Some(imp) = task
            .importance
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            payload["importance"] = serde_json::Value::String(imp.to_string());
        }
        if let Some(body_plain) = task
            .body_plain
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            payload["body"] = serde_json::json!({
                "contentType": "text",
                "content": body_plain,
            });
        }
        if let Some(due) = task
            .due_date_iso
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            payload["dueDateTime"] = serde_json::json!({
                "dateTime": due,
                "timeZone": "UTC",
            });
        }

        let path = format!("/me/todo/lists/{}/tasks", urlencoding::encode(&resolved_id));
        let created: GraphTodoTask = self.post_json(&path, &payload).await?;
        Ok(created.into_todo_task(resolved_id))
    }

    pub async fn set_todo_task_completed(
        &self,
        list_id: &str,
        task_id: &str,
        completed: bool,
    ) -> Result<TodoTask> {
        let resolved_id = self.resolve_todo_list_id(list_id).await?;
        let path = format!(
            "/me/todo/lists/{}/tasks/{}",
            urlencoding::encode(&resolved_id),
            urlencoding::encode(task_id.trim())
        );
        let status = if completed { "completed" } else { "notStarted" };
        let updated_opt: Option<GraphTodoTask> = self
            .patch_json_optional(&path, &serde_json::json!({ "status": status }))
            .await?;
        let raw = match updated_opt {
            Some(t) => t,
            None => self.get_json(&path).await?,
        };
        Ok(raw.into_todo_task(resolved_id))
    }

    pub async fn delete_todo_task(&self, list_id: &str, task_id: &str) -> Result<()> {
        let resolved_id = self.resolve_todo_list_id(list_id).await?;
        let path = format!(
            "/me/todo/lists/{}/tasks/{}",
            urlencoding::encode(&resolved_id),
            urlencoding::encode(task_id.trim())
        );
        self.delete_req(&path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::mail::tests::test_graph_client;
    use axum::{
        extract::{Path as AxumPath, State},
        http::StatusCode,
        routing::{get, patch},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_todo_lists_and_tasks_crud() {
        let deleted = Arc::new(Mutex::new(Vec::<String>::new()));
        let deleted_clone = deleted.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/todo/lists",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "list-other",
                                "displayName": "장보기",
                                "wellknownListName": "none",
                                "isOwner": true,
                                "isShared": false
                            },
                            {
                                "id": "list-default",
                                "displayName": "Tasks",
                                "wellknownListName": "defaultList",
                                "isOwner": true,
                                "isShared": false
                            }
                        ]
                    }))
                })
                .post(|Json(body): Json<serde_json::Value>| async move {
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({
                            "id": "list-new",
                            "displayName": body["displayName"],
                            "isOwner": true,
                            "isShared": false
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/todo/lists/{list_id}/tasks",
                get(|AxumPath(list_id): AxumPath<String>| async move {
                    assert_eq!(list_id, "list-default");
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "t1",
                                "title": "PR 리뷰",
                                "status": "notStarted",
                                "importance": "high",
                                "body": { "contentType": "text", "content": "코멘트 확인" },
                                "dueDateTime": { "dateTime": "2026-11-20T00:00:00", "timeZone": "UTC" }
                            },
                            {
                                "id": "t2",
                                "title": "완료된 작업",
                                "status": "completed",
                                "importance": "normal"
                            }
                        ]
                    }))
                })
                .post(
                    |AxumPath(list_id): AxumPath<String>, Json(body): Json<serde_json::Value>| async move {
                        assert_eq!(list_id, "list-default");
                        assert_eq!(body["dueDateTime"]["timeZone"], "UTC");
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({
                                "id": "t-new",
                                "title": body["title"],
                                "status": "notStarted",
                                "importance": body["importance"],
                                "body": body["body"],
                                "dueDateTime": body["dueDateTime"]
                            })),
                        )
                    },
                ),
            )
            .route(
                "/v1.0/me/todo/lists/{list_id}/tasks/{task_id}",
                patch(
                    |AxumPath((list_id, task_id)): AxumPath<(String, String)>,
                     Json(body): Json<serde_json::Value>| async move {
                        assert_eq!(list_id, "list-default");
                        Json(serde_json::json!({
                            "id": task_id,
                            "title": "PR 리뷰",
                            "status": body["status"],
                            "importance": "high"
                        }))
                    },
                )
                .delete(
                    |State(st): State<Arc<Mutex<Vec<String>>>>,
                     AxumPath((list_id, task_id)): AxumPath<(String, String)>| async move {
                        st.lock().unwrap().push(format!("{list_id}/{task_id}"));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(deleted);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));

        let lists = client.list_todo_lists().await.unwrap();
        assert_eq!(lists.len(), 2);

        let new_list = client.create_todo_list("업무").await.unwrap();
        assert_eq!(new_list.id, "list-new");
        assert_eq!(new_list.display_name, "업무");

        let open_tasks = client.list_todo_tasks("default", false, 10).await.unwrap();
        assert_eq!(open_tasks.len(), 1);
        assert_eq!(open_tasks[0].id, "t1");
        assert_eq!(open_tasks[0].list_id, "list-default");

        let all_tasks = client.list_todo_tasks("tasks", true, 10).await.unwrap();
        assert_eq!(all_tasks.len(), 2);

        let created = client
            .create_todo_task(
                "default",
                &OutgoingTodoTask {
                    title: "배포 준비".into(),
                    body_plain: Some("체크리스트".into()),
                    due_date_iso: Some("2026-11-21T09:00:00".into()),
                    importance: Some("high".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.id, "t-new");
        assert_eq!(created.body_plain.as_deref(), Some("체크리스트"));

        let completed = client
            .set_todo_task_completed("default", "t1", true)
            .await
            .unwrap();
        assert!(completed.is_completed());

        let reopened = client
            .set_todo_task_completed("list-default", "t1", false)
            .await
            .unwrap();
        assert!(!reopened.is_completed());

        client.delete_todo_task("default", "t1").await.unwrap();
        assert_eq!(*deleted_clone.lock().unwrap(), vec!["list-default/t1"]);
    }
}
