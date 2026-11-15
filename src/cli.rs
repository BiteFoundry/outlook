//! `teams` CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::{
    auth::{AuthManager, LoginOptions, StoredTokens},
    config::{self, AppConfig},
    graph::GraphClient,
    models::{
        ConversationKind, ConversationSummary, ConversationTarget, DownloadedFile,
        InlineImageUpload, MentionInfo, MessageItem, QuotedReply, TeamSummary,
    },
    parser, resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    sync, tui,
};

const DELETE_JSON_REQUIRES_YES: &str = "--json 모드에서 삭제하려면 --yes 를 함께 지정해야 합니다.";

#[derive(Parser, Debug)]
#[command(
    name = "teams",
    version,
    about = "Microsoft Teams 터미널 클라이언트 (CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 기반 Microsoft Teams 터미널 클라이언트입니다.\n\
                  인자 없이 실행하면 로그인 상태를 확인한 뒤 TUI를 엽니다."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Microsoft Entra ID(OAuth 2.0)로 로그인하고 토큰을 저장합니다
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        tenant_id: Option<String>,
        #[arg(long)]
        device_code: bool,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        json: bool,
    },
    /// 저장된 토큰을 삭제합니다
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// 현재 로그인한 사용자 프로필과 토큰 상태를 표시합니다
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// 설정/DB/키링 경로 및 선택적 Graph 연결 진단을 표시합니다
    Doctor {
        /// Graph `/me` 및 `/me/presence` 호출로 실시간 연결을 점검합니다
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// 1:1, 그룹 및 모임 채팅 목록을 표시합니다
    #[command(visible_alias = "ls")]
    Chats {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 새 1:1 또는 그룹 채팅을 생성합니다
    #[command(name = "new-chat")]
    NewChat {
        /// 초대할 사용자 이메일 또는 Entra 사용자 ID (여러 번 지정 가능)
        #[arg(long = "to", required = true, value_name = "email_or_id")]
        to: Vec<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 가입된 팀과 채널 목록을 표시합니다
    Teams {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 특정 팀의 채널 목록을 표시합니다
    Channels {
        team_query: String,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 채팅방 또는 채널의 최근 메시지를 읽습니다
    #[command(visible_alias = "cat")]
    Read {
        target: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 팀 채널 루트 게시물과 스레드 답글을 읽습니다
    Thread {
        channel_target: String,
        root_msg_id: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Markdown 메시지를 전송합니다
    #[command(visible_alias = "msg")]
    Send {
        target: String,
        markdown: String,
        #[arg(long)]
        subject: Option<String>,
        /// 멘션 추가: <user_id:DisplayName> (여러 번 지정 가능)
        #[arg(long = "mention", value_name = "user_id:DisplayName")]
        mention: Vec<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
    },
    /// 채널 스레드에 답글을 달거나 채팅 메시지를 인용 답장합니다
    Reply {
        target: String,
        msg_id: String,
        markdown: String,
        #[arg(long = "mention", value_name = "user_id:DisplayName")]
        mention: Vec<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
    },
    /// 파일을 OneDrive/SharePoint에 업로드해 전송합니다 (`--inline` 시 인라인 이미지)
    #[command(name = "send-file", visible_alias = "file")]
    SendFile {
        target: String,
        path: PathBuf,
        #[arg(long)]
        caption: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        inline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메시지의 첨부 파일을 다운로드합니다
    #[command(visible_alias = "dl")]
    Download {
        target: String,
        msg_id: String,
        #[arg(long, default_value_t = 0)]
        index: usize,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 메시지에 리액션(like, heart, laugh, surprised, sad, angry)을 설정하거나 해제합니다
    React {
        target: String,
        msg_id: String,
        reaction: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 자신이 보낸 메시지를 수정합니다
    Edit {
        target: String,
        msg_id: String,
        new_markdown: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 자신이 보낸 메시지를 삭제합니다
    Delete {
        target: String,
        msg_id: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// 채팅방 또는 채널의 구성원 목록을 표시합니다
    Members {
        target: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 로컬 SQLite FTS5 캐시에서 메시지를 검색합니다
    Search {
        query: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 채팅방/팀/채널 목록과 최근 메시지를 로컬 SQLite 캐시에 동기화합니다
    Sync {
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 대화를 Markdown, JSON 또는 텍스트로 내보냅니다
    Export {
        target: String,
        #[arg(long, default_value = "md", value_parser = ["md", "json", "txt"])]
        format: String,
        #[arg(long)]
        include_threads: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 내 현재 상태(Presence)를 조회하거나 설정합니다
    Presence {
        #[arg(long, value_name = "Available|Busy|DoNotDisturb|BeRightBack|Away|Reset")]
        set: Option<String>,
        #[arg(long)]
        activity: Option<String>,
        #[arg(long)]
        expiration_mins: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// 터미널 UI(TUI)를 실행합니다
    #[command(visible_alias = "chat")]
    Tui,
    /// 에이전트용 로컬 HTTP JSON API 서버를 실행합니다
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8788)]
        port: u16,
        #[arg(long)]
        token: Option<String>,
    },
    /// 에이전트용 stdio MCP(JSON-RPC 2.0) 서버를 실행합니다
    #[command(visible_alias = "mcp-server")]
    Mcp,
}

pub async fn run_from_env() -> Result<()> {
    let mut cfg = AppConfig::load();
    let cli = Cli::parse();
    Box::pin(dispatch_command(&mut cfg, cli.command)).await
}

async fn dispatch_command(cfg: &mut AppConfig, command: Option<Command>) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg)).await,
        Some(Command::Login {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            token,
            port,
            json,
        }) => {
            Box::pin(cmd_login(
                cfg,
                LoginOptions {
                    client_id,
                    tenant_id,
                    device_code,
                    client_secret,
                    access_token: token,
                    port,
                },
                json,
            ))
            .await
        }
        Some(Command::Logout { json }) => Box::pin(cmd_logout(cfg, json)).await,
        Some(Command::Whoami { json }) => Box::pin(cmd_whoami(cfg, json)).await,
        Some(Command::Doctor { probe, json }) => Box::pin(cmd_doctor(cfg, probe, json)).await,
        Some(Command::Chats {
            limit,
            refresh,
            json,
        }) => Box::pin(cmd_chats(cfg, limit, refresh, json)).await,
        Some(Command::NewChat {
            to,
            topic,
            message,
            json,
        }) => Box::pin(cmd_new_chat(cfg, to, topic, message, json)).await,
        Some(Command::Teams { refresh, json }) => Box::pin(cmd_teams(cfg, refresh, json)).await,
        Some(Command::Channels {
            team_query,
            refresh,
            json,
        }) => Box::pin(cmd_channels(cfg, &team_query, refresh, json)).await,
        Some(Command::Read {
            target,
            limit,
            offline,
            json,
        }) => Box::pin(cmd_read(cfg, &target, limit, offline, json)).await,
        Some(Command::Thread {
            channel_target,
            root_msg_id,
            limit,
            offline,
            json,
        }) => {
            Box::pin(cmd_thread(
                cfg,
                &channel_target,
                &root_msg_id,
                limit,
                offline,
                json,
            ))
            .await
        }
        Some(Command::Send {
            target,
            markdown,
            subject,
            mention,
            important,
            json,
        }) => {
            Box::pin(cmd_send(
                cfg,
                &target,
                &markdown,
                subject,
                &mention,
                important,
                json,
            ))
            .await
        }
        Some(Command::Reply {
            target,
            msg_id,
            markdown,
            mention,
            important,
            json,
        }) => {
            Box::pin(cmd_reply(
                cfg,
                &target,
                &msg_id,
                &markdown,
                &mention,
                important,
                json,
            ))
            .await
        }
        Some(Command::SendFile {
            target,
            path,
            caption,
            thread,
            inline,
            json,
        }) => {
            Box::pin(cmd_send_file(
                cfg,
                &target,
                &path,
                caption.as_deref(),
                thread.as_deref(),
                inline,
                json,
            ))
            .await
        }
        Some(Command::Download {
            target,
            msg_id,
            index,
            thread,
            out,
            json,
        }) => {
            Box::pin(cmd_download(
                cfg,
                &target,
                &msg_id,
                index,
                thread.as_deref(),
                out.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::React {
            target,
            msg_id,
            reaction,
            off,
            thread,
            json,
        }) => {
            Box::pin(cmd_react(
                cfg,
                &target,
                &msg_id,
                &reaction,
                !off,
                thread.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Edit {
            target,
            msg_id,
            new_markdown,
            thread,
            json,
        }) => {
            Box::pin(cmd_edit(
                cfg,
                &target,
                &msg_id,
                &new_markdown,
                thread.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Delete {
            target,
            msg_id,
            thread,
            yes,
            json,
        }) => Box::pin(cmd_delete(cfg, &target, &msg_id, thread.as_deref(), yes, json)).await,
        Some(Command::Members {
            target,
            offline,
            json,
        }) => Box::pin(cmd_members(cfg, &target, offline, json)).await,
        Some(Command::Search {
            query,
            target,
            limit,
            json,
        }) => Box::pin(cmd_search(cfg, &query, target.as_deref(), limit, json)).await,
        Some(Command::Sync {
            target,
            limit,
            json,
        }) => Box::pin(cmd_sync(cfg, target.as_deref(), limit, json)).await,
        Some(Command::Export {
            target,
            format,
            include_threads,
            limit,
            offline,
            out,
        }) => {
            Box::pin(cmd_export(
                cfg,
                &target,
                &format,
                include_threads,
                limit,
                offline,
                out.as_deref(),
            ))
            .await
        }
        Some(Command::Presence {
            set,
            activity,
            expiration_mins,
            json,
        }) => {
            Box::pin(cmd_presence(
                cfg,
                set.as_deref(),
                activity.as_deref(),
                expiration_mins,
                json,
            ))
            .await
        }
        Some(Command::Tui) => Box::pin(cmd_tui(cfg)).await,
        Some(Command::Serve { bind, port, token }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions { bind, port, token },
            ))
            .await
        }
        Some(Command::Mcp) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(crate::mcp::run(cfg.clone(), graph, storage)).await
        }
    }
}

async fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    if require_login && tokens.is_none() {
        bail!("로그인이 필요합니다. 먼저 `teams login` 을 실행하세요.");
    }
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let storage = Storage::open(&config::db_path(user_id))?;
    let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
    if let Some(uid) = user_id {
        graph = graph.with_my_user_id(uid);
    }
    Ok((auth, graph, storage))
}

async fn cmd_tui(cfg: &mut AppConfig) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    if auth.current_tokens().await.is_none() {
        println!("저장된 로그인 정보가 없습니다. Microsoft 계정 로그인을 시작합니다.");
        println!("(자체 Azure App 사용 시: teams login --client-id <APP_ID> --tenant-id <TENANT_ID>)");
        cmd_login(cfg, LoginOptions::default(), false).await?;
        println!();
    }

    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    if storage.list_conversations()?.is_empty() {
        let _ = sync::bootstrap_conversations(&graph, &storage).await;
    }
    tui::run(cfg.clone(), auth, graph, storage).await
}

async fn cmd_login(cfg: &mut AppConfig, opts: LoginOptions, json_out: bool) -> Result<()> {
    let mut config_changed = false;
    if let Some(ref cid) = opts.client_id {
        let trimmed = cid.trim();
        if !trimmed.is_empty() && cfg.client_id != trimmed {
            cfg.client_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if let Some(ref tid) = opts.tenant_id {
        let trimmed = tid.trim();
        if !trimmed.is_empty() && cfg.tenant_id != trimmed {
            cfg.tenant_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if config_changed {
        cfg.save()?;
    }

    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.login(&opts).await?;
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let me = graph.get_me().await?;
    let email = me
        .mail
        .clone()
        .or_else(|| me.user_principal_name.clone());
    auth.update_identity(me.id.clone(), me.display_name.clone(), email.clone())
        .await?;

    let updated = auth.current_tokens().await.unwrap_or(tokens);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "user": me,
                "client_id": updated.client_id,
                "tenant_id": updated.tenant_id,
                "auth_mode": updated.auth_mode,
                "expires_at": updated.expires_at,
            }))?
        );
    } else {
        println!(
            "로그인 성공: {} ({}) [id={}]",
            me.display_name,
            email.as_deref().unwrap_or("-"),
            me.id
        );
        println!(
            "방식: {} · tenant={} · 만료: {}",
            updated.auth_mode,
            updated.tenant_id,
            format_unix_ts(updated.expires_at)
        );
    }
    Ok(())
}

async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("로그아웃했습니다. 저장된 토큰을 삭제했습니다.");
    }
    Ok(())
}

async fn cmd_whoami(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let Some(tokens) = auth.current_tokens().await else {
        bail!("로그인이 필요합니다. 먼저 `teams login` 을 실행하세요.");
    };

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let live_profile = if tokens.user_id.is_none() || tokens.user_display_name.is_none() {
        graph.get_me().await.ok()
    } else {
        None
    };

    let user_id = tokens
        .user_id
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.id.clone()));
    let display_name = tokens
        .user_display_name
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.display_name.clone()));
    let email = tokens.user_email.clone().or_else(|| {
        live_profile
            .as_ref()
            .and_then(|p| p.mail.clone().or_else(|| p.user_principal_name.clone()))
    });

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "user_id": user_id,
                "display_name": display_name,
                "email": email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": tokens.is_expired_soon(0),
            }))?
        );
    } else {
        println!(
            "사용자: {} ({})",
            display_name.as_deref().unwrap_or("-"),
            email.as_deref().unwrap_or("-")
        );
        println!("user_id: {}", user_id.as_deref().unwrap_or("-"));
        println!("client_id: {}", tokens.client_id);
        println!("tenant_id: {}", tokens.tenant_id);
        println!("auth_mode: {}", tokens.auth_mode);
        println!(
            "토큰 만료: {} ({})",
            format_unix_ts(tokens.expires_at),
            if tokens.is_expired_soon(0) {
                "만료됨"
            } else {
                "유효"
            }
        );
    }
    Ok(())
}

