//! `teams` / `outlook` / `onenote` tri-persona CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::{
    auth::{AuthManager, LoginOptions, StoredTokens},
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    img_preview,
    models::{
        split_onenote_entity_key, AttachmentInfo, CalendarEvent, ContactItem, ConversationKind,
        ConversationSummary, ConversationTarget, DownloadedFile, InlineImageUpload, MailAttachment,
        MailFolder, MailMessage, MentionInfo, MessageItem, OnenoteBlock, OnenoteBlockKind,
        OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenotePatchCommand,
        OnenoteResource, OnenoteScope, OnenoteSearchHit, OnenoteSection, OnenoteTagKind,
        OutgoingContact, OutgoingEvent, OutgoingMail, OutgoingOnenotePage, OutgoingTodoTask,
        QuotedReply, RichSpan, TeamSummary, TodoList,
    },
    parser, resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    sync, tui, updater,
};

const DELETE_JSON_REQUIRES_YES: &str = "--json 모드에서 삭제하려면 --yes 를 함께 지정해야 합니다.";

const ENV_AFTER_HELP: &str = "\
환경 변수 (Environment Variables):
  TEAMS_PERSONA=teams|outlook|onenote   실행 페르소나 강제 지정
  TEAMS_TOKEN_FILE=<PATH>               외부 토큰 파일(JWT 또는 JSON) 직접 로드 (키링 자동 생략)
  TEAMS_TOKEN_REFRESH_CMD=<CMD>         토큰 만료 임박 시 자동 실행할 갱신 커맨드 훅
  TEAMS_TOKEN_REFRESH_MINS=<MINS>       갱신 훅 실행 기준 남은 시간(분) [기본: 5]
  TEAMS_ACCESS_TOKEN=<JWT>              직접 Bearer 액세스 토큰 주입 (키링 자동 생략)
  TEAMS_DISABLE_KEYRING=1               OS 키링 비활성화 (credentials.json 파일 전용)
  TEAMS_CLIENT_ID / TEAMS_TENANT_ID     Entra 앱 Client ID / Tenant ID 재정의
  TEAMS_CONFIG_DIR / TEAMS_DATA_DIR     설정 및 로컬 SQLite 캐시 디렉터리 재정의";

#[derive(Parser, Debug)]
#[command(
    name = "teams",
    version,
    about = "Microsoft Teams · Outlook · OneNote 터미널 클라이언트 (CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 기반 Microsoft Teams · Outlook · OneNote 터미널 클라이언트입니다.\n\
                  인자 없이 실행하면 로그인 상태를 확인한 뒤 TUI를 엽니다.",
    after_help = ENV_AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Parser, Debug)]
#[command(
    name = "outlook",
    version,
    about = "Microsoft Outlook 터미널 클라이언트 (메일 · 캘린더 · 연락처 · To Do · OneNote · CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 기반 Microsoft Outlook / M365 터미널 클라이언트입니다.\n\
                  인자 없이 실행하면 로그인 상태를 확인한 뒤 Outlook TUI를 엽니다.",
    after_help = ENV_AFTER_HELP
)]
pub struct OutlookCli {
    #[command(subcommand)]
    pub command: Option<OutlookCommand>,
}

