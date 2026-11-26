//! Sans-IO binary wire protocol and correct-by-construction domain types for `apack-terminal` v2.
//!
//! Zero `unsafe` code, zero OS or wall-clock dependencies. Every message in the unified control and
//! display protocol is framed identically from byte 0:
//!
//! ```text
//! [body_len: u32 LE][stream_id: u32 LE][msg_type: u16 LE][req_id: u32 LE][payload: body_len - 10]
//! ```
//!
//! A leading 4-byte zero (`body_len == 0`, `[0, 0, 0, 0]`) is the universal clean end-of-input
//! (`FIN`) sentinel across Unix sockets, Windows named pipes, and SSH `proxy` streams. Every
//! non-FIN frame has `body_len == 10 + payload_len` (`10..=10 + MAX_FRAME_PAYLOAD`), so empty-payload
//! messages never collide with `FIN`.

#![forbid(unsafe_code)]

pub mod codec;
pub mod frame;
pub mod messages;
pub mod types;

#[cfg(test)]
mod tests;

/// Current v2 wire protocol version negotiated during `Hello`.
pub const PROTOCOL_VERSION: u16 = 2;

/// Fixed binary frame header length in bytes: `len(4) + stream_id(4) + msg_type(2) + req_id(4)`.
pub const FRAME_HEADER_LEN: usize = 14;

/// Maximum payload size of a single binary frame (8 MiB).
pub const MAX_FRAME_PAYLOAD: usize = 8_388_608;

/// Chunk size used to fair-queue bulk payloads (`ImageBlob`, `FetchRows`) across multiplexed streams (64 KiB).
pub const BULK_CHUNK_BYTES: usize = 65_536;

/// Universal 4-byte end-of-input sentinel frame (`payload_len == 0`).
pub const FIN_FRAME: [u8; 4] = [0, 0, 0, 0];

/// Maximum rows returned in a single `FetchRows` response.
pub const MAX_ROWS_PER_FETCH: u32 = 2_000;

/// Maximum commands retained in a session's bounded command ring.
pub const MAX_COMMANDS: usize = 256;

/// Maximum OSC 7501 program-status records retained per session.
pub const MAX_STATUSES: usize = 256;

/// Maximum search matches held and reported per search watch.
pub const MAX_SEARCH_MATCHES: usize = 2_000;

/// Hard upper bound on opt-in lease reconnect grace (5 minutes).
pub const MAX_LEASE_GRACE_MS: u32 = 300_000;

/// Default opt-in reconnect grace for remote SSH leases (30 seconds).
pub const DEFAULT_SSH_GRACE_MS: u32 = 30_000;

/// Maximum length in bytes of any single decoded string field (1 MiB).
pub const MAX_STRING_BYTES: usize = 1_048_576;

/// Maximum number of elements in any single decoded wire sequence.
pub const MAX_VEC_ITEMS: usize = 65_536;

/// Maximum length in bytes of a single decoded byte-blob field (8 MiB).
pub const MAX_BLOB_BYTES: usize = MAX_FRAME_PAYLOAD;