async fn cmd_doctor(cfg: &AppConfig, probe: bool, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());

    let cfg_path = config::config_file_path();
    let cred_path = config::credential_fallback_path();
    let drafts = config::drafts_path();
    let db = config::db_path(user_id);
    let dl_dir = cfg.resolved_download_dir();

    let keyring_status = describe_keyring_status(tokens.as_ref(), &cred_path);

    let mut probe_me: Option<serde_json::Value> = None;
    let mut probe_presence: Option<serde_json::Value> = None;
    let mut probe_error: Option<String> = None;

    if probe {
        let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
        if let Some(uid) = user_id {
            graph = graph.with_my_user_id(uid);
        }
        match graph.get_me().await {
            Ok(me) => {
                probe_me = Some(serde_json::to_value(&me)?);
                match graph.get_my_presence().await {
                    Ok(p) => probe_presence = Some(serde_json::to_value(&p)?),
                    Err(err) => probe_error = Some(format!("presence probe 실패: {err:#}")),
                }
            }
            Err(err) => probe_error = Some(format!("get_me probe 실패: {err:#}")),
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "config_path": cfg_path,
                "config_exists": cfg_path.exists(),
                "credentials_fallback_path": cred_path,
                "credentials_fallback_exists": cred_path.exists(),
                "drafts_path": drafts,
                "db_path": db,
                "db_exists": db.exists(),
                "download_dir": dl_dir,
                "client_id": cfg.client_id,
                "tenant_id": cfg.tenant_id,
                "graph_base_url": cfg.graph_base_url,
                "auth_base_url": cfg.auth_base_url,
                "keyring_status": keyring_status,
                "logged_in": tokens.is_some(),
                "probe": if probe {
                    Some(json!({
                        "ok": probe_error.is_none(),
                        "me": probe_me,
                        "presence": probe_presence,
                        "error": probe_error,
                    }))
                } else {
                    None
                },
            }))?
        );
    } else {
        println!("teams v{}", env!("CARGO_PKG_VERSION"));
        println!("config.toml: {} (exists={})", cfg_path.display(), cfg_path.exists());
        println!(
            "credentials.json: {} (exists={})",
            cred_path.display(),
            cred_path.exists()
        );
        println!("drafts.json: {}", drafts.display());
        println!("cache.db: {} (exists={})", db.display(), db.exists());
        println!("download_dir: {}", dl_dir.display());
        println!("client_id: {}", cfg.client_id);
        println!("tenant_id: {}", cfg.tenant_id);
        println!("graph_base_url: {}", cfg.graph_base_url);
        println!("auth_base_url: {}", cfg.auth_base_url);
        println!("keyring_status: {keyring_status}");
        if probe {
            if let Some(err) = probe_error {
                println!("probe: 실패 ({err})");
            } else {
                println!(
                    "probe: 성공 (me={}, presence={})",
                    probe_me
                        .as_ref()
                        .and_then(|v| v.get("display_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                    probe_presence
                        .as_ref()
                        .and_then(|v| v.get("availability"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                );
            }
        }
    }

    Ok(())
}

fn describe_keyring_status(tokens: Option<&StoredTokens>, fallback_path: &Path) -> String {
    match tokens {
        None => "미로그인 (저장된 토큰 없음)".to_string(),
        Some(tok) if tok.auth_mode == "env_token" => {
            "env_token (TEAMS_ACCESS_TOKEN 사용 중)".to_string()
        }
        Some(_) if fallback_path.exists() => {
            "credentials.json 파일 폴백 사용 중".to_string()
        }
        Some(_) => "OS 키링 활성 (토큰 로드됨)".to_string(),
    }
}

async fn cmd_chats(cfg: &AppConfig, limit: usize, refresh: bool, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let limit = clamp_limit(limit);

    let cached_chats: Vec<ConversationSummary> = storage
        .list_conversations()?
        .into_iter()
        .filter(|c| !c.kind.is_channel())
        .collect();

    let mut chats = if refresh || cached_chats.is_empty() {
        let fetched = graph.list_chats(limit).await?;
        storage.upsert_conversations(&fetched)?;
        let stored: Vec<ConversationSummary> = storage
            .list_conversations()?
            .into_iter()
            .filter(|c| !c.kind.is_channel())
            .collect();
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached_chats
    };

    chats.truncate(limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "chats": chats }))?
        );
    } else if chats.is_empty() {
        println!("표시할 채팅이 없습니다.");
    } else {
        for c in &chats {
            let unread = if c.unread_count > 0 {
                format!(" [unread:{}]", c.unread_count)
            } else {
                String::new()
            };
            let preview = c.last_msg_preview.as_deref().unwrap_or("");
            println!(
                "{}  [{}] {}{}  {}",
                c.conv_key,
                c.kind.as_str(),
                c.title,
                unread,
                truncate_single_line(preview, 60)
            );
        }
    }
    Ok(())
}

