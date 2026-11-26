use apack_core::lease::ConnId;
use apack_core::marks::MarksMode;
use apack_core::session::{SessionEffect, SessionMachine};
use apack_core::time::TimestampMs;
use apack_core::vt::{CompressStep, PasteEncode, VtEngine};
use apack_proto::messages::{BrokerMsg, ClientMsg};
use apack_proto::types::{
    CellPos, CellPx, CellSpan, CursorShape, FrameCredit, GridSize, KeyAction, KeyInput, MarksKey, NonZeroStreamId,
    PasteOutcome, PointerAction, PointerInput, RequestId, ResizePolicy, SessionId, StreamId, SubFlags, ViewportHint,
    WaitCondition, WaitOutcome,
};

use crate::mock_vt::MockVt;
use crate::world::{ClientViewModel, Rng64, SimWorld};

#[test]
fn mock_vt_one_byte_split_and_queries() {
    let grid = GridSize::new(20, 4).unwrap();
    let mut whole = MockVt::new(grid, 10);
    let mut by_one = MockVt::new(grid, 10);
    let stream = b"\x1b]0;my-title\x07\x1b]7;/tmp\x07\x1b]133;A\x07hello world\r\nfoo bar baz\r\n\x1b[6n\x1b[?2026h\x1b[?2026l\x07";

    let mut ev_whole = Vec::new();
    whole.feed(stream, &mut ev_whole);

    let mut ev_one = Vec::new();
    for b in stream {
        by_one.feed(std::slice::from_ref(b), &mut ev_one);
    }
    assert_eq!(ev_whole, ev_one);
    assert_eq!(whole.meta(), by_one.meta());

    // Word/line/text/rect/search queries
    let w = whole.word_at(CellPos { line: 0, col: 7 }, " ").unwrap();
    assert_eq!(w, CellSpan { start: CellPos { line: 0, col: 6 }, end: CellPos { line: 0, col: 10 } });
    assert_eq!(whole.text_of(w), "world");
    assert_eq!(whole.word_at(CellPos { line: 0, col: 5 }, " ").unwrap().start.col, 5);
    assert!(whole.word_at(CellPos { line: 99, col: 0 }, " ").is_none());

    let l = whole.line_at(1).unwrap();
    assert_eq!(whole.text_of(l), "foo bar baz");
    assert!(whole.line_at(99).is_none());

    let rect = whole.text_of_rect(CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 1, col: 2 } });
    assert_eq!(rect, "hel\nfoo");

    let mut found = Vec::new();
    assert!(!whole.search_lines(0, 4, "WORLD", false, 10, &mut found));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0], w);

    // Search limit truncation
    let mut lim = Vec::new();
    assert!(whole.search_lines(0, 4, "o", true, 1, &mut lim));
    assert_eq!(lim.len(), 1);

    // Alt-screen bumps epoch, resize bumps epoch, scrollback compression steps to Done
    let ep0 = whole.meta().epoch;
    let mut ev = Vec::new();
    whole.feed(b"\x1b[?1049hALT\x1b[?1049l", &mut ev);
    assert_eq!(whole.meta().epoch, ep0 + 2);

    for i in 0..8 {
        whole.feed(format!("line-{i}\r\n").as_bytes(), &mut ev);
    }
    assert!(whole.scrollback_bytes() > 0);
    assert_eq!(whole.compress_step(2), CompressStep::More);
    while whole.compress_step(10) == CompressStep::More {}
    assert_eq!(whole.compress_step(1), CompressStep::Done);

    // Key/pointer/focus/paste/cursor-defaults
    let mut out = Vec::new();
    whole.encode_key(
        &KeyInput {
            code: "Enter".into(),
            action: KeyAction::Press,
            modifiers: 0,
            text: String::new(),
            unshifted_codepoint: 0,
            composed: false,
        },
        &mut out,
    );
    assert_eq!(out, b"\r");
    whole.encode_focus(true, &mut out);
    whole.encode_focus(false, &mut out);
    assert!(out.ends_with(b"\x1b[I\x1b[O"));
    assert_eq!(whole.encode_paste("ls\n", false), PasteEncode::Unsafe);
    assert_eq!(whole.encode_paste("ls\n", true), PasteEncode::Bytes(b"ls\n".to_vec()));
    whole.set_cursor_defaults(CursorShape::Bar, true);
    assert_eq!(whole.meta().cursor.shape, CursorShape::Bar);

    let mut ptr_out = Vec::new();
    whole.encode_pointer(
        &PointerInput {
            action: PointerAction::Press,
            button: 0,
            modifiers: 0,
            x_px: 1,
            y_px: 2,
            cell_px: CellPx::default(),
        },
        &mut ptr_out,
    );
    assert!(ptr_out.is_empty());
}

