#![forbid(unsafe_code)]

//! Cross-platform deterministic replay of every committed fuzz seed corpus (`fuzz/seeds/<target>/`).
//!
//! Shares the exact pure-Rust harness implementations in `fuzz/src/harnesses.rs` with libFuzzer,
//! verifying on Linux, macOS, Windows, and AddressSanitizer that every target declared in
//! `fuzz/Cargo.toml` has a compact, non-empty seed corpus and replays cleanly with zero panics.

#[path = "../fuzz/src/harnesses.rs"]
mod harnesses;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_SEED_BYTES: u64 = 64 * 1024;

type HarnessFn = fn(&[u8]);

fn harness_table() -> BTreeMap<&'static str, HarnessFn> {
    BTreeMap::from([
        ("v2_kitty_apc", harnesses::fuzz_v2_kitty_apc as HarnessFn),
        ("v2_vt", harnesses::fuzz_v2_vt as HarnessFn),
        ("v2_wal", harnesses::fuzz_v2_wal as HarnessFn),
        ("v2_wire", harnesses::fuzz_v2_wire as HarnessFn),
    ])
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn discover_fuzz_targets(manifest: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut in_bin = false;
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_bin = line == "[[bin]]";
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name").and_then(|s| s.trim_start().strip_prefix('=')) {
            let value = rest.trim();
            if let Some(unquoted) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                && !unquoted.is_empty()
            {
                targets.push(unquoted.to_owned());
            }
        }
    }
    targets
}

#[test]
fn replay_all_committed_fuzz_seed_corpora() {
    let fuzz_dir = repo_root().join("fuzz");
    let manifest = fs::read_to_string(fuzz_dir.join("Cargo.toml")).expect("read fuzz/Cargo.toml");
    let declared = discover_fuzz_targets(&manifest);
    assert!(!declared.is_empty(), "fuzz/Cargo.toml must declare at least one [[bin]] target");

    let table = harness_table();
    let mut sorted_declared: Vec<&str> = declared.iter().map(String::as_str).collect();
    sorted_declared.sort_unstable();
    let table_names: Vec<&str> = table.keys().copied().collect();
    assert_eq!(
        sorted_declared, table_names,
        "tests/fuzz_corpus.rs harness table must match fuzz/Cargo.toml [[bin]] targets 1:1"
    );

    for target in &declared {
        let seed_dir = fuzz_dir.join("seeds").join(target);
        assert!(seed_dir.is_dir(), "missing seed corpus directory {}", seed_dir.display());

        let mut seeds: Vec<PathBuf> = fs::read_dir(&seed_dir)
            .expect("read seed dir")
            .map(|entry| entry.expect("seed entry").path())
            .filter(|path| path.is_file())
            .collect();
        seeds.sort();
        assert!(!seeds.is_empty(), "seed corpus directory {} must not be empty", seed_dir.display());

        let runner = table[target.as_str()];
        for seed in &seeds {
            let len = fs::metadata(seed).expect("stat seed").len();
            assert!(
                len <= MAX_SEED_BYTES,
                "seed {} is {len} bytes (exceeds {MAX_SEED_BYTES} byte compact cap)",
                seed.display()
            );
            let bytes = fs::read(seed).expect("read seed");
            runner(&bytes);
        }
    }
}

