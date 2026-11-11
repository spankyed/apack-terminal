//! A view of a session: the screen sent as rows to something that draws it, and its input taken back.
//!
//! `wire` is the bytes a view and the broker exchange, `publish` is what each view of a session has been
//! sent and is owed, and `sink` is what is waiting to be written to one view connection. Nothing here
//! knows about a socket.

pub mod publish;
pub mod sink;
pub mod wire;
