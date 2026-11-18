# npm distribution

`@shinick/teams` is distributed on npm as small JavaScript launchers plus one
optional dependency per platform. The launchers resolve the prebuilt Rust binary
and exec it, so there is no compiler, Node native addon, or postinstall download
step.

The Rust crate is named `teams-cli` and produces a single unified binary
(`teams` / `teams.exe`) that powers Teams, Outlook, and OneNote
(`TEAMS_PERSONA=outlook` / `TEAMS_PERSONA=onenote`). The npm package is scoped
as `@shinick/teams` and installs **`teams`, `outlook`, and `onenote`** commands.

## Install and run

```sh
# one-off
npx @shinick/teams --version
npx --yes -p @shinick/teams outlook --version
npx --yes -p @shinick/teams onenote --version

# global install (installs `teams`, `outlook`, and `onenote`)
npm install -g @shinick/teams
teams --version
outlook --version
onenote --version

# project-local
npm install @shinick/teams
npx teams chats
npx outlook inbox
npx onenote notebooks
```

All three commands share the exact same Entra ID login, SQLite FTS5 cache, background sync daemon (`teams daemon start | status | stop | restart`), HTTP API,
and MCP server (read commands also support `--cached` / `--fast` for instant local cache reads, or run with zero Graph round-trips while the daemon is warm):
- `teams login`, `teams chats`, `teams read <target>`, `teams send <target> <message>`, `teams notes <cmd>`, `teams daemon <start|status|stop|restart>`, `teams tui`, `teams serve`, `teams mcp`, `teams update`
- `outlook`, `outlook inbox`, `outlook read`, `outlook send`, `outlook reply`, `outlook cal`, `outlook contacts`, `outlook todo`, `outlook notes`, `outlook daemon <start|status|stop|restart>`, `outlook update`
- `onenote`, `onenote notebooks`, `onenote sections`, `onenote pages`, `onenote read`, `onenote create`, `onenote append`, `onenote update`, `onenote check`, `onenote delete`, `onenote search`, `onenote resource`, `onenote daemon <start|status|stop|restart>`, `onenote self-update`

## Supported platforms

| npm package                   | OS      | CPU   | Rust target                         |
| ----------------------------- | ------- | ----- | ----------------------------------- |
| `@shinick/teams-win32-x64`    | Windows | x64   | `x86_64-pc-windows-msvc`            |
| `@shinick/teams-win32-arm64`  | Windows | arm64 | `aarch64-pc-windows-msvc`           |
| `@shinick/teams-darwin-x64`   | macOS   | x64   | `x86_64-apple-darwin`               |
| `@shinick/teams-darwin-arm64` | macOS   | arm64 | `aarch64-apple-darwin`              |
| `@shinick/teams-linux-x64`    | Linux   | x64   | `x86_64-unknown-linux-gnu` (2.28)   |
| `@shinick/teams-linux-arm64`  | Linux   | arm64 | `aarch64-unknown-linux-gnu` (2.28)  |

Linux packages are cross-compiled in CI with `cargo-zigbuild` against **GLIBC 2.28** (`*.2.28`) and verified with `scripts/verify-glibc.mjs` plus an `almalinux:8` container smoke test, so prebuilt binaries run out of the box on **AlmaLinux 8 / RHEL 8+ / Rocky Linux 8+ / Ubuntu 20.04+ / Debian 10+**. Older glibc distributions (e.g., CentOS 7 with `GLIBC < 2.28`) and Alpine/musl Linux should build from source with Cargo (`cargo build --release`).

## How resolution works

`bin/teams.js`, `bin/outlook.js`, and `bin/onenote.js` look up
`@shinick/teams-<platform>-<arch>` through `require.resolve`, then execute
`node_modules/@shinick/teams-<platform>-<arch>/bin/teams` (or `teams.exe` on
Windows) with `stdio: 'inherit'`. `bin/outlook.js` and `bin/onenote.js` set
`TEAMS_PERSONA=outlook` / `TEAMS_PERSONA=onenote` before spawning the binary so
the unified executable boots directly in the matching persona. Signals (`SIGINT`,
`SIGTERM`, `SIGHUP`) are forwarded and the launcher exits with the child's exit
code. If the platform package is missing (for example because optional
dependencies were skipped), the launcher prints the supported platforms and the
`--include=optional` hint instead of a stack trace.

