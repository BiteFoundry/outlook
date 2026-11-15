//! Microsoft Graph v1.0 endpoints for User Profile, User Search, and 1:1 / Group / Meeting Chats.

use anyhow::{bail, Result};
use serde::Deserialize;

use super::{
    client::{
        compare_rfc3339_gt, is_pure_system_noise, normalize_chat_message,
        sort_messages_chronologically, truncate_to_newest, BatchSubRequest, GraphChatMessage,
        GraphIdentitySet, GraphItemBody, GraphListPage,
    },
    GraphClient,
};
use crate::models::{
    ConversationKind, ConversationSummary, ConversationTarget, MemberInfo, MessageItem, UserProfile,
};

#[derive(Debug, Deserialize)]
struct GraphUser {
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    mail: Option<String>,
    #[serde(rename = "userPrincipalName", default)]
    user_principal_name: Option<String>,
}

impl From<GraphUser> for UserProfile {
    fn from(u: GraphUser) -> Self {
        let fallback_name = u
            .user_principal_name
            .clone()
            .or_else(|| u.mail.clone())
            .unwrap_or_else(|| u.id.clone());
        Self {
            id: u.id,
            display_name: u
                .display_name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(fallback_name),
            mail: u.mail,
            user_principal_name: u.user_principal_name,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GraphConversationMember {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

impl From<GraphConversationMember> for MemberInfo {
    fn from(m: GraphConversationMember) -> Self {
        let uid = m.user_id.unwrap_or_default();
        let name = m
            .display_name
            .filter(|s| !s.trim().is_empty())
            .or_else(|| m.email.clone())
            .unwrap_or_else(|| uid.clone());
        Self {
            user_id: uid,
            member_id: m.id,
            display_name: name,
            email: m.email,
            roles: m.roles,
            presence: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphChatMessagePreview {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "isDeleted", default)]
    is_deleted: Option<bool>,
    #[serde(default)]
    from: Option<GraphIdentitySet>,
    #[serde(default)]
    body: Option<GraphItemBody>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphChatViewpoint {
    #[serde(rename = "lastMessageReadDateTime", default)]
    last_message_read_date_time: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphChat {
    id: String,
    #[serde(rename = "chatType", default)]
    chat_type: Option<String>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(rename = "lastUpdatedDateTime", default)]
    last_updated_date_time: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "lastMessagePreview", default)]
    last_message_preview: Option<GraphChatMessagePreview>,
    #[serde(default)]
    members: Vec<GraphConversationMember>,
    #[serde(default)]
    viewpoint: Option<GraphChatViewpoint>,
}

impl GraphClient {
    pub async fn get_me(&self) -> Result<UserProfile> {
        let user: GraphUser = self
            .get_json("/me?$select=id,displayName,mail,userPrincipalName")
            .await?;
        Ok(user.into())
    }

    pub async fn search_users(&self, query: &str, limit: usize) -> Result<Vec<UserProfile>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let q = query.trim();
        if q.is_empty() {
            let page: GraphListPage<GraphUser> = self
                .get_json(&format!(
                    "/users?$top={limit}&$select=id,displayName,mail,userPrincipalName"
                ))
                .await?;
            return Ok(page.value.into_iter().map(Into::into).collect());
        }

        let escaped = q.replace('\'', "''");
        let filter = format!("startswith(displayName,'{escaped}') or startswith(mail,'{escaped}')");
        let path = format!(
            "/users?$filter={}&$top={limit}&$select=id,displayName,mail,userPrincipalName",
            urlencoding::encode(&filter)
        );
        if let Ok(page) = self.get_json::<GraphListPage<GraphUser>>(&path).await {
            return Ok(page.value.into_iter().map(Into::into).collect());
        }

        // Fallback when `$filter` is unsupported or restricted in the tenant
        let q_lower = q.to_lowercase();
        let fetch_top = limit.max(100);
        let page: GraphListPage<GraphUser> = self
            .get_json(&format!(
                "/users?$top={fetch_top}&$select=id,displayName,mail,userPrincipalName"
            ))
            .await?;
        let mut matches: Vec<UserProfile> = page
            .value
            .into_iter()
            .map(UserProfile::from)
            .filter(|u| {
                u.display_name.to_lowercase().contains(&q_lower)
                    || u.mail
                        .as_deref()
                        .map(|m| m.to_lowercase().contains(&q_lower))
                        .unwrap_or(false)
                    || u.user_principal_name
                        .as_deref()
                        .map(|p| p.to_lowercase().contains(&q_lower))
                        .unwrap_or(false)
            })
            .collect();
        matches.truncate(limit);
        Ok(matches)
    }

    pub async fn list_chats(&self, limit: usize) -> Result<Vec<ConversationSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let my_id = self.resolve_my_user_id().await;
        let top = limit.min(50);
        let mut next_url = Some(format!(
            "/me/chats?$expand=lastMessagePreview,members&$top={top}"
        ));
        let mut results = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphChat> = self.get_json(&url).await?;
            for chat in page.value {
                results.push(map_graph_chat(chat, my_id.as_deref()));
                if results.len() >= limit {
                    return Ok(results);
                }
            }
            next_url = page.next_link;
        }

        Ok(results)
    }

    /// Creates a `oneOnOne` (1 peer user ID/email) or `group` (2+ peers, optional topic) chat.
    pub async fn create_chat(
        &self,
        member_ids_or_emails: &[String],
        topic: Option<&str>,
    ) -> Result<ConversationSummary> {
        let peers: Vec<String> = member_ids_or_emails
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if peers.is_empty() {
            bail!("대화 상대를 1명 이상 지정해야 합니다.");
        }

        let my_id = self.require_my_user_id().await?;
        let topic_clean = topic.map(str::trim).filter(|s| !s.is_empty());
        let is_one_on_one = peers.len() == 1 && topic_clean.is_none();
        let chat_type = if is_one_on_one { "oneOnOne" } else { "group" };

        let mut members_json = vec![serde_json::json!({
            "@odata.type": "#microsoft.graph.aadUserConversationMember",
            "roles": ["owner"],
            "user@odata.bind": format!("https://graph.microsoft.com/v1.0/users('{}')", my_id)
        })];

        for peer in &peers {
            if peer == &my_id && !is_one_on_one {
                continue;
            }
            members_json.push(serde_json::json!({
                "@odata.type": "#microsoft.graph.aadUserConversationMember",
                "roles": ["owner"],
                "user@odata.bind": format!("https://graph.microsoft.com/v1.0/users('{}')", peer)
            }));
        }

        let mut body = serde_json::json!({
            "chatType": chat_type,
            "members": members_json,
        });
        if let (false, Some(t)) = (is_one_on_one, topic_clean) {
            body["topic"] = serde_json::Value::String(t.to_string());
        }

        let created: GraphChat = self.post_json("/chats", &body).await?;
        Ok(map_graph_chat(created, Some(&my_id)))
    }

    pub async fn list_chat_members(&self, chat_id: &str) -> Result<Vec<MemberInfo>> {
        let mut next_url = Some(format!("/chats/{}/members", urlencoding::encode(chat_id)));
        let mut members = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphConversationMember> = self.get_json(&url).await?;
            members.extend(page.value.into_iter().map(MemberInfo::from));
            next_url = page.next_link;
        }
        Ok(members)
    }

