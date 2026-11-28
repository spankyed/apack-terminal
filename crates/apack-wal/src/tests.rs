use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use apack_proto::MAX_COMMANDS;
use apack_proto::codec::WireError;
use apack_proto::types::{
    BlobHash, CellPx, CommandOutcome, CommandRecord, CursorShape, CursorState, GridSize, ImageEncoding, ImagePiece,
    ImagePlacement, MarksKey, PackedRow, PackedRun, PackedStyle, PaletteOverrides, Rgb, ViewModes, VouchedExitCode,
};
use tempfile::tempdir;

use crate::blobs::{
    WalBlob, blob_filename, blobs_total_bytes, hash_blob, parse_blob_filename, read_blob, sweep_unreferenced,
    verify_existing_blob_hash, write_blob,
};
use crate::crc32c::{crc32c, crc32c_append};
use crate::directory::{
    META_FILENAME, RecordingRegistry, discover_segments, parse_segment_id, prepare_recording_dir, read_meta,
    segment_filename,
};
use crate::format::{
    EngineCheckpoint, FORMAT_VERSION, GapReason, KeyframeV2, MAX_RECORD_PAYLOAD, META_LEN, META_MAGIC, MIN_RECORD_LEN,
    RECORD_FLAG_ZLIB, RECORD_HEADER_LEN, RecordingMeta, SEGMENT_FOOTER_LEN, SEGMENT_FOOTER_MAGIC, SegmentFooter,
    VisualKeyframe, WalRecord, check_payload_limit, decode_one_record, encode_framed_record, hash_segment_records,
    scan_segment,
};
use crate::reader::{WalReader, read_sealed_header_and_footer, try_read_footer};
use crate::reaper::{MAX_REAP_HANDLES, WalReaper, global_reaper};
use crate::safe_fs::{
    WIN_FILE_ATTRIBUTE_REPARSE_POINT, check_file_size, open_regular_rw, read_regular_file_bounded, sync_dir,
    validate_lstat, validate_reparse_attributes, validate_unix_dir_owner_mode,
};
use crate::writer::{
    MAX_CONTROL_QUEUE_LEN, RolloverHint, WalConfig, WalHandle, WalWorker, WalWriter, should_worker_wait,
    try_reserve_backlog, try_reserve_blob_backlog,
};
use crate::{
    DEFAULT_BUDGET_BYTES, DEFAULT_MAX_BACKLOG_BYTES, DEFAULT_MAX_NON_GROUND_BYTES, DEFAULT_SEGMENT_TARGET_BYTES,
    MAX_BLOB_FILE_BYTES, MAX_SEGMENT_FILE_BYTES, MIN_SEGMENT_TARGET_BYTES, WalError,
};

fn sample_marks_key() -> MarksKey {
    MarksKey::parse(b"0123456789abcdef0123456789abcdef").expect("valid key")
}

fn sample_keyframe(blob_a: Option<BlobHash>, blob_b: Option<BlobHash>, abi: Option<[u8; 32]>) -> KeyframeV2 {
    let mut pieces = Vec::new();
    let mut flags = PackedRow::FLAG_PROMPT;
    if let Some(b) = blob_b {
        flags |= PackedRow::FLAG_HAS_PLACEHOLDER;
        pieces.push(ImagePiece { blob: b, col_start: 0, col_len: 4, src_rect: [0, 0, 16, 16], dst_px: [16, 16] });
    }
    let primary_rows = vec![PackedRow {
        line: 42,
        flags,
        text: String::from("hello world"),
        runs: vec![PackedRun {
            col_start: 0,
            col_len: 11,
            byte_offset: 0,
            byte_len: 11,
            style_idx: 1,
            link_idx: 1,
            flags: 0,
        }],
        styles: vec![
            PackedStyle::default(),
            PackedStyle { fg: 0x01_11_22_33, bg: 0, underline_color: 0, flags: PackedStyle::FLAG_BOLD },
        ],
        links: vec![String::new(), String::from("https://example.test")],
        pieces,
    }];
    let images = blob_a
        .into_iter()
        .map(|blob| ImagePlacement {
            blob,
            line: 42,
            col: 0,
            source_rect: [0, 0, 64, 32],
            cell_offset: [0, 0],
            pixel_size: [64, 32],
            cell_span: [4, 2],
            z_index: 0,
        })
        .collect();
    let visual = VisualKeyframe {
        size: GridSize::new(80, 24).expect("valid grid"),
        cell_px: CellPx { width: 9, height: 18 },
        active_alt_screen: false,
        cursor: CursorState { col: 5, row: 1, shape: CursorShape::Bar, blinking: true, hidden: false },
        modes: ViewModes { alt_screen: false, mouse: true, reverse_video: false, bracketed_paste: true },
        palette: PaletteOverrides {
            fg: Some(Rgb([10, 20, 30])),
            bg: None,
            cursor: Some(Rgb([200, 210, 220])),
            palette: vec![(1, Rgb([255, 0, 0]))],
        },
        primary_rows,
        alt_rows: Vec::new(),
        images,
        marks_key: Some(sample_marks_key()),
        marks_last_seq: 7,
        marks_active_vouched_seq: Some(7),
        marks_refused: 2,
        commands: vec![CommandRecord {
            id: 1,
            command: Some(String::from("ls -la")),
            cwd: Some(String::from("/tmp")),
            started_ms: Some(1_000),
            finished_ms: Some(1_050),
            outcome: CommandOutcome::Exited(VouchedExitCode::from_vouched(0)),
            prompt_line: Some(41),
            output_line: Some(42),
            end_line: Some(43),
        }],
    };
    let engine = abi.map(|engine_abi_hash| EngineCheckpoint {
        engine_abi_hash,
        snapshot_bytes: vec![0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4],
    });
    KeyframeV2 { visual, engine }
}

#[test]
fn crc32c_matches_rfc3720_and_streaming_append() {
    assert_eq!(crc32c(b""), 0);
    // Standard RFC 3720 / iSCSI check value for ASCII "123456789".
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    let partial = crc32c(b"1234");
    assert_eq!(crc32c_append(partial, b"56789"), 0xE306_9283);
}

#[test]
fn recording_meta_roundtrip_and_corruption_rejection() {
    let uuid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 9, 10, 11, 12, 13, 14, 15, 16];
    let meta = RecordingMeta::new(1_700_000_123_456, uuid);
    let encoded = meta.encode();
    assert_eq!(encoded.len(), META_LEN);
    assert_eq!(&encoded[0..8], &META_MAGIC);
    assert_eq!(RecordingMeta::decode(&encoded).unwrap(), meta);

    assert!(matches!(RecordingMeta::decode(&encoded[..META_LEN - 1]), Err(WalError::InvalidFormat(_))));

    let mut bad_magic = encoded;
    bad_magic[0] ^= 0x01;
    assert!(matches!(RecordingMeta::decode(&bad_magic), Err(WalError::InvalidFormat(_))));

    let mut bad_crc = encoded;
    bad_crc[15] ^= 0x80;
    assert!(matches!(RecordingMeta::decode(&bad_crc), Err(WalError::ChecksumMismatch(_))));

    let mut bad_ver = encoded;
    bad_ver[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    let crc = crc32c(&bad_ver[0..36]);
    bad_ver[36..40].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(RecordingMeta::decode(&bad_ver), Err(WalError::InvalidFormat(_))));

    let mut bad_flags = encoded;
    bad_flags[10..12].copy_from_slice(&1u16.to_le_bytes());
    let crc = crc32c(&bad_flags[0..36]);
    bad_flags[36..40].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(RecordingMeta::decode(&bad_flags), Err(WalError::InvalidFormat(_))));
}

#[test]
fn segment_footer_roundtrip_and_validation() {
    let sha = [0x33; 32];
    let footer =
        SegmentFooter { first_ms: 100, last_ms: 500, records_end_offset: 128, record_count: 12, records_sha256: sha };
    let encoded = footer.encode();
    assert_eq!(encoded.len(), SEGMENT_FOOTER_LEN);
    assert_eq!(&encoded[0..8], &SEGMENT_FOOTER_MAGIC);
    assert_eq!(SegmentFooter::decode(&encoded).unwrap(), footer);

    assert!(matches!(SegmentFooter::decode(&encoded[..10]), Err(WalError::InvalidFormat(_))));

    let mut bad_magic = encoded;
    bad_magic[7] = b'X';
    assert!(matches!(SegmentFooter::decode(&bad_magic), Err(WalError::InvalidFormat(_))));

    let mut bad_crc = encoded;
    bad_crc[20] ^= 0x01;
    assert!(matches!(SegmentFooter::decode(&bad_crc), Err(WalError::ChecksumMismatch(_))));

    let mut bad_sha_crc = encoded;
    bad_sha_crc[50] ^= 0x01;
    assert!(matches!(SegmentFooter::decode(&bad_sha_crc), Err(WalError::ChecksumMismatch(_))));

    // Inverted timestamps rejected
    let inv =
        SegmentFooter { first_ms: 500, last_ms: 499, records_end_offset: 64, record_count: 1, records_sha256: sha }
            .encode();
    assert!(matches!(SegmentFooter::decode(&inv), Err(WalError::CorruptSegment(_))));

    // Zero record count rejected
    let zero =
        SegmentFooter { first_ms: 100, last_ms: 100, records_end_offset: 64, record_count: 0, records_sha256: sha }
            .encode();
    assert!(matches!(SegmentFooter::decode(&zero), Err(WalError::CorruptSegment(_))));

    // records_end_offset < MIN_RECORD_LEN rejected
    let tiny = SegmentFooter {
        first_ms: 100,
        last_ms: 100,
        records_end_offset: (MIN_RECORD_LEN - 1) as u64,
        record_count: 1,
        records_sha256: sha,
    }
    .encode();
    assert!(matches!(SegmentFooter::decode(&tiny), Err(WalError::CorruptSegment(_))));

    // Exact MIN_RECORD_LEN and equal timestamps accepted
    let exact = SegmentFooter {
        first_ms: 42,
        last_ms: 42,
        records_end_offset: MIN_RECORD_LEN as u64,
        record_count: 1,
        records_sha256: sha,
    };
    assert_eq!(SegmentFooter::decode(&exact.encode()).unwrap(), exact);
}

#[test]
fn all_wal_record_variants_roundtrip_and_abi_fallback() {
    let blob_a = BlobHash([0xAA; 32]);
    let blob_b = BlobHash([0xBB; 32]);
    let abi = [0x42; 32];
    let other_abi = [0x99; 32];

    let kf = sample_keyframe(Some(blob_a), Some(blob_b), Some(abi));
    assert_eq!(kf.engine_snapshot_if_compatible(&abi), Some(&[0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4][..]));
    assert_eq!(kf.engine_snapshot_if_compatible(&other_abi), None);
    let refs = kf.referenced_blobs();
    assert!(refs.contains(&blob_a));
    assert!(refs.contains(&blob_b));
    assert_eq!(refs.len(), 2);

    let records = vec![
        (1000u64, WalRecord::Keyframe(Box::new(kf.clone()))),
        (1005, WalRecord::Output(b"hello \x1b[31mred\x1b[0m\r\n".to_vec())),
        (1010, WalRecord::Resize { size: GridSize::new(120, 40).unwrap(), cell_px: CellPx { width: 10, height: 20 } }),
        (1015, WalRecord::Marks { key: Some(sample_marks_key()), last_seq: 99 }),
        (1020, WalRecord::Marks { key: None, last_seq: 0 }),
        (1025, WalRecord::Gap { dropped_bytes: 4096, reason: GapReason::NonGroundCap }),
        (1030, WalRecord::Gap { dropped_bytes: 8192, reason: GapReason::BacklogOverflow }),
        (1035, WalRecord::Gap { dropped_bytes: 0, reason: GapReason::DiskError }),
    ];

    let mut stream = Vec::new();
    for (at_ms, rec) in &records {
        encode_framed_record(*at_ms, rec, &mut stream).unwrap();
    }
    let records_end = stream.len() as u64;
    let footer = SegmentFooter {
        first_ms: 1000,
        last_ms: 1035,
        records_end_offset: records_end,
        record_count: records.len() as u32,
        records_sha256: hash_segment_records(&stream),
    };
    stream.extend_from_slice(&footer.encode());

    let scan = scan_segment(&stream);
    assert!(!scan.had_torn_tail);
    assert_eq!(scan.footer, Some(footer));
    assert_eq!(scan.records.len(), records.len());
    for ((at_ms, expected), actual) in records.iter().zip(scan.records.iter()) {
        assert_eq!(actual.at_ms, *at_ms);
        assert_eq!(&actual.record, expected);
    }

    // Keyframe without engine checkpoint works too.
    let kf_visual_only = sample_keyframe(None, None, None);
    assert_eq!(kf_visual_only.engine_snapshot_if_compatible(&abi), None);
    let mut single = Vec::new();
    encode_framed_record(50, &WalRecord::Keyframe(Box::new(kf_visual_only.clone())), &mut single).unwrap();
    let (decoded, consumed) = decode_one_record(&single).unwrap();
    assert_eq!(consumed, single.len());
    assert_eq!(decoded.record, WalRecord::Keyframe(Box::new(kf_visual_only)));
}