#[test]
fn deterministic_chaos_simulation_is_reproducible_and_upholds_all_oracles() {
    let r1 = SimWorld::run_chaos(0xDEAD_BEEF_CAFE_1234, 120);
    let r2 = SimWorld::run_chaos(0xDEAD_BEEF_CAFE_1234, 120);
    assert_eq!(r1, r2, "identical seed must produce bit-for-bit identical simulation trace");
    assert!(r1.wire_frames_decoded > 50);
    assert!(r1.frames_verified > 20);
    assert!(r1.keyed_commands_finished > 0);
    assert!(r1.waits_completed > 0);

    // Run across multiple independent seeds
    for seed in [0u64, 1, 7, 42, 1337] {
        let rep = SimWorld::run_chaos(seed, 80);
        assert!(rep.frames_verified > 10);
        assert_ne!(rep.trace_hash, 0);
    }
    assert_ne!(Rng64::new(0).next_u64(), Rng64::new(1).next_u64());
}

#[test]
fn sim_world_viewport_minfit_hidden_views_and_disconnect() {
    let mut world = SimWorld::new(77);
    let s1 = SessionId::new(1).unwrap();
    let c1 = ConnId::new(1).unwrap();
    let c2 = ConnId::new(2).unwrap();
    let st1 = NonZeroStreamId::new(1).unwrap();
    let st2 = NonZeroStreamId::new(2).unwrap();

    world.add_session(s1, GridSize::new(100, 40).unwrap(), ResizePolicy::MinFit, MarksMode::Claimed);
    world.connect_client(c1);
    world.connect_client(c2);

    // c1 visible (80x24), c2 hidden (20x5) -> hidden view MUST NOT shrink terminal under MinFit!
    world.send_client_msg(
        c1,
        StreamId::CONTROL,
        RequestId::from_raw(2),
        &ClientMsg::Attach {
            session: s1,
            stream_id: st1,
            flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
            initial_viewport: Some(ViewportHint {
                size: GridSize::new(80, 24).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
                visible: true,
                focused: true,
            }),
            initial_output_credit: 0,
            initial_frame_credit: FrameCredit { max_frames: 2, max_bytes: 262_144 },
        },
    );
    world.send_client_msg(
        c2,
        StreamId::CONTROL,
        RequestId::from_raw(2),
        &ClientMsg::Attach {
            session: s1,
            stream_id: st2,
            flags: SubFlags::from_bits(SubFlags::VIEW).unwrap(),
            initial_viewport: Some(ViewportHint {
                size: GridSize::new(20, 5).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
                visible: false,
                focused: false,
            }),
            initial_output_credit: 0,
            initial_frame_credit: FrameCredit { max_frames: 2, max_bytes: 262_144 },
        },
    );

    world.feed_pty(s1, b"hello minfit\r\n");
    world.assert_views_match_ground_truth();

    // Now make c2 visible (60x30) -> MinFit computes min(80,60)=60 cols, min(24,30)=24 rows
    world.send_client_msg(
        c2,
        st2.as_stream_id(),
        RequestId::from_raw(3),
        &ClientMsg::ViewportHint(ViewportHint {
            size: GridSize::new(60, 30).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            visible: true,
            focused: false,
        }),
    );
    world.assert_views_match_ground_truth();

    // Disconnect c2 cleanly
    world.disconnect_client(c2);
    world.advance(50);
    world.assert_views_match_ground_truth();
}

