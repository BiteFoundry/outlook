//! SQLite persistence for Outlook Mail, Calendar, Contacts, Microsoft To Do, and sync cursors.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

use crate::models::{
    CalendarEvent, ContactItem, EmailRecipient, EventAttendee, MailAttachment, MailFolder,
    MailMessage, MeetingRequestInfo, SyncCursor, TodoList, TodoTask,
};

const MAIL_SELECT_COLS: &str = "
    m.id, m.internet_message_id, m.conversation_id, m.folder_id, m.subject,
    m.from_json, m.to_json, m.cc_json, m.bcc_json, m.reply_to_json,
    m.received_at, m.sent_at, m.last_modified_at, m.is_read, m.is_draft, m.flagged,
    m.importance, m.inference_classification, m.has_attachments, m.body_loaded,
    m.preview, m.body_html, m.body_plain, m.attachments_json,
    m.meeting_request_json, m.web_link, m.sender_json, m.categories_json
";

pub(crate) const MAIL_SUMMARY_SELECT_COLS: &str = "
    m.id, m.internet_message_id, m.conversation_id, m.folder_id, m.subject,
    m.from_json,
    CASE
        WHEN m.to_json != '[]' AND json_valid(m.to_json) THEN json_extract(m.to_json, '$[0]')
        ELSE NULL
    END,
    m.received_at, m.sent_at, m.last_modified_at, m.is_read, m.is_draft,
    m.flagged, m.importance, m.inference_classification, m.has_attachments,
    m.body_loaded, m.preview, m.meeting_request_json, m.web_link,
    m.sender_json, m.categories_json
";

const CALENDAR_SELECT_COLS: &str = "
    id, subject, start_iso, end_iso, time_zone, is_all_day,
    location, organizer_name, organizer_email, attendees_json,
    response_status, is_online_meeting, online_meeting_url,
    body_html, body_plain, is_cancelled, web_link,
    categories_json, show_as, sensitivity, event_type,
    has_attachments, is_recurring, recurrence_json
";

pub(crate) const FOLDER_FILTER_PREDICATE: &str = "
    (
        m.folder_id = ?1
        OR m.folder_id IN (
            SELECT id FROM mail_folders
            WHERE LOWER(COALESCE(well_known_name, '')) = LOWER(?1)
               OR LOWER(display_name) = LOWER(?1)
        )
    )
";

pub fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn normalize_well_known_alias(input: &str) -> &str {
    match input.trim().to_ascii_lowercase().as_str() {
        "trash" | "deleted" => "deleteditems",
        "sent" => "sentitems",
        "junk" | "spam" => "junkemail",
        "draft" => "drafts",
        _ => input.trim(),
    }
}

pub fn upsert_mail_folders(conn: &mut Connection, folders: &[MailFolder]) -> Result<()> {
    if folders.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_mail_folders_in_tx(&tx, folders)?;
    tx.commit()?;
    Ok(())
}

fn upsert_mail_folders_in_tx(tx: &Transaction<'_>, folders: &[MailFolder]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "
        INSERT INTO mail_folders (
            id, display_name, well_known_name, parent_folder_id,
            child_folder_count, unread_item_count, total_item_count
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(id) DO UPDATE SET
            display_name = excluded.display_name,
            well_known_name = COALESCE(excluded.well_known_name, mail_folders.well_known_name),
            parent_folder_id = excluded.parent_folder_id,
            child_folder_count = excluded.child_folder_count,
            unread_item_count = excluded.unread_item_count,
            total_item_count = excluded.total_item_count
        ",
    )?;
    for f in folders {
        stmt.execute(params![
            f.id,
            f.display_name,
            f.well_known_name,
            f.parent_folder_id,
            f.child_folder_count,
            f.unread_item_count,
            f.total_item_count,
        ])?;
    }
    Ok(())
}

