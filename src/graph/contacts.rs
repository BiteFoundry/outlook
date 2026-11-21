//! Microsoft Graph v1.0 Outlook Contacts & Organization Directory search (`/me/contacts`, `/users`).

use std::collections::HashSet;

use anyhow::Result;
use serde::Deserialize;

use super::{client::GraphListPage, mail::GraphEmailAddress, GraphClient};
use crate::models::{ContactItem, OutgoingContact};

const DIRECTORY_USER_SELECT: &str = "id,displayName,givenName,surname,mail,userPrincipalName,mobilePhone,businessPhones,companyName,jobTitle,department";

#[derive(Debug, Default, Deserialize)]
struct GraphContact {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "givenName", default)]
    given_name: Option<String>,
    #[serde(default)]
    surname: Option<String>,
    #[serde(rename = "emailAddresses", default)]
    email_addresses: Vec<GraphEmailAddress>,
    #[serde(rename = "mobilePhone", default)]
    mobile_phone: Option<String>,
    #[serde(rename = "businessPhones", default)]
    business_phones: Vec<String>,
    #[serde(rename = "companyName", default)]
    company_name: Option<String>,
    #[serde(rename = "jobTitle", default)]
    job_title: Option<String>,
    #[serde(default)]
    department: Option<String>,
    #[serde(rename = "personalNotes", default)]
    personal_notes: Option<String>,
}

impl From<GraphContact> for ContactItem {
    fn from(raw: GraphContact) -> Self {
        let email_addresses: Vec<String> = raw
            .email_addresses
            .into_iter()
            .filter_map(|e| e.address)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let given_name = raw.given_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let surname = raw.surname.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let display_name = raw
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| match (given_name.as_deref(), surname.as_deref()) {
                (Some(g), Some(s)) => Some(format!("{g} {s}")),
                (Some(g), None) => Some(g.to_string()),
                (None, Some(s)) => Some(s.to_string()),
                (None, None) => None,
            })
            .or_else(|| email_addresses.first().cloned())
            .unwrap_or_else(|| raw.id.clone());

        Self {
            id: raw.id,
            display_name,
            given_name,
            surname,
            email_addresses,
            mobile_phone: raw.mobile_phone.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            business_phones: raw
                .business_phones
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            company_name: raw.company_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            job_title: raw.job_title.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            department: raw.department.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            notes: raw.personal_notes.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            source: "contact".to_string(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphDirectoryUser {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "givenName", default)]
    given_name: Option<String>,
    #[serde(default)]
    surname: Option<String>,
    #[serde(default)]
    mail: Option<String>,
    #[serde(rename = "userPrincipalName", default)]
    user_principal_name: Option<String>,
    #[serde(rename = "mobilePhone", default)]
    mobile_phone: Option<String>,
    #[serde(rename = "businessPhones", default)]
    business_phones: Vec<String>,
    #[serde(rename = "companyName", default)]
    company_name: Option<String>,
    #[serde(rename = "jobTitle", default)]
    job_title: Option<String>,
    #[serde(default)]
    department: Option<String>,
}

impl From<GraphDirectoryUser> for ContactItem {
    fn from(raw: GraphDirectoryUser) -> Self {
        let mut email_addresses = Vec::new();
        if let Some(m) = raw.mail.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            email_addresses.push(m);
        }
        if let Some(upn) = raw
            .user_principal_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            if !email_addresses
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&upn))
            {
                email_addresses.push(upn);
            }
        }
        let display_name = raw
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| email_addresses.first().cloned())
            .unwrap_or_else(|| raw.id.clone());

        Self {
            id: raw.id,
            display_name,
            given_name: raw.given_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            surname: raw.surname.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            email_addresses,
            mobile_phone: raw.mobile_phone.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            business_phones: raw
                .business_phones
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            company_name: raw.company_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            job_title: raw.job_title.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            department: raw.department.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            notes: None,
            source: "directory".to_string(),
        }
    }
}

pub struct ContactSearchOutcome {
    pub contacts: Vec<ContactItem>,
    pub directory_error: Option<anyhow::Error>,
}

impl GraphClient {
    pub async fn list_contacts(&self, limit: usize) -> Result<Vec<ContactItem>> {
        let (contacts, _) = self.list_contacts_complete(limit).await?;
        Ok(contacts)
    }

    pub async fn list_contacts_complete(&self, limit: usize) -> Result<(Vec<ContactItem>, bool)> {
        if limit == 0 {
            return Ok((Vec::new(), false));
        }
        let top = limit.min(100);
        let mut next_url = Some(format!("/me/contacts?$top={top}&$orderby=displayName asc"));
        let mut contacts = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphContact> = self.get_json(&url).await?;
            let page_has_more = page.next_link.is_some();
            let total_in_page = page.value.len();
            for (idx, raw) in page.value.into_iter().enumerate() {
                contacts.push(ContactItem::from(raw));
                if contacts.len() >= limit {
                    let reached_end = !page_has_more && idx + 1 >= total_in_page;
                    return Ok((contacts, reached_end));
                }
            }
            next_url = page.next_link;
        }

