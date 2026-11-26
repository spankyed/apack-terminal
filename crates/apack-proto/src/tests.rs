use std::collections::HashSet;

use crate::codec::{Reader, WireError, Writer};
use crate::frame::{
    DecodedFrame, FrameDecoder, FrameHeader, INNER_HEADER_LEN, MAX_BODY_LEN, encode_fin, encode_frame, encode_header,
};
use crate::messages::{
    BrokerMsg, ClientMsg, FrameUpdate, SessionSummary, SpawnSpec, decode_broker_frame, decode_client_frame, msg_type,
};
use crate::types::{
    BlobHash, BlockedKind, CellPos, CellPx, CellSpan, CommandOutcome, CommandRecord, Confirmed, CursorShape,
    CursorState, ErrorCode, ForegroundProcess, FrameCredit, GridSize, ImageEncoding, ImagePiece, ImagePlacement,
    KeyAction, KeyInput, LeaseKind, MarksKey, MetricsSnapshot, PackedRow, PackedRun, PackedStyle, PaletteOverrides,
    PasteOutcome, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy, Rgb, SessionId, SignalKind,
    SignalScope, SignalTarget, StatusRecord, StatusState, StreamId, SubFlags, UnknownReason, ViewModes, ViewportHint,
    VouchedExitCode, WaitCondition, WaitOutcome,
};
use crate::{
    BULK_CHUNK_BYTES, FIN_FRAME, FRAME_HEADER_LEN, MAX_BLOB_BYTES, MAX_FRAME_PAYLOAD, MAX_LEASE_GRACE_MS,
    MAX_ROWS_PER_FETCH, MAX_STRING_BYTES, MAX_VEC_ITEMS, PROTOCOL_VERSION,
};

fn sample_key() -> MarksKey {
    MarksKey::parse(b"0123456789abcdef0123456789abcdef").expect("valid marks key")
}

fn sample_grid() -> GridSize {
    GridSize::new(120, 40).expect("valid grid")
}

fn sample_sid(n: u64) -> SessionId {
    SessionId::new(n).expect("non-zero sid")
}

fn sample_pid(n: i32) -> SignalTarget {
    SignalTarget::new(n).expect("positive pid")
}

fn sample_blob(seed: u8) -> BlobHash {
    BlobHash([seed; 32])
}

fn sample_row() -> PackedRow {
    PackedRow {
        line: 42,
        flags: PackedRow::FLAG_WRAPPED
            | PackedRow::FLAG_PROMPT
            | PackedRow::FLAG_HAS_BLINK
            | PackedRow::FLAG_HAS_PLACEHOLDER,
        text: "hello 🚀 world".to_owned(),
        runs: vec![
            PackedRun { col_start: 0, col_len: 5, byte_offset: 0, byte_len: 5, style_idx: 0, link_idx: 0, flags: 0 },
            PackedRun { col_start: 6, col_len: 2, byte_offset: 6, byte_len: 4, style_idx: 1, link_idx: 1, flags: 1 },
        ],
        styles: vec![
            PackedStyle::default(),
            PackedStyle { fg: 0x01_FF_AA_00, bg: 12, underline_color: 0x01_00_FF_00, flags: 0x55 },
        ],
        links: vec![String::new(), "https://example.com".to_owned()],
        pieces: vec![ImagePiece {
            blob: sample_blob(7),
            col_start: 10,
            col_len: 4,
            src_rect: [1, 2, 30, 40],
            dst_px: [60, 80],
        }],
    }
}

fn sample_command(outcome: CommandOutcome) -> CommandRecord {
    CommandRecord {
        id: 9,
        command: Some("cargo test".to_owned()),
        cwd: Some("/tmp".to_owned()),
        started_ms: Some(1_000),
        finished_ms: Some(2_500),
        outcome,
        prompt_line: Some(10),
        output_line: Some(11),
        end_line: Some(25),
    }
}

fn sample_status(state: StatusState, kind: Option<BlockedKind>, confirmed: Confirmed) -> StatusRecord {
    StatusRecord {
        id: "agent/1".to_owned(),
        state,
        kind,
        progress: Some(75),
        app: Some("claude".to_owned()),
        title: Some("Running".to_owned()),
        message: Some("Working on tests".to_owned()),
        changed: 3,
        since_ms: 99_999,
        confirmed,
    }
}

