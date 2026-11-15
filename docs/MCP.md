# `teams` / `outlook` MCP Server

`teams mcp` or `outlook mcp` (alias `mcp-server`) runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over **stdio** using newline-delimited JSON-RPC 2.0. It exposes Microsoft Teams (chats, channels, threads, files, reactions, search, presence) as well as Outlook Mail, Outlook Calendar, Contacts / Entra Directory, and Microsoft To Do to MCP-capable agents such as Claude Desktop, Claude Code, and Codex.

## Requirements

1. Build or install the `teams` (and optionally `outlook` copy/symlink) binary:
   ```bash
   cargo build --release
   ```
2. Authenticate once (the MCP server never prompts interactively on stdin):
   ```bash
   teams login
   # or, if invoking under the outlook persona with M365 scopes:
   outlook login
   ```
   Or set `TEAMS_ACCESS_TOKEN` in the process environment.
3. Ensure `teams` (or `outlook`, e.g. `target/release/teams.exe` on Windows or on `PATH`) is accessible to your MCP host.

## Starting manually

```bash
teams mcp          # stdio: one JSON-RPC 2.0 object per line
outlook mcp        # same 36-tool MCP server via the outlook persona
teams mcp-server   # alias
```

`stdout` is strictly reserved for single-line JSON-RPC 2.0 responses; diagnostics and startup warnings are written to `stderr` only.

## Client configuration

### Claude Desktop

`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "teams": {
      "command": "teams",
      "args": ["mcp"]
    }
  }
}
```

On Windows with an explicit executable path:

```json
{
  "mcpServers": {
    "teams": {
      "command": "C:\\Users\\me\\source\\teams\\target\\release\\teams.exe",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code

```bash
claude mcp add teams -- teams mcp
# or with an explicit binary path:
claude mcp add teams -- C:\Users\me\source\teams\target\release\teams.exe mcp
```

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.teams]
command = "teams"
args = ["mcp"]
```

## Protocol

- **Transport**: JSON-RPC 2.0 over stdio, **newline-delimited JSON** (one JSON object per line, no `Content-Length` headers).
- **Supported `protocolVersion`s**: `2024-11-05`, `2025-03-26`, `2025-06-18` (unknown versions fall back to `2025-06-18`).
- **`serverInfo`**: `{ "name": "teams", "version": "<crate version>" }`.
- **Notifications**: requests without `id` or methods starting with `notifications/` (such as `notifications/initialized`) emit no response.
- **`ping`**: returns `{}`.
- **Error codes**: `-32700` parse error, `-32600` invalid JSON-RPC envelope, `-32601` method not found.
- **`tools/call`**: returns `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": false }` on success, or `"isError": true` with an error message on validation/Graph failures.

## Tools (36)

### Microsoft Teams Tools (17)

