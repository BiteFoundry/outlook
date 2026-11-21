# `outlook` MCP Server

`outlook mcp` (alias `mcp-server`) runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over **stdio** using newline-delimited JSON-RPC 2.0. It exposes pure **Microsoft Outlook** capabilities (**Mail**, **Calendar**, **Microsoft To Do**, and **Contacts & Entra Directory**) to MCP-capable agents such as Claude Desktop, Claude Code, and OpenAI Codex.

## Requirements

1. Install or build the `outlook` binary:
   ```bash
   npm install -g @bitefoundry/outlook
   # or from source:
   cargo build --release
   ```
2. Authenticate once (the MCP server never prompts interactively on stdin):
   ```bash
   outlook login
   ```
   Or configure `OUTLOOK_ACCESS_TOKEN` / `OUTLOOK_TOKEN_FILE` (+ optional `OUTLOOK_TOKEN_REFRESH_CMD`), or rely on the automatic M365 shared-session bootstrap from `teams-cli` / `TEAMS_TOKEN_FILE` (`OUTLOOK_SHARE_M365_AUTH=true`).
3. Ensure `outlook` (e.g. `target/release/outlook.exe` on Windows or on `PATH`) is accessible to your MCP host.

## Starting manually

```bash
outlook mcp          # stdio: one JSON-RPC 2.0 object per line
outlook mcp-server   # alias
```

`stdout` is strictly reserved for single-line JSON-RPC 2.0 responses; diagnostics and startup warnings are written to `stderr` only.

## Client configuration

### Claude Desktop

`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "outlook": {
      "command": "outlook",
      "args": ["mcp"]
    }
  }
}
```

On Windows with an explicit executable path:

```json
{
  "mcpServers": {
    "outlook": {
      "command": "C:\\Users\\me\\source\\outlook\\target\\release\\outlook.exe",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code

```bash
claude mcp add outlook -- outlook mcp
# or with an explicit binary path:
claude mcp add outlook -- C:\Users\me\source\outlook\target\release\outlook.exe mcp
```

### OpenAI Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.outlook]
command = "outlook"
args = ["mcp"]
```

## Protocol

- **Transport**: JSON-RPC 2.0 over stdio, **newline-delimited JSON** (one JSON object per line, no `Content-Length` headers; maximum `8 MiB` per request line).
- **Supported `protocolVersion`s**: `2024-11-05`, `2025-03-26`, `2025-06-18` (unknown versions fall back to `2025-06-18`).
- **`serverInfo`**: The server name is `outlook` and the version matches `CARGO_PKG_VERSION`.
- **Startup**: `initialize` responds immediately; on an empty cache, initial mail folder bootstrap runs asynchronously in the background.
- **Notifications**: requests without `id` or methods starting with `notifications/` (such as `notifications/initialized`) emit no response.
- **`ping`**: returns `{}`.
- **Error codes**: `-32700` parse error, `-32600` invalid JSON-RPC envelope (or oversized line > `8 MiB`), `-32601` method not found.
- **`tools/call`**: returns `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": false }` on success, or `"isError": true` with an error message (including JWT `scp`/`roles` permission diagnostics on `403`) on validation/Graph failures.

## Microsoft Outlook Tools (23)

Mail `folder` arguments accept a canonical folder ID, display name/prefix, or well-known alias (`inbox`, `sent`, `drafts`, `archive`, `trash`, `junk`). To Do `list` arguments accept a list ID, display name, or `'tasks'` / `'defaultList'`.

