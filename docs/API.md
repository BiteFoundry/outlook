# `teams` / `outlook` HTTP JSON API

Local HTTP/JSON server for AI agents, scripts, and integrations. Exposed by `teams serve` or `outlook serve` (alias `api`), backed by Microsoft Graph v1.0, local SQLite WAL/FTS5 caching, and the background sync engine (`sync::spawn_sync_engine`).

Both binary personas (`teams` and `outlook`) mount the complete Microsoft Teams + Outlook Mail, Calendar, Contacts, and Microsoft To Do route surface on the same HTTP server.

## Starting the server

```bash
teams serve                     # http://127.0.0.1:8788
outlook serve                   # same API server under the outlook persona
teams serve --port 9000         # custom port
teams serve --bind 127.0.0.1 --port 8788
teams serve --token secret123   # require Authorization: Bearer secret123
teams api                       # alias for `serve`
```

- `--bind` defaults to `127.0.0.1` (loopback only).
- `--port` defaults to `8788`.
- `--token` is optional; when set, every endpoint except `GET /health` requires `Authorization: Bearer <token>`.
- Requires an existing login (`teams login`) or `TEAMS_ACCESS_TOKEN`.
- On startup, `teams serve` spawns the background sync engine so the local SQLite cache stays warm while serving requests.

## Conventions

- All responses are JSON (`Content-Type: application/json`).
- Errors use `{"error": "<message>"}` with HTTP `400`, `401`, `404`, or `500`.
- `limit` defaults to `50` and is clamped to `1..=1000`.
- `{key}` path parameters accept:
  - URL-encoded `conv_key` (e.g. `chat%3A19%3A...` or `chan%3A<team_id>%3A<channel_id>`),
  - raw `chat_id`,
  - `"<team>/<channel>"` (URL-encoded as needed), or
  - a unique case-insensitive title substring.
- Permissive CORS is enabled for local browser-based agent UIs.

## Endpoints

### `GET /health`

Always public, even when `--token` is configured.

```bash
curl http://127.0.0.1:8788/health
```

```json
{
  "ok": true,
  "user_id": "00000000-0000-0000-0000-000000000000",
  "version": "0.2.0"
}
```

### `GET /conversations?refresh=false&kind=`

Lists both chats (`oneOnOne`, `group`, `meeting`) and team channels (`channel`) from the local cache, or syncs from Microsoft Graph first when `refresh=true`.

```bash
curl "http://127.0.0.1:8788/conversations?refresh=true&kind=oneOnOne" \
  -H "Authorization: Bearer secret123"
```

```json
{
  "conversations": [
    {
      "conv_key": "chat:19:abc@thread.v2",
      "target": { "target_type": "chat", "chat_id": "19:abc@thread.v2" },
      "kind": "oneOnOne",
      "team_id": null,
      "team_name": null,
      "title": "Alice Kim",
      "topic": null,
      "unread_count": 0,
      "last_read_at": null,
      "last_msg_id": "1700000000000",
      "last_msg_author": "Alice Kim",
      "last_msg_preview": "See you at 3pm",
      "last_updated_at": "2026-11-15T06:00:00Z",
      "pinned": false,
      "peer_user_id": "user-uuid",
      "web_url": "https://teams.microsoft.com/..."
    }
  ]
}
```

### `POST /chats`

Creates a 1:1 (`members` length 1) or group (`members` length >= 2, optional `topic`) chat.

```bash
curl -X POST http://127.0.0.1:8788/chats \
  -H "Content-Type: application/json" \
  -d '{"members":["alice@contoso.com","bob@contoso.com"],"topic":"Release Sync"}'
```

```json
{
  "ok": true,
  "conversation": { "conv_key": "chat:19:...", "title": "Release Sync" }
}
```

### `GET /teams?refresh=false`

Lists joined Microsoft Teams and their channels.

```bash
curl "http://127.0.0.1:8788/teams?refresh=true"
```

```json
{
  "teams": [
    {
      "id": "team-uuid",
      "display_name": "Engineering",
      "description": "Core engineering",
      "channels": [
        {
          "id": "19:...@thread.tacv2",
          "team_id": "team-uuid",
          "team_name": "Engineering",
          "display_name": "General",
          "description": null,
          "membership_type": "standard",
          "web_url": null
        }
      ]
    }
  ]
}
```

