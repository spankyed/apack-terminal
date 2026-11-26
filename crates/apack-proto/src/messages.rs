//! Unified v2 wire messages for control, view, event, and raw-output streams.
//!
//! Every message has an explicit `u16` wire discriminant in [`msg_type`] and a total, bounds-checked
//! binary encoder/decoder. Both [`ClientMsg::decode`] and [`BrokerMsg::decode`] reject trailing
//! bytes so wire drift is caught immediately.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::codec::{Reader, WireError, Writer};
use crate::frame::{DecodedFrame, FrameHeader, encode_frame};
use crate::types::{
    BlobHash, CellPos, CellPx, CellSpan, CommandRecord, CursorShape, CursorState, ErrorCode, ForegroundProcess,
    FrameCredit, GridSize, ImageEncoding, ImagePlacement, KeyInput, LeaseKind, MarksKey, MetricsSnapshot,
    NonZeroStreamId, PackedRow, PaletteOverrides, PasteOutcome, PointerInput, ProgressState, RequestId, ResizePolicy,
    SessionId, SignalKind, SignalScope, SignalTarget, StatusRecord, StreamId, SubFlags, ViewModes, ViewportHint,
    WaitCondition, WaitOutcome,
};
use crate::{BULK_CHUNK_BYTES, MAX_COMMANDS, MAX_ROWS_PER_FETCH, MAX_SEARCH_MATCHES};

/// Explicit `u16` wire discriminants shared between Rust and TypeScript codecs.
#[allow(missing_docs)]
pub mod msg_type {
    // Client -> Broker control (Stream 0)
    pub const HELLO: u16 = 0x0001;
    pub const AUTH_PROOF: u16 = 0x0002;
    pub const ACQUIRE_LEASE: u16 = 0x0003;
    pub const RELEASE_LEASE: u16 = 0x0004;
    pub const SPAWN: u16 = 0x0005;
    pub const KILL: u16 = 0x0006;
    pub const SIGNAL: u16 = 0x0007;
    pub const OWNS: u16 = 0x0008;
    pub const LIST_SESSIONS: u16 = 0x0009;
    pub const REWIND: u16 = 0x000A;
    pub const SEEK: u16 = 0x000B;
    pub const METRICS: u16 = 0x000C;
    pub const ATTACH: u16 = 0x000D;
    pub const DETACH: u16 = 0x000E;
    pub const KEEP: u16 = 0x000F;
    pub const WAIT: u16 = 0x0010;

    // Client -> Broker per-session stream (Stream >= 1)
    pub const FRAME_ACK: u16 = 0x0100;
    pub const VIEWPORT_HINT: u16 = 0x0101;
    pub const EXPLICIT_RESIZE: u16 = 0x0102;
    pub const SET_CURSOR_DEFAULTS: u16 = 0x0103;
    pub const KEY: u16 = 0x0104;
    pub const TEXT: u16 = 0x0105;
    pub const POINTER: u16 = 0x0106;
    pub const FOCUS: u16 = 0x0107;
    pub const PASTE: u16 = 0x0108;
    pub const WRITE: u16 = 0x0109;
    pub const PAUSE: u16 = 0x010A;
    pub const RESUME: u16 = 0x010B;
    pub const OUTPUT_CREDIT: u16 = 0x010C;
    pub const FETCH_ROWS: u16 = 0x010D;
    pub const WORD_AT: u16 = 0x010E;
    pub const LINE_AT: u16 = 0x010F;
    pub const TEXT_OF: u16 = 0x0110;
    pub const TEXT_OF_RECT: u16 = 0x0111;
    pub const SET_SEARCH: u16 = 0x0112;
    pub const COMMANDS: u16 = 0x0113;
    pub const COMMAND_OUTPUT: u16 = 0x0114;
    pub const IMAGES: u16 = 0x0115;
    pub const GET_IMAGE_BLOB: u16 = 0x0116;

    // Broker -> Client control replies
    pub const HELLO_OK: u16 = 0x0200;
    pub const AUTH_OK: u16 = 0x0201;
    pub const ERROR: u16 = 0x0202;
    pub const OK: u16 = 0x0203;
    pub const LEASE_ACK: u16 = 0x0204;
    pub const SPAWNED: u16 = 0x0205;
    pub const OWNS_REPLY: u16 = 0x0206;
    pub const SESSIONS_LIST: u16 = 0x0207;
    pub const REWOUND: u16 = 0x0208;
    pub const SOUGHT: u16 = 0x0209;
    pub const METRICS_REPLY: u16 = 0x020A;
    pub const ATTACHED: u16 = 0x020B;
    pub const WAITED: u16 = 0x020C;

    // Broker -> Client per-session replies and push events
    pub const FRAME: u16 = 0x0300;
    pub const ROWS: u16 = 0x0301;
    pub const PASTED: u16 = 0x0302;
    pub const SPAN_REPLY: u16 = 0x0303;
    pub const TEXT_REPLY: u16 = 0x0304;
    pub const SEARCH_MATCHES: u16 = 0x0305;
    pub const SEARCH_DELTA: u16 = 0x0306;
    pub const COMMANDS_REPLY: u16 = 0x0307;
    pub const COMMAND_OUTPUT_REPLY: u16 = 0x0308;
    pub const IMAGES_REPLY: u16 = 0x0309;
    pub const IMAGE_BLOB_CHUNK: u16 = 0x030A;
    pub const TITLE: u16 = 0x0310;
    pub const BELL: u16 = 0x0311;
    pub const CLIPBOARD: u16 = 0x0312;
    pub const NOTIFICATION: u16 = 0x0313;
    pub const PROGRESS: u16 = 0x0314;
    pub const PROGRAM_STATUS: u16 = 0x0315;
    pub const COMMAND_STARTED: u16 = 0x0316;
    pub const COMMAND_FINISHED: u16 = 0x0317;
    pub const CWD_CHANGED: u16 = 0x0318;
    pub const EXITED: u16 = 0x0319;
    pub const OUTPUT: u16 = 0x0320;
    pub const THROTTLED: u16 = 0x0321;
    pub const OUTPUT_DROPPED: u16 = 0x0322;
    pub const STREAM_CLOSED: u16 = 0x0323;
}

/// Specification for spawning a new live terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// Program and arguments (empty = default login shell).
    pub argv: Vec<String>,
    /// Optional working directory.
    pub cwd: Option<String>,
    /// Additional environment variable overrides `(key, value)`.
    pub env: Vec<(String, String)>,
    /// Initial terminal grid dimensions.
    pub size: GridSize,
    /// Initial cell dimensions in pixels.
    pub cell_px: CellPx,
    /// Maximum scrollback lines retained for the session.
    pub scrollback_limit: u32,
    /// Optional 32-char hex key enabling vouched shell marks (`OSC 633;V`).
    pub marks_key: Option<MarksKey>,
    /// How attached views arbitrate PTY geometry.
    pub resize_policy: ResizePolicy,
    /// Optional directory path for segmented WAL recording.
    pub record_dir: Option<String>,
    /// Optional recording directory to restore visual/engine state from before starting the shell.
    pub restore_from: Option<String>,
    /// Whether the session may read sandboxed local image files (`false` by default).
    pub image_files: bool,
    /// Whether the session survives lease release until explicitly killed (`false` by default).
    pub kept: bool,
}

