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

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

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

/// Universal 4-byte end-of-input sentinel frame (`body_len == 0`).
pub const FIN_FRAME: [u8; 4] = [0, 0, 0, 0];

/// Maximum terminal grid columns accepted by the protocol.
pub const MAX_GRID_COLS: u16 = 1_000;

/// Maximum terminal grid rows accepted by the protocol.
pub const MAX_GRID_ROWS: u16 = 1_000;

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

/// Maximum length in bytes of a search query or `WaitCondition::Text` needle (4 KiB).
pub const MAX_SEARCH_NEEDLE_BYTES: usize = 4_096;

/// Maximum number of elements in any single decoded wire sequence.
pub const MAX_VEC_ITEMS: usize = 65_536;

/// Maximum length in bytes of a single decoded byte-blob field (8 MiB).
pub const MAX_BLOB_BYTES: usize = MAX_FRAME_PAYLOAD;

/// Maximum progress percentage reported in `Progress` and `StatusRecord`.
pub const MAX_PROGRESS_PERCENT: u8 = 100;

/// Maximum `argv` entries in a `SpawnSpec`.
pub const MAX_SPAWN_ARGV: usize = 1_024;

/// Maximum `env` pairs in a `SpawnSpec`.
pub const MAX_SPAWN_ENV: usize = 1_024;

/// Maximum session summaries returned in `SessionsList`.
pub const MAX_SESSIONS_LIST: usize = 4_096;

/// Maximum image placements returned in `ImagesReply`.
pub const MAX_IMAGES_REPLY: usize = 4_096;

/// Maximum dynamic palette overrides in a `PaletteOverrides` table.
pub const MAX_PALETTE_OVERRIDES: usize = 256;

/// Maximum length in bytes of a single `PackedRow.text` field (`u16::MAX`, guaranteeing UTF-16 run
/// offsets fit in `u16` across Rust and TypeScript codecs).
pub const MAX_PACKED_ROW_TEXT_BYTES: usize = u16::MAX as usize;

/// Maximum style runs in a single `PackedRow`.
pub const MAX_PACKED_RUNS: usize = 4_096;

/// Maximum style entries in a single `PackedRow`.
pub const MAX_PACKED_STYLES: usize = 4_096;

/// Maximum hyperlink entries in a single `PackedRow`.
pub const MAX_PACKED_LINKS: usize = 1_024;

/// Maximum Kitty placeholder image pieces in a single `PackedRow`.
pub const MAX_PACKED_PIECES: usize = 1_024;

/// Maximum status states listed in `WaitCondition::Status`.
pub const MAX_WAIT_STATUSES: usize = 8;

/// Length in bytes of nonces and mutual-auth HMAC proofs.
pub const AUTH_TOKEN_LEN: usize = 32;

/// Maximum length in bytes of `ClientMsg::Hello::client_name`.
pub const MAX_CLIENT_NAME_BYTES: usize = 256;

/// Maximum frame payload permitted before `Hello` completes (4 KiB).
pub const MAX_HELLO_FRAME_PAYLOAD: usize = 4_096;

/// Maximum frame payload permitted while awaiting `AuthProof` (64 bytes).
pub const MAX_AUTH_FRAME_PAYLOAD: usize = 64;
