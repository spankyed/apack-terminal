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
use crate::broker::{BrokerAction, BrokerCore, CloseReason, DuplicateConnId, ShardTelemetry, StreamAttachmentId};
use crate::flow::{FlowError, FramePacer, MemoryBudget, OutboundPriority, OutboundQueue, QueueOverflow, RawOutputFlow};
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

fn att(n: u64) -> StreamAttachmentId {
    StreamAttachmentId::new(n).unwrap()
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
    let mut budget = MemoryBudget::new(16 * 1024 * 1024);
    // Credit clamping
    let clamped = RawOutputFlow::new(u32::MAX, false);
    assert_eq!(clamped.credit_bytes(), MAX_RAW_OUTPUT_CREDIT);

    // Best-effort mode: 1-byte split of 4-byte emoji + UTF-8 debt forward progress (never deadlocks or drops on 1..3 credit)
    let mut best = RawOutputFlow::with_limits(6, false, 64, 16);
    let rocket = "🚀".as_bytes(); // 4 bytes
    assert!(best.ingest(&[], ts(0), &mut budget).emit_chunks.is_empty());
    for b in &rocket[..3] {
        let d = best.ingest(std::slice::from_ref(b), ts(1), &mut budget);
        assert!(d.emit_chunks.is_empty());
        assert_eq!(d.dropped_bytes, 0);
    }
    let d = best.ingest(&rocket[3..], ts(2), &mut budget);
    assert_eq!(d.emit_chunks, vec!["🚀"]);
    assert_eq!(best.credit_bytes(), 2);
    assert_eq!(best.credit_debt(), 0);

    // With 2 bytes credit left, invalid UTF-8 byte -> U+FFFD (3 bytes) makes forward progress with debt=1, drops 'a'
    let d = best.ingest(&[0xFF, b'a'], ts(3), &mut budget);
    assert_eq!(d.emit_chunks, vec!["\u{FFFD}"]);
    assert_eq!(d.dropped_bytes, 1);
    assert_eq!(best.credit_bytes(), 0);
    assert_eq!(best.credit_debt(), 1);
    assert!(!best.is_pty_paused());

    // Granting 3 bytes pays 1 debt and leaves 2 credit; 3-byte input emits 2 bytes and drops 1
    let _ = best.grant_credit(3, ts(4), &mut budget);
    assert_eq!(best.credit_debt(), 0);
    assert_eq!(best.credit_bytes(), 2);
    let d = best.ingest(b"xyz", ts(5), &mut budget);
    assert_eq!(d.emit_chunks, vec!["xy"]);
    assert_eq!(d.dropped_bytes, 1);
    assert_eq!(best.credit_bytes(), 0);

    // Backpressure mode: drains directly against credit before buffering, so max_pending < credit never truncates available credit!
    let mut direct = RawOutputFlow::with_limits(20, true, 6, 8);
    let d_dir = direct.ingest(b"0123456789abcdefghij", ts(10), &mut budget);
    assert_eq!(d_dir.emit_chunks, vec!["01234567", "89abcdef", "ghij"]);
    assert_eq!(d_dir.dropped_bytes, 0);
    assert_eq!(direct.credit_bytes(), 0);
    assert!(d_dir.pty_paused);

    // Backpressure UTF-8 debt forward progress: credit=1 with 4-byte emoji emits emoji (debt=3, not 4/1=4) and pauses
    let mut bp_debt = RawOutputFlow::with_limits(1, true, 32, 16);
    let d_em = bp_debt.ingest("🚀!".as_bytes(), ts(20), &mut budget);
    assert_eq!(d_em.emit_chunks, vec!["🚀"]);
    assert_eq!(bp_debt.credit_bytes(), 0);
    assert_eq!(bp_debt.credit_debt(), 3);
    assert!(d_em.pty_paused);
    assert_eq!(bp_debt.pending_bytes(), 1);
    assert_eq!(budget.used_bytes(), 1);
    // Grant 3 bytes: only pays debt -> stays paused, emits nothing
    let d_pay = bp_debt.grant_credit(3, ts(25), &mut budget);
    assert!(d_pay.emit_chunks.is_empty());
    assert_eq!(bp_debt.credit_debt(), 0);
    assert_eq!(bp_debt.credit_bytes(), 0);
    assert!(d_pay.pty_paused);
    // Grant 4 bytes: drains "!" and unpauses with waited_us = (30 - 20) * 1000 = 10_000
    let d_fin = bp_debt.grant_credit(4, ts(30), &mut budget);
    assert_eq!(d_fin.emit_chunks, vec!["!"]);
    assert_eq!(d_fin.throttled_us, Some(10_000));
    assert!(!d_fin.pty_paused);
    assert_eq!(bp_debt.credit_bytes(), 3);
    assert_eq!(bp_debt.pending_bytes(), 0);
    assert_eq!(budget.used_bytes(), 0);

    // Backpressure pending cap, chunking, and throttled timing
    let mut bp = RawOutputFlow::with_limits(3, true, 6, 4);
    let d0 = bp.ingest(b"a", ts(90), &mut budget);
    assert_eq!(d0.emit_chunks, vec!["a"]);
    assert!(!d0.pty_paused);

    let d_exact = bp.ingest(b"bc", ts(95), &mut budget);
    assert_eq!(d_exact.emit_chunks, vec!["bc"]);
    assert!(d_exact.pty_paused);
    assert_eq!(bp.credit_bytes(), 0);
    let d_zero = bp.grant_credit(0, ts(96), &mut budget);
    assert!(d_zero.emit_chunks.is_empty());
    assert_eq!(d_zero.throttled_us, None);
    assert!(d_zero.pty_paused);
    let d_unpause = bp.grant_credit(3, ts(98), &mut budget);
    assert!(d_unpause.emit_chunks.is_empty());
    assert_eq!(d_unpause.throttled_us, Some(3_000));
    assert!(!d_unpause.pty_paused);

    let d1 = bp.ingest(b"hello", ts(100), &mut budget);
    assert_eq!(d1.emit_chunks, vec!["hel"]);
    assert_eq!(d1.dropped_bytes, 0);
    assert!(d1.pty_paused);
    assert_eq!(budget.used_bytes(), 2);

    // Second ingest while throttled does not overwrite throttled_since, and enforces max_pending=6
    let d_cap = bp.ingest(b"123456", ts(110), &mut budget);
    assert!(d_cap.emit_chunks.is_empty());
    assert_eq!(d_cap.dropped_bytes, 2); // pending had "lo" (2), kept "1234", dropped "56"
    assert_eq!(budget.used_bytes(), 6);

    assert!(bp.grant_credit(0, ts(112), &mut budget).emit_chunks.is_empty());
    let d2 = bp.grant_credit(1, ts(115), &mut budget);
    assert_eq!(d2.emit_chunks, vec!["l"]);
    assert_eq!(d2.throttled_us, None);
    assert!(d2.pty_paused);
    assert_eq!(budget.used_bytes(), 5);

    // Remaining credit drains "o123" + "4" in 4-byte chunks and unpauses with waited_us = 25_000
    let d3 = bp.grant_credit(10, ts(125), &mut budget);
    assert_eq!(d3.emit_chunks, vec!["o123", "4"]);
    assert_eq!(d3.throttled_us, Some(25_000));
    assert!(!d3.pty_paused);
    assert_eq!(bp.total_throttled_us(), 28_000);
    assert_eq!(bp.credit_bytes(), 5);
    assert_eq!(budget.used_bytes(), 0);

    // Large 1.5 MiB output chunks cleanly to <= BULK_CHUNK_BYTES and round-trips through BrokerMsg::decode!
    let mut big = RawOutputFlow::new(2_000_000, false);
    let payload = vec![b'x'; 1_500_000];
    let d_big = big.ingest(&payload, ts(200), &mut budget);
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
    let mut budget = MemoryBudget::new(150);
    assert_eq!(budget.max_bytes(), 150);
    assert_eq!(budget.remaining_bytes(), 150);
    let mut q = OutboundQueue::new();
    assert!(q.is_empty());
    assert_eq!(q.ordered_bytes(), 0);
    assert_eq!(q.frame_bytes(), 0);
    assert_eq!(q.total_bytes(), 0);
    assert_eq!(q.pending_frames(), 0);

    let s1 = StreamId::from_raw(1);
    let s2 = StreamId::from_raw(2);
    assert!(!q.has_pending_frame(s1));
    assert!(!q.has_stream_messages(s1));

    // Verify is_empty() individually for each of the 5 tiers
    q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::PUSH, BrokerMsg::Ok, 1, &mut budget)
        .unwrap();
    assert!(!q.is_empty());
    assert!(q.has_stream_messages(StreamId::CONTROL));
    assert!(q.pop_next(&mut budget).is_some());
    assert!(q.is_empty());
    assert!(!q.has_stream_messages(StreamId::CONTROL));

    q.push_frame(s1, att(1), sample_frame(), 1, &mut budget).unwrap();
    assert!(!q.is_empty());
    assert!(q.has_pending_frame(s1));
    assert!(q.has_stream_messages(s1));
    assert_eq!(q.frame_bytes(), 1);
    let popped_f = q.pop_next(&mut budget).unwrap();
    assert_eq!(popped_f.attachment_id, Some(att(1)));
    assert!(q.is_empty());
    assert_eq!(q.frame_bytes(), 0);

    q.push_ordered(OutboundPriority::Event, s1, RequestId::PUSH, BrokerMsg::Bell, 1, &mut budget).unwrap();
    assert!(!q.is_empty());
    assert!(q.has_stream_messages(s1));
    assert!(q.pop_next(&mut budget).is_some());
    assert!(q.is_empty());

    q.push_ordered(OutboundPriority::Bulk, s1, RequestId::PUSH, BrokerMsg::Bell, 1, &mut budget).unwrap();
    assert!(!q.is_empty());
    assert!(q.has_stream_messages(s1));
    assert!(q.pop_next(&mut budget).is_some());
    assert!(q.is_empty());

    q.push_ordered(OutboundPriority::RawOutput, s1, RequestId::PUSH, BrokerMsg::Bell, 1, &mut budget).unwrap();
    assert!(!q.is_empty());
    assert!(q.has_stream_messages(s1));
    assert!(q.pop_next(&mut budget).is_some());
    assert!(q.is_empty());
    assert_eq!(budget.used_bytes(), 0);

    // Enqueue across all 5 tiers
    q.push_ordered(
        OutboundPriority::RawOutput,
        s1,
        RequestId::PUSH,
        BrokerMsg::Output { text: "r1a".into() },
        10,
        &mut budget,
    )
    .unwrap();
    q.push_ordered(
        OutboundPriority::RawOutput,
        s1,
        RequestId::PUSH,
        BrokerMsg::Output { text: "r1b".into() },
        10,
        &mut budget,
    )
    .unwrap();
    q.push_ordered(
        OutboundPriority::RawOutput,
        s2,
        RequestId::PUSH,
        BrokerMsg::Output { text: "r2a".into() },
        10,
        &mut budget,
    )
    .unwrap();

    q.push_ordered(
        OutboundPriority::Bulk,
        s1,
        RequestId::from_raw(1),
        BrokerMsg::TextReply { text: "b1a".into() },
        10,
        &mut budget,
    )
    .unwrap();
    q.push_ordered(
        OutboundPriority::Bulk,
        s1,
        RequestId::from_raw(2),
        BrokerMsg::TextReply { text: "b1b".into() },
        10,
        &mut budget,
    )
    .unwrap();
    q.push_ordered(
        OutboundPriority::Bulk,
        s2,
        RequestId::from_raw(3),
        BrokerMsg::TextReply { text: "b2a".into() },
        10,
        &mut budget,
    )
    .unwrap();

    q.push_ordered(OutboundPriority::Event, s1, RequestId::PUSH, BrokerMsg::Bell, 10, &mut budget).unwrap();
    q.push_ordered(
        OutboundPriority::Control,
        StreamId::CONTROL,
        RequestId::from_raw(9),
        BrokerMsg::Ok,
        10,
        &mut budget,
    )
    .unwrap();

    // Frame replacement accounts delta cleanly and enforces budget cap (150)
    let mut f1 = sample_frame();
    f1.seq = 1;
    let mut f2 = sample_frame();
    f2.seq = 2;
    q.push_frame(s1, att(1), f1, 40, &mut budget).unwrap();
    q.push_frame(s1, att(1), f2, 50, &mut budget).unwrap();
    assert_eq!(q.pending_frames(), 1);
    assert_eq!(q.frame_bytes(), 50);
    assert_eq!(q.ordered_bytes(), 80);
    assert_eq!(q.total_bytes(), 130);
    assert_eq!(budget.used_bytes(), 130);

    // Frame exact boundary (130 + 20 == 150) succeeds, then remove s2 frame; +21 overflows
    assert_eq!(q.push_frame(s2, att(2), sample_frame(), 21, &mut budget), Err(QueueOverflow));
    q.push_frame(s2, att(2), sample_frame(), 20, &mut budget).unwrap();
    assert_eq!(q.total_bytes(), 150);
    q.remove_stream_frames(s2, &mut budget);
    assert_eq!(q.total_bytes(), 130);
    assert_eq!(budget.used_bytes(), 130);
    // Ordered overflow rejected (130 + 21 > 150); exact 20 fits
    assert_eq!(
        q.push_ordered(OutboundPriority::Control, StreamId::CONTROL, RequestId::PUSH, BrokerMsg::Ok, 21, &mut budget),
        Err(QueueOverflow)
    );
    q.push_ordered(OutboundPriority::Event, s2, RequestId::PUSH, BrokerMsg::Bell, 20, &mut budget).unwrap();
    assert_eq!(q.total_bytes(), 150);

    // Pop order: Control -> Frame(seq=2) -> Event(s1) -> Event(s2) -> Bulk WFQ(s1, s2, s1) -> Raw WFQ(s1, s2, s1)
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::Ok);
    match q.pop_next(&mut budget).unwrap().msg {
        BrokerMsg::Frame(f) => assert_eq!(f.seq, 2),
        other => panic!("expected Frame(seq=2), got {other:?}"),
    }
    assert_eq!(q.frame_bytes(), 0);
    assert_eq!(q.pop_next(&mut budget).unwrap().stream_id, s1); // Bell s1
    assert_eq!(q.pop_next(&mut budget).unwrap().stream_id, s2); // Bell s2
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::TextReply { text: "b1a".into() });
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::TextReply { text: "b2a".into() });
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::TextReply { text: "b1b".into() });
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::Output { text: "r1a".into() });
    assert_eq!(q.ordered_bytes(), 20);
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::Output { text: "r2a".into() });
    assert_eq!(q.ordered_bytes(), 10);
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::Output { text: "r1b".into() });
    assert!(q.is_empty());
    assert_eq!(q.total_bytes(), 0);
    assert_eq!(budget.used_bytes(), 0);
    assert_eq!(q.pop_next(&mut budget), None);

    // remove_stream cleans s1 across all tiers while preserving s2 in every tier
    q.push_frame(s1, att(1), sample_frame(), 20, &mut budget).unwrap();
    q.push_frame(s2, att(2), sample_frame(), 20, &mut budget).unwrap();
    q.push_ordered(OutboundPriority::Event, s1, RequestId::PUSH, BrokerMsg::Bell, 15, &mut budget).unwrap();
    q.push_ordered(OutboundPriority::Bulk, s1, RequestId::PUSH, BrokerMsg::Bell, 25, &mut budget).unwrap();
    q.push_ordered(
        OutboundPriority::Bulk,
        s2,
        RequestId::PUSH,
        BrokerMsg::TextReply { text: "keep_bulk".into() },
        5,
        &mut budget,
    )
    .unwrap();
    q.push_ordered(OutboundPriority::RawOutput, s1, RequestId::PUSH, BrokerMsg::Bell, 30, &mut budget).unwrap();
    q.push_ordered(
        OutboundPriority::RawOutput,
        s2,
        RequestId::PUSH,
        BrokerMsg::Output { text: "keep_raw".into() },
        5,
        &mut budget,
    )
    .unwrap();
    q.push_ordered(OutboundPriority::Event, s2, RequestId::PUSH, BrokerMsg::Bell, 10, &mut budget).unwrap();
    q.remove_stream(s1, &mut budget);
    assert_eq!(q.pending_frames(), 1);
    assert_eq!(q.frame_bytes(), 20);
    assert_eq!(q.ordered_bytes(), 20);
    assert_eq!(budget.used_bytes(), 40);
    assert_eq!(q.pop_next(&mut budget).unwrap().stream_id, s2); // frame
    assert_eq!(q.pop_next(&mut budget).unwrap().stream_id, s2); // event
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::TextReply { text: "keep_bulk".into() });
    assert_eq!(q.pop_next(&mut budget).unwrap().msg, BrokerMsg::Output { text: "keep_raw".into() });
    assert!(q.is_empty());
    assert_eq!(budget.used_bytes(), 0);
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
    let tok_a = ResumeToken::new([0x66; 32]).unwrap();
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
    let tok = ResumeToken::new([0x77; 32]).unwrap();
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
        t[0..8].copy_from_slice(&(i + 1).to_le_bytes());
        let cid = conn(100 + i);
        table
            .acquire(cid, LeaseKind::BrokerGrace { grace_ms: grace, resume_token: ResumeToken::new(t).unwrap() })
            .unwrap();
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
            ..
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
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::HandshakeFailed, .. })));

    // Unknown msg_type on a Ready connection fails decoding with CloseReason::ProtocolError.
    let mut ready_broker = BrokerCore::new(AuthConfig::None);
    handshake_ready(&mut ready_broker, conn(8), ts(8_000));
    actions.clear();
    ready_broker.feed_bytes(conn(8), &bad_msg_frame, ts(8_001), &mut actions);
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
    let s1_att = broker.stream_attachment(conn(1), nz_stream(1)).expect("stream 1 attached");
    actions.clear();
    assert!(broker.publish_frame(conn(1), nz_stream(1), s1_att, sample_frame(), ts(130), &mut actions));
    assert!(!broker.publish_frame(conn(1), nz_stream(1), s1_att, sample_frame(), ts(131), &mut actions));
    let f1 = broker.pop_outbound(conn(1)).unwrap();
    assert!(matches!(f1.msg, BrokerMsg::Frame(ref fr) if fr.seq == 1));
    // Once popped onto wire, 2nd in-flight frame IS accepted (seq=2), and 3rd is blocked by max_frames=2!
    assert!(broker.publish_frame(conn(1), nz_stream(1), s1_att, sample_frame(), ts(132), &mut actions));
    let f2 = broker.pop_outbound(conn(1)).unwrap();
    assert!(matches!(f2.msg, BrokerMsg::Frame(ref fr) if fr.seq == 2));
    assert!(!broker.publish_frame(conn(1), nz_stream(1), s1_att, sample_frame(), ts(133), &mut actions));

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
    assert!(broker.publish_frame(conn(1), nz_stream(1), s1_att, sample_frame(), ts(218), &mut actions));
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
    let tok = ResumeToken::new([0x33; 32]).unwrap();
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

    // stream_session + try_publish_frame budget boundary & ownership return without clone!
    assert_eq!(broker.stream_session(conn(3), nz_stream(1)), Some(sid(1)));
    assert_eq!(broker.stream_session(conn(3), nz_stream(9_999)), None);
    let mut small_budget = MemoryBudget::new(100);
    let mut small_q = OutboundQueue::new();
    assert!(small_q.can_fit_frame(StreamId::from_raw(1), 100, &small_budget));
    assert!(!small_q.can_fit_frame(StreamId::from_raw(1), 101, &small_budget));
    small_q.push_frame(StreamId::from_raw(1), att(1), sample_frame(), 60, &mut small_budget).unwrap();
    assert!(small_q.can_fit_frame(StreamId::from_raw(1), 100, &small_budget)); // replacing existing 60B frame with 100B fits!
    assert!(!small_q.can_fit_frame(StreamId::from_raw(2), 41, &small_budget)); // 60 + 41 > 100

    // try_publish_frame returns Err(frame) when queue cap is exceeded on a live attached stream
    let mut tight = BrokerCore::with_queue_cap(AuthConfig::None, 80);
    tight.register_session(sid(1), false, false);
    tight.open_connection(conn(1), [0; 32], ts(0)).unwrap();
    let mut a2 = Vec::new();
    tight.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            1,
            &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION, client_nonce: [0; 32], client_name: "t".into() },
        ),
        ts(1),
        &mut a2,
    );
    assert!(matches!(tight.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::HelloOk { .. }));
    tight.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            2,
            &ClientMsg::Attach {
                session: sid(1),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 4, max_bytes: 65_536 },
            },
        ),
        ts(2),
        &mut a2,
    );
    let tight_att = tight.stream_attachment(conn(1), nz_stream(1)).expect("tight att");
    // Fill 70 bytes of the 80-byte budget so sample_frame() (~37 bytes) exceeds budget without closing conn!
    tight.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::PUSH,
        OutboundPriority::Event,
        BrokerMsg::Title { title: "x".repeat(50) },
        ts(2),
        &mut a2,
    );
    let returned = tight.try_publish_frame(conn(1), nz_stream(1), tight_att, sample_frame(), ts(3), &mut a2);
    assert!(returned.is_err());
}

