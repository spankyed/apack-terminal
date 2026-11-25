//! Arbitrary program output fed into `Screen`, split across chunk boundaries.
#![no_main]

use apack_terminal::screen::Screen;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&first, payload)) = data.split_first() else {
        return;
    };
    let mut screen = Screen::new(40, 12);
    let _ = screen.allow_images(false);
    // Split on a data-chosen chunk size independent of payload prefix bytes
    let step = (usize::from(first) % 31) + 1;
    for chunk in payload.chunks(step) {
        screen.feed(chunk);
    }
    let _ = screen.snapshot(4);
    let _ = screen.take_effects();
    let _ = screen.take_replies();
    let _ = screen.commands();
});
