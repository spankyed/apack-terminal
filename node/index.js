// Where the broker binary is, asked of the package that ships it.
//
// **This is the package's whole job**, and the reason it exists rather than a path in the consumer: the
// binary's name differs by platform (`cargo` writes `apack-terminal.exe` on Windows), it sits beside this
// file rather than at a path anyone can derive from the consumer's own location, and a packaged app
// resolves it through Node's module lookup like any other dependency. A consumer that built the path
// itself was wrong about the extension on Windows and the terminal never started.
//
// `cargo xtask package` is what puts the binary here: a release build, copied in. It is gitignored, so a
// checkout that has not run that task holds none and this answers `undefined` — which is an absence the
// caller composes its own refusal from, not a throw, because in a development checkout the answer is the
// debug build somewhere else entirely.
'use strict';

const fs = require('node:fs');
const path = require('node:path');

/** What `cargo` names the broker on this platform */
const BINARY_NAME = process.platform === 'win32' ? 'apack-terminal.exe' : 'apack-terminal';

/** Where the binary would be, whether or not it is there: separate so a refusal can say what it looked for */
function brokerBinaryPath() {
  return path.join(__dirname, BINARY_NAME);
}

/** The broker this package ships, or `undefined` when it holds none */
function brokerBinary() {
  const binary = brokerBinaryPath();
  return fs.existsSync(binary) ? binary : undefined;
}

module.exports = { BINARY_NAME, brokerBinary, brokerBinaryPath };