#[derive(Parser, Debug)]
#[command(
    name = "onenote",
    version,
    about = "Microsoft OneNote 터미널 클라이언트 (개인/팀 공유 필기장 · 섹션 · 페이지 · CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 기반 Microsoft OneNote 터미널 클라이언트입니다.\n\
                  인자 없이 실행하면 로그인 상태를 확인한 뒤 OneNote TUI를 엽니다.",
    after_help = ENV_AFTER_HELP
)]
pub struct OnenoteCli {
    #[command(subcommand)]
    pub command: Option<OnenoteCommand>,
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
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
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
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
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
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
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
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 채팅방/팀/채널 목록과 최근 메시지를 로컬 SQLite 캐시에 동기화합니다
    Sync {
        #[arg(long)]
        target: Option<String>,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
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
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 200)]
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
    /// Outlook 메일 명령 (`teams mail <subcmd>`)
    Mail {
        #[command(subcommand)]
        command: MailCommand,
    },
    /// 받은편지함 또는 지정한 폴더의 메일 목록을 표시합니다 (teams mail list 단축 명령)
    Inbox {
        #[arg(long, default_value = "inbox")]
        folder: String,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Outlook 메일 검색 (`teams mail search` 단축 명령)
    #[command(name = "mail-search")]
    MailSearch {
        query: String,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        remote: bool,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Outlook 캘린더 명령 (`teams cal <subcmd>`)
    Cal {
        #[command(subcommand)]
        command: CalCommand,
    },
    /// Outlook 연락처 및 조직 디렉터리 명령 (`teams contacts <subcmd>`)
    Contacts {
        #[command(subcommand)]
        command: ContactsCommand,
    },
    /// Microsoft To Do 할 일 명령 (`teams todo <subcmd>`)
    Todo {
        #[command(subcommand)]
        command: TodoCommand,
    },
    /// Microsoft OneNote 필기장·섹션·페이지 명령 (`teams notes <subcmd>`)
    #[command(visible_alias = "onenote")]
    Notes {
        #[command(subcommand)]
        command: NotesCommand,
    },
    /// 메시지 이미지 첨부 또는 로컬 이미지를 터미널에 미리보기 렌더링합니다
    Preview {
        target: Option<String>,
        msg_id: Option<String>,
        #[arg(long)]
        attachment: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// 로컬 이미지 파일을 터미널에 미리보기 렌더링합니다
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// GitHub Releases에서 최신 버전을 확인하거나 자가 업데이트합니다
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
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

#[derive(Subcommand, Debug)]
pub enum OutlookCommand {
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
    /// 설정/DB/키링 경로, 스코프, 캐시 카운트 및 선택적 Graph 연결 진단을 표시합니다
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// Outlook 메일 폴더 목록을 표시합니다
    Folders {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 받은편지함 또는 지정한 폴더의 메일 목록을 표시합니다
    #[command(visible_alias = "list")]
    Inbox {
        #[arg(long, default_value = "inbox")]
        folder: String,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일 본문과 첨부파일 정보를 읽습니다
    #[command(visible_alias = "cat")]
    Read {
        mail_id: String,
        #[arg(long)]
        offline: bool,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long = "no-mark-read")]
        no_mark_read: bool,
        #[arg(long)]
        json: bool,
    },
    /// Markdown 본문으로 새 메일을 발송합니다
    Send {
        #[arg(long = "to", required = true, value_name = "email")]
        to: Vec<String>,
        #[arg(long = "cc", value_name = "email")]
        cc: Vec<String>,
        #[arg(long = "bcc", value_name = "email")]
        bcc: Vec<String>,
        #[arg(long, required = true)]
        subject: String,
        markdown_body: String,
        #[arg(long = "attach", value_name = "path")]
        attach: Vec<PathBuf>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일에 답장합니다
    Reply {
        mail_id: String,
        markdown_comment: String,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 전달합니다
    Forward {
        mail_id: String,
        #[arg(long = "to", required = true, value_name = "email")]
        to: Vec<String>,
        #[arg(default_value = "")]
        markdown_comment: String,
        #[arg(long)]
        json: bool,
    },
    /// 메일에 플래그를 설정하거나 해제합니다
    Flag {
        mail_id: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 읽음 또는 읽지 않음으로 표시합니다
    #[command(name = "mark-read")]
    MarkRead {
        mail_id: String,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 다른 폴더로 이동합니다
    Move {
        mail_id: String,
        dest_folder: String,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 삭제합니다
    Delete {
        mail_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일 첨부파일을 다운로드하거나 터미널에 미리보기합니다
    Attachment {
        mail_id: String,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        json: bool,
    },
    /// 로컬 SQLite 및 선택적 원격 Graph에서 메일을 검색합니다
    #[command(visible_alias = "mail-search")]
    Search {
        query: String,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        remote: bool,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Outlook 메일 이미지 첨부 또는 로컬 이미지를 터미널에 미리보기 렌더링합니다
    Preview {
        mail_id: Option<String>,
        #[arg(long)]
        attachment: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// 로컬 이미지 파일을 터미널에 미리보기 렌더링합니다
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Outlook 메일 명령 (outlook mail <subcmd> — teams mail 과 동일)
    Mail {
        #[command(subcommand)]
        command: MailCommand,
    },
    /// Outlook 캘린더 명령 (`outlook cal <subcmd>`)
    Cal {
        #[command(subcommand)]
        command: CalCommand,
    },
    /// Outlook 연락처 및 조직 디렉터리 명령 (`outlook contacts <subcmd>`)
    Contacts {
        #[command(subcommand)]
        command: ContactsCommand,
    },
    /// Microsoft To Do 할 일 명령 (`outlook todo <subcmd>`)
    Todo {
        #[command(subcommand)]
        command: TodoCommand,
    },
    /// Microsoft OneNote 필기장·섹션·페이지 명령 (`outlook notes <subcmd>`)
    #[command(visible_alias = "onenote")]
    Notes {
        #[command(subcommand)]
        command: NotesCommand,
    },
    /// GitHub Releases에서 최신 버전을 확인하거나 자가 업데이트합니다
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Teams 1:1/그룹/모임 채팅 목록을 표시합니다
    Chats {
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 가입된 Teams 팀과 채널 목록을 표시합니다
    Teams {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 터미널 UI(TUI)를 실행합니다
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

#[derive(Subcommand, Debug)]
pub enum MailCommand {
    /// 메일 폴더 목록을 표시합니다
    Folders {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// 받은편지함 또는 지정한 폴더의 메일 목록을 표시합니다
    #[command(visible_alias = "inbox")]
    List {
        #[arg(long, default_value = "inbox")]
        folder: String,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일 본문을 읽습니다
    Read {
        mail_id: String,
        #[arg(long)]
        offline: bool,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long = "no-mark-read")]
        no_mark_read: bool,
        #[arg(long)]
        json: bool,
    },
    /// Markdown 본문으로 새 메일을 발송합니다
    Send {
        #[arg(long = "to", required = true, value_name = "email")]
        to: Vec<String>,
        #[arg(long = "cc", value_name = "email")]
        cc: Vec<String>,
        #[arg(long = "bcc", value_name = "email")]
        bcc: Vec<String>,
        #[arg(long, required = true)]
        subject: String,
        markdown_body: String,
        #[arg(long = "attach", value_name = "path")]
        attach: Vec<PathBuf>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일에 답장합니다
    Reply {
        mail_id: String,
        markdown_comment: String,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 전달합니다
    Forward {
        mail_id: String,
        #[arg(long = "to", required = true, value_name = "email")]
        to: Vec<String>,
        #[arg(default_value = "")]
        markdown_comment: String,
        #[arg(long)]
        json: bool,
    },
    /// 메일에 플래그를 설정하거나 해제합니다
    Flag {
        mail_id: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 읽음 또는 읽지 않음으로 표시합니다
    #[command(name = "mark-read")]
    MarkRead {
        mail_id: String,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 다른 폴더로 이동합니다
    Move {
        mail_id: String,
        dest_folder: String,
        #[arg(long)]
        json: bool,
    },
    /// 메일을 삭제합니다
    Delete {
        mail_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// 메일 첨부파일을 다운로드하거나 터미널에 미리보기합니다
    Attachment {
        mail_id: String,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        json: bool,
    },
    /// 로컬 SQLite 및 선택적 원격 Graph에서 메일을 검색합니다
    #[command(visible_alias = "mail-search")]
    Search {
        query: String,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        remote: bool,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum CalCommand {
    /// 오늘 일정을 표시합니다
    Today {
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 향후 N일(기본 7일) 일정을 표시합니다
    Week {
        #[arg(long, default_value_t = 7)]
        days: i64,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 지정한 기간의 일정을 표시합니다
    List {
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 일정 상세 정보를 표시합니다
    Show {
        event_id: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 새 일정을 생성합니다
    Create {
        #[arg(long)]
        subject: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long = "to", value_name = "email")]
        to: Vec<String>,
        #[arg(long)]
        location: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "teams-meeting")]
        teams_meeting: bool,
        #[arg(long = "all-day")]
        all_day: bool,
        #[arg(long)]
        time_zone: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 일정 초대에 응답(accept/tentative/decline)합니다
    Rsvp {
        event_id: String,
        response: String,
        #[arg(long)]
        comment: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 온라인 모임 URL을 브라우저로 열거나 출력합니다
    Join {
        event_id: String,
        #[arg(long = "print-only")]
        print_only: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 일정을 삭제합니다
    Delete {
        event_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ContactsCommand {
    /// 개인 연락처 목록을 표시합니다
    List {
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 연락처 및 조직 디렉터리를 검색합니다
    Search {
        query: String,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 새 개인 연락처를 추가합니다
    Add {
        #[arg(long)]
        name: String,
        #[arg(long = "email", value_name = "addr")]
        email: Vec<String>,
        #[arg(long)]
        mobile: Option<String>,
        #[arg(long)]
        company: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 개인 연락처를 삭제합니다
    Delete {
        contact_id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TodoCommand {
    /// To Do 작업 목록을 표시합니다
    Lists {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// To Do 할 일 항목을 표시합니다
    Tasks {
        #[arg(long, default_value = "default")]
        list: String,
        #[arg(long)]
        all: bool,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 새 To Do 할 일을 추가합니다
    Add {
        title: String,
        #[arg(long, default_value = "default")]
        list: String,
        #[arg(long)]
        due: Option<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// To Do 할 일을 완료 또는 미완료 처리합니다
    Done {
        task_id: String,
        #[arg(long, default_value = "default")]
        list: String,
        #[arg(long)]
        undo: bool,
        #[arg(long)]
        json: bool,
    },
    /// To Do 할 일을 삭제합니다
    Delete {
        task_id: String,
        #[arg(long, default_value = "default")]
        list: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum NotesCommand {
    /// 개인 및 가입된 팀/그룹의 OneNote 필기장 목록을 표시합니다
    #[command(visible_alias = "ls")]
    Notebooks {
        /// 개인(`/me/onenote`) 필기장만 조회합니다
        #[arg(long = "personal-only")]
        personal_only: bool,
        /// 팀/그룹 공유 필기장을 포함합니다 (기본 true)
        #[arg(
            long = "include-teams",
            default_value_t = true,
            default_missing_value = "true",
            num_args = 0..=1,
            require_equals = true
        )]
        include_teams: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 새 OneNote 필기장을 생성합니다
    #[command(name = "create-notebook")]
    CreateNotebook {
        name: String,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 필기장 내(또는 전체) OneNote 섹션 목록을 표시합니다
    Sections {
        notebook: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 지정한 필기장에 새 OneNote 섹션을 생성합니다
    #[command(name = "create-section")]
    CreateSection {
        notebook: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 지정한 섹션(`"필기장/섹션"` 또는 섹션 이름/키)의 페이지 목록을 표시합니다
    #[command(visible_alias = "list")]
    Pages {
        section: String,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지 본문, 블록(`element_id`/태그) 및 리소스 상세 정보를 읽습니다
    #[command(visible_alias = "cat")]
    Read {
        page: String,
        #[arg(long, default_value = "md", value_parser = ["md", "plain", "blocks", "xhtml"])]
        format: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Markdown 본문과 선택적 인라인 이미지로 새 OneNote 페이지를 생성합니다
    Create {
        section: String,
        #[arg(long, required = true)]
        title: String,
        markdown_body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        #[arg(long, value_parser = clap::value_parser!(i32).range(0..=2))]
        level: Option<i32>,
        #[arg(long)]
        json: bool,
    },
    /// 기존 OneNote 페이지 끝에 Markdown 본문 및 인라인 이미지를 추가(append)합니다
    Append {
        page: String,
        markdown_body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지의 특정 블록(`--block-id`) 또는 전체 제목/본문을 수정합니다
    Update {
        page: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "block-id")]
        block_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지의 `to-do` 체크박스 블록을 완료/미완료 토글합니다
    Check {
        page: String,
        #[arg(long = "block-id")]
        block_id: Option<String>,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지를 삭제합니다
    Delete {
        page: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// 로컬 SQLite FTS5(`onenote_fts`)에서 OneNote 페이지를 검색합니다
    Search {
        query: String,
        #[arg(long)]
        section: Option<String>,
        #[arg(long)]
        refresh: bool,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지의 이미지 또는 첨부 리소스를 다운로드하거나 터미널에 미리보기합니다
    Resource {
        page: String,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum OnenoteCommand {
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
    /// 설정/DB/키링 경로, 스코프, 캐시 카운트 및 선택적 Graph 연결 진단을 표시합니다
    Doctor {
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// 개인 및 가입된 팀/그룹의 OneNote 필기장 목록을 표시합니다
    #[command(visible_alias = "ls")]
    Notebooks {
        #[arg(long = "personal-only")]
        personal_only: bool,
        #[arg(
            long = "include-teams",
            default_value_t = true,
            default_missing_value = "true",
            num_args = 0..=1,
            require_equals = true
        )]
        include_teams: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 새 OneNote 필기장을 생성합니다
    #[command(name = "create-notebook")]
    CreateNotebook {
        name: String,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 필기장 내(또는 전체) OneNote 섹션 목록을 표시합니다
    Sections {
        notebook: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// 지정한 필기장에 새 OneNote 섹션을 생성합니다
    #[command(name = "create-section")]
    CreateSection {
        notebook: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// 지정한 섹션(`"필기장/섹션"` 또는 섹션 이름/키)의 페이지 목록을 표시합니다
    #[command(visible_alias = "list")]
    Pages {
        section: String,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지 본문, 블록(`element_id`/태그) 및 리소스 상세 정보를 읽습니다
    #[command(visible_alias = "cat")]
    Read {
        page: String,
        #[arg(long, default_value = "md", value_parser = ["md", "plain", "blocks", "xhtml"])]
        format: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Markdown 본문과 선택적 인라인 이미지로 새 OneNote 페이지를 생성합니다
    Create {
        section: String,
        #[arg(long, required = true)]
        title: String,
        markdown_body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        #[arg(long, value_parser = clap::value_parser!(i32).range(0..=2))]
        level: Option<i32>,
        #[arg(long)]
        json: bool,
    },
    /// 기존 OneNote 페이지 끝에 Markdown 본문 및 인라인 이미지를 추가(append)합니다
    Append {
        page: String,
        markdown_body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지의 특정 블록(`--block-id`) 또는 전체 제목/본문을 수정합니다
    Update {
        page: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "block-id")]
        block_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지의 `to-do` 체크박스 블록을 완료/미완료 토글합니다
    Check {
        page: String,
        #[arg(long = "block-id")]
        block_id: Option<String>,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지를 삭제합니다
    Delete {
        page: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// 로컬 SQLite FTS5(`onenote_fts`)에서 OneNote 페이지를 검색합니다
    Search {
        query: String,
        #[arg(long)]
        section: Option<String>,
        #[arg(long)]
        refresh: bool,
        /// 최대 조회 개수
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// OneNote 페이지의 이미지 또는 첨부 리소스를 다운로드하거나 터미널에 미리보기합니다
    Resource {
        page: String,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        json: bool,
    },
    /// 로컬 이미지 파일을 터미널에 미리보기 렌더링합니다
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// GitHub Releases에서 최신 버전을 확인하거나 자가 업데이트합니다
    /// (`onenote update <page>` 는 페이지 수정이므로 자가 업데이트는 `onenote self-update` / `onenote upgrade`)
    #[command(name = "self-update", visible_alias = "upgrade")]
    SelfUpdate {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 터미널 UI(TUI)를 실행합니다
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
    /// Microsoft OneNote 필기장·섹션·페이지 명령 (`onenote notes <subcmd>`)
    #[command(visible_alias = "onenote")]
    Notes {
        #[command(subcommand)]
        command: NotesCommand,
    },
    /// Outlook 메일 명령 (`onenote mail <subcmd>`)
    Mail {
        #[command(subcommand)]
        command: MailCommand,
    },
}

pub async fn run_from_env() -> Result<()> {
    let mut cfg = AppConfig::load();
    let persona = Persona::detect();
    match persona {
        Persona::Teams => {
            let cli = Cli::parse();
            Box::pin(dispatch_command(&mut cfg, persona, cli.command)).await
        }
        Persona::Outlook => {
            let cli = OutlookCli::parse();
            Box::pin(dispatch_outlook_command(&mut cfg, persona, cli.command)).await
        }
        Persona::OneNote => {
            let cli = OnenoteCli::parse();
            Box::pin(dispatch_onenote_command(&mut cfg, persona, cli.command)).await
        }
    }
}

async fn dispatch_command(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<Command>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
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
        Some(Command::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(Command::Doctor { probe, json }) => Box::pin(cmd_doctor(cfg, persona, probe, json)).await,
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
        Some(Command::Mail { command }) => Box::pin(dispatch_mail_command(cfg, command)).await,
        Some(Command::Inbox {
            folder,
            limit,
            unread,
            refresh,
            offline,
            json,
        }) => Box::pin(cmd_mail_list(cfg, &folder, limit, unread, refresh, offline, json)).await,
        Some(Command::MailSearch {
            query,
            folder,
            remote,
            limit,
            json,
        }) => Box::pin(cmd_mail_search(cfg, &query, folder.as_deref(), remote, limit, json)).await,
        Some(Command::Cal { command }) => Box::pin(dispatch_cal_command(cfg, command)).await,
        Some(Command::Contacts { command }) => Box::pin(dispatch_contacts_command(cfg, command)).await,
        Some(Command::Todo { command }) => Box::pin(dispatch_todo_command(cfg, command)).await,
        Some(Command::Notes { command }) => Box::pin(dispatch_notes_command(cfg, command)).await,
        Some(Command::Preview {
            target,
            msg_id,
            attachment,
            thread,
            file,
            cols,
            rows,
        }) => {
            Box::pin(cmd_preview_teams(
                cfg,
                target.as_deref(),
                msg_id.as_deref(),
                attachment.as_deref(),
                thread.as_deref(),
                file.as_deref(),
                cols,
                rows,
            ))
            .await
        }
        Some(Command::PreviewFile { path, cols, rows }) => {
            cmd_preview_local_file(&path, cols, rows)
        }
        Some(Command::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(Command::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
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

async fn dispatch_outlook_command(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<OutlookCommand>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(OutlookCommand::Login {
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
        Some(OutlookCommand::Logout { json }) => Box::pin(cmd_logout(cfg, json)).await,
        Some(OutlookCommand::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(OutlookCommand::Doctor { probe, json }) => Box::pin(cmd_doctor(cfg, persona, probe, json)).await,
        Some(OutlookCommand::Folders { refresh, json }) => {
            Box::pin(cmd_mail_folders(cfg, refresh, json)).await
        }
        Some(OutlookCommand::Inbox {
            folder,
            limit,
            unread,
            refresh,
            offline,
            json,
        }) => Box::pin(cmd_mail_list(cfg, &folder, limit, unread, refresh, offline, json)).await,
        Some(OutlookCommand::Read {
            mail_id,
            offline,
            preview_images,
            no_mark_read,
            json,
        }) => {
            Box::pin(cmd_mail_read(
                cfg,
                &mail_id,
                offline,
                preview_images,
                no_mark_read,
                json,
            ))
            .await
        }
        Some(OutlookCommand::Send {
            to,
            cc,
            bcc,
            subject,
            markdown_body,
            attach,
            important,
            json,
        }) => {
            Box::pin(cmd_mail_send(
                cfg,
                to,
                cc,
                bcc,
                &subject,
                &markdown_body,
                attach,
                important,
                json,
            ))
            .await
        }
        Some(OutlookCommand::Reply {
            mail_id,
            markdown_comment,
            all,
            json,
        }) => Box::pin(cmd_mail_reply(cfg, &mail_id, &markdown_comment, all, json)).await,
        Some(OutlookCommand::Forward {
            mail_id,
            to,
            markdown_comment,
            json,
        }) => Box::pin(cmd_mail_forward(cfg, &mail_id, to, &markdown_comment, json)).await,
        Some(OutlookCommand::Flag { mail_id, off, json }) => {
            Box::pin(cmd_mail_flag(cfg, &mail_id, !off, json)).await
        }
        Some(OutlookCommand::MarkRead {
            mail_id,
            unread,
            json,
        }) => Box::pin(cmd_mail_mark_read(cfg, &mail_id, !unread, json)).await,
        Some(OutlookCommand::Move {
            mail_id,
            dest_folder,
            json,
        }) => Box::pin(cmd_mail_move(cfg, &mail_id, &dest_folder, json)).await,
        Some(OutlookCommand::Delete { mail_id, yes, json }) => {
            Box::pin(cmd_mail_delete(cfg, &mail_id, yes, json)).await
        }
        Some(OutlookCommand::Attachment {
            mail_id,
            index,
            id,
            out,
            preview,
            json,
        }) => {
            Box::pin(cmd_mail_attachment(
                cfg,
                &mail_id,
                index,
                id.as_deref(),
                out.as_deref(),
                preview,
                json,
            ))
            .await
        }
        Some(OutlookCommand::Search {
            query,
            folder,
            remote,
            limit,
            json,
        }) => Box::pin(cmd_mail_search(cfg, &query, folder.as_deref(), remote, limit, json)).await,
        Some(OutlookCommand::Preview {
            mail_id,
            attachment,
            file,
            offline,
            cols,
            rows,
        }) => {
            Box::pin(cmd_preview_outlook(
                cfg,
                mail_id.as_deref(),
                attachment.as_deref(),
                file.as_deref(),
                offline,
                cols,
                rows,
            ))
            .await
        }
        Some(OutlookCommand::PreviewFile { path, cols, rows }) => {
            cmd_preview_local_file(&path, cols, rows)
        }
        Some(OutlookCommand::Mail { command }) => {
            Box::pin(dispatch_mail_command(cfg, command)).await
        }
        Some(OutlookCommand::Cal { command }) => Box::pin(dispatch_cal_command(cfg, command)).await,
        Some(OutlookCommand::Contacts { command }) => {
            Box::pin(dispatch_contacts_command(cfg, command)).await
        }
        Some(OutlookCommand::Todo { command }) => Box::pin(dispatch_todo_command(cfg, command)).await,
        Some(OutlookCommand::Notes { command }) => {
            Box::pin(dispatch_notes_command(cfg, command)).await
        }
        Some(OutlookCommand::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(OutlookCommand::Chats {
            limit,
            refresh,
            json,
        }) => Box::pin(cmd_chats(cfg, limit, refresh, json)).await,
        Some(OutlookCommand::Teams { refresh, json }) => Box::pin(cmd_teams(cfg, refresh, json)).await,
        Some(OutlookCommand::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(OutlookCommand::Serve { bind, port, token }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions { bind, port, token },
            ))
            .await
        }
        Some(OutlookCommand::Mcp) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(crate::mcp::run(cfg.clone(), graph, storage)).await
        }
    }
}

async fn dispatch_mail_command(cfg: &AppConfig, command: MailCommand) -> Result<()> {
    match command {
        MailCommand::Folders { refresh, json } => cmd_mail_folders(cfg, refresh, json).await,
        MailCommand::List {
            folder,
            limit,
            unread,
            refresh,
            offline,
            json,
        } => cmd_mail_list(cfg, &folder, limit, unread, refresh, offline, json).await,
        MailCommand::Read {
            mail_id,
            offline,
            preview_images,
            no_mark_read,
            json,
        } => cmd_mail_read(cfg, &mail_id, offline, preview_images, no_mark_read, json).await,
        MailCommand::Send {
            to,
            cc,
            bcc,
            subject,
            markdown_body,
            attach,
            important,
            json,
        } => cmd_mail_send(cfg, to, cc, bcc, &subject, &markdown_body, attach, important, json).await,
        MailCommand::Reply {
            mail_id,
            markdown_comment,
            all,
            json,
        } => cmd_mail_reply(cfg, &mail_id, &markdown_comment, all, json).await,
        MailCommand::Forward {
            mail_id,
            to,
            markdown_comment,
            json,
        } => cmd_mail_forward(cfg, &mail_id, to, &markdown_comment, json).await,
        MailCommand::Flag { mail_id, off, json } => cmd_mail_flag(cfg, &mail_id, !off, json).await,
        MailCommand::MarkRead {
            mail_id,
            unread,
            json,
        } => cmd_mail_mark_read(cfg, &mail_id, !unread, json).await,
        MailCommand::Move {
            mail_id,
            dest_folder,
            json,
        } => cmd_mail_move(cfg, &mail_id, &dest_folder, json).await,
        MailCommand::Delete { mail_id, yes, json } => cmd_mail_delete(cfg, &mail_id, yes, json).await,
        MailCommand::Attachment {
            mail_id,
            index,
            id,
            out,
            preview,
            json,
        } => cmd_mail_attachment(cfg, &mail_id, index, id.as_deref(), out.as_deref(), preview, json).await,
        MailCommand::Search {
            query,
            folder,
            remote,
            limit,
            json,
        } => cmd_mail_search(cfg, &query, folder.as_deref(), remote, limit, json).await,
    }
}

async fn dispatch_cal_command(cfg: &AppConfig, command: CalCommand) -> Result<()> {
    match command {
        CalCommand::Today { offline, json } => cmd_cal_today(cfg, offline, json).await,
        CalCommand::Week {
            days,
            offline,
            json,
        } => cmd_cal_week(cfg, days, offline, json).await,
        CalCommand::List {
            start,
            end,
            limit,
            offline,
            json,
        } => cmd_cal_list(cfg, start.as_deref(), end.as_deref(), limit, offline, json).await,
        CalCommand::Show {
            event_id,
            offline,
            json,
        } => cmd_cal_show(cfg, &event_id, offline, json).await,
        CalCommand::Create {
            subject,
            start,
            end,
            to,
            location,
            body,
            teams_meeting,
            all_day,
            time_zone,
            json,
        } => {
            cmd_cal_create(
                cfg,
                &subject,
                &start,
                &end,
                to,
                location,
                body,
                teams_meeting,
                all_day,
                time_zone,
                json,
            )
            .await
        }
        CalCommand::Rsvp {
            event_id,
            response,
            comment,
            json,
        } => cmd_cal_rsvp(cfg, &event_id, &response, comment.as_deref(), json).await,
        CalCommand::Join {
            event_id,
            print_only,
            offline,
            json,
        } => cmd_cal_join(cfg, &event_id, print_only, offline, json).await,
        CalCommand::Delete {
            event_id,
            yes,
            json,
        } => cmd_cal_delete(cfg, &event_id, yes, json).await,
    }
}

async fn dispatch_contacts_command(cfg: &AppConfig, command: ContactsCommand) -> Result<()> {
    match command {
        ContactsCommand::List {
            limit,
            refresh,
            offline,
            json,
        } => cmd_contacts_list(cfg, limit, refresh, offline, json).await,
        ContactsCommand::Search {
            query,
            limit,
            offline,
            json,
        } => cmd_contacts_search(cfg, &query, limit, offline, json).await,
        ContactsCommand::Add {
            name,
            email,
            mobile,
            company,
            title,
            notes,
            json,
        } => cmd_contacts_add(cfg, &name, email, mobile, company, title, notes, json).await,
        ContactsCommand::Delete {
            contact_id,
            yes,
            json,
        } => cmd_contacts_delete(cfg, &contact_id, yes, json).await,
    }
}

async fn dispatch_todo_command(cfg: &AppConfig, command: TodoCommand) -> Result<()> {
    match command {
        TodoCommand::Lists {
            refresh,
            offline,
            json,
        } => cmd_todo_lists(cfg, refresh, offline, json).await,
        TodoCommand::Tasks {
            list,
            all,
            limit,
            refresh,
            offline,
            json,
        } => cmd_todo_tasks(cfg, &list, all, limit, refresh, offline, json).await,
        TodoCommand::Add {
            title,
            list,
            due,
            important,
            body,
            json,
        } => cmd_todo_add(cfg, &title, &list, due.as_deref(), important, body, json).await,
        TodoCommand::Done {
            task_id,
            list,
            undo,
            json,
        } => cmd_todo_done(cfg, &task_id, &list, !undo, json).await,
        TodoCommand::Delete {
            task_id,
            list,
            yes,
            json,
        } => cmd_todo_delete(cfg, &task_id, &list, yes, json).await,
    }
}

pub async fn dispatch_onenote_command(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<OnenoteCommand>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(OnenoteCommand::Login {
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
        Some(OnenoteCommand::Logout { json }) => Box::pin(cmd_logout(cfg, json)).await,
        Some(OnenoteCommand::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(OnenoteCommand::Doctor { probe, json }) => {
            Box::pin(cmd_doctor(cfg, persona, probe, json)).await
        }
        Some(OnenoteCommand::Notebooks {
            personal_only,
            include_teams,
            refresh,
            offline,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Notebooks {
                    personal_only,
                    include_teams,
                    refresh,
                    offline,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::CreateNotebook {
            name,
            group,
            site,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::CreateNotebook {
                    name,
                    group,
                    site,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Sections {
            notebook,
            refresh,
            offline,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Sections {
                    notebook,
                    refresh,
                    offline,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::CreateSection {
            notebook,
            name,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::CreateSection {
                    notebook,
                    name,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Pages {
            section,
            limit,
            refresh,
            offline,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Pages {
                    section,
                    limit,
                    refresh,
                    offline,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Read {
            page,
            format,
            preview_images,
            offline,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Read {
                    page,
                    format,
                    preview_images,
                    offline,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Create {
            section,
            title,
            markdown_body,
            file,
            image,
            level,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Create {
                    section,
                    title,
                    markdown_body,
                    file,
                    image,
                    level,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Append {
            page,
            markdown_body,
            file,
            image,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Append {
                    page,
                    markdown_body,
                    file,
                    image,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Update {
            page,
            title,
            body,
            file,
            block_id,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Update {
                    page,
                    title,
                    body,
                    file,
                    block_id,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Check {
            page,
            block_id,
            index,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Check {
                    page,
                    block_id,
                    index,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Delete { page, yes, json }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Delete { page, yes, json },
            ))
            .await
        }
        Some(OnenoteCommand::Search {
            query,
            section,
            refresh,
            limit,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Search {
                    query,
                    section,
                    refresh,
                    limit,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::Resource {
            page,
            index,
            out,
            preview,
            json,
        }) => {
            Box::pin(dispatch_notes_command(
                cfg,
                NotesCommand::Resource {
                    page,
                    index,
                    out,
                    preview,
                    json,
                },
            ))
            .await
        }
        Some(OnenoteCommand::PreviewFile { path, cols, rows }) => {
            cmd_preview_local_file(&path, cols, rows)
        }
        Some(OnenoteCommand::SelfUpdate {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(OnenoteCommand::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(OnenoteCommand::Serve { bind, port, token }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions { bind, port, token },
            ))
            .await
        }
        Some(OnenoteCommand::Mcp) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(crate::mcp::run(cfg.clone(), graph, storage)).await
        }
        Some(OnenoteCommand::Notes { command }) => {
            Box::pin(dispatch_notes_command(cfg, command)).await
        }
        Some(OnenoteCommand::Mail { command }) => {
            Box::pin(dispatch_mail_command(cfg, command)).await
        }
    }
}

pub async fn dispatch_notes_command(cfg: &AppConfig, command: NotesCommand) -> Result<()> {
    match command {
        NotesCommand::Notebooks {
            personal_only,
            include_teams,
            refresh,
            offline,
            json,
        } => {
            cmd_notes_notebooks(
                cfg,
                !personal_only && include_teams,
                refresh,
                offline,
                json,
            )
            .await
        }
        NotesCommand::CreateNotebook {
            name,
            group,
            site,
            json,
        } => cmd_notes_create_notebook(cfg, &name, group.as_deref(), site.as_deref(), json).await,
        NotesCommand::Sections {
            notebook,
            refresh,
            offline,
            json,
        } => cmd_notes_sections(cfg, notebook.as_deref(), refresh, offline, json).await,
        NotesCommand::CreateSection {
            notebook,
            name,
            json,
        } => cmd_notes_create_section(cfg, &notebook, &name, json).await,
        NotesCommand::Pages {
            section,
            limit,
            refresh,
            offline,
            json,
        } => cmd_notes_pages(cfg, &section, limit, refresh, offline, json).await,
        NotesCommand::Read {
            page,
            format,
            preview_images,
            offline,
            json,
        } => cmd_notes_read(cfg, &page, &format, preview_images, offline, json).await,
        NotesCommand::Create {
            section,
            title,
            markdown_body,
            file,
            image,
            level,
            json,
        } => {
            cmd_notes_create(
                cfg,
                &section,
                &title,
                markdown_body.as_deref(),
                file.as_deref(),
                &image,
                level,
                json,
            )
            .await
        }
        NotesCommand::Append {
            page,
            markdown_body,
            file,
            image,
            json,
        } => {
            cmd_notes_append(
                cfg,
                &page,
                markdown_body.as_deref(),
                file.as_deref(),
                &image,
                json,
            )
            .await
        }
        NotesCommand::Update {
            page,
            title,
            body,
            file,
            block_id,
            json,
        } => {
            cmd_notes_update(
                cfg,
                &page,
                title.as_deref(),
                body.as_deref(),
                file.as_deref(),
                block_id.as_deref(),
                json,
            )
            .await
        }
        NotesCommand::Check {
            page,
            block_id,
            index,
            json,
        } => cmd_notes_check(cfg, &page, block_id.as_deref(), index, json).await,
        NotesCommand::Delete { page, yes, json } => cmd_notes_delete(cfg, &page, yes, json).await,
        NotesCommand::Search {
            query,
            section,
            refresh,
            limit,
            json,
        } => cmd_notes_search(cfg, &query, section.as_deref(), refresh, limit, json).await,
        NotesCommand::Resource {
            page,
            index,
            out,
            preview,
            json,
        } => cmd_notes_resource(cfg, &page, index, out.as_deref(), preview, json).await,
    }
}

async fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    if require_login && tokens.is_none() {
        bail!(
            "로그인이 필요합니다. 먼저 `{} login` 을 실행하세요.",
            Persona::detect().bin_name()
        );
    }
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let storage = Storage::open(&config::db_path(user_id))?;
    let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
    if let Some(uid) = user_id {
        graph = graph.with_my_user_id(uid);
    }
    Ok((auth, graph, storage))
}

async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    if auth.current_tokens().await.is_none() {
        println!("저장된 로그인 정보가 없습니다. Microsoft 계정 로그인을 시작합니다.");
        println!(
            "(자체 Azure App 사용 시: {} login --client-id <APP_ID> --tenant-id <TENANT_ID>)",
            persona.bin_name()
        );
        cmd_login(cfg, LoginOptions::default(), false).await?;
        println!();
    }

    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    if storage.list_conversations()?.is_empty() {
        let _ = sync::bootstrap_conversations(&graph, &storage).await;
    }
    if persona == Persona::Outlook && storage.list_mail_folders()?.is_empty() {
        let _ = sync::bootstrap_outlook(&graph, &storage).await;
    }
    if persona == Persona::OneNote && storage.list_onenote_notebooks()?.is_empty() {
        let _ = sync::bootstrap_onenote(&graph, &storage).await;
    }
    tui::run_with_persona(cfg.clone(), persona, auth, graph, storage).await
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

async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let mut tokens = match auth.current_tokens().await {
        Some(t) => t,
        None => {
            if let Some(ref tf) = cfg.resolved_token_file() {
                if !tf.exists() {
                    bail!(
                        "로그인이 필요합니다. 먼저 `{} login` 을 실행하거나 `TEAMS_TOKEN_FILE` / `TEAMS_ACCESS_TOKEN` 을 설정하세요. (설정된 TEAMS_TOKEN_FILE 경로를 찾을 수 없음: {})",
                        persona.bin_name(),
                        tf.display()
                    );
                }
                bail!(
                    "로그인이 필요합니다. 먼저 `{} login` 을 실행하거나 유효한 `TEAMS_TOKEN_FILE` ({}) / `TEAMS_ACCESS_TOKEN` 을 설정하세요.",
                    persona.bin_name(),
                    tf.display()
                );
            }
            bail!(
                "로그인이 필요합니다. 먼저 `{} login` 을 실행하거나 `TEAMS_TOKEN_FILE` / `TEAMS_ACCESS_TOKEN` 을 설정하세요.",
                persona.bin_name()
            );
        }
    };

    let refresh_skew_secs = (cfg.token_refresh_mins.max(1) * 60) as i64;
    let mut refresh_failure: Option<anyhow::Error> = None;
    if tokens.is_expired_soon(refresh_skew_secs) && auth.is_refreshable().await {
        match auth.refresh_now().await {
            Ok(refreshed) => {
                tokens = refreshed;
            }
            Err(refresh_err) => {
                if tokens.is_expired_soon(0) {
                    refresh_failure = Some(refresh_err);
                } else {
                    eprintln!(
                        "경고: 토큰 자동 갱신 실패 ({refresh_err}); 기존 토큰(만료: {})으로 계속 조회합니다.",
                        format_unix_ts(tokens.expires_at)
                    );
                }
            }
        }
    }

    if tokens.is_expired_soon(0) {
        let err_detail = match refresh_failure {
            Some(ref err) => format!(
                "토큰 만료 (exp: {}, auth_mode={}) — 자동 갱신 실패: {err:#}",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode
            ),
            None => format!(
                "토큰 만료 (exp: {}, auth_mode={}) — 토큰을 갱신(refresh)한 뒤 재시도하세요.",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode
            ),
        };
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "persona": persona,
                    "user_id": tokens.user_id,
                    "display_name": tokens.user_display_name,
                    "email": tokens.user_email,
                    "client_id": tokens.client_id,
                    "tenant_id": tokens.tenant_id,
                    "auth_mode": tokens.auth_mode,
                    "expires_at": tokens.expires_at,
                    "expires_at_human": format_unix_ts(tokens.expires_at),
                    "expired": true,
                    "expiring_soon": true,
                    "status": "expired",
                    "error": format!("토큰 만료 (exp: {}) — refresh 후 재시도하세요", format_unix_ts(tokens.expires_at)),
                }))?
            );
            bail!("토큰 만료 (exp: {}) — 토큰을 갱신한 뒤 재시도하세요.", format_unix_ts(tokens.expires_at));
        }
        bail!("{err_detail}");
    }

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let mut profile_err: Option<anyhow::Error> = None;
    let live_profile = if tokens.user_id.is_none() || tokens.user_display_name.is_none() {
        match graph.get_me().await {
            Ok(p) => Some(p),
            Err(e) => {
                profile_err = Some(e);
                None
            }
        }
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

    if display_name.is_none() && user_id.is_none() {
        if let Some(err) = profile_err {
            bail!(
                "프로필 조회(/me) 실패 (auth_mode={}, 만료 시각: {}): {err:#}",
                tokens.auth_mode,
                format_unix_ts(tokens.expires_at)
            );
        }
    }

    let expiring_soon =
        !tokens.is_expired_soon(0) && tokens.is_expired_soon(refresh_skew_secs);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "persona": persona,
                "user_id": user_id,
                "display_name": display_name,
                "email": email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": false,
                "expiring_soon": expiring_soon,
                "status": if expiring_soon { "expiring_soon" } else { "valid" },
            }))?
        );
    } else {
        println!("persona: {}", persona.bin_name());
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
            if expiring_soon {
                "만료 임박 — 갱신 권장"
            } else {
                "유효"
            }
        );
    }
    Ok(())
}

async fn cmd_doctor(
    cfg: &AppConfig,
    persona: Persona,
    probe: bool,
    json_out: bool,
) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());

    let cfg_path = config::config_file_path();
    let cred_path = config::credential_fallback_path();
    let drafts = config::drafts_path();
    let db = config::db_path(user_id);
    let dl_dir = cfg.resolved_download_dir();
    let scopes = cfg.effective_scopes();
    let token_file = cfg.resolved_token_file();
    let token_refresh_cmd_configured = !cfg.token_refresh_cmd.trim().is_empty();
    let auth_mode = tokens.as_ref().map(|t| t.auth_mode.clone());
    let token_expires_at = tokens.as_ref().map(|t| format_unix_ts(t.expires_at));

    let keyring_status = describe_keyring_status(tokens.as_ref(), &cred_path);

    let (
        cached_conversations,
        cached_mail_folders,
        cached_mails,
        cached_events,
        cached_contacts,
        cached_todo_lists,
        cached_onenote_notebooks,
        cached_onenote_sections,
    ) = if db.exists() {
        if let Ok(storage) = Storage::open(&db) {
            (
                storage.list_conversations().map(|v| v.len()).unwrap_or(0),
                storage.list_mail_folders().map(|v| v.len()).unwrap_or(0),
                storage
                    .list_mail_messages(None, 100_000)
                    .map(|v| v.len())
                    .unwrap_or(0),
                storage
                    .list_calendar_events(None, None, 100_000)
                    .map(|v| v.len())
                    .unwrap_or(0),
                storage
                    .list_contacts(None, 100_000)
                    .map(|v| v.len())
                    .unwrap_or(0),
                storage.list_todo_lists().map(|v| v.len()).unwrap_or(0),
                storage
                    .list_onenote_notebooks()
                    .map(|v| v.len())
                    .unwrap_or(0),
                storage
                    .list_onenote_sections(None)
                    .map(|v| v.len())
                    .unwrap_or(0),
            )
        } else {
            (0, 0, 0, 0, 0, 0, 0, 0)
        }
    } else {
        (0, 0, 0, 0, 0, 0, 0, 0)
    };

    let mut probe_me: Option<serde_json::Value> = None;
    let mut probe_presence: Option<serde_json::Value> = None;
    let mut probe_onenote: Option<serde_json::Value> = None;
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
        // Non-fatal OneNote probe (/me/onenote/notebooks?$top=1)
        probe_onenote = Some(
            match graph
                .get_json::<serde_json::Value>("/me/onenote/notebooks?$top=1")
                .await
            {
                Ok(val) => {
                    let count = val
                        .get("value")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    json!({
                        "ok": true,
                        "sampled_notebooks": count,
                    })
                }
                Err(err) => json!({
                    "ok": false,
                    "error": format!("{err:#}"),
                }),
            },
        );
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "persona": persona,
                "scopes": scopes,
                "config_path": cfg_path,
                "config_exists": cfg_path.exists(),
                "credentials_fallback_path": cred_path,
                "credentials_fallback_exists": cred_path.exists(),
                "drafts_path": drafts,
                "db_path": db,
                "db_exists": db.exists(),
                "download_dir": dl_dir,
                "github_repo": cfg.github_repo,
                "client_id": cfg.client_id,
                "tenant_id": cfg.tenant_id,
                "graph_base_url": cfg.graph_base_url,
                "auth_base_url": cfg.auth_base_url,
                "keyring_status": keyring_status,
                "token_file": token_file,
                "token_file_exists": token_file.as_ref().map(|p| p.exists()),
                "token_refresh_cmd_configured": token_refresh_cmd_configured,
                "token_refresh_mins": cfg.token_refresh_mins,
                "auth_mode": auth_mode,
                "token_expires_at": token_expires_at,
                "logged_in": tokens.is_some(),
                "cache_counts": {
                    "conversations": cached_conversations,
                    "mail_folders": cached_mail_folders,
                    "mails": cached_mails,
                    "events": cached_events,
                    "contacts": cached_contacts,
                    "todo_lists": cached_todo_lists,
                    "onenote_notebooks": cached_onenote_notebooks,
                    "onenote_sections": cached_onenote_sections,
                },
                "probe": if probe {
                    Some(json!({
                        "ok": probe_error.is_none(),
                        "me": probe_me,
                        "presence": probe_presence,
                        "onenote": probe_onenote,
                        "error": probe_error,
                    }))
                } else {
                    None
                },
            }))?
        );
    } else {
        println!("{} v{} (persona={})", persona.bin_name(), env!("CARGO_PKG_VERSION"), persona.bin_name());
        println!("config.toml: {} (exists={})", cfg_path.display(), cfg_path.exists());
        println!(
            "credentials.json: {} (exists={})",
            cred_path.display(),
            cred_path.exists()
        );
        println!("drafts.json: {}", drafts.display());
        println!("cache.db: {} (exists={})", db.display(), db.exists());
        println!(
            "cache_counts: conversations={cached_conversations}, mail_folders={cached_mail_folders}, mails={cached_mails}, events={cached_events}, contacts={cached_contacts}, todo_lists={cached_todo_lists}, onenote_notebooks={cached_onenote_notebooks}, onenote_sections={cached_onenote_sections}"
        );
        println!("download_dir: {}", dl_dir.display());
        println!("github_repo: {}", cfg.github_repo);
        println!("scopes ({}): {}", scopes.len(), scopes.join(", "));
        println!("client_id: {}", cfg.client_id);
        println!("tenant_id: {}", cfg.tenant_id);
        println!("graph_base_url: {}", cfg.graph_base_url);
        println!("auth_base_url: {}", cfg.auth_base_url);
        println!("keyring_status: {keyring_status}");
        println!(
            "token_file: {}",
            token_file
                .as_ref()
                .map(|p| format!("{} (exists={})", p.display(), p.exists()))
                .unwrap_or_else(|| "(미설정)".to_string())
        );
        println!(
            "token_refresh_cmd_configured: {token_refresh_cmd_configured} (token_refresh_mins={})",
            cfg.token_refresh_mins
        );
        println!("auth_mode: {}", auth_mode.as_deref().unwrap_or("-"));
        println!(
            "token_expires_at: {}",
            token_expires_at.as_deref().unwrap_or("-")
        );
        if probe {
            let onenote_state = probe_onenote
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool())
                .map(|ok| if ok { "ok" } else { "unavailable" })
                .unwrap_or("-");
            if let Some(err) = probe_error {
                println!("probe: 실패 ({err}, onenote={onenote_state})");
            } else {
                println!(
                    "probe: 성공 (me={}, presence={}, onenote={onenote_state})",
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
        Some(tok) if tok.auth_mode == "token_file" => {
            "token_file (TEAMS_TOKEN_FILE 사용 중)".to_string()
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

// ---------------------------------------------------------------------------
// Update & Terminal Image Preview commands
// ---------------------------------------------------------------------------

async fn cmd_update(
    cfg: &AppConfig,
    check: bool,
    force: bool,
    repo_override: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let repo = repo_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.github_repo);
    let result = updater::check_or_apply_update(repo, check, force).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("저장소: {repo}");
        println!("현재 버전: v{}", result.current_version);
        println!("최신 버전: v{}", result.latest_version);
        if result.updated {
            println!(
                "업데이트 완료: {} ({})",
                result.binary_path.display(),
                result.asset_name.as_deref().unwrap_or("-")
            );
        } else if result.update_available {
            println!("새 버전이 있습니다: {}", result.release_url);
            if check {
                println!("--check 모드이므로 바이너리를 교체하지 않았습니다.");
            }
        } else {
            println!("이미 최신 버전입니다.");
        }
    }
    Ok(())
}

pub(crate) fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let c = cols.unwrap_or_else(|| term_cols.clamp(20, 120));
    let r = rows.unwrap_or_else(|| term_rows.saturating_sub(4).clamp(10, 40));
    (c, r)
}

fn cmd_preview_local_file(path: &Path, cols: Option<u16>, rows: Option<u16>) -> Result<()> {
    if !path.exists() || !path.is_file() {
        bail!("이미지 파일을 찾을 수 없습니다: {}", path.display());
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("이미지 파일 읽기 실패: {}", path.display()))?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout(&bytes, c, r)?;
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_preview_teams(
    cfg: &AppConfig,
    target: Option<&str>,
    msg_id: Option<&str>,
    attachment: Option<&str>,
    thread: Option<&str>,
    file: Option<&Path>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    if let Some(path) = file {
        return cmd_preview_local_file(path, cols, rows);
    }
    let target = target.context("대상 대화방(<target>) 또는 --file <path> 를 지정하세요.")?;
    let msg_id = msg_id.context("메시지 ID(<msg_id>) 또는 --file <path> 를 지정하세요.")?;

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let msg = find_message_with_fallback(&graph, &storage, &conv, msg_id, thread).await?;
    let att = select_teams_attachment(&msg, attachment)?;
    let (_name, _ct, bytes) = graph.fetch_attachment_bytes(att).await?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout(&bytes, c, r)?;
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

async fn cmd_preview_outlook(
    cfg: &AppConfig,
    mail_id: Option<&str>,
    attachment: Option<&str>,
    file: Option<&Path>,
    offline: bool,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    if let Some(path) = file {
        return cmd_preview_local_file(path, cols, rows);
    }
    let mail_id = mail_id.context("메일 ID(<mail_id>) 또는 --file <path> 를 지정하세요.")?;
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mail = load_mail_message(&storage, &graph, mail_id, offline).await?;
    let att = select_mail_attachment(&mail, None, attachment)?;
    let (_name, _ct, bytes) = load_mail_attachment_bytes(&graph, &mail.id, att, offline).await?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout(&bytes, c, r)?;
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

pub(crate) fn select_teams_attachment<'a>(
    msg: &'a MessageItem,
    selector: Option<&str>,
) -> Result<&'a AttachmentInfo> {
    if msg.attachments.is_empty() {
        bail!("이 메시지에는 첨부 파일이 없습니다: {}", msg.msg_id);
    }
    if let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(idx) = sel.parse::<usize>() {
            return msg.attachments.get(idx).with_context(|| {
                format!(
                    "첨부 파일 인덱스 {idx} 가 범위를 벗어났습니다 (총 {}개)",
                    msg.attachments.len()
                )
            });
        }
        if let Some(found) = msg.attachments.iter().find(|a| {
            a.id == sel
                || a.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(sel))
        }) {
            return Ok(found);
        }
        bail!("일치하는 첨부 파일을 찾을 수 없습니다: {sel}");
    }
    Ok(msg
        .attachments
        .iter()
        .find(|a| img_preview::is_image_mime_or_name(&a.content_type, a.name.as_deref()))
        .unwrap_or(&msg.attachments[0]))
}

pub(crate) fn select_mail_attachment<'a>(
    mail: &'a MailMessage,
    index: Option<usize>,
    selector: Option<&str>,
) -> Result<&'a MailAttachment> {
    if mail.attachments.is_empty() {
        bail!("이 메일에는 첨부 파일이 없습니다: {}", mail.id);
    }
    if let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(idx) = sel.parse::<usize>() {
            return mail.attachments.get(idx).with_context(|| {
                format!(
                    "첨부 파일 인덱스 {idx} 가 범위를 벗어났습니다 (총 {}개)",
                    mail.attachments.len()
                )
            });
        }
        if let Some(found) = mail
            .attachments
            .iter()
            .find(|a| a.id == sel || a.name.eq_ignore_ascii_case(sel))
        {
            return Ok(found);
        }
        bail!("일치하는 메일 첨부 파일을 찾을 수 없습니다: {sel}");
    }
    if let Some(idx) = index {
        return mail.attachments.get(idx).with_context(|| {
            format!(
                "첨부 파일 인덱스 {idx} 가 범위를 벗어났습니다 (총 {}개)",
                mail.attachments.len()
            )
        });
    }
    Ok(mail
        .attachments
        .iter()
        .find(|a| img_preview::is_image_mime_or_name(&a.content_type, Some(&a.name)))
        .unwrap_or(&mail.attachments[0]))
}

pub(crate) async fn load_mail_attachment_bytes(
    graph: &GraphClient,
    mail_id: &str,
    att: &MailAttachment,
    offline: bool,
) -> Result<(String, String, Vec<u8>)> {
    if let Some(ref b64) = att.content_bytes_b64 {
        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = BASE64_STANDARD
            .decode(cleaned)
            .with_context(|| format!("첨부 파일 base64 디코딩 실패: {}", att.name))?;
        return Ok((att.name.clone(), att.content_type.clone(), bytes));
    }
    if offline {
        bail!("오프라인 모드에서는 캐시되지 않은 첨부 파일을 가져올 수 없습니다: {}", att.name);
    }
    graph.fetch_mail_attachment_bytes(mail_id, &att.id).await
}

// ---------------------------------------------------------------------------
// Outlook Mail commands & shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn resolve_mail_folder_from_slice(folders: &[MailFolder], query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if let Some(f) = folders.iter().find(|f| f.id == q) {
        return Some(f.id.clone());
    }
    let normalized = match q.to_ascii_lowercase().as_str() {
        "sent" => "sentitems",
        "trash" | "deleted" => "deleteditems",
        "junk" | "spam" => "junkemail",
        other => {
            if let Some(f) = folders.iter().find(|f| {
                f.well_known_name
                    .as_deref()
                    .is_some_and(|wk| wk.eq_ignore_ascii_case(other))
                    || f.display_name.eq_ignore_ascii_case(q)
            }) {
                return Some(f.id.clone());
            }
            return None;
        }
    };
    folders
        .iter()
        .find(|f| {
            f.well_known_name
                .as_deref()
                .is_some_and(|wk| wk.eq_ignore_ascii_case(normalized))
                || f.display_name.eq_ignore_ascii_case(q)
        })
        .map(|f| f.id.clone())
}

pub(crate) async fn resolve_mail_folder_id(
    storage: &Storage,
    graph: &GraphClient,
    folder: &str,
    offline: bool,
) -> Result<String> {
    let q = folder.trim();
    if q.is_empty() {
        return Ok("inbox".to_string());
    }
    let cached = storage.list_mail_folders()?;
    if let Some(id) = resolve_mail_folder_from_slice(&cached, q) {
        return Ok(id);
    }
    if !offline && cached.is_empty() {
        if let Ok(fetched) = graph.list_mail_folders().await {
            let _ = storage.upsert_mail_folders(&fetched);
            if let Some(id) = resolve_mail_folder_from_slice(&fetched, q) {
                return Ok(id);
            }
        }
    }
    Ok(match q.to_ascii_lowercase().as_str() {
        "sent" => "sentitems".to_string(),
        "trash" | "deleted" => "deleteditems".to_string(),
        "junk" | "spam" => "junkemail".to_string(),
        _ => q.to_string(),
    })
}

async fn load_mail_message(
    storage: &Storage,
    graph: &GraphClient,
    mail_id: &str,
    offline: bool,
) -> Result<MailMessage> {
    if offline {
        return storage
            .get_mail_message(mail_id)?
            .with_context(|| format!("로컬 캐시에서 메일을 찾을 수 없습니다: {mail_id}"));
    }
    let msg = graph.get_mail_message(mail_id).await?;
    let _ = storage.upsert_mail_messages(std::slice::from_ref(&msg));
    Ok(msg)
}

async fn cmd_mail_folders(cfg: &AppConfig, refresh: bool, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let cached = storage.list_mail_folders()?;
    let folders = if refresh || cached.is_empty() {
        let fetched = graph.list_mail_folders().await?;
        storage.upsert_mail_folders(&fetched)?;
        let stored = storage.list_mail_folders()?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "folders": folders }))?
        );
    } else if folders.is_empty() {
        println!("메일 폴더가 없습니다.");
    } else {
        for f in &folders {
            let wk = f
                .well_known_name
                .as_deref()
                .map(|w| format!(" [{w}]"))
                .unwrap_or_default();
            println!(
                "{}{}  unread={}/{}  ({})",
                f.display_name, wk, f.unread_item_count, f.total_item_count, f.id
            );
        }
    }
    Ok(())
}

async fn cmd_mail_list(
    cfg: &AppConfig,
    folder: &str,
    limit: usize,
    unread_only: bool,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let folder_id = resolve_mail_folder_id(&storage, &graph, folder, offline).await?;

    let cached = storage.list_mail_messages(Some(&folder_id), limit)?;
    let mut messages = if !offline && (refresh || cached.is_empty()) {
        let fetched = graph.list_mail_messages(&folder_id, limit).await?;
        storage.upsert_mail_messages(&fetched)?;
        let stored = storage.list_mail_messages(Some(&folder_id), limit)?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };

    if unread_only {
        messages.retain(|m| !m.is_read);
    }
    messages.truncate(limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "folder": folder_id,
                "messages": messages,
            }))?
        );
    } else if messages.is_empty() {
        println!("표시할 메일이 없습니다 ({folder}).");
    } else {
        for m in &messages {
            let unread_mark = if m.is_read { " " } else { "*" };
            let flag_mark = if m.flagged { "⚑" } else { " " };
            let att_mark = if m.has_attachments { "📎" } else { "  " };
            println!(
                "{unread_mark}{flag_mark} {att_mark} [{}] {} | {} ({})",
                m.received_at,
                m.from_display(),
                truncate_single_line(&m.subject, 60),
                m.id
            );
        }
    }
    Ok(())
}

async fn cmd_mail_read(
    cfg: &AppConfig,
    mail_id: &str,
    offline: bool,
    preview_images: bool,
    no_mark_read: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let mut msg = load_mail_message(&storage, &graph, mail_id, offline).await?;

    if !offline && !no_mark_read && !msg.is_read && graph.set_mail_read(&msg.id, true).await.is_ok()
    {
        msg.is_read = true;
        let _ = storage.upsert_mail_messages(std::slice::from_ref(&msg));
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "message": msg }))?
        );
        return Ok(());
    }

    println!("제목: {}", msg.subject);
    println!("보낸사람: {}", msg.from_display());
    if !msg.to_recipients.is_empty() {
        let to_list: Vec<String> = msg.to_recipients.iter().map(|r| r.display()).collect();
        println!("받는사람: {}", to_list.join(", "));
    }
    if !msg.cc_recipients.is_empty() {
        let cc_list: Vec<String> = msg.cc_recipients.iter().map(|r| r.display()).collect();
        println!("참조: {}", cc_list.join(", "));
    }
    println!(
        "수신일: {} | 읽음={} | 플래그={} | 중요도={}",
        msg.received_at, msg.is_read, msg.flagged, msg.importance
    );
    if !msg.attachments.is_empty() {
        println!("첨부 ({}개):", msg.attachments.len());
        for (idx, att) in msg.attachments.iter().enumerate() {
            println!(
                "  [{idx}] {} ({}, {} bytes, id={})",
                att.name, att.content_type, att.size, att.id
            );
        }
    }
    println!("---");
    let body = if !msg.body_plain.trim().is_empty() {
        msg.body_plain.trim().to_string()
    } else {
        parser::html_and_cards_to_plain(&msg.body_html, &[])
    };
    println!("{body}");

    if preview_images {
        let (cols, rows) = default_preview_dimensions(None, None);
        for att in &msg.attachments {
            if img_preview::is_image_mime_or_name(&att.content_type, Some(&att.name)) {
                match load_mail_attachment_bytes(&graph, &msg.id, att, offline).await {
                    Ok((_name, _ct, bytes)) => match img_preview::render_ansi_stdout(&bytes, cols, rows)
                    {
                        Ok(rendered) => {
                            println!("\n[이미지 미리보기: {}]", att.name);
                            print!("{rendered}");
                            if !rendered.ends_with('\n') {
                                println!();
                            }
                        }
                        Err(err) => eprintln!("이미지 렌더링 실패 ({}): {err:#}", att.name),
                    },
                    Err(err) => eprintln!("이미지 로드 실패 ({}): {err:#}", att.name),
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_mail_send(
    cfg: &AppConfig,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: &str,
    markdown_body: &str,
    attach: Vec<PathBuf>,
    important: bool,
    json_out: bool,
) -> Result<()> {
    if markdown_body.trim().is_empty() {
        bail!("메일 본문이 비어 있습니다.");
    }
    for path in &attach {
        if !path.exists() || !path.is_file() {
            bail!("첨부 파일을 찾을 수 없습니다: {}", path.display());
        }
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let body_html =
        parser::compile_outgoing_markdown(markdown_body, None, vec![], vec![], vec![], None)
            .body_html;
    let mail = OutgoingMail {
        subject: subject.to_string(),
        body_html,
        to: to.clone(),
        cc,
        bcc,
        importance: if important {
            Some("high".to_string())
        } else {
            None
        },
        attachments: attach,
        save_to_sent_items: true,
    };
    graph.send_mail(&mail).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "to": to,
                "subject": subject,
            }))?
        );
    } else {
        println!("메일 전송 완료: '{}' -> {}", subject, to.join(", "));
    }
    Ok(())
}

async fn cmd_mail_reply(
    cfg: &AppConfig,
    mail_id: &str,
    markdown_comment: &str,
    reply_all: bool,
    json_out: bool,
) -> Result<()> {
    if markdown_comment.trim().is_empty() {
        bail!("답장 본문이 비어 있습니다.");
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let comment_html =
        parser::compile_outgoing_markdown(markdown_comment, None, vec![], vec![], vec![], None)
            .body_html;
    graph.reply_mail(mail_id, &comment_html, reply_all).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "mail_id": mail_id,
                "reply_all": reply_all,
            }))?
        );
    } else {
        println!(
            "메일 {} 완료: {mail_id}",
            if reply_all { "전체 답장" } else { "답장" }
        );
    }
    Ok(())
}

async fn cmd_mail_forward(
    cfg: &AppConfig,
    mail_id: &str,
    to: Vec<String>,
    markdown_comment: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let comment_html = if markdown_comment.trim().is_empty() {
        String::new()
    } else {
        parser::compile_outgoing_markdown(markdown_comment, None, vec![], vec![], vec![], None)
            .body_html
    };
    graph.forward_mail(mail_id, &to, &comment_html).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "mail_id": mail_id,
                "to": to,
            }))?
        );
    } else {
        println!("메일 전달 완료: {mail_id} -> {}", to.join(", "));
    }
    Ok(())
}

async fn cmd_mail_flag(
    cfg: &AppConfig,
    mail_id: &str,
    flagged: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    graph.set_mail_flag(mail_id, flagged).await?;
    if let Ok(Some(mut msg)) = storage.get_mail_message(mail_id) {
        msg.flagged = flagged;
        let _ = storage.upsert_mail_messages(&[msg]);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "mail_id": mail_id,
                "flagged": flagged,
            }))?
        );
    } else {
        println!(
            "메일 플래그 {}: {mail_id}",
            if flagged { "설정" } else { "해제" }
        );
    }
    Ok(())
}

async fn cmd_mail_mark_read(
    cfg: &AppConfig,
    mail_id: &str,
    is_read: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    graph.set_mail_read(mail_id, is_read).await?;
    if let Ok(Some(mut msg)) = storage.get_mail_message(mail_id) {
        msg.is_read = is_read;
        let _ = storage.upsert_mail_messages(&[msg]);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "mail_id": mail_id,
                "is_read": is_read,
            }))?
        );
    } else {
        println!(
            "메일 상태 변경 ({}): {mail_id}",
            if is_read { "읽음" } else { "읽지 않음" }
        );
    }
    Ok(())
}

async fn cmd_mail_move(
    cfg: &AppConfig,
    mail_id: &str,
    dest_folder: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let folder_id = resolve_mail_folder_id(&storage, &graph, dest_folder, false).await?;
    let moved = graph.move_mail_message(mail_id, &folder_id).await?;
    let _ = storage.upsert_mail_messages(std::slice::from_ref(&moved));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "message": moved,
            }))?
        );
    } else {
        println!("메일 이동 완료: {} -> {folder_id}", moved.id);
    }
    Ok(())
}

async fn cmd_mail_delete(cfg: &AppConfig, mail_id: &str, yes: bool, json_out: bool) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "메일 {mail_id} 를 삭제합니다. 계속하려면 'yes' 입력: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_mail_message(mail_id).await?;
    let _ = storage.delete_mail_messages_by_ids(&[mail_id.to_string()]);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "mail_id": mail_id,
            }))?
        );
    } else {
        println!("메일 삭제 완료: {mail_id}");
    }
    Ok(())
}

async fn cmd_mail_attachment(
    cfg: &AppConfig,
    mail_id: &str,
    index: Option<usize>,
    att_id: Option<&str>,
    out: Option<&Path>,
    preview: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let mail = load_mail_message(&storage, &graph, mail_id, false).await?;
    let idx = if att_id.is_none() {
        Some(index.unwrap_or(0))
    } else {
        index
    };
    let att = select_mail_attachment(&mail, idx, att_id)?;

    if preview && out.is_none() && !json_out {
        let (_name, _ct, bytes) = load_mail_attachment_bytes(&graph, &mail.id, att, false).await?;
        let (cols, rows) = default_preview_dimensions(None, None);
        let rendered = img_preview::render_ansi_stdout(&bytes, cols, rows)?;
        print!("{rendered}");
        if !rendered.ends_with('\n') {
            println!();
        }
        io::stdout().flush()?;
        return Ok(());
    }

    let out_dir = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = graph
        .download_mail_attachment(&mail.id, &att.id, &out_dir)
        .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "mail_id": mail.id,
                "attachment_id": att.id,
                "path": downloaded.path,
                "file_name": downloaded.file_name,
                "bytes_written": downloaded.bytes_written,
            }))?
        );
    } else {
        println!(
            "메일 첨부 다운로드 완료: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.bytes_written
        );
        if preview {
            let bytes = std::fs::read(&downloaded.path)?;
            let (cols, rows) = default_preview_dimensions(None, None);
            let rendered = img_preview::render_ansi_stdout(&bytes, cols, rows)?;
            print!("{rendered}");
            if !rendered.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

async fn cmd_mail_search(
    cfg: &AppConfig,
    query: &str,
    folder: Option<&str>,
    remote: bool,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("검색어가 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, remote).await?;
    let limit = clamp_limit(limit);
    let folder_filter = match folder.map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) => Some(resolve_mail_folder_id(&storage, &graph, f, !remote).await?),
        None => None,
    };

    let hits = storage.search_mail_messages(query, folder_filter.as_deref(), limit)?;
    let remote_messages = if remote {
        let msgs = graph.search_mail_remote(query, limit).await?;
        let _ = storage.upsert_mail_messages(&msgs);
        msgs
    } else {
        Vec::new()
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "hits": hits,
                "remote": remote_messages,
            }))?
        );
    } else if hits.is_empty() && remote_messages.is_empty() {
        println!("'{query}' 와 일치하는 메일이 없습니다.");
    } else {
        for hit in &hits {
            println!(
                "[로컬][{}] {} | {}: {}",
                hit.received_at, hit.from_display, hit.subject, hit.snippet
            );
        }
        for msg in &remote_messages {
            println!(
                "[원격][{}] {} | {} ({})",
                msg.received_at,
                msg.from_display(),
                msg.subject,
                msg.id
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Outlook Calendar commands & shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn normalize_datetime_iso(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("날짜/시간 문자열이 비어 있습니다.");
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.to_rfc3339());
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d_%H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            let dt = Local
                .from_local_datetime(&naive)
                .earliest()
                .or_else(|| Local.from_local_datetime(&naive).latest())
                .context("로컬 시간대 변환에 실패했습니다.")?;
            return Ok(dt.to_rfc3339());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let naive = date.and_hms_opt(0, 0, 0).context("날짜 변환 실패")?;
        let dt = Local
            .from_local_datetime(&naive)
            .earliest()
            .or_else(|| Local.from_local_datetime(&naive).latest())
            .context("로컬 시간대 변환에 실패했습니다.")?;
        return Ok(dt.to_rfc3339());
    }
    bail!("날짜/시간 형식을 해석할 수 없습니다: '{trimmed}' (ISO-8601 또는 YYYY-MM-DD_HH:MM)")
}

pub(crate) fn default_calendar_window(
    start: Option<&str>,
    end: Option<&str>,
    days: i64,
) -> Result<(String, String)> {
    let clamped_days = days.clamp(1, 365);
    let start_iso = match start.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => normalize_datetime_iso(s)?,
        None => {
            let today = Local::now().date_naive();
            let naive = today.and_hms_opt(0, 0, 0).context("오늘 시작 시각 계산 실패")?;
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap_or_else(Local::now)
                .to_rfc3339()
        }
    };
    let end_iso = match end.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => normalize_datetime_iso(e)?,
        None => {
            let start_dt = DateTime::parse_from_rfc3339(&start_iso)?;
            (start_dt + chrono::Duration::days(clamped_days)).to_rfc3339()
        }
    };
    Ok((start_iso, end_iso))
}

pub(crate) fn normalize_rsvp_response(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "accept" | "accepted" => Ok("accept"),
        "tentative" | "tentativelyaccept" | "tentatively_accept" | "tentativelyaccepted" => {
            Ok("tentativelyAccept")
        }
        "decline" | "declined" => Ok("decline"),
        other => bail!("지원하지 않는 RSVP 응답입니다: '{other}' (accept | tentative | decline)"),
    }
}

async fn cmd_cal_today(cfg: &AppConfig, offline: bool, json_out: bool) -> Result<()> {
    let (start_iso, end_iso) = default_calendar_window(None, None, 1)?;
    cmd_cal_list(cfg, Some(&start_iso), Some(&end_iso), 50, offline, json_out).await
}

async fn cmd_cal_week(cfg: &AppConfig, days: i64, offline: bool, json_out: bool) -> Result<()> {
    let (start_iso, end_iso) = default_calendar_window(None, None, days)?;
    cmd_cal_list(cfg, Some(&start_iso), Some(&end_iso), 100, offline, json_out).await
}

async fn cmd_cal_list(
    cfg: &AppConfig,
    start: Option<&str>,
    end: Option<&str>,
    limit: usize,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let (start_iso, end_iso) = default_calendar_window(start, end, 7)?;

    let events = if offline {
        storage.list_calendar_events(
            start.map(|_| start_iso.as_str()),
            end.map(|_| end_iso.as_str()),
            limit,
        )?
    } else {
        let fetched = graph.list_calendar_view(&start_iso, &end_iso, limit).await?;
        let _ = storage.upsert_calendar_events(&fetched);
        fetched
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "start": start_iso,
                "end": end_iso,
                "events": events,
            }))?
        );
    } else if events.is_empty() {
        println!("표시할 일정이 없습니다.");
    } else {
        for ev in &events {
            let loc = ev
                .location
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!(" @ {s}"))
                .unwrap_or_default();
            let meet = if ev.is_online_meeting { " [Teams]" } else { "" };
            println!(
                "[{} ~ {}] {}{}{} ({})",
                ev.start_iso, ev.end_iso, ev.subject, loc, meet, ev.id
            );
        }
    }
    Ok(())
}

async fn load_calendar_event(
    storage: &Storage,
    graph: &GraphClient,
    event_id: &str,
    offline: bool,
) -> Result<CalendarEvent> {
    if offline {
        return storage
            .list_calendar_events(None, None, 10_000)?
            .into_iter()
            .find(|e| e.id == event_id)
            .with_context(|| format!("로컬 캐시에서 일정을 찾을 수 없습니다: {event_id}"));
    }
    let ev = graph.get_calendar_event(event_id).await?;
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&ev));
    Ok(ev)
}

async fn cmd_cal_show(
    cfg: &AppConfig,
    event_id: &str,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let ev = load_calendar_event(&storage, &graph, event_id, offline).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "event": ev }))?);
    } else {
        println!("제목: {}", ev.subject);
        println!("시간: {} ~ {} ({})", ev.start_iso, ev.end_iso, ev.time_zone);
        if let Some(ref loc) = ev.location {
            println!("장소: {loc}");
        }
        if let Some(ref org) = ev.organizer_name.as_deref().or(ev.organizer_email.as_deref()) {
            println!("주최자: {org}");
        }
        println!("내 응답: {}", ev.response_status);
        if let Some(ref url) = ev.online_meeting_url {
            println!("온라인 모임: {url}");
        }
        if !ev.attendees.is_empty() {
            println!("참석자 ({}명):", ev.attendees.len());
            for a in &ev.attendees {
                println!(
                    "  - {} <{}> [{} / {}]",
                    a.name.as_deref().unwrap_or(&a.address),
                    a.address,
                    a.attendee_type,
                    a.response_status
                );
            }
        }
        if !ev.body_plain.trim().is_empty() {
            println!("---\n{}", ev.body_plain.trim());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_cal_create(
    cfg: &AppConfig,
    subject: &str,
    start: &str,
    end: &str,
    attendees: Vec<String>,
    location: Option<String>,
    body: Option<String>,
    is_online_meeting: bool,
    is_all_day: bool,
    time_zone: Option<String>,
    json_out: bool,
) -> Result<()> {
    if subject.trim().is_empty() {
        bail!("일정 제목이 비어 있습니다.");
    }
    let start_iso = normalize_datetime_iso(start)?;
    let end_iso = normalize_datetime_iso(end)?;
    let body_html = body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|md| {
            parser::compile_outgoing_markdown(md, None, vec![], vec![], vec![], None).body_html
        });

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let outgoing = OutgoingEvent {
        subject: subject.to_string(),
        start_iso,
        end_iso,
        time_zone,
        is_all_day,
        location,
        body_html,
        attendees,
        is_online_meeting,
    };
    let created = graph.create_calendar_event(&outgoing).await?;
    let _ = storage.upsert_calendar_events(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event": created,
            }))?
        );
    } else {
        println!(
            "일정 생성 완료: {} ({} ~ {}) id={}",
            created.subject, created.start_iso, created.end_iso, created.id
        );
        if let Some(ref url) = created.online_meeting_url {
            println!("Teams 모임 링크: {url}");
        }
    }
    Ok(())
}

