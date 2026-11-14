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
it, and it is said once: a second `hello` on a connection is refused with `HANDSHAKE` and changes nothing,
the connection's role being what it is counted as and how it is spoken to from the first.

**It is a contract across a pin boundary.** A consumer pins a commit of this repository, so a stale pin has
to be a clear refusal rather than a message that means something else.

`role` is `client` or `lifetime`, and **a caller that omits it is a client** — so a raw caller, a probe or
anything speaking the older handshake behaves exactly as before. [design.md](design.md) has what each role
means for a session's lifetime.

**A client is told what the sessions report for as long as it reads.** The last client to greet is the
one reported to. What it has not yet read waits in a queue of its own, and a client that has left more
than 64 MiB unread is hung up on. The sessions are as they were, and a client that connects again and
greets is reported to from then on. Output never gets a client there, because one that stops reading
stops the program instead ([design.md](design.md)); what can is everything else, titles and answers
among it, left unread.

## Requests

| kind | asks for |
|---|---|
| `hello` | the handshake, with a protocol version and a role |
| `spawn` | a shell in a new pty: `cols`, `rows`, `shell`, `args`, `env`, `unset`, `cwd`, and optionally `colors`, `output`, `record`, `restore`, `images`, `image_files`, `marks_key` |
| `write` | bytes the user typed |
| `resize` | the viewport changed; one size per session, because a pty has one |
| `pause` | stop reading this session's pty, so its process feels the reader stop |
| `resume` | read it again, and send what arrived while it was not being read |
| `signal` | a signal to the session's process, its process group, or one process inside its tree |
| `owns` | whether a pid is inside this session's process tree |
| `screen` | the screen as the broker has it, with up to `history` lines above the viewport |
| `facts` | what the shell reported about itself: directory, command line, whether one is running, how the last one ended, how many have finished — and whether the session's marks are vouched for, and how many were refused |
| `search` | where text appears in the screen and the history above it |
| `wait` | until idle, until text appears, or until the process ends, with a deadline |
| `commands` | the commands the session's shell has marked, oldest first |
| `command_output` | what one of them printed, as text |
| `keep` | whether the session outlives whoever holds the broker |
| `sessions` | every session the broker has |
| `rewind` | a recording, opened as a session with no process |
| `seek` | show a rewound session's recording as it was at a moment |
| `kill` | end the session and its process tree; for a rewound session, close it. The process is asked first (a hangup) and killed two seconds later if it is still there, so `exited` always follows |

`output` says whether the session's bytes are sent as `output` at all. A caller that reads a program's
bytes wants them; a terminal a person looks at is drawn by a view, which is sent rows
([views.md](views.md)), and its bytes would cross the socket to be thrown away and stall the program on
their credit. `record` and `restore` are a file to record the session in and whether to begin with what it
holds; `images` is whether its programs may send any ([design.md](design.md)).