#[test]
fn committed_seeds_exercise_deep_parser_branches() {
    use std::num::NonZeroU16;
    use std::sync::{Arc, Mutex};

    use apack_core::status::parse_osc7501;
    use apack_core::vt::{ProgramStatusUpdate, VtEngine, VtEvent};
    use apack_proto::types::{CellPx, GridSize};
    use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
    use apack_wal::format::scan_segment;

    let seeds_root = repo_root().join("fuzz").join("seeds");

    // 1. `osc7501_raw_payload` must actually parse a structured status update.
    let raw_osc7501 = fs::read_to_string(seeds_root.join("v2_vt").join("osc7501_raw_payload")).expect("read seed");
    assert!(
        matches!(parse_osc7501(&raw_osc7501), Some(ProgramStatusUpdate::Set { .. })),
        "osc7501_raw_payload seed must parse into ProgramStatusUpdate::Set"
    );

    // 2. `torn_tail_segment` must decode a valid keyframe + output prefix before hitting the torn tail.
    let torn_bytes = fs::read(seeds_root.join("v2_wal").join("torn_tail_segment")).expect("read torn seed");
    let scan = scan_segment(&torn_bytes);
    assert_eq!(scan.records.len(), 2, "torn_tail_segment must decode keyframe + output prefix");
    assert!(
        scan.valid_records_end > 0 && scan.valid_records_end < torn_bytes.len(),
        "torn_tail_segment must truncate strictly inside trailing partial record"
    );

    // 3. `v2_vt` seeds must actually exercise OSC 7501 ProgramStatus, OSC 633, scrollback history, and live images.
    let make_vt = || {
        GhosttyEngine::new(GhosttyConfig {
            size: GridSize { cols: NonZeroU16::new(80).unwrap(), rows: NonZeroU16::new(24).unwrap() },
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_lines: 256,
            allow_images: true,
            terminfo_name: String::from("xterm-256color"),
            image_store: Some(Arc::new(Mutex::new(ImageStore::new(1 << 20)))),
        })
        .expect("construct GhosttyEngine")
    };

    let mut vt_osc = make_vt();
    let mut ev_osc = Vec::new();
    let osc_bytes =
        fs::read(seeds_root.join("v2_vt").join("osc_7501_titles_hyperlinks_palette")).expect("read osc seed");
    vt_osc.feed(&osc_bytes, &mut ev_osc);
    assert!(
        ev_osc.iter().any(|e| matches!(e, VtEvent::ProgramStatus(ProgramStatusUpdate::Set { .. }))),
        "osc seed must trigger cb_program_status Set"
    );
    assert!(ev_osc.iter().any(|e| matches!(e, VtEvent::Osc633 { .. })), "osc seed must trigger OSC 633 sanitization");

    let mut vt_sb = make_vt();
    let mut ev_sb = Vec::new();
    let sb_bytes = fs::read(seeds_root.join("v2_vt").join("primary_scrollback_history")).expect("read scrollback seed");
    vt_sb.feed(&sb_bytes, &mut ev_sb);
    assert!(vt_sb.meta().scrollback_lines > 0, "primary_scrollback_history seed must populate scrollback");

    let mut vt_img = make_vt();
    let mut ev_img = Vec::new();
    let img_bytes = fs::read(seeds_root.join("v2_vt").join("kitty_inline_direct_and_delete")).expect("read img seed");
    vt_img.feed(&img_bytes, &mut ev_img);
    assert!(!vt_img.images().is_empty(), "kitty_inline_direct_and_delete seed must retain a live placed image");
}

