//! SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Initializes SQLite PRAGMAs, tables, indexes, and FTS5 virtual table.
/// Returns `true` if `messages_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
pub fn init_connection(conn: &Connection, is_memory: bool) -> Result<bool> {
    if !is_memory {
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        ",
    )
    .context("SQLite PRAGMA 설정 실패")?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS conversations (
            conv_key TEXT PRIMARY KEY,
            target_json TEXT NOT NULL,
            kind TEXT NOT NULL,
            team_id TEXT,
            team_name TEXT,
            title TEXT NOT NULL,
            topic TEXT,
            unread_count INTEGER NOT NULL DEFAULT 0,
            last_read_at TEXT,
            last_msg_id TEXT,
            last_msg_author TEXT,
            last_msg_preview TEXT,
            last_updated_at TEXT,
            pinned INTEGER NOT NULL DEFAULT 0,
            peer_user_id TEXT,
            web_url TEXT
        );

        CREATE TABLE IF NOT EXISTS teams (
            team_id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            description TEXT,
            channels_json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            msg_key TEXT PRIMARY KEY,
            conv_key TEXT NOT NULL,
            msg_id TEXT NOT NULL,
            parent_msg_id TEXT,
            reply_to_id TEXT,
            reply_to_author TEXT,
            reply_to_preview TEXT,
            subject TEXT,
            author_id TEXT,
            author_name TEXT NOT NULL,
            is_me INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            last_modified_at TEXT,
            deleted INTEGER NOT NULL DEFAULT 0,
            importance TEXT NOT NULL,
            message_type TEXT NOT NULL,
            body_html TEXT NOT NULL,
            body_plain TEXT NOT NULL,
            attachments_json TEXT NOT NULL,
            mentions_json TEXT NOT NULL,
            reactions_json TEXT NOT NULL,
            reply_count INTEGER NOT NULL DEFAULT 0,
            web_url TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_messages_conv_parent_created
            ON messages(conv_key, parent_msg_id, created_at);

        CREATE INDEX IF NOT EXISTS idx_messages_conv_msg
            ON messages(conv_key, msg_id);

        CREATE TABLE IF NOT EXISTS members (
            conv_key TEXT NOT NULL,
            user_id TEXT NOT NULL,
            member_id TEXT,
            display_name TEXT NOT NULL,
            email TEXT,
            roles_json TEXT NOT NULL,
            PRIMARY KEY (conv_key, user_id)
        );

        CREATE TABLE IF NOT EXISTS presences (
            user_id TEXT PRIMARY KEY,
            availability TEXT NOT NULL,
            activity TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_cursors (
            scope_key TEXT PRIMARY KEY,
            delta_link TEXT,
            oldest_msg_ts TEXT,
            synced_at TEXT NOT NULL
        );
        ",
    )
    .context("SQLite 기본 스키마 생성 실패")?;

    let trigram_sql = "
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            msg_key UNINDEXED,
            conv_key UNINDEXED,
            author_name,
            body_plain,
            subject,
            tokenize='trigram'
        );
    ";
    let unicode61_sql = "
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            msg_key UNINDEXED,
            conv_key UNINDEXED,
            author_name,
            body_plain,
            subject,
            tokenize='unicode61'
        );
    ";

    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql)
            .context("FTS5 가상 테이블 생성 실패 (unicode61)")?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'",
            [],
            |row| row.get(0),
        )
        .ok();
    let uses_trigram = sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true);
    Ok(uses_trigram)
}
