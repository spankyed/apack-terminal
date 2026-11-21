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
with the terminal written out whole every megabyte, so a replay starts from the last such keyframe before
the moment it wants rather than from the beginning (`recording.rs`). **A keyframe is the core's own
account of its terminal** (`vt/keyframe.rs`): the beginning of the snapshot `libghostty-vt` encodes, which
is everything that makes the terminal what it is — both screens, the cursor a program asked for and the
one it saved, modes, margins, tab stops, colours — and then its history from the newest line back, cut
where there are a thousand lines of it. A replay puts that terminal in place rather than redrawing one,
so it goes on exactly as the session did. The sequences that redraw a screen are not that: they are one
screen as the core's formatter writes it, which leaves out the cursor's shape, a saved cursor, and the
shell's screen under a full-screen program. That a terminal put back
goes on as the one written out is held as a property over generated output (`tests/keyframes.rs`). It
holds outside left and right margins; inside them it found what the core's snapshot does not hold — a
line about to wrap at a right margin short of the screen's edge, and which rows keep the shell's prompt
mark across a scroll — each pinned by a case that fails the day the core holds it.

A keyframe holds its terminal's images: the pixels a cut for its history leaves behind are kept after the
history, where the core takes them as well. Where they are more than a keyframe has room for it holds
none, rather than an image that would be drawn as an empty place, and what its recording then shows of
them is the ones sent after it. A terminal that does not fit a keyframe — an image a program is part way through sending is
megabytes written out with the screen — has its keyframe put off, and is recorded all the while.

**Keyframes are never more of a recording than its output.** The next keyframe falls due a megabyte of
output after the last, or as much output as the last held where that is more. A keyframe larger than the
distance is something growing in the terminal, and the core does not say what: nothing it can be asked
says a transmission is open until its last piece arrives, and until then the image is in the keyframe and
nowhere else. Taken every megabyte regardless, each would hold all of the image so far, and an image of
eight megabytes would be written again as twelve megabytes of keyframes — quadratic in the image, and with
the image itself enough to fill what may wait (`BACKLOG`) for a writer the machine is slow to run. Spaced
by their own size, the keyframes between two moments are at most the output between them and one keyframe
more, whatever is growing, and a replay feeds at most a megabyte after one or as much as it holds.

A keyframe is taken on whichever screen is showing, waits for the parser to be between sequences — which
is where the core writes a terminal out — and is what lets a recording past its budget drop its oldest
part. What the broker told a terminal is not that terminal's state and is not the keyframe's: how much
history a session keeps, the colours and name it answers with, the cursor a reset returns to, whether it
takes images and where its reports go are told again to a terminal put back (`vt/settings.rs`), so a
recording is replayed under the settings of the session replaying it.

**A keyframe is read by the core that wrote it.** The snapshot's format is the core's, and upstream says
of its first version that it will change. A recording therefore outlives a change of core as a log, not
as something to pick up: one whose keyframes this build's core does not read is begun again from the
first it does read, and a moment before that is not shown. That is the price of a keyframe that is the
whole terminal, and it is paid once, where the core is moved.

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
broker says until the broker has said it. **A connection let go of before the broker said a word was never
served**: a broker leaving — its last caller gone, or nothing having reached it within its start grace —
takes a connection that arrives as it goes into its queue and exits with it unread. So a proxy keeps what it
carried until the broker answers any of it, and carries it again on a connection made again.

## How a consumer gets the broker

**As npm packages attached to a release, one per platform.** `binaries.yml` builds every `master` commit for each
platform the app ships and publishes the release `bin-<commit>`, which carries the built files (`<platform>__<file>`,
with `SHA256SUMS`) and, packed from those same files, these `.tgz` files:

| Package | Holds |
|---|---|
| `@apackdev/terminal-darwin-arm64`, `-linux-x64`, `-win32-x64` | that platform's binary (executable), on Windows the ConPTY it loads from its own directory, and the notices, licenses and SBOM shipped beside it; `os` and `cpu` say where it installs |
| `@apackdev/terminal` | the protocol's types, the view's vectors, and `index.js`, which finds the binary in the platform package installed beside it |
| `@apackdev/terminal-view` | the view (`view/`), built |

