//! Local HTTP JSON API (`teams serve` / `teams api`) powered by `axum`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::cors::CorsLayer;

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    models::{
        ConversationKind, ConversationTarget, MentionInfo, OutgoingContact, OutgoingEvent,
        OutgoingMail, OutgoingTodoTask,
    },
    parser, resolver,
    storage::Storage,
    sync::{self, SyncCommand},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8788,
            token: None,
        }
    }
}

/// Shared state passed to every axum request handler.
pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub token: Option<String>,
}

/// JSON error returned by every failing handler.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub user_id: Option<String>,
    pub version: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConversationsQuery {
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateChatBody {
    pub members: Vec<String>,
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TeamsQuery {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct MessagesQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MentionInput {
    Spec(String),
    Object {
        #[serde(default)]
        id: Option<i32>,
        user_id: String,
        display_name: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct SendMessageBody {
    pub markdown: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub mentions: Vec<MentionInput>,
    #[serde(default)]
    pub important: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ThreadQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReplyBody {
    pub markdown: String,
    #[serde(default)]
    pub mentions: Vec<MentionInput>,
    #[serde(default)]
    pub important: bool,
}

#[derive(Debug, Deserialize)]
pub struct SendFileBody {
    pub path: String,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub thread_root_id: Option<String>,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadBody {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub out_dir: Option<String>,
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReactionBody {
    pub reaction: String,
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct EditMessageBody {
    pub markdown: String,
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteMessageParams {
    #[serde(default)]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PresenceQuery {
    #[serde(default)]
    pub user_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetPresenceBody {
    pub availability: String,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default)]
    pub expiration_mins: Option<u32>,
}

// --- Outlook Mail, Calendar, Contacts, To Do Query & Body DTOs ---

#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct MailMessagesQuery {
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct OfflineQuery {
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct SendMailBody {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub markdown: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub important: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReplyMailBody {
    pub markdown: String,
    #[serde(default)]
    pub reply_all: bool,
}

#[derive(Debug, Deserialize)]
pub struct ForwardMailBody {
    pub to: Vec<String>,
    #[serde(default)]
    pub markdown: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchMailBody {
    #[serde(default)]
    pub is_read: Option<bool>,
    #[serde(default)]
    pub flagged: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct MoveMailBody {
    pub destination_folder: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct MailAttachmentDownloadBody {
    #[serde(default)]
    pub out_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MailSearchQuery {
    pub q: String,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub remote: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CalendarEventsQuery {
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub days: Option<i64>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCalendarEventBody {
    pub subject: String,
    pub start_iso: String,
    pub end_iso: String,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub attendees: Vec<String>,
    #[serde(default)]
    pub is_online_meeting: bool,
    #[serde(default)]
    pub is_all_day: bool,
}

#[derive(Debug, Deserialize)]
pub struct RsvpEventBody {
    pub response: String,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContactsQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CreateContactBody {
    pub display_name: String,
    #[serde(default)]
    pub given_name: Option<String>,
    #[serde(default)]
    pub surname: Option<String>,
    #[serde(default)]
    pub email_addresses: Vec<String>,
    #[serde(default)]
    pub mobile_phone: Option<String>,
    #[serde(default)]
    pub company_name: Option<String>,
    #[serde(default)]
    pub job_title: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TodoListsQuery {
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct TodoTasksQuery {
    #[serde(default = "default_true")]
    pub include_completed: bool,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTodoTaskBody {
    pub title: String,
    #[serde(default)]
    pub body_plain: Option<String>,
    #[serde(default)]
    pub due_date_iso: Option<String>,
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchTodoTaskBody {
    pub completed: bool,
}

/// Build the `axum::Router` for the local HTTP JSON API.
pub fn build_router(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    token: Option<String>,
) -> axum::Router {
    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        token,
    });

    Router::new()
        .route("/health", get(health))
        .route("/conversations", get(list_conversations))
        .route("/chats", post(create_chat))
        .route("/teams", get(list_teams))
        .route(
            "/conversations/{key}/messages",
            get(get_messages).post(send_message),
        )
        .route("/conversations/{key}/threads/{root_id}", get(get_thread))
        .route(
            "/conversations/{key}/messages/{msg_id}/replies",
            post(reply_message),
        )
        .route("/conversations/{key}/files", post(send_file))
        .route(
            "/conversations/{key}/messages/{msg_id}/attachments/download",
            post(download_attachment),
        )
        .route(
            "/conversations/{key}/messages/{msg_id}/reactions",
            post(react_message),
        )
        .route(
            "/conversations/{key}/messages/{msg_id}",
            patch(edit_message).delete(delete_message),
        )
        .route("/conversations/{key}/read", post(mark_read))
        .route("/conversations/{key}/members", get(list_members))
        .route("/search", get(search))
        .route("/presence", get(get_presence).post(set_presence))
        // Outlook Mail
        .route("/mail/folders", get(list_mail_folders))
        .route("/mail/messages", get(list_mail_messages))
        .route(
            "/mail/messages/{id}",
            get(get_mail_message)
                .patch(patch_mail_message)
                .delete(delete_mail_message),
        )
        .route("/mail/send", post(send_mail))
        .route("/mail/messages/{id}/reply", post(reply_mail))
        .route("/mail/messages/{id}/forward", post(forward_mail))
        .route("/mail/messages/{id}/move", post(move_mail_message))
        .route(
            "/mail/messages/{id}/attachments/{att_id}/download",
            post(download_mail_attachment),
        )
        .route("/mail/search", get(search_mail))
        // Outlook Calendar
        .route(
            "/calendar/events",
            get(list_calendar_events).post(create_calendar_event),
        )
        .route("/calendar/events/{id}/rsvp", post(rsvp_calendar_event))
        .route("/calendar/events/{id}", delete(delete_calendar_event))
        // Outlook Contacts
        .route("/contacts", get(list_contacts).post(create_contact))
        .route("/contacts/{id}", delete(delete_contact))
        // Microsoft To Do
        .route("/todo/lists", get(list_todo_lists))
        .route(
            "/todo/lists/{list_id}/tasks",
            get(list_todo_tasks).post(create_todo_task),
        )
        .route(
            "/todo/lists/{list_id}/tasks/{task_id}",
            patch(patch_todo_task).delete(delete_todo_task),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the HTTP JSON API server and background sync engine until stopped.
pub async fn run(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let sync_ctl =
        sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), event_tx);
    sync_ctl.request(SyncCommand::RefreshConversations);
    sync_ctl.request(SyncCommand::RefreshOutlook);

    let app = build_router(config, graph, storage, opts.token);

    let addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("잘못된 바인드 주소: {}:{}", opts.bind, opts.port))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("리스너 바인드 실패: {addr}"))?;

    println!("teams HTTP API listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("HTTP 서버 오류")?;

    Ok(())
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(expected) = &state.token {
        if request.uri().path() != "/health" {
            let presented = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));

            if presented != Some(expected.as_str()) {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token",
                ));
            }
        }
    }

    Ok(next.run(request).await)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        user_id: state.graph.my_user_id().map(ToString::to_string),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ConversationsQuery>,
) -> Result<Json<Value>, ApiError> {
    if query.refresh {
        sync::bootstrap_conversations(&state.graph, &state.storage).await?;
    }

    let kind_filter = match query.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => Some(parse_kind_filter(raw)?),
    };

    let mut conversations = state.storage.list_conversations()?;
    if let Some(kind) = kind_filter {
        conversations.retain(|c| c.kind == kind);
    }

    Ok(Json(json!({ "conversations": conversations })))
}

async fn create_chat(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateChatBody>,
) -> Result<Json<Value>, ApiError> {
    let members: Vec<String> = body
        .members
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    if members.is_empty() {
        return Err(ApiError::bad_request("members must not be empty"));
    }

    let chat = state
        .graph
        .create_chat(&members, body.topic.as_deref())
        .await?;
    state
        .storage
        .upsert_conversations(std::slice::from_ref(&chat))?;

    Ok(Json(json!({ "ok": true, "conversation": chat })))
}

async fn list_teams(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TeamsQuery>,
) -> Result<Json<Value>, ApiError> {
    let teams = if query.refresh {
        let fetched = state.graph.list_joined_teams_with_channels().await?;
        state.storage.upsert_teams(&fetched)?;
        let stored = state.storage.list_teams()?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        state.storage.list_teams()?
    };

    Ok(Json(json!({ "teams": teams })))
}

async fn get_messages(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, query.offline)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let fetched = if !query.offline {
        let msgs = state.graph.list_messages(&conv.target, limit).await?;
        state.storage.upsert_messages(&msgs)?;
        Some(msgs)
    } else {
        None
    };

    let mut messages = state.storage.list_root_messages(&conv.conv_key, limit, None)?;
    if messages.is_empty() {
        if let Some(f) = fetched {
            messages = f;
        }
    }

    Ok(Json(json!({
        "conversation": conv,
        "messages": messages,
    })))
}

async fn send_message(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let mentions = parse_mention_inputs(&body.mentions)?;
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let mut outgoing = parser::compile_outgoing_markdown(
        &body.markdown,
        body.subject,
        mentions,
        vec![],
        vec![],
        None,
    );
    if body.important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = state.graph.send_message(&conv.target, &outgoing).await?;
    let _ = state.storage.upsert_messages(std::slice::from_ref(&sent));

    Ok(Json(json!({
        "ok": true,
        "conversation": conv,
        "message": sent,
    })))
}

async fn get_thread(
    State(state): State<Arc<AppState>>,
    Path((key, root_id)): Path<(String, String)>,
    Query(query): Query<ThreadQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, query.offline)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let ConversationTarget::Channel {
        ref team_id,
        ref channel_id,
    } = conv.target
    else {
        return Err(ApiError::bad_request(format!(
            "threads are only supported on team channels: {}",
            conv.conv_key
        )));
    };

    let thread = if query.offline {
        state
            .storage
            .get_thread(&conv.conv_key, &root_id, limit)?
            .ok_or_else(|| ApiError::not_found(format!("thread not found in cache: {root_id}")))?
    } else {
        let view = state
            .graph
            .get_thread(team_id, channel_id, &root_id, limit)
            .await?;
        let mut all = Vec::with_capacity(1 + view.replies.len());
        all.push(view.root.clone());
        all.extend(view.replies.iter().cloned());
        state.storage.upsert_messages(&all)?;
        view
    };

    Ok(Json(json!({ "thread": thread })))
}

async fn reply_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Json(body): Json<ReplyBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let mentions = parse_mention_inputs(&body.mentions)?;
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let quote = cli::build_quote_reply(&state.storage, &state.graph, &conv, &msg_id).await;
    let mut outgoing =
        parser::compile_outgoing_markdown(&body.markdown, None, mentions, vec![], vec![], quote);
    if body.important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = state
        .graph
        .reply_message(&conv.target, &msg_id, &outgoing)
        .await?;
    let _ = state.storage.upsert_messages(std::slice::from_ref(&sent));

    Ok(Json(json!({
        "ok": true,
        "conversation": conv,
        "reply_to": msg_id,
        "message": sent,
    })))
}

async fn send_file(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(body): Json<SendFileBody>,
) -> Result<Json<Value>, ApiError> {
    let trimmed_path = body.path.trim();
    if trimmed_path.is_empty() {
        return Err(ApiError::bad_request("path must not be empty"));
    }
    let path = std::path::Path::new(trimmed_path);
    if !path.exists() || !path.is_file() {
        return Err(ApiError::bad_request(format!(
            "file does not exist on host: {}",
            path.display()
        )));
    }
    if body.inline && cli::image_mime_for_path(path).is_none() {
        return Err(ApiError::bad_request(
            "inline=true requires an image file (png, jpg, jpeg, gif, webp, bmp)",
        ));
    }

    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let sent = cli::send_file_or_inline(
        &state.graph,
        &state.storage,
        &conv,
        path,
        body.caption.as_deref(),
        body.thread_root_id.as_deref(),
        body.inline,
    )
    .await?;

    Ok(Json(json!({
        "ok": true,
        "conversation": conv,
        "message": sent,
    })))
}

async fn download_attachment(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let parsed: DownloadBody = if body.iter().all(u8::is_ascii_whitespace) {
        DownloadBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };

    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let dest_dir = parsed
        .out_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.config.resolved_download_dir());

    let downloaded = cli::download_message_attachment(
        &state.graph,
        &state.storage,
        &conv,
        &msg_id,
        parsed.index.unwrap_or(0),
        parsed.thread_root_id.as_deref(),
        &dest_dir,
    )
    .await
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    Ok(Json(json!({
        "ok": true,
        "path": downloaded.path,
        "file_name": downloaded.file_name,
        "bytes_written": downloaded.bytes_written,
    })))
}

async fn react_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Json(body): Json<ReactionBody>,
) -> Result<Json<Value>, ApiError> {
    let normalized = cli::normalize_reaction(&body.reaction)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    state
        .graph
        .set_reaction(
            &conv.target,
            &msg_id,
            body.thread_root_id.as_deref(),
            &normalized,
            body.enable,
        )
        .await?;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
        "msg_id": msg_id,
        "reaction": normalized,
        "enable": body.enable,
    })))
}

async fn edit_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Json(body): Json<EditMessageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    let compiled =
        parser::compile_outgoing_markdown(&body.markdown, None, vec![], vec![], vec![], None);

    state
        .graph
        .edit_message(
            &conv.target,
            &msg_id,
            body.thread_root_id.as_deref(),
            &compiled.body_html,
        )
        .await?;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
        "msg_id": msg_id,
    })))
}