async fn cmd_cal_rsvp(
    cfg: &AppConfig,
    event_id: &str,
    response: &str,
    comment: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let normalized = normalize_rsvp_response(response)?;
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    graph
        .respond_calendar_event(event_id, normalized, comment)
        .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": event_id,
                "response": normalized,
            }))?
        );
    } else {
        println!("일정 응답 완료: {event_id} -> {normalized}");
    }
    Ok(())
}

async fn cmd_cal_join(
    cfg: &AppConfig,
    event_id: &str,
    print_only: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let ev = load_calendar_event(&storage, &graph, event_id, offline).await?;
    let url = ev
        .online_meeting_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("이 일정에는 온라인 모임 URL이 없습니다: {}", ev.subject))?;

    if !print_only && !json_out {
        open::that(url).with_context(|| format!("모임 URL 열기 실패: {url}"))?;
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": ev.id,
                "subject": ev.subject,
                "url": url,
                "opened": false,
            }))?
        );
    } else if print_only {
        println!("{url}");
    } else {
        println!("모임 URL 열기: {url}");
    }
    Ok(())
}

async fn cmd_cal_delete(cfg: &AppConfig, event_id: &str, yes: bool, json_out: bool) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "일정 {event_id} 를 삭제합니다. 계속하려면 'yes' 입력: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_calendar_event(event_id).await?;
    let _ = storage.delete_calendar_event(event_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "event_id": event_id,
            }))?
        );
    } else {
        println!("일정 삭제 완료: {event_id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Outlook Contacts commands
// ---------------------------------------------------------------------------

async fn cmd_contacts_list(
    cfg: &AppConfig,
    limit: usize,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let cached = storage.list_contacts(None, limit)?;
    let contacts = if !offline && (refresh || cached.is_empty()) {
        let fetched = graph.list_contacts(limit).await?;
        storage.upsert_contacts(&fetched)?;
        let stored = storage.list_contacts(None, limit)?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "contacts": contacts }))?
        );
    } else if contacts.is_empty() {
        println!("표시할 연락처가 없습니다.");
    } else {
        for c in &contacts {
            print_contact_line(c);
        }
    }
    Ok(())
}

