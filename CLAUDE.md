# CLAUDE.md

A terminal broker: one process that owns ptys so no session's lifetime is the lifetime of whatever asked
for it. Read [`docs/architecture/design.md`](docs/architecture/design.md) before changing anything, and the
document that owns the area you are changing — [`protocol.md`](docs/architecture/protocol.md),
[`testing.md`](docs/architecture/testing.md). [`docs/README.md`](docs/README.md) says what each directory
holds.

## Layout

| Path | What it is |
|---|---|
| `src/session.rs`, `src/output.rs`, `src/tree.rs` | the process layer: the pty, the lifetime, the bounded output channel, the owned process tree |
| `src/screen.rs`, `src/markers.rs` | the terminal capability: the VT core, and the facts a shell reported |
| `src/protocol.rs` | every message, and the one place their shape is defined |
| `src/main.rs` | the process: a socket, a handshake, the request loop |
| `src/lib.rs` | so the parser can be tested as a parser |
| `node/protocol.ts` | **generated** from `src/protocol.rs`; never edited by hand, and shipped by the package |
| `xtask/` | the repository's own tasks (`cargo xtask <task>`): the protocol's derivation, the mutation gate's own checks, and pre-push |
| `node/` | the npm package a consumer installs: the protocol's types, where the binary is, and the binary `cargo xtask package` stages into it |

## Checks

**Run `cargo xtask pre-push` before every push.** It is every check CI fails a change on, the change's
mutants included, cheapest first, stopping at the first that fails — and it refuses a dirty tree, because
what is checked has to be what is pushed. A surviving mutant is killed, or excluded in `.cargo/mutants.toml`
with why. What it cannot run is CI's alone: the Windows and Linux suites.

```sh
cargo xtask pre-push                         # before a push: everything below, and the change's mutants
cargo nextest run --workspace                # every suite, each test in its own process
cargo clippy --workspace --all-targets -- -D warnings    # the gate is zero warnings
cargo fmt --all --check                      # the layout rustfmt.toml pins, which is elmdb's
cargo xtask generate                         # re-derive node/protocol.ts from src/protocol.rs
cargo xtask check-generated                  # fails when that file has fallen behind
cargo xtask check-exclusions                 # fails when a mutants exclusion excludes nothing
cargo xtask package                          # release build, staged into the npm package in node/
```

- **Changed a protocol type?** `cargo xtask generate`, and commit `node/protocol.ts`. It is one place
  rather than the `TS_RS_EXPORT_DIR=protocol cargo test` it replaced, because without that variable ts-rs
  writes to a gitignored `bindings/`, which is a second answer to the same question that nobody reads. It
  lives *inside* the npm package, which is what ships it: a consumer imports the types from
  `apack-terminal` like any dependency's, rather than keeping a copy and a script to keep it fresh.
- **Changed anything a consumer pins?** The pin is a commit of this repository, so a stale pin has to fail
  loudly rather than behave strangely. That is what `PROTOCOL_VERSION` and the `hello` exchange are for.
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