async fn delete_message(
    State(state): State<Arc<AppState>>,
    Path((key, msg_id)): Path<(String, String)>,
    Query(query): Query<DeleteMessageParams>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let body_params: DeleteMessageParams = if body.iter().all(u8::is_ascii_whitespace) {
        DeleteMessageParams::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };

    let thread_root_id = body_params
        .thread_root_id
        .or(query.thread_root_id)
        .filter(|s| !s.trim().is_empty());

    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    state
        .graph
        .delete_message(&conv.target, &msg_id, thread_root_id.as_deref())
        .await?;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
        "msg_id": msg_id,
    })))
}

async fn mark_read(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    if let ConversationTarget::Chat { ref chat_id } = conv.target {
        state.graph.mark_chat_read(chat_id).await?;
    }
    let now = chrono::Utc::now().to_rfc3339();
    state.storage.mark_conversation_read(&conv.conv_key, &now)?;

    Ok(Json(json!({
        "ok": true,
        "conv_key": conv.conv_key,
    })))
}

async fn list_members(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let conv = cli::resolve_target(&state.storage, &state.graph, &key, false)
        .await
        .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let members = state.graph.list_members(&conv.target).await?;
    let _ = state.storage.upsert_members(&conv.conv_key, &members);

    Ok(Json(json!({
        "conv_key": conv.conv_key,
        "title": conv.title,
        "members": members,
    })))
}

