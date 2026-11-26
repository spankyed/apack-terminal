use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use apack_proto::codec::WireError;
use apack_proto::types::{
    BlobHash, CellPx, CommandOutcome, CommandRecord, CursorShape, CursorState, GridSize, ImageEncoding, ImagePiece,
    ImagePlacement, MarksKey, PackedRow, PackedRun, PackedStyle, PaletteOverrides, Rgb, ViewModes, VouchedExitCode,
};
use tempfile::tempdir;

use crate::blobs::{
    blob_filename, check_blob_size, hash_blob, parse_blob_filename, read_blob, sweep_unreferenced, write_blob,
};
use crate::crc32c::{crc32c, crc32c_append};
use crate::directory::{
    META_FILENAME, RecordingRegistry, discover_segments, parse_segment_id, prepare_recording_dir, read_meta,
    segment_filename,
};
use crate::format::{
    EngineCheckpoint, FORMAT_VERSION, GapReason, KeyframeV2, MAX_RECORD_PAYLOAD, META_LEN, META_MAGIC,
    RECORD_FLAG_ZLIB, RECORD_HEADER_LEN, RecordingMeta, SEGMENT_FOOTER_LEN, SEGMENT_FOOTER_MAGIC, SegmentFooter,
    VisualKeyframe, WalRecord, check_payload_limit, decode_one_record, encode_framed_record, scan_segment,
};
use crate::reader::{WalReader, index_segment_allow_vanished, try_read_footer};
use crate::writer::{RolloverHint, WalConfig, WalWorker, WalWriter, should_worker_wait, try_reserve_backlog};
use crate::{
    DEFAULT_BUDGET_BYTES, DEFAULT_MAX_BACKLOG_BYTES, DEFAULT_MAX_NON_GROUND_BYTES, DEFAULT_SEGMENT_TARGET_BYTES,
    MAX_BLOB_FILE_BYTES, MIN_SEGMENT_TARGET_BYTES, WalError,
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
        .map(|blob| ImagePlacement { blob, line: 42, col: 0, pixel_size: [64, 32], cell_span: [4, 2], z_index: 0 })
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

    // Wrong length
    assert!(matches!(RecordingMeta::decode(&encoded[..META_LEN - 1]), Err(WalError::InvalidFormat(_))));

    // Wrong magic
    let mut bad_magic = encoded;
    bad_magic[0] ^= 0x01;
    assert!(matches!(RecordingMeta::decode(&bad_magic), Err(WalError::InvalidFormat(_))));

    // Bit flip in body caught by CRC32C
    let mut bad_crc = encoded;
    bad_crc[15] ^= 0x80;
    assert!(matches!(RecordingMeta::decode(&bad_crc), Err(WalError::ChecksumMismatch(_))));

    // Unsupported version (re-signed with valid CRC)
    let mut bad_ver = encoded;
    bad_ver[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    let crc = crc32c(&bad_ver[0..36]);
    bad_ver[36..40].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(RecordingMeta::decode(&bad_ver), Err(WalError::InvalidFormat(_))));

    // Non-zero reserved flags (re-signed with valid CRC)
    let mut bad_flags = encoded;
    bad_flags[10..12].copy_from_slice(&1u16.to_le_bytes());
    let crc = crc32c(&bad_flags[0..36]);
    bad_flags[36..40].copy_from_slice(&crc.to_le_bytes());
    assert!(matches!(RecordingMeta::decode(&bad_flags), Err(WalError::InvalidFormat(_))));
}

