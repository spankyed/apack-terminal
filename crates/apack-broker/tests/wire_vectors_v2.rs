//! Golden v2 wire & mutual-HMAC test vectors shared with `@apackdev/terminal-view`.
//!
//! Run `UPDATE_VECTORS=1 cargo test -p apack-broker --test wire_vectors_v2` to regenerate
//! `node/wire-vectors-v2.json`; normal test runs verify that the committed vectors match
//! `apack-proto` and `apack-core::auth` byte-for-byte.

use std::fmt::Write as _;
use std::path::PathBuf;

use apack_core::auth::{AuthSecret, compute_client_proof, compute_server_proof};
use apack_proto::codec::Writer;
use apack_proto::frame::{DecodedFrame, FrameDecoder, encode_fin, encode_frame};
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SessionSummary, SpawnSpec, msg_type};
use apack_proto::types::{
    BlobHash, BlockedKind, CellPos, CellPx, CellSpan, CommandOutcome, CommandRecord, Confirmed, CursorShape,
    CursorState, ErrorCode, ForegroundProcess, FrameCredit, GridSize, ImageEncoding, ImagePiece, ImagePlacement,
    KeyAction, KeyInput, LeaseGraceMs, LeaseKind, MarksKey, MetricsSnapshot, NonZeroStreamId, PackedRow, PackedRun,
    PackedStyle, PaletteOverrides, PasteOutcome, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy,
    ResumeToken, Rgb, SessionId, SignalKind, SignalScope, SignalTarget, StatusRecord, StatusState, StreamId, SubFlags,
    UnknownReason, ViewModes, ViewportHint, VouchedExitCode, WaitCondition, WaitOutcome,
};
use apack_proto::{FIN_FRAME, PROTOCOL_VERSION};

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(&mut out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct ClientVector {
    name: &'static str,
    stream_id: StreamId,
    req_id: RequestId,
    msg: ClientMsg,
}

struct BrokerVector {
    name: &'static str,
    stream_id: StreamId,
    req_id: RequestId,
    msg: BrokerMsg,
}

struct MalformedVector {
    name: &'static str,
    direction: &'static str,
    hex: String,
    reason: &'static str,
}

fn sample_packed_row() -> PackedRow {
    // Rich row with ASCII + 2-cell CJK + 2-cell emoji + styles + hyperlink + ImagePiece
    let text = String::from("Hi 世界🚀");
    let hi_len = "Hi ".len() as u32;
    let cjk_off = hi_len;
    let cjk_len = "世界".len() as u32;
    let emoji_off = cjk_off + cjk_len;
    let emoji_len = "🚀".len() as u32;
    PackedRow {
        line: 42,
        flags: PackedRow::FLAG_WRAPPED
            | PackedRow::FLAG_PROMPT
            | PackedRow::FLAG_HAS_BLINK
            | PackedRow::FLAG_HAS_PLACEHOLDER,
        text,
        runs: vec![
            PackedRun {
                col_start: 0,
                col_len: 3,
                byte_offset: 0,
                byte_len: hi_len,
                style_idx: 1,
                link_idx: 1,
                flags: 0,
            },
            PackedRun {
                col_start: 3,
                col_len: 4,
                byte_offset: cjk_off,
                byte_len: cjk_len,
                style_idx: 2,
                link_idx: 0,
                flags: 1,
            },
            PackedRun {
                col_start: 7,
                col_len: 2,
                byte_offset: emoji_off,
                byte_len: emoji_len,
                style_idx: 0,
                link_idx: 0,
                flags: 1,
            },
        ],
        styles: vec![
            PackedStyle::default(),
            PackedStyle {
                fg: 0x01_ff_88_00,
                bg: 1 + 17,
                underline_color: 0x01_00_cc_ff,
                flags: PackedStyle::FLAG_BOLD
                    | PackedStyle::FLAG_ITALIC
                    | (3 << 2) // curly underline
                    | PackedStyle::FLAG_STRIKETHROUGH
                    | PackedStyle::FLAG_BLINK,
            },
            PackedStyle {
                fg: 1 + 200,
                bg: 0,
                underline_color: 0,
                flags: PackedStyle::FLAG_INVERSE | PackedStyle::FLAG_FAINT | (5 << 2), // dashed underline
            },
        ],
        links: vec![String::new(), String::from("https://example.com/docs")],
        pieces: vec![ImagePiece {
            blob: BlobHash([0xAB; 32]),
            col_start: 10,
            col_len: 4,
            src_rect: [0, 16, 64, 32],
            dst_px: [32, 16],
        }],
    }
}

fn build_client_vectors() -> Vec<ClientVector> {
    let sid = SessionId::new(7).unwrap();
    let stream = NonZeroStreamId::new(3).unwrap();
    let s3 = stream.as_stream_id();
    let ctl = StreamId::CONTROL;
    let r = |n| RequestId::from_raw(n);
    let push = RequestId::PUSH;

    vec![
        ClientVector {
            name: "hello",
            stream_id: ctl,
            req_id: r(1),
            msg: ClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_nonce: [0x11; 32],
                client_name: String::from("terminal-view-v2"),
            },
        },
        ClientVector {
            name: "auth_proof",
            stream_id: ctl,
            req_id: r(2),
            msg: ClientMsg::AuthProof { hmac: [0x22; 32] },
        },
        ClientVector {
            name: "acquire_lease_immediate",
            stream_id: ctl,
            req_id: r(3),
            msg: ClientMsg::AcquireLease { kind: LeaseKind::BrokerImmediate },
        },
        ClientVector {
            name: "acquire_lease_grace",
            stream_id: ctl,
            req_id: r(4),
            msg: ClientMsg::AcquireLease {
                kind: LeaseKind::BrokerGrace {
                    grace_ms: LeaseGraceMs::new(30_000).unwrap(),
                    resume_token: ResumeToken::new([0x33; 32]).unwrap(),
                },
            },
        },
        ClientVector {
            name: "acquire_lease_session",
            stream_id: ctl,
            req_id: r(5),
            msg: ClientMsg::AcquireLease { kind: LeaseKind::Session(sid) },
        },
        ClientVector {
            name: "release_lease",
            stream_id: ctl,
            req_id: r(6),
            msg: ClientMsg::ReleaseLease { lease_id: 99 },
        },
        ClientVector {
            name: "spawn",
            stream_id: ctl,
            req_id: r(7),
            msg: ClientMsg::Spawn(Box::new(SpawnSpec {
                argv: vec![String::from("/bin/sh"), String::from("-i")],
                cwd: Some(String::from("/tmp")),
                env: vec![(String::from("TERM"), String::from("xterm-256color"))],
                size: GridSize::new(120, 40).unwrap(),
                cell_px: CellPx { width: 9, height: 18 },
                scrollback_limit: 10_000,
                marks_key: Some(MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap()),
                resize_policy: ResizePolicy::MinFit,
                record_dir: Some(String::from("/tmp/rec")),
                restore_from: None,
                image_files: false,
                kept: true,
            })),
        },
        ClientVector { name: "kill", stream_id: ctl, req_id: r(8), msg: ClientMsg::Kill { session: sid } },
        ClientVector {
            name: "signal_pid",
            stream_id: ctl,
            req_id: r(9),
            msg: ClientMsg::Signal {
                session: sid,
                signal: SignalKind::Term,
                scope: SignalScope::Pid(SignalTarget::new(4242).unwrap()),
            },
        },
        ClientVector {
            name: "owns",
            stream_id: ctl,
            req_id: r(10),
            msg: ClientMsg::Owns { session: sid, pid: SignalTarget::new(4242).unwrap() },
        },
        ClientVector { name: "list_sessions", stream_id: ctl, req_id: r(11), msg: ClientMsg::ListSessions },
        ClientVector {
            name: "rewind",
            stream_id: ctl,
            req_id: r(12),
            msg: ClientMsg::Rewind { record_dir: String::from("/tmp/rec"), at_ms: Some(1234) },
        },
        ClientVector {
            name: "seek",
            stream_id: ctl,
            req_id: r(13),
            msg: ClientMsg::Seek { session: sid, at_ms: 5678 },
        },
        ClientVector { name: "metrics", stream_id: ctl, req_id: r(14), msg: ClientMsg::Metrics },
        ClientVector {
            name: "attach",
            stream_id: ctl,
            req_id: r(15),
            msg: ClientMsg::Attach {
                session: sid,
                stream_id: stream,
                flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
                initial_viewport: Some(ViewportHint {
                    size: GridSize::new(80, 24).unwrap(),
                    cell_px: CellPx { width: 8, height: 16 },
                    visible: true,
                    focused: true,
                }),
                initial_output_credit: 65_536,
                initial_frame_credit: FrameCredit { max_frames: 4, max_bytes: 262_144 },
            },
        },
        ClientVector { name: "detach", stream_id: ctl, req_id: r(16), msg: ClientMsg::Detach { stream_id: stream } },
        ClientVector { name: "keep", stream_id: ctl, req_id: r(17), msg: ClientMsg::Keep { session: sid, kept: true } },
        ClientVector {
            name: "wait_status",
            stream_id: ctl,
            req_id: r(18),
            msg: ClientMsg::Wait {
                session: sid,
                condition: WaitCondition::Status(vec![StatusState::Done, StatusState::Blocked]),
                timeout_ms: 5_000,
            },
        },
        ClientVector {
            name: "frame_ack",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::FrameAck { seq: 15, credit: FrameCredit { max_frames: 2, max_bytes: 131_072 } },
        },
        ClientVector {
            name: "viewport_hint",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::ViewportHint(ViewportHint {
                size: GridSize::new(100, 30).unwrap(),
                cell_px: CellPx { width: 9, height: 18 },
                visible: true,
                focused: false,
            }),
        },
        ClientVector {
            name: "explicit_resize",
            stream_id: s3,
            req_id: r(19),
            msg: ClientMsg::ExplicitResize {
                size: GridSize::new(100, 30).unwrap(),
                cell_px: CellPx { width: 9, height: 18 },
            },
        },
        ClientVector {
            name: "set_cursor_defaults",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::SetCursorDefaults { shape: CursorShape::Bar, blinking: true },
        },
        ClientVector {
            name: "key",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::Key(KeyInput {
                code: String::from("KeyC"),
                action: KeyAction::Press,
                modifiers: 0x02, // ctrl
                text: String::from("\u{0003}"),
                unshifted_codepoint: 'c' as u32,
                composed: false,
            }),
        },
        ClientVector {
            name: "text",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::Text { text: String::from("こんにちは") },
        },
        ClientVector {
            name: "pointer",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::Pointer(PointerInput {
                action: PointerAction::Press,
                button: 0,
                modifiers: 0x01,
                x_px: 120,
                y_px: 64,
                cell_px: CellPx { width: 8, height: 16 },
            }),
        },
        ClientVector { name: "focus", stream_id: s3, req_id: push, msg: ClientMsg::Focus { focused: true } },
        ClientVector {
            name: "paste",
            stream_id: s3,
            req_id: r(20),
            msg: ClientMsg::Paste { text: String::from("echo hi\n"), allow_unsafe: false },
        },
        ClientVector { name: "write", stream_id: s3, req_id: push, msg: ClientMsg::Write { data: b"ls\r\n".to_vec() } },
        ClientVector { name: "pause", stream_id: s3, req_id: push, msg: ClientMsg::Pause },
        ClientVector { name: "resume", stream_id: s3, req_id: push, msg: ClientMsg::Resume },
        ClientVector {
            name: "output_credit",
            stream_id: s3,
            req_id: push,
            msg: ClientMsg::OutputCredit { bytes: 32_768 },
        },
        ClientVector {
            name: "fetch_rows",
            stream_id: s3,
            req_id: r(21),
            msg: ClientMsg::FetchRows { epoch: 4, from_line: 100, count: 64 },
        },
        ClientVector {
            name: "word_at",
            stream_id: s3,
            req_id: r(22),
            msg: ClientMsg::WordAt { epoch: 4, pos: CellPos { line: 42, col: 5 } },
        },
        ClientVector { name: "line_at", stream_id: s3, req_id: r(23), msg: ClientMsg::LineAt { epoch: 4, line: 42 } },
        ClientVector {
            name: "text_of",
            stream_id: s3,
            req_id: r(24),
            msg: ClientMsg::TextOf {
                epoch: 4,
                span: CellSpan { start: CellPos { line: 40, col: 2 }, end: CellPos { line: 42, col: 9 } },
            },
        },
        ClientVector {
            name: "text_of_rect",
            stream_id: s3,
            req_id: r(25),
            msg: ClientMsg::TextOfRect {
                epoch: 4,
                span: CellSpan { start: CellPos { line: 40, col: 2 }, end: CellPos { line: 42, col: 9 } },
            },
        },
        ClientVector {
            name: "set_search",
            stream_id: s3,
            req_id: r(26),
            msg: ClientMsg::SetSearch { query: String::from("error"), case_sensitive: false },
        },
        ClientVector { name: "commands", stream_id: s3, req_id: r(27), msg: ClientMsg::Commands },
        ClientVector {
            name: "command_output",
            stream_id: s3,
            req_id: r(28),
            msg: ClientMsg::CommandOutput { command_id: 12 },
        },
        ClientVector { name: "images", stream_id: s3, req_id: r(29), msg: ClientMsg::Images },
        ClientVector {
            name: "get_image_blob",
            stream_id: s3,
            req_id: r(30),
            msg: ClientMsg::GetImageBlob { blob: BlobHash([0xAB; 32]), offset: 0, max_bytes: 65_536 },
        },
    ]
}

