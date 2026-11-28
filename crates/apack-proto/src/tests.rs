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
    KeyAction, KeyInput, LeaseGraceMs, LeaseKind, MarksKey, MetricsSnapshot, NonZeroStreamId, PackedRow, PackedRun,
    PackedStyle, PaletteOverrides, PasteOutcome, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy,
    ResumeToken, Rgb, SessionId, SignalKind, SignalScope, SignalTarget, StatusRecord, StatusState, StreamId, SubFlags,
    UnknownReason, ViewModes, ViewportHint, VouchedExitCode, WaitCondition, WaitOutcome,
};
use crate::{
    BULK_CHUNK_BYTES, FIN_FRAME, FRAME_HEADER_LEN, MAX_BLOB_BYTES, MAX_CLIENT_NAME_BYTES, MAX_FRAME_PAYLOAD,
    MAX_GRID_COLS, MAX_GRID_ROWS, MAX_LEASE_GRACE_MS, MAX_ROWS_PER_FETCH, MAX_STRING_BYTES, MAX_VEC_ITEMS,
    PROTOCOL_VERSION,
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

fn sample_nz_stream(n: u32) -> NonZeroStreamId {
    NonZeroStreamId::new(n).expect("non-zero stream id")
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
    let stream = sample_nz_stream(3);
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
        ClientMsg::AcquireLease {
            kind: LeaseKind::BrokerGrace {
                grace_ms: LeaseGraceMs::new(30_000).unwrap(),
                resume_token: ResumeToken::new([0x11; 32]).unwrap(),
            },
        },
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
    let stream = sample_nz_stream(3);
    let span = CellSpan { start: CellPos { line: 2, col: 0 }, end: CellPos { line: 2, col: 8 } };
    let delta_span = CellSpan { start: CellPos { line: 10, col: 0 }, end: CellPos { line: 19, col: 4 } };
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
            first_active_line: 600,
            scrollback_lines: 500,
            foreground: Some(fg),
            statuses: vec![StatusRecord {
                id: String::new(),
                state: StatusState::Working,
                kind: None,
                progress: Some(50),
                app: Some(String::from("codex")),
                title: Some(String::from("Indexing")),
                message: None,
                changed: 1,
                since_ms: 1_700_000_000_000,
                confirmed: Confirmed::Matched,
            }],
        },
        BrokerMsg::Waited { outcome: WaitOutcome::Met },
        BrokerMsg::Frame(Box::new(FrameUpdate {
            seq: 11,
            epoch: 2,
            size: sample_grid(),
            first_active_line: 600,
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
        BrokerMsg::SearchDelta { epoch: 2, from_line: 10, to_line: 20, added: vec![delta_span], truncated: false },
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
                source_rect: [4, 8, 320, 240],
                cell_offset: [2, 3],
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

    // Empty input slice does nothing
    let mut empty_dec = FrameDecoder::new();
    assert_eq!(empty_dec.step(&[]), Ok((0, None)));
    assert!(!empty_dec.is_closed());

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
        if let DecodedFrame::Frame { header, payload } = &frames[0] {
            assert_eq!(
                *header,
                FrameHeader {
                    stream_id: StreamId::CONTROL,
                    msg_type: msg_type::LIST_SESSIONS,
                    req_id: RequestId::from_raw(1),
                    payload_len: 0,
                }
            );
            assert!(payload.is_empty());
            assert_eq!(payload.capacity(), 0);
        } else {
            panic!("expected empty-payload frame");
        }
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

    // body_len in 1..10 is rejected, closes the decoder, and subsequent bytes are ignored
    for bad_len in [1u32, 9] {
        let mut dec = FrameDecoder::new();
        assert_eq!(dec.step(&bad_len.to_le_bytes()), Err(WireError::InvalidValue("frame shorter than header")));
        assert!(dec.is_closed());
        assert_eq!(dec.step(&[1, 2, 3]), Ok((3, None)));
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

    // Custom bounded FrameDecoder (used for AwaitingHello 4 KiB / AwaitingAuth 64 B phases)
    let mut small = FrameDecoder::with_max_payload(64);
    assert_eq!(small.max_payload_len(), 64);
    assert_eq!(FrameDecoder::with_max_payload(MAX_FRAME_PAYLOAD + 1024).max_payload_len(), MAX_FRAME_PAYLOAD);
    let mut ok_64 = Vec::new();
    encode_frame(StreamId::CONTROL, msg_type::AUTH_PROOF, RequestId::PUSH, &[0xAB; 64], &mut ok_64).unwrap();
    let (used, decoded) = small.step(&ok_64).expect("64B payload accepted");
    assert_eq!(used, ok_64.len());
    assert!(matches!(decoded, Some(DecodedFrame::Frame { header, .. }) if header.payload_len == 64));

    let mut small_over = FrameDecoder::with_max_payload(64);
    let over_body_len = (INNER_HEADER_LEN + 65) as u32;
    assert_eq!(small_over.step(&over_body_len.to_le_bytes()), Err(WireError::FrameTooLarge(65)));
    assert!(small_over.is_closed());

    // Hello.client_name is bounded to MAX_CLIENT_NAME_BYTES (256)
    let ok_hello = ClientMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_nonce: [1u8; 32],
        client_name: "a".repeat(MAX_CLIENT_NAME_BYTES),
    };
    let mut w = Writer::new();
    ok_hello.encode(&mut w);
    assert_eq!(ClientMsg::decode(msg_type::HELLO, w.as_slice()), Ok(ok_hello));

    let bad_hello = ClientMsg::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_nonce: [1u8; 32],
        client_name: "a".repeat(MAX_CLIENT_NAME_BYTES + 1),
    };
    let mut w_bad = Writer::new();
    bad_hello.encode(&mut w_bad);
    assert!(matches!(ClientMsg::decode(msg_type::HELLO, w_bad.as_slice()), Err(WireError::LimitExceeded { .. })));
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

    // Canonical varint enforcement & overflow boundaries
    assert_eq!(Reader::new(&[0x80, 0x00]).var_u32(), Err(WireError::InvalidValue("non-canonical varint")));
    assert_eq!(Reader::new(&[0x80, 0x00]).var_u64(), Err(WireError::InvalidValue("non-canonical varint")));
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

    // StreamId, NonZeroStreamId, RequestId helpers
    assert!(StreamId::CONTROL.is_control());
    assert!(!StreamId::from_raw(1).is_control());
    assert!(NonZeroStreamId::new(0).is_none());
    let nz = NonZeroStreamId::new(5).unwrap();
    assert_eq!(nz.raw(), 5);
    assert_eq!(nz.as_stream_id(), StreamId::from_raw(5));
    assert!(RequestId::PUSH.is_push());
    assert!(!RequestId::from_raw(1).is_push());

    // Attach / Detach / Attached reject stream_id == 0 on wire
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    w.u32(0);
    assert_eq!(
        ClientMsg::decode(msg_type::ATTACH, w.as_slice()),
        Err(WireError::InvalidValue("stream id must be non-zero"))
    );
    assert_eq!(
        ClientMsg::decode(msg_type::DETACH, &0u32.to_le_bytes()),
        Err(WireError::InvalidValue("stream id must be non-zero"))
    );
    assert_eq!(
        BrokerMsg::decode(msg_type::ATTACHED, w.as_slice()),
        Err(WireError::InvalidValue("stream id must be non-zero"))
    );

    // SignalTarget forbids < 1; test 1 and 42 explicitly
    assert!(SignalTarget::new(0).is_none());
    assert!(SignalTarget::new(-1).is_none());
    assert!(SignalTarget::new(i32::MIN).is_none());
    assert_eq!(SignalTarget::new(1).unwrap().pid(), 1);
    assert_eq!(SignalTarget::new(42).unwrap().pid(), 42);
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    w.i32(-1);
    assert_eq!(
        ClientMsg::decode(msg_type::OWNS, w.as_slice()),
        Err(WireError::InvalidValue("signal target pid must be > 0"))
    );

    // MarksKey parsing, wire decode rejection, and redacted Debug
    assert!(MarksKey::parse(b"short").is_none());
    assert!(MarksKey::parse(b"0123456789ABCDEF0123456789abcdef").is_none());
    assert!(MarksKey::parse(b"0123456789abcdef0123456789abcdeg").is_none());
    let key = sample_key();
    assert_eq!(key.as_str(), "0123456789abcdef0123456789abcdef");
    assert_eq!(key.as_bytes(), b"0123456789abcdef0123456789abcdef");
    assert_eq!(format!("{key:?}"), "MarksKey([REDACTED])");
    let mut w = Writer::new();
    w.var_u32(0); // argv
    w.u8(0); // cwd
    w.var_u32(0); // env
    sample_grid().encode(&mut w);
    CellPx::default().encode(&mut w);
    w.var_u32(0); // scrollback
    w.u8(1); // marks_key = Some
    w.raw(&[b'Z'; 32]); // invalid uppercase non-hex
    assert_eq!(
        ClientMsg::decode(msg_type::SPAWN, w.as_slice()),
        Err(WireError::InvalidValue("marks key must be 32 lowercase hex chars"))
    );

    // GridSize bounds (1..=MAX_GRID_COLS, 1..=MAX_GRID_ROWS)
    assert!(GridSize::new(0, 10).is_none());
    assert!(GridSize::new(10, 0).is_none());
    assert!(GridSize::new(MAX_GRID_COLS + 1, 1).is_none());
    assert!(GridSize::new(1, MAX_GRID_ROWS + 1).is_none());
    assert!(GridSize::new(1, 1).is_some());
    assert!(GridSize::new(MAX_GRID_COLS, MAX_GRID_ROWS).is_some());
    let mut w = Writer::new();
    w.u16(MAX_GRID_COLS + 1);
    w.u16(24);
    assert_eq!(
        ClientMsg::decode(msg_type::EXPLICIT_RESIZE, w.as_slice()),
        Err(WireError::InvalidValue("grid dimensions out of range"))
    );

    // SubFlags (reserved bits and BACKPRESSURE_PTY without RAW_OUTPUT)
    assert_eq!(SubFlags::VIEW, 0x01);
    assert_eq!(SubFlags::EVENTS, 0x02);
    assert_eq!(SubFlags::RAW_OUTPUT, 0x04);
    assert_eq!(SubFlags::BACKPRESSURE_PTY, 0x08);
    assert_eq!(SubFlags::ALL_MASK, 0x0F);
    assert!(SubFlags::from_bits(0x00).is_some());
    assert!(SubFlags::from_bits(0x10).is_none());
    assert!(SubFlags::from_bits(SubFlags::BACKPRESSURE_PTY).is_none());
    assert!(SubFlags::from_bits(SubFlags::BACKPRESSURE_PTY | SubFlags::RAW_OUTPUT).is_some());
    let flags = SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap();
    assert_eq!(flags.bits(), 0x03);
    assert!(flags.contains(SubFlags::VIEW));
    assert!(!flags.contains(SubFlags::RAW_OUTPUT));
    let mut w = Writer::new();
    sample_sid(1).encode(&mut w);
    sample_nz_stream(1).encode(&mut w);
    w.u8(0x10);
    assert_eq!(
        ClientMsg::decode(msg_type::ATTACH, w.as_slice()),
        Err(WireError::InvalidValue("unknown SubFlags bits"))
    );

    // PackedRow flag constants
    assert_eq!(PackedRow::FLAG_WRAPPED, 0x01);
    assert_eq!(PackedRow::FLAG_PROMPT, 0x02);
    assert_eq!(PackedRow::FLAG_HAS_BLINK, 0x04);
    assert_eq!(PackedRow::FLAG_HAS_PLACEHOLDER, 0x08);

    // LeaseGraceMs, ResumeToken (redacted), and LeaseKind invariants
    assert!(LeaseGraceMs::new(0).is_none());
    assert_eq!(LeaseGraceMs::new(1).unwrap().get(), 1);
    assert_eq!(LeaseGraceMs::new(MAX_LEASE_GRACE_MS).unwrap().get(), MAX_LEASE_GRACE_MS);
    assert!(LeaseGraceMs::new(MAX_LEASE_GRACE_MS + 1).is_none());
    assert_eq!(ResumeToken::new([0u8; 32]), None);
    let tok = ResumeToken::new([0x5A; 32]).unwrap();
    assert_eq!(tok.as_bytes(), &[0x5A; 32]);
    assert_eq!(format!("{tok:?}"), "ResumeToken([REDACTED])");

    for kind in [
        LeaseKind::BrokerImmediate,
        LeaseKind::BrokerGrace { grace_ms: LeaseGraceMs::new(MAX_LEASE_GRACE_MS).unwrap(), resume_token: tok },
        LeaseKind::Session(sample_sid(4)),
    ] {
        let msg = ClientMsg::AcquireLease { kind };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()), Ok(msg));
    }

    for bad_grace in [0u32, MAX_LEASE_GRACE_MS + 1] {
        let mut w = Writer::new();
        w.u8(1);
        w.var_u32(bad_grace);
        w.raw(&[0; 32]);
        assert_eq!(
            ClientMsg::decode(msg_type::ACQUIRE_LEASE, w.as_slice()),
            Err(WireError::LimitExceeded("lease grace_ms"))
        );
    }
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

    // GetImageBlob max_bytes == 0 rejected
    let mut w = Writer::new();
    sample_blob(1).encode(&mut w);
    w.var_u32(0);
    w.var_u32(0);
    assert_eq!(
        ClientMsg::decode(msg_type::GET_IMAGE_BLOB, w.as_slice()),
        Err(WireError::InvalidValue("get_image_blob max_bytes must be non-zero"))
    );

    // ImageBlobChunk bounds and overflow checks
    let ok_chunk = BrokerMsg::ImageBlobChunk {
        blob: sample_blob(1),
        encoding: ImageEncoding::Png,
        total_bytes: BULK_CHUNK_BYTES as u32,
        offset: 0,
        data: vec![0xAA; BULK_CHUNK_BYTES],
    };
    let mut w = Writer::new();
    ok_chunk.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::IMAGE_BLOB_CHUNK, w.as_slice()), Ok(ok_chunk));

    // Chunk larger than BULK_CHUNK_BYTES rejected
    let mut w = Writer::new();
    sample_blob(1).encode(&mut w);
    ImageEncoding::Png.encode(&mut w);
    w.var_u32((BULK_CHUNK_BYTES + 1) as u32);
    w.var_u32(0);
    w.var_u32((BULK_CHUNK_BYTES + 1) as u32);
    assert_eq!(
        BrokerMsg::decode(msg_type::IMAGE_BLOB_CHUNK, w.as_slice()),
        Err(WireError::LimitExceeded("image blob chunk"))
    );

    // offset + data.len() > total_bytes rejected
    let mut w = Writer::new();
    sample_blob(1).encode(&mut w);
    ImageEncoding::Png.encode(&mut w);
    w.var_u32(10);
    w.var_u32(8);
    w.bytes(&[1, 2, 3]);
    assert_eq!(
        BrokerMsg::decode(msg_type::IMAGE_BLOB_CHUNK, w.as_slice()),
        Err(WireError::InvalidValue("image blob chunk exceeds total_bytes"))
    );

    // offset + data.len() u32 overflow rejected
    let mut w = Writer::new();
    sample_blob(1).encode(&mut w);
    ImageEncoding::Png.encode(&mut w);
    w.var_u32(u32::MAX);
    w.var_u32(u32::MAX);
    w.bytes(&[1]);
    assert_eq!(
        BrokerMsg::decode(msg_type::IMAGE_BLOB_CHUNK, w.as_slice()),
        Err(WireError::InvalidValue("image blob chunk offset overflow"))
    );

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

    // SearchDelta invariants
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

    // SearchDelta span below from_line or at/above to_line rejected
    for bad_span in [
        CellSpan { start: CellPos { line: 9, col: 0 }, end: CellPos { line: 10, col: 1 } },
        CellSpan { start: CellPos { line: 10, col: 0 }, end: CellPos { line: 20, col: 0 } },
    ] {
        let bad_delta =
            BrokerMsg::SearchDelta { epoch: 1, from_line: 10, to_line: 20, added: vec![bad_span], truncated: false };
        let mut w = Writer::new();
        bad_delta.encode(&mut w);
        assert_eq!(
            BrokerMsg::decode(msg_type::SEARCH_DELTA, w.as_slice()),
            Err(WireError::InvalidValue("search delta span outside [from_line, to_line)"))
        );
    }

    // Progress > 100 and invalid state/percent combinations rejected
    let mut w = Writer::new();
    w.u8(1);
    w.u8(1);
    w.u8(101);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRESS, w.as_slice()),
        Err(WireError::InvalidValue("progress percent > 100"))
    );
    for state in [ProgressState::None, ProgressState::Indeterminate] {
        let mut w = Writer::new();
        BrokerMsg::Progress { state, percent: Some(50) }.encode(&mut w);
        assert_eq!(
            BrokerMsg::decode(msg_type::PROGRESS, w.as_slice()),
            Err(WireError::InvalidValue("progress percent invalid for state"))
        );
    }
    let ok_prog = BrokerMsg::Progress { state: ProgressState::Running, percent: Some(100) };
    let mut w = Writer::new();
    ok_prog.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::PROGRESS, w.as_slice()), Ok(ok_prog));

    // StatusRecord progress > 100 and non-Blocked kind rejected
    let mut bad_status = sample_status(StatusState::Working, None, Confirmed::Unknown);
    bad_status.progress = Some(101);
    let mut w = Writer::new();
    BrokerMsg::ProgramStatus(Box::new(bad_status)).encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRAM_STATUS, w.as_slice()),
        Err(WireError::InvalidValue("progress must be 0..=100"))
    );
    let bad_kind = sample_status(StatusState::Working, Some(BlockedKind::Auth), Confirmed::Unknown);
    let mut w = Writer::new();
    BrokerMsg::ProgramStatus(Box::new(bad_kind)).encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRAM_STATUS, w.as_slice()),
        Err(WireError::InvalidValue("status kind requires Blocked state"))
    );
    let mut ok_status = sample_status(StatusState::Working, None, Confirmed::Unknown);
    ok_status.progress = Some(100);
    let mut w = Writer::new();
    BrokerMsg::ProgramStatus(Box::new(ok_status.clone())).encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::PROGRAM_STATUS, w.as_slice()),
        Ok(BrokerMsg::ProgramStatus(Box::new(ok_status)))
    );

    // KeyInput modifiers and Unicode scalar validation
    let mut w = Writer::new();
    ClientMsg::Key(KeyInput {
        code: "KeyA".to_owned(),
        action: KeyAction::Press,
        modifiers: 0x40,
        text: String::new(),
        unshifted_codepoint: 0,
        composed: false,
    })
    .encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::KEY, w.as_slice()),
        Err(WireError::InvalidValue("unknown KeyInput modifier bits"))
    );
    let mut w = Writer::new();
    ClientMsg::Key(KeyInput {
        code: "KeyA".to_owned(),
        action: KeyAction::Press,
        modifiers: 0x3F,
        text: String::new(),
        unshifted_codepoint: 0xD800,
        composed: false,
    })
    .encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::KEY, w.as_slice()),
        Err(WireError::InvalidValue("invalid Unicode scalar codepoint"))
    );

    // PointerInput button and modifiers validation
    for ok_btn in [0u8, 1, 2, 255] {
        let msg = ClientMsg::Pointer(PointerInput {
            action: PointerAction::Press,
            button: ok_btn,
            modifiers: 0x3F,
            x_px: 0,
            y_px: 0,
            cell_px: CellPx::default(),
        });
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::POINTER, w.as_slice()), Ok(msg));
    }
    let mut w = Writer::new();
    ClientMsg::Pointer(PointerInput {
        action: PointerAction::Press,
        button: 3,
        modifiers: 0,
        x_px: 0,
        y_px: 0,
        cell_px: CellPx::default(),
    })
    .encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::POINTER, w.as_slice()),
        Err(WireError::InvalidValue("invalid PointerInput button"))
    );
    let mut w = Writer::new();
    ClientMsg::Pointer(PointerInput {
        action: PointerAction::Press,
        button: 0,
        modifiers: 0x40,
        x_px: 0,
        y_px: 0,
        cell_px: CellPx::default(),
    })
    .encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::POINTER, w.as_slice()),
        Err(WireError::InvalidValue("unknown PointerInput modifier bits"))
    );

    // WaitCondition rejects empty Text and empty Status
    let mut w = Writer::new();
    ClientMsg::Wait { session: sample_sid(1), condition: WaitCondition::Text(String::new()), timeout_ms: 1 }
        .encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::WAIT, w.as_slice()),
        Err(WireError::InvalidValue("wait text needle must be non-empty"))
    );
    let mut w = Writer::new();
    ClientMsg::Wait { session: sample_sid(1), condition: WaitCondition::Status(vec![]), timeout_ms: 1 }.encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::WAIT, w.as_slice()),
        Err(WireError::InvalidValue("wait status list must be non-empty"))
    );
}

