# Protocol

Newline-delimited JSON, both ways, over a Unix socket — or a named pipe on Windows, where the transport
refuses a filesystem path outright.

**Rust is the source of truth and `protocol/protocol.ts` is generated from it**:

```sh
TS_RS_EXPORT_DIR=protocol cargo test
```

The variable is not optional: `ts-rs` writes to a gitignored `bindings/` without it, which is a second
answer to the same question that nobody reads. The generated file is committed so a consumer reads it
without building Rust, and is meant to be treated there as a recorded artifact with `check` and `update`
halves — a protocol change nobody regenerated should fail a typecheck rather than become a strange bug.

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
