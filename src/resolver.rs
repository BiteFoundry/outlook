//! Fuzzy target resolver for CLI, HTTP API, and MCP (`"Alice"`, `"Group Chat"`, `"Engineering/General"`, `chat:...`, `chan:...:...`).

use anyhow::{bail, Result};

use crate::{
    graph::GraphClient,
    models::{ConversationKind, ConversationSummary, ConversationTarget},
    storage::Storage,
};

/// Resolves a user-supplied conversation query against the known conversation list.
///
/// Supported query forms:
/// 1. Exact `conv_key` (`chat:...` or `chan:<team_id>:<channel_id>`)
/// 2. Raw `chat_id` (`19:...@thread.v2` or `19:...@unq.gbl.spaces`) or `channel_id`
/// 3. `"<team substring>/<channel substring>"` for Team Channels
/// 4. Case-insensitive exact match on `title`
/// 5. Case-insensitive unique substring match on `title` (or `"team_name/title"`)
pub fn resolve_from_slice(
    conversations: &[ConversationSummary],
    query: &str,
) -> Result<ConversationSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Target conversation/channel name is empty.");
    }

    // Priority 1: Exact conv_key
    if let Some(exact) = conversations.iter().find(|c| c.conv_key == q) {
        return Ok(exact.clone());
    }

    // Priority 2: Exact raw chat_id or channel_id
    let raw_id_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| match &c.target {
            ConversationTarget::Chat { chat_id } => chat_id == q,
            ConversationTarget::Channel { channel_id, .. } => channel_id == q,
        })
        .collect();
    if raw_id_matches.len() == 1 {
        return Ok(raw_id_matches[0].clone());
    } else if raw_id_matches.len() > 1 {
        bail!(
            "Multiple conversations/channels match target ID '{q}':\n{}",
            format_candidates(&raw_id_matches)
        );
    }

    // Priority 3: Slash notation "<team_part>/<channel_part>" for Team Channels
    if let Some((team_raw, chan_raw)) = q.split_once('/') {
        let team_part = team_raw.trim().to_lowercase();
        let chan_part = chan_raw.trim().to_lowercase();

        if !team_part.is_empty() || !chan_part.is_empty() {
            let channels: Vec<&ConversationSummary> = conversations
                .iter()
                .filter(|c| c.kind == ConversationKind::Channel)
                .collect();

            let exact_matches: Vec<&ConversationSummary> = channels
                .iter()
                .copied()
                .filter(|c| {
                    let tname = c.team_name.as_deref().unwrap_or("").to_lowercase();
                    let cname = c.title.to_lowercase();
                    (team_part.is_empty() || tname == team_part)
                        && (chan_part.is_empty() || cname == chan_part)
                })
                .collect();

            if exact_matches.len() == 1 {
                return Ok(exact_matches[0].clone());
            } else if exact_matches.len() > 1 {
                bail!(
                    "Multiple channels match '{q}' exactly:\n{}",
                    format_candidates(&exact_matches)
                );
            }

            let sub_matches: Vec<&ConversationSummary> = channels
                .iter()
                .copied()
                .filter(|c| {
                    let tname = c.team_name.as_deref().unwrap_or("").to_lowercase();
                    let cname = c.title.to_lowercase();
                    (team_part.is_empty() || tname.contains(&team_part))
                        && (chan_part.is_empty() || cname.contains(&chan_part))
                })
                .collect();

            if sub_matches.len() == 1 {
                return Ok(sub_matches[0].clone());
            } else if sub_matches.len() > 1 {
                bail!(
                    "Multiple channels match '{q}'. Please specify a more specific target:\n{}",
                    format_candidates(&sub_matches)
                );
            }
        }
    }

    let q_lower = q.to_lowercase();

    // Priority 4: Exact case-insensitive title match across all conversations
    let exact_title_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| c.title.trim().to_lowercase() == q_lower)
        .collect();
    if exact_title_matches.len() == 1 {
        return Ok(exact_title_matches[0].clone());
    } else if exact_title_matches.len() > 1 {
        bail!(
            "Multiple targets match title '{q}' exactly. Use conv_key or 'TeamName/ChannelName':\n{}",
            format_candidates(&exact_title_matches)
        );
    }

    // Priority 5: Unique case-insensitive substring match on title or "team_name/title"
    let sub_matches: Vec<&ConversationSummary> = conversations
        .iter()
        .filter(|c| {
            let title_lower = c.title.to_lowercase();
            if title_lower.contains(&q_lower) {
                return true;
            }
            if let Some(team_name) = &c.team_name {
                let combined = format!("{team_name}/{}", c.title).to_lowercase();
                if combined.contains(&q_lower) {
                    return true;
                }
            }
            false
        })
        .collect();

    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous target '{q}' matches multiple conversations ({n}):\n{}",
            format_candidates(&sub_matches)
        ),
        _ => bail!("Conversation/channel not found: {q}"),
    }
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
    let chats = graph.list_chats(150).await.unwrap_or_default();
    storage.upsert_conversations(&chats)?;
    let teams = graph
        .list_joined_teams_with_channels()
        .await
        .unwrap_or_default();
    storage.upsert_teams(&teams)?;
    let refreshed = storage.list_conversations()?;
    resolve_from_slice(&refreshed, query)
}

