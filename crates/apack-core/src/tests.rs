use apack_proto::BULK_CHUNK_BYTES;
use apack_proto::PROTOCOL_VERSION;
use apack_proto::codec::Writer;
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
use crate::broker::{BrokerAction, BrokerCore, CloseReason, DuplicateConnId, ShardTelemetry};
use crate::flow::{FlowError, FramePacer, OutboundPriority, OutboundQueue, QueueOverflow, RawOutputFlow};
use crate::lease::{ConnId, LeaseError, LeaseId, LeaseTable};
use crate::time::{TimerWheel, TimestampMs};
use crate::{
    AUTH_HANDSHAKE_TIMEOUT_MS, MAX_BDP_FRAME_BYTES, MAX_IN_GRACE_LEASES, MAX_RAW_OUTPUT_CREDIT,
    MAX_SESSION_LEASES_PER_CONN, MAX_STREAMS_PER_CONN, MIN_BDP_FRAME_BYTES,
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
        first_active_line: 0,
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
    broker.open_connection(cid, [0x22; 32], now).unwrap();
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

    let mut fired = Vec::new();
    wheel.poll_expired(ts(99), &mut fired);
    assert!(fired.is_empty());

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
    let other_secret = AuthSecret::new([0x43; 32]);
    assert_eq!(secret, AuthSecret::new([0x42; 32]));
    assert_ne!(secret, other_secret);
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
    assert!(!verify_server_proof(&secret, &c_nonce, &s_nonce, &c_proof));
    assert!(!verify_client_proof(&secret, &s_nonce, &c_nonce, &s_proof));

    let mut bad = c_proof;
    bad[31] ^= 1;
    assert!(!verify_client_proof(&secret, &c_nonce, &s_nonce, &bad));
    assert!(!ct_eq_32(&c_proof, &bad));
    assert!(ct_eq_32(&c_proof, &c_proof));
}

