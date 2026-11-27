//! Binary on-disk format for `meta.apkm`, framed WAL records, dual-layer `KeyframeV2`, and
//! 72-byte SHA-256-committed sealed segment footers.
//!
//! All framing is little-endian, bounds-checked, and protected by trailing Castagnoli CRC32C
//! checksums, with sealed segment footers committing the SHA-256 digest of all preceding records.

use std::collections::BTreeSet;

use apack_proto::MAX_COMMANDS;
use apack_proto::codec::{Reader, WireError, Writer};
use apack_proto::types::{
    BlobHash, CellPx, CommandRecord, CursorState, GridSize, ImagePlacement, MarksKey, PackedRow, PaletteOverrides,
    ViewModes,
};
use miniz_oxide::deflate::compress_to_vec_zlib;
use miniz_oxide::inflate::decompress_to_vec_zlib_with_limit;
use sha2::{Digest, Sha256};

use crate::WalError;
use crate::crc32c::{crc32c, crc32c_append};

/// Magic bytes at offset `0` of `meta.apkm`.
pub const META_MAGIC: [u8; 8] = *b"APKREC2\0";

/// Current on-disk recording format version.
pub const FORMAT_VERSION: u16 = 2;

/// Exact byte length of `meta.apkm` (`magic[8] + ver[2] + flags[2] + created_ms[8] + uuid[16] + crc32c[4]`).
pub const META_LEN: usize = 40;

/// Fixed per-record header length (`kind[1] + flags[1] + reserved[2] + at_ms[8] + payload_len[4]`).
pub const RECORD_HEADER_LEN: usize = 16;

/// Trailing CRC32C length appended to every framed record.
pub const RECORD_TRAILER_LEN: usize = 4;

/// Minimum encoded byte length of a single valid framed record (`header + trailer`).
pub const MIN_RECORD_LEN: usize = RECORD_HEADER_LEN + RECORD_TRAILER_LEN;

/// Magic bytes at offset `0` of a sealed segment footer (byte `0` is `b'A'`, disjoint from record kinds `1..=5`).
pub const SEGMENT_FOOTER_MAGIC: [u8; 8] = *b"APKFTR2\0";

/// Exact byte length of a sealed segment footer
/// (`magic[8] + first_ms[8] + last_ms[8] + records_end_offset[8] + record_count[4] + records_sha256[32] + crc32c[4]`).
pub const SEGMENT_FOOTER_LEN: usize = 72;

/// Maximum on-disk payload bytes of a single WAL record (`16 MiB`).
pub const MAX_RECORD_PAYLOAD: usize = 16 << 20;

/// Maximum uncompressed bytes allowed when inflating a zlib-compressed `KeyframeV2` (`32 MiB`).
pub const MAX_KEYFRAME_UNCOMPRESSED: usize = 32 << 20;

/// Maximum history rows stored in a `VisualKeyframe` above the active viewport (`1_000`).
pub const MAX_KEYFRAME_HISTORY_ROWS: usize = 1_000;

/// Maximum total rows in `VisualKeyframe::primary_rows` (`MAX_GRID_ROWS + MAX_KEYFRAME_HISTORY_ROWS`).
pub const MAX_VISUAL_PRIMARY_ROWS: usize = 2_000;

/// Maximum rows in `VisualKeyframe::alt_rows`.
pub const MAX_VISUAL_ALT_ROWS: usize = 1_000;

/// Maximum image placements retained in a `VisualKeyframe`.
pub const MAX_VISUAL_IMAGES: usize = 4_096;

/// Maximum size of an optional `libghostty-vt` binary checkpoint (`8 MiB`).
pub const MAX_ENGINE_SNAPSHOT_BYTES: usize = 8 << 20;

/// Record flag bit indicating payload is zlib-compressed.
pub const RECORD_FLAG_ZLIB: u8 = 0x01;

const KIND_KEYFRAME_V2: u8 = 1;
const KIND_OUTPUT: u8 = 2;
const KIND_RESIZE: u8 = 3;
const KIND_MARKS: u8 = 4;
const KIND_GAP: u8 = 5;