fn format_candidates(candidates: &[&ConversationSummary]) -> String {
    const MAX_LIST: usize = 8;
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_LIST)
        .map(|c| match c.team_name.as_deref() {
            Some(team) if c.kind == ConversationKind::Channel && !team.is_empty() => {
                format!("  - {team}/{} ({})", c.title, c.conv_key)
            }
            _ => format!("  - {} ({})", c.title, c.conv_key),
        })
        .collect();
    if candidates.len() > MAX_LIST {
        lines.push(format!("  ... and {} more", candidates.len() - MAX_LIST));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chat(chat_id: &str, title: &str) -> ConversationSummary {
        let target = ConversationTarget::Chat {
            chat_id: chat_id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: title.to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: None,
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }
    }

    fn make_channel(
        team_id: &str,
        team_name: &str,
        channel_id: &str,
        title: &str,
    ) -> ConversationSummary {
        let target = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::Channel,
            team_id: Some(team_id.to_string()),
            team_name: Some(team_name.to_string()),
            title: title.to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: None,
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }
    }

    #[test]
    fn test_resolver_priorities_and_slash_notation() {
        let convs = vec![
            make_chat("19:alice@unq.gbl.spaces", "Alice Smith"),
            make_chat("19:bob@unq.gbl.spaces", "Bob Jones"),
            make_channel("t1", "Engineering", "19:gen1@thread.tacv2", "General"),
            make_channel("t1", "Engineering", "19:dep1@thread.tacv2", "Deploy"),
            make_channel("t2", "Infra", "19:dep2@thread.tacv2", "Deploy-Alerts"),
            make_channel("t2", "Infra", "19:gen2@thread.tacv2", "General"),
        ];

        // Priority 1: exact conv_key
        let r1 = resolve_from_slice(&convs, "chan:t1:19:dep1@thread.tacv2").unwrap();
        assert_eq!(r1.title, "Deploy");

        // Priority 2: raw chat_id / channel_id
        let r2 = resolve_from_slice(&convs, "19:alice@unq.gbl.spaces").unwrap();
        assert_eq!(r2.title, "Alice Smith");
        let r2b = resolve_from_slice(&convs, "19:dep2@thread.tacv2").unwrap();
        assert_eq!(r2b.title, "Deploy-Alerts");

        // Priority 3: slash notation exact & substring
        let r3 = resolve_from_slice(&convs, "Engineering/Deploy").unwrap();
        assert_eq!(r3.conv_key, "chan:t1:19:dep1@thread.tacv2");
        let r3b = resolve_from_slice(&convs, "inf/alerts").unwrap();
        assert_eq!(r3b.conv_key, "chan:t2:19:dep2@thread.tacv2");

        // Priority 4: exact title match
        let r4 = resolve_from_slice(&convs, "Alice Smith").unwrap();
        assert_eq!(r4.conv_key, "chat:19:alice@unq.gbl.spaces");

        // Priority 5: unique substring match
        let r5 = resolve_from_slice(&convs, "Jones").unwrap();
        assert_eq!(r5.title, "Bob Jones");
    }

    #[test]
    fn test_resolver_ambiguity_error_lists_candidates() {
        let convs = vec![
            make_channel("t1", "Engineering", "c1", "General"),
            make_channel("t2", "Design", "c2", "General"),
            make_channel("t3", "Engineering2", "c3", "Deploy-General"),
        ];

        // Ambiguous exact title
        let err = resolve_from_slice(&convs, "General").unwrap_err().to_string();
        assert!(err.contains("Engineering/General (chan:t1:c1)"));
        assert!(err.contains("Design/General (chan:t2:c2)"));

        // Ambiguous slash notation
        let err2 = resolve_from_slice(&convs, "Eng/General")
            .unwrap_err()
            .to_string();
        assert!(err2.contains("Engineering/General (chan:t1:c1)"));
        assert!(err2.contains("Engineering2/Deploy-General (chan:t3:c3)"));
    }

    #[test]
    fn test_resolver_empty_and_not_found() {
        let convs = vec![make_chat("c1", "Alice")];
        assert!(resolve_from_slice(&convs, "   ").is_err());
        let err = resolve_from_slice(&convs, "nonexistent")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"));
    }
}