#[test]
fn frame_pacer_bdp_clamping_paused_unpause_ewma_and_ack_validation() {
    // Hidden view attached paused (max_frames = 0) can un-pause via FrameAck(seq=0) before any frame is sent
    let mut paused = FramePacer::new(FrameCredit { max_frames: 0, max_bytes: 100_000 });
    assert!(paused.is_paused());
    assert!(!paused.can_send(1));
    assert_eq!(paused.peek_next_seq(), 1);
    paused.on_ack(0, FrameCredit { max_frames: 2, max_bytes: 100_000 }, ts(50)).unwrap();
    assert!(!paused.is_paused());
    assert!(paused.can_send(1_000));
    assert_eq!(paused.ewma_rtt_ms(), None);

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
    assert_eq!(pacer.peek_next_seq(), 1);
    let s1 = pacer.on_send(30_000, ts(100));
    assert_eq!(s1, 1);
    assert_eq!(pacer.peek_next_seq(), 2);
    assert_eq!(pacer.in_flight_frames(), 1);
    assert_eq!(pacer.in_flight_bytes(), 30_000);

    // Second frame fits exact remaining budget (10_000), +1 exceeds
    assert!(pacer.can_send(10_000));
    assert!(!pacer.can_send(10_001));
    let s2 = pacer.on_send(10_000, ts(110));
    assert_eq!(s2, 2);
    assert_eq!(pacer.in_flight_bytes(), 40_000);
    assert!(!pacer.can_send(1));

    // Invalid acks rejected (seq == 0 rejected once frames have been sent; seq > last_sent rejected)
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

    // Re-acking same seq (credit update) is allowed
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
fn raw_output_flow_utf8_splits_chunking_debt_backpressure_and_best_effort() {
    // Credit clamping
    let clamped = RawOutputFlow::new(u32::MAX, false);
    assert_eq!(clamped.credit_bytes(), MAX_RAW_OUTPUT_CREDIT);

    // Best-effort mode: 1-byte split of 4-byte emoji + UTF-8 debt forward progress (never deadlocks or drops on 1..3 credit)
    let mut best = RawOutputFlow::with_limits(6, false, 64, 16);
    let rocket = "🚀".as_bytes(); // 4 bytes
    assert!(best.ingest(&[], ts(0)).emit_chunks.is_empty());
    for b in &rocket[..3] {
        let d = best.ingest(std::slice::from_ref(b), ts(1));
        assert!(d.emit_chunks.is_empty());
        assert_eq!(d.dropped_bytes, 0);
    }
    let d = best.ingest(&rocket[3..], ts(2));
    assert_eq!(d.emit_chunks, vec!["🚀"]);
    assert_eq!(best.credit_bytes(), 2);
    assert_eq!(best.credit_debt(), 0);

    // With 2 bytes credit left, invalid UTF-8 byte -> U+FFFD (3 bytes) makes forward progress with debt=1, drops 'a'
    let d = best.ingest(&[0xFF, b'a'], ts(3));
    assert_eq!(d.emit_chunks, vec!["\u{FFFD}"]);
    assert_eq!(d.dropped_bytes, 1);
    assert_eq!(best.credit_bytes(), 0);
    assert_eq!(best.credit_debt(), 1);
    assert!(!best.is_pty_paused());

    // Granting 3 bytes pays 1 debt and leaves 2 credit; 3-byte input emits 2 bytes and drops 1
    let _ = best.grant_credit(3, ts(4));
    assert_eq!(best.credit_debt(), 0);
    assert_eq!(best.credit_bytes(), 2);
    let d = best.ingest(b"xyz", ts(5));
    assert_eq!(d.emit_chunks, vec!["xy"]);
    assert_eq!(d.dropped_bytes, 1);
    assert_eq!(best.credit_bytes(), 0);

    // Backpressure mode: drains directly against credit before buffering, so max_pending < credit never truncates available credit!
    let mut direct = RawOutputFlow::with_limits(20, true, 6, 8);
    let d_dir = direct.ingest(b"0123456789abcdefghij", ts(10));
    assert_eq!(d_dir.emit_chunks, vec!["01234567", "89abcdef", "ghij"]);
    assert_eq!(d_dir.dropped_bytes, 0);
    assert_eq!(direct.credit_bytes(), 0);
    assert!(d_dir.pty_paused);

    // Backpressure UTF-8 debt forward progress: credit=1 with 4-byte emoji emits emoji (debt=3, not 4/1=4) and pauses
    let mut bp_debt = RawOutputFlow::with_limits(1, true, 32, 16);
    let d_em = bp_debt.ingest("🚀!".as_bytes(), ts(20));
    assert_eq!(d_em.emit_chunks, vec!["🚀"]);
    assert_eq!(bp_debt.credit_bytes(), 0);
    assert_eq!(bp_debt.credit_debt(), 3);
    assert!(d_em.pty_paused);
    // Grant 3 bytes: only pays debt -> stays paused, emits nothing
    let d_pay = bp_debt.grant_credit(3, ts(25));
    assert!(d_pay.emit_chunks.is_empty());
    assert_eq!(bp_debt.credit_debt(), 0);
    assert_eq!(bp_debt.credit_bytes(), 0);
    assert!(d_pay.pty_paused);
    // Grant 4 bytes: drains "!" and unpauses with waited_us = (30 - 20) * 1000 = 10_000
    let d_fin = bp_debt.grant_credit(4, ts(30));
    assert_eq!(d_fin.emit_chunks, vec!["!"]);
    assert_eq!(d_fin.throttled_us, Some(10_000));
    assert!(!d_fin.pty_paused);
    assert_eq!(bp_debt.credit_bytes(), 3);

    // Backpressure pending cap, chunking, and throttled timing
    let mut bp = RawOutputFlow::with_limits(3, true, 6, 4);
    let d0 = bp.ingest(b"a", ts(90));
    assert_eq!(d0.emit_chunks, vec!["a"]);
    assert!(!d0.pty_paused);

    let d_exact = bp.ingest(b"bc", ts(95));
    assert_eq!(d_exact.emit_chunks, vec!["bc"]);
    assert!(d_exact.pty_paused);
    assert_eq!(bp.credit_bytes(), 0);
    let d_zero = bp.grant_credit(0, ts(96));
    assert!(d_zero.emit_chunks.is_empty());
    assert_eq!(d_zero.throttled_us, None);
    assert!(d_zero.pty_paused);
    let d_unpause = bp.grant_credit(3, ts(98));
    assert!(d_unpause.emit_chunks.is_empty());
    assert_eq!(d_unpause.throttled_us, Some(3_000));
    assert!(!d_unpause.pty_paused);

    let d1 = bp.ingest(b"hello", ts(100));
    assert_eq!(d1.emit_chunks, vec!["hel"]);
    assert_eq!(d1.dropped_bytes, 0);
    assert!(d1.pty_paused);

    // Second ingest while throttled does not overwrite throttled_since, and enforces max_pending=6
    let d_cap = bp.ingest(b"123456", ts(110));
    assert!(d_cap.emit_chunks.is_empty());
    assert_eq!(d_cap.dropped_bytes, 2); // pending had "lo" (2), kept "1234", dropped "56"

    assert!(bp.grant_credit(0, ts(112)).emit_chunks.is_empty());
    let d2 = bp.grant_credit(1, ts(115));
    assert_eq!(d2.emit_chunks, vec!["l"]);
    assert_eq!(d2.throttled_us, None);
    assert!(d2.pty_paused);

    // Remaining credit drains "o123" + "4" in 4-byte chunks and unpauses with waited_us = 25_000
    let d3 = bp.grant_credit(10, ts(125));
    assert_eq!(d3.emit_chunks, vec!["o123", "4"]);
    assert_eq!(d3.throttled_us, Some(25_000));
    assert!(!d3.pty_paused);
    assert_eq!(bp.total_throttled_us(), 28_000);
    assert_eq!(bp.credit_bytes(), 5);

    // Large 1.5 MiB output chunks cleanly to <= BULK_CHUNK_BYTES and round-trips through BrokerMsg::decode!
    let mut big = RawOutputFlow::new(2_000_000, false);
    let payload = vec![b'x'; 1_500_000];
    let d_big = big.ingest(&payload, ts(200));
    assert_eq!(d_big.dropped_bytes, 0);
    assert!(d_big.emit_chunks.len() > 20);
    for chunk in &d_big.emit_chunks {
        assert!(chunk.len() <= BULK_CHUNK_BYTES);
        let msg = BrokerMsg::Output { text: chunk.clone() };
        let mut w = Writer::new();
        msg.encode(&mut w);
        assert_eq!(BrokerMsg::decode(msg.msg_type(), w.as_slice()), Ok(msg));
    }
}

#[test]
fn outbound_queue_priority_frame_caps_wfq_and_stream_removal() {
    let mut q = OutboundQueue::new(150);
    assert!(q.is_empty());
    assert_eq!(q.ordered_bytes(), 0);
    assert_eq!(q.frame_bytes(), 0);
    assert_eq!(q.total_bytes(), 0);
    assert_eq!(q.pending_frames(), 0);

    let s1 = StreamId::from_raw(1);
    let s2 = StreamId::from_raw(2);
    assert!(!q.has_pending_frame(s1));

    // Verify is_empty() individually for each of the 5 tiers
    q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::PUSH, BrokerMsg::Ok, 1).unwrap();
    assert!(!q.is_empty());
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());

    q.push_frame(s1, sample_frame(), 1).unwrap();
    assert!(!q.is_empty());
    assert!(q.has_pending_frame(s1));
    assert_eq!(q.frame_bytes(), 1);
    assert!(q.pop_next().is_some());
    assert!(q.is_empty());
    assert_eq!(q.frame_bytes(), 0);

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

    // Frame replacement accounts delta cleanly and enforces max_total_bytes (150)
    let mut f1 = sample_frame();
    f1.seq = 1;
    let mut f2 = sample_frame();
    f2.seq = 2;
    q.push_frame(s1, f1, 40).unwrap();
    q.push_frame(s1, f2, 50).unwrap();
    assert_eq!(q.pending_frames(), 1);
    assert_eq!(q.frame_bytes(), 50);
    assert_eq!(q.ordered_bytes(), 80);
    assert_eq!(q.total_bytes(), 130);

    // Frame exact boundary (130 + 20 == 150) succeeds, then remove s2 frame; +21 overflows
    assert_eq!(q.push_frame(s2, sample_frame(), 21), Err(QueueOverflow));
    q.push_frame(s2, sample_frame(), 20).unwrap();
    assert_eq!(q.total_bytes(), 150);
    q.remove_stream_frames(s2);
    assert_eq!(q.total_bytes(), 130);
    // Ordered overflow rejected (130 + 21 > 150); exact 20 fits
    assert_eq!(
        q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::PUSH, BrokerMsg::Ok, 21),
        Err(QueueOverflow)
    );
    q.push_ordered(OutboundPriority::Event, s2, RequestId::PUSH, BrokerMsg::Bell, 20).unwrap();
    assert_eq!(q.total_bytes(), 150);

    // Pop order: Control -> Frame(seq=2) -> Event(s1) -> Event(s2) -> Bulk WFQ(s1, s2, s1) -> Raw WFQ(s1, s2, s1)
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Ok);
    match q.pop_next().unwrap().msg {
        BrokerMsg::Frame(f) => assert_eq!(f.seq, 2),
        other => panic!("expected Frame(seq=2), got {other:?}"),
    }
    assert_eq!(q.frame_bytes(), 0);
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
    assert_eq!(q.total_bytes(), 0);
    assert_eq!(q.pop_next(), None);

    // remove_stream cleans s1 across all tiers while preserving s2 in every tier
    q.push_frame(s1, sample_frame(), 20).unwrap();
    q.push_frame(s2, sample_frame(), 20).unwrap();
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
    assert_eq!(q.frame_bytes(), 20);
    assert_eq!(q.ordered_bytes(), 20);
    assert_eq!(q.pop_next().unwrap().stream_id, s2); // frame
    assert_eq!(q.pop_next().unwrap().stream_id, s2); // event
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::TextReply { text: "keep_bulk".into() });
    assert_eq!(q.pop_next().unwrap().msg, BrokerMsg::Output { text: "keep_raw".into() });
    assert!(q.is_empty());
}