#[test]
fn segment_footer_roundtrip_and_validation() {
    let footer = SegmentFooter { first_ms: 100, last_ms: 500, record_count: 12 };
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

    // Inverted timestamps rejected even with valid CRC
    let inv = SegmentFooter { first_ms: 500, last_ms: 499, record_count: 1 }.encode();
    assert!(matches!(SegmentFooter::decode(&inv), Err(WalError::CorruptSegment(_))));

    // Zero record count rejected even with valid CRC
    let zero = SegmentFooter { first_ms: 100, last_ms: 100, record_count: 0 }.encode();
    assert!(matches!(SegmentFooter::decode(&zero), Err(WalError::CorruptSegment(_))));
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
    let footer = SegmentFooter { first_ms: 1000, last_ms: 1035, record_count: records.len() as u32 };
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
fn decode_record_rejects_corrupt_headers_flags_lengths_and_crc() {
    let mut buf = Vec::new();
    encode_framed_record(10, &WalRecord::Output(b"abc".to_vec()), &mut buf).unwrap();

    // Too short
    assert!(matches!(decode_one_record(&buf[..RECORD_HEADER_LEN]), Err(WalError::InvalidFormat(_))));
    // Truncated payload
    assert!(matches!(decode_one_record(&buf[..buf.len() - 1]), Err(WalError::InvalidFormat(_))));
    // Corrupted CRC
    let mut bad_crc = buf.clone();
    *bad_crc.last_mut().unwrap() ^= 0xFF;
    assert!(matches!(decode_one_record(&bad_crc), Err(WalError::ChecksumMismatch(_))));

    // Helper to build an arbitrarily framed header + payload with valid CRC.
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

    // Oversized payload_len rejected before indexing
    let mut oversized = [0u8; RECORD_HEADER_LEN + 4];
    oversized[0] = 2;
    oversized[12..16].copy_from_slice(&((MAX_RECORD_PAYLOAD as u32) + 1).to_le_bytes());
    assert!(matches!(decode_one_record(&oversized), Err(WalError::LimitExceeded(_))));
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

    // Empty blob rejected
    assert!(matches!(write_blob(dir, ImageEncoding::Png, b""), Err(WalError::InvalidFormat(_))));

    // Filename roundtrip & invalid parses
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
fn directory_guards_refuse_foreign_dirs_symlinks_and_double_writers() {
    let tmp = tempdir().unwrap();

    // Non-empty directory without meta.apkm refused!
    let foreign = tmp.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("important.txt"), b"do not touch").unwrap();
    assert!(matches!(prepare_recording_dir(&foreign), Err(WalError::ForeignDirectory)));

    // Regular file at recording path refused!
    let reg_file = tmp.path().join("not-a-dir");
    fs::write(&reg_file, b"file").unwrap();
    assert!(matches!(prepare_recording_dir(&reg_file), Err(WalError::ForeignDirectory)));

    // Segment filename parsing
    assert_eq!(segment_filename(1), "seg-000001.apks");
    assert_eq!(parse_segment_id("seg-000001.apks"), Some(1));
    assert_eq!(parse_segment_id("seg-1234567.apks"), Some(1_234_567));
    assert_eq!(parse_segment_id("seg-000000.apks"), None);
    assert_eq!(parse_segment_id("seg-00001.apks"), None);
    assert_eq!(parse_segment_id("seg-00000a.apks"), None);
    assert_eq!(parse_segment_id("other-000001.apks"), None);

    // Single-writer claim exclusivity + Drop release
    let rec_dir = tmp.path().join("rec");
    let registry = RecordingRegistry::new();
    let writer = WalWriter::open_or_create(&rec_dir, [1; 16], 500, WalConfig::default(), registry.clone()).unwrap();
    assert!(registry.is_claimed(&rec_dir));
    assert!(matches!(
        WalWriter::open_or_create(&rec_dir, [1; 16], 500, WalConfig::default(), registry.clone()),
        Err(WalError::AlreadyClaimed)
    ));
    drop(writer);
    assert!(!registry.is_claimed(&rec_dir));

    // Symlink refusal on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link_dir = tmp.path().join("sym-rec");
        symlink(&rec_dir, &link_dir).unwrap();
        assert!(matches!(prepare_recording_dir(&link_dir), Err(WalError::SymlinkRefused)));

        let meta_link_dir = tmp.path().join("meta-link");
        fs::create_dir(&meta_link_dir).unwrap();
        symlink(rec_dir.join(META_FILENAME), meta_link_dir.join(META_FILENAME)).unwrap();
        assert!(matches!(read_meta(&meta_link_dir), Err(WalError::SymlinkRefused)));

        let seg_link_dir = tmp.path().join("seg-link");
        fs::create_dir(&seg_link_dir).unwrap();
        symlink(&reg_file, seg_link_dir.join("seg-000001.apks")).unwrap();
        assert!(matches!(discover_segments(&seg_link_dir), Err(WalError::SymlinkRefused)));
    }
}

