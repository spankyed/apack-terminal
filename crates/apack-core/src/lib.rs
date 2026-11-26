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
pub mod time;

#[cfg(test)]
mod tests;

/// Hard cap on non-conflatable outbound bytes queued for a single connection (16 MiB).
pub const MAX_CONTROL_QUEUE_BYTES: usize = 16_777_216;

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