        Ok((contacts, true))
    }

    pub async fn search_contacts_and_directory(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ContactItem>> {
        Ok(self
            .search_contacts_and_directory_detailed(query, limit)
            .await?
            .contacts)
    }

    pub async fn search_contacts_and_directory_detailed(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<ContactSearchOutcome> {
        if limit == 0 {
            return Ok(ContactSearchOutcome {
                contacts: Vec::new(),
                directory_error: None,
            });
        }
        let q = query.trim();
        let q_lower = q.to_lowercase();

        let personal = self.list_contacts(limit.max(100)).await?;
        let matching_personal = personal
            .into_iter()
            .filter(|c| q_lower.is_empty() || contact_matches_query(c, &q_lower));

        let (directory_items, directory_error) =
            self.fetch_directory_matches(q, &q_lower, limit).await;

        let mut merged = Vec::new();
        let mut seen_emails: HashSet<String> = HashSet::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        for item in matching_personal.chain(directory_items.into_iter()) {
            if let Some(primary) = item
                .primary_email()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
            {
                if !seen_emails.insert(primary) {
                    continue;
                }
            } else if !seen_ids.insert(item.id.clone()) {
                continue;
            }
            merged.push(item);
            if merged.len() >= limit {
                break;
            }
        }

        Ok(ContactSearchOutcome {
            contacts: merged,
            directory_error,
        })
    }

    async fn fetch_directory_matches(
        &self,
        query: &str,
        query_lower: &str,
        limit: usize,
    ) -> (Vec<ContactItem>, Option<anyhow::Error>) {
        let headers = [("ConsistencyLevel", "eventual")];
        if query.is_empty() {
            let path = format!("/users?$top={limit}&$select={DIRECTORY_USER_SELECT}");
            return match self.get_json::<GraphListPage<GraphDirectoryUser>>(&path).await {
                Ok(page) => (page.value.into_iter().map(ContactItem::from).collect(), None),
                Err(e) => (Vec::new(), Some(e)),
            };
        }

        let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
        let search_expr = format!("\"displayName:{escaped}\" OR \"mail:{escaped}\"");
        let search_path = format!(
            "/users?$top={limit}&$select={DIRECTORY_USER_SELECT}&$search={}",
            urlencoding::encode(&search_expr)
        );

        if let Ok(page) = self
            .get_json_with_headers::<GraphListPage<GraphDirectoryUser>>(&search_path, &headers)
            .await
        {
            return (page.value.into_iter().map(ContactItem::from).collect(), None);
        }

        // Graceful fallback when `$search` is unsupported; if `/users` fails, preserve the error.
        let fallback_top = limit.max(100);
        let fallback_path = format!("/users?$top={fallback_top}&$select={DIRECTORY_USER_SELECT}");
        match self
            .get_json::<GraphListPage<GraphDirectoryUser>>(&fallback_path)
            .await
        {
            Ok(page) => (
                page.value
                    .into_iter()
                    .map(ContactItem::from)
                    .filter(|c| contact_matches_query(c, query_lower))
                    .take(limit)
                    .collect(),
                None,
            ),
            Err(e) => (Vec::new(), Some(e)),
        }
    }

    pub async fn create_contact(&self, contact: &OutgoingContact) -> Result<ContactItem> {
        let mut payload = serde_json::json!({
            "displayName": contact.display_name,
        });
        if let Some(given) = contact.given_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["givenName"] = serde_json::Value::String(given.to_string());
        }
        if let Some(surname) = contact.surname.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["surname"] = serde_json::Value::String(surname.to_string());
        }
        let emails: Vec<serde_json::Value> = contact
            .email_addresses
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|addr| {
                serde_json::json!({
                    "address": addr,
                    "name": contact.display_name,
                })
            })
            .collect();
        if !emails.is_empty() {
            payload["emailAddresses"] = serde_json::Value::Array(emails);
        }
        if let Some(mobile) = contact.mobile_phone.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["mobilePhone"] = serde_json::Value::String(mobile.to_string());
        }
        if let Some(company) = contact.company_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["companyName"] = serde_json::Value::String(company.to_string());
        }
        if let Some(title) = contact.job_title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["jobTitle"] = serde_json::Value::String(title.to_string());
        }
        if let Some(dept) = contact.department.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["department"] = serde_json::Value::String(dept.to_string());
        }
        if let Some(notes) = contact.notes.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            payload["personalNotes"] = serde_json::Value::String(notes.to_string());
        }

        let created: GraphContact = self.post_json("/me/contacts", &payload).await?;
        Ok(ContactItem::from(created))
    }

    pub async fn delete_contact(&self, contact_id: &str) -> Result<()> {
        let path = format!("/me/contacts/{}", urlencoding::encode(contact_id.trim()));
        self.delete_req(&path).await
    }
}