#[test]
fn packed_row_style_piece_and_run_invariants() {
    // Valid PackedStyle colour & flag boundaries
    assert!(PackedStyle::is_valid_color(0));
    assert!(PackedStyle::is_valid_color(256));
    assert!(!PackedStyle::is_valid_color(257));
    assert!(!PackedStyle::is_valid_color(0x00_FF_FF_FF));
    assert!(PackedStyle::is_valid_color(0x01_00_00_00));
    assert!(PackedStyle::is_valid_color(0x01_FF_FF_FF));
    assert!(!PackedStyle::is_valid_color(0x02_00_00_00));

    let row = sample_row();
    assert_eq!(row.style(0), PackedStyle::default());
    assert_eq!(row.style(1), row.styles[1]);
    assert_eq!(row.link(0), "");
    assert_eq!(row.link(1), "https://example.com");

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
    assert_eq!(plain.style(0), PackedStyle::default());
    assert_eq!(plain.link(0), "");
    let ok_msg = BrokerMsg::Rows { epoch: 1, rows: vec![plain.clone()] };
    let mut w = Writer::new();
    ok_msg.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::ROWS, w.as_slice()), Ok(ok_msg));

    // Valid max PackedStyle flags (0x03F7: underline=5 + all other bits)
    let mut max_style_row = plain.clone();
    max_style_row.styles = vec![
        PackedStyle::default(),
        PackedStyle { fg: 256, bg: 0x01_00_00_00, underline_color: 0x01_FF_FF_FF, flags: 0x03F7 },
    ];
    max_style_row.runs[0].style_idx = 1;
    let ok_max_style = BrokerMsg::Rows { epoch: 1, rows: vec![max_style_row] };
    let mut w = Writer::new();
    ok_max_style.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::ROWS, w.as_slice()), Ok(ok_max_style));

    // Invalid PackedStyle colors & flags
    for bad_style in [
        PackedStyle { fg: 257, ..PackedStyle::default() },
        PackedStyle { bg: 0x02_00_00_00, ..PackedStyle::default() },
        PackedStyle { underline_color: 300, ..PackedStyle::default() },
    ] {
        let mut r = plain.clone();
        r.styles = vec![PackedStyle::default(), bad_style];
        let mut w = Writer::new();
        BrokerMsg::Rows { epoch: 1, rows: vec![r] }.encode(&mut w);
        assert_eq!(
            BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
            Err(WireError::InvalidValue("invalid PackedStyle colour encoding"))
        );
    }
    for bad_flags in [0x0400, 0x0018] {
        let mut r = plain.clone();
        r.styles = vec![PackedStyle::default(), PackedStyle { flags: bad_flags, ..PackedStyle::default() }];
        let mut w = Writer::new();
        BrokerMsg::Rows { epoch: 1, rows: vec![r] }.encode(&mut w);
        assert_eq!(
            BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
            Err(WireError::InvalidValue("invalid PackedStyle flags"))
        );
    }

    // Non-default styles[0] and non-empty links[0] rejected
    let mut bad_s0 = plain.clone();
    bad_s0.styles = vec![PackedStyle { fg: 1, ..PackedStyle::default() }];
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_s0] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("styles[0] must be default style"))
    );
    let mut bad_l0 = plain.clone();
    bad_l0.links = vec!["https://not-empty".to_owned()];
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_l0] }.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::ROWS, w.as_slice()), Err(WireError::InvalidValue("links[0] must be empty")));

    // Non-empty pieces without FLAG_HAS_PLACEHOLDER rejected
    let mut bad_piece_flag = sample_row();
    bad_piece_flag.flags &= !PackedRow::FLAG_HAS_PLACEHOLDER;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_piece_flag] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("pieces require FLAG_HAS_PLACEHOLDER"))
    );

    // Invalid ImagePiece geometry
    let valid_piece = sample_row().pieces[0];
    for (bad_piece, expected) in [
        (ImagePiece { col_len: 0, ..valid_piece }, "invalid ImagePiece column span"),
        (ImagePiece { col_start: u16::MAX, col_len: 1, ..valid_piece }, "invalid ImagePiece column span"),
        (ImagePiece { src_rect: [0, 0, 0, 1], ..valid_piece }, "invalid ImagePiece src_rect"),
        (ImagePiece { src_rect: [0, 0, 1, 0], ..valid_piece }, "invalid ImagePiece src_rect"),
        (ImagePiece { src_rect: [u32::MAX, 0, 1, 1], ..valid_piece }, "invalid ImagePiece src_rect"),
        (ImagePiece { src_rect: [0, u32::MAX, 1, 1], ..valid_piece }, "invalid ImagePiece src_rect"),
        (ImagePiece { dst_px: [0, 10], ..valid_piece }, "invalid ImagePiece dst_px"),
        (ImagePiece { dst_px: [10, 0], ..valid_piece }, "invalid ImagePiece dst_px"),
    ] {
        let mut r = sample_row();
        r.pieces = vec![bad_piece];
        let mut w = Writer::new();
        BrokerMsg::Rows { epoch: 1, rows: vec![r] }.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg_type::ROWS, w.as_slice()), Err(WireError::InvalidValue(expected)));
    }

    // Invalid PackedRun column span
    for (col_start, col_len) in [(0, 0), (u16::MAX, 1)] {
        let mut bad = plain.clone();
        bad.runs[0].col_start = col_start;
        bad.runs[0].col_len = col_len;
        let mut w = Writer::new();
        BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
        assert_eq!(
            BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
            Err(WireError::InvalidValue("invalid PackedRun column span"))
        );
    }

    // Unknown row flag bit & run flag bit
    let mut bad = plain.clone();
    bad.flags = 0x10;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("unknown PackedRow flag bits"))
    );

    let mut bad = plain.clone();
    bad.runs[0].flags = 2;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("unknown PackedRun flag bits"))
    );

    // Out-of-bounds byte_len and mid-UTF-8 start/end boundaries
    let mut bad = plain.clone();
    bad.runs[0].byte_len = 3;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run byte range out of bounds or mid-UTF-8"))
    );

    for (byte_offset, byte_len) in [(0, 1), (1, 1)] {
        let mut bad_utf8 = plain.clone();
        bad_utf8.text = "é".to_owned();
        bad_utf8.runs[0].byte_offset = byte_offset;
        bad_utf8.runs[0].byte_len = byte_len;
        let mut w = Writer::new();
        BrokerMsg::Rows { epoch: 1, rows: vec![bad_utf8] }.encode(&mut w);
        assert_eq!(
            BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
            Err(WireError::InvalidValue("run byte range out of bounds or mid-UTF-8"))
        );
    }

    // Exact boundary style_idx == styles.len() (1 == 1) and link_idx == links.len() (1 == 1)
    let mut bad_style = plain.clone();
    bad_style.styles = vec![PackedStyle::default()];
    bad_style.runs[0].style_idx = 1;
    let mut w = Writer::new();
    BrokerMsg::Rows { epoch: 1, rows: vec![bad_style] }.encode(&mut w);
    assert_eq!(
        BrokerMsg::decode(msg_type::ROWS, w.as_slice()),
        Err(WireError::InvalidValue("run style_idx out of bounds"))
    );

    let mut bad_link = plain;
    bad_link.links = vec![String::new()];
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
                first_active_line: 0,
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
    w.var_u64(0);
    w.i32(0);
    w.u16(0);
    w.u16(0);
    w.u8(0);
    w.u8(0x04);
    assert_eq!(
        BrokerMsg::decode(msg_type::FRAME, w.as_slice()),
        Err(WireError::InvalidValue("unknown CursorState flag bits"))
    );

    let mut w = Writer::new();
    w.var_u64(1);
    w.var_u64(1);
    sample_grid().encode(&mut w);
    w.var_u64(0);
    w.var_u64(0);
    w.i32(0);
    w.u16(0);
    w.u16(0);
    w.u8(0);
    w.u8(0);
    w.u8(0x10);
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

    // FrameUpdate & Attached: scrollback_lines == first_active_line is valid; scrollback_lines > first_active_line is rejected
    let ok_attached = BrokerMsg::Attached {
        session: SessionId::new(1).unwrap(),
        stream_id: NonZeroStreamId::new(1).unwrap(),
        size: sample_grid(),
        epoch: 1,
        first_active_line: 10,
        scrollback_lines: 10,
        foreground: None,
        statuses: Vec::new(),
    };
    let mut w = Writer::new();
    ok_attached.encode(&mut w);
    assert_eq!(BrokerMsg::decode(msg_type::ATTACHED, w.as_slice()), Ok(ok_attached));

    let bad_attached = BrokerMsg::Attached {
        session: SessionId::new(1).unwrap(),
        stream_id: NonZeroStreamId::new(1).unwrap(),
        size: sample_grid(),
        epoch: 1,
        first_active_line: 10,
        scrollback_lines: 11,
        foreground: None,
        statuses: Vec::new(),
    };
    let mut w = Writer::new();
    bad_attached.encode(&mut w);
    assert!(BrokerMsg::decode(msg_type::ATTACHED, w.as_slice()).is_err());

    let bad_frame = BrokerMsg::Frame(Box::new(FrameUpdate {
        seq: 1,
        epoch: 1,
        size: sample_grid(),
        first_active_line: 10,
        scrollback_lines: 11,
        scroll_delta: 0,
        cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
        modes: ViewModes::default(),
        palette: None,
        rows: vec![],
        images_gen: 0,
        full_sync: true,
    }));
    let mut w = Writer::new();
    bad_frame.encode(&mut w);
    assert!(BrokerMsg::decode(msg_type::FRAME, w.as_slice()).is_err());
}

