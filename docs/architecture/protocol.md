# Protocol

Newline-delimited JSON, both ways, over a Unix socket — or a named pipe on Windows, where the transport
refuses a filesystem path outright.

**Rust is the source of truth and `node/protocol.ts` is generated from it**:

```sh
cargo xtask generate         # re-derive it
cargo xtask check-generated  # fail when it has fallen behind
```

It is written **inside the npm package**, which is what ships it: a consumer imports these types from
`apack-terminal` like any dependency's, rather than keeping its own copy and a script to keep that copy
fresh. The file is committed, so reading it needs no Rust toolchain. `cargo xtask generate` rather than
`TS_RS_EXPORT_DIR=… cargo test`, because without that variable ts-rs writes to a gitignored `bindings/`,
which is a second answer to the same question that nobody reads.

JSON rather than a length prefix because the first consumer is a Node client and a debuggable frame is worth
more than a compact one. Nothing in the traffic makes framing the bottleneck. A length prefix is a change
`src/protocol.rs` can make alone.

## The handshake is first, or the connection is refused

```json
{"kind":"hello","protocol":1,"role":"client"}
```

The broker answers `hello` with its own version, or refuses with `HANDSHAKE`. Nothing can be asked before
it.

**It is a contract across a pin boundary.** A consumer pins a commit of this repository, so a stale pin has
to be a clear refusal rather than a message that means something else.

`role` is `client` or `lifetime`, and **a caller that omits it is a client** — so a raw caller, a probe or
anything speaking the older handshake behaves exactly as before. [design.md](design.md) has what each role
means for a session's lifetime.

## Requests

| kind | asks for |
|---|---|
| `hello` | the handshake, with a protocol version and a role |
| `spawn` | a shell in a new pty: `cols`, `rows`, `shell`, `args`, `env`, `cwd` |
| `write` | bytes the user typed |
| `resize` | the viewport changed; one size per session, because a pty has one |
| `screen` | the screen as the broker has it, with up to `history` lines above the viewport |
| `facts` | what the shell reported about itself: directory, command line, whether one is running, last status |
| `search` | where text appears in the screen and the history above it |
| `wait` | until idle, until text appears, or until the process ends, with a deadline |
| `kill` | end the session and its process tree |

`args` and `env` are how shell integration is arranged: the markers a terminal reads facts from come from
code the shell runs at startup, and every shell takes that a different way — bash wants `--init-file` and
PowerShell `-NoExit`, which are arguments; zsh wants `ZDOTDIR` and fish `XDG_DATA_DIRS`, which are
environment. **The broker chooses neither**: it spawns what it is handed, because the files those point at
belong to whoever ships them.

## Events

| kind | says |
|---|---|
| `hello` | the broker's own protocol version |
| `spawned` | a session exists and its process is running, with its pid |
| `output` | bytes the process wrote. A feed: unprompted, and nobody asked |
| `exited` | the process ended; `code` is absent when a signal ended it |
| `throttled` | a flush waited for credit, with the microseconds it waited. Not a loss |
| `screen` | the lines a `screen` asked for, the cursor, and whether an alternate-screen app is showing |
| `facts` | the shell's claims about itself |
| `search` | where the text was found, with the whole line for context |
| `waited` | how a wait ended: `MET`, `TIMEOUT` or `EXITED`. Once per wait, however it ended |
| `failed` | a request could not be carried out |

Lines in a `screen` are **text, not cells**: a wide character appears once rather than beside the spacer cell
it occupies, and the padding a row carries to its full width is not there. What it answers is what was
written, which is what matching against it means.

## What crosses as output is text, and that is a decision

**The broker forwards decoded text, not bytes.** Both ends are terminal emulators — `alacritty_terminal`
here, the consumer's in its window — and both replace an invalid UTF-8 sequence with U+FFFD independently,
so a byte-exact pipe would show the user exactly what this does. Nothing in this protocol consumes raw
bytes either: `screen` answers lines, a search is over text, a marker is a claim. The field is named `text`
for that reason; it was `data`, which reads as bytes to everyone.

A **UTF-8** sequence split across two reads is a different matter and is not lossy: the coalescer holds an
incomplete trailing sequence back until it completes (`output::sendable`), so the only thing ever replaced
is what was never valid. Before that, `é` arriving as `a\xC3` then `\xA9b` was delivered with both halves
replaced.

**An event is a fragment of a stream, not a unit of anything.** A whole character is the only boundary
promised — at most three bytes are ever held back, that being the longest tail of an incomplete one — and an
*escape* sequence is neither covered nor coverable: `\x1b[?1049h` is eight bytes of valid ASCII, and an OSC
payload has no length at all, so holding a partial one means stalling the feed on a sequence that may never
terminate, or inventing a cap. That trades a feed every incremental parser already handles for latency and a
new way to fail, so the contract stays where it is.

What it costs a consumer is one rule: **never match a pattern against a single event.** Both ends that read
this feed already obey it — a VT parser is a state machine, and the marker scanner carries state across
chunks — and anything else accumulates first.

**Why not base64 or a binary frame — measured 2026-11-08.** JSON escapes a control byte as `\u001b`, six
bytes for one, so its cost is `1 + 5 ×` the control-byte fraction, against base64's flat 1.33×. The
crossover is 6.7% control bytes, and real terminal output sits below it:

| sample | control bytes | JSON | base64 |
|---|---|---|---|
| ordinary build output | 1.1% | **1.01×** | 1.33× |
| `ls --color` | 6.5% | **1.22×** | 1.33× |
| a full-screen TUI redraw | 2.4% | **1.12×** | 1.33× |

So base64 would make every one of those *larger* and cost the frames their readability, which this protocol
values. Revisit only with a measurement showing sustained output above that crossover.

## The scrollback is one number, and the handshake answers it

`hello` carries `history`: how many lines the broker keeps for every session, drawn or not. A consumer sets
its emulator's scrollback from it rather than pinning its own, because anything beyond it cannot survive a
reattach — the broker is what answers a `screen` read when no window holds the bytes.

Two independent constants is what this replaced: 10,000 in apack's window against `HISTORY_LINES` of 1,000
here, so closing and reopening a window silently lost nine tenths of the scrollback. Raising the broker's is
a measured decision about its own memory — `alacritty_terminal` holds a grid per session — not a number to
pick twice.

## Errors are a closed set

`ErrorCode` generates as a TypeScript string union, so a caller switches on a value the compiler knows
rather than matching a message that is free to be reworded.

| code | means |
|---|---|
| `HANDSHAKE` | the first message was not `hello`, or its version is not ours |
| `MALFORMED` | a frame did not parse as a request |
| `NO_SUCH_SESSION` | no session has that id — it never existed, or it has already exited |
| `PTY` | the pty or the process refused: spawn, resize or write failed at the OS |

## Correlation

The frames carry no call of their own: **the session is the correlation**, and an answer must not be taken
for another session's ask. A handshake and a refusal of a frame that named no session are answered to the
connection that asked, rather than through the feed — otherwise a refusal of one connection's malformed
frame would land on whichever client was attached.