    pub async fn list_chat_messages(&self, chat_id: &str, limit: usize) -> Result<Vec<MessageItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Chat {
            chat_id: chat_id.to_string(),
        }
        .conv_key();

        let top = limit.min(50);
        let mut next_url = Some(format!(
            "/chats/{}/messages?$top={top}",
            urlencoding::encode(chat_id)
        ));
        let mut items = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphChatMessage> = self.get_json(&url).await?;
            for raw in page.value {
                let normalized = normalize_chat_message(&conv_key, None, my_id.as_deref(), raw);
                if !is_pure_system_noise(&normalized) {
                    items.push(normalized);
                }
            }
            if items.len() >= limit {
                break;
            }
            next_url = page.next_link;
        }

        sort_messages_chronologically(&mut items);
        truncate_to_newest(&mut items, limit);
        Ok(items)
    }

    pub async fn mark_chat_read(&self, chat_id: &str) -> Result<()> {
        let me_id = self.require_my_user_id().await?;
        let path = format!(
            "/chats/{}/markChatReadForUser",
            urlencoding::encode(chat_id)
        );
        let body = serde_json::json!({
            "user": { "id": me_id }
        });
        self.post_json_empty(&path, &body).await
    }

    /// Fetches multiple chat message lists in a single `POST /$batch` call (chunked by 20).
    pub async fn batch_fetch_chat_messages(
        &self,
        chat_ids: &[String],
        limit_per_chat: usize,
    ) -> Result<Vec<(String, Vec<MessageItem>)>> {
        if chat_ids.is_empty() || limit_per_chat == 0 {
            return Ok(Vec::new());
        }
        let my_id = self.resolve_my_user_id().await;
        let top = limit_per_chat.min(50);

        let requests: Vec<BatchSubRequest> = chat_ids
            .iter()
            .enumerate()
            .map(|(idx, chat_id)| BatchSubRequest {
                id: idx.to_string(),
                method: "GET".to_string(),
                url: format!("/chats/{}/messages?$top={top}", urlencoding::encode(chat_id)),
                body: None,
                headers: None,
            })
            .collect();

        let responses = self.batch(&requests).await?;
        let mut by_idx: std::collections::HashMap<usize, Vec<MessageItem>> =
            std::collections::HashMap::new();

        for resp in responses {
            let Ok(idx) = resp.id.parse::<usize>() else {
                continue;
            };
            let Some(chat_id) = chat_ids.get(idx) else {
                continue;
            };
            if !(200..300).contains(&resp.status) {
                continue;
            }
            let Ok(page) = serde_json::from_value::<GraphListPage<GraphChatMessage>>(resp.body)
            else {
                continue;
            };
            let conv_key = ConversationTarget::Chat {
                chat_id: chat_id.clone(),
            }
            .conv_key();
            let mut items: Vec<MessageItem> = page
                .value
                .into_iter()
                .map(|raw| normalize_chat_message(&conv_key, None, my_id.as_deref(), raw))
                .filter(|m| !is_pure_system_noise(m))
                .collect();
            sort_messages_chronologically(&mut items);
            truncate_to_newest(&mut items, limit_per_chat);
            by_idx.insert(idx, items);
        }

        let ordered = chat_ids
            .iter()
            .enumerate()
            .map(|(idx, cid)| (cid.clone(), by_idx.remove(&idx).unwrap_or_default()))
            .collect();
        Ok(ordered)
    }
}

