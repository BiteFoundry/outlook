# teams (Pure Microsoft Teams CLI · TUI · HTTP API · MCP)

**`teams`** is a pure **Microsoft Teams** terminal client and AI agent toolkit built in Rust. Read conversations at local-cache speed, navigate unlimited history, search offline with SQLite FTS5, upload files and inline screenshots, manage reactions and presence, and expose Teams to coding agents over HTTP or MCP—all from a single native binary.

> One standalone executable (`teams` / `teams.exe`) bundles OAuth 2.0 keyring credentials, a **SQLite WAL + FTS5 local cache (`cache.db`)**, a Minimal-Chrome **`ratatui` TUI**, a background sync daemon (`teams daemon`), an **Axum HTTP REST API (`teams serve`)**, and a **stdio JSON-RPC 2.0 MCP server (`teams mcp`)**. Prebuilt binaries ship for Windows, macOS, and Linux (`x86_64` / `aarch64`, compatible with **AlmaLinux 8 / GLIBC 2.28+**).

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
  - [Microsoft Graph Delegated Scopes](#microsoft-graph-delegated-scopes)
  - [External Token Brokers](#external-token-brokers)
  - [Custom Entra ID App Registration](#custom-entra-id-app-registration)
- [Redesigned TUI Guide](#redesigned-tui-guide)
  - [Layout, Density & Themes](#layout-density--themes)
  - [Composer-First Workflow](#composer-first-workflow)
  - [Key Shortcuts](#key-shortcuts)
  - [Deep History & Inline Image Preview](#deep-history--inline-image-preview)
- [CLI Reference](#cli-reference)
  - [Subcommands](#subcommands)
  - [Common CLI Workflows](#common-cli-workflows)
- [HTTP Server (`teams serve`)](#http-server-teams-serve)
- [MCP Server (`teams mcp`)](#mcp-server-teams-mcp)
- [Environment Variables & Configuration](#environment-variables--configuration)
  - [Storage & Config Locations](#storage--config-locations)
  - [Environment Variables](#environment-variables)
  - [Example `config.toml`](#example-configtoml)
- [License](#license)

---

## Overview & Key Highlights

- **100% Pure Microsoft Teams**: 1:1, group, and meeting chats; joined teams, channels, and threaded replies; Markdown formatting and `@mentions`; OneDrive/SharePoint file uploads and inline clipboard images; attachment downloads; reactions; message edit/delete; and live Presence.
- **Minimal-Chrome Responsive TUI (`ratatui`)**: 3-tier responsive layout (`Wide >= 120`, `Medium 80..119`, `Narrow < 80`), **Composer-First** input workflow, Cozy 5-minute author grouping and Compact gutter density (`Ctrl+D`), `─── N unread messages ───` divider, unified `Ctrl+K` Omni-Switcher, and colorblind-safe multi-depth themes (`NO_COLOR`, `Ansi16`, `Ansi256`, Truecolor Dark/Light).
- **Deep History Pagination (`@odata.nextLink`)**: Persists Microsoft Graph `$skiptoken` cursors in SQLite `sync_cursors` with deterministic timestamp tie-breaking, automatic recovery on expired tokens, CLI `teams read --before` / `teams sync --depth`, and seamless TUI scroll-up backfill.
- **Local SQLite WAL + FTS5 Cache & Daemon**: Stores chats, channels, threads, and messages locally in `cache.db` for instant hybrid trigram + `LIKE` offline search (`teams search`) and sub-5ms CLI reads when `teams daemon` is running.
- **Inline Terminal Image Preview (`v` / `Alt+v` / click / `teams preview`)**: Multi-protocol high-resolution rendering for PNG, JPEG, GIF, WebP, and pasted `hostedContents` inline screenshots with automatic viewport thumbnails, terminal detection & routing across **Windows, Linux, and macOS** (**Kitty Graphics Protocol**, **iTerm2 `OSC 1337`**, **Sixel `DCS q`**, and **24-bit Unicode Half-Blocks (`▀`)** fallback).
- **Automatic JWT Scope Diagnostics**: `teams whoami`, `teams doctor`, and `403 Forbidden` handlers inspect JWT `scp`/`roles` claims to flag restricted tokens (such as `Chat.ReadBasic`-only tokens that cannot read message bodies).
- **AI Agent Automation (`teams serve` & `teams mcp`)**: Local Axum HTTP JSON API (`127.0.0.1:8788`, optional `--token` Bearer auth) and newline-delimited stdio JSON-RPC 2.0 MCP server exposing 17 Microsoft Teams tools.
- **Cross-Platform & AlmaLinux 8 Ready**: Single static binary (bundled SQLite + `rustls`, zero OpenSSL dev dependencies). Prebuilt Linux binaries target **GLIBC 2.28** (`AlmaLinux 8`, `RHEL 8+`, `Rocky Linux 8+`, `Ubuntu 20.04+`, `Debian 10+`).

---

## Installation

### npm Global Install

Install `@shinick/teams` globally to link the native binary for Windows, macOS, or Linux (`x64` / `arm64`):

```bash
npm install -g @shinick/teams
teams --version

# Or run one-off without installing globally
npx @shinick/teams --version
```

See [docs/NPM.md](docs/NPM.md) for optional per-platform package details.

### Prebuilt Binaries & PowerShell Launcher

Download a prebuilt archive or binary directly from [GitHub Releases](https://github.com/shinick-dev/teams/releases):

```powershell
# Windows: download teams.exe directly
gh release download v0.6.0 -R shinick-dev/teams --pattern "teams.exe" --dir "$HOME\.local\bin" --clobber
```

If you clone the repository, you can register the PowerShell `$PROFILE` shim (`teams`) or use the bundled wrapper scripts (`teams.cmd` on Windows, `./teams` on macOS/Linux):

```powershell
gh repo clone shinick-dev/teams
cd teams
pwsh -File .\scripts\install-profile.ps1
. $PROFILE
teams --version
```

### Build from Source

Requires **Rust 1.80+**. SQLite (`rusqlite` `bundled`) and TLS (`rustls`) are compiled in statically—no external OpenSSL or SQLite libraries required:

```bash
cargo build --release
./target/release/teams --help
```

CI Linux releases (`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`) are cross-compiled with `cargo-zigbuild` targeting **GLIBC 2.28** and verified with `node scripts/verify-glibc.mjs --max 2.28` on `almalinux:8`. For older systems (e.g., CentOS 7) or Alpine/musl, build from source with `cargo build --release`.

### One-Click Self-Update

`teams update` checks GitHub Releases (`shinick-dev/teams`), verifies `SHA256SUMS.txt`, and safely swaps the running binary (including locked executables on Windows NTFS):

```bash
teams update --check    # Check for a newer release without installing
teams update            # Download, verify SHA-256, and update in place
teams update --force    # Reinstall the current release
```

---

## Authentication & External Token Brokers

### Interactive & Headless Login

```bash
# 1. Interactive browser login (OAuth 2.0 Authorization Code + PKCE)
teams login

# 2. Headless / SSH / remote terminal login (Device Code flow)
teams login --device-code

# 3. Supply an existing Microsoft Graph access token directly
teams login --token "$GRAPH_TOKEN"
# Or via environment variable:
export TEAMS_ACCESS_TOKEN="$GRAPH_TOKEN"

# 4. Inspect active identity, JWT scopes, and Graph connectivity
teams whoami
teams doctor --probe

# 5. Clear stored credentials (OS keyring + fallback file)
teams logout
```

On headless Linux without D-Bus Secret Service, `teams` automatically falls back to a `0600` `credentials.json` file in the config directory.

### Microsoft Graph Delegated Scopes

`teams` requests only pure Microsoft Teams delegated scopes (no Outlook/OneNote scopes):

| Area | Scope | Purpose |
| --- | --- | --- |
| **Auth & Profile** | `offline_access` | Issue refresh tokens for silent renewal |
| | `User.Read` | Read signed-in user profile (`/me`) |
| | `User.ReadBasic.All` | Look up colleagues for `@mentions` and new chats (`/users`) |
| **Chats** | `Chat.ReadWrite` | List 1:1/group/meeting chats; read, send, edit, delete, react, mark read |
| **Teams & Channels** | `Team.ReadBasic.All` | List joined teams (`/me/joinedTeams`) |
| | `Channel.ReadBasic.All` | List channels and channel members |
| | `ChannelMessage.Read.All` | Read channel posts, delta syncs, and thread replies |
| | `ChannelMessage.Send` | Post channel threads, replies, edits, deletes, and reactions |
| **Files & Presence** | `Files.ReadWrite.All` | Upload/download files in OneDrive Chat Files and SharePoint channel folders |
| | `Presence.ReadWrite` | Read and update user Presence status |

> **Note on `Chat.ReadBasic`-only tokens:** External token brokers sometimes issue tokens with `Chat.ReadBasic` instead of `Chat.Read` / `Chat.ReadWrite`. That lets Graph list `/me/chats`, but **reading message bodies and history (`/chats/{id}/messages`) fails with `403 Forbidden`**. `teams whoami`, `teams doctor`, and error diagnostics inspect JWT `scp`/`roles` claims and alert you immediately when message-read scopes are missing.

### External Token Brokers

When an external tool (e.g., `tc-auth`, `az account get-access-token`, HashiCorp Vault) manages your Graph token, configure `TEAMS_TOKEN_FILE` / `TEAMS_TOKEN_REFRESH_CMD` (or `config.toml`). Whenever `TEAMS_TOKEN_FILE` or `TEAMS_ACCESS_TOKEN` is active, `teams` automatically skips OS keyring and `credentials.json` access.

**Option 1 — `config.toml` (recommended persistent setup):**

```toml
# Windows: %APPDATA%\teams-cli\config.toml
# Linux/macOS: ~/.config/teams-cli/config.toml
token_file = "~/.cursor/tc-auth/access_token"
token_refresh_cmd = "python ~/.cursor/tc-auth/graph-auth.py refresh"
token_refresh_mins = 5
```

**Option 2 — Environment variables (`bash` / `PowerShell`):**

```bash
export TEAMS_TOKEN_FILE="$HOME/.cursor/tc-auth/access_token"
export TEAMS_TOKEN_REFRESH_CMD="python $HOME/.cursor/tc-auth/graph-auth.py refresh"
export TEAMS_TOKEN_REFRESH_MINS=5
```

```powershell
$env:TEAMS_TOKEN_FILE = "$HOME\.cursor\tc-auth\access_token"
$env:TEAMS_TOKEN_REFRESH_CMD = "python `"$HOME\.cursor\tc-auth\graph-auth.py`" refresh"
$env:TEAMS_TOKEN_REFRESH_MINS = "5"
```

**Option 3 — Direct stdout from Azure CLI (`az account get-access-token`):**

If `token_refresh_cmd` is set without `token_file`, `teams` parses either a raw JWT or Azure CLI camelCase JSON (`accessToken` / `expiresOn`) from `stdout`:

```toml
token_refresh_cmd = "az account get-access-token --resource https://graph.microsoft.com --output json"
token_refresh_mins = 5
```

```bash
TEAMS_TOKEN_REFRESH_CMD="az account get-access-token --resource https://graph.microsoft.com --output json" teams whoami
```

### Custom Entra ID App Registration

By default `teams` uses its built-in public client ID. If your organization requires an internal Entra ID app registration, add the delegated scopes above, enable **Public client flows** with redirect URI `http://localhost` under **Mobile and desktop applications**, and pass your IDs:

```bash
teams login --client-id "<AZURE_APP_CLIENT_ID>" --tenant-id "<TENANT_ID>"

# Or via environment variables (override config.toml):
export TEAMS_CLIENT_ID="<AZURE_APP_CLIENT_ID>"
export TEAMS_TENANT_ID="<TENANT_ID>"   # default: common
```

You can also override default scopes via `scopes = [...]` in `config.toml`.

---

## Redesigned TUI Guide

Launch the interactive TUI by running `teams` (or `teams tui` / `teams chat`):

```bash
teams
```

### Layout, Density & Themes

- **Minimal-Chrome 3-Tier Responsive Layout**:
  - **Wide (`>= 120` cols)**: Left navigation rail + conversation sidebar + main message stream & composer + optional right thread drawer.
  - **Medium (`80..119` cols)**: Compact sidebar + main workspace (chat or thread view).
  - **Narrow (`< 80` cols)**: Single-pane focus view with a compact top bar; switch panes smoothly without visual clutter.
- **Cozy & Compact Density (`Ctrl+D`)**:
  - **Cozy (default)**: Groups consecutive messages from the same author within a **5-minute** window cleanly under one header.
  - **Compact**: Single-line gutter layout for high-density scanning.
- **Unread Divider**: Marks your reading boundary inline with `─── N unread messages ───`.
- **Colorblind-Safe Multi-Depth Themes**: Automatically detects `NO_COLOR`, `Ansi16`, `Ansi256`, and Truecolor terminals (`Midnight Dark` & `Daylight Light`), or select a preset in Settings (`Ctrl+,`) or `config.toml`.

### Composer-First Workflow

Opening any chat or channel **immediately focuses the Composer** so you can start typing right away—zero extra `i` keystrokes required. While staying focused in the Composer, you can still scroll message history freely without losing your draft:

- **`PgUp` / `PgDn`**: Page through conversation history while typing.
- **`Ctrl+Up` / `Ctrl+Down`**: Scroll history line by line.
- **`Up` / `Down` (when Composer input is empty)**: Scroll history directly from the empty input box.
- **Mouse wheel**: Scroll the sidebar, message stream, or thread drawer at any time (clicking focuses/selects cleanly).

### Key Shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+K` | **Unified Omni-Switcher**: jump to chats/channels, type `@` for DMs, `>` for commands, `/` for search |
| `1` / `2` / `3` (`Alt+1..3`) | Switch to **Chats** / **Teams** / **Activity** tab (`Alt+1..3` works directly from Composer too) |
| `u` / `Alt+u` | Jump to next unread conversation |
| `` ` `` | Bounce back to your last active chat |
| `Alt+Up` / `Alt+Down` | Switch to previous / next room in sidebar |
| `Alt+Left` / `Alt+Right` | Navigate conversation history back / forward |
| `Ctrl+D` | Toggle **Cozy** / **Compact** message density |
| `Ctrl+B` | Toggle left sidebar |
| `Alt+T` | Toggle Composer target between **Thread reply** and **Channel post** |
| `Ctrl+S` | Toggle channel post **Subject** field |
| `Ctrl+V` | Paste clipboard text or **inline screenshot image** |
| `Ctrl+O` | Open file picker and upload/send attachment |
| `Ctrl+P` | Change your **Presence** status |
| `Ctrl+Q` | Safe quit |
| `?` | Open keybindings & help overlay |

Additional composer, message, and history controls:

| Context | Key | Action |
| --- | --- | --- |
| **Composer** | `Enter` | Send message |
| | `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | Insert newline |
| | `@` | Trigger `@mention` member autocomplete |
| | `Ctrl+E` | Edit draft in external `$VISUAL` / `$EDITOR` |
| | `Ctrl+Z` / `Ctrl+Y` | Undo / Redo |
| | `Esc` | Cancel reply/mention popup or return focus to message stream |
| **Message Stream** | `t` / `Enter` | Open thread drawer for selected channel post |
| | `r` | Quote-reply / reply to selected message |
| | `v` / `Alt+v` | Open inline terminal image preview (`Alt+v` works directly from Composer too; `+`/`-`/`0`/scroll zoom, `p` cycle protocol, `[`/`]` prev/next, `d` save) |
| | `e` / `+` / `=` | Open reaction picker (`like`, `heart`, `laugh`, `surprised`, `sad`, `angry`) |
| | `d` / `[` / `]` | Download attachment / cycle attachments |
| | `y` | Copy selected message body |
| | `Del` | Delete your own message (with confirmation) |
| | `o` / `H` | Load older page (`o`) / 150+ deep history backfill (`H`) |
| **Global Overlays** | `/` / `Ctrl+F` | Search messages |
| | `Ctrl+N` | Start new 1:1 or group chat |
| | `Ctrl+U` | Room info & members |
| | `Ctrl+,` | Settings (theme, density, notifications) |

### Deep History & Inline Image Preview

- Scrolling to the top of any chat or channel (`PgUp`, `Up`, or mouse wheel up) automatically fetches older pages via `@odata.nextLink` while anchoring your scroll position. Press `o` to fetch one page on demand or `H` for a 150+ message deep backfill.
- Scrolling to the bottom of the **Chats** list automatically pages beyond the initial 50 conversations.
- Pressing `v` (or `Alt+v` directly from the Composer, or left-clicking an `[IMG ...]` chip or inline thumbnail) on a message with an image attachment or pasted `hostedContents` screenshot opens a high-resolution inline preview with automatic terminal detection and routing across **Windows, Linux, and macOS**:
  - **Kitty Graphics Protocol** (Kitty, Ghostty)
  - **iTerm2 Inline Images (`OSC 1337`)** (WezTerm on Windows/Linux/macOS, iTerm2 on macOS, Rio, Mintty)
  - **Sixel (`DCS q`)** (Windows Terminal v1.22+, foot, Contour, mlterm)
  - **24-bit Unicode Half-Blocks (`▀`)** universal fallback (tmux/SSH/legacy consoles and fast-scrolling inline chat thumbnails)
- **TUI Image Preview Controls**: `v` / `Alt+v` or click `[IMG ...]` / thumbnail to open, `+` / `-` / `0` or mouse wheel to zoom, `p` to cycle protocol live (`auto` / `kitty` / `iterm2` / `sixel` / `halfblock`), `[` / `]` for previous/next attachment, and `d` to save to disk.
- Configure the default globally via `image_protocol = "auto"` in `config.toml`, `TEAMS_IMAGE_PROTOCOL`, or per-command via `teams preview --protocol <PROTOCOL>` (and inspect detected capabilities with `teams doctor`).

---

## CLI Reference

Most commands accept `<target>` as a canonical `conv_key` (`chat:...`, `chan:<team_id>:<channel_id>`), a raw `chat_id` or `channel_id`, `"Team/Channel"` (e.g. `"Engineering/General"`), or a unique case-insensitive conversation name substring (e.g. `"Alice Chen"`).

### Subcommands

| Command | Alias | Description | Key Options |
| --- | --- | --- | --- |
| `teams` | - | Launch the TUI (default when no subcommand is given) | - |
| `teams tui` | `chat` | Launch the interactive TUI | - |
| `teams login` | `in` | Sign in with Microsoft Entra ID (PKCE, Device Code, or token) | `--client-id`, `--tenant-id`, `--device-code`, `--client-secret`, `--token`, `--port`, `--json` |
| `teams logout` | `out` | Remove stored credentials from keyring & fallback file | `--json` |
| `teams whoami` | - | Show current account profile and JWT scope diagnostics | `--json` |
| `teams doctor` | - | Check config/DB/keyring paths, scopes, and Graph reachability | `--probe`, `--json` |
| `teams update` | - | Check GitHub Releases and self-update binary | `--check`, `--force`, `--repo`, `--json` |
| `teams chats` | `ls` | List 1:1, group, and meeting chats | `--limit <N>`, `--refresh`, `--cached`/`--fast`, `--json` |
| `teams new-chat` | - | Create a new 1:1 or group chat | `--to <email_or_id>` (repeatable), `--topic`, `--message`, `--json` |
| `teams teams` | - | List joined teams and their channels | `--refresh`, `--cached`/`--fast`, `--json` |
| `teams channels <team>` | - | List channels for a specific team | `--refresh`, `--cached`/`--fast`, `--json` |
| `teams read <target>` | `cat` | Read chat or channel messages (supports cursor history) | `--limit <N>`, `--before <ISO8601\|YYYY-MM-DD\|msg_id>`, `--thread <root_id>`, `--offline`, `--cached`/`--fast`, `--json` |
| `teams thread <chan> <root_id>` | - | Read a channel root post and all thread replies | `--limit <N>`, `--offline`, `--json` |
| `teams send <target> <md>` | `msg` | Send a Markdown message | `--subject`, `--mention <user_id:DisplayName>` (repeatable), `--important`, `--json` |
| `teams reply <target> <msg_id> <md>` | - | Reply to a channel thread or quote-reply in a chat | `--mention <user_id:DisplayName>`, `--important`, `--json` |
| `teams send-file <target> <path>` | `file` | Upload a file to OneDrive/SharePoint and post it | `--caption`, `--thread <root_id>`, `--inline`, `--json` |
| `teams download <target> <msg_id>` | `dl` | Download an attachment or inline image from a message | `--index <N>`, `--thread <root_id>`, `--out <dir>`, `--json` |
| `teams preview [target] [msg_id]` | - | Render a message image or local file inline in the terminal | `--file <path>`, `--attachment <id>`, `--thread <root_id>`, `--protocol <auto\|kitty\|iterm2\|sixel\|halfblock>`, `--cols`, `--rows` |
| `teams preview-file <path>` | - | Render a local image file in the terminal | `--protocol <auto\|kitty\|iterm2\|sixel\|halfblock>`, `--cols`, `--rows` |
| `teams react <target> <msg_id> <type>` | - | Add or remove a reaction (`like`, `heart`, `laugh`, `surprised`, `sad`, `angry`) | `--off`, `--thread <root_id>`, `--json` |
| `teams edit <target> <msg_id> <md>` | - | Edit a message you sent | `--thread <root_id>`, `--json` |
| `teams delete <target> <msg_id>` | - | Delete a message you sent (`--yes` required with `--json`) | `--thread <root_id>`, `--yes`, `--json` |
| `teams members <target>` | - | List members and presence for a chat or channel | `--offline`, `--json` |
| `teams presence` | - | Get or set your Presence availability | `--set Available\|Busy\|DoNotDisturb\|BeRightBack\|Away\|Reset`, `--activity`, `--expiration-mins`, `--json` |
| `teams search <query>` | - | Search local SQLite FTS5 + LIKE cache | `--target <target>`, `--limit <N>`, `--json` |
| `teams sync` | - | Backfill conversations and deep message history into SQLite | `--target <target>`, `--chats <N>`, `--depth <N>`, `--json` |
| `teams export <target>` | - | Export a conversation to Markdown, JSON, or plain text | `--format md\|json\|txt`, `--include-threads`, `--limit <N>`, `--offline`, `--out <path>` |
| `teams daemon <cmd>` | - | Control the background sync daemon (`start`, `status`, `stop`, `restart`, `run`) | `--json` (on `start`/`status`/`stop`/`restart`) |
| `teams serve` | `api` | Start the local HTTP JSON API server | `--bind <IP>`, `--port <8788>`, `--token <secret>` |
| `teams mcp` | `mcp-server` | Start the stdio MCP (JSON-RPC 2.0) server | - |

### Common CLI Workflows

```bash
# 1. List chats, start a 1:1 or group chat, and send a message
teams chats --limit 200 --refresh
teams new-chat --to alice.chen@contoso.com --message "Hi Alice!"
teams new-chat --to alice.chen@contoso.com --to bob.smith@contoso.com --topic "Backend Sprint"
teams send "Backend Sprint" "**Release v0.6.0** is ready for verification."

# 2. Post to a team channel with a subject, @mention, and high importance
teams teams --refresh
teams send "Engineering/General" "Hi @Alice Chen, please review the release checklist." \
  --subject "v0.6.0 Release Notice" \
  --mention "user-guid-123:Alice Chen" \
  --important

# 3. Read channel history before a date, inspect a thread, and reply
teams read "Engineering/General" --limit 100 --before 2026-11-01
teams sync --chats 100 --depth 100
teams thread "Engineering/General" 1700000000000
teams reply "Engineering/General" 1700000000000 "Verified—all integration checks passed!"

# 4. Upload files and inline images, download attachments, react, search, and export
teams send-file "Alice Chen" ./report.pdf --caption "Weekly engineering report"
teams send-file "Engineering/General" ./release-notes.md --thread 1700000000000 --caption "Release notes"
teams send-file "Engineering/General" ./dashboard.png --inline --caption "Latency dashboard"
teams download "Alice Chen" 1700000000100 --out ./downloads
teams react "Engineering/General" 1700000000000 heart
teams search "deployment" --target "Engineering/General" --json
teams export "Engineering/General" --format md --include-threads --out ./general-archive.md

# 5. Update Presence and manage background daemon
teams presence --set DoNotDisturb --activity "DeepWork" --expiration-mins 60
teams daemon start
teams chats --cached --json
```

---

## HTTP Server (`teams serve`)

Run a local Axum HTTP JSON API server for scripts, webhooks, and AI agents (defaults to `127.0.0.1:8788`):

```bash
teams serve                                # http://127.0.0.1:8788
teams serve --port 9000 --token secret123  # Require Authorization: Bearer secret123 (except /health)
```

```bash
curl http://127.0.0.1:8788/health
curl http://127.0.0.1:8788/conversations?refresh=true
curl "http://127.0.0.1:8788/conversations/Engineering%2FGeneral/messages?limit=50&before=2026-11-01T00:00:00Z"
curl -X POST http://127.0.0.1:8788/conversations/Alice%20Chen/messages \
  -H "Content-Type: application/json" \
  -d '{"markdown":"Hey Alice, build is green!"}'
```

Key endpoints:
- `GET /health`
- `GET /conversations`, `POST /chats`, `GET /teams`
- `GET /conversations/{key}/messages`, `POST /conversations/{key}/messages`
- `GET /conversations/{key}/threads/{root_id}`, `POST /conversations/{key}/messages/{msg_id}/replies`
- `POST /conversations/{key}/files`, `POST /conversations/{key}/messages/{msg_id}/attachments/download`
- `POST /conversations/{key}/messages/{msg_id}/reactions`, `PATCH` / `DELETE /conversations/{key}/messages/{msg_id}`
- `POST /conversations/{key}/read`, `GET /conversations/{key}/members`
- `GET /search?q=...`, `GET /presence`, `POST /presence`

See [docs/API.md](docs/API.md) and [docs/openapi.yaml](docs/openapi.yaml) for the complete HTTP API reference.

---

## MCP Server (`teams mcp`)

`teams mcp` (alias `teams mcp-server`) runs a **newline-delimited JSON-RPC 2.0** Model Context Protocol server over **stdio** (`stdout` is strictly reserved for JSON-RPC frames; logs go to `stderr`). Authenticate first with `teams login` or configure `TEAMS_ACCESS_TOKEN` / `TEAMS_TOKEN_FILE`.

### Claude Desktop (`claude_desktop_config.json`)

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

Or point directly to a repository launcher / binary without installing globally:

```json
{
  "mcpServers": {
    "teams": {
      "command": "C:\\path\\to\\teams\\teams.cmd",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code

```bash
claude mcp add teams -- teams mcp
```

### OpenAI Codex CLI (`~/.codex/config.toml`)

```toml
[mcp_servers.teams]
command = "teams"
args = ["mcp"]
```

**17 exposed MCP tools:** `list_chats`, `create_chat`, `list_teams_channels`, `get_messages`, `get_thread`, `send_message`, `reply_message`, `send_file`, `download_attachment`, `react_message`, `edit_message`, `delete_message` (requires `"confirm": true`), `search_messages`, `list_members`, `mark_read`, `get_presence`, and `set_presence`. See [docs/MCP.md](docs/MCP.md) for request/response schemas.

---

## Environment Variables & Configuration

### Storage & Config Locations

| Item | Default Path (Windows / Linux & macOS) | Override |
| --- | --- | --- |
| Configuration (`config.toml`) | `%APPDATA%\teams-cli\config.toml` / `~/.config/teams-cli/config.toml` | `TEAMS_CONFIG_DIR` |
| Unsent drafts (`drafts.json`) | `<config_dir>\drafts.json` | `TEAMS_CONFIG_DIR` |
| Credentials | OS Keyring (`teams-cli` / `default`) + fallback `<config_dir>\credentials.json` | `TEAMS_DISABLE_KEYRING=1` |
| SQLite cache (`cache.db`) | `%LOCALAPPDATA%\teams-cli\<user_id>\cache.db` / `~/.local/share/teams-cli/<user_id>/cache.db` | `TEAMS_DATA_DIR` |
| Downloads folder | `~/Downloads/teams` (or `download_dir` in `config.toml`) | - |
| Graph / Auth base URLs | `https://graph.microsoft.com/v1.0` / `https://login.microsoftonline.com` | `TEAMS_GRAPH_BASE_URL`, `TEAMS_AUTH_BASE_URL` |
| Self-update repository | `shinick-dev/teams` | `TEAMS_GITHUB_REPO` |

### Environment Variables

| Variable | Description |
| --- | --- |
| `TEAMS_CLIENT_ID` | Override custom Entra ID application (client) ID |
| `TEAMS_TENANT_ID` | Override Entra ID tenant ID (default: `common`) |
| `TEAMS_ACCESS_TOKEN` | Inject a Bearer access token directly (skips OS keyring lookup) |
| `TEAMS_TOKEN_FILE` | Path to external token file (raw JWT or JSON; skips OS keyring lookup) |
| `TEAMS_TOKEN_REFRESH_CMD` | External command hook to refresh tokens before expiry or on `401` |
| `TEAMS_TOKEN_REFRESH_MINS` | Minutes before `exp` to proactively run `TEAMS_TOKEN_REFRESH_CMD` (default: `5`) |
| `TEAMS_DISABLE_KEYRING` | Set to `1` to disable OS keyring and store credentials in `credentials.json` |
| `TEAMS_CLI_CACHE_SECS` | Local SQLite cache TTL in seconds for CLI reads (default: `0`; `--cached`/`--fast` prefers cache) |
| `TEAMS_AUTO_DAEMON` | Set to `1`/`true` to auto-start and attach to `teams daemon` for sub-5ms CLI reads (default: `false`) |
| `TEAMS_IMAGE_PROTOCOL` | Override terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`) |
| `TEAMS_CONFIG_DIR` | Override directory for `config.toml`, `credentials.json`, and `drafts.json` |
| `TEAMS_DATA_DIR` | Override directory for the SQLite cache (`cache.db`) |
| `TEAMS_NO_BROWSER` | Set to `1`/`true` to disable automatic browser launch during `teams login` |
| `TEAMS_GRAPH_BASE_URL` | Override Microsoft Graph v1.0 base URL |
| `TEAMS_AUTH_BASE_URL` | Override Entra ID OAuth 2.0 base URL |
| `TEAMS_GITHUB_REPO` | Override GitHub `owner/repo` used by `teams update` |
| `NO_COLOR` | Set to any non-empty value to force monochrome `NoColor` TUI rendering |

### Example `config.toml`

```toml
client_id = "14d82eec-204b-4c2f-b7e8-296a70dab67e"
tenant_id = "common"
density = "cozy"           # "cozy" | "compact"
theme = "auto"             # "auto" | "dark" | "light" | "ansi256" | "ansi16" | "nocolor"
notify = true
show_nav_rail = true
auto_thread_pane = true
inline_images = true
image_protocol = "auto"    # "auto" | "kitty" | "iterm2" | "sixel" | "halfblock"
download_dir = ""
cli_cache_ttl_secs = 0
auto_daemon = false
token_file = ""
token_refresh_cmd = ""
token_refresh_mins = 5
```

---

## License

MIT
