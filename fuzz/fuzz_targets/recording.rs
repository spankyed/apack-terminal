//! A recording file's bytes, as the reader decodes them after a process that wrote it may have died.
#![no_main]

use apack_terminal::recording::{decode, refusal};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = refusal(data);
    let _ = decode(data);
});
