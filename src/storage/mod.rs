//! Local SQLite WAL persistence + FTS5 (`trigram` + `LIKE` hybrid) search repository.

pub mod db;
pub mod repo;
pub mod search;

use std::{
    collections::HashMap,
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
    trigram_enabled: bool,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        crate::config::ensure_parent(path)?;
        let conn = rusqlite::Connection::open(path)?;
        let trigram_enabled = db::init_connection(&conn, false)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
            trigram_enabled,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let trigram_enabled = db::init_connection(&conn, true)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            conn: Arc::new(Mutex::new(conn)),
            trigram_enabled,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.conn
    }

    pub fn trigram_enabled(&self) -> bool {
        self.trigram_enabled
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite 연결 잠금 획득 실패 (poisoned mutex)"))
    }

    // --- Conversations & Teams ---
    pub fn upsert_conversations(&self, items: &[ConversationSummary]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_conversations(&mut conn, items)
    }

    pub fn upsert_teams(&self, teams: &[TeamSummary]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_teams(&mut conn, teams)
    }

    pub fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        let conn = self.lock_conn()?;
        repo::list_conversations(&conn)
    }

    pub fn list_teams(&self) -> Result<Vec<TeamSummary>> {
        let conn = self.lock_conn()?;
        repo::list_teams(&conn)
    }

    pub fn get_conversation(&self, conv_key: &str) -> Result<Option<ConversationSummary>> {
        let conn = self.lock_conn()?;
        repo::get_conversation(&conn, conv_key)
    }

    pub fn mark_conversation_read(&self, conv_key: &str, read_at: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        repo::mark_conversation_read(&conn, conv_key, read_at)
    }

    // --- Messages & Threads ---
    pub fn upsert_messages(&self, messages: &[MessageItem]) -> Result<usize> {
        let mut conn = self.lock_conn()?;
        repo::upsert_messages(&mut conn, messages)
    }

    /// Lists root messages in chronological order (oldest first, newest last), limited to newest `limit`.
    pub fn list_root_messages(
        &self,
        conv_key: &str,
        limit: usize,
        before_created_at: Option<&str>,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.lock_conn()?;
        repo::list_root_messages(&conn, conv_key, limit, before_created_at)
    }

    pub fn list_thread_replies(
        &self,
        conv_key: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.lock_conn()?;
        repo::list_thread_replies(&conn, conv_key, root_msg_id, limit)
    }

    pub fn get_message(&self, conv_key: &str, msg_id: &str) -> Result<Option<MessageItem>> {
        let conn = self.lock_conn()?;
        repo::get_message(&conn, conv_key, msg_id)
    }

    pub fn get_thread(
        &self,
        conv_key: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<Option<ThreadView>> {
        let conn = self.lock_conn()?;
        repo::get_thread(&conn, conv_key, root_msg_id, limit)
    }

    /// Lists messages across all conversations where `@me` is mentioned.
    pub fn list_mentions_for_user(
        &self,
        my_user_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.lock_conn()?;
        repo::list_mentions_for_user(&conn, my_user_id, limit)
    }

    // --- Members & Presence ---
    pub fn upsert_members(&self, conv_key: &str, members: &[MemberInfo]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_members(&mut conn, conv_key, members)
    }

    pub fn list_members(&self, conv_key: &str) -> Result<Vec<MemberInfo>> {
        let conn = self.lock_conn()?;
        repo::list_members(&conn, conv_key)
    }

    pub fn upsert_presences(&self, presences: &[PresenceStatus]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_presences(&mut conn, presences)
    }

    pub fn get_presences(&self) -> Result<HashMap<String, PresenceStatus>> {
        let conn = self.lock_conn()?;
        repo::get_presences(&conn)
    }

    // --- Search & Sync Cursors ---
    pub fn search_messages(
        &self,
        query: &str,
        conv_key_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.lock_conn()?;
        search::search_messages(&conn, query, conv_key_filter, limit)
    }

    pub fn get_sync_cursor(&self, scope_key: &str) -> Result<Option<SyncCursor>> {
        let conn = self.lock_conn()?;
        repo::get_sync_cursor(&conn, scope_key)
    }

    pub fn save_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        let conn = self.lock_conn()?;
        repo::save_sync_cursor(&conn, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChannelSummary, ConversationKind, ConversationTarget, MentionInfo};

    fn sample_chat(id: &str, title: &str, pinned: bool, updated: Option<&str>) -> ConversationSummary {
        let target = ConversationTarget::Chat {
            chat_id: id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: title.to_string(),
            topic: None,
            unread_count: 2,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: updated.map(str::to_string),
            pinned,
            peer_user_id: Some(format!("user-{id}")),
            web_url: None,
        }
    }

    fn sample_msg(
        conv_key: &str,
        msg_id: &str,
        parent_msg_id: Option<&str>,
        author: &str,
        body_plain: &str,
        subject: Option<&str>,
        created_at: &str,
    ) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: parent_msg_id.map(str::to_string),
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: subject.map(str::to_string),
            author_id: Some("u1".to_string()),
            author_name: author.to_string(),
            is_me: false,
            created_at: created_at.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: format!("<p>{body_plain}</p>"),
            body_plain: body_plain.to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }
    }

    #[test]
    fn test_teams_upsert_preserves_existing_channel_state() {
        let storage = Storage::open_in_memory().unwrap();
        let ch = ChannelSummary {
            id: "ch-1".to_string(),
            team_id: "team-1".to_string(),
            team_name: "개발팀".to_string(),
            display_name: "일반".to_string(),
            description: Some("일반 채널".to_string()),
            membership_type: "standard".to_string(),
            web_url: None,
        };
        let conv_key = ch.conv_key();

        let existing = ConversationSummary {
            conv_key: conv_key.clone(),
            target: ch.target(),
            kind: ConversationKind::Channel,
            team_id: Some("team-1".to_string()),
            team_name: Some("개발팀".to_string()),
            title: "일반".to_string(),
            topic: None,
            unread_count: 7,
            last_read_at: Some("2026-11-01T00:00:00Z".to_string()),
            last_msg_id: Some("m-10".to_string()),
            last_msg_author: Some("김철수".to_string()),
            last_msg_preview: Some("배포 완료".to_string()),
            last_updated_at: Some("2026-11-02T10:00:00Z".to_string()),
            pinned: true,
            peer_user_id: None,
            web_url: None,
        };
        storage.upsert_conversations(&[existing]).unwrap();

        let team = TeamSummary {
            id: "team-1".to_string(),
            display_name: "개발팀".to_string(),
            description: Some("팀 설명".to_string()),
            channels: vec![ChannelSummary {
                display_name: "일반-개명".to_string(),
                ..ch
            }],
        };
        storage.upsert_teams(&[team]).unwrap();

        let loaded = storage.get_conversation(&conv_key).unwrap().unwrap();
        assert_eq!(loaded.title, "일반-개명");
        assert_eq!(loaded.unread_count, 7);
        assert_eq!(loaded.last_msg_id.as_deref(), Some("m-10"));
        assert!(loaded.pinned);
    }

    #[test]
    fn test_conversation_ordering_and_mark_read() {
        let storage = Storage::open_in_memory().unwrap();
        let c1 = sample_chat("c1", "가나다", false, Some("2026-11-15T10:00:00Z"));
        let c2 = sample_chat("c2", "홍길동", true, Some("2026-11-14T10:00:00Z"));
        let c3 = sample_chat("c3", "박영희", false, Some("2026-11-15T12:00:00Z"));
        storage.upsert_conversations(&[c1, c2, c3]).unwrap();

        let list = storage.list_conversations().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].title, "홍길동"); // pinned first
        assert_eq!(list[1].title, "박영희"); // newer timestamp
        assert_eq!(list[2].title, "가나다");

        storage
            .mark_conversation_read("chat:c3", "2026-11-15T12:05:00Z")
            .unwrap();
        let c3_after = storage.get_conversation("chat:c3").unwrap().unwrap();
        assert_eq!(c3_after.unread_count, 0);
        assert_eq!(
            c3_after.last_read_at.as_deref(),
            Some("2026-11-15T12:05:00Z")
        );
    }

    #[test]
    fn test_root_messages_ordering_thread_reply_count_and_last_msg() {
        let storage = Storage::open_in_memory().unwrap();
        let team = TeamSummary {
            id: "t1".to_string(),
            display_name: "개발팀".to_string(),
            description: None,
            channels: vec![ChannelSummary {
                id: "ch1".to_string(),
                team_id: "t1".to_string(),
                team_name: "개발팀".to_string(),
                display_name: "배포".to_string(),
                description: None,
                membership_type: "standard".to_string(),
                web_url: None,
            }],
        };
        storage.upsert_teams(&[team]).unwrap();
        let conv_key = "chan:t1:ch1";

        let r1 = sample_msg(
            conv_key,
            "m1",
            None,
            "김철수",
            "첫 번째 루트",
            Some("공지"),
            "2026-11-15T09:00:00Z",
        );
        let r2 = sample_msg(
            conv_key,
            "m2",
            None,
            "이영희",
            "두 번째 루트",
            None,
            "2026-11-15T10:00:00Z",
        );
        let r3 = sample_msg(
            conv_key,
            "m3",
            None,
            "박민수",
            "세 번째 루트",
            None,
            "2026-11-15T11:00:00Z",
        );
        let rep1 = sample_msg(
            conv_key,
            "m1-r1",
            Some("m1"),
            "홍길동",
            "답글 1",
            None,
            "2026-11-15T11:30:00Z",
        );
        let rep2 = sample_msg(
            conv_key,
            "m1-r2",
            Some("m1"),
            "김철수",
            "답글 2",
            None,
            "2026-11-15T12:00:00Z",
        );

        storage.upsert_messages(&[r1, r2, r3, rep1, rep2]).unwrap();

        // Newest 2 root messages, returned oldest-first (m2, m3)
        let roots = storage.list_root_messages(conv_key, 2, None).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].msg_id, "m2");
        assert_eq!(roots[1].msg_id, "m3");

        // Pagination before m3 returns m1, m2
        let older = storage
            .list_root_messages(conv_key, 10, Some("2026-11-15T11:00:00Z"))
            .unwrap();
        assert_eq!(older.len(), 2);
        assert_eq!(older[0].msg_id, "m1");
        assert_eq!(older[0].reply_count, 2);
        assert_eq!(older[1].msg_id, "m2");

        // Thread view
        let thread = storage.get_thread(conv_key, "m1", 20).unwrap().unwrap();
        assert_eq!(thread.root.msg_id, "m1");
        assert_eq!(thread.root.reply_count, 2);
        assert_eq!(thread.replies.len(), 2);
        assert_eq!(thread.replies[0].msg_id, "m1-r1");
        assert_eq!(thread.replies[1].msg_id, "m1-r2");

        // Conversation last_msg updated from newest reply
        let conv = storage.get_conversation(conv_key).unwrap().unwrap();
        assert_eq!(conv.last_msg_id.as_deref(), Some("m1-r2"));
        assert_eq!(conv.last_msg_author.as_deref(), Some("김철수"));
        assert_eq!(conv.last_msg_preview.as_deref(), Some("답글 2"));
    }

    #[test]
    fn test_fts5_trigram_and_two_char_korean_like_fallback() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c1", "프로젝트방", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let m1 = sample_msg(
            &conv_key,
            "m1",
            None,
            "홍길동",
            "오늘 오후 3시에 배포 일정 회의가 있습니다. Kubernetes rollout 점검 부탁드립니다.",
            Some("주간 배포"),
            "2026-11-15T09:00:00Z",
        );
        let mut m2 = sample_msg(
            &conv_key,
            "m2",
            None,
            "김영희",
            "삭제될 메시지입니다",
            None,
            "2026-11-15T10:00:00Z",
        );
        m2.deleted = true;
        storage.upsert_messages(&[m1, m2]).unwrap();

        // >= 3 chars Korean trigram
        let hits = storage.search_messages("배포 일정", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].msg_id, "m1");
        assert_eq!(hits[0].conv_title, "프로젝트방");

        // >= 3 chars English case-insensitive
        let eng_hits = storage.search_messages("kubernetes", Some(&conv_key), 10).unwrap();
        assert_eq!(eng_hits.len(), 1);
        assert!(eng_hits[0].snippet.contains("Kubernetes"));

        // < 3 chars Korean 2-character fallback
        let two_char_ko = storage.search_messages("회의", None, 10).unwrap();
        assert_eq!(two_char_ko.len(), 1);
        assert_eq!(two_char_ko[0].msg_id, "m1");

        let two_char_subj = storage.search_messages("주간", None, 10).unwrap();
        assert_eq!(two_char_subj.len(), 1);

        // Deleted message must not appear in search
        let deleted_hits = storage.search_messages("삭제될", None, 10).unwrap();
        assert!(deleted_hits.is_empty());

        // Re-upsert m1 with updated body keeps FTS synchronized without duplicate rows
        let mut m1_updated = sample_msg(
            &conv_key,
            "m1",
            None,
            "홍길동",
            "새로운 내용으로 수정됨",
            None,
            "2026-11-15T09:00:00Z",
        );
        m1_updated.last_modified_at = Some("2026-11-15T09:10:00Z".to_string());
        storage.upsert_messages(&[m1_updated]).unwrap();
        assert!(storage.search_messages("Kubernetes", None, 10).unwrap().is_empty());
        assert_eq!(storage.search_messages("수정됨", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn test_members_presence_mentions_and_cursors() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chat:c1";
        storage
            .upsert_members(
                conv_key,
                &[MemberInfo {
                    user_id: "u1".to_string(),
                    member_id: Some("mem1".to_string()),
                    display_name: "홍길동".to_string(),
                    email: Some("hong@example.com".to_string()),
                    roles: vec!["owner".to_string()],
                    presence: None,
                }],
            )
            .unwrap();
        storage
            .upsert_presences(&[PresenceStatus {
                user_id: "u1".to_string(),
                availability: "Available".to_string(),
                activity: "Available".to_string(),
            }])
            .unwrap();

        let members = storage.list_members(conv_key).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(
            members[0].presence.as_ref().map(|p| p.availability.as_str()),
            Some("Available")
        );

        let mut msg = sample_msg(
            conv_key,
            "m10",
            None,
            "김철수",
            "@홍길동 확인해주세요",
            None,
            "2026-11-15T12:00:00Z",
        );
        msg.mentions.push(MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "홍길동".to_string(),
        });
        storage.upsert_messages(&[msg]).unwrap();
        let mentions = storage.list_mentions_for_user("u1", 10).unwrap();
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].msg_id, "m10");

        let cursor = SyncCursor {
            scope_key: "chan:t1:ch1".to_string(),
            delta_link: Some("https://graph.microsoft.com/v1.0/delta?token=abc".to_string()),
            oldest_msg_ts: Some("2026-11-01T00:00:00Z".to_string()),
            synced_at: "2026-11-15T12:00:00Z".to_string(),
        };
        storage.save_sync_cursor(&cursor).unwrap();
        let loaded = storage.get_sync_cursor("chan:t1:ch1").unwrap().unwrap();
        assert_eq!(loaded, cursor);
    }

    #[test]
    fn test_file_backed_wal_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("teams.db");
        {
            let storage = Storage::open(&db_file).unwrap();
            let chat = sample_chat("c-disk", "디스크방", false, Some("2026-11-15T12:00:00Z"));
            storage.upsert_conversations(&[chat]).unwrap();
        }
        let reopened = Storage::open(&db_file).unwrap();
        let loaded = reopened.get_conversation("chat:c-disk").unwrap().unwrap();
        assert_eq!(loaded.title, "디스크방");
    }
}
