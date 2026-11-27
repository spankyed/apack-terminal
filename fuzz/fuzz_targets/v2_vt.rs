#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    apack_terminal_fuzz::harnesses::fuzz_v2_vt(data);
});
