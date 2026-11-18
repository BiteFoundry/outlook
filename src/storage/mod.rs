//! Local SQLite WAL persistence + 4-reader pool + FTS5 (`trigram` + `LIKE` hybrid) search repository.

pub mod db;
pub mod outlook;
pub mod search;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;

use crate::models::{
    CalendarEvent, ContactItem, MailFolder, MailMessage, MailSearchHit, SyncCursor, TodoList,
    TodoTask,
};

const MAX_READER_POOL_SIZE: usize = 4;

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    writer: Arc<Mutex<rusqlite::Connection>>,
    readers: Option<Arc<Mutex<Vec<rusqlite::Connection>>>>,
    trigram_enabled: bool,
}

enum ReadConnInner<'a> {
    Pooled {
        conn: Option<rusqlite::Connection>,
        pool: &'a Arc<Mutex<Vec<rusqlite::Connection>>>,
    },
    Writer(std::sync::MutexGuard<'a, rusqlite::Connection>),
}

struct ReadConnGuard<'a>(ReadConnInner<'a>);

impl std::ops::Deref for ReadConnGuard<'_> {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            ReadConnInner::Pooled { conn, .. } => conn
                .as_ref()
                .expect("pooled reader connection available until Drop"),
            ReadConnInner::Writer(guard) => guard,
        }
    }
}

