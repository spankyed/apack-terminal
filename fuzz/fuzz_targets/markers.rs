//! Shell output as the marker parser reads it, with and without a key the session vouched with.
#![no_main]

use apack_terminal::markers::{MarksKey, read};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let key = MarksKey::new("00112233445566778899aabbccddeeff");
    let _ = read(data, true, None);
    let _ = read(data, false, key.as_ref());
});