fn map_graph_chat(chat: GraphChat, my_user_id: Option<&str>) -> ConversationSummary {
    let kind = match chat.chat_type.as_deref() {
        Some("oneOnOne") => ConversationKind::OneOnOne,
        Some("meeting") => ConversationKind::Meeting,
        _ => ConversationKind::Group,
    };

    let target = ConversationTarget::Chat {
        chat_id: chat.id.clone(),
    };
    let conv_key = target.conv_key();

    let non_self_members: Vec<&GraphConversationMember> = chat
        .members
        .iter()
        .filter(|m| match (m.user_id.as_deref(), my_user_id) {
            (Some(uid), Some(me)) => uid != me,
            _ => true,
        })
        .collect();

    let peer_user_id = if kind == ConversationKind::OneOnOne {
        non_self_members
            .first()
            .and_then(|m| m.user_id.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                chat.members
                    .first()
                    .and_then(|m| m.user_id.clone())
                    .filter(|s| !s.trim().is_empty())
            })
    } else {
        None
    };

    let topic = chat
        .topic
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    let title = if let Some(ref t) = topic {
        t.clone()
    } else if kind == ConversationKind::OneOnOne {
        non_self_members
            .first()
            .and_then(|m| m.display_name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                chat.members
                    .first()
                    .and_then(|m| m.display_name.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("1:1 채팅")
            .to_string()
    } else {
        let names: Vec<String> = non_self_members
            .iter()
            .filter_map(|m| m.display_name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(4)
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            if kind == ConversationKind::Meeting {
                "모임 채팅".to_string()
            } else {
                "그룹 채팅".to_string()
            }
        } else {
            names.join(", ")
        }
    };

    let last_read_at = chat
        .viewpoint
        .and_then(|v| v.last_message_read_date_time)
        .filter(|s| !s.trim().is_empty());

    let (last_msg_id, last_msg_author, last_msg_preview, last_msg_created, last_msg_sender_id) =
        if let Some(preview) = chat.last_message_preview {
            let preview_text = if preview.is_deleted == Some(true) {
                Some("[삭제된 메시지]".to_string())
            } else {
                let raw_html = preview
                    .body
                    .as_ref()
                    .and_then(|b| b.content.as_deref())
                    .unwrap_or("");
                let plain = crate::parser::html_and_cards_to_plain(raw_html, &[]);
                let trimmed = plain.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            };
            let author = preview
                .from
                .as_ref()
                .and_then(|f| f.primary_name())
                .map(str::to_string);
            let sender_id = preview
                .from
                .as_ref()
                .and_then(|f| f.primary_id())
                .map(str::to_string);
            (
                preview.id,
                author,
                preview_text,
                preview.created_date_time,
                sender_id,
            )
        } else {
            (None, None, None, None, None)
        };

    let is_unread_timestamp =
        compare_rfc3339_gt(last_msg_created.as_deref(), last_read_at.as_deref());
    let sent_by_me = match (last_msg_sender_id.as_deref(), my_user_id) {
        (Some(sid), Some(me)) => sid == me,
        _ => false,
    };
    let unread_count = if is_unread_timestamp && !sent_by_me {
        1
    } else {
        0
    };

    let last_updated_at = last_msg_created.or(chat.last_updated_date_time);

    ConversationSummary {
        conv_key,
        target,
        kind,
        team_id: None,
        team_name: None,
        title,
        topic,
        unread_count,
        last_read_at,
        last_msg_id,
        last_msg_author,
        last_msg_preview,
        last_updated_at,
        pinned: false,
        peer_user_id,
        web_url: chat.web_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_title_and_unread_derivation_one_on_one() {
        let raw: GraphChat = serde_json::from_value(serde_json::json!({
            "id": "19:abc@unq.gbl.spaces",
            "chatType": "oneOnOne",
            "members": [
                { "userId": "me-1", "displayName": "나" },
                { "userId": "peer-2", "displayName": "김영수" }
            ],
            "viewpoint": {
                "lastMessageReadDateTime": "2026-11-15T05:00:00Z"
            },
            "lastMessagePreview": {
                "id": "m100",
                "createdDateTime": "2026-11-15T06:00:00Z",
                "from": { "user": { "id": "peer-2", "displayName": "김영수" } },
                "body": { "contentType": "html", "content": "안녕하세요" }
            }
        }))
        .unwrap();

        let summary = map_graph_chat(raw, Some("me-1"));
        assert_eq!(summary.kind, ConversationKind::OneOnOne);
        assert_eq!(summary.title, "김영수");
        assert_eq!(summary.peer_user_id.as_deref(), Some("peer-2"));
        assert_eq!(summary.unread_count, 1);
        assert_eq!(summary.last_msg_author.as_deref(), Some("김영수"));
    }

    #[test]
    fn test_chat_title_group_without_topic_and_read_own_msg() {
        let raw: GraphChat = serde_json::from_value(serde_json::json!({
            "id": "19:grp@thread.v2",
            "chatType": "group",
            "topic": "",
            "members": [
                { "userId": "me-1", "displayName": "나" },
                { "userId": "u2", "displayName": "박민수" },
                { "userId": "u3", "displayName": "이지은" },
                { "userId": "u4", "displayName": "최준호" },
                { "userId": "u5", "displayName": "정수빈" },
                { "userId": "u6", "displayName": "한지민" }
            ],
            "viewpoint": {
                "lastMessageReadDateTime": "2026-11-15T05:00:00Z"
            },
            "lastMessagePreview": {
                "id": "m200",
                "createdDateTime": "2026-11-15T06:00:00Z",
                "from": { "user": { "id": "me-1", "displayName": "나" } },
                "body": { "contentType": "text", "content": "내가 보낸 메시지" }
            }
        }))
        .unwrap();

        let summary = map_graph_chat(raw, Some("me-1"));
        assert_eq!(summary.kind, ConversationKind::Group);
        assert_eq!(summary.title, "박민수, 이지은, 최준호, 정수빈");
        assert_eq!(summary.peer_user_id, None);
        // Sent by me -> unread_count must be 0
        assert_eq!(summary.unread_count, 0);
    }

    #[test]
    fn test_chat_title_group_with_topic() {
        let raw: GraphChat = serde_json::from_value(serde_json::json!({
            "id": "19:grp2@thread.v2",
            "chatType": "group",
            "topic": "백엔드 스프린트",
            "members": [
                { "userId": "me-1", "displayName": "나" },
                { "userId": "u2", "displayName": "박민수" }
            ]
        }))
        .unwrap();

        let summary = map_graph_chat(raw, Some("me-1"));
        assert_eq!(summary.title, "백엔드 스프린트");
        assert_eq!(summary.topic.as_deref(), Some("백엔드 스프린트"));
        assert_eq!(summary.unread_count, 0);
    }
}