#[test]
fn lease_table_caps_immediate_grace_resume_stale_timer_and_session_cleanup() {
    assert!(ConnId::new(0).is_none());
    assert_eq!(ConnId::new(3).unwrap().get(), 3);
    assert!(LeaseId::new(0).is_none());
    assert_eq!(LeaseId::new(5).unwrap().get(), 5);

    let mut table = LeaseTable::default();
    let mut wheel = TimerWheel::<LeaseId>::new();
    assert!(!table.has_active_broker_lease());
    assert_eq!(table.in_grace_count(), 0);

    // Disconnecting a connection with no broker lease does NOT trigger teardown
    assert!(!table.on_disconnect(conn(99), ts(5), |id, at| wheel.schedule(at, id)).teardown_non_kept);
    let dummy_timer = wheel.schedule(ts(1), LeaseId::new(1).unwrap());
    assert!(!table.on_grace_expired(LeaseId::new(999).unwrap(), dummy_timer).teardown_non_kept);
    wheel.cancel(dummy_timer);

    // A single connection can hold at most ONE broker lease
    let l1 = table.acquire(conn(1), LeaseKind::BrokerImmediate).unwrap();
    assert_eq!(table.acquire(conn(1), LeaseKind::BrokerImmediate), Err(LeaseError::LimitExceeded));
    let _l2 = table.acquire(conn(2), LeaseKind::BrokerImmediate).unwrap();
    assert!(!l1.resumed);
    assert!(table.has_active_broker_lease());

    // Wrong owner cannot release; disconnecting conn(1) leaves conn(2) alive
    assert_eq!(table.release(conn(2), l1.lease_id), Err(LeaseError::NotOwned));
    assert!(!table.on_disconnect(conn(1), ts(8), |id, at| wheel.schedule(at, id)).teardown_non_kept);
    assert!(table.has_active_broker_lease());
    assert!(table.on_disconnect(conn(2), ts(10), |id, at| wheel.schedule(at, id)).teardown_non_kept);
    assert!(!table.has_active_broker_lease());

    // Expiring grace with wrong TimerId is ignored; matching TimerId expires it
    let tok_a = ResumeToken::new([0x66; 32]);
    let grace = LeaseGraceMs::new(30_000).unwrap();
    let la = table.acquire(conn(20), LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok_a }).unwrap();
    let lb = table.acquire(conn(21), LeaseKind::BrokerImmediate).unwrap();
    let mut grace_timer = None;
    let _ = table.on_disconnect(conn(20), ts(50), |id, at| {
        let tid = wheel.schedule(at, id);
        grace_timer = Some(tid);
        tid
    });
    let wrong_tid = wheel.schedule(ts(60), la.lease_id);
    assert!(!table.on_grace_expired(la.lease_id, wrong_tid).teardown_non_kept);
    assert_eq!(table.in_grace_count(), 1);
    assert!(!table.on_grace_expired(la.lease_id, grace_timer.unwrap()).teardown_non_kept);
    assert_eq!(table.in_grace_count(), 0);
    assert!(table.release(conn(21), lb.lease_id).unwrap().teardown_non_kept);

    // Remote BrokerGrace: TokenInUse blocks live duplicate; resume decrements in_grace_count
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

    let resumed = table.acquire(conn(4), LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok }).unwrap();
    assert!(resumed.resumed);
    assert_eq!(resumed.lease_id, lg.lease_id);
    assert_eq!(table.in_grace_count(), 0);
    wheel.cancel(resumed.cancelled_timer.unwrap());

    let mut t2 = None;
    let _ = table.on_disconnect(conn(4), ts(2_000), |id, at| {
        let tid = wheel.schedule(at, id);
        t2 = Some(tid);
        tid
    });
    assert!(table.on_grace_expired(lg.lease_id, t2.unwrap()).teardown_non_kept);
    assert_eq!(table.in_grace_count(), 0);

    // Two broker leases: releasing one leaves the other active
    let r1 = table.acquire(conn(11), LeaseKind::BrokerImmediate).unwrap();
    let r2 = table.acquire(conn(12), LeaseKind::BrokerImmediate).unwrap();
    assert!(!table.release(conn(11), r1.lease_id).unwrap().teardown_non_kept);
    assert!(table.release(conn(12), r2.lease_id).unwrap().teardown_non_kept);

    // Session lease pin / unpin via release, disconnect, remove_session_leases, and per-conn cap
    let ls = table.acquire(conn(5), LeaseKind::Session(sid(9))).unwrap();
    assert!(table.is_session_pinned(sid(9)));
    assert_eq!(table.release(conn(5), ls.lease_id).unwrap().unpinned_sessions, vec![sid(9)]);
    assert!(!table.is_session_pinned(sid(9)));

    let _s1 = table.acquire(conn(6), LeaseKind::Session(sid(10))).unwrap();
    let _s2 = table.acquire(conn(7), LeaseKind::Session(sid(10))).unwrap();
    assert!(table.on_disconnect(conn(6), ts(3_000), |id, at| wheel.schedule(at, id)).unpinned_sessions.is_empty());
    assert!(table.is_session_pinned(sid(10)));
    assert_eq!(
        table.on_disconnect(conn(7), ts(3_001), |id, at| wheel.schedule(at, id)).unpinned_sessions,
        vec![sid(10)]
    );

    let _s_rm = table.acquire(conn(8), LeaseKind::Session(sid(11))).unwrap();
    assert!(table.is_session_pinned(sid(11)));
    table.remove_session_leases(sid(11));
    assert!(!table.is_session_pinned(sid(11)));

    // Per-connection session lease cap (independent of other connections' session leases)
    let _other_conn_sess = table.acquire(conn(49), LeaseKind::Session(sid(888))).unwrap();
    for i in 1..=(MAX_SESSION_LEASES_PER_CONN as u64) {
        table.acquire(conn(50), LeaseKind::Session(sid(i))).unwrap();
    }
    assert_eq!(table.acquire(conn(50), LeaseKind::Session(sid(999))), Err(LeaseError::LimitExceeded));

    // Global in-grace cap across separate connections
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
fn broker_handshake_mutual_auth_timeout_duplicate_conn_and_framing_errors() {
    let secret = AuthSecret::new([0x99; 32]);
    let mut broker = BrokerCore::new(AuthConfig::Required(secret.clone()));
    let mut actions = Vec::new();

    // 1. Handshake timeout + duplicate ConnId refusal
    broker.open_connection(conn(1), [0x10; 32], ts(1_000)).unwrap();
    assert_eq!(broker.open_connection(conn(1), [0x11; 32], ts(1_001)), Err(DuplicateConnId));
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

    // 2. Non-control stream and non-Hello in AwaitingHello rejected
    actions.clear();
    broker.open_connection(conn(10), [0x10; 32], ts(1_500)).unwrap();
    let bad_stream_hello = encode_client(
        StreamId::from_raw(1),
        1,
        &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION, client_nonce: [0; 32], client_name: "x".into() },
    );
    broker.feed_bytes(conn(10), &bad_stream_hello, ts(1_501), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    actions.clear();
    broker.open_connection(conn(11), [0x10; 32], ts(1_600)).unwrap();
    broker.feed_bytes(conn(11), &encode_client(StreamId::CONTROL, 1, &ClientMsg::Metrics), ts(1_601), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    // 3. Successful mutual HMAC handshake
    actions.clear();
    let c_nonce = [0xAA; 32];
    let s_nonce = [0xBB; 32];
    broker.open_connection(conn(2), s_nonce, ts(2_000)).unwrap();
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
    assert!(!broker.is_connection_ready(conn(2)));
    let auth_frame = encode_client(StreamId::CONTROL, 2, &ClientMsg::AuthProof { hmac: proof });
    broker.feed_bytes(conn(2), &auth_frame, ts(2_020), &mut actions);
    assert!(broker.is_connection_ready(conn(2)));
    let auth_ok = broker.pop_outbound(conn(2)).unwrap();
    match auth_ok.msg {
        BrokerMsg::AuthOk { server_hmac } => {
            assert!(verify_server_proof(&secret, &s_nonce, &c_nonce, &server_hmac));
        }
        other => panic!("expected AuthOk, got {other:?}"),
    }
    assert_eq!(broker.next_deadline(), None);

    // Duplicate Hello when Ready fails and closes
    actions.clear();
    broker.feed_bytes(conn(2), &hello, ts(2_030), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    // 4. AwaitingAuth rejections: non-control stream, unexpected msg, bad HMAC
    for (cid, bad_frame) in [
        (conn(12), encode_client(StreamId::from_raw(1), 2, &ClientMsg::AuthProof { hmac: proof })),
        (conn(13), encode_client(StreamId::CONTROL, 2, &ClientMsg::Metrics)),
        (conn(3), encode_client(StreamId::CONTROL, 2, &ClientMsg::AuthProof { hmac: [0; 32] })),
    ] {
        actions.clear();
        broker.open_connection(cid, s_nonce, ts(3_000)).unwrap();
        broker.feed_bytes(cid, &hello, ts(3_001), &mut actions);
        let _ = broker.pop_outbound(cid);
        broker.feed_bytes(cid, &bad_frame, ts(3_002), &mut actions);
        assert!(matches!(
            actions.last(),
            Some(BrokerAction::CloseConn {
                reason: CloseReason::Unauthorized,
                final_error: Some((_, _, BrokerMsg::Error { code: ErrorCode::Unauthorized, .. })),
                ..
            })
        ));
    }

    // 5. Wrong protocol version, FIN, invalid frame length, and malformed client message payload
    actions.clear();
    broker.open_connection(conn(4), s_nonce, ts(4_000)).unwrap();
    let bad_ver = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::Hello { protocol_version: 99, client_nonce: c_nonce, client_name: "old".into() },
    );
    broker.feed_bytes(conn(4), &bad_ver, ts(4_001), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    actions.clear();
    broker.open_connection(conn(5), s_nonce, ts(5_000)).unwrap();
    let mut fin = Vec::new();
    encode_fin(&mut fin);
    broker.feed_bytes(conn(5), &fin, ts(5_001), &mut actions);
    assert!(matches!(
        actions.last(),
        Some(BrokerAction::CloseConn { reason: CloseReason::ClientFin, final_error: None, .. })
    ));

    actions.clear();
    broker.open_connection(conn(6), s_nonce, ts(6_000)).unwrap();
    broker.feed_bytes(conn(6), &5u32.to_le_bytes(), ts(6_001), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));

    actions.clear();
    broker.open_connection(conn(7), s_nonce, ts(7_000)).unwrap();
    let mut bad_msg_frame = Vec::new();
    apack_proto::frame::encode_frame(StreamId::CONTROL, 0xFFFF, RequestId::from_raw(1), &[], &mut bad_msg_frame)
        .unwrap();
    broker.feed_bytes(conn(7), &bad_msg_frame, ts(7_001), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));
}

