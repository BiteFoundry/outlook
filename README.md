# outlook (Pure Microsoft Outlook CLI · TUI · HTTP API · MCP)

**`outlook`** (`@shinick/outlook`) is a pure **Microsoft Outlook** terminal client and AI agent toolkit built in Rust. Triage Mail, inspect Calendar agendas, manage Microsoft To Do tasks, and search Contacts & Entra Directory at local-cache speed—all from a single native binary.

> One standalone executable (`outlook` / `outlook.exe`) bundles OAuth 2.0 keyring credentials (plus automatic M365 shared-session bootstrap with `teams`), a **SQLite WAL + FTS5 local cache (`cache.db`)**, a Minimal-Chrome **`ratatui` TUI**, a background sync daemon (`outlook daemon`), an **Axum HTTP REST API (`outlook serve` on `:3939`)**, and a **stdio JSON-RPC 2.0 MCP server (`outlook mcp`)**. Prebuilt binaries ship for Windows, macOS, and Linux (`x86_64` / `aarch64`, compatible with **AlmaLinux 8 / GLIBC 2.28+**).

---

## Table of Contents

- [Overview & Key Highlights](#overview--key-highlights)
- [Installation](#installation)
  - [npm Global Install](#npm-global-install)
  - [Prebuilt Binaries & PowerShell Launcher](#prebuilt-binaries--powershell-launcher)
  - [Build from Source](#build-from-source)
  - [One-Click Self-Update](#one-click-self-update)
- [Authentication & External Token Brokers](#authentication--external-token-brokers)
  - [Interactive & Headless Login](#interactive--headless-login)
  - [Automatic M365 Shared-Session Bootstrap (`teams-cli`)](#automatic-m365-shared-session-bootstrap-teams-cli)
  - [Microsoft Graph Delegated Scopes](#microsoft-graph-delegated-scopes)
  - [External Token Brokers (`OUTLOOK_TOKEN_FILE`)](#external-token-brokers-outlook_token_file)
  - [Custom Entra ID App Registration](#custom-entra-id-app-registration)
- [Minimal-Chrome TUI Guide](#minimal-chrome-tui-guide)
  - [Layout, Density & Themes](#layout-density--themes)
  - [Key Shortcuts](#key-shortcuts)
  - [Quoted-Reply Folding & Inline Image Preview](#quoted-reply-folding--inline-image-preview)
- [CLI Reference](#cli-reference)
  - [Subcommands](#subcommands)
  - [Common CLI Workflows](#common-cli-workflows)
- [HTTP Server (`outlook serve`)](#http-server-outlook-serve)
- [MCP Server (`outlook mcp`)](#mcp-server-outlook-mcp)
- [Environment Variables & Configuration](#environment-variables--configuration)
  - [Storage & Config Locations](#storage--config-locations)
  - [Environment Variables](#environment-variables)
  - [Example `config.toml`](#example-configtoml)
- [License](#license)

---

## Overview & Key Highlights

- **100% Pure Microsoft Outlook (Mail · Calendar · Tasks · Contacts)**:
  - **Mail**: Header-first folder listing and incremental delta sync, on-demand HTML body hydration with `$batch` neighbor prefetch, conversation thread grouping (`t`), Focused/Other Inbox toggle (`F`), Markdown-to-HTML compose/reply/reply-all/forward, file & inline `cid:` attachments, and 0ms optimistic triage (`archive`, `mark-read`, `flag`, `move`, `delete`).
  - **Calendar**: `calendarView` agenda queries, event creation with Teams online meetings, RSVP (`accept` / `tentative` / `decline`), and event deletion.
  - **Tasks (Microsoft To Do)**: Task lists, due dates, importance, and instant completion toggling.
  - **Contacts & Directory**: Unified search across personal Outlook contacts (`/me/contacts`) and Entra ID organization directory (`/users`).
- **Minimal-Chrome Responsive TUI (`ratatui`)**: 3-tier responsive layout (`Wide >= 120`, `Medium 80..119`, `Narrow < 80`) across tabs `1` Mail, `2` Calendar, and `3` Tasks, unified `Ctrl+K` Omni-Switcher, Cozy/Compact density (`Ctrl+D`), quoted-reply folding (`z`), and colorblind-safe themes (`NO_COLOR`, `Ansi16`, `Ansi256`, Midnight Dark, Daylight Light).
- **Local SQLite WAL + FTS5 Cache & Daemon**: Stores folders, messages, events, tasks, and contacts in `cache.db` for instant hybrid FTS5 + `LIKE` offline search (`outlook search`) and sub-5ms CLI reads when `outlook daemon` is running.
- **Inline Terminal Image Preview (`v` / `Alt+v` / click / `outlook preview`)**: Multi-protocol high-resolution rendering for PNG, JPEG, GIF, WebP, and inline `cid:` / `https://` images with automatic terminal detection across **Windows, Linux, and macOS** (**Kitty Graphics Protocol**, **iTerm2 `OSC 1337`**, **Sixel `DCS q`**, and **24-bit Unicode Half-Blocks (`▀`)** fallback).
- **Automatic JWT Scope Diagnostics**: `outlook whoami`, `outlook doctor`, and `403 Forbidden` handlers inspect JWT `scp`/`roles` claims to flag missing Outlook permissions (such as `Mail.ReadBasic`-only tokens).
- **AI Agent Automation (`outlook serve` & `outlook mcp`)**: Local Axum HTTP JSON API (`127.0.0.1:3939`, optional `--api-key` / `--token` auth, `/openapi.json`) and newline-delimited stdio JSON-RPC 2.0 MCP server exposing **20 Microsoft Outlook tools**.
- **Cross-Platform & AlmaLinux 8 Ready**: Single static binary (bundled SQLite + `rustls`, zero OpenSSL dev dependencies). Prebuilt Linux binaries target **GLIBC 2.28** (`AlmaLinux 8`, `RHEL 8+`, `Rocky Linux 8+`, `Ubuntu 20.04+`, `Debian 10+`).

---

## Installation

### npm Global Install

Install `@shinick/outlook` globally to link the native binary for Windows, macOS, or Linux (`x64` / `arm64`):

```bash
npm install -g @shinick/outlook
outlook --version

# Or run one-off without installing globally
npx @shinick/outlook --version
```

See [docs/NPM.md](docs/NPM.md) for optional per-platform package details and zero-Node `postinstall` linking.

### Prebuilt Binaries & PowerShell Launcher

Download a prebuilt archive or binary directly from [GitHub Releases](https://github.com/shinick-dev/outlook/releases):

```powershell
# Windows: download outlook.exe directly
gh release download v0.1.0 -R shinick-dev/outlook --pattern "outlook.exe" --dir "$HOME\.local\bin" --clobber
```

If you clone the repository, you can register the PowerShell `$PROFILE` shim (`outlook`) or use the bundled wrapper scripts (`outlook.cmd` on Windows, `./outlook` on macOS/Linux):

```powershell
gh repo clone shinick-dev/outlook
cd outlook
pwsh -File .\scripts\install-profile.ps1
. $PROFILE
outlook --version
```

### Build from Source

Requires **Rust 1.80+**. SQLite (`rusqlite` `bundled`) and TLS (`rustls`) are compiled in statically—no external OpenSSL or SQLite libraries required:

```bash
cargo build --release
./target/release/outlook --help
```

CI Linux releases (`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`) are cross-compiled with `cargo-zigbuild` targeting **GLIBC 2.28** and verified with `node scripts/verify-glibc.mjs --max 2.28` on `almalinux:8`. For older systems (e.g., CentOS 7) or Alpine/musl, build from source with `cargo build --release`.

### One-Click Self-Update

`outlook update` checks GitHub Releases (`shinick-dev/outlook`), verifies `SHA256SUMS.txt`, and safely swaps the running binary (including locked executables on Windows NTFS):

```bash
outlook update --check    # Check for a newer release without installing
outlook update            # Download, verify SHA-256, and update in place
outlook update --force    # Reinstall the current release
```

---

## Authentication & External Token Brokers

### Interactive & Headless Login

```bash
# 1. Interactive browser login (OAuth 2.0 Authorization Code + PKCE)
outlook login

# 2. Headless / SSH / remote terminal login (Device Code flow)
outlook login --device-code

# 3. Supply an existing Microsoft Graph access token or token file
outlook login --access-token "$GRAPH_TOKEN"
outlook login --token-file ~/.cursor/tc-auth/access_token
# Or via environment variable:
export OUTLOOK_ACCESS_TOKEN="$GRAPH_TOKEN"

# 4. Inspect active identity, JWT scopes, auth source, and Graph connectivity
outlook whoami
outlook doctor --online

# 5. Clear stored credentials (OS keyring + fallback file)
outlook logout
```

On headless Linux without D-Bus Secret Service, `outlook` automatically falls back to a `0600` `credentials.json` file in the config directory.

### Automatic M365 Shared-Session Bootstrap (`teams-cli`)

When `share_m365_auth = true` (default, controlled by `OUTLOOK_SHARE_M365_AUTH`), `outlook` automatically checks for an existing `teams-cli` stored session (`teams-cli` OS keyring or `credentials.json`) or `TEAMS_TOKEN_FILE` whenever no dedicated `outlook-cli` token is configured:

- If the `teams-cli` session already holds Outlook scopes (`Mail.*`, `Calendars.*`, `Tasks.*`), `outlook` reuses it seamlessly (`auth_mode = "shared_teams_session"` or `"shared_teams_token_file"`).
- If the `teams-cli` session has a refresh token without Outlook scopes yet, `outlook` silently upgrades the refresh token for Outlook scopes without requiring a second browser login.
- Set `OUTLOOK_SHARE_M365_AUTH=false` (or `share_m365_auth = false` in `config.toml`) to disable cross-app session reuse.

### Microsoft Graph Delegated Scopes

`outlook` requests Microsoft Outlook delegated scopes across Mail, Calendar, Contacts, and To Do:

| Area | Scope | Purpose |
| --- | --- | --- |
| **Auth & Profile** | `offline_access` | Issue refresh tokens for silent renewal |
| | `User.Read` | Read signed-in user profile (`/me`) |
| | `User.ReadBasic.All` | Search Entra organization directory (`/users`) |
| **Mail** | `Mail.ReadWrite` | Read folders/messages, mark read/flag, move, delete, download attachments |
| | `Mail.Send` | Send, reply, reply-all, and forward emails |
| **Calendar** | `Calendars.ReadWrite` | List calendar events, create events, RSVP, cancel/delete |
| **Contacts** | `Contacts.Read` / `Contacts.ReadWrite` | List and search personal Outlook contacts |
| **Tasks** | `Tasks.ReadWrite` | List To Do task lists, create, complete, and delete tasks |

### External Token Brokers (`OUTLOOK_TOKEN_FILE`)

When an external tool (e.g., F12 browser token capture, `tc-auth`, `az account get-access-token`, HashiCorp Vault) manages your Graph token, configure `OUTLOOK_TOKEN_FILE` / `OUTLOOK_TOKEN_REFRESH_CMD` (or `config.toml`). Whenever `OUTLOOK_TOKEN_FILE` or `OUTLOOK_ACCESS_TOKEN` is active, `outlook` automatically skips OS keyring and `credentials.json` access.

**Option 1 — `config.toml` (recommended persistent setup):**

```toml
# Windows: %APPDATA%\outlook-cli\config.toml
# Linux/macOS: ~/.config/outlook-cli/config.toml
token_file = "~/.cursor/tc-auth/access_token"
token_refresh_cmd = "python ~/.cursor/tc-auth/graph-auth.py refresh"
token_refresh_mins = 5
```

**Option 2 — Environment variables (`bash` / `PowerShell`):**

```bash
export OUTLOOK_TOKEN_FILE="$HOME/.cursor/tc-auth/access_token"
export OUTLOOK_TOKEN_REFRESH_CMD="python $HOME/.cursor/tc-auth/graph-auth.py refresh"
export OUTLOOK_TOKEN_REFRESH_MINS=5
```

```powershell
$env:OUTLOOK_TOKEN_FILE = "$HOME\.cursor\tc-auth\access_token"
$env:OUTLOOK_TOKEN_REFRESH_CMD = "python `"$HOME\.cursor\tc-auth\graph-auth.py`" refresh"
$env:OUTLOOK_TOKEN_REFRESH_MINS = "5"
```

**Option 3 — Direct stdout from Azure CLI (`az account get-access-token`):**

If `token_refresh_cmd` is set without `token_file`, `outlook` parses either a raw JWT or Azure CLI camelCase JSON (`accessToken` / `expiresOn`) from `stdout`:

```toml
token_refresh_cmd = "az account get-access-token --resource https://graph.microsoft.com --output json"
token_refresh_mins = 5
```

```bash
OUTLOOK_TOKEN_REFRESH_CMD="az account get-access-token --resource https://graph.microsoft.com --output json" outlook whoami
```

### Custom Entra ID App Registration

By default `outlook` uses its built-in multi-tenant public client ID (`14d82eec-204b-4c2f-b7e8-296a70dab67e`). If your organization requires an internal Entra ID app registration, add the delegated scopes above, enable **Public client flows** with redirect URI `http://localhost` under **Mobile and desktop applications**, and pass your IDs:

```bash
outlook login --client-id "<AZURE_APP_CLIENT_ID>" --tenant-id "<TENANT_ID>"

# Or via environment variables (override config.toml):
export OUTLOOK_CLIENT_ID="<AZURE_APP_CLIENT_ID>"
export OUTLOOK_TENANT_ID="<TENANT_ID>"   # default: common
```

You can also override default scopes via `scopes = [...]` in `config.toml`.

---

## Minimal-Chrome TUI Guide

Launch the interactive TUI by running `outlook` (or `outlook tui`):

```bash
outlook
```

### Layout, Density & Themes

- **Minimal-Chrome 3-Tier Responsive Layout**:
  - **Wide (`>= 120` cols)**: Left navigation rail (Mail folders / Calendar / To Do lists) + middle message/agenda/task list + right reader & detail pane.
  - **Medium (`80..119` cols)**: Compact list + reader workspace (`Ctrl+B` toggles sidebar).
  - **Narrow (`< 80` cols)**: Single-pane focus view with a compact top bar.
- **Cozy & Compact Density (`Ctrl+D`)**: Toggle between spacious multi-line message rows (**Cozy**) and high-density single-line rows (**Compact**).
- **Colorblind-Safe Multi-Depth Themes**: Automatically detects `NO_COLOR`, `Ansi16`, `Ansi256`, and Truecolor terminals (`Midnight Dark` & `Daylight Light`), or select a preset in Settings (`Ctrl+,`) or `config.toml`.

### Key Shortcuts

| Key | Context | Action |
| --- | --- | --- |
| `1` / `2` / `3` (`Alt+1..3`) | Global | Switch to **Mail** / **Calendar** / **Tasks & Contacts** tab |
| `Ctrl+K` | Global | **Unified Omni-Switcher** (`>` commands, `#` folders, `@` contacts, `/` search) |
| `j` / `k` / `↑` / `↓` | Global | Navigate folders, message list, agenda, or scroll reading pane |
| `Tab` / `Enter` / `Esc` | Global | Cycle focus (`Folders <-> List <-> Reader`) or close modal |
| `Space` / `PgDn` / `PgUp` | List / Reader | Page-scroll reading pane directly from the list |
| `c` | Mail | **Compose** new email (`To`/`Cc`/`Bcc` contact autocomplete, Markdown body, `Ctrl+V` inline image) |
| `r` / `R` / `f` | Mail | **Reply** / **Reply-All** / **Forward** selected email |
| `e` | Mail | **Archive** selected email (optimistic 0ms UI update) |
| `u` / `g` | Mail | Toggle **Read/Unread** (`u`) or **Flag/Unflag** (`g`) |
| `m` | Mail | **Move** email to folder (fuzzy folder picker) |
| `Del` / `#` | Mail | Delete email (with confirmation) |
| `t` / `F` / `U` | Mail | Toggle **Thread grouping** (`t`), **Focused Inbox** (`F`), or **Unread filter** (`U`) |
| `z` | Mail Reader | **Fold / unfold** quoted email reply history |
| `v` / `Alt+v` | Mail Reader | Open high-resolution **inline terminal image preview** (`+`/`-`/`0`, `p` protocol, `[`/`]`, `d` save) |
| `d` / `A` / `o` | Mail Reader | Download attachment (`d`), open attachment picker (`A`), or open link (`o`) |
| `n` | Calendar / Tasks | Create **New Event** (Calendar) or **New Task / To Do List** (Tasks) |
| `a` / `y` / `x` | Calendar | RSVP **Accept** (`a`), **Tentative** (`y`), or **Decline** (`x`) |
| `Space` / `Enter` | Tasks | Toggle task completion (0ms optimistic UI) |
| `/` / `Ctrl+F` | Global | Search mail (SQLite FTS5 local + Graph KQL remote) |
| `Ctrl+B` / `Ctrl+D` | Global | Toggle left sidebar (`Ctrl+B`) / Cozy-Compact density (`Ctrl+D`) |
| `Ctrl+R` / `F5` | Global | Manual sync / refresh all domains |
| `Ctrl+,` / `?` / `Ctrl+Q` | Global | Settings / Help overlay / Safe quit |

Compose modal shortcuts (`c`, `r`, `R`, `f`): `Ctrl+Enter` or `Alt+Enter` to send, `Tab`/`Enter` on `To`/`Cc`/`Bcc` to insert autocomplete contact chip, `Ctrl+V` to paste text or clipboard screenshot (`cid:`), `Ctrl+A` to attach a file, `Ctrl+E` to edit in `$EDITOR`, `Ctrl+S` to save draft (`Esc` auto-stashes locally).

### Quoted-Reply Folding & Inline Image Preview

- Pressing `z` in the Mail reader collapses or expands nested quoted email threads (`QuotedHistory`) so long reply chains stay readable.
- Pressing `v` / `Alt+v` (or clicking an `[IMG ...]` chip or inline thumbnail) renders attached or inline `cid:`/`https://` images with automatic terminal protocol detection across **Windows, Linux, and macOS**:
  - **Kitty Graphics Protocol** (Kitty, Ghostty)
  - **iTerm2 Inline Images (`OSC 1337`)** (WezTerm on Windows/Linux/macOS, iTerm2 on macOS, Rio, Mintty)
  - **Sixel (`DCS q`)** (Windows Terminal v1.22+, foot, Contour, mlterm)
  - **24-bit Unicode Half-Blocks (`▀`)** universal fallback (tmux/SSH/legacy consoles)
- Configure the default globally via `image_protocol = "auto"` in `config.toml`, `OUTLOOK_IMAGE_PROTOCOL`, or `outlook preview --protocol <PROTOCOL>`.

---

## CLI Reference

### Subcommands

| Command | Alias | Description | Key Options |
| --- | --- | --- | --- |
| `outlook` | - | Launch the TUI (default when no subcommand is given) | - |
| `outlook tui` | - | Launch the interactive TUI | - |
| `outlook login` | `in` | Sign in with Microsoft Entra ID (PKCE, Device Code, or token) | `--client-id`, `--tenant-id`, `--device-code`, `--client-secret`, `--access-token` (`--token`), `--token-file`, `--port`, `--json` |
| `outlook logout` | `out` | Remove stored credentials from keyring & fallback file | `--json` |
| `outlook whoami` | - | Show current profile, token expiry, scopes, and auth source | `--json` |
| `outlook doctor` | - | Show config/DB/keyring paths, image protocol, and diagnostics | `--online` (`--probe`), `--json` |
| `outlook update` | - | Check GitHub Releases and self-update binary | `--check`, `--force`, `--repo`, `--json` |
| `outlook folders` | - | List Outlook mail folders with unread/total counts | `--refresh`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook mail` | `ls`, `inbox` | List messages in Inbox or a specified folder | `--folder <F>`, `--limit <N>`, `--unread`, `--flagged`, `--focused`, `--refresh`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook read <id>` | `cat` | Read a mail message with full headers, body, and attachments | `--raw-html`, `--mark-read`, `--preview-images`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook send` | `compose` | Compose and send a new email (Markdown + attachments) | `--to <ADDR>`, `--cc`, `--bcc`, `--subject <S>`, `--body <MD>`, `--attach <PATH>`, `--inline <IMG>`, `--importance <low\|normal\|high>`, `--json` |
| `outlook reply <id> <md>` | - | Reply (or reply-all) to a mail message | `--all`, `--json` |
| `outlook forward <id> [md]` | - | Forward a mail message | `--to <ADDR>`, `--json` |
| `outlook mark <id>` | - | Update read/unread or flag status | `--read` / `--unread`, `--flag` / `--unflag`, `--json` |
| `outlook move <id> <folder>` | - | Move a mail message to another folder | `--json` |
| `outlook archive <id>` | - | Archive a mail message (`move <id> archive`) | `--json` |
| `outlook delete <id>` | `rm` | Delete a mail message (`--yes` required with `--json`) | `--yes`, `--json` |
| `outlook search <query>` | - | Search local SQLite FTS5 or Graph KQL `$search` | `--folder <F>`, `--remote`, `--limit <N>`, `--json` |
| `outlook download <id>` | `dl` | Download a mail attachment to disk | `--attachment <IDX_OR_NAME>`, `--dir`/`--out <DIR>`, `--json` |
| `outlook preview <id>` | - | Render a mail image attachment in the terminal | `--attachment <IDX_OR_NAME>`, `--offline`, `--protocol`, `--cols`, `--rows` |
| `outlook preview-file <path>` | - | Render a local image file in the terminal | `--protocol`, `--cols`, `--rows` |
| `outlook calendar` | `cal`, `events` | List upcoming calendar events | `--days <N>`, `--from`/`--start`, `--to`/`--end`, `--limit <N>`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook event <id>` | - | Read full details and attendees of a calendar event | `--offline`, `--cached`/`--fast`, `--json` |
| `outlook create-event` | - | Create a new calendar event | `--subject`, `--start`, `--end`, `--location`, `--attendee`/`--to`, `--body`, `--online`, `--all-day`, `--time-zone`, `--json` |
| `outlook rsvp <id> <resp>` | - | Respond to a calendar invite (`accept`, `tentative`, `decline`) | `--comment <TEXT>`, `--json` |
| `outlook delete-event <id>` | - | Delete a calendar event (`--yes` required with `--json`) | `--yes`, `--json` |
| `outlook tasks` | `todo` | List Microsoft To Do tasks | `--list <NAME_OR_ID>`, `--all`, `--limit <N>`, `--refresh`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook task-lists` | - | List Microsoft To Do task lists | `--refresh`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook add-task <title>` | - | Add a new Microsoft To Do task | `--list`, `--due <YYYY-MM-DD>`, `--importance <low\|normal\|high>`, `--notes`/`--body`, `--json` |
| `outlook done <task_id>` | - | Mark a Microsoft To Do task completed (or reopen with `--undo`) | `--list`, `--undo`, `--json` |
| `outlook delete-task <task_id>` | - | Delete a Microsoft To Do task (`--yes` required with `--json`) | `--list`, `--yes`, `--json` |
| `outlook contacts` | - | List or search personal Outlook contacts and org directory | `--query`/`--q`, `--limit <N>`, `--refresh`, `--offline`, `--cached`/`--fast`, `--json` |
| `outlook sync` | - | Full sync of folders, inbox, calendar, tasks, and contacts | `--days <N>`, `--limit <N>`, `--json` |
| `outlook daemon <cmd>` | - | Control the background sync daemon (`start`, `status`, `stop`, `restart`, `run`) | `--json` (on `start`/`status`/`stop`/`restart`) |
| `outlook serve` | `api` | Start the local HTTP JSON API server | `--bind <IP>`, `--port <3939>`, `--api-key`/`--token <KEY>` |
| `outlook mcp` | `mcp-server` | Start the stdio MCP (JSON-RPC 2.0) server | - |

### Common CLI Workflows

```bash
# 1. Inspect folders, read unread inbox mail, and reply
outlook folders
outlook mail --folder inbox --unread --limit 20
outlook read AAMkAGI2... --mark-read
outlook reply AAMkAGI2... "Thanks, **approved** for release." --all

# 2. Send an email with Markdown, attachment, and inline image
outlook send --to alice@contoso.com --cc bob@contoso.com \
  --subject "v0.1.0 Release Summary" \
  --body "See attached report and latency chart." \
  --attach ./report.pdf \
  --inline ./latency.png \
  --importance high

# 3. Triage, search, and archive
outlook mark AAMkAGI2... --flag --read
outlook move AAMkAGI2... archive
outlook search "quarterly review" --remote --json

# 4. Calendar agenda, event creation, and RSVP
outlook calendar --days 7
outlook create-event --subject "Sprint Review" \
  --start "2026-11-20T14:00:00Z" --end "2026-11-20T15:00:00Z" \
  --attendee alice@contoso.com --online
outlook rsvp EVENT_ID accept --comment "Joining remotely"

# 5. Microsoft To Do tasks, contacts, and background daemon
outlook add-task "Verify GLIBC 2.28 build" --due 2026-11-20 --importance high
outlook done TASK_ID
outlook contacts --query alice --json
outlook daemon start
outlook mail --cached --json
```

---

## HTTP Server (`outlook serve`)

Run a local Axum HTTP JSON API server for scripts, webhooks, and AI agents (defaults to `127.0.0.1:3939`):

```bash
outlook serve                                 # http://127.0.0.1:3939
outlook serve --port 9000 --api-key secret123 # Require Bearer/x-api-key (except /health)
```

```bash
curl http://127.0.0.1:3939/health
curl http://127.0.0.1:3939/v1/mail/messages?folder=inbox&unread=true
curl -X POST http://127.0.0.1:3939/v1/mail/send \
  -H "Content-Type: application/json" \
  -d '{"to":["alice@contoso.com"],"subject":"Build green","markdown":"All checks passed!"}'
```

Key endpoints:
- `GET /health`, `GET /openapi.json`, `GET /v1/whoami`
- `GET /v1/mail/folders`, `GET /v1/mail/messages`, `GET / PATCH / DELETE /v1/mail/messages/{id}`
- `POST /v1/mail/send`, `POST /v1/mail/messages/{id}/reply`, `POST /v1/mail/messages/{id}/forward`
- `POST /v1/mail/messages/{id}/attachments/{att_id}/download`, `GET /v1/mail/search`
- `GET / POST /v1/calendar/events`, `GET / DELETE /v1/calendar/events/{id}`, `POST /v1/calendar/events/{id}/rsvp`
- `GET /v1/todo/lists`, `GET / POST /v1/todo/tasks`, `PATCH / DELETE /v1/todo/tasks/{id}`
- `GET /v1/contacts`

See [docs/API.md](docs/API.md) and [docs/openapi.yaml](docs/openapi.yaml) for the complete HTTP API reference.

---

## MCP Server (`outlook mcp`)

`outlook mcp` (alias `outlook mcp-server`) runs a **newline-delimited JSON-RPC 2.0** Model Context Protocol server over **stdio** (`stdout` is strictly reserved for JSON-RPC frames; logs go to `stderr`). Authenticate first with `outlook login` or configure `OUTLOOK_ACCESS_TOKEN` / `OUTLOOK_TOKEN_FILE` (or automatic `teams-cli` shared M365 session bootstrap).

### Claude Desktop (`claude_desktop_config.json`)

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

Or point directly to a repository launcher / binary without installing globally:

```json
{
  "mcpServers": {
    "outlook": {
      "command": "C:\\path\\to\\outlook\\outlook.cmd",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code

```bash
claude mcp add outlook -- outlook mcp
```

### OpenAI Codex CLI (`~/.codex/config.toml`)

```toml
[mcp_servers.outlook]
command = "outlook"
args = ["mcp"]
```

**20 exposed MCP tools:** `whoami`, `list_mail_folders`, `list_messages`, `get_message`, `send_mail`, `reply_mail`, `forward_mail`, `update_message`, `delete_message` (`confirm: true`), `download_attachment`, `search_mail`, `list_events`, `create_event`, `respond_event`, `delete_event` (`confirm: true`), `list_todo_lists`, `list_tasks`, `create_task`, `complete_task`, and `search_contacts`. See [docs/MCP.md](docs/MCP.md) for request/response schemas.

---

## Environment Variables & Configuration

### Storage & Config Locations

| Item | Default Path (Windows / Linux & macOS) | Override |
| --- | --- | --- |
| Configuration (`config.toml`) | `%APPDATA%\outlook-cli\config.toml` / `~/.config/outlook-cli/config.toml` | `OUTLOOK_CONFIG_DIR` |
| Unsent drafts (`drafts.json`) | `<config_dir>\drafts.json` | `OUTLOOK_CONFIG_DIR` |
| Credentials | OS Keyring (`outlook-cli` / `default`) + fallback `<config_dir>\credentials.json` | `OUTLOOK_DISABLE_KEYRING=1` |
| SQLite cache (`cache.db`) | `%LOCALAPPDATA%\outlook-cli\<user_id>\cache.db` / `~/.local/share/outlook-cli/<user_id>/cache.db` | `OUTLOOK_DATA_DIR` |
| Downloads folder | `~/Downloads/outlook` (or `download_dir` in `config.toml`) | - |
| Graph / Auth base URLs | `https://graph.microsoft.com/v1.0` / `https://login.microsoftonline.com` | `OUTLOOK_GRAPH_BASE_URL`, `OUTLOOK_AUTH_BASE_URL` |
| Self-update repository | `shinick-dev/outlook` | `OUTLOOK_GITHUB_REPO` |

### Environment Variables

| Variable | Description |
| --- | --- |
| `OUTLOOK_CLIENT_ID` | Override custom Entra ID application (client) ID (falls back to `TEAMS_CLIENT_ID`, `AZURE_CLIENT_ID`) |
| `OUTLOOK_TENANT_ID` | Override Entra ID tenant ID (default: `common`; falls back to `TEAMS_TENANT_ID`, `AZURE_TENANT_ID`) |
| `OUTLOOK_ACCESS_TOKEN` | Inject a Bearer access token directly (skips OS keyring lookup) |
| `OUTLOOK_TOKEN_FILE` | Path to external token file (raw JWT or JSON; skips OS keyring lookup) |
| `OUTLOOK_TOKEN_REFRESH_CMD` | External command hook to refresh tokens before expiry or on `401` |
| `OUTLOOK_TOKEN_REFRESH_MINS` | Minutes before `exp` to proactively run `OUTLOOK_TOKEN_REFRESH_CMD` (default: `5`) |
| `OUTLOOK_SHARE_M365_AUTH` | Reuse `TEAMS_TOKEN_FILE` or `teams-cli` stored session when no `outlook-cli` token exists (default: `true`) |
| `OUTLOOK_DISABLE_KEYRING` | Set to `1` to disable OS keyring and store credentials in `credentials.json` |
| `OUTLOOK_CLI_CACHE_SECS` | Local SQLite cache TTL in seconds for CLI reads (default: `0`; `--cached`/`--fast` prefers cache) |
| `OUTLOOK_AUTO_DAEMON` | Set to `1`/`true` to auto-start and attach to `outlook daemon` for sub-5ms CLI reads (default: `false`) |
| `OUTLOOK_IMAGE_PROTOCOL` | Override terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`) |
| `OUTLOOK_CONFIG_DIR` | Override directory for `config.toml`, `credentials.json`, and `drafts.json` |
| `OUTLOOK_DATA_DIR` | Override directory for the SQLite cache (`cache.db`) |
| `OUTLOOK_NO_BROWSER` | Set to `1`/`true` to disable automatic browser launch during `outlook login` |
| `OUTLOOK_GRAPH_BASE_URL` | Override Microsoft Graph v1.0 base URL |
| `OUTLOOK_AUTH_BASE_URL` | Override Entra ID OAuth 2.0 base URL |
| `OUTLOOK_GITHUB_REPO` | Override GitHub `owner/repo` used by `outlook update` |
| `OUTLOOK_GITHUB_TOKEN` | GitHub token for private release checks in `outlook update` |
| `OUTLOOK_ALLOW_INSECURE_SERVE` | Set to `1` to permit non-loopback `outlook serve` binds without `--api-key` |
| `NO_COLOR` | Set to any non-empty value to force monochrome `NoColor` TUI rendering |

### Example `config.toml`

```toml
client_id = "14d82eec-204b-4c2f-b7e8-296a70dab67e"
tenant_id = "common"
density = "cozy"           # "cozy" | "compact"
theme = "auto"             # "auto" | "dark" | "light" | "ansi256" | "ansi16" | "nocolor"
notify = true
show_nav_rail = true
group_threads = true
focused_only = false
inline_images = true
image_protocol = "auto"    # "auto" | "kitty" | "iterm2" | "sixel" | "halfblock"
download_dir = ""
share_m365_auth = true
cli_cache_ttl_secs = 0
auto_daemon = false
token_file = ""
token_refresh_cmd = ""
token_refresh_mins = 5
```

---

## License

MIT