fn contact_matches_query(c: &ContactItem, q_lower: &str) -> bool {
    c.display_name.to_lowercase().contains(q_lower)
        || c.email_addresses
            .iter()
            .any(|e| e.to_lowercase().contains(q_lower))
        || c.company_name
            .as_deref()
            .map(|s| s.to_lowercase().contains(q_lower))
            .unwrap_or(false)
        || c.department
            .as_deref()
            .map(|s| s.to_lowercase().contains(q_lower))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::mail::tests::test_graph_client;
    use axum::{
        extract::{Path as AxumPath, Query, State},
        http::{HeaderMap, StatusCode},
        routing::get,
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    };

    #[tokio::test]
    async fn test_list_create_delete_contacts() {
        #[derive(Clone)]
        struct ServerState {
            base_url: Arc<Mutex<String>>,
            deleted: Arc<Mutex<Vec<String>>>,
        }
        let state = ServerState {
            base_url: Arc::new(Mutex::new(String::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
        };
        let state_clone = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/contacts",
                get(|State(st): State<ServerState>, Query(params): Query<HashMap<String, String>>| async move {
                    if params.get("page").map(String::as_str) == Some("2") {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "c2",
                                    "displayName": "Bob Miller",
                                    "emailAddresses": [{ "address": "bob@example.com" }],
                                    "personalNotes": "VIP"
                                }
                            ]
                        }))
                    } else {
                        let base = st.base_url.lock().unwrap().clone();
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "c1",
                                    "displayName": "Alice Smith",
                                    "emailAddresses": [{ "address": "alice@example.com" }],
                                    "companyName": "Contoso",
                                    "department": "Engineering"
                                }
                            ],
                            "@odata.nextLink": format!("{base}/v1.0/me/contacts?page=2")
                        }))
                    }
                })
                .post(|Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["personalNotes"], "Key partner");
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({
                            "id": "c-new",
                            "displayName": body["displayName"],
                            "emailAddresses": body["emailAddresses"],
                            "personalNotes": body["personalNotes"]
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/contacts/{id}",
                axum::routing::delete(|State(st): State<ServerState>, AxumPath(id): AxumPath<String>| async move {
                    st.deleted.lock().unwrap().push(id);
                    StatusCode::NO_CONTENT
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        *state_clone.base_url.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));
        let list = client.list_contacts(10).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].source, "contact");
        assert_eq!(list[1].notes.as_deref(), Some("VIP"));

        let created = client
            .create_contact(&OutgoingContact {
                display_name: "Charlie Brown".into(),
                given_name: Some("Charlie".into()),
                surname: Some("Brown".into()),
                email_addresses: vec!["charlie@example.com".into()],
                mobile_phone: Some("555-0100".into()),
                company_name: Some("Acme".into()),
                job_title: Some("Lead".into()),
                department: Some("Platform".into()),
                notes: Some("Key partner".into()),
            })
            .await
            .unwrap();
        assert_eq!(created.id, "c-new");
        assert_eq!(created.primary_email(), Some("charlie@example.com"));

        client.delete_contact("c-new").await.unwrap();
        assert_eq!(*state_clone.deleted.lock().unwrap(), vec!["c-new"]);
    }

    #[tokio::test]
    async fn test_search_contacts_and_directory_dedup_and_403_fallback() {
        let forbid_users = Arc::new(AtomicBool::new(false));
        let forbid_clone = forbid_users.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/contacts",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "c1",
                                "displayName": "Alice (Personal)",
                                "emailAddresses": [{ "address": "Alice@Example.com" }],
                                "companyName": "Contoso",
                                "department": "Platform"
                            },
                            {
                                "id": "c2",
                                "displayName": "Bob Lee",
                                "emailAddresses": [{ "address": "bob@example.com" }],
                                "department": "Design"
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/users",
                get(move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| {
                    let forbid = forbid_clone.load(Ordering::SeqCst);
                    async move {
                        if forbid {
                            return (
                                StatusCode::FORBIDDEN,
                                Json(serde_json::json!({
                                    "error": { "code": "Authorization_RequestDenied", "message": "Forbidden" }
                                })),
                            );
                        }
                        if params.contains_key("$search") {
                            assert_eq!(
                                headers.get("consistencylevel").and_then(|v| v.to_str().ok()),
                                Some("eventual")
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "u1",
                                        "displayName": "Alice Smith",
                                        "mail": "alice@example.com",
                                        "department": "Platform"
                                    },
                                    {
                                        "id": "u2",
                                        "displayName": "Diana Prince",
                                        "mail": "diana@contoso.com",
                                        "department": "Platform"
                                    }
                                ]
                            })),
                        )
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));

        let res = client
            .search_contacts_and_directory("platform", 10)
            .await
            .unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].id, "c1");
        assert_eq!(res[0].source, "contact");
        assert_eq!(res[1].id, "u2");
        assert_eq!(res[1].source, "directory");

        forbid_users.store(true, Ordering::SeqCst);
        let fallback_res = client
            .search_contacts_and_directory("contoso", 10)
            .await
            .unwrap();
        assert_eq!(fallback_res.len(), 1);
        assert_eq!(fallback_res[0].id, "c1");
    }
}