async fn cmd_new_chat(
    cfg: &AppConfig,
    to: Vec<String>,
    topic: Option<String>,
    message: Option<String>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let members: Vec<String> = to
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        bail!("최소 1명의 수신자(--to)를 지정해야 합니다.");
    }

    let chat = graph.create_chat(&members, topic.as_deref()).await?;
    storage.upsert_conversations(std::slice::from_ref(&chat))?;

    let mut sent_msg: Option<MessageItem> = None;
    if let Some(md) = message {
        if !md.trim().is_empty() {
            let outgoing =
                parser::compile_outgoing_markdown(&md, None, vec![], vec![], vec![], None);
            let sent = graph.send_message(&chat.target, &outgoing).await?;
            let _ = storage.upsert_messages(std::slice::from_ref(&sent));
            sent_msg = Some(sent);
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "chat": chat,
                "message": sent_msg,
            }))?
        );
    } else {
        println!("채팅 생성 완료: {} ({})", chat.title, chat.conv_key);
        if let Some(msg) = sent_msg {
            println!("초기 메시지 전송됨: msg_id={}", msg.msg_id);
        }
    }
    Ok(())
}

async fn cmd_teams(cfg: &AppConfig, refresh: bool, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let teams = load_teams_with_refresh(&graph, &storage, refresh).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "teams": teams }))?
        );
    } else if teams.is_empty() {
        println!("가입된 팀이 없습니다.");
    } else {
        for team in &teams {
            println!("{} ({})", team.display_name, team.id);
            for ch in &team.channels {
                println!(
                    "  - {} [{}] ({})",
                    ch.display_name,
                    ch.membership_type,
                    ch.conv_key()
                );
            }
        }
    }
    Ok(())
}