fn all_client_messages() -> Vec<ClientMsg> {
    let sid = sample_sid(7);
    let stream = StreamId::from_raw(3);
    let hint =
        ViewportHint { size: sample_grid(), cell_px: CellPx { width: 9, height: 18 }, visible: true, focused: true };
    let credit = FrameCredit { max_frames: 2, max_bytes: 256 * 1024 };
    let span = CellSpan { start: CellPos { line: 1, col: 2 }, end: CellPos { line: 3, col: 4 } };

    vec![
        ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_nonce: [0xAB; 32],
            client_name: "view-v2".to_owned(),
        },
        ClientMsg::AuthProof { hmac: [0xCD; 32] },
        ClientMsg::AcquireLease { kind: LeaseKind::Broker { grace_ms: 30_000, resume_token: Some([0x11; 32]) } },
        ClientMsg::ReleaseLease { lease_id: 55 },
        ClientMsg::Spawn(Box::new(SpawnSpec {
            argv: vec!["/bin/zsh".to_owned(), "-l".to_owned()],
            cwd: Some("/home/dev".to_owned()),
            env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
            size: sample_grid(),
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_limit: 50_000,
            marks_key: Some(sample_key()),
            resize_policy: ResizePolicy::MinFit,
            record_dir: Some("/var/rec/1".to_owned()),
            restore_from: Some("/var/rec/0".to_owned()),
            image_files: true,
            kept: true,
        })),
        ClientMsg::Kill { session: sid },
        ClientMsg::Signal { session: sid, signal: SignalKind::Term, scope: SignalScope::Pid(sample_pid(4321)) },
        ClientMsg::Owns { session: sid, pid: sample_pid(4321) },
        ClientMsg::ListSessions,
        ClientMsg::Rewind { record_dir: "/tmp/rec".to_owned(), at_ms: Some(12_345) },
        ClientMsg::Seek { session: sid, at_ms: 54_321 },
        ClientMsg::Metrics,
        ClientMsg::Attach {
            session: sid,
            stream_id: stream,
            flags: SubFlags::from_bits(SubFlags::ALL_MASK).unwrap(),
            initial_viewport: Some(hint),
            initial_output_credit: 1_048_576,
            initial_frame_credit: credit,
        },
        ClientMsg::Detach { stream_id: stream },
        ClientMsg::Keep { session: sid, kept: true },
        ClientMsg::Wait {
            session: sid,
            condition: WaitCondition::Status(vec![StatusState::Done, StatusState::Error]),
            timeout_ms: 5_000,
        },
        ClientMsg::FrameAck { seq: 99, credit },
        ClientMsg::ViewportHint(hint),
        ClientMsg::ExplicitResize { size: sample_grid(), cell_px: CellPx { width: 10, height: 20 } },
        ClientMsg::SetCursorDefaults { shape: CursorShape::Bar, blinking: true },
        ClientMsg::Key(KeyInput {
            code: "KeyA".to_owned(),
            action: KeyAction::Press,
            modifiers: 0x05,
            text: "a".to_owned(),
            unshifted_codepoint: u32::from('a'),
            composed: false,
        }),
        ClientMsg::Text { text: "日本語".to_owned() },
        ClientMsg::Pointer(PointerInput {
            action: PointerAction::Motion,
            button: 0,
            modifiers: 2,
            x_px: 320,
            y_px: 180,
            cell_px: CellPx { width: 8, height: 16 },
        }),
        ClientMsg::Focus { focused: false },
        ClientMsg::Paste { text: "ls\n".to_owned(), allow_unsafe: true },
        ClientMsg::Write { data: vec![0x1b, b'[', b'A'] },
        ClientMsg::Pause,
        ClientMsg::Resume,
        ClientMsg::OutputCredit { bytes: 65_536 },
        ClientMsg::FetchRows { epoch: 4, from_line: 100, count: 250 },
        ClientMsg::WordAt { epoch: 4, pos: CellPos { line: 10, col: 5 } },
        ClientMsg::LineAt { epoch: 4, line: 10 },
        ClientMsg::TextOf { epoch: 4, span },
        ClientMsg::TextOfRect { epoch: 4, span },
        ClientMsg::SetSearch { query: "error".to_owned(), case_sensitive: true },
        ClientMsg::Commands,
        ClientMsg::CommandOutput { command_id: 42 },
        ClientMsg::Images,
        ClientMsg::GetImageBlob { blob: sample_blob(3), offset: 65_536, max_bytes: 65_536 },
    ]
}