## Layout

```
package.json                  # @shinick/teams launcher package (bin: teams, outlook, onenote)
bin/teams.js                  # teams launcher
bin/outlook.js                # outlook launcher (sets TEAMS_PERSONA=outlook)
bin/onenote.js                # onenote launcher (sets TEAMS_PERSONA=onenote)
teams / teams.cmd             # repo-local Teams launchers
outlook / outlook.cmd         # repo-local Outlook launchers (sets TEAMS_PERSONA=outlook)
onenote / onenote.cmd         # repo-local OneNote launchers (sets TEAMS_PERSONA=onenote)
npm/teams-<platform>/         # one package per platform
  package.json                # os/cpu/files, same version as root
  bin/teams[.exe]             # added by scripts/build-npm.mjs, never committed
scripts/build-npm.mjs         # copy target/<triple>/release/teams[.exe]
scripts/sync-version.mjs      # Cargo.toml version -> all package.json files
scripts/verify-glibc.mjs      # verify Linux ELF GLIBC <= 2.28 symbol ceiling
.github/workflows/release.yml # tag-triggered build, publish, GitHub Release
```

Binaries are gitignored (`npm/*/bin/teams*`). A clean checkout has empty
platform `bin/` directories until a build or `npm run build:npm` runs.

## Versioning

`Cargo.toml` (`[package].version`) is the single source of truth. To bump:

```sh
# edit Cargo.toml [package].version
node scripts/sync-version.mjs
```

The script updates the root `package.json`, every `npm/teams-*/package.json`,
and the root `optionalDependencies` pins. It is idempotent; `--check` exits
non-zero if anything is stale. All packages must be published at the same
version because the root package pins its platform packages exactly.

Tag the release with a matching `v` prefix:

```sh
git tag v0.5.0
git push origin v0.5.0
```

## Publishing

Publishing is automated by `.github/workflows/release.yml`, which is triggered
by pushing a `v*` tag (or manually via `workflow_dispatch`).

1. Add a repository secret `NPM_TOKEN` (an npm automation token with publish
   rights for the `@shinick/teams` packages).
2. Bump the version in `Cargo.toml`, run `node scripts/sync-version.mjs`,
   commit, and push `v<version>`.
3. The workflow builds six targets, stages binaries into the platform
   packages, uploads artifacts, publishes each platform package, publishes the
   root package, and creates a GitHub Release with the archives, `teams.exe`,
   and `SHA256SUMS.txt`.

The publish job verifies that the tag matches the package version and that all
six platform packages contain a binary before publishing anything. Platform
packages are published first so the root package's `optionalDependencies` can
resolve immediately.

Manual publish (matching order):

```sh
node scripts/sync-version.mjs --check
for dir in npm/teams-*; do npm publish "$dir" --access public; done
npm publish --access public
```

## Local validation

```sh
# stage the current host binary (Windows example)
node scripts/build-npm.mjs --target x86_64-pc-windows-msvc
node bin/teams.js --version
node bin/outlook.js --version
node bin/onenote.js --version

npm pack --dry-run
npm pack --dry-run ./npm/teams-win32-x64
```

`node scripts/build-npm.mjs` with no arguments uses the host target and falls
back to `target/release/` when a plain `cargo build --release` was used.
`--all` attempts every supported target and warns about the ones not built on
the current host.

## External token brokers & environment variables

When using an external Microsoft Graph token broker (`tc-auth`, `az account get-access-token`, Vault CLI, etc.), configure `token_file` / `token_refresh_cmd` instead of shell wrappers (avoids `cmd.exe` 1023-char `set /p` truncation and `.ps1`/`.cmd` shim issues). Setting `TEAMS_TOKEN_FILE` (`token_file`) or `TEAMS_ACCESS_TOKEN` automatically skips OS keyring reads.

### 1. `config.toml` (recommended persistent setup)