#[test]
fn broker_fin_drain_filters_frames_and_oversized_messages_enforced() {
    use crate::broker::MAX_WIRE_FRAME_BYTES;

    let mut broker = BrokerCore::with_queue_cap(AuthConfig::None, 32 * 1024 * 1024);
    broker.register_session(sid(1), false, false);
    let mut actions = Vec::new();
    broker.open_connection(conn(1), [1; 32], ts(0)).unwrap();
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            1,
            &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION, client_nonce: [0; 32], client_name: "c".into() },
        ),
        ts(1),
        &mut actions,
    );
    let _ = broker.pop_outbound(conn(1));
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            2,
            &ClientMsg::Attach {
                session: sid(1),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 4, max_bytes: 16 * 1024 * 1024 },
            },
        ),
        ts(2),
        &mut actions,
    );
    // 1. Oversized correlated enqueue_message -> ErrorCode::ResourceLimit; oversized PUSH -> dropped;
    //    exact MAX_WIRE_FRAME_BYTES -> allowed!
    let make_blob_msg = |data_len: usize| BrokerMsg::ImageBlobChunk {
        blob: apack_proto::types::BlobHash([0; 32]),
        encoding: apack_proto::types::ImageEncoding::Png,
        total_bytes: data_len as u32,
        offset: 0,
        data: vec![0u8; data_len],
    };
    // Measure exact overhead of ImageBlobChunk so we construct exact MAX_WIRE_FRAME_BYTES and +1.
    let probe_len = 8_000_000usize;
    let probe = make_blob_msg(probe_len);
    let mut w = apack_proto::codec::Writer::new();
    probe.encode(&mut w);
    let overhead = apack_proto::FRAME_HEADER_LEN + w.as_slice().len() - probe_len;
    let exact_data = MAX_WIRE_FRAME_BYTES - overhead;

    actions.clear();
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::from_raw(10),
        OutboundPriority::Bulk,
        make_blob_msg(exact_data),
        ts(3),
        &mut actions,
    );
    let exact_pop = broker.pop_outbound(conn(1)).expect("exact boundary must enqueue");
    assert_eq!(exact_pop.req_id, RequestId::from_raw(10));
    assert!(matches!(exact_pop.msg, BrokerMsg::ImageBlobChunk { .. }));

    // exact_data + 1 correlated -> ResourceLimit error
    actions.clear();
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::from_raw(11),
        OutboundPriority::Bulk,
        make_blob_msg(exact_data + 1),
        ts(4),
        &mut actions,
    );
    let over_pop = broker.pop_outbound(conn(1)).expect("correlated oversized converts to ResourceLimit");
    assert_eq!(over_pop.req_id, RequestId::from_raw(11));
    assert!(matches!(over_pop.msg, BrokerMsg::Error { code: ErrorCode::ResourceLimit, .. }));

    // exact_data + 1 unsolicited PUSH -> dropped cleanly
    actions.clear();
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::PUSH,
        OutboundPriority::Bulk,
        make_blob_msg(exact_data + 1),
        ts(5),
        &mut actions,
    );
    assert!(broker.pop_outbound(conn(1)).is_none(), "unsolicited oversized push must be dropped");

    let s1_att = broker.stream_attachment(conn(1), nz_stream(1)).expect("s1 att");
    // 2. try_publish_frame rejects > MAX_WIRE_FRAME_BYTES and admits exact MAX_WIRE_FRAME_BYTES
    let mut huge_frame = sample_frame();
    huge_frame.rows.push(apack_proto::types::PackedRow {
        line: 0,
        flags: 0,
        text: "x".repeat(apack_proto::MAX_FRAME_PAYLOAD),
        runs: Vec::new(),
        styles: Vec::new(),
        links: Vec::new(),
        pieces: Vec::new(),
    });
    assert!(broker.try_publish_frame(conn(1), nz_stream(1), s1_att, huge_frame, ts(6), &mut actions).is_err());

    // Construct a frame whose encoded_bytes == MAX_WIRE_FRAME_BYTES
    let mut probe_f = sample_frame();
    probe_f.seq = 1;
    let base_text = 8_000_000usize;
    probe_f.rows.push(apack_proto::types::PackedRow {
        line: 0,
        flags: 0,
        text: "a".repeat(base_text),
        runs: Vec::new(),
        styles: Vec::new(),
        links: Vec::new(),
        pieces: Vec::new(),
    });
    let mut wf = apack_proto::codec::Writer::new();
    BrokerMsg::Frame(probe_f.clone()).encode(&mut wf);
    let f_overhead = apack_proto::FRAME_HEADER_LEN + wf.as_slice().len() - base_text;
    let exact_text = MAX_WIRE_FRAME_BYTES - f_overhead;
    probe_f.rows[0].text = "a".repeat(exact_text);
    assert!(broker.try_publish_frame(conn(1), nz_stream(1), s1_att, probe_f, ts(7), &mut actions).is_ok());

    // 3. ClientFin drains queued ordered replies while dropping queued Frame!
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::from_raw(12),
        OutboundPriority::Control,
        BrokerMsg::Ok,
        ts(8),
        &mut actions,
    );
    actions.clear();
    broker.close_connection(conn(1), CloseReason::ClientFin, ts(9), &mut actions);
    let drained = actions
        .iter()
        .find_map(|a| match a {
            BrokerAction::CloseConn { reason: CloseReason::ClientFin, drained, .. } => Some(drained),
            _ => None,
        })
        .expect("CloseConn for ClientFin");
    assert_eq!(drained.len(), 1, "ClientFin must drain ordered Ok and drop conflatable Frame");
    assert!(matches!(drained[0].msg, BrokerMsg::Ok));

    // 4. Non-ClientFin close (SlowReader) must NOT drain queued replies!
    broker.open_connection(conn(2), [2; 32], ts(10)).unwrap();
    broker.enqueue_message(
        conn(2),
        StreamId::CONTROL,
        RequestId::from_raw(13),
        OutboundPriority::Control,
        BrokerMsg::Ok,
        ts(11),
        &mut actions,
    );
    actions.clear();
    broker.close_connection(conn(2), CloseReason::SlowReader, ts(12), &mut actions);
    let slow_drained = actions
        .iter()
        .find_map(|a| match a {
            BrokerAction::CloseConn { reason: CloseReason::SlowReader, drained, .. } => Some(drained),
            _ => None,
        })
        .expect("CloseConn for SlowReader");
    assert!(slow_drained.is_empty(), "SlowReader must not drain queued replies");
}

