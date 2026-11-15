//! Microsoft Graph v1.0 OneNote operations across `/me/onenote`, `/groups/{id}/onenote`, and `/sites/{id}/onenote`.

use std::collections::HashMap;

use anyhow::{bail, Result};
use futures::future::join_all;
use serde::Deserialize;

use super::{
    client::{GraphIdentitySet, GraphListPage},
    GraphClient,
};
use crate::models::{
    split_onenote_entity_key, InlineImageUpload, OnenoteNotebook, OnenotePageDetail,
    OnenotePageSummary, OnenotePatchCommand, OnenoteScope, OnenoteSection, OnenoteSectionGroup,
    OutgoingOnenotePage,
};

const MAX_JOINED_TEAMS_FOR_ONENOTE: usize = 25;
const ONENOTE_SCOPE_CONCURRENCY: usize = 6;

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteLink {
    #[serde(default)]
    href: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteLinks {
    #[serde(rename = "oneNoteWebUrl", default)]
    onenote_web_url: Option<GraphOnenoteLink>,
}

impl GraphOnenoteLinks {
    fn web_url(self) -> Option<String> {
        self.onenote_web_url
            .and_then(|l| l.href)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphParentEntity {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

impl GraphParentEntity {
    fn into_id_and_name(self) -> (Option<String>, Option<String>) {
        let id = self
            .id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        (id, name)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphJoinedTeamLite {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteNotebook {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(rename = "isDefault", default)]
    is_default: bool,
    #[serde(rename = "isShared", default)]
    is_shared: bool,
    #[serde(rename = "userRole", default)]
    user_role: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "createdBy", default)]
    created_by: Option<GraphIdentitySet>,
    #[serde(default)]
    links: Option<GraphOnenoteLinks>,
}

impl GraphOnenoteNotebook {
    fn into_notebook(self, scope: &OnenoteScope) -> OnenoteNotebook {
        let id = self.id;
        let notebook_key = scope.make_entity_key(&id);
        let display_name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());
        let created_by = self
            .created_by
            .as_ref()
            .and_then(|c| c.primary_name())
            .map(str::to_string);

        OnenoteNotebook {
            notebook_key,
            id,
            scope: scope.clone(),
            display_name,
            color: self
                .color
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            is_default: self.is_default,
            is_shared: self.is_shared,
            user_role: self
                .user_role
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
            created_by,
            web_url: self.links.and_then(GraphOnenoteLinks::web_url),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteSectionGroup {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "parentNotebook", default)]
    parent_notebook: Option<GraphParentEntity>,
    #[serde(rename = "parentSectionGroup", default)]
    parent_section_group: Option<GraphParentEntity>,
}

impl GraphOnenoteSectionGroup {
    fn into_section_group(
        self,
        scope: &OnenoteScope,
        fallback_notebook_id: Option<&str>,
    ) -> OnenoteSectionGroup {
        let id = self.id;
        let section_group_key = scope.make_entity_key(&id);
        let notebook_id = self
            .parent_notebook
            .and_then(|p| p.into_id_and_name().0)
            .or_else(|| fallback_notebook_id.map(str::to_string));
        let parent_section_group_id = self
            .parent_section_group
            .and_then(|p| p.into_id_and_name().0);
        let display_name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());

        OnenoteSectionGroup {
            section_group_key,
            id,
            scope: scope.clone(),
            notebook_id,
            parent_section_group_id,
            display_name,
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteSection {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(rename = "isDefault", default)]
    is_default: bool,
    #[serde(rename = "pagesUrl", default)]
    pages_url: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "parentNotebook", default)]
    parent_notebook: Option<GraphParentEntity>,
    #[serde(rename = "parentSectionGroup", default)]
    parent_section_group: Option<GraphParentEntity>,
    #[serde(default)]
    links: Option<GraphOnenoteLinks>,
}

impl GraphOnenoteSection {
    fn into_section(
        self,
        scope: &OnenoteScope,
        fallback_notebook_id: Option<&str>,
    ) -> OnenoteSection {
        let id = self.id;
        let section_key = scope.make_entity_key(&id);
        let (nb_id, notebook_name) = self
            .parent_notebook
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let notebook_id = nb_id.or_else(|| fallback_notebook_id.map(str::to_string));
        let (parent_section_group_id, parent_section_group_name) = self
            .parent_section_group
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let display_name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());

        OnenoteSection {
            section_key,
            id,
            scope: scope.clone(),
            notebook_id,
            notebook_name,
            parent_section_group_id,
            parent_section_group_name,
            display_name,
            color: self
                .color
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            is_default: self.is_default,
            pages_url: self
                .pages_url
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
            web_url: self.links.and_then(GraphOnenoteLinks::web_url),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenotePage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    level: Option<i32>,
    #[serde(default)]
    order: Option<i32>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "contentUrl", default)]
    content_url: Option<String>,
    #[serde(default)]
    links: Option<GraphOnenoteLinks>,
    #[serde(rename = "parentSection", default)]
    parent_section: Option<GraphParentEntity>,
    #[serde(rename = "parentNotebook", default)]
    parent_notebook: Option<GraphParentEntity>,
}

impl GraphOnenotePage {
    fn into_page_summary(
        self,
        scope: &OnenoteScope,
        fallback_section_id: Option<&str>,
        fallback_order: i32,
    ) -> OnenotePageSummary {
        let id = self.id;
        let page_key = scope.make_entity_key(&id);
        let (sec_id, section_name) = self
            .parent_section
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let section_id = sec_id
            .or_else(|| fallback_section_id.map(str::to_string))
            .unwrap_or_default();
        let (notebook_id, notebook_name) = self
            .parent_notebook
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let title = self
            .title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(제목 없음)".to_string());

        OnenotePageSummary {
            page_key,
            id,
            scope: scope.clone(),
            section_id,
            section_name,
            notebook_id,
            notebook_name,
            title,
            level: self.level.unwrap_or(0).max(0),
            order: self.order.unwrap_or(fallback_order),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
            content_url: self
                .content_url
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            web_url: self.links.and_then(GraphOnenoteLinks::web_url),
            preview_text: None,
            has_todo: false,
        }
    }
}

fn clean_raw_onenote_id(id_or_composite: &str) -> String {
    let trimmed = id_or_composite.trim();
    if trimmed.contains('|') {
        split_onenote_entity_key(trimmed).1
    } else {
        trimmed.to_string()
    }
}

fn is_pagelevel_bad_request(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("400") || msg.contains("pagelevel")
}

fn notebook_richness(nb: &OnenoteNotebook) -> usize {
    let mut score = 0usize;
    if !nb.scope.is_personal() {
        score += 2;
    }
    if nb.color.is_some() {
        score += 1;
    }
    if nb.user_role.is_some() {
        score += 1;
    }
    if nb.created_at.is_some() {
        score += 1;
    }
    if nb.last_modified_at.is_some() {
        score += 1;
    }
    if nb.created_by.is_some() {
        score += 1;
    }
    if nb.web_url.is_some() {
        score += 1;
    }
    score
}

fn merge_duplicate_notebook(existing: &mut OnenoteNotebook, mut candidate: OnenoteNotebook) {
    if notebook_richness(&candidate) > notebook_richness(existing) {
        if candidate.color.is_none() {
            candidate.color = existing.color.clone();
        }
        if candidate.user_role.is_none() {
            candidate.user_role = existing.user_role.clone();
        }
        if candidate.created_at.is_none() {
            candidate.created_at = existing.created_at.clone();
        }
        if candidate.last_modified_at.is_none() {
            candidate.last_modified_at = existing.last_modified_at.clone();
        }
        if candidate.created_by.is_none() {
            candidate.created_by = existing.created_by.clone();
        }
        if candidate.web_url.is_none() {
            candidate.web_url = existing.web_url.clone();
        }
        candidate.is_default |= existing.is_default;
        candidate.is_shared |= existing.is_shared;
        *existing = candidate;
    } else {
        if existing.color.is_none() {
            existing.color = candidate.color;
        }
        if existing.user_role.is_none() {
            existing.user_role = candidate.user_role;
        }
        if existing.created_at.is_none() {
            existing.created_at = candidate.created_at;
        }
        if existing.last_modified_at.is_none() {
            existing.last_modified_at = candidate.last_modified_at;
        }
        if existing.created_by.is_none() {
            existing.created_by = candidate.created_by;
        }
        if existing.web_url.is_none() {
            existing.web_url = candidate.web_url;
        }
        existing.is_default |= candidate.is_default;
        existing.is_shared |= candidate.is_shared;
    }
}

impl GraphClient {
    async fn list_joined_teams_lite(&self, max_teams: usize) -> Result<Vec<(String, String)>> {
        let mut next_url = Some("/me/joinedTeams".to_string());
        let mut out = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphJoinedTeamLite> = self.get_json(&url).await?;
            for raw in page.value {
                if raw.id.trim().is_empty() {
                    continue;
                }
                let name = raw
                    .display_name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| raw.id.clone());
                out.push((raw.id, name));
                if out.len() >= max_teams {
                    return Ok(out);
                }
            }
            next_url = page.next_link;
        }

        Ok(out)
    }

    pub async fn list_onenote_notebooks(
        &self,
        scope: &OnenoteScope,
    ) -> Result<Vec<OnenoteNotebook>> {
        let mut next_url = Some(format!("{}/notebooks?$top=100", scope.graph_base_path()));
        let mut notebooks = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteNotebook> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    notebooks.push(raw.into_notebook(scope));
                }
            }
            next_url = page.next_link;
        }

        Ok(notebooks)
    }

    pub async fn list_all_accessible_notebooks(
        &self,
        include_teams: bool,
    ) -> Result<Vec<OnenoteNotebook>> {
        let me_res = self.list_onenote_notebooks(&OnenoteScope::Me).await;
        if !include_teams {
            return me_res;
        }

        let mut merged: Vec<OnenoteNotebook> = Vec::new();
        let mut index_by_id: HashMap<String, usize> = HashMap::new();
        let mut any_succeeded = false;
        let mut me_err = None;

        match me_res {
            Ok(personal) => {
                any_succeeded = true;
                for nb in personal {
                    if let Some(&pos) = index_by_id.get(&nb.id) {
                        merge_duplicate_notebook(&mut merged[pos], nb);
                    } else {
                        index_by_id.insert(nb.id.clone(), merged.len());
                        merged.push(nb);
                    }
                }
            }
            Err(err) => {
                me_err = Some(err);
            }
        }

        if let Ok(teams) = self
            .list_joined_teams_lite(MAX_JOINED_TEAMS_FOR_ONENOTE)
            .await
        {
            let scopes: Vec<OnenoteScope> = teams
                .into_iter()
                .map(|(group_id, group_name)| OnenoteScope::Group {
                    group_id,
                    group_name: Some(group_name),
                })
                .collect();

            for chunk in scopes.chunks(ONENOTE_SCOPE_CONCURRENCY) {
                let futs = chunk.iter().map(|s| self.list_onenote_notebooks(s));
                let results = join_all(futs).await;
                for res in results {
                    if let Ok(group_nbs) = res {
                        any_succeeded = true;
                        for nb in group_nbs {
                            if let Some(&pos) = index_by_id.get(&nb.id) {
                                merge_duplicate_notebook(&mut merged[pos], nb);
                            } else {
                                index_by_id.insert(nb.id.clone(), merged.len());
                                merged.push(nb);
                            }
                        }
                    }
                }
            }
        }

        if !any_succeeded {
            if let Some(err) = me_err {
                return Err(err);
            }
        }

        Ok(merged)
    }

    pub async fn create_onenote_notebook(
        &self,
        scope: &OnenoteScope,
        display_name: &str,
    ) -> Result<OnenoteNotebook> {
        let name = display_name.trim();
        if name.is_empty() {
            bail!("OneNote 필기장 이름을 입력해야 합니다.");
        }
        let path = format!("{}/notebooks", scope.graph_base_path());
        let raw: GraphOnenoteNotebook = self
            .post_json(&path, &serde_json::json!({ "displayName": name }))
            .await?;
        Ok(raw.into_notebook(scope))
    }

    pub async fn list_onenote_section_groups(
        &self,
        scope: &OnenoteScope,
        notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSectionGroup>> {
        let nb_clean = notebook_id
            .map(clean_raw_onenote_id)
            .filter(|s| !s.is_empty());
        let base = scope.graph_base_path();
        let mut next_url = Some(match nb_clean.as_deref() {
            Some(nb_id) => format!(
                "{base}/notebooks/{}/sectionGroups?$top=100",
                urlencoding::encode(nb_id)
            ),
            None => format!("{base}/sectionGroups?$top=100"),
        });

        let mut groups = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteSectionGroup> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    groups.push(raw.into_section_group(scope, nb_clean.as_deref()));
                }
            }
            next_url = page.next_link;
        }

        Ok(groups)
    }

    pub async fn list_onenote_sections(
        &self,
        scope: &OnenoteScope,
        notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSection>> {
        let nb_clean = notebook_id
            .map(clean_raw_onenote_id)
            .filter(|s| !s.is_empty());
        let base = scope.graph_base_path();
        let mut next_url = Some(match nb_clean.as_deref() {
            Some(nb_id) => format!(
                "{base}/notebooks/{}/sections?$top=100",
                urlencoding::encode(nb_id)
            ),
            None => format!("{base}/sections?$top=100"),
        });

        let mut sections = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteSection> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    sections.push(raw.into_section(scope, nb_clean.as_deref()));
                }
            }
            next_url = page.next_link;
        }

        Ok(sections)
    }

    pub async fn list_all_accessible_sections(
        &self,
        notebooks: &[OnenoteNotebook],
    ) -> Result<(Vec<OnenoteSectionGroup>, Vec<OnenoteSection>)> {
        let mut unique_scopes: Vec<OnenoteScope> = Vec::new();
        let mut scope_pos_by_key: HashMap<String, usize> = HashMap::new();

        if notebooks.is_empty() {
            unique_scopes.push(OnenoteScope::Me);
        } else {
            for nb in notebooks {
                let sk = nb.scope.scope_key();
                if let Some(&pos) = scope_pos_by_key.get(&sk) {
                    let replace = matches!(
                        (&unique_scopes[pos], &nb.scope),
                        (
                            OnenoteScope::Group {
                                group_name: None,
                                ..
                            },
                            OnenoteScope::Group {
                                group_name: Some(_),
                                ..
                            }
                        ) | (
                            OnenoteScope::Site {
                                site_name: None,
                                ..
                            },
                            OnenoteScope::Site {
                                site_name: Some(_),
                                ..
                            }
                        )
                    );
                    if replace {
                        unique_scopes[pos] = nb.scope.clone();
                    }
                } else {
                    scope_pos_by_key.insert(sk, unique_scopes.len());
                    unique_scopes.push(nb.scope.clone());
                }
            }
        }

        let mut nb_name_by_scoped_id: HashMap<(String, String), String> = HashMap::new();
        let mut nb_name_by_id: HashMap<String, String> = HashMap::new();
        for nb in notebooks {
            nb_name_by_scoped_id.insert(
                (nb.scope.scope_key(), nb.id.clone()),
                nb.display_name.clone(),
            );
            nb_name_by_id
                .entry(nb.id.clone())
                .or_insert_with(|| nb.display_name.clone());
        }

        let mut all_groups: Vec<OnenoteSectionGroup> = Vec::new();
        let mut seen_groups: HashMap<String, usize> = HashMap::new();
        let mut all_sections: Vec<OnenoteSection> = Vec::new();
        let mut seen_sections: HashMap<String, usize> = HashMap::new();

        for chunk in unique_scopes.chunks(ONENOTE_SCOPE_CONCURRENCY) {
            let futs = chunk.iter().map(|scope| async move {
                let (groups_res, secs_res) = tokio::join!(
                    self.list_onenote_section_groups(scope, None),
                    self.list_onenote_sections(scope, None)
                );
                (groups_res, secs_res)
            });
            let results = join_all(futs).await;
            for (groups_res, secs_res) in results {
                if let Ok(groups) = groups_res {
                    for g in groups {
                        if !seen_groups.contains_key(&g.section_group_key) {
                            seen_groups.insert(g.section_group_key.clone(), all_groups.len());
                            all_groups.push(g);
                        }
                    }
                }
                if let Ok(secs) = secs_res {
                    for s in secs {
                        if !seen_sections.contains_key(&s.section_key) {
                            seen_sections.insert(s.section_key.clone(), all_sections.len());
                            all_sections.push(s);
                        }
                    }
                }
            }
        }

        let sg_name_by_scoped_id: HashMap<(String, String), String> = all_groups
            .iter()
            .map(|sg| ((sg.scope.scope_key(), sg.id.clone()), sg.display_name.clone()))
            .collect();

        for sec in &mut all_sections {
            if sec.notebook_name.is_none() {
                if let Some(ref nb_id) = sec.notebook_id {
                    let sk = sec.scope.scope_key();
                    sec.notebook_name = nb_name_by_scoped_id
                        .get(&(sk, nb_id.clone()))
                        .or_else(|| nb_name_by_id.get(nb_id))
                        .cloned();
                }
            }
            if sec.parent_section_group_name.is_none() {
                if let Some(ref sg_id) = sec.parent_section_group_id {
                    let sk = sec.scope.scope_key();
                    sec.parent_section_group_name =
                        sg_name_by_scoped_id.get(&(sk, sg_id.clone())).cloned();
                }
            }
        }

        Ok((all_groups, all_sections))
    }

    pub async fn create_onenote_section(
        &self,
        scope: &OnenoteScope,
        notebook_id: &str,
        display_name: &str,
    ) -> Result<OnenoteSection> {
        let nb_id = clean_raw_onenote_id(notebook_id);
        if nb_id.is_empty() {
            bail!("OneNote 필기장 ID를 지정해야 합니다.");
        }
        let name = display_name.trim();
        if name.is_empty() {
            bail!("OneNote 섹션 이름을 입력해야 합니다.");
        }
        let path = format!(
            "{}/notebooks/{}/sections",
            scope.graph_base_path(),
            urlencoding::encode(&nb_id)
        );
        let raw: GraphOnenoteSection = self
            .post_json(&path, &serde_json::json!({ "displayName": name }))
            .await?;
        Ok(raw.into_section(scope, Some(&nb_id)))
    }

    pub async fn list_onenote_pages(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        limit: usize,
    ) -> Result<Vec<OnenotePageSummary>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let sec_id = clean_raw_onenote_id(section_id);
        if sec_id.is_empty() {
            bail!("OneNote 섹션 ID를 지정해야 합니다.");
        }
        let top = limit.clamp(1, 100);
        let base = scope.graph_base_path();
        let sec_enc = urlencoding::encode(&sec_id);

        let url_with_level = format!("{base}/sections/{sec_enc}/pages?$top={top}&pagelevel=true");
        let first_page: GraphListPage<GraphOnenotePage> = match self.get_json(&url_with_level).await
        {
            Ok(page) => page,
            Err(err) if is_pagelevel_bad_request(&err) => {
                let fallback_url = format!("{base}/sections/{sec_enc}/pages?$top={top}");
                self.get_json(&fallback_url).await?
            }
            Err(err) => return Err(err),
        };

        let mut pages = Vec::new();
        for raw in first_page.value {
            if !raw.id.trim().is_empty() {
                let idx = pages.len() as i32;
                pages.push(raw.into_page_summary(scope, Some(&sec_id), idx));
                if pages.len() >= limit {
                    return Ok(pages);
                }
            }
        }

        let mut next_url = first_page.next_link;
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenotePage> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    let idx = pages.len() as i32;
                    pages.push(raw.into_page_summary(scope, Some(&sec_id), idx));
                    if pages.len() >= limit {
                        return Ok(pages);
                    }
                }
            }
            next_url = page.next_link;
        }

        Ok(pages)
    }

    pub async fn get_onenote_page_detail(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        cached_summary: Option<OnenotePageSummary>,
    ) -> Result<OnenotePageDetail> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote 페이지 ID를 지정해야 합니다.");
        }
        let base = scope.graph_base_path();
        let pid_enc = urlencoding::encode(&pid);

        let summary = match cached_summary {
            Some(s) => s,
            None => {
                let meta_with_level = format!("{base}/pages/{pid_enc}?pagelevel=true");
                let raw_meta: GraphOnenotePage = match self.get_json(&meta_with_level).await {
                    Ok(meta) => meta,
                    Err(err) if is_pagelevel_bad_request(&err) => {
                        let fallback_meta = format!("{base}/pages/{pid_enc}");
                        self.get_json(&fallback_meta).await?
                    }
                    Err(err) => return Err(err),
                };
                raw_meta.into_page_summary(scope, None, 0)
            }
        };

        let raw_xhtml = self
            .get_text(&format!("{base}/pages/{pid_enc}/content?includeIDs=true"))
            .await?;
        let mut detail = crate::parser::onenote::parse_onenote_page_xhtml(summary, &raw_xhtml);
        detail.synced_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(detail)
    }

    pub async fn create_onenote_page(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        outgoing: &OutgoingOnenotePage,
    ) -> Result<OnenotePageSummary> {
        let sec_id = clean_raw_onenote_id(section_id);
        if sec_id.is_empty() {
            bail!("OneNote 섹션 ID를 지정해야 합니다.");
        }
        let (bytes, content_type) = crate::parser::onenote::compile_new_page_payload(outgoing);
        let path = format!(
            "{}/sections/{}/pages",
            scope.graph_base_path(),
            urlencoding::encode(&sec_id)
        );
        let raw: GraphOnenotePage = self.post_bytes_json(&path, bytes, &content_type).await?;
        let had_wire_title = raw
            .title
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false);
        let mut summary = raw.into_page_summary(scope, Some(&sec_id), 0);
        if !had_wire_title && !outgoing.title.trim().is_empty() {
            summary.title = outgoing.title.trim().to_string();
        }
        if summary.section_id.is_empty() {
            summary.section_id = sec_id;
        }
        if let Some(l) = outgoing.level.filter(|&l| l > 0) {
            summary.level = l;
            if !summary.id.is_empty() {
                let _ = self
                    .patch_json(
                        &format!(
                            "{}/pages/{}",
                            scope.graph_base_path(),
                            urlencoding::encode(&summary.id)
                        ),
                        &serde_json::json!({ "level": l }),
                    )
                    .await;
            }
        }
        Ok(summary)
    }

    pub async fn patch_onenote_page_commands(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        commands: &[OnenotePatchCommand],
    ) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote 페이지 ID를 지정해야 합니다.");
        }
        let bytes = serde_json::to_vec(commands)?;
        self.patch_bytes_empty(
            &format!(
                "{}/pages/{}/content",
                scope.graph_base_path(),
                urlencoding::encode(&pid)
            ),
            bytes,
            "application/json",
        )
        .await
    }

    pub async fn append_onenote_page_markdown(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        primary_div_id: Option<&str>,
        markdown: &str,
        inline_images: &[InlineImageUpload],
    ) -> Result<()> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote 페이지 ID를 지정해야 합니다.");
        }
        let (bytes, ct) = crate::parser::onenote::build_append_markdown_payload(
            primary_div_id,
            markdown,
            inline_images,
        );
        self.patch_bytes_empty(
            &format!(
                "{}/pages/{}/content",
                scope.graph_base_path(),
                urlencoding::encode(&pid)
            ),
            bytes,
            &ct,
        )
        .await
    }

    pub async fn delete_onenote_page(&self, scope: &OnenoteScope, page_id: &str) -> Result<()> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote 페이지 ID를 지정해야 합니다.");
        }
        self.delete_req(&format!(
            "{}/pages/{}",
            scope.graph_base_path(),
            urlencoding::encode(&pid)
        ))
        .await
    }

    pub async fn download_onenote_resource_bytes(&self, resource_url: &str) -> Result<Vec<u8>> {
        self.get_bytes(resource_url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::mail::tests::test_graph_client;
    use axum::{
        body::Bytes,
        extract::{Path as AxumPath, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_onenote_hierarchy_pages_fallback_and_mutations() {
        #[derive(Clone, Default)]
        struct MockState {
            patched_levels: Arc<Mutex<Vec<(String, i32)>>>,
            content_patches: Arc<Mutex<Vec<(String, String)>>>,
            deleted_pages: Arc<Mutex<Vec<String>>>,
        }

        let state = MockState::default();
        let state_check = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "nb-me",
                                "displayName": "개인 노트",
                                "isDefault": true,
                                "isShared": false,
                                "userRole": "Owner",
                                "createdBy": { "user": { "displayName": "홍길동" } },
                                "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-me" } }
                            },
                            {
                                "id": "nb-dup",
                                "displayName": "공유 노트"
                            }
                        ]
                    }))
                })
                .post(|Json(body): Json<serde_json::Value>| async move {
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({
                            "id": "nb-created",
                            "displayName": body["displayName"],
                            "isDefault": false,
                            "isShared": false
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "team-ok", "displayName": "플랫폼팀" },
                            { "id": "team-404", "displayName": "미개설팀" }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/notebooks",
                get(|AxumPath(gid): AxumPath<String>| async move {
                    if gid == "team-ok" {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "nb-dup",
                                        "displayName": "공유 노트",
                                        "isShared": true,
                                        "userRole": "Contributor",
                                        "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-dup" } }
                                    }
                                ]
                            })),
                        )
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({
                                "error": { "code": "ResourceNotFound", "message": "No notebook" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sg-1",
                                "displayName": "2026 아카이브",
                                "parentNotebook": { "id": "nb-me", "displayName": "개인 노트" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/sectionGroups",
                get(|AxumPath(_gid): AxumPath<String>| async move {
                    Json(serde_json::json!({ "value": [] }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sec-1",
                                "displayName": "주간 회의",
                                "isDefault": true,
                                "parentNotebook": { "id": "nb-me" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/sections",
                get(|AxumPath(_gid): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sec-team",
                                "displayName": "런북",
                                "parentNotebook": { "id": "nb-dup" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sections",
                post(
                    |AxumPath(nb_id): AxumPath<String>, Json(body): Json<serde_json::Value>| async move {
                        assert_eq!(nb_id, "nb-me");
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({
                                "id": "sec-created",
                                "displayName": body["displayName"]
                            })),
                        )
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}/pages",
                get(
                    |AxumPath(sec_id): AxumPath<String>,
                     Query(params): Query<HashMap<String, String>>| async move {
                        assert_eq!(sec_id, "sec-1");
                        if params.contains_key("pagelevel") {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({
                                    "error": { "code": "20152", "message": "pagelevel is not supported here" }
                                })),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "pg-1",
                                        "title": "",
                                        "createdDateTime": "2026-11-15T09:00:00Z",
                                        "lastModifiedDateTime": "2026-11-15T10:00:00Z"
                                    }
                                ]
                            })),
                        )
                    },
                )
                .post(
                    |AxumPath(sec_id): AxumPath<String>, headers: HeaderMap, body: Bytes| async move {
                        assert_eq!(sec_id, "sec-1");
                        assert!(!body.is_empty());
                        assert!(headers.contains_key("content-type"));
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({
                                "id": "pg-new",
                                "title": "하위 페이지"
                            })),
                        )
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/pages/{page_id}",
                get(|AxumPath(page_id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "id": page_id,
                        "title": "아키텍처 설계",
                        "level": 1,
                        "order": 2,
                        "parentSection": { "id": "sec-1", "displayName": "주간 회의" }
                    }))
                })
                .patch(
                    |State(st): State<MockState>,
                     AxumPath(page_id): AxumPath<String>,
                     Json(body): Json<serde_json::Value>| async move {
                        let lvl = body["level"].as_i64().unwrap_or(0) as i32;
                        st.patched_levels.lock().unwrap().push((page_id, lvl));
                        StatusCode::NO_CONTENT
                    },
                )
                .delete(
                    |State(st): State<MockState>, AxumPath(page_id): AxumPath<String>| async move {
                        st.deleted_pages.lock().unwrap().push(page_id);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/pages/{page_id}/content",
                get(|AxumPath(_page_id): AxumPath<String>| async move {
                    (
                        StatusCode::OK,
                        r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>아키텍처 설계</title></head><body><div id="div:{1111}{32}"><p id="p:{2222}{40}" data-tag="to-do">마이그레이션 점검</p></div></body></html>"#,
                    )
                })
                .patch(
                    |State(st): State<MockState>,
                     AxumPath(page_id): AxumPath<String>,
                     headers: HeaderMap,
                     body: Bytes| async move {
                        let ct = headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        assert!(!body.is_empty());
                        st.content_patches.lock().unwrap().push((page_id, ct));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/resources/{res_id}/content",
                get(|AxumPath(res_id): AxumPath<String>| async move {
                    assert_eq!(res_id, "res-1");
                    vec![0x89u8, b'P', b'N', b'G']
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(&format!("http://{addr}/v1.0"));

        // 1. Accessible notebooks across Me + Joined Teams (deduped, 404 ignored)
        let notebooks = client.list_all_accessible_notebooks(true).await.unwrap();
        assert_eq!(notebooks.len(), 2);
        assert_eq!(notebooks[0].notebook_key, "me|nb-me");
        assert_eq!(notebooks[0].created_by.as_deref(), Some("홍길동"));
        assert_eq!(notebooks[1].notebook_key, "group:team-ok|nb-dup");
        assert_eq!(notebooks[1].scope.owner_label(), "플랫폼팀");

        // 2. Accessible section groups & sections with notebook_name enrichment
        let (sgs, secs) = client
            .list_all_accessible_sections(&notebooks)
            .await
            .unwrap();
        assert_eq!(sgs.len(), 1);
        assert_eq!(sgs[0].section_group_key, "me|sg-1");
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].notebook_name.as_deref(), Some("개인 노트"));
        assert_eq!(secs[1].notebook_name.as_deref(), Some("공유 노트"));

        // 3. Create notebook & section
        let created_nb = client
            .create_onenote_notebook(&OnenoteScope::Me, "신규 노트")
            .await
            .unwrap();
        assert_eq!(created_nb.notebook_key, "me|nb-created");
        let created_sec = client
            .create_onenote_section(&OnenoteScope::Me, "nb-me", "스프린트")
            .await
            .unwrap();
        assert_eq!(created_sec.notebook_id.as_deref(), Some("nb-me"));

        // 4. List pages with automatic fallback on 400 pagelevel error + untitled fallback
        let pages = client
            .list_onenote_pages(&OnenoteScope::Me, "sec-1", 20)
            .await
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_key, "me|pg-1");
        assert_eq!(pages[0].title, "(제목 없음)");
        assert_eq!(pages[0].section_id, "sec-1");

        // 5. Page detail fetch + XHTML parse
        let detail = client
            .get_onenote_page_detail(&OnenoteScope::Me, "pg-1", None)
            .await
            .unwrap();
        assert_eq!(detail.summary.id, "pg-1");
        assert!(detail.body_plain.contains("마이그레이션 점검"));
        assert!(detail.synced_at.is_some());

        // 6. Create subpage (triggers level PATCH) + patch commands + append markdown + resource + delete
        let new_page = client
            .create_onenote_page(
                &OnenoteScope::Me,
                "sec-1",
                &OutgoingOnenotePage {
                    title: "하위 페이지".into(),
                    markdown_body: "내용입니다".into(),
                    inline_images: Vec::new(),
                    level: Some(1),
                },
            )
            .await
            .unwrap();
        assert_eq!(new_page.page_key, "me|pg-new");
        assert_eq!(new_page.section_id, "sec-1");
        assert_eq!(new_page.level, 1);
        assert_eq!(
            *state_check.patched_levels.lock().unwrap(),
            vec![("pg-new".to_string(), 1)]
        );

        client
            .patch_onenote_page_commands(
                &OnenoteScope::Me,
                "pg-1",
                &[OnenotePatchCommand {
                    target: "p:{2222}{40}".into(),
                    action: "replace".into(),
                    position: None,
                    content: "<p data-tag=\"to-do:completed\">마이그레이션 점검</p>".into(),
                }],
            )
            .await
            .unwrap();

        client
            .append_onenote_page_markdown(
                &OnenoteScope::Me,
                "pg-1",
                detail.primary_div_id.as_deref(),
                "추가 메모",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(state_check.content_patches.lock().unwrap().len(), 2);

        let bytes = client
            .download_onenote_resource_bytes(&format!(
                "http://{addr}/v1.0/me/onenote/resources/res-1/content"
            ))
            .await
            .unwrap();
        assert_eq!(bytes, vec![0x89u8, b'P', b'N', b'G']);

        client
            .delete_onenote_page(&OnenoteScope::Me, "pg-1")
            .await
            .unwrap();
        assert_eq!(
            *state_check.deleted_pages.lock().unwrap(),
            vec!["pg-1".to_string()]
        );
    }
}
