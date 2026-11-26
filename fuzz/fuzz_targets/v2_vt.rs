#![no_main]

use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};

use apack_core::status::parse_osc7501;
use apack_core::vt::VtEngine;
use apack_proto::types::{CellPx, GridSize};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
use libfuzzer_sys::fuzz_target;

// Fuzzes the `libghostty-vt` FFI wrapper and OSC 7501 parser against hostile byte streams,
// checking totality, bounded row extraction, and metadata consistency.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_osc7501(text);
    }

    let store = Arc::new(Mutex::new(ImageStore::new(1 << 20)));
    let mut vt = GhosttyEngine::new(GhosttyConfig {
        size: GridSize { cols: NonZeroU16::new(80).unwrap(), rows: NonZeroU16::new(24).unwrap() },
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_lines: 256,
        allow_images: true,
        terminfo_name: String::from("xterm-256color"),
        image_store: Some(store),
    })
    .expect("fixed-size GhosttyEngine must construct");

    let mut events = Vec::new();
    vt.feed(data, &mut events);
    let _ = vt.meta();
    let _ = vt.palette_overrides();
    let _ = vt.images();
    let dirty = vt.take_dirty_viewport();
    let mut rows = Vec::new();
    vt.extract_viewport_rows(&dirty.dirty_rows, &mut rows);
});