impl SpawnSpec {
    fn encode(&self, w: &mut Writer) {
        w.slice(&self.argv, |w, a| w.str(a));
        w.opt(self.cwd.as_ref(), |w, s| w.str(s));
        w.slice(&self.env, |w, (k, v)| {
            w.str(k);
            w.str(v);
        });
        self.size.encode(w);
        self.cell_px.encode(w);
        w.var_u32(self.scrollback_limit);
        w.opt(self.marks_key.as_ref(), |w, k| k.encode(w));
        self.resize_policy.encode(w);
        w.opt(self.record_dir.as_ref(), |w, s| w.str(s));
        w.opt(self.restore_from.as_ref(), |w, s| w.str(s));
        w.bool(self.image_files);
        w.bool(self.kept);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            argv: r.vec_bounded(1024, "spawn argv", Reader::str)?,
            cwd: r.opt(Reader::str)?,
            env: r.vec_bounded(1024, "spawn env", |r| Ok((r.str()?, r.str()?)))?,
            size: GridSize::decode(r)?,
            cell_px: CellPx::decode(r)?,
            scrollback_limit: r.var_u32()?,
            marks_key: r.opt(MarksKey::decode)?,
            resize_policy: ResizePolicy::decode(r)?,
            record_dir: r.opt(Reader::str)?,
            restore_from: r.opt(Reader::str)?,
            image_files: r.bool()?,
            kept: r.bool()?,
        })
    }
}

/// Summary of one session returned by `ListSessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session identifier.
    pub id: SessionId,
    /// Current grid size.
    pub size: GridSize,
    /// Whether the session's PTY process is still running.
    pub alive: bool,
    /// Whether the session is marked `kept`.
    pub kept: bool,
    /// Whether the session is a read-only rewound recording.
    pub rewound: bool,
    /// Current terminal title.
    pub title: String,
    /// Last reported working directory.
    pub cwd: Option<String>,
    /// Leader process PID (live sessions only).
    pub pid: Option<SignalTarget>,
    /// Current foreground process (POSIX live sessions only).
    pub foreground: Option<ForegroundProcess>,
    /// Creation timestamp (ms since Unix epoch).
    pub created_ms: u64,
}

impl SessionSummary {
    fn encode(&self, w: &mut Writer) {
        self.id.encode(w);
        self.size.encode(w);
        w.bool(self.alive);
        w.bool(self.kept);
        w.bool(self.rewound);
        w.str(&self.title);
        w.opt(self.cwd.as_ref(), |w, s| w.str(s));
        w.opt(self.pid.as_ref(), |w, p| p.encode(w));
        w.opt(self.foreground.as_ref(), |w, f| f.encode(w));
        w.var_u64(self.created_ms);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            id: SessionId::decode(r)?,
            size: GridSize::decode(r)?,
            alive: r.bool()?,
            kept: r.bool()?,
            rewound: r.bool()?,
            title: r.str()?,
            cwd: r.opt(Reader::str)?,
            pid: r.opt(SignalTarget::decode)?,
            foreground: r.opt(ForegroundProcess::decode)?,
            created_ms: r.var_u64()?,
        })
    }
}

/// Differential or full screen update pushed to a `SUB_VIEW` subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameUpdate {
    /// Monotonic frame sequence number echoed in `FrameAck` for BDP RTT measurement.
    pub seq: u64,
    /// Epoch counter incremented on reflow or alternate-screen transitions.
    pub epoch: u64,
    /// Current terminal grid dimensions.
    pub size: GridSize,
    /// Monotonic epoch-local line number of viewport row `0`.
    ///
    /// History occupies `[first_active_line - scrollback_lines, first_active_line)` and the active
    /// viewport occupies `[first_active_line, first_active_line + size.rows)`. When scrollback
    /// reaches its limit and evicts old lines, `first_active_line` advances without renumbering
    /// any surviving line or bumping `epoch`.
    pub first_active_line: u64,
    /// Number of retained scrollback lines above the active viewport (`<= first_active_line`).
    pub scrollback_lines: u64,
    /// Net viewport scroll shift since the previous frame (`> 0` scrolled up, `< 0` down, `0` none).
    pub scroll_delta: i32,
    /// Current cursor state.
    pub cursor: CursorState,
    /// Active view-relevant terminal modes.
    pub modes: ViewModes,
    /// Included only when palette overrides changed since the last frame.
    pub palette: Option<PaletteOverrides>,
    /// Changed (or full-sync) packed rows.
    pub rows: Vec<PackedRow>,
    /// Monotonic image-placement generation counter; fetch `Images` only when this advances.
    pub images_gen: u64,
    /// `true` when this frame replaces the entire viewport state (initial attach / epoch change).
    pub full_sync: bool,
}

impl FrameUpdate {
    fn encode(&self, w: &mut Writer) {
        w.var_u64(self.seq);
        w.var_u64(self.epoch);
        self.size.encode(w);
        w.var_u64(self.first_active_line);
        w.var_u64(self.scrollback_lines);
        w.i32(self.scroll_delta);
        self.cursor.encode(w);
        self.modes.encode(w);
        w.opt(self.palette.as_ref(), |w, p| p.encode(w));
        w.slice(&self.rows, |w, row| row.encode(w));
        w.var_u64(self.images_gen);
        w.bool(self.full_sync);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let seq = r.var_u64()?;
        let epoch = r.var_u64()?;
        let size = GridSize::decode(r)?;
        let first_active_line = r.var_u64()?;
        let scrollback_lines = r.var_u64()?;
        if scrollback_lines > first_active_line {
            return Err(WireError::InvalidValue("FrameUpdate.scrollback_lines > first_active_line"));
        }
        Ok(Self {
            seq,
            epoch,
            size,
            first_active_line,
            scrollback_lines,
            scroll_delta: r.i32()?,
            cursor: CursorState::decode(r)?,
            modes: ViewModes::decode(r)?,
            palette: r.opt(PaletteOverrides::decode)?,
            rows: r.vec_bounded(MAX_ROWS_PER_FETCH as usize, "frame rows", PackedRow::decode)?,
            images_gen: r.var_u64()?,
            full_sync: r.bool()?,
        })
    }
}

