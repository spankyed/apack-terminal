//! The messages the app and the broker exchange, and the one place their shape is defined.
//!
//! **Rust is the source of truth and the TypeScript is generated**, which is `elmdb-node/protocol/`'s
//! arrangement reused rather than chosen again: `cargo xtask generate` writes `node/protocol.ts`, inside the
//! npm package, which ships it — so a consumer imports these types from `apack-terminal` like any
//! dependency's, and `cargo xtask check-generated` fails when the file has fallen behind the Rust.
//!
//! **It is a contract across a `.rev` boundary**, not an internal detail: the monorepo pins a commit of this
//! repository, so a message set that changes under a pinned app has to be a clear refusal. That is what
//! `PROTOCOL_VERSION` and the `Hello` exchange are for, and why every request carries the session it is
//! about rather than relying on an implied current one.
//!
//! **Newline-delimited JSON**, because the first consumer is a Node client and a debuggable frame is worth
//! more here than a compact one; nothing in the app's traffic makes framing the bottleneck, and Phase G's
//! reading says the terminal's cost is elsewhere. A length prefix is a change this file can make alone.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Bumped when a message's meaning changes. The app refuses a broker that does not answer with its own.
///
/// **Still 1, and a change to what a message means is not by itself a reason to move it.** The consumer
/// pins a commit of this repository and the two move in one change, so there is no pinned app reading an
/// older shape to protect — and nothing is published, so there is no third party either. What this is for
/// is the case that *can* happen: a checkout whose link points at a build from before the change, where a
/// clear refusal naming both numbers beats a message that means something else. It moves when a consumer
/// exists that cannot move with it.
pub const PROTOCOL_VERSION: u32 = 1;

/// A session's identity, minted by the broker: the app never invents one.
pub type SessionId = String;

/// What an absent `Spawn::output` reads as.
fn sent() -> bool {
    true
}

