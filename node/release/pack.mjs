// Packs the npm packages a `bin-<commit>` release carries: one per platform from that platform's built files, and
// `@apackdev/terminal` itself, as `.tgz` files a consumer installs by URL (`.github/workflows/binaries.yml`).
//
//   node node/release/pack.mjs --commit <sha> --files <dir> --out <dir> [--view <dir>] [--repo <owner/name>]
//
// `--files` holds the release's `<platform>__<file>` assets for every platform.
//
// **The source manifest names versions; the released tarball names URLs.** `node/package.json` lists each platform
// package at exactly its own version, which is what an npm registry would resolve. Nothing is on a registry: the
// packages are attached to the release of the commit they were built from, so the `@apackdev/terminal` packed here
// names each platform package by the URL of that same release's tarball. Installing it by its own release URL
// therefore installs that commit's broker for the platform it runs on and nothing else, and the lockfile records
// each URL with its integrity. A release is never replaced, so a URL keeps naming the same bytes.
//
// Not shipped: `node/package.json`'s `files` lists what the package holds, and this is not among them.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

/** The package `node/` is, which this script reads its version and its files from */
export const NODE_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');

/**
 * Every platform the broker is built for (`binaries.yml`'s matrix), as Node names it, with the `os` and `cpu`
 * npm installs its package on and the files it must hold beside its binary. The Linux build needs glibc
 * (2.28 or later), so its package says so to an npm that reads `libc`.
 */
export const PLATFORMS = [
  { platform: 'darwin-arm64', os: 'darwin', cpu: 'arm64', binary: 'apack-terminal', beside: [] },
  { platform: 'linux-x64', os: 'linux', cpu: 'x64', libc: 'glibc', binary: 'apack-terminal', beside: [] },
  // The ConPTY the binary loads from its own directory (`windows/conpty/`)
  { platform: 'win32-x64', os: 'win32', cpu: 'x64', binary: 'apack-terminal.exe', beside: ['conpty.dll', 'OpenConsole.exe'] },
];

/** A platform's package name */
export const platformPackage = (platform) => `@apackdev/terminal-${platform}`;

/** The file `npm pack` writes for a package: the scope's `@` dropped and its `/` made a `-` */
export const tarballName = (name, version) => `${name.replace(/^@/, '').replace('/', '-')}-${version}.tgz`;

/** Where a release's asset is downloaded from */
export const releaseUrl = (repo, commit, file) => `https://github.com/${repo}/releases/download/bin-${commit}/${file}`;

/**
 * Fails unless the manifest names every platform package, and only those, each at exactly its own version: the
 * platform packages are packed at that version, so any other spec names a package no release holds.
 */
export function checkOptionalDependencies(manifest) {
  const named = manifest.optionalDependencies ?? {};
  const expected = PLATFORMS.map(({ platform }) => platformPackage(platform));
  const problems = [];
  for (const name of expected) {
    if (!(name in named)) problems.push(`${name} is missing`);
    else if (named[name] !== manifest.version) problems.push(`${name} is at ${named[name]}, not ${manifest.version}`);
  }
  for (const name of Object.keys(named)) {
    if (!expected.includes(name)) problems.push(`${name} is not a platform the broker is built for`);
  }
  if (problems.length > 0) {
    throw new Error(
      `${manifest.name}'s optionalDependencies must name each platform package at exactly ${manifest.version}: ${problems.join('; ')}`,
    );
  }
}

/** The manifest of one platform's package, holding `files` (the binary first) */
export function platformManifest(main, { platform, os: osName, cpu, libc }, files, commit) {
  return {
    name: platformPackage(platform),
    version: main.version,
    description: `The apack-terminal broker's release build for ${platform}, with the files it is shipped with. Installed by @apackdev/terminal.`,
    license: main.license,
    repository: main.repository && { ...main.repository, directory: undefined },
    os: [osName],
    cpu: [cpu],
    ...(libc ? { libc: [libc] } : {}),
    files,
    gitHead: commit,
    publishConfig: { access: 'public' },
  };
}

/** `@apackdev/terminal`'s manifest as the release packs it: each platform package named by its release URL */
export function releasedMainManifest(main, repo, commit) {
  checkOptionalDependencies(main);
  const { scripts: _scripts, ...rest } = main;
  const optionalDependencies = Object.fromEntries(
    Object.keys(main.optionalDependencies).map((name) => [name, releaseUrl(repo, commit, tarballName(name, main.version))]),
  );
  return { ...rest, optionalDependencies, gitHead: commit };
}