/// Messages sent from a client (or `@apackdev/terminal-view`) to the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    /// First message on every connection: version, client nonce for mutual HMAC, and client label.
    Hello {
        /// Requested wire protocol version (must match [`crate::PROTOCOL_VERSION`]).
        protocol_version: u16,
        /// 32-byte random client challenge nonce.
        client_nonce: [u8; 32],
        /// Short client identification string for diagnostics.
        client_name: String,
    },
    /// Client's HMAC-SHA256 authentication proof computed over `client_nonce || server_nonce`.
    AuthProof {
        /// 32-byte HMAC-SHA256 digest.
        hmac: [u8; 32],
    },
    /// Acquire or resume a broker or session lifetime lease.
    AcquireLease {
        /// Lease scope and optional reconnect grace.
        kind: LeaseKind,
    },
    /// Explicitly release a previously acquired lease.
    ReleaseLease {
        /// Lease identifier returned in `LeaseAck`.
        lease_id: u64,
    },
    /// Spawn a new live session.
    Spawn(Box<SpawnSpec>),
    /// Terminate a live session immediately.
    Kill {
        /// Target session.
        session: SessionId,
    },
    /// Deliver a POSIX signal to a session or verified process inside its tree.
    Signal {
        /// Target session.
        session: SessionId,
        /// Signal to send.
        signal: SignalKind,
        /// Process scope.
        scope: SignalScope,
    },
    /// Query whether `pid` belongs to `session`'s process tree.
    Owns {
        /// Target session.
        session: SessionId,
        /// Strictly positive PID to verify.
        pid: SignalTarget,
    },
    /// List all live and rewound sessions known to the broker.
    ListSessions,
    /// Open a segmented recording directory for read-only inspection/playback.
    Rewind {
        /// Directory containing `meta.apkm` and `seg-*.apks`.
        record_dir: String,
        /// Optional initial timestamp offset in milliseconds.
        at_ms: Option<u64>,
    },
    /// Seek an open rewound session to `at_ms` milliseconds from recording start.
    Seek {
        /// Rewound session identifier.
        session: SessionId,
        /// Target timestamp offset in milliseconds.
        at_ms: u64,
    },
    /// Request a live broker telemetry snapshot.
    Metrics,
    /// Attach a multiplexed stream (`stream_id >= 1`) to a session.
    Attach {
        /// Target session.
        session: SessionId,
        /// Client-chosen non-zero stream ID for this connection.
        stream_id: NonZeroStreamId,
        /// Subscription capability bitmask (`VIEW`, `EVENTS`, `RAW_OUTPUT`, `BACKPRESSURE_PTY`).
        flags: SubFlags,
        /// Optional initial viewport hint for `SUB_VIEW` streams.
        initial_viewport: Option<ViewportHint>,
        /// Initial byte credit for `SUB_RAW_OUTPUT` streams.
        initial_output_credit: u32,
        /// Initial BDP credit for `SUB_VIEW` frame delivery.
        initial_frame_credit: FrameCredit,
    },
    /// Detach a previously attached stream.
    Detach {
        /// Non-zero stream ID to detach.
        stream_id: NonZeroStreamId,
    },
    /// Toggle whether a session survives lease release.
    Keep {
        /// Target session.
        session: SessionId,
        /// `true` to keep alive independently; `false` to bind to broker leases.
        kept: bool,
    },
    /// Reactive event-driven wait for a session condition or caller deadline.
    Wait {
        /// Target session.
        session: SessionId,
        /// Condition that satisfies the wait.
        condition: WaitCondition,
        /// Caller deadline in milliseconds.
        timeout_ms: u32,
    },

    // --- Per-session stream messages (`StreamId >= 1`) ---
    /// Acknowledge a rendered `FrameUpdate` and replenish frame/byte credit.
    FrameAck {
        /// Highest `FrameUpdate.seq` applied by the client.
        seq: u64,
        /// Replenished BDP credit window.
        credit: FrameCredit,
    },
    /// Declarative viewport size, visibility, and focus hint.
    ViewportHint(ViewportHint),
    /// Explicit control-plane resize (honoured under `ResizePolicy::ExplicitOverride` or single-client).
    ExplicitResize {
        /// New grid size.
        size: GridSize,
        /// Cell pixel dimensions.
        cell_px: CellPx,
    },
    /// Set fallback cursor defaults when the program has not overridden them.
    SetCursorDefaults {
        /// Default cursor shape.
        shape: CursorShape,
        /// Default cursor blink flag.
        blinking: bool,
    },
    /// Structured keyboard input for broker-side VT encoding.
    Key(KeyInput),
    /// IME or direct text input written to the PTY.
    Text {
        /// UTF-8 text to type.
        text: String,
    },
    /// Structured mouse/wheel input for broker-side VT encoding.
    Pointer(PointerInput),
    /// Terminal focus gained/lost (emits CSI I/O when focus reporting is enabled).
    Focus {
        /// `true` when focused, `false` when blurred.
        focused: bool,
    },
    /// Structured paste checked for unsafe command execution before writing to the PTY.
    Paste {
        /// Text to paste.
        text: String,
        /// `true` to bypass the unsafe-paste safety gate after user confirmation.
        allow_unsafe: bool,
    },
    /// Raw bytes written directly to the PTY stdin.
    Write {
        /// Raw byte payload.
        data: Vec<u8>,
    },
    /// Pause PTY reading for this stream's session.
    Pause,
    /// Resume PTY reading for this stream's session.
    Resume,
    /// Replenish `SUB_RAW_OUTPUT` byte credit for this stream.
    OutputCredit {
        /// Additional bytes granted to the broker.
        bytes: u32,
    },
    /// Fetch a slice of scrollback/viewport rows for a given epoch.
    FetchRows {
        /// Expected epoch (`StaleEpoch` error if mismatched).
        epoch: u64,
        /// First absolute line number to fetch.
        from_line: u64,
        /// Number of rows requested (`1..=MAX_ROWS_PER_FETCH`).
        count: u32,
    },
    /// Resolve word boundaries at `pos`.
    WordAt {
        /// Expected epoch.
        epoch: u64,
        /// Cell coordinate.
        pos: CellPos,
    },
    /// Resolve wrapped logical line boundaries containing `line`.
    LineAt {
        /// Expected epoch.
        epoch: u64,
        /// Any line within the wrapped logical line.
        line: u64,
    },
    /// Extract plain text across an inclusive cell span.
    TextOf {
        /// Expected epoch.
        epoch: u64,
        /// Inclusive selection span.
        span: CellSpan,
    },
    /// Extract rectangular block text across an inclusive cell span.
    TextOfRect {
        /// Expected epoch.
        epoch: u64,
        /// Inclusive rectangular corners.
        span: CellSpan,
    },
    /// Install or clear an incremental server-side `SearchWatch` (`query == ""` clears).
    SetSearch {
        /// Substring needle (empty string clears the active watch).
        query: String,
        /// Whether matching is case-sensitive.
        case_sensitive: bool,
    },
    /// Request the session's bounded command ring.
    Commands,
    /// Fetch recorded text output for a command ID.
    CommandOutput {
        /// Command identifier from `CommandRecord.id`.
        command_id: u64,
    },
    /// Request active image placements for the session.
    Images,
    /// Fetch a chunk of a content-addressed image blob.
    GetImageBlob {
        /// BLAKE3 content hash.
        blob: BlobHash,
        /// Starting byte offset.
        offset: u32,
        /// Maximum chunk bytes requested (clamped by broker to `BULK_CHUNK_BYTES`).
        max_bytes: u32,
    },
}

