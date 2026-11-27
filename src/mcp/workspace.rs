//! Account, To Do, and Contacts MCP tool handlers.

use serde_json::{json, Value};

use crate::{
    auth::diagnose_access_token_scopes,
    cli,
    config::AppConfig,
    graph::GraphClient,
    models::OutgoingTodoTask,
    storage::Storage,
};

use super::{
    args::{bounds, Args},
    format_error, map_graph_result, resolve_todo_list_mcp, with_storage,
};

pub(super) async fn handle_whoami(
    config: &AppConfig,
    graph: &GraphClient,
) -> Result<Value, String> {
    let tokens = graph.auth().current_tokens().await;
    let auth_source = cli::auth_source_label(tokens.as_ref(), config);
    let Some(tok) = tokens else {
        return Ok(json!({
            "authenticated": false,
            "auth_source": auth_source,
        }));
    };
    let diag = diagnose_access_token_scopes(&tok.access_token);
    Ok(json!({
        "authenticated": true,
        "user_id": tok.user_id,
        "display_name": tok.user_display_name,
        "email": tok.user_email,
        "client_id": tok.client_id,
        "tenant_id": tok.tenant_id,
        "auth_mode": tok.auth_mode,
        "auth_source": auth_source,
        "expires_at": tok.expires_at,
        "expired": tok.is_expired_soon(0),
        "scopes": diag.scopes,
        "roles": diag.roles,
    }))
}

pub(super) async fn handle_list_todo_lists(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
    let offline = args.optional_bool("offline")?.unwrap_or(false);
    let refresh = args.optional_bool("refresh")?.unwrap_or(false);
    let limit = args
        .optional_usize("limit")?
        .map(|v| bounds("limit", v, 1, 200))
        .transpose()?
        .unwrap_or(50);
    let cursor = args.optional_str("cursor")?;
    if cursor.is_some() && refresh {
        return Err("cursor cannot be combined with refresh".to_string());
    }
    let scope = cli::cursor_scope(&["task-lists"]);
    let (lists, offset) = if let Some(raw_cursor) = cursor {
        let parsed = cli::PageCursor::parse(&raw_cursor).map_err(format_error)?;
        let cli::CursorPos::Offset(offset) = parsed.pos else {
            return Err("invalid cursor: not valid for this command".to_string());
        };
        parsed.require_scope(&scope).map_err(format_error)?;
        let cached = with_storage(storage, |s| s.list_todo_lists())
            .await
            .map_err(format_error)?;
        (cached, offset)
    } else {
        let ttl = cli::cli_cache_ttl_secs(config);
        let (cached, fresh) = with_storage(storage, move |s| {
            let lists = s.list_todo_lists()?;
            let is_fresh = cli::is_cursor_fresh(s, &["cli:todo_lists"], ttl);
            Ok((lists, is_fresh))
        })
        .await
        .map_err(format_error)?;
        let lists = if !offline && (refresh || cached.is_empty() || !fresh) {
            let fetched = map_graph_result(graph, graph.list_todo_lists().await).await?;
            let fetched_clone = fetched.clone();
            with_storage(storage, move |s| {
                s.replace_todo_lists(&fetched_clone)?;
                cli::record_cli_sync_cursor(s, "cli:todo_lists");
                s.list_todo_lists()
            })
            .await
            .map_err(format_error)?
        } else {
            cached
        };
        (lists, 0)
    };
    let (page, has_more, next_cursor) = cli::offset_page(&lists, offset, limit, &scope);
    Ok(json!({
        "lists": page,
        "returned": page.len(),
        "has_more": has_more,
        "next_cursor": next_cursor,
    }))
}

pub(super) async fn handle_list_tasks(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
    let list_query = args.optional_str("list")?;
    let include_completed = args.optional_bool("include_completed")?.unwrap_or(false);
    let limit = args
        .optional_usize("limit")?
        .map(|v| bounds("limit", v, 1, 200))
        .transpose()?
        .map(cli::clamp_limit)
        .unwrap_or(50);
    let offline = args.optional_bool("offline")?.unwrap_or(false);

    let resolved =
        resolve_todo_list_mcp(storage, graph, list_query.as_deref(), offline).await?;
    let list_id = resolved.id.clone();

    let (mut tasks, has_more) = if offline {
        let lid = list_id.clone();
        let rows = with_storage(storage, move |s| {
            s.list_todo_tasks(Some(&lid), include_completed)
        })
        .await
        .map_err(format_error)?;
        let has_more = rows.len() > limit;
        (rows, has_more)
    } else {
        let (fetched, reached_end) = map_graph_result(
            graph,
            graph
                .list_todo_tasks_complete(&list_id, include_completed, limit)
                .await,
        )
        .await?;
        let fetched_clone = fetched.clone();
        let lid_clone = list_id.clone();
        let _ = with_storage(storage, move |s| {
            if reached_end {
                s.replace_todo_tasks_for_list(&lid_clone, include_completed, &fetched_clone)
            } else {
                s.upsert_todo_tasks(&fetched_clone)
            }
        })
        .await;
        let has_more = !reached_end || fetched.len() > limit;
        (fetched, has_more)
    };
    tasks.truncate(limit);

    Ok(json!({
        "list": resolved,
        "list_id": list_id,
        "tasks": tasks,
        "returned": tasks.len(),
        "has_more": has_more,
    }))
}

