//! Pure sans-IO `BrokerCore` connection, handshake, multiplexing, lease, and flow-control machine.
//!
//! All transport bytes, timers, and session callbacks enter `BrokerCore` as explicit method calls;
//! all side effects exit as deterministic [`BrokerAction`] values plus prioritized [`OutboundQueue`]
//! frames drained via [`BrokerCore::pop_outbound`].

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use apack_proto::codec::Writer;
use apack_proto::frame::{DecodedFrame, FrameDecoder, FrameHeader};
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SpawnSpec, msg_type};
use apack_proto::types::{
    ErrorCode, FrameCredit, LeaseKind, MetricsSnapshot, NonZeroStreamId, RequestId, SessionId, SignalKind, SignalScope,
    SignalTarget, StreamId, SubFlags, ViewportHint, WaitCondition,
};
use apack_proto::{FRAME_HEADER_LEN, MAX_AUTH_FRAME_PAYLOAD, MAX_HELLO_FRAME_PAYLOAD, PROTOCOL_VERSION};

use crate::auth::{AuthConfig, compute_server_proof, verify_client_proof};
use crate::flow::{FramePacer, MemoryBudget, OutboundPriority, OutboundQueue, QueuedMessage, RawOutputFlow};
use crate::lease::{ConnId, LeaseError, LeaseId, LeaseTable};
pub use crate::session::StreamAttachmentId;
use crate::time::{TimerId, TimerWheel, TimestampMs};
use crate::{
    AUTH_HANDSHAKE_TIMEOUT_MS, BROKER_CAPABILITIES, DEFAULT_WORD_DELIMITERS, MAX_CONTROL_QUEUE_BYTES,
    MAX_STREAMS_PER_CONN,
};

/// Maximum wire-encoded size of any single protocol frame (14-byte header + 8 MiB payload).
pub(crate) const MAX_WIRE_FRAME_BYTES: usize = apack_proto::FRAME_HEADER_LEN + apack_proto::MAX_FRAME_PAYLOAD;

/// Client name reported by `apack-terminal inspect` so read-only diagnostic probes do not flip
/// idle broker lifecycle tracking (`ever_connected`) or pin `LeaseBound` sessions.
pub const INSPECT_CLIENT_NAME: &str = "apack-inspect";

/// Reason why [`BrokerCore::try_publish_frame`] deferred or declined a `FrameUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameStallReason {
    /// Stream already has an unsent Priority 1 frame or `MemoryBudget` is temporarily full.
    OutboundOrBudget,
    /// `FramePacer` window (`max_frames` or `max_bytes`) cannot admit this frame until `FrameAck`.
    Pacer,
    /// Connection, stream, or attachment is no longer active; caller should drop the frame.
    Stale,
}

/// Error returned by [`BrokerCore::open_connection`] if `conn` is already registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateConnId;

/// Why `BrokerCore` instructed the transport reactor to close a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Client sent the universal `[0, 0, 0, 0]` `FIN` frame.
    ClientFin,
    /// Underlying socket/pipe reached EOF or I/O error.
    TransportClosed,
    /// Client violated binary framing or message decoding rules.
    ProtocolError,
    /// Handshake ordering, version, or protocol violation.
    HandshakeFailed,
    /// Handshake/auth did not finish before [`AUTH_HANDSHAKE_TIMEOUT_MS`].
    HandshakeTimeout,
    /// Mutual HMAC-SHA256 authentication failed.
    Unauthorized,
    /// Connection's outbound queue hit its hard byte cap.
    SlowReader,
}

/// Side effect emitted by [`BrokerCore`] for the OS transport reactor or session shards to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerAction {
    /// One or more frames are queued in `conn`'s [`OutboundQueue`] ready for `writev`.
    OutboundReady(ConnId),
    /// Client sent the universal `[0, 0, 0, 0]` `FIN` frame; reactor should half-close reads and
    /// call [`BrokerCore::close_connection`] with [`CloseReason::ClientFin`] once in-flight shard
    /// commands for `conn` finish draining.
    ClientFin(ConnId),
    /// Close the connection's socket/pipe (flushing `drained` and `final_error` beforehand if present).
    CloseConn {
        /// Connection to close.
        conn: ConnId,
        /// Structured close reason.
        reason: CloseReason,
        /// Optional final error frame to send best-effort before closing the transport.
        final_error: Option<(StreamId, RequestId, BrokerMsg)>,
        /// Already-queued ordered replies drained on graceful `ClientFin`.
        drained: Vec<QueuedMessage>,
    },
    /// Spawn a new live session on a session shard.
    SpawnSession {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Validated spawn specification.
        spec: Box<SpawnSpec>,
    },
    /// Terminate a live session.
    KillSession {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Session to kill.
        session: SessionId,
    },
    /// Deliver a POSIX signal to a session.
    SignalSession {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Target session.
        session: SessionId,
        /// Signal kind.
        signal: SignalKind,
        /// Process scope.
        scope: SignalScope,
    },
    /// Query whether `pid` belongs to `session`'s process tree.
    QueryOwns {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Target session.
        session: SessionId,
        /// Positive PID to check.
        pid: SignalTarget,
    },
    /// Request a full list of sessions from the shard registry.
    ListSessions {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
    },
    /// Open a segmented recording directory for rewind playback.
    RewindRecording {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Recording directory path.
        record_dir: String,
        /// Optional initial seek timestamp.
        at_ms: Option<u64>,
    },
    /// Seek an open rewound session.
    SeekRecording {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Rewound session ID.
        session: SessionId,
        /// Target offset in milliseconds.
        at_ms: u64,
    },
    /// Update `kept` status for a session.
    SetSessionKept {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Target session.
        session: SessionId,
        /// New kept state.
        kept: bool,
    },
    /// Register a reactive event-driven wait on a session.
    WaitSession {
        /// Requesting connection.
        conn: ConnId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Target session.
        session: SessionId,
        /// Wait condition.
        condition: WaitCondition,
        /// Caller deadline in milliseconds.
        timeout_ms: u32,
    },
    /// Notify the owning shard that a stream attached to `session`.
    StreamAttached {
        /// Owning connection.
        conn: ConnId,
        /// Request correlation ID of `Attach`.
        req_id: RequestId,
        /// Attached stream ID.
        stream_id: NonZeroStreamId,
        /// Monotonic attachment lifecycle ID minted by `BrokerCore`.
        attachment_id: StreamAttachmentId,
        /// Target session.
        session: SessionId,
        /// Subscription flags.
        flags: SubFlags,
        /// Optional initial viewport hint.
        initial_viewport: Option<ViewportHint>,
    },
    /// Notify the owning shard that a stream detached from `session`.
    StreamDetached {
        /// Owning connection.
        conn: ConnId,
        /// Detached stream ID.
        stream_id: NonZeroStreamId,
        /// Monotonic attachment lifecycle ID that ended.
        attachment_id: StreamAttachmentId,
        /// Target session.
        session: SessionId,
    },
    /// Forward an authenticated, stream-validated per-session client message to the session shard.
    DispatchToSession {
        /// Requesting connection.
        conn: ConnId,
        /// Attached non-zero stream ID.
        stream_id: NonZeroStreamId,
        /// Active attachment lifecycle ID.
        attachment_id: StreamAttachmentId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Bound session ID.
        session: SessionId,
        /// Client message to execute.
        msg: ClientMsg,
    },
    /// Notify a `SUB_VIEW` shard that a view acknowledged a frame and may now accept more frames.
    FrameCreditUpdated {
        /// Owning connection.
        conn: ConnId,
        /// Stream ID.
        stream_id: NonZeroStreamId,
        /// Active attachment lifecycle ID.
        attachment_id: StreamAttachmentId,
        /// Bound session ID.
        session: SessionId,
        /// Updated credit window.
        credit: FrameCredit,
    },
    /// Engage or release PTY read pause for `session` due to `BACKPRESSURE_PTY` raw-output credit.
    SetPtyPaused {
        /// Target session.
        session: SessionId,
        /// `true` to pause PTY reads, `false` to resume.
        paused: bool,
    },
    /// Last broker-wide lease ended (or its grace timer expired), or the last unleased client
    /// disconnected: tear down all non-kept, non-pinned `LeaseBound` sessions.
    TeardownNonKeptSessions,
    /// Last `LeaseKind::Session` pinning a `LeaseBound` session ended.
    SessionUnpinned(SessionId),
    /// Explicitly tear down a single orphaned or owner-disconnected (`ConnBound`) session.
    TeardownSession(SessionId),
}