async fn search(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));

    let conv_key_filter = match query.target.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(target) => {
            let cached = state.storage.list_conversations()?;
            let resolved = resolver::resolve_from_slice(&cached, target)
                .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
            Some(resolved.conv_key)
        }
        None => None,
    };

    let hits = state
        .storage
        .search_messages(q, conv_key_filter.as_deref(), limit)?;

    Ok(Json(json!({
        "query": q,
        "hits": hits,
    })))
}

async fn get_presence(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PresenceQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(raw_ids) = query.user_ids.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let ids: Vec<String> = raw_ids
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        if ids.is_empty() {
            return Err(ApiError::bad_request("user_ids must contain at least one ID"));
        }
        let presences = state.graph.get_presences_by_user_ids(&ids).await?;
        Ok(Json(json!({ "presences": presences })))
    } else {
        let presence = state.graph.get_my_presence().await?;
        Ok(Json(json!({ "presence": presence })))
    }
}

async fn set_presence(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetPresenceBody>,
) -> Result<Json<Value>, ApiError> {
    let availability = cli::normalize_presence_availability(&body.availability)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let activity = body
        .activity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&availability);

    state
        .graph
        .set_my_presence(&availability, activity, body.expiration_mins)
        .await?;

    Ok(Json(json!({
        "ok": true,
        "availability": availability,
        "activity": activity,
        "expiration_mins": body.expiration_mins,
    })))
}

