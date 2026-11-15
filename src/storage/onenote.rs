//! SQLite persistence and FTS5 (`trigram` + `LIKE` hybrid) search for Microsoft OneNote.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    models::{
        OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteScope, OnenoteSearchHit,
        OnenoteSection, OnenoteSectionGroup,
    },
    storage::repo::escape_like,
};

const PAGE_SUMMARY_COLS: &str = "
    page_key, id, scope_json, scope_key, section_id, section_name,
    notebook_id, notebook_name, title, level, page_order,
    created_at, last_modified_at, content_url, web_url, preview_text, has_todo
";

pub fn upsert_onenote_notebooks(conn: &mut Connection, items: &[OnenoteNotebook]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "
            INSERT INTO onenote_notebooks (
                notebook_key, id, scope_json, scope_key, display_name, color,
                is_default, is_shared, user_role, created_at, last_modified_at,
                created_by, web_url
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10, ?11,
                ?12, ?13
            )
            ON CONFLICT(notebook_key) DO UPDATE SET
                id = excluded.id,
                scope_json = excluded.scope_json,
                scope_key = excluded.scope_key,
                display_name = excluded.display_name,
                color = COALESCE(excluded.color, onenote_notebooks.color),
                is_default = excluded.is_default,
                is_shared = excluded.is_shared,
                user_role = COALESCE(excluded.user_role, onenote_notebooks.user_role),
                created_at = COALESCE(excluded.created_at, onenote_notebooks.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, onenote_notebooks.last_modified_at),
                created_by = COALESCE(excluded.created_by, onenote_notebooks.created_by),
                web_url = COALESCE(excluded.web_url, onenote_notebooks.web_url)
            ",
        )?;

        for nb in items {
            let scope_key = nb.scope.scope_key();
            let scope_json = serde_json::to_string(&nb.scope)?;
            let notebook_key = if nb.notebook_key.trim().is_empty() {
                nb.scope.make_entity_key(&nb.id)
            } else {
                nb.notebook_key.clone()
            };
            stmt.execute(params![
                notebook_key,
                nb.id,
                scope_json,
                scope_key,
                nb.display_name,
                nb.color,
                bool_to_i32(nb.is_default),
                bool_to_i32(nb.is_shared),
                nb.user_role,
                nb.created_at,
                nb.last_modified_at,
                nb.created_by,
                nb.web_url,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_notebooks(conn: &Connection) -> Result<Vec<OnenoteNotebook>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            notebook_key, id, scope_json, scope_key, display_name, color,
            is_default, is_shared, user_role, created_at, last_modified_at,
            created_by, web_url
        FROM onenote_notebooks
        ORDER BY
            CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
            is_default DESC,
            display_name COLLATE NOCASE ASC,
            notebook_key ASC
        ",
    )?;
    let rows = stmt.query_map([], map_notebook_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn upsert_onenote_section_groups(
    conn: &mut Connection,
    items: &[OnenoteSectionGroup],
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "
            INSERT INTO onenote_section_groups (
                section_group_key, id, scope_json, scope_key, notebook_id,
                parent_section_group_id, display_name, created_at, last_modified_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(section_group_key) DO UPDATE SET
                id = excluded.id,
                scope_json = excluded.scope_json,
                scope_key = excluded.scope_key,
                notebook_id = COALESCE(excluded.notebook_id, onenote_section_groups.notebook_id),
                parent_section_group_id = COALESCE(
                    excluded.parent_section_group_id,
                    onenote_section_groups.parent_section_group_id
                ),
                display_name = excluded.display_name,
                created_at = COALESCE(excluded.created_at, onenote_section_groups.created_at),
                last_modified_at = COALESCE(
                    excluded.last_modified_at,
                    onenote_section_groups.last_modified_at
                )
            ",
        )?;

        for sg in items {
            let scope_key = sg.scope.scope_key();
            let scope_json = serde_json::to_string(&sg.scope)?;
            let section_group_key = if sg.section_group_key.trim().is_empty() {
                sg.scope.make_entity_key(&sg.id)
            } else {
                sg.section_group_key.clone()
            };
            stmt.execute(params![
                section_group_key,
                sg.id,
                scope_json,
                scope_key,
                sg.notebook_id,
                sg.parent_section_group_id,
                sg.display_name,
                sg.created_at,
                sg.last_modified_at,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_section_groups(conn: &Connection) -> Result<Vec<OnenoteSectionGroup>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            section_group_key, id, scope_json, scope_key, notebook_id,
            parent_section_group_id, display_name, created_at, last_modified_at
        FROM onenote_section_groups
        ORDER BY
            CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
            display_name COLLATE NOCASE ASC,
            section_group_key ASC
        ",
    )?;
    let rows = stmt.query_map([], map_section_group_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn upsert_onenote_sections(conn: &mut Connection, items: &[OnenoteSection]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "
            INSERT INTO onenote_sections (
                section_key, id, scope_json, scope_key, notebook_id, notebook_name,
                parent_section_group_id, parent_section_group_name, display_name, color,
                is_default, pages_url, created_at, last_modified_at, web_url
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                COALESCE(
                    ?6,
                    (SELECT display_name FROM onenote_notebooks WHERE notebook_key = ?4 || '|' || COALESCE(?5, ''))
                ),
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15
            )
            ON CONFLICT(section_key) DO UPDATE SET
                id = excluded.id,
                scope_json = excluded.scope_json,
                scope_key = excluded.scope_key,
                notebook_id = COALESCE(excluded.notebook_id, onenote_sections.notebook_id),
                notebook_name = COALESCE(excluded.notebook_name, onenote_sections.notebook_name),
                parent_section_group_id = COALESCE(
                    excluded.parent_section_group_id,
                    onenote_sections.parent_section_group_id
                ),
                parent_section_group_name = COALESCE(
                    excluded.parent_section_group_name,
                    onenote_sections.parent_section_group_name
                ),
                display_name = excluded.display_name,
                color = COALESCE(excluded.color, onenote_sections.color),
                is_default = excluded.is_default,
                pages_url = COALESCE(excluded.pages_url, onenote_sections.pages_url),
                created_at = COALESCE(excluded.created_at, onenote_sections.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, onenote_sections.last_modified_at),
                web_url = COALESCE(excluded.web_url, onenote_sections.web_url)
            ",
        )?;

        for sec in items {
            let scope_key = sec.scope.scope_key();
            let scope_json = serde_json::to_string(&sec.scope)?;
            let section_key = if sec.section_key.trim().is_empty() {
                sec.scope.make_entity_key(&sec.id)
            } else {
                sec.section_key.clone()
            };
            stmt.execute(params![
                section_key,
                sec.id,
                scope_json,
                scope_key,
                sec.notebook_id,
                sec.notebook_name,
                sec.parent_section_group_id,
                sec.parent_section_group_name,
                sec.display_name,
                sec.color,
                bool_to_i32(sec.is_default),
                sec.pages_url,
                sec.created_at,
                sec.last_modified_at,
                sec.web_url,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_sections(
    conn: &Connection,
    notebook_key_filter: Option<&str>,
) -> Result<Vec<OnenoteSection>> {
    let filter = notebook_key_filter
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(nb_filter) = filter {
        let mut stmt = conn.prepare(
            "
            SELECT
                section_key, id, scope_json, scope_key, notebook_id, notebook_name,
                parent_section_group_id, parent_section_group_name, display_name, color,
                is_default, pages_url, created_at, last_modified_at, web_url
            FROM onenote_sections
            WHERE (scope_key || '|' || COALESCE(notebook_id, '')) = ?1
               OR notebook_id = ?1
               OR LOWER(COALESCE(notebook_name, '')) = LOWER(?1)
            ORDER BY
                CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
                is_default DESC,
                display_name COLLATE NOCASE ASC,
                section_key ASC
            ",
        )?;
        let rows = stmt.query_map(params![nb_filter], map_section_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare(
            "
            SELECT
                section_key, id, scope_json, scope_key, notebook_id, notebook_name,
                parent_section_group_id, parent_section_group_name, display_name, color,
                is_default, pages_url, created_at, last_modified_at, web_url
            FROM onenote_sections
            ORDER BY
                CASE WHEN scope_key = 'me' THEN 0 ELSE 1 END ASC,
                is_default DESC,
                display_name COLLATE NOCASE ASC,
                section_key ASC
            ",
        )?;
        let rows = stmt.query_map([], map_section_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn upsert_onenote_pages(conn: &mut Connection, items: &[OnenotePageSummary]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut page_stmt = tx.prepare(
            "
            INSERT INTO onenote_pages (
                page_key, id, scope_json, scope_key, section_key, section_id,
                section_name, notebook_id, notebook_name, title, level, page_order,
                created_at, last_modified_at, content_url, web_url,
                preview_text, has_todo, detail_json, body_plain
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                COALESCE(?7, (SELECT display_name FROM onenote_sections WHERE section_key = ?5)),
                COALESCE(?8, (SELECT notebook_id FROM onenote_sections WHERE section_key = ?5)),
                COALESCE(?9, (SELECT notebook_name FROM onenote_sections WHERE section_key = ?5)),
                ?10, ?11, ?12,
                ?13, ?14, ?15, ?16,
                ?17, ?18, NULL, NULL
            )
            ON CONFLICT(page_key) DO UPDATE SET
                id = excluded.id,
                scope_json = excluded.scope_json,
                scope_key = excluded.scope_key,
                section_key = excluded.section_key,
                section_id = excluded.section_id,
                section_name = COALESCE(excluded.section_name, onenote_pages.section_name),
                notebook_id = COALESCE(excluded.notebook_id, onenote_pages.notebook_id),
                notebook_name = COALESCE(excluded.notebook_name, onenote_pages.notebook_name),
                title = excluded.title,
                level = excluded.level,
                page_order = excluded.page_order,
                created_at = COALESCE(excluded.created_at, onenote_pages.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, onenote_pages.last_modified_at),
                content_url = COALESCE(excluded.content_url, onenote_pages.content_url),
                web_url = COALESCE(excluded.web_url, onenote_pages.web_url),
                preview_text = COALESCE(excluded.preview_text, onenote_pages.preview_text),
                has_todo = CASE
                    WHEN excluded.has_todo != 0 THEN 1
                    WHEN onenote_pages.detail_json IS NOT NULL THEN onenote_pages.has_todo
                    ELSE 0
                END,
                detail_json = COALESCE(excluded.detail_json, onenote_pages.detail_json),
                body_plain = COALESCE(excluded.body_plain, onenote_pages.body_plain)
            ",
        )?;

        let mut fts_del_stmt = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
        let mut fts_ins_stmt = tx.prepare(
            "
            INSERT INTO onenote_fts (
                page_key, section_key, title, notebook_name, section_name, body_plain
            )
            SELECT
                page_key,
                section_key,
                title,
                COALESCE(notebook_name, ''),
                COALESCE(section_name, ''),
                COALESCE(body_plain, preview_text, '')
            FROM onenote_pages
            WHERE page_key = ?1
            ",
        )?;

        for item in items {
            let scope_key = item.scope.scope_key();
            let scope_json = serde_json::to_string(&item.scope)?;
            let page_key = if item.page_key.trim().is_empty() {
                item.scope.make_entity_key(&item.id)
            } else {
                item.page_key.clone()
            };
            let section_key = item.scope.make_entity_key(&item.section_id);
            let preview_clean = item
                .preview_text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());

            page_stmt.execute(params![
                page_key,
                item.id,
                scope_json,
                scope_key,
                section_key,
                item.section_id,
                item.section_name,
                item.notebook_id,
                item.notebook_name,
                item.title,
                item.level,
                item.order,
                item.created_at,
                item.last_modified_at,
                item.content_url,
                item.web_url,
                preview_clean,
                bool_to_i32(item.has_todo),
            ])?;

            fts_del_stmt.execute(params![page_key])?;
            fts_ins_stmt.execute(params![page_key])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_onenote_pages(conn: &Connection, section_key: &str) -> Result<Vec<OnenotePageSummary>> {
    let trimmed = section_key.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.contains('|') {
        let sql = format!(
            "
            SELECT {PAGE_SUMMARY_COLS}
            FROM onenote_pages
            WHERE section_key = ?1
            ORDER BY page_order ASC, COALESCE(last_modified_at, '') DESC, id ASC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![trimmed], map_page_summary_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let me_key = OnenoteScope::Me.make_entity_key(trimmed);
        let sql = format!(
            "
            SELECT {PAGE_SUMMARY_COLS}
            FROM onenote_pages
            WHERE section_key = ?1 OR section_id = ?2
            ORDER BY page_order ASC, COALESCE(last_modified_at, '') DESC, id ASC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![me_key, trimmed], map_page_summary_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn upsert_onenote_page_detail(conn: &mut Connection, detail: &OnenotePageDetail) -> Result<()> {
    let mut normalized = detail.clone();
    if normalized.summary.page_key.trim().is_empty() {
        normalized.summary.page_key = normalized
            .summary
            .scope
            .make_entity_key(&normalized.summary.id);
    }

    let page_key = normalized.summary.page_key.clone();
    let scope_key = normalized.summary.scope.scope_key();
    let scope_json = serde_json::to_string(&normalized.summary.scope)?;
    let section_key = normalized
        .summary
        .scope
        .make_entity_key(&normalized.summary.section_id);
    let preview_text = normalized
        .summary
        .preview_text
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| derive_plain_preview(&normalized.body_plain, 140));
    let has_todo = normalized.summary.has_todo
        || normalized
            .blocks
            .iter()
            .any(|b| b.tag.as_ref().map(|t| t.is_todo()).unwrap_or(false));
    let detail_json = serde_json::to_string(&normalized)?;

    let tx = conn.transaction()?;
    {
        tx.execute(
            "
            INSERT INTO onenote_pages (
                page_key, id, scope_json, scope_key, section_key, section_id,
                section_name, notebook_id, notebook_name, title, level, page_order,
                created_at, last_modified_at, content_url, web_url,
                preview_text, has_todo, detail_json, body_plain
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                COALESCE(?7, (SELECT display_name FROM onenote_sections WHERE section_key = ?5)),
                COALESCE(?8, (SELECT notebook_id FROM onenote_sections WHERE section_key = ?5)),
                COALESCE(?9, (SELECT notebook_name FROM onenote_sections WHERE section_key = ?5)),
                ?10, ?11, ?12,
                ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20
            )
            ON CONFLICT(page_key) DO UPDATE SET
                id = excluded.id,
                scope_json = excluded.scope_json,
                scope_key = excluded.scope_key,
                section_key = excluded.section_key,
                section_id = excluded.section_id,
                section_name = COALESCE(excluded.section_name, onenote_pages.section_name),
                notebook_id = COALESCE(excluded.notebook_id, onenote_pages.notebook_id),
                notebook_name = COALESCE(excluded.notebook_name, onenote_pages.notebook_name),
                title = excluded.title,
                level = excluded.level,
                page_order = excluded.page_order,
                created_at = COALESCE(excluded.created_at, onenote_pages.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, onenote_pages.last_modified_at),
                content_url = COALESCE(excluded.content_url, onenote_pages.content_url),
                web_url = COALESCE(excluded.web_url, onenote_pages.web_url),
                preview_text = COALESCE(excluded.preview_text, onenote_pages.preview_text),
                has_todo = excluded.has_todo,
                detail_json = excluded.detail_json,
                body_plain = excluded.body_plain
            ",
            params![
                page_key,
                normalized.summary.id,
                scope_json,
                scope_key,
                section_key,
                normalized.summary.section_id,
                normalized.summary.section_name,
                normalized.summary.notebook_id,
                normalized.summary.notebook_name,
                normalized.summary.title,
                normalized.summary.level,
                normalized.summary.order,
                normalized.summary.created_at,
                normalized.summary.last_modified_at,
                normalized.summary.content_url,
                normalized.summary.web_url,
                preview_text,
                bool_to_i32(has_todo),
                detail_json,
                normalized.body_plain,
            ],
        )?;

        tx.execute("DELETE FROM onenote_fts WHERE page_key = ?1", params![page_key])?;
        tx.execute(
            "
            INSERT INTO onenote_fts (
                page_key, section_key, title, notebook_name, section_name, body_plain
            )
            SELECT
                page_key,
                section_key,
                title,
                COALESCE(notebook_name, ''),
                COALESCE(section_name, ''),
                COALESCE(body_plain, preview_text, '')
            FROM onenote_pages
            WHERE page_key = ?1
            ",
            params![page_key],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn get_onenote_page_detail(
    conn: &Connection,
    page_key_or_id: &str,
) -> Result<Option<OnenotePageDetail>> {
    let key = page_key_or_id.trim();
    if key.is_empty() {
        return Ok(None);
    }

    let row_opt: Option<(Option<String>, String, i32, i32, Option<String>)> = conn
        .query_row(
            "
            SELECT detail_json, title, level, page_order, last_modified_at
            FROM onenote_pages
            WHERE page_key = ?1
            ",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

    let row_data = match row_opt {
        Some(data) => Some(data),
        None => conn
            .query_row(
                "
                SELECT detail_json, title, level, page_order, last_modified_at
                FROM onenote_pages
                WHERE id = ?1
                ORDER BY
                    CASE WHEN detail_json IS NOT NULL THEN 0 ELSE 1 END ASC,
                    COALESCE(last_modified_at, '') DESC
                LIMIT 1
                ",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?,
    };

    let Some((Some(detail_json), row_title, row_level, row_order, row_modified)) = row_data else {
        return Ok(None);
    };

    let mut detail: OnenotePageDetail = serde_json::from_str(&detail_json)?;
    detail.summary.title = row_title;
    detail.summary.level = row_level;
    detail.summary.order = row_order;
    if row_modified.is_some() {
        detail.summary.last_modified_at = row_modified;
    }
    Ok(Some(detail))
}

pub fn get_onenote_page_summary(
    conn: &Connection,
    page_key_or_id: &str,
) -> Result<Option<OnenotePageSummary>> {
    let key = page_key_or_id.trim();
    if key.is_empty() {
        return Ok(None);
    }

    let exact_sql = format!(
        "
        SELECT {PAGE_SUMMARY_COLS}
        FROM onenote_pages
        WHERE page_key = ?1
        "
    );
    if let Some(found) = conn
        .query_row(&exact_sql, params![key], map_page_summary_row)
        .optional()?
    {
        return Ok(Some(found));
    }

    let fallback_sql = format!(
        "
        SELECT {PAGE_SUMMARY_COLS}
        FROM onenote_pages
        WHERE id = ?1
        ORDER BY COALESCE(last_modified_at, '') DESC, page_key ASC
        LIMIT 1
        "
    );
    conn.query_row(&fallback_sql, params![key], map_page_summary_row)
        .optional()
        .map_err(Into::into)
}

pub fn delete_onenote_page(conn: &mut Connection, page_key_or_id: &str) -> Result<()> {
    let key = page_key_or_id.trim();
    if key.is_empty() {
        return Ok(());
    }

    let matched_keys: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT page_key FROM onenote_pages WHERE page_key = ?1 OR id = ?1",
        )?;
        let rows = stmt.query_map(params![key], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let tx = conn.transaction()?;
    {
        tx.execute(
            "DELETE FROM onenote_pages WHERE page_key = ?1 OR id = ?1",
            params![key],
        )?;
        let mut del_fts = tx.prepare("DELETE FROM onenote_fts WHERE page_key = ?1")?;
        del_fts.execute(params![key])?;
        for pk in matched_keys {
            del_fts.execute(params![pk])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn search_onenote_pages(
    conn: &Connection,
    query: &str,
    section_key_filter: Option<&str>,
    limit: usize,
    trigram_enabled: bool,
) -> Result<Vec<OnenoteSearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let section_filter = section_key_filter
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if (trigram_enabled && trimmed.chars().count() >= 3)
        || (!trigram_enabled && trimmed.chars().count() >= 2)
    {
        if let Some(fts_query) = build_fts_query(trimmed) {
            if let Ok(hits) =
                run_onenote_fts_search(conn, &fts_query, trimmed, section_filter, limit)
            {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
    }

    run_onenote_like_search(conn, trimmed, section_filter, limit)
}

fn run_onenote_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    section_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<OnenoteSearchHit>> {
    let cols = "
        p.page_key, p.id, p.scope_json, p.scope_key, p.notebook_name,
        p.section_id, p.section_name, p.title, p.last_modified_at,
        COALESCE(p.body_plain, ''), COALESCE(p.preview_text, ''), p.web_url
    ";
    if let Some(sec) = section_filter {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_fts f
            JOIN onenote_pages p ON p.page_key = f.page_key
            WHERE onenote_fts MATCH ?1
              AND (p.section_key = ?2 OR p.section_id = ?2)
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![fts_query, sec, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_fts f
            JOIN onenote_pages p ON p.page_key = f.page_key
            WHERE onenote_fts MATCH ?1
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn run_onenote_like_search(
    conn: &Connection,
    raw_query: &str,
    section_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<OnenoteSearchHit>> {
    let cols = "
        p.page_key, p.id, p.scope_json, p.scope_key, p.notebook_name,
        p.section_id, p.section_name, p.title, p.last_modified_at,
        COALESCE(p.body_plain, ''), COALESCE(p.preview_text, ''), p.web_url
    ";
    let pattern = format!("%{}%", escape_like(&raw_query.to_lowercase()));
    if let Some(sec) = section_filter {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_pages p
            WHERE (p.section_key = ?1 OR p.section_id = ?1)
              AND (
                  LOWER(p.title) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.body_plain, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.preview_text, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.notebook_name, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(p.section_name, '')) LIKE ?2 ESCAPE '\\'
              )
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![sec, pattern, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {cols}
            FROM onenote_pages p
            WHERE (
                LOWER(p.title) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.body_plain, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.preview_text, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.notebook_name, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(p.section_name, '')) LIKE ?1 ESCAPE '\\'
            )
            ORDER BY COALESCE(p.last_modified_at, '') DESC, p.page_order ASC, p.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            map_onenote_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn build_fts_query(trimmed: &str) -> Option<String> {
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() > 1 && tokens.iter().all(|t| t.chars().count() >= 3) {
        let quoted: Vec<String> = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" "))
    } else {
        Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
    }
}

fn parse_scope_from_row(scope_json: &str, scope_key: &str) -> OnenoteScope {
    serde_json::from_str::<OnenoteScope>(scope_json)
        .ok()
        .or_else(|| OnenoteScope::from_scope_key(scope_key, None))
        .unwrap_or(OnenoteScope::Me)
}

fn map_notebook_row(row: &Row<'_>) -> rusqlite::Result<OnenoteNotebook> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let is_default_int: i32 = row.get(6)?;
    let is_shared_int: i32 = row.get(7)?;
    Ok(OnenoteNotebook {
        notebook_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        display_name: row.get(4)?,
        color: row.get(5)?,
        is_default: is_default_int != 0,
        is_shared: is_shared_int != 0,
        user_role: row.get(8)?,
        created_at: row.get(9)?,
        last_modified_at: row.get(10)?,
        created_by: row.get(11)?,
        web_url: row.get(12)?,
    })
}

fn map_section_group_row(row: &Row<'_>) -> rusqlite::Result<OnenoteSectionGroup> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    Ok(OnenoteSectionGroup {
        section_group_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        notebook_id: row.get(4)?,
        parent_section_group_id: row.get(5)?,
        display_name: row.get(6)?,
        created_at: row.get(7)?,
        last_modified_at: row.get(8)?,
    })
}

fn map_section_row(row: &Row<'_>) -> rusqlite::Result<OnenoteSection> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let is_default_int: i32 = row.get(10)?;
    Ok(OnenoteSection {
        section_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        notebook_id: row.get(4)?,
        notebook_name: row.get(5)?,
        parent_section_group_id: row.get(6)?,
        parent_section_group_name: row.get(7)?,
        display_name: row.get(8)?,
        color: row.get(9)?,
        is_default: is_default_int != 0,
        pages_url: row.get(11)?,
        created_at: row.get(12)?,
        last_modified_at: row.get(13)?,
        web_url: row.get(14)?,
    })
}

fn map_page_summary_row(row: &Row<'_>) -> rusqlite::Result<OnenotePageSummary> {
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let has_todo_int: i32 = row.get(16)?;
    Ok(OnenotePageSummary {
        page_key: row.get(0)?,
        id: row.get(1)?,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        section_id: row.get(4)?,
        section_name: row.get(5)?,
        notebook_id: row.get(6)?,
        notebook_name: row.get(7)?,
        title: row.get(8)?,
        level: row.get(9)?,
        order: row.get(10)?,
        created_at: row.get(11)?,
        last_modified_at: row.get(12)?,
        content_url: row.get(13)?,
        web_url: row.get(14)?,
        preview_text: row.get(15)?,
        has_todo: has_todo_int != 0,
    })
}

fn map_onenote_search_hit_row(
    row: &Row<'_>,
    raw_query: &str,
) -> rusqlite::Result<OnenoteSearchHit> {
    let page_key: String = row.get(0)?;
    let page_id: String = row.get(1)?;
    let scope_json: String = row.get(2)?;
    let scope_key: String = row.get(3)?;
    let notebook_name: Option<String> = row.get(4)?;
    let section_id: String = row.get(5)?;
    let section_name: Option<String> = row.get(6)?;
    let title: String = row.get(7)?;
    let last_modified_at: Option<String> = row.get(8)?;
    let body_plain: String = row.get(9)?;
    let preview_text: String = row.get(10)?;
    let web_url: Option<String> = row.get(11)?;

    let body_source = if body_plain.trim().is_empty() {
        preview_text.as_str()
    } else {
        body_plain.as_str()
    };
    let snippet = build_onenote_snippet(
        &title,
        body_source,
        section_name.as_deref(),
        notebook_name.as_deref(),
        raw_query,
        120,
    );

    Ok(OnenoteSearchHit {
        page_key,
        page_id,
        scope: parse_scope_from_row(&scope_json, &scope_key),
        notebook_name,
        section_id,
        section_name,
        title,
        last_modified_at,
        snippet,
        web_url,
    })
}

fn build_onenote_snippet(
    title: &str,
    body_plain: &str,
    section_name: Option<&str>,
    notebook_name: Option<&str>,
    query: &str,
    max_chars: usize,
) -> String {
    let clean_body = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean_title = title.trim();
    let source = if !clean_body.is_empty() {
        clean_body
    } else if !clean_title.is_empty() {
        clean_title.to_string()
    } else {
        section_name
            .or(notebook_name)
            .unwrap_or("(내용 없음)")
            .to_string()
    };

    let chars: Vec<char> = source.chars().collect();
    if chars.len() <= max_chars {
        return source;
    }

    let query_lower = query.trim().to_lowercase();
    let match_char_idx = find_case_insensitive_char_pos(&source, &query_lower)
        .or_else(|| {
            query_lower
                .split_whitespace()
                .filter_map(|tok| find_case_insensitive_char_pos(&source, tok))
                .min()
        })
        .unwrap_or(0);

    let half = max_chars / 2;
    let mut start = match_char_idx.saturating_sub(half);
    let end = (start + max_chars).min(chars.len());
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let slice: String = chars[start..end].iter().collect();
    out.push_str(slice.trim());
    if end < chars.len() {
        out.push('…');
    }
    out
}

fn find_case_insensitive_char_pos(haystack: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return None;
    }
    let haystack_lower = haystack.to_lowercase();
    let byte_idx = haystack_lower.find(needle_lower)?;
    Some(haystack_lower[..byte_idx].chars().count())
}

fn derive_plain_preview(body_plain: &str, max_chars: usize) -> Option<String> {
    let single_line = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() {
        return None;
    }
    if single_line.chars().count() <= max_chars {
        Some(single_line)
    } else {
        let truncated: String = single_line.chars().take(max_chars.saturating_sub(1)).collect();
        Some(format!("{truncated}…"))
    }
}

fn bool_to_i32(v: bool) -> i32 {
    if v {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{OnenoteBlock, OnenoteBlockKind, OnenoteResource, OnenoteTagKind, RichSpan},
        storage::Storage,
    };

    fn sample_page_summary(
        scope: OnenoteScope,
        id: &str,
        section_id: &str,
        title: &str,
        order: i32,
        modified_at: &str,
    ) -> OnenotePageSummary {
        OnenotePageSummary {
            page_key: scope.make_entity_key(id),
            id: id.to_string(),
            scope,
            section_id: section_id.to_string(),
            section_name: None,
            notebook_id: None,
            notebook_name: None,
            title: title.to_string(),
            level: 0,
            order,
            created_at: Some("2026-11-15T08:00:00Z".to_string()),
            last_modified_at: Some(modified_at.to_string()),
            content_url: None,
            web_url: Some(format!("https://onenote.example.com/{id}")),
            preview_text: None,
            has_todo: false,
        }
    }

    #[test]
    fn test_onenote_hierarchy_and_detail_preservation_across_summary_refresh() {
        let storage = Storage::open_in_memory().unwrap();
        let team_scope = OnenoteScope::Group {
            group_id: "g-eng".to_string(),
            group_name: Some("엔지니어링팀".to_string()),
        };

        let notebooks = vec![
            OnenoteNotebook {
                notebook_key: "me|nb-1".to_string(),
                id: "nb-1".to_string(),
                scope: OnenoteScope::Me,
                display_name: "개인 업무 노트".to_string(),
                color: Some("#7719AA".to_string()),
                is_default: true,
                is_shared: false,
                user_role: Some("Owner".to_string()),
                created_at: Some("2026-11-01T00:00:00Z".to_string()),
                last_modified_at: Some("2026-11-15T09:00:00Z".to_string()),
                created_by: Some("홍길동".to_string()),
                web_url: Some("https://onenote.example.com/nb-1".to_string()),
            },
            OnenoteNotebook {
                notebook_key: team_scope.make_entity_key("nb-team"),
                id: "nb-team".to_string(),
                scope: team_scope.clone(),
                display_name: "플랫폼 런북".to_string(),
                color: None,
                is_default: false,
                is_shared: true,
                user_role: Some("Contributor".to_string()),
                created_at: None,
                last_modified_at: None,
                created_by: None,
                web_url: None,
            },
        ];
        storage.upsert_onenote_notebooks(&notebooks).unwrap();
        let listed_nbs = storage.list_onenote_notebooks().unwrap();
        assert_eq!(listed_nbs.len(), 2);
        assert_eq!(listed_nbs[0].notebook_key, "me|nb-1");
        assert_eq!(listed_nbs[1].scope.owner_label(), "엔지니어링팀");

        let sgs = vec![OnenoteSectionGroup {
            section_group_key: "me|sg-1".to_string(),
            id: "sg-1".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            parent_section_group_id: None,
            display_name: "4분기 프로젝트".to_string(),
            created_at: None,
            last_modified_at: None,
        }];
        storage.upsert_onenote_section_groups(&sgs).unwrap();
        assert_eq!(storage.list_onenote_section_groups().unwrap().len(), 1);

        let sections = vec![
            OnenoteSection {
                section_key: "me|sec-1".to_string(),
                id: "sec-1".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb-1".to_string()),
                notebook_name: None, // auto-enriched from onenote_notebooks
                parent_section_group_id: Some("sg-1".to_string()),
                parent_section_group_name: Some("4분기 프로젝트".to_string()),
                display_name: "스프린트 회의록".to_string(),
                color: None,
                is_default: true,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
            OnenoteSection {
                section_key: team_scope.make_entity_key("sec-ops"),
                id: "sec-ops".to_string(),
                scope: team_scope.clone(),
                notebook_id: Some("nb-team".to_string()),
                notebook_name: Some("플랫폼 런북".to_string()),
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "장애 대응".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            },
        ];
        storage.upsert_onenote_sections(&sections).unwrap();

        let all_secs = storage.list_onenote_sections(None).unwrap();
        assert_eq!(all_secs.len(), 2);
        assert_eq!(all_secs[0].notebook_name.as_deref(), Some("개인 업무 노트"));

        let filtered_secs = storage.list_onenote_sections(Some("me|nb-1")).unwrap();
        assert_eq!(filtered_secs.len(), 1);
        assert_eq!(filtered_secs[0].section_key, "me|sec-1");

        // Initial page summaries ordered by (order ASC, last_modified_at DESC)
        let p_second = sample_page_summary(
            OnenoteScope::Me,
            "p-2",
            "sec-1",
            "후속 작업",
            1,
            "2026-11-15T12:00:00Z",
        );
        let p_first_older = sample_page_summary(
            OnenoteScope::Me,
            "p-1a",
            "sec-1",
            "오전 스탠드업",
            0,
            "2026-11-15T09:00:00Z",
        );
        let p_first_newer = sample_page_summary(
            OnenoteScope::Me,
            "p-1b",
            "sec-1",
            "오후 아키텍처 리뷰",
            0,
            "2026-11-15T11:00:00Z",
        );
        storage
            .upsert_onenote_pages(&[p_second, p_first_older, p_first_newer.clone()])
            .unwrap();

        let listed_pages = storage.list_onenote_pages("me|sec-1").unwrap();
        assert_eq!(listed_pages.len(), 3);
        assert_eq!(listed_pages[0].id, "p-1b"); // order 0, newer modified_at
        assert_eq!(listed_pages[1].id, "p-1a"); // order 0, older modified_at
        assert_eq!(listed_pages[2].id, "p-2"); // order 1
        assert_eq!(
            listed_pages[0].section_name.as_deref(),
            Some("스프린트 회의록")
        );
        assert_eq!(
            listed_pages[0].notebook_name.as_deref(),
            Some("개인 업무 노트")
        );

        // Cache full XHTML detail for p-1b
        let mut detail_summary = listed_pages[0].clone();
        detail_summary.preview_text = Some("Kubernetes 클러스터 업그레이드 및 체크리스트".to_string());
        detail_summary.has_todo = true;
        let detail = OnenotePageDetail {
            summary: detail_summary,
            raw_xhtml: "<html><body><p id=\"p:1\" data-tag=\"to-do\">Kubernetes 클러스터 업그레이드</p></body></html>".to_string(),
            body_markdown: "- [ ] Kubernetes 클러스터 업그레이드".to_string(),
            body_plain: "Kubernetes 클러스터 업그레이드 및 체크리스트 검토".to_string(),
            blocks: vec![OnenoteBlock {
                element_id: Some("p:1".to_string()),
                tag: Some(OnenoteTagKind::Todo { completed: false }),
                raw_html: Some("<p id=\"p:1\" data-tag=\"to-do\">Kubernetes 클러스터 업그레이드</p>".to_string()),
                kind: OnenoteBlockKind::Paragraph {
                    spans: vec![RichSpan::plain("Kubernetes 클러스터 업그레이드")],
                },
            }],
            resources: vec![OnenoteResource {
                element_id: Some("img:1".to_string()),
                resource_url: "https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content"
                    .to_string(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("diagram.png".to_string()),
                is_image: true,
                alt_text: Some("다이어그램".to_string()),
            }],
            primary_div_id: Some("div:{1111}{32}".to_string()),
            synced_at: Some("2026-11-15T11:05:00Z".to_string()),
        };
        storage.upsert_onenote_page_detail(&detail).unwrap();

        // Lookup by both composite page_key and raw page_id
        assert_eq!(
            storage
                .get_onenote_page_detail("me|p-1b")
                .unwrap()
                .unwrap(),
            detail
        );
        assert_eq!(
            storage.get_onenote_page_detail("p-1b").unwrap().unwrap(),
            detail
        );

        // Refreshing page summary (with preview_text=None, has_todo=false, new title) MUST NOT wipe detail_json or body_plain!
        let mut refreshed_summary = p_first_newer;
        refreshed_summary.title = "오후 아키텍처 리뷰 (수정됨)".to_string();
        refreshed_summary.preview_text = None;
        refreshed_summary.has_todo = false;
        storage.upsert_onenote_pages(&[refreshed_summary]).unwrap();

        let preserved_summary = storage.get_onenote_page_summary("p-1b").unwrap().unwrap();
        assert_eq!(preserved_summary.title, "오후 아키텍처 리뷰 (수정됨)");
        assert_eq!(
            preserved_summary.preview_text.as_deref(),
            Some("Kubernetes 클러스터 업그레이드 및 체크리스트")
        );
        assert!(preserved_summary.has_todo);

        let preserved_detail = storage
            .get_onenote_page_detail("me|p-1b")
            .unwrap()
            .unwrap();
        assert_eq!(preserved_detail.summary.title, "오후 아키텍처 리뷰 (수정됨)");
        assert_eq!(preserved_detail.blocks.len(), 1);
        assert_eq!(preserved_detail.resources.len(), 1);
        assert_eq!(
            preserved_detail.primary_div_id.as_deref(),
            Some("div:{1111}{32}")
        );
    }

    #[test]
    fn test_onenote_fts5_korean_english_search_and_delete() {
        let storage = Storage::open_in_memory().unwrap();
        let team_scope = OnenoteScope::Group {
            group_id: "g-1".to_string(),
            group_name: Some("인프라팀".to_string()),
        };

        // Page 1: summary-only upsert (title indexed into onenote_fts)
        let mut p1 = sample_page_summary(
            OnenoteScope::Me,
            "pg-budget",
            "sec-fin",
            "2026년도 클라우드 예산 계획",
            0,
            "2026-11-15T09:00:00Z",
        );
        p1.notebook_name = Some("재무 노트".to_string());
        p1.section_name = Some("예산안".to_string());
        storage.upsert_onenote_pages(&[p1]).unwrap();

        // Page 2: full detail upsert in group scope
        let mut p2_sum = sample_page_summary(
            team_scope.clone(),
            "pg-runbook",
            "sec-sre",
            "장애 복구 매뉴얼",
            0,
            "2026-11-15T10:00:00Z",
        );
        p2_sum.notebook_name = Some("SRE 런북".to_string());
        p2_sum.section_name = Some("프로덕션".to_string());
        let p2_detail = OnenotePageDetail {
            summary: p2_sum,
            raw_xhtml: "<html><body><p>PostgreSQL failover 절차 및 복구 명령어 안내</p></body></html>"
                .to_string(),
            body_markdown: "PostgreSQL failover 절차 및 복구 명령어 안내".to_string(),
            body_plain: "PostgreSQL failover 절차 및 복구 명령어 안내".to_string(),
            blocks: Vec::new(),
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: Some("2026-11-15T10:01:00Z".to_string()),
        };
        storage.upsert_onenote_page_detail(&p2_detail).unwrap();

        // >= 3 chars Korean trigram title match on summary-only page
        let hits_ko_title = storage
            .search_onenote_pages("클라우드 예산", None, 10)
            .unwrap();
        assert_eq!(hits_ko_title.len(), 1);
        assert_eq!(hits_ko_title[0].page_key, "me|pg-budget");

        // >= 3 chars English trigram body_plain match
        let hits_en_body = storage
            .search_onenote_pages("postgresql failover", None, 10)
            .unwrap();
        assert_eq!(hits_en_body.len(), 1);
        assert_eq!(hits_en_body[0].page_id, "pg-runbook");
        assert!(hits_en_body[0].snippet.contains("PostgreSQL failover"));

        // < 3 chars 2-character Korean LIKE fallback + section filter
        let hits_two_char = storage
            .search_onenote_pages("예산", Some("me|sec-fin"), 10)
            .unwrap();
        assert_eq!(hits_two_char.len(), 1);
        assert_eq!(hits_two_char[0].page_id, "pg-budget");

        let hits_filtered_out = storage
            .search_onenote_pages("예산", Some("group:g-1|sec-sre"), 10)
            .unwrap();
        assert!(hits_filtered_out.is_empty());

        // Delete removes from both onenote_pages and onenote_fts
        storage.delete_onenote_page("pg-runbook").unwrap();
        assert!(storage
            .get_onenote_page_detail("group:g-1|pg-runbook")
            .unwrap()
            .is_none());
        assert!(storage
            .get_onenote_page_summary("pg-runbook")
            .unwrap()
            .is_none());
        assert!(storage
            .search_onenote_pages("PostgreSQL", None, 10)
            .unwrap()
            .is_empty());
    }
}