### `GET /conversations/{key}/messages?limit=50&offline=false`

Refreshes latest root messages from Graph (unless `offline=true`), persists them to SQLite, and returns messages in chronological order (oldest first).

```bash
curl "http://127.0.0.1:8788/conversations/chat%3A19%3Aabc%40thread.v2/messages?limit=20"
```

```json
{
  "conversation": { "conv_key": "chat:19:abc@thread.v2", "title": "Alice Kim" },
  "messages": [
    {
      "msg_key": "chat:19:abc@thread.v2|1700000000000",
      "conv_key": "chat:19:abc@thread.v2",
      "msg_id": "1700000000000",
      "parent_msg_id": null,
      "reply_to_id": null,
      "reply_to_author": null,
      "reply_to_preview": null,
      "subject": null,
      "author_id": "user-uuid",
      "author_name": "Alice Kim",
      "is_me": false,
      "created_at": "2026-11-15T06:00:00Z",
      "last_modified_at": null,
      "deleted": false,
      "importance": "normal",
      "message_type": "message",
      "body_html": "<p>Hello!</p>",
      "body_plain": "Hello!",
      "attachments": [],
      "mentions": [],
      "reactions": [],
      "reply_count": 0,
      "web_url": null
    }
  ]
}
```

### `POST /conversations/{key}/messages`

Compiles Markdown to Teams HTML and sends a message to a chat or team channel.

Body:
- `markdown` (required, non-empty string)
- `subject` (optional string, for channel posts)
- `mentions` (optional array of `"user_id:DisplayName"` strings or `{ "id"?: 0, "user_id": "...", "display_name": "..." }` objects)
- `important` (optional boolean, default `false`)

```bash
curl -X POST http://127.0.0.1:8788/conversations/Engineering%2FGeneral/messages \
  -H "Content-Type: application/json" \
  -d '{"markdown":"Deploy **v1.2.0** complete!","subject":"Release","mentions":["u1:Alice"],"important":true}'
```

### `GET /conversations/{key}/threads/{root_id}?limit=50&offline=false`

Fetches a team channel root post and all thread replies (`400` if `{key}` is a chat).

```bash
curl "http://127.0.0.1:8788/conversations/chan%3Ateam1%3Achan1/threads/1700000000000?limit=50"
```

```json
{
  "thread": {
    "conv_key": "chan:team1:chan1",
    "root": { "msg_id": "1700000000000" },
    "replies": [ { "msg_id": "1700000000100", "parent_msg_id": "1700000000000" } ]
  }
}
```

### `POST /conversations/{key}/messages/{msg_id}/replies`

Posts a thread reply when `{key}` is a team channel, or sends a quote-reply when `{key}` is a 1:1/group chat.

```bash
curl -X POST http://127.0.0.1:8788/conversations/chan%3Ateam1%3Achan1/messages/1700000000000/replies \
  -H "Content-Type: application/json" \
  -d '{"markdown":"Looks good to me!"}'
```

### `POST /conversations/{key}/files`

Reads a file **on the host running `teams serve`**, uploads it to OneDrive (for chats) or the Channel SharePoint folder (for channels), and posts a reference attachment (or inline `hostedContents` image when `inline: true`).

```json
{
  "path": "C:\\Users\\me\\Documents\\report.pdf",
  "caption": "Q4 report attached",
  "thread_root_id": null,
  "inline": false
}
```

### `POST /conversations/{key}/messages/{msg_id}/attachments/download`

Downloads an attachment from a message to `out_dir` (defaults to configured `download_dir`, usually `Downloads/teams`).

```json
{
  "index": 0,
  "out_dir": null,
  "thread_root_id": null
}
```

```json
{
  "ok": true,
  "path": "C:\\Users\\me\\Downloads\\teams\\report.pdf",
  "file_name": "report.pdf",
  "bytes_written": 42190
}
```

### `POST /conversations/{key}/messages/{msg_id}/reactions`

Sets (`enable: true`) or unsets (`enable: false`) a reaction (`like`, `heart`, `laugh`, `surprised`, `sad`, `angry`, or unicode emoji).

```json
{
  "reaction": "like",
  "enable": true,
  "thread_root_id": null
}
```

### `PATCH /conversations/{key}/messages/{msg_id}` & `DELETE /conversations/{key}/messages/{msg_id}`

Edits or soft-deletes a message sent by the authenticated user.