async fn cmd_contacts_search(
    cfg: &AppConfig,
    query: &str,
    limit: usize,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("검색어가 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let contacts = if offline {
        storage.list_contacts(Some(query), limit)?
    } else {
        let fetched = graph.search_contacts_and_directory(query, limit).await?;
        let _ = storage.upsert_contacts(&fetched);
        fetched
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "contacts": contacts,
            }))?
        );
    } else if contacts.is_empty() {
        println!("'{query}' 와 일치하는 연락처가 없습니다.");
    } else {
        for c in &contacts {
            print_contact_line(c);
        }
    }
    Ok(())
}

fn print_contact_line(c: &ContactItem) {
    let email = c.primary_email().unwrap_or("-");
    let org = c
        .company_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {s}"))
        .unwrap_or_default();
    let phone = c
        .mobile_phone
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!(" · {s}"))
        .unwrap_or_default();
    println!(
        "[{}] {} <{}>{}{} ({})",
        c.source, c.display_name, email, org, phone, c.id
    );
}

#[allow(clippy::too_many_arguments)]
async fn cmd_contacts_add(
    cfg: &AppConfig,
    display_name: &str,
    email_addresses: Vec<String>,
    mobile_phone: Option<String>,
    company_name: Option<String>,
    job_title: Option<String>,
    notes: Option<String>,
    json_out: bool,
) -> Result<()> {
    if display_name.trim().is_empty() {
        bail!("연락처 이름이 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let outgoing = OutgoingContact {
        display_name: display_name.to_string(),
        given_name: None,
        surname: None,
        email_addresses,
        mobile_phone,
        company_name,
        job_title,
        department: None,
        notes,
    };
    let created = graph.create_contact(&outgoing).await?;
    let _ = storage.upsert_contacts(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "contact": created,
            }))?
        );
    } else {
        println!("연락처 추가 완료: {} ({})", created.display_name, created.id);
    }
    Ok(())
}