const ZLIB_COMPRESSION_LEVEL: u8 = 4;

/// Recording-level metadata persisted in `meta.apkm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingMeta {
    /// On-disk format version (always [`FORMAT_VERSION`]).
    pub format_version: u16,
    /// Wall-clock creation timestamp (ms since Unix epoch).
    pub created_ms: u64,
    /// Opaque 16-byte session UUID.
    pub session_uuid: [u8; 16],
}

impl RecordingMeta {
    /// Constructs a v2 metadata header.
    pub const fn new(created_ms: u64, session_uuid: [u8; 16]) -> Self {
        Self { format_version: FORMAT_VERSION, created_ms, session_uuid }
    }

    /// Encodes this metadata header into its exact 40-byte CRC32C-protected representation.
    pub fn encode(&self) -> [u8; META_LEN] {
        let mut out = [0u8; META_LEN];
        out[0..8].copy_from_slice(&META_MAGIC);
        out[8..10].copy_from_slice(&self.format_version.to_le_bytes());
        out[10..12].copy_from_slice(&0u16.to_le_bytes());
        out[12..20].copy_from_slice(&self.created_ms.to_le_bytes());
        out[20..36].copy_from_slice(&self.session_uuid);
        let checksum = crc32c(&out[0..36]);
        out[36..40].copy_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Decodes and verifies a 40-byte `meta.apkm` payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, WalError> {
        if bytes.len() != META_LEN {
            return Err(WalError::InvalidFormat("meta.apkm length"));
        }
        if bytes[0..8] != META_MAGIC {
            return Err(WalError::InvalidFormat("meta.apkm magic"));
        }
        let expected_crc = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        if crc32c(&bytes[0..36]) != expected_crc {
            return Err(WalError::ChecksumMismatch("meta.apkm"));
        }
        let format_version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if format_version != FORMAT_VERSION {
            return Err(WalError::InvalidFormat("unsupported meta.apkm version"));
        }
        let flags = u16::from_le_bytes([bytes[10], bytes[11]]);
        if flags != 0 {
            return Err(WalError::InvalidFormat("unknown meta.apkm flags"));
        }
        let created_ms = u64::from_le_bytes(bytes[12..20].try_into().expect("8 bytes"));
        let session_uuid: [u8; 16] = bytes[20..36].try_into().expect("16 bytes");
        Ok(Self { format_version, created_ms, session_uuid })
    }
}

/// Computes the 32-byte SHA-256 digest over a segment's raw framed record bytes `[0..records_end_offset]`.
pub fn hash_segment_records(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// 72-byte immutable footer appended when sealing a segment file for $O(1)$ `Rewind` open.
///
/// Commits `records_end_offset` and `records_sha256` (the SHA-256 digest of `[0..records_end_offset]`)
/// so untrusted PTY output embedded inside a record can never spoof a valid sealed footer at EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentFooter {
    /// Timestamp of record 0 in the segment.
    pub first_ms: u64,
    /// Timestamp of the last record in the segment (`>= first_ms`).
    pub last_ms: u64,
    /// Exact byte offset where framed records end and this footer starts (`>= MIN_RECORD_LEN`).
    pub records_end_offset: u64,
    /// Number of valid records in the segment (`>= 1`).
    pub record_count: u32,
    /// SHA-256 digest of the segment bytes `[0..records_end_offset]`.
    pub records_sha256: [u8; 32],
}

impl SegmentFooter {
    /// Encodes the footer into its exact 72-byte CRC32C-protected representation.
    pub fn encode(&self) -> [u8; SEGMENT_FOOTER_LEN] {
        let mut out = [0u8; SEGMENT_FOOTER_LEN];
        out[0..8].copy_from_slice(&SEGMENT_FOOTER_MAGIC);
        out[8..16].copy_from_slice(&self.first_ms.to_le_bytes());
        out[16..24].copy_from_slice(&self.last_ms.to_le_bytes());
        out[24..32].copy_from_slice(&self.records_end_offset.to_le_bytes());
        out[32..36].copy_from_slice(&self.record_count.to_le_bytes());
        out[36..68].copy_from_slice(&self.records_sha256);
        let checksum = crc32c(&out[0..68]);
        out[68..72].copy_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Decodes and verifies a 72-byte segment footer.
    pub fn decode(bytes: &[u8]) -> Result<Self, WalError> {
        if bytes.len() != SEGMENT_FOOTER_LEN {
            return Err(WalError::InvalidFormat("segment footer length"));
        }
        if bytes[0..8] != SEGMENT_FOOTER_MAGIC {
            return Err(WalError::InvalidFormat("segment footer magic"));
        }
        let expected_crc = u32::from_le_bytes([bytes[68], bytes[69], bytes[70], bytes[71]]);
        if crc32c(&bytes[0..68]) != expected_crc {
            return Err(WalError::ChecksumMismatch("segment footer"));
        }
        let first_ms = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        let last_ms = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));
        let records_end_offset = u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes"));
        let record_count = u32::from_le_bytes(bytes[32..36].try_into().expect("4 bytes"));
        let records_sha256: [u8; 32] = bytes[36..68].try_into().expect("32 bytes");
        if last_ms < first_ms || record_count == 0 || records_end_offset < MIN_RECORD_LEN as u64 {
            return Err(WalError::CorruptSegment("invalid footer bounds"));
        }
        Ok(Self { first_ms, last_ms, records_end_offset, record_count, records_sha256 })
    }
}