```bash
curl -X PATCH http://127.0.0.1:8788/conversations/chat%3A19%3Aabc/messages/1700000000000 \
  -H "Content-Type: application/json" \
  -d '{"markdown":"Updated text","thread_root_id":null}'

curl -X DELETE "http://127.0.0.1:8788/conversations/chat%3A19%3Aabc/messages/1700000000000"
```

For channel thread replies, pass `?thread_root_id=<root_id>` or JSON body `{"thread_root_id":"<root_id>"}`.

### `POST /conversations/{key}/read`

Marks a chat as read in Graph and updates the local SQLite read watermark.

### `GET /conversations/{key}/members`

Lists members of a chat or channel and caches them in SQLite.

### `GET /search?q=...&limit=50&target=`

Searches local SQLite FTS5 message cache. `target` is optional (conv_key or fuzzy name of a cached conversation).

```json
{
  "query": "deploy",
  "hits": [
    {
      "conv_key": "chan:team1:chan1",
      "conv_title": "Engineering / General",
      "kind": "channel",
      "msg_id": "1700000000000",
      "parent_msg_id": null,
      "author_name": "Alice Kim",
      "created_at": "2026-11-15T06:00:00Z",
      "snippet": "Deploy v1.2.0 complete!"
    }
  ]
}
```

### `GET /presence` & `POST /presence`

- `GET /presence`: returns `{ "presence": { "user_id", "availability", "activity" } }` for the current user.
- `GET /presence?user_ids=id1,id2`: returns `{ "presences": [...] }` for the given Entra user IDs.
- `POST /presence`: sets preferred presence (`Available`, `Busy`, `DoNotDisturb`, `BeRightBack`, `Away`, `Reset`), optional `activity`, and optional `expiration_mins`.

```json
{
  "availability": "DoNotDisturb",
  "activity": "InACall",
  "expiration_mins": 60
}
```

## Outlook Mail Endpoints

### `GET /mail/folders?refresh=false&offline=false`

Lists mail folders (`MailFolder[]`). Uses local SQLite cache unless `refresh=true` or cache is empty (when `offline=false`).

```json
{
  "folders": [
    {
      "id": "AAMk...",
      "display_name": "Inbox",
      "well_known_name": "inbox",
      "parent_folder_id": null,
      "unread_item_count": 3,
      "total_item_count": 42
    }
  ]
}
```

### `GET /mail/messages?folder=inbox&limit=30&unread=false&refresh=false&offline=false`

Lists messages in a mail folder. `folder` accepts a well-known name (`inbox`, `sentitems`, `drafts`, `deleteditems`, `junkemail`, `archive`, or aliases `sent`, `trash`, `junk`), folder display name, or folder ID.

```json
{
  "folder": "inbox",
  "messages": [
    {
      "id": "AAMk...",
      "conversation_id": "AAQk...",
      "folder_id": "inbox",
      "subject": "Quarterly Review",
      "from": { "name": "Alice Kim", "address": "alice@example.com" },
      "received_at": "2026-11-15T06:00:00Z",
      "is_read": false,
      "flagged": false,
      "has_attachments": false,
      "preview": "Here are the Q4 numbers..."
    }
  ]
}
```

### `GET /mail/messages/{id}?offline=false`

Returns `{ "message": <MailMessage> }` including `body_html`, `body_plain`, recipients, and `attachments`. Set `offline=true` to read strictly from the SQLite cache.

### `POST /mail/send`

Sends a new email. Compiles `markdown` to HTML and attaches any local files listed in `attachments`.

```json
{
  "to": ["alice@example.com"],
  "cc": ["bob@example.com"],
  "bcc": [],
  "subject": "Weekly Sync Notes",
  "markdown": "Hi team,\n\n- Action item 1",
  "attachments": [],
  "important": false
}
```

Returns `{ "ok": true, "to": [...], "subject": "..." }`.

### `POST /mail/messages/{id}/reply`

Replies to a mail message (`reply_all` defaults to `false`).

```json
{
  "markdown": "Thanks, confirmed.",
  "reply_all": false
}
```

### `POST /mail/messages/{id}/forward`

Forwards a mail message to one or more recipients (`markdown` optional).

```json
{
  "to": ["carol@example.com"],
  "markdown": "FYI for Tuesday's review."
}
```

### `PATCH /mail/messages/{id}`