async fn cmd_contacts_delete(
    cfg: &AppConfig,
    contact_id: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "연락처 {contact_id} 를 삭제합니다. 계속하려면 'yes' 입력: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_contact(contact_id).await?;
    let _ = storage.delete_contact(contact_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "contact_id": contact_id,
            }))?
        );
    } else {
        println!("연락처 삭제 완료: {contact_id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Microsoft To Do commands & shared helpers
// ---------------------------------------------------------------------------

pub(crate) fn resolve_todo_list_from_slice(lists: &[TodoList], query: &str) -> Result<TodoList> {
    let q = query.trim();
    if lists.is_empty() {
        bail!("To Do 목록이 비어 있습니다.");
    }
    if q.is_empty() || q.eq_ignore_ascii_case("default") {
        if let Some(def) = lists
            .iter()
            .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
        {
            return Ok(def.clone());
        }
        return Ok(lists[0].clone());
    }
    if let Some(exact) = lists.iter().find(|l| l.id == q) {
        return Ok(exact.clone());
    }
    let q_lower = q.to_lowercase();
    if let Some(wk) = lists.iter().find(|l| {
        l.wellknown_list_name
            .as_deref()
            .is_some_and(|w| w.to_lowercase() == q_lower)
    }) {
        return Ok(wk.clone());
    }
    let exact_name: Vec<&TodoList> = lists
        .iter()
        .filter(|l| l.display_name.to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    }
    let sub: Vec<&TodoList> = lists
        .iter()
        .filter(|l| l.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match sub.len() {
        0 => bail!("To Do 목록을 찾을 수 없습니다: {q}"),
        1 => Ok(sub[0].clone()),
        _ => bail!("여러 To Do 목록이 '{q}' 와 일치합니다."),
    }
}

pub(crate) async fn resolve_todo_list_id(
    storage: &Storage,
    graph: &GraphClient,
    list_query: &str,
    offline: bool,
) -> Result<String> {
    let cached = storage.list_todo_lists()?;
    if !cached.is_empty() {
        if let Ok(found) = resolve_todo_list_from_slice(&cached, list_query) {
            return Ok(found.id);
        }
    }
    if !offline {
        let fetched = graph.list_todo_lists().await?;
        let _ = storage.upsert_todo_lists(&fetched);
        if let Ok(found) = resolve_todo_list_from_slice(&fetched, list_query) {
            return Ok(found.id);
        }
    }
    if !list_query.trim().is_empty() && !list_query.trim().eq_ignore_ascii_case("default") {
        return Ok(list_query.trim().to_string());
    }
    bail!("기본 To Do 목록을 찾을 수 없습니다.")
}

pub(crate) fn normalize_todo_due_iso(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("마감일이 비어 있습니다.");
    }
    if NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok() {
        return Ok(format!("{trimmed}T00:00:00Z"));
    }
    normalize_datetime_iso(trimmed)
}

pub(crate) fn normalize_todo_importance(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low"),
        "normal" => Ok("normal"),
        "high" => Ok("high"),
        other => bail!("지원하지 않는 To Do 중요도입니다: '{other}' (low | normal | high)"),
    }
}

async fn cmd_todo_lists(
    cfg: &AppConfig,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let cached = storage.list_todo_lists()?;
    let lists = if !offline && (refresh || cached.is_empty()) {
        let fetched = graph.list_todo_lists().await?;
        storage.upsert_todo_lists(&fetched)?;
        let stored = storage.list_todo_lists()?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "lists": lists }))?
        );
    } else if lists.is_empty() {
        println!("To Do 목록이 없습니다.");
    } else {
        for l in &lists {
            let wk = l
                .wellknown_list_name
                .as_deref()
                .map(|w| format!(" [{w}]"))
                .unwrap_or_default();
            println!("{}{} ({})", l.display_name, wk, l.id);
        }
    }
    Ok(())
}

async fn cmd_todo_tasks(
    cfg: &AppConfig,
    list: &str,
    include_completed: bool,
    limit: usize,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let list_id = resolve_todo_list_id(&storage, &graph, list, offline).await?;
    let cached = storage.list_todo_tasks(&list_id, include_completed, limit)?;
    let mut tasks = if !offline && (refresh || cached.is_empty()) {
        let fetched = graph
            .list_todo_tasks(&list_id, include_completed, limit)
            .await?;
        storage.upsert_todo_tasks(&fetched)?;
        let stored = storage.list_todo_tasks(&list_id, include_completed, limit)?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };
    tasks.truncate(limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "list_id": list_id,
                "tasks": tasks,
            }))?
        );
    } else if tasks.is_empty() {
        println!("표시할 할 일이 없습니다.");
    } else {
        for t in &tasks {
            let check = if t.is_completed() { "x" } else { " " };
            let imp = if t.importance.eq_ignore_ascii_case("high") {
                "!"
            } else {
                " "
            };
            let due = t
                .due_date_iso
                .as_deref()
                .map(|d| format!(" (기한 {d})"))
                .unwrap_or_default();
            println!("[{check}]{imp} {}{}  ({})", t.title, due, t.id);
        }
    }
    Ok(())
}

async fn cmd_todo_add(
    cfg: &AppConfig,
    title: &str,
    list: &str,
    due: Option<&str>,
    important: bool,
    body: Option<String>,
    json_out: bool,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("할 일 제목이 비어 있습니다.");
    }
    let due_date_iso = due.map(normalize_todo_due_iso).transpose()?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let list_id = resolve_todo_list_id(&storage, &graph, list, false).await?;

    let outgoing = OutgoingTodoTask {
        title: title.to_string(),
        body_plain: body,
        due_date_iso,
        importance: if important {
            Some("high".to_string())
        } else {
            None
        },
    };
    let created = graph.create_todo_task(&list_id, &outgoing).await?;
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&created));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "task": created,
            }))?
        );
    } else {
        println!("할 일 추가 완료: {} ({})", created.title, created.id);
    }
    Ok(())
}

async fn cmd_todo_done(
    cfg: &AppConfig,
    task_id: &str,
    list: &str,
    completed: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let list_id = resolve_todo_list_id(&storage, &graph, list, false).await?;
    let updated = graph
        .set_todo_task_completed(&list_id, task_id, completed)
        .await?;
    let _ = storage.upsert_todo_tasks(std::slice::from_ref(&updated));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "task": updated,
            }))?
        );
    } else {
        println!(
            "할 일 {} 처리 완료: {} ({})",
            if completed { "완료" } else { "미완료" },
            updated.title,
            updated.id
        );
    }
    Ok(())
}