async fn cmd_channels(
    cfg: &AppConfig,
    team_query: &str,
    refresh: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let teams = load_teams_with_refresh(&graph, &storage, refresh).await?;
    let team = resolve_team_from_slice(&teams, team_query)?;

    let channels = if refresh {
        graph.list_team_channels(&team.id, &team.display_name).await?
    } else {
        team.channels.clone()
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "team": { "id": team.id, "display_name": team.display_name },
                "channels": channels,
            }))?
        );
    } else {
        println!("# {} ({})", team.display_name, team.id);
        for ch in &channels {
            println!(
                "  {}  [{}]  {}",
                ch.conv_key(),
                ch.membership_type,
                ch.display_name
            );
        }
    }
    Ok(())
}

async fn load_teams_with_refresh(
    graph: &GraphClient,
    storage: &Storage,
    refresh: bool,
) -> Result<Vec<TeamSummary>> {
    let cached = storage.list_teams()?;
    if refresh || cached.is_empty() {
        let fetched = graph.list_joined_teams_with_channels().await?;
        storage.upsert_teams(&fetched)?;
        let stored = storage.list_teams()?;
        if stored.is_empty() {
            Ok(fetched)
        } else {
            Ok(stored)
        }
    } else {
        Ok(cached)
    }
}

fn resolve_team_from_slice(teams: &[TeamSummary], query: &str) -> Result<TeamSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("팀 이름 또는 ID를 입력하세요.");
    }
    if let Some(exact_id) = teams.iter().find(|t| t.id == q) {
        return Ok(exact_id.clone());
    }
    let q_lower = q.to_lowercase();
    let exact_name: Vec<&TeamSummary> = teams
        .iter()
        .filter(|t| t.display_name.to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    }
    let matches: Vec<&TeamSummary> = teams
        .iter()
        .filter(|t| t.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match matches.len() {
        0 => bail!("팀을 찾을 수 없습니다: {q}"),
        1 => Ok(matches[0].clone()),
        _ => {
            let names: Vec<String> = matches
                .iter()
                .map(|t| format!("{} ({})", t.display_name, t.id))
                .collect();
            bail!("여러 팀이 일치합니다 ({q}): {}", names.join(", "))
        }
    }
}

async fn cmd_read(
    cfg: &AppConfig,
    target: &str,
    limit: usize,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let conv = resolve_target(&storage, &graph, target, offline).await?;

    let fetched = if !offline {
        let msgs = graph.list_messages(&conv.target, limit).await?;
        storage.upsert_messages(&msgs)?;
        Some(msgs)
    } else {
        None
    };

    let mut messages = storage.list_root_messages(&conv.conv_key, limit, None)?;
    if messages.is_empty() {
        if let Some(f) = fetched {
            messages = f;
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "conversation": conv,
                "messages": messages,
            }))?
        );
    } else {
        println!("# {} ({})", conv.title, conv.conv_key);
        if messages.is_empty() {
            println!("(메시지가 없습니다)");
        } else {
            for msg in &messages {
                print_message_line(msg, "");
            }
        }
    }
    Ok(())
}

async fn cmd_thread(
    cfg: &AppConfig,
    channel_target: &str,
    root_msg_id: &str,
    limit: usize,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let conv = resolve_target(&storage, &graph, channel_target, offline).await?;

    let ConversationTarget::Channel {
        ref team_id,
        ref channel_id,
    } = conv.target
    else {
        bail!("thread 명령은 팀 채널에서만 사용할 수 있습니다: {}", conv.conv_key);
    };

    let thread = if offline {
        storage
            .get_thread(&conv.conv_key, root_msg_id, limit)?
            .with_context(|| format!("로컬 캐시에 스레드가 없습니다: {root_msg_id}"))?
    } else {
        let view = graph
            .get_thread(team_id, channel_id, root_msg_id, limit)
            .await?;
        let mut all = Vec::with_capacity(1 + view.replies.len());
        all.push(view.root.clone());
        all.extend(view.replies.iter().cloned());
        storage.upsert_messages(&all)?;
        view
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "conversation": conv,
                "thread": thread,
            }))?
        );
    } else {
        println!("# {} ({}) — 스레드 {}", conv.title, conv.conv_key, thread.root.msg_id);
        print_message_line(&thread.root, "");
        if thread.replies.is_empty() {
            println!("  (답글 없음)");
        } else {
            for reply in &thread.replies {
                print_message_line(reply, "  ↳ ");
            }
        }
    }
    Ok(())
}

