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

/// The `libghostty-vt` core `screen` is built on: the terminal handle, the collector its callbacks fill,
/// and the safe layer over the C ABI that this repository owns rather than takes from a wrapper.
pub mod vt;
