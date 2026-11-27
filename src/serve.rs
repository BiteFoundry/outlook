//! Local HTTP JSON API (`outlook serve` / `outlook api`) + OpenAPI 3.1 powered by `axum`.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
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
    /// Bearer key chosen by the operator. Without one, a loopback server generates a random token.
    pub api_key: Option<String>,
    /// Run a loopback server without any authentication instead of generating a token.
    pub allow_keyless: bool,
    /// Browser origins allowed to call the API (exact match). Empty: no browser origin is allowed.
    pub cors_origins: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: config::DEFAULT_SERVE_PORT,
            api_key: None,
            allow_keyless: false,
            cors_origins: Vec::new(),
        }
    }
}

/// Who may call a router, and from where.
#[derive(Debug, Clone, Default)]
pub struct RouterSecurity {
    /// Bearer key required on every route except `/health`. `None` (or blank) means keyless.
    pub api_key: Option<String>,
    /// `Host` names accepted in keyless mode besides the loopback names.
    pub allowed_hosts: Vec<String>,
    /// Browser origins allowed to call the API, compared exactly with the `Origin` header.
    pub cors_origins: Vec<String>,
}

/// Shared state passed to every axum request handler.
pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub api_key: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub cors_origins: Vec<String>,
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
        let status = match cli::error_code(&err) {
            "not_found" => StatusCode::NOT_FOUND,
            "auth_required" | "token_expired" => StatusCode::UNAUTHORIZED,
            "forbidden" => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
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

/// Build the `axum::Router` for the local Outlook HTTP JSON API with the given key (`None`: keyless)
/// and no extra hosts or browser origins. `run` uses [`build_router_with_security`].
pub fn build_router(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    api_key: Option<String>,
) -> axum::Router {
    build_router_with_security(
        config,
        graph,
        storage,
        RouterSecurity {
            api_key,
            ..RouterSecurity::default()
        },
    )
}

/// Build the router with explicit access rules (see [`RouterSecurity`]).
pub fn build_router_with_security(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    security: RouterSecurity,
) -> axum::Router {
    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        api_key: security.api_key.filter(|key| !key.trim().is_empty()),
        allowed_hosts: security.allowed_hosts,
        cors_origins: security.cors_origins,
    });

    // CORS answers only the origins that were opted in; with none, no response carries CORS headers.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(
            state
                .cors_origins
                .iter()
                .filter_map(|origin| HeaderValue::from_str(origin).ok()),
        ))
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
        ]);

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
        // Outermost, so a request from a foreign host or origin is refused before CORS or auth see it
        // (preflights included).
        .layer(middleware::from_fn_with_state(state.clone(), request_guard))
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
    let cors_origins = opts
        .cors_origins
        .iter()
        .map(|origin| normalize_cors_origin(origin))
        .collect::<Result<Vec<_>>>()?;
    let key = resolve_serve_key(opts.api_key, opts.allow_keyless, addr.ip().is_loopback());
    let allowed_hosts = extra_allowed_hosts(
        addr.ip(),
        std::env::var("OUTLOOK_SERVE_ALLOWED_HOSTS").ok().as_deref(),
    );

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let sync_ctl =
        sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), event_tx);
    sync_ctl.request(sync::SyncCommand::Bootstrap);

    // Diagnostics go to stderr; stdout keeps its single "listening" line.
    match &key {
        ServeKey::Generated(token) => eprintln!(
            "No --api-key given: generated a Bearer token for this run (not stored anywhere):\n\n    {token}\n\n\
             Send it as `Authorization: Bearer <token>` or `x-api-key: <token>`. Pass --api-key <KEY> for a \
             key of your own, or --allow-keyless to serve loopback clients without authentication."
        ),
        ServeKey::Keyless => eprintln!(
            "Warning: authentication is off. Any process that can reach {addr} can read and send mail \
             through this API."
        ),
        ServeKey::Provided(_) => {}
    }
    let app = build_router_with_security(
        config,
        graph,
        storage,
        RouterSecurity {
            api_key: key.into_key(),
            allowed_hosts,
            cors_origins,
        },
    );

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