/// Layer 1 of [`KeyframeV2`]: broker-owned, version-stable visual & shell-metadata snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualKeyframe {
    /// Terminal grid dimensions at keyframe capture time.
    pub size: GridSize,
    /// Cell pixel geometry at keyframe capture time.
    pub cell_px: CellPx,
    /// Whether the alternate screen buffer was active.
    pub active_alt_screen: bool,
    /// Cursor position, shape, blink, and visibility.
    pub cursor: CursorState,
    /// View-relevant terminal modes.
    pub modes: ViewModes,
    /// Dynamic OSC 4/10/11/12 palette overrides.
    pub palette: PaletteOverrides,
    /// Primary screen rows (up to [`MAX_KEYFRAME_HISTORY_ROWS`] history + active viewport).
    pub primary_rows: Vec<PackedRow>,
    /// Alternate screen rows (empty when alternate screen is unused).
    pub alt_rows: Vec<PackedRow>,
    /// Active anchored image placements referencing content-addressed `blobs/`.
    pub images: Vec<ImagePlacement>,
    /// Active shell-integration vouching key, if any.
    pub marks_key: Option<MarksKey>,
    /// Last accepted monotonic vouched mark sequence number.
    pub marks_last_seq: u64,
    /// Total refused forged/replayed marks.
    pub marks_refused: u64,
    /// Bounded command history ring (`<= MAX_COMMANDS`).
    pub commands: Vec<CommandRecord>,
}

impl VisualKeyframe {
    fn validate_bounds(&self) -> Result<(), WalError> {
        check_payload_limit(self.primary_rows.len(), MAX_VISUAL_PRIMARY_ROWS, "visual primary_rows")?;
        check_payload_limit(self.alt_rows.len(), MAX_VISUAL_ALT_ROWS, "visual alt_rows")?;
        check_payload_limit(self.images.len(), MAX_VISUAL_IMAGES, "visual images")?;
        check_payload_limit(self.commands.len(), MAX_COMMANDS, "visual commands")?;
        Ok(())
    }

