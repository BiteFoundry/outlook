//! Pure Outlook SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_USER_VERSION_UNICODE61: i32 = 60;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 61;

/// Initializes SQLite PRAGMAs, tables, indexes, and FTS5 virtual table.
/// Returns `true` if `mail_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
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
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='mail_fts')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='mail_messages')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_mail_messages_folder_received')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_calendar_events_start')",
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
                DROP TABLE IF EXISTS messages_fts;
                DROP TABLE IF EXISTS messages;
                DROP TABLE IF EXISTS conversations;
                DROP TABLE IF EXISTS teams;
                DROP TABLE IF EXISTS members;
                DROP TABLE IF EXISTS presences;
                DROP TABLE IF EXISTS onenote_fts;
                DROP TABLE IF EXISTS onenote_page_details;
                DROP TABLE IF EXISTS onenote_pages;
                DROP TABLE IF EXISTS onenote_sections;
                DROP TABLE IF EXISTS onenote_section_groups;
                DROP TABLE IF EXISTS onenote_notebooks;
                ",
            )
            .context("Failed to drop legacy Teams/OneNote tables")?;

            // Recreate sync_cursors if upgrading from legacy schema lacking `history_next_link`.
            if table_exists(conn, "sync_cursors") && !column_exists(conn, "sync_cursors", "history_next_link") {
                conn.execute_batch("DROP TABLE IF EXISTS sync_cursors;")
                    .context("Failed to upgrade sync_cursors schema")?;
            }
        }
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS mail_folders (
            id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            well_known_name TEXT,
            parent_folder_id TEXT,
            child_folder_count INTEGER NOT NULL DEFAULT 0,
            unread_item_count INTEGER NOT NULL DEFAULT 0,
            total_item_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS mail_messages (
            id TEXT PRIMARY KEY,
            internet_message_id TEXT,
            conversation_id TEXT,
            folder_id TEXT,
            subject TEXT NOT NULL,
            from_json TEXT,
            to_json TEXT NOT NULL,
            cc_json TEXT NOT NULL,
            bcc_json TEXT NOT NULL,
            reply_to_json TEXT NOT NULL DEFAULT '[]',
            from_display TEXT NOT NULL,
            received_at TEXT NOT NULL,
            sent_at TEXT,
            is_read INTEGER NOT NULL DEFAULT 0,
            is_draft INTEGER NOT NULL DEFAULT 0,
            flagged INTEGER NOT NULL DEFAULT 0,
            importance TEXT NOT NULL,
            inference_classification TEXT,
            has_attachments INTEGER NOT NULL DEFAULT 0,
            body_loaded INTEGER NOT NULL DEFAULT 0,
            preview TEXT NOT NULL,
            body_html TEXT NOT NULL,
            body_plain TEXT NOT NULL,
            attachments_json TEXT NOT NULL,
            meeting_request_json TEXT,
            web_link TEXT
        );

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

        CREATE TABLE IF NOT EXISTS sync_cursors (
            scope_key TEXT PRIMARY KEY,
            delta_link TEXT,
            history_next_link TEXT,
            last_synced_at TEXT NOT NULL
        );
        ",
    )
    .context("Failed to create Outlook SQLite schema")?;

    migrate_legacy_outlook_columns(conn)?;

    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_mail_messages_folder_received
            ON mail_messages(folder_id, received_at DESC, id DESC);

        CREATE INDEX IF NOT EXISTS idx_mail_messages_received
            ON mail_messages(received_at DESC, id DESC);

        CREATE INDEX IF NOT EXISTS idx_mail_messages_conv
            ON mail_messages(conversation_id, received_at DESC);
        ",
    )
    .context("Failed to create mail_messages indexes")?;

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

    if !is_memory {
        // Align FTS rowids with mail_messages.rowid when upgrading from pre-v60 schemas.
        let prev_ver: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);
        if prev_ver > 0 && prev_ver < SCHEMA_USER_VERSION_UNICODE61 {
            conn.execute_batch(
                "
                DELETE FROM mail_fts;
                INSERT INTO mail_fts(rowid, msg_id, folder_id, subject, from_display, body_plain)
                SELECT rowid, id, COALESCE(folder_id, ''), subject, from_display, body_plain
                FROM mail_messages;
                ",
            )
            .context("Failed to rebuild mail_fts rowids during upgrade")?;
        }

        let version = if mail_trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("Failed to set SQLite user_version")?;
    }

    Ok(mail_trigram)
}