impl ClientMsg {
    /// Returns the explicit `u16` wire discriminant for this message.
    pub fn msg_type(&self) -> u16 {
        match self {
            Self::Hello { .. } => msg_type::HELLO,
            Self::AuthProof { .. } => msg_type::AUTH_PROOF,
            Self::AcquireLease { .. } => msg_type::ACQUIRE_LEASE,
            Self::ReleaseLease { .. } => msg_type::RELEASE_LEASE,
            Self::Spawn(_) => msg_type::SPAWN,
            Self::Kill { .. } => msg_type::KILL,
            Self::Signal { .. } => msg_type::SIGNAL,
            Self::Owns { .. } => msg_type::OWNS,
            Self::ListSessions => msg_type::LIST_SESSIONS,
            Self::Rewind { .. } => msg_type::REWIND,
            Self::Seek { .. } => msg_type::SEEK,
            Self::Metrics => msg_type::METRICS,
            Self::Attach { .. } => msg_type::ATTACH,
            Self::Detach { .. } => msg_type::DETACH,
            Self::Keep { .. } => msg_type::KEEP,
            Self::Wait { .. } => msg_type::WAIT,
            Self::FrameAck { .. } => msg_type::FRAME_ACK,
            Self::ViewportHint(_) => msg_type::VIEWPORT_HINT,
            Self::ExplicitResize { .. } => msg_type::EXPLICIT_RESIZE,
            Self::SetCursorDefaults { .. } => msg_type::SET_CURSOR_DEFAULTS,
            Self::Key(_) => msg_type::KEY,
            Self::Text { .. } => msg_type::TEXT,
            Self::Pointer(_) => msg_type::POINTER,
            Self::Focus { .. } => msg_type::FOCUS,
            Self::Paste { .. } => msg_type::PASTE,
            Self::Write { .. } => msg_type::WRITE,
            Self::Pause => msg_type::PAUSE,
            Self::Resume => msg_type::RESUME,
            Self::OutputCredit { .. } => msg_type::OUTPUT_CREDIT,
            Self::FetchRows { .. } => msg_type::FETCH_ROWS,
            Self::WordAt { .. } => msg_type::WORD_AT,
            Self::LineAt { .. } => msg_type::LINE_AT,
            Self::TextOf { .. } => msg_type::TEXT_OF,
            Self::TextOfRect { .. } => msg_type::TEXT_OF_RECT,
            Self::SetSearch { .. } => msg_type::SET_SEARCH,
            Self::Commands => msg_type::COMMANDS,
            Self::CommandOutput { .. } => msg_type::COMMAND_OUTPUT,
            Self::Images => msg_type::IMAGES,
            Self::GetImageBlob { .. } => msg_type::GET_IMAGE_BLOB,
        }
    }

    /// Encodes the message payload into `w`.
    pub fn encode(&self, w: &mut Writer) {
        match self {
            Self::Hello { protocol_version, client_nonce, client_name } => {
                w.u16(*protocol_version);
                w.raw(client_nonce);
                w.str(client_name);
            }
            Self::AuthProof { hmac } => w.raw(hmac),
            Self::AcquireLease { kind } => kind.encode(w),
            Self::ReleaseLease { lease_id } => w.var_u64(*lease_id),
            Self::Spawn(spec) => spec.encode(w),
            Self::Kill { session } => session.encode(w),
            Self::Signal { session, signal, scope } => {
                session.encode(w);
                signal.encode(w);
                scope.encode(w);
            }
            Self::Owns { session, pid } => {
                session.encode(w);
                pid.encode(w);
            }
            Self::ListSessions | Self::Metrics | Self::Pause | Self::Resume | Self::Commands | Self::Images => {}
            Self::Rewind { record_dir, at_ms } => {
                w.str(record_dir);
                w.opt(at_ms.as_ref(), |w, ms| w.var_u64(*ms));
            }
            Self::Seek { session, at_ms } => {
                session.encode(w);
                w.var_u64(*at_ms);
            }
            Self::Attach {
                session,
                stream_id,
                flags,
                initial_viewport,
                initial_output_credit,
                initial_frame_credit,
            } => {
                session.encode(w);
                stream_id.encode(w);
                flags.encode(w);
                w.opt(initial_viewport.as_ref(), |w, h| h.encode(w));
                w.var_u32(*initial_output_credit);
                initial_frame_credit.encode(w);
            }
            Self::Detach { stream_id } => stream_id.encode(w),
            Self::Keep { session, kept } => {
                session.encode(w);
                w.bool(*kept);
            }
            Self::Wait { session, condition, timeout_ms } => {
                session.encode(w);
                condition.encode(w);
                w.var_u32(*timeout_ms);
            }
            Self::FrameAck { seq, credit } => {
                w.var_u64(*seq);
                credit.encode(w);
            }
            Self::ViewportHint(hint) => hint.encode(w),
            Self::ExplicitResize { size, cell_px } => {
                size.encode(w);
                cell_px.encode(w);
            }
            Self::SetCursorDefaults { shape, blinking } => {
                shape.encode(w);
                w.bool(*blinking);
            }
            Self::Key(input) => input.encode(w),
            Self::Text { text } => w.str(text),
            Self::Pointer(input) => input.encode(w),
            Self::Focus { focused } => w.bool(*focused),
            Self::Paste { text, allow_unsafe } => {
                w.str(text);
                w.bool(*allow_unsafe);
            }
            Self::Write { data } => w.bytes(data),
            Self::OutputCredit { bytes } => w.var_u32(*bytes),
            Self::FetchRows { epoch, from_line, count } => {
                w.var_u64(*epoch);
                w.var_u64(*from_line);
                w.var_u32(*count);
            }
            Self::WordAt { epoch, pos } => {
                w.var_u64(*epoch);
                pos.encode(w);
            }
            Self::LineAt { epoch, line } => {
                w.var_u64(*epoch);
                w.var_u64(*line);
            }
            Self::TextOf { epoch, span } | Self::TextOfRect { epoch, span } => {
                w.var_u64(*epoch);
                span.encode(w);
            }
            Self::SetSearch { query, case_sensitive } => {
                w.str(query);
                w.bool(*case_sensitive);
            }
            Self::CommandOutput { command_id } => w.var_u64(*command_id),
            Self::GetImageBlob { blob, offset, max_bytes } => {
                blob.encode(w);
                w.var_u32(*offset);
                w.var_u32(*max_bytes);
            }
        }
    }