fn build_broker_vectors() -> Vec<BrokerVector> {
    let sid = SessionId::new(7).unwrap();
    let stream = NonZeroStreamId::new(3).unwrap();
    let s3 = stream.as_stream_id();
    let ctl = StreamId::CONTROL;
    let r = |n| RequestId::from_raw(n);
    let push = RequestId::PUSH;

    let cmd = CommandRecord {
        id: 12,
        command: Some(String::from("cargo test")),
        cwd: Some(String::from("/repo")),
        started_ms: Some(1_700_000_000_000),
        finished_ms: Some(1_700_000_002_500),
        outcome: CommandOutcome::Exited(VouchedExitCode::from_vouched(0)),
        prompt_line: Some(10),
        output_line: Some(11),
        end_line: Some(42),
    };

    vec![
        BrokerVector {
            name: "hello_ok",
            stream_id: ctl,
            req_id: r(1),
            msg: BrokerMsg::HelloOk {
                protocol_version: PROTOCOL_VERSION,
                auth_required: true,
                server_nonce: [0x44; 32],
                word_delimiters: String::from(" \t`'\"()[]{}<>:;,"),
                capabilities: 0x07,
            },
        },
        BrokerVector {
            name: "auth_ok",
            stream_id: ctl,
            req_id: r(2),
            msg: BrokerMsg::AuthOk { server_hmac: [0x55; 32] },
        },
        BrokerVector {
            name: "error",
            stream_id: ctl,
            req_id: r(3),
            msg: BrokerMsg::Error { code: ErrorCode::StaleEpoch, message: String::from("epoch changed") },
        },
        BrokerVector { name: "ok", stream_id: ctl, req_id: r(4), msg: BrokerMsg::Ok },
        BrokerVector {
            name: "lease_ack",
            stream_id: ctl,
            req_id: r(5),
            msg: BrokerMsg::LeaseAck { lease_id: 99, resumed: true },
        },
        BrokerVector {
            name: "spawned",
            stream_id: ctl,
            req_id: r(6),
            msg: BrokerMsg::Spawned { session: sid, pid: Some(SignalTarget::new(4242).unwrap()) },
        },
        BrokerVector { name: "owns_reply", stream_id: ctl, req_id: r(7), msg: BrokerMsg::OwnsReply { owned: true } },
        BrokerVector {
            name: "sessions_list",
            stream_id: ctl,
            req_id: r(8),
            msg: BrokerMsg::SessionsList {
                sessions: vec![SessionSummary {
                    id: sid,
                    size: GridSize::new(80, 24).unwrap(),
                    alive: true,
                    kept: false,
                    rewound: false,
                    title: String::from("bash"),
                    cwd: Some(String::from("/repo")),
                    pid: Some(SignalTarget::new(4242).unwrap()),
                    foreground: Some(ForegroundProcess {
                        pid: SignalTarget::new(4243).unwrap(),
                        name: Some(String::from("cargo")),
                    }),
                    created_ms: 1_700_000_000_000,
                }],
            },
        },
        BrokerVector {
            name: "rewound",
            stream_id: ctl,
            req_id: r(9),
            msg: BrokerMsg::Rewound { session: sid, duration_ms: 12_000, at_ms: 1_500 },
        },
        BrokerVector { name: "sought", stream_id: ctl, req_id: r(10), msg: BrokerMsg::Sought { at_ms: 5_000 } },
        BrokerVector {
            name: "metrics_reply",
            stream_id: ctl,
            req_id: r(11),
            msg: BrokerMsg::MetricsReply(MetricsSnapshot {
                live_sessions: 2,
                rewound_sessions: 1,
                connections: 3,
                active_timers: 5,
                scrollback_bytes: 1_048_576,
                image_store_bytes: 524_288,
                total_pty_bytes: 9_999_999,
                total_throttled_us: 12_345,
            }),
        },
        BrokerVector {
            name: "attached",
            stream_id: ctl,
            req_id: r(12),
            msg: BrokerMsg::Attached {
                session: sid,
                stream_id: stream,
                size: GridSize::new(80, 24).unwrap(),
                epoch: 4,
                first_active_line: 100,
                scrollback_lines: 80,
                foreground: Some(ForegroundProcess {
                    pid: SignalTarget::new(4243).unwrap(),
                    name: Some(String::from("zsh")),
                }),
            },
        },
        BrokerVector {
            name: "waited",
            stream_id: ctl,
            req_id: r(13),
            msg: BrokerMsg::Waited { outcome: WaitOutcome::Met },
        },
        BrokerVector {
            name: "frame",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Frame(Box::new(FrameUpdate {
                seq: 15,
                epoch: 4,
                size: GridSize::new(80, 24).unwrap(),
                first_active_line: 42,
                scrollback_lines: 30,
                scroll_delta: 2,
                cursor: CursorState { col: 7, row: 0, shape: CursorShape::Block, blinking: true, hidden: false },
                modes: ViewModes { alt_screen: false, mouse: true, reverse_video: false, bracketed_paste: true },
                palette: Some(PaletteOverrides {
                    fg: Some(Rgb([0xee, 0xee, 0xee])),
                    bg: Some(Rgb([0x10, 0x12, 0x16])),
                    cursor: Some(Rgb([0x00, 0xff, 0x88])),
                    palette: vec![(1, Rgb([0xff, 0x33, 0x44]))],
                }),
                rows: vec![sample_packed_row()],
                images_gen: 9,
                full_sync: true,
            })),
        },
        BrokerVector {
            name: "rows",
            stream_id: s3,
            req_id: r(21),
            msg: BrokerMsg::Rows { epoch: 4, rows: vec![sample_packed_row()] },
        },
        BrokerVector {
            name: "pasted",
            stream_id: s3,
            req_id: r(20),
            msg: BrokerMsg::Pasted { outcome: PasteOutcome::Unsafe },
        },
        BrokerVector {
            name: "span_reply",
            stream_id: s3,
            req_id: r(22),
            msg: BrokerMsg::SpanReply {
                span: Some(CellSpan { start: CellPos { line: 42, col: 0 }, end: CellPos { line: 42, col: 8 } }),
            },
        },
        BrokerVector {
            name: "text_reply",
            stream_id: s3,
            req_id: r(24),
            msg: BrokerMsg::TextReply { text: String::from("Hi 世界🚀") },
        },
        BrokerVector {
            name: "search_matches",
            stream_id: s3,
            req_id: r(26),
            msg: BrokerMsg::SearchMatches {
                epoch: 4,
                matches: vec![CellSpan { start: CellPos { line: 42, col: 0 }, end: CellPos { line: 42, col: 1 } }],
                truncated: false,
            },
        },
        BrokerVector {
            name: "search_delta",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::SearchDelta {
                epoch: 4,
                from_line: 40,
                to_line: 45,
                added: vec![CellSpan { start: CellPos { line: 42, col: 0 }, end: CellPos { line: 42, col: 1 } }],
                truncated: true,
            },
        },
        BrokerVector {
            name: "commands_reply",
            stream_id: s3,
            req_id: r(27),
            msg: BrokerMsg::CommandsReply {
                commands: vec![
                    cmd.clone(),
                    CommandRecord {
                        id: 13,
                        command: None,
                        cwd: None,
                        started_ms: None,
                        finished_ms: None,
                        outcome: CommandOutcome::Unknown(UnknownReason::Superseded),
                        prompt_line: None,
                        output_line: None,
                        end_line: None,
                    },
                    CommandRecord {
                        id: 14,
                        command: Some(String::from("ls")),
                        cwd: None,
                        started_ms: None,
                        finished_ms: None,
                        outcome: CommandOutcome::Claimed(Some(1)),
                        prompt_line: Some(43),
                        output_line: None,
                        end_line: None,
                    },
                ],
            },
        },
        BrokerVector {
            name: "command_output_reply",
            stream_id: s3,
            req_id: r(28),
            msg: BrokerMsg::CommandOutputReply { text: Some(String::from("ok\n")) },
        },
        BrokerVector {
            name: "images_reply",
            stream_id: s3,
            req_id: r(29),
            msg: BrokerMsg::ImagesReply {
                images_gen: 9,
                placements: vec![ImagePlacement {
                    blob: BlobHash([0xAB; 32]),
                    line: 42,
                    col: 10,
                    source_rect: [4, 2, 60, 30],
                    cell_offset: [1, 2],
                    pixel_size: [64, 32],
                    cell_span: [8, 2],
                    z_index: -1,
                }],
            },
        },
        BrokerVector {
            name: "image_blob_chunk",
            stream_id: s3,
            req_id: r(30),
            msg: BrokerMsg::ImageBlobChunk {
                blob: BlobHash([0xAB; 32]),
                encoding: ImageEncoding::Png,
                total_bytes: 16,
                offset: 0,
                data: vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            },
        },
        BrokerVector {
            name: "title",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Title { title: String::from("vim") },
        },
        BrokerVector { name: "bell", stream_id: s3, req_id: push, msg: BrokerMsg::Bell },
        BrokerVector {
            name: "clipboard",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Clipboard { selection: String::from("c"), data: String::from("copied") },
        },
        BrokerVector {
            name: "notification",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Notification { title: String::from("Build"), body: String::from("Done") },
        },
        BrokerVector {
            name: "progress",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Progress { state: ProgressState::Running, percent: Some(67) },
        },
        BrokerVector {
            name: "program_status",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::ProgramStatus(Box::new(StatusRecord {
                id: String::new(),
                state: StatusState::Blocked,
                kind: Some(BlockedKind::Permission),
                progress: Some(50),
                app: Some(String::from("agent")),
                title: Some(String::from("Approve")),
                message: Some(String::from("Run tests?")),
                changed: 3,
                since_ms: 1_700_000_000_500,
                confirmed: Confirmed::Matched,
            })),
        },
        BrokerVector {
            name: "program_status_cleared",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::ProgramStatusCleared { id: String::from("agent-1") },
        },
        BrokerVector {
            name: "command_started",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::CommandStarted(Box::new(cmd.clone())),
        },
        BrokerVector {
            name: "command_finished",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::CommandFinished(Box::new(cmd)),
        },
        BrokerVector {
            name: "cwd_changed",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::CwdChanged { cwd: String::from("/repo/src") },
        },
        BrokerVector {
            name: "exited",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Exited { exit_code: Some(0), signal: None },
        },
        BrokerVector {
            name: "output",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::Output { text: String::from("hello\r\n") },
        },
        BrokerVector { name: "throttled", stream_id: s3, req_id: push, msg: BrokerMsg::Throttled { waited_us: 4_200 } },
        BrokerVector {
            name: "output_dropped",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::OutputDropped { bytes: 1_024 },
        },
        BrokerVector {
            name: "stream_closed",
            stream_id: s3,
            req_id: push,
            msg: BrokerMsg::StreamClosed { reason: String::from("session exited") },
        },
    ]
}

