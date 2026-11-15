//! Fuzzy target resolver for CLI, HTTP API, and MCP (`"홍길동"`, `"그룹방"`, `"개발팀/일반"`, `chat:...`, `chan:...:...`).

use anyhow::{bail, Result};

use crate::{
    graph::GraphClient,
    models::ConversationSummary,
    storage::Storage,
};

/// Resolves a user-supplied conversation query against the known conversation list.
///
/// Supported query forms:
/// 1. Exact `conv_key` (`chat:...` or `chan:<team_id>:<channel_id>`)
/// 2. Raw `chat_id` (`19:...@thread.v2` or `19:...@unq.gbl.spaces`)
/// 3. `"<team substring>/<channel substring>"` for Team Channels
/// 4. Case-insensitive exact or unique substring match on `title` (or `"team / channel"`)
pub fn resolve_from_slice(
    conversations: &[ConversationSummary],
    query: &str,
) -> Result<ConversationSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("대상 대화방/채널 이름이 비어 있습니다.");
    }
    if let Some(exact) = conversations.iter().find(|c| c.conv_key == q) {
        return Ok(exact.clone());
    }
    bail!("대화방/채널을 찾을 수 없습니다: {q}")
}

/// Resolves against local SQLite cache first, refreshing from Graph once if not found.
pub async fn resolve_conversation(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
) -> Result<ConversationSummary> {
    let cached = storage.list_conversations()?;
    if let Ok(found) = resolve_from_slice(&cached, query) {
        return Ok(found);
    }
    let chats = graph.list_chats(50).await.unwrap_or_default();
    storage.upsert_conversations(&chats)?;
    let teams = graph.list_joined_teams_with_channels().await.unwrap_or_default();
    storage.upsert_teams(&teams)?;
    let refreshed = storage.list_conversations()?;
    resolve_from_slice(&refreshed, query)
}