#[test]
fn torn_tail_crash_recovery_at_every_byte_offset() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("crash-rec");
    let registry = RecordingRegistry::new();
    let kf = sample_keyframe(None, None, None);

    // Produce a clean 2-record unsealed segment and capture its exact raw bytes.
    let mut writer = WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    writer.start_segment_with_keyframe(100, &kf).unwrap();
    writer.record_output(110, b"first-clean-output", true).unwrap();
    writer.flush().unwrap();
    let clean_len = fs::metadata(rec_dir.join("seg-000001.apks")).unwrap().len() as usize;

    writer.record_output(120, b"torn-second-record-payload", true).unwrap();
    writer.flush().unwrap();
    let full_bytes = fs::read(rec_dir.join("seg-000001.apks")).unwrap();
    drop(writer);
    assert!(full_bytes.len() > clean_len);

    // Truncate inside the second record at EVERY single byte offset and confirm open_or_create
    // truncates cleanly back to `clean_len` and preserves record 0 + record 1!
    for cut in (clean_len + 1)..full_bytes.len() {
        fs::write(rec_dir.join("seg-000001.apks"), &full_bytes[..cut]).unwrap();
        let mut resumed =
            WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
        assert_eq!(fs::metadata(rec_dir.join("seg-000001.apks")).unwrap().len() as usize, clean_len);
        resumed.record_output(130, b"recovered-ok", true).unwrap();
        resumed.flush().unwrap();
        drop(resumed);

        let reader = WalReader::open(&rec_dir, &registry).unwrap();
        let latest = reader.latest_segment_records().unwrap().unwrap();
        assert_eq!(latest.tail_records.len(), 2);
        assert_eq!(latest.tail_records[0].record, WalRecord::Output(b"first-clean-output".to_vec()));
        assert_eq!(latest.tail_records[1].record, WalRecord::Output(b"recovered-ok".to_vec()));
    }

    // Corrupting record 0 entirely causes recovery to remove the unusable segment cleanly.
    fs::write(rec_dir.join("seg-000001.apks"), &full_bytes[..10]).unwrap();
    let resumed = WalWriter::open_or_create(&rec_dir, [2; 16], 100, WalConfig::default(), registry.clone()).unwrap();
    assert_eq!(resumed.active_segment_id(), None);
    assert!(!rec_dir.join("seg-000001.apks").exists());
}

#[test]
fn rollover_budget_eviction_pin_protection_and_blob_gc() {
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

    // Segment 1 (referencing blob1)
    writer.start_segment_with_keyframe(1_000, &sample_keyframe(Some(blob1), None, None)).unwrap();
    let hint = writer.record_output(1_100, &vec![b'A'; 4_500], true).unwrap();
    assert_eq!(hint, RolloverHint::NeedKeyframe);

    // Segment 2 (referencing blob2) — total ~9.5 KiB > 9 KiB -> would evict seg-1 if unpinned!
    // Pin seg-1 by opening a WalReader BEFORE rolling over!
    writer.flush().unwrap();
    let reader_pinning_seg1 = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader_pinning_seg1.segments().len(), 1);
    assert_eq!(reader_pinning_seg1.segments()[0].id, 1);

    let ev1 = writer.start_segment_with_keyframe(2_000, &sample_keyframe(Some(blob2), None, None)).unwrap();
    assert_eq!(ev1.evicted_segments, 0);
    assert!(ev1.skipped_pinned >= 1 || writer.total_segment_bytes() <= cfg.budget_bytes);
    writer.record_output(2_100, &vec![b'B'; 4_500], true).unwrap();

    // Segment 3 (referencing blob3) — seg-1 is STILL pinned, so seg-2 gets evicted instead!
    let ev2 = writer.start_segment_with_keyframe(3_000, &sample_keyframe(Some(blob3), None, None)).unwrap();
    assert!(rec_dir.join("seg-000001.apks").exists(), "pinned seg-1 must not be unlinked");
    assert_eq!(ev2.skipped_pinned, 1);
    assert_eq!(ev2.evicted_segments, 1);
    assert!(!rec_dir.join("seg-000002.apks").exists(), "unpinned seg-2 evicted");
    assert!(!rec_dir.join("blobs").join(blob_filename(blob2, ImageEncoding::Png)).exists(), "blob2 swept");
    assert!(rec_dir.join("blobs").join(blob_filename(blob1, ImageEncoding::Png)).exists(), "blob1 kept for seg-1");
    assert!(rec_dir.join("blobs").join(blob_filename(blob3, ImageEncoding::Png)).exists(), "blob3 kept for seg-3");

    // Drop reader -> unpins seg-1 -> next eviction cleans seg-1 and sweeps blob1!
    drop(reader_pinning_seg1);
    writer.record_output(3_100, &vec![b'C'; 4_500], true).unwrap();
    let ev3 = writer.evict_to_budget().unwrap();
    assert_eq!(ev3.evicted_segments, 1);
    assert_eq!(ev3.swept_blobs, 1);
    assert!(!rec_dir.join("seg-000001.apks").exists());
    assert!(!rec_dir.join("blobs").join(blob_filename(blob1, ImageEncoding::Png)).exists());
    assert!(rec_dir.join("blobs").join(blob_filename(blob3, ImageEncoding::Png)).exists());
}

