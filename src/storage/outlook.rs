//! SQLite persistence and FTS5 hybrid search for Outlook Mail, Calendar, Contacts, and Microsoft To Do.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    models::{
        CalendarEvent, ContactItem, EmailRecipient, EventAttendee, MailAttachment, MailFolder,
        MailMessage, MailSearchHit, TodoList, TodoTask,
    },
    storage::repo::escape_like,
};

const MAIL_SELECT_COLS: &str = "
    m.id, m.conversation_id, m.folder_id, m.subject, m.from_json,
    m.to_json, m.cc_json, m.bcc_json, m.received_at, m.sent_at,
    m.is_read, m.is_draft, m.flagged, m.importance, m.has_attachments,
    m.preview, m.body_html, m.body_plain, m.attachments_json, m.web_link
";

const MAIL_SUMMARY_SELECT_COLS: &str = "
    m.id, m.conversation_id, m.folder_id, m.subject, m.from_json,
    m.received_at, m.sent_at, m.is_read, m.is_draft, m.flagged,
    m.importance, m.has_attachments, m.preview, m.web_link
";

const FOLDER_FILTER_PREDICATE: &str = "
    (
        m.folder_id = ?1
        OR m.folder_id IN (
            SELECT id FROM mail_folders
            WHERE LOWER(COALESCE(well_known_name, '')) = LOWER(?1)
               OR LOWER(display_name) = LOWER(?1)
        )
    )
";

pub fn upsert_mail_folders(conn: &mut Connection, folders: &[MailFolder]) -> Result<()> {
    if folders.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO mail_folders (
                id, display_name, well_known_name, parent_folder_id,
                unread_item_count, total_item_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                display_name = excluded.display_name,
                well_known_name = COALESCE(excluded.well_known_name, mail_folders.well_known_name),
                parent_folder_id = excluded.parent_folder_id,
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
                f.unread_item_count,
                f.total_item_count,
            ])?;
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
            unread_item_count, total_item_count
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
            unread_item_count: row.get(4)?,
            total_item_count: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

struct ExistingMailRow {
    folder_id: Option<String>,
    subject: String,
    from_display: String,
    received_at: String,
    is_read: i32,
    is_draft: i32,
    flagged: i32,
    importance: String,
    has_attachments: i32,
    preview: String,
    body_html: String,
    body_plain: String,
    attachments_json: String,
    conversation_id: Option<String>,
    from_json: Option<String>,
    to_json: String,
    cc_json: String,
    bcc_json: String,
    sent_at: Option<String>,
    web_link: Option<String>,
}

