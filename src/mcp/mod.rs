//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! # Framing
//!
//! MCP's stdio transport uses **newline-delimited JSON**: exactly one JSON-RPC 2.0
//! object per line. `stdout` carries protocol messages only; all diagnostics go to
//! `stderr`. Notifications (`notifications/*` or requests without `id`) emit no
//! response.

mod args;
mod calendar;
mod catalog;
mod mail;
mod workspace;

#[cfg(test)]
mod tests;

pub use args::validate_tool_args;
pub use catalog::tool_catalog;

use std::io::Write;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    resolver,
    storage::Storage,
};

use args::Args;

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "outlook";
const MAX_RPC_LINE_BYTES: usize = 8 * 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

enum BoundedLine {
    Eof,
    Line(String),
    TooLong,
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut buf = Vec::new();
    let mut oversized = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if !oversized {
                if buf.len() + pos > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(&available[..pos]);
                }
            }
            reader.consume(pos + 1);
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        } else {
            if !oversized {
                if buf.len() + available.len() > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(available);
                }
            }
            let len = available.len();
            reader.consume(len);
        }
    }
}

async fn with_storage<T, F>(storage: &Storage, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
{
    let storage = storage.clone();
    tokio::task::spawn_blocking(move || f(&storage))
        .await
        .map_err(|err| anyhow::anyhow!("storage task panicked: {err}"))?
}

async fn map_graph_result<T>(graph: &GraphClient, res: Result<T>) -> Result<T, String> {
    match res {
        Ok(val) => Ok(val),
        Err(err) => Err(format_error(
            cli::wrap_graph_permission_error(graph, err).await,
        )),
    }
}

async fn resolve_mail_folder_mcp(
    storage: &Storage,
    graph: &GraphClient,
    folder_query: &str,
    offline: bool,
) -> Result<crate::models::MailFolder, String> {
    if offline {
        let q = folder_query.to_string();
        return with_storage(storage, move |s| resolver::resolve_mail_folder_cached(s, &q))
            .await
            .map_err(format_error);
    }
    map_graph_result(
        graph,
        resolver::resolve_mail_folder(graph, storage, folder_query, false).await,
    )
    .await
}

async fn resolve_todo_list_mcp(
    storage: &Storage,
    graph: &GraphClient,
    list_query: Option<&str>,
    offline: bool,
) -> Result<crate::models::TodoList, String> {
    let q = list_query
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tasks")
        .to_string();
    if offline {
        return with_storage(storage, move |s| resolver::resolve_todo_list_cached(s, &q))
            .await
            .map_err(format_error);
    }
    map_graph_result(
        graph,
        resolver::resolve_todo_list(graph, storage, &q, false).await,
    )
    .await
}

pub async fn run(config: AppConfig, graph: GraphClient, storage: Storage) -> Result<()> {
    let is_empty = with_storage(&storage, |s| Ok(s.list_mail_folders()?.is_empty()))
        .await
        .unwrap_or(false);
    if is_empty {
        let bg_graph = graph.clone();
        let bg_storage = storage.clone();
        tokio::spawn(async move {
            match bg_graph.list_mail_folders().await {
                Ok(folders) => {
                    let _ = with_storage(&bg_storage, move |s| s.upsert_mail_folders(&folders)).await;
                }
                Err(err) => {
                    eprintln!(
                        "{SERVER_NAME} MCP warning: initial mail folder bootstrap failed: {err:#}"
                    );
                }
            }
        });
    }

    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, user_id={})",
        graph.my_user_id().unwrap_or("unknown")
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = std::io::stdout();

    loop {
        match read_line_bounded(&mut reader, MAX_RPC_LINE_BYTES).await? {
            BoundedLine::Eof => break,
            BoundedLine::TooLong => {
                let err = error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    format!(
                        "invalid request: line exceeds {} MiB limit",
                        MAX_RPC_LINE_BYTES / (1024 * 1024)
                    ),
                );
                stdout.write_all(encode_message(&err).as_bytes())?;
                stdout.flush()?;
            }
            BoundedLine::Line(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if let Some(response) = handle_rpc_line(&config, &graph, &storage, trimmed).await {
                    stdout.write_all(encode_message(&response).as_bytes())?;
                    stdout.flush()?;
                }
            }
        }
    }

    eprintln!("{SERVER_NAME} MCP server: stdin closed, shutting down");
    Ok(())
}

