//! The request frames a client writes to the broker: each line is read as a frame, until the input ends.
#![no_main]

use std::io::Cursor;

use apack_terminal::frames::{Frame, read_frame};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = Cursor::new(data);
    let mut line = String::new();
    // Bounded, so a reader that answers without consuming cannot hold the target
    for _ in 0..4096 {
        if matches!(read_frame(&mut reader, &mut line), Frame::Ended) {
            break;
        }
    }
});
