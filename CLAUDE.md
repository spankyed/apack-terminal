# CLAUDE.md

A terminal broker: one process that owns ptys so no session's lifetime is the lifetime of whatever asked
for it. Read [`docs/architecture/design.md`](docs/architecture/design.md) before changing anything, and the
document that owns the area you are changing — [`protocol.md`](docs/architecture/protocol.md),
[`testing.md`](docs/architecture/testing.md). [`docs/README.md`](docs/README.md) says what each directory
holds.

## Layout

| Path | What it is |
|---|---|
| `src/session.rs`, `src/output.rs`, `src/tree.rs`, `src/writer.rs` | the process layer: the pty, the lifetime, the bounded output channel, the owned process tree, and the queue everything typed at a program goes through |
| `src/screen.rs`, `src/markers.rs`, `src/commands.rs` | the terminal capability: the VT core, the facts a shell reported, and the commands it ran |
| `src/vt/` | the core itself, behind the one module that calls it: the terminal, input encoding, row and text reads, images |
| `src/feed.rs` | what is waiting to be written to the client a broker reports to: one bounded queue a client, and what becomes of what it will not write |
| `src/view/` | what a view is sent: the binary wire (`wire.rs`), what changed since a view's last frame (`publish.rs`), and a connection's bounded queue (`sink.rs`) |
| `src/holds.rs`, `src/housekeeping.rs`, `src/deadlines.rs` | what happens on a clock rather than on a byte: a synchronized-output hold reaching its limit, a quiet session's history being compressed, and a wait nothing met being given up on |
| `src/recording.rs` | a session's recording: the file, its keyframes, and reading it back |
| `src/protocol.rs`, `src/frames.rs` | every message a client sends and is sent, the one place their shape is defined, and how one is read off a connection |
| `src/listening.rs`, `src/security.rs`, `src/proxy.rs` | who may reach a broker: the lock that makes it the only one on its socket, the socket's directory and permissions, the Windows pipe's descriptor, and `proxy`, which is a process's stdin and stdout as a connection |
| `src/main.rs` | the process: a socket, a handshake, the request loop for a client and for a view |
| `src/lib.rs` | so the parser can be tested as a parser |
| `node/protocol.ts`, `node/view-vectors.json` | **generated** from `src/protocol.rs` and `src/view/wire.rs`; never edited by hand, and shipped by the package |
| `xtask/` | the repository's own tasks (`cargo xtask <task>`): the protocol's derivation, the mutation gate's own checks, and pre-push |
| `node/` | the npm package a consumer installs: the protocol's types, where the binary is, and the binary `cargo xtask package` stages into it |
| `view/` | the `apack-terminal-view` npm package: what a window draws a session with, from the rows `src/view/` sends. TypeScript on npm, vite and vitest (`npm test`, `npm run typecheck`, `npm run build` there); its codec (`view/src/broker/wire.ts`) is the other half of `src/view/wire.rs`, and its suite reads `node/view-vectors.json` in place, so a change to the wire is one commit holding both halves |

## Checks

**Run `cargo xtask pre-push` before every push.** It is every check CI fails a change on, the change's
mutants included, cheapest first, stopping at the first that fails — and it refuses a dirty tree, because
what is checked has to be what is pushed. A surviving mutant is killed, or excluded in `.cargo/mutants.toml`
with why. What it cannot run is CI's alone: the Windows and Linux suites.

**A mutant of process control is not a finding, it is the damage.** This broker signals processes, and the
suite asks it to — so a mutation of *where* a signal is aimed makes the real binary execute the broadcast on
whatever machine the gate is running on. `kill(0, …)` is every process in this process's group and
`kill(-1, …)` is every process the user owns; a run on a developer's machine sent the second and SIGKILLed
every application they had open, and no assertion written afterwards undoes that. Three things keep it from
recurring, and a change to this area keeps all three:

- **The unsafe value is unrepresentable.** A signal's target is a `SignalTarget`, whose field is private and
  which has no `Default`, so every `Ok(…)` replacement of a function returning one fails to compile — a
  mutant that does not compile never runs, which is the only safe outcome here.
- **The guard is excluded from mutation, with that reason** (`.cargo/mutants.toml`). A mutant of a guard is
  the guard gone, so this is the one exclusion in that file that is not about two expressions answering
  alike.
- **The suite's broker runs in a session of its own** (`setsid` in `tests/broker.rs`), so a group signal
  cannot reach the test runner or the shell that started it.

**Anything new here gets the same three before its first gate run** — a primitive that ends processes, empties
a directory or execs, mutated, runs on the machine you are sitting at. If it cannot be made unrepresentable,
the mutants step for it belongs in a container, not on a workstation.

