# Design

The broker is one process that owns ptys, so that no terminal session's lifetime is the lifetime of
whatever asked for it. Everything else here follows from that sentence.

## The two layers

**The process layer** — `session.rs`, `output.rs`, `tree.rs` — spawns a shell in a pty, moves bytes, owns a
lifetime and owns the process tree on the platform that needs one owned. Nothing in it knows what a screen
is. `cols` and `rows` pass through to the pty because that is the pty's own shape, not because this layer
models a viewport.

**The terminal capability over it** — `screen.rs`, `vt/`, `view/`, `commands.rs`, `recording.rs` — is the
VT core and what is told of it: a grid, a cursor, which screen is showing, the history above the viewport,
the facts a shell reported about itself, the rows a window is sent to draw, and the recording a session
can be picked up from.

The split is visible in what each file does *not* do, which is the only way a layer stays one.

## One interpreter, and views over its rows

The broker is the only thing that interprets a program's bytes, with `libghostty-vt`. A window draws a
session from rows the broker sends it and interprets nothing ([views.md](views.md)), so there is one copy
of a screen and nothing for two copies to disagree about.

A session nobody is drawing still has a screen, which is the whole gain: `screen`, `facts`, `search` and
`wait` answer whether or not a window ever opened one, and a window that opens one later is sent it whole.

A session keeps a hundred thousand lines of history, bounded in bytes as well, and compresses it once the
session has gone two seconds without output (`housekeeping.rs`). A view is sent none of it until it
scrolls there.

## A session's lifetime belongs to whoever claims it

Sessions are the broker's, not a connection's. They were a connection's while the only question was whether
the seam worked, and keeping them there made a session's lifetime the lifetime of whichever process opened
it — the thing this component exists to stop.

`Role` says what a connection is for:

- a **`lifetime`** connection holds the sessions, and ending it ends them all;
- absent one, the last **`client`** leaving ends them, which is what the broker has always done.

The app's own process holds the former, so restarting the service that opened a session does not end it, and
quitting the app does — **by the kernel**, since a descriptor closes on process death however the process
died. There is no shutdown handler to skip. It is the same move `tree.rs` makes one level down with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, where a kill, a disconnect and a crash all reap the tree by one
mechanism rather than by three paths remembering to.

**Getting this wrong cannot orphan a shell.** A connection that names no role is a client, so a raw caller,
a probe or anything speaking the older handshake behaves as it did. Persistence has to be taken
deliberately; the failure mode is a session that does not outlive its client, never a shell left running
after the app is gone.

A broker nobody is connected to exits. The count is taken from the accept rather than from the handshake,
which is what makes zero a safe moment: a caller that has connected and not yet greeted is already counted,
so it cannot be hung up on by a broker deciding nobody wants it.

## Output is text, and the scrollback is one number

Both are in [protocol.md](protocol.md): the broker forwards decoded text because both ends replace an
invalid sequence identically and nothing here consumes bytes (base64 was measured and is *larger* on real
output), and `hello` answers the history the broker keeps so a consumer's scrollback is derived rather than
pinned a second time.

## Output is coalesced and bounded

The reader does not decide what a message is. It hands what one `read` returned to the coalescer, which
flushes on a window or a cap, so N bytes of output is not N messages. The bound is credit: a flush spends
it and gets it back when the bytes are gone, which makes "in flight to the window" the thing being bounded
rather than "allocated".

An overload is **reported, never silent**: `Throttled` carries the microseconds a flush waited. Microseconds
because milliseconds threw the measurement away — a full redraw fills the credit for a few hundred
microseconds, which truncated to `waited_ms: 0`, a report whose own number said nothing had happened.

Credit returns whether or not anyone was listening. A frame dropped for want of an attached client must not
hold a slot, or a client restarting would leave the channel permanently short.

## What a program does that is not on its screen

The core reports it through callbacks, and each becomes an event for the client that asked for the
session: a title, a bell, a clipboard write, a notification, progress. The broker acts on none of them —
whether a program's text reaches a clipboard is the app's decision, made where the person is.

Shell integration is read the same way. OSC 133 marks a prompt, a command's output and its end, which the
core understands; **OSC 633**, VS Code's set, carries the command line and the directory, which the core
hands over as a sequence it does not know (`markers.rs`). OSC 7 is read for the directory too. From these
come the facts a `facts` request answers, the events that say a command started or finished, and the
record of each command with the lines it is on (`commands.rs`), which is what lets a window jump between
commands and read one's output.

Everything a marker carries is a **claim**. It is written by whatever is running in the pty, which may be a
hostile program rather than a shell, so a payload is length-capped while it is collected and what is reported
is what was *said* — a path some program named, not a directory anything checked. Deciding what a claimed
path is good for belongs to the app.

## A session can be recorded, and picked up from its recording

Opt-in per session (`Spawn.record`), because a recording holds everything the program printed, including
what it printed once and erased. It is a log of what arrived — the pty's bytes and each resize, stamped —
with the screen written out whole every megabyte, so a replay starts from the last such keyframe before
the moment it wants rather than from the beginning (`recording.rs`). A keyframe carries the screen and a
thousand lines above it, waits for the primary screen, and is what lets a recording past its budget drop
its oldest part.

`Spawn.restore` begins a session with everything its recording holds, out of whatever application the
last one was left in, and carries the recording on in the same file. `Rewind` opens a recording as a
session with no process, which a view attaches to like any other, and `Seek` shows it at a moment. Nothing
a replay provokes is told again: no reply, clipboard write, bell or title.

## A broker somewhere else

`apack-terminal proxy <socket>` is a process's stdin and stdout as a connection to the broker on that
socket, started first if there is none (`proxy.rs`). Run under `ssh host` or `docker exec -i`, that is a
connection to a broker on another machine or in a container, in whichever role the caller greets with —
so a remote terminal is the same protocol over a different pipe. Proxies arriving together start one
broker between them.

## Nothing polls

A `wait` registers a condition that is checked where the bytes land. The broker already knows when something
arrived, so a caller waiting for a state costs a condition check per read rather than a request per tick. A
session ending settles every wait on it, because nothing is coming.

## What the broker will not do

- **No agent status rules, and no command semantics beyond what a shell reported.** A fact here was stated
  by the shell; a status rule would be the broker's guess about one. That belongs to the app.
- **No policy about what a reported path means.** It reports the claim.
- **No persistence it was not asked for.** The app owns the data; the broker owns processes and screens,
  and writes a session's recording only to the file a caller named for it.
- **No second answer to who ends a session.** One rule, in `Role`.