function npmPack(directory, out) {
  const packed = spawnSync('npm', ['pack', '--json', '--pack-destination', out], {
    cwd: directory,
    encoding: 'utf8',
    shell: process.platform === 'win32',
  });
  if (packed.status !== 0) throw new Error(`npm pack in ${directory} failed: ${packed.stderr}`);
  const [{ filename }] = JSON.parse(packed.stdout);
  return path.join(out, filename);
}

function stage(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `apack-terminal-${prefix}-`));
}

/** `@apackdev/terminal-view`'s manifest as the release packs it: stamped with `gitHead`, no dev/build scripts. */
export function releasedViewManifest(viewManifest, commit) {
  const { scripts: _scripts, devDependencies: _devDeps, ...rest } = viewManifest;
  return { ...rest, gitHead: commit };
}

/** Packs `@apackdev/terminal-view` from a built `view/` directory with `gitHead` stamped on its manifest. */
export function packView({ commit, viewDir, out }) {
  const viewManifest = JSON.parse(fs.readFileSync(path.join(viewDir, 'package.json'), 'utf8'));
  const released = releasedViewManifest(viewManifest, commit);
  fs.mkdirSync(out, { recursive: true });
  const directory = stage('view');
  try {
    for (const entry of viewManifest.files ?? []) {
      const src = path.join(viewDir, entry);
      if (!fs.existsSync(src)) throw new Error(`${viewDir} is missing required release entry ${entry}`);
      fs.cpSync(src, path.join(directory, entry), { recursive: true });
    }
    for (const required of ['index.js', 'index.d.ts']) {
      if (!fs.existsSync(path.join(directory, 'dist', required))) {
        throw new Error(`${viewDir} has no built dist/${required} (run npm --prefix view run build first)`);
      }
    }
    fs.writeFileSync(path.join(directory, 'package.json'), `${JSON.stringify(released, null, 2)}\n`);
    return npmPack(directory, out);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

/** Packs every platform's package, `@apackdev/terminal`, and optionally `@apackdev/terminal-view` into `out`. */
export function packAll({ commit, files, out, repo, viewDir }) {
  const main = JSON.parse(fs.readFileSync(path.join(NODE_DIR, 'package.json'), 'utf8'));
  const released = releasedMainManifest(main, repo, commit);
  fs.mkdirSync(out, { recursive: true });
  const tarballs = [];

  for (const target of PLATFORMS) {
    const prefix = `${target.platform}__`;
    const names = fs
      .readdirSync(files)
      .filter((file) => file.startsWith(prefix))
      .map((file) => file.slice(prefix.length));
    for (const required of [target.binary, ...target.beside]) {
      if (!names.includes(required)) throw new Error(`${files} has no ${prefix}${required}`);
    }
    const ordered = [target.binary, ...names.filter((name) => name !== target.binary).sort()];
    const directory = stage(target.platform);
    try {
      for (const name of ordered) fs.copyFileSync(path.join(files, prefix + name), path.join(directory, name));
      // A download from a workflow's artifacts loses the mode it was built with; the tarball carries this one
      fs.chmodSync(path.join(directory, target.binary), 0o755);
      const manifest = platformManifest(main, target, ordered, commit);
      fs.writeFileSync(path.join(directory, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`);
      tarballs.push(npmPack(directory, out));
    } finally {
      fs.rmSync(directory, { recursive: true, force: true });
    }
  }

  const directory = stage('main');
  try {
    for (const name of main.files) fs.copyFileSync(path.join(NODE_DIR, name), path.join(directory, name));
    fs.writeFileSync(path.join(directory, 'package.json'), `${JSON.stringify(released, null, 2)}\n`);
    tarballs.push(npmPack(directory, out));
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }

  if (viewDir) {
    const viewPkg = JSON.parse(fs.readFileSync(path.join(viewDir, 'package.json'), 'utf8'));
    if (viewPkg.version !== main.version) {
      throw new Error(`view/package.json is at ${viewPkg.version}, not ${main.version}`);
    }
    tarballs.push(packView({ commit, viewDir, out }));
  }
  return tarballs;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const { values } = parseArgs({
    options: {
      commit: { type: 'string' },
      files: { type: 'string' },
      out: { type: 'string' },
      view: { type: 'string' },
      repo: { type: 'string', default: 'spankyed/apack-terminal' },
    },
  });
  for (const option of ['commit', 'files', 'out']) {
    if (!values[option]) {
      console.error(
        `usage: node node/release/pack.mjs --commit <sha> --files <dir> --out <dir> [--view <dir>] [--repo <owner/name>]`,
      );
      process.exit(2);
    }
  }
  const viewDir = values.view ? path.resolve(values.view) : undefined;
  for (const tarball of packAll({ ...values, files: path.resolve(values.files), out: path.resolve(values.out), viewDir })) {
    console.log(tarball);
  }
}