// ---------------------------------------------------------------------------
// Outlook Mail HTTP handlers
// ---------------------------------------------------------------------------

async fn list_mail_folders(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RefreshQuery>,
) -> Result<Json<Value>, ApiError> {
    let cached = state.storage.list_mail_folders()?;
    let folders = if !query.offline && (query.refresh || cached.is_empty()) {
        let fetched = state.graph.list_mail_folders().await?;
        state.storage.upsert_mail_folders(&fetched)?;
        let stored = state.storage.list_mail_folders()?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };
    Ok(Json(json!({ "folders": folders })))
}

async fn list_mail_messages(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MailMessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(30));
    let folder_arg = query
        .folder
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("inbox");
    let folder_id =
        cli::resolve_mail_folder_id(&state.storage, &state.graph, folder_arg, query.offline)
            .await?;

    let cached = state.storage.list_mail_messages(Some(&folder_id), limit)?;
    let mut messages = if !query.offline && (query.refresh || cached.is_empty()) {
        let fetched = state.graph.list_mail_messages(&folder_id, limit).await?;
        state.storage.upsert_mail_messages(&fetched)?;
        let stored = state.storage.list_mail_messages(Some(&folder_id), limit)?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };

    if query.unread {
        messages.retain(|m| !m.is_read);
    }
    messages.truncate(limit);

    Ok(Json(json!({
        "folder": folder_id,
        "messages": messages,
    })))
}

async fn get_mail_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let msg = if query.offline {
        state
            .storage
            .get_mail_message(&id)?
            .ok_or_else(|| ApiError::not_found(format!("mail message not found in cache: {id}")))?
    } else {
        let fetched = state.graph.get_mail_message(&id).await?;
        let _ = state
            .storage
            .upsert_mail_messages(std::slice::from_ref(&fetched));
        fetched
    };
    Ok(Json(json!({ "message": msg })))
}

async fn send_mail(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SendMailBody>,
) -> Result<Json<Value>, ApiError> {
    let to: Vec<String> = body
        .to
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if to.is_empty() {
        return Err(ApiError::bad_request("to must not be empty"));
    }
    if body.subject.trim().is_empty() {
        return Err(ApiError::bad_request("subject must not be empty"));
    }
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }

    let mut attachments = Vec::with_capacity(body.attachments.len());
    for raw in &body.attachments {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let p = PathBuf::from(trimmed);
        if !p.exists() || !p.is_file() {
            return Err(ApiError::bad_request(format!(
                "attachment file does not exist on host: {}",
                p.display()
            )));
        }
        attachments.push(p);
    }

    let body_html =
        parser::compile_outgoing_markdown(&body.markdown, None, vec![], vec![], vec![], None)
            .body_html;

    let mail = OutgoingMail {
        subject: body.subject.clone(),
        body_html,
        to: to.clone(),
        cc: body.cc,
        bcc: body.bcc,
        importance: if body.important {
            Some("high".to_string())
        } else {
            None
        },
        attachments,
        save_to_sent_items: true,
    };
    state.graph.send_mail(&mail).await?;

    Ok(Json(json!({
        "ok": true,
        "to": to,
        "subject": body.subject,
    })))
}