fn build_malformed_vectors() -> Vec<MalformedVector> {
    let mut out = Vec::new();

    // 1. Body len < 10 (frame shorter than inner header)
    out.push(MalformedVector {
        name: "body_len_too_short",
        direction: "frame",
        hex: to_hex(&[5, 0, 0, 0]),
        reason: "body_len 1..9 must fail framing",
    });

    // 2. Body len > MAX_BODY_LEN
    let huge = (10u32 + apack_proto::MAX_FRAME_PAYLOAD as u32 + 1).to_le_bytes();
    out.push(MalformedVector {
        name: "body_len_too_large",
        direction: "frame",
        hex: to_hex(&huge),
        reason: "body_len exceeding MAX_BODY_LEN must fail framing",
    });

    // 3. Unknown BrokerMsg tag
    let mut buf = Vec::new();
    encode_frame(StreamId::CONTROL, 0xFFFF, RequestId::PUSH, &[], &mut buf).unwrap();
    out.push(MalformedVector {
        name: "unknown_broker_msg_type",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "unknown msg_type must be rejected",
    });

    // 4. Trailing bytes on Bell (empty payload message)
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::BELL, RequestId::PUSH, &[0x00], &mut buf).unwrap();
    out.push(MalformedVector {
        name: "trailing_bytes_on_bell",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "trailing payload bytes must be rejected",
    });

    // 5. Invalid bool byte (2) on OwnsReply
    buf.clear();
    encode_frame(StreamId::CONTROL, msg_type::OWNS_REPLY, RequestId::from_raw(1), &[2], &mut buf).unwrap();
    out.push(MalformedVector {
        name: "invalid_bool_tag",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "bool byte other than 0 or 1 must be rejected",
    });

    // 6. Invalid UTF-8 in Title
    let mut w = Writer::new();
    w.bytes(&[0xFF, 0xFE]);
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::TITLE, RequestId::PUSH, w.as_slice(), &mut buf).unwrap();
    out.push(MalformedVector {
        name: "invalid_utf8_title",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "non-UTF-8 string bytes must be rejected",
    });

    // 7. Varint overflow (u64 > 10 bytes) on Sought
    buf.clear();
    encode_frame(
        StreamId::CONTROL,
        msg_type::SOUGHT,
        RequestId::from_raw(1),
        &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02],
        &mut buf,
    )
    .unwrap();
    out.push(MalformedVector {
        name: "varint_u64_overflow",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "10th byte > 0x01 overflows u64 varint",
    });

    // 8. FrameUpdate with scrollback_lines > first_active_line
    let mut w = Writer::new();
    w.var_u64(1); // seq
    w.var_u64(1); // epoch
    GridSize::new(80, 24).unwrap().encode(&mut w);
    w.var_u64(5); // first_active_line
    w.var_u64(10); // scrollback_lines > first_active_line!
    w.i32(0);
    CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false }.encode(&mut w);
    ViewModes::default().encode(&mut w);
    w.u8(0); // palette None
    w.var_u32(0); // 0 rows
    w.var_u64(0); // images_gen
    w.bool(true);
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::FRAME, RequestId::PUSH, w.as_slice(), &mut buf).unwrap();
    out.push(MalformedVector {
        name: "frame_scrollback_exceeds_first_active",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "scrollback_lines > first_active_line must be rejected",
    });

    // 9. PackedRow with pieces but missing FLAG_HAS_PLACEHOLDER
    let mut w = Writer::new();
    w.var_u64(1); // epoch
    w.var_u32(1); // 1 row
    w.var_u64(0); // line
    w.u8(0); // flags without FLAG_HAS_PLACEHOLDER
    w.str("a");
    w.var_u32(0); // runs
    w.var_u32(0); // styles
    w.var_u32(0); // links
    w.var_u32(1); // 1 piece!
    BlobHash([0x11; 32]).encode(&mut w);
    w.u16(0);
    w.u16(1);
    w.var_u32(0);
    w.var_u32(0);
    w.var_u32(8);
    w.var_u32(8);
    w.var_u32(8);
    w.var_u32(8);
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::ROWS, RequestId::from_raw(1), w.as_slice(), &mut buf).unwrap();
    out.push(MalformedVector {
        name: "row_pieces_without_placeholder_flag",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "non-empty pieces without FLAG_HAS_PLACEHOLDER must be rejected",
    });

    // 10. SearchDelta with to_line < from_line
    let mut w = Writer::new();
    w.var_u64(1); // epoch
    w.var_u64(20); // from_line
    w.var_u64(10); // to_line < from_line
    w.var_u32(0); // added
    w.bool(false);
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::SEARCH_DELTA, RequestId::PUSH, w.as_slice(), &mut buf).unwrap();
    out.push(MalformedVector {
        name: "search_delta_inverted_range",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "to_line < from_line must be rejected",
    });

    // 11. ImageBlobChunk where offset + data.len() > total_bytes
    let mut w = Writer::new();
    BlobHash([0xAB; 32]).encode(&mut w);
    ImageEncoding::Png.encode(&mut w);
    w.var_u32(4); // total_bytes
    w.var_u32(2); // offset
    w.bytes(&[1, 2, 3]); // end = 5 > 4
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::IMAGE_BLOB_CHUNK, RequestId::from_raw(1), w.as_slice(), &mut buf)
        .unwrap();
    out.push(MalformedVector {
        name: "image_blob_chunk_overflow",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "offset + len > total_bytes must be rejected",
    });

    // 12. Non-canonical LEB128 varint (overlong zero `[0x80, 0x00]` on Sought.at_ms)
    buf.clear();
    encode_frame(StreamId::CONTROL, msg_type::SOUGHT, RequestId::from_raw(1), &[0x80, 0x00], &mut buf).unwrap();
    out.push(MalformedVector {
        name: "non_canonical_varint",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "overlong LEB128 varint must be rejected",
    });

    // 13. LeaseKind::BrokerGrace with all-zero resume_token
    let mut w = Writer::new();
    w.u8(LeaseKind::TAG_BROKER_GRACE);
    w.var_u32(1_000);
    w.raw(&[0u8; 32]);
    buf.clear();
    encode_frame(StreamId::CONTROL, msg_type::ACQUIRE_LEASE, RequestId::from_raw(1), w.as_slice(), &mut buf).unwrap();
    out.push(MalformedVector {
        name: "lease_zero_resume_token",
        direction: "client",
        hex: to_hex(&buf),
        reason: "all-zero resume_token must be rejected",
    });

    // 14. PackedRow with empty non-zero link entry
    let bad_link_row = PackedRow {
        line: 0,
        flags: 0,
        text: String::new(),
        runs: vec![],
        styles: vec![],
        links: vec![String::new(), String::new()],
        pieces: vec![],
    };
    let mut w = Writer::new();
    w.var_u64(1);
    w.var_u32(1);
    bad_link_row.encode(&mut w);
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::ROWS, RequestId::from_raw(1), w.as_slice(), &mut buf).unwrap();
    out.push(MalformedVector {
        name: "packed_row_empty_nonzero_link",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "empty non-zero link entry in PackedRow.links[1..] must be rejected",
    });

    // 15. ImagePlacement with zero pixel_size
    let mut w = Writer::new();
    w.var_u64(1); // images_gen
    w.var_u32(1); // 1 placement
    ImagePlacement {
        blob: BlobHash([0xAB; 32]),
        line: 0,
        col: 0,
        source_rect: [0, 0, 10, 10],
        cell_offset: [0, 0],
        pixel_size: [0, 10],
        cell_span: [1, 1],
        z_index: 0,
    }
    .encode(&mut w);
    buf.clear();
    encode_frame(StreamId::from_raw(1), msg_type::IMAGES_REPLY, RequestId::from_raw(1), w.as_slice(), &mut buf)
        .unwrap();
    out.push(MalformedVector {
        name: "image_placement_zero_pixel_size",
        direction: "broker",
        hex: to_hex(&buf),
        reason: "ImagePlacement with zero pixel_size must be rejected",
    });

    out
}