```toml
token_file = "~/.cursor/tc-auth/access_token"
token_refresh_cmd = "python ~/.cursor/tc-auth/graph-auth.py refresh"
token_refresh_mins = 5
```

### 2. Environment variables

```sh
export TEAMS_TOKEN_FILE="$HOME/.cursor/tc-auth/access_token"
export TEAMS_TOKEN_REFRESH_CMD="python $HOME/.cursor/tc-auth/graph-auth.py refresh"
export TEAMS_TOKEN_REFRESH_MINS=5
```

### 3. Azure CLI (`az account get-access-token`) JSON/stdout integration

If `token_file` is omitted, `token_refresh_cmd` stdout (raw JWT or Azure CLI camelCase JSON `accessToken` / `expiresOn`) is parsed and cached in memory:

```toml
token_refresh_cmd = "az account get-access-token --resource https://graph.microsoft.com --output json"
token_refresh_mins = 5
```

### Key environment variables

| Variable | Summary |
| --- | --- |
| `TEAMS_PERSONA` | Force persona (`teams` \| `outlook` \| `onenote`) |
| `TEAMS_TOKEN_FILE` | External token file path (raw JWT or JSON; skips OS keyring) |
| `TEAMS_TOKEN_REFRESH_CMD` | Shell command run to refresh expiring/`401` tokens |
| `TEAMS_TOKEN_REFRESH_MINS` | Refresh lead window in minutes before expiry (default `5`) |
| `TEAMS_ACCESS_TOKEN` | Static Bearer token override (automatically skips OS keyring) |
| `TEAMS_DISABLE_KEYRING=1` | Disable OS keyring and use `credentials.json` only |
| `TEAMS_CLIENT_ID` | Override Entra ID application client ID |
| `TEAMS_TENANT_ID` | Override Entra ID tenant ID (default `common`) |
| `TEAMS_CONFIG_DIR` | Override config/credentials directory |
| `TEAMS_DATA_DIR` | Override SQLite cache (`cache.db`) directory |
| `TEAMS_NO_BROWSER` | Disable automatic browser launch during login (`1` / `true`) |
| `TEAMS_CLI_CACHE_SECS` | CLI read cache TTL in seconds (default `0`; `--cached` / `--fast` forces cache-first reads) |
| `TEAMS_BACKGROUND_ALL` | Poll inactive personas/domains in background on TUI startup (`1` / `true`, default `false`) |
| `TEAMS_AUTO_DAEMON` | Auto-start/attach to the background sync daemon (`1` / `true`, or `config.toml`: `auto_daemon = true`, default `false`) |

## Troubleshooting

- **`no prebuilt binary found for <platform>-<arch>`** — the optional
  dependency was skipped. Reinstall with
  `npm install -g @shinick/teams --include=optional` or
  `npm install @shinick/teams --include=optional`. Check that your npm config
  does not set `omit=optional`.
- **`EBADPLATFORM` / package skipped** — npm installs only the matching
  `os`/`cpu` package. Cross-installing (for example x64 Node on ARM Windows via
  emulation) still reports the Node architecture, so install under the native
  Node build.
- **Older Linux (`GLIBC < 2.28`, e.g. CentOS 7) or Alpine/musl Linux** — prebuilt Linux binaries require **GLIBC >= 2.28** (AlmaLinux 8 / RHEL 8+ / Rocky Linux 8+ / Ubuntu 20.04+ / Debian 10+). On older glibc or musl systems, build from source with Cargo (`cargo build --release`).
- **macOS Gatekeeper** — npm-installed binaries are not notarized. If macOS
  blocks execution, allow the binary in System Settings → Privacy & Security, or
  build from source.
- **Corporate proxy/firewall** — `npx @shinick/teams` performs a registry
  download each time unless cached; use a global install for offline-friendly
  use.
- **Verify the binary** —
  `node -p "require.resolve('@shinick/teams-win32-x64/package.json')"`
  (adjust for your platform) shows which platform package npm selected.
- **Tag push did not publish** — check the publish job log; a tag/version
  mismatch or a missing `NPM_TOKEN` fails the job before any publish.
