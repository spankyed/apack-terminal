// Runs the broker as a consumer reaches it: from a project that installed `@apackdev/terminal`, asks the package for
// this platform's binary and runs it (`.github/workflows/binaries.yml`, before and after the release is published).
//
//   node node/release/smoke.mjs <project dir>
//
// **Run with no arguments, which is the whole probe.** The broker answers that with its usage and exit 2, so it
// needs no socket and no cleanup, and what it proves is what can be wrong in a package: a binary built for another
// architecture never reaches its own argument check, and one the package did not carry is not there to run.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const args = process.argv.slice(2);
const requireView = args.includes('--require-view');
const positional = args.filter((arg) => arg !== '--require-view');
const project = path.resolve(positional[0] ?? '.');
const projectRequire = createRequire(path.join(project, 'package.json'));
const required = projectRequire('@apackdev/terminal');
const imported = await import(pathToFileURL(projectRequire.resolve('@apackdev/terminal')));

const fail = (message) => {
  console.error(`smoke: ${message}`);
  process.exit(1);
};

if (imported.brokerBinary !== required.brokerBinary) fail('import and require reach different modules');
const binary = required.brokerBinary();
if (!binary) fail(`${required.PACKAGE_NAME} is not installed, or not every file it lists is there`);
console.log(`smoke: ${required.PLATFORM} broker at ${binary}, commit ${required.commit ?? '(none)'}`);

// Only this platform's package: npm skipped the others by their `os` and `cpu`
const scope = path.join(project, 'node_modules', '@apackdev');
const others = fs
  .readdirSync(scope)
  .filter((name) => /^terminal-(darwin|linux|win32)-/.test(name) && `@apackdev/${name}` !== required.PACKAGE_NAME);
if (others.length > 0) fail(`another platform's package is installed too: ${others.join(', ')}`);

const ran = spawnSync(binary, [], { encoding: 'utf8', timeout: 30_000 });
if (ran.error) fail(`apack-terminal did not run: ${ran.error.message}`);
if (ran.status !== 2 || !/usage: apack-terminal/.test(ran.stderr ?? '')) {
  fail(`apack-terminal answered ${ran.status} and ${JSON.stringify(ran.stderr)}, not its usage`);
}
console.log('smoke: the broker binary runs');

const viewPkgPath = path.join(scope, 'terminal-view', 'package.json');
if (requireView || fs.existsSync(viewPkgPath)) {
  if (!fs.existsSync(viewPkgPath)) fail('@apackdev/terminal-view is not installed');
  const viewManifest = JSON.parse(fs.readFileSync(viewPkgPath, 'utf8'));
  if (typeof viewManifest.gitHead !== 'string' || !/^[0-9a-f]{40}$/.test(viewManifest.gitHead)) {
    fail(`@apackdev/terminal-view package.json has invalid gitHead: ${JSON.stringify(viewManifest.gitHead)}`);
  }
  if (required.commit && viewManifest.gitHead !== required.commit) {
    fail(`@apackdev/terminal-view gitHead (${viewManifest.gitHead}) != @apackdev/terminal commit (${required.commit})`);
  }
  const viewEntry = path.join(scope, 'terminal-view', 'dist', 'index.js');
  if (!fs.existsSync(viewEntry)) fail(`@apackdev/terminal-view is missing ${viewEntry}`);
  const viewMod = await import(pathToFileURL(viewEntry).href);
  for (const symbol of ['TerminalView', 'TerminalClient', 'WebGL2AtlasPainter', 'CanvasPainter', 'createPainter']) {
    if (typeof viewMod[symbol] !== 'function') {
      fail(`@apackdev/terminal-view missing expected export ${symbol}`);
    }
  }
  console.log(`smoke: @apackdev/terminal-view@${viewManifest.version} imports cleanly (gitHead ${viewManifest.gitHead})`);
}