```sh
cargo xtask pre-push                         # before a push: everything below, and the change's mutants
cargo nextest run --workspace                # every suite, each test in its own process
cargo clippy --workspace --all-targets -- -D warnings    # the gate is zero warnings
cargo fmt --all --check                      # the layout rustfmt.toml pins, which is elmdb's
cargo xtask generate                         # re-derive node/protocol.ts from src/protocol.rs
cargo xtask check-generated                  # fails when that file has fallen behind
cargo xtask check-exclusions                 # fails when a mutants exclusion excludes nothing
cargo xtask clear-mutants                    # removes the working copies a mutants run that was ended left behind
cargo xtask package                          # release build, staged into the npm package in node/
npm --prefix view test                       # the view's suite (also: run typecheck, run build)
```

- **Changed a protocol type?** `cargo xtask generate`, and commit `node/protocol.ts`. It is one place
  rather than the `TS_RS_EXPORT_DIR=protocol cargo test` it replaced, because without that variable ts-rs
  writes to a gitignored `bindings/`, which is a second answer to the same question that nobody reads. It
  lives *inside* the npm package, which is what ships it: a consumer imports the types from
  `apack-terminal` like any dependency's, rather than keeping a copy and a script to keep it fresh.
- **Changed anything a consumer pins?** The pin is a commit of this repository, so a stale pin has to fail
  loudly rather than behave strangely. That is what the `hello` exchange is for.
- **`PROTOCOL_VERSION` is 1 and a change to what a message means is not a reason to move it.** The consumer
  pins a commit of this repository and the two halves move in one change, so there is no consumer left
  reading an older shape; nothing here is published, so there is no third party either. A bump buys a number
  and costs a round of edits on both sides. What the handshake catches is a *link* pointing at a build from
  before the change, and it catches that by the numbers disagreeing — which works at 1 and does not need 1
  to become 2. **It moves when a consumer exists that cannot move with it**: a published package, or a
  release in someone else's hands. That is the condition, and it is a fact to check rather than assume.
- **The broker reaches a consumer as an npm package**, `node/`, whose one job is answering where the binary
  is: the name differs by platform and a consumer that derived the path itself was wrong about the
  extension on Windows, so the terminal never started there. `cargo xtask package` stages a release build
  into it, beside the protocol's types; the binary is gitignored, as elmdb gitignores the `*.node` its own
  package ships. A development
  checkout does not use it — the consumer prefers the debug build in `target/`, which is what a
  contributor editing the broker has.
- **Each test runs in its own process**, so a regression that loses a signal is killed at its deadline
  instead of hanging the run (`.config/nextest.toml`). A deadline there is several times the slowest
  reading, never near it: it is for a test that has stopped ending at all.

## Rules

- **A claim is not a fact.** Everything a marker carries is written by whatever is running in the pty,
  which may be a hostile program rather than a shell. Report what was *said*, cap what you accumulate, and
  leave what a claimed path is good for to the caller.
- **Only a mark that carries the session's key is more than a claim, and only against content.** A keyed
  session's commands come from `markers::Vouched` and from nothing else; a status that did not come from
  one is never given the type of an exit (`commands::Outcome`). A mark that fails to be vouched for is
  refused and counted, never read as a claim instead. None of it is a boundary against a program running
  as the user, which can read the key ([design.md](docs/architecture/design.md#a-sessions-marks-can-be-vouched-for)).
- **Feed the authority before the bytes leave.** The renderer's copy is a replica, and the only acceptable
  disagreement is one where the authority is ahead.
- **Nothing polls.** The broker knows when bytes arrived, so a condition is checked where they land. A
  timer is the symptom of a missing signal.
- **A partial result says so.** An overload is reported with what it cost; it is never silent.
- **One rule for who ends a session**, in `Role`, and its failure mode must stay "did not persist" rather
  than "left a shell running".
- **A platform claim is made on that platform.** Windows behaviour — Job Objects, ConPTY's missing EOF, its
  cursor-position request, `\r\n` submitting a line — is claimed on a Windows run or not at all. None of it
  is reachable from a Mac, and all of it was wrong before it was run there.
- **Test the parser at every byte boundary.** Output arrives in whatever sizes a `read` returned; a wrapper
  that buffers a whole chunk passes every test that feeds a stream whole.
- **Kill by a pid you captured**, never by matching a process by name.

## The toolchain is pinned to the consumer's

`rust-toolchain.toml` names the channel apack's `elmdb` checkout names. apack derives one toolchain from
every Rust checkout linked under `vendor/` and refuses a disagreement, naming both paths, because its Linux
test container builds them all with one image. Moving this pin means moving elmdb's in the same change.