#[test]
fn seek_binary_search_reads_single_segment_and_respects_monotonic_timestamps() {
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

    // Segment 1: t = 1000..1200 (with out-of-order clock rollback clamped to 1100)
    writer.start_segment_with_keyframe(1_000, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(1_100, b"s1-a", true).unwrap();
    writer.record_output(900, b"s1-rollback-clamped", true).unwrap(); // clamped to 1100!
    writer.record_output(1_200, b"s1-b", true).unwrap();

    // Segment 2: t = 2000..2200
    writer.start_segment_with_keyframe(2_000, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(2_100, b"s2-a", true).unwrap();
    writer.record_resize(2_150, GridSize::new(100, 30).unwrap(), CellPx { width: 8, height: 16 }).unwrap();
    writer.record_output(2_200, b"s2-b", true).unwrap();

    // Segment 3 (unsealed active tail): t = 3000..3100
    writer.start_segment_with_keyframe(3_000, &sample_keyframe(None, None, None)).unwrap();
    writer.record_output(3_100, b"s3-active", true).unwrap();
    writer.flush().unwrap();

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader.segments().len(), 3);
    assert!(reader.segments()[0].sealed);
    assert!(reader.segments()[1].sealed);
    assert!(!reader.segments()[2].sealed);
    assert_eq!(reader.span(), Some((1_000, 3_100)));

    // Seek before start -> segment 1 keyframe only
    let before = reader.records_for_moment(500).unwrap().unwrap();
    assert_eq!(before.segment_id, 1);
    assert!(before.tail_records.is_empty());

    // Seek at 1100 -> includes rollback-clamped record, excludes 1200
    let mid1 = reader.records_for_moment(1_100).unwrap().unwrap();
    assert_eq!(mid1.segment_id, 1);
    assert_eq!(mid1.tail_records.len(), 2);
    assert_eq!(mid1.tail_records[1].at_ms, 1_100);

    // Seek inside segment 2 at 2150 -> s2-a + resize, excludes s2-b
    let mid2 = reader.records_for_moment(2_150).unwrap().unwrap();
    assert_eq!(mid2.segment_id, 2);
    assert_eq!(mid2.tail_records.len(), 2);

    // Seek at 9999 -> segment 3 with active record
    let end = reader.records_for_moment(9_999).unwrap().unwrap();
    assert_eq!(end.segment_id, 3);
    assert_eq!(end.tail_records.len(), 1);

    // Restore (`latest_segment_records`) returns segment 3
    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(latest.segment_id, 3);
    assert_eq!(latest.tail_records.len(), 1);
}

#[test]
fn non_ground_run_cap_emits_explicit_gap_marker() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("nonground-rec");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { max_non_ground_bytes: 100, ..WalConfig::default() };

    let mut writer = WalWriter::open_or_create(&rec_dir, [5; 16], 100, cfg, registry.clone()).unwrap();
    // Calling record_output before starting a segment fails clearly.
    assert!(matches!(writer.record_output(100, b"hi", true), Err(WalError::CorruptSegment(_))));

    writer.start_segment_with_keyframe(100, &sample_keyframe(None, None, None)).unwrap();
    // 80 bytes non-ground: still under 100 cap -> written
    writer.record_output(110, &[b'x'; 80], false).unwrap();
    // 50 bytes non-ground: pushes run to 130 > 100 -> paused/dropped (50 bytes)
    writer.record_output(120, &[b'y'; 50], false).unwrap();
    // 70 more bytes non-ground: dropped (total 120 bytes)
    writer.record_output(130, &[b'z'; 70], false).unwrap();
    // Ground resumes -> flushes Gap(120, NonGroundCap) then writes "back"
    writer.record_output(140, b"back", true).unwrap();
    writer.seal_active().unwrap();
    drop(writer);

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert_eq!(latest.tail_records.len(), 3);
    assert_eq!(latest.tail_records[0].record, WalRecord::Output(vec![b'x'; 80]));
    assert_eq!(latest.tail_records[1].record, WalRecord::Gap { dropped_bytes: 120, reason: GapReason::NonGroundCap });
    assert_eq!(latest.tail_records[2].record, WalRecord::Output(b"back".to_vec()));
}

