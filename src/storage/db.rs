//! SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_USER_VERSION_UNICODE61: i32 = 40;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 41;

/// Initializes SQLite PRAGMAs, tables, indexes, and FTS5 virtual table.
/// Returns `true` if `messages_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
pub fn init_connection(conn: &Connection, is_memory: bool) -> Result<bool> {
    conn.set_prepared_statement_cache_capacity(64);
    if !is_memory {
        let _ = conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA journal_size_limit = 67108864;
            ",
        );
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -8000;
        ",
    )
    .context("SQLite PRAGMA 설정 실패")?;
    if !is_memory {
        let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
        let user_version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);
        if user_version == SCHEMA_USER_VERSION_TRIGRAM
            || user_version == SCHEMA_USER_VERSION_UNICODE61
        {
            let intact: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='onenote_fts')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if intact {
                return Ok(user_version == SCHEMA_USER_VERSION_TRIGRAM);
            }
        }
    }

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

        CREATE INDEX IF NOT EXISTS idx_conversations_sort
            ON conversations(pinned DESC, last_updated_at DESC);

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

        CREATE INDEX IF NOT EXISTS idx_messages_created
            ON messages(created_at DESC);

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

        CREATE TABLE IF NOT EXISTS mail_folders (
            id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            well_known_name TEXT,
            parent_folder_id TEXT,
            unread_item_count INTEGER NOT NULL DEFAULT 0,
            total_item_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS mail_messages (
            id TEXT PRIMARY KEY,
            conversation_id TEXT,
            folder_id TEXT,
            subject TEXT NOT NULL,
            from_json TEXT,
            to_json TEXT NOT NULL,
            cc_json TEXT NOT NULL,
            bcc_json TEXT NOT NULL,
            from_display TEXT NOT NULL,
            received_at TEXT NOT NULL,
            sent_at TEXT,
            is_read INTEGER NOT NULL DEFAULT 0,
            is_draft INTEGER NOT NULL DEFAULT 0,
            flagged INTEGER NOT NULL DEFAULT 0,
            importance TEXT NOT NULL,
            has_attachments INTEGER NOT NULL DEFAULT 0,
            preview TEXT NOT NULL,
            body_html TEXT NOT NULL,
            body_plain TEXT NOT NULL,
            attachments_json TEXT NOT NULL,
            web_link TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_mail_messages_folder_received
            ON mail_messages(folder_id, received_at DESC);

        CREATE TABLE IF NOT EXISTS calendar_events (
            id TEXT PRIMARY KEY,
            subject TEXT NOT NULL,
            start_iso TEXT NOT NULL,
            end_iso TEXT NOT NULL,
            time_zone TEXT NOT NULL,
            is_all_day INTEGER NOT NULL DEFAULT 0,
            location TEXT,
            organizer_name TEXT,
            organizer_email TEXT,
            attendees_json TEXT NOT NULL,
            response_status TEXT NOT NULL,
            is_online_meeting INTEGER NOT NULL DEFAULT 0,
            online_meeting_url TEXT,
            body_html TEXT NOT NULL,
            body_plain TEXT NOT NULL,
            is_cancelled INTEGER NOT NULL DEFAULT 0,
            web_link TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_calendar_events_start
            ON calendar_events(start_iso ASC);

        CREATE TABLE IF NOT EXISTS contacts (
            id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            given_name TEXT,
            surname TEXT,
            emails_json TEXT NOT NULL,
            mobile_phone TEXT,
            business_phones_json TEXT NOT NULL,
            company_name TEXT,
            job_title TEXT,
            department TEXT,
            notes TEXT,
            source TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_contacts_display_name
            ON contacts(display_name COLLATE NOCASE ASC);

        CREATE TABLE IF NOT EXISTS todo_lists (
            id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            wellknown_list_name TEXT,
            is_owner INTEGER NOT NULL DEFAULT 1,
            is_shared INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS todo_tasks (
            id TEXT PRIMARY KEY,
            list_id TEXT NOT NULL,
            title TEXT NOT NULL,
            status TEXT NOT NULL,
            importance TEXT NOT NULL,
            body_plain TEXT,
            due_date_iso TEXT,
            reminder_iso TEXT,
            completed_at TEXT,
            created_at TEXT,
            updated_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_todo_tasks_list_status
            ON todo_tasks(list_id, status);

        CREATE TABLE IF NOT EXISTS onenote_notebooks (
            notebook_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            display_name TEXT NOT NULL,
            color TEXT,
            is_default INTEGER NOT NULL DEFAULT 0,
            is_shared INTEGER NOT NULL DEFAULT 0,
            user_role TEXT,
            created_at TEXT,
            last_modified_at TEXT,
            created_by TEXT,
            web_url TEXT
        );

        CREATE TABLE IF NOT EXISTS onenote_section_groups (
            section_group_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            notebook_id TEXT,
            parent_section_group_id TEXT,
            display_name TEXT NOT NULL,
            created_at TEXT,
            last_modified_at TEXT
        );

        CREATE TABLE IF NOT EXISTS onenote_sections (
            section_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            notebook_id TEXT,
            notebook_name TEXT,
            parent_section_group_id TEXT,
            parent_section_group_name TEXT,
            display_name TEXT NOT NULL,
            color TEXT,
            is_default INTEGER NOT NULL DEFAULT 0,
            pages_url TEXT,
            created_at TEXT,
            last_modified_at TEXT,
            web_url TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_onenote_sections_scope_notebook
            ON onenote_sections(scope_key, notebook_id);

        CREATE TABLE IF NOT EXISTS onenote_pages (
            page_key TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            section_key TEXT NOT NULL,
            section_id TEXT NOT NULL,
            section_name TEXT,
            notebook_id TEXT,
            notebook_name TEXT,
            title TEXT NOT NULL,
            level INTEGER NOT NULL DEFAULT 0,
            page_order INTEGER NOT NULL DEFAULT 0,
            created_at TEXT,
            last_modified_at TEXT,
            content_url TEXT,
            web_url TEXT,
            preview_text TEXT,
            has_todo INTEGER NOT NULL DEFAULT 0,
            detail_json TEXT,
            body_plain TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_onenote_pages_section_order
            ON onenote_pages(section_key, page_order ASC, last_modified_at DESC);

        CREATE INDEX IF NOT EXISTS idx_onenote_pages_id
            ON onenote_pages(id);
        ",
    )
    .context("SQLite 기본 스키마 생성 실패")?;

    let messages_trigram = ensure_fts5_table(
        conn,
        "messages_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            msg_key UNINDEXED,
            conv_key UNINDEXED,
            author_name,
            body_plain,
            subject,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            msg_key UNINDEXED,
            conv_key UNINDEXED,
            author_name,
            body_plain,
            subject,
            tokenize='unicode61'
        );
        ",
    )?;

    let mail_trigram = ensure_fts5_table(
        conn,
        "mail_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS mail_fts USING fts5(
            msg_id UNINDEXED,
            folder_id UNINDEXED,
            subject,
            from_display,
            body_plain,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS mail_fts USING fts5(
            msg_id UNINDEXED,
            folder_id UNINDEXED,
            subject,
            from_display,
            body_plain,
            tokenize='unicode61'
        );
        ",
    )?;

    let onenote_trigram = ensure_fts5_table(
        conn,
        "onenote_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS onenote_fts USING fts5(
            page_key UNINDEXED,
            section_key UNINDEXED,
            title,
            notebook_name,
            section_name,
            body_plain,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS onenote_fts USING fts5(
            page_key UNINDEXED,
            section_key UNINDEXED,
            title,
            notebook_name,
            section_name,
            body_plain,
            tokenize='unicode61'
        );
        ",
    )?;

    let all_trigram = messages_trigram && mail_trigram && onenote_trigram;
    if !is_memory {
        let version = if all_trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("SQLite user_version 설정 실패")?;
    }

    Ok(all_trigram)
}

/// Configures a read-only WAL reader connection for concurrent UI reads.
pub fn init_reader_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -4000;
        ",
    )
    .context("SQLite reader PRAGMA 설정 실패")?;
    let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
    Ok(())
}

/// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`).
pub fn shrink_connection_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql)
            .with_context(|| format!("FTS5 가상 테이블 생성 실패 ({table_name}, unicode61)"))?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .ok();
    let uses_trigram = sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true);
    Ok(uses_trigram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_user_version_fast_path_reopen_and_cache_caps() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fastpath.db");

        // First open initializes schema and stamps user_version
        let first_trigram = {
            let conn = Connection::open(&db_path).unwrap();
            let trigram = init_connection(&conn, false).unwrap();
            let ver: i32 = conn
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                ver,
                if trigram {
                    SCHEMA_USER_VERSION_TRIGRAM
                } else {
                    SCHEMA_USER_VERSION_UNICODE61
                }
            );
            let cache_size: i32 = conn
                .query_row("PRAGMA cache_size;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cache_size, -8000);
            trigram
        };

        // Second open takes user_version fast-path and preserves trigram_enabled
        {
            let conn = Connection::open(&db_path).unwrap();
            let second_trigram = init_connection(&conn, false).unwrap();
            assert_eq!(second_trigram, first_trigram);

            // Force unicode61 user_version stamp and verify fast-path returns false without running DDL
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_USER_VERSION_UNICODE61};"))
                .unwrap();
        }
        {
            let conn = Connection::open(&db_path).unwrap();
            assert!(!init_connection(&conn, false).unwrap());
        }

        // Reader connection uses -4000 cache cap
        let reader = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        init_reader_connection(&reader).unwrap();
        let reader_cache: i32 = reader
            .query_row("PRAGMA cache_size;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reader_cache, -4000);
    }
}