async fn reply_mail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ReplyMailBody>,
) -> Result<Json<Value>, ApiError> {
    if body.markdown.trim().is_empty() {
        return Err(ApiError::bad_request("markdown must not be empty"));
    }
    let html =
        parser::compile_outgoing_markdown(&body.markdown, None, vec![], vec![], vec![], None)
            .body_html;
    state
        .graph
        .reply_mail(&id, &html, body.reply_all)
        .await?;

    Ok(Json(json!({
        "ok": true,
        "message_id": id,
        "reply_all": body.reply_all,
    })))
}

async fn forward_mail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ForwardMailBody>,
) -> Result<Json<Value>, ApiError> {
    let to: Vec<String> = body
        .to
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if to.is_empty() {
        return Err(ApiError::bad_request("to must not be empty"));
    }
    let html = if body.markdown.trim().is_empty() {
        String::new()
    } else {
        parser::compile_outgoing_markdown(&body.markdown, None, vec![], vec![], vec![], None)
            .body_html
    };
    state.graph.forward_mail(&id, &to, &html).await?;

    Ok(Json(json!({
        "ok": true,
        "message_id": id,
        "to": to,
    })))
}

async fn patch_mail_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PatchMailBody>,
) -> Result<Json<Value>, ApiError> {
    if body.is_read.is_none() && body.flagged.is_none() {
        return Err(ApiError::bad_request(
            "at least one of is_read or flagged must be provided",
        ));
    }
    if let Some(is_read) = body.is_read {
        state.graph.set_mail_read(&id, is_read).await?;
    }
    if let Some(flagged) = body.flagged {
        state.graph.set_mail_flag(&id, flagged).await?;
    }
    if let Ok(Some(mut cached)) = state.storage.get_mail_message(&id) {
        if let Some(is_read) = body.is_read {
            cached.is_read = is_read;
        }
        if let Some(flagged) = body.flagged {
            cached.flagged = flagged;
        }
        let _ = state.storage.upsert_mail_messages(&[cached]);
    }

    Ok(Json(json!({
        "ok": true,
        "message_id": id,
        "is_read": body.is_read,
        "flagged": body.flagged,
    })))
}

async fn move_mail_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<MoveMailBody>,
) -> Result<Json<Value>, ApiError> {
    let dest = body.destination_folder.trim();
    if dest.is_empty() {
        return Err(ApiError::bad_request("destination_folder must not be empty"));
    }
    let folder_id = cli::resolve_mail_folder_id(&state.storage, &state.graph, dest, false).await?;
    let moved = state.graph.move_mail_message(&id, &folder_id).await?;
    let _ = state
        .storage
        .upsert_mail_messages(std::slice::from_ref(&moved));

    Ok(Json(json!({
        "ok": true,
        "message": moved,
    })))
}

async fn delete_mail_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.graph.delete_mail_message(&id).await?;
    let _ = state.storage.delete_mail_messages_by_ids(&[id.clone()]);
    Ok(Json(json!({
        "ok": true,
        "message_id": id,
    })))
}

async fn download_mail_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, att_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let parsed: MailAttachmentDownloadBody = if body.iter().all(u8::is_ascii_whitespace) {
        MailAttachmentDownloadBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))?
    };

    let dest_dir = parsed
        .out_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.config.resolved_download_dir());

    let downloaded = state
        .graph
        .download_mail_attachment(&id, &att_id, &dest_dir)
        .await?;

    Ok(Json(json!({
        "ok": true,
        "path": downloaded.path,
        "file_name": downloaded.file_name,
        "bytes_written": downloaded.bytes_written,
    })))
}

async fn search_mail(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MailSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let limit = cli::clamp_limit(query.limit.unwrap_or(30));
    let folder_filter = match query.folder.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) => {
            Some(cli::resolve_mail_folder_id(&state.storage, &state.graph, f, !query.remote).await?)
        }
        None => None,
    };

    let hits = state
        .storage
        .search_mail_messages(q, folder_filter.as_deref(), limit)?;
    let remote = if query.remote {
        let msgs = state.graph.search_mail_remote(q, limit).await?;
        let _ = state.storage.upsert_mail_messages(&msgs);
        msgs
    } else {
        Vec::new()
    };

    Ok(Json(json!({
        "query": q,
        "hits": hits,
        "remote": remote,
    })))
}

// ---------------------------------------------------------------------------
// Outlook Calendar HTTP handlers
// ---------------------------------------------------------------------------

async fn list_calendar_events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CalendarEventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let (start_iso, end_iso) = cli::default_calendar_window(
        query.start.as_deref(),
        query.end.as_deref(),
        query.days.unwrap_or(7),
    )
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    let events = if query.offline {
        state.storage.list_calendar_events(
            query.start.as_deref().map(|_| start_iso.as_str()),
            query.end.as_deref().map(|_| end_iso.as_str()),
            limit,
        )?
    } else {
        let fetched = state
            .graph
            .list_calendar_view(&start_iso, &end_iso, limit)
            .await?;
        let _ = state.storage.upsert_calendar_events(&fetched);
        fetched
    };

    Ok(Json(json!({
        "start": start_iso,
        "end": end_iso,
        "events": events,
    })))
}

