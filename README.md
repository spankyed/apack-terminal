# apack-terminal

The terminal broker apack's app talks to: one process that owns ptys and their process trees, parses VT and
answers a protocol, so that no session's lifetime is tied to the app's API process.

Pinned into the monorepo the way `elmdb` is — a gitignored `vendor/apack-terminal` symlink beside a committed
`vendor/apack-terminal.rev`. The goal it belongs to is `docs/goals/goal-terminal-ownership.md` there.

## The candidate set, and what trying it settled

Phase S of that goal says to add the whole candidate set at once and settle the library questions by trying
them rather than by argument. `tests/candidates.rs` is those attempts, kept as tests so each answer stays
checked instead of becoming a note that rots. Resolved against Rust 1.99.1 and built together in 13.9s.

| crate | version | what trying it established |
|---|---|---|
| `portable-pty` | 0.9.0 | a pty runs a command and its output comes back. Containment, cancellation and ConPTY resize/exit are still Phase B's acceptance |
| `alacritty_terminal` | 0.26.0 | the VT core: bytes in, a screen read back cell by cell |
| `command-group` | 5.0.1 | a child spawns into a group this process owns, and killing the group reaps it |
| `ts-rs` | 12.0.1 | a Rust type generates its TypeScript declaration |
| `interprocess` | 2.4.7 | resolves and builds; the transport is not chosen yet |
| `insta`, `proptest` | 1.x | resolve and build, for the snapshot and boundary-splitting tests Phase P asks for |

**`termwiz` was a candidate and is not a dependency.** It is wezterm's terminal crate, from the same project
as `portable-pty`, and the question was whether a matched pair beat two ecosystems. The registry answered it:
**`wezterm-term`, wezterm's actual terminal model, is not published to crates.io** — termwiz offers a parser
and surface primitives, not the emulation — so using it would mean vendoring or a git dependency for the part
that matters. `alacritty_terminal` stands, now on a checkable ground rather than on lineage.

**What the Windows half cannot be settled here.** `command-group`'s reason for being chosen is
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, where the OS kills the tree when the handle closes — so that X1 is
answered by the kernel rather than by cleanup code. That is unreachable from macOS and belongs to a Windows
run, as the goal's acceptance says.

## The protocol

Rust is the source of truth and `protocol/protocol.ts` is generated from it, which is
`elmdb-node/protocol/`'s arrangement reused rather than chosen again:

```sh
TS_RS_EXPORT_DIR=protocol cargo test export_bindings
```

It is committed, so the app reads it without building Rust, and it is meant to be treated there as a
recorded artifact with `check`/`update` halves — a protocol change nobody regenerated should fail a
typecheck rather than become a strange bug. `ErrorCode` generates as a closed string union, so a caller
switches on a value the compiler knows instead of matching a message that is free to be reworded.

**It is a contract across a `.rev` boundary.** The monorepo pins a commit of this repository, so a stale pin
has to be a clear refusal: every connection's first message is `hello` with a `protocol` number, anything
else is refused with `HANDSHAKE`, and nothing can be asked before it. `tests/broker.rs` drives all of that
through a real socket in the real binary.

Newline-delimited JSON, because the first consumer is a Node client and a debuggable frame is worth more
here than a compact one — Phase G's readings put the terminal's cost elsewhere. A length prefix is a change
`src/protocol.rs` can make alone.

## The toolchain is pinned to the monorepo's

`rust-toolchain.toml` names the channel `elmdb` names. The monorepo's `scripts/linux-runner.ts` derives one
toolchain from every Rust checkout linked under `vendor/` and refuses a disagreement, naming both paths,
because the Linux test container builds them all with one image. Moving this pin means moving elmdb's in the
same change.
