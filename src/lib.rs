//! The broker's two layers, as a library so each can be driven without a socket.
//!
//! `main.rs` is the process: a socket, a handshake and the request loop. Everything it serves lives here —
//! the **process layer** (`session`, `output`) and the **terminal capability** over it (`screen`) — because
//! the parser has to be testable as a parser. Phase P of apack's terminal-ownership goal asks for grid
//! snapshots and a byte-boundary property test, and neither is a thing you write through a pty and a socket.

pub mod frames;
pub mod markers;
pub mod output;
pub mod protocol;
pub mod screen;
pub mod security;
pub mod session;
pub mod tree;

/// The `libghostty-vt` core, for Phase 0a's comparison against `screen`. Off by default: the feature
/// exists so the measurement can be built without the default build acquiring a native archive.
#[cfg(feature = "ghostty")]
pub mod vt;