/// Atomically replaces the cached mail folder snapshot with `folders`, removing folders
/// deleted remotely while preserving existing `well_known_name` metadata on surviving rows.
pub fn replace_mail_folders(conn: &mut Connection, folders: &[MailFolder]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        upsert_mail_folders_in_tx(&tx, folders)?;
        let keep: HashSet<&str> = folders.iter().map(|f| f.id.as_str()).collect();
        let existing_ids: Vec<String> = {
            let mut sel = tx.prepare_cached("SELECT id FROM mail_folders")?;
            let rows = sel.query_map([], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut del = tx.prepare_cached("DELETE FROM mail_folders WHERE id = ?1")?;
        for id in existing_ids {
            if !keep.contains(id.as_str()) {
                del.execute(params![id])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_mail_folders(conn: &Connection) -> Result<Vec<MailFolder>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT
            id, display_name, well_known_name, parent_folder_id,
            child_folder_count, unread_item_count, total_item_count
        FROM mail_folders
        ORDER BY
            CASE LOWER(COALESCE(well_known_name, ''))
                WHEN 'inbox' THEN 0
                WHEN 'drafts' THEN 1
                WHEN 'sentitems' THEN 2
                WHEN 'deleteditems' THEN 3
                WHEN 'archive' THEN 4
                ELSE 5
            END ASC,
            display_name COLLATE NOCASE ASC,
            id ASC
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(MailFolder {
            id: row.get(0)?,
            display_name: row.get(1)?,
            well_known_name: row.get(2)?,
            parent_folder_id: row.get(3)?,
            child_folder_count: row.get(4)?,
            unread_item_count: row.get(5)?,
            total_item_count: row.get(6)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn find_well_known_folder_id(conn: &Connection, well_known: &str) -> Result<Option<String>> {
    let trimmed = well_known.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let alias = normalize_well_known_alias(trimmed);
    let mut stmt = conn.prepare_cached(
        "
        SELECT id FROM mail_folders
        WHERE LOWER(COALESCE(well_known_name, '')) = LOWER(?2)
           OR id = ?1
           OR LOWER(display_name) = LOWER(?1)
        ORDER BY
            CASE
                WHEN LOWER(COALESCE(well_known_name, '')) = LOWER(?2) THEN 0
                WHEN id = ?1 THEN 1
                ELSE 2
            END ASC
        LIMIT 1
        ",
    )?;
    stmt.query_row(params![trimmed, alias], |row| row.get(0))
        .optional()
        .map_err(Into::into)
}

struct ExistingMailRow {
    rowid: i64,
    internet_message_id: Option<String>,
    conversation_id: Option<String>,
    folder_id: Option<String>,
    subject: String,
    from_json: Option<String>,
    to_json: String,
    cc_json: String,
    bcc_json: String,
    reply_to_json: String,
    from_display: String,
    received_at: String,
    sent_at: Option<String>,
    last_modified_at: Option<String>,
    is_read: i32,
    is_draft: i32,
    flagged: i32,
    importance: String,
    inference_classification: Option<String>,
    has_attachments: i32,
    body_loaded: i32,
    preview: String,
    body_html: String,
    body_plain: String,
    attachments_json: String,
    meeting_request_json: Option<String>,
    web_link: Option<String>,
    sender_json: Option<String>,
    categories_json: String,
}

/// Compares two RFC 3339 / ISO 8601 timestamps, falling back to lexicographical order.
fn compare_iso_timestamps(incoming: &str, existing: &str) -> std::cmp::Ordering {
    let inc = incoming.trim();
    let ex = existing.trim();
    match (
        chrono::DateTime::parse_from_rfc3339(inc),
        chrono::DateTime::parse_from_rfc3339(ex),
    ) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => inc.cmp(ex),
    }
}

/// Upserts `messages`, enforcing the header-first body freshness rule:
/// - Older summaries (`last_modified_at < existing`) never regress newer cached rows or bodies.
/// - Same-timestamp or timestamp-less metadata-only header updates preserve cached full bodies.
/// - Strictly newer `last_modified_at` (even alongside read/flag/folder changes) or content
///   changes (`subject`, `preview`, `is_draft`, `has_attachments`) mark `body_loaded = 0` so
///   the active reader re-hydrates the updated body while other messages hydrate on open.
pub fn upsert_mail_messages(conn: &mut Connection, messages: &[MailMessage]) -> Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let changed_count = upsert_mail_messages_in_tx(&tx, messages)?;
    tx.commit()?;
    Ok(changed_count)
}

fn upsert_mail_messages_in_tx(tx: &Transaction<'_>, messages: &[MailMessage]) -> Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let mut changed_count = 0usize;
    let mut check_stmt = tx.prepare_cached(
        "
        SELECT
            rowid, internet_message_id, conversation_id, folder_id, subject,
            from_json, to_json, cc_json, bcc_json, reply_to_json,
            from_display, received_at, sent_at, last_modified_at, is_read, is_draft,
            flagged, importance, inference_classification, has_attachments, body_loaded,
            preview, body_html, body_plain, attachments_json, meeting_request_json, web_link,
            sender_json, categories_json
        FROM mail_messages
        WHERE id = ?1
        ",
    )?;

    let mut msg_stmt = tx.prepare_cached(
        "
        INSERT INTO mail_messages (
            id, internet_message_id, conversation_id, folder_id, subject,
            from_json, to_json, cc_json, bcc_json, reply_to_json,
            from_display, received_at, sent_at, last_modified_at, is_read, is_draft,
            flagged, importance, inference_classification, has_attachments, body_loaded,
            preview, body_html, body_plain, attachments_json, meeting_request_json, web_link,
            sender_json, categories_json
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5,
            ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21,
            ?22, ?23, ?24, ?25, ?26, ?27,
            ?28, ?29
        )
        ON CONFLICT(id) DO UPDATE SET
            internet_message_id = excluded.internet_message_id,
            conversation_id = excluded.conversation_id,
            folder_id = excluded.folder_id,
            subject = excluded.subject,
            from_json = excluded.from_json,
            to_json = excluded.to_json,
            cc_json = excluded.cc_json,
            bcc_json = excluded.bcc_json,
            reply_to_json = excluded.reply_to_json,
            from_display = excluded.from_display,
            received_at = excluded.received_at,
            sent_at = excluded.sent_at,
            last_modified_at = excluded.last_modified_at,
            is_read = excluded.is_read,
            is_draft = excluded.is_draft,
            flagged = excluded.flagged,
            importance = excluded.importance,
            inference_classification = excluded.inference_classification,
            has_attachments = excluded.has_attachments,
            body_loaded = excluded.body_loaded,
            preview = excluded.preview,
            body_html = excluded.body_html,
            body_plain = excluded.body_plain,
            attachments_json = excluded.attachments_json,
            meeting_request_json = excluded.meeting_request_json,
            web_link = excluded.web_link,
            sender_json = excluded.sender_json,
            categories_json = excluded.categories_json
        ",
    )?;

    let mut fts_del_stmt = tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?;
    let mut fts_ins_stmt = tx.prepare_cached(
        "
        INSERT INTO mail_fts (rowid, msg_id, folder_id, subject, from_display, body_plain)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
    )?;

    for msg in messages {
        let from_json = msg
            .from
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let sender_json = msg
            .sender
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let categories_json = serde_json::to_string(&msg.categories)?;
        let to_json = serde_json::to_string(&msg.to_recipients)?;
        let cc_json = serde_json::to_string(&msg.cc_recipients)?;
        let bcc_json = serde_json::to_string(&msg.bcc_recipients)?;
        let reply_to_json = serde_json::to_string(&msg.reply_to)?;
        let attachments_json = serde_json::to_string(&msg.attachments)?;
        let meeting_request_json = msg
            .meeting_request
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let from_display = msg.from_display();
        let is_read_i = bool_to_i32(msg.is_read);
        let is_draft_i = bool_to_i32(msg.is_draft);
        let flagged_i = bool_to_i32(msg.flagged);
        let has_att_i = bool_to_i32(msg.has_attachments);
        let body_loaded_i = bool_to_i32(msg.body_loaded);

        let existing = check_stmt
            .query_row(params![&msg.id], |row| {
                Ok(ExistingMailRow {
                    rowid: row.get(0)?,
                    internet_message_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    folder_id: row.get(3)?,
                    subject: row.get(4)?,
                    from_json: row.get(5)?,
                    to_json: row.get(6)?,
                    cc_json: row.get(7)?,
                    bcc_json: row.get(8)?,
                    reply_to_json: row.get(9)?,
                    from_display: row.get(10)?,
                    received_at: row.get(11)?,
                    sent_at: row.get(12)?,
                    last_modified_at: row.get(13)?,
                    is_read: row.get(14)?,
                    is_draft: row.get(15)?,
                    flagged: row.get(16)?,
                    importance: row.get(17)?,
                    inference_classification: row.get(18)?,
                    has_attachments: row.get(19)?,
                    body_loaded: row.get(20)?,
                    preview: row.get(21)?,
                    body_html: row.get(22)?,
                    body_plain: row.get(23)?,
                    attachments_json: row.get(24)?,
                    meeting_request_json: row.get(25)?,
                    web_link: row.get(26)?,
                    sender_json: row.get(27)?,
                    categories_json: row.get(28)?,
                })
            })
            .optional()?;

        if let Some(ref ex) = existing {
            let eff_inet_id = msg
                .internet_message_id
                .as_deref()
                .or(ex.internet_message_id.as_deref());
            let eff_conversation_id = msg
                .conversation_id
                .as_deref()
                .or(ex.conversation_id.as_deref());
            let eff_folder_id = msg.folder_id.as_deref().or(ex.folder_id.as_deref());
            let eff_from_json = from_json.as_deref().or(ex.from_json.as_deref());
            let eff_sent_at = msg.sent_at.as_deref().or(ex.sent_at.as_deref());
            let inc_last_modified = msg
                .last_modified_at
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let ex_last_modified = ex
                .last_modified_at
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let last_modified_cmp = match (inc_last_modified, ex_last_modified) {
                (Some(inc), Some(old)) => Some(compare_iso_timestamps(inc, old)),
                _ => None,
            };
            // Never let a late/older delta summary or stale hydration regress a newer cached row.
            if matches!(last_modified_cmp, Some(std::cmp::Ordering::Less)) {
                continue;
            }
            let eff_last_modified = inc_last_modified.or(ex_last_modified);
            let eff_inference = msg
                .inference_classification
                .as_deref()
                .or(ex.inference_classification.as_deref());
            let eff_meeting_req = meeting_request_json
                .as_deref()
                .or(ex.meeting_request_json.as_deref());
            let eff_web_link = msg.web_link.as_deref().or(ex.web_link.as_deref());
            let eff_sender_json = sender_json.as_deref().or(ex.sender_json.as_deref());

            let eff_preview = if msg.preview.is_empty() && !ex.preview.is_empty() {
                ex.preview.as_str()
            } else {
                msg.preview.as_str()
            };

            let header_over_hydrated = !msg.body_loaded && ex.body_loaded != 0;
            let incoming_newer_modified =
                matches!(last_modified_cmp, Some(std::cmp::Ordering::Greater));
            let invalidate_body = header_over_hydrated
                && (msg.subject != ex.subject
                    || (!msg.preview.is_empty() && msg.preview != ex.preview)
                    || is_draft_i != ex.is_draft
                    || has_att_i != ex.has_attachments
                    || incoming_newer_modified);

            let preserve_hydrated = header_over_hydrated && !invalidate_body;

            let eff_categories_json =
                if !msg.body_loaded && categories_json == "[]" && ex.categories_json != "[]" && !incoming_newer_modified {
                    ex.categories_json.as_str()
                } else {
                    categories_json.as_str()
                };

            let eff_to_json = if preserve_hydrated && to_json == "[]" && ex.to_json != "[]" {
                ex.to_json.as_str()
            } else {
                to_json.as_str()
            };
            let eff_cc_json = if preserve_hydrated && cc_json == "[]" && ex.cc_json != "[]" {
                ex.cc_json.as_str()
            } else {
                cc_json.as_str()
            };
            let eff_bcc_json = if preserve_hydrated && bcc_json == "[]" && ex.bcc_json != "[]" {
                ex.bcc_json.as_str()
            } else {
                bcc_json.as_str()
            };
            let eff_reply_to_json =
                if preserve_hydrated && reply_to_json == "[]" && ex.reply_to_json != "[]" {
                    ex.reply_to_json.as_str()
                } else {
                    reply_to_json.as_str()
                };

            let keep_existing_att = !invalidate_body
                && (preserve_hydrated
                    || (msg.has_attachments
                        && attachments_json == "[]"
                        && ex.attachments_json != "[]"
                        && !ex.attachments_json.is_empty()));
            let eff_attachments_json = if invalidate_body {
                "[]"
            } else if keep_existing_att {
                ex.attachments_json.as_str()
            } else {
                attachments_json.as_str()
            };
            let eff_has_attachments = if invalidate_body {
                has_att_i
            } else if msg.has_attachments || (preserve_hydrated && ex.has_attachments != 0) {
                1i32
            } else {
                0i32
            };
            let eff_body_loaded = if invalidate_body {
                0i32
            } else {
                ex.body_loaded.max(body_loaded_i)
            };
            let eff_body_html = if invalidate_body {
                ""
            } else if preserve_hydrated
                || (!msg.body_loaded && msg.body_html.is_empty() && !ex.body_html.is_empty())
            {
                ex.body_html.as_str()
            } else {
                msg.body_html.as_str()
            };
            let eff_body_plain = if invalidate_body {
                eff_preview
            } else if preserve_hydrated
                || (!msg.body_loaded
                    && msg.body_html.is_empty()
                    && !ex.body_html.is_empty()
                    && !ex.body_plain.is_empty())
            {
                ex.body_plain.as_str()
            } else {
                msg.body_plain.as_str()
            };

            let row_unchanged = eff_inet_id == ex.internet_message_id.as_deref()
                && eff_conversation_id == ex.conversation_id.as_deref()
                && eff_folder_id == ex.folder_id.as_deref()
                && msg.subject == ex.subject
                && eff_from_json == ex.from_json.as_deref()
                && eff_to_json == ex.to_json
                && eff_cc_json == ex.cc_json
                && eff_bcc_json == ex.bcc_json
                && eff_reply_to_json == ex.reply_to_json
                && from_display == ex.from_display
                && msg.received_at == ex.received_at
                && eff_sent_at == ex.sent_at.as_deref()
                && eff_last_modified == ex.last_modified_at.as_deref()
                && is_read_i == ex.is_read
                && is_draft_i == ex.is_draft
                && flagged_i == ex.flagged
                && msg.importance == ex.importance
                && eff_inference == ex.inference_classification.as_deref()
                && eff_has_attachments == ex.has_attachments
                && eff_body_loaded == ex.body_loaded
                && eff_preview == ex.preview
                && eff_body_html == ex.body_html
                && eff_body_plain == ex.body_plain
                && eff_attachments_json == ex.attachments_json
                && eff_meeting_req == ex.meeting_request_json.as_deref()
                && eff_web_link == ex.web_link.as_deref()
                && eff_sender_json == ex.sender_json.as_deref()
                && eff_categories_json == ex.categories_json;

            if row_unchanged {
                continue;
            }

            let fts_same = eff_folder_id.unwrap_or("") == ex.folder_id.as_deref().unwrap_or("")
                && msg.subject == ex.subject
                && from_display == ex.from_display
                && eff_body_plain == ex.body_plain;
            let eff_folder_str = eff_folder_id.unwrap_or("").to_string();
            let eff_body_plain_owned = eff_body_plain.to_string();

            msg_stmt.execute(params![
                msg.id,
                eff_inet_id,
                eff_conversation_id,
                eff_folder_id,
                msg.subject,
                eff_from_json,
                eff_to_json,
                eff_cc_json,
                eff_bcc_json,
                eff_reply_to_json,
                from_display,
                msg.received_at,
                eff_sent_at,
                eff_last_modified,
                is_read_i,
                is_draft_i,
                flagged_i,
                msg.importance,
                eff_inference,
                eff_has_attachments,
                eff_body_loaded,
                eff_preview,
                eff_body_html,
                eff_body_plain,
                eff_attachments_json,
                eff_meeting_req,
                eff_web_link,
                eff_sender_json,
                eff_categories_json,
            ])?;
            changed_count += 1;

            if !fts_same {
                fts_del_stmt.execute(params![ex.rowid])?;
                fts_ins_stmt.execute(params![
                    ex.rowid,
                    msg.id,
                    eff_folder_str,
                    msg.subject,
                    from_display,
                    eff_body_plain_owned,
                ])?;
            }
        } else {
            msg_stmt.execute(params![
                msg.id,
                msg.internet_message_id,
                msg.conversation_id,
                msg.folder_id,
                msg.subject,
                from_json,
                to_json,
                cc_json,
                bcc_json,
                reply_to_json,
                from_display,
                msg.received_at,
                msg.sent_at,
                msg.last_modified_at,
                is_read_i,
                is_draft_i,
                flagged_i,
                msg.importance,
                msg.inference_classification,
                has_att_i,
                body_loaded_i,
                msg.preview,
                msg.body_html,
                msg.body_plain,
                attachments_json,
                meeting_request_json,
                msg.web_link,
                sender_json,
                categories_json,
            ])?;
            changed_count += 1;
            let rowid = tx.last_insert_rowid();
            fts_ins_stmt.execute(params![
                rowid,
                msg.id,
                msg.folder_id.as_deref().unwrap_or(""),
                msg.subject,
                from_display,
                msg.body_plain,
            ])?;
        }
    }
    Ok(changed_count)
}

#[allow(clippy::too_many_arguments)]
fn query_mail_rows(
    conn: &Connection,
    select_cols: &str,
    mapper: fn(&Row<'_>) -> rusqlite::Result<MailMessage>,
    folder_id: Option<&str>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
    before: Option<(&str, &str)>,
) -> Result<Vec<MailMessage>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let filter = folder_id.map(str::trim).filter(|s| !s.is_empty());

    let mut extra_clauses = String::new();
    if unread_only {
        extra_clauses.push_str(" AND m.is_read = 0");
    }
    if flagged_only {
        extra_clauses.push_str(" AND m.flagged = 1");
    }
    if focused_only {
        extra_clauses.push_str(
            " AND LOWER(COALESCE(m.inference_classification, 'focused')) != 'other'",
        );
    }

    if let Some(folder) = filter {
        if before.is_some() {
            extra_clauses.push_str(" AND (m.received_at < ?3 OR (m.received_at = ?3 AND m.id < ?4))");
        }
        let sql = format!(
            "
            SELECT {select_cols}
            FROM mail_messages m
            WHERE {FOLDER_FILTER_PREDICATE}
              {extra_clauses}
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = if let Some((ts, id)) = before {
            stmt.query_map(params![folder, limit as i64, ts, id], mapper)?
        } else {
            stmt.query_map(params![folder, limit as i64], mapper)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        if before.is_some() {
            extra_clauses.push_str(" AND (m.received_at < ?2 OR (m.received_at = ?2 AND m.id < ?3))");
        }
        let sql = format!(
            "
            SELECT {select_cols}
            FROM mail_messages m
            WHERE 1 = 1
              {extra_clauses}
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?1
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = if let Some((ts, id)) = before {
            stmt.query_map(params![limit as i64, ts, id], mapper)?
        } else {
            stmt.query_map(params![limit as i64], mapper)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn list_mail_messages(
    conn: &Connection,
    folder_id: Option<&str>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
) -> Result<Vec<MailMessage>> {
    list_mail_messages_before(
        conn,
        folder_id,
        unread_only,
        flagged_only,
        focused_only,
        limit,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn list_mail_messages_before(
    conn: &Connection,
    folder_id: Option<&str>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
    before: Option<(&str, &str)>,
) -> Result<Vec<MailMessage>> {
    query_mail_rows(
        conn,
        MAIL_SELECT_COLS,
        map_mail_message_row,
        folder_id,
        unread_only,
        flagged_only,
        focused_only,
        limit,
        before,
    )
}

/// Lightweight header-only mail list projection with full filter/order parity
/// (skips large body/recipient/attachment columns while retaining the first `to` recipient for Sent/Drafts).
pub fn list_mail_summaries_filtered(
    conn: &Connection,
    folder_id_or_wellknown: Option<&str>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
) -> Result<Vec<MailMessage>> {
    list_mail_summaries_before(
        conn,
        folder_id_or_wellknown,
        unread_only,
        flagged_only,
        focused_only,
        limit,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn list_mail_summaries_before(
    conn: &Connection,
    folder_id_or_wellknown: Option<&str>,
    unread_only: bool,
    flagged_only: bool,
    focused_only: bool,
    limit: usize,
    before: Option<(&str, &str)>,
) -> Result<Vec<MailMessage>> {
    query_mail_rows(
        conn,
        MAIL_SUMMARY_SELECT_COLS,
        map_mail_summary_row,
        folder_id_or_wellknown,
        unread_only,
        flagged_only,
        focused_only,
        limit,
        before,
    )
}

/// Lightweight header-only mail list projection (skips large body/recipient/attachment columns).
pub fn list_mail_summaries(
    conn: &Connection,
    folder_id_or_wellknown: Option<&str>,
    limit: usize,
) -> Result<Vec<MailMessage>> {
    list_mail_summaries_filtered(conn, folder_id_or_wellknown, false, false, false, limit)
}

/// Queries mail summaries for smart folders (`Unread`, `Flagged`), excluding Deleted Items
/// via well-known folder mapping and an optional explicit excluded folder ID.
pub fn list_smart_folder_summaries(
    conn: &Connection,
    unread_only: bool,
    flagged_only: bool,
    excluded_folder_id: Option<&str>,
    limit: usize,
) -> Result<Vec<MailMessage>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut extra_clauses = String::new();
    if unread_only {
        extra_clauses.push_str(" AND m.is_read = 0");
    }
    if flagged_only {
        extra_clauses.push_str(" AND m.flagged = 1");
    }
    extra_clauses.push_str(
        " AND (m.folder_id IS NULL OR (
            LOWER(m.folder_id) NOT IN ('deleteditems', 'trash', 'deleted')
            AND m.folder_id NOT IN (
                SELECT id FROM mail_folders
                WHERE LOWER(COALESCE(well_known_name, '')) = 'deleteditems'
            )
        ))",
    );

    if let Some(ex_fid) = excluded_folder_id.map(str::trim).filter(|s| !s.is_empty()) {
        let sql = format!(
            "
            SELECT {MAIL_SUMMARY_SELECT_COLS}
            FROM mail_messages m
            WHERE 1 = 1
              {extra_clauses}
              AND COALESCE(m.folder_id, '') != ?1
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![ex_fid, limit as i64], map_mail_summary_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {MAIL_SUMMARY_SELECT_COLS}
            FROM mail_messages m
            WHERE 1 = 1
              {extra_clauses}
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?1
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![limit as i64], map_mail_summary_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

/// Counts cached mail messages matching smart folder criteria (`Unread`, `Flagged`),
/// excluding Deleted Items via well-known folder mapping and an optional explicit excluded folder ID.
pub fn count_smart_folder_messages(
    conn: &Connection,
    unread_only: bool,
    flagged_only: bool,
    excluded_folder_id: Option<&str>,
) -> Result<i32> {
    let mut extra_clauses = String::new();
    if unread_only {
        extra_clauses.push_str(" AND m.is_read = 0");
    }
    if flagged_only {
        extra_clauses.push_str(" AND m.flagged = 1");
    }
    extra_clauses.push_str(
        " AND (m.folder_id IS NULL OR (
            LOWER(m.folder_id) NOT IN ('deleteditems', 'trash', 'deleted')
            AND m.folder_id NOT IN (
                SELECT id FROM mail_folders
                WHERE LOWER(COALESCE(well_known_name, '')) = 'deleteditems'
            )
        ))",
    );

    let count: i64 = if let Some(ex_fid) = excluded_folder_id.map(str::trim).filter(|s| !s.is_empty())
    {
        let sql = format!(
            "
            SELECT COUNT(*)
            FROM mail_messages m
            WHERE 1 = 1
              {extra_clauses}
              AND COALESCE(m.folder_id, '') != ?1
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.query_row(params![ex_fid], |row| row.get(0))?
    } else {
        let sql = format!(
            "
            SELECT COUNT(*)
            FROM mail_messages m
            WHERE 1 = 1
              {extra_clauses}
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.query_row([], |row| row.get(0))?
    };
    Ok(count.max(0) as i32)
}

/// Reload a folder through the last exposed row, including newer arrivals above it.
pub(super) fn list_mail_summaries_through(
    conn: &Connection,
    folder_id: &str,
    received_at: &str,
    id: &str,
) -> Result<Vec<MailMessage>> {
    let sql = format!(
        "SELECT {MAIL_SUMMARY_SELECT_COLS}
         FROM mail_messages m
         WHERE {FOLDER_FILTER_PREDICATE} AND (m.received_at, m.id) >= (?2, ?3)
         ORDER BY m.received_at DESC, m.id DESC"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![folder_id, received_at, id], map_mail_summary_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_mail_message(conn: &Connection, message_id: &str) -> Result<Option<MailMessage>> {
    let sql = format!(
        "
        SELECT {MAIL_SELECT_COLS}
        FROM mail_messages m
        WHERE m.id = ?1
        "
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_row(params![message_id], map_mail_message_row)
        .optional()
        .map_err(Into::into)
}

pub fn set_mail_read_local(conn: &mut Connection, message_id: &str, is_read: bool) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut sel = tx.prepare_cached("SELECT folder_id, is_read FROM mail_messages WHERE id = ?1")?;
        let existing: Option<(Option<String>, i32)> = sel
            .query_row(params![message_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        if let Some((folder_id, old_is_read_i)) = existing {
            let old_is_read = old_is_read_i != 0;
            if old_is_read != is_read {
                let mut upd_msg =
                    tx.prepare_cached("UPDATE mail_messages SET is_read = ?2 WHERE id = ?1")?;
                upd_msg.execute(params![message_id, bool_to_i32(is_read)])?;
                if let Some(fid) = folder_id {
                    let delta: i32 = if is_read { -1 } else { 1 };
                    let mut upd_fld = tx.prepare_cached(
                        "UPDATE mail_folders SET unread_item_count = MAX(0, unread_item_count + ?2) WHERE id = ?1",
                    )?;
                    upd_fld.execute(params![fid, delta])?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn set_mail_flagged_local(conn: &Connection, message_id: &str, flagged: bool) -> Result<()> {
    let flag_i = bool_to_i32(flagged);
    let mut stmt = conn.prepare_cached(
        "UPDATE mail_messages SET flagged = ?2 WHERE id = ?1 AND flagged != ?2",
    )?;
    stmt.execute(params![message_id, flag_i])?;
    Ok(())
}

pub fn move_mail_folder_local(
    conn: &mut Connection,
    message_id: &str,
    target_folder_id: &str,
) -> Result<()> {
    let trimmed = target_folder_id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let alias = normalize_well_known_alias(trimmed);

    let tx = conn.transaction()?;
    {
        let mut resolve_stmt = tx.prepare_cached(
            "
            SELECT id FROM mail_folders
            WHERE id = ?1
               OR LOWER(COALESCE(well_known_name, '')) = LOWER(?2)
               OR LOWER(display_name) = LOWER(?1)
            LIMIT 1
            ",
        )?;
        let resolved_target: String = resolve_stmt
            .query_row(params![trimmed, alias], |row| row.get(0))
            .optional()?
            .unwrap_or_else(|| trimmed.to_string());

        let mut sel = tx.prepare_cached(
            "SELECT rowid, folder_id, is_read, subject, from_display, body_plain FROM mail_messages WHERE id = ?1",
        )?;
        let existing: Option<(i64, Option<String>, i32, String, String, String)> = sel
            .query_row(params![message_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .optional()?;

        if let Some((rowid, old_folder_id, is_read_i, subject, from_display, body_plain)) = existing
        {
            if old_folder_id.as_deref() != Some(resolved_target.as_str()) {
                let mut upd_msg =
                    tx.prepare_cached("UPDATE mail_messages SET folder_id = ?2 WHERE id = ?1")?;
                upd_msg.execute(params![message_id, resolved_target])?;

                let mut del_fts = tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?;
                del_fts.execute(params![rowid])?;
                let mut ins_fts = tx.prepare_cached(
                    "INSERT INTO mail_fts (rowid, msg_id, folder_id, subject, from_display, body_plain) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                ins_fts.execute(params![
                    rowid,
                    message_id,
                    resolved_target,
                    subject,
                    from_display,
                    body_plain
                ])?;

                if let Some(old_fid) = old_folder_id {
                    let mut dec_old = tx.prepare_cached(
                        "
                        UPDATE mail_folders
                        SET total_item_count = MAX(0, total_item_count - 1),
                            unread_item_count = CASE
                                WHEN ?2 = 0 THEN MAX(0, unread_item_count - 1)
                                ELSE unread_item_count
                            END
                        WHERE id = ?1
                        ",
                    )?;
                    dec_old.execute(params![old_fid, is_read_i])?;
                }

                let mut inc_new = tx.prepare_cached(
                    "
                    UPDATE mail_folders
                    SET total_item_count = total_item_count + 1,
                        unread_item_count = CASE
                            WHEN ?2 = 0 THEN unread_item_count + 1
                            ELSE unread_item_count
                        END
                    WHERE id = ?1
                    ",
                )?;
                inc_new.execute(params![resolved_target, is_read_i])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Atomically replaces a moved/archived/soft-deleted mail message (`old_id` -> `moved.id`)
/// across `mail_messages`, `mail_fts`, and `mail_folders` counts in a single SQLite transaction.
pub fn replace_moved_mail_local(
    conn: &mut Connection,
    old_id: &str,
    moved: &MailMessage,
) -> Result<()> {
    replace_moved_mail_local_ex(conn, old_id, moved, !moved.attachments.is_empty())?;
    Ok(())
}

/// Variant of `replace_moved_mail_local` that tracks whether the move response explicitly
/// expanded `attachments` (distinguishing an omitted field from an explicit `attachments: []`)
/// and returns the merged persisted `MailMessage`.
pub fn replace_moved_mail_local_ex(
    conn: &mut Connection,
    old_id: &str,
    moved: &MailMessage,
    attachments_expanded: bool,
) -> Result<MailMessage> {
    let tx = conn.transaction()?;
    let effective = {
        let old_msg = {
            let sql = format!("SELECT {MAIL_SELECT_COLS} FROM mail_messages m WHERE m.id = ?1");
            let mut stmt = tx.prepare_cached(&sql)?;
            stmt.query_row(params![old_id], map_mail_message_row)
                .optional()?
        };
        let old_rowid: Option<i64> = {
            let mut sel = tx.prepare_cached("SELECT rowid FROM mail_messages WHERE id = ?1")?;
            sel.query_row(params![old_id], |row| row.get(0)).optional()?
        };

        let mut effective = moved.clone();
        if let Some(ref raw_fid) = effective.folder_id {
            let trimmed = raw_fid.trim();
            if !trimmed.is_empty() {
                let alias = normalize_well_known_alias(trimmed);
                let mut resolve_stmt = tx.prepare_cached(
                    "
                    SELECT id FROM mail_folders
                    WHERE id = ?1
                       OR LOWER(COALESCE(well_known_name, '')) = LOWER(?2)
                       OR LOWER(display_name) = LOWER(?1)
                    LIMIT 1
                    ",
                )?;
                if let Some(resolved) = resolve_stmt
                    .query_row(params![trimmed, alias], |row| row.get::<_, String>(0))
                    .optional()?
                {
                    effective.folder_id = Some(resolved);
                }
            }
        }

        if let Some(ref old) = old_msg {
            if !effective.body_loaded && old.body_loaded {
                effective.body_loaded = true;
                if effective.body_html.is_empty() {
                    effective.body_html = old.body_html.clone();
                }
                if effective.body_plain.is_empty() {
                    effective.body_plain = old.body_plain.clone();
                }
                if effective.to_recipients.is_empty() {
                    effective.to_recipients = old.to_recipients.clone();
                }
                if effective.cc_recipients.is_empty() {
                    effective.cc_recipients = old.cc_recipients.clone();
                }
                if effective.bcc_recipients.is_empty() {
                    effective.bcc_recipients = old.bcc_recipients.clone();
                }
                if effective.reply_to.is_empty() {
                    effective.reply_to = old.reply_to.clone();
                }
            }
            if !attachments_expanded
                && effective.has_attachments
                && effective.attachments.is_empty()
                && !old.attachments.is_empty()
            {
                effective.attachments = old.attachments.clone();
            }
            if effective.from.is_none() {
                effective.from = old.from.clone();
            }
            if effective.sender.is_none() {
                effective.sender = old.sender.clone();
            }
            if effective.categories.is_empty() {
                effective.categories = old.categories.clone();
            }
            if effective.preview.is_empty() {
                effective.preview = old.preview.clone();
            }
            if effective.inference_classification.is_none() {
                effective.inference_classification = old.inference_classification.clone();
            }
            if effective.internet_message_id.is_none() {
                effective.internet_message_id = old.internet_message_id.clone();
            }
            if effective.conversation_id.is_none() {
                effective.conversation_id = old.conversation_id.clone();
            }
            if effective.sent_at.is_none() {
                effective.sent_at = old.sent_at.clone();
            }
            if effective.last_modified_at.is_none() {
                effective.last_modified_at = old.last_modified_at.clone();
            }
            if effective.meeting_request.is_none() {
                effective.meeting_request = old.meeting_request.clone();
            }
            if effective.web_link.is_none() {
                effective.web_link = old.web_link.clone();
            }
        }

        if old_id != effective.id || (attachments_expanded && effective.attachments.is_empty()) {
            if let Some(rowid) = old_rowid {
                let mut del_fts = tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?;
                del_fts.execute(params![rowid])?;
                let mut del_msg = tx.prepare_cached("DELETE FROM mail_messages WHERE id = ?1")?;
                del_msg.execute(params![old_id])?;
            }
        }

        upsert_mail_messages_in_tx(&tx, std::slice::from_ref(&effective))?;

        let old_folder_id = old_msg.as_ref().and_then(|m| m.folder_id.as_deref());
        let new_folder_id = effective.folder_id.as_deref();
        if old_folder_id != new_folder_id {
            if let Some(old_fid) = old_folder_id {
                let old_read_i = bool_to_i32(old_msg.as_ref().is_some_and(|m| m.is_read));
                let mut dec_old = tx.prepare_cached(
                    "
                    UPDATE mail_folders
                    SET total_item_count = MAX(0, total_item_count - 1),
                        unread_item_count = CASE
                            WHEN ?2 = 0 THEN MAX(0, unread_item_count - 1)
                            ELSE unread_item_count
                        END
                    WHERE id = ?1
                    ",
                )?;
                dec_old.execute(params![old_fid, old_read_i])?;
            }
            if let Some(new_fid) = new_folder_id {
                let new_read_i = bool_to_i32(effective.is_read);
                let mut inc_new = tx.prepare_cached(
                    "
                    UPDATE mail_folders
                    SET total_item_count = total_item_count + 1,
                        unread_item_count = CASE
                            WHEN ?2 = 0 THEN unread_item_count + 1
                            ELSE unread_item_count
                        END
                    WHERE id = ?1
                    ",
                )?;
                inc_new.execute(params![new_fid, new_read_i])?;
            }
        }
        effective
    };
    tx.commit()?;
    Ok(effective)
}

fn folder_matches_scope(
    row_folder: Option<&str>,
    folder_id: &str,
    folder_alias: Option<&str>,
) -> bool {
    let Some(rf) = row_folder else {
        return false;
    };
    if rf == folder_id {
        return true;
    }
    if let Some(alias) = folder_alias {
        if !alias.is_empty() && rf.eq_ignore_ascii_case(alias) {
            return true;
        }
    }
    false
}

fn delete_scoped_removed_in_tx(
    tx: &Transaction<'_>,
    folder_id: &str,
    folder_alias: Option<&str>,
    removed_ids: &[String],
) -> Result<usize> {
    if removed_ids.is_empty() {
        return Ok(0);
    }
    let mut deleted_count = 0usize;
    let mut sel_row =
        tx.prepare_cached("SELECT rowid, folder_id FROM mail_messages WHERE id = ?1")?;
    let mut del_fts = tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?;
    let mut del_msg = tx.prepare_cached("DELETE FROM mail_messages WHERE id = ?1")?;
    for id in removed_ids {
        if let Some((rowid, row_folder)) = sel_row
            .query_row(params![id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .optional()?
        {
            if folder_matches_scope(row_folder.as_deref(), folder_id, folder_alias) {
                del_fts.execute(params![rowid])?;
                del_msg.execute(params![id])?;
                deleted_count += 1;
            }
        }
    }
    Ok(deleted_count)
}

/// Applies an intermediate page of a folder's mail delta (scoped `@removed` deletions that
/// preserve items already moved to another folder, plus upserts) in a single SQLite transaction
/// without modifying `sync_cursors` or pruning unseen rows.
/// Returns `(changed_upserts, deleted_count)`.
pub fn apply_mail_folder_delta_page(
    conn: &mut Connection,
    folder_id: &str,
    folder_alias: Option<&str>,
    incoming: &[MailMessage],
    removed_ids: &[String],
) -> Result<(usize, usize)> {
    if incoming.is_empty() && removed_ids.is_empty() {
        return Ok((0, 0));
    }
    let tx = conn.transaction()?;
    let deleted_count;
    let changed_count;
    {
        deleted_count = delete_scoped_removed_in_tx(&tx, folder_id, folder_alias, removed_ids)?;
        changed_count = upsert_mail_messages_in_tx(&tx, incoming)?;
    }
    tx.commit()?;
    Ok((changed_count, deleted_count))
}

/// Atomically applies a folder's mail delta (scoped `@removed` deletions that preserve items
/// already moved to another folder, upserts, optional full-bootstrap reconciliation when
/// `reconcile_seen_ids` is `Some`, and sync cursor persistence) in a single SQLite transaction.
/// Returns `(changed_upserts, deleted_count)`.
pub fn apply_mail_folder_delta(
    conn: &mut Connection,
    folder_id: &str,
    folder_alias: Option<&str>,
    incoming: &[MailMessage],
    removed_ids: &[String],
    reconcile_seen_ids: Option<&HashSet<String>>,
    cursor: &SyncCursor,
) -> Result<(usize, usize)> {
    let tx = conn.transaction()?;
    let mut deleted_count;
    let changed_count;
    {
        deleted_count = delete_scoped_removed_in_tx(&tx, folder_id, folder_alias, removed_ids)?;
        changed_count = upsert_mail_messages_in_tx(&tx, incoming)?;

        if let Some(seen_ids) = reconcile_seen_ids {
            let candidates: Vec<(String, i64)> = if let Some(alias) =
                folder_alias.filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case(folder_id))
            {
                let mut sel = tx.prepare_cached(
                    "SELECT id, rowid FROM mail_messages WHERE folder_id = ?1 OR LOWER(COALESCE(folder_id, '')) = LOWER(?2)",
                )?;
                let rows = sel.query_map(params![folder_id, alias], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                let mut sel =
                    tx.prepare_cached("SELECT id, rowid FROM mail_messages WHERE folder_id = ?1")?;
                let rows =
                    sel.query_map(params![folder_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            let mut del_fts = tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?;
            let mut del_msg = tx.prepare_cached("DELETE FROM mail_messages WHERE id = ?1")?;
            for (id, rowid) in candidates {
                if !seen_ids.contains(&id) {
                    del_fts.execute(params![rowid])?;
                    del_msg.execute(params![id])?;
                    deleted_count += 1;
                }
            }
        }

        upsert_sync_cursor_in_tx(&tx, cursor)?;
    }
    tx.commit()?;
    Ok((changed_count, deleted_count))
}

pub fn delete_mail_messages_by_ids(conn: &mut Connection, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut sel_row =
            tx.prepare_cached("SELECT rowid, folder_id, is_read FROM mail_messages WHERE id = ?1")?;
        let mut del_fts = tx.prepare_cached("DELETE FROM mail_fts WHERE rowid = ?1")?;
        let mut del_msg = tx.prepare_cached("DELETE FROM mail_messages WHERE id = ?1")?;
        let mut dec_folder = tx.prepare_cached(
            "
            UPDATE mail_folders
            SET total_item_count = MAX(0, total_item_count - 1),
                unread_item_count = CASE
                    WHEN ?2 = 0 THEN MAX(0, unread_item_count - 1)
                    ELSE unread_item_count
                END
            WHERE id = ?1
            ",
        )?;
        for id in ids {
            if let Some((rowid, folder_id, is_read_i)) = sel_row
                .query_row(params![id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i32>(2)?,
                    ))
                })
                .optional()?
            {
                del_fts.execute(params![rowid])?;
                del_msg.execute(params![id])?;
                if let Some(fid) = folder_id {
                    dec_folder.execute(params![fid, is_read_i])?;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn upsert_calendar_events(conn: &mut Connection, events: &[CalendarEvent]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_calendar_events_in_tx(&tx, events)?;
    tx.commit()?;
    Ok(())
}

fn upsert_calendar_events_in_tx(tx: &Transaction<'_>, events: &[CalendarEvent]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "
        INSERT INTO calendar_events (
            id, subject, start_iso, end_iso, time_zone, is_all_day,
            location, organizer_name, organizer_email, attendees_json,
            response_status, is_online_meeting, online_meeting_url,
            body_html, body_plain, is_cancelled, web_link,
            categories_json, show_as, sensitivity, event_type,
            has_attachments, is_recurring, recurrence_json
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10,
            ?11, ?12, ?13,
            ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21,
            ?22, ?23, ?24
        )
        ON CONFLICT(id) DO UPDATE SET
            subject = excluded.subject,
            start_iso = excluded.start_iso,
            end_iso = excluded.end_iso,
            time_zone = excluded.time_zone,
            is_all_day = excluded.is_all_day,
            location = excluded.location,
            organizer_name = excluded.organizer_name,
            organizer_email = excluded.organizer_email,
            attendees_json = excluded.attendees_json,
            response_status = excluded.response_status,
            is_online_meeting = excluded.is_online_meeting,
            online_meeting_url = COALESCE(excluded.online_meeting_url, calendar_events.online_meeting_url),
            body_html = excluded.body_html,
            body_plain = excluded.body_plain,
            is_cancelled = excluded.is_cancelled,
            web_link = COALESCE(excluded.web_link, calendar_events.web_link),
            categories_json = excluded.categories_json,
            show_as = COALESCE(excluded.show_as, calendar_events.show_as),
            sensitivity = COALESCE(excluded.sensitivity, calendar_events.sensitivity),
            event_type = COALESCE(excluded.event_type, calendar_events.event_type),
            has_attachments = excluded.has_attachments,
            is_recurring = excluded.is_recurring,
            recurrence_json = COALESCE(excluded.recurrence_json, calendar_events.recurrence_json)
        ",
    )?;
    for ev in events {
        let attendees_json = serde_json::to_string(&ev.attendees)?;
        let categories_json = serde_json::to_string(&ev.categories)?;
        let recurrence_json = ev
            .recurrence
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        stmt.execute(params![
            ev.id,
            ev.subject,
            ev.start_iso,
            ev.end_iso,
            ev.time_zone,
            bool_to_i32(ev.is_all_day),
            ev.location,
            ev.organizer_name,
            ev.organizer_email,
            attendees_json,
            ev.response_status,
            bool_to_i32(ev.is_online_meeting),
            ev.online_meeting_url,
            ev.body_html,
            ev.body_plain,
            bool_to_i32(ev.is_cancelled),
            ev.web_link,
            categories_json,
            ev.show_as,
            ev.sensitivity,
            ev.event_type,
            bool_to_i32(ev.has_attachments),
            bool_to_i32(ev.is_recurring),
            recurrence_json,
        ])?;
    }
    Ok(())
}

/// Atomically reconciles cached calendar events within `[start_iso, end_iso]` against a
/// complete snapshot from Graph, removing events deleted remotely inside that window.
pub fn replace_calendar_events_in_window(
    conn: &mut Connection,
    start_iso: &str,
    end_iso: &str,
    events: &[CalendarEvent],
) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut deleted = 0usize;
    {
        upsert_calendar_events_in_tx(&tx, events)?;
        let keep: HashSet<&str> = events.iter().map(|e| e.id.as_str()).collect();
        let existing_ids: Vec<String> = {
            let mut sel = tx.prepare_cached(
                "
                SELECT id FROM calendar_events
                WHERE (end_iso >= ?1 OR start_iso >= ?1)
                  AND start_iso <= ?2
                ",
            )?;
            let rows = sel.query_map(params![start_iso, end_iso], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut del = tx.prepare_cached("DELETE FROM calendar_events WHERE id = ?1")?;
        for id in existing_ids {
            if !keep.contains(id.as_str()) {
                del.execute(params![id])?;
                deleted += 1;
            }
        }
    }
    tx.commit()?;
    Ok(deleted)
}

/// Atomically applies a calendar delta (deletions, upserts, optional initial-delta window
/// reconciliation, and sync cursor save) in a single transaction.
pub fn apply_calendar_delta(
    conn: &mut Connection,
    start_iso: &str,
    end_iso: &str,
    events: &[CalendarEvent],
    removed_ids: &[String],
    reconcile_window: bool,
    cursor: &SyncCursor,
) -> Result<bool> {
    let tx = conn.transaction()?;
    let mut changed = !events.is_empty() || !removed_ids.is_empty();
    {
        if !removed_ids.is_empty() {
            let mut del = tx.prepare_cached("DELETE FROM calendar_events WHERE id = ?1")?;
            for id in removed_ids {
                del.execute(params![id])?;
            }
        }
        if !events.is_empty() {
            upsert_calendar_events_in_tx(&tx, events)?;
        }
        if reconcile_window {
            let keep: HashSet<&str> = events.iter().map(|e| e.id.as_str()).collect();
            let existing_ids: Vec<String> = {
                let mut sel = tx.prepare_cached(
                    "
                    SELECT id FROM calendar_events
                    WHERE (end_iso >= ?1 OR start_iso >= ?1)
                      AND start_iso <= ?2
                    ",
                )?;
                let rows = sel.query_map(params![start_iso, end_iso], |row| row.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut del = tx.prepare_cached("DELETE FROM calendar_events WHERE id = ?1")?;
            for id in existing_ids {
                if !keep.contains(id.as_str()) {
                    del.execute(params![id])?;
                    changed = true;
                }
            }
        }
        upsert_sync_cursor_in_tx(&tx, cursor)?;
    }
    tx.commit()?;
    Ok(changed)
}

pub fn list_calendar_events(
    conn: &Connection,
    start_iso: Option<&str>,
    end_iso: Option<&str>,
    limit: usize,
) -> Result<Vec<CalendarEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let start = start_iso.map(str::trim).filter(|s| !s.is_empty());
    let end = end_iso.map(str::trim).filter(|s| !s.is_empty());

    match (start, end) {
        (Some(s), Some(e)) => {
            let sql = format!(
                "SELECT {CALENDAR_SELECT_COLS} FROM calendar_events WHERE (end_iso >= ?1 OR start_iso >= ?1) AND start_iso <= ?2 ORDER BY start_iso ASC, id ASC LIMIT ?3"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![s, e, limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        (Some(s), None) => {
            let sql = format!(
                "SELECT {CALENDAR_SELECT_COLS} FROM calendar_events WHERE end_iso >= ?1 OR start_iso >= ?1 ORDER BY start_iso ASC, id ASC LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![s, limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        (None, Some(e)) => {
            let sql = format!(
                "SELECT {CALENDAR_SELECT_COLS} FROM calendar_events WHERE start_iso <= ?1 ORDER BY start_iso ASC, id ASC LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![e, limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        (None, None) => {
            let sql = format!(
                "SELECT {CALENDAR_SELECT_COLS} FROM calendar_events ORDER BY start_iso ASC, id ASC LIMIT ?1"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
    }
}

pub fn get_calendar_event(conn: &Connection, event_id: &str) -> Result<Option<CalendarEvent>> {
    let sql = format!("SELECT {CALENDAR_SELECT_COLS} FROM calendar_events WHERE id = ?1");
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_row(params![event_id], map_calendar_event_row)
        .optional()
        .map_err(Into::into)
}

pub fn update_event_response_local(
    conn: &Connection,
    event_id: &str,
    response_status: &str,
) -> Result<()> {
    let mut stmt =
        conn.prepare_cached("UPDATE calendar_events SET response_status = ?2 WHERE id = ?1")?;
    stmt.execute(params![event_id, response_status])?;
    Ok(())
}

pub fn delete_calendar_event(conn: &Connection, event_id: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM calendar_events WHERE id = ?1")?;
    stmt.execute(params![event_id])?;
    Ok(())
}

pub fn delete_calendar_events_by_ids(conn: &mut Connection, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("DELETE FROM calendar_events WHERE id = ?1")?;
        for id in ids {
            stmt.execute(params![id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn upsert_contacts(conn: &mut Connection, contacts: &[ContactItem]) -> Result<()> {
    if contacts.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_contacts_in_tx(&tx, contacts)?;
    tx.commit()?;
    Ok(())
}

fn upsert_contacts_in_tx(tx: &Transaction<'_>, contacts: &[ContactItem]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "
        INSERT INTO contacts (
            id, display_name, given_name, surname, emails_json,
            mobile_phone, business_phones_json, company_name, job_title,
            department, notes, source
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(id) DO UPDATE SET
            display_name = excluded.display_name,
            given_name = excluded.given_name,
            surname = excluded.surname,
            emails_json = excluded.emails_json,
            mobile_phone = excluded.mobile_phone,
            business_phones_json = excluded.business_phones_json,
            company_name = excluded.company_name,
            job_title = excluded.job_title,
            department = excluded.department,
            notes = excluded.notes,
            source = excluded.source
        ",
    )?;
    for c in contacts {
        let emails_json = serde_json::to_string(&c.email_addresses)?;
        let business_phones_json = serde_json::to_string(&c.business_phones)?;
        stmt.execute(params![
            c.id,
            c.display_name,
            c.given_name,
            c.surname,
            emails_json,
            c.mobile_phone,
            business_phones_json,
            c.company_name,
            c.job_title,
            c.department,
            c.notes,
            c.source,
        ])?;
    }
    Ok(())
}

/// Atomically reconciles personal Outlook contacts (`source = 'contact'`) against a complete
/// `/me/contacts` snapshot while preserving cached directory entries.
pub fn replace_personal_contacts(conn: &mut Connection, contacts: &[ContactItem]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        upsert_contacts_in_tx(&tx, contacts)?;
        let keep: HashSet<&str> = contacts.iter().map(|c| c.id.as_str()).collect();
        let existing_ids: Vec<String> = {
            let mut sel =
                tx.prepare_cached("SELECT id FROM contacts WHERE source = 'contact'")?;
            let rows = sel.query_map([], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut del = tx.prepare_cached("DELETE FROM contacts WHERE id = ?1")?;
        for id in existing_ids {
            if !keep.contains(id.as_str()) {
                del.execute(params![id])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_contacts(
    conn: &Connection,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<ContactItem>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let q = query.map(str::trim).filter(|s| !s.is_empty());
    if let Some(q_str) = q {
        let pattern = format!("%{}%", escape_like(&q_str.to_lowercase()));
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                id, display_name, given_name, surname, emails_json,
                mobile_phone, business_phones_json, company_name, job_title,
                department, notes, source
            FROM contacts
            WHERE LOWER(display_name) LIKE ?1 ESCAPE '\\'
               OR LOWER(COALESCE(given_name, '')) LIKE ?1 ESCAPE '\\'
               OR LOWER(COALESCE(surname, '')) LIKE ?1 ESCAPE '\\'
               OR LOWER(emails_json) LIKE ?1 ESCAPE '\\'
               OR LOWER(COALESCE(mobile_phone, '')) LIKE ?1 ESCAPE '\\'
               OR LOWER(COALESCE(company_name, '')) LIKE ?1 ESCAPE '\\'
               OR LOWER(COALESCE(job_title, '')) LIKE ?1 ESCAPE '\\'
               OR LOWER(COALESCE(department, '')) LIKE ?1 ESCAPE '\\'
            ORDER BY display_name COLLATE NOCASE ASC, id ASC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], map_contact_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                id, display_name, given_name, surname, emails_json,
                mobile_phone, business_phones_json, company_name, job_title,
                department, notes, source
            FROM contacts
            ORDER BY display_name COLLATE NOCASE ASC, id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt.query_map(params![limit as i64], map_contact_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn delete_contact(conn: &Connection, contact_id: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM contacts WHERE id = ?1")?;
    stmt.execute(params![contact_id])?;
    Ok(())
}

pub fn upsert_todo_lists(conn: &mut Connection, lists: &[TodoList]) -> Result<()> {
    if lists.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_todo_lists_in_tx(&tx, lists)?;
    tx.commit()?;
    Ok(())
}

fn upsert_todo_lists_in_tx(tx: &Transaction<'_>, lists: &[TodoList]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "
        INSERT INTO todo_lists (id, display_name, wellknown_list_name, is_owner, is_shared)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(id) DO UPDATE SET
            display_name = excluded.display_name,
            wellknown_list_name = COALESCE(excluded.wellknown_list_name, todo_lists.wellknown_list_name),
            is_owner = excluded.is_owner,
            is_shared = excluded.is_shared
        ",
    )?;
    for list in lists {
        stmt.execute(params![
            list.id,
            list.display_name,
            list.wellknown_list_name,
            bool_to_i32(list.is_owner),
            bool_to_i32(list.is_shared),
        ])?;
    }
    Ok(())
}

/// Atomically reconciles cached To Do lists against a complete snapshot, deleting lists
/// (and their orphaned tasks) removed remotely.
pub fn replace_todo_lists(conn: &mut Connection, lists: &[TodoList]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        upsert_todo_lists_in_tx(&tx, lists)?;
        let keep: HashSet<&str> = lists.iter().map(|l| l.id.as_str()).collect();
        let existing_ids: Vec<String> = {
            let mut sel = tx.prepare_cached("SELECT id FROM todo_lists")?;
            let rows = sel.query_map([], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut del_list = tx.prepare_cached("DELETE FROM todo_lists WHERE id = ?1")?;
        let mut del_tasks = tx.prepare_cached("DELETE FROM todo_tasks WHERE list_id = ?1")?;
        for id in existing_ids {
            if !keep.contains(id.as_str()) {
                del_tasks.execute(params![id])?;
                del_list.execute(params![id])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_todo_lists(conn: &Connection) -> Result<Vec<TodoList>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT id, display_name, wellknown_list_name, is_owner, is_shared
        FROM todo_lists
        ORDER BY
            CASE WHEN LOWER(COALESCE(wellknown_list_name, '')) = 'defaultlist' THEN 0 ELSE 1 END ASC,
            display_name COLLATE NOCASE ASC,
            id ASC
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        let is_owner_int: i32 = row.get(3)?;
        let is_shared_int: i32 = row.get(4)?;
        Ok(TodoList {
            id: row.get(0)?,
            display_name: row.get(1)?,
            wellknown_list_name: row.get(2)?,
            is_owner: is_owner_int != 0,
            is_shared: is_shared_int != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn upsert_todo_tasks(conn: &mut Connection, tasks: &[TodoTask]) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    upsert_todo_tasks_in_tx(&tx, tasks)?;
    tx.commit()?;
    Ok(())
}

fn upsert_todo_tasks_in_tx(tx: &Transaction<'_>, tasks: &[TodoTask]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "
        INSERT INTO todo_tasks (
            id, list_id, title, status, importance, body_plain,
            due_date_iso, reminder_iso, completed_at, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ON CONFLICT(id) DO UPDATE SET
            list_id = excluded.list_id,
            title = excluded.title,
            status = excluded.status,
            importance = excluded.importance,
            body_plain = excluded.body_plain,
            due_date_iso = excluded.due_date_iso,
            reminder_iso = excluded.reminder_iso,
            completed_at = excluded.completed_at,
            created_at = COALESCE(excluded.created_at, todo_tasks.created_at),
            updated_at = COALESCE(excluded.updated_at, todo_tasks.updated_at)
        ",
    )?;
    for t in tasks {
        stmt.execute(params![
            t.id,
            t.list_id,
            t.title,
            t.status,
            t.importance,
            t.body_plain,
            t.due_date_iso,
            t.reminder_iso,
            t.completed_at,
            t.created_at,
            t.updated_at,
        ])?;
    }
    Ok(())
}

/// Atomically reconciles cached tasks for `list_id` against a complete snapshot.
/// When `include_completed` is false, completed tasks are left intact and only missing
/// active tasks are pruned.
pub fn replace_todo_tasks_for_list(
    conn: &mut Connection,
    list_id: &str,
    include_completed: bool,
    tasks: &[TodoTask],
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        upsert_todo_tasks_in_tx(&tx, tasks)?;
        let keep: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let existing_ids: Vec<String> = if include_completed {
            let mut sel = tx.prepare_cached("SELECT id FROM todo_tasks WHERE list_id = ?1")?;
            let rows = sel.query_map(params![list_id], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut sel = tx.prepare_cached(
                "SELECT id FROM todo_tasks WHERE list_id = ?1 AND LOWER(status) != 'completed'",
            )?;
            let rows = sel.query_map(params![list_id], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut del = tx.prepare_cached("DELETE FROM todo_tasks WHERE id = ?1")?;
        for id in existing_ids {
            if !keep.contains(id.as_str()) {
                del.execute(params![id])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_todo_tasks(
    conn: &Connection,
    list_id: Option<&str>,
    include_completed: bool,
) -> Result<Vec<TodoTask>> {
    let list_filter = list_id.map(str::trim).filter(|s| !s.is_empty());
    let status_clause = if include_completed {
        ""
    } else {
        "AND LOWER(status) != 'completed'"
    };
    let order_clause = "
        ORDER BY
            CASE WHEN LOWER(status) = 'completed' THEN 1 ELSE 0 END ASC,
            CASE WHEN due_date_iso IS NULL OR due_date_iso = '' THEN 1 ELSE 0 END ASC,
            due_date_iso ASC,
            CASE LOWER(importance)
                WHEN 'high' THEN 0
                WHEN 'normal' THEN 1
                ELSE 2
            END ASC,
            id ASC
    ";

    if let Some(lid) = list_filter {
        let sql = format!(
            "
            SELECT
                id, list_id, title, status, importance, body_plain,
                due_date_iso, reminder_iso, completed_at, created_at, updated_at
            FROM todo_tasks
            WHERE list_id = ?1
              {status_clause}
            {order_clause}
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![lid], map_todo_task_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT
                id, list_id, title, status, importance, body_plain,
                due_date_iso, reminder_iso, completed_at, created_at, updated_at
            FROM todo_tasks
            WHERE 1 = 1
              {status_clause}
            {order_clause}
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([], map_todo_task_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn set_todo_task_completed_local(
    conn: &Connection,
    task_id: &str,
    completed: bool,
) -> Result<()> {
    let status = if completed { "completed" } else { "notStarted" };
    let completed_at = completed.then(|| chrono::Utc::now().to_rfc3339());
    let mut stmt = conn.prepare_cached(
        "UPDATE todo_tasks SET status = ?2, completed_at = ?3 WHERE id = ?1",
    )?;
    stmt.execute(params![task_id, status, completed_at])?;
    Ok(())
}

pub fn delete_todo_task(conn: &Connection, task_id: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM todo_tasks WHERE id = ?1")?;
    stmt.execute(params![task_id])?;
    Ok(())
}

pub fn get_sync_cursor(conn: &Connection, scope_key: &str) -> Result<Option<SyncCursor>> {
    let mut stmt = conn.prepare_cached(
        "SELECT scope_key, delta_link, history_next_link, last_synced_at FROM sync_cursors WHERE scope_key = ?1",
    )?;
    stmt.query_row(params![scope_key], |row| {
        Ok(SyncCursor {
            scope_key: row.get(0)?,
            delta_link: row.get(1)?,
            history_next_link: row.get(2)?,
            last_synced_at: row.get(3)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

pub fn upsert_sync_cursor(conn: &Connection, cursor: &SyncCursor) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "
        INSERT INTO sync_cursors (scope_key, delta_link, history_next_link, last_synced_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(scope_key) DO UPDATE SET
            delta_link = excluded.delta_link,
            history_next_link = excluded.history_next_link,
            last_synced_at = excluded.last_synced_at
        ",
    )?;
    stmt.execute(params![
        cursor.scope_key,
        cursor.delta_link,
        cursor.history_next_link,
        cursor.last_synced_at,
    ])?;
    Ok(())
}

fn upsert_sync_cursor_in_tx(tx: &Transaction<'_>, cursor: &SyncCursor) -> Result<()> {
    upsert_sync_cursor(tx, cursor)
}

fn map_mail_message_row(row: &Row<'_>) -> rusqlite::Result<MailMessage> {
    let from_json: Option<String> = row.get(5)?;
    let to_json: String = row.get(6)?;
    let cc_json: String = row.get(7)?;
    let bcc_json: String = row.get(8)?;
    let reply_to_json: String = row.get(9)?;
    let is_read_int: i32 = row.get(13)?;
    let is_draft_int: i32 = row.get(14)?;
    let flagged_int: i32 = row.get(15)?;
    let has_att_int: i32 = row.get(18)?;
    let body_loaded_int: i32 = row.get(19)?;
    let attachments_json: String = row.get(23)?;
    let meeting_req_json: Option<String> = row.get(24)?;
    let sender_json: Option<String> = row.get(26)?;
    let categories_json: String = row.get(27)?;

    let from: Option<EmailRecipient> = from_json.and_then(|s| serde_json::from_str(&s).ok());
    let sender: Option<EmailRecipient> = sender_json.and_then(|s| serde_json::from_str(&s).ok());
    let categories: Vec<String> = serde_json::from_str(&categories_json).unwrap_or_default();
    let to_recipients: Vec<EmailRecipient> = serde_json::from_str(&to_json).unwrap_or_default();
    let cc_recipients: Vec<EmailRecipient> = serde_json::from_str(&cc_json).unwrap_or_default();
    let bcc_recipients: Vec<EmailRecipient> = serde_json::from_str(&bcc_json).unwrap_or_default();
    let reply_to: Vec<EmailRecipient> = serde_json::from_str(&reply_to_json).unwrap_or_default();
    let attachments: Vec<MailAttachment> =
        serde_json::from_str(&attachments_json).unwrap_or_default();
    let meeting_request: Option<MeetingRequestInfo> =
        meeting_req_json.and_then(|s| serde_json::from_str(&s).ok());

    Ok(MailMessage {
        id: row.get(0)?,
        internet_message_id: row.get(1)?,
        conversation_id: row.get(2)?,
        folder_id: row.get(3)?,
        subject: row.get(4)?,
        from,
        sender,
        to_recipients,
        cc_recipients,
        bcc_recipients,
        reply_to,
        received_at: row.get(10)?,
        sent_at: row.get(11)?,
        last_modified_at: row.get(12)?,
        is_read: is_read_int != 0,
        is_draft: is_draft_int != 0,
        flagged: flagged_int != 0,
        importance: row.get(16)?,
        inference_classification: row.get(17)?,
        categories,
        has_attachments: has_att_int != 0,
        body_loaded: body_loaded_int != 0,
        preview: row.get(20)?,
        body_html: row.get(21)?,
        body_plain: row.get(22)?,
        attachments,
        meeting_request,
        web_link: row.get(25)?,
    })
}

fn map_mail_summary_row(row: &Row<'_>) -> rusqlite::Result<MailMessage> {
    let from_json: Option<String> = row.get(5)?;
    let first_to_json: Option<String> = row.get(6)?;
    let is_read_int: i32 = row.get(10)?;
    let is_draft_int: i32 = row.get(11)?;
    let flagged_int: i32 = row.get(12)?;
    let has_att_int: i32 = row.get(15)?;
    let body_loaded_int: i32 = row.get(16)?;
    let meeting_req_json: Option<String> = row.get(18)?;
    let sender_json: Option<String> = row.get(20)?;
    let categories_json: String = row.get(21)?;

    let from: Option<EmailRecipient> = from_json.and_then(|s| serde_json::from_str(&s).ok());
    let sender: Option<EmailRecipient> = sender_json.and_then(|s| serde_json::from_str(&s).ok());
    let categories: Vec<String> = serde_json::from_str(&categories_json).unwrap_or_default();
    let to_recipients: Vec<EmailRecipient> = first_to_json
        .and_then(|s| serde_json::from_str::<EmailRecipient>(&s).ok())
        .into_iter()
        .collect();
    let meeting_request: Option<MeetingRequestInfo> =
        meeting_req_json.and_then(|s| serde_json::from_str(&s).ok());

    Ok(MailMessage {
        id: row.get(0)?,
        internet_message_id: row.get(1)?,
        conversation_id: row.get(2)?,
        folder_id: row.get(3)?,
        subject: row.get(4)?,
        from,
        sender,
        to_recipients,
        cc_recipients: Vec::new(),
        bcc_recipients: Vec::new(),
        reply_to: Vec::new(),
        received_at: row.get(7)?,
        sent_at: row.get(8)?,
        last_modified_at: row.get(9)?,
        is_read: is_read_int != 0,
        is_draft: is_draft_int != 0,
        flagged: flagged_int != 0,
        importance: row.get(13)?,
        inference_classification: row.get(14)?,
        categories,
        has_attachments: has_att_int != 0,
        body_loaded: body_loaded_int != 0,
        preview: row.get(17)?,
        body_html: String::new(),
        body_plain: String::new(),
        attachments: Vec::new(),
        meeting_request,
        web_link: row.get(19)?,
    })
}

fn map_calendar_event_row(row: &Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let is_all_day_int: i32 = row.get(5)?;
    let attendees_json: String = row.get(9)?;
    let is_online_int: i32 = row.get(11)?;
    let is_cancelled_int: i32 = row.get(15)?;
    let categories_json: String = row.get(17)?;
    let has_attachments_int: i32 = row.get(21)?;
    let is_recurring_int: i32 = row.get(22)?;
    let recurrence_json: Option<String> = row.get(23)?;
    let attendees: Vec<EventAttendee> = serde_json::from_str(&attendees_json).unwrap_or_default();
    let categories: Vec<String> = serde_json::from_str(&categories_json).unwrap_or_default();
    let recurrence: Option<serde_json::Value> = recurrence_json
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|v: &serde_json::Value| !v.is_null());

    // Rows cached by <= 0.3.0 hold Graph's offset-less UTC values; make the offset explicit on read.
    let is_all_day = is_all_day_int != 0;
    let time_zone: String = row.get(4)?;
    let start_raw: String = row.get(2)?;
    let end_raw: String = row.get(3)?;
    let start_iso =
        crate::graph::calendar::normalize_graph_event_time(&start_raw, &time_zone, is_all_day);
    let end_iso =
        crate::graph::calendar::normalize_graph_event_time(&end_raw, &time_zone, is_all_day);

    Ok(CalendarEvent {
        id: row.get(0)?,
        subject: row.get(1)?,
        start_iso,
        end_iso,
        time_zone,
        is_all_day,
        location: row.get(6)?,
        organizer_name: row.get(7)?,
        organizer_email: row.get(8)?,
        attendees,
        response_status: row.get(10)?,
        is_online_meeting: is_online_int != 0,
        online_meeting_url: row.get(12)?,
        body_html: row.get(13)?,
        body_plain: row.get(14)?,
        is_cancelled: is_cancelled_int != 0,
        web_link: row.get(16)?,
        categories,
        show_as: row.get(18)?,
        sensitivity: row.get(19)?,
        event_type: row.get(20)?,
        has_attachments: has_attachments_int != 0,
        is_recurring: is_recurring_int != 0,
        recurrence,
    })
}

fn map_contact_row(row: &Row<'_>) -> rusqlite::Result<ContactItem> {
    let emails_json: String = row.get(4)?;
    let business_phones_json: String = row.get(6)?;
    let email_addresses: Vec<String> = serde_json::from_str(&emails_json).unwrap_or_default();
    let business_phones: Vec<String> =
        serde_json::from_str(&business_phones_json).unwrap_or_default();

    Ok(ContactItem {
        id: row.get(0)?,
        display_name: row.get(1)?,
        given_name: row.get(2)?,
        surname: row.get(3)?,
        email_addresses,
        mobile_phone: row.get(5)?,
        business_phones,
        company_name: row.get(7)?,
        job_title: row.get(8)?,
        department: row.get(9)?,
        notes: row.get(10)?,
        source: row.get(11)?,
    })
}

fn map_todo_task_row(row: &Row<'_>) -> rusqlite::Result<TodoTask> {
    Ok(TodoTask {
        id: row.get(0)?,
        list_id: row.get(1)?,
        title: row.get(2)?,
        status: row.get(3)?,
        importance: row.get(4)?,
        body_plain: row.get(5)?,
        due_date_iso: row.get(6)?,
        reminder_iso: row.get(7)?,
        completed_at: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn bool_to_i32(v: bool) -> i32 {
    if v {
        1
    } else {
        0
    }
}