/// What the app asks the broker to do.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum Request {
    /// The first message on a connection; the broker answers `Hello` or refuses.
    ///
    /// `role` says what this connection is for, and a caller that omits it is a `client` — so a raw caller,
    /// a probe or an older client behaves exactly as before.
    Hello {
        protocol: u32,
        #[serde(default)]
        role: Role,
    },
    /// Start a shell in a pty. `shell` absent means the platform's default.
    ///
    /// **`args` and `env` are how shell integration is arranged**, and why they exist at all: the markers a
    /// terminal reads facts from — the working directory, prompt and command boundaries — come from code the
    /// shell runs at startup, and every shell takes that a different way. bash wants `--init-file` and
    /// PowerShell `-NoExit`, which are arguments; zsh wants `ZDOTDIR` and fish `XDG_DATA_DIRS`, which are
    /// environment. The alternative is typing into the user's interactive shell after it starts, which
    /// echoes, races startup and needs a branch per shell — and has no branch at all for PowerShell or cmd.
    ///
    /// The broker chooses neither: it spawns what it is handed. What each shell needs is the app's to know,
    /// because the rc files it points at are the app's to ship.
    Spawn {
        cols: u16,
        rows: u16,
        shell: Option<String>,
        /// The shell's own launch arguments, in order. Empty starts it as the user would.
        args: Vec<String>,
        /// Added to the environment the broker runs in, which is already stripped of the app's own
        env: Vec<(String, String)>,
        /// **Taken out of it, which `env` cannot express.** A session inherits the broker's environment, so
        /// a caller that wants a name *absent* from its child cannot do it by adding entries — and a caller
        /// spawning a program rather than the user's shell often does: a CLI handed an API key the app
        /// happens to carry would authenticate as the app rather than as whoever the user signed in as.
        ///
        /// Applied after `env`, so the absence is what a caller gets when it asks for both.
        unset: Vec<String>,
        cwd: Option<String>,
        /// **What the session is drawn in**, so a program that asks the terminal for its colours — to pick
        /// a light or a dark theme, usually — is told the ones on the screen. The broker answers that
        /// question and draws nothing, so without this it answers with a palette nobody is looking at.
        /// Absent leaves the terminal's own, which is what a caller with no window has.
        #[serde(default)]
        colors: Option<Colors>,
        /// **Whether the session's bytes are sent as `Output`.** A caller that reads a program's output —
        /// to parse it, to forward it — wants them. A terminal a person looks at is drawn by a view, which
        /// is sent rows, and its bytes would cross the socket to be thrown away; worse, they are bounded
        /// by credit, so an unread stream would stall the program. Absent means sent, as it always was.
        #[serde(default = "sent")]
        output: bool,
    },
    /// Bytes the user typed.
    /// `text`, not `data`: what crosses here is decoded text, which is the contract and not an accident.
    /// See `Event::Output`.
    Write { session: SessionId, text: String },
    /// The viewport changed. One size per session: a pty has one.
    ///
    /// `cell_width` and `cell_height` are one cell in pixels, as the window drawing the session measured
    /// it — what a program asking for the terminal's size in pixels is told. Zero, which is what an absent
    /// field reads as, means the caller measures nothing and whatever was last known stands.
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width: u16,
        #[serde(default)]
        cell_height: u16,
    },
    /// **Stop reading this session's pty**, so the process feels the reader stop.
    ///
    /// This is flow control for a consumer whose own sink is slower than a shell can write — the app's
    /// Claude integration drives a TUI through an image transport it has to let drain before the next
    /// frame. The alternative is a consumer that buffers whatever arrives, which moves the unbounded
    /// growth into it rather than removing it.
    ///
    /// **Per session, which is the whole reason it is a request.** A consumer could stop reading its
    /// socket instead and the existing credit would throttle the producer — but one socket carries every
    /// session, so that stalls all of them to slow one down.
    ///
    /// **The pty is not read while paused, rather than read and held.** The bytes stay in the kernel's
    /// buffer, it fills, and the process blocks on its own write; nothing accumulates here. So a pause is
    /// backpressure reaching the program, which is what a consumer asking for one means. What follows from
    /// that, and is the honest cost: a paused session's screen stops being fed, so `Screen`, `Facts` and a
    /// `Text` wait all describe the session as it was when it was paused. An exit is unaffected — that is
    /// waited for on the child rather than read off the pty.
    ///
    /// Pausing a session that is already paused does nothing, and a pause does not survive the client that
    /// asked for it: a connection leaving resumes everything, because the next client never asked.
    Pause { session: SessionId },
    /// Read this session's pty again, and send what has arrived since. Resuming a running session does nothing.
    Resume { session: SessionId },
    /// **Send a signal to a session's process, its process group, or one process inside its tree.**
    ///
    /// `Kill` ends a session and its tree, which is the whole of what a terminal needs. This is for a
    /// consumer driving a particular program and shutting it down the way that program documents: the
    /// app's Claude integration signals the CLI so the servers it owns outlive its own shutdown handlers,
    /// then the group, then the group harder — three signals to two targets, which one `Kill` cannot say.
    ///
    /// **The broker checks the target is its own**, which is why `Scope::Pid` is here rather than left to a
    /// caller holding a pid and a kill of its own: a pid is a number anyone can name, and a socket that
    /// signals an arbitrary one on request is a privilege escalation wearing a terminal's clothes. A pid
    /// that is not inside the named session's tree is refused by code (`ErrorCode::OutsideSession`).
    ///
    /// **POSIX only, and refused rather than approximated elsewhere.** Windows has no signals; its route to
    /// the same end is the job object `tree.rs` already holds, and what that can express is not this. A
    /// platform claim is made on that platform, so there it answers `ErrorCode::Unsupported`.
    Signal { session: SessionId, signal: Signal, scope: Scope },
    /// **Whether a pid is inside this session's process tree** — the session's own process, or anything it
    /// started, however deep.
    ///
    /// **A question, which is why it is a request of its own rather than a signal's refusal.** The broker
    /// checks exactly this before it signals a pid (`Scope::Pid`), but a signal is an act: a caller that
    /// wanted the answer rather than the effect had nothing to ask, and reaching for the act to learn the
    /// answer is the mistake this prevents. Both come from one walk, so the two cannot disagree.
    ///
    /// **What it is for is a claim a caller cannot check itself.** A process says *"I am yours, pid N"* over
    /// a socket, and the pid is whatever it chose to write; a consumer that walks the parent chain from
    /// outside is answering about a tree the broker owns, with a second implementation that can disagree
    /// with the one the refusal uses.
    ///
    /// The answer names the pid it is about, because a session may have more than one of these outstanding.
    Owns { session: SessionId, pid: u32 },
    /// **The screen as the broker has it**, with up to `history` lines above the viewport.
    ///
    /// This is the read that makes a session's state independent of a window: the broker parses the same
    /// bytes the renderer draws and is the authority on them, so a session nobody is looking at still
    /// answers. `history` is a ceiling and not a promise — a session that has scrolled less answers with
    /// less, and an alternate-screen app has no scrollback to answer with at all.
    Screen { session: SessionId, history: u32 },
    /// **What the shell has reported about itself** — the working directory, the command line it last said
    /// it was running, whether one is running, the status the last one ended with, and how many have ended.
    ///
    /// This is the read shell integration exists for, and it answers for a session nobody is drawing. Every
    /// field is a claim the program in the pty made, so what a caller may do with a reported path is the
    /// app's decision; the broker reports what was said.
    Facts { session: SessionId },
    /// Where `text` appears in the screen and the `history` lines above it, oldest first.
    ///
    /// A read of what is already parsed, bounded by the history asked for and by a cap on matches — so a
    /// caller looking for a prompt or an error pays a snapshot rather than a scan of everything a session
    /// ever wrote.
    Search { session: SessionId, text: String, history: u32 },
    /// **Waits for a session to reach a state, answering once.**
    ///
    /// The alternative is a caller asking `Facts` or `Screen` on a timer, which is the shape this repo
    /// refuses: the broker already knows when something arrived, so the condition is checked where the bytes
    /// land and nothing polls. `timeout_ms` is the caller's deadline — a wait that is never met answers
    /// `Timeout` rather than lasting for ever — and the answer comes exactly once however the race goes.
    Wait { session: SessionId, until: Until, timeout_ms: u32 },
    /// End the session and its process tree.
    Kill { session: SessionId },
}