/// Single-line JSON-RPC handler exposed for deterministic unit & integration testing.
pub async fn handle_rpc_line(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    line: &str,
) -> Option<Value> {
    let message = match parse_message(line) {
        Ok(msg) => msg,
        Err(err_resp) => return Some(err_resp),
    };

    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let has_id = message.get("id").is_some();

    // Notifications (including `notifications/initialized`) never receive a response.
    if method.starts_with("notifications/") || !has_id {
        return None;
    }

    match method {
        "initialize" => Some(success_response(id, initialize_result(&message))),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let outcome = match validate_tool_args(name, &arguments) {
                Ok(()) => call_tool(config, graph, storage, name, &arguments).await,
                Err(error) => Err(error),
            };

            Some(success_response(id, tool_result(outcome)))
        }
        _ => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    }
}

async fn call_tool(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let args = Args::new(arguments);

    match name {
        "whoami" => workspace::handle_whoami(config, graph).await,
        "list_mail_folders" => mail::handle_list_mail_folders(config, graph, storage, args).await,
        "list_messages" => mail::handle_list_messages(graph, storage, args).await,
        "get_message" => mail::handle_get_message(graph, storage, args).await,
        "send_mail" => mail::handle_send_mail(graph, args).await,
        "reply_mail" => mail::handle_reply_mail(graph, args).await,
        "forward_mail" => mail::handle_forward_mail(graph, args).await,
        "update_draft" => mail::handle_update_draft(graph, storage, args).await,
        "update_event" => calendar::handle_update_event(graph, storage, args).await,
        "update_task" => workspace::handle_update_task(graph, storage, args).await,
        "update_message" => mail::handle_update_message(graph, storage, args).await,
        "delete_message" => mail::handle_delete_message(graph, storage, args).await,
        "download_attachment" => mail::handle_download_attachment(config, graph, storage, args).await,
        "search_mail" => mail::handle_search_mail(graph, storage, args).await,
        "list_events" => calendar::handle_list_events(graph, storage, args).await,
        "get_event" => calendar::handle_get_event(graph, storage, args).await,
        "create_event" => calendar::handle_create_event(graph, storage, args).await,
        "respond_event" => calendar::handle_respond_event(graph, storage, args).await,
        "delete_event" => calendar::handle_delete_event(graph, storage, args).await,
        "list_todo_lists" => workspace::handle_list_todo_lists(config, graph, storage, args).await,
        "list_tasks" => workspace::handle_list_tasks(graph, storage, args).await,
        "create_task" => workspace::handle_create_task(graph, storage, args).await,
        "complete_task" => workspace::handle_complete_task(graph, storage, args).await,
        "search_contacts" => workspace::handle_search_contacts(graph, storage, args).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC framing
// ---------------------------------------------------------------------------

fn parse_message(line: &str) -> Result<Value, Value> {
    let trimmed = line.trim();
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            return Err(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {error}"),
            ));
        }
    };

    let Some(object) = value.as_object() else {
        return Err(error_response(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected JSON object",
        ));
    };

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        let id = object.get("id").cloned().unwrap_or(Value::Null);
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: jsonrpc must be '2.0'",
        ));
    }

    if object
        .get("method")
        .and_then(Value::as_str)
        .is_none_or(|m| m.trim().is_empty())
    {
        let id = object.get("id").cloned().unwrap_or(Value::Null);
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: method must be a non-empty string",
        ));
    }

    Ok(value)
}

fn initialize_result(request: &Value) -> Value {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL_VERSION
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Microsoft Outlook MCP server (Mail, Calendar, To Do, Contacts). Pass `offline: true` on read/search tools to query the local SQLite cache without network calls. `delete_message` and `delete_event` require `confirm: true`. Reads are compact by default (omitted fields are explicitly listed in `omitted.fields` — never assume omitted fields are absent); pass `fields: [...]` on `get_message`/`get_event` for selective projection or `compact: false` for full fields. Follow next_cursor to continue a listing.",
    })
}

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })
        }
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    })
}

fn encode_message(value: &Value) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

// ---------------------------------------------------------------------------
// Tool catalog (JSON Schemas)
// ---------------------------------------------------------------------------

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}