fn all_broker_messages() -> Vec<BrokerMsg> {
    let sid = sample_sid(7);
    let stream = StreamId::from_raw(3);
    let span = CellSpan { start: CellPos { line: 2, col: 0 }, end: CellPos { line: 2, col: 8 } };
    let fg = ForegroundProcess { pid: sample_pid(900), name: Some("vim".to_owned()) };

    vec![
        BrokerMsg::HelloOk {
            protocol_version: PROTOCOL_VERSION,
            auth_required: true,
            server_nonce: [0x22; 32],
            word_delimiters: " \t/|()[]{}".to_owned(),
            capabilities: 0x0000_000F,
        },
        BrokerMsg::AuthOk { server_hmac: [0x33; 32] },
        BrokerMsg::Error { code: ErrorCode::StaleEpoch, message: "reflowed".to_owned() },
        BrokerMsg::Ok,
        BrokerMsg::LeaseAck { lease_id: 77, resumed: true },
        BrokerMsg::Spawned { session: sid, pid: Some(sample_pid(1234)) },
        BrokerMsg::OwnsReply { owned: true },
        BrokerMsg::SessionsList {
            sessions: vec![SessionSummary {
                id: sid,
                size: sample_grid(),
                alive: true,
                kept: false,
                rewound: false,
                title: "zsh".to_owned(),
                cwd: Some("/tmp".to_owned()),
                pid: Some(sample_pid(1234)),
                foreground: Some(fg.clone()),
                created_ms: 1_700_000_000_000,
            }],
        },
        BrokerMsg::Rewound { session: sid, duration_ms: 60_000, at_ms: 12_000 },
        BrokerMsg::Sought { at_ms: 45_000 },
        BrokerMsg::MetricsReply(MetricsSnapshot {
            live_sessions: 3,
            rewound_sessions: 1,
            connections: 2,
            active_timers: 5,
            scrollback_bytes: 1_000_000,
            image_store_bytes: 2_000_000,
            total_pty_bytes: 99_000_000,
            total_throttled_us: 12_345,
        }),
        BrokerMsg::Attached {
            session: sid,
            stream_id: stream,
            size: sample_grid(),
            epoch: 2,
            scrollback_lines: 500,
            foreground: Some(fg),
        },
        BrokerMsg::Waited { outcome: WaitOutcome::Met },
        BrokerMsg::Frame(Box::new(FrameUpdate {
            seq: 11,
            epoch: 2,
            size: sample_grid(),
            scrollback_lines: 500,
            scroll_delta: -3,
            cursor: CursorState { col: 15, row: 4, shape: CursorShape::Underline, blinking: true, hidden: true },
            modes: ViewModes { alt_screen: true, mouse: true, reverse_video: true, bracketed_paste: true },
            palette: Some(PaletteOverrides {
                fg: Some(Rgb([1, 2, 3])),
                bg: Some(Rgb([4, 5, 6])),
                cursor: Some(Rgb([7, 8, 9])),
                palette: vec![(0, Rgb([10, 20, 30])), (255, Rgb([250, 240, 230]))],
            }),
            rows: vec![sample_row()],
            images_gen: 5,
            full_sync: true,
        })),
        BrokerMsg::Rows { epoch: 2, rows: vec![sample_row()] },
        BrokerMsg::Pasted { outcome: PasteOutcome::Unsafe },
        BrokerMsg::SpanReply { span: Some(span) },
        BrokerMsg::TextReply { text: "selected".to_owned() },
        BrokerMsg::SearchMatches { epoch: 2, matches: vec![span], truncated: true },
        BrokerMsg::SearchDelta { epoch: 2, from_line: 10, to_line: 20, added: vec![span], truncated: false },
        BrokerMsg::CommandsReply {
            commands: vec![
                sample_command(CommandOutcome::Running),
                sample_command(CommandOutcome::Exited(VouchedExitCode::from_vouched(0))),
                sample_command(CommandOutcome::Unknown(UnknownReason::Superseded)),
                sample_command(CommandOutcome::Claimed(Some(127))),
            ],
        },
        BrokerMsg::CommandOutputReply { text: Some("ok\n".to_owned()) },
        BrokerMsg::ImagesReply {
            images_gen: 5,
            placements: vec![ImagePlacement {
                blob: sample_blob(9),
                line: 20,
                col: -2,
                pixel_size: [640, 480],
                cell_span: [40, 15],
                z_index: -1,
            }],
        },
        BrokerMsg::ImageBlobChunk {
            blob: sample_blob(9),
            encoding: ImageEncoding::Png,
            total_bytes: 128,
            offset: 64,
            data: vec![0x89, b'P', b'N', b'G'],
        },
        BrokerMsg::Title { title: "build".to_owned() },
        BrokerMsg::Bell,
        BrokerMsg::Clipboard { selection: "c".to_owned(), data: "copied".to_owned() },
        BrokerMsg::Notification { title: "Done".to_owned(), body: "Tests passed".to_owned() },
        BrokerMsg::Progress { state: ProgressState::Running, percent: Some(80) },
        BrokerMsg::ProgramStatus(Box::new(sample_status(
            StatusState::Blocked,
            Some(BlockedKind::Permission),
            Confirmed::Matched,
        ))),
        BrokerMsg::CommandStarted(Box::new(sample_command(CommandOutcome::Running))),
        BrokerMsg::CommandFinished(Box::new(sample_command(CommandOutcome::Exited(VouchedExitCode::from_vouched(1))))),
        BrokerMsg::CwdChanged { cwd: "/workspace".to_owned() },
        BrokerMsg::Exited { exit_code: Some(0), signal: Some("SIGTERM".to_owned()) },
        BrokerMsg::Output { text: "hello\r\n".to_owned() },
        BrokerMsg::Throttled { waited_us: 4_200 },
        BrokerMsg::OutputDropped { bytes: 8_192 },
        BrokerMsg::StreamClosed { reason: "detached".to_owned() },
    ]
}

#[test]
fn round_trip_all_client_and_broker_messages_and_distinct_tags() {
    let mut tags = HashSet::new();
    let client_msgs = all_client_messages();
    let broker_msgs = all_broker_messages();

    for (idx, msg) in client_msgs.iter().enumerate() {
        assert!(tags.insert(msg.msg_type()), "duplicate client msg_type {:#06x}", msg.msg_type());

        let mut wire = Vec::new();
        let sid = StreamId::from_raw((idx as u32) + 1);
        let req = RequestId::from_raw((idx as u32) + 10);
        msg.encode_to_frame(sid, req, &mut wire).unwrap();

        // 1-byte chunk boundary invariance
        let mut dec = FrameDecoder::new();
        let mut decoded = None;
        for byte in &wire {
            let (used, frame) = dec.step(std::slice::from_ref(byte)).unwrap();
            assert_eq!(used, 1);
            if frame.is_some() {
                assert!(decoded.is_none());
                decoded = frame;
            }
        }
        let (hdr, back) = decode_client_frame(decoded.expect("complete frame")).unwrap().unwrap();
        assert_eq!(hdr.stream_id, sid);
        assert_eq!(hdr.req_id, req);
        assert_eq!(hdr.msg_type, msg.msg_type());
        assert_eq!(&back, msg);

        // Reject trailing bytes
        let mut w = Writer::new();
        msg.encode(&mut w);
        w.u8(0xAA);
        assert_eq!(ClientMsg::decode(msg.msg_type(), w.as_slice()), Err(WireError::TrailingBytes(1)));
    }

    for (idx, msg) in broker_msgs.iter().enumerate() {
        assert!(tags.insert(msg.msg_type()), "duplicate broker msg_type {:#06x}", msg.msg_type());

        let mut wire = Vec::new();
        let sid = StreamId::from_raw(idx as u32);
        let req = RequestId::from_raw(idx as u32);
        msg.encode_to_frame(sid, req, &mut wire).unwrap();

        // Whole-chunk decode
        let mut dec = FrameDecoder::default();
        let (used, frame) = dec.step(&wire).unwrap();
        assert_eq!(used, wire.len());
        let (hdr, back) = decode_broker_frame(frame.expect("complete frame")).unwrap().unwrap();
        assert_eq!(hdr.stream_id, sid);
        assert_eq!(hdr.req_id, req);
        assert_eq!(hdr.msg_type, msg.msg_type());
        assert_eq!(&back, msg);

        // Reject trailing bytes
        let mut w = Writer::new();
        msg.encode(&mut w);
        w.u8(0x55);
        assert_eq!(BrokerMsg::decode(msg.msg_type(), w.as_slice()), Err(WireError::TrailingBytes(1)));
    }

    assert_eq!(ClientMsg::decode(0xFFFF, &[]), Err(WireError::InvalidTag { kind: "ClientMsg", tag: 0xFFFF }));
    assert_eq!(BrokerMsg::decode(0xFFFF, &[]), Err(WireError::InvalidTag { kind: "BrokerMsg", tag: 0xFFFF }));
}

