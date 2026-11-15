#!/usr/bin/env node
// Copy a Cargo release binary into the matching npm platform package.
//
// Usage:
//   node scripts/build-npm.mjs                     # host target
//   node scripts/build-npm.mjs --target <triple>   # one or more targets
//   node scripts/build-npm.mjs --all               # every supported target
//
// Only targets whose binary actually exists in the Cargo target directory are
// copied. With --all, missing targets are reported and skipped.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/** Supported Rust targets mapped to npm platform packages. */
export const TARGETS = {
  'x86_64-pc-windows-msvc': {
    package: 'teams-win32-x64',
    source: 'teams.exe',
    binary: 'teams.exe',
  },
  'aarch64-pc-windows-msvc': {
    package: 'teams-win32-arm64',
    source: 'teams.exe',
    binary: 'teams.exe',
  },
  'x86_64-apple-darwin': {
    package: 'teams-darwin-x64',
    source: 'teams',
    binary: 'teams',
  },
  'aarch64-apple-darwin': {
    package: 'teams-darwin-arm64',
    source: 'teams',
    binary: 'teams',
  },
  'x86_64-unknown-linux-gnu': {
    package: 'teams-linux-x64',
    source: 'teams',
    binary: 'teams',
  },
  'aarch64-unknown-linux-gnu': {
    package: 'teams-linux-arm64',
    source: 'teams',
    binary: 'teams',
  },
};

function parseArgs(argv) {
  const options = { targets: [], all: false, help: false };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--all') {
      options.all = true;
    } else if (arg === '--help' || arg === '-h') {
      options.help = true;
    } else if (arg === '--target' || arg === '-t') {
      const value = argv[i + 1];
      if (!value) throw new Error(`${arg} requires a value`);
      options.targets.push(...value.split(','));
      i += 1;
    } else if (arg.startsWith('--target=')) {
      options.targets.push(...arg.slice('--target='.length).split(','));
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }

  return options;
}

function hostTriple() {
  try {
    const out = execFileSync('rustc', ['-vV'], { encoding: 'utf8' });
    const match = out.match(/^host:\s*(\S+)$/m);
    if (match) return match[1];
  } catch {
    // rustc is not installed; fall back to the Node platform/arch mapping.
  }

  const fallback = {
    'win32-x64': 'x86_64-pc-windows-msvc',
    'win32-arm64': 'aarch64-pc-windows-msvc',
    'darwin-x64': 'x86_64-apple-darwin',
    'darwin-arm64': 'aarch64-apple-darwin',
    'linux-x64': 'x86_64-unknown-linux-gnu',
    'linux-arm64': 'aarch64-unknown-linux-gnu',
  }[`${process.platform}-${process.arch}`];

  if (!fallback) {
    throw new Error(
      `cannot infer host target for ${process.platform}-${process.arch}; pass --target <triple>`,
    );
  }
  return fallback;
}

function candidatePaths(triple, binary) {
  const cargoTargetDir = process.env.CARGO_TARGET_DIR
    ? path.resolve(process.env.CARGO_TARGET_DIR)
    : path.join(ROOT, 'target');

  const candidates = [path.join(cargoTargetDir, triple, 'release', binary)];

  // A plain `cargo build --release` on the host writes to target/release,
  // not target/<triple>/release. Accept that for the host triple only.
  if (triple === hostTriple()) {
    candidates.push(path.join(cargoTargetDir, 'release', binary));
  }
  return candidates;
}

function findBinary(triple, source) {
  for (const candidate of candidatePaths(triple, source)) {
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
}

function copyBinary(source, packageDir, binary) {
  const binDir = path.join(packageDir, 'bin');
  const destination = path.join(binDir, binary);
  fs.mkdirSync(binDir, { recursive: true });
  fs.copyFileSync(source, destination);
  if (binary !== 'teams.exe') fs.chmodSync(destination, 0o755);
  return destination;
}

function build(triple) {
  const mapping = TARGETS[triple];
  if (!mapping) {
    throw new Error(
      `unsupported target "${triple}". Supported: ${Object.keys(TARGETS).join(', ')}`,
    );
  }

  const source = findBinary(triple, mapping.source);
  if (!source) {
    return {
      triple,
      ok: false,
      reason: `binary not found: ${candidatePaths(triple, mapping.source).join(' or ')}`,
    };
  }

  const packageDir = path.join(ROOT, 'npm', mapping.package);
  const packageJson = path.join(packageDir, 'package.json');
  if (!fs.existsSync(packageJson)) {
    return { triple, ok: false, reason: `missing package manifest: ${packageJson}` };
  }

  const destination = copyBinary(source, packageDir, mapping.binary);
  return {
    triple,
    ok: true,
    package: mapping.package,
    source,
    destination,
    bytes: fs.statSync(destination).size,
  };
}

function main() {
  const options = parseArgs(process.argv.slice(2));

  if (options.help) {
    console.log(`Usage: node scripts/build-npm.mjs [--target <triple>]... [--all]

Supported targets:
${Object.keys(TARGETS).map((t) => `  ${t} -> ${TARGETS[t].package}`).join('\n')}`);
    return;
  }

  const targets = options.all
    ? Object.keys(TARGETS)
    : options.targets.length > 0
      ? options.targets
      : [hostTriple()];

  const results = targets.map((triple) => {
    try {
      return build(triple);
    } catch (error) {
      return { triple, ok: false, reason: error.message };
    }
  });

  let failed = false;
  for (const result of results) {
    if (result.ok) {
      console.log(
        `[build-npm] ${result.triple} -> npm/${result.package}/bin ` +
          `(${result.bytes} bytes)`,
      );
    } else {
      console.error(`[build-npm] ${result.triple}: ${result.reason}`);
      // --all legitimately runs on a single host, so missing other targets
      // are only a warning there.
      if (!options.all) failed = true;
    }
  }

  if (failed) process.exitCode = 1;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (error) {
    console.error(`[build-npm] ${error.message}`);
    process.exit(1);
  }
}
