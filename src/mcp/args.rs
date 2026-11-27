//! Typed MCP tool argument extractors and schema-aligned validation.

use serde_json::Value;

use crate::{cli, graph::calendar::CalendarEditTime};

use super::format_error;

pub fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    let args = Args::new(arguments);
    args.object()?;

    match name {
        "whoami" => Ok(()),
        "list_mail_folders" => {
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            let cursor = args.optional_str("cursor")?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            if cursor.is_some() && refresh {
                return Err("cursor cannot be combined with refresh".to_string());
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        "list_messages" => {
            args.only(
                name,
                &[
                    "folder", "limit", "cursor", "unread", "flagged", "focused", "compact",
                    "offline",
                ],
            )?;
            args.optional_str("folder")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_str("cursor")?;
            args.optional_bool("unread")?;
            args.optional_bool("flagged")?;
            args.optional_bool("focused")?;
            args.optional_bool("compact")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "get_message" => {
            args.only(name, &["message_id", "mark_read", "compact", "fields", "offline"])?;
            args.required_str("message_id")?;
            args.optional_bool("mark_read")?;
            args.optional_bool("offline")?;
            args.optional_bool("compact")?;
            args.optional_str_array("fields")?;
            Ok(())
        }
        "send_mail" => {
            let to = args.required_str_array("to")?;
            cli::check_recipient_addresses(&to).map_err(format_error)?;
            args.required_str("subject")?;
            args.required_text("body")?;
            if let Some(cc) = args.optional_str_array("cc")? {
                cli::check_recipient_addresses(&cc).map_err(format_error)?;
            }
            if let Some(bcc) = args.optional_str_array("bcc")? {
                cli::check_recipient_addresses(&bcc).map_err(format_error)?;
            }
            if let Some(imp) = args.optional_str("importance")? {
                cli::normalize_mail_importance(&imp).map_err(format_error)?;
            }
            args.optional_str_array("attachments")?;
            args.optional_bool("save_to_sent_items")?;
            Ok(())
        }
        "reply_mail" => {
            args.required_str("message_id")?;
            args.required_text("body")?;
            args.optional_bool("reply_all")?;
            Ok(())
        }
        "forward_mail" => {
            args.required_str("message_id")?;
            let to = args.required_str_array("to")?;
            cli::check_recipient_addresses(&to).map_err(format_error)?;
            args.optional_str("comment")?;
            Ok(())
        }
        "update_draft" => {
            args.only(name, &["message_id", "subject", "body", "body_html"])?;
            args.required_str("message_id")?;
            let subject = args.optional_text("subject")?;
            let body = args.optional_text("body")?;
            let html = args.optional_text("body_html")?;
            if body.is_some() && html.is_some() {
                return Err("Provide either body or body_html.".into());
            }
            if subject.is_none() && body.is_none() && html.is_none() {
                return Err("Provide a draft subject or body to update.".into());
            }
            Ok(())
        }
        "update_event" => {
            args.only(name, &["event_id", "subject", "start", "end"])?;
            args.required_str("event_id")?;
            let subject = args.optional_text("subject")?;
            let start = args
                .optional_text("start")?
                .map(CalendarEditTime::parse)
                .transpose()
                .map_err(format_error)?;
            let end = args
                .optional_text("end")?
                .map(CalendarEditTime::parse)
                .transpose()
                .map_err(format_error)?;
            if subject.is_none() && start.is_none() && end.is_none() {
                return Err("Provide an event subject, start, or end to update.".into());
            }
            Ok(())
        }
        "update_task" => {
            args.only(name, &["task_id", "list", "title", "due"])?;
            args.required_str("task_id")?;
            args.optional_str("list")?;
            let title = args.optional_text("title")?;
            let due = args
                .optional_text("due")?
                .map(cli::normalize_todo_edit_due)
                .transpose()
                .map_err(format_error)?;
            if title.is_some_and(|title| title.trim().is_empty()) {
                return Err("Task title must not be empty.".into());
            }
            if title.is_none() && due.is_none() {
                return Err("Provide a task title or due date to update.".into());
            }
            Ok(())
        }
        "update_message" => {
            args.only(
                name,
                &[
                    "message_id",
                    "is_read",
                    "flagged",
                    "destination_folder",
                    "destination_folder_id",
                    "move_to_folder",
                    "compact",
                ],
            )?;
            args.required_str("message_id")?;
            let is_read = args.optional_bool("is_read")?;
            let flagged = args.optional_bool("flagged")?;
            args.optional_bool("compact")?;
            let dest = args
                .optional_str("destination_folder")?
                .or(args.optional_str("destination_folder_id")?)
                .or(args.optional_str("move_to_folder")?);
            if is_read.is_none() && flagged.is_none() && dest.is_none() {
                return Err(
                    "at least one of is_read, flagged, or destination_folder must be provided"
                        .to_string(),
                );
            }
            Ok(())
        }
        "delete_message" => {
            args.required_str("message_id")?;
            args.required_true("confirm")?;
            Ok(())
        }
        "download_attachment" => {
            args.required_str("message_id")?;
            args.optional_usize("attachment_index")?;
            args.optional_str("attachment")?;
            args.optional_str("output_dir")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "search_mail" => {
            args.only(
                name,
                &["query", "folder", "limit", "cursor", "compact", "offline"],
            )?;
            args.required_str("query")?;
            args.optional_str("folder")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_str("cursor")?;
            args.optional_bool("compact")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "list_events" => {
            args.only(
                name,
                &[
                    "from", "start", "to", "end", "days", "limit", "compact", "offline",
                ],
            )?;
            if let Some(f) = args.optional_str("from")?.or(args.optional_str("start")?) {
                cli::normalize_datetime_iso(&f).map_err(format_error)?;
            }
            if let Some(t) = args.optional_str("to")?.or(args.optional_str("end")?) {
                cli::normalize_datetime_iso(&t).map_err(format_error)?;
            }
            if let Some(days) = args.optional_i64("days")? {
                let d = usize::try_from(days)
                    .map_err(|_| "days must be between 1 and 365".to_string())?;
                bounds("days", d, 1, 365)?;
            }
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("compact")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "get_event" => {
            args.only(name, &["event_id", "compact", "fields", "offline"])?;
            args.required_str("event_id")?;
            args.optional_bool("compact")?;
            args.optional_str_array("fields")?;
            args.optional_bool("offline")?;
            Ok(())
        }
        "create_event" => {
            args.required_str("subject")?;
            let start = args.required_str("start")?;
            let end = args.required_str("end")?;
            cli::normalize_datetime_iso(&start).map_err(format_error)?;
            cli::normalize_datetime_iso(&end).map_err(format_error)?;
            args.optional_str("time_zone")?;
            args.optional_bool("is_all_day")?;
            args.optional_str("location")?;
            args.optional_str("body")?;
            if let Some(attendees) = args.optional_str_array("attendees")? {
                cli::check_recipient_addresses(&attendees).map_err(format_error)?;
            }
            args.optional_bool("is_online_meeting")?;
            Ok(())
        }
        "respond_event" => {
            args.required_str("event_id")?;
            let resp = args.required_str("response")?;
            cli::normalize_rsvp_response(&resp).map_err(format_error)?;
            args.optional_str("comment")?;
            Ok(())
        }
        "delete_event" => {
            args.required_str("event_id")?;
            args.required_true("confirm")?;
            Ok(())
        }
        "list_todo_lists" => {
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            let cursor = args.optional_str("cursor")?;
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            if cursor.is_some() && refresh {
                return Err("cursor cannot be combined with refresh".to_string());
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        "list_tasks" => {
            args.optional_str("list")?;
            args.optional_bool("include_completed")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        "create_task" => {
            args.required_str("title")?;
            args.optional_str("list")?;
            args.optional_str("notes")?;
            args.optional_str("body")?;
            if let Some(due) = args.optional_str("due")?.or(args.optional_str("due_date_iso")?) {
                cli::normalize_todo_due_iso(&due).map_err(format_error)?;
            }
            if let Some(imp) = args.optional_str("importance")? {
                cli::normalize_todo_importance(&imp).map_err(format_error)?;
            }
            Ok(())
        }
        "complete_task" => {
            args.required_str("task_id")?;
            args.optional_str("list")?;
            args.optional_bool("completed")?;
            Ok(())
        }
        "search_contacts" => {
            args.optional_str("query")?;
            if let Some(limit) = args.optional_usize("limit")? {
                bounds("limit", limit, 1, 200)?;
            }
            args.optional_bool("offline")?;
            Ok(())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

pub(super) fn bounds(name: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if value < min || value > max {
        Err(format!("{name} must be between {min} and {max} (got {value})"))
    } else {
        Ok(value)
    }
}

pub(super) struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    pub(super) fn new(value: &'a Value) -> Self {
        Self(value)
    }

    pub(super) fn object(&self) -> Result<&serde_json::Map<String, Value>, String> {
        self.0
            .as_object()
            .ok_or_else(|| "arguments must be a JSON object".to_string())
    }

    pub(super) fn only(&self, tool: &str, fields: &[&str]) -> Result<(), String> {
        if let Some(key) = self
            .object()?
            .keys()
            .find(|key| !fields.contains(&key.as_str()))
        {
            return Err(format!("unsupported argument for {tool}: {key}"));
        }
        Ok(())
    }

    pub(super) fn optional_text(&self, key: &str) -> Result<Option<&str>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    pub(super) fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    pub(super) fn required_str(&self, key: &str) -> Result<String, String> {
        match self.object()?.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    pub(super) fn required_text(&self, key: &str) -> Result<String, String> {
        match self.object()?.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    pub(super) fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("{key} must be a boolean")),
        }
    }

    pub(super) fn required_true(&self, key: &str) -> Result<(), String> {
        match self.object()?.get(key) {
            Some(Value::Bool(true)) => Ok(()),
            _ => Err(format!("{key} must be literal boolean true")),
        }
    }

    pub(super) fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => {
                let val = n
                    .as_u64()
                    .and_then(|u| usize::try_from(u).ok())
                    .ok_or_else(|| format!("{key} must be a non-negative integer"))?;
                Ok(Some(val))
            }
            Some(_) => Err(format!("{key} must be an integer")),
        }
    }

    pub(super) fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .ok_or_else(|| format!("{key} must be an integer"))
                .map(Some),
            Some(_) => Err(format!("{key} must be an integer")),
        }
    }

    pub(super) fn optional_str_array(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let s = item
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| format!("{key}[{i}] must be a non-empty string"))?;
                    out.push(s.to_string());
                }
                Ok(Some(out))
            }
            Some(_) => Err(format!("{key} must be an array of strings")),
        }
    }

    pub(super) fn required_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        let items = self
            .optional_str_array(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))?;
        if items.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        Ok(items)
    }
}