#[test]
fn frame_decoder_splits_multi_frames_fin_and_bounds() {
    let mut wire = Vec::new();
    encode_frame(StreamId::CONTROL, msg_type::LIST_SESSIONS, RequestId::from_raw(1), &[], &mut wire).unwrap();
    encode_frame(StreamId::from_raw(2), msg_type::WRITE, RequestId::PUSH, b"abc", &mut wire).unwrap();
    encode_fin(&mut wire);
    // Extra trailing bytes after FIN should be ignored once closed
    wire.extend_from_slice(&[1, 2, 3]);

    for chunk_size in [1, 2, 3, 5, 7, 14, 100] {
        let mut dec = FrameDecoder::new();
        let mut frames = Vec::new();
        for chunk in wire.chunks(chunk_size) {
            let mut rest = chunk;
            while !rest.is_empty() {
                let (used, item) = dec.step(rest).unwrap();
                assert!(used > 0);
                rest = &rest[used..];
                if let Some(f) = item {
                    frames.push(f);
                }
            }
        }
        assert!(dec.is_closed());
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            DecodedFrame::Frame {
                header: FrameHeader {
                    stream_id: StreamId::CONTROL,
                    msg_type: msg_type::LIST_SESSIONS,
                    req_id: RequestId::from_raw(1),
                    payload_len: 0,
                },
                payload: Vec::new(),
            }
        );
        assert_eq!(
            frames[1],
            DecodedFrame::Frame {
                header: FrameHeader {
                    stream_id: StreamId::from_raw(2),
                    msg_type: msg_type::WRITE,
                    req_id: RequestId::PUSH,
                    payload_len: 3,
                },
                payload: b"abc".to_vec(),
            }
        );
        assert_eq!(frames[2], DecodedFrame::Fin);
        assert_eq!(decode_client_frame(DecodedFrame::Fin), Ok(None));
        assert_eq!(decode_broker_frame(DecodedFrame::Fin), Ok(None));
    }

    // Exact MAX_FRAME_PAYLOAD header succeeds; +1 fails
    let ok_hdr = encode_header(StreamId::CONTROL, 1, RequestId::PUSH, MAX_FRAME_PAYLOAD).unwrap();
    assert_eq!(ok_hdr.len(), FRAME_HEADER_LEN);
    assert_eq!(u32::from_le_bytes(ok_hdr[0..4].try_into().unwrap()) as usize, MAX_BODY_LEN);
    assert_eq!(
        encode_header(StreamId::CONTROL, 1, RequestId::PUSH, MAX_FRAME_PAYLOAD + 1),
        Err(WireError::FrameTooLarge((MAX_FRAME_PAYLOAD + 1) as u32))
    );

    // body_len in 1..10 is rejected and closes the decoder
    for bad_len in [1u32, 9] {
        let mut dec = FrameDecoder::new();
        assert_eq!(dec.step(&bad_len.to_le_bytes()), Err(WireError::InvalidValue("frame shorter than header")));
        assert!(dec.is_closed());
    }

    // body_len == MAX_BODY_LEN is accepted as in-progress; +1 is rejected
    let mut dec_ok = FrameDecoder::new();
    assert_eq!(dec_ok.step(&(MAX_BODY_LEN as u32).to_le_bytes()), Ok((4, None)));
    assert!(!dec_ok.is_closed());

    let mut dec_big = FrameDecoder::new();
    assert_eq!(
        dec_big.step(&((MAX_BODY_LEN as u32) + 1).to_le_bytes()),
        Err(WireError::FrameTooLarge((MAX_FRAME_PAYLOAD + 1) as u32))
    );
    assert!(dec_big.is_closed());

    // Exact FIN_FRAME constant
    let mut fin_dec = FrameDecoder::new();
    assert_eq!(fin_dec.step(&FIN_FRAME), Ok((4, Some(DecodedFrame::Fin))));
    assert!(fin_dec.is_closed());
    assert_eq!(INNER_HEADER_LEN, 10);
}