async fn create_calendar_event(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateCalendarEventBody>,
) -> Result<Json<Value>, ApiError> {
    if body.subject.trim().is_empty() {
        return Err(ApiError::bad_request("subject must not be empty"));
    }
    let start_iso = cli::normalize_datetime_iso(&body.start_iso)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let end_iso = cli::normalize_datetime_iso(&body.end_iso)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let body_html = body
        .markdown
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|md| {
            parser::compile_outgoing_markdown(md, None, vec![], vec![], vec![], None).body_html
        });

    let outgoing = OutgoingEvent {
        subject: body.subject,
        start_iso,
        end_iso,
        time_zone: body.time_zone,
        is_all_day: body.is_all_day,
        location: body.location,
        body_html,
        attendees: body.attendees,
        is_online_meeting: body.is_online_meeting,
    };
    let created = state.graph.create_calendar_event(&outgoing).await?;
    let _ = state
        .storage
        .upsert_calendar_events(std::slice::from_ref(&created));

    Ok(Json(json!({
        "ok": true,
        "event": created,
    })))
}

async fn rsvp_calendar_event(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RsvpEventBody>,
) -> Result<Json<Value>, ApiError> {
    let normalized = cli::normalize_rsvp_response(&body.response)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    state
        .graph
        .respond_calendar_event(&id, normalized, body.comment.as_deref())
        .await?;

    Ok(Json(json!({
        "ok": true,
        "event_id": id,
        "response": normalized,
    })))
}

async fn delete_calendar_event(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.graph.delete_calendar_event(&id).await?;
    let _ = state.storage.delete_calendar_event(&id);
    Ok(Json(json!({
        "ok": true,
        "event_id": id,
    })))
}

// ---------------------------------------------------------------------------
// Outlook Contacts HTTP handlers
// ---------------------------------------------------------------------------

async fn list_contacts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ContactsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let q = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let contacts = if query.offline {
        state.storage.list_contacts(q, limit)?
    } else if let Some(q_str) = q {
        let fetched = state
            .graph
            .search_contacts_and_directory(q_str, limit)
            .await?;
        let _ = state.storage.upsert_contacts(&fetched);
        fetched
    } else {
        let fetched = state.graph.list_contacts(limit).await?;
        let _ = state.storage.upsert_contacts(&fetched);
        fetched
    };

    Ok(Json(json!({ "contacts": contacts })))
}

async fn create_contact(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateContactBody>,
) -> Result<Json<Value>, ApiError> {
    if body.display_name.trim().is_empty() {
        return Err(ApiError::bad_request("display_name must not be empty"));
    }
    let outgoing = OutgoingContact {
        display_name: body.display_name,
        given_name: body.given_name,
        surname: body.surname,
        email_addresses: body.email_addresses,
        mobile_phone: body.mobile_phone,
        company_name: body.company_name,
        job_title: body.job_title,
        department: body.department,
        notes: body.notes,
    };
    let created = state.graph.create_contact(&outgoing).await?;
    let _ = state
        .storage
        .upsert_contacts(std::slice::from_ref(&created));

    Ok(Json(json!({
        "ok": true,
        "contact": created,
    })))
}

async fn delete_contact(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.graph.delete_contact(&id).await?;
    let _ = state.storage.delete_contact(&id);
    Ok(Json(json!({
        "ok": true,
        "contact_id": id,
    })))
}

// ---------------------------------------------------------------------------
// Microsoft To Do HTTP handlers
// ---------------------------------------------------------------------------

async fn list_todo_lists(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TodoListsQuery>,
) -> Result<Json<Value>, ApiError> {
    let cached = state.storage.list_todo_lists()?;
    let lists = if !query.offline && (query.refresh || cached.is_empty()) {
        let fetched = state.graph.list_todo_lists().await?;
        state.storage.upsert_todo_lists(&fetched)?;
        let stored = state.storage.list_todo_lists()?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };
    Ok(Json(json!({ "lists": lists })))
}

async fn list_todo_tasks(
    State(state): State<Arc<AppState>>,
    Path(list_id): Path<String>,
    Query(query): Query<TodoTasksQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let resolved_list_id =
        cli::resolve_todo_list_id(&state.storage, &state.graph, &list_id, query.offline)
            .await
            .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let cached =
        state
            .storage
            .list_todo_tasks(&resolved_list_id, query.include_completed, limit)?;
    let mut tasks = if !query.offline && (query.refresh || cached.is_empty()) {
        let fetched = state
            .graph
            .list_todo_tasks(&resolved_list_id, query.include_completed, limit)
            .await?;
        state.storage.upsert_todo_tasks(&fetched)?;
        let stored =
            state
                .storage
                .list_todo_tasks(&resolved_list_id, query.include_completed, limit)?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };
    tasks.truncate(limit);

    Ok(Json(json!({
        "list_id": resolved_list_id,
        "tasks": tasks,
    })))
}

