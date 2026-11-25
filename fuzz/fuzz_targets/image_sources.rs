//! Arbitrary APC/SOS/PM byte streams fed into the Kitty image-source scanner without touching disk.
#![no_main]

use apack_terminal::vt::image_sources::Scanner;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&first, payload)) = data.split_first() else {
        return;
    };
    let mut scanner = Scanner::default();
    scanner.begin();
    let step = (usize::from(first) % 19) + 1;
    let mut offset = 0;
    while offset < payload.len() {
        if !scanner.reading() {
            scanner.begin();
        }
        let end = (offset + step).min(payload.len());
        let (taken, load) = scanner.read(&payload[offset..end], |_| {});
        if let Some(load) = load {
            let _ = load.askable();
            let _ = load.read_unasked();
            let _ = load.asked();
            let _ = load.refusal();
            let _ = load.refuse();
        }
        offset += taken.max(1);
    }
});
