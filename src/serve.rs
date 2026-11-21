//! Local HTTP JSON API (`outlook serve` / `outlook api`) + OpenAPI 3.1 powered by `axum`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::mpsc};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    auth::diagnose_access_token_scopes,
    cli,
    config::{self, AppConfig},
    graph::GraphClient,
    models::{MailMessage, OutgoingEvent, OutgoingMail, OutgoingTodoTask},
    resolver,
    storage::Storage,
    sync,
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub api_key: Option<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: config::DEFAULT_SERVE_PORT,
            api_key: None,
        }
    }
}

/// Shared state passed to every axum request handler.
pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub api_key: Option<String>,
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
        let status = if cli::is_forbidden_error(&err) {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self::new(status, format!("{err:#}"))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub user_id: Option<String>,
    pub version: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RefreshOfflineQuery {
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

#[derive(Debug, Default, Deserialize)]
pub struct MailMessagesQuery {
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub flagged: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub refresh: bool,
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
    #[serde(default, alias = "body")]
    pub markdown: String,
    #[serde(default)]
    pub importance: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default = "default_true")]
    pub save_to_sent_items: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ReplyMailBody {
    #[serde(default, alias = "body")]
    pub markdown: String,
    #[serde(default)]
    pub reply_all: bool,
}

#[derive(Debug, Deserialize)]
pub struct ForwardMailBody {
    pub to: Vec<String>,
    #[serde(default, alias = "comment")]
    pub markdown: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchMailBody {
    #[serde(default)]
    pub is_read: Option<bool>,
    #[serde(default)]
    pub flagged: Option<bool>,
    #[serde(default, alias = "destination_folder")]
    pub destination_folder_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AttachmentDownloadBody {
    #[serde(default, alias = "dir")]
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
    #[serde(default, alias = "start")]
    pub from: Option<String>,
    #[serde(default, alias = "end")]
    pub to: Option<String>,
    #[serde(default)]
    pub days: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateCalendarEventBody {
    pub subject: String,
    #[serde(alias = "start")]
    pub start_iso: String,
    #[serde(alias = "end")]
    pub end_iso: String,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub is_all_day: bool,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default, alias = "body")]
    pub markdown: Option<String>,
    #[serde(default)]
    pub attendees: Vec<String>,
    #[serde(default, alias = "online")]
    pub is_online_meeting: bool,
}

#[derive(Debug, Deserialize)]
pub struct RsvpEventBody {
    pub response: String,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TodoTasksQuery {
    #[serde(default)]
    pub list: Option<String>,
    #[serde(default, alias = "include_completed")]
    pub all: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateTodoTaskBody {
    #[serde(default)]
    pub list: Option<String>,
    pub title: String,
    #[serde(default, alias = "notes")]
    pub body_plain: Option<String>,
    #[serde(default, alias = "due")]
    pub due_date_iso: Option<String>,
    #[serde(default)]
    pub importance: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchTodoTaskBody {
    #[serde(default)]
    pub list: Option<String>,
    pub completed: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteTodoTaskQuery {
    #[serde(default)]
    pub list: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContactsQuery {
    #[serde(default, alias = "query")]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
}

/// Build the `axum::Router` for the local Outlook HTTP JSON API.
pub fn build_router(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    api_key: Option<String>,
) -> axum::Router {
    let has_key = api_key.as_deref().is_some_and(|t| !t.trim().is_empty());
    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        api_key,
    });

    let cors = if has_key {
        CorsLayer::permissive()
    } else {
        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(|origin, _| {
                origin.to_str().ok().is_some_and(is_loopback_origin)
            }))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
                header::HeaderName::from_static("x-api-key"),
            ])
    };

    Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_spec))
        .route("/v1/whoami", get(whoami))
        .route("/v1/mail/folders", get(list_mail_folders))
        .route("/v1/mail/messages", get(list_mail_messages))
        .route(
            "/v1/mail/messages/{id}",
            get(get_mail_message)
                .patch(patch_mail_message)
                .delete(delete_mail_message),
        )
        .route("/v1/mail/send", post(send_mail))
        .route("/v1/mail/messages/{id}/reply", post(reply_mail))
        .route("/v1/mail/messages/{id}/forward", post(forward_mail))
        .route(
            "/v1/mail/messages/{id}/attachments/{att_id}/download",
            post(download_mail_attachment),
        )
        .route("/v1/mail/search", get(search_mail))
        .route(
            "/v1/calendar/events",
            get(list_calendar_events).post(create_calendar_event),
        )
        .route(
            "/v1/calendar/events/{id}",
            get(get_calendar_event).delete(delete_calendar_event),
        )
        .route("/v1/calendar/events/{id}/rsvp", post(rsvp_calendar_event))
        .route("/v1/todo/lists", get(list_todo_lists))
        .route(
            "/v1/todo/tasks",
            get(list_todo_tasks).post(create_todo_task),
        )
        .route(
            "/v1/todo/tasks/{id}",
            patch(patch_todo_task).delete(delete_todo_task),
        )
        .route("/v1/contacts", get(list_contacts))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .layer(cors)
        .with_state(state)
}