async fn create_todo_task(
    State(state): State<Arc<AppState>>,
    Path(list_id): Path<String>,
    Json(body): Json<CreateTodoTaskBody>,
) -> Result<Json<Value>, ApiError> {
    if body.title.trim().is_empty() {
        return Err(ApiError::bad_request("title must not be empty"));
    }
    let due_date_iso = body
        .due_date_iso
        .as_deref()
        .map(cli::normalize_todo_due_iso)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let importance = body
        .importance
        .as_deref()
        .map(cli::normalize_todo_importance)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?
        .map(str::to_string);

    let resolved_list_id =
        cli::resolve_todo_list_id(&state.storage, &state.graph, &list_id, false)
            .await
            .map_err(|e| ApiError::not_found(format!("{e:#}")))?;

    let outgoing = OutgoingTodoTask {
        title: body.title,
        body_plain: body.body_plain,
        due_date_iso,
        importance,
    };
    let created = state
        .graph
        .create_todo_task(&resolved_list_id, &outgoing)
        .await?;
    let _ = state
        .storage
        .upsert_todo_tasks(std::slice::from_ref(&created));

    Ok(Json(json!({
        "ok": true,
        "task": created,
    })))
}

async fn patch_todo_task(
    State(state): State<Arc<AppState>>,
    Path((list_id, task_id)): Path<(String, String)>,
    Json(body): Json<PatchTodoTaskBody>,
) -> Result<Json<Value>, ApiError> {
    let resolved_list_id =
        cli::resolve_todo_list_id(&state.storage, &state.graph, &list_id, false)
            .await
            .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    let updated = state
        .graph
        .set_todo_task_completed(&resolved_list_id, &task_id, body.completed)
        .await?;
    let _ = state
        .storage
        .upsert_todo_tasks(std::slice::from_ref(&updated));

    Ok(Json(json!({
        "ok": true,
        "task": updated,
    })))
}

