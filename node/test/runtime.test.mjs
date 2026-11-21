// `index.js` as a consumer reaches it: installed under `node_modules` beside (or without) a platform package.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

const NODE_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const MAIN = JSON.parse(fs.readFileSync(path.join(NODE_DIR, 'package.json'), 'utf8'));
const PLATFORM = `${process.platform}-${process.arch}`;
const BINARY = process.platform === 'win32' ? 'apack-terminal.exe' : 'apack-terminal';

/**
 * A project with `@apackdev/terminal` installed and, unless `platform` is null, this platform's package holding
 * `present` of the `listed` files. Answers the project and its `require` of the package.
 */
function install({ platform = { listed: [BINARY, 'THIRD-PARTY-NOTICES'] }, gitHead } = {}) {
  const project = fs.mkdtempSync(path.join(os.tmpdir(), 'apack-terminal-runtime-'));
  fs.writeFileSync(path.join(project, 'package.json'), '{"name":"probe","private":true}\n');
  const main = path.join(project, 'node_modules', '@apackdev', 'terminal');
  fs.mkdirSync(main, { recursive: true });
  fs.copyFileSync(path.join(NODE_DIR, 'index.js'), path.join(main, 'index.js'));
  fs.writeFileSync(path.join(main, 'package.json'), JSON.stringify({ ...MAIN, ...(gitHead ? { gitHead } : {}) }));
  let directory;
  if (platform !== null) {
    directory = path.join(project, 'node_modules', '@apackdev', `terminal-${PLATFORM}`);
    fs.mkdirSync(directory, { recursive: true });
    const manifest = { name: `@apackdev/terminal-${PLATFORM}`, version: MAIN.version, files: platform.listed };
    fs.writeFileSync(path.join(directory, 'package.json'), JSON.stringify(manifest));
    for (const file of platform.present ?? platform.listed) fs.writeFileSync(path.join(directory, file), file);
  }
  return { project, directory, module: createRequire(path.join(project, 'package.json'))('@apackdev/terminal') };
}

test('names this platform, its binary and its package', () => {
  const { module } = install();
  assert.equal(module.PLATFORM, PLATFORM);
  assert.equal(module.BINARY_NAME, BINARY);
  assert.equal(module.PACKAGE_NAME, `@apackdev/terminal-${PLATFORM}`);
});

test('answers the binary in the installed platform package when every file it lists is there', () => {
  const { module, directory } = install();
  assert.equal(fs.realpathSync(module.brokerDirectory()), fs.realpathSync(directory));
  assert.equal(module.brokerBinary(), path.join(module.brokerDirectory(), BINARY));
  assert.equal(module.brokerBinaryPath(), module.brokerBinary());
});

test('answers no binary when a file the platform package lists is missing', () => {
  const { module } = install({ platform: { listed: [BINARY, 'conpty.dll'], present: [BINARY] } });
  assert.equal(module.brokerBinary(), undefined);
  assert.equal(module.brokerBinaryPath(), path.join(module.brokerDirectory(), BINARY));
});

test('answers no binary when the binary itself is missing', () => {
  const { module } = install({ platform: { listed: [BINARY, 'LICENSE-MIT'], present: ['LICENSE-MIT'] } });
  assert.equal(module.brokerBinary(), undefined);
});

test('answers no binary when the platform package does not list one', () => {
  const { module } = install({ platform: { listed: ['LICENSE-MIT'] } });
  assert.equal(module.brokerBinary(), undefined);
});

test('answers absence, not a throw, when no platform package is installed', () => {
  const { module } = install({ platform: null });
  assert.equal(module.brokerBinary(), undefined);
  assert.equal(module.brokerBinaryPath(), undefined);
  assert.equal(module.brokerDirectory(), undefined);
});

test('a broken platform manifest is a failure, not an absence', () => {
  const { module, directory } = install();
  fs.writeFileSync(path.join(directory, 'package.json'), '{ not json');
  assert.throws(() => module.brokerBinary(), /package\.json/);
});

test('answers the commit a release packed it for, and none from a checkout', () => {
  assert.equal(install({ gitHead: 'abc123' }).module.commit, 'abc123');
  assert.equal(install().module.commit, undefined);
});

test('an ES module imports the same named exports', async () => {
  const { project, module } = install();
  const resolved = createRequire(path.join(project, 'package.json')).resolve('@apackdev/terminal');
  const imported = await import(pathToFileURL(resolved));
  for (const name of ['BINARY_NAME', 'PACKAGE_NAME', 'PLATFORM', 'brokerBinary', 'brokerBinaryPath', 'brokerDirectory', 'commit']) {
    assert.equal(imported[name], module[name], name);
  }
});

test('the manifest ships the runtime entry beside the types', () => {
  assert.deepEqual(MAIN.exports['.'], { types: './index.d.ts', default: './index.js' });
  assert.equal(MAIN.main, './index.js');
  assert.ok(MAIN.files.includes('index.js'));
  assert.ok(MAIN.files.includes('index.d.ts'));
});