async fn cmd_todo_delete(
    cfg: &AppConfig,
    task_id: &str,
    list: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let list_id = resolve_todo_list_id(&storage, &graph, list, false).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "할 일 {task_id} 를 삭제합니다. 계속하려면 'yes' 입력: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_todo_task(&list_id, task_id).await?;
    let _ = storage.delete_todo_task(task_id);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "list_id": list_id,
                "task_id": task_id,
            }))?
        );
    } else {
        println!("할 일 삭제 완료: {task_id}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Microsoft OneNote commands & shared helpers (reused by `serve.rs` & `mcp.rs`)
// ---------------------------------------------------------------------------

pub(crate) fn build_onenote_scope_from_args(
    group_id: Option<&str>,
    site_id: Option<&str>,
) -> Result<OnenoteScope> {
    let g = group_id.map(str::trim).filter(|s| !s.is_empty());
    let s = site_id.map(str::trim).filter(|s| !s.is_empty());
    match (g, s) {
        (Some(_), Some(_)) => bail!("--group 과 --site 는 동시에 지정할 수 없습니다."),
        (Some(gid), None) => Ok(OnenoteScope::Group {
            group_id: gid.to_string(),
            group_name: None,
        }),
        (None, Some(sid)) => Ok(OnenoteScope::Site {
            site_id: sid.to_string(),
            site_name: None,
        }),
        (None, None) => Ok(OnenoteScope::Me),
    }
}

pub(crate) fn filter_notebooks_by_scope(
    items: Vec<OnenoteNotebook>,
    include_teams: bool,
) -> Vec<OnenoteNotebook> {
    if include_teams {
        items
    } else {
        items
            .into_iter()
            .filter(|n| n.scope.is_personal())
            .collect()
    }
}

pub(crate) fn resolve_onenote_notebook_from_slice(
    notebooks: &[OnenoteNotebook],
    query: &str,
) -> Result<OnenoteNotebook> {
    let q = query.trim();
    if q.is_empty() {
        bail!("필기장 이름 또는 키가 비어 있습니다.");
    }
    if let Some(exact_key) = notebooks
        .iter()
        .find(|n| n.notebook_key == q || n.id == q)
    {
        return Ok(exact_key.clone());
    }
    let q_lower = q.to_lowercase();
    if let Some((owner_part, name_part)) = q_lower.rsplit_once('/') {
        let owner_q = owner_part.trim();
        let name_q = name_part.trim();
        if !owner_q.is_empty() && !name_q.is_empty() {
            let exact_pair: Vec<&OnenoteNotebook> = notebooks
                .iter()
                .filter(|n| {
                    n.display_name.to_lowercase() == name_q
                        && n.scope.owner_label().to_lowercase().contains(owner_q)
                })
                .collect();
            if exact_pair.len() == 1 {
                return Ok(exact_pair[0].clone());
            }
            let sub_pair: Vec<&OnenoteNotebook> = notebooks
                .iter()
                .filter(|n| {
                    n.display_name.to_lowercase().contains(name_q)
                        && n.scope.owner_label().to_lowercase().contains(owner_q)
                })
                .collect();
            if sub_pair.len() == 1 {
                return Ok(sub_pair[0].clone());
            }
        }
    }

    let exact_name: Vec<&OnenoteNotebook> = notebooks
        .iter()
        .filter(|n| n.display_name.to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    }
    if exact_name.len() > 1 {
        let personal: Vec<&&OnenoteNotebook> = exact_name
            .iter()
            .filter(|n| n.scope.is_personal())
            .collect();
        if personal.len() == 1 {
            return Ok((*personal[0]).clone());
        }
        let candidates: Vec<String> = exact_name
            .iter()
            .map(|n| format!("{}/{} ({})", n.scope.owner_label(), n.display_name, n.notebook_key))
            .collect();
        bail!(
            "여러 필기장이 '{q}' 와 일치합니다: {}",
            candidates.join(", ")
        );
    }

    let sub: Vec<&OnenoteNotebook> = notebooks
        .iter()
        .filter(|n| {
            n.display_name.to_lowercase().contains(&q_lower)
                || format!("{}/{}", n.scope.owner_label(), n.display_name)
                    .to_lowercase()
                    .contains(&q_lower)
        })
        .collect();
    match sub.len() {
        0 => bail!("필기장을 찾을 수 없습니다: {q}"),
        1 => Ok(sub[0].clone()),
        _ => {
            let candidates: Vec<String> = sub
                .iter()
                .map(|n| {
                    format!(
                        "{}/{} ({})",
                        n.scope.owner_label(),
                        n.display_name,
                        n.notebook_key
                    )
                })
                .collect();
            bail!(
                "여러 필기장이 '{q}' 와 일치합니다: {}",
                candidates.join(", ")
            );
        }
    }
}

pub(crate) async fn load_onenote_notebooks(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
) -> Result<Vec<OnenoteNotebook>> {
    let cached = filter_notebooks_by_scope(storage.list_onenote_notebooks()?, include_teams);
    if !offline && (refresh || cached.is_empty()) {
        let fetched = if include_teams {
            graph.list_all_accessible_notebooks(true).await?
        } else {
            graph.list_onenote_notebooks(&OnenoteScope::Me).await?
        };
        storage.upsert_onenote_notebooks(&fetched)?;
        let stored = filter_notebooks_by_scope(storage.list_onenote_notebooks()?, include_teams);
        if stored.is_empty() {
            Ok(filter_notebooks_by_scope(fetched, include_teams))
        } else {
            Ok(stored)
        }
    } else {
        Ok(cached)
    }
}

pub(crate) async fn resolve_onenote_notebook(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    include_teams: bool,
    refresh: bool,
    offline: bool,
) -> Result<OnenoteNotebook> {
    if !refresh {
        let cached = filter_notebooks_by_scope(storage.list_onenote_notebooks()?, include_teams);
        if !cached.is_empty() {
            if let Ok(found) = resolve_onenote_notebook_from_slice(&cached, query) {
                return Ok(found);
            }
        }
    }
    if !offline {
        let fetched = if include_teams {
            graph.list_all_accessible_notebooks(true).await?
        } else {
            graph.list_onenote_notebooks(&OnenoteScope::Me).await?
        };
        storage.upsert_onenote_notebooks(&fetched)?;
        let stored = filter_notebooks_by_scope(storage.list_onenote_notebooks()?, include_teams);
        let pool = if stored.is_empty() { fetched } else { stored };
        if let Ok(found) = resolve_onenote_notebook_from_slice(&pool, query) {
            return Ok(found);
        }
    }
    if query.contains('|') {
        let (scope, nb_id) = split_onenote_entity_key(query);
        return Ok(OnenoteNotebook {
            notebook_key: scope.make_entity_key(&nb_id),
            id: nb_id.clone(),
            scope,
            display_name: nb_id,
            color: None,
            is_default: false,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        });
    }
    bail!("필기장을 찾을 수 없습니다: {}", query.trim())
}

pub(crate) fn resolve_onenote_section_from_slice(
    sections: &[OnenoteSection],
    query: &str,
) -> Result<OnenoteSection> {
    let q = query.trim();
    if q.is_empty() {
        bail!("섹션 이름 또는 키가 비어 있습니다.");
    }
    if let Some(exact_key) = sections
        .iter()
        .find(|s| s.section_key == q || s.id == q)
    {
        return Ok(exact_key.clone());
    }

    let q_lower = q.to_lowercase();
    if let Some((lhs_raw, rhs_raw)) = q_lower.rsplit_once('/') {
        let lhs = lhs_raw.trim();
        let rhs = rhs_raw.trim();
        if !lhs.is_empty() && !rhs.is_empty() {
            let parent_matches = |s: &OnenoteSection, needle: &str, exact: bool| -> bool {
                let nb = s.notebook_name.as_deref().unwrap_or_default().to_lowercase();
                let sg = s
                    .parent_section_group_name
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase();
                let owner = s.scope.owner_label().to_lowercase();
                let combined = if sg.is_empty() {
                    nb.clone()
                } else {
                    format!("{nb}/{sg}")
                };
                if exact {
                    nb == needle || sg == needle || owner == needle || combined == needle
                } else {
                    nb.contains(needle)
                        || sg.contains(needle)
                        || owner.contains(needle)
                        || combined.contains(needle)
                        || format!("{owner}/{nb}").contains(needle)
                }
            };

            let exact_slash: Vec<&OnenoteSection> = sections
                .iter()
                .filter(|s| {
                    s.display_name.to_lowercase() == rhs && parent_matches(s, lhs, true)
                })
                .collect();
            if exact_slash.len() == 1 {
                return Ok(exact_slash[0].clone());
            }

            let exact_rhs_sub_lhs: Vec<&OnenoteSection> = sections
                .iter()
                .filter(|s| {
                    s.display_name.to_lowercase() == rhs && parent_matches(s, lhs, false)
                })
                .collect();
            if exact_rhs_sub_lhs.len() == 1 {
                return Ok(exact_rhs_sub_lhs[0].clone());
            }

            let sub_slash: Vec<&OnenoteSection> = sections
                .iter()
                .filter(|s| {
                    s.display_name.to_lowercase().contains(rhs) && parent_matches(s, lhs, false)
                })
                .collect();
            match sub_slash.len() {
                0 => {}
                1 => return Ok(sub_slash[0].clone()),
                _ => {
                    let candidates: Vec<String> = sub_slash
                        .iter()
                        .map(|s| {
                            format!(
                                "{}/{} ({})",
                                s.notebook_name.as_deref().unwrap_or("-"),
                                s.display_name,
                                s.section_key
                            )
                        })
                        .collect();
                    bail!(
                        "여러 섹션이 '{q}' 와 일치합니다: {}",
                        candidates.join(", ")
                    );
                }
            }
        }
    }

    let exact_name: Vec<&OnenoteSection> = sections
        .iter()
        .filter(|s| s.display_name.to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    }
    if exact_name.len() > 1 {
        let candidates: Vec<String> = exact_name
            .iter()
            .map(|s| {
                format!(
                    "{}/{} ({})",
                    s.notebook_name.as_deref().unwrap_or("-"),
                    s.display_name,
                    s.section_key
                )
            })
            .collect();
        bail!(
            "동일한 이름의 섹션이 여러 개 있습니다 ('필기장/섹션' 또는 키를 지정하세요): {}",
            candidates.join(", ")
        );
    }

    let sub: Vec<&OnenoteSection> = sections
        .iter()
        .filter(|s| {
            s.display_name.to_lowercase().contains(&q_lower)
                || format!(
                    "{}/{}",
                    s.notebook_name.as_deref().unwrap_or_default(),
                    s.display_name
                )
                .to_lowercase()
                .contains(&q_lower)
        })
        .collect();
    match sub.len() {
        0 => bail!("섹션을 찾을 수 없습니다: {q}"),
        1 => Ok(sub[0].clone()),
        _ => {
            let candidates: Vec<String> = sub
                .iter()
                .map(|s| {
                    format!(
                        "{}/{} ({})",
                        s.notebook_name.as_deref().unwrap_or("-"),
                        s.display_name,
                        s.section_key
                    )
                })
                .collect();
            bail!(
                "여러 섹션이 '{q}' 와 일치합니다: {}",
                candidates.join(", ")
            );
        }
    }
}

pub(crate) async fn load_onenote_sections(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>)> {
    if let Some(nb_q) = notebook_query.map(str::trim).filter(|s| !s.is_empty()) {
        let nb = resolve_onenote_notebook(storage, graph, nb_q, true, refresh, offline).await?;
        let cached = storage.list_onenote_sections(Some(&nb.notebook_key))?;
        if !offline && (refresh || cached.is_empty()) {
            if let Ok(groups) = graph
                .list_onenote_section_groups(&nb.scope, Some(&nb.id))
                .await
            {
                if !groups.is_empty() {
                    let _ = storage.upsert_onenote_section_groups(&groups);
                }
            }
            let mut secs = graph.list_onenote_sections(&nb.scope, Some(&nb.id)).await?;
            for sec in &mut secs {
                if sec.notebook_id.is_none() {
                    sec.notebook_id = Some(nb.id.clone());
                }
                if sec.notebook_name.is_none() {
                    sec.notebook_name = Some(nb.display_name.clone());
                }
            }
            storage.upsert_onenote_sections(&secs)?;
            let stored = storage.list_onenote_sections(Some(&nb.notebook_key))?;
            Ok((Some(nb), if stored.is_empty() { secs } else { stored }))
        } else {
            Ok((Some(nb), cached))
        }
    } else {
        let cached = storage.list_onenote_sections(None)?;
        if !offline && (refresh || cached.is_empty()) {
            let notebooks = load_onenote_notebooks(storage, graph, true, refresh, false).await?;
            let (groups, secs) = graph.list_all_accessible_sections(&notebooks).await?;
            if !groups.is_empty() {
                storage.upsert_onenote_section_groups(&groups)?;
            }
            storage.upsert_onenote_sections(&secs)?;
            let stored = storage.list_onenote_sections(None)?;
            Ok((None, if stored.is_empty() { secs } else { stored }))
        } else {
            Ok((None, cached))
        }
    }
}

pub(crate) async fn resolve_onenote_section(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    refresh: bool,
    offline: bool,
) -> Result<OnenoteSection> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        bail!("섹션 이름 또는 키가 비어 있습니다.");
    }
    if !refresh {
        let cached = storage.list_onenote_sections(None)?;
        if !cached.is_empty() {
            if let Ok(found) = resolve_onenote_section_from_slice(&cached, trimmed) {
                return Ok(found);
            }
        }
    }
    if !offline {
        if let Ok((_, fresh)) = load_onenote_sections(storage, graph, None, true, false).await {
            if let Ok(found) = resolve_onenote_section_from_slice(&fresh, trimmed) {
                return Ok(found);
            }
        }
    }
    if trimmed.contains('|') {
        let (scope, sec_id) = split_onenote_entity_key(trimmed);
        return Ok(OnenoteSection {
            section_key: scope.make_entity_key(&sec_id),
            id: sec_id.clone(),
            scope,
            notebook_id: None,
            notebook_name: None,
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: sec_id,
            color: None,
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        });
    }
    let cached = storage.list_onenote_sections(None)?;
    resolve_onenote_section_from_slice(&cached, trimmed)
}

pub(crate) async fn list_onenote_pages_helper(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    limit: usize,
    refresh: bool,
    offline: bool,
) -> Result<(OnenoteSection, Vec<OnenotePageSummary>)> {
    let limit = clamp_limit(limit);
    let sec = resolve_onenote_section(storage, graph, section_query, refresh, offline).await?;
    let cached = storage.list_onenote_pages(&sec.section_key)?;
    let mut pages = if !offline && (refresh || cached.is_empty()) {
        let mut fetched = graph
            .list_onenote_pages(&sec.scope, &sec.id, limit)
            .await?;
        for p in &mut fetched {
            if p.section_id.is_empty() {
                p.section_id = sec.id.clone();
            }
            if p.section_name.is_none() {
                p.section_name = Some(sec.display_name.clone());
            }
            if p.notebook_id.is_none() {
                p.notebook_id = sec.notebook_id.clone();
            }
            if p.notebook_name.is_none() {
                p.notebook_name = sec.notebook_name.clone();
            }
        }
        storage.upsert_onenote_pages(&fetched)?;
        let stored = storage.list_onenote_pages(&sec.section_key)?;
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached
    };
    pages.truncate(limit);
    Ok((sec, pages))
}

fn pick_search_hit_for_title<'a>(
    hits: &'a [OnenoteSearchHit],
    query: &str,
) -> Option<&'a OnenoteSearchHit> {
    let q_lower = query.trim().to_lowercase();
    if let Some(exact) = hits.iter().find(|h| h.title.to_lowercase() == q_lower) {
        return Some(exact);
    }
    if hits.len() == 1 {
        return Some(&hits[0]);
    }
    let title_matches: Vec<&OnenoteSearchHit> = hits
        .iter()
        .filter(|h| h.title.to_lowercase().contains(&q_lower))
        .collect();
    if title_matches.len() == 1 {
        return Some(title_matches[0]);
    }
    None
}

pub(crate) async fn load_onenote_page_detail(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    offline: bool,
) -> Result<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("페이지 ID 또는 검색어가 비어 있습니다.");
    }

    if offline {
        if let Some(detail) = storage.get_onenote_page_detail(q)? {
            return Ok(detail);
        }
        let hits = storage.search_onenote_pages(q, None, 10)?;
        if let Some(hit) = pick_search_hit_for_title(&hits, q) {
            if let Some(detail) = storage.get_onenote_page_detail(&hit.page_key)? {
                return Ok(detail);
            }
        }
        bail!("오프라인 캐시에서 OneNote 페이지를 찾을 수 없습니다: {q}");
    }

    let mut cached_summary = storage.get_onenote_page_summary(q)?;
    if cached_summary.is_none() && !q.contains('|') {
        let hits = storage.search_onenote_pages(q, None, 10)?;
        if let Some(hit) = pick_search_hit_for_title(&hits, q) {
            cached_summary = storage.get_onenote_page_summary(&hit.page_key)?.or_else(|| {
                Some(OnenotePageSummary {
                    page_key: hit.page_key.clone(),
                    id: hit.page_id.clone(),
                    scope: hit.scope.clone(),
                    section_id: hit.section_id.clone(),
                    section_name: hit.section_name.clone(),
                    notebook_id: None,
                    notebook_name: hit.notebook_name.clone(),
                    title: hit.title.clone(),
                    level: 0,
                    order: 0,
                    created_at: None,
                    last_modified_at: hit.last_modified_at.clone(),
                    content_url: None,
                    web_url: hit.web_url.clone(),
                    preview_text: Some(hit.snippet.clone()),
                    has_todo: false,
                })
            });
        }
    }

    if let Some(summary) = cached_summary {
        let scope = summary.scope.clone();
        let id = summary.id.clone();
        let detail = graph
            .get_onenote_page_detail(&scope, &id, Some(summary))
            .await?;
        storage.upsert_onenote_page_detail(&detail)?;
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&detail.summary));
        return Ok(detail);
    }

    if q.contains('|') {
        let (scope, page_id) = split_onenote_entity_key(q);
        let detail = graph.get_onenote_page_detail(&scope, &page_id, None).await?;
        storage.upsert_onenote_page_detail(&detail)?;
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&detail.summary));
        return Ok(detail);
    }

    // Try direct personal page ID first unless query clearly contains whitespace/non-ID chars.
    let looks_like_id = !q.chars().any(char::is_whitespace) && q.is_ascii();
    if looks_like_id {
        if let Ok(detail) = graph
            .get_onenote_page_detail(&OnenoteScope::Me, q, None)
            .await
        {
            storage.upsert_onenote_page_detail(&detail)?;
            let _ = storage.upsert_onenote_pages(std::slice::from_ref(&detail.summary));
            return Ok(detail);
        }
    }

    // Bootstrap hierarchy/pages and retry summary + FTS lookup.
    let _ = sync::bootstrap_onenote(graph, storage).await;
    if let Some(summary) = storage.get_onenote_page_summary(q)? {
        let scope = summary.scope.clone();
        let id = summary.id.clone();
        let detail = graph
            .get_onenote_page_detail(&scope, &id, Some(summary))
            .await?;
        storage.upsert_onenote_page_detail(&detail)?;
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&detail.summary));
        return Ok(detail);
    }
    let hits = storage.search_onenote_pages(q, None, 10)?;
    if let Some(hit) = pick_search_hit_for_title(&hits, q) {
        let summary = storage.get_onenote_page_summary(&hit.page_key)?;
        let detail = graph
            .get_onenote_page_detail(&hit.scope, &hit.page_id, summary)
            .await?;
        storage.upsert_onenote_page_detail(&detail)?;
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&detail.summary));
        return Ok(detail);
    }

    // Also scan cached page titles across all sections in case FTS didn't index uncached bodies yet.
    let sections = storage.list_onenote_sections(None)?;
    let q_lower = q.to_lowercase();
    let mut title_candidates = Vec::new();
    for sec in &sections {
        if let Ok(pages) = storage.list_onenote_pages(&sec.section_key) {
            for p in pages {
                if p.id == q
                    || p.page_key == q
                    || p.title.to_lowercase() == q_lower
                    || p.title.to_lowercase().contains(&q_lower)
                {
                    title_candidates.push(p);
                }
            }
        }
    }
    if let Some(exact) = title_candidates
        .iter()
        .find(|p| p.id == q || p.title.to_lowercase() == q_lower)
        .cloned()
        .or_else(|| {
            if title_candidates.len() == 1 {
                Some(title_candidates[0].clone())
            } else {
                None
            }
        })
    {
        let scope = exact.scope.clone();
        let id = exact.id.clone();
        let detail = graph
            .get_onenote_page_detail(&scope, &id, Some(exact))
            .await?;
        storage.upsert_onenote_page_detail(&detail)?;
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&detail.summary));
        return Ok(detail);
    }

    bail!("OneNote 페이지를 찾을 수 없습니다: {q}")
}

pub(crate) fn read_markdown_input_and_images(
    body_arg: Option<&str>,
    file_arg: Option<&Path>,
    image_paths: &[PathBuf],
) -> Result<(String, Vec<InlineImageUpload>)> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = body_arg.filter(|s| !s.is_empty()) {
        parts.push(b.to_string());
    }
    if let Some(path) = file_arg {
        let from_file = std::fs::read_to_string(path)
            .with_context(|| format!("Markdown 파일 읽기 실패: {}", path.display()))?;
        if !from_file.is_empty() {
            parts.push(from_file);
        }
    }
    let mut markdown = parts.join("\n\n");

    let mut uploads = Vec::with_capacity(image_paths.len());
    for (idx, img_path) in image_paths.iter().enumerate() {
        if !img_path.exists() || !img_path.is_file() {
            bail!("이미지 파일을 찾을 수 없습니다: {}", img_path.display());
        }
        let mime_type = image_mime_for_path(img_path).with_context(|| {
            format!(
                "지원하지 않는 이미지 형식입니다(png/jpg/jpeg/gif/webp/bmp): {}",
                img_path.display()
            )
        })?;
        let bytes = std::fs::read(img_path)
            .with_context(|| format!("이미지 파일 읽기 실패: {}", img_path.display()))?;
        let file_name = img_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToString::to_string);
        let temp_id = format!("img{}", idx + 1);
        let marker = format!("name:{temp_id}");
        if !markdown.contains(&marker) {
            let alt = file_name.as_deref().unwrap_or("image");
            if markdown.trim().is_empty() {
                markdown = format!("![{alt}]({marker})");
            } else {
                markdown.push_str(&format!("\n\n![{alt}]({marker})"));
            }
        }
        uploads.push(InlineImageUpload {
            temp_id,
            mime_type: mime_type.to_string(),
            bytes,
            file_name,
        });
    }

    Ok((markdown, uploads))
}

pub(crate) async fn create_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    title: &str,
    markdown_body: &str,
    inline_images: Vec<InlineImageUpload>,
    level: Option<i32>,
) -> Result<(OnenoteSection, OnenotePageSummary)> {
    let trimmed_title = title.trim();
    if trimmed_title.is_empty() {
        bail!("페이지 제목(--title)이 비어 있습니다.");
    }
    if let Some(lv) = level {
        if !(0..=2).contains(&lv) {
            bail!("페이지 들여쓰기 level 은 0, 1, 2 중 하나여야 합니다 (입력값: {lv}).");
        }
    }
    let sec = resolve_onenote_section(storage, graph, section_query, false, false).await?;
    let outgoing = OutgoingOnenotePage {
        title: trimmed_title.to_string(),
        markdown_body: markdown_body.to_string(),
        inline_images,
        level,
    };
    let mut created = graph
        .create_onenote_page(&sec.scope, &sec.id, &outgoing)
        .await?;
    if created.section_id.is_empty() {
        created.section_id = sec.id.clone();
    }
    if created.section_name.is_none() {
        created.section_name = Some(sec.display_name.clone());
    }
    if created.notebook_id.is_none() {
        created.notebook_id = sec.notebook_id.clone();
    }
    if created.notebook_name.is_none() {
        created.notebook_name = sec.notebook_name.clone();
    }
    let _ = storage.upsert_onenote_pages(std::slice::from_ref(&created));
    Ok((sec, created))
}

pub(crate) async fn append_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    markdown: &str,
    inline_images: &[InlineImageUpload],
) -> Result<serde_json::Value> {
    if markdown.trim().is_empty() && inline_images.is_empty() {
        bail!("추가할 Markdown 본문 또는 이미지(--file / --image)를 지정하세요.");
    }
    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;
    graph
        .append_onenote_page_markdown(
            &detail.summary.scope,
            &detail.summary.id,
            detail.primary_div_id.as_deref(),
            markdown,
            inline_images,
        )
        .await?;

    let refreshed = graph
        .get_onenote_page_detail(
            &detail.summary.scope,
            &detail.summary.id,
            Some(detail.summary.clone()),
        )
        .await
        .ok();
    if let Some(ref d) = refreshed {
        let _ = storage.upsert_onenote_page_detail(d);
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&d.summary));
    }

    Ok(json!({
        "ok": true,
        "page_key": detail.summary.page_key,
        "page_id": detail.summary.id,
        "primary_div_id": detail.primary_div_id,
        "page": refreshed,
    }))
}