/// The key a server runs with, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServeKey {
    /// `--api-key` from the operator.
    Provided(String),
    /// Random token minted for this run because a loopback server was started without a key.
    Generated(String),
    /// No authentication: `--allow-keyless`, or a non-loopback bind allowed by
    /// `OUTLOOK_ALLOW_INSECURE_SERVE=1`.
    Keyless,
}

impl ServeKey {
    fn into_key(self) -> Option<String> {
        match self {
            ServeKey::Provided(key) | ServeKey::Generated(key) => Some(key),
            ServeKey::Keyless => None,
        }
    }
}

/// Decides the key for a server whose bind address already passed [`check_bind_security`].
fn resolve_serve_key(api_key: Option<String>, allow_keyless: bool, loopback: bool) -> ServeKey {
    match api_key.filter(|key| !key.trim().is_empty()) {
        Some(key) => ServeKey::Provided(key),
        // A keyless non-loopback bind only gets here through the explicit insecure override.
        None if allow_keyless || !loopback => ServeKey::Keyless,
        None => ServeKey::Generated(generate_serve_token()),
    }
}

/// 32 random bytes from the OS generator, hex-encoded (64 characters).
fn generate_serve_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    crate::hex_encode(bytes)
}

/// Host names accepted in keyless mode besides loopback: a specific non-loopback bind address and
/// the comma-separated `OUTLOOK_SERVE_ALLOWED_HOSTS`.
fn extra_allowed_hosts(bind_ip: IpAddr, env_hosts: Option<&str>) -> Vec<String> {
    let mut hosts = Vec::new();
    if !bind_ip.is_loopback() && !bind_ip.is_unspecified() {
        hosts.push(bind_ip.to_string());
    }
    hosts.extend(
        env_hosts
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_string),
    );
    hosts
}

/// Canonical `scheme://host[:port]` form of a `--cors-origin` value, as browsers send it.
fn normalize_cors_origin(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let parsed = url::Url::parse(trimmed).with_context(|| {
        format!("Invalid --cors-origin '{trimmed}': expected e.g. http://localhost:5173")
    })?;
    let has_extra = !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some();
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() || has_extra {
        anyhow::bail!(
            "Invalid --cors-origin '{trimmed}': expected scheme://host[:port] with an http or https scheme"
        );
    }
    Ok(parsed.origin().ascii_serialization())
}

