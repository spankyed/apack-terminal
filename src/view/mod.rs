//! A view of a session: the screen sent as rows to something that draws it, and its input taken back.
//!
//! `wire` is the bytes a view and the broker exchange, and `publish` is what each view of a session has been
//! sent and is owed. Nothing here knows about a socket.

pub mod publish;
pub mod wire;
