# The candidate set, and what trying it settled

The goal this broker belongs to says to add the whole candidate set at once and settle the library
questions by **trying** them rather than by argument. `tests/candidates.rs` is those attempts, kept as tests
so each answer stays checked instead of becoming a note that rots.

Resolved against Rust 1.99.1 and built together in 13.9s.

| crate | version | what trying it established |
|---|---|---|
| `portable-pty` | 0.9.0 | a pty runs a command and its output comes back |
| `alacritty_terminal` | 0.26.0 | the VT core: bytes in, a screen read back cell by cell |
| `command-group` | 5.0.1 | a child spawns into a group this process owns, and killing the group reaps it |
| `ts-rs` | 12.0.1 | a Rust type generates its TypeScript declaration |
| `interprocess` | 2.4.7 | the transport, on a Unix socket and a Windows named pipe |
| `insta`, `proptest` | 1.x | the grid snapshots and the byte-boundary property |

## `termwiz` was a candidate and is not a dependency

It is wezterm's terminal crate, from the same project as `portable-pty`, and the question was whether a
matched pair beat two ecosystems. The registry answered it: **`wezterm-term`, wezterm's actual terminal
model, is not published to crates.io** — termwiz offers a parser and surface primitives, not the emulation —
so using it would mean vendoring or a git dependency for the part that matters.

`alacritty_terminal` stands, on a checkable ground rather than on lineage.

## `command-group` is not in the dependency list either

Its reason for being chosen was `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, where the OS kills the tree when the
handle closes. That turned out to be a few lines against `windows-sys` directly (`src/tree.rs`), which is
one dependency rather than two and puts the mechanism where its comment can explain it.

The probe was still worth running: it is what established that the approach works before anything depended
on it.

## What a probe cannot settle

Holding the pty's slave keeps its fd open, so the master never sees EOF and a read loop blocks for ever
after the shell exits — found by the pty probe, and the reason the slave is dropped immediately after the
spawn.

ConPTY's own behaviours — no EOF on the master, the cursor-position request it waits on before
initialising, and `\r\n` rather than `\n` submitting a line — are not reachable from a Mac. They were
settled on a Windows run, which is the only thing that settles them.