#[test]
fn decode_and_scan_reject_bombs_corrupt_headers_and_non_monotonic_records() {
    let mut buf = Vec::new();
    encode_framed_record(10, &WalRecord::Output(b"abc".to_vec()), &mut buf).unwrap();

    assert!(matches!(decode_one_record(&buf[..RECORD_HEADER_LEN]), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&buf[..buf.len() - 1]), Err(WalError::InvalidFormat(_))));

    let mut bad_crc = buf.clone();
    *bad_crc.last_mut().unwrap() ^= 0xFF;
    assert!(matches!(decode_one_record(&bad_crc), Err(WalError::ChecksumMismatch(_))));

    let frame = |kind: u8, flags: u8, reserved: u16, payload: &[u8]| -> Vec<u8> {
        let mut out = Vec::new();
        out.push(kind);
        out.push(flags);
        out.extend_from_slice(&reserved.to_le_bytes());
        out.extend_from_slice(&10u64.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    };

    assert!(matches!(decode_one_record(&frame(2, 0, 1, b"a")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&frame(2, 0x02, 0, b"a")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&frame(2, RECORD_FLAG_ZLIB, 0, b"a")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&frame(3, RECORD_FLAG_ZLIB, 0, b"a")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&frame(4, RECORD_FLAG_ZLIB, 0, b"a")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&frame(5, RECORD_FLAG_ZLIB, 0, b"a")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(decode_one_record(&frame(99, 0, 0, b"")), Err(WalError::InvalidFormat(_))));
    assert!(matches!(
        decode_one_record(&frame(1, RECORD_FLAG_ZLIB, 0, b"not-valid-zlib")),
        Err(WalError::DecompressFailed)
    ));
    assert!(matches!(decode_one_record(&frame(5, 0, 0, &[0, 99])), Err(WalError::Wire(_))));

    let mut oversized = [0u8; RECORD_HEADER_LEN + 4];
    oversized[0] = 2;
    oversized[12..16].copy_from_slice(&((MAX_RECORD_PAYLOAD as u32) + 1).to_le_bytes());
    assert!(matches!(decode_one_record(&oversized), Err(WalError::LimitExceeded(_))));

    // encode_framed_record validates VisualKeyframe bounds BEFORE writing (> MAX_COMMANDS refused)
    let mut bad_kf = sample_keyframe(None, None, None);
    let template_cmd = bad_kf.visual.commands[0].clone();
    bad_kf.visual.commands = vec![template_cmd; MAX_COMMANDS + 1];
    assert!(matches!(
        encode_framed_record(100, &WalRecord::Keyframe(Box::new(bad_kf)), &mut Vec::new()),
        Err(WalError::LimitExceeded(_))
    ));

    // scan_segment rejects non-Keyframe at record 0
    let mut non_kf_stream = Vec::new();
    encode_framed_record(100, &WalRecord::Output(b"oops".to_vec()), &mut non_kf_stream).unwrap();
    let non_kf_scan = scan_segment(&non_kf_stream);
    assert!(non_kf_scan.had_torn_tail);
    assert!(non_kf_scan.records.is_empty());

    // scan_segment rejects a second KeyframeV2 at record 1 (anti-zlib-amplification-bomb invariant!)
    let kf = sample_keyframe(None, None, None);
    let mut double_kf = Vec::new();
    encode_framed_record(100, &WalRecord::Keyframe(Box::new(kf.clone())), &mut double_kf).unwrap();
    let after_first = double_kf.len();
    encode_framed_record(110, &WalRecord::Keyframe(Box::new(kf.clone())), &mut double_kf).unwrap();
    let bomb_scan = scan_segment(&double_kf);
    assert!(bomb_scan.had_torn_tail);
    assert_eq!(bomb_scan.records.len(), 1);
    assert_eq!(bomb_scan.valid_records_end, after_first);

    // scan_segment rejects backwards timestamp on record 1
    let mut backwards = Vec::new();
    encode_framed_record(200, &WalRecord::Keyframe(Box::new(kf)), &mut backwards).unwrap();
    let first_end = backwards.len();
    encode_framed_record(199, &WalRecord::Output(b"back".to_vec()), &mut backwards).unwrap();
    let back_scan = scan_segment(&backwards);
    assert!(back_scan.had_torn_tail);
    assert_eq!(back_scan.records.len(), 1);
    assert_eq!(back_scan.valid_records_end, first_end);
}

#[test]
fn blobs_write_dedup_read_verify_and_gc_sweep() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();

    let h1 = write_blob(dir, ImageEncoding::Png, b"fake-png-payload-1").unwrap();
    let h1_again = write_blob(dir, ImageEncoding::Png, b"fake-png-payload-1").unwrap();
    assert_eq!(h1, h1_again);
    assert_eq!(read_blob(dir, h1, ImageEncoding::Png).unwrap(), b"fake-png-payload-1");

    let h2 = write_blob(dir, ImageEncoding::ZstdRgba, b"fake-zst-payload-2").unwrap();
    assert_eq!(read_blob(dir, h2, ImageEncoding::ZstdRgba).unwrap(), b"fake-zst-payload-2");
    assert_eq!(blobs_total_bytes(dir).unwrap(), (b"fake-png-payload-1".len() + b"fake-zst-payload-2".len()) as u64);

    assert!(matches!(write_blob(dir, ImageEncoding::Png, b""), Err(WalError::InvalidFormat(_))));

    let name = blob_filename(h1, ImageEncoding::Png);
    assert_eq!(parse_blob_filename(&name), Some((h1, ImageEncoding::Png)));
    assert_eq!(parse_blob_filename("short.png"), None);
    assert_eq!(parse_blob_filename(&format!("{}.jpg", "a".repeat(64))), None);
    assert_eq!(parse_blob_filename(&format!("{}Z.png", "a".repeat(63))), None);
    assert_eq!(parse_blob_filename(&format!("{}A.png", "a".repeat(63))), None);

    // Bitrot detection on read
    let path_h2 = dir.join("blobs").join(blob_filename(h2, ImageEncoding::ZstdRgba));
    fs::write(&path_h2, b"tampered-payload!!").unwrap();
    assert!(matches!(read_blob(dir, h2, ImageEncoding::ZstdRgba), Err(WalError::CorruptBlob)));

    // Truncated existing blob is overwritten cleanly on rewrite
    fs::write(&path_h2, b"short").unwrap();
    let rewritten = write_blob(dir, ImageEncoding::ZstdRgba, b"fake-zst-payload-2").unwrap();
    assert_eq!(rewritten, h2);
    assert_eq!(read_blob(dir, h2, ImageEncoding::ZstdRgba).unwrap(), b"fake-zst-payload-2");

    // Leftover temp file + unreferenced blob swept, live blob kept.
    fs::write(dir.join("blobs").join(".tmp-999-1"), b"junk").unwrap();
    let live: HashSet<BlobHash> = [h1].into_iter().collect();
    let removed = sweep_unreferenced(dir, &live).unwrap();
    assert_eq!(removed, 2);
    assert!(dir.join("blobs").join(blob_filename(h1, ImageEncoding::Png)).exists());
    assert!(!path_h2.exists());
    assert!(!dir.join("blobs").join(".tmp-999-1").exists());
}

#[test]
fn directory_guards_refuse_foreign_dirs_symlinks_non_canonical_ids_and_recover_meta_tmp() {
    let tmp = tempdir().unwrap();

    // Non-empty directory without meta.apkm refused
    let foreign = tmp.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("important.txt"), b"do not touch").unwrap();
    assert!(matches!(prepare_recording_dir(&foreign), Err(WalError::ForeignDirectory)));

    // Directory containing ONLY leftover `.meta.tmp-*` crash staging files recovers cleanly!
    let crash_meta_dir = tmp.path().join("crash-meta");
    fs::create_dir(&crash_meta_dir).unwrap();
    fs::write(crash_meta_dir.join(".meta.tmp-1234-1"), b"half-written").unwrap();
    assert_eq!(prepare_recording_dir(&crash_meta_dir).unwrap(), None);
    assert!(!crash_meta_dir.join(".meta.tmp-1234-1").exists());

    // Regular file at recording path refused
    let reg_file = tmp.path().join("not-a-dir");
    fs::write(&reg_file, b"file").unwrap();
    assert!(matches!(prepare_recording_dir(&reg_file), Err(WalError::ForeignDirectory)));

    // Segment filename parsing enforces canonical format and rejects u64::MAX
    assert_eq!(segment_filename(1), "seg-000001.apks");
    assert_eq!(parse_segment_id("seg-000001.apks"), Some(1));
    assert_eq!(parse_segment_id("seg-1234567.apks"), Some(1_234_567));
    assert_eq!(parse_segment_id("seg-0000001.apks"), None);
    assert_eq!(parse_segment_id("seg-000000.apks"), None);
    assert_eq!(parse_segment_id("seg-00001.apks"), None);
    assert_eq!(parse_segment_id("seg-00000a.apks"), None);
    assert_eq!(parse_segment_id("other-000001.apks"), None);
    assert_eq!(parse_segment_id(&format!("seg-{}.apks", u64::MAX)), None);

    // Single-writer claim exclusivity + cross-platform read_meta & discover_segments
    let rec_dir = tmp.path().join("rec");
    let registry = RecordingRegistry::new();
    let writer = WalWriter::open_or_create(&rec_dir, [1; 16], 500, WalConfig::default(), registry.clone()).unwrap();
    assert!(rec_dir.join(META_FILENAME).exists());
    assert_eq!(read_meta(&rec_dir).unwrap().created_ms, 500);
    assert!(discover_segments(&rec_dir).unwrap().is_empty());
    assert!(registry.is_claimed(&rec_dir));
    assert!(matches!(
        WalWriter::open_or_create(&rec_dir, [1; 16], 500, WalConfig::default(), registry.clone()),
        Err(WalError::AlreadyClaimed)
    ));
    drop(writer);
    assert!(!registry.is_claimed(&rec_dir));

    // Directory named like a segment file is rejected by discover_segments (non-regular file)
    let bad_seg_dir = tmp.path().join("dir-as-seg");
    fs::create_dir(&bad_seg_dir).unwrap();
    fs::create_dir(bad_seg_dir.join("seg-000001.apks")).unwrap();
    assert!(matches!(discover_segments(&bad_seg_dir), Err(WalError::InvalidFormat(_))));

    // Symlink refusal on Unix across root, meta, segments, and blobs/
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link_dir = tmp.path().join("sym-rec");
        symlink(&rec_dir, &link_dir).unwrap();
        assert!(matches!(prepare_recording_dir(&link_dir), Err(WalError::SymlinkRefused)));
        assert!(matches!(WalReader::open(&link_dir, &registry), Err(WalError::SymlinkRefused)));

        let meta_link_dir = tmp.path().join("meta-link");
        fs::create_dir(&meta_link_dir).unwrap();
        symlink(rec_dir.join(META_FILENAME), meta_link_dir.join(META_FILENAME)).unwrap();
        assert!(matches!(read_meta(&meta_link_dir), Err(WalError::SymlinkRefused)));

        let seg_link_dir = tmp.path().join("seg-link");
        fs::create_dir(&seg_link_dir).unwrap();
        symlink(&reg_file, seg_link_dir.join("seg-000001.apks")).unwrap();
        assert!(matches!(discover_segments(&seg_link_dir), Err(WalError::SymlinkRefused)));

        let victim_dir = tmp.path().join("victim");
        fs::create_dir(&victim_dir).unwrap();
        let victim_hash = hash_blob(b"do-not-delete");
        let victim_file = victim_dir.join(blob_filename(victim_hash, ImageEncoding::Png));
        fs::write(&victim_file, b"do-not-delete").unwrap();

        let sym_blobs_root = tmp.path().join("sym-blobs-root");
        fs::create_dir(&sym_blobs_root).unwrap();
        symlink(&victim_dir, sym_blobs_root.join("blobs")).unwrap();
        assert!(matches!(sweep_unreferenced(&sym_blobs_root, &HashSet::new()), Err(WalError::SymlinkRefused)));
        assert!(matches!(read_blob(&sym_blobs_root, victim_hash, ImageEncoding::Png), Err(WalError::SymlinkRefused)));
        assert!(matches!(blobs_total_bytes(&sym_blobs_root), Err(WalError::SymlinkRefused)));
        assert!(victim_file.exists(), "symlinked blobs/ must never be swept");
    }
}