#[test]
fn session_machine_direct_rpcs_and_wait_outcomes() {
    let key = MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap();
    let vt = MockVt::new(GridSize::new(40, 6).unwrap(), 64);
    let mut sess = SessionMachine::with_pty_write_cap(
        SessionId::new(1).unwrap(),
        vt,
        ResizePolicy::ExplicitOverride,
        MarksMode::Keyed(key),
        CellPx { width: 8, height: 16 },
        8,
    );
    let c1 = ConnId::new(1).unwrap();
    let st1 = NonZeroStreamId::new(1).unwrap();
    let mut fx = Vec::new();
    sess.attach_stream((c1, st1), SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);

    // ExplicitResize updates size even under ExplicitOverride
    fx.clear();
    sess.handle_stream_msg(
        (c1, st1),
        RequestId::from_raw(1),
        ClientMsg::ExplicitResize { size: GridSize::new(50, 8).unwrap(), cell_px: CellPx { width: 9, height: 18 } },
        &mut fx,
    );
    assert_eq!(sess.current_size(), GridSize::new(50, 8).unwrap());
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::ResizePty { .. })));

    // Bounded PTY write queue returns ErrorCode::Pty when exceeded
    fx.clear();
    sess.handle_stream_msg(
        (c1, st1),
        RequestId::from_raw(2),
        ClientMsg::Write { data: b"123456789".to_vec() },
        &mut fx,
    );
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { msg: BrokerMsg::Error { code: apack_proto::types::ErrorCode::Pty, .. }, .. }
    )));
    sess.handle_stream_msg((c1, st1), RequestId::from_raw(3), ClientMsg::Write { data: b"abcd".to_vec() }, &mut fx);
    assert_eq!(sess.pty_write_len(), 4);
    assert_eq!(sess.drain_pty_write(2), b"ab");
    assert_eq!(sess.drain_pty_write(10), b"cd");

    // Paste unsafe gate
    fx.clear();
    sess.handle_stream_msg(
        (c1, st1),
        RequestId::from_raw(4),
        ClientMsg::Paste { text: "rm -rf /\n".into(), allow_unsafe: false },
        &mut fx,
    );
    assert!(
        fx.iter()
            .any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Pasted { outcome: PasteOutcome::Unsafe }, .. }))
    );

    // Wait(Exit) wakes with Met on process exit; Wait(Idle) wakes with Exited
    fx.clear();
    sess.register_wait(c1, RequestId::from_raw(5), WaitCondition::Exit, 5_000, TimestampMs::from_millis(10), &mut fx);
    sess.register_wait(c1, RequestId::from_raw(6), WaitCondition::Idle, 5_000, TimestampMs::from_millis(10), &mut fx);
    sess.on_process_exit(Some(0), None, TimestampMs::from_millis(20), &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. }
        if *req_id == RequestId::from_raw(5)
    )));
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Exited }, .. }
        if *req_id == RequestId::from_raw(6)
    )));

    // ClientViewModel scroll-down path coverage
    let mut vm = ClientViewModel::new(GridSize::new(10, 3).unwrap());
    vm.apply_frame(&apack_proto::messages::FrameUpdate {
        seq: 1,
        epoch: 1,
        size: GridSize::new(10, 3).unwrap(),
        first_active_line: 5,
        scrollback_lines: 5,
        scroll_delta: 0,
        cursor: sess.vt().meta().cursor,
        modes: sess.vt().meta().modes,
        palette: None,
        rows: vec![],
        images_gen: 0,
        full_sync: true,
    });
    vm.apply_frame(&apack_proto::messages::FrameUpdate {
        seq: 2,
        epoch: 1,
        size: GridSize::new(10, 3).unwrap(),
        first_active_line: 5,
        scrollback_lines: 4,
        scroll_delta: -1,
        cursor: sess.vt().meta().cursor,
        modes: sess.vt().meta().modes,
        palette: None,
        rows: vec![],
        images_gen: 0,
        full_sync: false,
    });
    assert_eq!(vm.last_seq, 2);

    // Extreme u64::MAX span must clamp safely without hanging/OOM
    assert_eq!(
        sess.vt()
            .text_of(CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: u64::MAX, col: u16::MAX } }),
        "\n\n\n\n\n\n\n"
    );
    assert_eq!(
        sess.vt().text_of_rect(CellSpan {
            start: CellPos { line: 0, col: 0 },
            end: CellPos { line: u64::MAX, col: u16::MAX },
        }),
        "\n\n\n\n\n\n\n"
    );
}