fn generate_vectors_json() -> String {
    let mut fin_bytes = Vec::new();
    encode_fin(&mut fin_bytes);
    assert_eq!(fin_bytes, FIN_FRAME);

    let client_vecs = build_client_vectors();
    let broker_vecs = build_broker_vectors();
    let malformed_vecs = build_malformed_vectors();

    // Verify all Rust roundtrips & malformed rejections!
    for v in &client_vecs {
        let mut frame = Vec::new();
        v.msg.encode_to_frame(v.stream_id, v.req_id, &mut frame).unwrap();
        let mut dec = FrameDecoder::new();
        let (used, decoded) = dec.step(&frame).unwrap();
        assert_eq!(used, frame.len());
        let DecodedFrame::Frame { header, payload } = decoded.unwrap() else { panic!("expected frame") };
        assert_eq!(header.stream_id, v.stream_id);
        assert_eq!(header.req_id, v.req_id);
        assert_eq!(header.msg_type, v.msg.msg_type());
        assert_eq!(ClientMsg::decode(header.msg_type, &payload).unwrap(), v.msg);
    }

    for v in &broker_vecs {
        let mut frame = Vec::new();
        v.msg.encode_to_frame(v.stream_id, v.req_id, &mut frame).unwrap();
        let mut dec = FrameDecoder::new();
        let (used, decoded) = dec.step(&frame).unwrap();
        assert_eq!(used, frame.len());
        let DecodedFrame::Frame { header, payload } = decoded.unwrap() else { panic!("expected frame") };
        assert_eq!(header.stream_id, v.stream_id);
        assert_eq!(header.req_id, v.req_id);
        assert_eq!(header.msg_type, v.msg.msg_type());
        assert_eq!(BrokerMsg::decode(header.msg_type, &payload).unwrap(), v.msg);
    }

    for v in &malformed_vecs {
        let raw: Vec<u8> =
            (0..v.hex.len()).step_by(2).map(|i| u8::from_str_radix(&v.hex[i..i + 2], 16).unwrap()).collect();
        let mut dec = FrameDecoder::new();
        match v.direction {
            "frame" => assert!(dec.step(&raw).is_err(), "malformed frame {} must fail", v.name),
            "client" => {
                let (_, Some(DecodedFrame::Frame { header, payload })) = dec.step(&raw).unwrap() else {
                    panic!("expected framed payload for {}", v.name);
                };
                assert!(ClientMsg::decode(header.msg_type, &payload).is_err(), "malformed client {} must fail", v.name);
            }
            "broker" => {
                let (_, Some(DecodedFrame::Frame { header, payload })) = dec.step(&raw).unwrap() else {
                    panic!("expected framed payload for {}", v.name);
                };
                assert!(BrokerMsg::decode(header.msg_type, &payload).is_err(), "malformed broker {} must fail", v.name);
            }
            other => panic!("unexpected direction {other}"),
        }
    }

    // Golden mutual HMAC-SHA256 vector
    let secret = AuthSecret::new([0x5A; 32]);
    let client_nonce = [0x11; 32];
    let server_nonce = [0x44; 32];
    let client_hmac = compute_client_proof(&secret, &client_nonce, &server_nonce);
    let server_hmac = compute_server_proof(&secret, &server_nonce, &client_nonce);

    let mut json = String::new();
    json.push_str("{\n");
    let _ = writeln!(json, "  \"protocolVersion\": {PROTOCOL_VERSION},");
    let _ = writeln!(json, "  \"finFrameHex\": {},", json_str(&to_hex(&fin_bytes)));
    json.push_str("  \"auth\": {\n");
    let _ = writeln!(json, "    \"secretHex\": {},", json_str(&to_hex(secret.as_bytes())));
    let _ = writeln!(json, "    \"clientNonceHex\": {},", json_str(&to_hex(&client_nonce)));
    let _ = writeln!(json, "    \"serverNonceHex\": {},", json_str(&to_hex(&server_nonce)));
    let _ = writeln!(json, "    \"clientHmacHex\": {},", json_str(&to_hex(&client_hmac)));
    let _ = writeln!(json, "    \"serverHmacHex\": {}", json_str(&to_hex(&server_hmac)));
    json.push_str("  },\n");

    json.push_str("  \"client\": [\n");
    for (i, v) in client_vecs.iter().enumerate() {
        let mut frame = Vec::new();
        v.msg.encode_to_frame(v.stream_id, v.req_id, &mut frame).unwrap();
        let comma = if i + 1 < client_vecs.len() { "," } else { "" };
        let _ = writeln!(
            json,
            "    {{ \"name\": {}, \"streamId\": {}, \"msgType\": {}, \"reqId\": {}, \"hex\": {} }}{comma}",
            json_str(v.name),
            v.stream_id.raw(),
            v.msg.msg_type(),
            v.req_id.raw(),
            json_str(&to_hex(&frame)),
        );
    }
    json.push_str("  ],\n");

    json.push_str("  \"broker\": [\n");
    for (i, v) in broker_vecs.iter().enumerate() {
        let mut frame = Vec::new();
        v.msg.encode_to_frame(v.stream_id, v.req_id, &mut frame).unwrap();
        let comma = if i + 1 < broker_vecs.len() { "," } else { "" };
        let _ = writeln!(
            json,
            "    {{ \"name\": {}, \"streamId\": {}, \"msgType\": {}, \"reqId\": {}, \"hex\": {} }}{comma}",
            json_str(v.name),
            v.stream_id.raw(),
            v.msg.msg_type(),
            v.req_id.raw(),
            json_str(&to_hex(&frame)),
        );
    }
    json.push_str("  ],\n");

    json.push_str("  \"malformed\": [\n");
    for (i, v) in malformed_vecs.iter().enumerate() {
        let comma = if i + 1 < malformed_vecs.len() { "," } else { "" };
        let _ = writeln!(
            json,
            "    {{ \"name\": {}, \"direction\": {}, \"hex\": {}, \"reason\": {} }}{comma}",
            json_str(v.name),
            json_str(v.direction),
            json_str(&v.hex),
            json_str(v.reason),
        );
    }
    json.push_str("  ]\n");
    json.push_str("}\n");
    json
}

#[test]
fn golden_wire_vectors_v2_match_committed_file() {
    let generated = generate_vectors_json();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf();
    let path = root.join("node").join("wire-vectors-v2.json");

    if std::env::var_os("UPDATE_VECTORS").is_some() {
        std::fs::write(&path, &generated).expect("write node/wire-vectors-v2.json");
        return;
    }

    let committed = std::fs::read_to_string(&path).expect(
        "node/wire-vectors-v2.json missing; run UPDATE_VECTORS=1 cargo test -p apack-broker --test wire_vectors_v2",
    );
    let committed_norm = committed.replace("\r\n", "\n");
    assert_eq!(
        committed_norm, generated,
        "node/wire-vectors-v2.json drifted from apack-proto; regenerate with UPDATE_VECTORS=1 cargo test -p apack-broker --test wire_vectors_v2"
    );
}
