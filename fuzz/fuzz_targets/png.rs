//! An image a program sent inline, as the terminal decodes it.
#![no_main]

use apack_terminal::vt::images::rgba_of_png;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rgba_of_png(data);
});
