//! Deterministic seeded simulation world connecting clients, `BrokerCore`, and `SessionMachine<MockVt>`.
//!
//! Runs completely in-memory over virtual [`TimestampMs`], verifying wire round-trip totality,
//! differential view-replication fidelity (rows, cursor, modes, scrollback, first_active_line,
//! images_gen), exact keyed-mark forgery & replay immunity, `SearchWatch` ground-truth parity
//! across resizes/alt-screen/eviction, DEC 2026 hold bounds, and waiter termination continuously.

use std::collections::{BTreeMap, VecDeque};

use apack_core::auth::AuthConfig;
use apack_core::broker::{BrokerAction, BrokerCore, CloseReason};
use apack_core::lease::ConnId;
use apack_core::marks::MarksMode;
use apack_core::session::{SessionEffect, SessionMachine, StreamAttachmentId, StreamKey};
use apack_core::time::TimestampMs;
use apack_core::vt::VtEngine;
use apack_proto::frame::FrameDecoder;
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, decode_broker_frame};
use apack_proto::types::{
    CellPx, CellSpan, CommandOutcome, CursorShape, CursorState, FrameCredit, GridSize, MarksKey, NonZeroStreamId,
    RequestId, ResizePolicy, SessionId, StreamId, SubFlags, ViewModes, ViewportHint, VouchedExitCode, WaitCondition,
    WaitOutcome,
};
use apack_proto::{MAX_SEARCH_MATCHES, PROTOCOL_VERSION};

use crate::mock_vt::MockVt;

/// Deterministic xorshift64* PRNG for repeatable chaos simulation.
#[derive(Debug, Clone)]
pub struct Rng64 {
    state: u64,
}

impl Rng64 {
    /// Creates a new PRNG from `seed` (mapping `0` to a fixed non-zero constant).
    pub fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }

    /// Generates the next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Generates a pseudo-random index in `0..upper`.
    pub fn gen_range(&mut self, upper: usize) -> usize {
        (self.next_u64() as usize) % upper
    }
}

/// Client-side reconstructed view projection verifying differential `FrameUpdate` fidelity.
#[derive(Debug, Clone)]
pub struct ClientViewModel {
    /// Last applied frame sequence number (must increase strictly monotonically).
    pub last_seq: u64,
    /// Active epoch.
    pub epoch: u64,
    /// Grid dimensions.
    pub size: GridSize,
    /// Monotonic epoch-local line number of viewport row `0`.
    pub first_active_line: u64,
    /// Scrollback depth reported in the last frame.
    pub scrollback_lines: u64,
    /// Last cursor state applied.
    pub cursor: CursorState,
    /// Last view modes applied.
    pub modes: ViewModes,
    /// Last images generation counter applied.
    pub images_gen: u64,
    /// Reconstructed viewport row strings (`len == size.rows`).
    pub rows: Vec<String>,
}

impl ClientViewModel {
    /// Creates an empty client view projection.
    pub fn new(size: GridSize) -> Self {
        Self {
            last_seq: 0,
            epoch: 0,
            size,
            first_active_line: 0,
            scrollback_lines: 0,
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes::default(),
            images_gen: 0,
            rows: vec![String::new(); usize::from(size.rows.get())],
        }
    }

    /// Applies a broker `FrameUpdate`, asserting strict monotonicity and valid row indices.
    pub fn apply_frame(&mut self, frame: &FrameUpdate) {
        assert!(frame.seq > self.last_seq, "frame seq regressed: {} <= {}", frame.seq, self.last_seq);
        self.last_seq = frame.seq;
        let height = usize::from(frame.size.rows.get());
        if frame.epoch != self.epoch || frame.size != self.size {
            assert!(
                frame.full_sync,
                "broker must set full_sync=true on epoch ({} -> {}) or size ({:?} -> {:?}) change",
                self.epoch, frame.epoch, self.size, frame.size
            );
        }
        if frame.full_sync {
            self.epoch = frame.epoch;
            self.size = frame.size;
            self.rows = vec![String::new(); height];
        } else if frame.scroll_delta > 0 {
            let shift = (frame.scroll_delta as usize).min(height);
            self.rows.drain(0..shift);
            self.rows.resize(height, String::new());
        } else if frame.scroll_delta < 0 {
            let shift = (frame.scroll_delta.unsigned_abs() as usize).min(height);
            self.rows.truncate(height - shift);
            for _ in 0..shift {
                self.rows.insert(0, String::new());
            }
        }
        self.first_active_line = frame.first_active_line;
        self.scrollback_lines = frame.scrollback_lines;
        self.cursor = frame.cursor;
        self.modes = frame.modes;
        self.images_gen = frame.images_gen;
        for row in &frame.rows {
            assert!(
                row.line >= frame.first_active_line,
                "frame row line {} below viewport start {}",
                row.line,
                frame.first_active_line
            );
            let idx = (row.line - frame.first_active_line) as usize;
            assert!(idx < height, "frame row index {idx} out of viewport height {height}");
            self.rows[idx] = row.text.clone();
        }
    }
}