    fn encode(&self, w: &mut Writer) {
        self.size.encode(w);
        self.cell_px.encode(w);
        w.bool(self.active_alt_screen);
        self.cursor.encode(w);
        self.modes.encode(w);
        self.palette.encode(w);
        w.slice(&self.primary_rows, |w, row| row.encode(w));
        w.slice(&self.alt_rows, |w, row| row.encode(w));
        w.slice(&self.images, |w, img| img.encode(w));
        w.opt(self.marks_key.as_ref(), |w, key| key.encode(w));
        w.var_u64(self.marks_last_seq);
        w.var_u64(self.marks_refused);
        w.slice(&self.commands, |w, cmd| cmd.encode(w));
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, WalError> {
        let size = GridSize::decode(r)?;
        let cell_px = CellPx::decode(r)?;
        let active_alt_screen = r.bool()?;
        let cursor = CursorState::decode(r)?;
        let modes = ViewModes::decode(r)?;
        let palette = PaletteOverrides::decode(r)?;
        let primary_rows = r.vec_bounded(MAX_VISUAL_PRIMARY_ROWS, "visual primary_rows", PackedRow::decode)?;
        let alt_rows = r.vec_bounded(MAX_VISUAL_ALT_ROWS, "visual alt_rows", PackedRow::decode)?;
        let images = r.vec_bounded(MAX_VISUAL_IMAGES, "visual images", ImagePlacement::decode)?;
        let marks_key = r.opt(MarksKey::decode)?;
        let marks_last_seq = r.var_u64()?;
        let marks_refused = r.var_u64()?;
        let commands = r.vec_bounded(MAX_COMMANDS, "visual commands", CommandRecord::decode)?;
        Ok(Self {
            size,
            cell_px,
            active_alt_screen,
            cursor,
            modes,
            palette,
            primary_rows,
            alt_rows,
            images,
            marks_key,
            marks_last_seq,
            marks_refused,
            commands,
        })
    }
}

/// Layer 2 of [`KeyframeV2`]: optional ABI-tagged lossless `libghostty-vt` binary checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCheckpoint {
    /// 32-byte ABI identifier for the `libghostty-vt` build that produced `snapshot_bytes`.
    pub engine_abi_hash: [u8; 32],
    /// Raw bounded binary snapshot produced by `GhosttyEngine::export_snapshot`.
    pub snapshot_bytes: Vec<u8>,
}

impl EngineCheckpoint {
    fn encode(&self, w: &mut Writer) {
        w.raw(&self.engine_abi_hash);
        w.bytes(&self.snapshot_bytes);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let engine_abi_hash = r.fixed::<32>()?;
        let snapshot_bytes = r.bytes_bounded(MAX_ENGINE_SNAPSHOT_BYTES, "engine checkpoint")?.to_vec();
        Ok(Self { engine_abi_hash, snapshot_bytes })
    }
}

/// Dual-layer segment keyframe stored as record 0 of every WAL segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyframeV2 {
    /// Always-present, broker-owned visual + shell state snapshot.
    pub visual: VisualKeyframe,
    /// Optional ABI-tagged `libghostty-vt` lossless checkpoint.
    pub engine: Option<EngineCheckpoint>,
}

impl KeyframeV2 {
    /// Returns the raw `libghostty-vt` snapshot bytes iff present AND matching `expected_abi`.
    ///
    /// When `None` is returned (either omitted or produced by a different `libghostty-vt` build),
    /// callers degrade gracefully to [`Self::visual`].
    pub fn engine_snapshot_if_compatible(&self, expected_abi: &[u8; 32]) -> Option<&[u8]> {
        self.engine.as_ref().filter(|e| e.engine_abi_hash == *expected_abi).map(|e| e.snapshot_bytes.as_slice())
    }

    /// Collects the deduplicated set of all [`BlobHash`]es referenced by this keyframe's image
    /// placements and Unicode placeholder row pieces.
    pub fn referenced_blobs(&self) -> BTreeSet<BlobHash> {
        let mut blobs = BTreeSet::new();
        for img in &self.visual.images {
            blobs.insert(img.blob);
        }
        for row in self.visual.primary_rows.iter().chain(self.visual.alt_rows.iter()) {
            for piece in &row.pieces {
                blobs.insert(piece.blob);
            }
        }
        blobs
    }

    /// Validates all decode-side count and size bounds before serialization.
    pub(crate) fn validate_bounds(&self) -> Result<(), WalError> {
        self.visual.validate_bounds()?;
        if let Some(engine) = &self.engine {
            check_payload_limit(engine.snapshot_bytes.len(), MAX_ENGINE_SNAPSHOT_BYTES, "engine checkpoint")?;
        }
        Ok(())
    }

