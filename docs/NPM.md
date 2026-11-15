# npm distribution

`@shinick/teams` is distributed on npm as a small JavaScript launcher plus one
optional dependency per platform. The launcher resolves the prebuilt Rust
binary and execs it, so there is no compiler, Node native addon, or postinstall
download step.

The Rust crate is named `teams-cli` and produces the `teams` binary; the npm
package is scoped as `@shinick/teams` while still installing the `teams`
command.

## Install and run

```sh
# one-off
npx @shinick/teams --version

# global install
npm install -g @shinick/teams
teams --version

# project-local
npm install @shinick/teams
npx teams chats
```

The CLI is otherwise the same as the Cargo build: `teams login`, `teams chats`,
`teams read <target>`, `teams send <target> <message>`, `teams tui`,
`teams serve`, `teams mcp`, etc.

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

`bin/teams.js` looks for `@shinick/teams-<platform>-<arch>` through
`require.resolve`, then executes
`node_modules/@shinick/teams-<platform>-<arch>/bin/teams` (or `teams.exe` on
Windows) with `stdio: 'inherit'`. Signals (`SIGINT`, `SIGTERM`, `SIGHUP`) are
forwarded and the launcher exits with the child's exit code. If the platform
package is missing (for example because optional dependencies were skipped), it
prints the supported platforms and the `--include=optional` hint instead of a
stack trace.

## Layout

```
package.json                  # @shinick/teams launcher package
bin/teams.js                  # launcher
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
git tag v0.1.0
git push origin v0.1.0
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
   root package, and creates a GitHub Release with the raw binaries.

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