fn table_exists(conn: &Connection, table_name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table_name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn column_exists(conn: &Connection, table_name: &str, col_name: &str) -> bool {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table_name}') WHERE name = ?1)");
    conn.query_row(&sql, [col_name], |row| row.get(0))
        .unwrap_or(false)
}

fn migrate_legacy_outlook_columns(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "mail_folders", "child_folder_count") {
        conn.execute_batch(
            "ALTER TABLE mail_folders ADD COLUMN child_folder_count INTEGER NOT NULL DEFAULT 0;",
        )
        .context("Failed to migrate mail_folders.child_folder_count")?;
    }
    let mail_migrations = [
        ("internet_message_id", "ALTER TABLE mail_messages ADD COLUMN internet_message_id TEXT;"),
        ("reply_to_json", "ALTER TABLE mail_messages ADD COLUMN reply_to_json TEXT NOT NULL DEFAULT '[]';"),
        ("inference_classification", "ALTER TABLE mail_messages ADD COLUMN inference_classification TEXT;"),
        ("body_loaded", "ALTER TABLE mail_messages ADD COLUMN body_loaded INTEGER NOT NULL DEFAULT 0;"),
        ("meeting_request_json", "ALTER TABLE mail_messages ADD COLUMN meeting_request_json TEXT;"),
    ];
    for (col, ddl) in mail_migrations {
        if !column_exists(conn, "mail_messages", col) {
            conn.execute_batch(ddl)
                .with_context(|| format!("Failed to migrate mail_messages.{col}"))?;
        }
    }
    Ok(())
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
        conn.execute_batch(unicode61_sql).with_context(|| {
            format!("Failed to create FTS5 virtual table ({table_name}, unicode61)")
        })?;
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

    const LEGACY_TEAMS_ONENOTE_TABLES: &[&str] = &[
        "conversations",
        "teams",
        "messages",
        "messages_fts",
        "members",
        "presences",
        "onenote_fts",
        "onenote_page_details",
        "onenote_pages",
        "onenote_sections",
        "onenote_section_groups",
        "onenote_notebooks",
    ];

    #[test]
    fn test_pure_outlook_schema_and_user_version_fast_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("outlook_fastpath.db");

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
            for required in [
                "mail_folders",
                "mail_messages",
                "mail_fts",
                "calendar_events",
                "contacts",
                "todo_lists",
                "todo_tasks",
                "sync_cursors",
            ] {
                assert!(table_exists(&conn, required), "{required} should exist");
            }
            for legacy in LEGACY_TEAMS_ONENOTE_TABLES {
                assert!(!table_exists(&conn, legacy), "legacy {legacy} must not exist");
            }
            trigram
        };

        let conn2 = Connection::open(&db_path).unwrap();
        assert_eq!(init_connection(&conn2, false).unwrap(), first_trigram);

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
    fn test_drops_legacy_teams_and_onenote_tables_on_upgrade() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("legacy_teams.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE conversations (conv_key TEXT PRIMARY KEY);
                CREATE TABLE teams (team_id TEXT PRIMARY KEY);
                CREATE TABLE messages (msg_key TEXT PRIMARY KEY);
                CREATE VIRTUAL TABLE messages_fts USING fts5(msg_key UNINDEXED, body_plain);
                CREATE TABLE members (conv_key TEXT, user_id TEXT);
                CREATE TABLE presences (user_id TEXT PRIMARY KEY);
                CREATE TABLE onenote_notebooks (notebook_key TEXT PRIMARY KEY);
                CREATE TABLE sync_cursors (scope_key TEXT PRIMARY KEY, delta_link TEXT, oldest_msg_ts TEXT, synced_at TEXT NOT NULL);
                PRAGMA user_version = 53;
                ",
            )
            .unwrap();
        }

        let conn = Connection::open(&db_path).unwrap();
        init_connection(&conn, false).unwrap();
        for legacy in LEGACY_TEAMS_ONENOTE_TABLES {
            assert!(!table_exists(&conn, legacy));
        }
        assert!(column_exists(&conn, "sync_cursors", "history_next_link"));
        assert!(column_exists(&conn, "mail_messages", "body_loaded"));
    }
}