/// Explicit ownership model governing when a session is automatically torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifetime {
    /// Survives all client disconnects and broker lease releases (only explicit `Kill` or broker shutdown ends it).
    Kept,
    /// Owned by broker/session leases, or by active client connections when no broker lease exists.
    LeaseBound,
    /// Bound to the specific connection that opened it (used for `Rewind` recordings: closed as soon
    /// as `owner` disconnects, unless pinned by a `LeaseKind::Session`).
    ConnBound(ConnId),
}

/// Aggregate shard counters fed into `BrokerCore` for `Metrics` replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShardTelemetry {
    /// Aggregate scrollback bytes across all sessions.
    pub scrollback_bytes: u64,
    /// Aggregate bytes in the global deduplicated image store.
    pub image_store_bytes: u64,
    /// Cumulative PTY bytes parsed since broker startup.
    pub total_pty_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrokerTimer {
    HandshakeTimeout(ConnId),
    LeaseGrace(LeaseId),
}

#[derive(Debug)]
enum ConnPhase {
    AwaitingHello { decoder: FrameDecoder, server_nonce: [u8; 32], timeout: TimerId },
    AwaitingAuth { decoder: FrameDecoder, client_nonce: [u8; 32], server_nonce: [u8; 32], timeout: TimerId },
    Ready { decoder: FrameDecoder },
}

impl ConnPhase {
    fn decoder_mut(&mut self) -> &mut FrameDecoder {
        match self {
            Self::AwaitingHello { decoder, .. } | Self::AwaitingAuth { decoder, .. } | Self::Ready { decoder } => {
                decoder
            }
        }
    }
}

#[derive(Debug)]
struct AttachedStream {
    attachment_id: StreamAttachmentId,
    session: SessionId,
    flags: SubFlags,
    frame_pacer: Option<FramePacer>,
    raw_output: Option<RawOutputFlow>,
}

#[derive(Debug)]
struct ConnState {
    phase: ConnPhase,
    is_inspect: bool,
    fin_received: bool,
    budget: MemoryBudget,
    outbound: OutboundQueue,
    streams: BTreeMap<NonZeroStreamId, AttachedStream>,
    /// Stream IDs whose exit/close messages are still queued in `outbound` and not yet popped.
    closing_streams: BTreeSet<NonZeroStreamId>,
}

#[derive(Debug, Clone, Copy)]
struct SessionRecord {
    lifetime: SessionLifetime,
    rewound: bool,
    pty_paused: bool,
}

/// Deterministic, sans-IO broker core coordinating connections, auth, leases, timers, and flow control.
#[derive(Debug)]
pub struct BrokerCore {
    auth: AuthConfig,
    word_delimiters: String,
    max_queue_bytes: usize,
    next_attachment_id: u64,
    timers: TimerWheel<BrokerTimer>,
    leases: LeaseTable,
    conns: BTreeMap<ConnId, ConnState>,
    sessions: BTreeMap<SessionId, SessionRecord>,
    shard_telemetry: ShardTelemetry,
    total_throttled_us: u64,
}

impl BrokerCore {
    /// Creates a new `BrokerCore` with the default `16 MiB` per-connection total memory budget.
    pub fn new(auth: AuthConfig) -> Self {
        Self::with_queue_cap(auth, MAX_CONTROL_QUEUE_BYTES)
    }

    /// Creates a new `BrokerCore` with a custom per-connection total memory budget (useful in tests/sim).
    pub fn with_queue_cap(auth: AuthConfig, max_queue_bytes: usize) -> Self {
        Self {
            auth,
            word_delimiters: String::from(DEFAULT_WORD_DELIMITERS),
            max_queue_bytes,
            next_attachment_id: 1,
            timers: TimerWheel::new(),
            leases: LeaseTable::new(),
            conns: BTreeMap::new(),
            sessions: BTreeMap::new(),
            shard_telemetry: ShardTelemetry::default(),
            total_throttled_us: 0,
        }
    }

    fn alloc_attachment_id(&mut self) -> StreamAttachmentId {
        let id = self.next_attachment_id;
        self.next_attachment_id = self.next_attachment_id.checked_add(1).expect("attachment id space exhausted");
        StreamAttachmentId::new(id).expect("non-zero attachment id")
    }

    /// Earliest pending timer deadline across all connections and grace leases.
    pub fn next_deadline(&self) -> Option<TimestampMs> {
        self.timers.next_deadline()
    }

    /// Number of currently open connections.
    pub fn connection_count(&self) -> usize {
        self.conns.len()
    }

    /// Returns total bytes currently charged against `conn`'s shared [`MemoryBudget`]
    /// (`OutboundQueue` + all `RawOutputFlow::pending_utf8` buffers + reactor-parked stalled frames).
    pub fn connection_budget_used(&self, conn: ConnId) -> Option<usize> {
        self.conns.get(&conn).map(|c| c.budget.used_bytes())
    }

    /// Returns the hard per-connection [`MemoryBudget`] cap in bytes.
    pub fn max_queue_bytes(&self) -> usize {
        self.max_queue_bytes
    }