#[test]
fn packed_row_text_cap_links_resume_token_and_image_placement_validation() {
    // 1. PackedRow text exact boundary: MAX_PACKED_ROW_TEXT_BYTES passes, +1 fails with LimitExceeded.
    let exact_text = "a".repeat(crate::MAX_PACKED_ROW_TEXT_BYTES);
    let exact_row = PackedRow {
        line: 0,
        flags: 0,
        text: exact_text,
        runs: vec![PackedRun {
            col_start: 0,
            col_len: 1,
            byte_offset: (crate::MAX_PACKED_ROW_TEXT_BYTES - 1) as u32,
            byte_len: 1,
            style_idx: 0,
            link_idx: 0,
            flags: 0,
        }],
        styles: vec![],
        links: vec![],
        pieces: vec![],
    };
    let mut w = Writer::new();
    exact_row.encode(&mut w);
    let mut r = Reader::new(w.as_slice());
    assert_eq!(PackedRow::decode(&mut r).unwrap(), exact_row);

    let over_row = PackedRow {
        line: 0,
        flags: 0,
        text: "a".repeat(crate::MAX_PACKED_ROW_TEXT_BYTES + 1),
        runs: vec![],
        styles: vec![],
        links: vec![],
        pieces: vec![],
    };
    let mut w = Writer::new();
    over_row.encode(&mut w);
    let mut r = Reader::new(w.as_slice());
    assert_eq!(PackedRow::decode(&mut r), Err(WireError::LimitExceeded("packed row text")));

    // 2. PackedRow links[1..] rejects empty non-zero entries.
    for bad_links in
        [vec![String::new(), String::new()], vec![String::new(), "https://a.test".to_owned(), String::new()]]
    {
        let bad_row = PackedRow {
            line: 0,
            flags: 0,
            text: String::new(),
            runs: vec![],
            styles: vec![],
            links: bad_links,
            pieces: vec![],
        };
        let mut w = Writer::new();
        bad_row.encode(&mut w);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(PackedRow::decode(&mut r), Err(WireError::InvalidValue("non-zero link entry must be non-empty")));
    }

    // 3. LeaseKind::BrokerGrace rejects all-zero resume token on decode.
    let mut w = Writer::new();
    w.u8(LeaseKind::TAG_BROKER_GRACE);
    w.var_u32(1_000);
    w.raw(&[0u8; 32]);
    let mut r = Reader::new(w.as_slice());
    assert_eq!(LeaseKind::decode(&mut r), Err(WireError::InvalidValue("resume_token must be non-zero")));

    // 4. ImagePlacement validation: source_rect zero/overflow, pixel_size zero, cell_span zero.
    let valid_placement = ImagePlacement {
        blob: sample_blob(3),
        line: 10,
        col: -1,
        source_rect: [4, 8, 64, 32],
        cell_offset: [3, 5],
        pixel_size: [128, 64],
        cell_span: [8, 4],
        z_index: 2,
    };
    let mut w = Writer::new();
    valid_placement.encode(&mut w);
    let mut r = Reader::new(w.as_slice());
    assert_eq!(ImagePlacement::decode(&mut r).unwrap(), valid_placement);

    let invalid_placements = [
        (ImagePlacement { source_rect: [0, 0, 0, 10], ..valid_placement }, "invalid ImagePlacement source_rect"),
        (ImagePlacement { source_rect: [0, 0, 10, 0], ..valid_placement }, "invalid ImagePlacement source_rect"),
        (ImagePlacement { source_rect: [u32::MAX, 0, 1, 10], ..valid_placement }, "invalid ImagePlacement source_rect"),
        (ImagePlacement { source_rect: [0, u32::MAX, 10, 1], ..valid_placement }, "invalid ImagePlacement source_rect"),
        (ImagePlacement { pixel_size: [0, 10], ..valid_placement }, "invalid ImagePlacement pixel_size"),
        (ImagePlacement { pixel_size: [10, 0], ..valid_placement }, "invalid ImagePlacement pixel_size"),
        (ImagePlacement { cell_span: [0, 4], ..valid_placement }, "invalid ImagePlacement cell_span"),
        (ImagePlacement { cell_span: [8, 0], ..valid_placement }, "invalid ImagePlacement cell_span"),
    ];
    for (bad, reason) in invalid_placements {
        let mut w = Writer::new();
        bad.encode(&mut w);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(ImagePlacement::decode(&mut r), Err(WireError::InvalidValue(reason)));
    }

    // 5. SpawnSpec::scrollback_limit exact boundary: 0 and MAX_SCROLLBACK_LINES accepted; +1 rejected.
    let base_spawn = |limit: u32| {
        ClientMsg::Spawn(Box::new(SpawnSpec {
            argv: vec![],
            cwd: None,
            env: vec![],
            size: GridSize::new(80, 24).unwrap(),
            cell_px: CellPx::default(),
            scrollback_limit: limit,
            marks_key: None,
            resize_policy: ResizePolicy::ExplicitOverride,
            record_dir: None,
            restore_from: None,
            image_files: false,
            kept: false,
        }))
    };
    for ok_limit in [0u32, crate::MAX_SCROLLBACK_LINES] {
        let msg = base_spawn(ok_limit);
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(ClientMsg::decode(msg_type::SPAWN, w.as_slice()), Ok(msg));
    }
    let mut w = Writer::new();
    base_spawn(crate::MAX_SCROLLBACK_LINES + 1).encode(&mut w);
    assert_eq!(
        ClientMsg::decode(msg_type::SPAWN, w.as_slice()),
        Err(WireError::LimitExceeded("spawn scrollback_limit"))
    );
}