pub fn upsert_mail_messages(conn: &mut Connection, messages: &[MailMessage]) -> Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    {
        let mut check_stmt = tx.prepare_cached(
            "
            SELECT
                folder_id, subject, from_display, received_at,
                is_read, is_draft, flagged, importance,
                has_attachments, preview, body_html, body_plain, attachments_json,
                conversation_id, from_json, to_json, cc_json, bcc_json, sent_at, web_link
            FROM mail_messages
            WHERE id = ?1
            ",
        )?;

        let mut msg_stmt = tx.prepare_cached(
            "
            INSERT INTO mail_messages (
                id, conversation_id, folder_id, subject, from_json,
                to_json, cc_json, bcc_json, from_display, received_at,
                sent_at, is_read, is_draft, flagged, importance,
                has_attachments, preview, body_html, body_plain, attachments_json, web_link
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21
            )
            ON CONFLICT(id) DO UPDATE SET
                conversation_id = COALESCE(excluded.conversation_id, mail_messages.conversation_id),
                folder_id = COALESCE(excluded.folder_id, mail_messages.folder_id),
                subject = excluded.subject,
                from_json = COALESCE(excluded.from_json, mail_messages.from_json),
                to_json = excluded.to_json,
                cc_json = excluded.cc_json,
                bcc_json = excluded.bcc_json,
                from_display = excluded.from_display,
                received_at = excluded.received_at,
                sent_at = COALESCE(excluded.sent_at, mail_messages.sent_at),
                is_read = excluded.is_read,
                is_draft = excluded.is_draft,
                flagged = excluded.flagged,
                importance = excluded.importance,
                has_attachments = CASE
                    WHEN excluded.has_attachments != 0
                        OR (excluded.attachments_json = '[]'
                            AND mail_messages.attachments_json != '[]'
                            AND mail_messages.attachments_json != '')
                    THEN 1
                    ELSE 0
                END,
                preview = CASE
                    WHEN excluded.preview = '' AND mail_messages.preview != ''
                    THEN mail_messages.preview
                    ELSE excluded.preview
                END,
                body_html = CASE
                    WHEN excluded.body_html = '' AND mail_messages.body_html != ''
                    THEN mail_messages.body_html
                    ELSE excluded.body_html
                END,
                body_plain = CASE
                    WHEN excluded.body_html = ''
                        AND mail_messages.body_html != ''
                        AND mail_messages.body_plain != ''
                    THEN mail_messages.body_plain
                    ELSE excluded.body_plain
                END,
                attachments_json = CASE
                    WHEN excluded.attachments_json = '[]'
                        AND mail_messages.attachments_json != '[]'
                        AND mail_messages.attachments_json != ''
                    THEN mail_messages.attachments_json
                    ELSE excluded.attachments_json
                END,
                web_link = COALESCE(excluded.web_link, mail_messages.web_link)
            ",
        )?;

        let mut fts_del_stmt = tx.prepare_cached("DELETE FROM mail_fts WHERE msg_id = ?1")?;
        let mut fts_ins_stmt = tx.prepare_cached(
            "
            INSERT INTO mail_fts (msg_id, folder_id, subject, from_display, body_plain)
            SELECT id, COALESCE(folder_id, ''), subject, from_display, body_plain
            FROM mail_messages
            WHERE id = ?1
            ",
        )?;

        for msg in messages {
            let from_json = msg
                .from
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let to_json = serde_json::to_string(&msg.to_recipients)?;
            let cc_json = serde_json::to_string(&msg.cc_recipients)?;
            let bcc_json = serde_json::to_string(&msg.bcc_recipients)?;
            let attachments_json = serde_json::to_string(&msg.attachments)?;
            let from_display = msg.from_display();
            let is_read_i = bool_to_i32(msg.is_read);
            let is_draft_i = bool_to_i32(msg.is_draft);
            let flagged_i = bool_to_i32(msg.flagged);
            let has_att_i = bool_to_i32(msg.has_attachments);

            let existing = check_stmt
                .query_row(params![&msg.id], |row| {
                    Ok(ExistingMailRow {
                        folder_id: row.get(0)?,
                        subject: row.get(1)?,
                        from_display: row.get(2)?,
                        received_at: row.get(3)?,
                        is_read: row.get(4)?,
                        is_draft: row.get(5)?,
                        flagged: row.get(6)?,
                        importance: row.get(7)?,
                        has_attachments: row.get(8)?,
                        preview: row.get(9)?,
                        body_html: row.get(10)?,
                        body_plain: row.get(11)?,
                        attachments_json: row.get(12)?,
                        conversation_id: row.get(13)?,
                        from_json: row.get(14)?,
                        to_json: row.get(15)?,
                        cc_json: row.get(16)?,
                        bcc_json: row.get(17)?,
                        sent_at: row.get(18)?,
                        web_link: row.get(19)?,
                    })
                })
                .optional()?;

            let fts_unchanged = if let Some(ref ex) = existing {
                let eff_folder_id = msg.folder_id.as_deref().or(ex.folder_id.as_deref());
                let eff_conversation_id = msg
                    .conversation_id
                    .as_deref()
                    .or(ex.conversation_id.as_deref());
                let eff_from_json = from_json.as_deref().or(ex.from_json.as_deref());
                let eff_sent_at = msg.sent_at.as_deref().or(ex.sent_at.as_deref());
                let eff_web_link = msg.web_link.as_deref().or(ex.web_link.as_deref());
                let keep_existing_att = attachments_json == "[]"
                    && ex.attachments_json != "[]"
                    && !ex.attachments_json.is_empty();
                let eff_attachments_json = if keep_existing_att {
                    ex.attachments_json.as_str()
                } else {
                    attachments_json.as_str()
                };
                let eff_has_attachments = if msg.has_attachments || keep_existing_att {
                    1i32
                } else {
                    0i32
                };
                let eff_preview = if msg.preview.is_empty() && !ex.preview.is_empty() {
                    ex.preview.as_str()
                } else {
                    msg.preview.as_str()
                };
                let eff_body_html = if msg.body_html.is_empty() && !ex.body_html.is_empty() {
                    ex.body_html.as_str()
                } else {
                    msg.body_html.as_str()
                };
                let eff_body_plain = if msg.body_html.is_empty()
                    && !ex.body_html.is_empty()
                    && !ex.body_plain.is_empty()
                {
                    ex.body_plain.as_str()
                } else {
                    msg.body_plain.as_str()
                };

                let row_unchanged = eff_folder_id == ex.folder_id.as_deref()
                    && msg.subject == ex.subject
                    && from_display == ex.from_display
                    && msg.received_at == ex.received_at
                    && is_read_i == ex.is_read
                    && is_draft_i == ex.is_draft
                    && flagged_i == ex.flagged
                    && msg.importance == ex.importance
                    && eff_has_attachments == ex.has_attachments
                    && eff_preview == ex.preview
                    && eff_body_html == ex.body_html
                    && eff_body_plain == ex.body_plain
                    && eff_attachments_json == ex.attachments_json
                    && eff_conversation_id == ex.conversation_id.as_deref()
                    && eff_from_json == ex.from_json.as_deref()
                    && to_json == ex.to_json
                    && cc_json == ex.cc_json
                    && bcc_json == ex.bcc_json
                    && eff_sent_at == ex.sent_at.as_deref()
                    && eff_web_link == ex.web_link.as_deref();
                if row_unchanged {
                    continue;
                }

                eff_folder_id.unwrap_or("") == ex.folder_id.as_deref().unwrap_or("")
                    && msg.subject == ex.subject
                    && from_display == ex.from_display
                    && eff_body_plain == ex.body_plain
            } else {
                false
            };

            msg_stmt.execute(params![
                msg.id,
                msg.conversation_id,
                msg.folder_id,
                msg.subject,
                from_json,
                to_json,
                cc_json,
                bcc_json,
                from_display,
                msg.received_at,
                msg.sent_at,
                is_read_i,
                is_draft_i,
                flagged_i,
                msg.importance,
                has_att_i,
                msg.preview,
                msg.body_html,
                msg.body_plain,
                attachments_json,
                msg.web_link,
            ])?;

            if !fts_unchanged {
                fts_del_stmt.execute(params![msg.id])?;
                fts_ins_stmt.execute(params![msg.id])?;
            }
        }
    }
    tx.commit()?;
    Ok(messages.len())
}

