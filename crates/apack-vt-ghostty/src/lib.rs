//! `libghostty-vt` backing engine for `apack-terminal` v2.
//!
//! Wraps unmodified `ghostty-vt-rs-sys` (`0.3.0`) behind [`apack_core::vt::VtEngine`], emitting
//! style-run [`apack_proto::types::PackedRow`]s in a single pass, deduplicating Kitty graphics
//! into a global content-addressed [`ImageStore`] (`256 MiB` cap), enforcing zero file/SHM reads
//! inside the VT core, and exposing bounded binary snapshots for v2 keyframes.

pub mod engine;
pub mod error;
pub mod image_store;
pub mod input;
pub mod placeholders;
pub mod rows;

#[cfg(test)]
mod tests;

pub use engine::{GhosttyConfig, GhosttyEngine, NamedImageProbe, VtFeatures, features};
pub use error::GhosttyError;
pub use image_store::{BlobChunk, ImageStore, decode_png_rgba, encode_png_rgba};

/// Maximum cached/reported Kitty image placements per session (aligned with `MAX_IMAGES_REPLY`).
pub const MAX_IMAGE_PLACEMENTS: usize = apack_proto::MAX_IMAGES_REPLY;

/// Default broker-wide deduplicated image store cap (`256 MiB`).
pub const GLOBAL_IMAGE_STORE_BYTES: usize = 256 << 20;

/// Per-session `libghostty-vt` Kitty graphics storage limit (`64 MiB`).
pub const SESSION_IMAGE_STORAGE_BYTES: u64 = 64 << 20;

/// Per-session `libghostty-vt` scrollback memory cap (`256 MiB`, matching v1 `HISTORY_BYTES`).
pub const SCROLLBACK_MAX_BYTES: usize = 256 << 20;

/// Global scrollback memory pressure threshold (`512 MiB`) triggering eager compression across sessions.
pub const GLOBAL_SCROLLBACK_PRESSURE_BYTES: u64 = 512 << 20;

/// Maximum bytes of one unknown OSC sequence retained by `libghostty-vt` (matches v1 `PAYLOAD_CAP`).
pub const UNKNOWN_OSC_MAX_BYTES: usize = 4 * 1024;
