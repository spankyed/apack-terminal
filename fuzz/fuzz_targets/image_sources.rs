//! Arbitrary APC/SOS/PM byte streams fed into the Kitty image-source scanner without touching disk.
#![no_main]

use apack_terminal::vt::image_sources::Scanner;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut scanner = Scanner::default();
    scanner.begin();
    let step = data.first().map_or(1, |&b| (usize::from(b) % 19) + 1);
    let mut offset = 0;
    while offset < data.len() {
        if !scanner.reading() {
            scanner.begin();
        }
        let end = (offset + step).min(data.len());
        let (taken, load) = scanner.read(&data[offset..end], |_| {});
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