#[test]
fn async_worker_writes_flushes_and_sheds_backlog_with_gap() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("worker-rec");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { max_backlog_bytes: 128, ..WalConfig::default() };

    let writer = WalWriter::open_or_create(&rec_dir, [6; 16], 1_000, cfg, registry.clone()).unwrap();
    let handle = WalWorker::spawn(writer);
    assert!(!handle.is_failed());

    let blob_data = b"worker-png-bytes".to_vec();
    let blob_hash = hash_blob(&blob_data);
    assert!(handle.write_blob(ImageEncoding::Png, blob_data.clone()));
    assert!(handle.start_segment(1_000, sample_keyframe(Some(blob_hash), None, None)));
    assert!(handle.write_output(1_010, b"hello-worker".to_vec(), true));
    assert!(handle.write_resize(1_020, GridSize::new(90, 30).unwrap(), CellPx { width: 8, height: 16 }));
    assert!(handle.write_marks(1_030, Some(sample_marks_key()), 4));

    // Oversized output > max_backlog_bytes (128) is shed non-blockingly and emits Gap(BacklogOverflow)
    // before the next accepted command!
    assert!(!handle.write_output(1_040, vec![b'!'; 512], true));
    assert!(!handle.write_blob(ImageEncoding::Png, vec![b'!'; 512]));
    assert!(handle.write_output(1_050, b"after-shed".to_vec(), true));
    assert!(handle.seal());

    handle.flush_barrier().unwrap();
    handle.shutdown().unwrap();

    let reader = WalReader::open(&rec_dir, &registry).unwrap();
    assert_eq!(reader.read_blob(blob_hash, ImageEncoding::Png).unwrap(), blob_data);
    let latest = reader.latest_segment_records().unwrap().unwrap();
    assert!(
        latest
            .tail_records
            .iter()
            .any(|r| matches!(r.record, WalRecord::Gap { dropped_bytes: 512, reason: GapReason::BacklogOverflow }))
    );
    assert!(latest.tail_records.iter().any(|r| r.record == WalRecord::Output(b"after-shed".to_vec())));
}

#[test]
fn corrupted_footer_falls_back_cleanly_in_scan_segment() {
    let mut stream = Vec::new();
    let kf = sample_keyframe(None, None, None);
    encode_framed_record(100, &WalRecord::Keyframe(Box::new(kf)), &mut stream).unwrap();
    let valid_end = stream.len();

    // Footer claiming wrong record_count (99 instead of 1) -> treated as torn tail.
    let bad_count_footer = SegmentFooter { first_ms: 100, last_ms: 100, record_count: 99 }.encode();
    stream.extend_from_slice(&bad_count_footer);
    let scan = scan_segment(&stream);
    assert!(scan.had_torn_tail);
    assert_eq!(scan.footer, None);
    assert_eq!(scan.valid_records_end, valid_end);
    assert_eq!(scan.records.len(), 1);
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

    // Append garbage after the sealed footer so try_read_footer returns None and fallback scan runs.
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
    ];
    for err in errors {
        assert!(!err.to_string().is_empty());
    }

    let reg = RecordingRegistry::new();
    assert!(format!("{reg:?}").contains("RecordingRegistry"));
}

