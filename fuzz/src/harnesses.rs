//! Pure-Rust fuzz harnesses shared between `fuzz/fuzz_targets/*.rs` (libFuzzer) and
//! `tests/fuzz_corpus.rs` (cross-platform deterministic corpus replay on stable Rust).

use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};

use apack_core::status::parse_osc7501;
use apack_core::vt::VtEngine;
use apack_os::image_sources::{Base64, Decoded, Load, PATH_BYTES, Scanner, Source, encoded, rules, shared_memory_name};
use apack_proto::frame::{DecodedFrame, FrameDecoder};
use apack_proto::messages::{BrokerMsg, ClientMsg};
use apack_proto::types::{CellPx, GridSize};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
use apack_wal::format::{RecordingMeta, SegmentFooter, decode_one_record, encode_framed_record, scan_segment};

/// Fuzzes the v2 binary frame decoder across arbitrary chunk splits and directly exercises
/// both client and broker message decoders on raw payloads, verifying totality and round-trips.
pub fn fuzz_v2_wire(data: &[u8]) {
    let whole = decode_all(data, data.len().max(1));
    let by_one = decode_all(data, 1);
    assert_eq!(whole, by_one);

    if let Ok(frames) = whole {
        for frame in frames {
            if let DecodedFrame::Frame { header, payload } = frame {
                check_client(header.msg_type, &payload);
                check_broker(header.msg_type, &payload);
            }
        }
    }

    // Directly fuzz message decoders without needing a synchronized 4-byte frame length header.
    if data.len() >= 2 {
        let tag = u16::from_le_bytes([data[0], data[1]]);
        let body = &data[2..];
        check_client(tag, body);
        check_broker(tag, body);
    }
}

fn check_client(tag: u16, payload: &[u8]) {
    if let Ok(msg) = ClientMsg::decode(tag, payload) {
        let mut w = apack_proto::codec::Writer::new();
        msg.encode(&mut w);
        let reparsed = ClientMsg::decode(msg.msg_type(), w.as_slice()).expect("encoded ClientMsg must round-trip");
        assert_eq!(reparsed, msg);
    }
}

fn check_broker(tag: u16, payload: &[u8]) {
    if let Ok(msg) = BrokerMsg::decode(tag, payload) {
        let mut w = apack_proto::codec::Writer::new();
        msg.encode(&mut w);
        let reparsed = BrokerMsg::decode(msg.msg_type(), w.as_slice()).expect("encoded BrokerMsg must round-trip");
        assert_eq!(reparsed, msg);
    }
}

fn decode_all(data: &[u8], chunk_size: usize) -> Result<Vec<DecodedFrame>, apack_proto::codec::WireError> {
    let mut dec = FrameDecoder::new();
    let mut out = Vec::new();
    for chunk in data.chunks(chunk_size) {
        let mut rest = chunk;
        while !rest.is_empty() {
            let (used, frame) = dec.step(rest)?;
            assert!(used > 0);
            rest = &rest[used..];
            if let Some(f) = frame {
                out.push(f);
            }
        }
    }
    Ok(out)
}

/// Fuzzes every v2 WAL on-disk decoder (`meta.apkm`, `SegmentFooter`, individual framed records,
/// and full segment scans with torn tails and zlib keyframe decompression caps).
pub fn fuzz_v2_wal(data: &[u8]) {
    if let Ok(meta) = RecordingMeta::decode(data) {
        let encoded = meta.encode();
        assert_eq!(RecordingMeta::decode(&encoded).expect("meta round-trip"), meta);
    }

    if let Ok(footer) = SegmentFooter::decode(data) {
        let encoded = footer.encode();
        assert_eq!(SegmentFooter::decode(&encoded).expect("footer round-trip"), footer);
    }

    if let Ok((timed, consumed)) = decode_one_record(data) {
        assert!(consumed <= data.len());
        let mut reencoded = Vec::new();
        if encode_framed_record(timed.at_ms, &timed.record, &mut reencoded).is_ok() {
            let (reparsed, re_consumed) = decode_one_record(&reencoded).expect("encoded WAL record must round-trip");
            assert_eq!(re_consumed, reencoded.len());
            assert_eq!(reparsed, timed);
        }
    }

    let scan = scan_segment(data);
    assert!(scan.valid_records_end <= data.len());
}

fn new_fuzz_vt(cols: u16, rows: u16, scrollback_lines: usize, image_bytes: usize) -> GhosttyEngine {
    let store = Arc::new(Mutex::new(ImageStore::new(image_bytes)));
    GhosttyEngine::new(GhosttyConfig {
        size: GridSize { cols: NonZeroU16::new(cols).unwrap(), rows: NonZeroU16::new(rows).unwrap() },
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_lines,
        allow_images: true,
        terminfo_name: String::from("xterm-256color"),
        image_store: Some(store),
    })
    .expect("fixed-size GhosttyEngine must construct")
}