/// Regenerates the compact deterministic seed corpora under `fuzz/seeds/<target>/`.
///
/// Run manually with:
/// `cargo test --test fuzz_corpus regenerate_fuzz_seeds -- --ignored --nocapture`
#[test]
#[ignore]
fn regenerate_fuzz_seeds() {
    use std::num::NonZeroU16;

    use apack_os::image_sources::encoded;
    use apack_proto::codec::Writer;
    use apack_proto::frame::encode_frame;
    use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SpawnSpec};
    use apack_proto::types::{
        CellPx, CursorShape, CursorState, FrameCredit, GridSize, KeyAction, KeyInput, LeaseKind, MarksKey,
        NonZeroStreamId, PackedRow, PackedRun, PackedStyle, PaletteOverrides, RequestId, ResizePolicy, SessionId,
        SignalKind, SignalScope, StreamId, SubFlags, ViewModes, ViewportHint, WaitCondition, modifier,
    };
    use apack_wal::format::{
        GapReason, KeyframeV2, RecordingMeta, SegmentFooter, VisualKeyframe, WalRecord, encode_framed_record,
        hash_segment_records,
    };

    let seeds_root = repo_root().join("fuzz").join("seeds");

    let write_seed = |target: &str, name: &str, bytes: &[u8]| {
        let dir = seeds_root.join(target);
        fs::create_dir_all(&dir).expect("create seed dir");
        fs::write(dir.join(name), bytes).expect("write seed");
    };

    // --- 1. v2_wire seeds ---
    let frame_client = |stream: StreamId, req: RequestId, msg: &ClientMsg| -> Vec<u8> {
        let mut w = Writer::new();
        msg.encode(&mut w);
        let mut out = Vec::new();
        encode_frame(stream, msg.msg_type(), req, w.as_slice(), &mut out).expect("encode client frame");
        out
    };
    let frame_broker = |stream: StreamId, req: RequestId, msg: &BrokerMsg| -> Vec<u8> {
        let mut w = Writer::new();
        msg.encode(&mut w);
        let mut out = Vec::new();
        encode_frame(stream, msg.msg_type(), req, w.as_slice(), &mut out).expect("encode broker frame");
        out
    };
    let raw_client = |msg: &ClientMsg| -> Vec<u8> {
        let mut out = msg.msg_type().to_le_bytes().to_vec();
        let mut w = Writer::new();
        msg.encode(&mut w);
        out.extend_from_slice(w.as_slice());
        out
    };
    let raw_broker = |msg: &BrokerMsg| -> Vec<u8> {
        let mut out = msg.msg_type().to_le_bytes().to_vec();
        let mut w = Writer::new();
        msg.encode(&mut w);
        out.extend_from_slice(w.as_slice());
        out
    };

    let grid = GridSize { cols: NonZeroU16::new(80).unwrap(), rows: NonZeroU16::new(24).unwrap() };
    let cell_px = CellPx { width: 8, height: 16 };
    let stream1 = NonZeroStreamId::new(1).unwrap();
    let stream1_id = StreamId::from_raw(1);
    let sess1 = SessionId::new(1).unwrap();
    let marks_key = MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap();

    let mut wire_client_stream = Vec::new();
    wire_client_stream.extend_from_slice(&frame_client(
        StreamId::CONTROL,
        RequestId::from_raw(1),
        &ClientMsg::Hello {
            protocol_version: apack_proto::PROTOCOL_VERSION,
            client_nonce: [0x11; 32],
            client_name: "fuzz-seed".into(),
        },
    ));
    wire_client_stream.extend_from_slice(&frame_client(
        StreamId::CONTROL,
        RequestId::from_raw(2),
        &ClientMsg::AcquireLease { kind: LeaseKind::BrokerImmediate },
    ));
    wire_client_stream.extend_from_slice(&frame_client(
        StreamId::CONTROL,
        RequestId::from_raw(3),
        &ClientMsg::Spawn(Box::new(SpawnSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "echo ok".into()],
            cwd: None,
            env: vec![("TERM".into(), "xterm-256color".into())],
            size: grid,
            cell_px,
            scrollback_limit: 1000,
            marks_key: Some(marks_key),
            resize_policy: ResizePolicy::MinFit,
            record_dir: None,
            restore_from: None,
            image_files: true,
            kept: false,
        })),
    ));
    wire_client_stream.extend_from_slice(&frame_client(
        StreamId::CONTROL,
        RequestId::from_raw(4),
        &ClientMsg::Attach {
            session: sess1,
            stream_id: stream1,
            flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
            initial_viewport: Some(ViewportHint { size: grid, cell_px, visible: true, focused: true }),
            initial_output_credit: 0,
            initial_frame_credit: FrameCredit { max_frames: 8, max_bytes: 512 * 1024 },
        },
    ));
    wire_client_stream.extend_from_slice(&frame_client(
        stream1_id,
        RequestId::PUSH,
        &ClientMsg::Write { data: b"ls -la\n".to_vec() },
    ));
    wire_client_stream.extend_from_slice(&apack_proto::FIN_FRAME);
    write_seed("v2_wire", "client_session_stream", &wire_client_stream);

    let mut wire_control_ops = Vec::new();
    wire_control_ops.extend_from_slice(&frame_client(
        stream1_id,
        RequestId::from_raw(10),
        &ClientMsg::Key(KeyInput {
            code: "Enter".into(),
            action: KeyAction::Press,
            modifiers: modifier::CTRL | modifier::SHIFT,
            text: "\r".into(),
            unshifted_codepoint: 13,
            composed: false,
        }),
    ));
    wire_control_ops.extend_from_slice(&frame_client(
        stream1_id,
        RequestId::from_raw(11),
        &ClientMsg::SetSearch { query: "error".into(), case_sensitive: false },
    ));
    wire_control_ops.extend_from_slice(&frame_client(
        StreamId::CONTROL,
        RequestId::from_raw(12),
        &ClientMsg::Wait { session: sess1, condition: WaitCondition::Text("READY".into()), timeout_ms: 5000 },
    ));
    wire_control_ops.extend_from_slice(&frame_client(
        StreamId::CONTROL,
        RequestId::from_raw(13),
        &ClientMsg::Signal { session: sess1, signal: SignalKind::Term, scope: SignalScope::Group },
    ));
    write_seed("v2_wire", "client_ops_stream", &wire_control_ops);

    let sample_row = PackedRow {
        line: 12,
        flags: PackedRow::FLAG_WRAPPED,
        text: "hello 🌍".into(),
        runs: vec![PackedRun {
            col_start: 0,
            col_len: 8,
            byte_offset: 0,
            byte_len: "hello 🌍".len() as u32,
            style_idx: 1,
            link_idx: 0,
            flags: PackedRun::FLAG_PER_CELL,
        }],
        styles: vec![
            PackedStyle::default(),
            PackedStyle { fg: 0x01_10_E0_80, bg: 0, underline_color: 0, flags: PackedStyle::FLAG_BOLD },
        ],
        links: vec![String::new()],
        pieces: Vec::new(),
    };
    let sample_frame = BrokerMsg::Frame(Box::new(FrameUpdate {
        seq: 1,
        epoch: 1,
        size: grid,
        first_active_line: 12,
        scrollback_lines: 12,
        scroll_delta: 0,
        cursor: CursorState { col: 5, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
        modes: ViewModes::default(),
        palette: None,
        rows: vec![sample_row.clone()],
        images_gen: 0,
        full_sync: true,
    }));
    write_seed("v2_wire", "broker_frame_packed", &frame_broker(stream1_id, RequestId::PUSH, &sample_frame));
    write_seed("v2_wire", "raw_tag_broker_frame", &raw_broker(&sample_frame));
    write_seed("v2_wire", "raw_tag_client_metrics", &raw_client(&ClientMsg::Metrics));
    write_seed("v2_wire", "malformed_short_header", &[0x05, 0x00, 0x00, 0x00, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    write_seed("v2_wire", "fin_sentinel", &apack_proto::FIN_FRAME);

    // --- 2. v2_wal seeds ---
    let meta = RecordingMeta::new(1_700_000_000_000, [0x42; 16]);
    write_seed("v2_wal", "recording_meta_v2", &meta.encode());

    let footer = SegmentFooter {
        first_ms: 1_700_000_000_000,
        last_ms: 1_700_000_005_000,
        records_end_offset: 128,
        record_count: 3,
        records_sha256: [0x11; 32],
    };
    write_seed("v2_wal", "segment_footer_v2", &footer.encode());

    let mut rec_output = Vec::new();
    encode_framed_record(
        1_700_000_000_100,
        &WalRecord::Output(b"\x1b[32mbuild ok\x1b[0m\r\n".to_vec()),
        &mut rec_output,
    )
    .expect("encode output");
    write_seed("v2_wal", "framed_output", &rec_output);

    let keyframe = KeyframeV2 {
        visual: VisualKeyframe {
            size: grid,
            cell_px,
            active_alt_screen: false,
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes::default(),
            palette: PaletteOverrides::default(),
            primary_rows: vec![sample_row],
            alt_rows: Vec::new(),
            images: Vec::new(),
            marks_key: Some(marks_key),
            marks_last_seq: 3,
            marks_refused: 1,
            commands: Vec::new(),
        },
        engine: None,
    };
    let mut rec_kf = Vec::new();
    encode_framed_record(1_700_000_000_000, &WalRecord::Keyframe(Box::new(keyframe)), &mut rec_kf)
        .expect("encode keyframe");
    write_seed("v2_wal", "framed_keyframe_v2", &rec_kf);

    let mut sealed_seg = Vec::new();
    sealed_seg.extend_from_slice(&rec_kf);
    sealed_seg.extend_from_slice(&rec_output);
    encode_framed_record(
        1_700_000_000_200,
        &WalRecord::Resize {
            size: GridSize { cols: NonZeroU16::new(100).unwrap(), rows: NonZeroU16::new(30).unwrap() },
            cell_px,
        },
        &mut sealed_seg,
    )
    .expect("encode resize");
    encode_framed_record(1_700_000_000_300, &WalRecord::Marks { key: Some(marks_key), last_seq: 4 }, &mut sealed_seg)
        .expect("encode marks");
    encode_framed_record(
        1_700_000_000_400,
        &WalRecord::Gap { dropped_bytes: 4096, reason: GapReason::BacklogOverflow },
        &mut sealed_seg,
    )
    .expect("encode gap");
    let seg_footer = SegmentFooter {
        first_ms: 1_700_000_000_000,
        last_ms: 1_700_000_000_400,
        records_end_offset: sealed_seg.len() as u64,
        record_count: 5,
        records_sha256: hash_segment_records(&sealed_seg),
    };
    sealed_seg.extend_from_slice(&seg_footer.encode());
    write_seed("v2_wal", "complete_sealed_segment", &sealed_seg);

    let mut torn = rec_kf.clone();
    torn.extend_from_slice(&rec_output);
    torn.extend_from_slice(&rec_output[..rec_output.len() / 2]);
    write_seed("v2_wal", "torn_tail_segment", &torn);

    let mut bad_crc = rec_output.clone();
    if let Some(last) = bad_crc.last_mut() {
        *last ^= 0xFF;
    }
    write_seed("v2_wal", "corrupted_crc_record", &bad_crc);

    // --- 3. v2_vt seeds ---
    write_seed(
        "v2_vt",
        "sgr_truecolor_attrs",
        b"\x1b[1;3;4:3;9;38;2;255;128;64;48;5;236;58;2;0;200;255mStyled\x1b[0m plain\r\n",
    );
    write_seed(
        "v2_vt",
        "csi_sync_altscreen_scroll",
        b"\x1b[?2026h\x1b[?1049h\x1b[2J\x1b[1;1HHeader\x1b[2;10r\x1b[5S\x1b[?1049l\x1b[?2026l",
    );
    let mut scrollback_seed = Vec::new();
    for i in 0..32u32 {
        scrollback_seed.extend_from_slice(format!("scrollback line {i:02}\r\n").as_bytes());
    }
    write_seed("v2_vt", "primary_scrollback_history", &scrollback_seed);
    let osc7501_title_b64 = String::from_utf8(encoded(b"Run;x=1")).expect("ascii b64");
    let osc7501_msg_b64 = String::from_utf8(encoded(b"Approve build")).expect("ascii b64");
    let osc_stream = format!(
        concat!(
            "\x1b]0;Window Title\x07",
            "\x1b]7;file://host/tmp/work\x1b\\",
            "\x1b]8;id=1;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            "\x1b]4;1;rgb:ff/10/20\x1b\\",
            "\x1b]10;rgb:ee/ee/ee\x1b\\",
            "\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07ok\x1b]133;D;0\x07",
            "\x1b]633;A\x07\x1b]633;V=0123456789abcdef0123456789abcdef;1;C\x07",
            "\x1b]7501;state=blocked:kind=permission:progress=50:app=claude:title={}:msg={}\x1b\\",
            "\x1b]7501;state=clear\x1b\\",
        ),
        osc7501_title_b64, osc7501_msg_b64
    );
    write_seed("v2_vt", "osc_7501_titles_hyperlinks_palette", osc_stream.as_bytes());
    write_seed(
        "v2_vt",
        "osc7501_raw_payload",
        b"id=agent1;state=blocked;kind=permission;progress=50;app=claude;title=Waiting;message=Approve run",
    );
    write_seed(
        "v2_vt",
        "dcs_xtgettcap_decrqss",
        b"\x1bP+q544e\x1b\\\x1bP$qm\x1b\\\x1b[>1u\x1b[<u\x1bPq#0;2;0;0;0~-\x1b\\",
    );
    write_seed(
        "v2_vt",
        "kitty_inline_direct_and_delete",
        concat!(
            "\x1b_Ga=T,f=32,s=1,v=1,i=42,m=1;AAAA\x1b\\\x1b_Gm=0;AA==\x1b\\\x1b_Ga=d,d=i,i=42\x1b\\",
            "\x1b_Ga=T,f=32,s=1,v=1,i=43;/wAA/w==\x1b\\",
        )
        .as_bytes(),
    );
    write_seed("v2_vt", "utf8_emoji_cjk_invalid_c1", "hello 🚀 👨‍👩‍👧‍👦 café 中文 日本語 \u{0301}".as_bytes());
    write_seed(
        "v2_vt",
        "hostile_invalid_utf8_c1",
        &[0x80, 0x9b, b'3', b'1', b'm', 0xff, 0xfe, 0x1b, b'[', b'9', b'9', b'9', b'9', b'9', b'A', 0x18, 0x1a],
    );

    // --- 4. v2_kitty_apc seeds ---
    let mut apc_file = b"\x1b_Ga=T,f=32,s=1,v=1,i=7,t=f;".to_vec();
    apc_file.extend_from_slice(&encoded(b"/tmp/fake-seed.raw"));
    apc_file.extend_from_slice(b"\x1b\\");
    write_seed("v2_kitty_apc", "file_transmit_unix", &apc_file);

    let mut apc_shm = b"\x1b_Ga=q,f=32,s=2,v=2,t=s,q=1,I=9;".to_vec();
    apc_shm.extend_from_slice(&encoded(b"/apack-shm-seed"));
    apc_shm.extend_from_slice(b"\x1b\\");
    write_seed("v2_kitty_apc", "shm_query_unix", &apc_shm);

    write_seed(
        "v2_kitty_apc",
        "direct_passthrough_and_control",
        b"before\x1b_Ga=T,f=32,s=1,v=1,t=d,z=-5,H=10,V=-2;AAAA\x1b\\after",
    );

    let mut apc_win = Vec::new();
    for path in [
        br"\\server\share\img.png".as_slice(),
        br"\??\C:\img.png".as_slice(),
        br"C:\tmp\NUL".as_slice(),
        b"C:CON".as_slice(),
        "C:\\tmp\\COM\u{00b2}.png".as_bytes(),
    ] {
        apc_win.extend_from_slice(b"\x1b_Ga=t,f=100,t=f,O=4,S=16,o=z;");
        apc_win.extend_from_slice(&encoded(path));
        apc_win.extend_from_slice(b"\x1b\\");
    }
    write_seed("v2_kitty_apc", "windows_hostile_paths", &apc_win);

    write_seed(
        "v2_kitty_apc",
        "c1_apc_sos_pm_interrupts",
        b"\x9fGa=t,t=f;L2E=\x9c\x1bXignored_sos\x1b\\\x1b^ignored_pm\x1b\\\x1b_Ga=t,t=f;L2E=\x18\x1b_Ga=t,t=f;!!!bad===\x1b\\",
    );

    let mut apc_overlong = b"Ga=t,t=f;".to_vec();
    apc_overlong.extend_from_slice(&encoded(&vec![b'a'; 4100]));
    apc_overlong.extend_from_slice(b"\x1b\\");
    write_seed("v2_kitty_apc", "scanner_overlong_path", &apc_overlong);
}