#[test]
fn boundary_helpers_and_footer_edge_cases() {
    // Blob size helper exact boundary
    assert!(check_blob_size(99, 100).is_ok());
    assert!(check_blob_size(100, 100).is_ok());
    assert!(matches!(check_blob_size(101, 100), Err(WalError::LimitExceeded(_))));

    // Payload limit helper exact boundary
    assert!(check_payload_limit(99, 100, "x").is_ok());
    assert!(check_payload_limit(100, 100, "x").is_ok());
    assert!(matches!(check_payload_limit(101, 100, "x"), Err(WalError::LimitExceeded(_))));

    // Single-record footer with equal first_ms == last_ms is valid!
    let same_ts = SegmentFooter { first_ms: 42, last_ms: 42, record_count: 1 };
    let encoded = same_ts.encode();
    assert_eq!(SegmentFooter::decode(&encoded).unwrap(), same_ts);

    // try_read_footer exact boundary: 31 bytes -> None, 32 bytes -> Some
    let tmp = tempdir().unwrap();
    let short_path = tmp.path().join("short.apks");
    fs::write(&short_path, &encoded[..SEGMENT_FOOTER_LEN - 1]).unwrap();
    assert_eq!(try_read_footer(&short_path).unwrap(), None);

    let exact_path = tmp.path().join("exact.apks");
    fs::write(&exact_path, encoded).unwrap();
    assert_eq!(try_read_footer(&exact_path).unwrap(), Some(same_ts));

    // index_segment_allow_vanished: missing file -> Ok(None), directory -> Err(Io)
    assert_eq!(index_segment_allow_vanished(1, tmp.path().join("missing.apks")).unwrap(), None);
    let sub_dir = tmp.path().join("a-dir");
    fs::create_dir(&sub_dir).unwrap();
    assert!(matches!(index_segment_allow_vanished(2, sub_dir), Err(WalError::Io(_))));
}

#[test]
fn directory_and_blobs_io_error_paths_and_multi_reader_pin_refcount() {
    let tmp = tempdir().unwrap();

    // sweep_unreferenced when blobs/ does not exist -> Ok(0)
    assert_eq!(sweep_unreferenced(tmp.path(), &HashSet::new()).unwrap(), 0);

    // When `blobs` is a regular file: write_blob -> InvalidFormat, sweep_unreferenced -> Io error
    let file_blobs_root = tmp.path().join("file-blobs");
    fs::create_dir(&file_blobs_root).unwrap();
    fs::write(file_blobs_root.join("blobs"), b"not-a-dir").unwrap();
    assert!(matches!(write_blob(&file_blobs_root, ImageEncoding::Png, b"x"), Err(WalError::InvalidFormat(_))));
    assert!(matches!(sweep_unreferenced(&file_blobs_root, &HashSet::new()), Err(WalError::Io(_))));

    // When parent is a regular file: symlink_metadata returns non-NotFound -> Err(Io)
    let blocking_file = tmp.path().join("blocker");
    fs::write(&blocking_file, b"x").unwrap();
    assert!(matches!(prepare_recording_dir(&blocking_file.join("child")), Err(WalError::Io(_))));
    assert!(matches!(write_blob(&blocking_file, ImageEncoding::Png, b"x"), Err(WalError::Io(_))));

    // Two concurrent readers pin the same segment: dropping one keeps pin active until second drops!
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
    writer.flush().unwrap();

    let canonical = fs::canonicalize(&rec_dir).unwrap();
    let r1 = WalReader::open(&rec_dir, &registry).unwrap();
    let r2 = WalReader::open(&rec_dir, &registry).unwrap();
    assert!(registry.is_pinned(&canonical, 1));
    drop(r1);
    assert!(registry.is_pinned(&canonical, 1), "second reader still pins seg 1");
    drop(r2);
    assert!(!registry.is_pinned(&canonical, 1), "pin released once all readers drop");

    // Verify record_marks persisted in segment 1
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

    // Writing >= segment_target_bytes while NOT at ground (`vt_at_ground = false`) must NOT request
    // rollover AND exact `max_non_ground_bytes` (5000) must NOT drop bytes!
    let hint = writer.record_output(110, &vec![b'a'; 5_000], false).unwrap();
    assert_eq!(hint, RolloverHint::Continue);
    // One more non-ground byte triggers drop (1 byte).
    let hint2 = writer.record_output(115, b"z", false).unwrap();
    assert_eq!(hint2, RolloverHint::Continue);
    // Returning to ground flushes Gap(1) and triggers NeedKeyframe since size >= 4096.
    let hint3 = writer.record_output(120, b"ok", true).unwrap();
    assert_eq!(hint3, RolloverHint::NeedKeyframe);
    writer.seal_active().unwrap();

    let exact_total = writer.total_segment_bytes();
    drop(writer);

    // Reopen with budget_bytes == exact_total: evict_to_budget must NOT evict seg 1!
    let exact_cfg = WalConfig { budget_bytes: exact_total, ..cfg };
    let mut w2 = WalWriter::open_or_create(&rec_dir, [10; 16], 100, exact_cfg, registry.clone()).unwrap();
    let ev = w2.evict_to_budget().unwrap();
    assert_eq!(ev.evicted_segments, 0);
    assert!(rec_dir.join("seg-000001.apks").exists());

    // Delete seg-000001 externally and force budget smaller: NotFound during eviction is ignored cleanly!
    fs::remove_file(rec_dir.join("seg-000001.apks")).unwrap();
    w2.start_segment_with_keyframe(300, &sample_keyframe(None, None, None)).unwrap();
    assert_eq!(w2.segment_count(), 1);
}

