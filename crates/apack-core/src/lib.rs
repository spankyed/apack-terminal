//! Sans-IO deterministic broker core for `apack-terminal` v2.
//!
//! Enforces compile-time sans-IO purity via `#![no_std]` + `alloc` and `#![forbid(unsafe_code)]`:
//! no wall clocks, sockets, pipes, files, threads, or environment reads can exist in this crate.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod auth;
pub mod broker;
pub mod flow;
pub mod lease;
pub mod marks;
pub mod session;
pub mod status;
pub mod time;
pub mod vt;

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;

/// Hard cap on non-conflatable outbound bytes queued for a single connection (16 MiB).
pub const MAX_CONTROL_QUEUE_BYTES: usize = 16_777_216;

/// Hard cap on queued outbound bytes waiting to be written to a session's PTY stdin (16 MiB).
pub const MAX_PTY_WRITE_BYTES: usize = 16_777_216;

/// Hard upper bound on a DEC 2026 synchronized-output hold (1 second).
pub const SYNC_HOLD_MAX_MS: u64 = 1_000;

/// Coalescing window for incremental server-side `SearchWatch` delta pushes (150 ms).
pub const SEARCH_COALESCE_MS: u64 = 150;

/// Maximum concurrent `Wait` requests registered on a single session.
pub const MAX_WAITERS_PER_SESSION: usize = 256;

/// Maximum UTF-8 byte length for control/event metadata strings (title, CWD, command, notification).
pub const MAX_EVENT_STRING_BYTES: usize = 4_096;

/// Maximum image placements returned in a single `ImagesReply` (matches `apack-proto` decode cap).
pub const MAX_IMAGE_PLACEMENTS: usize = 4_096;

/// Truncates `s` to at most `max_bytes` bytes on a valid UTF-8 character boundary.
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> alloc::string::String {
    if s.len() <= max_bytes {
        return alloc::string::String::from(s);
    }
    let end = s
        .char_indices()
        .map(|(idx, ch)| idx + ch.len_utf8())
        .take_while(|&end_idx| end_idx <= max_bytes)
        .last()
        .unwrap_or(0);
    alloc::string::String::from(&s[..end])
}

/// Hard cap on per-stream `SUB_RAW_OUTPUT` byte credit (4 MiB).
pub const MAX_RAW_OUTPUT_CREDIT: u32 = 4_194_304;

/// Default BDP in-flight frame byte window (256 KiB).
pub const DEFAULT_BDP_FRAME_BYTES: u32 = 262_144;

/// Lower clamp on adaptive BDP in-flight frame byte window (32 KiB).
pub const MIN_BDP_FRAME_BYTES: u32 = 32_768;

/// Upper clamp on adaptive BDP in-flight frame byte window (1 MiB).
pub const MAX_BDP_FRAME_BYTES: u32 = 1_048_576;

/// Handshake and mutual-authentication deadline for newly opened connections (10 seconds).
pub const AUTH_HANDSHAKE_TIMEOUT_MS: u64 = 10_000;

/// Maximum concurrent multiplexed streams attached on a single connection.
pub const MAX_STREAMS_PER_CONN: usize = 1_024;

/// Maximum concurrent in-grace disconnected broker leases retained at once.
pub const MAX_IN_GRACE_LEASES: usize = 64;

/// Maximum concurrent per-session leases held by a single connection.
pub const MAX_SESSION_LEASES_PER_CONN: usize = 64;

/// Default word-selection delimiter characters advertised in `HelloOk`.
pub const DEFAULT_WORD_DELIMITERS: &str = " \t\n\r`'\"()[]{}<>|;:,";

/// Broker capability bitmask advertised in `HelloOk`.
pub const BROKER_CAPABILITIES: u32 = 0x0000_0001;