impl Drop for ReadConnGuard<'_> {
    fn drop(&mut self) {
        if let ReadConnInner::Pooled { conn, pool } = &mut self.0 {
            if let Some(c) = conn.take() {
                if let Ok(mut guard) = pool.lock() {
                    if guard.len() < MAX_READER_POOL_SIZE {
                        guard.push(c);
                    }
                }
            }
        }
    }
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        crate::config::ensure_parent(path)?;
        let conn = rusqlite::Connection::open(path)?;
        let trigram_enabled = db::init_connection(&conn, false)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(Mutex::new(conn)),
            readers: Some(Arc::new(Mutex::new(Vec::with_capacity(
                MAX_READER_POOL_SIZE,
            )))),
            trigram_enabled,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let trigram_enabled = db::init_connection(&conn, true)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            writer: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    /// Opens an existing database with a single read-only connection without creating an empty DB file.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(path, flags)?;
        db::init_reader_connection(&conn)?;
        let trigram_enabled: bool = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'mail_fts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|s| s.to_ascii_lowercase().contains("trigram"))
            .unwrap_or(false);
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.writer
    }

    pub fn trigram_enabled(&self) -> bool {
        self.trigram_enabled
    }

    /// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`) and runs `PRAGMA optimize`.
    pub fn shrink_memory(&self) {
        if let Ok(conn) = self.writer.lock() {
            let _ = conn.execute_batch("PRAGMA optimize;");
            db::shrink_connection_memory(&conn);
        }
        if let Some(pool) = &self.readers {
            if let Ok(guard) = pool.lock() {
                for c in guard.iter() {
                    db::shrink_connection_memory(c);
                }
            }
        }
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire SQLite connection lock (poisoned mutex)"))
    }

    fn read_conn(&self) -> Result<ReadConnGuard<'_>> {
        if let Some(pool) = &self.readers {
            let existing = {
                let mut guard = pool.lock().map_err(|_| {
                    anyhow::anyhow!("Failed to acquire SQLite reader pool lock (poisoned mutex)")
                })?;
                guard.pop()
            };
            let conn = match existing {
                Some(c) => c,
                None => {
                    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let c = rusqlite::Connection::open_with_flags(&self.path, flags)?;
                    db::init_reader_connection(&c)?;
                    c
                }
            };
            Ok(ReadConnGuard(ReadConnInner::Pooled {
                conn: Some(conn),
                pool,
            }))
        } else {
            Ok(ReadConnGuard(ReadConnInner::Writer(self.lock_conn()?)))
        }
    }

    // --- Mail ---

    pub fn upsert_mail_folders(&self, folders: &[MailFolder]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_mail_folders(&mut conn, folders)
    }

    pub fn list_mail_folders(&self) -> Result<Vec<MailFolder>> {
        let conn = self.read_conn()?;
        outlook::list_mail_folders(&conn)
    }

    pub fn find_well_known_folder_id(&self, well_known: &str) -> Result<Option<String>> {
        let conn = self.read_conn()?;
        outlook::find_well_known_folder_id(&conn, well_known)
    }

    pub fn upsert_mail_messages(&self, msgs: &[MailMessage]) -> Result<usize> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_mail_messages(&mut conn, msgs)
    }

    pub fn list_mail_messages(
        &self,
        folder_id: Option<&str>,
        unread_only: bool,
        flagged_only: bool,
        focused_only: bool,
        limit: usize,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_messages(&conn, folder_id, unread_only, flagged_only, focused_only, limit)
    }

    pub fn list_mail_summaries(
        &self,
        folder_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_summaries(&conn, folder_id, limit)
    }

    pub fn get_mail_message(&self, id: &str) -> Result<Option<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::get_mail_message(&conn, id)
    }

    pub fn set_mail_read_local(&self, id: &str, is_read: bool) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::set_mail_read_local(&mut conn, id, is_read)
    }

    pub fn set_mail_flagged_local(&self, id: &str, flagged: bool) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::set_mail_flagged_local(&conn, id, flagged)
    }

    pub fn move_mail_folder_local(&self, id: &str, target_folder_id: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::move_mail_folder_local(&mut conn, id, target_folder_id)
    }

    pub fn delete_mail_messages_by_ids(&self, ids: &[String]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::delete_mail_messages_by_ids(&mut conn, ids)
    }

    pub fn search_mail(
        &self,
        query: &str,
        folder_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailSearchHit>> {
        let conn = self.read_conn()?;
        search::search_mail(&conn, query, folder_id, limit, self.trigram_enabled)
    }

    // --- Calendar ---

    pub fn upsert_calendar_events(&self, events: &[CalendarEvent]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_calendar_events(&mut conn, events)
    }

    pub fn list_calendar_events(
        &self,
        start_iso: Option<&str>,
        end_iso: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CalendarEvent>> {
        let conn = self.read_conn()?;
        outlook::list_calendar_events(&conn, start_iso, end_iso, limit)
    }

    pub fn get_calendar_event(&self, id: &str) -> Result<Option<CalendarEvent>> {
        let conn = self.read_conn()?;
        outlook::get_calendar_event(&conn, id)
    }

    pub fn update_event_response_local(&self, id: &str, response_status: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::update_event_response_local(&conn, id, response_status)
    }

    pub fn delete_calendar_event(&self, id: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::delete_calendar_event(&conn, id)
    }

    pub fn delete_calendar_events_by_ids(&self, ids: &[String]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::delete_calendar_events_by_ids(&mut conn, ids)
    }

    // --- Contacts ---

    pub fn upsert_contacts(&self, contacts: &[ContactItem]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_contacts(&mut conn, contacts)
    }

    pub fn list_contacts(&self, query: Option<&str>, limit: usize) -> Result<Vec<ContactItem>> {
        let conn = self.read_conn()?;
        outlook::list_contacts(&conn, query, limit)
    }

    pub fn delete_contact(&self, id: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::delete_contact(&conn, id)
    }

    // --- To Do ---

    pub fn upsert_todo_lists(&self, lists: &[TodoList]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_todo_lists(&mut conn, lists)
    }

    pub fn list_todo_lists(&self) -> Result<Vec<TodoList>> {
        let conn = self.read_conn()?;
        outlook::list_todo_lists(&conn)
    }

    pub fn upsert_todo_tasks(&self, tasks: &[TodoTask]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_todo_tasks(&mut conn, tasks)
    }

    pub fn list_todo_tasks(
        &self,
        list_id: Option<&str>,
        include_completed: bool,
    ) -> Result<Vec<TodoTask>> {
        let conn = self.read_conn()?;
        outlook::list_todo_tasks(&conn, list_id, include_completed)
    }

    pub fn set_todo_task_completed_local(&self, task_id: &str, completed: bool) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::set_todo_task_completed_local(&conn, task_id, completed)
    }

    pub fn delete_todo_task(&self, task_id: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::delete_todo_task(&conn, task_id)
    }

    // --- Sync Cursors ---

    pub fn get_sync_cursor(&self, scope_key: &str) -> Result<Option<SyncCursor>> {
        let conn = self.read_conn()?;
        outlook::get_sync_cursor(&conn, scope_key)
    }

    pub fn upsert_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        let conn = self.lock_conn()?;
        outlook::upsert_sync_cursor(&conn, cursor)
    }

    pub fn save_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        self.upsert_sync_cursor(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EmailRecipient, EventAttendee, MailAttachment};

    fn sample_mail(
        id: &str,
        folder_id: &str,
        subject: &str,
        from_name: &str,
        from_addr: &str,
        received_at: &str,
        body_loaded: bool,
        body_html: &str,
        body_plain: &str,
    ) -> MailMessage {
        MailMessage {
            id: id.to_string(),
            internet_message_id: Some(format!("<{id}@example.com>")),
            conversation_id: Some(format!("conv-{id}")),
            folder_id: Some(folder_id.to_string()),
            subject: subject.to_string(),
            from: Some(EmailRecipient {
                name: Some(from_name.to_string()),
                address: from_addr.to_string(),
            }),
            to_recipients: vec![EmailRecipient {
                name: Some("Me".to_string()),
                address: "me@example.com".to_string(),
            }],
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            reply_to: Vec::new(),
            received_at: received_at.to_string(),
            sent_at: Some(received_at.to_string()),
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            inference_classification: Some("focused".to_string()),
            has_attachments: false,
            body_loaded,
            preview: body_plain.chars().take(40).collect(),
            body_html: body_html.to_string(),
            body_plain: body_plain.to_string(),
            attachments: Vec::new(),
            meeting_request: None,
            web_link: Some(format!("https://outlook.office.com/mail/{id}")),
        }
    }

    #[test]
    fn test_header_first_upsert_preserves_hydrated_body_and_fts5_search() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[
                MailFolder {
                    id: "fld-inbox".to_string(),
                    display_name: "Inbox".to_string(),
                    well_known_name: Some("inbox".to_string()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 2,
                    total_item_count: 5,
                },
                MailFolder {
                    id: "fld-archive".to_string(),
                    display_name: "Archive".to_string(),
                    well_known_name: Some("archive".to_string()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 1,
                },
            ])
            .unwrap();

        assert_eq!(
            storage.find_well_known_folder_id("inbox").unwrap().as_deref(),
            Some("fld-inbox")
        );
        assert_eq!(
            storage.find_well_known_folder_id("archive").unwrap().as_deref(),
            Some("fld-archive")
        );

        let mut full_msg = sample_mail(
            "m1",
            "fld-inbox",
            "Q4 Budget Review",
            "Alice",
            "alice@example.com",
            "2026-11-18T10:00:00Z",
            true,
            "<p>Full body with Kubernetes rollout details</p>",
            "Full body with Kubernetes rollout details",
        );
        full_msg.preview = "Full body with...".to_string();
        full_msg.has_attachments = true;
        full_msg.attachments = vec![MailAttachment {
            id: "att-1".to_string(),
            name: "budget.xlsx".to_string(),
            content_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                .to_string(),
            size: 4096,
            is_inline: false,
            content_id: None,
            content_bytes_b64: None,
        }];

        assert_eq!(storage.upsert_mail_messages(&[full_msg.clone()]).unwrap(), 1);

        // Identical re-upsert returns 0 changed rows and performs 0 SQLite changes.
        let before_noop = storage.conn().lock().unwrap().total_changes();
        assert_eq!(storage.upsert_mail_messages(&[full_msg]).unwrap(), 0);
        assert_eq!(storage.conn().lock().unwrap().total_changes() - before_noop, 0);

        // Header-only delta update (`body_loaded == false`) must NOT overwrite cached body/attachments,
        // while updating mutable metadata (`is_read`, `flagged`, `importance`, `inference_classification`, `preview`).
        let mut header_update = sample_mail(
            "m1",
            "fld-inbox",
            "Q4 Budget Review",
            "Alice",
            "alice@example.com",
            "2026-11-18T10:00:00Z",
            false,
            "",
            "",
        );
        header_update.is_read = true;
        header_update.flagged = true;
        header_update.importance = "high".to_string();
        header_update.inference_classification = Some("other".to_string());
        header_update.preview = "Updated preview".to_string();
        header_update.to_recipients.clear();
        header_update.has_attachments = false;

        assert_eq!(storage.upsert_mail_messages(&[header_update.clone()]).unwrap(), 1);
        // Repeating the exact same header-only poll returns 0 changed rows.
        assert_eq!(storage.upsert_mail_messages(&[header_update]).unwrap(), 0);

        let loaded = storage.get_mail_message("m1").unwrap().unwrap();
        assert!(loaded.body_loaded);
        assert!(loaded.is_read);
        assert!(loaded.flagged);
        assert_eq!(loaded.importance, "high");
        assert_eq!(loaded.inference_classification.as_deref(), Some("other"));
        assert_eq!(loaded.preview, "Updated preview");
        assert_eq!(
            loaded.body_html,
            "<p>Full body with Kubernetes rollout details</p>"
        );
        assert_eq!(
            loaded.body_plain,
            "Full body with Kubernetes rollout details"
        );
        assert_eq!(loaded.attachments.len(), 1);
        assert_eq!(loaded.attachments[0].name, "budget.xlsx");
        assert_eq!(loaded.to_recipients.len(), 1);

        // FTS5 search still finds the preserved full body & respects filters
        let hits = storage.search_mail("Kubernetes", Some("inbox"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m1");
        assert!(hits[0].flagged);
        assert!(hits[0].is_read);

        // Focused filter excludes `other`
        let focused = storage
            .list_mail_messages(Some("inbox"), false, false, true, 10)
            .unwrap();
        assert!(focused.is_empty());

        let flagged = storage
            .list_mail_messages(Some("inbox"), false, true, false, 10)
            .unwrap();
        assert_eq!(flagged.len(), 1);

        // Delete removes both row and FTS index via O(1) rowid path
        storage
            .delete_mail_messages_by_ids(&["m1".to_string()])
            .unwrap();
        assert!(storage.get_mail_message("m1").unwrap().is_none());
        assert!(storage
            .search_mail("Kubernetes", None, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_file_backed_reader_pool_concurrent_with_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("outlook-concurrent.db");
        let storage = Storage::open(&db_file).unwrap();

        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 1,
                total_item_count: 1,
            }])
            .unwrap();

        let m1 = sample_mail(
            "m-wal",
            "fld-inbox",
            "WAL Concurrent Read",
            "Bob",
            "bob@example.com",
            "2026-11-18T12:00:00Z",
            true,
            "<p>Concurrent WAL reader verification body</p>",
            "Concurrent WAL reader verification body",
        );
        storage.upsert_mail_messages(&[m1]).unwrap();

        {
            let _writer_guard = storage.conn().lock().unwrap();
            std::thread::scope(|s| {
                for _ in 0..6 {
                    s.spawn(|| {
                        let folders = storage.list_mail_folders().unwrap();
                        assert_eq!(folders.len(), 1);
                        let msgs = storage
                            .list_mail_messages(Some("inbox"), false, false, false, 10)
                            .unwrap();
                        assert_eq!(msgs.len(), 1);
                        assert_eq!(msgs[0].id, "m-wal");
                        let hits = storage.search_mail("verification", None, 10).unwrap();
                        assert_eq!(hits.len(), 1);
                    });
                }
            });
        }

        let pooled_count = storage.readers.as_ref().unwrap().lock().unwrap().len();
        assert!((1..=MAX_READER_POOL_SIZE).contains(&pooled_count));

        // Open readonly handle and shrink_memory work cleanly
        let ro = Storage::open_readonly(&db_file).unwrap();
        assert_eq!(ro.get_mail_message("m-wal").unwrap().unwrap().subject, "WAL Concurrent Read");
        storage.shrink_memory();
    }

    #[test]
    fn test_calendar_contacts_todo_and_cursors() {
        let storage = Storage::open_in_memory().unwrap();

        let ev = CalendarEvent {
            id: "ev-1".to_string(),
            subject: "Design Sync".to_string(),
            start_iso: "2026-11-18T15:00:00Z".to_string(),
            end_iso: "2026-11-18T15:30:00Z".to_string(),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: Some("Room 1".to_string()),
            organizer_name: Some("Alice".to_string()),
            organizer_email: Some("alice@example.com".to_string()),
            attendees: vec![EventAttendee {
                name: Some("Bob".to_string()),
                address: "bob@example.com".to_string(),
                attendee_type: "required".to_string(),
                response_status: "accepted".to_string(),
            }],
            response_status: "notResponded".to_string(),
            is_online_meeting: true,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        };
        storage.upsert_calendar_events(&[ev]).unwrap();
        storage
            .update_event_response_local("ev-1", "accepted")
            .unwrap();
        let loaded_ev = storage.get_calendar_event("ev-1").unwrap().unwrap();
        assert_eq!(loaded_ev.response_status, "accepted");

        storage
            .upsert_contacts(&[ContactItem {
                id: "c1".to_string(),
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
        assert_eq!(storage.list_contacts(Some("contoso"), 10).unwrap().len(), 1);

        storage
            .upsert_todo_lists(&[TodoList {
                id: "list-1".to_string(),
                display_name: "Tasks".to_string(),
                wellknown_list_name: Some("defaultList".to_string()),
                is_owner: true,
                is_shared: false,
            }])
            .unwrap();
        storage
            .upsert_todo_tasks(&[TodoTask {
                id: "t-1".to_string(),
                list_id: "list-1".to_string(),
                title: "Ship Outlook".to_string(),
                status: "notStarted".to_string(),
                importance: "high".to_string(),
                body_plain: None,
                due_date_iso: None,
                reminder_iso: None,
                completed_at: None,
                created_at: None,
                updated_at: None,
            }])
            .unwrap();
        storage.set_todo_task_completed_local("t-1", true).unwrap();
        assert!(storage.list_todo_tasks(None, false).unwrap().is_empty());
        assert_eq!(storage.list_todo_tasks(Some("list-1"), true).unwrap().len(), 1);

        let cursor = SyncCursor {
            scope_key: "mail_delta:inbox".to_string(),
            delta_link: Some("https://graph.microsoft.com/v1.0/delta?t=1".to_string()),
            history_next_link: None,
            last_synced_at: "2026-11-18T15:00:00Z".to_string(),
        };
        storage.upsert_sync_cursor(&cursor).unwrap();
        assert_eq!(
            storage.get_sync_cursor("mail_delta:inbox").unwrap(),
            Some(cursor)
        );
    }
}