#[test]
fn codec_primitives_varints_bounds_and_display() {
    let mut w = Writer::with_capacity(64);
    assert!(w.capacity() >= 64);
    w.u8(0xFE);
    w.clear();
    assert!(w.as_slice().is_empty());
    assert_eq!(MAX_FRAME_PAYLOAD, 8_388_608);
    assert_eq!(BULK_CHUNK_BYTES, 65_536);

    w.u8(7);
    w.bool(false);
    w.bool(true);
    w.u16(0x1234);
    w.u32(0x1234_5678);
    w.i32(-42);
    w.u64(0x0123_4567_89AB_CDEF);
    for v in [0u32, 1, 127, 128, 16_383, 16_384, 1 << 28, u32::MAX] {
        w.var_u32(v);
    }
    for v in [0u64, 1, 127, 128, 1 << 28, 1 << 56, 1 << 63, u64::MAX] {
        w.var_u64(v);
    }
    w.bytes(b"xyz");
    w.str("hello");

    let bytes = w.into_bytes();
    let mut r = Reader::new(&bytes);
    assert_eq!(r.remaining(), bytes.len());
    assert_eq!(r.u8(), Ok(7));
    assert_eq!(r.bool(), Ok(false));
    assert_eq!(r.bool(), Ok(true));
    assert_eq!(r.u16(), Ok(0x1234));
    assert_eq!(r.u32(), Ok(0x1234_5678));
    assert_eq!(r.i32(), Ok(-42));
    assert_eq!(r.u64(), Ok(0x0123_4567_89AB_CDEF));
    for v in [0u32, 1, 127, 128, 16_383, 16_384, 1 << 28, u32::MAX] {
        assert_eq!(r.var_u32(), Ok(v));
    }
    for v in [0u64, 1, 127, 128, 1 << 28, 1 << 56, 1 << 63, u64::MAX] {
        assert_eq!(r.var_u64(), Ok(v));
    }
    assert_eq!(r.bytes(), Ok(b"xyz".to_vec()));
    assert_eq!(r.str(), Ok("hello".to_owned()));
    assert_eq!(r.remaining(), 0);
    assert_eq!(r.finish(), Ok(()));

    // Varint overflow boundaries
    assert_eq!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).var_u32(), Ok(u32::MAX));
    assert_eq!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).var_u32(), Err(WireError::VarintOverflow));
    assert_eq!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80]).var_u32(), Err(WireError::VarintOverflow));
    assert_eq!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]).var_u64(), Ok(u64::MAX));
    assert_eq!(
        Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02]).var_u64(),
        Err(WireError::VarintOverflow)
    );
    assert_eq!(Reader::new(&[0x80; 10]).var_u64(), Err(WireError::VarintOverflow));

    // Invalid bool, EOF, invalid UTF-8, bounded limits
    assert_eq!(Reader::new(&[2]).bool(), Err(WireError::InvalidTag { kind: "bool", tag: 2 }));
    assert_eq!(Reader::new(&[]).u8(), Err(WireError::UnexpectedEof));
    assert_eq!(Reader::new(&[2, 0xFF, 0xFE]).str(), Err(WireError::InvalidUtf8));

    let mut w_str = Writer::new();
    w_str.var_u32((MAX_STRING_BYTES + 1) as u32);
    assert_eq!(Reader::new(w_str.as_slice()).str(), Err(WireError::LimitExceeded("string")));

    let mut w_blob = Writer::new();
    w_blob.var_u32((MAX_BLOB_BYTES + 1) as u32);
    assert_eq!(
        Reader::new(w_blob.as_slice()).bytes_bounded(MAX_BLOB_BYTES + 100, "blob"),
        Err(WireError::LimitExceeded("blob"))
    );

    let mut w_exact = Writer::new();
    w_exact.bytes(b"abcd");
    assert_eq!(Reader::new(w_exact.as_slice()).bytes_bounded(4, "small"), Ok(&b"abcd"[..]));

    let mut w_small = Writer::new();
    w_small.var_u32(5);
    assert_eq!(Reader::new(w_small.as_slice()).bytes_bounded(4, "small"), Err(WireError::LimitExceeded("small")));

    let mut w_vec = Writer::new();
    w_vec.var_u32((MAX_VEC_ITEMS + 1) as u32);
    assert_eq!(
        Reader::new(w_vec.as_slice()).vec_bounded(MAX_VEC_ITEMS + 100, "items", Reader::u8),
        Err(WireError::LimitExceeded("items"))
    );
    // Hostile count within MAX_VEC_ITEMS with 0 bytes left hits UnexpectedEof cleanly
    let mut w_hostile = Writer::new();
    w_hostile.var_u32(MAX_VEC_ITEMS as u32);
    assert_eq!(
        Reader::new(w_hostile.as_slice()).vec_bounded(MAX_VEC_ITEMS, "items", Reader::u8),
        Err(WireError::UnexpectedEof)
    );

    // Display coverage for every WireError variant
    let errors = [
        WireError::UnexpectedEof,
        WireError::TrailingBytes(3),
        WireError::FrameTooLarge(99),
        WireError::InvalidTag { kind: "Test", tag: 7 },
        WireError::InvalidUtf8,
        WireError::VarintOverflow,
        WireError::LimitExceeded("cap"),
        WireError::InvalidValue("bad"),
    ];
    for err in errors {
        assert!(!err.to_string().is_empty());
    }
}

