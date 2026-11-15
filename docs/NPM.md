# npm distribution

`@shinick/teams` is distributed on npm as small JavaScript launchers plus one
optional dependency per platform. The launchers resolve the prebuilt Rust binary
and exec it, so there is no compiler, Node native addon, or postinstall download
step.

The Rust crate is named `teams-cli` and produces a single unified binary
(`teams` / `teams.exe`) that powers both Teams and Outlook (`TEAMS_PERSONA=outlook`).
The npm package is scoped as `@shinick/teams` and installs **both** `teams` and
`outlook` commands.

## Install and run

```sh
# one-off
npx @shinick/teams --version
npx --yes -p @shinick/teams outlook --version

# global install (installs both `teams` and `outlook`)
npm install -g @shinick/teams
teams --version
outlook --version

# project-local
npm install @shinick/teams
npx teams chats
npx outlook inbox
```

Both commands share the exact same Entra ID login, SQLite FTS5 cache, HTTP API,
and MCP server:
- `teams login`, `teams chats`, `teams read <target>`, `teams send <target> <message>`, `teams tui`, `teams serve`, `teams mcp`, `teams update`
- `outlook`, `outlook inbox`, `outlook read`, `outlook send`, `outlook reply`, `outlook cal`, `outlook contacts`, `outlook todo`, `outlook update`

## Supported platforms

| npm package                   | OS      | CPU   | Rust target                 |
| ----------------------------- | ------- | ----- | --------------------------- |
| `@shinick/teams-win32-x64`    | Windows | x64   | `x86_64-pc-windows-msvc`    |
| `@shinick/teams-win32-arm64`  | Windows | arm64 | `aarch64-pc-windows-msvc`   |
| `@shinick/teams-darwin-x64`   | macOS   | x64   | `x86_64-apple-darwin`       |
| `@shinick/teams-darwin-arm64` | macOS   | arm64 | `aarch64-apple-darwin`      |
| `@shinick/teams-linux-x64`    | Linux   | x64   | `x86_64-unknown-linux-gnu`  |
| `@shinick/teams-linux-arm64`  | Linux   | arm64 | `aarch64-unknown-linux-gnu` |

Linux packages are built against glibc. Alpine/musl users should build from
source with Cargo; there is no musl package yet.

## How resolution works

`bin/teams.js` and `bin/outlook.js` look up `@shinick/teams-<platform>-<arch>`
through `require.resolve`, then execute
`node_modules/@shinick/teams-<platform>-<arch>/bin/teams` (or `teams.exe` on
Windows) with `stdio: 'inherit'`. `bin/outlook.js` sets
`TEAMS_PERSONA=outlook` before spawning the binary so the unified executable
boots directly in Outlook mode. Signals (`SIGINT`, `SIGTERM`, `SIGHUP`) are
forwarded and the launcher exits with the child's exit code. If the platform
package is missing (for example because optional dependencies were skipped), the
launcher prints the supported platforms and the `--include=optional` hint instead
of a stack trace.

## Layout

```
package.json                  # @shinick/teams launcher package (bin: teams, outlook)
bin/teams.js                  # teams launcher
bin/outlook.js                # outlook launcher (sets TEAMS_PERSONA=outlook)
teams / teams.cmd             # repo-local Teams launchers
outlook / outlook.cmd         # repo-local Outlook launchers (sets TEAMS_PERSONA=outlook)
npm/teams-<platform>/         # one package per platform
  package.json                # os/cpu/files, same version as root
  bin/teams[.exe]             # added by scripts/build-npm.mjs, never committed
scripts/build-npm.mjs         # copy target/<triple>/release/teams[.exe]
scripts/sync-version.mjs      # Cargo.toml version -> all package.json files
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
git tag v0.2.0
git push origin v0.2.0
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

npm pack --dry-run
npm pack --dry-run ./npm/teams-win32-x64
```

`node scripts/build-npm.mjs` with no arguments uses the host target and falls
back to `target/release/` when a plain `cargo build --release` was used.
`--all` attempts every supported target and warns about the ones not built on
the current host.

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
- **Alpine/musl Linux** — unsupported; build with Cargo.
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
