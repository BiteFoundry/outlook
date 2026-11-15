//! Local SQLite WAL persistence + FTS5 (`trigram` + `LIKE` hybrid) search repository.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;

use crate::models::{
    ConversationSummary, MemberInfo, MessageItem, PresenceStatus, SearchHit, SyncCursor,
    TeamSummary, ThreadView,
};

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        crate::config::ensure_parent(path)?;
        let conn = rusqlite::Connection::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.conn
    }

    // --- Conversations & Teams ---
    pub fn upsert_conversations(&self, _items: &[ConversationSummary]) -> Result<()> {
        Ok(())
    }

    pub fn upsert_teams(&self, _teams: &[TeamSummary]) -> Result<()> {
        Ok(())
    }

    pub fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        Ok(Vec::new())
    }

    pub fn list_teams(&self) -> Result<Vec<TeamSummary>> {
        Ok(Vec::new())
    }

    pub fn get_conversation(&self, _conv_key: &str) -> Result<Option<ConversationSummary>> {
        Ok(None)
    }

    pub fn mark_conversation_read(&self, _conv_key: &str, _read_at: &str) -> Result<()> {
        Ok(())
    }

    // --- Messages & Threads ---
    pub fn upsert_messages(&self, _messages: &[MessageItem]) -> Result<usize> {
        Ok(0)
    }

    /// Lists root messages in chronological order (oldest first, newest last), limited to newest `limit`.
    pub fn list_root_messages(
        &self,
        _conv_key: &str,
        _limit: usize,
        _before_created_at: Option<&str>,
    ) -> Result<Vec<MessageItem>> {
        Ok(Vec::new())
    }

    pub fn list_thread_replies(
        &self,
        _conv_key: &str,
        _root_msg_id: &str,
        _limit: usize,
    ) -> Result<Vec<MessageItem>> {
        Ok(Vec::new())
    }

    pub fn get_message(&self, _conv_key: &str, _msg_id: &str) -> Result<Option<MessageItem>> {
        Ok(None)
    }

    pub fn get_thread(
        &self,
        _conv_key: &str,
        _root_msg_id: &str,
        _limit: usize,
    ) -> Result<Option<ThreadView>> {
        Ok(None)
    }

    /// Lists messages across all conversations where `@me` is mentioned.
    pub fn list_mentions_for_user(
        &self,
        _my_user_id: &str,
        _limit: usize,
    ) -> Result<Vec<MessageItem>> {
        Ok(Vec::new())
    }

    // --- Members & Presence ---
    pub fn upsert_members(&self, _conv_key: &str, _members: &[MemberInfo]) -> Result<()> {
        Ok(())
    }

    pub fn list_members(&self, _conv_key: &str) -> Result<Vec<MemberInfo>> {
        Ok(Vec::new())
    }

    pub fn upsert_presences(&self, _presences: &[PresenceStatus]) -> Result<()> {
        Ok(())
    }

    pub fn get_presences(&self) -> Result< std::collections::HashMap<String, PresenceStatus> > {
        Ok(std::collections::HashMap::new())
    }

    // --- Search & Sync Cursors ---
    pub fn search_messages(
        &self,
        _query: &str,
        _conv_key_filter: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<SearchHit>> {
        Ok(Vec::new())
    }

    pub fn get_sync_cursor(&self, _scope_key: &str) -> Result<Option<SyncCursor>> {
        Ok(None)
    }

    pub fn save_sync_cursor(&self, _cursor: &SyncCursor) -> Result<()> {
        Ok(())
    }
}