/// The colours a session is drawn in, each as red, green and blue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct Colors {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: Option<[u8; 3]>,
    /// The first entries of the palette, in order — the sixteen a theme names. Entries past what is given
    /// keep the terminal's own.
    pub ansi: Vec<[u8; 3]>,
}

/// What a connection is for.
///
/// **A session's lifetime belongs to whoever claims it, not to the connection that asked for the session.**
/// The app's own process holds a `lifetime` connection for as long as the app is running, so a restart of
/// the service that *opened* a session does not end it, while quitting the app does — by the kernel, since
/// a descriptor closes on process death however the process died.
///
/// **Absent a `lifetime` connection the broker ends its sessions when the last client leaves**, which is
/// what it has always done. So persistence is something a caller takes responsibility for rather than
/// something it can acquire by accident: the failure mode of getting this wrong is a session that does not
/// outlive its client, never a shell left running after the app is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum Role {
    /// Asks for sessions and receives their output. The default.
    #[default]
    Client,
    /// Holds the broker's sessions alive and receives no output. Ending this connection ends them all.
    Lifetime,
    /// Draws sessions. After the handshake this connection speaks `view::wire` rather than these
    /// messages: it attaches to sessions by id, is sent their rows, and sends their input. It asks for no
    /// session and ends none, so one coming or going changes nothing but what is drawn.
    View,
}

/// A signal a `Signal` request may send.
///
/// **A closed set, because the point is a caller that cannot name something else.** An arbitrary number
/// would make this an interface to `kill(2)` rather than to a session, and the five here are what shutting
/// a program down is written with: hang up, interrupt, quit, terminate, and the one that cannot be caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export, export_to = "protocol.ts")]
pub enum Signal {
    Sighup,
    Sigint,
    Sigquit,
    Sigterm,
    Sigkill,
}

impl Signal {
    /// The number this is on this platform, from libc rather than written out here.
    #[cfg(unix)]
    pub fn number(self) -> i32 {
        match self {
            Self::Sighup => libc::SIGHUP,
            Self::Sigint => libc::SIGINT,
            Self::Sigquit => libc::SIGQUIT,
            Self::Sigterm => libc::SIGTERM,
            Self::Sigkill => libc::SIGKILL,
        }
    }
}