A consumer depends on `@apackdev/terminal` by its URL on a release. Its optional dependencies name the three
platform packages by their URLs on that same release, so npm installs the one whose `os` and `cpu` match and skips
the others, and the lockfile records each URL with its integrity. A release is never replaced, so the URL keeps
naming the same bytes.

**The source manifest names versions; the released one names URLs.** `node/package.json` lists each platform
package at exactly its own version, which is what a registry would resolve, and its suite fails when one is not.
`node/release/pack.mjs` packs the platform packages at that version and packs `@apackdev/terminal` with those
specs replaced by the release's URLs and the commit as `gitHead`. Nothing is published to npm.

The runtime entry, CommonJS and so reachable by `require`, `createRequire` and `import` alike:

- `brokerBinary()`: the binary's path, or `undefined` when no package for this platform is installed or not every
  file it lists is there, since a Windows binary without its ConPTY beside it runs on the system's.
- `brokerBinaryPath()`, `brokerDirectory()`: where the binary is and the directory of this platform's files,
  `undefined` with no package.
- `BINARY_NAME`, `PLATFORM` (Node's `<platform>-<arch>`), `PACKAGE_NAME` (the package that holds this platform's
  broker), and `commit`, the commit a release packed the package for.

Before the release exists each platform installs the packed tarballs into a new project and runs the binary
`brokerBinary()` answers (`smoke`); after it, each installs `@apackdev/terminal` by its release URL, then again with
`npm ci`, and does the same (`consume`). The binary is run with no arguments and answers with its usage and exit 2.

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
- **Windows builds and runs the suite, less the cases Windows has nothing to test with.** CI's Windows job
  builds the terminal core from source (nobody publishes one for Windows, so the job installs the Zig its commit
  wants and builds it for speed, as the library every other platform links was) and runs the suite. The suites
  that drive the real binary run there over a named pipe, with PowerShell as the shell (with the same `$ `
  prompt every case waits for). A named pipe is hung up by cancelling its pending I/O (`CancelIoEx`), since it
  cannot be shut down one way and a disconnect waits behind a read parked on the same handle. **A client that
  finished is let go rather than hung up on** (`listening::let_go`): a disconnect throws away what the far end has
  not read, its last answers included, so only the pending I/O is cancelled and the handle closes unflushed,
  which leaves what was written for the far end to read.
  **Sessions run on the ConPTY the broker carries** (`windows/conpty/`): Microsoft's own, from its
  `Microsoft.Windows.Console.ConPTY` package, beside the binary. The one built into Windows drops the image
  APC, the mouse-mode request and a program's clipboard question before the broker sees them, so on it images
  and mouse reporting do not work at all.
  **Still POSIX only, each case says why, and each is something a Windows pipe or console does not have:** a
  socket file left behind or taken away, a directory's or a socket's mode bits (a pipe's access is its
  descriptor, `windows-pipe-access.md`), one direction of a connection closed, and a hangup a program ignores.
  What is known to differ on Windows: a proxy whose input ends leaves without carrying what the broker had still
  to say, and `signal` answers `UNSUPPORTED`.
- **A session ends when the broker stops** (decision D4 in the v1 goal). Accepted for v1: there is no
  handoff of a session to another broker.