#[test]
fn torn_tail_crash_recovery_at_every_byte_offset_and_earlier_unsealed_repair() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("crash-rec");
    let registry = RecordingRegistry::new();
    let kf = sample_keyframe(None, None, None);

    let mut writer = WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    writer.start_segment_with_keyframe(100, &kf).unwrap();
    writer.record_output(110, b"first-clean-output", true).unwrap();
    let clean_len = fs::metadata(rec_dir.join("seg-000001.apks")).unwrap().len() as usize;

    writer.record_output(120, b"torn-second-record-payload", true).unwrap();
    let full_bytes = fs::read(rec_dir.join("seg-000001.apks")).unwrap();
    drop(writer);
    assert!(full_bytes.len() > clean_len);

    // Truncate inside record 2 at every byte offset and confirm exact recovery.
    for cut in (clean_len + 1)..full_bytes.len() {
        fs::write(rec_dir.join("seg-000001.apks"), &full_bytes[..cut]).unwrap();
        let mut resumed =
            WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
        assert_eq!(fs::metadata(rec_dir.join("seg-000001.apks")).unwrap().len() as usize, clean_len);
        resumed.record_output(130, b"recovered-ok", true).unwrap();
        drop(resumed);

        let reader = WalReader::open(&rec_dir, &registry).unwrap();
        let latest = reader.latest_segment_records().unwrap().unwrap();
        assert_eq!(latest.tail_records.len(), 2);
        assert_eq!(latest.tail_records[0].record, WalRecord::Output(b"first-clean-output".to_vec()));
        assert_eq!(latest.tail_records[1].record, WalRecord::Output(b"recovered-ok".to_vec()));
    }

    // Corrupt non-tail segments must NEVER be silently deleted or truncated (preserving timeline integrity).
    fs::write(rec_dir.join("seg-000001.apks"), &full_bytes[..clean_len]).unwrap(); // unsealed earlier
    fs::write(rec_dir.join("seg-000002.apks"), b"corrupt-earlier-seg").unwrap(); // corrupt earlier (bad record 0)
    fs::write(rec_dir.join("seg-000003.apks"), &full_bytes[..clean_len]).unwrap(); // active tail
    assert!(matches!(
        WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()),
        Err(WalError::CorruptSegment(_))
    ));
    assert!(rec_dir.join("seg-000002.apks").exists(), "corrupt non-tail segment must be preserved on disk");

    // Non-tail segment with valid record 0 + trailing garbage must ALSO fail without truncation!
    let mut corrupt_tail_seg = full_bytes[..clean_len].to_vec();
    corrupt_tail_seg.extend_from_slice(b"corrupt-trailing-garbage");
    let corrupt_len = corrupt_tail_seg.len() as u64;
    fs::write(rec_dir.join("seg-000002.apks"), &corrupt_tail_seg).unwrap();
    assert!(matches!(
        WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()),
        Err(WalError::CorruptSegment(_))
    ));
    assert_eq!(
        fs::metadata(rec_dir.join("seg-000002.apks")).unwrap().len(),
        corrupt_len,
        "corrupt non-tail segment must not be silently truncated"
    );

    // Once the corrupt middle segment is removed externally, an earlier unsealed segment repairs cleanly.
    fs::remove_file(rec_dir.join("seg-000002.apks")).unwrap();
    let resumed = WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert_eq!(resumed.segment_count(), 2);
    assert_eq!(
        resumed.total_segment_bytes(),
        (clean_len + SEGMENT_FOOTER_LEN + clean_len) as u64,
        "repaired sealed segment size must include footer length"
    );
    assert!(try_read_footer(&rec_dir.join("seg-000001.apks")).unwrap().is_some(), "earlier unsealed segment sealed");
    drop(resumed);

    // Corrupting record 0 on the active TAIL segment removes the torn tail cleanly.
    fs::remove_file(rec_dir.join("seg-000001.apks")).unwrap();
    fs::write(rec_dir.join("seg-000003.apks"), &full_bytes[..10]).unwrap();
    let resumed2 = WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert_eq!(resumed2.active_segment_id(), None);
    assert!(!rec_dir.join("seg-000003.apks").exists());
}

#[test]
fn rollover_budget_eviction_pin_protection_live_blob_safety_and_gc() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("budget-rec");
    let registry = RecordingRegistry::new();

    let cfg = WalConfig {
        segment_target_bytes: MIN_SEGMENT_TARGET_BYTES,
        budget_bytes: 9_000,
        max_backlog_bytes: 64 * 1024,
        max_non_ground_bytes: 8 * 1024,
    };
    assert_eq!(cfg.effective_segment_target(), MIN_SEGMENT_TARGET_BYTES);
    assert_eq!(WalConfig { segment_target_bytes: 100, ..cfg }.effective_segment_target(), MIN_SEGMENT_TARGET_BYTES);

    let mut writer = WalWriter::open_or_create(&rec_dir, [3; 16], 1_000, cfg, registry.clone()).unwrap();
    let blob1 = writer.write_blob(ImageEncoding::Png, b"image-blob-seg-1").unwrap();
    let blob2 = writer.write_blob(ImageEncoding::Png, b"image-blob-seg-2").unwrap();
    let blob3 = writer.write_blob(ImageEncoding::Png, b"image-blob-seg-3").unwrap();

    // Segment 1 (referencing blob1; note blob2/blob3 were staged before seg-1, so rollover clears pending_blobs!)
    writer.start_segment_with_keyframe(1_000, &sample_keyframe(Some(blob1), None, None)).unwrap();
    assert!(
        !rec_dir.join("blobs").join(blob_filename(blob2, ImageEncoding::Png)).exists(),
        "unreferenced blob2 swept on rollover"
    );
    assert!(
        !rec_dir.join("blobs").join(blob_filename(blob3, ImageEncoding::Png)).exists(),
        "unreferenced blob3 swept on rollover"
    );

    // Re-stage blob2 mid-segment and confirm it stays pinned while seg-1 is active!
    let blob2 = writer.write_blob(ImageEncoding::Png, b"image-blob-seg-2").unwrap();
    let hint = writer.record_output(1_100, &vec![b'A'; 4_500], true).unwrap();
    assert_eq!(hint, RolloverHint::NeedKeyframe);

    // Pin seg-1 by opening a WalReader BEFORE rolling over!
    let reader_pinning_seg1 = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader_pinning_seg1.segments().len(), 1);
    assert_eq!(reader_pinning_seg1.segments()[0].id, 1);

    let ev1 = writer.start_segment_with_keyframe(2_000, &sample_keyframe(Some(blob2), None, None)).unwrap();
    assert_eq!(ev1.evicted_segments, 0);
    writer.record_output(2_100, &vec![b'B'; 4_500], true).unwrap();

    // Stage blob3 right before rolling to seg-3 -> seg-1 is pinned, so eviction MUST STOP at seg-1
    // and keep seg-2 intact (no timeline hole!), preserving blob1, blob2, and blob3 until unpinned!
    let blob3 = writer.write_blob(ImageEncoding::Png, b"image-blob-seg-3").unwrap();
    let ev2 = writer.start_segment_with_keyframe(3_000, &sample_keyframe(Some(blob3), None, None)).unwrap();
    assert!(rec_dir.join("seg-000001.apks").exists(), "pinned seg-1 must not be unlinked");
    assert!(rec_dir.join("seg-000002.apks").exists(), "seg-2 must not be evicted past pinned seg-1 (no timeline hole)");
    assert_eq!(ev2.skipped_pinned, 1);
    assert_eq!(ev2.evicted_segments, 0);
    assert!(rec_dir.join("blobs").join(blob_filename(blob1, ImageEncoding::Png)).exists(), "blob1 kept for seg-1");
    assert!(rec_dir.join("blobs").join(blob_filename(blob2, ImageEncoding::Png)).exists(), "blob2 kept for seg-2");
    assert!(rec_dir.join("blobs").join(blob_filename(blob3, ImageEncoding::Png)).exists(), "blob3 kept for seg-3");

    // Once seg-1 is unpinned, chronological eviction removes seg-1 AND seg-2 and sweeps blob1 + blob2,
    // while keeping mid-segment live blob written during seg-3!
    let live_mid = writer.write_blob(ImageEncoding::Png, b"mid-segment-live-blob").unwrap();
    drop(reader_pinning_seg1);
    writer.record_output(3_100, &vec![b'C'; 4_500], true).unwrap();
    let ev3 = writer.evict_to_budget().unwrap();
    assert_eq!(ev3.evicted_segments, 2);
    assert_eq!(ev3.swept_blobs, 2); // blob1 + blob2 swept once seg-1 and seg-2 are gone!
    assert!(!rec_dir.join("seg-000001.apks").exists());
    assert!(!rec_dir.join("seg-000002.apks").exists());
    assert!(!rec_dir.join("blobs").join(blob_filename(blob1, ImageEncoding::Png)).exists());
    assert!(!rec_dir.join("blobs").join(blob_filename(blob2, ImageEncoding::Png)).exists());
    assert!(!rec_dir.join("blobs").join(blob_filename(blob2, ImageEncoding::Png)).exists());
    assert!(rec_dir.join("blobs").join(blob_filename(blob3, ImageEncoding::Png)).exists());
    assert!(
        rec_dir.join("blobs").join(blob_filename(live_mid, ImageEncoding::Png)).exists(),
        "live mid-segment blob must survive eviction sweep!"
    );
}

#[test]
fn seek_binary_search_sealed_integrity_and_monotonic_segment_validation() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("seek-rec");
    let registry = RecordingRegistry::new();

    let mut writer = WalWriter::open_or_create(&rec_dir, [4; 16], 100, WalConfig::default(), registry.clone()).unwrap();

    // Empty recording returns None
    {
        let r = WalReader::open(&rec_dir, &registry).unwrap();
        assert_eq!(r.span(), None);
        assert!(r.latest_segment_records().unwrap().is_none());
        assert!(r.records_for_moment(100).unwrap().is_none());
    }

    // Segment 1: t = 1000..1200 (clock rollback clamped to 1100)
    writer.start_segment_with_keyframe(1_000, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(1_100, b"s1-a", true).unwrap();
    writer.record_output(900, b"s1-rollback-clamped", true).unwrap();
    writer.record_output(1_200, b"s1-b", true).unwrap();

    // Segment 2: t = 2000..2200
    writer.start_segment_with_keyframe(2_000, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(2_100, b"s2-a", true).unwrap();
    writer.record_resize(2_150, GridSize::new(100, 30).unwrap(), CellPx { width: 8, height: 16 }).unwrap();
    writer.record_output(2_200, b"s2-b", true).unwrap();

    // Segment 3 (unsealed active tail): t = 3000..3100
    writer.start_segment_with_keyframe(3_000, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(3_100, b"s3-active", true).unwrap();
    drop(writer);

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader.segments().len(), 3);
    assert!(reader.segments()[0].sealed);
    assert!(reader.segments()[1].sealed);
    assert!(!reader.segments()[2].sealed);
    assert_eq!(reader.span(), Some((1_000, 3_100)));

    // Verify fast sealed header+footer reader works on seg-1
    let (hdr_rec, hdr_ftr, _) = read_sealed_header_and_footer(&rec_dir.join("seg-000001.apks")).unwrap().unwrap();
    assert_eq!(hdr_rec.at_ms, 1_000);
    assert_eq!(hdr_ftr.last_ms, 1_200);

    let before = reader.records_for_moment(500).unwrap().unwrap();
    assert_eq!(before.segment_id, 1);
    assert!(before.tail_records.is_empty());

    let mid1 = reader.records_for_moment(1_100).unwrap().unwrap();
    assert_eq!(mid1.segment_id, 1);
    assert_eq!(mid1.tail_records.len(), 2);
    assert_eq!(mid1.tail_records[1].at_ms, 1_100);

    let mid2 = reader.records_for_moment(2_150).unwrap().unwrap();
    assert_eq!(mid2.segment_id, 2);
    assert_eq!(mid2.tail_records.len(), 2);

    let end = reader.records_for_moment(9_999).unwrap().unwrap();
    assert_eq!(end.segment_id, 3);
    assert_eq!(end.tail_records.len(), 1);

    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(latest.segment_id, 3);
    assert_eq!(latest.tail_records.len(), 1);

    // Flip a byte inside sealed seg-2's body (footer intact): load_segment_slice must fail with CorruptSegment!
    let seg2_path = rec_dir.join("seg-000002.apks");
    let mut seg2_bytes = fs::read(&seg2_path).unwrap();
    let mid_byte = seg2_bytes.len() - SEGMENT_FOOTER_LEN - 10;
    seg2_bytes[mid_byte] ^= 0x55;
    fs::write(&seg2_path, &seg2_bytes).unwrap();
    assert!(matches!(reader.records_for_moment(2_150), Err(WalError::CorruptSegment(_))));
    drop(reader);

    // Non-monotonic segment timestamps across segments rejected by WalReader::open!
    let mut seg4 = Vec::new();
    encode_framed_record(500, &WalRecord::Keyframe(Box::new(sample_keyframe(None, None, None))), &mut seg4).unwrap();
    fs::write(rec_dir.join("seg-000004.apks"), &seg4).unwrap();
    assert!(matches!(WalReader::open(&rec_dir, &registry), Err(WalError::CorruptSegment(_))));
}

#[test]
fn non_ground_run_cap_emits_explicit_gap_marker_and_no_active_does_not_advance_clock() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("nonground-rec");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { max_non_ground_bytes: 100, ..WalConfig::default() };

    let mut writer = WalWriter::open_or_create(&rec_dir, [5; 16], 100, cfg, registry.clone()).unwrap();
    // Errors before starting a segment must NOT advance monotonic last_at_ms!
    assert!(matches!(writer.record_output(9_999, b"hi", true), Err(WalError::CorruptSegment(_))));
    assert!(matches!(
        writer.record_resize(9_999, GridSize::new(80, 24).unwrap(), CellPx::default()),
        Err(WalError::CorruptSegment(_))
    ));
    assert!(matches!(writer.record_marks(9_999, None, 0), Err(WalError::CorruptSegment(_))));
    assert!(matches!(writer.record_gap(9_999, 1, GapReason::DiskError), Err(WalError::CorruptSegment(_))));

    writer.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(110, &[b'x'; 80], false).unwrap();
    writer.record_output(120, &[b'y'; 50], false).unwrap();
    writer.record_output(130, &[b'z'; 70], false).unwrap();
    writer.record_output(140, b"back", true).unwrap();
    writer.seal_active().unwrap();
    drop(writer);

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(latest.keyframe_at_ms, 100, "earlier errors must not have advanced clock to 9999");
    assert_eq!(latest.tail_records.len(), 3);
    assert_eq!(latest.tail_records[0].record, WalRecord::Output(vec![b'x'; 80]));
    assert_eq!(latest.tail_records[1].record, WalRecord::Gap { dropped_bytes: 120, reason: GapReason::NonGroundCap });
    assert_eq!(latest.tail_records[2].record, WalRecord::Output(b"back".to_vec()));
}

