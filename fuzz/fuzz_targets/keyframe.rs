//! Arbitrary keyframe bytes passed to `Terminal::restore`.
#![no_main]

use apack_terminal::vt::Terminal;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(mut terminal) = Terminal::new(40, 10) else { return };
    let _ = terminal.allow_images(false);
    let _ = terminal.restore(data);
});
