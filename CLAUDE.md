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
| `protocol/protocol.ts` | **generated** from `src/protocol.rs`; never edited by hand |

## Checks

```sh
cargo test                                   # every suite
cargo clippy --all-targets -- -D warnings    # the gate is zero warnings
cargo fmt --all --check
```

- **Changed a protocol type?** `TS_RS_EXPORT_DIR=protocol cargo test`, and commit `protocol/protocol.ts`.
  The variable is not optional — `ts-rs` writes to a gitignored `bindings/` without it, which is a second
  answer to the same question that nobody reads.
- **Changed anything a consumer pins?** The pin is a commit of this repository, so a stale pin has to fail
  loudly rather than behave strangely. That is what `PROTOCOL_VERSION` and the `hello` exchange are for.

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