fn print_message_line(msg: &MessageItem, indent: &str) {
    let mut tags = Vec::new();
    if let Some(ref subj) = msg.subject {
        if !subj.trim().is_empty() {
            tags.push(format!("[주제: {}]", subj.trim()));
        }
    }
    if msg.importance.eq_ignore_ascii_case("high") || msg.importance.eq_ignore_ascii_case("urgent") {
        tags.push(format!("[중요:{}]", msg.importance));
    }
    if msg.reply_count > 0 {
        tags.push(format!("[💬 {}]", msg.reply_count));
    }
    if !msg.attachments.is_empty() {
        let names: Vec<String> = msg
            .attachments
            .iter()
            .map(|a| {
                a.name
                    .clone()
                    .unwrap_or_else(|| a.content_type.clone())
            })
            .collect();
        tags.push(format!("[📎 {}]", names.join(", ")));
    }
    if !msg.reactions.is_empty() {
        let reacts: Vec<String> = msg
            .reactions
            .iter()
            .map(|r| format!("{} {}", reaction_symbol(&r.reaction_type), r.count))
            .collect();
        tags.push(format!("[{}]", reacts.join(" ")));
    }

    let body = if msg.deleted {
        "(삭제된 메시지)".to_string()
    } else if !msg.body_plain.trim().is_empty() {
        msg.body_plain.trim().to_string()
    } else {
        parser::html_and_cards_to_plain(&msg.body_html, &msg.attachments)
            .trim()
            .to_string()
    };

    let quote = match (&msg.reply_to_author, &msg.reply_to_preview) {
        (Some(author), Some(prev)) => format!(" ↩ {author}: {}", truncate_single_line(prev, 40)),
        _ => String::new(),
    };

    let tag_str = if tags.is_empty() {
        String::new()
    } else {
        format!(" {}", tags.join(" "))
    };

    println!(
        "{indent}[{}] {} ({}){quote}{tag_str}: {}",
        msg.created_at, msg.author_name, msg.msg_id, body
    );
}

fn reaction_symbol(reaction_type: &str) -> &str {
    match reaction_type {
        "like" => "👍",
        "heart" => "❤️",
        "laugh" => "😄",
        "surprised" => "😮",
        "sad" => "😢",
        "angry" => "😡",
        other => other,
    }
}

async fn cmd_send(
    cfg: &AppConfig,
    target: &str,
    markdown: &str,
    subject: Option<String>,
    mentions_raw: &[String],
    important: bool,
    json_out: bool,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("보낼 메시지가 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;

    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, subject, mentions, vec![], vec![], None);
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.send_message(&conv.target, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conversation": conv,
                "message": sent,
            }))?
        );
    } else {
        println!("전송 완료: {} ({}) msg_id={}", conv.title, conv.conv_key, sent.msg_id);
    }
    Ok(())
}

async fn cmd_reply(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    markdown: &str,
    mentions_raw: &[String],
    important: bool,
    json_out: bool,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("보낼 답글이 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;

    let quote = build_quote_reply(&storage, &graph, &conv, msg_id).await;
    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, None, mentions, vec![], vec![], quote);
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.reply_message(&conv.target, msg_id, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conversation": conv,
                "reply_to": msg_id,
                "message": sent,
            }))?
        );
    } else {
        println!(
            "답장 완료: {} ({}) msg_id={} (대상 {})",
            conv.title, conv.conv_key, sent.msg_id, msg_id
        );
    }
    Ok(())
}

async fn cmd_send_file(
    cfg: &AppConfig,
    target: &str,
    path: &Path,
    caption: Option<&str>,
    thread: Option<&str>,
    inline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let sent = send_file_or_inline(&graph, &storage, &conv, path, caption, thread, inline).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conversation": conv,
                "path": path,
                "inline": inline,
                "message": sent,
            }))?
        );
    } else {
        println!(
            "파일 전송 완료: {} ({}) msg_id={} ({})",
            conv.title,
            conv.conv_key,
            sent.msg_id,
            path.display()
        );
    }
    Ok(())
}

async fn cmd_download(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    index: usize,
    thread: Option<&str>,
    out: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let out_dir = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded =
        download_message_attachment(&graph, &storage, &conv, msg_id, index, thread, &out_dir)
            .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": downloaded.path,
                "file_name": downloaded.file_name,
                "bytes_written": downloaded.bytes_written,
            }))?
        );
    } else {
        println!(
            "다운로드 완료: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.bytes_written
        );
    }
    Ok(())
}

async fn cmd_react(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    reaction: &str,
    enable: bool,
    thread: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let normalized = normalize_reaction(reaction)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    graph
        .set_reaction(&conv.target, msg_id, thread, &normalized, enable)
        .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
                "reaction": normalized,
                "enable": enable,
            }))?
        );
    } else {
        println!(
            "리액션 {}: {} ({}) msg_id={}",
            if enable { "설정" } else { "해제" },
            normalized,
            conv.conv_key,
            msg_id
        );
    }
    Ok(())
}

async fn cmd_edit(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    new_markdown: &str,
    thread: Option<&str>,
    json_out: bool,
) -> Result<()> {
    if new_markdown.trim().is_empty() {
        bail!("수정할 메시지 본문이 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let compiled =
        parser::compile_outgoing_markdown(new_markdown, None, vec![], vec![], vec![], None);
    graph
        .edit_message(&conv.target, msg_id, thread, &compiled.body_html)
        .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))?
        );
    } else {
        println!("메시지 수정 완료: {} msg_id={}", conv.conv_key, msg_id);
    }
    Ok(())
}

async fn cmd_delete(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    thread: Option<&str>,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;

    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "'{}' ({}) 의 메시지 {msg_id} 를 삭제합니다. 계속하려면 'yes' 입력: ",
            conv.title, conv.conv_key
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_message(&conv.target, msg_id, thread).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))?
        );
    } else {
        println!("메시지 삭제 완료: {} msg_id={}", conv.conv_key, msg_id);
    }
    Ok(())
}

fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("확인 입력을 받지 못했습니다. 메시지를 삭제하지 않았습니다.")?;
    if !answer.trim().eq_ignore_ascii_case("yes") {
        bail!("취소했습니다. 메시지를 삭제하지 않았습니다.");
    }
    Ok(())
}