#[test]
fn async_worker_preserves_gap_order_reports_rollover_and_sheds_backlog() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("worker-rec");
    let registry = RecordingRegistry::new();
    let cfg =
        WalConfig { segment_target_bytes: MIN_SEGMENT_TARGET_BYTES, max_backlog_bytes: 5_000, ..WalConfig::default() };

    let writer = WalWriter::open_or_create(&rec_dir, [6; 16], 1_000, cfg, registry.clone()).unwrap();
    let handle = WalWorker::spawn(writer);
    assert!(!handle.is_failed());
    assert!(!handle.take_rollover_needed());

    // Zero timeout returns Timeout (distinct from WorkerClosed), Duration::MAX never panics!
    assert!(matches!(handle.flush_barrier_timeout(Duration::ZERO), Err(WalError::Timeout)));

    let blob_data = b"worker-png-bytes".to_vec();
    let blob_hash = hash_blob(&blob_data);
    assert!(handle.write_blob(ImageEncoding::Png, blob_data.clone()));
    assert!(handle.start_segment(1_000, sample_keyframe(Some(blob_hash), None, None)));
    assert!(handle.write_output(1_010, b"before-gap".to_vec(), true));
    assert!(handle.write_resize(1_020, GridSize::new(90, 30).unwrap(), CellPx { width: 8, height: 16 }));
    assert!(handle.write_marks(1_030, Some(sample_marks_key()), 4));

    // Oversized output > max_backlog_bytes (5000) is shed in chronological order (after t=1030, never before!)
    assert!(!handle.write_output(1_040, vec![b'!'; 6_000], true));
    assert!(!handle.write_output(1_045, vec![b'?'; 6_000], true)); // coalesces into same tail BacklogGap!
    assert!(handle.write_output(1_050, vec![b'R'; 4_500], true)); // triggers RolloverHint::NeedKeyframe!
    handle.flush_barrier_timeout(Duration::MAX).unwrap();
    assert!(handle.take_rollover_needed(), "worker must surface RolloverHint::NeedKeyframe");
    assert!(!handle.take_rollover_needed(), "take_rollover_needed clears atomically");

    assert!(handle.seal());
    handle.shutdown().unwrap();

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader.read_blob(blob_hash, ImageEncoding::Png).unwrap(), blob_data);
    let latest = reader.latest_segment_records().unwrap().unwrap();
    // Exact chronological order: Output(1010), Resize(1020), Marks(1030), Gap(1045, 12000), Output(1050)
    assert_eq!(latest.tail_records.len(), 5);
    assert_eq!(latest.tail_records[0].at_ms, 1_010);
    assert_eq!(latest.tail_records[1].at_ms, 1_020);
    assert_eq!(latest.tail_records[2].at_ms, 1_030);
    assert_eq!(
        latest.tail_records[3].record,
        WalRecord::Gap { dropped_bytes: 12_000, reason: GapReason::BacklogOverflow }
    );
    assert_eq!(latest.tail_records[4].at_ms, 1_050);
}

#[test]
fn corrupted_footer_and_spoofed_pty_footer_rejected() {
    let mut stream = Vec::new();
    let kf = sample_keyframe(None, None, None);
    encode_framed_record(100, &WalRecord::Keyframe(Box::new(kf)), &mut stream).unwrap();
    let valid_end = stream.len();

    // Footer claiming wrong record_count (99 instead of 1) -> treated as torn tail.
    let valid_sha = hash_segment_records(&stream);
    let bad_count_footer = SegmentFooter {
        first_ms: 100,
        last_ms: 100,
        records_end_offset: valid_end as u64,
        record_count: 99,
        records_sha256: valid_sha,
    }
    .encode();
    let mut s1 = stream.clone();
    s1.extend_from_slice(&bad_count_footer);
    let scan1 = scan_segment(&s1);
    assert!(scan1.had_torn_tail);
    assert_eq!(scan1.footer, None);
    assert_eq!(scan1.valid_records_end, valid_end);
    assert_eq!(scan1.records.len(), 1);

    // Footer with wrong SHA-256 digest -> treated as torn tail even when bounds & CRC32C match!
    let mut wrong_sha = valid_sha;
    wrong_sha[0] ^= 0xFF;
    let bad_sha_footer = SegmentFooter {
        first_ms: 100,
        last_ms: 100,
        records_end_offset: valid_end as u64,
        record_count: 1,
        records_sha256: wrong_sha,
    }
    .encode();
    let mut s_sha = stream.clone();
    s_sha.extend_from_slice(&bad_sha_footer);
    let scan_sha = scan_segment(&s_sha);
    assert!(scan_sha.had_torn_tail);
    assert_eq!(scan_sha.footer, None);
    assert_eq!(scan_sha.valid_records_end, valid_end);

    // Footer claiming wrong records_end_offset -> treated as torn tail AND rejected by try_read_footer!
    let bad_off_footer = SegmentFooter {
        first_ms: 100,
        last_ms: 100,
        records_end_offset: (valid_end + 1) as u64,
        record_count: 1,
        records_sha256: valid_sha,
    }
    .encode();
    let mut s2 = stream.clone();
    s2.extend_from_slice(&bad_off_footer);
    assert!(scan_segment(&s2).had_torn_tail);

    let tmp = tempdir().unwrap();
    let spoof_path = tmp.path().join("spoof.apks");
    fs::write(&spoof_path, &s2).unwrap();
    assert_eq!(try_read_footer(&spoof_path).unwrap(), None);

    // 72-byte file with zero records rejected by try_read_footer!
    let zero_rec_path = tmp.path().join("only-footer.apks");
    fs::write(
        &zero_rec_path,
        SegmentFooter { first_ms: 1, last_ms: 1, records_end_offset: 20, record_count: 1, records_sha256: [0; 32] }
            .encode(),
    )
    .unwrap();
    assert_eq!(try_read_footer(&zero_rec_path).unwrap(), None);
}

#[test]
fn non_ground_gap_flushed_on_seal_if_never_returned_to_ground() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("seal-gap");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { max_non_ground_bytes: 10, ..WalConfig::default() };

    let mut writer = WalWriter::open_or_create(&rec_dir, [7; 16], 100, cfg, registry.clone()).unwrap();
    writer.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(110, &[b'a'; 25], false).unwrap();
    writer.seal_active().unwrap();
    drop(writer);

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(latest.tail_records.len(), 1);
    assert_eq!(latest.tail_records[0].record, WalRecord::Gap { dropped_bytes: 25, reason: GapReason::NonGroundCap });
}

#[test]
fn corrupted_sealed_segment_trailing_garbage_is_detected_on_open() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("corrupt-seg");
    let registry = RecordingRegistry::new();
    let mut writer = WalWriter::open_or_create(&rec_dir, [8; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    writer.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(110, b"ok", true).unwrap();
    writer.seal_active().unwrap();
    drop(writer);

    let seg_path = rec_dir.join("seg-000001.apks");
    append_bytes(&seg_path, b"trailing-garbage-bytes");
    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader.segments().len(), 1);
    assert!(!reader.segments()[0].sealed);
    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(latest.tail_records.len(), 1);
}

fn append_bytes(path: &Path, extra: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(extra).unwrap();
}

#[test]
fn constants_and_error_display_and_registry_debug_are_exact() {
    assert_eq!(DEFAULT_SEGMENT_TARGET_BYTES, 2 * 1024 * 1024);
    assert_eq!(MIN_SEGMENT_TARGET_BYTES, 4096);
    assert_eq!(DEFAULT_BUDGET_BYTES, 32 * 1024 * 1024);
    assert_eq!(DEFAULT_MAX_BACKLOG_BYTES, 16 * 1024 * 1024);
    assert_eq!(DEFAULT_MAX_NON_GROUND_BYTES, 4 * 1024 * 1024);
    assert_eq!(MAX_BLOB_FILE_BYTES, 64 * 1024 * 1024);
    assert_eq!(MAX_SEGMENT_FILE_BYTES, 64 * 1024 * 1024);
    assert_eq!(MAX_CONTROL_QUEUE_LEN, 1_024);

    let errors: Vec<WalError> = vec![
        WalError::Io(std::io::Error::other("disk")),
        WalError::Wire(WireError::UnexpectedEof),
        WalError::ChecksumMismatch("meta"),
        WalError::ForeignDirectory,
        WalError::SymlinkRefused,
        WalError::AlreadyClaimed,
        WalError::InvalidFormat("bad"),
        WalError::LimitExceeded("cap"),
        WalError::DecompressFailed,
        WalError::CorruptBlob,
        WalError::CorruptSegment("seg"),
        WalError::WorkerClosed,
        WalError::Timeout,
    ];
    for err in errors {
        assert!(!err.to_string().is_empty());
    }

    let reg = RecordingRegistry::new();
    assert!(format!("{reg:?}").contains("RecordingRegistry"));
}

#[test]
fn boundary_helpers_and_bounded_file_reads() {
    assert!(check_file_size(99, 100, "x").is_ok());
    assert!(check_file_size(100, 100, "x").is_ok());
    assert!(matches!(check_file_size(101, 100, "x"), Err(WalError::LimitExceeded(_))));

    assert!(check_payload_limit(99, 100, "x").is_ok());
    assert!(check_payload_limit(100, 100, "x").is_ok());
    assert!(matches!(check_payload_limit(101, 100, "x"), Err(WalError::LimitExceeded(_))));

    let tmp = tempdir().unwrap();
    let f = tmp.path().join("ten.bin");
    fs::write(&f, b"0123456789").unwrap();
    assert_eq!(read_regular_file_bounded(&f, 10, "ten").unwrap(), b"0123456789");
    assert!(matches!(read_regular_file_bounded(&f, 9, "ten"), Err(WalError::LimitExceeded(_))));
    assert!(matches!(read_regular_file_bounded(tmp.path(), 100, "dir"), Err(WalError::InvalidFormat(_))));

    // Exact minimum sealed length (MIN_RECORD_LEN + SEGMENT_FOOTER_LEN = 92 bytes) accepted; 91 rejected!
    let mut exact = vec![0u8; MIN_RECORD_LEN];
    let ftr = SegmentFooter {
        first_ms: 42,
        last_ms: 42,
        records_end_offset: MIN_RECORD_LEN as u64,
        record_count: 1,
        records_sha256: hash_segment_records(&exact),
    };
    exact.extend_from_slice(&ftr.encode());
    let short_path = tmp.path().join("short.apks");
    fs::write(&short_path, &exact[1..]).unwrap();
    assert_eq!(try_read_footer(&short_path).unwrap(), None);

    let exact_path = tmp.path().join("exact.apks");
    fs::write(&exact_path, &exact).unwrap();
    assert_eq!(try_read_footer(&exact_path).unwrap(), Some(ftr));

    // Directory passed to try_read_footer fails with InvalidFormat (cross-platform!)
    assert!(matches!(try_read_footer(tmp.path()), Err(WalError::InvalidFormat(_))));
    #[cfg(unix)]
    {
        let sym_seg = tmp.path().join("sym-seg.apks");
        std::os::unix::fs::symlink(&exact_path, &sym_seg).unwrap();
        assert!(matches!(try_read_footer(&sym_seg), Err(WalError::SymlinkRefused)));
    }
}

