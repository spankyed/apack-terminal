# Design

The broker is one process that owns ptys, so that no terminal session's lifetime is the lifetime of
whatever asked for it. Everything else here follows from that sentence.

## The two layers

**The process layer** — `session.rs`, `output.rs`, `tree.rs` — spawns a shell in a pty, moves bytes, owns a
lifetime and owns the process tree on the platform that needs one owned. Nothing in it knows what a screen
is. `cols` and `rows` pass through to the pty because that is the pty's own shape, not because this layer
models a viewport.

**The terminal capability over it** — `screen.rs`, `markers.rs` — is the VT core: a grid, a cursor, which
screen is showing, a bounded run of lines above the viewport, and the facts a shell reported about itself.

The split is visible in what each file does *not* do, which is the only way a layer stays one.

## Both sides parse, and only one is right

The app's renderer keeps xterm.js, because it owns interaction — selection, reflow, IME — and that copy is a
replica. This one is the authority, and nothing in the app may read the renderer's copy as a fact.

The screen is fed **before** the bytes leave for the renderer. That is the only direction in which a
disagreement is merely stale rather than wrong.

A session nobody is drawing still has a screen, which is the whole gain: `screen`, `facts`, `search` and
`wait` answer whether or not a window ever opened one.

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

## Markers are their own pass

`alacritty_terminal` implements the OSC codes a terminal acts on — a title, a colour, the clipboard — and
drops the rest. That is correct: a sequence it has no behaviour for must not disturb the screen. But shell
integration *is* those dropped sequences, so they are scanned from the same bytes before the grid ignores
them, and the stream reaches the grid untouched.

**OSC 633**, VS Code's set, because it is the one shells already emit and its scripts are readable. OSC 7 and
iTerm2's 1337 are read too, for a user whose own configuration emits one.

Everything a marker carries is a **claim**. It is written by whatever is running in the pty, which may be a
hostile program rather than a shell, so a payload is length-capped while it is collected and what is reported
is what was *said* — a path some program named, not a directory anything checked. Deciding what a claimed
path is good for belongs to the app.

## Nothing polls

A `wait` registers a condition that is checked where the bytes land. The broker already knows when something
arrived, so a caller waiting for a state costs a condition check per read rather than a request per tick. A
session ending settles every wait on it, because nothing is coming.

## What the broker will not do

- **No agent status rules, and no command semantics beyond what a shell reported.** A fact here was stated
  by the shell; a status rule would be the broker's guess about one. That belongs to the app.
- **No policy about what a reported path means.** It reports the claim.
- **No persistence of its own.** The app owns the data; the broker owns processes and screens.
- **No second answer to who ends a session.** One rule, in `Role`.