async fn cmd_members(cfg: &AppConfig, target: &str, offline: bool, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let conv = resolve_target(&storage, &graph, target, offline).await?;

    let members = if offline {
        storage.list_members(&conv.conv_key)?
    } else {
        let fetched = graph.list_members(&conv.target).await?;
        let _ = storage.upsert_members(&conv.conv_key, &fetched);
        fetched
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "conv_key": conv.conv_key,
                "title": conv.title,
                "members": members,
            }))?
        );
    } else if members.is_empty() {
        println!("구성원 정보가 없습니다.");
    } else {
        println!("# {} ({}) 구성원 ({}명)", conv.title, conv.conv_key, members.len());
        for m in &members {
            let role = if m.roles.is_empty() {
                String::new()
            } else {
                format!(" [{}]", m.roles.join(","))
            };
            let pres = m
                .presence
                .as_ref()
                .map(|p| format!(" ({})", p.availability))
                .unwrap_or_default();
            println!(
                "  {}  <{}>{}{}",
                m.display_name,
                m.email.as_deref().unwrap_or(&m.user_id),
                role,
                pres
            );
        }
    }
    Ok(())
}

async fn cmd_search(
    cfg: &AppConfig,
    query: &str,
    target: Option<&str>,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("검색어가 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, false).await?;
    let limit = clamp_limit(limit);

    let conv_key_filter = match target {
        Some(t) => Some(resolve_target(&storage, &graph, t, true).await?.conv_key),
        None => None,
    };

    let hits = storage.search_messages(query, conv_key_filter.as_deref(), limit)?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "hits": hits,
            }))?
        );
    } else if hits.is_empty() {
        println!("'{query}' 와 일치하는 메시지가 없습니다.");
    } else {
        for hit in &hits {
            println!(
                "[{}] {} | {}: {}",
                hit.created_at, hit.conv_title, hit.author_name, hit.snippet
            );
        }
    }
    Ok(())
}

async fn cmd_sync(
    cfg: &AppConfig,
    target: Option<&str>,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let limit = clamp_limit(limit);

    let targets: Vec<ConversationSummary> = match target {
        Some(t) => vec![resolve_target(&storage, &graph, t, false).await?],
        None => {
            sync::bootstrap_conversations(&graph, &storage).await?;
            storage.list_conversations()?
        }
    };

    let mut total_messages = 0usize;
    let mut synced_convs = 0usize;
    let mut errors = Vec::new();

    for conv in &targets {
        match graph.list_messages(&conv.target, limit).await {
            Ok(msgs) => {
                let count = storage.upsert_messages(&msgs).unwrap_or(msgs.len());
                total_messages += count.max(msgs.len());
                synced_convs += 1;
            }
            Err(err) => {
                errors.push(format!("{}: {err:#}", conv.conv_key));
            }
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": errors.is_empty(),
                "conversations_total": targets.len(),
                "conversations_synced": synced_convs,
                "messages_synced": total_messages,
                "errors": errors,
            }))?
        );
    } else {
        println!(
            "동기화 완료: 대화 {synced_convs}/{}개 · 메시지 {total_messages}건",
            targets.len()
        );
        for err in &errors {
            eprintln!("경고: {err}");
        }
    }
    Ok(())
}

async fn cmd_export(
    cfg: &AppConfig,
    target: &str,
    format: &str,
    include_threads: bool,
    limit: usize,
    offline: bool,
    out: Option<&Path>,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let conv = resolve_target(&storage, &graph, target, offline).await?;

    let fetched = if !offline {
        let msgs = graph.list_messages(&conv.target, limit).await?;
        storage.upsert_messages(&msgs)?;
        Some(msgs)
    } else {
        None
    };

    let mut messages = storage.list_root_messages(&conv.conv_key, limit, None)?;
    if messages.is_empty() {
        if let Some(f) = fetched {
            messages = f;
        }
    }

    let mut threads_by_root: HashMap<String, Vec<MessageItem>> = HashMap::new();
    if include_threads {
        if let ConversationTarget::Channel {
            ref team_id,
            ref channel_id,
        } = conv.target
        {
            for msg in &messages {
                if msg.reply_count > 0 {
                    let replies = if offline {
                        storage.list_thread_replies(&conv.conv_key, &msg.msg_id, limit)?
                    } else {
                        let r = graph
                            .list_thread_replies(team_id, channel_id, &msg.msg_id, limit)
                            .await
                            .unwrap_or_default();
                        let _ = storage.upsert_messages(&r);
                        r
                    };
                    if !replies.is_empty() {
                        threads_by_root.insert(msg.msg_id.clone(), replies);
                    }
                }
            }
        }
    }

    let body = match format {
        "json" => {
            if include_threads {
                let entries: Vec<serde_json::Value> = messages
                    .iter()
                    .map(|m| {
                        let replies = threads_by_root
                            .get(&m.msg_id)
                            .cloned()
                            .unwrap_or_default();
                        json!({
                            "message": m,
                            "replies": replies,
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&json!({
                    "conversation": conv,
                    "items": entries,
                }))?
            } else {
                serde_json::to_string_pretty(&json!({
                    "conversation": conv,
                    "messages": messages,
                }))?
            }
        }
        "txt" => {
            let mut out_str = format!("# {} ({})\n\n", conv.title, conv.conv_key);
            for m in &messages {
                let plain = if !m.body_plain.trim().is_empty() {
                    m.body_plain.trim().to_string()
                } else {
                    parser::html_and_cards_to_plain(&m.body_html, &m.attachments)
                };
                out_str.push_str(&format!("[{}] {}: {}\n", m.created_at, m.author_name, plain));
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rplain = if !r.body_plain.trim().is_empty() {
                            r.body_plain.trim().to_string()
                        } else {
                            parser::html_and_cards_to_plain(&r.body_html, &r.attachments)
                        };
                        out_str.push_str(&format!(
                            "  ↳ [{}] {}: {}\n",
                            r.created_at, r.author_name, rplain
                        ));
                    }
                }
            }
            out_str
        }
        _ => {
            let mut out_str = format!("# {}\n\n", conv.title);
            for m in &messages {
                if let Some(ref subj) = m.subject {
                    if !subj.trim().is_empty() {
                        out_str.push_str(&format!("## {}\n\n", subj.trim()));
                    }
                }
                let md = parser::html_and_cards_to_markdown(&m.body_html, &m.attachments);
                let text = if md.trim().is_empty() {
                    m.body_plain.trim().to_string()
                } else {
                    md.trim().to_string()
                };
                out_str.push_str(&format!(
                    "- **{}** `{}`\n\n  {}\n\n",
                    m.author_name,
                    m.created_at,
                    text.replace('\n', "\n  ")
                ));
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rmd = parser::html_and_cards_to_markdown(&r.body_html, &r.attachments);
                        let rtext = if rmd.trim().is_empty() {
                            r.body_plain.trim().to_string()
                        } else {
                            rmd.trim().to_string()
                        };
                        out_str.push_str(&format!(
                            "  - ↳ **{}** `{}`: {}\n",
                            r.author_name, r.created_at, rtext
                        ));
                    }
                    out_str.push('\n');
                }
            }
            out_str
        }
    };

    match out {
        Some(path) => {
            config::ensure_parent(path)?;
            std::fs::write(path, body)
                .with_context(|| format!("내보내기 파일 쓰기 실패: {}", path.display()))?;
            println!("내보내기 완료: {}", path.display());
        }
        None => print!("{body}"),
    }
    Ok(())
}