    /// Returns remaining byte headroom in `conn`'s [`MemoryBudget`], or `0` if closed.
    pub fn connection_budget_remaining(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |c| c.budget.remaining_bytes())
    }

    /// Charges `bytes` for a reactor-parked stalled view frame against `conn`'s [`MemoryBudget`].
    /// Returns `false` if `conn` is closed or reserving `bytes` would exceed the budget cap.
    pub fn reserve_stalled_frame_bytes(&mut self, conn: ConnId, bytes: usize) -> bool {
        self.conns.get_mut(&conn).is_some_and(|c| c.budget.reserve(bytes).is_ok())
    }

    /// Releases `bytes` previously reserved for a reactor-parked stalled view frame.
    pub fn release_stalled_frame_bytes(&mut self, conn: ConnId, bytes: usize) {
        if let Some(c) = self.conns.get_mut(&conn) {
            c.budget.release(bytes);
        }
    }

    /// Returns `true` iff `conn` has completed `Hello` (and mutual HMAC `AuthProof` when required).
    pub fn is_connection_ready(&self, conn: ConnId) -> bool {
        matches!(self.conns.get(&conn).map(|c| &c.phase), Some(ConnPhase::Ready { .. }))
    }

    /// Returns `true` iff `conn` is authenticated and is a normal lifecycle client (not `apack-inspect`).
    pub fn is_lifecycle_client_ready(&self, conn: ConnId) -> bool {
        self.conns.get(&conn).is_some_and(|c| matches!(c.phase, ConnPhase::Ready { .. }) && !c.is_inspect)
    }

    /// Returns `true` when at least one live or in-grace broker-wide lease is active.
    pub fn has_active_broker_lease(&self) -> bool {
        self.leases.has_active_broker_lease()
    }

    /// Returns `true` when `session` is currently pinned by at least one `LeaseKind::Session`.
    pub fn is_session_pinned(&self, session: SessionId) -> bool {
        self.leases.is_session_pinned(session)
    }

    fn has_ready_connections(&self) -> bool {
        self.conns.values().any(|c| matches!(c.phase, ConnPhase::Ready { .. }) && !c.is_inspect)
    }

    fn has_attached_streams(&self, session: SessionId) -> bool {
        self.conns
            .values()
            .any(|c| matches!(c.phase, ConnPhase::Ready { .. }) && c.streams.values().any(|st| st.session == session))
    }

    /// Registers a live or rewound session with an explicit [`SessionLifetime`] in the broker's
    /// session table, immediately scheduling teardown if its owner or last unleased client already
    /// disconnected while creation was in flight.
    pub fn register_session(
        &mut self,
        session: SessionId,
        lifetime: SessionLifetime,
        rewound: bool,
        actions: &mut Vec<BrokerAction>,
    ) {
        self.sessions.insert(session, SessionRecord { lifetime, rewound, pty_paused: false });
        let orphaned = match lifetime {
            SessionLifetime::Kept => false,
            SessionLifetime::ConnBound(owner) => {
                !self.conns.contains_key(&owner) && !self.leases.is_session_pinned(session)
            }
            SessionLifetime::LeaseBound => {
                !self.has_ready_connections()
                    && !self.leases.has_active_broker_lease()
                    && !self.leases.is_session_pinned(session)
            }
        };
        if orphaned {
            actions.push(BrokerAction::TeardownSession(session));
        }
    }

    /// Updates a non-rewound session's kept state (`Kept` vs `LeaseBound`).
    pub fn set_session_kept(&mut self, session: SessionId, kept: bool) {
        if let Some(rec) = self.sessions.get_mut(&session)
            && !rec.rewound
        {
            rec.lifetime = if kept { SessionLifetime::Kept } else { SessionLifetime::LeaseBound };
        }
    }

    /// Returns `true` if `session` is registered with [`SessionLifetime::Kept`].
    pub fn is_session_kept(&self, session: SessionId) -> bool {
        self.sessions.get(&session).is_some_and(|rec| rec.lifetime == SessionLifetime::Kept)
    }

    /// Returns the explicit [`SessionLifetime`] of `session`, if registered.
    pub fn session_lifetime(&self, session: SessionId) -> Option<SessionLifetime> {
        self.sessions.get(&session).map(|rec| rec.lifetime)
    }

    fn emit_unpinned_actions(&self, unpinned: Vec<SessionId>, actions: &mut Vec<BrokerAction>) {
        for sid in unpinned {
            match self.sessions.get(&sid).map(|r| r.lifetime) {
                Some(SessionLifetime::ConnBound(owner)) if !self.conns.contains_key(&owner) => {
                    actions.push(BrokerAction::TeardownSession(sid));
                }
                Some(SessionLifetime::LeaseBound) if !self.has_attached_streams(sid) => {
                    actions.push(BrokerAction::SessionUnpinned(sid));
                }
                _ => {}
            }
        }
    }

    /// Removes a session, cleans up its session leases, flushes any pending `BACKPRESSURE_PTY`
    /// raw output chunks, drops unsent frames, and enqueues `BrokerMsg::StreamClosed` behind any
    /// queued `Exited` / final `Output` on attached streams that have not already been terminated
    /// by `BrokerMsg::Exited` without `SUB_RAW_OUTPUT`.
    pub fn unregister_session(
        &mut self,
        session: SessionId,
        reason: &str,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        self.sessions.remove(&session);
        self.leases.remove_session_leases(session);
        let mut to_close: Vec<(ConnId, NonZeroStreamId)> = Vec::new();
        for (&conn_id, conn) in &self.conns {
            for (&stream_id, st) in &conn.streams {
                if st.session == session {
                    to_close.push((conn_id, stream_id));
                }
            }
        }
        for (conn_id, stream_id) in to_close {
            let flushed_raw = if let Some(conn) = self.conns.get_mut(&conn_id)
                && let Some(mut st) = conn.streams.remove(&stream_id)
            {
                let chunks =
                    st.raw_output.as_mut().map(|flow| flow.flush_on_exit(&mut conn.budget)).unwrap_or_default();
                // Preserve already-queued `Exited` events and final `Output` chunks; drop only unsent frames.
                conn.outbound.remove_stream_frames(stream_id.as_stream_id(), &mut conn.budget);
                conn.closing_streams.insert(stream_id);
                chunks
            } else {
                Vec::new()
            };
            for text in flushed_raw {
                self.enqueue_message(
                    conn_id,
                    stream_id.as_stream_id(),
                    RequestId::PUSH,
                    OutboundPriority::RawOutput,
                    BrokerMsg::Output { text },
                    now,
                    actions,
                );
            }
            let msg = BrokerMsg::StreamClosed { reason: String::from(reason) };
            // Enqueued at `RawOutput` priority so it drains strictly after `Exited` (Event) and
            // FIFO behind any already-queued or flushed final `Output` chunks for this stream.
            self.enqueue_message(
                conn_id,
                stream_id.as_stream_id(),
                RequestId::PUSH,
                OutboundPriority::RawOutput,
                msg,
                now,
                actions,
            );
        }
    }

    /// Updates shard-reported memory and byte counters exposed by `Metrics`.
    pub fn update_shard_telemetry(&mut self, telemetry: ShardTelemetry) {
        self.shard_telemetry = telemetry;
    }

    /// Computes a live [`MetricsSnapshot`] for `ClientMsg::Metrics` or diagnostics.
    pub fn metrics_snapshot(&self) -> MetricsSnapshot {
        let mut live_sessions = 0u32;
        let mut rewound_sessions = 0u32;
        for rec in self.sessions.values() {
            if rec.rewound {
                rewound_sessions = rewound_sessions.saturating_add(1);
            } else {
                live_sessions = live_sessions.saturating_add(1);
            }
        }
        MetricsSnapshot {
            live_sessions,
            rewound_sessions,
            connections: u32::try_from(self.conns.len()).unwrap_or(u32::MAX),
            active_timers: u32::try_from(self.timers.len()).unwrap_or(u32::MAX),
            scrollback_bytes: self.shard_telemetry.scrollback_bytes,
            image_store_bytes: self.shard_telemetry.image_store_bytes,
            total_pty_bytes: self.shard_telemetry.total_pty_bytes,
            total_throttled_us: self.total_throttled_us,
        }
    }

    /// Registers a newly accepted transport connection and schedules its handshake deadline.
    ///
    /// Returns `Err(DuplicateConnId)` if `conn` is already open.
    pub fn open_connection(
        &mut self,
        conn: ConnId,
        server_nonce: [u8; 32],
        now: TimestampMs,
    ) -> Result<(), DuplicateConnId> {
        if self.conns.contains_key(&conn) {
            return Err(DuplicateConnId);
        }
        let deadline = now.saturating_add(AUTH_HANDSHAKE_TIMEOUT_MS);
        let timeout = self.timers.schedule(deadline, BrokerTimer::HandshakeTimeout(conn));
        self.conns.insert(
            conn,
            ConnState {
                phase: ConnPhase::AwaitingHello {
                    decoder: FrameDecoder::with_max_payload(MAX_HELLO_FRAME_PAYLOAD),
                    server_nonce,
                    timeout,
                },
                is_inspect: false,
                fin_received: false,
                budget: MemoryBudget::new(self.max_queue_bytes),
                outbound: OutboundQueue::new(),
                streams: BTreeMap::new(),
                closing_streams: BTreeSet::new(),
            },
        );
        Ok(())
    }

    /// Feeds raw inbound bytes from `conn` into its phase-typed [`FrameDecoder`] and processes complete frames.
    pub fn feed_bytes(&mut self, conn: ConnId, mut bytes: &[u8], now: TimestampMs, actions: &mut Vec<BrokerAction>) {
        while !bytes.is_empty() {
            let step = {
                let Some(state) = self.conns.get_mut(&conn) else {
                    return;
                };
                if state.fin_received {
                    return;
                }
                state.phase.decoder_mut().step(bytes)
            };
            match step {
                Ok((consumed, None)) => {
                    bytes = &bytes[consumed..];
                }
                Ok((_, Some(DecodedFrame::Fin))) => {
                    if let Some(state) = self.conns.get_mut(&conn) {
                        state.fin_received = true;
                    }
                    actions.push(BrokerAction::ClientFin(conn));
                    return;
                }
                Ok((consumed, Some(DecodedFrame::Frame { header, payload }))) => {
                    bytes = &bytes[consumed..];
                    self.handle_decoded_frame(conn, header, &payload, now, actions);
                }
                Err(_) => {
                    self.fail_and_close(
                        conn,
                        StreamId::CONTROL,
                        RequestId::PUSH,
                        ErrorCode::Malformed,
                        "malformed wire frame",
                        CloseReason::ProtocolError,
                        now,
                        actions,
                    );
                    return;
                }
            }
        }
    }

    /// Handles connection teardown (transport EOF, FIN, protocol error, auth failure, or slow reader).
    pub fn close_connection(
        &mut self,
        conn: ConnId,
        reason: CloseReason,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        self.close_connection_with_error(conn, reason, None, now, actions);
    }

    fn close_connection_with_error(
        &mut self,
        conn: ConnId,
        reason: CloseReason,
        final_error: Option<(StreamId, RequestId, BrokerMsg)>,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let Some(mut state) = self.conns.remove(&conn) else {
            return;
        };
        match state.phase {
            ConnPhase::AwaitingHello { timeout, .. } | ConnPhase::AwaitingAuth { timeout, .. } => {
                self.timers.cancel(timeout);
            }
            ConnPhase::Ready { .. } => {}
        }

        let mut drained = Vec::new();
        if reason == CloseReason::ClientFin {
            // Preserve already-queued ordered replies (e.g. pipelined RPC + FIN_FRAME in one read),
            // dropping conflatable view frames since the peer is closing.
            while let Some(msg) = state.outbound.pop_next(&mut state.budget) {
                if !matches!(msg.msg, BrokerMsg::Frame(_)) {
                    drained.push(msg);
                }
            }
        }

        let mut affected_sessions = BTreeSet::new();
        for (stream_id, st) in state.streams {
            actions.push(BrokerAction::StreamDetached {
                conn,
                stream_id,
                attachment_id: st.attachment_id,
                session: st.session,
            });
            affected_sessions.insert(st.session);
        }
        for session in affected_sessions {
            self.reconcile_pty_pause(session, actions);
        }

        let changes = self.leases.on_disconnect(conn, now, |lease_id, deadline| {
            self.timers.schedule(deadline, BrokerTimer::LeaseGrace(lease_id))
        });
        let conn_bound_victims: Vec<SessionId> = self
            .sessions
            .iter()
            .filter_map(|(&sid, rec)| {
                (rec.lifetime == SessionLifetime::ConnBound(conn) && !self.leases.is_session_pinned(sid)).then_some(sid)
            })
            .collect();
        for &sid in &conn_bound_victims {
            actions.push(BrokerAction::TeardownSession(sid));
        }
        let remaining_unpinned: Vec<SessionId> =
            changes.unpinned_sessions.into_iter().filter(|sid| !conn_bound_victims.contains(sid)).collect();
        self.emit_unpinned_actions(remaining_unpinned, actions);
        if changes.teardown_non_kept || (!self.has_ready_connections() && !self.leases.has_active_broker_lease()) {
            actions.push(BrokerAction::TeardownNonKeptSessions);
        }
        actions.push(BrokerAction::CloseConn { conn, reason, final_error, drained });
    }

    /// Fires all timers due at or before `now` in deterministic `(deadline, id)` order.
    pub fn poll_timers(&mut self, now: TimestampMs, actions: &mut Vec<BrokerAction>) {
        let mut expired = Vec::new();
        self.timers.poll_expired(now, &mut expired);
        for (timer_id, timer) in expired {
            match timer {
                BrokerTimer::HandshakeTimeout(conn) => {
                    let matches_timer = self.conns.get(&conn).is_some_and(|c| match c.phase {
                        ConnPhase::AwaitingHello { timeout, .. } | ConnPhase::AwaitingAuth { timeout, .. } => {
                            timeout == timer_id
                        }
                        ConnPhase::Ready { .. } => false,
                    });
                    if matches_timer {
                        self.fail_and_close(
                            conn,
                            StreamId::CONTROL,
                            RequestId::PUSH,
                            ErrorCode::Handshake,
                            "handshake timeout",
                            CloseReason::HandshakeTimeout,
                            now,
                            actions,
                        );
                    }
                }
                BrokerTimer::LeaseGrace(lease_id) => {
                    let changes = self.leases.on_grace_expired(lease_id, timer_id);
                    if changes.teardown_non_kept {
                        actions.push(BrokerAction::TeardownNonKeptSessions);
                    }
                }
            }
        }
    }

    /// Pops the highest-priority pending outbound message for `conn`.
    pub fn pop_outbound(&mut self, conn: ConnId) -> Option<QueuedMessage> {
        let state = self.conns.get_mut(&conn)?;
        let popped = state.outbound.pop_next(&mut state.budget)?;
        if let Some(nz) = NonZeroStreamId::new(popped.stream_id.raw())
            && state.closing_streams.contains(&nz)
            && !state.outbound.has_stream_messages(popped.stream_id)
        {
            state.closing_streams.remove(&nz);
        }
        Some(popped)
    }

    /// Enqueues a reply or event on `conn`, closing the connection as a slow reader on budget overflow.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_message(
        &mut self,
        conn: ConnId,
        stream_id: StreamId,
        req_id: RequestId,
        priority: OutboundPriority,
        mut msg: BrokerMsg,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let mut encoded_bytes = estimate_encoded_bytes(&msg);
        if encoded_bytes > MAX_WIRE_FRAME_BYTES {
            if req_id == RequestId::PUSH {
                return;
            }
            msg = BrokerMsg::Error {
                code: ErrorCode::ResourceLimit,
                message: String::from("response exceeds maximum frame payload"),
            };
            encoded_bytes = estimate_encoded_bytes(&msg);
        }
        // For streams without `SUB_RAW_OUTPUT`, `Exited` is the terminal stream message (no trailing
        // `Output` or redundant `StreamClosed` follows): enqueue at P4 (`RawOutput`) so any already-queued
        // P1 Frame / P2 Event / P3 Bulk messages strictly drain first, and immediately move the stream into
        // `closing_streams` so `stream_id` becomes reusable the instant `Exited` is popped onto the wire.
        let closes_on_exit = matches!(msg, BrokerMsg::Exited { .. })
            && NonZeroStreamId::new(stream_id.raw()).is_some_and(|nz| {
                self.conns
                    .get(&conn)
                    .and_then(|s| s.streams.get(&nz))
                    .is_some_and(|st| !st.flags.contains(SubFlags::RAW_OUTPUT))
            });
        let effective_priority = if closes_on_exit { OutboundPriority::RawOutput } else { priority };
        let overflowed = match self.conns.get_mut(&conn) {
            Some(state) => {
                let res = state.outbound.push_ordered(
                    effective_priority,
                    stream_id,
                    req_id,
                    msg,
                    encoded_bytes,
                    &mut state.budget,
                );
                if res.is_ok()
                    && closes_on_exit
                    && let Some(nz) = NonZeroStreamId::new(stream_id.raw())
                {
                    state.streams.remove(&nz);
                    state.closing_streams.insert(nz);
                }
                res.is_err()
            }
            None => return,
        };
        if overflowed {
            self.close_connection(conn, CloseReason::SlowReader, now, actions);
        } else {
            actions.push(BrokerAction::OutboundReady(conn));
        }
    }

    /// Returns the [`SessionId`] bound to `(conn, stream_id)`, if that stream is currently attached.
    pub fn stream_session(&self, conn: ConnId, stream_id: NonZeroStreamId) -> Option<SessionId> {
        self.conns.get(&conn)?.streams.get(&stream_id).map(|s| s.session)
    }

    /// Returns the active [`StreamAttachmentId`] bound to `(conn, stream_id)`, if attached.
    pub fn stream_attachment(&self, conn: ConnId, stream_id: NonZeroStreamId) -> Option<StreamAttachmentId> {
        self.conns.get(&conn)?.streams.get(&stream_id).map(|s| s.attachment_id)
    }

    /// Returns `true` iff `(conn, stream_id)` is currently attached with the exact `attachment_id`.
    pub fn is_attachment_active(
        &self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
    ) -> bool {
        self.stream_attachment(conn, stream_id) == Some(attachment_id)
    }

    /// Rolls back a newly inserted stream attachment if the reactor could not dispatch `Attach` to a shard.
    pub fn abort_attach(&mut self, conn: ConnId, stream_id: NonZeroStreamId, attachment_id: StreamAttachmentId) {
        if let Some(state) = self.conns.get_mut(&conn)
            && state.streams.get(&stream_id).is_some_and(|st| st.attachment_id == attachment_id)
            && let Some(mut st) = state.streams.remove(&stream_id)
        {
            if let Some(flow) = st.raw_output.as_mut() {
                flow.clear_pending(&mut state.budget);
            }
            state.outbound.remove_stream(stream_id.as_stream_id(), &mut state.budget);
        }
    }

    /// Returns `true` iff `(conn, stream_id)` is bound to `attachment_id`, has no unsent Priority 1
    /// frame waiting, and its `FramePacer` currently permits at least one more frame.
    pub fn can_publish_frame_now(
        &self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
    ) -> bool {
        let Some(state) = self.conns.get(&conn) else {
            return false;
        };
        if state.outbound.has_pending_frame(stream_id.as_stream_id()) {
            return false;
        }
        let Some(stream) = state.streams.get(&stream_id) else {
            return false;
        };
        if stream.attachment_id != attachment_id {
            return false;
        }
        stream.frame_pacer.as_ref().is_some_and(FramePacer::can_send_any)
    }

    /// Attempts to publish a `FrameUpdate` on `(conn, stream_id)` for `attachment_id` respecting
    /// BDP `FramePacer` and the shared per-connection [`MemoryBudget`], returning
    /// `Err((frame, reason, encoded_bytes))` without cloning if deferred or stale.
    ///
    /// Oversized frames exceeding [`MAX_WIRE_FRAME_BYTES`] are degraded (marked `full_sync = true`
    /// and trimmed to fit) instead of stalling indefinitely.
    pub fn try_publish_frame(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
        mut frame: Box<FrameUpdate>,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) -> Result<(), (Box<FrameUpdate>, FrameStallReason, usize)> {
        let Some(state) = self.conns.get_mut(&conn) else {
            return Err((frame, FrameStallReason::Stale, 0));
        };
        let Some(stream) = state.streams.get_mut(&stream_id) else {
            return Err((frame, FrameStallReason::Stale, 0));
        };
        if stream.attachment_id != attachment_id {
            return Err((frame, FrameStallReason::Stale, 0));
        }
        let Some(pacer) = stream.frame_pacer.as_mut() else {
            return Err((frame, FrameStallReason::Stale, 0));
        };
        if state.outbound.has_pending_frame(stream_id.as_stream_id()) {
            return Err((frame, FrameStallReason::OutboundOrBudget, 0));
        }
        // Fast pre-check before measuring/encoding payload bytes.
        if !pacer.can_send_any() {
            return Err((frame, FrameStallReason::Pacer, 0));
        }
        // Assign the real monotonic sequence before measuring encoded bytes so varint length is exact.
        frame.seq = pacer.peek_next_seq();
        let encoded_bytes = degrade_frame_to_wire_cap(&mut frame);
        let frame_u32 = u32::try_from(encoded_bytes).unwrap_or(u32::MAX);
        if !pacer.can_send(frame_u32) {
            return Err((frame, FrameStallReason::Pacer, encoded_bytes));
        }
        if !state.outbound.can_fit_frame(stream_id.as_stream_id(), encoded_bytes, &state.budget) {
            return Err((frame, FrameStallReason::OutboundOrBudget, encoded_bytes));
        }
        let _ =
            state.outbound.push_frame(stream_id.as_stream_id(), attachment_id, frame, encoded_bytes, &mut state.budget);
        let _ = pacer.on_send(frame_u32, now);
        actions.push(BrokerAction::OutboundReady(conn));
        Ok(())
    }

    /// Publishes a non-droppable final exit `FrameUpdate` on the ordered `Event` (P2) lane so it
    /// is never discarded by pacer backpressure, `SessionEnded` P1 cleanup, or `Exited` ordering,
    /// draining strictly ahead of `Exited` and `StreamClosed`.
    pub fn publish_final_frame(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
        mut frame: Box<FrameUpdate>,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let overflowed = {
            let Some(state) = self.conns.get_mut(&conn) else {
                return;
            };
            let Some(stream) = state.streams.get_mut(&stream_id) else {
                return;
            };
            if stream.attachment_id != attachment_id {
                return;
            }
            let Some(pacer) = stream.frame_pacer.as_mut() else {
                return;
            };
            // Drop any stale conflatable P1 frame for this stream before enqueueing the ordered final snapshot.
            state.outbound.remove_stream_frames(stream_id.as_stream_id(), &mut state.budget);
            frame.full_sync = true;
            frame.seq = pacer.peek_next_seq();
            let encoded_bytes = degrade_frame_to_wire_cap(&mut frame);
            let frame_u32 = u32::try_from(encoded_bytes).unwrap_or(u32::MAX);
            let res = state.outbound.push_ordered(
                OutboundPriority::Event,
                stream_id.as_stream_id(),
                RequestId::PUSH,
                BrokerMsg::Frame(frame),
                encoded_bytes,
                &mut state.budget,
            );
            if res.is_ok() {
                let _ = pacer.on_send(frame_u32, now);
            }
            res.is_err()
        };
        if overflowed {
            self.close_connection(conn, CloseReason::SlowReader, now, actions);
        } else {
            actions.push(BrokerAction::OutboundReady(conn));
        }
    }

    /// Attempts to publish a `FrameUpdate` on `(conn, stream_id)` for `attachment_id` respecting
    /// BDP `FramePacer` and per-connection [`MemoryBudget`] bounds.
    pub fn publish_frame(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
        frame: Box<FrameUpdate>,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) -> bool {
        self.try_publish_frame(conn, stream_id, attachment_id, frame, now, actions).is_ok()
    }

    /// Returns the measured RTT EWMA in milliseconds for a `SUB_VIEW` stream, if available.
    pub fn stream_rtt_ms(&self, conn: ConnId, stream_id: NonZeroStreamId) -> Option<u32> {
        self.conns.get(&conn)?.streams.get(&stream_id)?.frame_pacer.as_ref()?.ewma_rtt_ms()
    }

    /// Distributes raw PTY output bytes to every `SUB_RAW_OUTPUT` stream attached to `session`.
    pub fn publish_raw_output(
        &mut self,
        session: SessionId,
        raw: &[u8],
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let targets: Vec<(ConnId, NonZeroStreamId)> = self
            .conns
            .iter()
            .flat_map(|(&cid, c)| {
                c.streams.iter().filter_map(move |(&sid, st)| {
                    if st.session == session && st.raw_output.is_some() { Some((cid, sid)) } else { None }
                })
            })
            .collect();

        for (cid, sid) in targets {
            let delta = {
                let Some(conn_state) = self.conns.get_mut(&cid) else {
                    continue;
                };
                let Some(st) = conn_state.streams.get_mut(&sid) else {
                    continue;
                };
                let Some(flow) = st.raw_output.as_mut() else {
                    continue;
                };
                flow.ingest(raw, now, &mut conn_state.budget)
            };
            for text in delta.emit_chunks {
                self.enqueue_message(
                    cid,
                    sid.as_stream_id(),
                    RequestId::PUSH,
                    OutboundPriority::RawOutput,
                    BrokerMsg::Output { text },
                    now,
                    actions,
                );
            }
            if delta.dropped_bytes > 0 {
                self.enqueue_message(
                    cid,
                    sid.as_stream_id(),
                    RequestId::PUSH,
                    OutboundPriority::RawOutput,
                    BrokerMsg::OutputDropped { bytes: delta.dropped_bytes },
                    now,
                    actions,
                );
            }
        }
        self.reconcile_pty_pause(session, actions);
    }

    fn handle_decoded_frame(
        &mut self,
        conn: ConnId,
        header: FrameHeader,
        payload: &[u8],
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let stream_id = header.stream_id;
        let req_id = header.req_id;
        let phase_info = match self.conns.get(&conn).map(|c| &c.phase) {
            Some(ConnPhase::AwaitingHello { server_nonce, timeout, .. }) => Ok((*server_nonce, *timeout, None)),
            Some(ConnPhase::AwaitingAuth { client_nonce, server_nonce, timeout, .. }) => {
                Ok((*server_nonce, *timeout, Some(*client_nonce)))
            }
            Some(ConnPhase::Ready { .. }) => Err(()),
            None => return,
        };

        match phase_info {
            Ok((server_nonce, timeout, None)) => {
                if !stream_id.is_control() {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Handshake,
                        "Hello must be sent on control stream",
                        CloseReason::HandshakeFailed,
                        now,
                        actions,
                    );
                    return;
                }
                if header.msg_type != msg_type::HELLO {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Handshake,
                        "expected Hello as first message",
                        CloseReason::HandshakeFailed,
                        now,
                        actions,
                    );
                    return;
                }
                let Ok(ClientMsg::Hello { protocol_version, client_nonce, client_name }) =
                    ClientMsg::decode(msg_type::HELLO, payload)
                else {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Malformed,
                        "malformed client message",
                        CloseReason::ProtocolError,
                        now,
                        actions,
                    );
                    return;
                };
                if protocol_version != PROTOCOL_VERSION {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Handshake,
                        "unsupported protocol version",
                        CloseReason::HandshakeFailed,
                        now,
                        actions,
                    );
                    return;
                }
                let auth_required = self.auth.is_required();
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.is_inspect = client_name == INSPECT_CLIENT_NAME;
                    if auth_required {
                        state.phase = ConnPhase::AwaitingAuth {
                            decoder: FrameDecoder::with_max_payload(MAX_AUTH_FRAME_PAYLOAD),
                            client_nonce,
                            server_nonce,
                            timeout,
                        };
                    } else {
                        self.timers.cancel(timeout);
                        state.phase = ConnPhase::Ready { decoder: FrameDecoder::new() };
                    }
                }
                let reply = BrokerMsg::HelloOk {
                    protocol_version: PROTOCOL_VERSION,
                    auth_required,
                    server_nonce,
                    word_delimiters: self.word_delimiters.clone(),
                    capabilities: BROKER_CAPABILITIES,
                };
                self.enqueue_message(conn, StreamId::CONTROL, req_id, OutboundPriority::Control, reply, now, actions);
            }
            Ok((server_nonce, timeout, Some(client_nonce))) => {
                if !stream_id.is_control() {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Unauthorized,
                        "authentication required before stream messages",
                        CloseReason::Unauthorized,
                        now,
                        actions,
                    );
                    return;
                }
                if header.msg_type != msg_type::AUTH_PROOF {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Unauthorized,
                        "expected AuthProof",
                        CloseReason::Unauthorized,
                        now,
                        actions,
                    );
                    return;
                }
                let Ok(ClientMsg::AuthProof { hmac }) = ClientMsg::decode(msg_type::AUTH_PROOF, payload) else {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Malformed,
                        "malformed client message",
                        CloseReason::ProtocolError,
                        now,
                        actions,
                    );
                    return;
                };
                let AuthConfig::Required(secret) = &self.auth else {
                    return;
                };
                if !verify_client_proof(secret, &client_nonce, &server_nonce, &hmac) {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Unauthorized,
                        "invalid authentication proof",
                        CloseReason::Unauthorized,
                        now,
                        actions,
                    );
                    return;
                }
                let server_hmac = compute_server_proof(secret, &server_nonce, &client_nonce);
                self.timers.cancel(timeout);
                if let Some(state) = self.conns.get_mut(&conn) {
                    state.phase = ConnPhase::Ready { decoder: FrameDecoder::new() };
                }
                self.enqueue_message(
                    conn,
                    StreamId::CONTROL,
                    req_id,
                    OutboundPriority::Control,
                    BrokerMsg::AuthOk { server_hmac },
                    now,
                    actions,
                );
            }
            Err(()) => match ClientMsg::decode(header.msg_type, payload) {
                Ok(msg) => {
                    if stream_id.is_control() {
                        self.handle_control_msg(conn, req_id, msg, now, actions);
                    } else {
                        let nz = NonZeroStreamId::new(stream_id.raw()).expect("non-control stream_id is non-zero");
                        self.handle_stream_msg(conn, nz, req_id, msg, now, actions);
                    }
                }
                Err(_) => {
                    self.fail_and_close(
                        conn,
                        stream_id,
                        req_id,
                        ErrorCode::Malformed,
                        "malformed client message",
                        CloseReason::ProtocolError,
                        now,
                        actions,
                    );
                }
            },
        }
    }

    fn handle_control_msg(
        &mut self,
        conn: ConnId,
        req_id: RequestId,
        msg: ClientMsg,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        match msg {
            ClientMsg::Hello { .. } | ClientMsg::AuthProof { .. } => {
                self.fail_and_close(
                    conn,
                    StreamId::CONTROL,
                    req_id,
                    ErrorCode::Handshake,
                    "duplicate handshake message",
                    CloseReason::HandshakeFailed,
                    now,
                    actions,
                );
            }
            ClientMsg::AcquireLease { kind } => {
                if let LeaseKind::Session(sid) = kind
                    && !self.sessions.contains_key(&sid)
                {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                }
                match self.leases.acquire(conn, kind) {
                    Ok(outcome) => {
                        if let Some(timer) = outcome.cancelled_timer {
                            self.timers.cancel(timer);
                        }
                        self.enqueue_message(
                            conn,
                            StreamId::CONTROL,
                            req_id,
                            OutboundPriority::Control,
                            BrokerMsg::LeaseAck { lease_id: outcome.lease_id.get(), resumed: outcome.resumed },
                            now,
                            actions,
                        );
                    }
                    Err(LeaseError::TokenInUse) => {
                        self.send_error(
                            conn,
                            StreamId::CONTROL,
                            req_id,
                            ErrorCode::Busy,
                            "resume token already active on another connection",
                            now,
                            actions,
                        );
                    }
                    Err(LeaseError::LimitExceeded) => {
                        self.send_error(
                            conn,
                            StreamId::CONTROL,
                            req_id,
                            ErrorCode::ResourceLimit,
                            "connection lease limit reached",
                            now,
                            actions,
                        );
                    }
                    Err(LeaseError::NotOwned) => {}
                }
            }
            ClientMsg::ReleaseLease { lease_id } => {
                let Some(lid) = LeaseId::new(lease_id) else {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Malformed,
                        "invalid lease id",
                        now,
                        actions,
                    );
                    return;
                };
                match self.leases.release(conn, lid) {
                    Ok(changes) => {
                        self.emit_unpinned_actions(changes.unpinned_sessions, actions);
                        if changes.teardown_non_kept {
                            actions.push(BrokerAction::TeardownNonKeptSessions);
                        }
                        self.enqueue_message(
                            conn,
                            StreamId::CONTROL,
                            req_id,
                            OutboundPriority::Control,
                            BrokerMsg::Ok,
                            now,
                            actions,
                        );
                    }
                    Err(_) => {
                        self.send_error(
                            conn,
                            StreamId::CONTROL,
                            req_id,
                            ErrorCode::Malformed,
                            "lease not owned by connection",
                            now,
                            actions,
                        );
                    }
                }
            }
            ClientMsg::Spawn(spec) => actions.push(BrokerAction::SpawnSession { conn, req_id, spec }),
            ClientMsg::Kill { session } => {
                if !self.sessions.contains_key(&session) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                }
                actions.push(BrokerAction::KillSession { conn, req_id, session });
            }
            ClientMsg::Signal { session, signal, scope } => {
                if !self.sessions.contains_key(&session) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                }
                actions.push(BrokerAction::SignalSession { conn, req_id, session, signal, scope });
            }
            ClientMsg::Owns { session, pid } => {
                if !self.sessions.contains_key(&session) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                }
                actions.push(BrokerAction::QueryOwns { conn, req_id, session, pid });
            }
            ClientMsg::ListSessions => actions.push(BrokerAction::ListSessions { conn, req_id }),
            ClientMsg::Rewind { record_dir, at_ms } => {
                actions.push(BrokerAction::RewindRecording { conn, req_id, record_dir, at_ms });
            }
            ClientMsg::Seek { session, at_ms } => {
                let Some(rec) = self.sessions.get(&session) else {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                };
                if !rec.rewound {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Malformed,
                        "seek is only valid on rewound sessions",
                        now,
                        actions,
                    );
                    return;
                }
                actions.push(BrokerAction::SeekRecording { conn, req_id, session, at_ms });
            }
            ClientMsg::Metrics => {
                let snapshot = self.metrics_snapshot();
                self.enqueue_message(
                    conn,
                    StreamId::CONTROL,
                    req_id,
                    OutboundPriority::Control,
                    BrokerMsg::MetricsReply(snapshot),
                    now,
                    actions,
                );
            }
            ClientMsg::Attach {
                session,
                stream_id,
                flags,
                initial_viewport,
                initial_output_credit,
                initial_frame_credit,
            } => {
                if !self.sessions.contains_key(&session) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                }
                let Some(state) = self.conns.get_mut(&conn) else {
                    return;
                };
                if state.streams.contains_key(&stream_id) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Malformed,
                        "stream id already attached",
                        now,
                        actions,
                    );
                    return;
                }
                if state.closing_streams.contains(&stream_id) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Busy,
                        "stream id still draining close events",
                        now,
                        actions,
                    );
                    return;
                }
                if state.streams.len() >= MAX_STREAMS_PER_CONN {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::ResourceLimit,
                        "maximum attached streams reached",
                        now,
                        actions,
                    );
                    return;
                }
                let attachment_id = self.alloc_attachment_id();
                let frame_pacer =
                    if flags.contains(SubFlags::VIEW) { Some(FramePacer::new(initial_frame_credit)) } else { None };
                let raw_output = if flags.contains(SubFlags::RAW_OUTPUT) {
                    Some(RawOutputFlow::new(initial_output_credit, flags.contains(SubFlags::BACKPRESSURE_PTY)))
                } else {
                    None
                };
                if let Some(state) = self.conns.get_mut(&conn) {
                    state
                        .streams
                        .insert(stream_id, AttachedStream { attachment_id, session, flags, frame_pacer, raw_output });
                }
                actions.push(BrokerAction::StreamAttached {
                    conn,
                    req_id,
                    stream_id,
                    attachment_id,
                    session,
                    flags,
                    initial_viewport,
                });
            }
            ClientMsg::Detach { stream_id } => {
                let detached = self.conns.get_mut(&conn).and_then(|c| {
                    let mut st = c.streams.remove(&stream_id)?;
                    if let Some(flow) = st.raw_output.as_mut() {
                        flow.clear_pending(&mut c.budget);
                    }
                    c.outbound.remove_stream(stream_id.as_stream_id(), &mut c.budget);
                    Some((st.session, st.attachment_id))
                });
                let Some((session, attachment_id)) = detached else {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchStream,
                        "stream id not attached",
                        now,
                        actions,
                    );
                    return;
                };
                actions.push(BrokerAction::StreamDetached { conn, stream_id, attachment_id, session });
                self.reconcile_pty_pause(session, actions);
                self.enqueue_message(
                    conn,
                    StreamId::CONTROL,
                    req_id,
                    OutboundPriority::Control,
                    BrokerMsg::Ok,
                    now,
                    actions,
                );
            }
            ClientMsg::Keep { session, kept } => {
                let Some(rec) = self.sessions.get(&session).copied() else {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                };
                if rec.rewound {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Malformed,
                        "cannot keep a read-only rewound session",
                        now,
                        actions,
                    );
                    return;
                }
                self.set_session_kept(session, kept);
                actions.push(BrokerAction::SetSessionKept { conn, req_id, session, kept });
            }
            ClientMsg::Wait { session, condition, timeout_ms } => {
                if !self.sessions.contains_key(&session) {
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                        now,
                        actions,
                    );
                    return;
                }
                actions.push(BrokerAction::WaitSession { conn, req_id, session, condition, timeout_ms });
            }
            _ => {
                self.send_error(
                    conn,
                    StreamId::CONTROL,
                    req_id,
                    ErrorCode::Malformed,
                    "session stream message sent on control stream",
                    now,
                    actions,
                );
            }
        }
    }

    fn handle_stream_msg(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        req_id: RequestId,
        msg: ClientMsg,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let Some((attachment_id, session, flags)) = self
            .conns
            .get(&conn)
            .and_then(|c| c.streams.get(&stream_id))
            .map(|st| (st.attachment_id, st.session, st.flags))
        else {
            self.send_error(
                conn,
                stream_id.as_stream_id(),
                req_id,
                ErrorCode::NoSuchStream,
                "no such attached stream",
                now,
                actions,
            );
            return;
        };

        match msg {
            ClientMsg::FrameAck { seq, credit } => {
                if !flags.contains(SubFlags::VIEW) {
                    self.send_error(
                        conn,
                        stream_id.as_stream_id(),
                        req_id,
                        ErrorCode::Malformed,
                        "FrameAck requires SUB_VIEW",
                        now,
                        actions,
                    );
                    return;
                }
                let ack_result = self
                    .conns
                    .get_mut(&conn)
                    .and_then(|c| c.streams.get_mut(&stream_id))
                    .and_then(|st| st.frame_pacer.as_mut())
                    .map(|p| p.on_ack(seq, credit, now));
                match ack_result {
                    Some(Ok(())) => {
                        actions.push(BrokerAction::FrameCreditUpdated {
                            conn,
                            stream_id,
                            attachment_id,
                            session,
                            credit,
                        });
                    }
                    Some(Err(_)) | None => {
                        self.send_error(
                            conn,
                            stream_id.as_stream_id(),
                            req_id,
                            ErrorCode::Malformed,
                            "invalid FrameAck sequence",
                            now,
                            actions,
                        );
                    }
                }
            }
            ClientMsg::OutputCredit { bytes } => {
                if !flags.contains(SubFlags::RAW_OUTPUT) {
                    self.send_error(
                        conn,
                        stream_id.as_stream_id(),
                        req_id,
                        ErrorCode::Malformed,
                        "OutputCredit requires SUB_RAW_OUTPUT",
                        now,
                        actions,
                    );
                    return;
                }
                let delta = self.conns.get_mut(&conn).and_then(|c| {
                    let st = c.streams.get_mut(&stream_id)?;
                    let flow = st.raw_output.as_mut()?;
                    Some(flow.grant_credit(bytes, now, &mut c.budget))
                });
                if let Some(delta) = delta {
                    for text in delta.emit_chunks {
                        self.enqueue_message(
                            conn,
                            stream_id.as_stream_id(),
                            RequestId::PUSH,
                            OutboundPriority::RawOutput,
                            BrokerMsg::Output { text },
                            now,
                            actions,
                        );
                    }
                    if let Some(waited_us) = delta.throttled_us {
                        self.total_throttled_us = self.total_throttled_us.saturating_add(waited_us);
                        self.enqueue_message(
                            conn,
                            stream_id.as_stream_id(),
                            RequestId::PUSH,
                            OutboundPriority::RawOutput,
                            BrokerMsg::Throttled { waited_us },
                            now,
                            actions,
                        );
                    }
                    self.reconcile_pty_pause(session, actions);
                }
            }
            ClientMsg::ViewportHint(_) if !flags.contains(SubFlags::VIEW) => {
                self.send_error(
                    conn,
                    stream_id.as_stream_id(),
                    req_id,
                    ErrorCode::Malformed,
                    "ViewportHint requires SUB_VIEW",
                    now,
                    actions,
                );
            }
            ClientMsg::ViewportHint(_)
            | ClientMsg::ExplicitResize { .. }
            | ClientMsg::SetCursorDefaults { .. }
            | ClientMsg::Key(_)
            | ClientMsg::Text { .. }
            | ClientMsg::Pointer(_)
            | ClientMsg::Focus { .. }
            | ClientMsg::Paste { .. }
            | ClientMsg::Write { .. }
            | ClientMsg::Pause
            | ClientMsg::Resume
            | ClientMsg::FetchRows { .. }
            | ClientMsg::WordAt { .. }
            | ClientMsg::LineAt { .. }
            | ClientMsg::TextOf { .. }
            | ClientMsg::TextOfRect { .. }
            | ClientMsg::SetSearch { .. }
            | ClientMsg::Commands
            | ClientMsg::CommandOutput { .. }
            | ClientMsg::Images
            | ClientMsg::GetImageBlob { .. } => {
                actions.push(BrokerAction::DispatchToSession { conn, stream_id, attachment_id, req_id, session, msg });
            }
            _ => {
                self.send_error(
                    conn,
                    stream_id.as_stream_id(),
                    req_id,
                    ErrorCode::Malformed,
                    "control message sent on session stream",
                    now,
                    actions,
                );
            }
        }
    }

    fn reconcile_pty_pause(&mut self, session: SessionId, actions: &mut Vec<BrokerAction>) {
        let should_pause = self.conns.values().any(|c| {
            c.streams
                .values()
                .any(|st| st.session == session && st.raw_output.as_ref().is_some_and(RawOutputFlow::is_pty_paused))
        });
        if let Some(rec) = self.sessions.get_mut(&session)
            && rec.pty_paused != should_pause
        {
            rec.pty_paused = should_pause;
            actions.push(BrokerAction::SetPtyPaused { session, paused: should_pause });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn send_error(
        &mut self,
        conn: ConnId,
        stream_id: StreamId,
        req_id: RequestId,
        code: ErrorCode,
        message: &str,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        self.enqueue_message(
            conn,
            stream_id,
            req_id,
            OutboundPriority::Control,
            BrokerMsg::Error { code, message: String::from(message) },
            now,
            actions,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_and_close(
        &mut self,
        conn: ConnId,
        stream_id: StreamId,
        req_id: RequestId,
        code: ErrorCode,
        message: &str,
        reason: CloseReason,
        now: TimestampMs,
        actions: &mut Vec<BrokerAction>,
    ) {
        let err = BrokerMsg::Error { code, message: String::from(message) };
        self.close_connection_with_error(conn, reason, Some((stream_id, req_id, err)), now, actions);
    }
}

fn estimate_encoded_bytes(msg: &BrokerMsg) -> usize {
    let mut w = Writer::new();
    msg.encode(&mut w);
    FRAME_HEADER_LEN + w.as_slice().len()
}

fn estimate_frame_encoded_bytes(frame: &FrameUpdate) -> usize {
    let mut w = Writer::new();
    frame.encode(&mut w);
    FRAME_HEADER_LEN + w.as_slice().len()
}

/// Fast zero-allocation approximate wire size of `frame` for charging reactor-parked stalled frames
/// against [`MemoryBudget`] without serializing the full payload while the pacer is closed.
pub fn estimate_frame_approx_bytes(frame: &FrameUpdate) -> usize {
    let mut total = FRAME_HEADER_LEN + 64;
    for row in &frame.rows {
        let links_bytes: usize = row.links.iter().map(|s| s.len().saturating_add(4)).sum();
        total = total
            .saturating_add(32)
            .saturating_add(row.text.len())
            .saturating_add(row.runs.len().saturating_mul(12))
            .saturating_add(row.styles.len().saturating_mul(16))
            .saturating_add(links_bytes)
            .saturating_add(row.pieces.len().saturating_mul(32));
    }
    if frame.palette.is_some() {
        total = total.saturating_add(1024);
    }
    total
}

fn varint_extra_bytes(len: usize) -> usize {
    let mut w = Writer::new();
    w.var_u64(len as u64);
    w.as_slice().len() - 1
}

/// Ensures `frame` encodes within [`MAX_WIRE_FRAME_BYTES`] by degrading oversized frames to a
/// truncated `full_sync` prefix so they can never stall a stream indefinitely.
fn degrade_frame_to_wire_cap(frame: &mut FrameUpdate) -> usize {
    let encoded = estimate_frame_encoded_bytes(frame);
    if encoded <= MAX_WIRE_FRAME_BYTES {
        return encoded;
    }
    frame.full_sync = true;
    let all_rows = core::mem::take(&mut frame.rows);
    let base_bytes = estimate_frame_encoded_bytes(frame);
    let mut used = 0usize;
    let mut scratch = Writer::new();
    for row in all_rows {
        scratch.clear();
        row.encode(&mut scratch);
        let row_len = scratch.as_slice().len();
        let next_total = base_bytes
            .saturating_add(varint_extra_bytes(frame.rows.len() + 1))
            .saturating_add(used)
            .saturating_add(row_len);
        if next_total > MAX_WIRE_FRAME_BYTES {
            break;
        }
        used += row_len;
        frame.rows.push(row);
    }
    estimate_frame_encoded_bytes(frame)
}
