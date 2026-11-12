//! The messages a view writes to the broker over its wire: each is read off the connection, and its body is
//! decoded the way the broker decodes it, until the input ends.
#![no_main]

use std::io::Cursor;

use apack_terminal::view::wire::{Incoming, ToBroker, read_message};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut reader = Cursor::new(data);
    for _ in 0..4096 {
        match read_message(&mut reader) {
            Incoming::Message(body) => {
                let _ = ToBroker::decode(&body);
            }
            Incoming::TooLong | Incoming::Ended => break,
        }
    }
});
