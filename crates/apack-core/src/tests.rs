use apack_proto::PROTOCOL_VERSION;
use apack_proto::frame::encode_fin;
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SpawnSpec};
use apack_proto::types::{
    CellPx, CursorShape, CursorState, ErrorCode, FrameCredit, GridSize, LeaseGraceMs, LeaseKind, NonZeroStreamId,
    RequestId, ResizePolicy, ResumeToken, SessionId, SignalKind, SignalScope, SignalTarget, StreamId, SubFlags,
    ViewModes, ViewportHint, WaitCondition,
};

use crate::auth::{
    AuthConfig, AuthSecret, compute_client_proof, compute_server_proof, ct_eq_32, verify_client_proof,
    verify_server_proof,
};
use crate::broker::{BrokerAction, BrokerCore, CloseReason, ShardTelemetry};
use crate::flow::{FlowError, FramePacer, OutboundPriority, OutboundQueue, QueueOverflow, RawOutputFlow};
use crate::lease::{ConnId, LeaseError, LeaseId, LeaseTable};
use crate::time::{TimerWheel, TimestampMs};
use crate::{
    AUTH_HANDSHAKE_TIMEOUT_MS, MAX_BDP_FRAME_BYTES, MAX_IN_GRACE_LEASES, MAX_RAW_OUTPUT_CREDIT, MIN_BDP_FRAME_BYTES,
};

fn ts(ms: u64) -> TimestampMs {
    TimestampMs::from_millis(ms)
}

fn conn(n: u64) -> ConnId {
    ConnId::new(n).unwrap()
}

fn sid(n: u64) -> SessionId {
    SessionId::new(n).unwrap()
}

fn nz_stream(n: u32) -> NonZeroStreamId {
    NonZeroStreamId::new(n).unwrap()
}

fn encode_client(stream: StreamId, req: u32, msg: &ClientMsg) -> Vec<u8> {
    let mut out = Vec::new();
    msg.encode_to_frame(stream, RequestId::from_raw(req), &mut out).unwrap();
    out
}

fn sample_frame() -> Box<FrameUpdate> {
    Box::new(FrameUpdate {
        seq: 0,
        epoch: 1,
        size: GridSize::new(80, 24).unwrap(),
        scrollback_lines: 0,
        scroll_delta: 0,
        cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
        modes: ViewModes::default(),
        palette: None,
        rows: vec![],
        images_gen: 0,
        full_sync: true,
    })
}

fn sample_spawn() -> Box<SpawnSpec> {
    Box::new(SpawnSpec {
        argv: vec!["/bin/sh".to_owned()],
        cwd: None,
        env: vec![],
        size: GridSize::new(80, 24).unwrap(),
        cell_px: CellPx::default(),
        scrollback_limit: 1000,
        marks_key: None,
        resize_policy: ResizePolicy::LatestActive,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    })
}

fn handshake_ready(broker: &mut BrokerCore, cid: ConnId, now: TimestampMs) {
    broker.open_connection(cid, [0x22; 32], now);
    let mut actions = Vec::new();
    let hello = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_nonce: [0x11; 32],
            client_name: "test".to_owned(),
        },
    );
    broker.feed_bytes(cid, &hello, now, &mut actions);
    let reply = broker.pop_outbound(cid).expect("HelloOk");
    assert!(matches!(reply.msg, BrokerMsg::HelloOk { auth_required: false, .. }));
}

#[test]
fn timer_wheel_ordering_cancellation_and_exact_deadlines() {
    let t0 = TimestampMs::ZERO;
    assert_eq!(t0.as_millis(), 0);
    assert_eq!(ts(100).saturating_add(50).as_millis(), 150);
    assert_eq!(ts(100).saturating_sub(ts(30)), 70);
    assert_eq!(ts(30).saturating_sub(ts(100)), 0);

    let mut wheel = TimerWheel::<&str>::default();
    assert!(wheel.is_empty());
    assert_eq!(wheel.len(), 0);
    assert_eq!(wheel.next_deadline(), None);

    let id_b = wheel.schedule(ts(200), "b");
    let id_a1 = wheel.schedule(ts(100), "a1");
    let id_a2 = wheel.schedule(ts(100), "a2");
    let id_c = wheel.schedule(ts(150), "c");
    assert_eq!(id_b.get(), 1);
    assert_eq!(id_a1.get(), 2);
    assert!(!wheel.is_empty());
    assert_eq!(wheel.len(), 4);
    assert_eq!(wheel.next_deadline(), Some(ts(100)));

    assert_eq!(wheel.cancel(id_c), Some("c"));
    assert_eq!(wheel.cancel(id_c), None);
    assert_eq!(wheel.len(), 3);

    // 1ms before deadline: nothing fires
    let mut fired = Vec::new();
    wheel.poll_expired(ts(99), &mut fired);
    assert!(fired.is_empty());

    // Exact deadline: ties fire in FIFO TimerId order
    wheel.poll_expired(ts(100), &mut fired);
    assert_eq!(fired, vec![(id_a1, "a1"), (id_a2, "a2")]);
    assert_eq!(wheel.next_deadline(), Some(ts(200)));
    assert_eq!(wheel.cancel(id_a1), None);

    fired.clear();
    wheel.poll_expired(ts(250), &mut fired);
    assert_eq!(fired, vec![(id_b, "b")]);
    assert!(wheel.is_empty());
    assert_eq!(wheel.next_deadline(), None);
}

