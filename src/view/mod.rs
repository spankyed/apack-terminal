//! A view of a session: the screen sent as rows to something that draws it, and its input taken back.
//!
//! `wire` is the bytes a view and the broker exchange. Nothing here knows about a socket.

pub mod wire;
