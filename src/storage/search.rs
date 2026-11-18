//! Hybrid FTS5 (`trigram` / `unicode61`) + `LIKE` message search with contextual Korean-safe snippets.

use anyhow::Result;
use rusqlite::{params, Connection, Row};

use crate::{
    models::{ConversationKind, SearchHit},
    storage::repo::escape_like,
};

pub fn search_messages(
    conn: &Connection,
    query: &str,
    conv_key_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let char_count = trimmed.chars().count();
    if char_count >= 3 {
        if let Some(fts_query) = build_fts_query(trimmed) {
            if let Ok(hits) = run_fts_search(conn, &fts_query, trimmed, conv_key_filter, limit) {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
    }

    run_like_search(conn, trimmed, conv_key_filter, limit)
}

fn build_fts_query(trimmed: &str) -> Option<String> {
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    // In FTS5 trigram mode, every token in an AND query needs >= 3 chars to match.
    // If any token is < 3 chars, try exact phrase if overall >= 3 chars; if 0 hits, LIKE handles it.
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

fn run_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    conv_key_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let hits = if let Some(conv_key) = conv_key_filter {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                m.conv_key,
                COALESCE(c.title, m.conv_key) AS conv_title,
                c.kind,
                m.msg_id,
                m.parent_msg_id,
                m.author_name,
                m.created_at,
                m.body_plain,
                m.subject
            FROM messages_fts f
            JOIN messages m ON m.msg_key = f.msg_key
            LEFT JOIN conversations c ON c.conv_key = m.conv_key
            WHERE messages_fts MATCH ?1
              AND m.deleted = 0
              AND m.conv_key = ?2
            ORDER BY m.created_at DESC, m.msg_id DESC
            LIMIT ?3
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, conv_key, limit as i64], |row| {
            map_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                m.conv_key,
                COALESCE(c.title, m.conv_key) AS conv_title,
                c.kind,
                m.msg_id,
                m.parent_msg_id,
                m.author_name,
                m.created_at,
                m.body_plain,
                m.subject
            FROM messages_fts f
            JOIN messages m ON m.msg_key = f.msg_key
            LEFT JOIN conversations c ON c.conv_key = m.conv_key
            WHERE messages_fts MATCH ?1
              AND m.deleted = 0
            ORDER BY m.created_at DESC, m.msg_id DESC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(hits)
}

fn run_like_search(
    conn: &Connection,
    raw_query: &str,
    conv_key_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let pattern = format!("%{}%", escape_like(&raw_query.to_lowercase()));
    let hits = if let Some(conv_key) = conv_key_filter {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                m.conv_key,
                COALESCE(c.title, m.conv_key) AS conv_title,
                c.kind,
                m.msg_id,
                m.parent_msg_id,
                m.author_name,
                m.created_at,
                m.body_plain,
                m.subject
            FROM messages m
            LEFT JOIN conversations c ON c.conv_key = m.conv_key
            WHERE m.deleted = 0
              AND m.conv_key = ?1
              AND (
                  LOWER(m.body_plain) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(m.subject, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(m.author_name) LIKE ?2 ESCAPE '\\'
              )
            ORDER BY m.created_at DESC, m.msg_id DESC
            LIMIT ?3
            ",
        )?;
        let rows = stmt.query_map(params![conv_key, pattern, limit as i64], |row| {
            map_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                m.conv_key,
                COALESCE(c.title, m.conv_key) AS conv_title,
                c.kind,
                m.msg_id,
                m.parent_msg_id,
                m.author_name,
                m.created_at,
                m.body_plain,
                m.subject
            FROM messages m
            LEFT JOIN conversations c ON c.conv_key = m.conv_key
            WHERE m.deleted = 0
              AND (
                  LOWER(m.body_plain) LIKE ?1 ESCAPE '\\'
                  OR LOWER(COALESCE(m.subject, '')) LIKE ?1 ESCAPE '\\'
                  OR LOWER(m.author_name) LIKE ?1 ESCAPE '\\'
              )
            ORDER BY m.created_at DESC, m.msg_id DESC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |row| {
            map_search_hit_row(row, raw_query)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(hits)
}

fn map_search_hit_row(row: &Row<'_>, query: &str) -> rusqlite::Result<SearchHit> {
    let conv_key: String = row.get(0)?;
    let conv_title: String = row.get(1)?;
    let kind_opt: Option<String> = row.get(2)?;
    let kind = match kind_opt.as_deref() {
        Some(k) => ConversationKind::from_db_str(k),
        None if conv_key.starts_with("chan:") => ConversationKind::Channel,
        None => ConversationKind::Group,
    };
    let msg_id: String = row.get(3)?;
    let parent_msg_id: Option<String> = row.get(4)?;
    let author_name: String = row.get(5)?;
    let created_at: String = row.get(6)?;
    let body_plain: String = row.get(7)?;
    let subject: Option<String> = row.get(8)?;

    let snippet = build_contextual_snippet(
        subject.as_deref(),
        &body_plain,
        &author_name,
        query,
        120,
    );

    Ok(SearchHit {
        conv_key,
        conv_title,
        kind,
        msg_id,
        parent_msg_id,
        author_name,
        created_at,
        snippet,
    })
}

fn build_contextual_snippet(
    subject: Option<&str>,
    body_plain: &str,
    author_name: &str,
    query: &str,
    max_chars: usize,
) -> String {
    let clean_body = body_plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let source = match subject.map(str::trim).filter(|s| !s.is_empty()) {
        Some(subj) if !clean_body.is_empty() => format!("[{subj}] {clean_body}"),
        Some(subj) => format!("[{subj}]"),
        None if !clean_body.is_empty() => clean_body,
        None => author_name.to_string(),
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