pub(super) async fn handle_create_task(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
    let list_query = args.optional_str("list")?;
    let title = args.required_str("title")?;
    let body_plain = args.optional_str("notes")?.or(args.optional_str("body")?);
    let due_raw = args.optional_str("due")?.or(args.optional_str("due_date_iso")?);
    let due_date_iso = due_raw
        .map(|raw| cli::normalize_todo_due_iso(&raw))
        .transpose()
        .map_err(format_error)?;
    let importance = args
        .optional_str("importance")?
        .map(|raw| cli::normalize_todo_importance(&raw).map(str::to_string))
        .transpose()
        .map_err(format_error)?;

    let resolved =
        resolve_todo_list_mcp(storage, graph, list_query.as_deref(), false).await?;

    let outgoing = OutgoingTodoTask {
        title,
        body_plain,
        due_date_iso,
        importance,
    };
    let created =
        map_graph_result(graph, graph.create_todo_task(&resolved.id, &outgoing).await)
            .await?;
    let created_clone = created.clone();
    let _ = with_storage(storage, move |s| {
        s.upsert_todo_tasks(std::slice::from_ref(&created_clone))
    })
    .await;

    Ok(json!({
        "ok": true,
        "task": created,
    }))
}

pub(super) async fn handle_update_task(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
    let id = args.required_str("task_id")?;
    let list = args.optional_str("list")?;
    let title = args.optional_text("title")?;
    let due = args
        .optional_text("due")?
        .map(cli::normalize_todo_edit_due)
        .transpose()
        .map_err(format_error)?;
    let resolved = resolve_todo_list_mcp(storage, graph, list.as_deref(), false).await?;
    let updated = map_graph_result(
        graph,
        graph
            .edit_todo_task(&resolved.id, &id, title, due.as_deref())
            .await,
    )
    .await?;
    let cached = updated.clone();
    let _ = with_storage(storage, move |s| s.upsert_todo_tasks(&[cached])).await;
    Ok(json!({ "ok": true, "task": updated }))
}

pub(super) async fn handle_complete_task(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
    let list_query = args.optional_str("list")?;
    let task_id = args.required_str("task_id")?;
    let completed = args.optional_bool("completed")?.unwrap_or(true);

    let resolved =
        resolve_todo_list_mcp(storage, graph, list_query.as_deref(), false).await?;
    let updated = map_graph_result(
        graph,
        graph
            .set_todo_task_completed(&resolved.id, &task_id, completed)
            .await,
    )
    .await?;
    let updated_clone = updated.clone();
    let _ = with_storage(storage, move |s| {
        s.upsert_todo_tasks(std::slice::from_ref(&updated_clone))
    })
    .await;

    Ok(json!({
        "ok": true,
        "task": updated,
    }))
}

pub(super) async fn handle_search_contacts(
    graph: &GraphClient,
    storage: &Storage,
    args: Args<'_>,
) -> Result<Value, String> {
    let query = args.optional_str("query")?;
    let limit = args
        .optional_usize("limit")?
        .map(|v| bounds("limit", v, 1, 200))
        .transpose()?
        .map(cli::clamp_limit)
        .unwrap_or(25);
    let offline = args.optional_bool("offline")?.unwrap_or(false);

    let (contacts, directory) = if offline {
        let q_clone = query.clone();
        let rows = with_storage(storage, move |s| s.list_contacts(q_clone.as_deref(), limit))
            .await
            .map_err(format_error)?;
        (rows, cli::directory_status_json(false, None))
    } else if let Some(ref q) = query {
        let outcome = map_graph_result(
            graph,
            graph.search_contacts_and_directory_detailed(q, limit).await,
        )
        .await?;
        let fetched_clone = outcome.contacts.clone();
        let _ = with_storage(storage, move |s| s.upsert_contacts(&fetched_clone)).await;
        let dir = cli::directory_status_json(true, outcome.directory_error.as_ref());
        (outcome.contacts, dir)
    } else {
        let (fetched, reached_end) =
            map_graph_result(graph, graph.list_contacts_complete(limit).await).await?;
        let fetched_clone = fetched.clone();
        let _ = with_storage(storage, move |s| {
            if reached_end {
                s.replace_personal_contacts(&fetched_clone)
            } else {
                s.upsert_contacts(&fetched_clone)
            }
        })
        .await;
        (fetched, cli::directory_status_json(false, None))
    };

    Ok(json!({
        "contacts": contacts,
        "directory": directory,
    }))
}