    fn encode_uncompressed(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.visual.encode(&mut w);
        w.opt(self.engine.as_ref(), |w, eng| eng.encode(w));
        w.into_bytes()
    }

    fn decode_uncompressed(bytes: &[u8]) -> Result<Self, WalError> {
        let mut r = Reader::new(bytes);
        let visual = VisualKeyframe::decode(&mut r)?;
        let engine = r.opt(EngineCheckpoint::decode)?;
        r.finish()?;
        Ok(Self { visual, engine })
    }
}

/// Explicit cause of a [`WalRecord::Gap`] marker (never silent loss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GapReason {
    /// A continuous non-ground escape sequence exceeded `max_non_ground_bytes`.
    NonGroundCap = 1,
    /// Async disk writer backlog exceeded `max_backlog_bytes` under slow storage.
    BacklogOverflow = 2,
    /// Disk I/O failure interrupted recording.
    DiskError = 3,
}

impl GapReason {
    fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, WalError> {
        match r.u8()? {
            1 => Ok(Self::NonGroundCap),
            2 => Ok(Self::BacklogOverflow),
            3 => Ok(Self::DiskError),
            other => Err(WalError::Wire(WireError::InvalidTag { kind: "GapReason", tag: u16::from(other) })),
        }
    }
}

/// Typed payload stored inside one framed WAL segment record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    /// Dual-layer keyframe (required as record 0 of every segment).
    Keyframe(Box<KeyframeV2>),
    /// Raw PTY output bytes.
    Output(Vec<u8>),
    /// Terminal geometry / pixel-size change.
    Resize {
        /// New grid dimensions.
        size: GridSize,
        /// New cell pixel size.
        cell_px: CellPx,
    },
    /// Shell integration marks key activation or sequence sync.
    Marks {
        /// Active marks key (`None` for claimed/unkeyed mode).
        key: Option<MarksKey>,
        /// Monotonic accepted mark sequence number.
        last_seq: u64,
    },
    /// Explicit loss or pause accounting marker.
    Gap {
        /// Number of bytes omitted during the gap.
        dropped_bytes: u64,
        /// Cause of the gap.
        reason: GapReason,
    },
}

/// A decoded WAL record paired with its monotonic timestamp (ms since Unix epoch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedRecord {
    /// Monotonic timestamp (ms since Unix epoch).
    pub at_ms: u64,
    /// Decoded record payload.
    pub record: WalRecord,
}

pub(crate) fn check_payload_limit(len: usize, max: usize, what: &'static str) -> Result<(), WalError> {
    if len > max {
        return Err(WalError::LimitExceeded(what));
    }
    Ok(())
}