    /// Decodes a client message from `msg_type` and `payload`, rejecting trailing bytes.
    pub fn decode(msg_type: u16, payload: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(payload);
        let msg = match msg_type {
            msg_type::HELLO => {
                Self::Hello { protocol_version: r.u16()?, client_nonce: r.fixed::<32>()?, client_name: r.str()? }
            }
            msg_type::AUTH_PROOF => Self::AuthProof { hmac: r.fixed::<32>()? },
            msg_type::ACQUIRE_LEASE => Self::AcquireLease { kind: LeaseKind::decode(&mut r)? },
            msg_type::RELEASE_LEASE => Self::ReleaseLease { lease_id: r.var_u64()? },
            msg_type::SPAWN => Self::Spawn(Box::new(SpawnSpec::decode(&mut r)?)),
            msg_type::KILL => Self::Kill { session: SessionId::decode(&mut r)? },
            msg_type::SIGNAL => Self::Signal {
                session: SessionId::decode(&mut r)?,
                signal: SignalKind::decode(&mut r)?,
                scope: SignalScope::decode(&mut r)?,
            },
            msg_type::OWNS => Self::Owns { session: SessionId::decode(&mut r)?, pid: SignalTarget::decode(&mut r)? },
            msg_type::LIST_SESSIONS => Self::ListSessions,
            msg_type::REWIND => Self::Rewind { record_dir: r.str()?, at_ms: r.opt(Reader::var_u64)? },
            msg_type::SEEK => Self::Seek { session: SessionId::decode(&mut r)?, at_ms: r.var_u64()? },
            msg_type::METRICS => Self::Metrics,
            msg_type::ATTACH => Self::Attach {
                session: SessionId::decode(&mut r)?,
                stream_id: NonZeroStreamId::decode(&mut r)?,
                flags: SubFlags::decode(&mut r)?,
                initial_viewport: r.opt(ViewportHint::decode)?,
                initial_output_credit: r.var_u32()?,
                initial_frame_credit: FrameCredit::decode(&mut r)?,
            },
            msg_type::DETACH => Self::Detach { stream_id: NonZeroStreamId::decode(&mut r)? },
            msg_type::KEEP => Self::Keep { session: SessionId::decode(&mut r)?, kept: r.bool()? },
            msg_type::WAIT => Self::Wait {
                session: SessionId::decode(&mut r)?,
                condition: WaitCondition::decode(&mut r)?,
                timeout_ms: r.var_u32()?,
            },
            msg_type::FRAME_ACK => Self::FrameAck { seq: r.var_u64()?, credit: FrameCredit::decode(&mut r)? },
            msg_type::VIEWPORT_HINT => Self::ViewportHint(ViewportHint::decode(&mut r)?),
            msg_type::EXPLICIT_RESIZE => {
                Self::ExplicitResize { size: GridSize::decode(&mut r)?, cell_px: CellPx::decode(&mut r)? }
            }
            msg_type::SET_CURSOR_DEFAULTS => {
                Self::SetCursorDefaults { shape: CursorShape::decode(&mut r)?, blinking: r.bool()? }
            }
            msg_type::KEY => Self::Key(KeyInput::decode(&mut r)?),
            msg_type::TEXT => Self::Text { text: r.str()? },
            msg_type::POINTER => Self::Pointer(PointerInput::decode(&mut r)?),
            msg_type::FOCUS => Self::Focus { focused: r.bool()? },
            msg_type::PASTE => Self::Paste { text: r.str()?, allow_unsafe: r.bool()? },
            msg_type::WRITE => Self::Write { data: r.bytes()? },
            msg_type::PAUSE => Self::Pause,
            msg_type::RESUME => Self::Resume,
            msg_type::OUTPUT_CREDIT => Self::OutputCredit { bytes: r.var_u32()? },
            msg_type::FETCH_ROWS => {
                let epoch = r.var_u64()?;
                let from_line = r.var_u64()?;
                let count = r.var_u32()?;
                if count == 0 || count > MAX_ROWS_PER_FETCH {
                    return Err(WireError::LimitExceeded("fetch rows count"));
                }
                Self::FetchRows { epoch, from_line, count }
            }
            msg_type::WORD_AT => Self::WordAt { epoch: r.var_u64()?, pos: CellPos::decode(&mut r)? },
            msg_type::LINE_AT => Self::LineAt { epoch: r.var_u64()?, line: r.var_u64()? },
            msg_type::TEXT_OF => Self::TextOf { epoch: r.var_u64()?, span: CellSpan::decode(&mut r)? },
            msg_type::TEXT_OF_RECT => Self::TextOfRect { epoch: r.var_u64()?, span: CellSpan::decode(&mut r)? },
            msg_type::SET_SEARCH => Self::SetSearch { query: r.str()?, case_sensitive: r.bool()? },
            msg_type::COMMANDS => Self::Commands,
            msg_type::COMMAND_OUTPUT => Self::CommandOutput { command_id: r.var_u64()? },
            msg_type::IMAGES => Self::Images,
            msg_type::GET_IMAGE_BLOB => {
                let blob = BlobHash::decode(&mut r)?;
                let offset = r.var_u32()?;
                let max_bytes = r.var_u32()?;
                if max_bytes == 0 {
                    return Err(WireError::InvalidValue("get_image_blob max_bytes must be non-zero"));
                }
                Self::GetImageBlob { blob, offset, max_bytes }
            }
            other => return Err(WireError::InvalidTag { kind: "ClientMsg", tag: other }),
        };
        r.finish()?;
        Ok(msg)
    }

    /// Encodes this message directly into a framed wire packet in `out`.
    pub fn encode_to_frame(&self, stream_id: StreamId, req_id: RequestId, out: &mut Vec<u8>) -> Result<(), WireError> {
        let mut w = Writer::new();
        self.encode(&mut w);
        encode_frame(stream_id, self.msg_type(), req_id, w.as_slice(), out)
    }
}

