//! The messages the app and the broker exchange, and the one place their shape is defined.
//!
//! **Rust is the source of truth and the TypeScript is generated**, which is `elmdb-node/protocol/`'s
//! arrangement reused rather than chosen again: `cargo test -- --ignored export_protocol` writes
//! `protocol/protocol.ts`, and the app treats that file as a recorded artifact with `check`/`update` halves,
//! so a protocol change that nobody regenerated fails a typecheck instead of becoming a strange bug.
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
    Hello { protocol: u32 },
    /// Start a shell in a pty. `shell` absent means the platform's default.
    Spawn {
        cols: u16,
        rows: u16,
        shell: Option<String>,
        cwd: Option<String>,
    },
    /// Bytes the user typed.
    Write { session: SessionId, data: String },
    /// The viewport changed. One size per session: a pty has one.
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// **The screen as the broker has it**, with up to `history` lines above the viewport.
    ///
    /// This is the read that makes a session's state independent of a window: the broker parses the same
    /// bytes the renderer draws and is the authority on them (D9), so a session nobody is looking at still
    /// answers. `history` is a ceiling and not a promise — a session that has scrolled less answers with
    /// less, and an alternate-screen app has no scrollback to answer with at all.
    Screen { session: SessionId, history: u32 },
    /// End the session and its process tree.
    Kill { session: SessionId },
}

/// What the broker tells the app, unprompted or in answer.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "protocol.ts")]
pub enum Event {
    /// The broker's own version, answering `Hello`.
    Hello { protocol: u32 },
    /// A session exists and its process is running.
    Spawned { session: SessionId, pid: u32 },
    /// Bytes the process wrote. A feed: unprompted, and nobody asked for it.
    Output { session: SessionId, data: String },
    /// The process ended. `code` is absent when it was killed by a signal.
    Exited {
        session: SessionId,
        code: Option<i32>,
    },
    /// **The channel was full and a flush had to wait.** Not a loss: output is held, the pty's buffer fills
    /// behind it, and the process feels it — so this says a producer was throttled, which the goal requires
    /// be observable rather than silent. Sent as an answer, so reporting the overload never queues behind
    /// the output that caused it.
    /// `u32` rather than `u64`: ts-rs generates a `bigint` for 64 bits, and `JSON.parse` hands a client
    /// a number — the type has to describe what arrives. Milliseconds of waiting fit either way.
    Throttled { session: SessionId, waited_ms: u32 },
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
    /// A request could not be carried out. `session` is absent when the request named none.
    Failed {
        session: Option<SessionId>,
        code: ErrorCode,
        message: String,
    },
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