async fn cmd_presence(
    cfg: &AppConfig,
    set: Option<&str>,
    activity: Option<&str>,
    expiration_mins: Option<u32>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;

    if let Some(raw_avail) = set {
        let availability = normalize_presence_availability(raw_avail)?;
        let act = activity
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .unwrap_or(&availability);
        graph
            .set_my_presence(&availability, act, expiration_mins)
            .await?;

        let latest = graph.get_my_presence().await.ok();
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "availability": availability,
                    "activity": act,
                    "expiration_mins": expiration_mins,
                    "presence": latest,
                }))?
            );
        } else {
            println!("상태 설정 완료: {availability} ({act})");
        }
    } else {
        let status = graph.get_my_presence().await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            println!("현재 상태: {} ({})", status.availability, status.activity);
        }
    }
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(&['\r', '\n'][..]).to_string())
}

// ---------------------------------------------------------------------------
// Shared `pub(crate)` helpers reused by `serve.rs` and `mcp.rs`
// ---------------------------------------------------------------------------

pub(crate) fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 1_000)
}

pub(crate) async fn resolve_target(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    offline: bool,
) -> Result<ConversationSummary> {
    if offline {
        let cached = storage.list_conversations()?;
        resolver::resolve_from_slice(&cached, query)
    } else {
        resolver::resolve_conversation(storage, graph, query).await
    }
}

pub(crate) fn parse_mentions_cli(values: &[String]) -> Result<Vec<MentionInfo>> {
    values
        .iter()
        .enumerate()
        .map(|(idx, raw)| parse_mention_spec(raw, idx as i32))
        .collect()
}

pub(crate) fn parse_mention_spec(spec: &str, id: i32) -> Result<MentionInfo> {
    let Some((uid, name)) = spec.split_once(':') else {
        bail!("멘션 형식이 잘못되었습니다: '{spec}' (user_id:DisplayName)");
    };
    let user_id = uid.trim();
    let display_name = name.trim();
    if user_id.is_empty() || display_name.is_empty() {
        bail!("멘션 user_id 와 DisplayName 은 비어 있을 수 없습니다: '{spec}'");
    }
    Ok(MentionInfo {
        id,
        user_id: user_id.to_string(),
        display_name: display_name.to_string(),
    })
}

pub(crate) fn normalize_reaction(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("리액션 이름이 비어 있습니다.");
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "like" | "heart" | "laugh" | "surprised" | "sad" | "angry" => Ok(lower),
        _ if !trimmed.is_ascii() => Ok(trimmed.to_string()),
        _ => bail!(
            "지원하지 않는 리액션입니다: '{trimmed}' (like, heart, laugh, surprised, sad, angry 또는 이모지)"
        ),
    }
}

pub(crate) fn normalize_presence_availability(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "available" => Ok("Available".to_string()),
        "busy" => Ok("Busy".to_string()),
        "donotdisturb" | "do_not_disturb" | "dnd" => Ok("DoNotDisturb".to_string()),
        "berightback" | "be_right_back" | "brb" => Ok("BeRightBack".to_string()),
        "away" => Ok("Away".to_string()),
        "reset" => Ok("Reset".to_string()),
        other => bail!(
            "지원하지 않는 상태 값입니다: '{other}' (Available|Busy|DoNotDisturb|BeRightBack|Away|Reset)"
        ),
    }
}

pub(crate) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

pub(crate) async fn build_quote_reply(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    msg_id: &str,
) -> Option<QuotedReply> {
    if conv.kind == ConversationKind::Channel {
        return None;
    }
    let msg = match storage.get_message(&conv.conv_key, msg_id).ok().flatten() {
        Some(m) => Some(m),
        None => {
            if let Ok(msgs) = graph.list_messages(&conv.target, 50).await {
                let _ = storage.upsert_messages(&msgs);
                msgs.into_iter().find(|m| m.msg_id == msg_id)
            } else {
                None
            }
        }
    };
    msg.map(|m| QuotedReply {
        msg_id: m.msg_id,
        author_name: m.author_name,
        preview_plain: truncate_single_line(&m.body_plain, 120),
    })
}

pub(crate) async fn send_file_or_inline(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    path: &Path,
    caption: Option<&str>,
    thread_root_id: Option<&str>,
    inline: bool,
) -> Result<MessageItem> {
    if !path.exists() || !path.is_file() {
        bail!("파일을 찾을 수 없습니다: {}", path.display());
    }

    let sent = if inline {
        let mime_type = image_mime_for_path(path).with_context(|| {
            format!(
                "--inline 은 이미지 파일(png/jpg/jpeg/gif/webp/bmp)만 지원합니다: {}",
                path.display()
            )
        })?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("이미지 파일 읽기 실패: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToString::to_string);
        let temp_id = "1".to_string();
        let inline_upload = InlineImageUpload {
            temp_id: temp_id.clone(),
            mime_type: mime_type.to_string(),
            bytes,
            file_name: file_name.clone(),
        };
        let mut outgoing = parser::compile_outgoing_markdown(
            caption.unwrap_or(""),
            None,
            vec![],
            vec![],
            vec![inline_upload],
            None,
        );
        let marker = format!("hostedContents/{temp_id}/$value");
        if !outgoing.body_html.contains(&marker) {
            let alt = file_name.as_deref().unwrap_or("image");
            outgoing.body_html.push_str(&format!(
                "<p><img src=\"../{marker}\" alt=\"{alt}\" /></p>"
            ));
        }
        if let Some(root_id) = thread_root_id {
            graph.reply_message(&conv.target, root_id, &outgoing).await?
        } else {
            graph.send_message(&conv.target, &outgoing).await?
        }
    } else {
        let caption_html = caption
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| {
                parser::compile_outgoing_markdown(c, None, vec![], vec![], vec![], None).body_html
            });
        graph
            .send_file(&conv.target, path, caption_html.as_deref(), thread_root_id)
            .await?
    };

    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
    Ok(sent)
}