#[test]
fn mutual_hmac_auth_domain_separation_and_redaction() {
    let secret = AuthSecret::new([0x42; 32]);
    assert_eq!(format!("{secret:?}"), "AuthSecret([REDACTED])");
    assert_eq!(secret.as_bytes(), &[0x42; 32]);
    assert!(!AuthConfig::None.is_required());
    assert!(AuthConfig::Required(secret.clone()).is_required());

    let c_nonce = [0xAA; 32];
    let s_nonce = [0xBB; 32];
    let c_proof = compute_client_proof(&secret, &c_nonce, &s_nonce);
    let s_proof = compute_server_proof(&secret, &s_nonce, &c_nonce);
    assert_ne!(c_proof, [0; 32]);
    assert_ne!(c_proof, [1; 32]);
    assert_ne!(s_proof, [0; 32]);
    assert_ne!(s_proof, [1; 32]);
    assert_ne!(c_proof, s_proof);
    assert_ne!(c_proof, compute_client_proof(&secret, &[0xAB; 32], &s_nonce));
    assert_ne!(s_proof, compute_server_proof(&secret, &[0xBC; 32], &c_nonce));

    assert!(verify_client_proof(&secret, &c_nonce, &s_nonce, &c_proof));
    assert!(verify_server_proof(&secret, &s_nonce, &c_nonce, &s_proof));
    // Domain separation: client proof never verifies as server proof even with swapped nonces
    assert!(!verify_server_proof(&secret, &c_nonce, &s_nonce, &c_proof));
    assert!(!verify_client_proof(&secret, &s_nonce, &c_nonce, &s_proof));

    // Single-bit flip fails
    let mut bad = c_proof;
    bad[31] ^= 1;
    assert!(!verify_client_proof(&secret, &c_nonce, &s_nonce, &bad));
    assert!(!ct_eq_32(&c_proof, &bad));
    assert!(ct_eq_32(&c_proof, &c_proof));
}

#[test]
fn frame_pacer_bdp_clamping_ewma_and_ack_validation() {
    // Hidden view (max_frames = 0) never sends
    let paused = FramePacer::new(FrameCredit { max_frames: 0, max_bytes: 100_000 });
    assert!(paused.is_paused());
    assert!(!paused.can_send(1));

    // Byte window clamping
    let low = FramePacer::new(FrameCredit { max_frames: 3, max_bytes: 1_000 });
    assert_eq!(low.effective_max_bytes(), MIN_BDP_FRAME_BYTES);
    let high = FramePacer::new(FrameCredit { max_frames: 3, max_bytes: 99_000_000 });
    assert_eq!(high.effective_max_bytes(), MAX_BDP_FRAME_BYTES);

    let mut pacer = FramePacer::new(FrameCredit { max_frames: 3, max_bytes: 40_000 });
    assert!(!pacer.is_paused());
    assert_eq!(pacer.ewma_rtt_ms(), None);

    // First frame allowed even if larger than max_bytes (prevents full-sync deadlock)
    assert!(pacer.can_send(100_000));
    let s1 = pacer.on_send(30_000, ts(100));
    assert_eq!(s1, 1);
    assert_eq!(pacer.in_flight_frames(), 1);
    assert_eq!(pacer.in_flight_bytes(), 30_000);

    // Second frame fits exact remaining budget (10_000), +1 exceeds
    assert!(pacer.can_send(10_000));
    assert!(!pacer.can_send(10_001));
    let s2 = pacer.on_send(10_000, ts(110));
    assert_eq!(s2, 2);
    assert_eq!(pacer.in_flight_bytes(), 40_000);
    assert!(!pacer.can_send(1));

    // Invalid acks rejected
    assert_eq!(
        pacer.on_ack(0, FrameCredit { max_frames: 3, max_bytes: 40_000 }, ts(120)),
        Err(FlowError::InvalidAckSeq)
    );
    assert_eq!(
        pacer.on_ack(3, FrameCredit { max_frames: 3, max_bytes: 40_000 }, ts(120)),
        Err(FlowError::InvalidAckSeq)
    );

    // Cumulative ack of seq 2 clears both 1 & 2 and sets initial RTT = 150 - 110 = 40ms
    pacer.on_ack(2, FrameCredit { max_frames: 1, max_bytes: 40_000 }, ts(150)).unwrap();
    assert_eq!(pacer.in_flight_frames(), 0);
    assert_eq!(pacer.in_flight_bytes(), 0);
    assert_eq!(pacer.ewma_rtt_ms(), Some(40));

    // Re-acking same seq (e.g., credit update on un-hide) is allowed
    pacer.on_ack(2, FrameCredit { max_frames: 1, max_bytes: 40_000 }, ts(155)).unwrap();
    assert_eq!(pacer.ewma_rtt_ms(), Some(40));

    // Regressing ack rejected
    assert_eq!(
        pacer.on_ack(1, FrameCredit { max_frames: 1, max_bytes: 40_000 }, ts(160)),
        Err(FlowError::InvalidAckSeq)
    );

    // max_frames == 1 blocks 2nd frame even with plenty of byte budget
    let s3 = pacer.on_send(500, ts(200));
    assert_eq!(s3, 3);
    assert!(!pacer.can_send(500));
    // RTT sample = 280 - 200 = 80ms; EWMA = (40*3 + 80)/4 = 50ms
    pacer.on_ack(3, FrameCredit { max_frames: 2, max_bytes: 60_000 }, ts(280)).unwrap();
    assert_eq!(pacer.ewma_rtt_ms(), Some(50));
}