#[test]
fn domain_invariants_reject_illegal_values() {
    // SessionId
    assert!(SessionId::new(0).is_none());
    assert_eq!(SessionId::new(9).unwrap().get(), 9);
    assert_eq!(ClientMsg::decode(msg_type::KILL, &[0]), Err(WireError::InvalidValue("zero session id")));

    // StreamId & RequestId helpers
    assert!(StreamId::CONTROL.is_control());
    assert!(!StreamId::from_raw(1).is_control());
    assert!(RequestId::PUSH.is_push());
    assert!(!RequestId::from_raw(1).is_push());

    // Attach / Detach / Attached forbid StreamId::CONTROL (0)
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    w.u32(0);
    assert_eq!(
        ClientMsg::decode(msg_type::ATTACH, w.as_slice()),
        Err(WireError::InvalidValue("attach stream_id must be non-zero"))
    );
    assert_eq!(
        ClientMsg::decode(msg_type::DETACH, &0u32.to_le_bytes()),
        Err(WireError::InvalidValue("detach stream_id must be non-zero"))
    );
    assert_eq!(
        BrokerMsg::decode(msg_type::ATTACHED, w.as_slice()),
        Err(WireError::InvalidValue("attached stream_id must be non-zero"))
    );

    // SignalTarget forbids <= 0
    assert!(SignalTarget::new(0).is_none());
    assert!(SignalTarget::new(-1).is_none());
    assert!(SignalTarget::new(i32::MIN).is_none());
    assert_eq!(SignalTarget::new(42).unwrap().pid(), 42);
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    w.i32(-1);
    assert_eq!(
        ClientMsg::decode(msg_type::OWNS, w.as_slice()),
        Err(WireError::InvalidValue("signal target pid must be > 0"))
    );

    // MarksKey parsing + redacted Debug
    assert!(MarksKey::parse(b"short").is_none());
    assert!(MarksKey::parse(b"0123456789ABCDEF0123456789abcdef").is_none());
    assert!(MarksKey::parse(b"0123456789abcdef0123456789abcdeg").is_none());
    let key = sample_key();
    assert_eq!(key.as_str(), "0123456789abcdef0123456789abcdef");
    assert_eq!(key.as_bytes(), b"0123456789abcdef0123456789abcdef");
    assert_eq!(format!("{key:?}"), "MarksKey([REDACTED])");

    // GridSize forbids 0 cols or 0 rows
    assert!(GridSize::new(0, 10).is_none());
    assert!(GridSize::new(10, 0).is_none());
    let mut w = Writer::new();
    w.u16(0);
    w.u16(24);
    assert_eq!(
        ClientMsg::decode(msg_type::EXPLICIT_RESIZE, w.as_slice()),
        Err(WireError::InvalidValue("grid dimensions must be non-zero"))
    );

    // SubFlags
    assert_eq!(SubFlags::VIEW, 0x01);
    assert_eq!(SubFlags::EVENTS, 0x02);
    assert_eq!(SubFlags::RAW_OUTPUT, 0x04);
    assert_eq!(SubFlags::BACKPRESSURE_PTY, 0x08);
    assert_eq!(SubFlags::ALL_MASK, 0x0F);
    assert!(SubFlags::from_bits(0x10).is_none());
    let flags = SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap();
    assert_eq!(flags.bits(), 0x03);
    assert!(flags.contains(SubFlags::VIEW));
    assert!(!flags.contains(SubFlags::RAW_OUTPUT));

    // PackedRow flag constants
    assert_eq!(PackedRow::FLAG_WRAPPED, 0x01);
    assert_eq!(PackedRow::FLAG_PROMPT, 0x02);
    assert_eq!(PackedRow::FLAG_HAS_BLINK, 0x04);
    assert_eq!(PackedRow::FLAG_HAS_PLACEHOLDER, 0x08);

    // LeaseKind invariants
    let ok_zero = ClientMsg::AcquireLease { kind: LeaseKind::Broker { grace_ms: 0, resume_token: None } };
    let mut w = Writer::new();
    ok_zero.encode(&mut w);
    assert_eq!(ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()), Ok(ok_zero));

    let ok_max = ClientMsg::AcquireLease {
        kind: LeaseKind::Broker { grace_ms: MAX_LEASE_GRACE_MS, resume_token: Some([1; 32]) },
    };
    let mut w = Writer::new();
    ok_max.encode(&mut w);
    assert_eq!(ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()), Ok(ok_max));

    let ok_sess = ClientMsg::AcquireLease { kind: LeaseKind::Session(sample_sid(4)) };
    let mut w = Writer::new();
    ok_sess.encode(&mut w);
    assert_eq!(ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()), Ok(ok_sess));

    // grace_ms > MAX_LEASE_GRACE_MS rejected
    let mut w = Writer::new();
    w.u8(0);
    w.var_u32(MAX_LEASE_GRACE_MS + 1);
    assert_eq!(
        ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()),
        Err(WireError::LimitExceeded("lease grace_ms"))
    );
    // grace_ms > 0 without resume_token rejected
    let mut w = Writer::new();
    w.u8(0);
    w.var_u32(1);
    w.u8(0);
    assert_eq!(
        ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()),
        Err(WireError::InvalidValue("grace_ms > 0 requires resume_token"))
    );
    // Invalid LeaseKind tag
    assert_eq!(
        ClientMsg::decode(msg_type::ACQUIRE_LEASE, &[9]),
        Err(WireError::InvalidTag { kind: "LeaseKind", tag: 9 })
    );

    // FetchRows count 0 and > MAX_ROWS_PER_FETCH rejected; exact MAX accepted
    for bad in [0u32, MAX_ROWS_PER_FETCH + 1] {
        let mut w = Writer::new();
        w.var_u64(1);
        w.var_u64(0);
        w.var_u32(bad);
        assert_eq!(
            ClientMsg::decode(msg_type::FETCH_ROWS, w.as_slice()),
            Err(WireError::LimitExceeded("fetch rows count"))
        );
    }
    let ok_fetch = ClientMsg::FetchRows { epoch: 1, from_line: 0, count: MAX_ROWS_PER_FETCH };
    let mut w = Writer::new();
    ok_fetch.encode(&mut w);
    assert_eq!(ClientMsg::decode(msg_type::FETCH_ROWS, w.as_slice()), Ok(ok_fetch));

    // CellSpan end < start rejected; start == end accepted
    let mut w = Writer::new();
    w.var_u64(1);
    w.var_u64(5);
    w.u16(3);
    w.var_u64(5);
    w.u16(2);
    assert_eq!(
        ClientMsg::decode(msg_type::TEXT_OF, w.as_slice()),
        Err(WireError::InvalidValue("span end before start"))
    );
    let eq_span = CellSpan { start: CellPos { line: 5, col: 3 }, end: CellPos { line: 5, col: 3 } };
    let eq_msg = ClientMsg::TextOf { epoch: 1, span: eq_span };
    let mut w = Writer::new();
    eq_msg.encode(&mut w);
    assert_eq!(ClientMsg::decode(msg_type::TEXT_OF, w.as_slice()), Ok(eq_msg));

    // SearchDelta to_line < from_line rejected; equal accepted
    let mut w = Writer::new();
    w.var_u64(1);
    w.var_u64(10);
    w.var_u64(9);
    assert_eq!(
        BrokerMsg::decode(msg_type::SEARCH_DELTA, w.as_slice()),
        Err(WireError::InvalidValue("search delta to_line < from_line"))
    );
    let ok_delta = BrokerMsg::SearchDelta { epoch: 1, from_line: 10, to_line: 10, added: vec![], truncated: false };
    let mut w = Writer::new();
    ok_delta.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::SEARCH_DELTA, w.as_slice()), Ok(ok_delta));

    // Progress > 100 rejected; 100 accepted
    let mut w = Writer::new();
    w.u8(1);
    w.u8(1);
    w.u8(101);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRESS, w.as_slice()),
        Err(WireError::InvalidValue("progress percent > 100"))
    );
    let ok_prog = BrokerMsg::Progress { state: ProgressState::Running, percent: Some(100) };
    let mut w = Writer::new();
    ok_prog.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::PROGRESS, w.as_slice()), Ok(ok_prog));

    // StatusRecord progress > 100 rejected; 100 accepted
    let mut bad_status = sample_status(StatusState::Working, None, Confirmed::Unknown);
    bad_status.progress = Some(101);
    let mut w = Writer::new();
    BrokerMsg::ProgramStatus(Box::new(bad_status)).encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRAM_STATUS, w.as_slice()),
        Err(WireError::InvalidValue("progress must be 0..=100"))
    );
    let mut ok_status = sample_status(StatusState::Working, None, Confirmed::Unknown);
    ok_status.progress = Some(100);
    let mut w = Writer::new();
    BrokerMsg::ProgramStatus(Box::new(ok_status.clone())).encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRAM_STATUS, w.as_slice()),
        Ok(BrokerMsg::ProgramStatus(Box::new(ok_status)))
    );
}