async fn delete_todo_task(
    State(state): State<Arc<AppState>>,
    Path((list_id, task_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let resolved_list_id =
        cli::resolve_todo_list_id(&state.storage, &state.graph, &list_id, false)
            .await
            .map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    state
        .graph
        .delete_todo_task(&resolved_list_id, &task_id)
        .await?;
    let _ = state.storage.delete_todo_task(&task_id);

    Ok(Json(json!({
        "ok": true,
        "list_id": resolved_list_id,
        "task_id": task_id,
    })))
}

fn parse_kind_filter(raw: &str) -> Result<ConversationKind, ApiError> {
    match raw.to_ascii_lowercase().as_str() {
        "oneonone" | "one_on_one" | "1:1" => Ok(ConversationKind::OneOnOne),
        "group" => Ok(ConversationKind::Group),
        "meeting" => Ok(ConversationKind::Meeting),
        "channel" => Ok(ConversationKind::Channel),
        _ => Err(ApiError::bad_request(format!(
            "invalid kind '{raw}' (expected oneOnOne, group, meeting, or channel)"
        ))),
    }
}

fn parse_mention_inputs(inputs: &[MentionInput]) -> Result<Vec<MentionInfo>, ApiError> {
    let mut out = Vec::with_capacity(inputs.len());
    for (idx, item) in inputs.iter().enumerate() {
        match item {
            MentionInput::Spec(spec) => {
                let parsed = cli::parse_mention_spec(spec, idx as i32)
                    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
                out.push(parsed);
            }
            MentionInput::Object {
                id,
                user_id,
                display_name,
            } => {
                let uid = user_id.trim();
                let name = display_name.trim();
                if uid.is_empty() || name.is_empty() {
                    return Err(ApiError::bad_request(
                        "mention user_id and display_name must not be empty",
                    ));
                }
                out.push(MentionInfo {
                    id: id.unwrap_or(idx as i32),
                    user_id: uid.to_string(),
                    display_name: name.to_string(),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;

    async fn spawn_test_server(token: Option<String>) -> String {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-123");
        let storage = Storage::open_in_memory().unwrap();
        let router = build_router(cfg, graph, storage, token);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn health_is_public_and_protected_routes_require_bearer_token() {
        let base = spawn_test_server(Some("secret-token".to_string())).await;
        let client = reqwest::Client::new();

        // /health must succeed without Authorization header
        let res = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: HealthResponse = res.json().await.unwrap();
        assert!(body.ok);
        assert_eq!(body.user_id.as_deref(), Some("me-123"));
        assert_eq!(body.version, env!("CARGO_PKG_VERSION"));

        // Protected route without token -> 401 JSON error
        let res = client
            .get(format!("{base}/conversations"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let err: Value = res.json().await.unwrap();
        assert!(err["error"].as_str().is_some());

        // Protected route with wrong token -> 401
        let res = client
            .get(format!("{base}/conversations"))
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Protected route with valid token -> 200
        let res = client
            .get(format!("{base}/conversations"))
            .bearer_auth("secret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let data: Value = res.json().await.unwrap();
        assert!(data["conversations"].is_array());
    }

    #[tokio::test]
    async fn validation_rejects_empty_bodies_and_bad_queries() {
        let base = spawn_test_server(None).await;
        let client = reqwest::Client::new();

        let res = client
            .post(format!("{base}/chats"))
            .json(&json!({ "members": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .get(format!("{base}/search?q=%20%20"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .get(format!("{base}/conversations?kind=bogus"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/presence"))
            .json(&json!({ "availability": "InvalidState" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/conversations/chat%3A1/messages"))
            .json(&json!({ "markdown": "  " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parses_mention_inputs_both_formats() {
        let parsed = parse_mention_inputs(&[
            MentionInput::Spec("u1:Alice".to_string()),
            MentionInput::Object {
                id: Some(5),
                user_id: "u2".to_string(),
                display_name: "Bob".to_string(),
            },
        ])
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, 0);
        assert_eq!(parsed[0].user_id, "u1");
        assert_eq!(parsed[1].id, 5);
        assert_eq!(parsed[1].display_name, "Bob");
    }

    #[tokio::test]
    async fn offline_routes_resolve_url_encoded_conv_key_and_search() {
        use crate::models::MessageItem;

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let conv = crate::models::ConversationSummary {
            conv_key: "chat:19:room1@thread.v2".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "19:room1@thread.v2".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "홍길동".to_string(),
            topic: None,
            unread_count: 1,
            last_read_at: None,
            last_msg_id: Some("m1".to_string()),
            last_msg_author: Some("홍길동".to_string()),
            last_msg_preview: Some("배포 완료".to_string()),
            last_updated_at: Some("2026-11-15T06:00:00Z".to_string()),
            pinned: false,
            peer_user_id: Some("u-hong".to_string()),
            web_url: None,
        };
        storage.upsert_conversations(&[conv]).unwrap();
        storage
            .upsert_messages(&[MessageItem {
                msg_key: "chat:19:room1@thread.v2|m1".to_string(),
                conv_key: "chat:19:room1@thread.v2".to_string(),
                msg_id: "m1".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("u-hong".to_string()),
                author_name: "홍길동".to_string(),
                is_me: false,
                created_at: "2026-11-15T06:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>배포 완료했습니다</p>".to_string(),
                body_plain: "배포 완료했습니다".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            }])
            .unwrap();

        let router = build_router(cfg, graph, storage, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();

        // URL-encoded conv_key resolves cleanly in offline mode
        let res = client
            .get(format!(
                "{base}/conversations/chat%3A19%3Aroom1%40thread.v2/messages?offline=true"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["conversation"]["title"], "홍길동");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["body_plain"], "배포 완료했습니다");

        // Search finds cached message
        let res = client
            .get(format!("{base}/search?q=%EB%B0%B0%ED%8F%AC&target=%ED%99%8D%EA%B8%B8%EB%8F%99"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let search_body: Value = res.json().await.unwrap();
        assert_eq!(search_body["hits"].as_array().unwrap().len(), 1);
        assert_eq!(search_body["hits"][0]["msg_id"], "m1");
    }

    #[tokio::test]
    async fn outlook_routes_require_bearer_and_validate_inputs() {
        let base = spawn_test_server(Some("tok-123".to_string())).await;
        let client = reqwest::Client::new();

        // Bearer token enforced on new Outlook endpoints
        for path in [
            "/mail/folders",
            "/mail/messages?offline=true",
            "/calendar/events?offline=true",
            "/contacts?offline=true",
            "/todo/lists?offline=true",
        ] {
            let res = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
        }

        // Offline GET endpoints succeed with valid token
        for (path, key) in [
            ("/mail/folders?offline=true", "folders"),
            ("/mail/messages?offline=true", "messages"),
            ("/calendar/events?offline=true", "events"),
            ("/contacts?offline=true", "contacts"),
            ("/todo/lists?offline=true", "lists"),
        ] {
            let res = client
                .get(format!("{base}{path}"))
                .bearer_auth("tok-123")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            let body: Value = res.json().await.unwrap();
            assert!(body[key].is_array(), "{path}: {body}");
        }

        // Validation errors -> 400
        let res = client
            .post(format!("{base}/mail/send"))
            .bearer_auth("tok-123")
            .json(&json!({ "to": [], "subject": "Hi", "markdown": "Body" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .patch(format!("{base}/mail/messages/m1"))
            .bearer_auth("tok-123")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .get(format!("{base}/mail/search?q=%20%20"))
            .bearer_auth("tok-123")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/calendar/events/ev1/rsvp"))
            .bearer_auth("tok-123")
            .json(&json!({ "response": "maybe" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/calendar/events"))
            .bearer_auth("tok-123")
            .json(&json!({ "subject": " ", "start_iso": "2026-11-16T09:00:00Z", "end_iso": "2026-11-16T10:00:00Z" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/contacts"))
            .bearer_auth("tok-123")
            .json(&json!({ "display_name": "  " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/todo/lists/default/tasks"))
            .bearer_auth("tok-123")
            .json(&json!({ "title": "  " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