#[test]
fn raw_output_flow_utf8_splits_backpressure_and_best_effort() {
    // Credit clamping
    let clamped = RawOutputFlow::new(u32::MAX, false);
    assert_eq!(clamped.credit_bytes(), MAX_RAW_OUTPUT_CREDIT);

    // Best-effort mode: 1-byte split of 4-byte emoji + invalid byte + credit boundary shedding
    let mut best = RawOutputFlow::new(6, false);
    let rocket = "🚀".as_bytes(); // 4 bytes
    assert_eq!(best.ingest(&[], ts(0)).emit_text, None);
    for b in &rocket[..3] {
        let d = best.ingest(std::slice::from_ref(b), ts(1));
        assert_eq!(d.emit_text, None);
        assert_eq!(d.dropped_bytes, 0);
    }
    let d = best.ingest(&rocket[3..], ts(2));
    assert_eq!(d.emit_text.as_deref(), Some("🚀"));
    assert_eq!(best.credit_bytes(), 2);

    // Invalid UTF-8 byte replaced with U+FFFD (3 bytes) exceeds 2 bytes remaining -> dropped cleanly
    let d = best.ingest(&[0xFF, b'a'], ts(3));
    assert_eq!(d.emit_text, None);
    assert_eq!(d.dropped_bytes, 4); // "\u{FFFD}a" = 4 bytes

    // Valid 2-byte ASCII fits remaining 2-byte credit, 3rd byte shed
    let d = best.ingest(b"xyz", ts(4));
    assert_eq!(d.emit_text.as_deref(), Some("xy"));
    assert_eq!(d.dropped_bytes, 1);
    assert_eq!(best.credit_bytes(), 0);
    assert!(!best.is_pty_paused());

    // Backpressure mode: holds remainder, pauses PTY, drains on credit, reports waited_us
    let mut bp = RawOutputFlow::with_max_pending(3, true, 6);
    // Ingest smaller than credit: does NOT pause
    let d0 = bp.ingest(b"a", ts(90));
    assert_eq!(d0.emit_text.as_deref(), Some("a"));
    assert!(!d0.pty_paused);
    assert!(!bp.is_pty_paused());

    // Ingest hitting exact remaining credit (2 bytes): credit becomes 0 -> pauses with empty pending
    let d_exact = bp.ingest(b"bc", ts(95));
    assert_eq!(d_exact.emit_text.as_deref(), Some("bc"));
    assert!(d_exact.pty_paused);
    assert_eq!(bp.credit_bytes(), 0);
    // Granting 0 credit does NOT unpause or emit throttled_us
    let d_zero = bp.grant_credit(0, ts(96));
    assert_eq!(d_zero.emit_text, None);
    assert_eq!(d_zero.throttled_us, None);
    assert!(d_zero.pty_paused);
    // Granting 3 credit unpauses immediately and reports (98 - 95) * 1000 = 3_000
    let d_unpause = bp.grant_credit(3, ts(98));
    assert_eq!(d_unpause.emit_text, None);
    assert_eq!(d_unpause.throttled_us, Some(3_000));
    assert!(!d_unpause.pty_paused);

    // Ingest exceeding credit (3) and testing bounded pending cap (6)
    let d1 = bp.ingest(b"hello", ts(100));
    assert_eq!(d1.emit_text.as_deref(), Some("hel"));
    assert_eq!(d1.dropped_bytes, 0);
    assert!(d1.pty_paused);
    assert!(bp.is_pty_paused());

    // Additional ingest while already throttled does NOT overwrite throttled_since, and enforces max_pending=6
    let d_cap = bp.ingest(b"123456", ts(110));
    assert_eq!(d_cap.emit_text, None);
    assert_eq!(d_cap.dropped_bytes, 2); // pending already had "lo" (2), room = 4, kept "1234", dropped "56"

    // Zero credit while pending is non-empty emits nothing
    assert_eq!(bp.grant_credit(0, ts(112)).emit_text, None);

    // Partial credit drains 1 byte, stays paused
    let d2 = bp.grant_credit(1, ts(115));
    assert_eq!(d2.emit_text.as_deref(), Some("l"));
    assert_eq!(d2.throttled_us, None);
    assert!(d2.pty_paused);

    // Remaining credit drains "o1234" and releases pause with waited_us = (125 - 100) * 1000 = 25_000
    let d3 = bp.grant_credit(10, ts(125));
    assert_eq!(d3.emit_text.as_deref(), Some("o1234"));
    assert_eq!(d3.throttled_us, Some(25_000));
    assert!(!d3.pty_paused);
    assert_eq!(bp.total_throttled_us(), 28_000);
    assert_eq!(bp.credit_bytes(), 5);
}

