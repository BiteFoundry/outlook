//! MCP tool JSON Schema catalog definitions.

use serde_json::{json, Value};

pub fn tool_catalog() -> Vec<Value> {
    vec![
        json!({
            "name": "whoami",
            "description": "Display current user identity, token expiry, OAuth scopes, and auth source (outlook-cli vs shared teams-cli).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_mail_folders",
            "description": "List Outlook mail folders with unread and total item counts. Graph cost: 1 request plus 1 batch per folder nesting level, skipped while the cache is younger than OUTLOOK_CLI_CACHE_SECS (default 0).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of folders to return (1-200, default 50)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous response" },
                    "refresh": { "type": "boolean", "description": "Force a full folder-tree fetch even when the cache is fresh. Use only when a folder is missing." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "list_messages",
            "description": "List emails in a mail folder (accepts folder ID, display name/prefix, or well-known alias such as 'inbox', 'sent', 'drafts', 'archive', 'trash', 'junk'). Usually 1-2 Graph requests. The first call on a large folder syncs at most 40 pages (sync_complete: false until done); unread/flagged/focused filters scan at most 20 pages. offline: true reads the cache only.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "folder": { "type": "string", "description": "Folder name, alias, or ID (default 'inbox')" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of messages to return (1-200, default 25)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous response" },
                    "unread": { "type": "boolean", "description": "If true, return only unread messages." },
                    "flagged": { "type": "boolean", "description": "If true, return only flagged messages." },
                    "focused": { "type": "boolean", "description": "If true, return only Focused Inbox messages." },
                    "compact": { "type": "boolean", "description": "Default true. Set false for every cached field (no attachment bytes)" },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "get_message",
            "description": "Read an email by ID. Compact by default; pass fields: [...] for selective field projection (e.g. ['cc', 'bcc', 'categories', 'attachments']) without pulling full HTML, or set compact:false for HTML and attachment metadata. 1 Graph request unless offline (plus 1 when mark_read changes the state); compact:false can be large.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID from list_messages or search_mail." },
                    "mark_read": { "type": "boolean", "description": "If true, mark the message as read online (default false)." },
                    "compact": { "type": "boolean", "description": "Default true. Set false for HTML and attachment metadata (no attachment bytes)" },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional selective field projection (e.g. ['cc', 'bcc', 'categories', 'attachments']) returning only matched fields plus missing_fields if unrecognized."
                    },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "send_mail",
            "description": "Compose and send an email with Markdown body and optional file attachments. Attachments over 3 MiB are uploaded in chunks before sending, which takes several requests.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Array of recipient email addresses (at least one required)." },
                    "subject": { "type": "string", "description": "Subject line of the outgoing email." },
                    "body": { "type": "string", "description": "Markdown body" },
                    "cc": { "type": "array", "items": { "type": "string" }, "description": "Optional array of CC recipient email addresses." },
                    "bcc": { "type": "array", "items": { "type": "string" }, "description": "Optional array of BCC recipient email addresses." },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"], "description": "Message priority level: low, normal, or high." },
                    "attachments": { "type": "array", "items": { "type": "string" }, "description": "Optional array of local file paths to attach." },
                    "save_to_sent_items": { "type": "boolean", "description": "Save a copy in Sent Items (default true)." }
                },
                "required": ["to", "subject", "body"]
            }
        }),
        json!({
            "name": "reply_mail",
            "description": "Reply (or reply all) to an email using Markdown. Sends immediately (does not create a draft).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to reply to, from list_messages or search_mail." },
                    "body": { "type": "string", "description": "Reply body content formatted in Markdown." },
                    "reply_all": { "type": "boolean", "description": "If true, reply to all recipients instead of sender only (default false)." }
                },
                "required": ["message_id", "body"]
            }
        }),
        json!({
            "name": "forward_mail",
            "description": "Forward an email (including the original message and attachments) to one or more recipients. Sends immediately (does not create a draft).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to forward, from list_messages or search_mail." },
                    "to": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Array of recipient email addresses (at least one required)." },
                    "comment": { "type": "string", "description": "Optional Markdown note prepended above the forwarded message." }
                },
                "required": ["message_id", "to"]
            }
        }),
        json!({
            "name": "update_message",
            "description": "Update an email's read state, flag status, or move it to another folder. Does not edit message content; use update_draft to edit a draft's subject or body.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to update, from list_messages or search_mail." },
                    "is_read": { "type": "boolean", "description": "Set true to mark read or false to mark unread." },
                    "flagged": { "type": "boolean", "description": "Set true to flag or false to clear the flag." },
                    "destination_folder": { "type": "string", "description": "Target folder name, alias (e.g. 'archive'), or ID" },
                    "destination_folder_id": { "type": "string", "description": "Alias for destination_folder" },
                    "move_to_folder": { "type": "string", "description": "Alias for destination_folder" },
                    "compact": { "type": "boolean", "description": "Default true. Set false to include full moved message fields (no attachment bytes)" }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "delete_message",
            "description": "Destructive: delete an email message from Outlook and local cache. Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Message ID to delete, from list_messages or search_mail." },
                    "confirm": { "type": "boolean", "description": "Must be literal boolean true" }
                },
                "required": ["message_id", "confirm"]
            }
        }),
        json!({
            "name": "download_attachment",
            "description": "Download an Outlook email attachment or inline screenshot to disk and return structured JSON { ok, path, file_name, mime_type, size_bytes, is_image } so multimodal AI agents can read media directly from disk. Fetches the full attachment bytes with Graph requests unless offline reads them from the cache; time grows with file size.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "Outlook message ID" },
                    "attachment_index": { "type": "integer", "minimum": 0, "description": "0-based attachment index (defaults to first image or first attachment)" },
                    "attachment": { "type": "string", "description": "Attachment ID, filename, or 0-based index" },
                    "output_dir": { "type": "string", "description": "Target directory on disk (defaults to configured download_dir)" },
                    "offline": { "type": "boolean", "description": "Read cached message metadata/inline bytes from SQLite without Graph API call" }
                },
                "required": ["message_id"]
            }
        }),
        json!({
            "name": "search_mail",
            "description": "Search emails in local SQLite FTS5 (`offline: true`) or remotely via Microsoft Graph KQL `$search`. Remote search is 1 Graph request per page and is rate-limited by Exchange; prefer offline: true (local FTS, no request) for mail that is already synced.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "query": { "type": "string", "description": "Search query string (Graph KQL online, or FTS5 terms offline)." },
                    "folder": { "type": "string", "description": "Optional folder name, alias, or ID to restrict search." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of matching messages to return (1-200, default 25)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous remote search response" },
                    "compact": { "type": "boolean", "description": "Default true. Set false for full message fields in online results (no attachment bytes)" },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "list_events",
            "description": "List calendar events within a date/time window (defaults to next 7 days). Compact by default (omits body_html and recurrence, listed in omitted.fields). 1 Graph request per 100 events in the window; offline: true reads the cache only.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "from": { "type": "string", "description": "Start ISO-8601 or YYYY-MM-DD" },
                    "start": { "type": "string", "description": "Alias for from" },
                    "to": { "type": "string", "description": "End ISO-8601 or YYYY-MM-DD" },
                    "end": { "type": "string", "description": "Alias for to" },
                    "days": { "type": "integer", "minimum": 1, "maximum": 365, "description": "Number of days from start when to/end is omitted (1-365, default 7)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of events to return (1-200, default 50)." },
                    "compact": { "type": "boolean", "description": "Default true. Set false to include body_html and recurrence on each event" },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "get_event",
            "description": "Read a calendar event by ID. Compact by default (omits body_html); pass fields: [...] for selective field projection (e.g. ['attendees', 'body_plain', 'recurrence', 'categories']) without pulling full HTML, or compact:false for all fields. 1 Graph request unless offline.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID from list_events." },
                    "compact": { "type": "boolean", "description": "Default true. Set false to include body_html and all fields" },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional selective field projection (e.g. ['attendees', 'body_plain', 'recurrence', 'categories']) returning only matched fields plus missing_fields if unrecognized."
                    },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                },
                "required": ["event_id"]
            }
        }),
        json!({
            "name": "create_event",
            "description": "Create a new Outlook calendar event (optionally as an online Teams meeting) using ISO-8601/RFC3339 or wall-clock dates. Sends calendar invitations to all listed attendees.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "Title or subject of the calendar event." },
                    "start": { "type": "string", "description": "RFC3339 instant, wall clock 'YYYY-MM-DD HH:MM' (machine zone, or time_zone when given), or 'YYYY-MM-DD' when is_all_day." },
                    "end": { "type": "string", "description": "Same form as start. For all-day events the end date is exclusive (next day for a one-day event)." },
                    "time_zone": { "type": "string", "description": "IANA or Windows zone. Wall-clock start/end are read in this zone; RFC3339 values stay exact instants in UTC." },
                    "is_all_day": { "type": "boolean", "description": "All-day event: start and end must be 'YYYY-MM-DD'." },
                    "location": { "type": "string", "description": "Optional meeting location or room name." },
                    "body": { "type": "string", "description": "Optional event description or agenda in Markdown." },
                    "attendees": { "type": "array", "items": { "type": "string" }, "description": "Optional array of attendee email addresses to invite." },
                    "is_online_meeting": { "type": "boolean", "description": "If true, attach a Microsoft Teams meeting link (default false)." }
                },
                "required": ["subject", "start", "end"]
            }
        }),
        json!({
            "name": "respond_event",
            "description": "Respond (accept, tentative, or decline) to a calendar event invitation. Sends the RSVP response (and optional comment) to the organizer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID from list_events." },
                    "response": { "type": "string", "enum": ["accept", "tentative", "decline"], "description": "RSVP decision: accept, tentative, or decline." },
                    "comment": { "type": "string", "description": "Optional plain-text note sent to the organizer with your RSVP." }
                },
                "required": ["event_id", "response"]
            }
        }),
        json!({
            "name": "delete_event",
            "description": "Destructive: delete a calendar event from Outlook and local cache (cancels/notifies attendees if organizer). Requires confirm: true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID to delete, from list_events." },
                    "confirm": { "type": "boolean", "description": "Must be literal boolean true" }
                },
                "required": ["event_id", "confirm"]
            }
        }),
        json!({
            "name": "list_todo_lists",
            "description": "List Microsoft To Do task lists. Graph cost: 1 request per page of lists, skipped while the cache is younger than OUTLOOK_CLI_CACHE_SECS (default 0).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of task lists to return (1-200, default 50)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous response" },
                    "refresh": { "type": "boolean", "description": "Force a refetch even when the cache is fresh." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "list_tasks",
            "description": "List tasks in a Microsoft To Do list (accepts list name, 'tasks'/'defaultList', or ID). 1 Graph request per 100 tasks unless offline.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "list": { "type": "string", "description": "To Do list name, alias ('tasks'), or ID from list_todo_lists (default 'tasks')." },
                    "include_completed": { "type": "boolean", "description": "If true, include completed tasks in results (default false)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of tasks to return (1-200, default 50)." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "create_task",
            "description": "Create a new task in a Microsoft To Do list (default 'tasks'; use list_todo_lists to find list names or IDs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Title of the new To Do task." },
                    "list": { "type": "string", "description": "Target To Do list name, alias, or ID from list_todo_lists (default 'tasks')." },
                    "notes": { "type": "string", "description": "Optional plain-text notes or body for the task." },
                    "due": { "type": "string", "description": "YYYY-MM-DD or ISO-8601" },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"], "description": "Task priority level: low, normal, or high." }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "complete_task",
            "description": "Mark a Microsoft To Do task completed (or incomplete with completed: false). Get task_id from list_tasks and list names/IDs from list_todo_lists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to update, from list_tasks." },
                    "list": { "type": "string", "description": "Target To Do list name, alias, or ID from list_todo_lists (default 'tasks')." },
                    "completed": { "type": "boolean", "description": "Set true to complete or false to reopen (default true)." }
                },
                "required": ["task_id"]
            }
        }),
        json!({
            "name": "search_contacts",
            "description": "List or search personal Outlook contacts and Entra organization directory. A query reads personal contacts and the organization directory: 2-3 Graph requests.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Optional name or email search string; omit to list personal contacts." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum number of contacts to return (1-200, default 25)." },
                    "offline": { "type": "boolean", "description": "Read the local cache only; no Graph request." }
                }
            }
        }),
        json!({
            "name": "update_draft",
            "description": "Edit subject and/or body of an existing Outlook draft only (not sent or received mail). Provide Markdown body or raw body_html.",
            "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["message_id"],
                "properties": {
                    "message_id": { "type": "string", "description": "Draft message ID from list_messages in the drafts folder." },
                    "subject": { "type": "string", "description": "Replacement subject line for the draft." },
                    "body": { "type": "string", "description": "Replacement body in Markdown" },
                    "body_html": { "type": "string", "description": "Replacement body in HTML" }
                }
            }
        }),
        json!({
            "name": "update_event",
            "description": "Edit subject and/or ISO-8601 start/end times of an existing calendar event. Updates notify invited attendees.",
            "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["event_id"],
                "properties": {
                    "event_id": { "type": "string", "description": "Calendar event ID to edit, from list_events." },
                    "subject": { "type": "string", "description": "Replacement title or subject for the event." },
                    "start": { "type": "string", "description": "RFC3339 instant, wall-clock time in the event's zone, or all-day date" },
                    "end": { "type": "string", "description": "RFC3339 instant, wall-clock time in the event's zone, or exclusive all-day end date" }
                }
            }
        }),
        json!({
            "name": "update_task",
            "description": "Edit title and/or due date of an existing Microsoft To Do task. Get task_id from list_tasks and list names/IDs from list_todo_lists.",
            "inputSchema": {
                "type": "object", "additionalProperties": false, "required": ["task_id"],
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to edit, from list_tasks." },
                    "list": { "type": "string", "description": "To Do list name, alias, or ID (default tasks)" },
                    "title": { "type": "string", "description": "Replacement title for the To Do task." },
                    "due": { "type": "string", "description": "YYYY-MM-DD at midnight UTC, or RFC3339" }
                }
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Argument validation & typed extraction
// ---------------------------------------------------------------------------