/// Start the HTTP JSON API server and background sync engine until stopped.
pub async fn run(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("Invalid bind address: {}:{}", opts.bind, opts.port))?;
    let allow_insecure = std::env::var("OUTLOOK_ALLOW_INSECURE_SERVE").ok();
    check_bind_security(addr, opts.api_key.as_deref(), allow_insecure.as_deref())?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let sync_ctl =
        sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), event_tx);
    sync_ctl.request(sync::SyncCommand::Bootstrap);

    let app = build_router(config, graph, storage, opts.api_key);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind listener: {addr}"))?;

    println!("outlook HTTP API listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;

    Ok(())
}

fn check_bind_security(
    addr: SocketAddr,
    api_key: Option<&str>,
    allow_insecure_env: Option<&str>,
) -> Result<()> {
    let has_key = api_key.is_some_and(|t| !t.trim().is_empty());
    let allow_insecure = allow_insecure_env.map(str::trim) == Some("1");
    if !addr.ip().is_loopback() && !has_key && !allow_insecure {
        anyhow::bail!(
            "Refusing to bind HTTP API on non-loopback address {addr} without `--api-key <KEY>`. \
             Pass `--api-key <KEY>` to require Bearer authentication before exposing Outlook and host file endpoints, \
             or set `OUTLOOK_ALLOW_INSECURE_SERVE=1` to override."
        );
    }
    Ok(())
}

