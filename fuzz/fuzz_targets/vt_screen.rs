//! Arbitrary program output fed into `Screen`, split across chunk boundaries.
#![no_main]

use apack_terminal::screen::Screen;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut screen = Screen::new(40, 12);
    let _ = screen.allow_images(false);
    // Split on a data-chosen chunk size to exercise cross-chunk escape/UTF-8 parsing
    let step = data.first().map_or(1, |&b| (usize::from(b) % 31) + 1);
    for chunk in data.chunks(step) {
        screen.feed(chunk);
    }
    let _ = screen.snapshot(4);
    let _ = screen.take_effects();
    let _ = screen.take_replies();
    let _ = screen.commands();
});