/// Messages sent from the broker to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerMsg {
    /// Reply to `Hello` advertising broker capabilities, word delimiters, and auth challenge.
    HelloOk {
        /// Negotiated wire protocol version.
        protocol_version: u16,
        /// `true` when the client must follow with `AuthProof` before issuing any command.
        auth_required: bool,
        /// 32-byte random server nonce.
        server_nonce: [u8; 32],
        /// Default word-selection delimiter characters shared with local view `WordAt`.
        word_delimiters: String,
        /// Capability bitmask advertised by the broker.
        capabilities: u32,
    },
    /// Mutual-auth confirmation carrying the server's HMAC proof over `server_nonce || client_nonce`.
    AuthOk {
        /// 32-byte server HMAC-SHA256 digest.
        server_hmac: [u8; 32],
    },
    /// Structured error reply correlated by `req_id` (or fatal connection error when `req_id == 0`).
    Error {
        /// Closed error code.
        code: ErrorCode,
        /// Human-readable diagnostic message.
        message: String,
    },
    /// Generic unit acknowledgement for commands with no return payload.
    Ok,
    /// Reply to `AcquireLease`.
    LeaseAck {
        /// Assigned lease ID.
        lease_id: u64,
        /// `true` if an in-grace remote lease was seamlessly resumed.
        resumed: bool,
    },
    /// Reply to `Spawn`.
    Spawned {
        /// Newly assigned session ID.
        session: SessionId,
        /// Leader process PID (when available).
        pid: Option<SignalTarget>,
    },
    /// Reply to `Owns`.
    OwnsReply {
        /// `true` iff the queried PID belongs to the session's process tree.
        owned: bool,
    },
    /// Reply to `ListSessions`.
    SessionsList {
        /// Snapshot of all sessions.
        sessions: Vec<SessionSummary>,
    },
    /// Reply to `Rewind`.
    Rewound {
        /// Read-only rewound session ID.
        session: SessionId,
        /// Total recording duration in milliseconds.
        duration_ms: u64,
        /// Current playback position in milliseconds.
        at_ms: u64,
    },
    /// Reply to `Seek`.
    Sought {
        /// Actual playback timestamp reached in milliseconds.
        at_ms: u64,
    },
    /// Reply to `Metrics`.
    MetricsReply(MetricsSnapshot),
    /// Reply to `Attach`.
    Attached {
        /// Attached session ID.
        session: SessionId,
        /// Bound non-zero stream ID.
        stream_id: NonZeroStreamId,
        /// Current terminal grid size.
        size: GridSize,
        /// Current screen epoch.
        epoch: u64,
        /// Monotonic epoch-local line number of viewport row `0`.
        first_active_line: u64,
        /// Current retained scrollback depth in lines (`<= first_active_line`).
        scrollback_lines: u64,
        /// Current foreground process (POSIX live sessions only).
        foreground: Option<ForegroundProcess>,
    },
    /// Reply to `Wait`.
    Waited {
        /// How the wait completed.
        outcome: WaitOutcome,
    },

    // --- Per-session replies & pushes ---
    /// Differential or full screen update (`SUB_VIEW`).
    Frame(Box<FrameUpdate>),
    /// Reply to `FetchRows`.
    Rows {
        /// Epoch the rows belong to.
        epoch: u64,
        /// Requested packed rows.
        rows: Vec<PackedRow>,
    },
    /// Reply to `Paste`.
    Pasted {
        /// Whether the paste was queued or held as unsafe.
        outcome: PasteOutcome,
    },
    /// Reply to `WordAt` or `LineAt`.
    SpanReply {
        /// Resolved cell span, or `None` if target cell was out of bounds.
        span: Option<CellSpan>,
    },
    /// Reply to `TextOf` or `TextOfRect`.
    TextReply {
        /// Extracted UTF-8 text.
        text: String,
    },
    /// Full match set sent after `SetSearch` or an epoch reset.
    SearchMatches {
        /// Epoch of the match coordinates.
        epoch: u64,
        /// Matching inclusive spans (bounded by [`MAX_SEARCH_MATCHES`]).
        matches: Vec<CellSpan>,
        /// `true` when additional matches existed beyond the cap.
        truncated: bool,
    },
    /// Incremental search delta pushed when dirty lines change within the same epoch.
    SearchDelta {
        /// Epoch of the match coordinates.
        epoch: u64,
        /// Inclusive start line replaced by `added`.
        from_line: u64,
        /// Exclusive end line replaced by `added`.
        to_line: u64,
        /// Replacement matches within `[from_line, to_line)`.
        added: Vec<CellSpan>,
        /// `true` when total matches reached the cap.
        truncated: bool,
    },
    /// Reply to `Commands`.
    CommandsReply {
        /// Snapshot of the session's bounded command ring.
        commands: Vec<CommandRecord>,
    },
    /// Reply to `CommandOutput`.
    CommandOutputReply {
        /// Command output text, or `None` if evicted or unknown.
        text: Option<String>,
    },
    /// Reply to `Images`.
    ImagesReply {
        /// Image generation counter matching `FrameUpdate.images_gen`.
        images_gen: u64,
        /// Active image placements.
        placements: Vec<ImagePlacement>,
    },
    /// Reply to `GetImageBlob` carrying one chunk (`<= BULK_CHUNK_BYTES`).
    ImageBlobChunk {
        /// Content hash.
        blob: BlobHash,
        /// Wire/disk image encoding (`Png` or `ZstdRgba`).
        encoding: ImageEncoding,
        /// Total byte length of the complete blob.
        total_bytes: u32,
        /// Byte offset of `data` within the blob.
        offset: u32,
        /// Chunk payload bytes.
        data: Vec<u8>,
    },
    /// Terminal title changed (OSC 0/2).
    Title {
        /// New title string.
        title: String,
    },
    /// Audible/visual bell (BEL).
    Bell,
    /// Clipboard write request (OSC 52).
    Clipboard {
        /// Selection target (e.g., `"c"`).
        selection: String,
        /// Decoded UTF-8 clipboard text.
        data: String,
    },
    /// Desktop notification (OSC 9 / OSC 777).
    Notification {
        /// Optional notification title.
        title: String,
        /// Notification body text.
        body: String,
    },
    /// Progress bar update (OSC 9;4).
    Progress {
        /// Progress bar mode.
        state: ProgressState,
        /// Optional percentage (`0..=100`).
        percent: Option<u8>,
    },
    /// OSC 7501 program status record updated.
    ProgramStatus(Box<StatusRecord>),
    /// Shell command started (OSC 133/633).
    CommandStarted(Box<CommandRecord>),
    /// Shell command finished (OSC 133/633).
    CommandFinished(Box<CommandRecord>),
    /// Working directory changed (OSC 7 / OSC 633;P;Cwd).
    CwdChanged {
        /// Reported working directory.
        cwd: String,
    },
    /// Live session process exited.
    Exited {
        /// Exit code, if exited normally.
        exit_code: Option<i32>,
        /// Signal name, if terminated by a signal.
        signal: Option<String>,
    },
    /// Coalesced valid UTF-8 raw output chunk (`SUB_RAW_OUTPUT`).
    Output {
        /// Valid UTF-8 output text.
        text: String,
    },
    /// Reports microseconds spent throttled waiting on output credit during a pause episode (`SUB_RAW_OUTPUT`).
    Throttled {
        /// Microseconds waited during the completed backpressure episode.
        waited_us: u64,
    },
    /// Explicit accounting of shed raw output bytes in best-effort mode (`SUB_RAW_OUTPUT`).
    OutputDropped {
        /// Number of bytes shed since the previous report.
        bytes: u64,
    },
    /// Broker closed an attached stream (session killed/exited or detached).
    StreamClosed {
        /// Short closure reason.
        reason: String,
    },
}