fn is_loopback_origin(origin: &str) -> bool {
    let Ok(parsed) = url::Url::parse(origin) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    match parsed.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
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

async fn map_graph_error(graph: &GraphClient, err: anyhow::Error) -> ApiError {
    if cli::is_forbidden_error(&err) {
        let wrapped = cli::wrap_graph_permission_error(graph, err).await;
        return ApiError::new(StatusCode::FORBIDDEN, format!("{wrapped:#}"));
    }
    ApiError::from(err)
}

fn map_resolve_error(err: anyhow::Error) -> ApiError {
    let msg = format!("{err:#}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("empty") {
        ApiError::bad_request(msg)
    } else if lower.contains("not found") || lower.contains("ambiguous") {
        ApiError::not_found(msg)
    } else {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

async fn resolve_mail_folder_for_api(
    state: &AppState,
    folder_query: &str,
    offline: bool,
) -> Result<crate::models::MailFolder, ApiError> {
    if offline {
        let q = folder_query.to_string();
        return with_storage(&state.storage, move |s| {
            resolver::resolve_mail_folder_cached(s, &q)
        })
        .await
        .map_err(map_resolve_error);
    }
    match resolver::resolve_mail_folder(&state.graph, &state.storage, folder_query, false).await {
        Ok(f) => Ok(f),
        Err(err) => {
            if cli::is_forbidden_error(&err) {
                Err(map_graph_error(&state.graph, err).await)
            } else {
                Err(map_resolve_error(err))
            }
        }
    }
}

async fn resolve_todo_list_for_api(
    state: &AppState,
    list_query: &str,
    offline: bool,
) -> Result<crate::models::TodoList, ApiError> {
    if offline {
        let q = list_query.to_string();
        return with_storage(&state.storage, move |s| {
            resolver::resolve_todo_list_cached(s, &q)
        })
        .await
        .map_err(map_resolve_error);
    }
    match resolver::resolve_todo_list(&state.graph, &state.storage, list_query, false).await {
        Ok(l) => Ok(l),
        Err(err) => {
            if cli::is_forbidden_error(&err) {
                Err(map_graph_error(&state.graph, err).await)
            } else {
                Err(map_resolve_error(err))
            }
        }
    }
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(expected) = &state.api_key {
        if request.uri().path() != "/health" {
            let bearer = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));
            let x_api_key = request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok());

            if bearer != Some(expected.as_str()) && x_api_key != Some(expected.as_str()) {
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

async fn whoami(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let tokens = state.graph.auth().current_tokens().await;
    let auth_source = cli::auth_source_label(tokens.as_ref(), &state.config);
    let Some(tok) = tokens else {
        return Ok(Json(json!({
            "authenticated": false,
            "auth_source": auth_source,
        })));
    };
    let scope_diag = diagnose_access_token_scopes(&tok.access_token);
    Ok(Json(json!({
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
        "scopes": scope_diag.scopes,
        "roles": scope_diag.roles,
    })))
}

// ---------------------------------------------------------------------------
// Mail handlers
// ---------------------------------------------------------------------------

async fn list_mail_folders(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RefreshOfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let ttl = cli::cli_cache_ttl_secs(&state.config);
    let (cached, fresh) = with_storage(&state.storage, move |s| {
        let folders = s.list_mail_folders()?;
        let is_fresh = cli::is_cursor_fresh(s, &["cli:mail_folders"], ttl);
        Ok((folders, is_fresh))
    })
    .await?;
    let folders = if !query.offline && (query.refresh || cached.is_empty() || !fresh) {
        let fetched = match state.graph.list_mail_folders().await {
            Ok(f) => f,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        with_storage(&state.storage, move |s| {
            s.replace_mail_folders(&fetched_clone)?;
            cli::record_cli_sync_cursor(s, "cli:mail_folders");
            s.list_mail_folders()
        })
        .await?
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
    let has_filter = query.unread || query.flagged || query.focused;
    let folder_arg = query
        .folder
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("inbox");
    let mut resolved_folder =
        resolve_mail_folder_for_api(&state, folder_arg, query.offline).await?;
    let folder_id = resolved_folder.id.clone();

    let ttl = cli::cli_cache_ttl_secs(&state.config);
    let fid_for_read = folder_id.clone();
    let unread = query.unread;
    let flagged = query.flagged;
    let focused = query.focused;
    let (cached, has_any_cached, fresh) = with_storage(&state.storage, move |s| {
        let cli_key = format!("cli:mail:{fid_for_read}");
        let delta_key = format!("mail_delta:{fid_for_read}");
        let is_fresh = cli::is_cursor_fresh(s, &[cli_key.as_str(), delta_key.as_str()], ttl);
        let has_any = !s.list_mail_summaries(Some(&fid_for_read), 1)?.is_empty();
        let msgs = s.list_mail_messages(Some(&fid_for_read), unread, flagged, focused, limit)?;
        Ok((msgs, has_any, is_fresh))
    })
    .await?;

    let messages = if !query.offline && (query.refresh || !has_any_cached || !fresh) {
        if has_filter {
            let (matched, fetched) = match state
                .graph
                .list_mail_messages_filtered(
                    &resolved_folder.id,
                    unread,
                    flagged,
                    focused,
                    limit,
                    true,
                )
                .await
            {
                Ok(res) => res,
                Err(err) if resolver::is_graph_not_found_error(&err) => {
                    match resolver::refresh_mail_folder_after_not_found(
                        &state.graph,
                        &state.storage,
                        folder_arg,
                        &resolved_folder.id,
                    )
                    .await
                    {
                        Ok(Some(new_folder)) => {
                            resolved_folder = new_folder;
                            match state
                                .graph
                                .list_mail_messages_filtered(
                                    &resolved_folder.id,
                                    unread,
                                    flagged,
                                    focused,
                                    limit,
                                    true,
                                )
                                .await
                            {
                                Ok(res) => res,
                                Err(retry_err) => {
                                    return Err(map_graph_error(&state.graph, retry_err).await);
                                }
                            }
                        }
                        Ok(None) => return Err(map_graph_error(&state.graph, err).await),
                        Err(refresh_err) => {
                            return Err(map_graph_error(&state.graph, refresh_err).await);
                        }
                    }
                }
                Err(err) => return Err(map_graph_error(&state.graph, err).await),
            };
            with_storage(&state.storage, move |s| {
                s.upsert_mail_messages(&fetched)?;
                Ok(matched
                    .into_iter()
                    .map(|m| s.get_mail_message(&m.id).ok().flatten().unwrap_or(m))
                    .collect())
            })
            .await?
        } else {
            let outcome = match crate::sync::sync_mail_folder_delta(
                &resolved_folder.id,
                &state.graph,
                &state.storage,
                None,
                false,
            )
            .await
            {
                Ok(o) => o,
                Err(err) if resolver::is_graph_not_found_error(&err) => {
                    match resolver::refresh_mail_folder_after_not_found(
                        &state.graph,
                        &state.storage,
                        folder_arg,
                        &resolved_folder.id,
                    )
                    .await
                    {
                        Ok(Some(new_folder)) => {
                            resolved_folder = new_folder;
                            match crate::sync::sync_mail_folder_delta(
                                &resolved_folder.id,
                                &state.graph,
                                &state.storage,
                                None,
                                false,
                            )
                            .await
                            {
                                Ok(o) => o,
                                Err(retry_err) => {
                                    return Err(map_graph_error(&state.graph, retry_err).await);
                                }
                            }
                        }
                        Ok(None) => return Err(map_graph_error(&state.graph, err).await),
                        Err(refresh_err) => {
                            return Err(map_graph_error(&state.graph, refresh_err).await);
                        }
                    }
                }
                Err(err) => return Err(map_graph_error(&state.graph, err).await),
            };
            let fid_for_store = outcome.folder_id;
            with_storage(&state.storage, move |s| {
                cli::record_cli_sync_cursor(s, &format!("cli:mail:{fid_for_store}"));
                s.list_mail_messages(Some(&fid_for_store), false, false, false, limit)
            })
            .await?
        }
    } else {
        cached
    };

    Ok(Json(json!({
        "folder": resolved_folder,
        "messages": messages,
    })))
}

async fn get_mail_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let msg: MailMessage = if query.offline {
        let id_owned = id.clone();
        with_storage(&state.storage, move |s| s.get_mail_message(&id_owned))
            .await?
            .ok_or_else(|| ApiError::not_found(format!("mail message not found in cache: {id}")))?
    } else {
        let fetched = match state.graph.get_mail_message(&id).await {
            Ok(m) => m,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        let _ = with_storage(&state.storage, move |s| {
            s.upsert_mail_messages(std::slice::from_ref(&fetched_clone))?;
            Ok(())
        })
        .await;
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
        return Err(ApiError::bad_request("markdown/body must not be empty"));
    }

    let importance = body
        .importance
        .as_deref()
        .map(|v| {
            cli::normalize_mail_importance(v)
                .map(str::to_string)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))
        })
        .transpose()?;

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

    let body_html = crate::parser::compile_outgoing_mail_html(&body.markdown, &[], None);
    let mail = OutgoingMail {
        subject: body.subject.clone(),
        body_html,
        body_markdown: Some(body.markdown),
        to: to.clone(),
        cc: body.cc,
        bcc: body.bcc,
        importance,
        attachments,
        inline_images: Vec::new(),
        in_reply_to_id: None,
        reply_all: false,
        forward_of_id: None,
        draft_id: None,
        save_to_sent_items: body.save_to_sent_items,
    };
    if let Err(err) = state.graph.send_mail(&mail).await {
        return Err(map_graph_error(&state.graph, err).await);
    }

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
        return Err(ApiError::bad_request("markdown/body must not be empty"));
    }
    let html = crate::parser::compile_outgoing_mail_html(&body.markdown, &[], None);
    if let Err(err) = state.graph.reply_mail(&id, &html, body.reply_all).await {
        return Err(map_graph_error(&state.graph, err).await);
    }

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
        crate::parser::compile_outgoing_mail_html(&body.markdown, &[], None)
    };
    if let Err(err) = state.graph.forward_mail(&id, &to, &html).await {
        return Err(map_graph_error(&state.graph, err).await);
    }

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
    let dest_query = body
        .destination_folder_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if body.is_read.is_none() && body.flagged.is_none() && dest_query.is_none() {
        return Err(ApiError::bad_request(
            "at least one of is_read, flagged, or destination_folder_id must be provided",
        ));
    }

    if let Some(is_read) = body.is_read {
        if let Err(err) = state.graph.set_mail_read(&id, is_read).await {
            return Err(map_graph_error(&state.graph, err).await);
        }
    }
    if let Some(flagged) = body.flagged {
        if let Err(err) = state.graph.set_mail_flag(&id, flagged).await {
            return Err(map_graph_error(&state.graph, err).await);
        }
    }

    let id_for_cache = id.clone();
    let is_read_val = body.is_read;
    let flagged_val = body.flagged;
    let _ = with_storage(&state.storage, move |s| {
        if let Some(r) = is_read_val {
            s.set_mail_read_local(&id_for_cache, r)?;
        }
        if let Some(f) = flagged_val {
            s.set_mail_flagged_local(&id_for_cache, f)?;
        }
        Ok(())
    })
    .await;

    let moved_message = if let Some(dest) = dest_query {
        let folder = resolve_mail_folder_for_api(&state, dest, false).await?;
        let (moved, attachments_expanded) =
            match state.graph.move_mail_message_with_meta(&id, &folder.id).await {
                Ok(m) => m,
                Err(err) => return Err(map_graph_error(&state.graph, err).await),
            };
        let moved_clone = moved.clone();
        let old_id_clone = id.clone();
        let persisted = with_storage(&state.storage, move |s| {
            s.replace_moved_mail_local_ex(&old_id_clone, &moved_clone, attachments_expanded)
        })
        .await
        .unwrap_or(moved);
        Some(persisted)
    } else {
        None
    };

    Ok(Json(json!({
        "ok": true,
        "message_id": moved_message.as_ref().map(|m| m.id.as_str()).unwrap_or(&id),
        "is_read": body.is_read,
        "flagged": body.flagged,
        "moved": moved_message,
    })))
}

async fn delete_mail_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if let Err(err) = state.graph.delete_mail_message(&id).await {
        return Err(map_graph_error(&state.graph, err).await);
    }
    let id_clone = id.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.delete_mail_messages_by_ids(&[id_clone])?;
        Ok(())
    })
    .await;
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
    let parsed: AttachmentDownloadBody = if body.iter().all(u8::is_ascii_whitespace) {
        AttachmentDownloadBody::default()
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

    let downloaded = match state
        .graph
        .download_mail_attachment_by_id(&id, &att_id, &dest_dir)
        .await
    {
        Ok(d) => d,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };

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
    let q = query.q.trim().to_string();
    if q.is_empty() {
        return Err(ApiError::bad_request("q must not be empty"));
    }
    let limit = cli::clamp_limit(query.limit.unwrap_or(30));
    let folder_filter = match query.folder.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) => Some(resolve_mail_folder_for_api(&state, f, !query.remote).await?.id),
        None => None,
    };

    let q_clone = q.clone();
    let folder_clone = folder_filter.clone();
    let hits = with_storage(&state.storage, move |s| {
        s.search_mail(&q_clone, folder_clone.as_deref(), limit)
    })
    .await?;

    let remote = if query.remote {
        let msgs = match state
            .graph
            .search_mail_remote_in_folder(&q, folder_filter.as_deref(), limit)
            .await
        {
            Ok(m) => m,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let msgs_clone = msgs.clone();
        let _ = with_storage(&state.storage, move |s| {
            s.upsert_mail_messages(&msgs_clone)?;
            Ok(())
        })
        .await;
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
// Calendar handlers
// ---------------------------------------------------------------------------

async fn list_calendar_events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CalendarEventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let (start_iso, end_iso) = cli::default_calendar_window(
        query.from.as_deref(),
        query.to.as_deref(),
        query.days.unwrap_or(7),
    )
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    let events = if query.offline {
        let start_filter = start_iso.clone();
        let end_filter = end_iso.clone();
        with_storage(&state.storage, move |s| {
            s.list_calendar_events(Some(&start_filter), Some(&end_filter), limit)
        })
        .await?
    } else {
        let (fetched, reached_end) = match state
            .graph
            .list_calendar_view_complete(&start_iso, &end_iso, limit)
            .await
        {
            Ok(ev) => ev,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        let s_iso = start_iso.clone();
        let e_iso = end_iso.clone();
        let _ = with_storage(&state.storage, move |s| {
            if reached_end {
                s.replace_calendar_events_in_window(&s_iso, &e_iso, &fetched_clone)?;
                Ok(())
            } else {
                s.upsert_calendar_events(&fetched_clone)
            }
        })
        .await;
        fetched
    };

    Ok(Json(json!({
        "start": start_iso,
        "end": end_iso,
        "events": events,
    })))
}

async fn get_calendar_event(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<OfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let event = if query.offline {
        let id_owned = id.clone();
        let events = with_storage(&state.storage, |s| {
            s.list_calendar_events(None, None, 10_000)
        })
        .await?;
        resolver::resolve_calendar_event_from_slice(&events, &id_owned)
            .map_err(map_resolve_error)?
    } else {
        let fetched = match state.graph.get_calendar_event(&id).await {
            Ok(ev) => ev,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        let _ = with_storage(&state.storage, move |s| {
            s.upsert_calendar_events(std::slice::from_ref(&fetched_clone))
        })
        .await;
        fetched
    };
    Ok(Json(json!({ "event": event })))
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
        .map(|md| crate::parser::compile_outgoing_mail_html(md, &[], None));

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
    let created = match state.graph.create_calendar_event(&outgoing).await {
        Ok(ev) => ev,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let created_clone = created.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.upsert_calendar_events(std::slice::from_ref(&created_clone))
    })
    .await;

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
    if let Err(err) = state
        .graph
        .respond_calendar_event(&id, normalized, body.comment.as_deref())
        .await
    {
        return Err(map_graph_error(&state.graph, err).await);
    }
    let id_clone = id.clone();
    let status = cli::rsvp_action_to_graph_status(normalized);
    let _ = with_storage(&state.storage, move |s| {
        s.update_event_response_local(&id_clone, status)
    })
    .await;

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
    if let Err(err) = state.graph.delete_calendar_event(&id).await {
        return Err(map_graph_error(&state.graph, err).await);
    }
    let id_clone = id.clone();
    let _ = with_storage(&state.storage, move |s| s.delete_calendar_event(&id_clone)).await;
    Ok(Json(json!({
        "ok": true,
        "event_id": id,
    })))
}

// ---------------------------------------------------------------------------
// To Do handlers
// ---------------------------------------------------------------------------

async fn list_todo_lists(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RefreshOfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let ttl = cli::cli_cache_ttl_secs(&state.config);
    let (cached, fresh) = with_storage(&state.storage, move |s| {
        let lists = s.list_todo_lists()?;
        let is_fresh = cli::is_cursor_fresh(s, &["cli:todo_lists"], ttl);
        Ok((lists, is_fresh))
    })
    .await?;
    let lists = if !query.offline && (query.refresh || cached.is_empty() || !fresh) {
        let fetched = match state.graph.list_todo_lists().await {
            Ok(l) => l,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        with_storage(&state.storage, move |s| {
            s.replace_todo_lists(&fetched_clone)?;
            cli::record_cli_sync_cursor(s, "cli:todo_lists");
            s.list_todo_lists()
        })
        .await?
    } else {
        cached
    };
    Ok(Json(json!({ "lists": lists })))
}

async fn list_todo_tasks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TodoTasksQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = cli::clamp_limit(query.limit.unwrap_or(50));
    let list_query = query
        .list
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tasks");

    let resolved = resolve_todo_list_for_api(&state, list_query, query.offline).await?;
    let list_id = resolved.id.clone();

    let ttl = cli::cli_cache_ttl_secs(&state.config);
    let lid_for_read = list_id.clone();
    let include_completed = query.all;
    let (cached, fresh) = with_storage(&state.storage, move |s| {
        let cursor_key = format!("cli:todo_tasks:{lid_for_read}");
        let is_fresh = cli::is_cursor_fresh(s, &[cursor_key.as_str()], ttl);
        let tasks = s.list_todo_tasks(Some(&lid_for_read), include_completed)?;
        Ok((tasks, is_fresh))
    })
    .await?;

    let mut tasks = if !query.offline && (query.refresh || cached.is_empty() || !fresh) {
        let (fetched, reached_end) = match state
            .graph
            .list_todo_tasks_complete(&list_id, include_completed, limit)
            .await
        {
            Ok(t) => t,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        let lid_for_store = list_id.clone();
        with_storage(&state.storage, move |s| {
            if reached_end {
                s.replace_todo_tasks_for_list(&lid_for_store, include_completed, &fetched_clone)?;
            } else {
                s.upsert_todo_tasks(&fetched_clone)?;
            }
            cli::record_cli_sync_cursor(s, &format!("cli:todo_tasks:{lid_for_store}"));
            s.list_todo_tasks(Some(&lid_for_store), include_completed)
        })
        .await?
    } else {
        cached
    };
    tasks.truncate(limit);

    Ok(Json(json!({
        "list": resolved,
        "list_id": list_id,
        "tasks": tasks,
    })))
}

async fn create_todo_task(
    State(state): State<Arc<AppState>>,
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

    let list_query = body
        .list
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tasks");
    let resolved = resolve_todo_list_for_api(&state, list_query, false).await?;

    let outgoing = OutgoingTodoTask {
        title: body.title,
        body_plain: body.body_plain,
        due_date_iso,
        importance,
    };
    let created = match state.graph.create_todo_task(&resolved.id, &outgoing).await {
        Ok(t) => t,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let created_clone = created.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.upsert_todo_tasks(std::slice::from_ref(&created_clone))
    })
    .await;

    Ok(Json(json!({
        "ok": true,
        "task": created,
    })))
}

async fn patch_todo_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Json(body): Json<PatchTodoTaskBody>,
) -> Result<Json<Value>, ApiError> {
    let list_query = body
        .list
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tasks");
    let resolved = resolve_todo_list_for_api(&state, list_query, false).await?;
    let updated = match state
        .graph
        .set_todo_task_completed(&resolved.id, &task_id, body.completed)
        .await
    {
        Ok(t) => t,
        Err(err) => return Err(map_graph_error(&state.graph, err).await),
    };
    let updated_clone = updated.clone();
    let _ = with_storage(&state.storage, move |s| {
        s.upsert_todo_tasks(std::slice::from_ref(&updated_clone))
    })
    .await;

    Ok(Json(json!({
        "ok": true,
        "task": updated,
    })))
}

async fn delete_todo_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(query): Query<DeleteTodoTaskQuery>,
) -> Result<Json<Value>, ApiError> {
    let list_query = query
        .list
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tasks");
    let resolved = resolve_todo_list_for_api(&state, list_query, false).await?;
    if let Err(err) = state.graph.delete_todo_task(&resolved.id, &task_id).await {
        return Err(map_graph_error(&state.graph, err).await);
    }
    let tid_clone = task_id.clone();
    let _ = with_storage(&state.storage, move |s| s.delete_todo_task(&tid_clone)).await;

    Ok(Json(json!({
        "ok": true,
        "list_id": resolved.id,
        "task_id": task_id,
    })))
}