#[test]
fn pr4_stream_attachment_phase_decoders_unified_budget_and_exited_suppression() {
    use apack_proto::frame::{INNER_HEADER_LEN, encode_frame};
    use apack_proto::messages::msg_type;
    use apack_proto::{MAX_AUTH_FRAME_PAYLOAD, MAX_CLIENT_NAME_BYTES, MAX_HELLO_FRAME_PAYLOAD};

    assert!(StreamAttachmentId::new(0).is_none());
    assert_eq!(StreamAttachmentId::new(42).unwrap().get(), 42);

    // 1. Phase-typed ConnPhase decoders (`H7`):
    //    (a) AwaitingHello rejects > 4 KiB on the 4-byte length prefix alone (zero payload needed).
    let mut b_phase = BrokerCore::new(AuthConfig::Required(AuthSecret::new([0xAA; 32])));
    b_phase.open_connection(conn(1), [1; 32], ts(0)).unwrap();
    let mut actions = Vec::new();
    let over_hello = (INNER_HEADER_LEN + MAX_HELLO_FRAME_PAYLOAD + 1) as u32;
    b_phase.feed_bytes(conn(1), &over_hello.to_le_bytes(), ts(1), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));

    //    (b) AwaitingHello rejects non-HELLO msg_type BEFORE decoding payload (even garbage Spawn bytes).
    b_phase.open_connection(conn(2), [2; 32], ts(2)).unwrap();
    actions.clear();
    let mut bad_type = Vec::new();
    encode_frame(StreamId::CONTROL, msg_type::SPAWN, RequestId::from_raw(1), &[0xFF, 0xFF, 0xFF], &mut bad_type)
        .unwrap();
    b_phase.feed_bytes(conn(2), &bad_type, ts(3), &mut actions);
    assert!(matches!(
        actions.last(),
        Some(BrokerAction::CloseConn {
            reason: CloseReason::HandshakeFailed,
            final_error: Some((_, _, BrokerMsg::Error { code: ErrorCode::Handshake, .. })),
            ..
        })
    ));

    //    (c) AwaitingHello rejects Hello.client_name > 256 bytes as malformed.
    b_phase.open_connection(conn(3), [3; 32], ts(4)).unwrap();
    actions.clear();
    let long_hello = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_nonce: [0x11; 32],
            client_name: "n".repeat(MAX_CLIENT_NAME_BYTES + 1),
        },
    );
    b_phase.feed_bytes(conn(3), &long_hello, ts(5), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));

    //    (d) AwaitingAuth rejects > 64 B on the 4-byte length prefix alone.
    b_phase.open_connection(conn(4), [4; 32], ts(6)).unwrap();
    actions.clear();
    let ok_hello = encode_client(
        StreamId::CONTROL,
        1,
        &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION, client_nonce: [0x22; 32], client_name: "ok".into() },
    );
    b_phase.feed_bytes(conn(4), &ok_hello, ts(7), &mut actions);
    assert!(matches!(b_phase.pop_outbound(conn(4)).unwrap().msg, BrokerMsg::HelloOk { auth_required: true, .. }));
    let over_auth = (INNER_HEADER_LEN + MAX_AUTH_FRAME_PAYLOAD + 1) as u32;
    actions.clear();
    b_phase.feed_bytes(conn(4), &over_auth.to_le_bytes(), ts(8), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));

    //    (e) AwaitingAuth rejects non-AUTH_PROOF msg_type BEFORE decoding payload, and malformed AUTH_PROOF.
    b_phase.open_connection(conn(5), [5; 32], ts(9)).unwrap();
    b_phase.feed_bytes(conn(5), &ok_hello, ts(10), &mut actions);
    let _ = b_phase.pop_outbound(conn(5));
    actions.clear();
    b_phase.feed_bytes(conn(5), &bad_type, ts(11), &mut actions);
    assert!(matches!(
        actions.last(),
        Some(BrokerAction::CloseConn {
            reason: CloseReason::Unauthorized,
            final_error: Some((_, _, BrokerMsg::Error { code: ErrorCode::Unauthorized, .. })),
            ..
        })
    ));

    b_phase.open_connection(conn(6), [6; 32], ts(12)).unwrap();
    b_phase.feed_bytes(conn(6), &ok_hello, ts(13), &mut actions);
    let _ = b_phase.pop_outbound(conn(6));
    actions.clear();
    let mut bad_auth = Vec::new();
    encode_frame(StreamId::CONTROL, msg_type::AUTH_PROOF, RequestId::from_raw(2), &[0xAA; 10], &mut bad_auth).unwrap();
    b_phase.feed_bytes(conn(6), &bad_auth, ts(14), &mut actions);
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::ProtocolError, .. })));

    // 2. Monotonic StreamAttachmentId, stale frame rejection on same-session re-attach, and abort_attach (`B2`):
    let mut broker = BrokerCore::with_queue_cap(AuthConfig::None, 400);
    broker.register_session(sid(1), false, false);
    broker.register_session(sid(2), false, false);
    handshake_ready(&mut broker, conn(1), ts(20));

    let attach_s1 = |req: u32, flags: u8, credit: u32| {
        encode_client(
            StreamId::CONTROL,
            req,
            &ClientMsg::Attach {
                session: sid(1),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(flags).unwrap(),
                initial_viewport: None,
                initial_output_credit: credit,
                initial_frame_credit: FrameCredit { max_frames: 2, max_bytes: 65_536 },
            },
        )
    };
    actions.clear();
    broker.feed_bytes(conn(1), &attach_s1(10, SubFlags::VIEW | SubFlags::EVENTS, 0), ts(21), &mut actions);
    let att1 = broker.stream_attachment(conn(1), nz_stream(1)).expect("att1");
    assert!(broker.is_attachment_active(conn(1), nz_stream(1), att1));

    // Detach and re-attach SAME stream_id to SAME session -> new monotonic att2; att1 is rejected!
    actions.clear();
    broker.feed_bytes(
        conn(1),
        &encode_client(StreamId::CONTROL, 11, &ClientMsg::Detach { stream_id: nz_stream(1) }),
        ts(22),
        &mut actions,
    );
    let _ = broker.pop_outbound(conn(1));
    assert!(!broker.is_attachment_active(conn(1), nz_stream(1), att1));

    actions.clear();
    broker.feed_bytes(conn(1), &attach_s1(12, SubFlags::VIEW | SubFlags::EVENTS, 0), ts(23), &mut actions);
    let att2 = broker.stream_attachment(conn(1), nz_stream(1)).expect("att2");
    assert!(att2.get() > att1.get());
    assert!(!broker.is_attachment_active(conn(1), nz_stream(1), att1));
    assert!(broker.is_attachment_active(conn(1), nz_stream(1), att2));
    assert!(broker.try_publish_frame(conn(1), nz_stream(1), att1, sample_frame(), ts(24), &mut actions).is_err());
    assert!(broker.try_publish_frame(conn(1), nz_stream(1), att2, sample_frame(), ts(24), &mut actions).is_ok());
    let _ = broker.pop_outbound(conn(1));

    // abort_attach ignores mismatched attachment_id and rolls back matching attachment_id.
    broker.abort_attach(conn(1), nz_stream(1), att1);
    assert!(broker.is_attachment_active(conn(1), nz_stream(1), att2));
    broker.abort_attach(conn(1), nz_stream(1), att2);
    assert!(broker.stream_attachment(conn(1), nz_stream(1)).is_none());

    // 3. Redundant StreamClosed suppression, P3 Bulk ordering, and immediate stream reuse on Exited (!RAW_OUTPUT):
    actions.clear();
    broker.feed_bytes(conn(1), &attach_s1(13, SubFlags::VIEW | SubFlags::EVENTS, 0), ts(25), &mut actions);
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            14,
            &ClientMsg::Attach {
                session: sid(1),
                stream_id: nz_stream(2),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(26),
        &mut actions,
    );
    // Queue a P3 Bulk reply BEFORE Exited on stream 1 -> Exited (demoted to P4 for !RAW_OUTPUT)
    // must drain strictly AFTER P3 Bulk so the client never sees Exited overtake an in-flight RPC reply!
    broker.enqueue_message(
        conn(1),
        nz_stream(1).as_stream_id(),
        RequestId::from_raw(77),
        OutboundPriority::Bulk,
        BrokerMsg::TextReply { text: String::from("bulk-before-exit") },
        ts(27),
        &mut actions,
    );
    broker.enqueue_message(
        conn(1),
        nz_stream(1).as_stream_id(),
        RequestId::PUSH,
        OutboundPriority::Event,
        BrokerMsg::Exited { exit_code: Some(0), signal: None },
        ts(27),
        &mut actions,
    );
    // While Bulk + Exited are still queued on stream 1, stream 1 is already in closing_streams -> Busy on re-attach!
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            15,
            &ClientMsg::Attach {
                session: sid(2),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(28),
        &mut actions,
    );
    assert!(matches!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Busy, .. }));
    // P3 Bulk drains strictly BEFORE P4 Exited!
    let bulk_msg = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(bulk_msg.stream_id, nz_stream(1).as_stream_id());
    assert_eq!(bulk_msg.msg, BrokerMsg::TextReply { text: String::from("bulk-before-exit") });
    // Still in closing_streams while Exited remains queued:
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            16,
            &ClientMsg::Attach {
                session: sid(2),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(28),
        &mut actions,
    );
    assert!(matches!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Error { code: ErrorCode::Busy, .. }));
    // Pop Exited (stream 1) -> clears closing_streams immediately EVEN BEFORE unregister_session runs!
    let m1 = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(m1.stream_id, nz_stream(1).as_stream_id());
    assert_eq!(m1.msg, BrokerMsg::Exited { exit_code: Some(0), signal: None });
    // Immediate reuse of stream_id 1 for sid(2) BEFORE unregister_session(sid(1)) succeeds cleanly!
    actions.clear();
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            17,
            &ClientMsg::Attach {
                session: sid(2),
                stream_id: nz_stream(1),
                flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 1, max_bytes: 65_536 },
            },
        ),
        ts(28),
        &mut actions,
    );
    let reused_att = broker.stream_attachment(conn(1), nz_stream(1)).expect("reused stream 1");
    assert_eq!(broker.stream_session(conn(1), nz_stream(1)), Some(sid(2)));
    // Now unregister_session(sid(1)) runs on Turn 2: emits StreamClosed ONLY for stream 2 (VIEW-only),
    // leaves reused stream 1 untouched, and emits NO duplicate StreamClosed for stream 1!
    broker.unregister_session(sid(1), "session ended", ts(29), &mut actions);
    assert!(broker.is_attachment_active(conn(1), nz_stream(1), reused_att));
    let m2 = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(m2.stream_id, nz_stream(2).as_stream_id());
    assert_eq!(m2.msg, BrokerMsg::StreamClosed { reason: "session ended".into() });
    assert_eq!(broker.pop_outbound(conn(1)), None);
    assert_eq!(broker.connection_budget_used(conn(1)), Some(0));
    // Detach reused stream 1 before next section.
    broker.feed_bytes(
        conn(1),
        &encode_client(StreamId::CONTROL, 18, &ClientMsg::Detach { stream_id: nz_stream(1) }),
        ts(29),
        &mut actions,
    );
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Ok);

    // 4. Unified MemoryBudget headroom & OutputCredit drain (`M1`):
    let large_budget = MemoryBudget::new(2 * 1024 * 1024);
    assert_eq!(large_budget.pending_raw_room(), 2 * 1024 * 1024 - crate::flow::RAW_PENDING_BUDGET_HEADROOM);
    let small_budget = MemoryBudget::new(400);
    assert_eq!(small_budget.pending_raw_room(), 300);

    broker.register_session(sid(3), false, false);
    let attach_raw = |req: u32, session: SessionId, stream: u32| {
        encode_client(
            StreamId::CONTROL,
            req,
            &ClientMsg::Attach {
                session,
                stream_id: nz_stream(stream),
                flags: SubFlags::from_bits(SubFlags::RAW_OUTPUT | SubFlags::BACKPRESSURE_PTY).unwrap(),
                initial_viewport: None,
                initial_output_credit: 0,
                initial_frame_credit: FrameCredit { max_frames: 0, max_bytes: 0 },
            },
        )
    };
    broker.feed_bytes(conn(1), &attach_raw(20, sid(2), 1), ts(30), &mut actions);
    broker.feed_bytes(conn(1), &attach_raw(21, sid(3), 2), ts(31), &mut actions);
    // With 400B cap, pending_raw_room reserves 100B headroom (cap = 300B for pending_utf8).
    // Buffer 200B on stream 1 and 150B on stream 2 -> stream 2 keeps 100B and drops 50B without SlowReader!
    actions.clear();
    broker.publish_raw_output(sid(2), &[b'a'; 200], ts(32), &mut actions);
    broker.publish_raw_output(sid(3), &[b'b'; 150], ts(33), &mut actions);
    assert_eq!(broker.connection_count(), 1);
    let dropped_msg = broker.pop_outbound(conn(1)).unwrap();
    assert_eq!(dropped_msg.msg, BrokerMsg::OutputDropped { bytes: 50 });
    assert_eq!(broker.connection_budget_used(conn(1)), Some(300));
    // Draining full pending_utf8 via OutputCredit (releasing 200B, enqueuing Output + Throttled with framing
    // overhead) MUST succeed using the reserved headroom without tripping CloseReason::SlowReader!
    actions.clear();
    broker.feed_bytes(
        conn(1),
        &encode_client(nz_stream(1).as_stream_id(), 22, &ClientMsg::OutputCredit { bytes: 250 }),
        ts(34),
        &mut actions,
    );
    assert_eq!(broker.connection_count(), 1);
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "a".repeat(200) });
    assert!(matches!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Throttled { .. }));
    assert_eq!(broker.connection_budget_used(conn(1)), Some(100));
    // Detaching stream 2 releases its 100B pending_utf8 immediately!
    broker.feed_bytes(
        conn(1),
        &encode_client(StreamId::CONTROL, 23, &ClientMsg::Detach { stream_id: nz_stream(2) }),
        ts(35),
        &mut actions,
    );
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Ok);
    assert_eq!(broker.connection_budget_used(conn(1)), Some(0));
    // Re-attach stream 2 with initial_output_credit = 200 and publish 500B in a single call:
    // ingest emits 200B directly (`offset = 200`), caps pending_utf8 to `pending_raw_room() - 200 = 100B`,
    // drops 200B, and enqueues Output(200B) + OutputDropped(200B) WITHOUT tripping SlowReader!
    actions.clear();
    broker.feed_bytes(
        conn(1),
        &encode_client(
            StreamId::CONTROL,
            24,
            &ClientMsg::Attach {
                session: sid(3),
                stream_id: nz_stream(2),
                flags: SubFlags::from_bits(SubFlags::RAW_OUTPUT | SubFlags::BACKPRESSURE_PTY).unwrap(),
                initial_viewport: None,
                initial_output_credit: 200,
                initial_frame_credit: FrameCredit { max_frames: 0, max_bytes: 0 },
            },
        ),
        ts(36),
        &mut actions,
    );
    broker.publish_raw_output(sid(3), &[b'd'; 500], ts(37), &mut actions);
    assert_eq!(broker.connection_count(), 1);
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::Output { text: "d".repeat(200) });
    assert_eq!(broker.pop_outbound(conn(1)).unwrap().msg, BrokerMsg::OutputDropped { bytes: 200 });
    assert_eq!(broker.connection_budget_used(conn(1)), Some(100));
    // Refill remaining pending_utf8 room (+200B -> 300B total), then push a 120B outbound message -> SlowReader!
    broker.publish_raw_output(sid(3), &[b'c'; 200], ts(38), &mut actions);
    assert_eq!(broker.connection_budget_used(conn(1)), Some(300));
    actions.clear();
    broker.enqueue_message(
        conn(1),
        StreamId::CONTROL,
        RequestId::PUSH,
        OutboundPriority::Event,
        BrokerMsg::Title { title: "z".repeat(120) },
        ts(39),
        &mut actions,
    );
    assert!(matches!(actions.last(), Some(BrokerAction::CloseConn { reason: CloseReason::SlowReader, .. })));
    assert_eq!(broker.connection_count(), 0);
}