#[test]
fn packed_row_rejects_corrupt_runs_and_flags() {
    // Valid empty-style-table row using style_idx=0 and link_idx=0
    let plain = PackedRow {
        line: 0,
        flags: 0,
        text: "ok".to_owned(),
        runs: vec![PackedRun {
            col_start: 0,
            col_len: 2,
            byte_offset: 0,
            byte_len: 2,
            style_idx: 0,
            link_idx: 0,
            flags: 0,
        }],
        styles: vec![],
        links: vec![],
        pieces: vec![],
    };
    let ok_msg = BrokerMsg::Rows { epoch: 1, rows: vec![plain.clone()] };
    let mut w = Writer::new();
    ok_msg.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::ROWS, w.as_slice()), Ok(ok_msg));

    // Unknown row flag bit
    let mut bad = plain.clone();
    bad.flags = 0x10;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("unknown PackedRow flag bits"))
    );

    // Unknown run flag bit
    let mut bad = plain.clone();
    bad.runs[0].flags = 2;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("unknown PackedRun flag bits"))
    );

    // Out-of-bounds byte_len
    let mut bad = plain.clone();
    bad.runs[0].byte_len = 3;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run byte range out of bounds or mid-UTF-8"))
    );

    // Mid-UTF-8 start and end boundaries
    let mut bad_utf8 = plain.clone();
    bad_utf8.text = "é".to_owned(); // 2 bytes
    bad_utf8.runs[0].byte_offset = 0;
    bad_utf8.runs[0].byte_len = 1;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_utf8] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run byte range out of bounds or mid-UTF-8"))
    );

    let mut bad_utf8_start = plain.clone();
    bad_utf8_start.text = "é".to_owned();
    bad_utf8_start.runs[0].byte_offset = 1;
    bad_utf8_start.runs[0].byte_len = 1;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_utf8_start] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run byte range out of bounds or mid-UTF-8"))
    );

    // Out-of-bounds style_idx and link_idx
    let mut bad_style = plain.clone();
    bad_style.runs[0].style_idx = 1;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_style] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run style_idx out of bounds"))
    );

    let mut bad_link = plain;
    bad_link.runs[0].link_idx = 1;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_link] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run link_idx out of bounds"))
    );
}