impl BrokerMsg {
    /// Returns the explicit `u16` wire discriminant for this message.
    pub fn msg_type(&self) -> u16 {
        match self {
            Self::HelloOk { .. } => msg_type::HELLO_OK,
            Self::AuthOk { .. } => msg_type::AUTH_OK,
            Self::Error { .. } => msg_type::ERROR,
            Self::Ok => msg_type::OK,
            Self::LeaseAck { .. } => msg_type::LEASE_ACK,
            Self::Spawned { .. } => msg_type::SPAWNED,
            Self::OwnsReply { .. } => msg_type::OWNS_REPLY,
            Self::SessionsList { .. } => msg_type::SESSIONS_LIST,
            Self::Rewound { .. } => msg_type::REWOUND,
            Self::Sought { .. } => msg_type::SOUGHT,
            Self::MetricsReply(_) => msg_type::METRICS_REPLY,
            Self::Attached { .. } => msg_type::ATTACHED,
            Self::Waited { .. } => msg_type::WAITED,
            Self::Frame(_) => msg_type::FRAME,
            Self::Rows { .. } => msg_type::ROWS,
            Self::Pasted { .. } => msg_type::PASTED,
            Self::SpanReply { .. } => msg_type::SPAN_REPLY,
            Self::TextReply { .. } => msg_type::TEXT_REPLY,
            Self::SearchMatches { .. } => msg_type::SEARCH_MATCHES,
            Self::SearchDelta { .. } => msg_type::SEARCH_DELTA,
            Self::CommandsReply { .. } => msg_type::COMMANDS_REPLY,
            Self::CommandOutputReply { .. } => msg_type::COMMAND_OUTPUT_REPLY,
            Self::ImagesReply { .. } => msg_type::IMAGES_REPLY,
            Self::ImageBlobChunk { .. } => msg_type::IMAGE_BLOB_CHUNK,
            Self::Title { .. } => msg_type::TITLE,
            Self::Bell => msg_type::BELL,
            Self::Clipboard { .. } => msg_type::CLIPBOARD,
            Self::Notification { .. } => msg_type::NOTIFICATION,
            Self::Progress { .. } => msg_type::PROGRESS,
            Self::ProgramStatus(_) => msg_type::PROGRAM_STATUS,
            Self::CommandStarted(_) => msg_type::COMMAND_STARTED,
            Self::CommandFinished(_) => msg_type::COMMAND_FINISHED,
            Self::CwdChanged { .. } => msg_type::CWD_CHANGED,
            Self::Exited { .. } => msg_type::EXITED,
            Self::Output { .. } => msg_type::OUTPUT,
            Self::Throttled { .. } => msg_type::THROTTLED,
            Self::OutputDropped { .. } => msg_type::OUTPUT_DROPPED,
            Self::StreamClosed { .. } => msg_type::STREAM_CLOSED,
        }
    }

    /// Encodes the message payload into `w`.
    pub fn encode(&self, w: &mut Writer) {
        match self {
            Self::HelloOk { protocol_version, auth_required, server_nonce, word_delimiters, capabilities } => {
                w.u16(*protocol_version);
                w.bool(*auth_required);
                w.raw(server_nonce);
                w.str(word_delimiters);
                w.u32(*capabilities);
            }
            Self::AuthOk { server_hmac } => w.raw(server_hmac),
            Self::Error { code, message } => {
                code.encode(w);
                w.str(message);
            }
            Self::Ok | Self::Bell => {}
            Self::LeaseAck { lease_id, resumed } => {
                w.var_u64(*lease_id);
                w.bool(*resumed);
            }
            Self::Spawned { session, pid } => {
                session.encode(w);
                w.opt(pid.as_ref(), |w, p| p.encode(w));
            }
            Self::OwnsReply { owned } => w.bool(*owned),
            Self::SessionsList { sessions } => w.slice(sessions, |w, s| s.encode(w)),
            Self::Rewound { session, duration_ms, at_ms } => {
                session.encode(w);
                w.var_u64(*duration_ms);
                w.var_u64(*at_ms);
            }
            Self::Sought { at_ms } => w.var_u64(*at_ms),
            Self::MetricsReply(m) => m.encode(w),
            Self::Attached { session, stream_id, size, epoch, first_active_line, scrollback_lines, foreground } => {
                session.encode(w);
                stream_id.encode(w);
                size.encode(w);
                w.var_u64(*epoch);
                w.var_u64(*first_active_line);
                w.var_u64(*scrollback_lines);
                w.opt(foreground.as_ref(), |w, f| f.encode(w));
            }
            Self::Waited { outcome } => outcome.encode(w),
            Self::Frame(frame) => frame.encode(w),
            Self::Rows { epoch, rows } => {
                w.var_u64(*epoch);
                w.slice(rows, |w, row| row.encode(w));
            }
            Self::Pasted { outcome } => outcome.encode(w),
            Self::SpanReply { span } => w.opt(span.as_ref(), |w, s| s.encode(w)),
            Self::TextReply { text } => w.str(text),
            Self::SearchMatches { epoch, matches, truncated } => {
                w.var_u64(*epoch);
                w.slice(matches, |w, s| s.encode(w));
                w.bool(*truncated);
            }
            Self::SearchDelta { epoch, from_line, to_line, added, truncated } => {
                w.var_u64(*epoch);
                w.var_u64(*from_line);
                w.var_u64(*to_line);
                w.slice(added, |w, s| s.encode(w));
                w.bool(*truncated);
            }
            Self::CommandsReply { commands } => w.slice(commands, |w, c| c.encode(w)),
            Self::CommandOutputReply { text } => w.opt(text.as_ref(), |w, s| w.str(s)),
            Self::ImagesReply { images_gen, placements } => {
                w.var_u64(*images_gen);
                w.slice(placements, |w, p| p.encode(w));
            }
            Self::ImageBlobChunk { blob, encoding, total_bytes, offset, data } => {
                blob.encode(w);
                encoding.encode(w);
                w.var_u32(*total_bytes);
                w.var_u32(*offset);
                w.bytes(data);
            }
            Self::Title { title } => w.str(title),
            Self::Clipboard { selection, data } => {
                w.str(selection);
                w.str(data);
            }
            Self::Notification { title, body } => {
                w.str(title);
                w.str(body);
            }
            Self::Progress { state, percent } => {
                state.encode(w);
                w.opt(percent.as_ref(), |w, p| w.u8(*p));
            }
            Self::ProgramStatus(record) => record.encode(w),
            Self::CommandStarted(record) | Self::CommandFinished(record) => record.encode(w),
            Self::CwdChanged { cwd } => w.str(cwd),
            Self::Exited { exit_code, signal } => {
                w.opt(exit_code.as_ref(), |w, c| w.i32(*c));
                w.opt(signal.as_ref(), |w, s| w.str(s));
            }
            Self::Output { text } => w.str(text),
            Self::Throttled { waited_us } => w.var_u64(*waited_us),
            Self::OutputDropped { bytes } => w.var_u64(*bytes),
            Self::StreamClosed { reason } => w.str(reason),
        }
    }

