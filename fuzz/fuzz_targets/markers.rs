//! Shell output as the marker parser reads it, with and without a key the session vouched with.
#![no_main]

use apack_terminal::markers::{MarksKey, read};
use apack_terminal::screen::Screen;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let key = MarksKey::new("00112233445566778899aabbccddeeff");
    let _ = read(data, true, None);
    let _ = read(data, false, key.as_ref());
    let _ = read(data, true, key.as_ref());
    // Also exercise OSC 7 (`cwd_from_url`) through the terminal's OSC 7 callback on arbitrary URL payloads
    let mut osc7 = Vec::with_capacity(5 + data.len());
    osc7.extend_from_slice(b"\x1b]7;");
    osc7.extend_from_slice(data);
    osc7.push(0x07);
    let mut screen = Screen::new(40, 6);
    screen.feed(&osc7);
    let _ = screen.take_effects();
});