#[test]
fn broker_multiplexing_frame_pacing_exit_ordering_and_slow_reader() {
    let mut broker = BrokerCore::with_queue_cap(AuthConfig::None, 512);
    handshake_ready(&mut broker, conn(1), ts(100));
    broker.register_session(sid(1), false, false);
    broker.register_session(sid(2), true, false);
    broker.update_shard_telemetry(ShardTelemetry {
        scrollback_bytes: 1_000,
        image_store_bytes: 2_000,
        total_pty_bytes: 3_000,
    });

    let mut actions = Vec::new();

    // Control messages + NoSuchSession checks + stream message on control stream
    let mut batch = Vec::new();
    batch.extend(encode_client(StreamId::CONTROL, 10, &ClientMsg::Spawn(sample_spawn())));
    batch.extend(encode_client(StreamId::CONTROL, 11, &ClientMsg::Kill { session: sid(1) }));
    batch.extend(encode_client(StreamId::CONTROL, 12, &ClientMsg::Kill { session: sid(99) }));
    batch.extend(encode_client(
        StreamId::CONTROL,
        13,
        &ClientMsg::Signal { session: sid(1), signal: SignalKind::Int, scope: SignalScope::Group },
    ));
    batch.extend(encode_client(
        StreamId::CONTROL,
        14,
        &ClientMsg::Signal { session: sid(99), signal: SignalKind::Int, scope: SignalScope::Group },
    ));
    batch.extend(encode_client(
        StreamId::CONTROL,
        15,
        &ClientMsg::Owns { session: sid(1), pid: SignalTarget::new(42).unwrap() },
    ));
    batch.extend(encode_client(
        StreamId::CONTROL,
        16,
        &ClientMsg::Owns { session: sid(99), pid: SignalTarget::new(42).unwrap() },
    ));
    batch.extend(encode_client(StreamId::CONTROL, 17, &ClientMsg::ListSessions));
    batch.extend(encode_client(StreamId::CONTROL, 18, &ClientMsg::Rewind { record_dir: "/tmp/r".into(), at_ms: None }));
    batch.extend(encode_client(StreamId::CONTROL, 19, &ClientMsg::Seek { session: sid(1), at_ms: 50 }));
    batch.extend(encode_client(StreamId::CONTROL, 20, &ClientMsg::Seek { session: sid(99), at_ms: 50 }));
    batch.extend(encode_client(StreamId::CONTROL, 21, &ClientMsg::Seek { session: sid(2), at_ms: 50 }));
    batch.extend(encode_client(StreamId::CONTROL, 22, &ClientMsg::Keep { session: sid(1), kept: true }));
    batch.extend(encode_client(StreamId::CONTROL, 23, &ClientMsg::Keep { session: sid(99), kept: true }));
    batch.extend(encode_client(
        StreamId::CONTROL,
        24,
        &ClientMsg::Wait { session: sid(1), condition: WaitCondition::Idle, timeout_ms: 500 },
    ));
    batch.extend(encode_client(
        StreamId::CONTROL,
        25,
        &ClientMsg::Wait { session: sid(99), condition: WaitCondition::Idle, timeout_ms: 500 },
    ));
    batch.extend(encode_client(StreamId::CONTROL, 26, &ClientMsg::Pause)); // stream msg on control -> Malformed
    batch.extend(encode_client(StreamId::CONTROL, 27, &ClientMsg::Metrics));
    broker.feed_bytes(conn(1), &batch, ts(110), &mut actions);

    assert!(broker.is_session_kept(sid(1)));
    assert!(!broker.is_session_kept(sid(2)));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SpawnSession { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::KillSession { session, .. } if *session == sid(1))));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SignalSession { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::QueryOwns { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::ListSessions { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::RewindRecording { .. })));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::SeekRecording { session, .. } if *session == sid(2))));
    assert!(actions.iter().any(|a| matches!(a, BrokerAction::WaitSession { .. })));

    // Drain expected errors and MetricsReply
    let expected_errors = [
        (12, ErrorCode::NoSuchSession),
        (14, ErrorCode::NoSuchSession),
        (16, ErrorCode::NoSuchSession),
        (19, ErrorCode::Malformed),
        (20, ErrorCode::NoSuchSession),
        (23, ErrorCode::NoSuchSession),
        (25, ErrorCode::NoSuchSession),
        (26, ErrorCode::Malformed),
    ];
    for (req, code) in expected_errors {
        let m = broker.pop_outbound(conn(1)).unwrap();
        assert_eq!(m.req_id, RequestId::from_raw(req));
        assert!(matches!(m.msg, BrokerMsg::Error { code: c, .. } if c == code));
    }
    let metrics = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(metrics.req_id, RequestId::from_raw(27));
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

    // Attach stream 1 with max_frames=2 (tests non-conflating pending guard in publish_frame!)
    actions.clear();
    let attach = encode_client(
        StreamId::CONTROL,
        30,
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
            initial_frame_credit: FrameCredit { max_frames: 2, max_bytes: 65_536 },
        },
    );
    broker.feed_bytes(conn(1), &attach, ts(120), &mut actions);

    // ViewportHint succeeds on SUB_VIEW stream
    actions.clear();
    let ok_hint = encode_client(
        nz_stream(1).as_stream_id(),
        31,
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
        if *req_id == RequestId::from_raw(31)
    )));

    // Unattached stream returns NoSuchStream; Attach to nonexistent session returns NoSuchSession
    broker.feed_bytes(conn(1), &encode_client(StreamId::from_raw(77), 32, &ClientMsg::Pause), ts(126), &mut actions);
    assert!(matches!(
        broker.pop_outbound(conn(1)).unwrap().msg,
        BrokerMsg::Error { code: ErrorCode::NoSuchStream, .. }
    ));
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            33,
            &ClientMsg::Attach {
                session: sid(99),
                stream_id: nz_stream(99),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(126),
        &mut actions,
    );
    assert!(matches!(
        broker.pop_outbound(conn(1)).unwrap().msg,
        BrokerMsg::Error { code: ErrorCode::NoSuchSession, .. }
    ));

    // Best-effort RAW_OUTPUT on sid(2) + VIEW-only on sid(1)
    let attach_best = encode_client(
        StreamId::CONTROL,
        34,
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
        35,
        &ClientMsg::Attach {
            session: sid(1),
            stream_id: nz_stream(3),
            flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
            initial_viewport: None,
            initial_output_credit: 0,
            initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
        },
    );
    broker.feed_bytes(conn(1), &attach_best, ts(127), &mut actions);
    broker.feed_bytes(conn(1), &attach_view_only, ts(128), &mut actions);

    actions.clear();
    broker.publish_raw_output(sid(2), b"12345", ts(129), &mut actions);
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "12".into() });
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::OutputDropped { bytes: 3 });

    // Even with max_frames=2, publish_frame refuses a 2nd frame while the 1st is still unsent in OutboundQueue!
    actions.clear();
    assert!(broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(130), &mut actions));
    assert!(!broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(131), &mut actions));
    let f1 = broker.pop_outbound(conn(1)).unwrap();
    assert!(matches!(f1.msg, BrokerMsg::Frame(ref fr) if fr.seq == 1));
    // Once popped onto wire, 2nd in-flight frame IS accepted (seq=2), and 3rd is blocked by max_frames=2!
    assert!(broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(132), &mut actions));
    let f2 = broker.pop_outbound(conn(1)).unwrap();
    assert!(matches!(f2.msg, BrokerMsg::Frame(ref fr) if fr.seq == 2));
    assert!(!broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(133), &mut actions));

    // Invalid FrameAck (seq=99) rejected; valid FrameAck(seq=2) updates RTT = 152 - 132 = 20ms
    broker.feed_bytes(
        conn(1),
        &encode_client(
            nz_stream(1).as_stream_id(),
            36,
            &ClientMsg::FrameAck { seq: 99, credit: FrameCredit { max_frames: 2, max_bytes: 65_536 } },
        ),
        ts(150),
        &mut actions,
    );
    assert!(matches!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));
    let ack = encode_client(
        nz_stream(1).as_stream_id(),
        0,
        &ClientMsg::FrameAck { seq: 2, credit: FrameCredit { max_frames: 2, max_bytes: 65_536 } },
    );
    broker.feed_bytes(conn(1), &ack, ts(152), &mut actions);
    assert_eq!(broker.stream_rtt_ms(conn(1), nz_stream(1)), Some(20));

    // Raw output with BACKPRESSURE_PTY pauses PTY, resumes on OutputCredit
    actions.clear();
    broker.publish_raw_output(sid(1), b"abcdef", ts(200), &mut actions);
    assert!(
        actions.iter().any(|a| matches!(a, BrokerAction::SetPtyPaused { session, paused: true } if *session == sid(1)))
    );
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "abcd".into() });

    actions.clear();
    broker.feed_bytes(
        conn(1),
        &encode_client(nz_stream(1).as_stream_id(), 0, &ClientMsg::OutputCredit { bytes: 10 }),
        ts(215),
        &mut actions,
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, BrokerAction::SetPtyPaused { session, paused: false } if *session == sid(1)))
    );
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "ef".into() });
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Throttled { waited_us: 15_000 });
    assert_eq!(broker.metrics_snapshot().total_throttled_us, 15_000);

    // Session exit ordering: queued Exited + final Output MUST drain BEFORE StreamClosed (unsent frames dropped!)
    actions.clear();
    assert!(broker.publish_frame(conn(1), nz_stream(1), sample_frame(), ts(218), &mut actions));
    broker.publish_raw_output(sid(1), b"bye!", ts(219), &mut actions);
    broker.enqueue_message(
        conn(1),
        nz_stream(1).as_stream_id(),
        RequestId::PUSH,
        OutboundPriority::Event,
        BrokerMsg::Exited { exit_code: Some(0), signal: None },
        ts(219),
        &mut actions,
    );
    broker.unregister_session(sid(1), "exited", ts(220), &mut actions);

    // Expect: Exited (Event) -> Output("bye!") (RawOutput s1) -> StreamClosed (RawOutput s3) -> StreamClosed (RawOutput s1)
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Exited { exit_code: Some(0), signal: None });
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "bye!".into() });
    let c_a = broker.pop_outbound(conn(1)).unwrap();
    let c_b = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(c_a.msg, BrokerMsg::StreamClosed { reason: "exited".into() });
    assert_eq!(c_b.msg, BrokerMsg::StreamClosed { reason: "exited".into() });
    assert_eq!(broker.pop_outbound(conn(1)), None);

    // Slow reader hitting 512-byte queue cap is closed cleanly with CloseReason::SlowReader
    actions.clear();
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::PUSH,
        OutboundPriority::Event,
        BrokerMsg::Title { title: "x".repeat(600) },
        ts(230),
        &mut actions,
    );
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::SlowReader, .. })));
    assert_eq!(broker.connection_count(), 0);
}