/// Encodes one framed record (`16B header + payload + 4B CRC32C`) into `out`.
pub fn encode_framed_record(at_ms: u64, record: &WalRecord, out: &mut Vec<u8>) -> Result<(), WalError> {
    let (kind, flags, payload): (u8, u8, Vec<u8>) = match record {
        WalRecord::Keyframe(kf) => {
            kf.validate_bounds()?;
            let raw = kf.encode_uncompressed();
            check_payload_limit(raw.len(), MAX_KEYFRAME_UNCOMPRESSED, "keyframe uncompressed bytes")?;
            let compressed = compress_to_vec_zlib(&raw, ZLIB_COMPRESSION_LEVEL);
            (KIND_KEYFRAME_V2, RECORD_FLAG_ZLIB, compressed)
        }
        WalRecord::Output(bytes) => (KIND_OUTPUT, 0, bytes.clone()),
        WalRecord::Resize { size, cell_px } => {
            let mut w = Writer::with_capacity(8);
            size.encode(&mut w);
            cell_px.encode(&mut w);
            (KIND_RESIZE, 0, w.into_bytes())
        }
        WalRecord::Marks { key, last_seq } => {
            let mut w = Writer::with_capacity(44);
            w.opt(key.as_ref(), |w, k| k.encode(w));
            w.var_u64(*last_seq);
            (KIND_MARKS, 0, w.into_bytes())
        }
        WalRecord::Gap { dropped_bytes, reason } => {
            let mut w = Writer::with_capacity(12);
            w.var_u64(*dropped_bytes);
            reason.encode(&mut w);
            (KIND_GAP, 0, w.into_bytes())
        }
    };

    check_payload_limit(payload.len(), MAX_RECORD_PAYLOAD, "WAL record payload")?;
    let payload_len = u32::try_from(payload.len()).expect("checked <= MAX_RECORD_PAYLOAD");

    let mut header = [0u8; RECORD_HEADER_LEN];
    header[0] = kind;
    header[1] = flags;
    header[2..4].copy_from_slice(&0u16.to_le_bytes());
    header[4..12].copy_from_slice(&at_ms.to_le_bytes());
    header[12..16].copy_from_slice(&payload_len.to_le_bytes());

    let crc = crc32c_append(crc32c(&header), &payload);

    out.extend_from_slice(&header);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Outcome of scanning raw segment bytes from offset `0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentScan {
    /// All valid records decoded before footer or torn tail (record 0 is guaranteed `Keyframe`).
    pub records: Vec<TimedRecord>,
    /// Byte offset of the end of the last valid record (truncation point on crash recovery).
    pub valid_records_end: usize,
    /// Present iff a valid 40-byte [`SegmentFooter`] terminated the file cleanly and matched all records.
    pub footer: Option<SegmentFooter>,
    /// `true` iff trailing bytes after `valid_records_end` were truncated, corrupt, or invalid.
    pub had_torn_tail: bool,
}

/// Scans a raw segment byte buffer sequentially, validating every record's CRC32C, payload,
/// monotonic timestamps, single-keyframe-at-index-0 invariant, and optional trailing footer.
///
/// Rejects any second `KeyframeV2` after record 0 (neutralizing multi-keyframe zlib amplification
/// bombs) and stops cleanly at `valid_records_end` if a torn tail or corruption is encountered.
pub fn scan_segment(bytes: &[u8]) -> SegmentScan {
    let mut records: Vec<TimedRecord> = Vec::new();
    let mut rest = bytes;
    let mut footer = None;
    let mut had_torn_tail = false;

    while !rest.is_empty() {
        let valid_so_far = bytes.len() - rest.len();
        if rest.len() == SEGMENT_FOOTER_LEN && rest[0..8] == SEGMENT_FOOTER_MAGIC {
            if let Ok(parsed_footer) = SegmentFooter::decode(rest)
                && let (Some(first), Some(last)) = (records.first(), records.last())
                && usize::try_from(parsed_footer.record_count).ok() == Some(records.len())
                && parsed_footer.records_end_offset == valid_so_far as u64
                && parsed_footer.first_ms == first.at_ms
                && parsed_footer.last_ms == last.at_ms
                && parsed_footer.records_sha256 == hash_segment_records(&bytes[..valid_so_far])
            {
                footer = Some(parsed_footer);
            } else {
                had_torn_tail = true;
            }
            break;
        }
        // Only record 0 may be a KeyframeV2 (rejects multi-keyframe zlib amplification bombs BEFORE inflating!).
        let allow_keyframe = records.is_empty();
        match decode_one_record_checked(rest, allow_keyframe) {
            Ok((timed, consumed)) => {
                if records.is_empty() && !matches!(timed.record, WalRecord::Keyframe(_)) {
                    had_torn_tail = true;
                    break;
                }
                if records.last().is_some_and(|prev| timed.at_ms < prev.at_ms) {
                    had_torn_tail = true;
                    break;
                }
                records.push(timed);
                rest = &rest[consumed..];
            }
            Err(_) => {
                had_torn_tail = true;
                break;
            }
        }
    }

    let valid_records_end = bytes.len() - rest.len();
    SegmentScan { records, valid_records_end, footer, had_torn_tail }
}

/// Decodes one framed record from the beginning of `bytes`, returning `(record, bytes_consumed)`.
pub fn decode_one_record(bytes: &[u8]) -> Result<(TimedRecord, usize), WalError> {
    decode_one_record_checked(bytes, true)
}

fn decode_one_record_checked(bytes: &[u8], allow_keyframe: bool) -> Result<(TimedRecord, usize), WalError> {
    if bytes.len() < MIN_RECORD_LEN {
        return Err(WalError::InvalidFormat("truncated record header"));
    }
    let kind = bytes[0];
    if kind == KIND_KEYFRAME_V2 && !allow_keyframe {
        return Err(WalError::CorruptSegment("unexpected keyframe after record 0"));
    }
    let flags = bytes[1];
    let reserved = u16::from_le_bytes([bytes[2], bytes[3]]);
    if reserved != 0 {
        return Err(WalError::InvalidFormat("non-zero record reserved field"));
    }
    if flags & !RECORD_FLAG_ZLIB != 0 {
        return Err(WalError::InvalidFormat("unknown record flag bits"));
    }
    let at_ms = u64::from_le_bytes(bytes[4..12].try_into().expect("8 bytes"));
    let payload_len = u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")) as usize;
    check_payload_limit(payload_len, MAX_RECORD_PAYLOAD, "WAL record payload")?;
    let total = RECORD_HEADER_LEN
        .checked_add(payload_len)
        .and_then(|n| n.checked_add(RECORD_TRAILER_LEN))
        .ok_or(WalError::LimitExceeded("WAL record frame"))?;
    if bytes.len() < total {
        return Err(WalError::InvalidFormat("truncated record payload"));
    }
    let body_end = RECORD_HEADER_LEN + payload_len;
    let expected_crc = u32::from_le_bytes(bytes[body_end..total].try_into().expect("4 bytes"));
    if crc32c(&bytes[0..body_end]) != expected_crc {
        return Err(WalError::ChecksumMismatch("WAL record"));
    }
    let payload = &bytes[RECORD_HEADER_LEN..body_end];
    let record = decode_record_payload(kind, flags, payload)?;
    Ok((TimedRecord { at_ms, record }, total))
}

fn decode_record_payload(kind: u8, flags: u8, payload: &[u8]) -> Result<WalRecord, WalError> {
    let compressed = flags & RECORD_FLAG_ZLIB != 0;
    match kind {
        KIND_KEYFRAME_V2 => {
            let raw = if compressed {
                decompress_to_vec_zlib_with_limit(payload, MAX_KEYFRAME_UNCOMPRESSED)
                    .map_err(|_| WalError::DecompressFailed)?
            } else {
                payload.to_vec()
            };
            let kf = KeyframeV2::decode_uncompressed(&raw)?;
            Ok(WalRecord::Keyframe(Box::new(kf)))
        }
        KIND_OUTPUT => {
            if compressed {
                return Err(WalError::InvalidFormat("unexpected compression flag on Output"));
            }
            Ok(WalRecord::Output(payload.to_vec()))
        }
        KIND_RESIZE => {
            if compressed {
                return Err(WalError::InvalidFormat("unexpected compression flag on Resize"));
            }
            let mut r = Reader::new(payload);
            let size = GridSize::decode(&mut r)?;
            let cell_px = CellPx::decode(&mut r)?;
            r.finish()?;
            Ok(WalRecord::Resize { size, cell_px })
        }
        KIND_MARKS => {
            if compressed {
                return Err(WalError::InvalidFormat("unexpected compression flag on Marks"));
            }
            let mut r = Reader::new(payload);
            let key = r.opt(MarksKey::decode)?;
            let last_seq = r.var_u64()?;
            r.finish()?;
            Ok(WalRecord::Marks { key, last_seq })
        }
        KIND_GAP => {
            if compressed {
                return Err(WalError::InvalidFormat("unexpected compression flag on Gap"));
            }
            let mut r = Reader::new(payload);
            let dropped_bytes = r.var_u64()?;
            let reason = GapReason::decode(&mut r)?;
            r.finish()?;
            Ok(WalRecord::Gap { dropped_bytes, reason })
        }
        _ => Err(WalError::InvalidFormat("unknown WAL record kind")),
    }
}