// ---------------------------------------------------------------------------
// Contacts handler
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
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let contacts = if query.offline {
        let q_clone = q.clone();
        with_storage(&state.storage, move |s| {
            s.list_contacts(q_clone.as_deref(), limit)
        })
        .await?
    } else if let Some(ref q_str) = q {
        let fetched = match state.graph.search_contacts_and_directory(q_str, limit).await {
            Ok(c) => c,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        let _ = with_storage(&state.storage, move |s| s.upsert_contacts(&fetched_clone)).await;
        fetched
    } else {
        let (fetched, reached_end) = match state.graph.list_contacts_complete(limit).await {
            Ok(c) => c,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        let fetched_clone = fetched.clone();
        let _ = with_storage(&state.storage, move |s| {
            if reached_end {
                s.replace_personal_contacts(&fetched_clone)
            } else {
                s.upsert_contacts(&fetched_clone)
            }
        })
        .await;
        fetched
    };

    Ok(Json(json!({ "contacts": contacts })))
}

// ---------------------------------------------------------------------------
// OpenAPI 3.1 Specification
// ---------------------------------------------------------------------------

/// OpenAPI 3.1.0 description of every route exposed by `build_router`.
pub fn openapi_document() -> Value {
    let ops: &[(&str, &str, &str)] = &[
        ("/health", "get", "Health check (public)"),
        ("/openapi.json", "get", "OpenAPI 3.1 specification"),
        ("/v1/whoami", "get", "Current user profile, auth source, and token status"),
        ("/v1/mail/folders", "get", "List Outlook mail folders"),
        ("/v1/mail/messages", "get", "List messages in a folder"),
        ("/v1/mail/messages/{id}", "get patch delete", "Read, update (read/flag/move), or delete a mail message"),
        ("/v1/mail/send", "post", "Send a new email"),
        ("/v1/mail/messages/{id}/reply", "post", "Reply to a mail message"),
        ("/v1/mail/messages/{id}/forward", "post", "Forward a mail message"),
        ("/v1/mail/messages/{id}/attachments/{att_id}/download", "post", "Download a mail attachment to disk"),
        ("/v1/mail/search", "get", "Search mail messages locally (SQLite FTS5) or remotely (Graph KQL)"),
        ("/v1/calendar/events", "get post", "List calendar events or create a new event"),
        ("/v1/calendar/events/{id}", "get delete", "Read or delete a calendar event"),
        ("/v1/calendar/events/{id}/rsvp", "post", "Respond (accept/tentative/decline) to a calendar event"),
        ("/v1/todo/lists", "get", "List Microsoft To Do task lists"),
        ("/v1/todo/tasks", "get post", "List or create To Do tasks"),
        ("/v1/todo/tasks/{id}", "patch delete", "Complete/reopen or delete a To Do task"),
        ("/v1/contacts", "get", "List or search Outlook contacts and directory"),
    ];

    let mut paths = serde_json::Map::new();
    for (path, methods, summary) in ops {
        let mut item = serde_json::Map::new();
        for m in methods.split_whitespace() {
            item.insert(
                m.to_string(),
                json!({
                    "summary": summary,
                    "responses": { "200": { "description": "JSON response" } },
                }),
            );
        }
        paths.insert((*path).to_string(), Value::Object(item));
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "outlook-cli local HTTP API (Mail · Calendar · To Do · Contacts)",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": { "type": "http", "scheme": "bearer" }
            }
        },
        "security": [ { "bearerAuth": [] } ],
        "paths": Value::Object(paths),
    })
}