#[test]
fn broker_leases_grace_expiry_multi_stream_backpressure_and_limits() {
    let mut broker = BrokerCore::new(AuthConfig::None);
    handshake_ready(&mut broker, conn(1), ts(10));
    handshake_ready(&mut broker, conn(2), ts(10));
    broker.register_session(sid(1), false, false);
    let mut actions = Vec::new();

    // AcquireLease for non-existent SessionId returns NoSuchSession
    broker.feed_bytes(
        conn(1),
        &encode_client(StreamId::CONTROL, 1, &ClientMsg::AcquireLease { kind: LeaseKind::Session(sid(99)) }),
        ts(15),
        &mut actions,
    );
    assert!(matches!(
        broker.pop_outbound(conn(1)).unwrap().msg,
        BrokerMsg::Error { code: ErrorCode::NoSuchSession, .. }
    ));

    // Acquire BrokerGrace lease, check TokenInUse from conn(2) and LimitExceeded from conn(1)
    let tok = ResumeToken::new([0x33; 32]);
    let grace = LeaseGraceMs::new(5_000).unwrap();
    let acq = encode_client(
        StreamId::CONTROL,
        2,
        &ClientMsg::AcquireLease { kind: LeaseKind::BrokerGrace { grace_ms: grace, resume_token: tok } },
    );
    broker.feed_bytes(conn(1), &acq, ts(20), &mut actions);
    let lease_id = match broker.pop_outbound(conn(1)).unwrap().msg {
        BrokerMsg::LeaseAck { lease_id, resumed: false } => lease_id,
        other => panic!("expected LeaseAck, got {other:?}"),
    };
    assert!(broker.has_active_broker_lease());

    broker.feed_bytes(conn(1), &acq, ts(21), &mut actions);
    assert!(matches!(
        broker.pop_outbound(conn(1)).unwrap().msg,
        BrokerMsg::Error { code: ErrorCode::ResourceLimit, .. }
    ));
    broker.feed_bytes(conn(2), &acq, ts(22), &mut actions);
    assert!(matches!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Busy, .. }));

    // Invalid ReleaseLease (0 and non-owned)
    broker.feed_bytes(
        conn(1),
        &encode_client(StreamId::CONTROL, 3, &ClientMsg::ReleaseLease { lease_id: 0 }),
        ts(23),
        &mut actions,
    );
    assert!(matches!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));
    broker.feed_bytes(
        conn(2),
        &encode_client(StreamId::CONTROL, 4, &ClientMsg::ReleaseLease { lease_id }),
        ts(24),
        &mut actions,
    );
    assert!(matches!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));

    // Pin sid(1) from conn(2) -> is_session_pinned == true; release -> SessionUnpinned
    broker.feed_bytes(
        conn(2),
        &encode_client(StreamId::CONTROL, 5, &ClientMsg::AcquireLease { kind: LeaseKind::Session(sid(1)) }),
        ts(25),
        &mut actions,
    );
    let sess_lid = match broker.pop_outbound(conn(2)).unwrap().msg {
        BrokerMsg::LeaseAck { lease_id, resumed: false } => lease_id,
        other => panic!("expected LeaseAck, got {other:?}"),
    };
    assert!(broker.is_session_pinned(sid(1)));
    actions.clear();
    broker.feed_bytes(
        conn(2),
        &encode_client(StreamId::CONTROL, 6, &ClientMsg::ReleaseLease { lease_id: sess_lid }),
        ts(26),
        &mut actions,
    );
    assert!(actions.contains(&BrokerAction::SessionUnpinned(sid(1))));
    assert!(!broker.is_session_pinned(sid(1)));
    assert_eq!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::Ok);

    // Multi-stream BACKPRESSURE_PTY: 2 streams paused; detaching 1 keeps PTY paused until 2nd releases!
    for (cid, st_id) in [(conn(1), 10u32), (conn(2), 11u32)] {
        broker.feed_bytes(
            cid,
            &encode_client(
                StreamId::CONTROL,
                st_id,
                &ClientMsg::Attach {
                    session: sid(1),
                    stream_id: nz_stream(st_id),
                    flags: SubFlags::from_bits(SubFlags::RAW_OUTPUT | SubFlags::BACKPRESSURE_PTY).unwrap(),
                    initial_viewport: None,
                    initial_output_credit: 2,
                    initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
                },
            ),
            ts(27),
            &mut actions,
        );
    }
    actions.clear();
    broker.publish_raw_output(sid(1), b"hello", ts(28), &mut actions);
    assert!(actions.contains(&BrokerAction::SetPtyPaused { session: sid(1), paused: true }));
    let _ = broker.pop_outbound(conn(1));
    let _ = broker.pop_outbound(conn(2));

    // Detach conn(2)'s stream -> conn(1) is STILL paused, so NO SetPtyPaused(false) yet!
    actions.clear();
    broker.feed_bytes(
        conn(2),
        &encode_client(StreamId::CONTROL, 12, &ClientMsg::Detach { stream_id: nz_stream(11) }),
        ts(29),
        &mut actions,
    );
    assert!(!actions.contains(&BrokerAction::SetPtyPaused { session: sid(1), paused: false }));
    assert_eq!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::Ok);

    // Disconnect conn(1) -> detaches last paused stream (SetPtyPaused(false)) AND enters grace until ts(5_030)
    actions.clear();
    broker.close_connection(conn(1), CloseReason::TransportClosed, ts(30), &mut actions);
    assert!(actions.contains(&BrokerAction::SetPtyPaused { session: sid(1), paused: false }));
    assert!(!actions.contains(&BrokerAction::TeardownNonKeptSessions));
    assert_eq!(broker.next_deadline(), Some(ts(5_030)));

    // Resume on conn(2), disconnect again, then let grace timer EXPIRE via BrokerCore::poll_timers!
    broker.feed_bytes(conn(2), &acq, ts(1_000), &mut actions);
    assert_eq!(broker.pop_outbound(conn(2)).unwrap().msg, BrokerMsg::LeaseAck { lease_id, resumed: true });
    assert_eq!(broker.next_deadline(), None);

    actions.clear();
    broker.close_connection(conn(2), CloseReason::TransportClosed, ts(2_000), &mut actions);
    assert_eq!(broker.next_deadline(), Some(ts(7_000)));
    broker.poll_timers(ts(6_999), &mut actions);
    assert!(!actions.contains(&BrokerAction::TeardownNonKeptSessions));
    broker.poll_timers(ts(7_000), &mut actions);
    assert!(actions.contains(&BrokerAction::TeardownNonKeptSessions));
    assert!(!broker.has_active_broker_lease());

    // MAX_STREAMS_PER_CONN limit + capability rejections on EVENTS-only stream
    handshake_ready(&mut broker, conn(3), ts(8_000));
    for i in 1..=(MAX_STREAMS_PER_CONN as u32) {
        broker.feed_bytes(
            conn(3),
            &encode_client(
                StreamId::CONTROL,
                i,
                &ClientMsg::Attach {
                    session: sid(1),
                    stream_id: nz_stream(i),
                    flags: SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
                    initial_viewport: None,
                    initial_output_credit: 0,
                    initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
                },
            ),
            ts(8_001),
            &mut actions,
        );
    }
    // Duplicate stream id -> Malformed; 1025th stream -> ResourceLimit
    broker.feed_bytes(
        conn(3),
        &encode_client(
            StreamId::CONTROL,
            2000,
            &ClientMsg::Attach {
                session: sid(1),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(8_002),
        &mut actions,
    );
    assert!(matches!(broker.pop_outbound(conn(3)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));
    broker.feed_bytes(
        conn(3),
        &encode_client(
            StreamId::CONTROL,
            2001,
            &ClientMsg::Attach {
                session: sid(1),
                stream_id: nz_stream((MAX_STREAMS_PER_CONN as u32) + 1),
                flags: SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(8_003),
        &mut actions,
    );
    assert!(matches!(
        broker.pop_outbound(conn(3)).unwrap().msg,
        BrokerMsg::Error { code: ErrorCode::ResourceLimit, .. }
    ));

    // Disallowed stream ops without capabilities on stream 1 (EVENTS-only)
    for (req, msg) in [
        (3001, ClientMsg::FrameAck { seq: 1, credit: FrameCredit { max_frames: 1, max_bytes: 65_536 } }),
        (
            3002,
            ClientMsg::ViewportHint(ViewportHint {
                size: GridSize::new(80, 24).unwrap(),
                cell_px: CellPx::default(),
                visible: true,
                focused: true,
            }),
        ),
        (3003, ClientMsg::OutputCredit { bytes: 100 }),
        (3004, ClientMsg::ListSessions),
    ] {
        broker.feed_bytes(conn(3), &encode_client(nz_stream(1).as_stream_id(), req, &msg), ts(8_010), &mut actions);
        let m = broker.pop_outbound(conn(3)).unwrap();
        assert_eq!(m.req_id, RequestId::from_raw(req));
        assert!(matches!(m.msg, BrokerMsg::Error { code: ErrorCode::Malformed, .. }));
    }
    actions.clear();
    broker.feed_bytes(
        conn(3),
        &encode_client(nz_stream(1).as_stream_id(), 3005, &ClientMsg::Write { data: b"hi".to_vec() }),
        ts(8_011),
        &mut actions,
    );
    assert!(actions.iter().any(|a| matches!(
        a,
        BrokerAction::DispatchToSession { req_id, msg: ClientMsg::Write { .. }, .. }
        if *req_id == RequestId::from_raw(3005)
    )));
}