#[test]
fn outbound_queue_priority_conflation_wfq_and_caps() {
    let mut q = OutboundQueue::new(100);
    assert!(q.is_empty());
    assert_eq!(q.ordered_bytes(), 0);
    assert_eq!(q.pending_frames(), 0);

    let s1 = StreamId::from_raw(1);
    let s2 = StreamId::from_raw(2);

    // Verify is_empty() individually for each of the 5 tiers
    q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::PUSH, BrokerMsg::Ok, 1).unwrap();
    assert!(!q.is_empty());
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());

    q.push_frame(s1, sample_frame(), 1);
    assert!(!q.is_empty());
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());

    q.push_ordered(OutboundPriority::Event, s1, RequestId::PUSH, BrokerMsg::Bell, 1).unwrap();
    assert!(!q.is_empty());
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());

    q.push_ordered(OutboundPriority::Bulk, s1, RequestId::PUSH, BrokerMsg::Bell, 1).unwrap();
    assert!(!q.is_empty());
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());

    q.push_ordered(OutboundPriority::RawOutput, s1, RequestId::PUSH, BrokerMsg::Bell, 1).unwrap();
    assert!(!q.is_empty());
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());

    // Enqueue across all 5 tiers
    q.push_ordered(OutboundPriority::RawOutput, s1, RequestId::PUSH, BrokerMsg::Output { text: "r1a".into() }, 10)
        .unwrap();
    q.push_ordered(OutboundPriority::RawOutput, s1, RequestId::PUSH, BrokerMsg::Output { text: "r1b".into() }, 10)
        .unwrap();
    q.push_ordered(OutboundPriority::RawOutput, s2, RequestId::PUSH, BrokerMsg::Output { text: "r2a".into() }, 10)
        .unwrap();

    q.push_ordered(OutboundPriority::Bulk, s1, RequestId::from_raw(1), BrokerMsg::TextReply { text: "b1a".into() }, 10)
        .unwrap();
    q.push_ordered(OutboundPriority::Bulk, s1, RequestId::from_raw(2), BrokerMsg::TextReply { text: "b1b".into() }, 10)
        .unwrap();
    q.push_ordered(OutboundPriority::Bulk, s2, RequestId::from_raw(3), BrokerMsg::TextReply { text: "b2a".into() }, 10)
        .unwrap();

    q.push_ordered(OutboundPriority::Event, s1, RequestId::PUSH, BrokerMsg::Bell, 10).unwrap();
    q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::from_raw(9), BrokerMsg::Ok, 10).unwrap();

    // Frame conflation on s1: second push supersedes first without increasing pending_frames
    let mut f1 = sample_frame();
    f1.seq = 1;
    let mut f2 = sample_frame();
    f2.seq = 2;
    q.push_frame(s1, f1, 50);
    q.push_frame(s1, f2, 50);
    assert_eq!(q.pending_frames(), 1);
    assert_eq!(q.ordered_bytes(), 80);

    // Hard ordered byte cap: exact 20 bytes fits, +1 fails with QueueOverflow
    assert_eq!(
        q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::PUSH, BrokerMsg::Ok, 21),
        Err(QueueOverflow)
    );
    q.push_ordered(OutboundPriority::Event, s2, RequestId::PUSH, BrokerMsg::Bell, 20).unwrap();
    assert_eq!(q.ordered_bytes(), 100);

    // Pop order: Control -> Frame(seq=2) -> Event(s1) -> Event(s2) -> Bulk WFQ(s1, s2, s1) -> Raw WFQ(s1, s2, s1)
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Ok);
    match q.pop_next().unwrap().msg {
        BrokerMsg::Frame(f) => assert_eq!(f.seq, 2),
        other => panic!("expected conflated Frame(seq=2), got {other:?}"),
    }
    assert_eq!(q.pop_next().unwrap().stream_id, s1); // Bell s1
    assert_eq!(q.pop_next().unwrap().stream_id, s2); // Bell s2
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::TextReply { text: "b1a".into() });
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::TextReply { text: "b2a".into() });
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::TextReply { text: "b1b".into() });
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Output { text: "r1a".into() });
    assert_eq!(q.ordered_bytes(), 20);
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Output { text: "r2a".into() });
    assert_eq!(q.ordered_bytes(), 10);
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Output { text: "r1b".into() });
    assert!(q.is_empty());
    assert_eq!(q.ordered_bytes(), 0);
    assert_eq!(q.pop_next(), None);

    // remove_stream cleans s1 across all tiers while preserving s2 in every tier
    q.push_frame(s1, sample_frame(), 20);
    q.push_frame(s2, sample_frame(), 20);
    q.push_ordered(OutboundPriority::Event, s1, RequestId::PUSH, BrokerMsg::Bell, 15).unwrap();
    q.push_ordered(OutboundPriority::Bulk, s1, RequestId::PUSH, BrokerMsg::Bell, 25).unwrap();
    q.push_ordered(OutboundPriority::Bulk, s2, RequestId::PUSH, BrokerMsg::TextReply { text: "keep_bulk".into() }, 5)
        .unwrap();
    q.push_ordered(OutboundPriority::RawOutput, s1, RequestId::PUSH, BrokerMsg::Bell, 30).unwrap();
    q.push_ordered(OutboundPriority::RawOutput, s2, RequestId::PUSH, BrokerMsg::Output { text: "keep_raw".into() }, 5)
        .unwrap();
    q.push_ordered(OutboundPriority::Event, s2, RequestId::PUSH, BrokerMsg::Bell, 10).unwrap();
    q.remove_stream(s1);
    assert_eq!(q.pending_frames(), 1);
    assert_eq!(q.ordered_bytes(), 20);
    assert_eq!(q.pop_next().unwrap().stream_id, s2); // frame
    assert_eq!(q.pop_next().unwrap().stream_id, s2); // event
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::TextReply { text: "keep_bulk".into() });
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Output { text: "keep_raw".into() });
    assert!(q.is_empty());
}