/// What a signal is sent to.
///
/// **The three are different questions, and a consumer needs all three.** Signalling the shell alone lets
/// it run its own shutdown; signalling the group is what reaches whatever it started; naming one process is
/// for a consumer that knows which of a tree's processes it means — the CLI's own pid, where the group
/// would take the servers it is still tidying up with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum Scope {
    /// The process the broker started for this session.
    Leader,
    /// The process group it leads, which is `kill(-pid)` and no more than that.
    ///
    /// **Not the whole tree, and the difference bites.** A shell with job control puts each background job
    /// in a process group of its own, so a signal to the leader's group does not reach one — `Kill` is what
    /// ends a session and everything under it, by the mechanism each platform gives (the kernel's SIGHUP to
    /// the session on POSIX, the job object on Windows). What the group *is* for is a program the broker
    /// started directly and the children it left in its own group, which is the case a consumer driving one
    /// CLI is in.
    Group,
    /// One process, which must be inside this session's tree or the request is refused.
    Pid { pid: u32 },
}

/// What a `Wait` is waiting for.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum Until {
    /// No command is running: the shell has reported a prompt or a finish. **Needs shell integration** —
    /// without the markers nothing ever reports a command starting, so a session that emits none is idle
    /// from the start and this answers at once. That is the honest answer for a session whose shell says
    /// nothing, rather than a guess read off the screen.
    Idle,
    /// `text` appears in the screen or its history. The fallback for a program that reports nothing, and the
    /// one a caller reaches for when waiting for something a shell has no marker for.
    Text { text: String },
    /// The process ended.
    Exit,
}

/// How a `Wait` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export, export_to = "protocol.ts")]
pub enum WaitOutcome {
    /// The condition held.
    Met,
    /// The deadline passed first.
    Timeout,
    /// The session ended before the condition held. Distinct from `Met` on an `Exit` wait, which that
    /// session reaching its condition *is*, and distinct from `Timeout` because nothing is coming.
    Exited,
}

/// What the kernel says is reading the pty, as against what the shell claims about itself.
///
/// **The foreground process group is the one answer that does not need the shell's cooperation.** A shell
/// with no integration installed emits no markers, so every field of `Facts` is empty for it and a caller
/// cannot tell a session sitting at a prompt from one three minutes into a build — which is the question a
/// tab title and a confirmation before closing are both asking.
///
/// **Absent on Windows**, which has no foreground process group: a ConPTY has no equivalent, and the route
/// there is the job object `tree.rs` already holds (`JobObjectBasicProcessIdList`). Unimplemented rather
/// than approximated, because a platform claim is made on that platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct Foreground {
    /// The process group leader reading the pty.
    pub pid: i32,
    /// Its name as the OS reports it. `None` when the process ended between being named and being looked
    /// up, which is ordinary rather than a failure.
    pub name: Option<String>,
}

/// Where a search found what it was asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct SearchMatch {
    /// The row the matching line starts on, into the lines the same request's `Screen` would answer.
    pub line: u32,
    /// Where in that line the match starts, in characters from the line's start.
    pub column: u32,
    /// The whole line, with the rows it wrapped onto joined, so a caller sees the context without a
    /// second read.
    pub text: String,
}

