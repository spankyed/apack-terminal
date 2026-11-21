// Where the broker binary this package installed is, asked of the platform package that holds it.
//
// **One package per platform, installed by `os` and `cpu`.** `@apackdev/terminal` names
// `@apackdev/terminal-<platform>` for each platform the broker is built for as an optional dependency, and each of
// those declares the `os` and `cpu` it runs on, so npm installs the one this machine is and skips the others. That
// package holds the release build of one commit and the files it is shipped with (on Windows the ConPTY the binary
// loads from its own directory, and on every platform the notices its licenses ask a redistribution to carry).
//
// CommonJS so that `require`, `createRequire` and `import` all reach it: Node gives an ES module this file's named
// exports.
'use strict';

const fs = require('node:fs');
const path = require('node:path');

/** Node's name for the platform this process runs on, which is how the platform packages are named */
const PLATFORM = `${process.platform}-${process.arch}`;

/** What the broker's binary is called on this platform */
const BINARY_NAME = process.platform === 'win32' ? 'apack-terminal.exe' : 'apack-terminal';

/** The package that holds this platform's broker, whether or not this platform has one */
const PACKAGE_NAME = `@apackdev/terminal-${PLATFORM}`;

/**
 * The apack-terminal commit whose release this package was packed for, or `undefined` for a package packed from
 * a checkout: the release's tarball carries it as `gitHead`, and its optional dependencies are that commit's
 * platform tarballs (`node/release/pack.mjs`).
 */
const commit = require('./package.json').gitHead;

/** The installed platform package's manifest and directory, or `undefined` when it is not installed */
function installed() {
  let manifestPath;
  try {
    manifestPath = require.resolve(`${PACKAGE_NAME}/package.json`);
  } catch (error) {
    if (error && error.code === 'MODULE_NOT_FOUND') return undefined;
    throw error;
  }
  return { manifest: JSON.parse(fs.readFileSync(manifestPath, 'utf8')), directory: path.dirname(manifestPath) };
}

/** The directory holding this platform's files, or `undefined` when its package is not installed */
function brokerDirectory() {
  const found = installed();
  return found && found.directory;
}

/** Where the binary is in this platform's installed package, whether or not the file is there */
function brokerBinaryPath() {
  const directory = brokerDirectory();
  return directory && path.join(directory, BINARY_NAME);
}

/**
 * The broker binary, or `undefined` when it is not installed: this platform has no package (none is built for it,
 * or the install skipped optional dependencies), or not every file its package lists is there. All of them,
 * since a Windows binary without its ConPTY beside it runs on the system's.
 */
function brokerBinary() {
  const found = installed();
  if (found === undefined) return undefined;
  const files = Array.isArray(found.manifest.files) ? found.manifest.files : [BINARY_NAME];
  if (!files.includes(BINARY_NAME)) return undefined;
  return files.every((file) => fs.existsSync(path.join(found.directory, file)))
    ? path.join(found.directory, BINARY_NAME)
    : undefined;
}

module.exports = { BINARY_NAME, PACKAGE_NAME, PLATFORM, brokerBinary, brokerBinaryPath, brokerDirectory, commit };