#[test]
fn lease_table_immediate_grace_resume_expiry_and_cap() {
    assert!(ConnId::new(0).is_none());
    assert_eq!(ConnId::new(3).unwrap().get(), 3);
    assert!(LeaseId::new(0).is_none());
    assert_eq!(LeaseId::new(5).unwrap().get(), 5);

    let mut table = LeaseTable::default();
    let mut wheel = TimerWheel::<LeaseId>::new();
    assert!(!table.has_active_broker_lease());
    assert_eq!(table.in_grace_count(), 0);

    // Disconnecting a connection with no broker lease does NOT trigger teardown
    let dc_none = table.on_disconnect(conn(99), ts(5), |id, at| wheel.schedule(at, id));
    assert!(!dc_none.teardown_non_kept);
    // Expiring a non-existent lease does NOT trigger teardown
    assert!(!table.on_grace_expired(LeaseId::new(999).unwrap()).teardown_non_kept);

    // Two broker leases: disconnecting first leaves second alive (no teardown!)
    let l1 = table.acquire(conn(1), LeaseKind::BrokerImmediate).unwrap();
    let _l2 = table.acquire(conn(2), LeaseKind::BrokerImmediate).unwrap();
    assert!(!l1.resumed);
    assert!(table.has_active_broker_lease());

    // Wrong owner cannot release
    assert_eq!(table.release(conn(2), l1.lease_id), Err(LeaseError::NotOwned));
    let c1 = table.on_disconnect(conn(1), ts(8), |id, at| wheel.schedule(at, id));
    assert!(!c1.teardown_non_kept);
    assert!(table.has_active_broker_lease());

    // Disconnecting second tears down non-kept sessions immediately
    let c2 = table.on_disconnect(conn(2), ts(10), |id, at| wheel.schedule(at, id));
    assert!(c2.teardown_non_kept);
    assert!(!table.has_active_broker_lease());

    // Expiring a grace lease while another broker lease is active does NOT trigger teardown
    let tok_a = ResumeToken::new([0x66; 32]);
    let grace = LeaseGraceMs::new(30_000).unwrap();
    let la = table.acquire(conn(20), LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok_a }).unwrap();
    let lb = table.acquire(conn(21), LeaseKind::BrokerImmediate).unwrap();
    let _ = table.on_disconnect(conn(20), ts(50), |id, at| wheel.schedule(at, id));
    assert!(!table.on_grace_expired(la.lease_id).teardown_non_kept);
    assert!(table.release(conn(21), lb.lease_id).unwrap().teardown_non_kept);

    // Remote BrokerGrace: disconnect schedules grace timer; TokenInUse blocks live duplicate
    let tok = ResumeToken::new([0x77; 32]);
    let lg = table.acquire(conn(3), LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok }).unwrap();
    assert_eq!(
        table.acquire(conn(4), LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok }),
        Err(LeaseError::TokenInUse)
    );

    let dc = table.on_disconnect(conn(3), ts(1_000), |id, at| wheel.schedule(at, id));
    assert!(!dc.teardown_non_kept);
    assert_eq!(dc.schedule_grace, vec![(lg.lease_id, ts(31_000))]);
    assert_eq!(table.in_grace_count(), 1);
    assert!(table.has_active_broker_lease());

    // Seamless resume from conn(4) cancels timer and returns same lease_id with resumed=true
    let resumed = table.acquire(conn(4), LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok }).unwrap();
    assert!(resumed.resumed);
    assert_eq!(resumed.lease_id, lg.lease_id);
    assert!(resumed.cancelled_timer.is_some());
    assert_eq!(table.in_grace_count(), 0);
    wheel.cancel(resumed.cancelled_timer.unwrap());

    // Disconnect again and let grace timer expire -> teardown_non_kept = true
    let dc2 = table.on_disconnect(conn(4), ts(2_000), |id, at| wheel.schedule(at, id));
    assert_eq!(dc2.schedule_grace, vec![(lg.lease_id, ts(32_000))]);
    let exp = table.on_grace_expired(lg.lease_id);
    assert!(exp.teardown_non_kept);
    assert!(!table.has_active_broker_lease());

    // Two broker leases: releasing one leaves the other active (no teardown)
    let r1 = table.acquire(conn(11), LeaseKind::BrokerImmediate).unwrap();
    let r2 = table.acquire(conn(12), LeaseKind::BrokerImmediate).unwrap();
    assert!(!table.release(conn(11), r1.lease_id).unwrap().teardown_non_kept);
    assert!(table.release(conn(12), r2.lease_id).unwrap().teardown_non_kept);

    // Session lease pin / unpin via both release and disconnect (including shared pin across 2 conns)
    let ls = table.acquire(conn(5), LeaseKind::Session(sid(9))).unwrap();
    assert!(table.is_session_pinned(sid(9)));
    let rel = table.release(conn(5), ls.lease_id).unwrap();
    assert_eq!(rel.unpinned_sessions, vec![sid(9)]);
    assert!(!table.is_session_pinned(sid(9)));

    let _s1 = table.acquire(conn(6), LeaseKind::Session(sid(10))).unwrap();
    let _s2 = table.acquire(conn(7), LeaseKind::Session(sid(10))).unwrap();
    let dc_s1 = table.on_disconnect(conn(6), ts(3_000), |id, at| wheel.schedule(at, id));
    assert!(dc_s1.unpinned_sessions.is_empty());
    assert!(table.is_session_pinned(sid(10)));
    let dc_s2 = table.on_disconnect(conn(7), ts(3_001), |id, at| wheel.schedule(at, id));
    assert_eq!(dc_s2.unpinned_sessions, vec![sid(10)]);
    assert!(!table.is_session_pinned(sid(10)));

    // Hard cap on in-grace leases: exceeding MAX_IN_GRACE_LEASES drops excess instead of leaking
    for i in 0..=(MAX_IN_GRACE_LEASES as u64) {
        let mut t = [0u8; 32];
        t[0..8].copy_from_slice(&i.to_le_bytes());
        let cid = conn(100 + i);
        table.acquire(cid, LeaseKind::BrokerGrace { grace_ms: grace, resume_token: ResumeToken::new(t) }).unwrap();
        table.on_disconnect(cid, ts(5_000), |id, at| wheel.schedule(at, id));
    }
    assert_eq!(table.in_grace_count(), MAX_IN_GRACE_LEASES);
}