/// Client-side reconstructed search highlight projection verifying `SearchMatches` + `SearchDelta` parity.
#[derive(Debug, Clone)]
pub struct ClientSearchModel {
    /// Active query.
    pub query: String,
    /// Case sensitivity.
    pub case_sensitive: bool,
    /// Last applied epoch.
    pub epoch: u64,
    /// Current reconstructed match spans.
    pub matches: Vec<CellSpan>,
}

impl ClientSearchModel {
    /// Creates an empty client search projection.
    pub fn new(query: String, case_sensitive: bool) -> Self {
        Self { query, case_sensitive, epoch: 0, matches: Vec::new() }
    }

    /// Applies a full `SearchMatches` snapshot.
    pub fn apply_full(&mut self, epoch: u64, matches: Vec<CellSpan>) {
        self.epoch = epoch;
        self.matches = matches;
    }

    /// Applies an incremental `SearchDelta` replacing `[from_line, to_line)`.
    pub fn apply_delta(&mut self, epoch: u64, from_line: u64, to_line: u64, added: Vec<CellSpan>) {
        if epoch != self.epoch {
            return;
        }
        self.matches.retain(|s| s.start.line < from_line || s.start.line >= to_line);
        self.matches.extend(added);
        self.matches.sort_by_key(|s| (s.start.line, s.start.col, s.end.line, s.end.col));
    }
}

/// Summary statistics and deterministic digest returned by [`SimWorld::run_chaos`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimReport {
    /// Total virtual milliseconds elapsed.
    pub elapsed_ms: u64,
    /// Total wire frames decoded end-to-end by simulated clients.
    pub wire_frames_decoded: u64,
    /// Total differential/full `FrameUpdate`s verified against `MockVt` ground truth.
    pub frames_verified: u64,
    /// Total `CommandFinished` events verified in keyed sessions.
    pub keyed_commands_finished: u64,
    /// Total reactive `Wait` calls completed.
    pub waits_completed: u64,
    /// Deterministic FNV-1a digest of observed broker messages for exact-reproducibility checks.
    pub trace_hash: u64,
}

/// Deterministic simulation world wiring `BrokerCore` + `SessionMachine<MockVt>` + virtual clients.
#[derive(Debug)]
pub struct SimWorld {
    now: TimestampMs,
    rng: Rng64,
    broker: BrokerCore,
    sessions: BTreeMap<SessionId, SessionMachine<MockVt>>,
    stream_sessions: BTreeMap<StreamKey, SessionId>,
    keyed_sessions: BTreeMap<SessionId, MarksKey>,
    expected_vouched: BTreeMap<SessionId, VecDeque<(u64, i32)>>,
    views: BTreeMap<StreamKey, ClientViewModel>,
    searches: BTreeMap<StreamKey, ClientSearchModel>,
    pending_acks: BTreeMap<StreamKey, u64>,
    auto_ack: bool,
    client_decoders: BTreeMap<ConnId, FrameDecoder>,
    wire_frames_decoded: u64,
    frames_verified: u64,
    keyed_commands_finished: u64,
    waits_completed: u64,
    trace_hash: u64,
}

