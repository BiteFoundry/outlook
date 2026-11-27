//! Hybrid FTS5 (`trigram` / `unicode61`) + `LIKE` mail search with contextual snippets.

use anyhow::Result;
use rusqlite::{params, Connection, Row};

use crate::{
    models::MailSearchHit,
    storage::outlook::{escape_like, FOLDER_FILTER_PREDICATE},
};

const SEARCH_SELECT_COLS: &str = "
    m.id, m.conversation_id, m.folder_id, m.subject, m.from_display,
    m.received_at, m.body_plain, m.preview, m.has_attachments, m.is_read, m.flagged
";

pub fn search_mail(
    conn: &Connection,
    query: &str,
    folder_id: Option<&str>,
    limit: usize,
    trigram_enabled: bool,
) -> Result<Vec<MailSearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let folder = folder_id.map(str::trim).filter(|s| !s.is_empty());

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let all_tokens_at_least_3 = !tokens.is_empty() && tokens.iter().all(|t| t.chars().count() >= 3);
    if trigram_enabled && trimmed.chars().count() >= 3 {
        if let Some(fts_query) = build_fts_query(trimmed, &tokens, all_tokens_at_least_3) {
            if let Ok(hits) = run_mail_fts_search(conn, &fts_query, trimmed, folder, limit) {
                if !hits.is_empty() || all_tokens_at_least_3 {
                    return Ok(hits);
                }
            }
        }
    }

    run_mail_like_search(conn, trimmed, folder, limit)
}

fn build_fts_query(trimmed: &str, tokens: &[&str], all_tokens_at_least_3: bool) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() > 1 && all_tokens_at_least_3 {
        let quoted: Vec<String> = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" "))
    } else {
        Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
    }
}

fn run_mail_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    folder_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<MailSearchHit>> {
    if let Some(folder) = folder_filter {
        let sql = format!(
            "
            SELECT {SEARCH_SELECT_COLS}
            FROM mail_fts f
            JOIN mail_messages m ON m.rowid = f.rowid
            WHERE mail_fts MATCH ?2
              AND {FOLDER_FILTER_PREDICATE}
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![folder, fts_query, limit as i64], |row| {
            map_mail_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {SEARCH_SELECT_COLS}
            FROM mail_fts f
            JOIN mail_messages m ON m.rowid = f.rowid
            WHERE mail_fts MATCH ?1
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_mail_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn run_mail_like_search(
    conn: &Connection,
    raw_query: &str,
    folder_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<MailSearchHit>> {
    let pattern = format!("%{}%", escape_like(&raw_query.to_lowercase()));
    if let Some(folder) = folder_filter {
        let sql = format!(
            "
            SELECT {SEARCH_SELECT_COLS}
            FROM mail_messages m
            WHERE {FOLDER_FILTER_PREDICATE}
              AND (
                  LOWER(m.subject) LIKE ?2 ESCAPE '\\'
                  OR LOWER(m.from_display) LIKE ?2 ESCAPE '\\'
                  OR LOWER(m.body_plain) LIKE ?2 ESCAPE '\\'
                  OR LOWER(m.preview) LIKE ?2 ESCAPE '\\'
              )
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![folder, pattern, limit as i64], |row| {
            map_mail_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {SEARCH_SELECT_COLS}
            FROM mail_messages m
            WHERE (
                LOWER(m.subject) LIKE ?1 ESCAPE '\\'
                OR LOWER(m.from_display) LIKE ?1 ESCAPE '\\'
                OR LOWER(m.body_plain) LIKE ?1 ESCAPE '\\'
                OR LOWER(m.preview) LIKE ?1 ESCAPE '\\'
            )
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            map_mail_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

fn map_mail_search_hit_row(row: &Row<'_>, raw_query: &str) -> rusqlite::Result<MailSearchHit> {
    let id: String = row.get(0)?;
    let conversation_id: Option<String> = row.get(1)?;
    let folder_id: Option<String> = row.get(2)?;
    let subject: String = row.get(3)?;
    let from_display: String = row.get(4)?;
    let received_at: String = row.get(5)?;
    let body_plain: String = row.get(6)?;
    let preview: String = row.get(7)?;
    let has_att_int: i32 = row.get(8)?;
    let is_read_int: i32 = row.get(9)?;
    let flagged_int: i32 = row.get(10)?;

    let body_source = if body_plain.trim().is_empty() {
        preview.as_str()
    } else {
        body_plain.as_str()
    };
    let snippet = build_mail_snippet(&subject, body_source, &from_display, raw_query, 120);

    Ok(MailSearchHit {
        id,
        conversation_id,
        folder_id,
        subject,
        from_display,
        received_at,
        snippet,
        has_attachments: has_att_int != 0,
        is_read: is_read_int != 0,
        flagged: flagged_int != 0,
    })
}

fn build_mail_snippet(
    subject: &str,
    body_plain: &str,
    from_display: &str,
    query: &str,
    max_chars: usize,
) -> String {
    let clean_body = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean_subj = subject.trim();
    let source = if !clean_body.is_empty() {
        clean_body
    } else if !clean_subj.is_empty() {
        clean_subj.to_string()
    } else {
        from_display.to_string()
    };

    let total_chars = source.chars().count();
    if total_chars <= max_chars {
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
    let end = (start + max_chars).min(total_chars);
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    let start_byte = source
        .char_indices()
        .nth(start)
        .map(|(b, _)| b)
        .unwrap_or(source.len());
    let end_byte = source
        .char_indices()
        .nth(end)
        .map(|(b, _)| b)
        .unwrap_or(source.len());

    let mut out = String::with_capacity(end_byte.saturating_sub(start_byte) + 8);
    if start > 0 {
        out.push('…');
    }
    out.push_str(source[start_byte..end_byte].trim());
    if end < total_chars {
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