/// What the broker tells the app, unprompted or in answer.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum Event {
    /// The broker's own version, answering `Hello`.
    /// The broker's own version, and the history it keeps.
    ///
    /// **`history` is the one scrollback number, and it is the broker's because the broker is what has to
    /// answer for it.** A consumer's emulator holds whatever it was sent while attached; the broker holds
    /// `HISTORY_LINES` for every session whether or not anyone is drawing one. A consumer that scrolled
    /// further back than this would be showing lines that cannot survive a reattach — which is what two
    /// independent constants produced: 10,000 in the window against 1,000 here, so closing and reopening a
    /// window silently lost nine tenths of the scrollback. Taken from the handshake rather than pinned in
    /// both places, so the two cannot drift.
    Hello { protocol: u32, history: u32 },
    /// A session exists and its process is running.
    Spawned { session: SessionId, pid: u32 },
    /// Bytes the process wrote, **as text**. A feed: unprompted, and nobody asked for it.
    ///
    /// **The broker forwards decoded text, not bytes, and that is a decision rather than a compromise.**
    /// Both ends are terminal emulators — `libghostty-vt` here, the consumer's in its window — and both
    /// replace an invalid sequence with U+FFFD independently, so a byte-exact pipe would show the user
    /// exactly what this does. Nothing in this protocol consumes raw bytes either: `Screen` answers lines,
    /// a search is over text, and a marker is a claim. A *UTF-8* sequence split across two reads is held
    /// back until it completes (`output::sendable`), so the only thing replaced is what was never valid.
    ///
    /// **An event is a fragment of a stream and not a unit of anything, and that is the whole contract.**
    /// The only boundary promised is a whole character: at most three bytes are ever held back, because
    /// that is the longest tail of an incomplete one. An *escape* sequence is not covered and cannot be —
    /// `\x1b[?1049h` is eight bytes of valid ASCII, and an OSC string's payload has no length at all, so
    /// holding a partial one means either stalling the feed on a sequence that never terminates or
    /// inventing a cap. That would trade a feed every incremental parser already handles for latency and a
    /// new way to fail.
    ///
    /// So **a consumer must never match a pattern against one event.** Both ends that read this feed are
    /// already right: a VT parser is a state machine, and this broker's own marker scanner carries its
    /// state across chunks (`markers.rs`, cut at every boundary in `tests/markers.rs`). Anything else
    /// accumulates first. `tests/coalescing.rs`'s
    /// `an_escape_sequence_is_not_held_back_the_way_a_character_is` is that asserted rather than said.
    ///
    /// **Measured 2026-11-08, which is why this is not base64 or a binary frame.** JSON escapes a control
    /// byte as `\u001b`, six bytes for one, so its cost is `1 + 5 x` the control-byte fraction; base64 is a
    /// flat 1.33x. The crossover is 6.7% control bytes, and real terminal output sits below it: ordinary
    /// build output 1.1% (1.01x), `ls --color` 6.5% (1.22x), a full-screen TUI redraw 2.4% (1.12x). Base64
    /// would make every one of those *larger*, and cost the frames their readability.
    Output { session: SessionId, text: String },
    /// The process ended: `code` when it exited, `signal` when something killed it, and never both.
    ///
    /// **A signal is not an exit code, and reporting one as the other loses the only thing a consumer
    /// wanted to know.** A shell the user quit and a CLI the app terminated are the same `1` otherwise, so
    /// a window cannot tell a crash it should draw a seam for from the shutdown it asked for itself.
    ///
    /// `signal` is its name — `SIGTERM` — rather than a number, because a number is not the same number
    /// everywhere, and rather than the system's description of it, which is written for a person and
    /// translated. An unrecognised one is carried through as the system worded it, which is the honest
    /// answer for a signal this protocol has no name for.
    Exited { session: SessionId, code: Option<i32>, signal: Option<String> },
    /// **The channel was full and a flush had to wait.** Not a loss: output is held, the pty's buffer fills
    /// behind it, and the process feels it — so this says a producer was throttled, which the goal requires
    /// be observable rather than silent. Sent as an answer, so reporting the overload never queues behind
    /// the output that caused it.
    ///
    /// **Microseconds, because milliseconds threw the measurement away.** A full redraw by `less` fills the
    /// credit for a few hundred microseconds, which truncated to `waited_ms: 0` — a report whose number said
    /// no wait had happened, logged for every flush. The reading is kept whole here and what to make of it
    /// is the client's; `u32` because ts-rs generates a `bigint` for 64 bits and `JSON.parse` hands a client
    /// a number, and 71 minutes of waiting fits.
    Throttled { session: SessionId, waited_us: u32 },
    /// The screen a `Screen` request asked for: the history it had, then the viewport, oldest first.
    ///
    /// Lines are text, not cells: a wide character appears once rather than beside the spacer cell it
    /// occupies, and the padding a row carries to its full width is not there. What this answers is what was
    /// written, which is what a caller matching against it means.
    Screen {
        session: SessionId,
        lines: Vec<String>,
        /// For each line, whether it runs on into the next. A line longer than the grid is wide is drawn
        /// over several rows, and a caller reading them as text joins the ones this marks.
        wrapped: Vec<bool>,
        /// Within the viewport, so `0` is its first line however much history came with it.
        cursor_row: u16,
        cursor_column: u16,
        alt_screen: bool,
    },
    /// Whether that pid is inside the session's tree, answering `Owns`.
    ///
    /// **`pid` is the correlation**, as the session is for every other answer: a caller may ask about
    /// several at once, and an answer taken for another pid's is a claim admitted or refused wrongly.
    Owns { session: SessionId, pid: u32, owned: bool },
    /// What the shell reported about itself and what is running in the pty, answering `Facts`.
    ///
    /// **`cwd`, `command`, `running`, `exit_status` and `finished` are claims; `foreground` is a
    /// measurement**, and the
    /// two are not interchangeable — a shell with no integration installed claims nothing at all, and a
    /// program in the pty may claim whatever it likes. They travel in one event because a caller asking
    /// what a session is doing wants both and should not pay two round trips to find out.
    Facts {
        session: SessionId,
        cwd: Option<String>,
        command: Option<String>,
        running: bool,
        exit_status: Option<i32>,
        /// Moves each time the shell reports a command it started has ended. A caller that asks after a
        /// command has already ended learns one ran by this differing from what it last read; it is not
        /// an exact number of commands and it wraps, so it is compared for a difference only.
        finished: u32,
        foreground: Option<Foreground>,
    },
    /// Where a search found its text, answering `Search`.
    Search { session: SessionId, matches: Vec<SearchMatch> },
    /// How a `Wait` ended. Sent once per wait, whichever way it ended.
    Waited { session: SessionId, outcome: WaitOutcome },
    /// The program named its terminal (OSC 0, OSC 2). Empty when it took the name back.
    Title { session: SessionId, title: String },
    /// The program rang the bell.
    Bell { session: SessionId },
    /// **The program asked for text to be put on the clipboard** (OSC 52 and its relatives).
    ///
    /// A claim like every other a program makes: the broker reports what was asked, and whether it
    /// reaches a clipboard is the app's decision, made where the person is.
    Clipboard { session: SessionId, text: String },
    /// The program asked for the person to be told something (OSC 9, OSC 777).
    Notification { session: SessionId, title: String, body: String },
    /// How far along the program says it is (OSC 9;4). `percent` is absent where it gave none.
    Progress { session: SessionId, state: ProgressState, percent: Option<u8> },
    /// **The shell is about to run a command**, with the line it read where it said.
    ///
    /// Pushed, as the two below are, because each is a moment: a caller asking `Facts` on a timer learns
    /// that a command ran and not when, and one that asks between two commands misses the first.
    CommandStarted { session: SessionId, command: Option<String> },
    /// What was running ended. `exit_status` is absent when the shell reported none, which is not zero.
    CommandFinished { session: SessionId, exit_status: Option<i32> },
    /// The shell's working directory changed, as the shell reported it.
    CwdChanged { session: SessionId, cwd: String },
    /// A request could not be carried out. `session` is absent when the request named none.
    Failed { session: Option<SessionId>, code: ErrorCode, message: String },
}

/// What a program says about how far along it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum ProgressState {
    /// Nothing to report any more
    None,
    /// Working, perhaps with a percentage
    Running,
    Error,
    /// Working, with no way to say how far along
    Indeterminate,
    Paused,
}

/// **A closed set, generated into the TypeScript as a string union** — elmdb's `ErrorCode` arrangement, and
/// what #372's name-matching should become. A caller switches on a value the compiler knows, rather than
/// matching on a message that is free to be reworded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export, export_to = "protocol.ts")]
pub enum ErrorCode {
    /// The connection's first message was not `Hello`, or its version is not ours.
    Handshake,
    /// A frame did not parse as a `Request`.
    Malformed,
    /// No session has that id — it never existed, or it has already exited.
    NoSuchSession,
    /// The pty or the process refused: spawn, resize or write failed at the OS.
    Pty,
    /// A `Signal` named a pid that is not in that session's process tree, so the broker would be signalling
    /// a process it does not own. Refused by code, because a caller may legitimately race a process ending
    /// and should be able to tell that from having asked for the wrong thing.
    OutsideSession,
    /// This platform has no such operation — signals, on Windows. Distinct from a refusal by the OS: the
    /// request can never succeed here, however it is retried.
    Unsupported,
}
