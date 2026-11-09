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
pub const PROTOCOL_VERSION: u32 = 1;

/// A session's identity, minted by the broker: the app never invents one.
pub type SessionId = String;

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
        cwd: Option<String>,
    },
    /// Bytes the user typed.
    /// `text`, not `data`: what crosses here is decoded text, which is the contract and not an accident.
    /// See `Event::Output`.
    Write { session: SessionId, text: String },
    /// The viewport changed. One size per session: a pty has one.
    Resize { session: SessionId, cols: u16, rows: u16 },
    /// **The screen as the broker has it**, with up to `history` lines above the viewport.
    ///
    /// This is the read that makes a session's state independent of a window: the broker parses the same
    /// bytes the renderer draws and is the authority on them (D9), so a session nobody is looking at still
    /// answers. `history` is a ceiling and not a promise — a session that has scrolled less answers with
    /// less, and an alternate-screen app has no scrollback to answer with at all.
    Screen { session: SessionId, history: u32 },
    /// **What the shell has reported about itself** — the working directory, the command line it last said
    /// it was running, whether one is running, and the status the last one ended with.
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
    /// Into the lines the same request's `Screen` would answer, oldest first.
    pub line: u32,
    /// Where in that line the match starts, in characters.
    pub column: u32,
    /// The whole line, so a caller sees the context without a second read.
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
    /// Bytes the process wrote. A feed: unprompted, and nobody asked for it.
    /// Bytes the process wrote, **as text**.
    ///
    /// **The broker forwards decoded text, not bytes, and that is a decision rather than a compromise.**
    /// Both ends are terminal emulators — `alacritty_terminal` here, the consumer's in its window — and both
    /// replace an invalid sequence with U+FFFD independently, so a byte-exact pipe would show the user
    /// exactly what this does. Nothing in this protocol consumes raw bytes either: `Screen` answers lines,
    /// a search is over text, and a marker is a claim. A sequence split across two reads is held back until
    /// it completes (`output::sendable`), so the only thing replaced is what was never valid.
    ///
    /// **Measured 2026-11-08, which is why this is not base64 or a binary frame.** JSON escapes a control
    /// byte as `\u001b`, six bytes for one, so its cost is `1 + 5 x` the control-byte fraction; base64 is a
    /// flat 1.33x. The crossover is 6.7% control bytes, and real terminal output sits below it: ordinary
    /// build output 1.1% (1.01x), `ls --color` 6.5% (1.22x), a full-screen TUI redraw 2.4% (1.12x). Base64
    /// would make every one of those *larger*, and cost the frames their readability.
    Output { session: SessionId, text: String },
    /// The process ended. `code` is absent when it was killed by a signal.
    Exited { session: SessionId, code: Option<i32> },
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
        /// Within the viewport, so `0` is its first line however much history came with it.
        cursor_row: u16,
        cursor_column: u16,
        alt_screen: bool,
    },
    /// What the shell reported about itself and what is running in the pty, answering `Facts`.
    ///
    /// **`cwd`, `command`, `running` and `exit_status` are claims; `foreground` is a measurement**, and the
    /// two are not interchangeable — a shell with no integration installed claims nothing at all, and a
    /// program in the pty may claim whatever it likes. They travel in one event because a caller asking
    /// what a session is doing wants both and should not pay two round trips to find out.
    Facts {
        session: SessionId,
        cwd: Option<String>,
        command: Option<String>,
        running: bool,
        exit_status: Option<i32>,
        foreground: Option<Foreground>,
    },
    /// Where a search found its text, answering `Search`.
    Search { session: SessionId, matches: Vec<SearchMatch> },
    /// How a `Wait` ended. Sent once per wait, whichever way it ended.
    Waited { session: SessionId, outcome: WaitOutcome },
    /// A request could not be carried out. `session` is absent when the request named none.
    Failed { session: Option<SessionId>, code: ErrorCode, message: String },
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
}