#[test]
fn directory_and_blobs_io_error_paths_and_multi_reader_pin_refcount() {
    let tmp = tempdir().unwrap();

    assert_eq!(sweep_unreferenced(tmp.path(), &HashSet::new()).unwrap(), 0);
    assert_eq!(blobs_total_bytes(tmp.path()).unwrap(), 0);

    let file_blobs_root = tmp.path().join("file-blobs");
    fs::create_dir(&file_blobs_root).unwrap();
    fs::write(file_blobs_root.join("blobs"), b"not-a-dir").unwrap();
    assert!(matches!(write_blob(&file_blobs_root, ImageEncoding::Png, b"x"), Err(WalError::InvalidFormat(_))));
    assert!(matches!(sweep_unreferenced(&file_blobs_root, &HashSet::new()), Err(WalError::InvalidFormat(_))));
    assert!(matches!(blobs_total_bytes(&file_blobs_root), Err(WalError::InvalidFormat(_))));

    let blocking_file = tmp.path().join("blocker");
    fs::write(&blocking_file, b"x").unwrap();
    assert!(matches!(prepare_recording_dir(&blocking_file.join("child")), Err(WalError::Io(_))));
    assert!(matches!(write_blob(&blocking_file, ImageEncoding::Png, b"x"), Err(WalError::InvalidFormat(_))));

    let rec_dir = tmp.path().join("refcount");
    let registry = RecordingRegistry::new();
    let mut writer = WalWriter::open_or_create(&rec_dir, [9; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert_eq!(writer.segment_count(), 0);
    writer.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    assert_eq!(writer.segment_count(), 1);
    writer.record_marks(105, Some(sample_marks_key()), 12).unwrap();
    writer.seal_active().unwrap();
    assert_eq!(writer.segment_count(), 1);
    writer.start_segment_with_keyframe(200, &sample_keyframe(None, None, None)).unwrap();
    assert_eq!(writer.segment_count(), 2);

    let canonical = fs::canonicalize(&rec_dir).unwrap();
    let r1 = WalReader::open(&rec_dir, &registry).unwrap();
    let r2 = WalReader::open(&rec_dir, &registry).unwrap();
    assert!(registry.is_pinned(&canonical, 1));
    drop(r1);
    assert!(registry.is_pinned(&canonical, 1));
    drop(r2);
    assert!(!registry.is_pinned(&canonical, 1));

    let r3 = WalReader::open(&rec_dir, &registry).unwrap();
    let s1 = r3.records_for_moment(110).unwrap().unwrap();
    assert_eq!(s1.tail_records.len(), 1);
    assert_eq!(s1.tail_records[0].record, WalRecord::Marks { key: Some(sample_marks_key()), last_seq: 12 });
}

#[test]
fn non_ground_exact_boundary_rollover_gate_and_budget_exact_boundary() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("exact-bounds");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig {
        segment_target_bytes: MIN_SEGMENT_TARGET_BYTES,
        budget_bytes: 100_000,
        max_backlog_bytes: 1024,
        max_non_ground_bytes: 5_000,
    };
    let mut writer = WalWriter::open_or_create(&rec_dir, [10; 16], 100, cfg, registry.clone()).unwrap();
    writer.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();

    let hint = writer.record_output(110, &vec![b'a'; 5_000], false).unwrap();
    assert_eq!(hint, RolloverHint::Continue);
    let hint2 = writer.record_output(115, b"z", false).unwrap();
    assert_eq!(hint2, RolloverHint::Continue);
    let hint3 = writer.record_output(120, b"ok", true).unwrap();
    assert_eq!(hint3, RolloverHint::NeedKeyframe);
    writer.seal_active().unwrap();

    let exact_total = writer.total_segment_bytes();
    drop(writer);

    let exact_cfg = WalConfig { budget_bytes: exact_total, ..cfg };
    let mut w2 = WalWriter::open_or_create(&rec_dir, [10; 16], 100, exact_cfg, registry.clone()).unwrap();
    let ev = w2.evict_to_budget().unwrap();
    assert_eq!(ev.evicted_segments, 0);
    assert!(rec_dir.join("seg-000001.apks").exists());

    fs::remove_file(rec_dir.join("seg-000001.apks")).unwrap();
    w2.start_segment_with_keyframe(300, &sample_keyframe(None, None, None)).unwrap();
    assert_eq!(w2.segment_count(), 1);
}

#[test]
fn should_worker_wait_truth_table() {
    assert!(should_worker_wait(true, false));
    assert!(!should_worker_wait(true, true));
    assert!(!should_worker_wait(false, false));
    assert!(!should_worker_wait(false, true));
}

#[test]
fn worker_backlog_accounting_failure_propagation_and_drop_cleanup() {
    let tmp = tempdir().unwrap();

    let rec1 = tmp.path().join("worker-bounds");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { max_backlog_bytes: 100, ..WalConfig::default() };
    let writer1 = WalWriter::open_or_create(&rec1, [11; 16], 100, cfg, registry.clone()).unwrap();
    let h1 = WalWorker::spawn(writer1);
    assert!(h1.start_segment(100, sample_keyframe(None, None, None)));
    h1.flush_barrier_timeout(Duration::from_secs(2)).unwrap();
    assert!(h1.write_output(110, vec![b'x'; 100], true));
    h1.flush_barrier_timeout(Duration::from_secs(2)).unwrap();
    assert!(h1.write_blob(ImageEncoding::Png, vec![b'p'; 100]));
    assert!(h1.write_resize(120, GridSize::new(100, 40).unwrap(), CellPx { width: 8, height: 16 }));
    assert!(h1.write_marks(130, Some(sample_marks_key()), 5));
    assert!(h1.seal());
    h1.flush_barrier_timeout(Duration::from_secs(2)).unwrap();

    drop(h1);
    assert!(!registry.is_claimed(&rec1), "Drop for WalHandle must stop thread and release WriterClaim");

    let reader = WalReader::open(&rec1, &registry).unwrap();
    assert!(reader.segments()[0].sealed, "seal() must seal the active segment");
    let slice = reader.latest_segment_records().unwrap().unwrap();
    assert!(!slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Gap { .. })));
    assert!(slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Resize { .. })));
    assert!(slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Marks { last_seq: 5, .. })));

    // Worker sticky failure propagation
    let rec2 = tmp.path().join("worker-fail");
    let writer2 = WalWriter::open_or_create(&rec2, [12; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    let h2 = WalWorker::spawn(writer2);
    assert!(h2.write_output(100, b"boom".to_vec(), true));
    assert!(h2.flush_barrier_timeout(Duration::from_secs(2)).is_err());
    assert!(h2.is_failed());
    assert!(!h2.write_output(110, b"no".to_vec(), true));
    assert!(!h2.write_resize(110, GridSize::new(80, 24).unwrap(), CellPx::default()));
    assert!(!h2.write_marks(110, None, 0));
    assert!(!h2.write_blob(ImageEncoding::Png, b"no".to_vec()));
    assert!(!h2.seal());
    assert!(!h2.start_segment(110, sample_keyframe(None, None, None)));
    assert!(h2.shutdown().is_err());
    assert!(!registry.is_claimed(&rec2));

    // Deterministic backlog reservation boundary checks (normal + empty-queue single large blob)
    let mut queued = 0usize;
    assert!(try_reserve_backlog(&mut queued, 60, 100));
    assert_eq!(queued, 60);
    assert!(try_reserve_backlog(&mut queued, 40, 100));
    assert_eq!(queued, 100);
    assert!(!try_reserve_backlog(&mut queued, 1, 100));
    assert_eq!(queued, 100);

    let mut blob_q = 0usize;
    assert!(try_reserve_blob_backlog(&mut blob_q, 512, 128), "empty queue admits single blob > max_backlog");
    assert_eq!(blob_q, 512);
    assert!(!try_reserve_blob_backlog(&mut blob_q, 10, 128), "non-empty queue enforces max_backlog");
    assert_eq!(blob_q, 512);
}

#[test]
fn remaining_edge_cases_and_helper_invariants() {
    let tmp = tempdir().unwrap();

    // verify_existing_blob_hash true and false
    let h = hash_blob(b"abc");
    assert!(verify_existing_blob_hash(b"abc", h));
    assert!(!verify_existing_blob_hash(b"abd", h));

    // sync_dir succeeds on existing dir; on Unix fails on nonexistent path
    assert!(sync_dir(tmp.path()).is_ok());
    #[cfg(unix)]
    assert!(sync_dir(&tmp.path().join("does-not-exist")).is_err());

    // Two adjacent segments with identical `first_ms` are valid in WalReader::open
    let same_ms_dir = tmp.path().join("same-ms");
    let registry = RecordingRegistry::new();
    let mut w = WalWriter::open_or_create(&same_ms_dir, [14; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    w.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    w.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    // evict_to_budget sweeps orphaned blobs/tmp files even when evicted_segments == 0 so cleared
    // images never leak disk space until budget exhaustion.
    fs::create_dir_all(same_ms_dir.join("blobs")).unwrap();
    fs::write(same_ms_dir.join("blobs").join(".tmp-orphan"), b"x").unwrap();
    let ev0 = w.evict_to_budget().unwrap();
    assert_eq!(ev0.evicted_segments, 0);
    assert_eq!(ev0.swept_blobs, 1);
    assert!(!same_ms_dir.join("blobs").join(".tmp-orphan").exists());
    drop(w);

    let r = WalReader::open(&same_ms_dir, &registry).unwrap();
    assert_eq!(r.segments().len(), 2);

    // read_sealed_header_and_footer rejects non-keyframe record 0 and mismatched footer.first_ms
    let non_kf_path = tmp.path().join("non-kf-sealed.apks");
    let mut non_kf = Vec::new();
    encode_framed_record(100, &WalRecord::Output(b"not-a-kf".to_vec()), &mut non_kf).unwrap();
    let end1 = non_kf.len() as u64;
    let sha1 = hash_segment_records(&non_kf);
    non_kf.extend_from_slice(
        &SegmentFooter { first_ms: 100, last_ms: 100, records_end_offset: end1, record_count: 1, records_sha256: sha1 }
            .encode(),
    );
    fs::write(&non_kf_path, &non_kf).unwrap();
    assert_eq!(read_sealed_header_and_footer(&non_kf_path).unwrap(), None);

    let bad_ts_path = tmp.path().join("bad-ts-sealed.apks");
    let mut bad_ts = Vec::new();
    encode_framed_record(100, &WalRecord::Keyframe(Box::new(sample_keyframe(None, None, None))), &mut bad_ts).unwrap();
    let end2 = bad_ts.len() as u64;
    let sha2 = hash_segment_records(&bad_ts);
    bad_ts.extend_from_slice(
        &SegmentFooter { first_ms: 200, last_ms: 200, records_end_offset: end2, record_count: 1, records_sha256: sha2 }
            .encode(),
    );
    fs::write(&bad_ts_path, &bad_ts).unwrap();
    assert_eq!(read_sealed_header_and_footer(&bad_ts_path).unwrap(), None);

    // Small output on WalWorker must NOT set rollover_needed
    let small_dir = tmp.path().join("small-worker");
    let sw = WalWriter::open_or_create(&small_dir, [15; 16], 100, WalConfig::default(), registry).unwrap();
    let sh = WalWorker::spawn(sw);
    assert!(sh.start_segment(100, sample_keyframe(None, None, None)));
    assert!(sh.write_output(110, b"tiny".to_vec(), true));
    sh.flush_barrier_timeout(Duration::from_secs(2)).unwrap();
    assert!(!sh.take_rollover_needed());
    sh.shutdown().unwrap();
}

#[test]
fn evict_credits_freed_blob_bytes_before_cascading_segments() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("blob-credit");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { budget_bytes: 20_000, segment_target_bytes: 1_000_000, ..WalConfig::default() };
    let mut w = WalWriter::open_or_create(&dir, [21; 16], 100, cfg, registry.clone()).unwrap();

    // Segment 1 references a large 16 KiB image blob.
    let big_blob = vec![0xAB; 16_384];
    let hash = w.write_blob(ImageEncoding::Png, &big_blob).unwrap();
    let kf1 = sample_keyframe(Some(hash), None, None);
    w.start_segment_with_keyframe(100, &kf1).unwrap();

    // Segment 2 has no blobs (~small keyframe), plus 4 KiB of output so seg1+seg2+seg3+blob > 20_000
    // while seg2+seg3 alone << 20_000.
    w.start_segment_with_keyframe(200, &sample_keyframe(None, None, None)).unwrap();
    w.record_output(210, &vec![b'a'; 4_000], true).unwrap();
    let ev = w.start_segment_with_keyframe(300, &sample_keyframe(None, None, None)).unwrap();
    // Evicting seg1 sweeps the 16 KiB blob immediately and credits it to `total`, so seg2 MUST survive!
    assert_eq!(ev.evicted_segments, 1);
    assert_eq!(ev.swept_blobs, 1);
    assert_eq!(w.segment_count(), 2, "seg2 must not be cascade-evicted once seg1's blob is freed");
    drop(w);

    let r = WalReader::open(&dir, &registry).unwrap();
    assert_eq!(r.segments().len(), 2);
}

#[test]
fn corrupt_record0_payload_len_returns_ok_none_and_overlapping_segments_rejected() {
    let tmp = tempdir().unwrap();
    // 1. Sealed file whose record-0 header claims payload_len > MAX_RECORD_PAYLOAD must return Ok(None).
    let huge_rec0 = tmp.path().join("huge-rec0.apks");
    let mut bytes = Vec::new();
    encode_framed_record(100, &WalRecord::Keyframe(Box::new(sample_keyframe(None, None, None))), &mut bytes).unwrap();
    // Bytes 12..16 are u32 LE payload_len in RECORD_HEADER_LEN ([kind:1][flags:1][reserved:2][at_ms:8][len:4]).
    let huge_len = ((MAX_RECORD_PAYLOAD as u32) + 1).to_le_bytes();
    bytes[12..16].copy_from_slice(&huge_len);
    let end = bytes.len() as u64;
    let sha = hash_segment_records(&bytes);
    bytes.extend_from_slice(
        &SegmentFooter { first_ms: 100, last_ms: 100, records_end_offset: end, record_count: 1, records_sha256: sha }
            .encode(),
    );
    fs::write(&huge_rec0, &bytes).unwrap();
    assert_eq!(read_sealed_header_and_footer(&huge_rec0).unwrap(), None);

    // 2. Two sealed segments where seg2.first_ms < seg1.last_ms must be rejected by WalReader::open.
    let overlap_dir = tmp.path().join("overlap");
    let registry = RecordingRegistry::new();
    let mut w = WalWriter::open_or_create(&overlap_dir, [22; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    w.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    w.record_output(500, b"later", true).unwrap();
    w.seal_active().unwrap();
    drop(w);
    // Forge a second sealed segment with first_ms = 200 (< seg1.last_ms = 500).
    let mut seg2 = Vec::new();
    encode_framed_record(200, &WalRecord::Keyframe(Box::new(sample_keyframe(None, None, None))), &mut seg2).unwrap();
    let end2 = seg2.len() as u64;
    let sha2 = hash_segment_records(&seg2);
    seg2.extend_from_slice(
        &SegmentFooter { first_ms: 200, last_ms: 250, records_end_offset: end2, record_count: 1, records_sha256: sha2 }
            .encode(),
    );
    fs::write(overlap_dir.join(segment_filename(2)), &seg2).unwrap();
    match WalReader::open(&overlap_dir, &registry) {
        Err(WalError::CorruptSegment(_)) => {}
        Err(other) => panic!("expected CorruptSegment, got {other:?}"),
        Ok(_) => panic!("expected CorruptSegment, got Ok"),
    }
}

#[test]
fn wal_handle_coalesces_small_outputs_and_never_drops_seal() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("coalesce-seal");
    let registry = RecordingRegistry::new();
    let w = WalWriter::open_or_create(&dir, [23; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    let h = WalWorker::spawn(w);
    assert!(h.start_segment(100, sample_keyframe(None, None, None)));
    // Unit-check empty-output control-slot release on coalescing + non-output hard segment cap.
    let mut ctrl = 3usize;
    crate::writer::release_empty_output_control_slot(5, &mut ctrl);
    assert_eq!(ctrl, 3, "coalescing onto non-empty output must not decrement control_queued");
    crate::writer::release_empty_output_control_slot(0, &mut ctrl);
    assert_eq!(ctrl, 2, "coalescing onto empty output must release its control slot");

    let sz = GridSize::new(80, 24).unwrap();
    let px = CellPx { width: 8, height: 16 };
    let cap_dir = tmp.path().join("cap-non-output");
    let cap_reg = RecordingRegistry::new();
    let mut cap_w = WalWriter::open_or_create(&cap_dir, [24; 16], 100, WalConfig::default(), cap_reg).unwrap();
    cap_w.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    let cur = cap_w.total_segment_bytes();
    assert!(!cap_w.append_record_capped(101, &WalRecord::Resize { size: sz, cell_px: px }, cur + 40).unwrap());
    assert!(cap_w.append_record_capped(101, &WalRecord::Resize { size: sz, cell_px: px }, cur + 512).unwrap());
    drop(cap_w);

    // Exercise empty ground-notification path and control_queued decrement across > MAX_CONTROL_QUEUE_LEN resizes,
    // including immediate empty->non-empty coalescing without leaking control slots.
    assert!(h.write_output(100, Vec::new(), true));
    assert!(h.write_output(100, b"coalesced".to_vec(), true));
    for i in 0..=MAX_CONTROL_QUEUE_LEN {
        if i % 256 == 0 {
            h.flush_barrier_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(h.write_resize(100 + i as u64, sz, px), "control_queued must decrement as worker drains");
    }
    for i in 0..2_000u64 {
        assert!(h.write_output(100 + i, b"x".to_vec(), true));
    }
    assert!(h.seal(), "seal must never be dropped behind coalesced output items");
    h.flush_barrier_timeout(Duration::from_secs(5)).unwrap();
    h.shutdown().unwrap();

    let r = WalReader::open(&dir, &registry).unwrap();
    assert_eq!(r.segments().len(), 1);
    assert!(r.segments()[0].sealed);
}

#[test]
fn review_mutants_wal_hard_cap_coalesce_control_slot_and_fifo_rejection() {
    use crate::writer::{try_coalesce_output, try_reserve_control_slot};

    // 1. try_reserve_control_slot exact boundaries
    let mut cq = MAX_CONTROL_QUEUE_LEN - 1;
    assert!(!try_reserve_control_slot(true, &mut cq));
    assert_eq!(cq, MAX_CONTROL_QUEUE_LEN - 1);
    assert!(try_reserve_control_slot(false, &mut cq));
    assert_eq!(cq, MAX_CONTROL_QUEUE_LEN);
    assert!(!try_reserve_control_slot(false, &mut cq));
    assert_eq!(cq, MAX_CONTROL_QUEUE_LEN);

    // 2. try_coalesce_output exact boundaries (ground mismatch & max_payload boundary)
    let mut prev = b"ab".to_vec();
    assert!(!try_coalesce_output(&mut prev, true, b"cd", false, 10));
    assert_eq!(prev, b"ab");
    assert!(try_coalesce_output(&mut prev, true, b"cd", true, 4), "exact max_payload must coalesce");
    assert_eq!(prev, b"abcd");
    assert!(!try_coalesce_output(&mut prev, true, b"e", true, 4), "max_payload + 1 must not coalesce");
    assert_eq!(prev, b"abcd");

    // 3. record_output_capped exact hard cap boundary + Gap emission + tight gap-room check
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("hard-cap");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { segment_target_bytes: 1_000_000, budget_bytes: 10_000_000, ..WalConfig::default() };
    let mut w = WalWriter::open_or_create(&dir, [24; 16], 100, cfg, registry.clone()).unwrap();
    w.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    let base_size = w.active_segment_size().unwrap();
    let chunk_len = 16usize;
    let framed_len = (RECORD_HEADER_LEN + chunk_len + crate::RECORD_TRAILER_LEN) as u64;
    // Choose max_file_bytes so hard_data_cap == base_size + framed_len (exact boundary admits chunk!)
    let exact_max = base_size + framed_len + (SEGMENT_FOOTER_LEN as u64) + 1024;
    assert_eq!(w.record_output_capped(110, &vec![b'a'; chunk_len], true, exact_max).unwrap(), RolloverHint::Continue);
    assert_eq!(w.active_segment_size().unwrap(), base_size + framed_len);
    // Next 1-byte chunk exceeds hard_data_cap -> emits Gap and returns NeedKeyframe!
    assert_eq!(w.record_output_capped(120, b"z", true, exact_max).unwrap(), RolloverHint::NeedKeyframe);
    let after_gap = w.active_segment_size().unwrap();
    assert!(after_gap > base_size + framed_len, "Gap record must be appended when room remains");
    assert!(after_gap + (SEGMENT_FOOTER_LEN as u64) <= exact_max);
    // When remaining room before SEGMENT_FOOTER_LEN is smaller than the encoded Gap record, skip appending it
    let gap_len = after_gap - (base_size + framed_len);
    let tight_max = after_gap + (SEGMENT_FOOTER_LEN as u64) + gap_len - 1;
    assert_eq!(w.record_output_capped(130, b"w", true, tight_max).unwrap(), RolloverHint::NeedKeyframe);
    assert_eq!(w.active_segment_size().unwrap(), after_gap);
    // Exact encoded Gap record room accepts it without exceeding max_file_bytes
    let exact_gap_max = after_gap + (SEGMENT_FOOTER_LEN as u64) + gap_len;
    assert_eq!(w.record_output_capped(131, b"w", true, exact_gap_max).unwrap(), RolloverHint::NeedKeyframe);
    assert_eq!(w.active_segment_size().unwrap(), after_gap + gap_len);
    drop(w);

    // 4. Cross-platform lstat classification + Unix FIFO/symlink refusal in safe_fs.
    assert!(matches!(validate_lstat(true, false, "x"), Err(WalError::SymlinkRefused)));
    assert!(matches!(validate_lstat(false, true, "dir"), Err(WalError::InvalidFormat("dir"))));
    assert!(validate_lstat(false, false, "reg").is_ok());

    #[cfg(unix)]
    {
        let fifo_path = tmp.path().join("named.fifo");
        let status = std::process::Command::new("mkfifo").arg(&fifo_path).status().unwrap();
        assert!(status.success());
        let err = read_regular_file_bounded(&fifo_path, 1024, "fifo").unwrap_err();
        assert!(matches!(err, WalError::InvalidFormat(_)), "FIFO must fail with InvalidFormat, got {err:?}");
        assert!(matches!(open_regular_rw(&fifo_path, "fifo"), Err(WalError::InvalidFormat(_))));
        let sym_path = tmp.path().join("sym.file");
        std::os::unix::fs::symlink(&fifo_path, &sym_path).unwrap();
        let sym_err = read_regular_file_bounded(&sym_path, 1024, "sym").unwrap_err();
        assert!(matches!(sym_err, WalError::SymlinkRefused), "symlink must fail with SymlinkRefused, got {sym_err:?}");
        assert!(matches!(open_regular_rw(&sym_path, "sym"), Err(WalError::SymlinkRefused)));
    }
}

#[test]
fn seam4_sha256_footer_tail_validation_safe_rw_wal_blob_and_control_rollover_hint() {
    let tmp = tempdir().unwrap();
    let registry = RecordingRegistry::new();

    // 1. H1: Spoofed CRC-valid footer on tail segment rejected by BOTH WalReader::open and WalWriter::open_or_create.
    let spoof_dir = tmp.path().join("spoof-tail");
    let mut w = WalWriter::open_or_create(&spoof_dir, [31; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    w.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    w.record_output(110, b"seg1", true).unwrap();
    w.start_segment_with_keyframe(200, &sample_keyframe(None, None, None)).unwrap();
    w.record_output(210, b"seg2-live", true).unwrap();
    // Drop WITHOUT sealing seg-2 (simulates crash or hostile PTY bytes at EOF).
    drop(w);

    let seg2_path = spoof_dir.join(segment_filename(2));
    let seg2_clean = fs::read(&seg2_path).unwrap();
    let fake_footer = SegmentFooter {
        first_ms: 200,
        last_ms: 9_999,
        records_end_offset: seg2_clean.len() as u64,
        record_count: 2,
        records_sha256: [0xEE; 32], // wrong SHA-256, valid CRC32C + exact offset!
    };
    let mut seg2_spoofed = seg2_clean.clone();
    seg2_spoofed.extend_from_slice(&fake_footer.encode());
    fs::write(&seg2_path, &seg2_spoofed).unwrap();

    // Unclaimed reader MUST NOT trust O(1) footer fast path on tail seg-2!
    let r = WalReader::open(&spoof_dir, &registry).unwrap();
    assert_eq!(r.segments().len(), 2);
    assert!(r.segments()[0].sealed);
    assert!(!r.segments()[1].sealed, "spoofed tail footer must be rejected");
    assert_eq!(r.segments()[1].last_ms, 210);
    drop(r);

    // Claimed reader MUST NOT trust O(1) footer fast path even on non-tail seg-1!
    let seg1_path = spoof_dir.join(segment_filename(1));
    let seg1_orig = fs::read(&seg1_path).unwrap();
    let seg1_records_end = seg1_orig.len() - SEGMENT_FOOTER_LEN;
    let fake_seg1_footer = SegmentFooter {
        first_ms: 100,
        last_ms: 150,
        records_end_offset: seg1_records_end as u64,
        record_count: 2,
        records_sha256: [0xDD; 32],
    };
    let mut seg1_tampered = seg1_orig[..seg1_records_end].to_vec();
    seg1_tampered.extend_from_slice(&fake_seg1_footer.encode());
    fs::write(&seg1_path, &seg1_tampered).unwrap();
    // Unclaimed reader uses O(1) footer fast path on historical non-tail seg-1 (body SHA-256 verified on read),
    // while ALWAYS scanning tail seg-2!
    let r_unclaimed = WalReader::open(&spoof_dir, &registry).unwrap();
    assert!(r_unclaimed.segments()[0].sealed, "unclaimed non-tail seg-1 uses O(1) footer fast path");
    assert_eq!(r_unclaimed.segments()[0].last_ms, 150);
    assert!(!r_unclaimed.segments()[1].sealed, "tail seg-2 never uses O(1) footer fast path");
    drop(r_unclaimed);
    // Claimed reader MUST NOT trust O(1) footer fast path even on non-tail seg-1!
    {
        let _claim = registry.claim_writer(&spoof_dir).unwrap();
        let r_claimed = WalReader::open(&spoof_dir, &registry).unwrap();
        assert!(!r_claimed.segments()[0].sealed, "claimed WalReader::open must not trust O(1) footer");
        assert_eq!(r_claimed.segments()[0].last_ms, 110);
    }
    // Restore genuine seg-1 before testing WalWriter recovery on spoofed tail seg-2.
    fs::write(&seg1_path, &seg1_orig).unwrap();

    // Finding 1 regression: while a WalReader pins an unsealed/spoofed segment, WalWriter::open_or_create
    // MUST refuse with AlreadyClaimed rather than promoting an unsealed segment into `closed`!
    {
        let pinned_reader = WalReader::open(&spoof_dir, &registry).unwrap();
        assert!(!pinned_reader.segments()[1].sealed);
        assert!(matches!(
            WalWriter::open_or_create(&spoof_dir, [31; 16], 100, WalConfig::default(), registry.clone()),
            Err(WalError::AlreadyClaimed)
        ));
    }

    // Once the reader drops, WalWriter::open_or_create must reject spoofed tail footer, truncate it,
    // reseed incremental SHA-256, append another record, and produce a genuine sealed SHA-256 footer!
    let mut w2 = WalWriter::open_or_create(&spoof_dir, [31; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert_eq!(w2.active_segment_id(), Some(2));
    assert_eq!(w2.record_output(220, b"after-recover", true).unwrap(), RolloverHint::Continue);
    w2.seal_active().unwrap();
    drop(w2);

    let r2 = WalReader::open(&spoof_dir, &registry).unwrap();
    assert!(r2.segments()[1].sealed);
    assert_eq!(r2.segments()[1].last_ms, 220);
    assert_eq!(r2.segments()[1].record_count, 3);
    drop(r2);

    // Reopening a genuinely sealed tail segment keeps it closed (`active_segment_id() == None`).
    let w3 = WalWriter::open_or_create(&spoof_dir, [31; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert_eq!(w3.active_segment_id(), None);
    assert_eq!(w3.segment_count(), 2);
    drop(w3);

    // 2. M2: open_regular_rw refuses directories cross-platform and accepts regular files;
    //    sync_dir refuses regular files and directory symlinks on Unix.
    assert!(matches!(open_regular_rw(tmp.path(), "dir"), Err(WalError::InvalidFormat("dir"))));
    let reg = tmp.path().join("rw.bin");
    fs::write(&reg, b"rw").unwrap();
    assert!(open_regular_rw(&reg, "reg").is_ok());
    #[cfg(unix)]
    {
        assert!(sync_dir(&reg).is_err(), "sync_dir must refuse regular files (O_DIRECTORY)");
        let sym_dir = tmp.path().join("sym-dir");
        std::os::unix::fs::symlink(tmp.path(), &sym_dir).unwrap();
        assert!(sync_dir(&sym_dir).is_err(), "sync_dir must refuse directory symlinks (O_NOFOLLOW)");
    }

    // 3. M3: WalBlob rejects empty payloads, WalHandle::write_blob rejects empty and post-close writes without bricking worker.
    assert!(matches!(WalBlob::new(ImageEncoding::Png, Vec::new()), Err(WalError::InvalidFormat(_))));
    let wb = WalBlob::new(ImageEncoding::ZstdRgba, b"abc".to_vec()).unwrap();
    assert_eq!(wb.encoding(), ImageEncoding::ZstdRgba);
    assert_eq!(wb.bytes(), b"abc");
    assert_eq!(wb.into_parts(), (ImageEncoding::ZstdRgba, b"abc".to_vec()));

    let blob_dir = tmp.path().join("blob-worker");
    let bw = WalWriter::open_or_create(&blob_dir, [32; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    let mut bh = WalWorker::spawn(bw);
    assert!(!bh.write_blob(ImageEncoding::Png, Vec::new()), "empty blob must be rejected at handle");
    assert!(!bh.is_failed(), "empty blob rejection must never brick WalWorker");
    assert!(bh.start_segment(100, sample_keyframe(None, None, None)));
    assert!(bh.write_blob(ImageEncoding::Png, b"valid-blob".to_vec()));
    bh.flush_barrier().unwrap();
    assert!(!bh.is_failed());
    bh.stop_thread();
    assert!(!bh.write_blob(ImageEncoding::Png, b"after-close".to_vec()), "closed handle must reject write_blob");

    // 4. L3: Control records and non-ground gap surface RolloverHint::NeedKeyframe when segment cap is hit.
    let cap_dir = tmp.path().join("cap-control");
    let cfg = WalConfig { max_non_ground_bytes: 10, ..WalConfig::default() };
    let mut cw = WalWriter::open_or_create(&cap_dir, [33; 16], 100, cfg, registry).unwrap();
    cw.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    let cur = cw.active_segment_size().unwrap();
    let tight = cur + (SEGMENT_FOOTER_LEN as u64) + 5; // too small for any framed control record!
    assert_eq!(
        cw.record_control_capped(
            110,
            &WalRecord::Resize { size: GridSize::new(90, 30).unwrap(), cell_px: CellPx::default() },
            tight,
        )
        .unwrap(),
        RolloverHint::NeedKeyframe
    );
    assert_eq!(
        cw.record_resize(111, GridSize::new(90, 30).unwrap(), CellPx::default()).unwrap(),
        RolloverHint::Continue
    );
    assert_eq!(cw.record_marks(112, None, 1).unwrap(), RolloverHint::Continue);
    assert_eq!(cw.record_gap(113, 10, GapReason::DiskError).unwrap(), RolloverHint::Continue);
    // Trigger non-ground drop, then return to ground with a tight max_file_bytes so Gap cannot fit.
    assert_eq!(cw.record_output(114, &[b'x'; 20], false).unwrap(), RolloverHint::Continue);
    let cur2 = cw.active_segment_size().unwrap();
    assert_eq!(
        cw.record_output_capped(115, b"ok", true, cur2 + (SEGMENT_FOOTER_LEN as u64) + 5).unwrap(),
        RolloverHint::NeedKeyframe
    );
}

#[test]
fn pr4_async_close_reaper_panic_guard_backlog_atomic_degrade_and_security() {
    use std::sync::mpsc;
    use std::time::Instant;

    let tmp = tempdir().unwrap();
    let registry = RecordingRegistry::new();

    // 1. H1: Non-blocking `close_async_into` + `WalReaper` bounded cap + background reaping + clean drop.
    assert!(std::ptr::eq(global_reaper(), global_reaper()), "global_reaper must return the same singleton");
    let reaper = WalReaper::with_max_handles(2);
    let reaper_probe = reaper.probe();
    assert!(format!("{reaper:?}").contains("WalReaper"));
    let _default_reaper = WalReaper::default();
    assert_eq!(MAX_REAP_HANDLES, 64);

    let (tx1, rx1) = mpsc::channel();
    let blocked1 = WalHandle::spawn_test_blocked(rx1);
    let t0 = Instant::now();
    reaper.close_async(blocked1);
    assert!(t0.elapsed() < Duration::from_millis(250), "close_async must return without blocking");
    assert!(!reaper.drain_bounded(Duration::from_millis(30)), "drain_bounded must return false while worker is hung");
    assert_eq!(reaper.pending_count(), 1);

    // Enqueue 2 more hung workers -> bounded cap=2 detaches the oldest so pending_count never exceeds 2.
    let (tx2, rx2) = mpsc::channel();
    let (tx3, rx3) = mpsc::channel();
    reaper.close_async(WalHandle::spawn_test_blocked(rx2));
    reaper.close_async(WalHandle::spawn_test_blocked(rx3));
    assert_eq!(reaper.pending_count(), 2);

    // Unblock all 3 workers and confirm the background reaper loop itself reaps them (raw_pending_len == 0).
    let _ = tx1.send(());
    let _ = tx2.send(());
    let _ = tx3.send(());
    let deadline = Instant::now() + Duration::from_secs(2);
    while reaper.raw_pending_len() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(reaper.raw_pending_len(), 0, "background reaper thread must join completed workers");
    assert!(reaper.drain_bounded(Duration::from_millis(100)));
    assert!(!reaper_probe.is_shutting_down());
    assert!(reaper_probe.is_started(), "background reaper stays alive waiting while idle");
    drop(reaper);
    let stop_dl = Instant::now() + Duration::from_secs(2);
    while reaper_probe.is_started() && Instant::now() < stop_dl {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(reaper_probe.is_shutting_down(), "ReaperGuard::drop must signal shutting_down");
    assert!(!reaper_probe.is_started(), "background reaper thread must exit once WalReaper drops");

    // Standalone `WalHandle::close_async` on a blocked worker must return immediately (not join in Drop!).
    let (gtx, grx) = mpsc::channel();
    let t_global = Instant::now();
    WalHandle::spawn_test_blocked(grx).close_async();
    assert!(t_global.elapsed() < Duration::from_millis(250), "standalone close_async must not block");
    let _ = gtx.send(());

    // Standalone `WalHandle::close_async` on a real writer seals/finishes and releases WriterClaim asynchronously.
    let async_dir = tmp.path().join("async-close");
    let writer = WalWriter::open_or_create(&async_dir, [41; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    let handle = WalWorker::spawn(writer);
    assert!(handle.start_segment(100, sample_keyframe(None, None, None)));
    assert!(handle.write_output(110, b"async-sealed".to_vec(), true));
    assert!(handle.seal());
    handle.close_async();
    let release_dl = Instant::now() + Duration::from_secs(3);
    while registry.is_claimed(&async_dir) && Instant::now() < release_dl {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!registry.is_claimed(&async_dir), "async closed worker must exit and release WriterClaim");

    // 2. Panic RAII `DoneGuard`: panicking worker unblocks `flush_barrier_timeout` immediately with WorkerClosed.
    let panicking = WalHandle::spawn_test_panicking();
    let t_panic = Instant::now();
    let panic_res = panicking.flush_barrier_timeout(Duration::from_secs(5));
    assert!(matches!(panic_res, Err(WalError::WorkerClosed)), "expected WorkerClosed, got {panic_res:?}");
    assert!(t_panic.elapsed() < Duration::from_secs(2), "DoneGuard must unblock immediately on panic");

    // 3. `KeyframeV2::estimated_memory_bytes`, `fits_limits` exact boundary, & `degrade_to_limits` ladder.
    let small_kf = sample_keyframe(None, None, None);
    let small_est = small_kf.estimated_memory_bytes();
    let small_raw_len = small_kf.encode_uncompressed().len();
    assert!(small_est > 128);
    assert!(small_kf.fits_limits(small_raw_len, 1 << 20), "exact raw.len() == max_uncompressed must fit");
    assert!(!small_kf.fits_limits(small_raw_len - 1, 1 << 20), "raw.len() > max_uncompressed must not fit");
    let mut unchanged = small_kf.clone();
    assert!(!unchanged.degrade_to_limits(small_raw_len, 1 << 20), "already-fitting keyframe must not degrade");
    assert_eq!(unchanged, small_kf);

    // 3a. History rows above viewport trimmed first (len=3, vp_rows=1 -> (3-1).div_ceil(2)=1 keeps 2 rows,
    //     whereas mutated (3/1).div_ceil(2)=2 would over-drop to 1 row!).
    let mut hist_kf = sample_keyframe(None, None, Some([0x33; 32]));
    hist_kf.visual.size = GridSize::new(80, 1).unwrap();
    hist_kf.visual.primary_rows = (0..3u64)
        .map(|i| PackedRow {
            line: i,
            text: format!("row-{i:02}-{}", "X".repeat(200)),
            styles: Vec::new(),
            runs: Vec::new(),
            flags: 0,
            links: Vec::new(),
            pieces: Vec::new(),
        })
        .collect();
    hist_kf.engine = Some(EngineCheckpoint { engine_abi_hash: [0x33; 32], snapshot_bytes: vec![0xAA; 64] });
    assert!(hist_kf.estimated_memory_bytes() > small_est);
    let mut keep_two_kf = hist_kf.clone();
    keep_two_kf.visual.primary_rows.drain(..1);
    let exact_two_raw = keep_two_kf.encode_uncompressed().len();
    assert!(hist_kf.degrade_to_limits(exact_two_raw, 1 << 20));
    assert_eq!(hist_kf.visual.primary_rows.len(), 2, "must drop exactly 1 history row and keep 2 rows");
    assert!(hist_kf.visual.primary_rows[0].text.starts_with("row-01-"));
    assert!(hist_kf.engine.is_some(), "engine snapshot preserved when trimming history suffices");

    // 3b. Engine checkpoint dropped before sacrificing viewport rows.
    let mut eng_kf = sample_keyframe(None, None, Some([0x44; 32]));
    eng_kf.visual.size = GridSize::new(80, 2).unwrap();
    eng_kf.visual.primary_rows = vec![
        PackedRow {
            line: 0,
            text: "P0".repeat(40),
            styles: Vec::new(),
            runs: Vec::new(),
            flags: 0,
            links: Vec::new(),
            pieces: Vec::new(),
        },
        PackedRow {
            line: 1,
            text: "P1".repeat(40),
            styles: Vec::new(),
            runs: Vec::new(),
            flags: 0,
            links: Vec::new(),
            pieces: Vec::new(),
        },
    ];
    eng_kf.engine = Some(EngineCheckpoint { engine_abi_hash: [0x44; 32], snapshot_bytes: vec![0xBB; 1024] });
    let mut no_eng = eng_kf.clone();
    no_eng.engine = None;
    let no_eng_raw = no_eng.encode_uncompressed().len();
    assert!(eng_kf.degrade_to_limits(no_eng_raw, 1 << 20));
    assert!(eng_kf.engine.is_none());
    assert_eq!(eng_kf.visual.primary_rows.len(), 2, "viewport rows preserved when dropping engine suffices");

    // 3c. Viewport `primary_rows` trimmed when alone over limit, preserving `images` & `commands`.
    let mut pri_kf = sample_keyframe(Some(hash_blob(b"i1")), None, None);
    pri_kf.visual.size = GridSize::new(80, 2).unwrap();
    pri_kf.visual.primary_rows = (0..2u64)
        .map(|i| PackedRow {
            line: i,
            text: format!("P{i}-{}", "Q".repeat(300)),
            styles: Vec::new(),
            runs: Vec::new(),
            flags: 0,
            links: Vec::new(),
            pieces: Vec::new(),
        })
        .collect();
    let mut pri_empty = pri_kf.clone();
    pri_empty.visual.primary_rows.clear();
    let pri_empty_raw = pri_empty.encode_uncompressed().len();
    assert!(pri_kf.degrade_to_limits(pri_empty_raw, 1 << 20));
    assert!(pri_kf.fits_limits(pri_empty_raw, 1 << 20));
    assert!(pri_kf.visual.primary_rows.is_empty());
    assert!(!pri_kf.visual.images.is_empty(), "images preserved when trimming primary_rows suffices");
    assert!(!pri_kf.visual.commands.is_empty(), "commands preserved when trimming primary_rows suffices");

    // 3d. `alt_rows` trimmed when alone over limit, preserving `images` & `commands`.
    let mut alt_kf = sample_keyframe(Some(hash_blob(b"i2")), None, None);
    alt_kf.visual.primary_rows.clear();
    alt_kf.visual.active_alt_screen = true;
    alt_kf.visual.alt_rows = (0..2u64)
        .map(|i| PackedRow {
            line: i,
            text: format!("A{i}-{}", "Z".repeat(300)),
            styles: Vec::new(),
            runs: Vec::new(),
            flags: 0,
            links: Vec::new(),
            pieces: Vec::new(),
        })
        .collect();
    let mut alt_empty = alt_kf.clone();
    alt_empty.visual.alt_rows.clear();
    let alt_empty_raw = alt_empty.encode_uncompressed().len();
    assert!(alt_kf.degrade_to_limits(alt_empty_raw, 1 << 20));
    assert!(alt_kf.fits_limits(alt_empty_raw, 1 << 20));
    assert!(alt_kf.visual.alt_rows.is_empty());
    assert!(!alt_kf.visual.images.is_empty());
    assert!(!alt_kf.visual.commands.is_empty());

    // 3e. `images` trimmed when alone over limit, preserving `commands`.
    let mut img_kf = sample_keyframe(Some(hash_blob(b"i3")), None, None);
    img_kf.visual.primary_rows.clear();
    img_kf.visual.alt_rows.clear();
    let mut img_empty = img_kf.clone();
    img_empty.visual.images.clear();
    let img_empty_raw = img_empty.encode_uncompressed().len();
    assert!(img_kf.degrade_to_limits(img_empty_raw, 1 << 20));
    assert!(img_kf.fits_limits(img_empty_raw, 1 << 20));
    assert!(img_kf.visual.images.is_empty());
    assert!(!img_kf.visual.commands.is_empty(), "commands preserved when trimming images suffices");

    // 3f. `commands` trimmed when alone over limit.
    let mut cmd_kf = img_empty.clone();
    let mut cmd_empty = cmd_kf.clone();
    cmd_empty.visual.commands.clear();
    let cmd_empty_raw = cmd_empty.encode_uncompressed().len();
    assert!(cmd_kf.degrade_to_limits(cmd_empty_raw, 1 << 20));
    assert!(cmd_kf.fits_limits(cmd_empty_raw, 1 << 20));
    assert!(cmd_kf.visual.commands.is_empty());

    // 4. Keyframe backlog charging (`start_segment`) + `write_output_atomic` + `record_backlog_gap`.
    let bl_dir = tmp.path().join("backlog-atomic");
    let bl_cfg = WalConfig { max_backlog_bytes: 200, ..WalConfig::default() };
    let bl_writer = WalWriter::open_or_create(&bl_dir, [42; 16], 100, bl_cfg, registry.clone()).unwrap();
    let bl_handle = WalWorker::spawn(bl_writer);
    assert_eq!(bl_handle.max_backlog_bytes(), 200);
    // Single keyframe > max_backlog_bytes is admitted on an empty queue.
    assert!(bl_handle.start_segment(100, sample_keyframe(None, None, None)));
    bl_handle.flush_barrier().unwrap();

    // Atomic multi-chunk write within cap succeeds completely.
    assert!(bl_handle.write_output_atomic(110, vec![(vec![], false)]));
    assert!(bl_handle.write_output_atomic(
        111,
        vec![(b"\x1b_Gm=1;AAA".to_vec(), false), (b"\x1b\\\x1b_Gm=0;BBB\x1b\\".to_vec(), true)],
    ));
    // Atomic multi-chunk write exceeding cap writes ZERO chunks and emits ONE Gap for the total!
    assert!(!bl_handle.write_output_atomic(112, vec![(vec![b'1'; 120], false), (vec![b'2'; 120], true)],));
    // Explicit `record_backlog_gap` ignores 0 and coalesces non-zero gaps.
    assert!(!bl_handle.record_backlog_gap(113, 0));
    assert!(bl_handle.record_backlog_gap(114, 60));
    assert!(bl_handle.seal());
    bl_handle.shutdown().unwrap();

    let bl_reader = WalReader::open(&bl_dir, &registry).unwrap();
    let bl_slice = bl_reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(bl_slice.tail_records.len(), 3);
    assert_eq!(bl_slice.tail_records[0].record, WalRecord::Output(b"\x1b_Gm=1;AAA".to_vec()));
    assert_eq!(bl_slice.tail_records[1].record, WalRecord::Output(b"\x1b\\\x1b_Gm=0;BBB\x1b\\".to_vec()));
    assert_eq!(
        bl_slice.tail_records[2].record,
        WalRecord::Gap { dropped_bytes: 300, reason: GapReason::BacklogOverflow }
    );
    drop(bl_reader);

    // Keyframe backlog rejection when queue is non-empty, plus control_queued accounting & closed check!
    let (block_tx, block_rx) = mpsc::channel();
    let blocked_kf = WalHandle::spawn_test_blocked(block_rx);
    assert!(blocked_kf.start_segment(100, sample_keyframe(None, None, None)));
    for i in 1..MAX_CONTROL_QUEUE_LEN {
        assert!(
            blocked_kf.write_resize(100 + i as u64, GridSize::new(80, 24).unwrap(), CellPx::default()),
            "control slot {i} must succeed"
        );
    }
    // `control_queued` is now exactly MAX_CONTROL_QUEUE_LEN (proving `start_segment` incremented it by 1).
    assert!(
        !blocked_kf.write_resize(9_999, GridSize::new(80, 24).unwrap(), CellPx::default()),
        "control queue must reject once start_segment + resizes fill MAX_CONTROL_QUEUE_LEN"
    );
    assert!(
        !blocked_kf.start_segment(10_000, sample_keyframe(None, None, None)),
        "start_segment must reject when control queue is full"
    );
    let _ = block_tx.send(());
    blocked_kf.close_async();

    // Separate blocked handle verifying backlog rejection (with control_queued < cap) and closed rejection.
    let (block_tx2, block_rx2) = mpsc::channel();
    let mut blocked_kf2 = WalHandle::spawn_test_blocked(block_rx2);
    assert!(blocked_kf2.write_output(100, vec![b'z'; 900], true));
    assert!(
        !blocked_kf2.start_segment(101, sample_keyframe(None, None, None)),
        "start_segment must reject when queued_bytes + keyframe_cost > max_backlog_bytes"
    );
    let _ = block_tx2.send(());
    blocked_kf2.stop_thread();
    assert!(
        !blocked_kf2.start_segment(102, sample_keyframe(None, None, None)),
        "closed handle must reject start_segment"
    );

    // 5. Out-of-order `WalRecord::Gap` fix in `record_output_capped`:
    //    chunk 0 (16 MiB) exceeds hard_data_cap; trailing 100 bytes WOULD have fit but must be shed too!
    let gap_order_dir = tmp.path().join("gap-order");
    let mut gw =
        WalWriter::open_or_create(&gap_order_dir, [43; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    gw.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    let cur_sz = gw.active_segment_size().unwrap();
    let tight_cap = cur_sz + (SEGMENT_FOOTER_LEN as u64) + 1024 + 500;
    let payload = vec![b'Q'; MAX_RECORD_PAYLOAD + 100];
    assert_eq!(gw.record_output_capped(110, &payload, true, tight_cap).unwrap(), RolloverHint::NeedKeyframe);
    gw.seal_active().unwrap();
    drop(gw);
    let gap_reader = WalReader::open(&gap_order_dir, &registry).unwrap();
    let gap_slice = gap_reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(gap_slice.tail_records.len(), 1, "trailing small chunk must not be written ahead of Gap");
    assert_eq!(
        gap_slice.tail_records[0].record,
        WalRecord::Gap { dropped_bytes: (MAX_RECORD_PAYLOAD + 100) as u64, reason: GapReason::BacklogOverflow }
    );
    drop(gap_reader);

    // 6. `.meta.tmp-*` cleanup under `WriterClaim` + Unix owner/mode + Windows reparse checks.
    let race_dir = tmp.path().join("meta-tmp-claim-race");
    fs::create_dir(&race_dir).unwrap();
    let live_tmp = race_dir.join(".meta.tmp-live-1");
    fs::write(&live_tmp, b"in-flight-meta").unwrap();
    let held_claim = registry.claim_writer(&race_dir).unwrap();
    assert!(matches!(
        WalWriter::open_or_create(&race_dir, [44; 16], 100, WalConfig::default(), registry.clone()),
        Err(WalError::AlreadyClaimed)
    ));
    assert!(live_tmp.exists(), "unclaimed writer must NEVER delete another writer's .meta.tmp-* file");
    drop(held_claim);
    // Once claim is released, stale `.meta.tmp-*` is cleaned cleanly under the new WriterClaim.
    let w_after = WalWriter::open_or_create(&race_dir, [44; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert!(!live_tmp.exists(), "claimed writer cleans stale .meta.tmp-*");
    drop(w_after);

    assert!(validate_unix_dir_owner_mode(1000, 1000, 0o700).is_ok());
    assert!(validate_unix_dir_owner_mode(1000, 1000, 0o755).is_ok());
    assert!(matches!(validate_unix_dir_owner_mode(1001, 1000, 0o700), Err(WalError::ForeignDirectory)));
    assert!(matches!(validate_unix_dir_owner_mode(1000, 1000, 0o775), Err(WalError::ForeignDirectory)));
    assert!(matches!(validate_unix_dir_owner_mode(1000, 1000, 0o757), Err(WalError::ForeignDirectory)));
    assert!(validate_reparse_attributes(0).is_ok());
    assert!(matches!(validate_reparse_attributes(WIN_FILE_ATTRIBUTE_REPARSE_POINT), Err(WalError::SymlinkRefused)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let ww_dir = tmp.path().join("world-writable");
        fs::create_dir(&ww_dir).unwrap();
        fs::set_permissions(&ww_dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            WalWriter::open_or_create(&ww_dir, [45; 16], 100, WalConfig::default(), registry.clone()),
            Err(WalError::ForeignDirectory)
        ));
        assert!(matches!(WalReader::open(&ww_dir, &registry), Err(WalError::ForeignDirectory)));
        fs::set_permissions(&ww_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let bad_blobs_root = tmp.path().join("world-writable-blobs");
        let bw =
            WalWriter::open_or_create(&bad_blobs_root, [46; 16], 100, WalConfig::default(), registry.clone()).unwrap();
        drop(bw);
        let blobs_dir = bad_blobs_root.join("blobs");
        fs::create_dir(&blobs_dir).unwrap();
        fs::set_permissions(&blobs_dir, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(matches!(write_blob(&bad_blobs_root, ImageEncoding::Png, b"png"), Err(WalError::ForeignDirectory)));
        assert!(matches!(sweep_unreferenced(&bad_blobs_root, &HashSet::new()), Err(WalError::ForeignDirectory)));
        fs::set_permissions(&blobs_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