`image_files` is whether an image may be *named* rather than sent: as a file on this machine or a shared
memory object, which the terminal then reads. It is read only where `images` is on, and it is the caller's
to say because only the caller knows what a session's output is. A program on this machine could have read
the file itself; a file shown with `cat`, a log, or the far end of an `ssh` that spells the same request
is asking this machine to open a path of its choosing. So it is for a session started to run one program
the caller trusts, and absent — a shell — such an image is refused and the program told so
([design.md](design.md#known-limits)).

`marks_key` is what vouches for the shell's marks: 32 lowercase hexadecimal characters the caller also
gives the shell — the broker does not; it spawns what it is handed — and nothing else. A session spawned
with one records a command, and says one is running, only on the word of a mark carrying it; one spawned
without believes every marker and reports how a command ended as a claim. Which it is holds for the
session's whole life. A key that is not one is refused with `MALFORMED` rather than spawned without
([design.md](design.md#a-sessions-marks-can-be-vouched-for)).

**A `wait` for idle in a keyed session is met by a keyed finish and by nothing else.** A prompt mark is
what ends a command for a session without a key, and anything can print one.

**A `write` is queued, not written.** What is bound for a program — a `write`, and everything a view
types, pastes or points at — waits its turn for the program to read it, and the request is answered
without waiting: a program that has stopped reading holds up its own input and nothing else. Up to 16 MiB
may wait for one session; a `write` past that is refused with `PTY` rather than dropped or held.

**A rewound session lists the commands since its keyframe.** `commands` on one answers those the shell
marked between the keyframe the moment shown was replayed from and that moment, with the times they ran.

**A rewound session is its opener's.** It ends when the connection that asked for it closes, as well as
when it is killed: it is no process, nothing else knows its id, and a client that reconnects asks again.

A connection that greets with `role: "view"` leaves this protocol after the handshake: what it speaks is
[views.md](views.md).

**A kept session is a reason for the broker to stay.** A session is ended when the holder's connection
closes, and when the last client leaves a broker nobody holds; one that was asked to be kept is not, and
the broker exits only when nobody is connected and no session is left.

`args` and `env` are how shell integration is arranged: the markers a terminal reads facts from come from
code the shell runs at startup, and every shell takes that a different way — bash wants `--init-file` and
PowerShell `-NoExit`, which are arguments; zsh wants `ZDOTDIR` and fish `XDG_DATA_DIRS`, which are
environment. **The broker chooses neither**: it spawns what it is handed, because the files those point at
belong to whoever ships them.

`unset` is the other half of that, and it is what `env` cannot express: a session inherits the broker's
environment, so adding entries cannot make a name *absent*. A consumer spawning a program rather than the
user's shell often needs exactly that — a CLI handed a credential the app happens to carry would
authenticate as the app rather than as whoever the user signed in as. It is applied after `env`, so absence
is what a caller gets when it asks for both.

## Events

| kind | says |
|---|---|
| `hello` | the broker's own protocol version |
| `spawned` | a session exists and its process is running, with its pid. `recorded` says whether it is being recorded and `restored` whether its screen began as what the recording held — each `false` when it was not asked for, and when it was asked for and not got: a file that is not a recording is left alone, a file another session is recording to has one writer, and a file the session before is still writing the last of is waited for five seconds and no longer |
| `output` | bytes the process wrote. A feed: unprompted, and nobody asked |
| `exited` | the process ended: `code` when it exited, `signal` by name when one killed it, never both |
| `throttled` | a flush waited for credit, with the microseconds it waited. Not a loss |
| `screen` | the lines a `screen` asked for, the cursor, and whether an alternate-screen app is showing |
| `owns` | whether that pid is inside the session's tree; the pid is the correlation |
| `facts` | what the shell said about itself. `outcome` is how the last command to end ended, absent when none has. `finished` moves each time a command it reported starting has ended, so a caller that asks after a quick command has ended still learns one ran. It is compared for a difference only: it wraps, and a shell emitting both marker families moves it twice per command. `vouched` says whether the session was spawned with a key, which is whether `command`, `running`, `outcome` and `finished` come from keyed marks alone; `cwd` is a claim either way. `refused` counts the marks spelled as keyed ones that were not accepted |
| `search` | where the text was found, with the whole line for context |
| `waited` | how a wait ended: `MET`, `TIMEOUT` or `EXITED`. Once per wait, however it ended |
| `title`, `bell`, `clipboard`, `notification`, `progress` | what the program did that is not on its screen. Reported, never acted on |
| `command_started`, `command_finished`, `cwd_changed` | what the shell marked, as it happened. `command_finished` carries how it ended (`outcome`), and a keyed session that ends with a command running sends one for it before `exited` |
| `commands`, `command_output` | the answers to the requests of those names |
| `kept` | whether the session outlives the broker's holder, answering `keep` |
| `sessions` | every session there is: its size, whether it is kept and recorded, its title and its shell's claims |
| `rewound` | a recording opened as a session, with the moments it runs between |
| `sought` | the moment a rewound session now shows |
| `failed` | a request could not be carried out |

## How a command ended is one value, and it says who said so

Wherever a command's end is told — a record in `commands`, `command_finished`, `facts` — it is an
`outcome`, tagged by `kind`:

| outcome | means |
|---|---|
| `{"kind":"running"}` | it has not ended. A record still waiting at its prompt has not begun either, which its `started_ms` says |
| `{"kind":"exited","code":N}` | it exited with this status, as a mark carrying the session's key reported |
| `{"kind":"unknown","why":…}` | it is over, in a keyed session, and no keyed mark said how: `no_status` (the shell reported the end and had no status), `superseded` (a later command began while it ran), `session_ended` |
| `{"kind":"claimed","code":N\|null}` | something printed that it ended, with this status where it gave one. Every command that ended in a session spawned without a key, and none in one spawned with |

**`exited` is the only one that is an exit status.** A reader that wants to know a command succeeded asks
for `exited` with a code of zero, and nothing a program printed can be handed to it as that. There is no
bare status field anywhere to read instead.

A record's `cwd` is where the shell was: when the command began, in a session without a key, and when it
ended, in one with — which is what a keyed finish carries.

Lines in a `screen` are **text, not cells**: a wide character appears once rather than beside the spacer cell
it occupies, and the padding a row carries to its full width is not there. What it answers is what was
written, which is what matching against it means.

## Flow control has two classes, and a `pause` is the second

Credit bounds how much output may be in flight **for every session at once**, because one socket carries
them all; a consumer reading its socket more slowly gets that for free and stalls every terminal in the app
to slow one of them down. `pause` is the other class: one session stops being read because *its* consumer
asked.

**A paused session's pty is not read, rather than read and held.** The bytes stay in the kernel's buffer, it
fills, and the process blocks on its own write — so a pause is backpressure reaching the program, and
nothing accumulates in the broker. Two things follow, and both are the contract rather than a limitation:

- **A read already outstanding still lands.** The reader is parked before a `read`, so the chunk the
  outstanding one returns arrives after the pause was asked for. `node-pty`'s `pause()` is the same, and it
  is what a consumer handling a chunk means by pausing.
- **A paused session's screen stops being fed**, so `screen`, `facts` and a `text` wait describe it as it was
  when it was paused. An `exit` is unaffected: that is waited for on the child, not read off the pty.

**A pause does not outlive the client that asked for one.** A connection leaving resumes everything, because
the next client never asked and has no way to find out.

## A signal is for driving one program, and the broker checks the target is its own

`kill` ends a session and its tree, which is the whole of what a terminal needs. `signal` is for a consumer
driving a particular program and shutting it down the way that program documents — signal the CLI so the
servers it owns outlive its own shutdown handlers, then its group, then the group harder.

| scope | reaches |
|---|---|
| `leader` | the process the broker started for the session |
| `group` | the process group it leads, which is `kill(-pid)` and no more |
| `pid` | one process, which must be inside that session's tree |

**`group` is not the whole tree.** A shell with job control puts each background job in a process group of
its own, so a group signal does not reach one; `kill` is what ends everything, by the mechanism each
platform gives. What `group` is for is a program the broker started directly and the children it left in
its own group.

**The pid scope is checked, and that check is why it can be offered at all.** A pid is a number any caller
can name, so a socket that signals an arbitrary one on request is `kill(2)` for whatever can reach the
broker. A pid outside the named session's tree is refused with `OUTSIDE_SESSION`.

**POSIX only.** Windows has no signals, and what the job object can express is not this, so it answers
`UNSUPPORTED` rather than approximating it.

## The question the refusal answers is also a request of its own

`owns` asks whether a pid is inside a session's tree, and it is the same walk the `pid` scope refuses with —
so what a caller reads and what it would have been refused cannot disagree.

**It exists because a signal is an act.** A caller that wanted the answer rather than the effect had nothing
to ask, and reaching for the act to learn the answer is the mistake this removes. What needs the answer is a
claim a caller cannot check itself: a process says *"I am yours, pid N"* over a socket, where the pid is
whatever it chose to write. The alternative is a consumer walking the parent chain from outside — a second
implementation of the broker's own invariant, which can disagree with it.

**The pid is the correlation**, as the session is for every other answer: a consumer may have several claims
to admit at once, and an answer taken for another pid's is one admitted or refused wrongly.

**Windows answers `UNSUPPORTED` for now.** The route there is the job object `tree.rs` already holds —
`JobObjectBasicProcessIdList` lists what is in it — and a platform claim is made on that platform, so it is
unwritten rather than guessed. A refusal a caller can see is still the better of the two answers available:
a parent walk with `/bin/ps` finds no `ps` there, answers *not owned* for everything, and refuses every
claim without saying why.

## What crosses as output is text, and that is a decision

**The broker forwards decoded text, not bytes.** The terminal here replaces an invalid UTF-8 sequence with
U+FFFD, and so does anything a consumer would draw the bytes with, so a byte-exact pipe would show the user
exactly what this does. Nothing in this protocol consumes raw
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
a measured decision about its own memory — the core holds a grid per session — not a number to pick twice.

## Errors are a closed set

`ErrorCode` generates as a TypeScript string union, so a caller switches on a value the compiler knows
rather than matching a message that is free to be reworded.

| code | means |
|---|---|
| `HANDSHAKE` | the first message was not `hello`, its version is not ours, or `hello` was said a second time |
| `MALFORMED` | a frame did not parse as a request, or a field of one holds what it cannot: a `marks_key` that is not 32 lowercase hexadecimal characters |
| `NO_SUCH_SESSION` | no session has that id — it never existed, or it has already exited |
| `PTY` | the pty or the process refused: spawn or resize failed at the OS, or more is already waiting for the program to read than a session holds |
| `OUTSIDE_SESSION` | a `signal` named a pid that is not in that session's tree |
| `UNSUPPORTED` | this platform has no such operation — signals, on Windows |

## Correlation

The frames carry no call of their own: **the session is the correlation**, and an answer must not be taken
for another session's ask. A handshake and a refusal of a frame that named no session are answered to the
connection that asked, rather than through the feed — otherwise a refusal of one connection's malformed
frame would land on whichever client was attached.
