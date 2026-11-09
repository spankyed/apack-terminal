//! Reading one frame off a connection, before anything knows what it means.
//!
//! **This is the only part of the protocol a stranger reaches.** Everything else in `main.rs` runs on a
//! `Request` that already parsed, so the bytes an unknown sender controls are handled here and nowhere
//! else — and a process that owns the app's terminals must survive every one of them. That is what makes
//! this a library module rather than a loop in the binary: arbitrary bytes can be put through it without a
//! socket, a pty or a process, which is what `tests/frames.rs` does.
//!
//! It decides nothing about what a frame means. `Hello`-first, the session a request names and what the
//! broker does about it all stay in `main.rs`: this answers only "is there a frame here, and is it the
//! protocol".

use std::io::{BufRead, Read};

use crate::protocol::Request;

/// The largest frame a client may send.
///
/// **A ceiling on what is read before it is parsed**, not a judgement about any request's size. The biggest
/// frame the app sends is a `write` carrying a paste, and a megabyte is far past any of them; what this is
/// here for is the sender that never sends a newline at all, which is unbounded by construction.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// What one read of a connection produced.
#[derive(Debug)]
pub enum Frame {
    /// A request to carry out.
    Request(Request),
    /// A blank line, which the protocol lets pass: read the next one.
    Blank,
    /// Not the protocol. Answer with this and read the next one — a sender that sent one bad frame may
    /// still send a good one, and a client holding live sessions should not lose them to a typo.
    Malformed(String),
    /// Past the cap. Answer with this and stop: the bytes beyond the cap are unread, so where the next
    /// frame starts is no longer known, and carrying on would parse the tail of this one as a frame.
    TooLong(String),
    /// Nothing more will arrive. The connection closed, or the sender wrote bytes that are not UTF-8 —
    /// neither leaves anything to answer, since the sender is not speaking the protocol and a refusal
    /// would be read by nobody.
    Ended,
}

/// Read the next frame, reusing `line`'s allocation across calls.
///
/// Taking a byte more than the cap is what tells a frame at the limit from one past it.
pub fn read_frame<R: BufRead>(reader: &mut R, line: &mut String) -> Frame {
    line.clear();
    match (&mut *reader).take(MAX_FRAME_BYTES as u64 + 1).read_line(line) {
        Ok(0) => return Frame::Ended,
        Ok(_) => {}
        Err(_) => return Frame::Ended,
    }
    if line.len() > MAX_FRAME_BYTES {
        return Frame::TooLong(format!("a frame may be at most {MAX_FRAME_BYTES} bytes"));
    }
    if line.trim().is_empty() {
        return Frame::Blank;
    }
    match serde_json::from_str::<Request>(line) {
        Ok(request) => Frame::Request(request),
        Err(error) => Frame::Malformed(format!("{error}")),
    }
}
