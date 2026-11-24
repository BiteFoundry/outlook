//! Local SQLite WAL persistence + 4-reader pool + FTS5 (`trigram` + `LIKE` hybrid) search repository.

pub mod db;
pub mod outlook;
pub mod search;

use std::{
    collections::HashSet,
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

    pub fn replace_mail_folders(&self, folders: &[MailFolder]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::replace_mail_folders(&mut conn, folders)
    }

    /// Stores a Graph folder listing. A complete listing replaces the cached tree (folders missing from
    /// it are deleted); a partial one is merged so a throttled or failed sub-request never deletes folders.
    pub fn store_mail_folder_listing(&self, folders: &[MailFolder], complete: bool) -> Result<()> {
        if complete {
            self.replace_mail_folders(folders)
        } else {
            self.upsert_mail_folders(folders)
        }
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

    pub fn list_mail_messages_before(
        &self,
        folder_id: Option<&str>,
        unread_only: bool,
        flagged_only: bool,
        focused_only: bool,
        limit: usize,
        before: Option<(&str, &str)>,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_messages_before(
            &conn,
            folder_id,
            unread_only,
            flagged_only,
            focused_only,
            limit,
            before,
        )
    }

    pub fn list_mail_summaries(
        &self,
        folder_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_summaries(&conn, folder_id, limit)
    }

    pub fn list_mail_summaries_filtered(
        &self,
        folder_id: Option<&str>,
        unread_only: bool,
        flagged_only: bool,
        focused_only: bool,
        limit: usize,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_summaries_filtered(
            &conn,
            folder_id,
            unread_only,
            flagged_only,
            focused_only,
            limit,
        )
    }

    pub fn list_mail_summaries_before(
        &self,
        folder_id: Option<&str>,
        unread_only: bool,
        flagged_only: bool,
        focused_only: bool,
        limit: usize,
        before: Option<(&str, &str)>,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_summaries_before(
            &conn,
            folder_id,
            unread_only,
            flagged_only,
            focused_only,
            limit,
            before,
        )
    }

    pub fn list_smart_folder_summaries(
        &self,
        unread_only: bool,
        flagged_only: bool,
        excluded_folder_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_smart_folder_summaries(
            &conn,
            unread_only,
            flagged_only,
            excluded_folder_id,
            limit,
        )
    }

    pub(crate) fn list_mail_summaries_through(
        &self,
        folder_id: &str,
        received_at: &str,
        id: &str,
    ) -> Result<Vec<MailMessage>> {
        let conn = self.read_conn()?;
        outlook::list_mail_summaries_through(&conn, folder_id, received_at, id)
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

    pub fn replace_moved_mail_local(&self, old_id: &str, moved: &MailMessage) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::replace_moved_mail_local(&mut conn, old_id, moved)
    }

    pub fn replace_moved_mail_local_ex(
        &self,
        old_id: &str,
        moved: &MailMessage,
        attachments_expanded: bool,
    ) -> Result<MailMessage> {
        let mut conn = self.lock_conn()?;
        outlook::replace_moved_mail_local_ex(&mut conn, old_id, moved, attachments_expanded)
    }

    pub fn apply_mail_folder_delta_page(
        &self,
        folder_id: &str,
        folder_alias: Option<&str>,
        incoming: &[MailMessage],
        removed_ids: &[String],
    ) -> Result<(usize, usize)> {
        let mut conn = self.lock_conn()?;
        outlook::apply_mail_folder_delta_page(
            &mut conn,
            folder_id,
            folder_alias,
            incoming,
            removed_ids,
        )
    }

    pub fn apply_mail_folder_delta(
        &self,
        folder_id: &str,
        folder_alias: Option<&str>,
        incoming: &[MailMessage],
        removed_ids: &[String],
        reconcile_seen_ids: Option<&HashSet<String>>,
        cursor: &SyncCursor,
    ) -> Result<(usize, usize)> {
        let mut conn = self.lock_conn()?;
        outlook::apply_mail_folder_delta(
            &mut conn,
            folder_id,
            folder_alias,
            incoming,
            removed_ids,
            reconcile_seen_ids,
            cursor,
        )
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

    pub fn replace_calendar_events_in_window(
        &self,
        start_iso: &str,
        end_iso: &str,
        events: &[CalendarEvent],
    ) -> Result<usize> {
        let mut conn = self.lock_conn()?;
        outlook::replace_calendar_events_in_window(&mut conn, start_iso, end_iso, events)
    }

    pub fn apply_calendar_delta(
        &self,
        start_iso: &str,
        end_iso: &str,
        events: &[CalendarEvent],
        removed_ids: &[String],
        reconcile_window: bool,
        cursor: &SyncCursor,
    ) -> Result<bool> {
        let mut conn = self.lock_conn()?;
        outlook::apply_calendar_delta(
            &mut conn,
            start_iso,
            end_iso,
            events,
            removed_ids,
            reconcile_window,
            cursor,
        )
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

    pub fn replace_personal_contacts(&self, contacts: &[ContactItem]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::replace_personal_contacts(&mut conn, contacts)
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

    pub fn replace_todo_lists(&self, lists: &[TodoList]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::replace_todo_lists(&mut conn, lists)
    }

    pub fn list_todo_lists(&self) -> Result<Vec<TodoList>> {
        let conn = self.read_conn()?;
        outlook::list_todo_lists(&conn)
    }

    pub fn upsert_todo_tasks(&self, tasks: &[TodoTask]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::upsert_todo_tasks(&mut conn, tasks)
    }

    pub fn replace_todo_tasks_for_list(
        &self,
        list_id: &str,
        include_completed: bool,
        tasks: &[TodoTask],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        outlook::replace_todo_tasks_for_list(&mut conn, list_id, include_completed, tasks)
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
            last_modified_at: None,
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
            ..Default::default()
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

        // Metadata-only header delta update (`body_loaded == false`, same subject/preview/attachments)
        // must NOT overwrite cached body/attachments while updating mutable metadata
        // (`is_read`, `flagged`, `importance`, `inference_classification`).
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
        header_update.preview = "Full body with...".to_string();
        header_update.to_recipients.clear();
        header_update.has_attachments = true;

        assert_eq!(storage.upsert_mail_messages(&[header_update.clone()]).unwrap(), 1);
        // Repeating the exact same header-only poll returns 0 changed rows.
        assert_eq!(storage.upsert_mail_messages(&[header_update]).unwrap(), 0);

        let loaded = storage.get_mail_message("m1").unwrap().unwrap();
        assert!(loaded.body_loaded);
        assert!(loaded.is_read);
        assert!(loaded.flagged);
        assert_eq!(loaded.importance, "high");
        assert_eq!(loaded.inference_classification.as_deref(), Some("other"));
        assert_eq!(loaded.preview, "Full body with...");
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

        // Content-changing header update (subject or preview changed) invalidates `body_loaded`
        // so stale v1 body is not served alongside v2 summary metadata.
        let mut content_update = sample_mail(
            "m1",
            "fld-inbox",
            "Q4 Budget Review v2",
            "Alice",
            "alice@example.com",
            "2026-11-18T10:00:00Z",
            false,
            "",
            "",
        );
        content_update.is_read = true;
        content_update.flagged = true;
        content_update.importance = "high".to_string();
        content_update.inference_classification = Some("other".to_string());
        content_update.preview = "Updated v2 body preview".to_string();
        content_update.has_attachments = true;

        assert_eq!(storage.upsert_mail_messages(&[content_update]).unwrap(), 1);
        let invalidated = storage.get_mail_message("m1").unwrap().unwrap();
        assert!(!invalidated.body_loaded);
        assert_eq!(invalidated.subject, "Q4 Budget Review v2");
        assert_eq!(invalidated.preview, "Updated v2 body preview");
        assert!(invalidated.body_html.is_empty());
        assert_eq!(invalidated.body_plain, "Updated v2 body preview");

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
            .search_mail("Updated", None, 10)
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
            ..Default::default()
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

    fn calendar_event_at(id: &str, start: chrono::DateTime<chrono::Utc>) -> CalendarEvent {
        let fmt = |t: chrono::DateTime<chrono::Utc>| {
            t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };
        CalendarEvent {
            id: id.to_string(),
            subject: id.to_string(),
            start_iso: fmt(start),
            end_iso: fmt(start + chrono::Duration::hours(1)),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: Vec::new(),
            response_status: "none".to_string(),
            is_online_meeting: false,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
            ..Default::default()
        }
    }

    #[test]
    fn calendar_rows_cached_without_offset_are_normalized_on_read() {
        let storage = Storage::open_in_memory().unwrap();
        {
            let conn = storage.conn().lock().unwrap();
            for (id, start, end, zone, all_day) in [
                ("legacy", "2026-11-23T05:00:00.0000000", "2026-11-23T06:00:00.0000000", "UTC", 0),
                ("all-day", "2026-11-23T00:00:00.0000000", "2026-11-24T00:00:00.0000000", "UTC", 1),
                ("zoned", "2026-11-23T09:00:00.0000000", "2026-11-23T10:00:00.0000000", "Asia/Seoul", 0),
            ] {
                conn.execute(
                    "INSERT INTO calendar_events (
                        id, subject, start_iso, end_iso, time_zone, is_all_day,
                        attendees_json, response_status, body_html, body_plain
                    ) VALUES (?1, ?1, ?2, ?3, ?4, ?5, '[]', 'none', '', '')",
                    rusqlite::params![id, start, end, zone, all_day],
                )
                .unwrap();
            }
        }

        let legacy = storage.get_calendar_event("legacy").unwrap().unwrap();
        assert_eq!(legacy.start_iso, "2026-11-23T05:00:00Z");
        assert_eq!(legacy.end_iso, "2026-11-23T06:00:00Z");
        let all_day = storage.get_calendar_event("all-day").unwrap().unwrap();
        assert_eq!(all_day.start_iso, "2026-11-23T00:00:00.0000000");
        let zoned = storage.get_calendar_event("zoned").unwrap().unwrap();
        assert_eq!(zoned.start_iso, "2026-11-23T09:00:00.0000000");

        let listed = storage.list_calendar_events(None, None, 10).unwrap();
        let legacy_listed = listed.iter().find(|e| e.id == "legacy").unwrap();
        assert_eq!(legacy_listed.start_iso, "2026-11-23T05:00:00Z");
    }

    #[test]
    fn calendar_cache_window_start_keeps_upcoming_events_ahead_of_old_history() {
        let storage = Storage::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let mut events: Vec<CalendarEvent> = (0..250)
            .map(|i| {
                calendar_event_at(
                    &format!("old-{i:03}"),
                    now - chrono::Duration::days(60 + i),
                )
            })
            .collect();
        for day in 1..=3 {
            events.push(calendar_event_at(
                &format!("upcoming-{day}"),
                now + chrono::Duration::days(day),
            ));
        }
        storage.upsert_calendar_events(&events).unwrap();

        // Without a lower bound the 200-row cap is filled by history and hides everything upcoming.
        let unbounded = storage.list_calendar_events(None, None, 200).unwrap();
        assert_eq!(unbounded.len(), 200);
        assert!(unbounded.iter().all(|e| e.id.starts_with("old-")));

        let start = crate::sync::calendar_cache_window_start();
        let windowed = storage.list_calendar_events(Some(&start), None, 200).unwrap();
        let ids: Vec<&str> = windowed.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["upcoming-1", "upcoming-2", "upcoming-3"]);
    }

    #[test]
    fn test_list_mail_summaries_filtered_projection_order_and_first_recipient() {
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
                    total_item_count: 4,
                },
                MailFolder {
                    id: "fld-sent".to_string(),
                    display_name: "Sent Items".to_string(),
                    well_known_name: Some("sentitems".to_string()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 1,
                },
            ])
            .unwrap();

        let cols_lower = outlook::MAIL_SUMMARY_SELECT_COLS.to_ascii_lowercase();
        for forbidden in [
            "body_html",
            "body_plain",
            "attachments_json",
            "cc_json",
            "bcc_json",
            "reply_to_json",
        ] {
            assert!(
                !cols_lower.contains(forbidden),
                "MAIL_SUMMARY_SELECT_COLS must not select {forbidden}: {}",
                outlook::MAIL_SUMMARY_SELECT_COLS
            );
        }

        let make = |id: &str, folder: &str, ts: &str, read: bool, flagged: bool, focused: bool| {
            let mut m = sample_mail(
                id,
                folder,
                &format!("Subject {id}"),
                "Alice",
                "alice@example.com",
                ts,
                true,
                &format!("<p>Large body for {id} {}</p>", "X".repeat(2048)),
                &format!("Large plain for {id}"),
            );
            m.is_read = read;
            m.flagged = flagged;
            m.inference_classification = Some(if focused { "focused" } else { "other" }.into());
            m.to_recipients = vec![
                EmailRecipient {
                    name: Some("Bob First".into()),
                    address: "bob@example.com".into(),
                },
                EmailRecipient {
                    name: Some("Carol Second".into()),
                    address: "carol@example.com".into(),
                },
            ];
            m.cc_recipients = vec![EmailRecipient {
                name: Some("Dan Cc".into()),
                address: "dan@example.com".into(),
            }];
            m.has_attachments = true;
            m.attachments = vec![MailAttachment {
                id: format!("att-{id}"),
                name: "deck.pdf".into(),
                content_type: "application/pdf".into(),
                size: 1234,
                is_inline: false,
                content_id: None,
                content_bytes_b64: Some("QUJD".into()),
            }];
            m.web_link = Some(format!("https://outlook.office.com/mail/{id}"));
            m
        };

        // 2 older unread+flagged messages and 3 newer read+unflagged messages
        let msgs = vec![
            make("m1", "fld-inbox", "2026-11-10T10:00:00Z", false, true, true),
            make("m2", "fld-inbox", "2026-11-11T10:00:00Z", false, true, false),
            make("m3", "fld-inbox", "2026-11-12T10:00:00Z", true, false, true),
            make("m4", "fld-inbox", "2026-11-13T10:00:00Z", true, false, true),
            make("m5", "fld-sent", "2026-11-14T10:00:00Z", true, false, true),
        ];
        storage.upsert_mail_messages(&msgs).unwrap();

        // Filters and ORDER BY received_at DESC must apply BEFORE LIMIT 2
        let unread = storage
            .list_mail_summaries_filtered(None, true, false, false, 2)
            .unwrap();
        let unread_ids: Vec<&str> = unread.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(unread_ids, vec!["m2", "m1"]);

        let flagged_focused = storage
            .list_mail_summaries_filtered(Some("inbox"), false, true, true, 2)
            .unwrap();
        assert_eq!(flagged_focused.len(), 1);
        assert_eq!(flagged_focused[0].id, "m1");

        // Summary omits heavy columns, preserves body_loaded and web_link, and keeps ONLY the first to_recipient
        let sent = storage
            .list_mail_summaries_filtered(Some("sentitems"), false, false, false, 10)
            .unwrap();
        assert_eq!(sent.len(), 1);
        let s = &sent[0];
        assert_eq!(s.id, "m5");
        assert!(s.body_loaded);
        assert!(s.has_attachments);
        assert!(s.body_html.is_empty());
        assert!(s.body_plain.is_empty());
        assert!(s.attachments.is_empty());
        assert!(s.cc_recipients.is_empty());
        assert_eq!(
            s.web_link.as_deref(),
            Some("https://outlook.office.com/mail/m5")
        );
        assert_eq!(s.to_recipients.len(), 1);
        assert_eq!(s.to_recipients[0].short_display(), "Bob First");

        // Full lookup still loads complete body, recipients, attachments, and matching web_link
        let full = storage.get_mail_message("m5").unwrap().unwrap();
        assert!(full.body_html.contains("Large body for m5"));
        assert_eq!(full.to_recipients.len(), 2);
        assert_eq!(full.cc_recipients.len(), 1);
        assert_eq!(full.attachments.len(), 1);
        assert_eq!(full.web_link, s.web_link);
    }

    #[test]
    fn test_replace_moved_mail_local_atomic_fts_and_late_source_delete_preservation() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .replace_mail_folders(&[
                MailFolder {
                    id: "fld-inbox".into(),
                    display_name: "Inbox".into(),
                    well_known_name: Some("inbox".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 2,
                    total_item_count: 2,
                },
                MailFolder {
                    id: "fld-archive".into(),
                    display_name: "Archive".into(),
                    well_known_name: Some("archive".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 0,
                },
            ])
            .unwrap();

        let m1 = sample_mail(
            "m1",
            "fld-inbox",
            "Unique Quantum Budget",
            "Alice",
            "alice@example.com",
            "2026-11-18T10:00:00Z",
            true,
            "<p>Quantum details</p>",
            "Quantum details",
        );
        let m2 = sample_mail(
            "m2",
            "fld-inbox",
            "Keep Same Id Move",
            "Bob",
            "bob@example.com",
            "2026-11-18T09:00:00Z",
            true,
            "<p>Preserved body</p>",
            "Preserved body",
        );
        storage.upsert_mail_messages(&[m1, m2]).unwrap();

        // 1. Move m1 -> m1-moved into Archive (Graph ID change): atomic replace of SQLite + FTS + counts
        let mut m1_moved = sample_mail(
            "m1-moved",
            "fld-archive",
            "Unique Quantum Budget",
            "Alice",
            "alice@example.com",
            "2026-11-18T10:00:00Z",
            false,
            "",
            "",
        );
        m1_moved.preview = "Quantum details".into();
        storage.replace_moved_mail_local("m1", &m1_moved).unwrap();

        assert!(storage.get_mail_message("m1").unwrap().is_none());
        let moved_loaded = storage.get_mail_message("m1-moved").unwrap().unwrap();
        assert_eq!(moved_loaded.folder_id.as_deref(), Some("fld-archive"));
        assert!(moved_loaded.body_loaded, "should preserve hydrated body across move");
        assert_eq!(moved_loaded.body_html, "<p>Quantum details</p>");
        let fts_hits = storage.search_mail("Quantum", None, 10).unwrap();
        assert_eq!(fts_hits.len(), 1);
        assert_eq!(fts_hits[0].id, "m1-moved");
        assert_eq!(fts_hits[0].folder_id.as_deref(), Some("fld-archive"));
        let folders = storage.list_mail_folders().unwrap();
        let inbox = folders.iter().find(|f| f.id == "fld-inbox").unwrap();
        let archive = folders.iter().find(|f| f.id == "fld-archive").unwrap();
        assert_eq!(inbox.total_item_count, 1);
        assert_eq!(inbox.unread_item_count, 1);
        assert_eq!(archive.total_item_count, 1);
        assert_eq!(archive.unread_item_count, 1);

        // 2. Move m2 -> Archive keeping same ID, then a late source-folder @removed arrives for m2 in fld-inbox:
        // m2 in fld-archive MUST be preserved!
        storage.move_mail_folder_local("m2", "fld-archive").unwrap();
        let cursor = SyncCursor {
            scope_key: "mail_delta:fld-inbox".into(),
            delta_link: Some("https://graph.microsoft.com/v1.0/delta?t=2".into()),
            history_next_link: None,
            last_synced_at: "2026-11-18T10:05:00Z".into(),
        };
        let (_changed, deleted) = storage
            .apply_mail_folder_delta(
                "fld-inbox",
                Some("inbox"),
                &[],
                &["m2".to_string()],
                None,
                &cursor,
            )
            .unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(
            storage.get_mail_message("m2").unwrap().unwrap().folder_id.as_deref(),
            Some("fld-archive")
        );
        assert_eq!(storage.search_mail("Preserved", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn test_selective_body_invalidation_and_non_delta_snapshot_reconciliation() {
        let storage = Storage::open_in_memory().unwrap();
        let mut msg = sample_mail(
            "m-mod",
            "fld-inbox",
            "Draft Subject",
            "Alice",
            "alice@example.com",
            "2026-11-18T10:00:00Z",
            true,
            "<p>Long body v1</p>",
            "Long body v1",
        );
        msg.last_modified_at = Some("2026-11-18T10:00:00Z".into());
        storage.upsert_mail_messages(&[msg.clone()]).unwrap();

        // Same-timestamp metadata-only header update preserves hydrated body
        let mut same_ts_meta = msg.clone();
        same_ts_meta.body_loaded = false;
        same_ts_meta.body_html.clear();
        same_ts_meta.body_plain.clear();
        same_ts_meta.is_read = true;
        storage.upsert_mail_messages(&[same_ts_meta.clone()]).unwrap();
        let after_same_ts = storage.get_mail_message("m-mod").unwrap().unwrap();
        assert!(after_same_ts.body_loaded);
        assert!(after_same_ts.is_read);
        assert_eq!(after_same_ts.body_html, "<p>Long body v1</p>");

        // Late/older summary must NOT regress newer metadata or invalidate hydrated body
        let mut older_delta = same_ts_meta.clone();
        older_delta.is_read = false;
        older_delta.last_modified_at = Some("2026-11-18T09:59:00Z".into());
        assert_eq!(storage.upsert_mail_messages(&[older_delta]).unwrap(), 0);
        let after_older = storage.get_mail_message("m-mod").unwrap().unwrap();
        assert!(after_older.body_loaded);
        assert!(after_older.is_read);
        assert_eq!(
            after_older.last_modified_at.as_deref(),
            Some("2026-11-18T10:00:00Z")
        );
        assert_eq!(after_older.body_html, "<p>Long body v1</p>");

        // Combined read-state change + strictly newer last_modified_at (same subject/preview)
        // MUST mark body_loaded=false so open mail re-checks its body
        let mut combined_edit = same_ts_meta;
        combined_edit.is_read = false;
        combined_edit.last_modified_at = Some("2026-11-18T10:01:00Z".into());
        assert_eq!(storage.upsert_mail_messages(&[combined_edit.clone()]).unwrap(), 1);
        let after_combined = storage.get_mail_message("m-mod").unwrap().unwrap();
        assert!(!after_combined.body_loaded);
        assert!(!after_combined.is_read);
        assert!(after_combined.body_html.is_empty());

        // Re-hydrating v2 body then re-receiving the exact same delta keeps body_loaded=true (0 changes)
        let mut hydrated_v2 = msg.clone();
        hydrated_v2.is_read = false;
        hydrated_v2.last_modified_at = Some("2026-11-18T10:01:00Z".into());
        hydrated_v2.body_html = "<p>Long body v2</p>".into();
        hydrated_v2.body_plain = "Long body v2".into();
        storage.upsert_mail_messages(&[hydrated_v2]).unwrap();
        assert_eq!(storage.upsert_mail_messages(&[combined_edit.clone()]).unwrap(), 0);
        let after_repeat = storage.get_mail_message("m-mod").unwrap().unwrap();
        assert!(after_repeat.body_loaded);
        assert_eq!(after_repeat.body_html, "<p>Long body v2</p>");

        // last_modified_at bump WITHOUT read/flag/folder change (e.g. body edit past 255 chars) invalidates body
        let mut deep_body_edit = combined_edit;
        deep_body_edit.last_modified_at = Some("2026-11-18T10:02:00Z".into());
        storage.upsert_mail_messages(&[deep_body_edit]).unwrap();
        let after_edit = storage.get_mail_message("m-mod").unwrap().unwrap();
        assert!(!after_edit.body_loaded);
        assert!(after_edit.body_html.is_empty());

        // Non-delta snapshot reconciliation for folders, todo lists/tasks, personal contacts, calendar window
        storage
            .replace_mail_folders(&[
                MailFolder {
                    id: "f1".into(),
                    display_name: "Inbox".into(),
                    well_known_name: Some("inbox".into()),
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 0,
                },
                MailFolder {
                    id: "f2".into(),
                    display_name: "Temp".into(),
                    well_known_name: None,
                    parent_folder_id: None,
                    child_folder_count: 0,
                    unread_item_count: 0,
                    total_item_count: 0,
                },
            ])
            .unwrap();
        storage
            .replace_mail_folders(&[MailFolder {
                id: "f1".into(),
                display_name: "Inbox".into(),
                well_known_name: Some("inbox".into()),
                parent_folder_id: None,
                child_folder_count: 0,
                unread_item_count: 0,
                total_item_count: 0,
            }])
            .unwrap();
        assert_eq!(storage.list_mail_folders().unwrap().len(), 1);

        // A partial Graph listing is merged: the folder it failed to list survives.
        let partial_listing = [MailFolder {
            id: "f3".into(),
            display_name: "Projects".into(),
            well_known_name: None,
            parent_folder_id: Some("f1".into()),
            child_folder_count: 0,
            unread_item_count: 0,
            total_item_count: 0,
        }];
        storage.store_mail_folder_listing(&partial_listing, false).unwrap();
        let mut ids: Vec<String> =
            storage.list_mail_folders().unwrap().into_iter().map(|f| f.id).collect();
        ids.sort();
        assert_eq!(ids, ["f1", "f3"]);
        // The same listing reported complete replaces the tree.
        storage.store_mail_folder_listing(&partial_listing, true).unwrap();
        let ids: Vec<String> =
            storage.list_mail_folders().unwrap().into_iter().map(|f| f.id).collect();
        assert_eq!(ids, ["f3"]);

        storage
            .replace_todo_lists(&[TodoList {
                id: "l1".into(),
                display_name: "Tasks".into(),
                wellknown_list_name: Some("defaultList".into()),
                is_owner: true,
                is_shared: false,
            }])
            .unwrap();
        storage
            .replace_todo_tasks_for_list(
                "l1",
                true,
                &[
                    TodoTask {
                        id: "t1".into(),
                        list_id: "l1".into(),
                        title: "Keep".into(),
                        status: "notStarted".into(),
                        importance: "normal".into(),
                        body_plain: None,
                        due_date_iso: None,
                        reminder_iso: None,
                        completed_at: None,
                        created_at: None,
                        updated_at: None,
                    },
                    TodoTask {
                        id: "t2".into(),
                        list_id: "l1".into(),
                        title: "Delete me".into(),
                        status: "notStarted".into(),
                        importance: "normal".into(),
                        body_plain: None,
                        due_date_iso: None,
                        reminder_iso: None,
                        completed_at: None,
                        created_at: None,
                        updated_at: None,
                    },
                ],
            )
            .unwrap();
        storage
            .replace_todo_tasks_for_list(
                "l1",
                true,
                &[TodoTask {
                    id: "t1".into(),
                    list_id: "l1".into(),
                    title: "Keep".into(),
                    status: "notStarted".into(),
                    importance: "normal".into(),
                    body_plain: None,
                    due_date_iso: None,
                    reminder_iso: None,
                    completed_at: None,
                    created_at: None,
                    updated_at: None,
                }],
            )
            .unwrap();
        assert_eq!(storage.list_todo_tasks(Some("l1"), true).unwrap().len(), 1);

        storage
            .upsert_contacts(&[
                ContactItem {
                    id: "c-personal-del".into(),
                    display_name: "Old Personal".into(),
                    given_name: None,
                    surname: None,
                    email_addresses: vec!["old@example.com".into()],
                    mobile_phone: None,
                    business_phones: vec![],
                    company_name: None,
                    job_title: None,
                    department: None,
                    notes: None,
                    source: "contact".into(),
                },
                ContactItem {
                    id: "c-dir-keep".into(),
                    display_name: "Directory Person".into(),
                    given_name: None,
                    surname: None,
                    email_addresses: vec!["dir@example.com".into()],
                    mobile_phone: None,
                    business_phones: vec![],
                    company_name: None,
                    job_title: None,
                    department: None,
                    notes: None,
                    source: "directory".into(),
                },
            ])
            .unwrap();
        storage.replace_personal_contacts(&[]).unwrap();
        let remaining_contacts = storage.list_contacts(None, 10).unwrap();
        assert_eq!(remaining_contacts.len(), 1);
        assert_eq!(remaining_contacts[0].id, "c-dir-keep");
    }
}