- **A session that reads images by name has the broker read them, and the core reads none.**
  `Spawn.image_files` lets a program name an image's source — a file on this machine, or a shared memory
  object — rather than send its bytes. The core would open what is named itself, inside the write that
  parses the session's output and before it looks at what it opened, so a FIFO stopped the session being
  read (observed on macOS: the read had not returned after four seconds), and so did one under `/dev/shm`
  named as shared memory (glibc 2.41: still waiting after three seconds). So each command naming one is
  taken out of the stream (`vt/image_sources/`), its source read by the broker, and the core fed a
  transmission of the bytes, with the program's other keys as the core read them.
  - **The core decides everything up to the opening, and says so itself.** It is fed the command with
    a source it refuses before opening anything — a name with a NUL in it — with its file and shared
    memory sources allowed for that one command (`Load::asked`). It does all it would have done first:
    looks for a frame's image by id or number, refuses an id given with a number and a format it does
    not take, abandons what a program left part sent, lets go of an image of the same id. Its answer says
    whether it got as far as the source, and only then is the source read. A source that cannot be read
    is refused with that same answer. A command with neither an id nor a number gets no answer to read
    that from, so what the core would do with it is decided here, and one not to be read is refused by
    the core outright. So a session tells a program, keeps and shows what the core alone would;
    `tests/image_sources.rs` holds it to the core reading files itself, over every spelling of a command
    it can make and random spellings of a name.
  - **Opened without waiting, and asked about through what was opened.** On POSIX a source is opened
    with `O_NONBLOCK`, so a FIFO answers at once, and `fstat`ed on the same descriptor: only a regular
    file is read (a shared memory object is a regular file on Linux and of no file type on macOS), and
    not one on a filesystem mounted at or beneath `/proc`, `/sys` or `/dev` but for `/dev/shm` — those
    three, and on Linux every mount the mount table (`/proc/self/mountinfo`) lists beneath them, which is
    where debugfs, tracefs and the cgroups are — told apart by device rather than by path. A file is then
    read as any file is, waiting for its reads. On Windows the path is held to the core's rules before it
    is opened (no share, no device namespace, no device name), and what was opened to them after, with
    its type a disk file's. Nothing can be swapped in between the question and the read, because the
    question is asked of what was opened.
  - **Read with the screen let go of, and given up on once it stops answering.** The pty's reader stops
    at such a command, has its source read with nothing locked, and feeds the rest once it has
    (`Screen::feed_until_image`). A source that is slow to read holds up that session's own output, as a
    program writing slowly does, and nothing else: views, snapshots and other clients of the screen go
    on. **A read the system is in the middle of cannot be cancelled** — a file on a mount that has
    stopped answering holds `open` and `read` in a wait no signal ends — so it is made on a thread of its
    own, and every answer the system gives a read counts as progress (`image_sources::Worker`). After
    `QUIET` (five seconds) with none, the session stops waiting: the program is told no and its output
    goes on. A file read slowly is never cut off, only one that has stopped. The thread is left in its
    wait, and while it is, that session's next named source is refused without another being started,
    so a mount that never answers costs a session one thread.
  - **What the scanner holds is what the core keeps.** Where such a command begins is asked of the core,
    which begins one in more places than `ESC _ G`. Of its body only what the core keeps is held: its
    keys, read as the core reads them, and a name, decoded as the core's decoder decodes it; a command
    that names no source is fed as those keys, and one the core would drop is given to it as nothing.
  - **What was read is the core's to judge.** A source is read up to the most the core reads of one itself
    (400 MiB, `SOURCE_BYTES`) and sent whole: a transmission a piece to a command, a query in one command
    the core is let take whole for that command alone. So an image too large for a session's images to
    hold is refused by the core, in its own words. Each piece is let go of once it has been sent, so a
    source is held about once, as the core alone holds it — a query a third more, the core holding its
    one command as base64 until it is whole.

  Where a session answers otherwise than the core alone: a source given up on as quiet is refused as one
  that could not be read, where the core alone would have waited on it. No shared memory object is read
  on Windows, where the core reads none either. A source on a mount that has
  stopped answering holds its session's output for `QUIET`, and one thread for as long as it does not
  answer. The option stays the caller's, for a session running a program it trusts — a file it names is
  read as the user — and a shell is not started with it.

## What the broker will not do

- **No agent status rules, and no command semantics beyond what a shell reported.** A fact here was stated
  by the shell; a status rule would be the broker's guess about one. That belongs to the app.
- **No policy about what a reported path means.** It reports the claim.
- **No persistence it was not asked for.** The app owns the data; the broker owns processes and screens,
  and writes a session's recording only to the file a caller named for it.
- **No second answer to who ends a session.** One rule, in `Role`.