#[test]
fn broker_handshake_mutual_auth_timeout_and_framing_errors() {
    let secret = AuthSecret::new([0x99; 32]);
    let mut broker = BrokerCore::new(AuthConfig::Required(secret.clone()));
    let mut actions = Vec::new();

    // 1. Handshake timeout fires at exact AUTH_HANDSHAKE_TIMEOUT_MS
    broker.open_connection(conn(1), [0x10; 32], ts(1_000));
    assert_eq!(broker.next_deadline(), Some(ts(1_000 + AUTH_HANDSHAKE_TIMEOUT_MS)));
    broker.poll_timers(ts(1_000 + AUTH_HANDSHAKE_TIMEOUT_MS - 1), &mut actions);
    assert_eq!(broker.connection_count(), 1);
    broker.poll_timers(ts(1_000 + AUTH_HANDSHAKE_TIMEOUT_MS), &mut actions);
    assert_eq!(broker.connection_count(), 0);
    assert!(matches!(
        actions.last(),
        Some(BrokerAction::CloseConn {
            conn: c,
            reason: CloseReason::HandshakeTimeout,
            final_error: Some((_, _, BrokerMsg::Error { code: ErrorCode::Handshake, .. })),
        }) if *c == conn(1)
    ));

    // 2. Successful mutual HMAC handshake
    actions.clear();
    let c_nonce = [0xAA; 32];
    let s_nonce = [0xBB; 32];
    broker.open_connection(conn(2), s_nonce, ts(2_000));
    let hello = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION, client_nonce: c_nonce, client_name: "c".into() },
    );
    broker.feed_bytes(conn(2), &hello, ts(2_010), &mut actions);
    let reply = broker.pop_outbound(conn(2)).unwrap();
    assert!(matches!(
        reply.msg,
        BrokerMsg::HelloOk { auth_required: true, server_nonce, .. } if server_nonce == s_nonce
    ));

    let proof = compute_client_proof(&secret, &c_nonce, &s_nonce);
    let auth_frame = encode_client(StreamId::CONTROL, 2, &ClientMsg::AuthProof { hmac: proof });
    broker.feed_bytes(conn(2), &auth_frame, ts(2_020), &mut actions);
    let auth_ok = broker.pop_outbound(conn(2)).unwrap();
    match auth_ok.msg {
        BrokerMsg::AuthOk { server_hmac } => {
            assert!(verify_server_proof(&secret, &s_nonce, &c_nonce, &server_hmac));
        }
        other => panic!("expected AuthOk, got {other:?}"),
    }
    // Handshake timer was cancelled
    assert_eq!(broker.next_deadline(), None);

    // 3. Duplicate Hello when Ready fails and closes
    actions.clear();
    broker.feed_bytes(conn(2), &hello, ts(2_030), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    // 4. Bad HMAC fails with Unauthorized
    actions.clear();
    broker.open_connection(conn(3), s_nonce, ts(3_000));
    broker.feed_bytes(conn(3), &hello, ts(3_001), &mut actions);
    let _ = broker.pop_outbound(conn(3));
    let bad_auth = encode_client(StreamId::CONTROL, 2, &ClientMsg::AuthProof { hmac: [0; 32] });
    broker.feed_bytes(conn(3), &bad_auth, ts(3_002), &mut actions);
    assert!(matches!(
        actions.last(),
        Some(BrokerAction::CloseConn {
            reason: CloseReason::Unauthorized,
            final_error: Some((_, _, BrokerMsg::Error { code: ErrorCode::Unauthorized, .. })),
            ..
        })
    ));

    // 5. Wrong protocol version & FIN & malformed wire frame
    actions.clear();
    broker.open_connection(conn(4), s_nonce, ts(4_000));
    let bad_ver = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::Hello { protocol_version: 99, client_nonce: c_nonce, client_name: "old".into() },
    );
    broker.feed_bytes(conn(4), &bad_ver, ts(4_001), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    actions.clear();
    broker.open_connection(conn(5), s_nonce, ts(5_000));
    let mut fin = Vec::new();
    encode_fin(&mut fin);
    broker.feed_bytes(conn(5), &fin, ts(5_001), &mut actions);
    assert!(matches!(
        actions.last(),
        Some(BrokerAction::CloseConn { reason: CloseReason::ClientFin, final_error: None, .. })
    ));

    actions.clear();
    broker.open_connection(conn(6), s_nonce, ts(6_000));
    broker.feed_bytes(conn(6), &5u32.to_le_bytes(), ts(6_001), &mut actions); // body_len < 10
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));
}