pub(crate) async fn update_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    new_title: Option<&str>,
    new_markdown: Option<&str>,
    block_id: Option<&str>,
) -> Result<serde_json::Value> {
    let title_opt = new_title.map(str::trim).filter(|s| !s.is_empty());
    let md_opt = new_markdown.filter(|s| !s.trim().is_empty());
    let bid_opt = block_id.map(str::trim).filter(|s| !s.is_empty());

    if bid_opt.is_some() && md_opt.is_none() {
        bail!("block_id 지정 시 교체할 Markdown 본문(--body 또는 --file)이 필요합니다.");
    }
    if title_opt.is_none() && md_opt.is_none() {
        bail!("수정할 제목(--title) 또는 본문(--body / --file) 중 하나 이상을 지정하세요.");
    }

    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;
    let commands: Vec<OnenotePatchCommand> = if let Some(bid) = bid_opt {
        let mut cmds = vec![parser::onenote::build_replace_block_patch(
            bid,
            md_opt.unwrap_or_default(),
        )];
        if let Some(t) = title_opt {
            cmds.push(OnenotePatchCommand {
                target: "title".to_string(),
                action: "replace".to_string(),
                position: None,
                content: t.to_string(),
            });
        }
        cmds
    } else if let Some(md) = md_opt {
        parser::onenote::build_replace_page_body_commands(&detail, title_opt, md)
    } else {
        vec![OnenotePatchCommand {
            target: "title".to_string(),
            action: "replace".to_string(),
            position: None,
            content: title_opt.unwrap_or_default().to_string(),
        }]
    };

    if commands.is_empty() {
        bail!("적용할 OneNote PATCH 명령을 생성할 수 없습니다.");
    }

    graph
        .patch_onenote_page_commands(&detail.summary.scope, &detail.summary.id, &commands)
        .await?;

    let refreshed = graph
        .get_onenote_page_detail(
            &detail.summary.scope,
            &detail.summary.id,
            Some(detail.summary.clone()),
        )
        .await
        .ok();
    if let Some(ref d) = refreshed {
        let _ = storage.upsert_onenote_page_detail(d);
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&d.summary));
    }

    Ok(json!({
        "ok": true,
        "page_key": detail.summary.page_key,
        "page_id": detail.summary.id,
        "commands_applied": commands.len(),
        "page": refreshed,
    }))
}

pub(crate) async fn toggle_onenote_todo_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    block_id: Option<&str>,
    todo_index: Option<usize>,
) -> Result<serde_json::Value> {
    let bid_opt = block_id.map(str::trim).filter(|s| !s.is_empty());
    if bid_opt.is_none() && todo_index.is_none() {
        bail!("체크박스 토글을 위해 --block-id <element_id> 또는 --index <todo_index> 를 지정하세요.");
    }

    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;
    let target_block: &OnenoteBlock = if let Some(bid) = bid_opt {
        let clean = bid.trim_start_matches('#');
        detail
            .blocks
            .iter()
            .find(|b| {
                b.element_id
                    .as_deref()
                    .is_some_and(|eid| eid.trim_start_matches('#') == clean)
            })
            .with_context(|| format!("페이지에서 block_id '{bid}' 를 찾을 수 없습니다."))?
    } else {
        let idx = todo_index.unwrap_or(0);
        let todo_blocks: Vec<&OnenoteBlock> = detail
            .blocks
            .iter()
            .filter(|b| b.tag.as_ref().is_some_and(OnenoteTagKind::is_todo))
            .collect();
        if todo_blocks.is_empty() {
            bail!("이 페이지에는 토글할 OneNote to-do 체크박스가 없습니다.");
        }
        todo_blocks.get(idx).copied().with_context(|| {
            format!(
                "체크박스 인덱스 {idx} 가 범위를 벗어났습니다 (총 {}개)",
                todo_blocks.len()
            )
        })?
    };

    let (cmd, completed) = parser::onenote::build_toggle_todo_patch(target_block).context(
        "선택한 블록은 OneNote to-do 태그가 아니거나 element_id(?includeIDs=true)가 없습니다.",
    )?;

    graph
        .patch_onenote_page_commands(
            &detail.summary.scope,
            &detail.summary.id,
            std::slice::from_ref(&cmd),
        )
        .await?;

    let refreshed = graph
        .get_onenote_page_detail(
            &detail.summary.scope,
            &detail.summary.id,
            Some(detail.summary.clone()),
        )
        .await
        .ok();
    if let Some(ref d) = refreshed {
        let _ = storage.upsert_onenote_page_detail(d);
        let _ = storage.upsert_onenote_pages(std::slice::from_ref(&d.summary));
    }

    Ok(json!({
        "ok": true,
        "page_key": detail.summary.page_key,
        "page_id": detail.summary.id,
        "element_id": target_block.element_id,
        "completed": completed,
        "command": cmd,
        "page": refreshed,
    }))
}

pub(crate) async fn delete_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
) -> Result<serde_json::Value> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("삭제할 페이지 ID 또는 검색어가 비어 있습니다.");
    }
    let (page_key, scope, page_id) = if let Some(sum) = storage.get_onenote_page_summary(q)? {
        (sum.page_key, sum.scope, sum.id)
    } else if !q.contains('|') {
        let hits = storage.search_onenote_pages(q, None, 5)?;
        if let Some(hit) = pick_search_hit_for_title(&hits, q) {
            (hit.page_key.clone(), hit.scope.clone(), hit.page_id.clone())
        } else {
            let (s, id) = split_onenote_entity_key(q);
            (s.make_entity_key(&id), s, id)
        }
    } else {
        let (s, id) = split_onenote_entity_key(q);
        (s.make_entity_key(&id), s, id)
    };

    graph.delete_onenote_page(&scope, &page_id).await?;
    let _ = storage.delete_onenote_page(&page_key);
    if page_key != page_id {
        let _ = storage.delete_onenote_page(&page_id);
    }

    Ok(json!({
        "ok": true,
        "page_key": page_key,
        "page_id": page_id,
    }))
}

pub(crate) async fn search_onenote_pages_helper(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    section_query: Option<&str>,
    refresh: bool,
    limit: usize,
) -> Result<Vec<OnenoteSearchHit>> {
    let q = query.trim();
    if q.is_empty() {
        bail!("검색어가 비어 있습니다.");
    }
    let limit = clamp_limit(limit);

    if refresh {
        sync::bootstrap_onenote(graph, storage).await?;
    }

    let section_filter = match section_query.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sec_q) => {
            let sec = match resolve_onenote_section(storage, graph, sec_q, false, true).await {
                Ok(s) => s,
                Err(_) => resolve_onenote_section(storage, graph, sec_q, false, false).await?,
            };
            Some(sec.section_key)
        }
        None => None,
    };

    let hits = storage.search_onenote_pages(q, section_filter.as_deref(), limit)?;
    if !hits.is_empty() || refresh {
        return Ok(hits);
    }

    if storage.list_onenote_notebooks()?.is_empty() {
        let _ = sync::bootstrap_onenote(graph, storage).await;
        return storage.search_onenote_pages(q, section_filter.as_deref(), limit);
    }

    Ok(hits)
}

pub(crate) fn select_onenote_resource<'a>(
    detail: &'a OnenotePageDetail,
    index: Option<usize>,
    prefer_image: bool,
) -> Result<(usize, &'a OnenoteResource)> {
    if detail.resources.is_empty() {
        bail!(
            "이 OneNote 페이지에는 다운로드 가능한 리소스가 없습니다: {}",
            detail.summary.title
        );
    }
    if let Some(idx) = index {
        let res = detail.resources.get(idx).with_context(|| {
            format!(
                "리소스 인덱스 {idx} 가 범위를 벗어났습니다 (총 {}개)",
                detail.resources.len()
            )
        })?;
        return Ok((idx, res));
    }
    if prefer_image {
        if let Some((idx, res)) = detail
            .resources
            .iter()
            .enumerate()
            .find(|(_, r)| r.is_image)
        {
            return Ok((idx, res));
        }
    }
    Ok((0, &detail.resources[0]))
}

pub(crate) fn default_resource_filename(res: &OnenoteResource, index: usize) -> String {
    if let Some(name) = res.file_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let safe: String = name
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if !safe.is_empty() {
            return safe;
        }
    }
    let ext = match res.mime_type.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        _ => "bin",
    };
    format!("onenote-resource-{index}.{ext}")
}

fn spans_to_plain(spans: &[RichSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn format_onenote_blocks_human(detail: &OnenotePageDetail) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} ({})\n",
        detail.summary.title, detail.summary.page_key
    ));
    if let Some(ref div_id) = detail.primary_div_id {
        out.push_str(&format!("primary_div_id: {div_id}\n"));
    }
    for (idx, b) in detail.blocks.iter().enumerate() {
        let id_badge = b
            .element_id
            .as_deref()
            .map(|id| format!("[id={id}] "))
            .unwrap_or_default();
        let tag_badge = match &b.tag {
            Some(OnenoteTagKind::Todo { completed: true }) => "[x] ".to_string(),
            Some(OnenoteTagKind::Todo { completed: false }) => "[ ] ".to_string(),
            Some(other) => format!("[tag={}] ", other.data_tag_value()),
            None => String::new(),
        };
        let summary = match &b.kind {
            OnenoteBlockKind::Heading { level, spans } => {
                format!("H{level}: {}", spans_to_plain(spans))
            }
            OnenoteBlockKind::Paragraph { spans } => spans_to_plain(spans),
            OnenoteBlockKind::ListItem {
                ordered,
                index,
                depth,
                spans,
            } => {
                let bullet = if *ordered {
                    format!("{}.", index.unwrap_or(1))
                } else {
                    "-".to_string()
                };
                format!(
                    "{}{} {}",
                    "  ".repeat(*depth as usize),
                    bullet,
                    spans_to_plain(spans)
                )
            }
            OnenoteBlockKind::Quote { spans } => format!("> {}", spans_to_plain(spans)),
            OnenoteBlockKind::CodeBlock { lang, code } => {
                format!(
                    "```{}\n{}\n```",
                    lang.as_deref().unwrap_or_default(),
                    code
                )
            }
            OnenoteBlockKind::Table { headers, rows } => {
                format!("Table ({} cols × {} rows)", headers.len(), rows.len())
            }
            OnenoteBlockKind::Image {
                resource_index,
                alt,
                ..
            } => format!(
                "Image[#{resource_index}] {}",
                alt.as_deref().unwrap_or_default()
            ),
            OnenoteBlockKind::Attachment {
                resource_index,
                name,
                mime_type,
                ..
            } => format!("Attachment[#{resource_index}] {name} ({mime_type})"),
            OnenoteBlockKind::Divider => "---".to_string(),
        };
        out.push_str(&format!("{idx:02}: {id_badge}{tag_badge}{summary}\n"));
    }
    out
}

async fn cmd_notes_notebooks(
    cfg: &AppConfig,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let notebooks =
        load_onenote_notebooks(&storage, &graph, include_teams, refresh, offline).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "notebooks": notebooks }))?
        );
    } else if notebooks.is_empty() {
        println!("표시할 OneNote 필기장이 없습니다.");
    } else {
        for nb in &notebooks {
            let badge = if nb.scope.is_personal() {
                "개인".to_string()
            } else {
                format!("공유:{}", nb.scope.owner_label())
            };
            let def = if nb.is_default { " [기본]" } else { "" };
            println!(
                "[{}] {}{}  ({})",
                badge, nb.display_name, def, nb.notebook_key
            );
        }
    }
    Ok(())
}

async fn cmd_notes_create_notebook(
    cfg: &AppConfig,
    name: &str,
    group: Option<&str>,
    site: Option<&str>,
    json_out: bool,
) -> Result<()> {
    if name.trim().is_empty() {
        bail!("필기장 이름이 비어 있습니다.");
    }
    let scope = build_onenote_scope_from_args(group, site)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let created = graph.create_onenote_notebook(&scope, name.trim()).await?;
    storage.upsert_onenote_notebooks(std::slice::from_ref(&created))?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "notebook": created,
            }))?
        );
    } else {
        println!(
            "OneNote 필기장 생성 완료: {} ({})",
            created.display_name, created.notebook_key
        );
    }
    Ok(())
}

async fn cmd_notes_sections(
    cfg: &AppConfig,
    notebook: Option<&str>,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let (nb, sections) =
        load_onenote_sections(&storage, &graph, notebook, refresh, offline).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "notebook": nb,
                "sections": sections,
            }))?
        );
    } else if sections.is_empty() {
        println!("표시할 OneNote 섹션이 없습니다.");
    } else {
        for sec in &sections {
            let nb_label = sec.notebook_name.as_deref().unwrap_or("-");
            let sg_label = sec
                .parent_section_group_name
                .as_deref()
                .map(|g| format!("{g}/"))
                .unwrap_or_default();
            println!(
                "{}/{}{}  ({})",
                nb_label, sg_label, sec.display_name, sec.section_key
            );
        }
    }
    Ok(())
}

async fn cmd_notes_create_section(
    cfg: &AppConfig,
    notebook: &str,
    name: &str,
    json_out: bool,
) -> Result<()> {
    if name.trim().is_empty() {
        bail!("섹션 이름이 비어 있습니다.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let nb = resolve_onenote_notebook(&storage, &graph, notebook, true, false, false).await?;
    let mut created = graph
        .create_onenote_section(&nb.scope, &nb.id, name.trim())
        .await?;
    if created.notebook_id.is_none() {
        created.notebook_id = Some(nb.id.clone());
    }
    if created.notebook_name.is_none() {
        created.notebook_name = Some(nb.display_name.clone());
    }
    storage.upsert_onenote_sections(std::slice::from_ref(&created))?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "notebook": nb,
                "section": created,
            }))?
        );
    } else {
        println!(
            "OneNote 섹션 생성 완료: {}/{} ({})",
            nb.display_name, created.display_name, created.section_key
        );
    }
    Ok(())
}

async fn cmd_notes_pages(
    cfg: &AppConfig,
    section: &str,
    limit: usize,
    refresh: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let (sec, pages) =
        list_onenote_pages_helper(&storage, &graph, section, limit, refresh, offline).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "section": sec,
                "pages": pages,
            }))?
        );
    } else if pages.is_empty() {
        println!("표시할 OneNote 페이지가 없습니다.");
    } else {
        for p in &pages {
            let indent = "  ".repeat(p.level.clamp(0, 4) as usize);
            let todo_mark = if p.has_todo { "[todo] " } else { "" };
            let modified = p.last_modified_at.as_deref().unwrap_or("-");
            println!(
                "{indent}{todo_mark}{}  [{}] ({})",
                p.title, modified, p.page_key
            );
        }
    }
    Ok(())
}

async fn cmd_notes_read(
    cfg: &AppConfig,
    page: &str,
    format: &str,
    preview_images: bool,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let detail = load_onenote_page_detail(&storage, &graph, page, offline).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "format": format,
                "page": detail,
            }))?
        );
        return Ok(());
    }

    match format {
        "xhtml" => println!("{}", detail.raw_xhtml),
        "plain" => {
            println!("# {}\n", detail.summary.title);
            println!("{}", detail.body_plain);
        }
        "blocks" => print!("{}", format_onenote_blocks_human(&detail)),
        _ => {
            println!("# {}\n", detail.summary.title);
            println!("{}", detail.body_markdown);
        }
    }

    if preview_images && !offline {
        let (cols, rows) = default_preview_dimensions(None, None);
        for res in detail.resources.iter().filter(|r| r.is_image) {
            let url = res.fullres_url.as_deref().unwrap_or(&res.resource_url);
            if let Ok(bytes) = graph.download_onenote_resource_bytes(url).await {
                if let Ok(rendered) = img_preview::render_ansi_stdout(&bytes, cols, rows) {
                    println!();
                    print!("{rendered}");
                    if !rendered.ends_with('\n') {
                        println!();
                    }
                }
            }
        }
        io::stdout().flush()?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_notes_create(
    cfg: &AppConfig,
    section: &str,
    title: &str,
    markdown_body: Option<&str>,
    file: Option<&Path>,
    image: &[PathBuf],
    level: Option<i32>,
    json_out: bool,
) -> Result<()> {
    let (md, inline_images) = read_markdown_input_and_images(markdown_body, file, image)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let (sec, created) =
        create_onenote_page_helper(&storage, &graph, section, title, &md, inline_images, level)
            .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "section": sec,
                "page": created,
            }))?
        );
    } else {
        println!(
            "OneNote 페이지 생성 완료: {} ({})",
            created.title, created.page_key
        );
    }
    Ok(())
}

async fn cmd_notes_append(
    cfg: &AppConfig,
    page: &str,
    markdown_body: Option<&str>,
    file: Option<&Path>,
    image: &[PathBuf],
    json_out: bool,
) -> Result<()> {
    let (md, inline_images) = read_markdown_input_and_images(markdown_body, file, image)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let out = append_onenote_page_helper(&storage, &graph, page, &md, &inline_images).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "OneNote 페이지 내용 추가 완료: {}",
            out["page_key"].as_str().unwrap_or(page)
        );
    }
    Ok(())
}

async fn cmd_notes_update(
    cfg: &AppConfig,
    page: &str,
    title: Option<&str>,
    body: Option<&str>,
    file: Option<&Path>,
    block_id: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let (md, _) = read_markdown_input_and_images(body, file, &[])?;
    let md_opt = if body.is_some() || file.is_some() {
        Some(md.as_str())
    } else {
        None
    };
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let out =
        update_onenote_page_helper(&storage, &graph, page, title, md_opt, block_id).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "OneNote 페이지 수정 완료: {} ({}개 명령 적용)",
            out["page_key"].as_str().unwrap_or(page),
            out["commands_applied"].as_u64().unwrap_or(1)
        );
    }
    Ok(())
}

async fn cmd_notes_check(
    cfg: &AppConfig,
    page: &str,
    block_id: Option<&str>,
    index: Option<usize>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let out = toggle_onenote_todo_helper(&storage, &graph, page, block_id, index).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let state = if out["completed"].as_bool().unwrap_or(false) {
            "완료 [x]"
        } else {
            "미완료 [ ]"
        };
        println!(
            "OneNote 체크박스 토글 ({state}): {} ({})",
            out["element_id"].as_str().unwrap_or("-"),
            out["page_key"].as_str().unwrap_or(page)
        );
    }
    Ok(())
}

async fn cmd_notes_delete(
    cfg: &AppConfig,
    page: &str,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "OneNote 페이지 '{page}' 를 삭제합니다. 계속하려면 'yes' 입력: "
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    let out = delete_onenote_page_helper(&storage, &graph, page).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "OneNote 페이지 삭제 완료: {}",
            out["page_key"].as_str().unwrap_or(page)
        );
    }
    Ok(())
}

