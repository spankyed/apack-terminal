//! Sharded v2 terminal broker runtime, dumb single-channel `proxy`, `inspect` diagnostics, and
//! synchronous v2 client for `apack-terminal`.
//!
//! Zero `unsafe` code: all OS FFI is encapsulated inside `apack-os` and `apack-vt-ghostty`, while
//! all protocol, flow-control, lease, marker, and view-diff logic lives in sans-IO `apack-core`.

#![forbid(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

pub mod client;
pub mod config;
pub mod inspect;
pub mod proxy;
pub mod runtime;
pub mod shard;
#[cfg(windows)]
mod shard_windows;
pub mod transport_loop;
#[cfg(windows)]
mod transport_windows;

#[cfg(test)]
mod tests;

pub use client::{BrokerClient, ClientError, ReceivedFrame};
pub use config::{BrokerConfig, CliCommand, parse_cli};
pub use runtime::{BrokerHandle, BrokerServer};

/// Returns the current wall-clock timestamp in milliseconds since Unix epoch.
pub fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Reads from `reader` into `buf`, retrying on `EINTR` (`io::ErrorKind::Interrupted`).
pub(crate) fn read_retry_eintr(reader: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Fills `buf` with cryptographically secure random bytes from the OS CSPRNG.
pub fn fill_random(buf: &mut [u8]) -> Result<(), String> {
    getrandom::fill(buf).map_err(|err| format!("OS CSPRNG failure: {err}"))
}

/// Computes the 32-byte ABI tag identifying a `libghostty-vt` build and feature set.
pub(crate) fn abi_hash_for_features(f: apack_vt_ghostty::VtFeatures) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"apack-vt-ghostty-v2:");
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update([
        u8::from(f.render_state),
        u8::from(f.formatter),
        u8::from(f.selection),
        u8::from(f.search),
        u8::from(f.input_encode),
        u8::from(f.grid_introspection),
        u8::from(f.snapshot),
        u8::from(f.kitty_graphics),
    ]);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Computes the 32-byte ABI tag identifying the linked `libghostty-vt` build and feature set.
///
/// Stored in [`apack_wal::EngineCheckpoint::engine_abi_hash`] so `Spawn { restore }` performs
/// lossless Ghostty restoration on matching builds and degrades cleanly to `VisualKeyframe` when
/// `libghostty-vt` is upgraded.
pub fn engine_abi_hash() -> [u8; 32] {
    abi_hash_for_features(apack_vt_ghostty::features())
}
