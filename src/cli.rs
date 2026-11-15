//! `teams` / `outlook` dual-persona CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

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
        AttachmentInfo, CalendarEvent, ContactItem, ConversationKind, ConversationSummary,
        ConversationTarget, DownloadedFile, InlineImageUpload, MailAttachment, MailFolder,
        MailMessage, MentionInfo, MessageItem, OutgoingContact, OutgoingEvent, OutgoingMail,
        OutgoingTodoTask, QuotedReply, TeamSummary, TodoList,
    },
    parser, resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    sync, tui, updater,
};

const DELETE_JSON_REQUIRES_YES: &str = "--json 모드에서 삭제하려면 --yes 를 함께 지정해야 합니다.";

#[derive(Parser, Debug)]
#[command(
    name = "teams",
    version,
    about = "Microsoft Teams & Outlook 터미널 클라이언트 (CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 기반 Microsoft Teams & Outlook 터미널 클라이언트입니다.\n\
                  인자 없이 실행하면 로그인 상태를 확인한 뒤 TUI를 엽니다."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Parser, Debug)]
#[command(
    name = "outlook",
    version,
    about = "Microsoft Outlook 터미널 클라이언트 (메일 · 캘린더 · 연락처 · To Do · CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 기반 Microsoft Outlook / M365 터미널 클라이언트입니다.\n\
                  인자 없이 실행하면 로그인 상태를 확인한 뒤 Outlook TUI를 엽니다."
)]
pub struct OutlookCli {
    #[command(subcommand)]
    pub command: Option<OutlookCommand>,
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
    /// Outlook 메일 명령 (`teams mail <subcmd>`)
    Mail {
        #[command(subcommand)]
        command: MailCommand,
    },
    /// Outlook 메일 검색 (`teams mail search` 단축 명령)
    #[command(name = "mail-search")]
    MailSearch {
        query: String,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        remote: bool,
        #[arg(long, default_value_t = 30)]
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
        #[arg(long, default_value_t = 30)]
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
        #[arg(long, default_value_t = 30)]
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
        #[arg(long, default_value_t = 50)]
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
        #[arg(long, default_value_t = 30)]
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
        #[arg(long, default_value_t = 30)]
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
        #[arg(long, default_value_t = 50)]
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
        #[arg(long, default_value_t = 50)]
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
        #[arg(long, default_value_t = 30)]
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
        #[arg(long, default_value_t = 50)]
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
        Some(OutlookCommand::Cal { command }) => Box::pin(dispatch_cal_command(cfg, command)).await,
        Some(OutlookCommand::Contacts { command }) => {
            Box::pin(dispatch_contacts_command(cfg, command)).await
        }
        Some(OutlookCommand::Todo { command }) => Box::pin(dispatch_todo_command(cfg, command)).await,
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
    let Some(tokens) = auth.current_tokens().await else {
        bail!(
            "로그인이 필요합니다. 먼저 `{} login` 을 실행하세요.",
            persona.bin_name()
        );
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
                "persona": persona,
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
            if tokens.is_expired_soon(0) {
                "만료됨"
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

    let keyring_status = describe_keyring_status(tokens.as_ref(), &cred_path);

    let (
        cached_conversations,
        cached_mail_folders,
        cached_mails,
        cached_events,
        cached_contacts,
        cached_todo_lists,
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
            )
        } else {
            (0, 0, 0, 0, 0, 0)
        }
    } else {
        (0, 0, 0, 0, 0, 0)
    };

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
                "logged_in": tokens.is_some(),
                "cache_counts": {
                    "conversations": cached_conversations,
                    "mail_folders": cached_mail_folders,
                    "mails": cached_mails,
                    "events": cached_events,
                    "contacts": cached_contacts,
                    "todo_lists": cached_todo_lists,
                },
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
            "cache_counts: conversations={cached_conversations}, mail_folders={cached_mail_folders}, mails={cached_mails}, events={cached_events}, contacts={cached_contacts}, todo_lists={cached_todo_lists}"
        );
        println!("download_dir: {}", dl_dir.display());
        println!("github_repo: {}", cfg.github_repo);
        println!("scopes ({}): {}", scopes.len(), scopes.join(", "));
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
}