    /// Decodes a broker message from `msg_type` and `payload`, rejecting trailing bytes.
    pub fn decode(msg_type: u16, payload: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(payload);
        let msg = match msg_type {
            msg_type::HELLO_OK => Self::HelloOk {
                protocol_version: r.u16()?,
                auth_required: r.bool()?,
                server_nonce: r.fixed::<32>()?,
                word_delimiters: r.str()?,
                capabilities: r.u32()?,
            },
            msg_type::AUTH_OK => Self::AuthOk { server_hmac: r.fixed::<32>()? },
            msg_type::ERROR => Self::Error { code: ErrorCode::decode(&mut r)?, message: r.str()? },
            msg_type::OK => Self::Ok,
            msg_type::LEASE_ACK => Self::LeaseAck { lease_id: r.var_u64()?, resumed: r.bool()? },
            msg_type::SPAWNED => {
                Self::Spawned { session: SessionId::decode(&mut r)?, pid: r.opt(SignalTarget::decode)? }
            }
            msg_type::OWNS_REPLY => Self::OwnsReply { owned: r.bool()? },
            msg_type::SESSIONS_LIST => {
                Self::SessionsList { sessions: r.vec_bounded(4096, "sessions list", SessionSummary::decode)? }
            }
            msg_type::REWOUND => {
                Self::Rewound { session: SessionId::decode(&mut r)?, duration_ms: r.var_u64()?, at_ms: r.var_u64()? }
            }
            msg_type::SOUGHT => Self::Sought { at_ms: r.var_u64()? },
            msg_type::METRICS_REPLY => Self::MetricsReply(MetricsSnapshot::decode(&mut r)?),
            msg_type::ATTACHED => {
                let session = SessionId::decode(&mut r)?;
                let stream_id = NonZeroStreamId::decode(&mut r)?;
                let size = GridSize::decode(&mut r)?;
                let epoch = r.var_u64()?;
                let first_active_line = r.var_u64()?;
                let scrollback_lines = r.var_u64()?;
                if scrollback_lines > first_active_line {
                    return Err(WireError::InvalidValue("Attached.scrollback_lines > first_active_line"));
                }
                let foreground = r.opt(ForegroundProcess::decode)?;
                Self::Attached { session, stream_id, size, epoch, first_active_line, scrollback_lines, foreground }
            }
            msg_type::WAITED => Self::Waited { outcome: WaitOutcome::decode(&mut r)? },
            msg_type::FRAME => Self::Frame(Box::new(FrameUpdate::decode(&mut r)?)),
            msg_type::ROWS => Self::Rows {
                epoch: r.var_u64()?,
                rows: r.vec_bounded(MAX_ROWS_PER_FETCH as usize, "rows reply", PackedRow::decode)?,
            },
            msg_type::PASTED => Self::Pasted { outcome: PasteOutcome::decode(&mut r)? },
            msg_type::SPAN_REPLY => Self::SpanReply { span: r.opt(CellSpan::decode)? },
            msg_type::TEXT_REPLY => Self::TextReply { text: r.str()? },
            msg_type::SEARCH_MATCHES => Self::SearchMatches {
                epoch: r.var_u64()?,
                matches: r.vec_bounded(MAX_SEARCH_MATCHES, "search matches", CellSpan::decode)?,
                truncated: r.bool()?,
            },
            msg_type::SEARCH_DELTA => {
                let epoch = r.var_u64()?;
                let from_line = r.var_u64()?;
                let to_line = r.var_u64()?;
                if to_line < from_line {
                    return Err(WireError::InvalidValue("search delta to_line < from_line"));
                }
                let added = r.vec_bounded(MAX_SEARCH_MATCHES, "search delta", CellSpan::decode)?;
                for span in &added {
                    if span.start.line < from_line || span.end.line >= to_line {
                        return Err(WireError::InvalidValue("search delta span outside [from_line, to_line)"));
                    }
                }
                Self::SearchDelta { epoch, from_line, to_line, added, truncated: r.bool()? }
            }
            msg_type::COMMANDS_REPLY => {
                Self::CommandsReply { commands: r.vec_bounded(MAX_COMMANDS, "commands", CommandRecord::decode)? }
            }
            msg_type::COMMAND_OUTPUT_REPLY => Self::CommandOutputReply { text: r.opt(Reader::str)? },
            msg_type::IMAGES_REPLY => Self::ImagesReply {
                images_gen: r.var_u64()?,
                placements: r.vec_bounded(4096, "image placements", ImagePlacement::decode)?,
            },
            msg_type::IMAGE_BLOB_CHUNK => {
                let blob = BlobHash::decode(&mut r)?;
                let encoding = ImageEncoding::decode(&mut r)?;
                let total_bytes = r.var_u32()?;
                let offset = r.var_u32()?;
                let data = r.bytes_bounded(BULK_CHUNK_BYTES, "image blob chunk")?.to_vec();
                let end = offset
                    .checked_add(data.len() as u32)
                    .ok_or(WireError::InvalidValue("image blob chunk offset overflow"))?;
                if end > total_bytes {
                    return Err(WireError::InvalidValue("image blob chunk exceeds total_bytes"));
                }
                Self::ImageBlobChunk { blob, encoding, total_bytes, offset, data }
            }
            msg_type::TITLE => Self::Title { title: r.str()? },
            msg_type::BELL => Self::Bell,
            msg_type::CLIPBOARD => Self::Clipboard { selection: r.str()?, data: r.str()? },
            msg_type::NOTIFICATION => Self::Notification { title: r.str()?, body: r.str()? },
            msg_type::PROGRESS => {
                let state = ProgressState::decode(&mut r)?;
                let percent = r.opt(|r| {
                    let p = r.u8()?;
                    if p > 100 { Err(WireError::InvalidValue("progress percent > 100")) } else { Ok(p) }
                })?;
                if percent.is_some() && matches!(state, ProgressState::None | ProgressState::Indeterminate) {
                    return Err(WireError::InvalidValue("progress percent invalid for state"));
                }
                Self::Progress { state, percent }
            }
            msg_type::PROGRAM_STATUS => Self::ProgramStatus(Box::new(StatusRecord::decode(&mut r)?)),
            msg_type::COMMAND_STARTED => Self::CommandStarted(Box::new(CommandRecord::decode(&mut r)?)),
            msg_type::COMMAND_FINISHED => Self::CommandFinished(Box::new(CommandRecord::decode(&mut r)?)),
            msg_type::CWD_CHANGED => Self::CwdChanged { cwd: r.str()? },
            msg_type::EXITED => Self::Exited { exit_code: r.opt(Reader::i32)?, signal: r.opt(Reader::str)? },
            msg_type::OUTPUT => Self::Output { text: r.str()? },
            msg_type::THROTTLED => Self::Throttled { waited_us: r.var_u64()? },
            msg_type::OUTPUT_DROPPED => Self::OutputDropped { bytes: r.var_u64()? },
            msg_type::STREAM_CLOSED => Self::StreamClosed { reason: r.str()? },
            other => return Err(WireError::InvalidTag { kind: "BrokerMsg", tag: other }),
        };
        r.finish()?;
        Ok(msg)
    }

    /// Encodes this message directly into a framed wire packet in `out`.
    pub fn encode_to_frame(&self, stream_id: StreamId, req_id: RequestId, out: &mut Vec<u8>) -> Result<(), WireError> {
        let mut w = Writer::new();
        self.encode(&mut w);
        encode_frame(stream_id, self.msg_type(), req_id, w.as_slice(), out)
    }
}

/// Helper for decoding a client message directly from a [`DecodedFrame`].
pub fn decode_client_frame(frame: DecodedFrame) -> Result<Option<(FrameHeader, ClientMsg)>, WireError> {
    match frame {
        DecodedFrame::Fin => Ok(None),
        DecodedFrame::Frame { header, payload } => {
            let msg = ClientMsg::decode(header.msg_type, &payload)?;
            Ok(Some((header, msg)))
        }
    }
}

/// Helper for decoding a broker message directly from a [`DecodedFrame`].
pub fn decode_broker_frame(frame: DecodedFrame) -> Result<Option<(FrameHeader, BrokerMsg)>, WireError> {
    match frame {
        DecodedFrame::Fin => Ok(None),
        DecodedFrame::Frame { header, payload } => {
            let msg = BrokerMsg::decode(header.msg_type, &payload)?;
            Ok(Some((header, msg)))
        }
    }
}
