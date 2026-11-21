// `release/pack.mjs`: the manifests a release packs, the guard on the platform packages' versions, and one whole
// pack read back out of its tarballs.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import {
  NODE_DIR,
  PLATFORMS,
  checkOptionalDependencies,
  packAll,
  platformManifest,
  releaseUrl,
  releasedMainManifest,
  tarballName,
} from '../release/pack.mjs';

const MAIN = JSON.parse(fs.readFileSync(path.join(NODE_DIR, 'package.json'), 'utf8'));
const REPO = 'spankyed/apack-terminal';
const COMMIT = '0123456789abcdef0123456789abcdef01234567';

test('the source manifest names each platform package at exactly its own version', () => {
  checkOptionalDependencies(MAIN);
  assert.deepEqual(
    Object.keys(MAIN.optionalDependencies).sort(),
    ['@apackdev/terminal-darwin-arm64', '@apackdev/terminal-linux-x64', '@apackdev/terminal-win32-x64'],
  );
});

test('the guard refuses a platform package at another version, a missing one and an unknown one', () => {
  const at = (deps) => ({ ...MAIN, optionalDependencies: deps });
  const every = Object.fromEntries(PLATFORMS.map(({ platform }) => [`@apackdev/terminal-${platform}`, MAIN.version]));
  assert.throws(() => checkOptionalDependencies(at({ ...every, '@apackdev/terminal-linux-x64': '0.0.9' })), /at 0\.0\.9/);
  const { ['@apackdev/terminal-win32-x64']: _, ...missing } = every;
  assert.throws(() => checkOptionalDependencies(at(missing)), /win32-x64 is missing/);
  assert.throws(() => checkOptionalDependencies(at({ ...every, '@apackdev/terminal-linux-arm64': MAIN.version })), /linux-arm64 is not/);
  assert.throws(() => checkOptionalDependencies({ ...MAIN, optionalDependencies: undefined }), /missing/);
});

test('names a tarball as npm pack does, and its release URL by commit', () => {
  assert.equal(tarballName('@apackdev/terminal-win32-x64', '0.1.0'), 'apackdev-terminal-win32-x64-0.1.0.tgz');
  assert.equal(tarballName('@apackdev/terminal', '0.1.0'), 'apackdev-terminal-0.1.0.tgz');
  assert.equal(
    releaseUrl(REPO, COMMIT, 'x.tgz'),
    `https://github.com/spankyed/apack-terminal/releases/download/bin-${COMMIT}/x.tgz`,
  );
});

test("a platform's manifest is the main package's version, installed on its os and cpu alone", () => {
  const linux = PLATFORMS.find(({ platform }) => platform === 'linux-x64');
  const manifest = platformManifest(MAIN, linux, ['apack-terminal', 'LICENSE-MIT'], COMMIT);
  assert.equal(manifest.name, '@apackdev/terminal-linux-x64');
  assert.equal(manifest.version, MAIN.version);
  assert.deepEqual(manifest.os, ['linux']);
  assert.deepEqual(manifest.cpu, ['x64']);
  assert.deepEqual(manifest.libc, ['glibc']);
  assert.deepEqual(manifest.files, ['apack-terminal', 'LICENSE-MIT']);
  assert.equal(manifest.license, MAIN.license);
  assert.equal(manifest.gitHead, COMMIT);
  assert.deepEqual(manifest.publishConfig, { access: 'public' });
  const mac = platformManifest(MAIN, PLATFORMS.find(({ platform }) => platform === 'darwin-arm64'), [], COMMIT);
  assert.equal(mac.libc, undefined);
});

test('the released main manifest names each platform package by its release URL, and runs nothing', () => {
  const released = releasedMainManifest(MAIN, REPO, COMMIT);
  for (const { platform } of PLATFORMS) {
    const name = `@apackdev/terminal-${platform}`;
    assert.equal(released.optionalDependencies[name], releaseUrl(REPO, COMMIT, tarballName(name, MAIN.version)));
  }
  assert.equal(released.gitHead, COMMIT);
  assert.equal(released.scripts, undefined);
  assert.equal(released.version, MAIN.version);
  assert.deepEqual(released.exports, MAIN.exports);
  assert.throws(() => releasedMainManifest({ ...MAIN, optionalDependencies: {} }, REPO, COMMIT), /missing/);
});

/** Every platform's release assets, as `binaries.yml` collects them; `except` leaves one out */
function assets(except) {
  const files = fs.mkdtempSync(path.join(os.tmpdir(), 'apack-terminal-assets-'));
  for (const { platform, binary, beside } of PLATFORMS) {
    for (const file of [binary, ...beside, 'LICENSE-MIT', 'THIRD-PARTY-NOTICES']) {
      if (`${platform}__${file}` !== except) fs.writeFileSync(path.join(files, `${platform}__${file}`), file, { mode: 0o644 });
    }
  }
  return files;
}

function unpack(tarball) {
  const into = fs.mkdtempSync(path.join(os.tmpdir(), 'apack-terminal-unpacked-'));
  const ran = spawnSync('tar', ['-xzf', tarball, '-C', into]);
  assert.equal(ran.status, 0, String(ran.stderr));
  return path.join(into, 'package');
}

test('packs every platform package and the main package, read back out of their tarballs', () => {
  const out = fs.mkdtempSync(path.join(os.tmpdir(), 'apack-terminal-packed-'));
  const tarballs = packAll({ commit: COMMIT, files: assets(), out, repo: REPO });
  assert.deepEqual(
    tarballs.map((file) => path.basename(file)),
    [...PLATFORMS.map(({ platform }) => tarballName(`@apackdev/terminal-${platform}`, MAIN.version)), tarballName(MAIN.name, MAIN.version)],
  );

  for (const [index, { platform, binary, beside }] of PLATFORMS.entries()) {
    const unpacked = unpack(tarballs[index]);
    const manifest = JSON.parse(fs.readFileSync(path.join(unpacked, 'package.json'), 'utf8'));
    assert.equal(manifest.name, `@apackdev/terminal-${platform}`);
    assert.equal(manifest.files[0], binary);
    for (const file of [binary, ...beside, 'LICENSE-MIT', 'THIRD-PARTY-NOTICES']) {
      assert.ok(fs.existsSync(path.join(unpacked, file)), `${platform} holds ${file}`);
    }
    assert.equal(fs.statSync(path.join(unpacked, binary)).mode & 0o111, 0o111, `${platform}'s binary is executable`);
  }

  const main = unpack(tarballs.at(-1));
  const manifest = JSON.parse(fs.readFileSync(path.join(main, 'package.json'), 'utf8'));
  assert.deepEqual(manifest.optionalDependencies, releasedMainManifest(MAIN, REPO, COMMIT).optionalDependencies);
  for (const file of MAIN.files) assert.ok(fs.existsSync(path.join(main, file)), `the main package holds ${file}`);
});

test('refuses to pack a platform missing a file it must hold', () => {
  const out = fs.mkdtempSync(path.join(os.tmpdir(), 'apack-terminal-packed-'));
  assert.throws(() => packAll({ commit: COMMIT, files: assets('win32-x64__conpty.dll'), out, repo: REPO }), /win32-x64__conpty\.dll/);
  assert.throws(() => packAll({ commit: COMMIT, files: assets('linux-x64__apack-terminal'), out, repo: REPO }), /linux-x64__apack-terminal/);
});