async fn cmd_notes_search(
    cfg: &AppConfig,
    query: &str,
    section: Option<&str>,
    refresh: bool,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, refresh).await?;
    let hits =
        search_onenote_pages_helper(&storage, &graph, query, section, refresh, limit).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "hits": hits }))?
        );
    } else if hits.is_empty() {
        println!("일치하는 OneNote 페이지가 없습니다.");
    } else {
        for h in &hits {
            let loc = match (&h.notebook_name, &h.section_name) {
                (Some(nb), Some(sec)) => format!("{nb}/{sec}"),
                (None, Some(sec)) => sec.clone(),
                _ => h.section_id.clone(),
            };
            println!(
                "[{}] {}  ({})  {}",
                loc,
                h.title,
                h.page_key,
                truncate_single_line(&h.snippet, 80)
            );
        }
    }
    Ok(())
}

async fn cmd_notes_resource(
    cfg: &AppConfig,
    page: &str,
    index: Option<usize>,
    out: Option<&Path>,
    preview: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let detail = load_onenote_page_detail(&storage, &graph, page, false).await?;
    let (resolved_idx, res) = select_onenote_resource(&detail, index, preview)?;
    let url = res.fullres_url.as_deref().unwrap_or(&res.resource_url);
    let bytes = graph.download_onenote_resource_bytes(url).await?;

    if preview && out.is_none() && !json_out {
        let (cols, rows) = default_preview_dimensions(None, None);
        let rendered = img_preview::render_ansi_stdout(&bytes, cols, rows)?;
        print!("{rendered}");
        if !rendered.ends_with('\n') {
            println!();
        }
        io::stdout().flush()?;
        return Ok(());
    }

    let file_name = default_resource_filename(res, resolved_idx);
    let target_path = match out {
        Some(p) if p.is_dir() => p.join(&file_name),
        Some(p) => p.to_path_buf(),
        None => cfg.resolved_download_dir().join(&file_name),
    };
    config::ensure_parent(&target_path)?;
    std::fs::write(&target_path, &bytes)
        .with_context(|| format!("OneNote 리소스 저장 실패: {}", target_path.display()))?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "page_key": detail.summary.page_key,
                "index": resolved_idx,
                "mime_type": res.mime_type,
                "file_name": file_name,
                "path": target_path,
                "bytes_written": bytes.len(),
            }))?
        );
    } else {
        println!(
            "OneNote 리소스 다운로드 완료: {} ({} bytes)",
            target_path.display(),
            bytes.len()
        );
        if preview && res.is_image {
            let (cols, rows) = default_preview_dimensions(None, None);
            if let Ok(rendered) = img_preview::render_ansi_stdout(&bytes, cols, rows) {
                print!("{rendered}");
                if !rendered.ends_with('\n') {
                    println!();
                }
            }
        }
    }
    Ok(())
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

        let outlook = OutlookCli::try_parse_from(["outlook"]).unwrap();
        assert!(outlook.command.is_none());
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
    fn parses_teams_and_outlook_persona_commands() {
        // teams update & preview
        let cli = Cli::try_parse_from([
            "teams",
            "update",
            "--check",
            "--force",
            "--repo",
            "org/repo",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Update {
                check,
                force,
                repo,
                json,
            } => {
                assert!(check);
                assert!(force);
                assert_eq!(repo.as_deref(), Some("org/repo"));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli = Cli::try_parse_from(["teams", "preview-file", "shot.png", "--cols", "80"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::PreviewFile { cols: Some(80), .. }
        ));

        // teams mail / cal / contacts / todo
        let cli = Cli::try_parse_from([
            "teams",
            "mail",
            "send",
            "--to",
            "a@b.com",
            "--subject",
            "주간 보고",
            "본문 **Markdown**",
            "--important",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Mail {
                command: MailCommand::Send { .. }
            }
        ));

        let cli = Cli::try_parse_from([
            "teams",
            "cal",
            "create",
            "--subject",
            "스탠드업",
            "--start",
            "2026-11-16_09:30",
            "--end",
            "2026-11-16_10:00",
            "--teams-meeting",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Cal {
                command: CalCommand::Create {
                    teams_meeting: true,
                    ..
                }
            }
        ));

        // outlook top-level persona commands
        let out = OutlookCli::try_parse_from([
            "outlook",
            "inbox",
            "--unread",
            "--limit",
            "15",
            "--offline",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Inbox {
                unread: true,
                limit: 15,
                offline: true,
                json: true,
                ..
            }
        ));

        let out = OutlookCli::try_parse_from([
            "outlook",
            "read",
            "mail-1",
            "--preview-images",
            "--offline",
        ])
        .unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Read {
                preview_images: true,
                offline: true,
                ..
            }
        ));

        let out = OutlookCli::try_parse_from([
            "outlook",
            "search",
            "인보이스",
            "--remote",
            "--limit",
            "20",
        ])
        .unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Search {
                remote: true,
                limit: 20,
                ..
            }
        ));

        let out = OutlookCli::try_parse_from([
            "outlook",
            "todo",
            "add",
            "릴리스 검토",
            "--due",
            "2026-11-20",
            "--important",
        ])
        .unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Todo {
                command: TodoCommand::Add {
                    important: true,
                    ..
                }
            }
        ));

        let out = OutlookCli::try_parse_from(["outlook", "chats", "--refresh"]).unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Chats { refresh: true, .. }
        ));
    }

    #[test]
    fn normalizes_dates_rsvp_and_resolves_folders_and_lists() {
        let iso = normalize_datetime_iso("2026-11-16_09:30").unwrap();
        assert!(iso.starts_with("2026-11-16T09:30:00"));
        assert_eq!(
            normalize_datetime_iso("2026-11-16T09:30:00Z").unwrap(),
            "2026-11-16T09:30:00+00:00"
        );
        assert_eq!(
            normalize_todo_due_iso("2026-11-20").unwrap(),
            "2026-11-20T00:00:00Z"
        );
        assert!(normalize_datetime_iso("not-a-date").is_err());

        assert_eq!(normalize_rsvp_response("accept").unwrap(), "accept");
        assert_eq!(
            normalize_rsvp_response("tentative").unwrap(),
            "tentativelyAccept"
        );
        assert_eq!(normalize_rsvp_response("DECLINE").unwrap(), "decline");
        assert!(normalize_rsvp_response("maybe").is_err());

        let folders = vec![
            MailFolder {
                id: "f-inbox".to_string(),
                display_name: "받은 편지함".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                unread_item_count: 2,
                total_item_count: 10,
            },
            MailFolder {
                id: "f-sent".to_string(),
                display_name: "보낸 편지함".to_string(),
                well_known_name: Some("sentitems".to_string()),
                parent_folder_id: None,
                unread_item_count: 0,
                total_item_count: 5,
            },
        ];
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "inbox").as_deref(),
            Some("f-inbox")
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "sent").as_deref(),
            Some("f-sent")
        );
        assert_eq!(
            resolve_mail_folder_from_slice(&folders, "보낸 편지함").as_deref(),
            Some("f-sent")
        );

        let lists = vec![
            TodoList {
                id: "l-1".to_string(),
                display_name: "Tasks".to_string(),
                wellknown_list_name: Some("defaultList".to_string()),
                is_owner: true,
                is_shared: false,
            },
            TodoList {
                id: "l-2".to_string(),
                display_name: "업무".to_string(),
                wellknown_list_name: None,
                is_owner: true,
                is_shared: false,
            },
        ];
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "default").unwrap().id,
            "l-1"
        );
        assert_eq!(
            resolve_todo_list_from_slice(&lists, "업무").unwrap().id,
            "l-2"
        );
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

    #[test]
    fn parses_onenote_persona_and_shared_notes_commands() {
        let cli = OnenoteCli::try_parse_from(["onenote"]).unwrap();
        assert!(cli.command.is_none());

        let cli = OnenoteCli::try_parse_from(["onenote", "ls", "--personal-only", "--json"]).unwrap();
        match cli.command.unwrap() {
            OnenoteCommand::Notebooks {
                personal_only,
                include_teams,
                json,
                ..
            } => {
                assert!(personal_only);
                assert!(include_teams);
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli = OnenoteCli::try_parse_from([
            "onenote", "create", "업무/회의", "--title", "주간 회의록", "배포 준비", "--image", "a.png",
            "--level", "1", "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            OnenoteCommand::Create {
                section,
                title,
                markdown_body,
                image,
                level,
                json,
                ..
            } => {
                assert_eq!(section, "업무/회의");
                assert_eq!(title, "주간 회의록");
                assert_eq!(markdown_body.as_deref(), Some("배포 준비"));
                assert_eq!(image, vec![PathBuf::from("a.png")]);
                assert_eq!(level, Some(1));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(OnenoteCli::try_parse_from(["onenote", "create", "s", "--title", "t", "--level", "3"]).is_err());

        let cli = OnenoteCli::try_parse_from(["onenote", "cat", "me|p1", "--format", "blocks"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            OnenoteCommand::Read { ref format, .. } if format == "blocks"
        ));
        assert!(OnenoteCli::try_parse_from(["onenote", "read", "p", "--format", "pdf"]).is_err());

        let cli = OnenoteCli::try_parse_from(["onenote", "check", "me|p1", "--index", "2"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            OnenoteCommand::Check { index: Some(2), block_id: None, .. }
        ));

        // `update <page>` edits a page; self-update is `self-update` / `upgrade`.
        let cli = OnenoteCli::try_parse_from([
            "onenote", "update", "me|p1", "--block-id", "p:{a}{1}", "--body", "교체",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            OnenoteCommand::Update { block_id: Some(_), body: Some(_), .. }
        ));
        let cli = OnenoteCli::try_parse_from(["onenote", "upgrade", "--check"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            OnenoteCommand::SelfUpdate { check: true, .. }
        ));
        let cli = OnenoteCli::try_parse_from(["onenote", "doctor", "--probe"]).unwrap();
        assert!(matches!(cli.command.unwrap(), OnenoteCommand::Doctor { probe: true, .. }));
        let cli = OnenoteCli::try_parse_from(["onenote", "api", "--port", "9100"]).unwrap();
        assert!(matches!(cli.command.unwrap(), OnenoteCommand::Serve { port: 9100, .. }));
        let cli = OnenoteCli::try_parse_from(["onenote", "mcp"]).unwrap();
        assert!(matches!(cli.command.unwrap(), OnenoteCommand::Mcp));

        // `teams notes <sub>` and `outlook notes <sub>` share NotesCommand.
        let cli = Cli::try_parse_from(["teams", "notes", "search", "배포", "--limit", "5", "--refresh"]).unwrap();
        match cli.command.unwrap() {
            Command::Notes {
                command: NotesCommand::Search { query, limit, refresh, .. },
            } => {
                assert_eq!(query, "배포");
                assert_eq!(limit, 5);
                assert!(refresh);
            }
            other => panic!("unexpected: {other:?}"),
        }
        let cli = OutlookCli::try_parse_from(["outlook", "notes", "pages", "업무/회의", "--offline"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            OutlookCommand::Notes {
                command: NotesCommand::Pages { offline: true, limit: 50, .. }
            }
        ));
        let cli = Cli::try_parse_from(["teams", "notes", "delete", "me|p1", "--yes", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Notes {
                command: NotesCommand::Delete { yes: true, json: true, .. }
            }
        ));
    }

    #[test]
    fn resolves_onenote_notebooks_and_sections_fuzzily() {
        let nb = |key: &str, id: &str, name: &str, scope: OnenoteScope| OnenoteNotebook {
            notebook_key: key.to_string(),
            id: id.to_string(),
            scope,
            display_name: name.to_string(),
            color: None,
            is_default: false,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        };
        let team = OnenoteScope::Group {
            group_id: "g1".to_string(),
            group_name: Some("플랫폼팀".to_string()),
        };
        let notebooks = vec![
            nb("me|nb1", "nb1", "업무", OnenoteScope::Me),
            nb("group:g1|nb2", "nb2", "업무", team.clone()),
            nb("me|nb3", "nb3", "개인 일기", OnenoteScope::Me),
        ];
        assert_eq!(resolve_onenote_notebook_from_slice(&notebooks, "group:g1|nb2").unwrap().id, "nb2");
        assert_eq!(resolve_onenote_notebook_from_slice(&notebooks, "nb3").unwrap().id, "nb3");
        // Duplicate name prefers the personal notebook; owner-qualified picks the team one.
        assert_eq!(resolve_onenote_notebook_from_slice(&notebooks, "업무").unwrap().id, "nb1");
        assert_eq!(resolve_onenote_notebook_from_slice(&notebooks, "플랫폼팀/업무").unwrap().id, "nb2");
        assert_eq!(resolve_onenote_notebook_from_slice(&notebooks, "일기").unwrap().id, "nb3");
        assert!(resolve_onenote_notebook_from_slice(&notebooks, "없는필기장").is_err());
        assert_eq!(filter_notebooks_by_scope(notebooks.clone(), false).len(), 2);

        let sec = |key: &str, id: &str, nb_name: &str, name: &str| OnenoteSection {
            section_key: key.to_string(),
            id: id.to_string(),
            scope: OnenoteScope::Me,
            notebook_id: None,
            notebook_name: Some(nb_name.to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: name.to_string(),
            color: None,
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        };
        let sections = vec![
            sec("me|s1", "s1", "업무", "회의"),
            sec("me|s2", "s2", "개인 일기", "회의"),
            sec("me|s3", "s3", "업무", "아이디어"),
        ];
        assert_eq!(resolve_onenote_section_from_slice(&sections, "업무/회의").unwrap().id, "s1");
        assert_eq!(resolve_onenote_section_from_slice(&sections, "me|s2").unwrap().id, "s2");
        assert_eq!(resolve_onenote_section_from_slice(&sections, "아이디어").unwrap().id, "s3");
        assert!(resolve_onenote_section_from_slice(&sections, "회의").is_err());

        assert!(build_onenote_scope_from_args(Some("g"), Some("s")).is_err());
        assert_eq!(build_onenote_scope_from_args(None, None).unwrap(), OnenoteScope::Me);
        assert_eq!(
            build_onenote_scope_from_args(Some(" g1 "), None).unwrap().scope_key(),
            "group:g1"
        );
    }

    #[test]
    fn unifies_cross_persona_subcommands_top_alias_and_env_help() {
        // 1. `outlook mail <subcmd>`
        let out = OutlookCli::try_parse_from(["outlook", "mail", "list", "--top", "5"]).unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Mail {
                command: MailCommand::List { limit: 5, .. }
            }
        ));
        let out = OutlookCli::try_parse_from(["outlook", "mail", "folders"]).unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Mail {
                command: MailCommand::Folders { .. }
            }
        ));

        // 2. `outlook inbox --top 5`
        let out = OutlookCli::try_parse_from(["outlook", "inbox", "--top", "5"]).unwrap();
        assert!(matches!(
            out.command.unwrap(),
            OutlookCommand::Inbox { limit: 5, .. }
        ));

        // 3. `teams mail list --top 5` & `teams inbox --top 5`
        let cli = Cli::try_parse_from(["teams", "mail", "list", "--top", "5"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Mail {
                command: MailCommand::List { limit: 5, .. }
            }
        ));
        let cli = Cli::try_parse_from(["teams", "inbox", "--top", "5"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Inbox { limit: 5, .. }
        ));

        // 4. `onenote notes notebooks` & `onenote mail list --top 5`
        let one = OnenoteCli::try_parse_from(["onenote", "notes", "notebooks"]).unwrap();
        assert!(matches!(
            one.command.unwrap(),
            OnenoteCommand::Notes {
                command: NotesCommand::Notebooks { .. }
            }
        ));
        let one = OnenoteCli::try_parse_from(["onenote", "mail", "list", "--top", "5"]).unwrap();
        assert!(matches!(
            one.command.unwrap(),
            OnenoteCommand::Mail {
                command: MailCommand::List { limit: 5, .. }
            }
        ));

        // 5. `--top` alias across all remaining commands with `limit`
        assert!(matches!(
            Cli::try_parse_from(["teams", "chats", "--top", "7"]).unwrap().command.unwrap(),
            Command::Chats { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "read", "c1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Read { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "thread", "c1", "m1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Thread { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            Command::Search { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "sync", "--top", "7"]).unwrap().command.unwrap(),
            Command::Sync { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "export", "c1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Export { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "mail-search", "q", "--top", "7"]).unwrap().command.unwrap(),
            Command::MailSearch { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "mail", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            Command::Mail { command: MailCommand::Search { limit: 7, .. } }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "cal", "list", "--top", "7"]).unwrap().command.unwrap(),
            Command::Cal { command: CalCommand::List { limit: 7, .. } }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "contacts", "list", "--top", "7"]).unwrap().command.unwrap(),
            Command::Contacts { command: ContactsCommand::List { limit: 7, .. } }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "contacts", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            Command::Contacts { command: ContactsCommand::Search { limit: 7, .. } }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "todo", "tasks", "--top", "7"]).unwrap().command.unwrap(),
            Command::Todo { command: TodoCommand::Tasks { limit: 7, .. } }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "notes", "pages", "s1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Notes { command: NotesCommand::Pages { limit: 7, .. } }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "notes", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            Command::Notes { command: NotesCommand::Search { limit: 7, .. } }
        ));
        assert!(matches!(
            OutlookCli::try_parse_from(["outlook", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            OutlookCommand::Search { limit: 7, .. }
        ));
        assert!(matches!(
            OutlookCli::try_parse_from(["outlook", "chats", "--top", "7"]).unwrap().command.unwrap(),
            OutlookCommand::Chats { limit: 7, .. }
        ));
        assert!(matches!(
            OnenoteCli::try_parse_from(["onenote", "pages", "s1", "--top", "7"]).unwrap().command.unwrap(),
            OnenoteCommand::Pages { limit: 7, .. }
        ));
        assert!(matches!(
            OnenoteCli::try_parse_from(["onenote", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            OnenoteCommand::Search { limit: 7, .. }
        ));

        // 6. `--help` renders key environment variables across all three personas
        let required_envs = [
            "TEAMS_PERSONA",
            "TEAMS_TOKEN_FILE",
            "TEAMS_TOKEN_REFRESH_CMD",
            "TEAMS_ACCESS_TOKEN",
            "TEAMS_DISABLE_KEYRING",
        ];
        let teams_help = <Cli as clap::CommandFactory>::command().render_help().to_string();
        let outlook_help = <OutlookCli as clap::CommandFactory>::command().render_help().to_string();
        let onenote_help = <OnenoteCli as clap::CommandFactory>::command().render_help().to_string();
        for env_name in required_envs {
            assert!(teams_help.contains(env_name), "teams help missing {env_name}");
            assert!(outlook_help.contains(env_name), "outlook help missing {env_name}");
            assert!(onenote_help.contains(env_name), "onenote help missing {env_name}");
        }
    }
}