fn query_mail_rows<F>(
    conn: &Connection,
    select_cols: &str,
    folder_id_or_wellknown: Option<&str>,
    limit: usize,
    mapper: F,
) -> Result<Vec<MailMessage>>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<MailMessage>,
{
    if limit == 0 {
        return Ok(Vec::new());
    }
    let filter = folder_id_or_wellknown
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(folder) = filter {
        let sql = format!(
            "
            SELECT {select_cols}
            FROM mail_messages m
            WHERE {FOLDER_FILTER_PREDICATE}
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![folder, limit as i64], mapper)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let sql = format!(
            "
            SELECT {select_cols}
            FROM mail_messages m
            ORDER BY m.received_at DESC, m.id DESC
            LIMIT ?1
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![limit as i64], mapper)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

pub fn list_mail_messages(
    conn: &Connection,
    folder_id_or_wellknown: Option<&str>,
    limit: usize,
) -> Result<Vec<MailMessage>> {
    query_mail_rows(
        conn,
        MAIL_SELECT_COLS,
        folder_id_or_wellknown,
        limit,
        map_mail_message_row,
    )
}

/// Lightweight header-only mail list projection (skips large body/recipient/attachment columns).
pub fn list_mail_summaries(
    conn: &Connection,
    folder_id_or_wellknown: Option<&str>,
    limit: usize,
) -> Result<Vec<MailMessage>> {
    query_mail_rows(
        conn,
        MAIL_SUMMARY_SELECT_COLS,
        folder_id_or_wellknown,
        limit,
        map_mail_summary_row,
    )
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
    let alias = match trimmed.to_ascii_lowercase().as_str() {
        "trash" => "deleteditems",
        "sent" => "sentitems",
        "junk" => "junkemail",
        _ => trimmed,
    };

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

        let mut sel = tx.prepare_cached("SELECT folder_id, is_read FROM mail_messages WHERE id = ?1")?;
        let existing: Option<(Option<String>, i32)> = sel
            .query_row(params![message_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;

        if let Some((old_folder_id, is_read_i)) = existing {
            if old_folder_id.as_deref() != Some(resolved_target.as_str()) {
                let mut upd_msg =
                    tx.prepare_cached("UPDATE mail_messages SET folder_id = ?2 WHERE id = ?1")?;
                upd_msg.execute(params![message_id, resolved_target])?;

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

pub fn delete_mail_messages_by_ids(conn: &mut Connection, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut del_msg = tx.prepare_cached("DELETE FROM mail_messages WHERE id = ?1")?;
        let mut del_fts = tx.prepare_cached("DELETE FROM mail_fts WHERE msg_id = ?1")?;
        for id in ids {
            del_msg.execute(params![id])?;
            del_fts.execute(params![id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn search_mail_messages(
    conn: &Connection,
    query: &str,
    folder_filter: Option<&str>,
    limit: usize,
    trigram_enabled: bool,
) -> Result<Vec<MailSearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let folder = folder_filter.map(str::trim).filter(|s| !s.is_empty());

    if trigram_enabled && trimmed.chars().count() >= 3 {
        if let Some(fts_query) = build_fts_query(trimmed) {
            if let Ok(hits) = run_mail_fts_search(conn, &fts_query, trimmed, folder, limit) {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
    }

    run_mail_like_search(conn, trimmed, folder, limit)
}

pub fn upsert_calendar_events(conn: &mut Connection, events: &[CalendarEvent]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO calendar_events (
                id, subject, start_iso, end_iso, time_zone, is_all_day,
                location, organizer_name, organizer_email, attendees_json,
                response_status, is_online_meeting, online_meeting_url,
                body_html, body_plain, is_cancelled, web_link
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, ?15, ?16, ?17
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
                web_link = COALESCE(excluded.web_link, calendar_events.web_link)
            ",
        )?;
        for ev in events {
            let attendees_json = serde_json::to_string(&ev.attendees)?;
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
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
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
            let mut stmt = conn.prepare_cached(
                "
                SELECT
                    id, subject, start_iso, end_iso, time_zone, is_all_day,
                    location, organizer_name, organizer_email, attendees_json,
                    response_status, is_online_meeting, online_meeting_url,
                    body_html, body_plain, is_cancelled, web_link
                FROM calendar_events
                WHERE (end_iso >= ?1 OR start_iso >= ?1)
                  AND start_iso <= ?2
                ORDER BY start_iso ASC, id ASC
                LIMIT ?3
                ",
            )?;
            let rows = stmt.query_map(params![s, e, limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        (Some(s), None) => {
            let mut stmt = conn.prepare_cached(
                "
                SELECT
                    id, subject, start_iso, end_iso, time_zone, is_all_day,
                    location, organizer_name, organizer_email, attendees_json,
                    response_status, is_online_meeting, online_meeting_url,
                    body_html, body_plain, is_cancelled, web_link
                FROM calendar_events
                WHERE end_iso >= ?1 OR start_iso >= ?1
                ORDER BY start_iso ASC, id ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![s, limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        (None, Some(e)) => {
            let mut stmt = conn.prepare_cached(
                "
                SELECT
                    id, subject, start_iso, end_iso, time_zone, is_all_day,
                    location, organizer_name, organizer_email, attendees_json,
                    response_status, is_online_meeting, online_meeting_url,
                    body_html, body_plain, is_cancelled, web_link
                FROM calendar_events
                WHERE start_iso <= ?1
                ORDER BY start_iso ASC, id ASC
                LIMIT ?2
                ",
            )?;
            let rows = stmt.query_map(params![e, limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        (None, None) => {
            let mut stmt = conn.prepare_cached(
                "
                SELECT
                    id, subject, start_iso, end_iso, time_zone, is_all_day,
                    location, organizer_name, organizer_email, attendees_json,
                    response_status, is_online_meeting, online_meeting_url,
                    body_html, body_plain, is_cancelled, web_link
                FROM calendar_events
                ORDER BY start_iso ASC, id ASC
                LIMIT ?1
                ",
            )?;
            let rows = stmt.query_map(params![limit as i64], map_calendar_event_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
    }
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
    {
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
    {
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
    {
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
    }
    tx.commit()?;
    Ok(())
}

pub fn list_todo_tasks(
    conn: &Connection,
    list_id: &str,
    include_completed: bool,
    limit: usize,
) -> Result<Vec<TodoTask>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let status_clause = if include_completed {
        ""
    } else {
        "AND LOWER(status) != 'completed'"
    };
    let sql = format!(
        "
        SELECT
            id, list_id, title, status, importance, body_plain,
            due_date_iso, reminder_iso, completed_at, created_at, updated_at
        FROM todo_tasks
        WHERE list_id = ?1
          {status_clause}
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
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![list_id, limit as i64], map_todo_task_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn delete_todo_task(conn: &Connection, task_id: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM todo_tasks WHERE id = ?1")?;
    stmt.execute(params![task_id])?;
    Ok(())
}

fn run_mail_fts_search(
    conn: &Connection,
    fts_query: &str,
    raw_query: &str,
    folder_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<MailSearchHit>> {
    let cols = "m.id, m.folder_id, m.subject, m.from_display, m.received_at, m.body_plain, m.preview, m.has_attachments, m.is_read";
    if let Some(folder) = folder_filter {
        let sql = format!(
            "
            SELECT {cols}
            FROM mail_fts f
            JOIN mail_messages m ON m.id = f.msg_id
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
            SELECT {cols}
            FROM mail_fts f
            JOIN mail_messages m ON m.id = f.msg_id
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
    let cols = "m.id, m.folder_id, m.subject, m.from_display, m.received_at, m.body_plain, m.preview, m.has_attachments, m.is_read";
    let pattern = format!("%{}%", escape_like(&raw_query.to_lowercase()));
    if let Some(folder) = folder_filter {
        let sql = format!(
            "
            SELECT {cols}
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
            SELECT {cols}
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

fn map_mail_message_row(row: &Row<'_>) -> rusqlite::Result<MailMessage> {
    let from_json: Option<String> = row.get(4)?;
    let to_json: String = row.get(5)?;
    let cc_json: String = row.get(6)?;
    let bcc_json: String = row.get(7)?;
    let is_read_int: i32 = row.get(10)?;
    let is_draft_int: i32 = row.get(11)?;
    let flagged_int: i32 = row.get(12)?;
    let has_att_int: i32 = row.get(14)?;
    let attachments_json: String = row.get(18)?;

    let from: Option<EmailRecipient> = from_json.and_then(|s| serde_json::from_str(&s).ok());
    let to_recipients: Vec<EmailRecipient> = serde_json::from_str(&to_json).unwrap_or_default();
    let cc_recipients: Vec<EmailRecipient> = serde_json::from_str(&cc_json).unwrap_or_default();
    let bcc_recipients: Vec<EmailRecipient> = serde_json::from_str(&bcc_json).unwrap_or_default();
    let attachments: Vec<MailAttachment> =
        serde_json::from_str(&attachments_json).unwrap_or_default();

    Ok(MailMessage {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        folder_id: row.get(2)?,
        subject: row.get(3)?,
        from,
        to_recipients,
        cc_recipients,
        bcc_recipients,
        received_at: row.get(8)?,
        sent_at: row.get(9)?,
        is_read: is_read_int != 0,
        is_draft: is_draft_int != 0,
        flagged: flagged_int != 0,
        importance: row.get(13)?,
        has_attachments: has_att_int != 0,
        preview: row.get(15)?,
        body_html: row.get(16)?,
        body_plain: row.get(17)?,
        attachments,
        web_link: row.get(19)?,
    })
}

fn map_mail_summary_row(row: &Row<'_>) -> rusqlite::Result<MailMessage> {
    let from_json: Option<String> = row.get(4)?;
    let is_read_int: i32 = row.get(7)?;
    let is_draft_int: i32 = row.get(8)?;
    let flagged_int: i32 = row.get(9)?;
    let has_att_int: i32 = row.get(11)?;

    let from: Option<EmailRecipient> = from_json.and_then(|s| serde_json::from_str(&s).ok());

    Ok(MailMessage {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        folder_id: row.get(2)?,
        subject: row.get(3)?,
        from,
        to_recipients: Vec::new(),
        cc_recipients: Vec::new(),
        bcc_recipients: Vec::new(),
        received_at: row.get(5)?,
        sent_at: row.get(6)?,
        is_read: is_read_int != 0,
        is_draft: is_draft_int != 0,
        flagged: flagged_int != 0,
        importance: row.get(10)?,
        has_attachments: has_att_int != 0,
        preview: row.get(12)?,
        body_html: String::new(),
        body_plain: String::new(),
        attachments: Vec::new(),
        web_link: row.get(13)?,
    })
}

fn map_mail_search_hit_row(row: &Row<'_>, raw_query: &str) -> rusqlite::Result<MailSearchHit> {
    let id: String = row.get(0)?;
    let folder_id: Option<String> = row.get(1)?;
    let subject: String = row.get(2)?;
    let from_display: String = row.get(3)?;
    let received_at: String = row.get(4)?;
    let body_plain: String = row.get(5)?;
    let preview: String = row.get(6)?;
    let has_att_int: i32 = row.get(7)?;
    let is_read_int: i32 = row.get(8)?;

    let body_source = if body_plain.trim().is_empty() {
        preview.as_str()
    } else {
        body_plain.as_str()
    };
    let snippet = build_mail_snippet(&subject, body_source, &from_display, raw_query, 120);

    Ok(MailSearchHit {
        id,
        folder_id,
        subject,
        from_display,
        received_at,
        snippet,
        has_attachments: has_att_int != 0,
        is_read: is_read_int != 0,
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

fn map_calendar_event_row(row: &Row<'_>) -> rusqlite::Result<CalendarEvent> {
    let is_all_day_int: i32 = row.get(5)?;
    let attendees_json: String = row.get(9)?;
    let is_online_int: i32 = row.get(11)?;
    let is_cancelled_int: i32 = row.get(15)?;
    let attendees: Vec<EventAttendee> = serde_json::from_str(&attendees_json).unwrap_or_default();

    Ok(CalendarEvent {
        id: row.get(0)?,
        subject: row.get(1)?,
        start_iso: row.get(2)?,
        end_iso: row.get(3)?,
        time_zone: row.get(4)?,
        is_all_day: is_all_day_int != 0,
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
    if v { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    fn sample_mail(
        id: &str,
        folder_id: &str,
        subject: &str,
        from_name: &str,
        from_addr: &str,
        received_at: &str,
        body_html: &str,
        body_plain: &str,
    ) -> MailMessage {
        MailMessage {
            id: id.to_string(),
            conversation_id: Some(format!("conv-{id}")),
            folder_id: Some(folder_id.to_string()),
            subject: subject.to_string(),
            from: Some(EmailRecipient {
                name: Some(from_name.to_string()),
                address: from_addr.to_string(),
            }),
            to_recipients: vec![EmailRecipient {
                name: Some("나".to_string()),
                address: "me@example.com".to_string(),
            }],
            cc_recipients: Vec::new(),
            bcc_recipients: Vec::new(),
            received_at: received_at.to_string(),
            sent_at: Some(received_at.to_string()),
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".to_string(),
            has_attachments: false,
            preview: body_plain.chars().take(60).collect(),
            body_html: body_html.to_string(),
            body_plain: body_plain.to_string(),
            attachments: Vec::new(),
            web_link: Some(format!("https://outlook.office.com/mail/{id}")),
        }
    }

    #[test]
    fn test_mail_folders_messages_upsert_preservation_and_delete() {
        let storage = Storage::open_in_memory().unwrap();
        let folders = vec![
            MailFolder {
                id: "fld-inbox".to_string(),
                display_name: "받은 편지함".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                unread_item_count: 2,
                total_item_count: 10,
            },
            MailFolder {
                id: "fld-sent".to_string(),
                display_name: "보낸 편지함".to_string(),
                well_known_name: Some("sentitems".to_string()),
                parent_folder_id: None,
                unread_item_count: 0,
                total_item_count: 5,
            },
        ];
        storage.upsert_mail_folders(&folders).unwrap();
        let listed_folders = storage.list_mail_folders().unwrap();
        assert_eq!(listed_folders.len(), 2);
        assert_eq!(listed_folders[0].id, "fld-inbox");

        let mut full_msg = sample_mail(
            "m1",
            "fld-inbox",
            "3분기 예산 검토 요청",
            "김영희",
            "younghee@example.com",
            "2026-11-15T10:00:00Z",
            "<p>첨부된 예산안을 확인해 주세요.</p>",
            "첨부된 예산안을 확인해 주세요.",
        );
        full_msg.has_attachments = true;
        full_msg.attachments = vec![MailAttachment {
            id: "att-1".to_string(),
            name: "budget.xlsx".to_string(),
            content_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                .to_string(),
            size: 12345,
            is_inline: false,
            content_id: None,
            content_bytes_b64: None,
        }];
        let sent_msg = sample_mail(
            "m2",
            "fld-sent",
            "배포 완료 보고",
            "나",
            "me@example.com",
            "2026-11-15T11:00:00Z",
            "<p>프로덕션 배포가 완료되었습니다.</p>",
            "프로덕션 배포가 완료되었습니다.",
        );
        storage.upsert_mail_messages(&[full_msg, sent_msg]).unwrap();

        // Well-known alias & display_name resolution
        let inbox_msgs = storage.list_mail_messages(Some("inbox"), 10).unwrap();
        assert_eq!(inbox_msgs.len(), 1);
        assert_eq!(inbox_msgs[0].id, "m1");

        let sent_msgs = storage.list_mail_messages(Some("보낸 편지함"), 10).unwrap();
        assert_eq!(sent_msgs.len(), 1);
        assert_eq!(sent_msgs[0].id, "m2");

        let all_msgs = storage.list_mail_messages(None, 10).unwrap();
        assert_eq!(all_msgs.len(), 2);
        assert_eq!(all_msgs[0].id, "m2");

        // Summary list item with empty attachments & empty body_html must preserve expanded details
        let mut summary_update = sample_mail(
            "m1",
            "fld-inbox",
            "3분기 예산 검토 요청",
            "김영희",
            "younghee@example.com",
            "2026-11-15T10:00:00Z",
            "",
            "첨부된 예산안...",
        );
        summary_update.is_read = true;
        summary_update.has_attachments = true;
        storage.upsert_mail_messages(&[summary_update]).unwrap();

        let loaded_m1 = storage.get_mail_message("m1").unwrap().unwrap();
        assert!(loaded_m1.is_read);
        assert_eq!(loaded_m1.attachments.len(), 1);
        assert_eq!(loaded_m1.attachments[0].name, "budget.xlsx");
        assert_eq!(loaded_m1.body_html, "<p>첨부된 예산안을 확인해 주세요.</p>");
        assert_eq!(loaded_m1.body_plain, "첨부된 예산안을 확인해 주세요.");

        // Delete m2 and confirm removed from both table and search
        storage
            .delete_mail_messages_by_ids(&["m2".to_string()])
            .unwrap();
        assert!(storage.get_mail_message("m2").unwrap().is_none());
        assert!(storage.search_mail_messages("배포", None, 10).unwrap().is_empty());
    }

    #[test]
    fn test_mail_hybrid_search_trigram_and_two_char_korean_like() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[
                MailFolder {
                    id: "fld-inbox".to_string(),
                    display_name: "Inbox".to_string(),
                    well_known_name: Some("inbox".to_string()),
                    parent_folder_id: None,
                    unread_item_count: 1,
                    total_item_count: 1,
                },
                MailFolder {
                    id: "fld-sent".to_string(),
                    display_name: "Sent Items".to_string(),
                    well_known_name: Some("sentitems".to_string()),
                    parent_folder_id: None,
                    unread_item_count: 0,
                    total_item_count: 1,
                },
            ])
            .unwrap();

        let m1 = sample_mail(
            "m1",
            "fld-inbox",
            "2026년 예산 승인",
            "박지성",
            "park@example.com",
            "2026-11-15T09:00:00Z",
            "<p>Kubernetes 클러스터 비용과 관련한 예산 자료입니다.</p>",
            "Kubernetes 클러스터 비용과 관련한 예산 자료입니다.",
        );
        let m2 = sample_mail(
            "m2",
            "fld-sent",
            "정기 배포 안내",
            "이순신",
            "lee@example.com",
            "2026-11-15T10:00:00Z",
            "<p>오늘 밤 10시 v0.2.0 배포 예정입니다.</p>",
            "오늘 밤 10시 v0.2.0 배포 예정입니다.",
        );
        storage.upsert_mail_messages(&[m1, m2]).unwrap();

        // 2-char Korean LIKE fallback
        let budget_hits = storage.search_mail_messages("예산", None, 10).unwrap();
        assert_eq!(budget_hits.len(), 1);
        assert_eq!(budget_hits[0].id, "m1");

        let deploy_hits = storage
            .search_mail_messages("배포", Some("sentitems"), 10)
            .unwrap();
        assert_eq!(deploy_hits.len(), 1);
        assert_eq!(deploy_hits[0].id, "m2");

        // >=3 char trigram FTS5
        let fts_hits = storage
            .search_mail_messages("클러스터", Some("inbox"), 10)
            .unwrap();
        assert_eq!(fts_hits.len(), 1);
        assert_eq!(fts_hits[0].id, "m1");

        // Punctuation fallback via LIKE when FTS returns 0 or tokenizes differently
        let punct_hits = storage.search_mail_messages("v0.2.0", None, 10).unwrap();
        assert_eq!(punct_hits.len(), 1);
        assert_eq!(punct_hits[0].id, "m2");
    }

    #[test]
    fn test_calendar_contacts_and_todo_crud() {
        let storage = Storage::open_in_memory().unwrap();

        // Calendar
        let ev1 = CalendarEvent {
            id: "ev-1".to_string(),
            subject: "주간 스탠드업".to_string(),
            start_iso: "2026-11-15T09:00:00Z".to_string(),
            end_iso: "2026-11-15T09:30:00Z".to_string(),
            time_zone: "Asia/Seoul".to_string(),
            is_all_day: false,
            location: Some("회의실 A".to_string()),
            organizer_name: Some("홍길동".to_string()),
            organizer_email: Some("hong@example.com".to_string()),
            attendees: vec![EventAttendee {
                name: Some("김철수".to_string()),
                address: "kim@example.com".to_string(),
                attendee_type: "required".to_string(),
                response_status: "accepted".to_string(),
            }],
            response_status: "accepted".to_string(),
            is_online_meeting: true,
            online_meeting_url: Some("https://teams.microsoft.com/l/meetup-join/1".to_string()),
            body_html: "<p>안건 공유</p>".to_string(),
            body_plain: "안건 공유".to_string(),
            is_cancelled: false,
            web_link: None,
        };
        let ev2 = CalendarEvent {
            id: "ev-2".to_string(),
            subject: "다음 달 계획".to_string(),
            start_iso: "2026-12-01T10:00:00Z".to_string(),
            end_iso: "2026-12-01T11:00:00Z".to_string(),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: Vec::new(),
            response_status: "organizer".to_string(),
            is_online_meeting: false,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        };
        storage.upsert_calendar_events(&[ev2, ev1]).unwrap();
        let nov_events = storage
            .list_calendar_events(
                Some("2026-11-01T00:00:00Z"),
                Some("2026-11-30T23:59:59Z"),
                10,
            )
            .unwrap();
        assert_eq!(nov_events.len(), 1);
        assert_eq!(nov_events[0].id, "ev-1");
        storage.delete_calendar_event("ev-1").unwrap();
        assert_eq!(storage.list_calendar_events(None, None, 10).unwrap().len(), 1);

        // Contacts
        let c1 = ContactItem {
            id: "c1".to_string(),
            display_name: "홍길동".to_string(),
            given_name: Some("길동".to_string()),
            surname: Some("홍".to_string()),
            email_addresses: vec!["hong@example.com".to_string()],
            mobile_phone: Some("010-1234-5678".to_string()),
            business_phones: vec!["02-111-2222".to_string()],
            company_name: Some("엔지니어링".to_string()),
            job_title: Some("테크 리드".to_string()),
            department: Some("플랫폼팀".to_string()),
            notes: None,
            source: "contact".to_string(),
        };
        let c2 = ContactItem {
            id: "c2".to_string(),
            display_name: "Alice Kim".to_string(),
            given_name: Some("Alice".to_string()),
            surname: Some("Kim".to_string()),
            email_addresses: vec!["alice@corp.com".to_string()],
            mobile_phone: None,
            business_phones: Vec::new(),
            company_name: Some("Corp".to_string()),
            job_title: None,
            department: None,
            notes: None,
            source: "directory".to_string(),
        };
        storage.upsert_contacts(&[c1, c2]).unwrap();
        let all_contacts = storage.list_contacts(None, 10).unwrap();
        assert_eq!(all_contacts.len(), 2);
        assert_eq!(all_contacts[0].id, "c2"); // Alice before 홍길동
        let filtered = storage.list_contacts(Some("플랫폼"), 10).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "c1");
        storage.delete_contact("c2").unwrap();
        assert_eq!(storage.list_contacts(None, 10).unwrap().len(), 1);

        // To Do
        let l1 = TodoList {
            id: "list-1".to_string(),
            display_name: "작업".to_string(),
            wellknown_list_name: Some("defaultList".to_string()),
            is_owner: true,
            is_shared: false,
        };
        storage.upsert_todo_lists(&[l1]).unwrap();
        assert_eq!(storage.list_todo_lists().unwrap().len(), 1);

        let t1 = TodoTask {
            id: "t1".to_string(),
            list_id: "list-1".to_string(),
            title: "문서 작성".to_string(),
            status: "notStarted".to_string(),
            importance: "high".to_string(),
            body_plain: Some("API 명세 정리".to_string()),
            due_date_iso: Some("2026-11-16T00:00:00Z".to_string()),
            reminder_iso: None,
            completed_at: None,
            created_at: Some("2026-11-15T08:00:00Z".to_string()),
            updated_at: Some("2026-11-15T08:00:00Z".to_string()),
        };
        let t2 = TodoTask {
            id: "t2".to_string(),
            list_id: "list-1".to_string(),
            title: "완료된 작업".to_string(),
            status: "completed".to_string(),
            importance: "normal".to_string(),
            body_plain: None,
            due_date_iso: None,
            reminder_iso: None,
            completed_at: Some("2026-11-15T09:00:00Z".to_string()),
            created_at: Some("2026-11-14T08:00:00Z".to_string()),
            updated_at: Some("2026-11-15T09:00:00Z".to_string()),
        };
        storage.upsert_todo_tasks(&[t1, t2]).unwrap();
        let active_only = storage.list_todo_tasks("list-1", false, 10).unwrap();
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].id, "t1");
        let with_done = storage.list_todo_tasks("list-1", true, 10).unwrap();
        assert_eq!(with_done.len(), 2);
        assert_eq!(with_done[0].id, "t1");
        storage.delete_todo_task("t1").unwrap();
        assert_eq!(storage.list_todo_tasks("list-1", true, 10).unwrap().len(), 1);
    }

    #[test]
    fn test_mail_upsert_idempotency_and_fts_skip() {
        let storage = Storage::open_in_memory().unwrap();
        let mut msg = sample_mail(
            "m-idem",
            "fld-inbox",
            "초기 메일 제목",
            "홍길동",
            "hong@example.com",
            "2026-11-15T10:00:00Z",
            "<p>Kubernetes 장애 대응</p>",
            "Kubernetes 장애 대응",
        );
        assert_eq!(storage.upsert_mail_messages(&[msg.clone()]).unwrap(), 1);
        assert_eq!(
            storage
                .search_mail_messages("Kubernetes", None, 10)
                .unwrap()
                .len(),
            1
        );

        // Identical re-upsert performs 0 SQLite row modifications.
        let changes_before_noop = storage.conn().lock().unwrap().total_changes();
        assert_eq!(storage.upsert_mail_messages(&[msg.clone()]).unwrap(), 1);
        let changes_after_noop = storage.conn().lock().unwrap().total_changes();
        assert_eq!(changes_after_noop - changes_before_noop, 0);
        assert_eq!(
            storage
                .search_mail_messages("Kubernetes", None, 10)
                .unwrap()
                .len(),
            1
        );

        // Read/flag toggle updates mail_messages (1 change) without touching mail_fts.
        msg.is_read = true;
        msg.flagged = true;
        let changes_before_flag = storage.conn().lock().unwrap().total_changes();
        assert_eq!(storage.upsert_mail_messages(&[msg.clone()]).unwrap(), 1);
        let changes_after_flag = storage.conn().lock().unwrap().total_changes();
        assert_eq!(changes_after_flag - changes_before_flag, 1);
        let loaded = storage.get_mail_message("m-idem").unwrap().unwrap();
        assert!(loaded.is_read && loaded.flagged);
        assert_eq!(
            storage
                .search_mail_messages("Kubernetes", None, 10)
                .unwrap()
                .len(),
            1
        );

        // Subject & body update refreshes mail_fts too.
        msg.subject = "수정된 메일 제목".to_string();
        msg.preview = "PostgreSQL 점검 보고".to_string();
        msg.body_plain = "PostgreSQL 점검 보고".to_string();
        msg.body_html = "<p>PostgreSQL 점검 보고</p>".to_string();
        storage.upsert_mail_messages(&[msg]).unwrap();
        assert!(storage
            .search_mail_messages("Kubernetes", None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            storage
                .search_mail_messages("PostgreSQL", None, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_list_mail_summaries_header_projection_and_body_preservation() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[MailFolder {
                id: "fld-inbox".to_string(),
                display_name: "Inbox".to_string(),
                well_known_name: Some("inbox".to_string()),
                parent_folder_id: None,
                unread_item_count: 1,
                total_item_count: 1,
            }])
            .unwrap();

        let mut full = sample_mail(
            "m-sum-1",
            "fld-inbox",
            "프로젝트 보고",
            "홍길동",
            "hong@example.com",
            "2026-11-18T10:00:00Z",
            "<p>본문 전체에 있는 SecretKeywordFullBody 내용</p>",
            "본문 전체에 있는 SecretKeywordFullBody 내용",
        );
        full.preview = "짧은 미리보기".to_string();
        full.has_attachments = true;
        full.attachments = vec![MailAttachment {
            id: "att-sum".to_string(),
            name: "spec.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 1024,
            is_inline: false,
            content_id: None,
            content_bytes_b64: None,
        }];
        storage.upsert_mail_messages(&[full]).unwrap();

        // list_mail_summaries must return lightweight headers without blobs
        let summaries = storage.list_mail_summaries(Some("inbox"), 10).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "m-sum-1");
        assert_eq!(summaries[0].subject, "프로젝트 보고");
        assert_eq!(summaries[0].preview, "짧은 미리보기");
        assert!(summaries[0].has_attachments);
        assert!(summaries[0].body_html.is_empty());
        assert!(summaries[0].body_plain.is_empty());
        assert!(summaries[0].to_recipients.is_empty());
        assert!(summaries[0].attachments.is_empty());

        // Header-only delta update arrives with empty body_html and body_plain == preview
        let mut header_update = sample_mail(
            "m-sum-1",
            "fld-inbox",
            "프로젝트 보고 (수정)",
            "홍길동",
            "hong@example.com",
            "2026-11-18T10:00:00Z",
            "",
            "짧은 미리보기",
        );
        header_update.preview = "짧은 미리보기".to_string();
        header_update.is_read = true;
        storage.upsert_mail_messages(&[header_update]).unwrap();

        // Full body_html, body_plain, attachments, and mail_fts full-body search must be preserved
        let full_loaded = storage.get_mail_message("m-sum-1").unwrap().unwrap();
        assert!(full_loaded.is_read);
        assert_eq!(
            full_loaded.body_html,
            "<p>본문 전체에 있는 SecretKeywordFullBody 내용</p>"
        );
        assert_eq!(
            full_loaded.body_plain,
            "본문 전체에 있는 SecretKeywordFullBody 내용"
        );
        assert_eq!(full_loaded.attachments.len(), 1);
        assert_eq!(
            storage
                .search_mail_messages("SecretKeywordFullBody", Some("inbox"), 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_optimistic_local_mail_mutations_and_calendar_batch_delete() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_mail_folders(&[
                MailFolder {
                    id: "fld-inbox".to_string(),
                    display_name: "Inbox".to_string(),
                    well_known_name: Some("inbox".to_string()),
                    parent_folder_id: None,
                    unread_item_count: 1,
                    total_item_count: 1,
                },
                MailFolder {
                    id: "fld-archive".to_string(),
                    display_name: "Archive".to_string(),
                    well_known_name: Some("archive".to_string()),
                    parent_folder_id: None,
                    unread_item_count: 0,
                    total_item_count: 0,
                },
            ])
            .unwrap();

        let msg = sample_mail(
            "m-opt-1",
            "fld-inbox",
            "Optimistic test",
            "Alice",
            "alice@example.com",
            "2026-11-18T12:00:00Z",
            "<p>hello</p>",
            "hello",
        );
        storage.upsert_mail_messages(&[msg]).unwrap();

        // Mark read: unread count 1 -> 0
        storage.set_mail_read_local("m-opt-1", true).unwrap();
        assert!(storage.get_mail_message("m-opt-1").unwrap().unwrap().is_read);
        let folders = storage.list_mail_folders().unwrap();
        let inbox = folders.iter().find(|f| f.id == "fld-inbox").unwrap();
        assert_eq!(inbox.unread_item_count, 0);

        // Idempotent mark read again does not underflow or decrement
        storage.set_mail_read_local("m-opt-1", true).unwrap();
        let folders = storage.list_mail_folders().unwrap();
        assert_eq!(
            folders.iter().find(|f| f.id == "fld-inbox").unwrap().unread_item_count,
            0
        );

        // Mark unread: 0 -> 1
        storage.set_mail_read_local("m-opt-1", false).unwrap();
        let folders = storage.list_mail_folders().unwrap();
        assert_eq!(
            folders.iter().find(|f| f.id == "fld-inbox").unwrap().unread_item_count,
            1
        );

        // Flag toggle
        storage.set_mail_flagged_local("m-opt-1", true).unwrap();
        assert!(storage.get_mail_message("m-opt-1").unwrap().unwrap().flagged);
        storage.set_mail_flagged_local("m-opt-1", false).unwrap();
        assert!(!storage.get_mail_message("m-opt-1").unwrap().unwrap().flagged);

        // Move folder by well-known name adjusts folder_id and counts
        storage.move_mail_folder_local("m-opt-1", "archive").unwrap();
        assert_eq!(
            storage.get_mail_message("m-opt-1").unwrap().unwrap().folder_id.as_deref(),
            Some("fld-archive")
        );
        let folders = storage.list_mail_folders().unwrap();
        let inbox = folders.iter().find(|f| f.id == "fld-inbox").unwrap();
        let archive = folders.iter().find(|f| f.id == "fld-archive").unwrap();
        assert_eq!(inbox.total_item_count, 0);
        assert_eq!(inbox.unread_item_count, 0);
        assert_eq!(archive.total_item_count, 1);
        assert_eq!(archive.unread_item_count, 1);

        // Calendar batch delete
        let ev1 = CalendarEvent {
            id: "ev-b1".to_string(),
            subject: "One".to_string(),
            start_iso: "2026-11-18T09:00:00Z".to_string(),
            end_iso: "2026-11-18T10:00:00Z".to_string(),
            time_zone: "UTC".to_string(),
            is_all_day: false,
            location: None,
            organizer_name: None,
            organizer_email: None,
            attendees: Vec::new(),
            response_status: "accepted".to_string(),
            is_online_meeting: false,
            online_meeting_url: None,
            body_html: String::new(),
            body_plain: String::new(),
            is_cancelled: false,
            web_link: None,
        };
        let mut ev2 = ev1.clone();
        ev2.id = "ev-b2".to_string();
        let mut ev3 = ev1.clone();
        ev3.id = "ev-b3".to_string();
        storage.upsert_calendar_events(&[ev1, ev2, ev3]).unwrap();
        storage
            .delete_calendar_events_by_ids(&["ev-b1".to_string(), "ev-b3".to_string()])
            .unwrap();
        let remaining = storage.list_calendar_events(None, None, 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "ev-b2");
    }
}