/// Fuzzes the `libghostty-vt` FFI wrapper and OSC 7501 parser against hostile byte streams,
/// checking totality, mid-sequence chunk splits, bounded row extraction, and metadata consistency.
pub fn fuzz_v2_vt(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_osc7501(text);
    }

    let mut vt = new_fuzz_vt(80, 24, 256, 1 << 20);

    let mut events = Vec::new();
    // Split once at a data-derived offset so multi-byte CSI/OSC/DCS/UTF-8 sequences cross chunks.
    if data.len() >= 2 {
        let split = usize::from(data[0]) % data.len();
        vt.feed(&data[..split], &mut events);
        vt.feed(&data[split..], &mut events);
    } else {
        vt.feed(data, &mut events);
    }

    let meta = vt.meta();
    let _ = vt.palette_overrides();
    let _ = vt.images();
    let dirty = vt.take_dirty_viewport();
    let mut rows = Vec::new();
    vt.extract_viewport_rows(&dirty.dirty_rows, &mut rows);
    if meta.scrollback_lines > 0 {
        let mut sb_rows = Vec::new();
        let count = meta.scrollback_lines.min(8) as u32;
        vt.extract_history_rows(0, count, &mut sb_rows);
        assert!(sb_rows.len() <= count as usize);
    }
}

/// Fuzzes the Kitty graphics APC scanner (`Scanner`, `Base64`, `rules`) and its integration with
/// `GhosttyEngine::feed_until_apc` without performing any host filesystem or shared-memory I/O.
pub fn fuzz_v2_kitty_apc(data: &[u8]) {
    // 1. Scanner chunk-split invariance: whole-slice scanning must match 1-byte streaming exactly.
    let whole = scan_once(data, data.len().max(1));
    let by_one = scan_once(data, 1);
    assert_eq!(whole, by_one);

    if let Some(load) = whole.1 {
        inspect_load_without_io(load);
    }

    // 2. Direct path-rule and base64 totality + round-trip invariants.
    let _ = rules::refused(data);
    let _ = rules::refused_once_opened(data);
    let _ = shared_memory_name(data);

    let mut b64 = Base64::default();
    for &b in data {
        b64.push(b);
    }
    let _ = b64.finish();

    if data.len() <= PATH_BYTES + 16 {
        let enc = encoded(data);
        let mut dec = Base64::default();
        for &b in &enc {
            dec.push(b);
        }
        match dec.finish() {
            Decoded::Name(out) => {
                assert!(data.len() <= PATH_BYTES);
                assert_eq!(out, data);
            }
            Decoded::TooLong => assert!(data.len() > PATH_BYTES),
            Decoded::Refused => panic!("encoded() output must always decode cleanly"),
        }
    }

    // 3. End-to-end `GhosttyEngine::feed_until_apc` + `Scanner` shard loop (zero host I/O).
    let mut vt = new_fuzz_vt(40, 12, 16, 256 * 1024);

    let mut scanner = Scanner::default();
    let mut events = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        if !scanner.reading() {
            let (used, entered_apc) = vt.feed_until_apc(rest, &mut events);
            assert!(used > 0 || entered_apc, "feed_until_apc must make forward progress");
            rest = &rest[used..];
            if entered_apc {
                scanner.begin();
            }
        } else {
            let mut passthrough = Vec::new();
            let (used, maybe_load) = scanner.read(rest, |b| passthrough.extend_from_slice(b));
            assert!(used > 0 || !scanner.reading(), "scanner.read must make forward progress");
            rest = &rest[used..];
            if !passthrough.is_empty() {
                vt.feed(&passthrough, &mut events);
            }
            if let Some(load) = maybe_load {
                let refusal = apack_vt_ghostty::NamedImageProbe::parse(load.refusal())
                    .expect("Load::refusal must always be a valid NamedImageProbe");
                inspect_load_without_io(load);
                vt.feed_named_probe(&refusal, &mut events);
            }
        }
    }
}

fn scan_once(data: &[u8], chunk_size: usize) -> (Vec<u8>, Option<Load>, usize) {
    let mut scanner = Scanner::default();
    scanner.begin();
    let mut passthrough = Vec::new();
    let mut consumed = 0usize;
    for chunk in data.chunks(chunk_size.max(1)) {
        let (used, load) = scanner.read(chunk, |b| passthrough.extend_from_slice(b));
        consumed += used;
        if load.is_some() || !scanner.reading() {
            return (passthrough, load, consumed);
        }
        assert_eq!(used, chunk.len());
    }
    (passthrough, None, consumed)
}

fn inspect_load_without_io(load: Load) {
    let _ = load.quiet();
    let _ = load.askable();
    let _ = load.read_unasked();
    let _ = load.refusal();
    let _ = load.asked();
    let _ = load.within_file_limit(4096);
    let _ = load.range(4096);
    let _ = load.range(0);
    match load.source() {
        Source::File(path) => {
            let _ = rules::refused(path);
            let _ = rules::refused_once_opened(path);
        }
        Source::SharedMemory(name) => {
            assert!(shared_memory_name(name));
        }
        Source::Unreadable => {}
    }
    assert!(load.refuse().body().is_err());
}