Updates read and/or flagged status in Graph and SQLite cache (at least one of `is_read` or `flagged` required).

```json
{
  "is_read": true,
  "flagged": false
}
```

### `POST /mail/messages/{id}/move`

Moves a mail message to `destination_folder` (well-known name, display name, or folder ID).

```json
{
  "destination_folder": "archive"
}
```

Returns `{ "ok": true, "message": <MailMessage> }`.

### `DELETE /mail/messages/{id}`

Deletes a mail message from Exchange/Graph and local cache. Returns `{ "ok": true, "message_id": "..." }`.

### `POST /mail/messages/{id}/attachments/{att_id}/download`

Downloads mail attachment `att_id` to `out_dir` (defaults to configured `download_dir`).

```json
{
  "out_dir": null
}
```

Returns `{ "ok": true, "path": "...", "file_name": "...", "bytes_written": 12345 }`.

### `GET /mail/search?q=...&folder=&limit=30&remote=false`

Searches cached messages via SQLite FTS5 (`hits`). Pass `remote=true` to also execute a Microsoft Graph `$search` (`remote`).

## Outlook Calendar Endpoints

### `GET /calendar/events?start=&end=&days=7&limit=50&offline=false`

Lists calendar events in `[start, end]`. When `start`/`end` are omitted, defaults to now through `+days` days (clamped `1..=365`).

```json
{
  "start": "2026-11-15T00:00:00Z",
  "end": "2026-11-22T00:00:00Z",
  "events": []
}
```

### `POST /calendar/events`

Creates a calendar event. `start_iso` and `end_iso` accept RFC3339 timestamps, `YYYY-MM-DD HH:MM`, or `YYYY-MM-DD`.

```json
{
  "subject": "Architecture Review",
  "start_iso": "2026-11-16T10:00:00Z",
  "end_iso": "2026-11-16T11:00:00Z",
  "time_zone": "Asia/Seoul",
  "is_all_day": false,
  "location": "Room 3B",
  "markdown": "Agenda:\n- API review",
  "attendees": ["alice@example.com"],
  "is_online_meeting": true
}
```

Returns `{ "ok": true, "event": <CalendarEvent> }`.

### `POST /calendar/events/{id}/rsvp`

Responds to a calendar meeting invitation (`response`: `accept`, `tentative`, or `decline`; `comment` optional).

```json
{
  "response": "accept",
  "comment": "See you there!"
}
```

### `DELETE /calendar/events/{id}`

Deletes/cancels a calendar event in Graph and local SQLite cache.

## Outlook Contacts Endpoints

### `GET /contacts?q=&limit=50&offline=false`

Lists personal contacts, or searches personal contacts + Entra organization directory when `q` is provided.

### `POST /contacts`

Creates a personal Outlook contact (`display_name` required; `given_name`, `surname`, `email_addresses`, `mobile_phone`, `company_name`, `job_title`, `department`, `notes` optional).

### `DELETE /contacts/{id}`

Deletes a personal Outlook contact from Graph and SQLite cache.

## Microsoft To Do Endpoints

### `GET /todo/lists?refresh=false&offline=false`

Lists Microsoft To Do task lists (`{ "lists": [...] }`).

### `GET /todo/lists/{list_id}/tasks?include_completed=false&limit=50&refresh=false&offline=false`

Lists tasks in a To Do list (`{list_id}` accepts list ID or list display name / `"Tasks"`).

### `POST /todo/lists/{list_id}/tasks`

Creates a To Do task (`title` required; `body_plain`, `due_date_iso` [`YYYY-MM-DD` or RFC3339], `importance` [`low|normal|high`] optional).

### `PATCH /todo/lists/{list_id}/tasks/{task_id}`

Marks a To Do task completed or active (`{ "completed": true }`).

### `DELETE /todo/lists/{list_id}/tasks/{task_id}`

Deletes a To Do task from Graph and local SQLite cache.

## Errors

All failing endpoints return `{"error": "<message>"}`:

| HTTP Status | Meaning |
| --- | --- |
| `400` | Validation error (empty body, invalid reaction/kind/presence/rsvp/importance/datetime, missing local file, etc.) |
| `401` | `--token` is enabled and `Authorization: Bearer <token>` is missing or invalid |
| `404` | Target conversation, cached thread, mail message, folder, or To Do list not found |
| `500` | Microsoft Graph or SQLite error |
