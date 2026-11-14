# Design

The broker is one process that owns ptys, so that no terminal session's lifetime is the lifetime of
whatever asked for it. Everything else here follows from that sentence.

## What v1 is

The scope, platforms and crash behaviour are stated in the [README's status table](../../README.md#status);
this document says how each is built and where each platform's limits are. v1 is macOS and Linux in full,
and Windows with the POSIX-only cases gated and named (see Known limits). A session ends when the broker stops:
there is no handoff to a second broker, and a kept session is the one case that outlives its app.

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
so it cannot be hung up on by a broker deciding nobody wants it. A broker nothing *ever* connects to has no
connection closing to tell it so, and exits ten seconds after it began listening (`START_GRACE`; a case
about the wait gives it a shorter one through `APACK_TERMINAL_START_GRACE_MS`) — by connecting to
itself and hanging up, so the decision is taken where every other is, or outright when its own socket
cannot be reached, since then nothing else can reach it either.

**Ending a session ends it.** A session that is killed, or goes with its holder, is sent a hangup, which
is what a terminal closing is and what a shell tidies up on. A program may ignore one: two seconds later
a process that is still there is killed with its process group, by the same guarded signal a caller
could have sent (`session.rs`, `end_for_certain`), so a session that was ended is always reported
`exited`. On Windows the job object ends the tree when the session's record goes, and nothing is waited
for.

**A broker leaves with its recordings written.** A recording's last entries are on a thread of its own
when its session ends, and a thread ends with its process; every way a broker exits waits for the
recorders that have stopped, for five seconds at most.

## One broker a socket, and one session's work is no other's wait

**A broker holds a lock beside its socket for its life** (`listening.rs`). One that cannot take it exits
with a code of its own, 3, having touched nothing — so a second broker never takes the socket from a
first, which would leave the first holding its sessions with nothing able to reach them — and only the
lock's holder replaces a socket file, which is then known to be a dead broker's. The socket is made for
its owner alone, in a directory only its owner can put files in; a directory other users can write to is
refused.

**The record of sessions is locked to look one up and for nothing else** (`session.rs`). Everything done
*to* a session — a search of a long history, a recording replayed, a resize that reflows a hundred
thousand lines — is done with that lock let go, so it holds up that session alone. And nothing that has
something for a session's program waits for the program to take it: one thread per session writes to its
pty, from a queue everything else adds to (`writer.rs`), so a program that has stopped reading stalls its
own input and no request.

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

Everything a marker carries is a **claim**. It is written by whatever is running in the pty — a file shown
with `cat`, a log line, a program that is not the shell — so a payload is length-capped while it is
collected and what is reported is what was *said*: a path some program named, not a directory anything
checked. Deciding what a claimed path is good for belongs to the app.

## A session's marks can be vouched for

A claim is enough for a person looking at a terminal. It is not enough for anything that acts on a
record — a flow that goes on when "the build passed" — because a forged marker then makes it act on a
failure as a success. So a session can be spawned with a **key** (`Spawn.marks_key`), which its caller
also gives its shell and nothing else, and the mode holds for the session's whole life:

- **Keyed.** A command is recorded, and one is said to be running, only on the word of a mark that
  carries the key: `633;V;S;<number>;<key>;<line>` when one begins and
  `633;V;F;<number>;<key>;<status>;<cwd>` when it ends. The standard marks are still read by the core,
  which flags prompt rows from them, and the directory they carry is still reported; no record and no
  idle is taken from them. A `wait` for idle is that machine's idle.
- **Claimed.** A session spawned without a key believes every marker, and says so: how a command ended
  is `claimed`, never `exited`.

**The number and the key come before anything a command line or a path could hold**, and the last field
is taken whole, so nothing in a line moves where the key is looked for. A keyed mark is read into a type
of its own (`markers::Vouched`), and only a value of that type moves the record machine
(`commands::Machine`); a mark with the wrong key, a number that is not one or a field missing is refused
and counted, never read as a claim instead.

**Numbers only go up.** The key is in the terminal's own output — every mark carries it — so a log of the
session shown again, or its recording printed, is a stream of marks with the right key. What makes them
the past is the number: a start is accepted only past the last one accepted, and a finish only for the
command that is running. A start past a running command ends that one as `superseded`; a session that
ends with one running ends it as `session_ended`, and says so before it says it has exited.

**How a command ended is one value** (`CommandOutcome`): `running`, `exited` with a code, `unknown` with
why, or `claimed` with whatever status was printed. An exit code can be made only from a keyed finish
(`markers::ExitCode` has no other constructor), so nothing a session without a key heard has the type of
a status. **Refusals are counted and told** in `facts`, so a hook that is out of step with its session
and a stream that is forging both show, where a shell that marks nothing shows nothing.

**A keyed mark longer than the core keeps is still read**, cut short: a start keeps as much of its line
as arrived and a finish its status without its directory. What vouches for it comes first, and a command
with a very long line is running all the same — dropped, the session would say nothing is.

**What this is proof against is content, not code.** A file, a log or a web response printed into the
terminal by something that was never given the key cannot move a keyed session's records. A program
running as the user in that shell can read the key and write any mark it likes; nothing here is a
boundary against one, and nothing should be described as if it were.

## A session can be recorded, and picked up from its recording

Opt-in per session (`Spawn.record`), because a recording holds everything the program printed, including
what it printed once and erased. It is a log of what arrived — the pty's bytes and each resize, stamped —
with the screen written out whole every megabyte, so a replay starts from the last such keyframe before
the moment it wants rather than from the beginning (`recording.rs`). A keyframe carries the screen and a
thousand lines above it, waits for the primary screen and for the parser to be between sequences — a
replay may begin at any keyframe, and one taken part way through a sequence would be followed by the rest
of it — and is what lets a recording past its budget drop its oldest part.

The file is written by a thread of its own, so a slow disk holds up no session. What that leaves is this:
an entry is in the operating system's hands moments after it happened and survives the broker dying,
however it dies; nothing is synced, so losing power can lose a recording's last moments or tear its last
entry, which a reader stops at. A recording that cannot be written stops, and the session carries on
unrecorded (`sessions` says so). One session records to a file at a time, and a file that is there and is
not a recording is never emptied to make one.

**Nothing waits on a disk without a limit.** A session that replaces one just ended may find the last of
that one's recording still on its way to the file. It is waited for before the new program is started,
for five seconds at most (`RECORDER_PATIENCE`); past that the session starts unrecorded and `spawned`
says `recorded: false`, and a `rewind` of such a file is refused with a message saying it is still being
written. Asking after a recording makes nothing: a `rewind` of a path that is not there is refused, and
no directory is left behind by the asking.

`Spawn.restore` begins a session with everything its recording holds, out of whatever application the
last one was left in, and carries the recording on in the same file. `Rewind` opens a recording as a
session with no process, which a view attaches to like any other, and `Seek` shows it at a moment; it is
its opener's, and ends with the connection that asked for it. Nothing a replay provokes is told again: no
reply, clipboard write, bell or title. The commands it holds are the recording's, with the times they ran
— **those the shell marked since the keyframe the moment shown was replayed from**, which is at most a
megabyte of output back, and not every command the recording covers: a keyframe is a screen, and says
nothing of the commands that drew it.

**A recording says which key its stream was vouched with**, where it was. The stream holds the marks as
they were printed, the refused ones beside the believed, and a replay rebuilds the commands from them —
so after every keyframe of a keyed session, and where a session picks a recording up, an entry names the
key and the number of the last start accepted under it (`Entry::Marks`). A replay reads each stretch with
that key and that numbering: what the session refused is refused again, from whichever keyframe the
replay begins, and a command ends as it ended. The replay's key and numbering are its own; the session
doing the replaying keeps its key, its last number and its count of refusals. A recording of a session
without a key holds no such entry and is what it always was. The key is in every mark the stream holds
already, so the file keeps no secret the stream did not.

## A broker somewhere else

`apack-terminal proxy [<socket>]` is a process's stdin and stdout as a connection to the broker on that
socket, started first if there is none (`proxy.rs`). With no socket named it is the one place a user is
known to have on a machine nobody has set up: `$XDG_RUNTIME_DIR/apack-terminal/broker.sock`, or
`~/.apack-terminal/broker.sock` where there is no runtime directory. Run under `ssh host` or `docker exec -i`, that is a
connection to a broker on another machine or in a container, in whichever role the caller greets with —
so a remote terminal is the same protocol over a different pipe. Proxies arriving together start one
broker between them: each that finds none starts one, and all but the first to take the socket's lock
exit, leaving their proxies to connect to the one that is there. A proxy carries nothing until the kernel
has said the broker it reached runs as its own user, and when its input ends it goes on carrying what the
broker says until the broker has said it.

## Nothing polls

A `wait` registers a condition that is checked where the bytes land. The broker already knows when something
arrived, so a caller waiting for a state costs a condition check per read rather than a request per tick. A
session ending settles every wait on it, because nothing is coming.

## Known limits

- **A client that stops reading is let go once 64 MiB is waiting for it.** Each client has a queue and a
  writer of its own (`feed.rs`), so one that is connected and not reading holds up nothing but what it
  is told: a session that ends is still forgotten and its views told. Output is under credit, so a
  program that prints stalls rather than the queue growing (sixty-four messages in flight at most).
  Everything else a client is told (a title, a bell, a command ending, the answer to a request) spends
  none, because an answer made to wait for room is a deadlock, so it is counted instead: past the bound
  the queue is dropped and the connection closed, and the client is where one whose broker went away
  is. On Windows the queue ends and the connection does not, as for a view (`views.md`).
- **Windows builds and runs most of the suite, which is not all of it.** CI's Windows job builds the
  terminal core from source (nobody publishes one for Windows, so the job installs the Zig its commit wants
  and builds it for speed, as the library every other platform links was) and runs the suite. The suites
  that drive the real binary run there over a named pipe, with PowerShell as the shell (with the same `$ `
  prompt every case waits for): a view attaching, drawing, typing and being told its session ended; a
  proxy starting a broker, a second joining it and several arriving at once; a second broker refused the
  pipe the first holds; and kept sessions and recordings (`keeping`), 12 of its 15 cases. A named pipe is
  hung up by cancelling its pending I/O (`CancelIoEx`), since it cannot be shut down one way and a
  disconnect waits behind a read parked on the same handle.
  **Still POSIX only, each case says why:** what needs a POSIX tty mode (`stty`, `od`), a socket file or
  its permission bits, a default socket path checked for mode bits, or one direction of a connection
  closed. Five view cases run on POSIX and not yet on Windows, and fail there: two that place images (no
  placement is listed), the title and clipboard sequences (no answer), the mouse-mode report (no frame
  with mouse reporting on), and hanging up on a view that reads none of its answers. Their causes are not
  diagnosed. Gated and named in `tests/view_broker.rs`.
  What is known to differ on Windows: a view or a client past its limit is not hung up on (`views.md`); a
  proxy whose input ends leaves without carrying what the broker had still to say; `signal` answers
  `UNSUPPORTED`; and the pipe's directory and peer are not checked as a socket's are on POSIX.
- **A session ends when the broker stops** (decision D4 in the v1 goal). Accepted for v1: there is no
  handoff of a session to another broker.

- **A session that reads images by name reads what it is named.** `Spawn.image_files` has the core open
  the file or shared memory object a program's output names. The core refuses what is not a regular file
  and anything under `/proc`, `/sys` or `/dev`, and reads no more than an image may be — but it opens the
  path before it looks at what it is, and opening a FIFO waits for a writer. So a program in such a
  session that names a FIFO stops that session's output being read until something opens the other end
  (observed on macOS: the read did not return in four seconds). Nothing checks a path before the core
  opens it. That is why the option is the caller's, for a session running a program it trusts, and why a
  shell is not started with it.

## What the broker will not do

- **No agent status rules, and no command semantics beyond what a shell reported.** A fact here was stated
  by the shell; a status rule would be the broker's guess about one. That belongs to the app.
- **No policy about what a reported path means.** It reports the claim.
- **No persistence it was not asked for.** The app owns the data; the broker owns processes and screens,
  and writes a session's recording only to the file a caller named for it.
- **No second answer to who ends a session.** One rule, in `Role`.