/// Host part of a `Host` header / URI authority, without port and lower-cased; `None` when the
/// value is not a plain `host[:port]`.
fn host_of_authority(authority: &str) -> Option<String> {
    let trimmed = authority.trim();
    if trimmed.is_empty() || trimmed.contains(['/', '@', '?', '#', ' ']) {
        return None;
    }
    let parsed = url::Url::parse(&format!("http://{trimmed}")).ok()?;
    Some(match parsed.host()? {
        url::Host::Domain(host) => host.trim_end_matches('.').to_ascii_lowercase(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    })
}

/// Whether a keyless server answers a request addressed to `host_header`: `localhost`, a loopback
/// address, or one of `extra`. Anything else may be a page that rebound its own name to 127.0.0.1.
fn is_allowed_host(host_header: &str, extra: &[String]) -> bool {
    let Some(host) = host_of_authority(host_header) else {
        return false;
    };
    host == "localhost"
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        || extra
            .iter()
            .any(|allowed| host_of_authority(allowed).is_some_and(|allowed| allowed == host))
}

/// Refuses requests that come through a foreign host name (keyless servers) or from a browser
/// origin that was not opted in with `--cors-origin` (every server).
async fn request_guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // With a key the Host check is skipped: a page that rebinds its name cannot supply the key, and
    // a reverse proxy may legitimately forward its own host name.
    if state.api_key.is_none() {
        let authority = request
            .uri()
            .authority()
            .map(|a| a.as_str().to_string())
            .or_else(|| {
                request
                    .headers()
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            });
        if !authority.is_some_and(|a| is_allowed_host(&a, &state.allowed_hosts)) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Host header not allowed; use http://127.0.0.1:<port> or pass --api-key",
            ));
        }
    }

    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let allowed = origin
            .to_str()
            .is_ok_and(|origin| state.cors_origins.iter().any(|allowed| allowed == origin));
        if !allowed {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Origin not allowed; start the server with --cors-origin <ORIGIN> to allow a browser app",
            ));
        }
    }

    Ok(next.run(request).await)
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
    if lower.contains("empty") || lower.contains("ambiguous") {
        ApiError::bad_request(msg)
    } else if lower.contains("not found") {
        ApiError::not_found(msg)
    } else {
        ApiError::from(err)
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
        let (fetched, complete) = match state.graph.list_mail_folders_complete().await {
            Ok(f) => f,
            Err(err) => return Err(map_graph_error(&state.graph, err).await),
        };
        with_storage(&state.storage, move |s| {
            s.store_mail_folder_listing(&fetched, complete)?;
            if complete {
                cli::record_cli_sync_cursor(s, "cli:mail_folders");
            }
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

    // `has_more` is known only for a live filtered scan; `sync_complete` turns false only when a first
    // folder sync stopped at the interactive page budget.
    let mut has_more: Option<bool> = None;
    let mut sync_complete = true;
    let messages = if !query.offline && (query.refresh || !has_any_cached || !fresh) {
        if has_filter {
            let page = match state
                .graph
                .list_mail_messages_filtered_paged(
                    &resolved_folder.id,
                    unread,
                    flagged,
                    focused,
                    limit,
                    true,
                    "",
                    None,
                    Some(cli::FILTER_SCAN_MAX_PAGES),
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
                                .list_mail_messages_filtered_paged(
                                    &resolved_folder.id,
                                    unread,
                                    flagged,
                                    focused,
                                    limit,
                                    true,
                                    "",
                                    None,
                                    Some(cli::FILTER_SCAN_MAX_PAGES),
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
            has_more = page.has_more;
            let (matched, fetched) = (page.matched, page.fetched);
            with_storage(&state.storage, move |s| {
                s.upsert_mail_messages(&fetched)?;
                Ok(matched
                    .into_iter()
                    .map(|m| s.get_mail_message(&m.id).ok().flatten().unwrap_or(m))
                    .collect())
            })
            .await?
        } else {
            let outcome = match crate::sync::sync_mail_folder_for_listing(
                &resolved_folder.id,
                &state.graph,
                &state.storage,
                limit,
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
                            match crate::sync::sync_mail_folder_for_listing(
                                &resolved_folder.id,
                                &state.graph,
                                &state.storage,
                                limit,
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
            sync_complete = outcome.complete;
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

    Ok(Json(cli::with_sync_complete_marker(
        json!({
            "folder": resolved_folder,
            "messages": messages,
            "has_more": has_more,
        }),
        sync_complete,
    )))
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

    if body.is_read.is_some() || body.flagged.is_some() {
        if let Err(err) = state
            .graph
            .update_mail_flags(&id, body.is_read, body.flagged)
            .await
        {
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

    let moved_result = if let Some(dest) = dest_query {
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
        Some((persisted, folder.id))
    } else {
        None
    };

    Ok(Json(json!({
        "ok": true,
        "message_id": moved_result.as_ref().map(|(m, _)| m.id.as_str()).unwrap_or(&id),
        "is_read": body.is_read,
        "flagged": body.flagged,
        "moved": moved_result.as_ref().map(|(m, folder_id)| json!({
            "id": m.id,
            "folder_id": m.folder_id.as_deref().unwrap_or(folder_id.as_str()),
        })),
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
    let (start_iso, end_iso, time_zone) = crate::graph::calendar::resolve_new_event_times(
        &body.start_iso,
        &body.end_iso,
        body.is_all_day,
        body.time_zone.as_deref(),
    )
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
        time_zone,
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
        spawn_test_server_with(RouterSecurity {
            api_key,
            ..RouterSecurity::default()
        })
        .await
    }

    async fn spawn_test_server_with(security: RouterSecurity) -> String {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("me-123");
        let storage = Storage::open_in_memory().unwrap();
        let router = build_router_with_security(cfg, graph, storage, security);

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
    async fn live_mail_listing_is_bounded_for_filtered_scans_and_first_syncs() {
        use axum::extract::Query as AxumQuery;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A folder that never runs out of pages: every list or delta page holds one unflagged message
        // and another `@odata.nextLink`.
        let list_hits = Arc::new(AtomicUsize::new(0));
        let delta_hits = Arc::new(AtomicUsize::new(0));
        let graph_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let graph_base = format!("http://{}/v1.0", graph_listener.local_addr().unwrap());

        let endless = |hits: Arc<AtomicUsize>, base: String, path: &'static str| {
            get(move |AxumQuery(params): AxumQuery<HashMap<String, String>>| {
                let hits = Arc::clone(&hits);
                let base = base.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let page: usize = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
                    Json(json!({
                        "value": [{
                            "id": format!("{path}-{page}"),
                            "parentFolderId": "folder-inbox",
                            "subject": format!("Mail {page}"),
                            "receivedDateTime": "2026-11-20T05:00:00Z",
                            "isRead": true,
                            "flag": { "flagStatus": "notFlagged" }
                        }],
                        "@odata.nextLink": format!("{base}/me/mailFolders/folder-inbox/{path}?page={}", page + 1)
                    }))
                }
            })
        };
        let graph_app = Router::new()
            .route(
                "/v1.0/me/mailFolders/folder-inbox/messages",
                endless(Arc::clone(&list_hits), graph_base.clone(), "messages"),
            )
            .route(
                "/v1.0/me/mailFolders/folder-inbox/messages/delta",
                endless(Arc::clone(&delta_hits), graph_base.clone(), "messages/delta"),
            );
        tokio::spawn(async move {
            axum::serve(graph_listener, graph_app).await.unwrap();
        });

        let cfg = AppConfig {
            graph_base_url: graph_base,
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            crate::auth::StoredTokens {
                access_token: "test-access-token".into(),
                refresh_token: None,
                expires_at: 9_999_999_999,
                client_id: "c".into(),
                tenant_id: "t".into(),
                user_id: Some("me-123".into()),
                user_display_name: None,
                user_email: None,
                auth_mode: "access_token".into(),
            },
        );
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
                unread_item_count: 0,
                total_item_count: 0,
            }])
            .unwrap();

        let router = build_router(cfg, graph, storage, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();

        // Filtered listing with no match anywhere: the scan stops at the page cap instead of walking
        // the whole folder, and reports that it does not know whether more matches exist.
        let res = client
            .get(format!("{base}/v1/mail/messages?folder=inbox&flagged=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert_eq!(list_hits.swap(0, Ordering::SeqCst), cli::FILTER_SCAN_MAX_PAGES);
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
        assert!(body.as_object().unwrap().contains_key("has_more"));
        assert!(body["has_more"].is_null());
        assert!(body.get("sync_complete").is_none());

        // Unfiltered listing of a folder with no delta link: the first sync stops at the interactive
        // budget, says so, and still returns the newest messages from a plain listing.
        let res = client
            .get(format!("{base}/v1/mail/messages?folder=inbox&limit=3&refresh=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = res.json().await.unwrap();
        assert_eq!(
            delta_hits.load(Ordering::SeqCst),
            crate::sync::INTERACTIVE_DELTA_MAX_PAGES
        );
        assert_eq!(body["sync_complete"], false);
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert!(list_hits.load(Ordering::SeqCst) >= 1);
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
                ..Default::default()
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
                ..Default::default()
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

        // All-day events need YYYY-MM-DD boundaries and an exclusive end; both are rejected before Graph is called.
        for (start, end, needle) in [
            ("2026-11-23T09:00:00Z", "2026-11-24", "YYYY-MM-DD"),
            ("2026-11-23", "2026-11-23", "2026-11-24"),
        ] {
            let res = client
                .post(format!("{base}/v1/calendar/events"))
                .json(&json!({ "subject": "PTO", "start": start, "end": end, "is_all_day": true }))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let body: Value = res.json().await.unwrap();
            assert!(body.to_string().contains(needle), "{body}");
        }

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
    async fn keyless_server_refuses_foreign_hosts_and_every_browser_origin() {
        let base = spawn_test_server(None).await;
        let port = base.rsplit(':').next().unwrap().to_string();
        let client = reqwest::Client::new();
        let url = format!("{base}/v1/mail/folders?offline=true");

        // A plain local client (no Origin, loopback Host) is served.
        let res = client.get(&url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = client
            .get(&url)
            .header(header::HOST, format!("localhost:{port}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // DNS rebinding: the page's own name resolves to 127.0.0.1, so the Host header is foreign.
        for host in [
            "evil.example.com".to_string(),
            format!("evil.example.com:{port}"),
            format!("127.0.0.1.evil.example.com:{port}"),
            format!("localhost.evil.example.com:{port}"),
        ] {
            for path in ["/v1/mail/folders?offline=true", "/health", "/openapi.json"] {
                let res = client
                    .get(format!("{base}{path}"))
                    .header(header::HOST, &host)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    res.status(),
                    StatusCode::FORBIDDEN,
                    "host {host} path {path}"
                );
            }
        }

        // No browser origin is allowed unless it was opted in - loopback pages included.
        for origin in [
            "https://evil.example.com",
            "http://localhost:3000",
            "http://127.0.0.1:5173",
            "null",
        ] {
            let res = client
                .get(&url)
                .header(header::ORIGIN, origin)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "origin {origin}");
            assert!(res
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none());

            // The preflight is refused as well, so a browser never sends the real request.
            let res = client
                .request(Method::OPTIONS, &url)
                .header(header::ORIGIN, origin)
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "preflight {origin}");
            assert!(res
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none());
        }
    }

    #[tokio::test]
    async fn cors_answers_only_origins_that_were_opted_in() {
        let base = spawn_test_server_with(RouterSecurity {
            api_key: Some("secret-key".to_string()),
            allowed_hosts: Vec::new(),
            cors_origins: vec!["http://localhost:5173".to_string()],
        })
        .await;
        let client = reqwest::Client::new();
        let url = format!("{base}/v1/mail/folders?offline=true");

        let res = client
            .get(&url)
            .bearer_auth("secret-key")
            .header(header::ORIGIN, "http://localhost:5173")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:5173")
        );

        // The preflight carries no credentials and still succeeds for the opted-in origin.
        let res = client
            .request(Method::OPTIONS, &url)
            .header(header::ORIGIN, "http://localhost:5173")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type",
            )
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success(), "preflight: {}", res.status());
        assert_eq!(
            res.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:5173")
        );

        // A key does not make the API permissive: other origins stay refused, valid key or not.
        for origin in ["https://evil.example.com", "http://localhost:3000"] {
            let res = client
                .get(&url)
                .bearer_auth("secret-key")
                .header(header::ORIGIN, origin)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "origin {origin}");
            assert!(res
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none());
        }

        // With a key the Host check is off (reverse proxies forward their own name); auth still applies.
        let res = client
            .get(&url)
            .bearer_auth("secret-key")
            .header(header::HOST, "outlook.internal.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = client
            .get(&url)
            .header(header::HOST, "outlook.internal.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn keyless_server_accepts_extra_allowed_hosts() {
        let base = spawn_test_server_with(RouterSecurity {
            api_key: None,
            allowed_hosts: vec!["outlook.lan".to_string()],
            cors_origins: Vec::new(),
        })
        .await;
        let client = reqwest::Client::new();
        let url = format!("{base}/v1/mail/folders?offline=true");

        let res = client
            .get(&url)
            .header(header::HOST, "Outlook.LAN:3939")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = client
            .get(&url)
            .header(header::HOST, "other.lan:3939")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn allowed_host_matching_is_exact_on_the_host_part() {
        for host in [
            "localhost",
            "LOCALHOST:3939",
            "localhost.:3939",
            "127.0.0.1",
            "127.0.0.1:3939",
            "127.8.9.10:1",
            "[::1]",
            "[::1]:3939",
        ] {
            assert!(is_allowed_host(host, &[]), "{host} should be allowed");
        }
        for host in [
            "",
            "evil.example.com",
            "localhost.evil.example.com",
            "127.0.0.1.evil.example.com:3939",
            "evil-localhost",
            "192.168.1.10:3939",
            "0.0.0.0:3939",
            "[::]:3939",
            "localhost@evil.example.com",
            "evil.example.com/localhost",
            "localhost evil.example.com",
        ] {
            assert!(!is_allowed_host(host, &[]), "{host} should be refused");
        }

        let extra = vec!["192.168.1.10".to_string(), "Outlook.LAN".to_string()];
        assert!(is_allowed_host("192.168.1.10:3939", &extra));
        assert!(is_allowed_host("outlook.lan", &extra));
        assert!(!is_allowed_host("192.168.1.11:3939", &extra));
        assert!(!is_allowed_host("sub.outlook.lan", &extra));
    }

    #[test]
    fn serve_key_is_generated_only_for_a_loopback_server_without_one() {
        assert_eq!(
            resolve_serve_key(Some("mine".to_string()), false, true),
            ServeKey::Provided("mine".to_string())
        );
        assert_eq!(resolve_serve_key(None, true, true), ServeKey::Keyless);
        // Non-loopback and keyless is only reachable through OUTLOOK_ALLOW_INSECURE_SERVE=1.
        assert_eq!(resolve_serve_key(None, false, false), ServeKey::Keyless);

        let ServeKey::Generated(first) = resolve_serve_key(None, false, true) else {
            panic!("a loopback server without a key must generate one");
        };
        // A blank key counts as no key.
        let ServeKey::Generated(second) = resolve_serve_key(Some("  ".to_string()), false, true)
        else {
            panic!("a blank key must not disable authentication");
        };
        for token in [&first, &second] {
            assert_eq!(token.len(), 64);
            assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert_ne!(first, second);
        assert_eq!(ServeKey::Generated(first.clone()).into_key(), Some(first));
        assert_eq!(ServeKey::Keyless.into_key(), None);
    }

    #[test]
    fn extra_allowed_hosts_come_from_a_specific_bind_address_and_the_env_list() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let any: IpAddr = "0.0.0.0".parse().unwrap();
        let lan: IpAddr = "192.168.1.10".parse().unwrap();

        assert!(extra_allowed_hosts(loopback, None).is_empty());
        assert!(extra_allowed_hosts(any, Some("")).is_empty());
        assert_eq!(extra_allowed_hosts(lan, None), ["192.168.1.10"]);
        assert_eq!(
            extra_allowed_hosts(any, Some(" outlook.lan , ,box.local ")),
            ["outlook.lan", "box.local"]
        );
    }

    #[test]
    fn cors_origins_are_normalised_to_what_browsers_send() {
        assert_eq!(
            normalize_cors_origin(" http://localhost:5173/ ").unwrap(),
            "http://localhost:5173"
        );
        assert_eq!(
            normalize_cors_origin("HTTPS://App.Example.com:443").unwrap(),
            "https://app.example.com"
        );
        for bad in [
            "",
            "*",
            "null",
            "localhost:5173",
            "file:///C:/app/index.html",
            "http://localhost:5173/app",
            "http://localhost:5173/?x=1",
        ] {
            assert!(
                normalize_cors_origin(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn api_error_maps_graph_404_401_and_resolver_ambiguity() {
        let not_found = ApiError::from(anyhow::anyhow!(
            "Graph API error (GET http://127.0.0.1/v1.0/me/messages/msg-missing-999): 404 Not Found [ErrorItemNotFound]"
        ));
        assert_eq!(not_found.status, StatusCode::NOT_FOUND);

        let unauth = ApiError::from(anyhow::anyhow!(
            "Graph API error (GET http://127.0.0.1/v1.0/me/messages): 401 Unauthorized [InvalidAuthenticationToken]"
        ));
        assert_eq!(unauth.status, StatusCode::UNAUTHORIZED);

        let forbidden = ApiError::from(anyhow::anyhow!(
            "Insufficient Graph permissions (403): restricted"
        ));
        assert_eq!(forbidden.status, StatusCode::FORBIDDEN);

        let ambiguous = map_resolve_error(anyhow::anyhow!(
            "Ambiguous mail folder 'Reports' matches multiple folders"
        ));
        assert_eq!(ambiguous.status, StatusCode::BAD_REQUEST);

        let missing_folder = map_resolve_error(anyhow::anyhow!("Mail folder not found: Ghost"));
        assert_eq!(missing_folder.status, StatusCode::NOT_FOUND);
    }
}
