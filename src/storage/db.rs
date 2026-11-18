//! SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_USER_VERSION_UNICODE61: i32 = 50;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 51;

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
    .context("Failed to configure SQLite PRAGMAs")?;
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
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages_fts')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_messages_conv_root_created_id')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if intact {
                return Ok(user_version == SCHEMA_USER_VERSION_TRIGRAM);
            }
        }
        if user_version < SCHEMA_USER_VERSION_UNICODE61 {
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS mail_fts;
                DROP TABLE IF EXISTS mail_messages;
                DROP TABLE IF EXISTS mail_folders;
                DROP TABLE IF EXISTS calendar_events;
                DROP TABLE IF EXISTS contacts;
                DROP TABLE IF EXISTS todo_tasks;
                DROP TABLE IF EXISTS todo_lists;
                DROP TABLE IF EXISTS onenote_fts;
                DROP TABLE IF EXISTS onenote_page_details;
                DROP TABLE IF EXISTS onenote_pages;
                DROP TABLE IF EXISTS onenote_sections;
                DROP TABLE IF EXISTS onenote_section_groups;
                DROP TABLE IF EXISTS onenote_notebooks;
                ",
            )
            .context("Failed to drop legacy Outlook/OneNote tables")?;
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

        CREATE INDEX IF NOT EXISTS idx_messages_conv_root_created_id
            ON messages(conv_key, parent_msg_id, created_at DESC, msg_id DESC);

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
        ",
    )
    .context("Failed to create base SQLite schema")?;

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

    if !is_memory {
        let version = if messages_trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("Failed to set SQLite user_version")?;
    }

    Ok(messages_trigram)
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
    .context("Failed to configure SQLite reader PRAGMAs")?;
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
            .with_context(|| format!("Failed to create FTS5 virtual table ({table_name}, unicode61)"))?;
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

    const LEGACY_TABLES: &[&str] = &[
        "mail_fts",
        "mail_messages",
        "mail_folders",
        "calendar_events",
        "contacts",
        "todo_tasks",
        "todo_lists",
        "onenote_fts",
        "onenote_page_details",
        "onenote_pages",
        "onenote_sections",
        "onenote_section_groups",
        "onenote_notebooks",
    ];

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn test_pure_teams_schema_and_user_version_fast_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fastpath.db");

        // First open initializes pure-Teams schema and stamps user_version
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

            for required in [
                "conversations",
                "teams",
                "messages",
                "members",
                "presences",
                "sync_cursors",
                "messages_fts",
            ] {
                assert!(table_exists(&conn, required), "{required} should exist");
            }
            assert!(index_exists(&conn, "idx_messages_conv_root_created_id"));
            for legacy in LEGACY_TABLES {
                assert!(
                    !table_exists(&conn, legacy),
                    "legacy table {legacy} must not exist"
                );
            }
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

    #[test]
    fn test_legacy_outlook_onenote_tables_dropped_on_upgrade() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("legacy_v41.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE conversations (
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
                INSERT INTO conversations (conv_key, target_json, kind, title)
                VALUES ('chat:c1', '{\"target_type\":\"chat\",\"chat_id\":\"c1\"}', 'oneOnOne', 'Alice');

                CREATE TABLE mail_folders (id TEXT PRIMARY KEY);
                CREATE TABLE mail_messages (id TEXT PRIMARY KEY);
                CREATE VIRTUAL TABLE mail_fts USING fts5(msg_id UNINDEXED, body_plain);
                CREATE TABLE calendar_events (id TEXT PRIMARY KEY);
                CREATE TABLE contacts (id TEXT PRIMARY KEY);
                CREATE TABLE todo_lists (id TEXT PRIMARY KEY);
                CREATE TABLE todo_tasks (id TEXT PRIMARY KEY);
                CREATE TABLE onenote_notebooks (notebook_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_section_groups (section_group_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_sections (section_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_pages (page_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_page_details (page_key TEXT PRIMARY KEY);
                CREATE VIRTUAL TABLE onenote_fts USING fts5(page_key UNINDEXED, body_plain);
                PRAGMA user_version = 41;
                ",
            )
            .unwrap();
            for legacy in LEGACY_TABLES {
                assert!(table_exists(&conn, legacy), "setup expected {legacy}");
            }
        }

        // Upgrade via init_connection drops all legacy tables while keeping Teams data intact
        {
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
            for legacy in LEGACY_TABLES {
                assert!(
                    !table_exists(&conn, legacy),
                    "legacy table {legacy} should have been dropped on upgrade"
                );
            }
            assert!(index_exists(&conn, "idx_messages_conv_root_created_id"));
            let title: String = conn
                .query_row(
                    "SELECT title FROM conversations WHERE conv_key = 'chat:c1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(title, "Alice");
        }
    }
}