| # | Tool | Domain | Arguments | Returns |
| --- | --- | --- | --- | --- |
| 1 | `whoami` | Auth | *(none)* | `{ "authenticated": bool, "user_id": ..., "email": ..., "auth_mode": ..., "auth_source": ..., "scopes": [...] }` |
| 2 | `list_mail_folders` | Mail | `refresh?` (bool), `offline?` (bool) | `{ "folders": [...] }` |
| 3 | `list_messages` | Mail | `folder?` (default `'inbox'`), `limit?` (1..200), `unread?`, `flagged?`, `focused?`, `offline?`, `compact?` | `{ "folder": {...}, "messages": [...] }` |
| 4 | `get_message` | Mail | `message_id` (req), `mark_read?`, `offline?`, `compact?` | `{ "message": {...} }` |
| 5 | `send_mail` | Mail | `to` (`string[]`, req), `subject` (req), `body` (Markdown, req), `cc?`, `bcc?`, `importance?` (`low\|normal\|high`), `attachments?`, `save_to_sent_items?` | `{ "ok": true, "to": [...], "subject": "..." }` |
| 6 | `reply_mail` | Mail | `message_id` (req), `body` (req), `reply_all?` | `{ "ok": true, "message_id": "...", "reply_all": bool }` |
| 7 | `forward_mail` | Mail | `message_id` (req), `to` (`string[]`, req), `comment?` | `{ "ok": true, "message_id": "...", "to": [...] }` |
| 8 | `update_message` | Mail | `message_id` (req), `is_read?`, `flagged?`, `destination_folder?` | `{ "ok": true, "message_id": "...", "is_read": ..., "flagged": ..., "moved": ... }` |
| 9 | `delete_message` | Mail | `message_id` (req), `confirm` (must be literal `true`) | `{ "ok": true, "message_id": "..." }` |
| 10 | `download_attachment` | Mail | `message_id` (req), `attachment_index?` (>=0), `attachment?` (ID/name/index), `output_dir?` | `{ "ok": true, "attachment": {...}, "file": {...} }` |
| 11 | `search_mail` | Mail | `query` (req), `folder?`, `limit?` (1..200), `offline?` | `{ "query": "...", "hits": [...] }` (offline) or `{ "query": "...", "messages": [...] }` (online) |
| 12 | `list_events` | Calendar | `from?` (`ISO-8601\|YYYY-MM-DD`), `to?`, `days?` (1..365), `limit?` (1..200), `offline?` | `{ "start": "...", "end": "...", "events": [...] }` |
| 13 | `create_event` | Calendar | `subject` (req), `start` (req), `end` (req), `time_zone?`, `is_all_day?`, `location?`, `body?`, `attendees?`, `is_online_meeting?` | `{ "ok": true, "event": {...} }` |
| 14 | `respond_event` | Calendar | `event_id` (req), `response` (`accept\|tentative\|decline`, req), `comment?` | `{ "ok": true, "event_id": "...", "response": "..." }` |
| 15 | `delete_event` | Calendar | `event_id` (req), `confirm` (must be literal `true`) | `{ "ok": true, "event_id": "..." }` |
| 16 | `list_todo_lists` | To Do | `refresh?`, `offline?` | `{ "lists": [...] }` |
| 17 | `list_tasks` | To Do | `list?`, `include_completed?`, `limit?` (1..200), `offline?` | `{ "list": {...}, "list_id": "...", "tasks": [...] }` |
| 18 | `create_task` | To Do | `title` (req), `list?`, `notes?`, `due?` (`YYYY-MM-DD\|ISO-8601`), `importance?` (`low\|normal\|high`) | `{ "ok": true, "task": {...} }` |
| 19 | `complete_task` | To Do | `task_id` (req), `list?`, `completed?` (default `true`) | `{ "ok": true, "task": {...} }` |
| 20 | `search_contacts` | Contacts | `query?`, `limit?` (1..200), `offline?` | `{ "contacts": [...] }` |
| 21 | `update_draft` | Mail | `message_id` (req), `subject?`, `body?` (Markdown), `body_html?` (HTML) | `{ "ok": true, "message_id": "..." }` |
| 22 | `update_event` | Calendar | `event_id` (req), `subject?`, `start?`, `end?` | `{ "ok": true, "event": {...} }` |
| 23 | `update_task` | To Do | `task_id` (req), `list?` (default `'tasks'`), `title?`, `due?` | `{ "ok": true, "task": {...} }` |

### Destructive tool guard (`confirm: true`)

`delete_message` and `delete_event` require `"confirm": true` as a literal JSON boolean. Omitting `confirm`, passing `false`, `"true"`, or `1` is rejected during argument validation before any Graph API call is made.

### Example JSON-RPC session

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_messages","arguments":{"folder":"inbox","unread":true,"limit":20,"compact":true}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"send_mail","arguments":{"to":["alice@contoso.com"],"subject":"Release v0.1.0","body":"Build **passed**!"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"create_event","arguments":{"subject":"Design Sync","start":"2026-11-20T09:00:00Z","end":"2026-11-20T10:00:00Z","is_online_meeting":true}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete_message","arguments":{"message_id":"AAMkAGI2...","confirm":true}}}
```

## Notes and limitations

- `send_mail` `attachments` paths and `download_attachment` `output_dir` paths are resolved on the host running `outlook mcp`, not on a remote MCP client.
- Pass `"offline": true` on read/search tools (`list_mail_folders`, `list_messages`, `get_message`, `search_mail`, `list_events`, `list_todo_lists`, `list_tasks`, `search_contacts`) to query the local SQLite cache (`cache.db`) without network calls.
- With `"compact": true`, `list_messages` returns the message ID, subject, sender, received time, and read, draft, flag, and attachment indicators. `get_message` returns addresses, timestamps, status, and the complete `body_plain` when loaded. For HTML and attachments, call `get_message` with the default output.
- `update_message` accepts read and flag changes or a destination folder. An unsupported argument such as `subject` fails validation before any write. Use `update_draft` for a draft's subject or body.

## Partial edits

- **Drafts.** `update_draft` checks the message's current draft status in Graph. A subject-only edit preserves the stored HTML and attachments. To replace the body, send either `body` for rendered Markdown or `body_html` for the supplied HTML. The HTML value can include existing CID references that you want to keep. [Microsoft Graph message update](https://learn.microsoft.com/en-us/graph/api/message-update?view=graph-rest-1.0)
- **Calendar.** `update_event` sends only the supplied subject, start, and end. It leaves the body, attendees, meeting link, and recurrence out of the PATCH. A wall-clock value such as `2026-11-23T09:00` uses the original zone for that endpoint. An RFC3339 timestamp represents an instant and is converted to UTC. For a recurring event in a non-UTC zone, use wall-clock values to keep the series in its original zone. For all-day events, use `YYYY-MM-DD` in the original zone and provide an exclusive end date. Graph may send meeting updates and can reject occurrences moved across their neighbors. [Microsoft Graph event update](https://learn.microsoft.com/en-us/graph/api/event-update?view=graph-rest-1.0) · [Event time zones](https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0)
- **To Do.** `update_task` sends only the supplied title and due date. The checklist, body, recurrence, and completion state stay outside the PATCH. A due date of `2026-11-25` means midnight UTC on that date. A due timestamp must be RFC3339 and is converted to UTC. [Microsoft Graph To Do update](https://learn.microsoft.com/en-us/graph/api/todotask-update?view=graph-rest-1.0)
- `search_mail` with `"offline": false` (default) queries Microsoft Graph KQL `$search` and caches matched messages into SQLite.