`target` arguments accept a canonical `conv_key` (`chat:...` or `chan:<team_id>:<channel_id>`), raw `chat_id`, `"Team/Channel"`, or a unique case-insensitive conversation title substring.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_chats` | `refresh?` (bool), `limit?` (1..1000) | `{ "chats": [...] }` |
| `create_chat` | `members` (`string[]`, min 1), `topic?` | `{ "ok": true, "conversation": {...} }` |
| `list_teams_channels` | `refresh?` (bool) | `{ "teams": [...] }` |
| `get_messages` | `target`, `limit?` (1..1000), `offline?` (bool) | `{ "conversation": {...}, "messages": [...] }` |
| `get_thread` | `target`, `root_msg_id`, `limit?` (1..1000) | `{ "conversation": {...}, "thread": {...} }` |
| `send_message` | `target`, `markdown`, `subject?`, `mentions?`, `important?` | `{ "ok": true, "conversation": {...}, "message": {...} }` |
| `reply_message` | `target`, `msg_id`, `markdown` | `{ "ok": true, "conversation": {...}, "reply_to": "...", "message": {...} }` |
| `send_file` | `target`, `path`, `caption?`, `thread_root_id?`, `inline?` | `{ "ok": true, "conversation": {...}, "path": "...", "message": {...} }` |
| `download_attachment` | `target`, `msg_id`, `index?`, `out_dir?`, `thread_root_id?` | `{ "ok": true, "path": "...", "file_name": "...", "bytes_written": ... }` |
| `react_message` | `target`, `msg_id`, `reaction`, `enable?` (default `true`), `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "...", "reaction": "...", "enable": ... }` |
| `edit_message` | `target`, `msg_id`, `markdown`, `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "..." }` |
| `delete_message` | `target`, `msg_id`, `confirm` (must be literal `true`), `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "..." }` |
| `search_messages` | `query`, `target?`, `limit?` (1..1000) | `{ "query": "...", "hits": [...] }` |
| `list_members` | `target` | `{ "conv_key": "...", "title": "...", "members": [...] }` |
| `mark_read` | `target` | `{ "ok": true, "conv_key": "..." }` |
| `get_presence` | `user_ids?` (`string[]`) | `{ "presence": {...} }` or `{ "presences": [...] }` |
| `set_presence` | `availability` (`Available\|Busy\|DoNotDisturb\|BeRightBack\|Away\|Reset`), `activity?`, `expiration_mins?` (1..1440) | `{ "ok": true, "availability": "...", "activity": "...", "expiration_mins": ... }` |

### Outlook Mail, Calendar, Contacts & To Do Tools (19)

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_mail_folders` | `offline?` (bool) | `{ "folders": [...] }` |
| `list_mails` | `folder?` (default `"inbox"`), `limit?` (1..200), `unread?` (bool), `offline?` (bool) | `{ "folder": "...", "messages": [...] }` |
| `get_mail` | `mail_id`, `offline?` (bool) | `{ "mail": {...} }` |
| `send_mail` | `to` (`string[]`, min 1), `subject`, `body` (Markdown), `cc?`, `bcc?`, `importance?` (`low\|normal\|high`), `attachments?` (`string[]`), `save_to_sent_items?` | `{ "ok": true, "to": [...], "subject": "..." }` |
| `reply_mail` | `mail_id`, `body` (Markdown), `reply_all?` (bool) | `{ "ok": true, "mail_id": "...", "reply_all": ... }` |
| `forward_mail` | `mail_id`, `to` (`string[]`, min 1), `comment?` | `{ "ok": true, "mail_id": "...", "to": [...] }` |
| `update_mail` | `mail_id`, `is_read?`, `flagged?`, `move_to_folder?` (at least one required) | `{ "ok": true, "mail_id": "...", "is_read": ..., "flagged": ..., "moved": ... }` |
| `delete_mail` | `mail_id`, `confirm` (must be literal `true`) | `{ "ok": true, "mail_id": "..." }` |
| `download_mail_attachment` | `mail_id`, `attachment_index?` (0-based), `attachment?` (ID or filename), `output_dir?` | `{ "ok": true, "attachment": {...}, "file": {...} }` |
| `search_mails` | `query`, `folder?`, `limit?` (1..200), `offline?` (bool) | `{ "query": "...", "messages": [...] }` or `{ "query": "...", "hits": [...] }` |
| `list_calendar_events` | `start?`, `end?`, `days?` (1..365, default `7`), `limit?` (1..200), `offline?` (bool) | `{ "start": "...", "end": "...", "events": [...] }` |
| `create_calendar_event` | `subject`, `start`, `end`, `time_zone?`, `is_all_day?`, `location?`, `body?`, `attendees?` (`string[]`), `is_online_meeting?` | `{ "ok": true, "event": {...} }` |
| `respond_calendar_event` | `event_id`, `response` (`accept\|tentative\|decline`), `comment?` | `{ "ok": true, "event_id": "...", "response": "..." }` |
| `delete_calendar_event` | `event_id`, `confirm` (must be literal `true`) | `{ "ok": true, "event_id": "..." }` |
| `search_contacts` | `query?`, `limit?` (1..200), `offline?` (bool) | `{ "contacts": [...] }` |
| `list_todo_tasks` | `list?`, `include_completed?` (bool), `limit?` (1..200), `offline?` (bool) | `{ "list_id": "...", "tasks": [...] }` |
| `create_todo_task` | `list?`, `title`, `body?`, `due_date_iso?`, `importance?` (`low\|normal\|high`) | `{ "ok": true, "task": {...} }` |
| `complete_todo_task` | `list?`, `task_id`, `completed?` (default `true`) | `{ "ok": true, "task": {...} }` |
| `delete_todo_task` | `list?`, `task_id`, `confirm` (must be literal `true`) | `{ "ok": true, "list_id": "...", "task_id": "..." }` |

### Mentions in `send_message`

`mentions` accepts either `"user_id:DisplayName"` strings or `{ "id"?: 0, "user_id": "...", "display_name": "..." }` objects.

### Destructive tool guard (`confirm: true`)

`delete_message`, `delete_mail`, `delete_calendar_event`, and `delete_todo_task` require `"confirm": true` as a literal JSON boolean. Omitting `confirm`, passing `false`, `"true"`, or `1` is rejected during argument validation before any Graph API call is made.

### Example JSON-RPC session

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_chats","arguments":{"refresh":true,"limit":20}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"send_message","arguments":{"target":"Engineering/General","markdown":"Build **passed**!","subject":"CI","mentions":["u1:Alice"]}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"delete_message","arguments":{"target":"Engineering/General","msg_id":"1700000000000","confirm":true}}}
```

## Notes and limitations

- `send_file` reads `path` on the host running `teams mcp`, not on a remote MCP client.
- `search_messages` queries the local SQLite FTS5 cache (`cache.db`); run `teams sync` or `get_messages` first if local history is empty.
- `react_message` supports `like`, `heart`, `laugh`, `surprised`, `sad`, `angry`, and unicode emoji.