#[test]
fn should_worker_wait_truth_table() {
    for queue_empty in [false, true] {
        for has_active in [false, true] {
            for pending_drop in [0u64, 1, 50] {
                for closed in [false, true] {
                    let expected = queue_empty && !(has_active && pending_drop > 0) && !closed;
                    assert_eq!(
                        should_worker_wait(queue_empty, has_active, pending_drop, closed),
                        expected,
                        "mismatch for ({queue_empty}, {has_active}, {pending_drop}, {closed})"
                    );
                }
            }
        }
    }
}

#[test]
fn worker_backlog_accounting_failure_propagation_and_drop_cleanup() {
    let tmp = tempdir().unwrap();

    // 1. Backlog exact boundary & cumulative queued_bytes accounting + verifying resize/marks/seal persisted
    let rec1 = tmp.path().join("worker-bounds");
    let registry = RecordingRegistry::new();
    let cfg = WalConfig { max_backlog_bytes: 100, ..WalConfig::default() };
    let writer1 = WalWriter::open_or_create(&rec1, [11; 16], 100, cfg, registry.clone()).unwrap();
    let h1 = WalWorker::spawn(writer1);
    assert!(h1.start_segment(100, sample_keyframe(None, None, None)));
    // Exact boundary: 100 bytes succeeds
    assert!(h1.write_output(110, vec![b'x'; 100], true));
    h1.flush_barrier_timeout(Duration::from_secs(2)).unwrap();
    assert!(h1.write_blob(ImageEncoding::Png, vec![b'p'; 100]));
    assert!(h1.write_resize(120, GridSize::new(100, 40).unwrap(), CellPx { width: 8, height: 16 }));
    assert!(h1.write_marks(130, Some(sample_marks_key()), 5));
    assert!(h1.seal());
    h1.flush_barrier_timeout(Duration::from_secs(2)).unwrap();

    // Drop h1 directly (without shutdown) -> releases writer claim!
    drop(h1);
    assert!(!registry.is_claimed(&rec1), "Drop for WalHandle must stop thread and release WriterClaim");

    let reader = WalReader::open(&rec1, &registry).unwrap();
    assert!(reader.segments()[0].sealed, "seal() must seal the active segment");
    let slice = reader.latest_segment_records().unwrap().unwrap();
    assert!(!slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Gap { .. })));
    assert!(slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Resize { .. })));
    assert!(slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Marks { last_seq: 5, .. })));

    // 2. Worker sticky failure propagation: writing output with no active segment triggers CorruptSegment!
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

    // 3. Deterministic backlog reservation boundary check (exact limit, cumulative, overflow)
    let mut queued = 0usize;
    assert!(try_reserve_backlog(&mut queued, 60, 100));
    assert_eq!(queued, 60);
    assert!(try_reserve_backlog(&mut queued, 40, 100));
    assert_eq!(queued, 100);
    assert!(!try_reserve_backlog(&mut queued, 1, 100));
    assert_eq!(queued, 100);
}