#[test]
fn enum_variant_and_flag_exhaustiveness() {
    // SignalKind & SignalScope
    for kind in [
        SignalKind::Hup,
        SignalKind::Int,
        SignalKind::Quit,
        SignalKind::Term,
        SignalKind::Kill,
        SignalKind::Usr1,
        SignalKind::Usr2,
        SignalKind::Stop,
        SignalKind::Cont,
    ] {
        for scope in [SignalScope::Leader, SignalScope::Group, SignalScope::Pid(sample_pid(5))] {
            let msg = ClientMsg::Signal { session: sample_sid(1), signal: kind, scope };
            let mut w = Writer::new();
            msg.encode(&mut w);
            assert_eq!(ClientMsg::decode(msg_type::SIGNAL, w.as_slice()), Ok(msg));
        }
    }
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    w.u8(99);
    assert_eq!(
        ClientMsg::decode(msg_type::SIGNAL, w.as_slice()),
        Err(WireError::InvalidTag { kind: "SignalKind", tag: 99 })
    );
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    w.u8(1);
    w.u8(99);
    assert_eq!(
        ClientMsg::decode(msg_type::SIGNAL, w.as_slice()),
        Err(WireError::InvalidTag { kind: "SignalScope", tag: 99 })
    );

    // ErrorCode
    for code in [
        ErrorCode::Handshake,
        ErrorCode::Unauthorized,
        ErrorCode::Malformed,
        ErrorCode::NoSuchSession,
        ErrorCode::NoSuchStream,
        ErrorCode::StaleEpoch,
        ErrorCode::Pty,
        ErrorCode::OutsideSession,
        ErrorCode::Unsupported,
        ErrorCode::ResourceLimit,
        ErrorCode::Busy,
    ] {
        let msg = BrokerMsg::Error { code, message: "m".to_owned() };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::ERROR, w.as_slice()), Ok(msg));
    }
    let mut w = Writer::new();
    w.u16(999);
    w.str("x");
    assert_eq!(
        BrokerMsg::decode(msg_type::ERROR, w.as_slice()),
        Err(WireError::InvalidTag { kind: "ErrorCode", tag: 999 })
    );

    // ResizePolicy, CursorShape, ImageEncoding, KeyAction, PointerAction, PasteOutcome, WaitCondition, WaitOutcome
    for policy in [ResizePolicy::LatestActive, ResizePolicy::MinFit, ResizePolicy::ExplicitOverride] {
        let msg = ClientMsg::Spawn(Box::new(SpawnSpec {
            argv: vec![],
            cwd: None,
            env: vec![],
            size: sample_grid(),
            cell_px: CellPx::default(),
            scrollback_limit: 0,
            marks_key: None,
            resize_policy: policy,
            record_dir: None,
            restore_from: None,
            image_files: false,
            kept: false,
        }));
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::SPAWN, w.as_slice()), Ok(msg));
    }

    for shape in [CursorShape::Block, CursorShape::Bar, CursorShape::Underline, CursorShape::Hollow] {
        for (blinking, hidden) in [(false, false), (true, false), (false, true), (true, true)] {
            let msg = BrokerMsg::Frame(Box::new(FrameUpdate {
                seq: 1,
                epoch: 1,
                size: sample_grid(),
                scrollback_lines: 0,
                scroll_delta: 0,
                cursor: CursorState { col: 0, row: 0, shape, blinking, hidden },
                modes: ViewModes {
                    alt_screen: blinking,
                    mouse: hidden,
                    reverse_video: blinking && !hidden,
                    bracketed_paste: !blinking && hidden,
                },
                palette: None,
                rows: vec![],
                images_gen: 0,
                full_sync: false,
            }));
            let mut w = Writer::new();
            msg.encode(&mut w);
            assert_eq!(BrokerMsg::decode(msg_type::FRAME, w.as_slice()), Ok(msg));
        }
    }

    // CursorState unknown flag bit & ViewModes unknown flag bit
    let mut w = Writer::new();
    w.var_u64(1);
    w.var_u64(1);
    sample_grid().encode(&mut w);
    w.var_u64(0);
    w.i32(0);
    w.u16(0);
    w.u16(0);
    w.u8(0);
    w.u8(0x04); // bad CursorState flags
    assert_eq!(
        BrokerMsg::decode(msg_type::FRAME, w.as_slice()),
        Err(WireError::InvalidValue("unknown CursorState flag bits"))
    );

    let mut w = Writer::new();
    w.var_u64(1);
    w.var_u64(1);
    sample_grid().encode(&mut w);
    w.var_u64(0);
    w.i32(0);
    w.u16(0);
    w.u16(0);
    w.u8(0);
    w.u8(0);
    w.u8(0x10); // bad ViewModes flags
    assert_eq!(
        BrokerMsg::decode(msg_type::FRAME, w.as_slice()),
        Err(WireError::InvalidValue("unknown ViewModes flag bits"))
    );

    for enc in [ImageEncoding::Png, ImageEncoding::ZstdRgba] {
        let msg = BrokerMsg::ImageBlobChunk {
            blob: sample_blob(1),
            encoding: enc,
            total_bytes: 2,
            offset: 0,
            data: vec![1, 2],
        };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::IMAGE_BLOB_CHUNK, w.as_slice()), Ok(msg));
    }

    for action in [KeyAction::Press, KeyAction::Repeat, KeyAction::Release] {
        let msg = ClientMsg::Key(KeyInput {
            code: "Enter".to_owned(),
            action,
            modifiers: 0,
            text: String::new(),
            unshifted_codepoint: 0,
            composed: false,
        });
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::KEY, w.as_slice()), Ok(msg));
    }

    for action in [
        PointerAction::Press,
        PointerAction::Release,
        PointerAction::Motion,
        PointerAction::WheelUp,
        PointerAction::WheelDown,
        PointerAction::WheelLeft,
        PointerAction::WheelRight,
    ] {
        let msg = ClientMsg::Pointer(PointerInput {
            action,
            button: 1,
            modifiers: 0,
            x_px: 10,
            y_px: 20,
            cell_px: CellPx { width: 8, height: 16 },
        });
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::POINTER, w.as_slice()), Ok(msg));
    }

    for outcome in [PasteOutcome::Pasted, PasteOutcome::Unsafe] {
        let msg = BrokerMsg::Pasted { outcome };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::PASTED, w.as_slice()), Ok(msg));
    }

    for cond in [
        WaitCondition::Idle,
        WaitCondition::Text("ready".to_owned()),
        WaitCondition::Status(vec![
            StatusState::Idle,
            StatusState::Working,
            StatusState::Blocked,
            StatusState::Done,
            StatusState::Error,
        ]),
        WaitCondition::Exit,
    ] {
        let msg = ClientMsg::Wait { session: sample_sid(1), condition: cond, timeout_ms: 100 };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::WAIT, w.as_slice()), Ok(msg));
    }

    for outcome in [WaitOutcome::Met, WaitOutcome::Timeout, WaitOutcome::Exited] {
        let msg = BrokerMsg::Waited { outcome };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::WAITED, w.as_slice()), Ok(msg));
    }

    for state in [
        ProgressState::None,
        ProgressState::Running,
        ProgressState::Error,
        ProgressState::Indeterminate,
        ProgressState::Paused,
    ] {
        let msg = BrokerMsg::Progress { state, percent: None };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::PROGRESS, w.as_slice()), Ok(msg));
    }

    for (why, kind, conf) in [
        (UnknownReason::NoStatus, BlockedKind::Permission, Confirmed::Matched),
        (UnknownReason::Superseded, BlockedKind::Question, Confirmed::Mismatched),
        (UnknownReason::SessionEnded, BlockedKind::Auth, Confirmed::Unknown),
    ] {
        let cmd = BrokerMsg::CommandFinished(Box::new(sample_command(CommandOutcome::Unknown(why))));
        let mut w = Writer::new();
        cmd.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::COMMAND_FINISHED, w.as_slice()), Ok(cmd));

        let st = BrokerMsg::ProgramStatus(Box::new(sample_status(StatusState::Blocked, Some(kind), conf)));
        let mut w = Writer::new();
        st.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::PROGRAM_STATUS, w.as_slice()), Ok(st));
    }
}