async fn openapi_spec() -> Json<Value> {
    Json(openapi_document())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        models::{CalendarEvent, ContactItem, MailFolder, MailMessage, TodoList, TodoTask},
    };

    async fn spawn_test_server(api_key: Option<String>) -> String {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-123");
        let storage = Storage::open_in_memory().unwrap();
        let router = build_router(cfg, graph, storage, api_key);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn health_is_public_and_protected_routes_require_bearer_token() {
        let base = spawn_test_server(Some("secret-key".to_string())).await;
        let client = reqwest::Client::new();

        // /health must succeed without Authorization header
        let res = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: HealthResponse = res.json().await.unwrap();
        assert!(body.ok);
        assert_eq!(body.user_id.as_deref(), Some("me-123"));
        assert_eq!(body.version, env!("CARGO_PKG_VERSION"));

        // Protected route without token -> 401
        let res = client
            .get(format!("{base}/v1/mail/folders?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Protected route with wrong token -> 401
        let res = client
            .get(format!("{base}/v1/mail/folders?offline=true"))
            .bearer_auth("wrong-key")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Protected route with valid Bearer token -> 200
        let res = client
            .get(format!("{base}/v1/mail/folders?offline=true"))
            .bearer_auth("secret-key")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let data: Value = res.json().await.unwrap();
        assert!(data["folders"].is_array());
    }

    #[tokio::test]
    async fn offline_sqlite_routes_and_openapi_3_1_work() {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-123");
        let storage = Storage::open_in_memory().unwrap();

        storage
            .upsert_mail_folders(&[MailFolder {
                id: "folder-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 1,
            }])
            .unwrap();
        storage
            .upsert_mail_messages(&[MailMessage {
                id: "mail-1".to_string(),
                internet_message_id: None,
                conversation_id: Some("conv-1".to_string()),
                folder_id: Some("folder-inbox".to_string()),
                subject: "Quarterly Release Plan".to_string(),
                from: None,
                to_recipients: Vec::new(),
                cc_recipients: Vec::new(),
                bcc_recipients: Vec::new(),
                reply_to: Vec::new(),
                received_at: "2026-11-18T10:00:00Z".to_string(),
                sent_at: None,
                last_modified_at: None,
                is_read: false,
                is_draft: false,
                flagged: true,
                importance: "high".to_string(),
                inference_classification: Some("focused".to_string()),
                has_attachments: false,
                body_loaded: true,
                preview: "Release checklist ready".to_string(),
                body_html: "<p>Release checklist ready</p>".to_string(),
                body_plain: "Release checklist ready".to_string(),
                attachments: Vec::new(),
                meeting_request: None,
                web_link: None,
            }])
            .unwrap();
        storage
            .upsert_calendar_events(&[CalendarEvent {
                id: "ev-1".to_string(),
                subject: "Architecture Review".to_string(),
                start_iso: "2026-11-20T09:00:00Z".to_string(),
                end_iso: "2026-11-20T10:00:00Z".to_string(),
                time_zone: "UTC".to_string(),
                is_all_day: false,
                location: Some("Room 4B".to_string()),
                organizer_name: Some("Alice".to_string()),
                organizer_email: Some("alice@example.com".to_string()),
                attendees: Vec::new(),
                response_status: "accepted".to_string(),
                is_online_meeting: true,
                online_meeting_url: Some("https://teams.microsoft.com/l/meetup/1".to_string()),
                body_html: String::new(),
                body_plain: String::new(),
                is_cancelled: false,
                web_link: None,
            }])
            .unwrap();
        storage
            .upsert_todo_lists(&[TodoList {
                id: "list-default".to_string(),
                display_name: "Tasks".to_string(),
                wellknown_list_name: Some("defaultList".to_string()),
                is_owner: true,
                is_shared: false,
            }])
            .unwrap();
        storage
            .upsert_todo_tasks(&[TodoTask {
                id: "task-1".to_string(),
                list_id: "list-default".to_string(),
                title: "Verify release checksums".to_string(),
                status: "notStarted".to_string(),
                importance: "high".to_string(),
                body_plain: None,
                due_date_iso: Some("2026-11-21T00:00:00Z".to_string()),
                reminder_iso: None,
                completed_at: None,
                created_at: None,
                updated_at: None,
            }])
            .unwrap();
        storage
            .upsert_contacts(&[ContactItem {
                id: "c-1".to_string(),
                display_name: "Alice Smith".to_string(),
                given_name: Some("Alice".to_string()),
                surname: Some("Smith".to_string()),
                email_addresses: vec!["alice@example.com".to_string()],
                mobile_phone: None,
                business_phones: Vec::new(),
                company_name: Some("Contoso".to_string()),
                job_title: None,
                department: None,
                notes: None,
                source: "contact".to_string(),
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

        // Mail list offline via well-known alias
        let res = client
            .get(format!("{base}/v1/mail/messages?folder=inbox&offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["folder"]["id"], "folder-inbox");
        assert_eq!(body["messages"][0]["id"], "mail-1");

        // Mail get offline
        let res = client
            .get(format!("{base}/v1/mail/messages/mail-1?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let msg_body: Value = res.json().await.unwrap();
        assert_eq!(msg_body["message"]["subject"], "Quarterly Release Plan");

        // Mail search offline
        let res = client
            .get(format!("{base}/v1/mail/search?q=checklist"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let search_body: Value = res.json().await.unwrap();
        assert_eq!(search_body["hits"][0]["id"], "mail-1");

        // Calendar list & get offline
        let res = client
            .get(format!("{base}/v1/calendar/events?offline=true&from=2026-11-19T00:00:00Z&to=2026-11-21T00:00:00Z"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cal_body: Value = res.json().await.unwrap();
        assert_eq!(cal_body["events"][0]["id"], "ev-1");

        let res = client
            .get(format!("{base}/v1/calendar/events/ev-1?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ev_body: Value = res.json().await.unwrap();
        assert_eq!(ev_body["event"]["subject"], "Architecture Review");

        // To Do lists & tasks offline
        let res = client
            .get(format!("{base}/v1/todo/lists?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = client
            .get(format!("{base}/v1/todo/tasks?list=tasks&offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let tasks_body: Value = res.json().await.unwrap();
        assert_eq!(tasks_body["tasks"][0]["id"], "task-1");

        // Contacts offline
        let res = client
            .get(format!("{base}/v1/contacts?q=Alice&offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let contacts_body: Value = res.json().await.unwrap();
        assert_eq!(contacts_body["contacts"][0]["id"], "c-1");

        // OpenAPI 3.1 spec lists all Outlook v1 routes
        let res = client.get(format!("{base}/openapi.json")).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let spec: Value = res.json().await.unwrap();
        assert_eq!(spec["openapi"], "3.1.0");
        let paths = spec["paths"].as_object().unwrap();
        for expected in [
            "/health",
            "/openapi.json",
            "/v1/whoami",
            "/v1/mail/folders",
            "/v1/mail/messages",
            "/v1/mail/messages/{id}",
            "/v1/mail/send",
            "/v1/mail/messages/{id}/reply",
            "/v1/mail/messages/{id}/forward",
            "/v1/mail/messages/{id}/attachments/{att_id}/download",
            "/v1/mail/search",
            "/v1/calendar/events",
            "/v1/calendar/events/{id}",
            "/v1/calendar/events/{id}/rsvp",
            "/v1/todo/lists",
            "/v1/todo/tasks",
            "/v1/todo/tasks/{id}",
            "/v1/contacts",
        ] {
            assert!(paths.contains_key(expected), "OpenAPI missing {expected}");
        }
    }

    #[tokio::test]
    async fn validation_rejects_empty_bodies_and_bad_queries() {
        let base = spawn_test_server(None).await;
        let client = reqwest::Client::new();

        let res = client
            .post(format!("{base}/v1/mail/send"))
            .json(&json!({ "to": [], "subject": "Hi", "markdown": "Body" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .patch(format!("{base}/v1/mail/messages/m1"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .get(format!("{base}/v1/mail/search?q=%20%20"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/v1/calendar/events/ev1/rsvp"))
            .json(&json!({ "response": "maybe" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/v1/calendar/events"))
            .json(&json!({ "subject": " ", "start_iso": "2026-11-20T09:00:00Z", "end_iso": "2026-11-20T10:00:00Z" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = client
            .post(format!("{base}/v1/todo/tasks"))
            .json(&json!({ "title": "  " }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn non_loopback_bind_requires_api_key_or_env_override() {
        let loopback: SocketAddr = "127.0.0.1:3939".parse().unwrap();
        let any_v4: SocketAddr = "0.0.0.0:3939".parse().unwrap();
        let lan_v4: SocketAddr = "192.168.1.10:3939".parse().unwrap();

        assert!(check_bind_security(loopback, None, None).is_ok());
        assert!(check_bind_security(any_v4, None, None).is_err());
        assert!(check_bind_security(lan_v4, None, Some("0")).is_err());
        assert!(check_bind_security(any_v4, Some("   "), None).is_err());
        assert!(check_bind_security(any_v4, Some("secret"), None).is_ok());
        assert!(check_bind_security(any_v4, None, Some("1")).is_ok());
    }

    #[tokio::test]
    async fn cors_restricts_unauthenticated_remote_origins_and_allows_loopback() {
        let base_no_tok = spawn_test_server(None).await;
        let client = reqwest::Client::new();

        let res = client
            .get(format!("{base_no_tok}/v1/mail/folders?offline=true"))
            .header(header::ORIGIN, "https://evil.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());

        let res = client
            .get(format!("{base_no_tok}/v1/mail/folders?offline=true"))
            .header(header::ORIGIN, "http://localhost:3000")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:3000")
        );

        assert!(is_loopback_origin("http://[::1]:5173"));
        assert!(!is_loopback_origin("https://localhost.evil.com"));
    }
}