pub(crate) async fn find_message_with_fallback(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    msg_id: &str,
    thread_root_id: Option<&str>,
) -> Result<MessageItem> {
    if let Some(msg) = storage.get_message(&conv.conv_key, msg_id)? {
        return Ok(msg);
    }

    if let (
        Some(root_id),
        ConversationTarget::Channel {
            team_id,
            channel_id,
        },
    ) = (thread_root_id, &conv.target)
    {
        let thread = graph
            .get_thread(team_id, channel_id, root_id, 50)
            .await?;
        let mut all = Vec::with_capacity(1 + thread.replies.len());
        all.push(thread.root);
        all.extend(thread.replies);
        let _ = storage.upsert_messages(&all);
        if let Some(found) = all.into_iter().find(|m| m.msg_id == msg_id) {
            return Ok(found);
        }
    }

    let msgs = graph.list_messages(&conv.target, 50).await?;
    let _ = storage.upsert_messages(&msgs);
    msgs.into_iter()
        .find(|m| m.msg_id == msg_id)
        .with_context(|| format!("메시지를 찾을 수 없습니다: {msg_id}"))
}

pub(crate) async fn download_message_attachment(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    msg_id: &str,
    index: usize,
    thread_root_id: Option<&str>,
    dest_dir: &Path,
) -> Result<DownloadedFile> {
    let msg = find_message_with_fallback(graph, storage, conv, msg_id, thread_root_id).await?;
    if msg.attachments.is_empty() {
        bail!("이 메시지에는 첨부 파일이 없습니다: {msg_id}");
    }
    let att = msg.attachments.get(index).with_context(|| {
        format!(
            "첨부 파일 인덱스 {index} 가 범위를 벗어났습니다 (총 {}개)",
            msg.attachments.len()
        )
    })?;
    graph.download_attachment(att, dest_dir).await
}

fn format_unix_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let single = input.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_tui_without_subcommand() {
        let cli = Cli::try_parse_from(["teams"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_aliases_and_flags() {
        let cli = Cli::try_parse_from(["teams", "ls", "--limit", "20", "--refresh", "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Chats {
                limit: 20,
                refresh: true,
                json: true
            }
        ));

        let cli = Cli::try_parse_from([
            "teams",
            "msg",
            "개발팀/일반",
            "안녕하세요",
            "--subject",
            "공지",
            "--mention",
            "u1:홍길동",
            "--important",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Send {
                target,
                markdown,
                subject,
                mention,
                important,
                json,
            } => {
                assert_eq!(target, "개발팀/일반");
                assert_eq!(markdown, "안녕하세요");
                assert_eq!(subject.as_deref(), Some("공지"));
                assert_eq!(mention, vec!["u1:홍길동"]);
                assert!(important);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "file",
            "홍길동",
            "C:\\tmp\\shot.png",
            "--caption",
            "화면",
            "--thread",
            "root1",
            "--inline",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::SendFile {
                target,
                path,
                caption,
                thread,
                inline,
                json,
            } => {
                assert_eq!(target, "홍길동");
                assert_eq!(path, PathBuf::from("C:\\tmp\\shot.png"));
                assert_eq!(caption.as_deref(), Some("화면"));
                assert_eq!(thread.as_deref(), Some("root1"));
                assert!(inline);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_serve_and_mcp_aliases() {
        let cli = Cli::try_parse_from([
            "teams",
            "api",
            "--bind",
            "0.0.0.0",
            "--port",
            "9000",
            "--token",
            "sec",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Serve { bind, port, token } => {
                assert_eq!(bind, "0.0.0.0");
                assert_eq!(port, 9000);
                assert_eq!(token.as_deref(), Some("sec"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["teams", "mcp-server"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Command::Mcp));
    }

    #[test]
    fn parses_mentions_and_rejects_malformed() {
        let parsed =
            parse_mentions_cli(&["user-1:Alice".to_string(), " user-2 : Bob Kim ".to_string()])
                .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, 0);
        assert_eq!(parsed[0].user_id, "user-1");
        assert_eq!(parsed[0].display_name, "Alice");
        assert_eq!(parsed[1].id, 1);
        assert_eq!(parsed[1].user_id, "user-2");
        assert_eq!(parsed[1].display_name, "Bob Kim");

        for bad in ["nocolon", ":noname", "nouid:", "  :  "] {
            assert!(parse_mention_spec(bad, 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn normalizes_reactions_and_presence() {
        assert_eq!(normalize_reaction("LIKE").unwrap(), "like");
        assert_eq!(normalize_reaction("Heart").unwrap(), "heart");
        assert_eq!(normalize_reaction("👍").unwrap(), "👍");
        assert!(normalize_reaction("  ").is_err());
        assert!(normalize_reaction("bogus").is_err());

        assert_eq!(
            normalize_presence_availability("donotdisturb").unwrap(),
            "DoNotDisturb"
        );
        assert_eq!(normalize_presence_availability("brb").unwrap(), "BeRightBack");
        assert_eq!(normalize_presence_availability("reset").unwrap(), "Reset");
        assert!(normalize_presence_availability("unknown").is_err());
    }

    #[test]
    fn delete_gate_requires_yes_or_confirmation() {
        delete_gate(true, false, None).unwrap();
        delete_gate(true, true, None).unwrap();
        assert!(delete_gate(false, true, Some("yes")).is_err());
        delete_gate(false, false, Some("YES")).unwrap();
        assert!(delete_gate(false, false, Some("no")).is_err());
        assert!(delete_gate(false, false, None).is_err());
    }
}