impl SimWorld {
    /// Creates a new simulation world with deterministic `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            now: TimestampMs::ZERO,
            rng: Rng64::new(seed),
            broker: BrokerCore::new(AuthConfig::None),
            sessions: BTreeMap::new(),
            stream_sessions: BTreeMap::new(),
            keyed_sessions: BTreeMap::new(),
            expected_vouched: BTreeMap::new(),
            views: BTreeMap::new(),
            searches: BTreeMap::new(),
            pending_acks: BTreeMap::new(),
            auto_ack: true,
            client_decoders: BTreeMap::new(),
            wire_frames_decoded: 0,
            frames_verified: 0,
            keyed_commands_finished: 0,
            waits_completed: 0,
            trace_hash: 0xcbf2_9ce4_8422_2325,
        }
    }

    /// Current virtual timestamp.
    pub fn now(&self) -> TimestampMs {
        self.now
    }

    /// Advances virtual time by `delta_ms` and fires all due broker and session timers.
    pub fn advance(&mut self, delta_ms: u64) {
        self.now = self.now.saturating_add(delta_ms);
        let mut b_actions = Vec::new();
        self.broker.poll_timers(self.now, &mut b_actions);
        self.process_broker_actions(b_actions);

        let sids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for sid in sids {
            let mut s_effects = Vec::new();
            if let Some(sess) = self.sessions.get_mut(&sid) {
                sess.poll_timers(self.now, &mut s_effects);
            }
            self.process_session_effects(sid, s_effects);
        }
        self.pump_all_outbound();
    }

    /// Spawns a simulated session inside the world and registers it with `BrokerCore`.
    pub fn add_session(&mut self, id: SessionId, size: GridSize, policy: ResizePolicy, marks_mode: MarksMode) {
        self.add_session_with_scrollback(id, size, 256, policy, marks_mode);
    }

    /// Spawns a simulated session with a custom scrollback depth (useful for scrollback-eviction stress).
    pub fn add_session_with_scrollback(
        &mut self,
        id: SessionId,
        size: GridSize,
        scrollback_limit: usize,
        policy: ResizePolicy,
        marks_mode: MarksMode,
    ) {
        if let MarksMode::Keyed(k) = marks_mode {
            self.keyed_sessions.insert(id, k);
            self.expected_vouched.insert(id, VecDeque::new());
        }
        let vt = MockVt::new(size, scrollback_limit);
        let sess = SessionMachine::new(id, vt, policy, marks_mode, CellPx { width: 8, height: 16 });
        self.broker.register_session(id, false, false);
        self.sessions.insert(id, sess);
    }

    /// Opens a client connection and completes the v2 `Hello` handshake via random-chunk framing.
    pub fn connect_client(&mut self, conn: ConnId) {
        let nonce = [0x42; 32];
        self.broker.open_connection(conn, nonce, self.now).unwrap();
        self.client_decoders.insert(conn, FrameDecoder::new());
        self.send_client_msg(
            conn,
            StreamId::CONTROL,
            RequestId::from_raw(1),
            &ClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_nonce: [0x11; 32],
                client_name: format!("sim-{}", conn.get()),
            },
        );
    }

    /// Encodes `msg` and feeds it into `BrokerCore` in random 1..=13 byte chunks (chunk-boundary totality!).
    pub fn send_client_msg(&mut self, conn: ConnId, stream: StreamId, req: RequestId, msg: &ClientMsg) {
        if let ClientMsg::SetSearch { query, case_sensitive } = msg
            && let Some(nz) = NonZeroStreamId::new(stream.raw())
        {
            if query.is_empty() {
                self.searches.remove(&(conn, nz));
            } else {
                self.searches.insert((conn, nz), ClientSearchModel::new(query.clone(), *case_sensitive));
            }
        }
        let mut wire = Vec::new();
        msg.encode_to_frame(stream, req, &mut wire).unwrap();
        let mut offset = 0usize;
        while offset < wire.len() {
            let chunk_len = (self.rng.gen_range(13) + 1).min(wire.len() - offset);
            let mut b_actions = Vec::new();
            self.broker.feed_bytes(conn, &wire[offset..offset + chunk_len], self.now, &mut b_actions);
            self.process_broker_actions(b_actions);
            offset += chunk_len;
        }
        self.pump_all_outbound();
    }

    /// Feeds raw PTY output bytes into session `sid` (randomly split across 1..=16 byte chunks) and pumps outbound.
    pub fn feed_pty(&mut self, sid: SessionId, bytes: &[u8]) {
        let mut offset = 0usize;
        while offset < bytes.len() {
            let chunk_len = (self.rng.gen_range(16) + 1).min(bytes.len() - offset);
            let chunk = &bytes[offset..offset + chunk_len];
            let mut b_actions = Vec::new();
            self.broker.publish_raw_output(sid, chunk, self.now, &mut b_actions);
            self.process_broker_actions(b_actions);

            let mut s_effects = Vec::new();
            if let Some(sess) = self.sessions.get_mut(&sid) {
                sess.on_pty_output(chunk, self.now, &mut s_effects);
            }
            self.process_session_effects(sid, s_effects);
            self.pump_all_outbound();
            offset += chunk_len;
        }
    }

    /// Returns the reconstructed client view model for `(conn, stream)`, if any frames were received.
    pub fn client_view(&self, conn: ConnId, stream: NonZeroStreamId) -> Option<&ClientViewModel> {
        self.views.get(&(conn, stream))
    }

    /// Simulates process exit for session `sid`, flushing exit effects and pumping outbound frames.
    pub fn exit_session(&mut self, sid: SessionId, exit_code: Option<i32>, signal: Option<String>) {
        let mut s_effects = Vec::new();
        if let Some(sess) = self.sessions.get_mut(&sid) {
            sess.on_process_exit(exit_code, signal, self.now, &mut s_effects);
        }
        self.process_session_effects(sid, s_effects);
        self.sessions.remove(&sid);
        let mut b_actions = Vec::new();
        self.broker.unregister_session(sid, "session ended", self.now, &mut b_actions);
        self.process_broker_actions(b_actions);
        self.pump_all_outbound();
    }

    /// Flushes any delayed client `FrameAck`s accumulated while `auto_ack == false`.
    pub fn flush_pending_acks(&mut self) {
        let pending = core::mem::take(&mut self.pending_acks);
        for ((cid, nz), seq) in pending {
            let mut ack_wire = Vec::new();
            ClientMsg::FrameAck { seq, credit: FrameCredit { max_frames: 2, max_bytes: 262_144 } }
                .encode_to_frame(nz.as_stream_id(), RequestId::PUSH, &mut ack_wire)
                .unwrap();
            let mut b_actions = Vec::new();
            self.broker.feed_bytes(cid, &ack_wire, self.now, &mut b_actions);
            self.process_broker_actions(b_actions);
        }
        self.pump_all_outbound();
    }

    /// Verifies that every attached `SUB_VIEW` client's reconstructed viewport matches `MockVt` ground truth
    /// across rows, epoch, size, `first_active_line`, `scrollback_lines`, cursor, modes, and `images_gen`.
    pub fn assert_views_match_ground_truth(&self) {
        for (&key, client_view) in &self.views {
            let Some(&sid) = self.stream_sessions.get(&key) else {
                continue;
            };
            let Some(sess) = self.sessions.get(&sid) else {
                continue;
            };
            if !sess.is_sync_held() {
                let meta = sess.vt().meta();
                assert_eq!(client_view.epoch, meta.epoch, "epoch mismatch for view {key:?}");
                assert_eq!(client_view.size, meta.size, "size mismatch for view {key:?}");
                assert_eq!(
                    client_view.first_active_line, meta.first_active_line,
                    "first_active_line mismatch for view {key:?}"
                );
                assert_eq!(
                    client_view.scrollback_lines, meta.scrollback_lines,
                    "scrollback_lines mismatch for view {key:?}"
                );
                assert_eq!(client_view.cursor, meta.cursor, "cursor mismatch for view {key:?}");
                assert_eq!(client_view.modes, meta.modes, "modes mismatch for view {key:?}");
                assert_eq!(client_view.images_gen, meta.images_gen, "images_gen mismatch for view {key:?}");
                for r in 0..meta.size.rows.get() {
                    let abs = meta.first_active_line + u64::from(r);
                    let expected = sess.vt().line_text(abs).unwrap_or("").trim_end_matches(' ');
                    assert_eq!(
                        client_view.rows[usize::from(r)],
                        expected,
                        "viewport row {r} diverged between broker MockVt and client view"
                    );
                }
            }
        }
    }

    /// Verifies every active `SearchWatch` client's reconstructed matches equal a full VT ground-truth scan.
    pub fn assert_searches_match_ground_truth(&self) {
        for (&key, model) in &self.searches {
            let Some(&sid) = self.stream_sessions.get(&key) else {
                continue;
            };
            let Some(sess) = self.sessions.get(&sid) else {
                continue;
            };
            let meta = sess.vt().meta();
            assert_eq!(model.epoch, meta.epoch, "search watch epoch diverged for {key:?}");
            let mut expected = Vec::new();
            let _ = sess.vt().search_lines(
                meta.oldest_line(),
                meta.total_lines(),
                &model.query,
                model.case_sensitive,
                MAX_SEARCH_MATCHES,
                &mut expected,
            );
            assert_eq!(model.matches, expected, "SearchWatch highlights diverged from VT ground truth for {key:?}");
        }
    }

    /// Runs a deterministic seeded chaos workload exercising frames, delayed acks, opposing-sign scrolls,
    /// scrollback eviction, alt-screen, DEC 2026 holds, hostile forged/replayed marks, keyed vouched marks,
    /// OSC 7501 status, incremental search, reactive waits, resizes, and raw output.
    pub fn run_chaos(seed: u64, steps: usize) -> SimReport {
        let mut world = Self::new(seed);
        let key = MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap();
        let s1 = SessionId::new(1).unwrap();
        let c1 = ConnId::new(1).unwrap();
        let st1 = NonZeroStreamId::new(1).unwrap();

        // Small scrollback limit (32) guarantees scrollback eviction occurs heavily under chaos!
        world.add_session_with_scrollback(
            s1,
            GridSize::new(40, 8).unwrap(),
            32,
            ResizePolicy::LatestActive,
            MarksMode::Keyed(key),
        );
        world.connect_client(c1);
        world.send_client_msg(
            c1,
            StreamId::CONTROL,
            RequestId::from_raw(2),
            &ClientMsg::Attach {
                session: s1,
                stream_id: st1,
                flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS | SubFlags::RAW_OUTPUT).unwrap(),
                initial_viewport: Some(ViewportHint {
                    size: GridSize::new(40, 8).unwrap(),
                    cell_px: CellPx { width: 8, height: 16 },
                    visible: true,
                    focused: true,
                }),
                initial_output_credit: 65_536,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 262_144 },
            },
        );
        world.send_client_msg(
            c1,
            st1.as_stream_id(),
            RequestId::from_raw(3),
            &ClientMsg::SetSearch { query: "needle".into(), case_sensitive: true },
        );

        let mut req_seq = 10u32;
        let mut mark_seq = 0u64;
        let mut in_alt = false;
        for step in 0..steps {
            match world.rng.gen_range(12) {
                0 | 1 => {
                    let line = format!("step-{step} needle-{}\r\n", world.rng.gen_range(100));
                    world.feed_pty(s1, line.as_bytes());
                }
                2 => {
                    // Hostile forged OSC 633/133 marks AND replayed keyed marks (`mark_seq` is 0 or already used):
                    // MUST NEVER produce CommandFinished!
                    let bad_replay = format!(
                        "\x1b]633;C\x07forged\r\n\x1b]633;D;0\x07\x1b]133;D;0\x07\
                         \x1b]633;V;S;{mark_seq};{};replay\x07\x1b]633;V;F;{mark_seq};{};0;/tmp\x07\
                         \x1b]633;V;F;999999;ffffffffffffffffffffffffffffffff;0;/tmp\x07",
                        key.as_str(),
                        key.as_str(),
                    );
                    world.feed_pty(s1, bad_replay.as_bytes());
                }
                3 => {
                    // Genuine keyed shell command lifecycle with strictly monotonic seq!
                    mark_seq += 1;
                    let cmd_seq = mark_seq;
                    let code = (world.rng.gen_range(5)) as i32;
                    world.expected_vouched.get_mut(&s1).unwrap().push_back((cmd_seq, code));
                    let seq = format!(
                        "\x1b]633;V;S;{cmd_seq};{};run-{step}\x07needle-out-{step}\r\n\x1b]633;V;F;{cmd_seq};{};{code};/tmp\x07",
                        key.as_str(),
                        key.as_str(),
                    );
                    world.feed_pty(s1, seq.as_bytes());
                }
                4 => {
                    // DEC 2026 hold + output + either release or safety-cap expiry
                    world.feed_pty(s1, b"\x1b[?2026hheld-frame\r\n");
                    if world.rng.gen_range(2) == 0 {
                        world.feed_pty(s1, b"\x1b[?2026l");
                    } else {
                        world.advance(1_000);
                        assert!(!world.sessions.get(&s1).unwrap().is_sync_held(), "DEC 2026 hold exceeded 1000ms cap");
                    }
                }
                5 => {
                    // ViewportHint resize
                    let cols = 30 + (world.rng.gen_range(40) as u16);
                    let rows = 6 + (world.rng.gen_range(10) as u16);
                    req_seq += 1;
                    world.send_client_msg(
                        c1,
                        st1.as_stream_id(),
                        RequestId::from_raw(req_seq),
                        &ClientMsg::ViewportHint(ViewportHint {
                            size: GridSize::new(cols, rows).unwrap(),
                            cell_px: CellPx { width: 8, height: 16 },
                            visible: true,
                            focused: true,
                        }),
                    );
                }
                6 => {
                    // Reactive Wait with short deadline
                    req_seq += 1;
                    world.send_client_msg(
                        c1,
                        StreamId::CONTROL,
                        RequestId::from_raw(req_seq),
                        &ClientMsg::Wait {
                            session: s1,
                            condition: WaitCondition::Text(format!("wake-{step}")),
                            timeout_ms: 50,
                        },
                    );
                    if world.rng.gen_range(2) == 0 {
                        world.feed_pty(s1, format!("wake-{step}\r\n").as_bytes());
                    } else {
                        world.advance(50);
                    }
                }
                7 => {
                    // Toggle alt-screen (?1049h / ?1049l) + needle output
                    in_alt = !in_alt;
                    let seq = if in_alt { "\x1b[?1049halt-needle\r\n" } else { "\x1b[?1049lmain-needle\r\n" };
                    world.feed_pty(s1, seq.as_bytes());
                }
                8 => {
                    // Backpressured view: pause acks, accumulate opposing-sign scrolls + dirty lines, then flush acks!
                    world.auto_ack = false;
                    world.feed_pty(s1, format!("coalesce-up-{step}\r\n\x1b[Tcoalesce-down-needle\r\n").as_bytes());
                    world.auto_ack = true;
                    world.flush_pending_acks();
                }
                9 => {
                    // OSC 7501 program status burst
                    world.feed_pty(s1, b"\x1b]7501;state=working;app=build\x07\x1b]7501;state=done;app=build\x07");
                }
                10 => {
                    // Scroll down + needle
                    world.feed_pty(s1, b"\x1b[Ttop-needle\r\n");
                }
                _ => {
                    world.advance(150);
                }
            }
            world.assert_views_match_ground_truth();
        }

        // Release any active DEC 2026 hold, flush acks & SearchFlush timers, and verify ALL oracles!
        world.auto_ack = true;
        world.flush_pending_acks();
        world.advance(1_100);
        world.assert_views_match_ground_truth();
        world.assert_searches_match_ground_truth();
        assert!(world.expected_vouched.get(&s1).unwrap().is_empty(), "not all expected vouched commands finished");

        SimReport {
            elapsed_ms: world.now.as_millis(),
            wire_frames_decoded: world.wire_frames_decoded,
            frames_verified: world.frames_verified,
            keyed_commands_finished: world.keyed_commands_finished,
            waits_completed: world.waits_completed,
            trace_hash: world.trace_hash,
        }
    }

    fn process_broker_actions(&mut self, actions: Vec<BrokerAction>) {
        for action in actions {
            match action {
                BrokerAction::StreamAttached {
                    conn,
                    stream_id,
                    attachment_id,
                    session,
                    flags,
                    initial_viewport,
                    ..
                } => {
                    self.stream_sessions.insert((conn, stream_id), session);
                    if flags.contains(SubFlags::VIEW) {
                        let size = self
                            .sessions
                            .get(&session)
                            .map_or(GridSize::new(80, 24).unwrap(), SessionMachine::current_size);
                        self.views.insert((conn, stream_id), ClientViewModel::new(size));
                    }
                    let mut s_effects = Vec::new();
                    if let Some(sess) = self.sessions.get_mut(&session) {
                        sess.attach_stream((conn, stream_id), attachment_id, flags, initial_viewport, &mut s_effects);
                    }
                    self.process_session_effects(session, s_effects);
                }
                BrokerAction::StreamDetached { conn, stream_id, session, .. } => {
                    self.stream_sessions.remove(&(conn, stream_id));
                    self.views.remove(&(conn, stream_id));
                    self.searches.remove(&(conn, stream_id));
                    self.pending_acks.remove(&(conn, stream_id));
                    let mut s_effects = Vec::new();
                    if let Some(sess) = self.sessions.get_mut(&session) {
                        sess.detach_stream((conn, stream_id), &mut s_effects);
                    }
                    self.process_session_effects(session, s_effects);
                }
                BrokerAction::DispatchToSession { conn, stream_id, attachment_id, req_id, session, msg } => {
                    let mut s_effects = Vec::new();
                    if let Some(sess) = self.sessions.get_mut(&session) {
                        sess.handle_stream_msg((conn, stream_id), attachment_id, req_id, msg, &mut s_effects);
                    }
                    self.process_session_effects(session, s_effects);
                }
                BrokerAction::WaitSession { conn, req_id, session, condition, timeout_ms } => {
                    let mut s_effects = Vec::new();
                    if let Some(sess) = self.sessions.get_mut(&session) {
                        sess.register_wait(conn, req_id, condition, timeout_ms, self.now, &mut s_effects);
                    }
                    self.process_session_effects(session, s_effects);
                }
                BrokerAction::FrameCreditUpdated { conn, stream_id, attachment_id, session, .. } => {
                    self.publish_dirty_view(session, (conn, stream_id), attachment_id);
                }
                BrokerAction::CloseConn { conn, reason, .. } => {
                    let _ = reason;
                    self.client_decoders.remove(&conn);
                    for sess in self.sessions.values_mut() {
                        sess.cancel_waiters_for_conn(conn);
                    }
                }
                _ => {}
            }
        }
    }

    fn flush_dirty_views(&mut self, sid: SessionId) {
        let subs = self.sessions.get(&sid).map_or_else(Vec::new, SessionMachine::view_subscribers);
        for (key, att) in subs {
            self.publish_dirty_view(sid, key, att);
        }
    }

    fn process_session_effects(&mut self, sid: SessionId, effects: Vec<SessionEffect>) {
        let mut views_dirty = false;
        for ef in effects {
            match ef {
                SessionEffect::Send { conn, stream_id, attachment, req_id, priority, msg } => {
                    if matches!(msg, BrokerMsg::Exited { .. }) {
                        views_dirty = false;
                        self.flush_dirty_views(sid);
                    }
                    if let Some(att) = attachment {
                        let Some(nz) = NonZeroStreamId::new(stream_id.raw()) else {
                            continue;
                        };
                        if !self.broker.is_attachment_active(conn, nz, att) {
                            continue;
                        }
                    }
                    let mut b_actions = Vec::new();
                    self.broker.enqueue_message(conn, stream_id, req_id, priority, msg, self.now, &mut b_actions);
                    self.process_broker_actions(b_actions);
                }
                SessionEffect::ViewsDirty => views_dirty = true,
                _ => {}
            }
        }
        if views_dirty {
            self.flush_dirty_views(sid);
        }
    }

    fn publish_dirty_view(&mut self, sid: SessionId, key: StreamKey, attachment_id: StreamAttachmentId) {
        let (conn, stream_id) = key;
        let now = self.now;
        let broker = &mut self.broker;
        let mut b_actions = Vec::new();
        if let Some(sess) = self.sessions.get_mut(&sid) {
            let _ = sess.try_publish_view(key, |frame| {
                broker.publish_frame(conn, stream_id, attachment_id, frame, now, &mut b_actions)
            });
        }
        self.process_broker_actions(b_actions);
    }

    fn pump_all_outbound(&mut self) {
        loop {
            let cids: Vec<ConnId> = self.client_decoders.keys().copied().collect();
            let mut progressed = false;
            for cid in cids {
                while let Some(queued) = self.broker.pop_outbound(cid) {
                    progressed = true;
                    // Encode and stream-decode through FrameDecoder + BrokerMsg::decode (wire totality!).
                    let mut wire = Vec::new();
                    queued
                        .msg
                        .encode_to_frame(queued.stream_id, queued.req_id, &mut wire)
                        .expect("broker message must frame");
                    let dec = self.client_decoders.get_mut(&cid).expect("client decoder");
                    let mut decoded_frame = None;
                    let mut offset = 0usize;
                    while offset < wire.len() {
                        let step = (self.rng.gen_range(17) + 1).min(wire.len() - offset);
                        let (used, item) = dec.step(&wire[offset..offset + step]).expect("valid broker wire");
                        offset += used;
                        if item.is_some() {
                            decoded_frame = item;
                        }
                    }
                    let (hdr, msg) = decode_broker_frame(decoded_frame.expect("complete frame"))
                        .expect("valid BrokerMsg decode")
                        .expect("non-FIN frame");
                    assert_eq!(msg, queued.msg);
                    self.wire_frames_decoded += 1;
                    self.record_trace_hash(hdr.msg_type);

                    // Inspect invariants and send FrameAck / OutputCredit from the simulated client.
                    match &msg {
                        BrokerMsg::Frame(frame) => {
                            let nz = NonZeroStreamId::new(hdr.stream_id.raw()).unwrap();
                            if let Some(view) = self.views.get_mut(&(cid, nz)) {
                                view.apply_frame(frame);
                                self.frames_verified += 1;
                            }
                            if self.auto_ack {
                                let seq = frame.seq;
                                let mut ack_wire = Vec::new();
                                ClientMsg::FrameAck { seq, credit: FrameCredit { max_frames: 2, max_bytes: 262_144 } }
                                    .encode_to_frame(hdr.stream_id, RequestId::PUSH, &mut ack_wire)
                                    .unwrap();
                                let mut b_actions = Vec::new();
                                self.broker.feed_bytes(cid, &ack_wire, self.now, &mut b_actions);
                                self.process_broker_actions(b_actions);
                            } else {
                                self.pending_acks.insert((cid, nz), frame.seq);
                            }
                        }
                        BrokerMsg::SearchMatches { epoch, matches, .. } => {
                            if let Some(nz) = NonZeroStreamId::new(hdr.stream_id.raw())
                                && let Some(model) = self.searches.get_mut(&(cid, nz))
                            {
                                model.apply_full(*epoch, matches.clone());
                            }
                        }
                        BrokerMsg::SearchDelta { epoch, from_line, to_line, added, .. } => {
                            if let Some(nz) = NonZeroStreamId::new(hdr.stream_id.raw())
                                && let Some(model) = self.searches.get_mut(&(cid, nz))
                            {
                                model.apply_delta(*epoch, *from_line, *to_line, added.clone());
                            }
                        }
                        BrokerMsg::CommandFinished(rec) => {
                            // Exact keyed-session forgery & replay oracle: ONLY expected vouched (seq, code)
                            // may ever appear on a keyed session stream!
                            if let Some(nz) = NonZeroStreamId::new(hdr.stream_id.raw())
                                && let Some(&sid) = self.stream_sessions.get(&(cid, nz))
                                && self.keyed_sessions.contains_key(&sid)
                            {
                                let (exp_seq, exp_code) = self
                                    .expected_vouched
                                    .get_mut(&sid)
                                    .and_then(VecDeque::pop_front)
                                    .expect("unexpected/forged CommandFinished on keyed session!");
                                assert_eq!(rec.id, exp_seq, "keyed command seq mismatch (replay/forgery?)");
                                assert_eq!(
                                    rec.outcome,
                                    CommandOutcome::Exited(VouchedExitCode::from_vouched(exp_code)),
                                    "keyed session outcome mismatch"
                                );
                                self.keyed_commands_finished += 1;
                            }
                        }
                        BrokerMsg::Output { text } => {
                            let mut cr_wire = Vec::new();
                            ClientMsg::OutputCredit { bytes: text.len() as u32 }
                                .encode_to_frame(hdr.stream_id, RequestId::PUSH, &mut cr_wire)
                                .unwrap();
                            let mut b_actions = Vec::new();
                            self.broker.feed_bytes(cid, &cr_wire, self.now, &mut b_actions);
                            self.process_broker_actions(b_actions);
                        }
                        BrokerMsg::Waited { outcome } => {
                            assert!(matches!(outcome, WaitOutcome::Met | WaitOutcome::Timeout | WaitOutcome::Exited));
                            self.waits_completed += 1;
                        }
                        _ => {}
                    }
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn record_trace_hash(&mut self, tag: u16) {
        self.trace_hash ^= u64::from(tag);
        self.trace_hash = self.trace_hash.wrapping_mul(0x0100_0000_01b3);
    }

    /// Cleanly disconnects a simulated client (useful in tests verifying disconnect/teardown).
    pub fn disconnect_client(&mut self, conn: ConnId) {
        let mut b_actions = Vec::new();
        self.broker.close_connection(conn, CloseReason::TransportClosed, self.now, &mut b_actions);
        self.process_broker_actions(b_actions);
    }
}