#[test]
fn broker_multiplexing_flow_control_leases_and_slow_reader() {
    let mut broker = BrokerCore::with_queue_cap(AuthConfig::None, 256);
    handshake_ready(&mut broker, conn(1), ts(100));
    broker.register_session(sid(1), false, false);
    broker.register_session(sid(2), true, false);
    broker.update_shard_telemetry(ShardTelemetry {
        scrollback_bytes: 1_000,
        image_store_bytes: 2_000,
        total_pty_bytes: 3_000,
    });

    let mut actions = Vec::new();

    // Control messages: Spawn, Kill, Signal, Owns, ListSessions, Rewind, Seek (live fails, rewound ok), Keep, Wait, Metrics
    let mut batch = Vec::new();
    batch.extend(encode_client(StreamId::CONTROL, 10, &ClientMsg::Spawn(sample_spawn())));
    batch.extend(encode_client(StreamId::CONTROL, 11, &ClientMsg::Kill { session: sid(1) }));
    batch.extend(encode_client(
        StreamId::CONTROL,
        12,
        &ClientMsg::Signal { session: sid(1), signal: SignalKind::Int, scope: SignalScope::Group },
    ));
    batch.extend(encode_client(
        StreamId::CONTROL,
        13,
        &ClientMsg::Owns { session: sid(1), pid: SignalTarget::new(42).unwrap() },
    ));
    batch.extend(encode_client(StreamId::CONTROL, 14, &ClientMsg::ListSessions));
    batch.extend(encode_client(StreamId::CONTROL, 15, &ClientMsg::Rewind { record_dir: "/tmp/r".into(), at_ms: None }));
    batch.extend(encode_client(StreamId::CONTROL, 16, &ClientMsg::Seek { session: sid(1), at_ms: 50 })); // live -> err
    batch.extend(encode_client(StreamId::CONTROL, 17, &ClientMsg::Seek { session: sid(2), at_ms: 50 })); // rewound -> ok
    batch.extend(encode_client(StreamId::CONTROL, 18, &ClientMsg::Keep { session: sid(1), kept: true }));
    batch.extend(encode_client(
        StreamId::CONTROL,
        19,
        &ClientMsg::Wait { session: sid(1), condition: WaitCondition::Idle, timeout_ms: 500 },
    ));
    batch.extend(encode_client(StreamId::CONTROL, 20, &ClientMsg::Metrics));
    broker.feed_bytes(conn(1), &batch, ts(110), &mut actions);

    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SpawnSession { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::KillSession { session, .. } if *session == sid(1))));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SignalSession { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::QueryOwns { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::ListSessions { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::RewindRecording { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SeekRecording { session, .. } if *session == sid(2))));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SetSessionKept { kept: true, .. })));
    assert!(broker.is_session_kept(sid(1)));
    assert!(!broker.is_session_kept(sid(2)));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::WaitSession { .. })));

    // Drain outbound: Error(Seek on live) + MetricsReply
    let err = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(err.req_id, RequestId::from_raw(16));
    assert!(matches!(err.msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));
    let metrics = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(metrics.req_id, RequestId::from_raw(20));
    match metrics.msg {
        BrokerMsg::MetricsReply(m) => {
            assert_eq!(m.live_sessions, 1);
            assert_eq!(m.rewound_sessions, 1);
            assert_eq!(m.connections, 1);
            assert_eq!(m.scrollback_bytes, 1_000);
            assert_eq!(m.image_store_bytes, 2_000);
            assert_eq!(m.total_pty_bytes, 3_000);
        }
        other => panic!("expected MetricsReply, got {other:?}"),
    }

    // Attach stream 1 with VIEW | RAW_OUTPUT | BACKPRESSURE_PTY
    actions.clear();
    let attach = encode_client(
        StreamId::CONTROL,
        21,
        &ClientMsg::Attach {
            session: sid(1),
            stream_id: nz_stream(1),
            flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::RAW_OUTPUT | SubFlags::BACKPRESSURE_PTY).unwrap(),
            initial_viewport: Some(ViewportHint {
                size: GridSize::new(80, 24).unwrap(),
                cell_px: CellPx::default(),
                visible: true,
                focused: true,
            }),
            initial_output_credit: 4,
            initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
        },
    );
    broker.feed_bytes(conn(1), &attach, ts(120), &mut actions);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, BrokerAction::StreamAttached { stream_id, .. } if *stream_id == nz_stream(1)))
    );

    // ViewportHint succeeds on a SUB_VIEW stream
    actions.clear();
    let ok_hint = encode_client(
        nz_stream(1).as_stream_id(),
        22,
        &ClientMsg::ViewportHint(ViewportHint {
            size: GridSize::new(100, 30).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: true,
        }),
    );
    broker.feed_bytes(conn(1), &ok_hint, ts(125), &mut actions);
    assert!(actions.iter().any(|a| matches!(
        a,
        BrokerAction::DispatchToSession { req_id, msg: ClientMsg::ViewportHint(_), .. }
        if *req_id == RequestId::from_raw(22)
    )));

    // Attach a best-effort RAW_OUTPUT stream on sid(2) and a VIEW-only stream on sid(1)
    let attach_best = encode_client(
        StreamId::CONTROL,
        23,
        &ClientMsg::Attach {
            session: sid(2),
            stream_id: nz_stream(2),
            flags: SubFlags::from_bits(SubFlags::RAW_OUTPUT).unwrap(),
            initial_viewport: None,
            initial_output_credit: 2,
            initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
        },
    );
    let attach_view_only = encode_client(
        StreamId::CONTROL,
        24,
        &ClientMsg::Attach {
            session: sid(1),
            stream_id: nz_stream(3),
            flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
            initial_viewport: None,
            initial_output_credit: 0,
            initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
        },
    );
    broker.feed_bytes(conn(1), &attach_best, ts(126), &mut actions);
    broker.feed_bytes(conn(1), &attach_view_only, ts(127), &mut actions);

    // Best-effort RAW_OUTPUT on sid(2): emits 2 bytes and explicitly reports OutputDropped(3)
    actions.clear();
    broker.publish_raw_output(sid(2), b"12345", ts(128), &mut actions);
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "12".into() });
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::OutputDropped { bytes: 3 });
    assert_eq!(broker.pop_outbound(conn(1)), None);

    // Publish frame -> accepted (seq=1), 2nd blocked until FrameAck
    actions.clear();
    assert!(broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(130), &mut actions));
    assert!(!broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(135), &mut actions));
    let f = broker.pop_outbound(conn(1)).unwrap();
    assert!(matches!(f.msg, BrokerMsg::Frame(ref fr) if fr.seq == 1));

    // FrameAck replenishes credit and computes RTT = 150 - 130 = 20ms
    let ack = encode_client(
        nz_stream(1).as_stream_id(),
        0,
        &ClientMsg::FrameAck { seq: 1, credit: FrameCredit { max_frames: 2, max_bytes: 65_536 } },
    );
    broker.feed_bytes(conn(1), &ack, ts(150), &mut actions);
    assert_eq!(broker.stream_rtt_ms(conn(1), nz_stream(1)), Some(20));
    assert!(broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(155), &mut actions));
    let _ = broker.pop_outbound(conn(1));

    // Raw output with BACKPRESSURE_PTY pauses PTY when credit exhausted, resumes on OutputCredit
    actions.clear();
    broker.publish_raw_output(sid(1), b"abcdef", ts(200), &mut actions);
    assert!(
        actions.iter().any(|a| matches!(a, BrokerAction::SetPtyPaused { session, paused: true } if *session == sid(1)))
    );
    let out1 = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(out1.msg, BrokerMsg::Output { text: "abcd".into() });

    actions.clear();
    let credit = encode_client(nz_stream(1).as_stream_id(), 0, &ClientMsg::OutputCredit { bytes: 10 });
    broker.feed_bytes(conn(1), &credit, ts(215), &mut actions);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, BrokerAction::SetPtyPaused { session, paused: false } if *session == sid(1)))
    );
    let out2 = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(out2.msg, BrokerMsg::Output { text: "ef".into() });
    let thr = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(thr.msg, BrokerMsg::Throttled { waited_us: 15_000 });
    assert_eq!(broker.metrics_snapshot().total_throttled_us, 15_000);

    // Unregistering session emits StreamClosed on both attached streams (1 and 3) and cleans up
    actions.clear();
    broker.unregister_session(sid(1), "exited", ts(220), &mut actions);
    let closed1 = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(closed1.msg, BrokerMsg::StreamClosed { reason: "exited".into() });
    let closed3 = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(closed3.msg, BrokerMsg::StreamClosed { reason: "exited".into() });
    assert_eq!(broker.pop_outbound(conn(1)), None);

    // Slow reader hitting 256-byte queue cap is closed cleanly with CloseReason::SlowReader
    actions.clear();
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::PUSH,
        OutboundPriority::Event,
        BrokerMsg::Title { title: "x".repeat(300) },
        ts(230),
        &mut actions,
    );
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::SlowReader, .. })));
    assert_eq!(broker.connection_count(), 0);
}

