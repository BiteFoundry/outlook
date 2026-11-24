import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { runPostinstall } from '../scripts/postinstall.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const POSTINSTALL_SRC = path.join(REPO_ROOT, 'scripts', 'postinstall.mjs');

function makeInstalledLayout(tmpRoot, platform = process.platform, arch = process.arch) {
  const scopeDir = path.join(tmpRoot, 'node_modules', '@bitefoundry');
  const pkgRoot = path.join(scopeDir, 'outlook');
  const platformPkg = path.join(scopeDir, `outlook-${platform}-${arch}`);
  const binaryName = platform === 'win32' ? 'outlook.exe' : 'outlook';

  fs.mkdirSync(path.join(pkgRoot, 'bin'), { recursive: true });
  fs.mkdirSync(path.join(pkgRoot, 'scripts'), { recursive: true });
  fs.mkdirSync(path.join(platformPkg, 'bin'), { recursive: true });

  fs.writeFileSync(
    path.join(pkgRoot, 'package.json'),
    JSON.stringify({ name: '@bitefoundry/outlook', version: '0.3.1' }),
  );
  fs.writeFileSync(
    path.join(platformPkg, 'package.json'),
    JSON.stringify({ name: `@bitefoundry/outlook-${platform}-${arch}`, version: '0.3.1' }),
  );
  fs.writeFileSync(path.join(pkgRoot, 'bin', 'outlook.js'), '#!/usr/bin/env node\nconsole.log("fallback");\n');
  fs.writeFileSync(path.join(platformPkg, 'bin', binaryName), 'FAKE-NATIVE-OUTLOOK-BINARY');
  fs.copyFileSync(POSTINSTALL_SRC, path.join(pkgRoot, 'scripts', 'postinstall.mjs'));

  return { pkgRoot, platformPkg, binaryName };
}

test('runPostinstall skips mutation inside dev repository', () => {
  const res = runPostinstall({ pkgRoot: REPO_ROOT });
  assert.equal(res.skipped, true);
  assert.equal(res.reason, 'dev_repo');
});

test('runPostinstall links native binary in installed package layout', () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'outlook-npm-postinstall-'));
  try {
    const { pkgRoot } = makeInstalledLayout(tmp);
    const res = runPostinstall({ pkgRoot });
    assert.equal(res.skipped, false);
    assert.equal(res.linked, 1);

    if (process.platform === 'win32') {
      const destExe = path.join(pkgRoot, 'bin', 'outlook.exe');
      assert.equal(fs.readFileSync(destExe, 'utf8'), 'FAKE-NATIVE-OUTLOOK-BINARY');
    } else {
      const destJs = path.join(pkgRoot, 'bin', 'outlook.js');
      assert.equal(fs.readFileSync(destJs, 'utf8'), 'FAKE-NATIVE-OUTLOOK-BINARY');
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('postinstall.mjs main-module check executes when invoked via symlinked path', (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'outlook-npm-symlink-'));
  try {
    const { pkgRoot } = makeInstalledLayout(tmp);
    const realScript = path.join(pkgRoot, 'scripts', 'postinstall.mjs');
    const symlinkScript = path.join(tmp, 'linked-postinstall.mjs');

    try {
      fs.symlinkSync(realScript, symlinkScript);
    } catch (err) {
      if (process.platform === 'win32' && (err.code === 'EPERM' || err.code === 'EACCES')) {
        t.skip('Windows symlink creation requires Developer Mode or elevated privileges');
        return;
      }
      throw err;
    }

    const result = spawnSync(process.execPath, [symlinkScript], {
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, `stderr: ${result.stderr}`);

    if (process.platform === 'win32') {
      const destExe = path.join(pkgRoot, 'bin', 'outlook.exe');
      assert.equal(fs.existsSync(destExe), true, 'expected symlinked postinstall to create bin/outlook.exe');
      assert.equal(fs.readFileSync(destExe, 'utf8'), 'FAKE-NATIVE-OUTLOOK-BINARY');
    } else {
      const destJs = path.join(pkgRoot, 'bin', 'outlook.js');
      assert.equal(fs.readFileSync(destJs, 'utf8'), 'FAKE-NATIVE-OUTLOOK-BINARY');
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('postinstall.mjs main-module check executes via aliased/junctioned parent directory', (t) => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'outlook-npm-dirlink-'));
  try {
    const { pkgRoot } = makeInstalledLayout(tmp);
    const linkedPkgDir = path.join(tmp, 'linked-outlook-pkg');
    const linkType = process.platform === 'win32' ? 'junction' : 'dir';

    try {
      fs.symlinkSync(pkgRoot, linkedPkgDir, linkType);
    } catch (err) {
      if (process.platform === 'win32' && (err.code === 'EPERM' || err.code === 'EACCES')) {
        t.skip('Windows directory link creation not permitted');
        return;
      }
      throw err;
    }

    const aliasedScript = path.join(linkedPkgDir, 'scripts', 'postinstall.mjs');
    const result = spawnSync(process.execPath, [aliasedScript], {
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, `stderr: ${result.stderr}`);

    if (process.platform === 'win32') {
      const destExe = path.join(pkgRoot, 'bin', 'outlook.exe');
      assert.equal(fs.existsSync(destExe), true, 'expected junction-invoked postinstall to create bin/outlook.exe');
      assert.equal(fs.readFileSync(destExe, 'utf8'), 'FAKE-NATIVE-OUTLOOK-BINARY');
    } else {
      const destJs = path.join(pkgRoot, 'bin', 'outlook.js');
      assert.equal(fs.readFileSync(destJs, 'utf8'), 'FAKE-NATIVE-OUTLOOK-BINARY');
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