#[test]
fn broker_stream_capability_checks_leases_and_detach() {
    let mut broker = BrokerCore::new(AuthConfig::None);
    handshake_ready(&mut broker, conn(1), ts(10));
    broker.register_session(sid(1), false, false);
    let mut actions = Vec::new();

    // Acquire BrokerGrace lease, disconnect, verify grace timer scheduled, reconnect on conn(2) & resume
    let tok = ResumeToken::new([0x33; 32]);
    let grace = LeaseGraceMs::new(5_000).unwrap();
    let acq = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::AcquireLease { kind: LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok } },
    );
    broker.feed_bytes(conn(1), &acq, ts(20), &mut actions);
    let lease_ack = broker.pop_outbound(conn(1)).unwrap();
    let lease_id = match lease_ack.msg {
        BrokerMsg::LeaseAck { lease_id, resumed } => {
            assert!(!resumed);
            lease_id
        }
        other => panic!("expected LeaseAck, got {other:?}"),
    };

    // Attach EVENTS-only stream (no VIEW, no RAW_OUTPUT) -> FrameAck, ViewportHint, OutputCredit rejected
    let attach = encode_client(
        StreamId::CONTROL,
        2,
        &ClientMsg::Attach {
            session: sid(1),
            stream_id: nz_stream(2),
            flags: SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
            initial_viewport: None,
            initial_output_credit: 0,
            initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
        },
    );
    broker.feed_bytes(conn(1), &attach, ts(25), &mut actions);

    // Duplicate attach on same stream id rejected
    broker.feed_bytes(conn(1), &attach, ts(26), &mut actions);
    assert!(matches!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));

    // Disallowed stream ops without capabilities
    let bad_ack = encode_client(
        nz_stream(2).as_stream_id(),
        3,
        &ClientMsg::FrameAck { seq: 1, credit: FrameCredit { max_frames: 1, max_bytes: 65_536 } },
    );
    let bad_hint = encode_client(
        nz_stream(2).as_stream_id(),
        4,
        &ClientMsg::ViewportHint(ViewportHint {
            size: GridSize::new(80, 24).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: true,
        }),
    );
    let bad_credit = encode_client(nz_stream(2).as_stream_id(), 5, &ClientMsg::OutputCredit { bytes: 100 });
    let bad_ctrl = encode_client(nz_stream(2).as_stream_id(), 6, &ClientMsg::ListSessions);
    let good_write = encode_client(nz_stream(2).as_stream_id(), 7, &ClientMsg::Write { data: b"hi".to_vec() });
    let mut bad_batch = Vec::new();
    bad_batch.extend(bad_ack);
    bad_batch.extend(bad_hint);
    bad_batch.extend(bad_credit);
    bad_batch.extend(bad_ctrl);
    bad_batch.extend(good_write);
    actions.clear();
    broker.feed_bytes(conn(1), &bad_batch, ts(30), &mut actions);
    for req in 3..=6 {
        let m = broker.pop_outbound(conn(1)).unwrap();
        assert_eq!(m.req_id, RequestId::from_raw(req));
        assert!(matches!(m.msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));
    }
    assert!(actions.iter().any(|a| matches!(
        a,
        BrokerAction::DispatchToSession { req_id, msg: ClientMsg::Write { .. }, .. }
        if *req_id == RequestId::from_raw(7)
    )));

    // Detach stream 2 -> Ok; second detach -> NoSuchStream
    let detach = encode_client(StreamId::CONTROL, 8, &ClientMsg::Detach { stream_id: nz_stream(2) });
    broker.feed_bytes(conn(1), &detach, ts(35), &mut actions);
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Ok);
    broker.feed_bytes(conn(1), &detach, ts(36), &mut actions);
    assert!(matches!(
        broker.pop_outbound(conn(1)).unwrap().msg,
        BrokerMsg::Error { code: ErrorCode::NoSuchStream, .. }
    ));

    // Disconnect conn(1) -> enters grace until ts(5040)
    actions.clear();
    broker.close_connection(conn(1), CloseReason::TransportClosed, ts(40), &mut actions);
    assert!(!actions.contains(&BrokerAction::TeardownNonKeptSessions));
    assert_eq!(broker.next_deadline(), Some(ts(5_040)));

    // Reconnect on conn(2) and resume lease -> grace timer cancelled
    handshake_ready(&mut broker, conn(2), ts(1_000));
    broker.feed_bytes(conn(2), &acq, ts(1_010), &mut actions);
    assert_eq!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::LeaseAck { lease_id, resumed: true });
    assert_eq!(broker.next_deadline(), None);

    // Explicit ReleaseLease tears down non-kept sessions
    actions.clear();
    let rel = encode_client(StreamId::CONTROL, 9, &ClientMsg::ReleaseLease { lease_id });
    broker.feed_bytes(conn(2), &rel, ts(1_020), &mut actions);
    assert!(actions.contains(&BrokerAction::TeardownNonKeptSessions));
    assert_eq!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::Ok);
}
