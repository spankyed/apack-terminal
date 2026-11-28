use super::*;

#[test]
fn e2e_interactive_write_resize_pause_raw_output_and_multi_shard() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-inter");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 2;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    let mut client = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "inter-test").unwrap();
    let (lease_id, _) = client.acquire_lease(LeaseKind::BrokerImmediate).unwrap();

    // Spawn 2 sessions on 2 shards (one with scrollback_limit = 0 default, one interactive shell).
    let mut spec1 = echo_command_spec("SHARD_ONE_READY", None, None);
    spec1.scrollback_limit = 0;
    let (s1, _) = client.spawn(spec1).unwrap();
    assert_eq!(client.wait(s1, WaitCondition::Text(String::from("SHARD_ONE_READY")), 5_000).unwrap(), WaitOutcome::Met);

    #[cfg(unix)]
    let interactive_argv = vec![String::from("/bin/sh"), String::from("-c"), String::from("echo READY_IN; exec cat")];
    #[cfg(windows)]
    let interactive_argv = vec![String::from("cmd.exe")];

    let spec2 = SpawnSpec {
        argv: interactive_argv,
        cwd: None,
        env: Vec::new(),
        size: GridSize::new(80, 24).unwrap(),
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_limit: 200,
        marks_key: None,
        resize_policy: ResizePolicy::ExplicitOverride,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    };
    let (s2, _) = client.spawn(spec2).unwrap();
    assert_eq!(client.list_sessions().unwrap().len(), 2);

    // Attach VIEW + RAW_OUTPUT stream to s2 with 2 pre-granted frame credits.
    let st = NonZeroStreamId::new(1).unwrap();
    client
        .attach(
            s2,
            st,
            SubFlags::from_bits(SubFlags::VIEW | SubFlags::RAW_OUTPUT).unwrap(),
            None,
            65_536,
            FrameCredit { max_frames: 2, max_bytes: 256 * 1024 },
        )
        .unwrap();

    // Receive initial full-sync frame (frame 1) — deliberately DO NOT send FrameAck yet so Frame 2
    // MUST rely on `on_outbound_frame_drained` freeing the shard's 1-slot view handshake!
    let f1 = loop {
        let f = client.recv_frame().unwrap();
        if let BrokerMsg::Frame(upd) = f.msg {
            break upd;
        }
    };
    let _ = f1.seq;

    // Also attach a second viewer client (viewer_b), then disconnect a third viewer (viewer_drop)
    // to prove `CancelConnWaiters` / `view_slot_free.retain(|&(c, _)| c != conn)` does NOT wipe peers!
    let mut viewer_b = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "viewer-b").unwrap();
    let st_b = NonZeroStreamId::new(1).unwrap();
    viewer_b
        .attach(
            s2,
            st_b,
            SubFlags::from_bits(SubFlags::VIEW).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 4, max_bytes: 256 * 1024 },
        )
        .unwrap();
    let _vb_f1 = loop {
        if let BrokerMsg::Frame(u) = viewer_b.recv_frame().unwrap().msg {
            break u;
        }
    };
    {
        let mut viewer_drop = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "viewer-drop").unwrap();
        viewer_drop
            .attach(
                s2,
                NonZeroStreamId::new(1).unwrap(),
                SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                None,
                0,
                FrameCredit { max_frames: 2, max_bytes: 256 * 1024 },
            )
            .unwrap();
        viewer_drop.send_fin().unwrap();
    }
    // Wait until reactor has closed `viewer_drop` AND the shard has processed `CancelConnWaiters`.
    for _ in 0..50 {
        if client.metrics().unwrap().connections == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = client.list_sessions().unwrap();

    // Explicit Resize + interactive Write -> second view frame (without prior FrameAck!) + RawOutput!
    client
        .send_raw(
            st.as_stream_id(),
            RequestId::PUSH,
            &ClientMsg::ExplicitResize {
                size: GridSize::new(100, 30).unwrap(),
                cell_px: CellPx { width: 9, height: 18 },
            },
        )
        .unwrap();
    client
        .send_raw(
            st.as_stream_id(),
            RequestId::PUSH,
            &ClientMsg::Write { data: b"echo ECHO_SECOND_FRAME\r\n".to_vec() },
        )
        .unwrap();

    // Wait for BOTH a second FrameUpdate containing ECHO_SECOND_FRAME and RawOutput containing it!
    let mut saw_frame2 = false;
    let mut raw_buf = String::new();
    for _ in 0..64 {
        let f = client.recv_frame().unwrap();
        match f.msg {
            BrokerMsg::Frame(upd) => {
                if upd.rows.iter().any(|r| r.text.contains("ECHO_SECOND_FRAME")) {
                    saw_frame2 = true;
                }
                client
                    .send_raw(
                        st.as_stream_id(),
                        RequestId::PUSH,
                        &ClientMsg::FrameAck {
                            seq: upd.seq,
                            credit: FrameCredit { max_frames: 2, max_bytes: 256 * 1024 },
                        },
                    )
                    .unwrap();
            }
            BrokerMsg::Output { text } => {
                raw_buf.push_str(&text);
            }
            _ => {}
        }
        if saw_frame2 && raw_buf.contains("ECHO_SECOND_FRAME") {
            break;
        }
    }
    assert!(saw_frame2, "must receive second pipelined FrameUpdate before FrameAck");
    assert!(
        raw_buf.contains("ECHO_SECOND_FRAME"),
        "must receive post-disconnect Output bytes on SUB_RAW_OUTPUT stream"
    );

    // Verify viewer_b ALSO still received ECHO_SECOND_FRAME after viewer_drop disconnected!
    let mut vb_saw = false;
    for _ in 0..64 {
        if let BrokerMsg::Frame(upd) = viewer_b.recv_frame().unwrap().msg {
            let matched = upd.rows.iter().any(|r| r.text.contains("ECHO_SECOND_FRAME"));
            viewer_b
                .send_raw(
                    st_b.as_stream_id(),
                    RequestId::PUSH,
                    &ClientMsg::FrameAck { seq: upd.seq, credit: FrameCredit { max_frames: 4, max_bytes: 256 * 1024 } },
                )
                .unwrap();
            if matched {
                vb_saw = true;
                break;
            }
        }
    }
    assert!(vb_saw, "disconnecting one viewer must not break another viewer's frame slot");
    viewer_b.send_fin().unwrap();
    drop(viewer_b);

    // Real Pause / Resume behavior on Unix: while paused, PTY output must NOT be read into VT!
    #[cfg(unix)]
    {
        client.send_raw(st.as_stream_id(), RequestId::PUSH, &ClientMsg::Pause).unwrap();
        // Barrier: `list_sessions` roundtrips through the shard, ensuring `Pause` is active before `Write`.
        let _ = client.list_sessions().unwrap();
        client
            .send_raw(st.as_stream_id(), RequestId::PUSH, &ClientMsg::Write { data: b"PAUSED_MARKER_99\n".to_vec() })
            .unwrap();
        assert_eq!(
            client.wait(s2, WaitCondition::Text(String::from("PAUSED_MARKER_99")), 80).unwrap(),
            WaitOutcome::Timeout,
            "paused PTY must stop reading output until Resume"
        );
        client.send_raw(st.as_stream_id(), RequestId::PUSH, &ClientMsg::Resume).unwrap();
        assert_eq!(
            client.wait(s2, WaitCondition::Text(String::from("PAUSED_MARKER_99")), 2_000).unwrap(),
            WaitOutcome::Met,
            "resumed PTY must immediately drain buffered output"
        );

        // Test scrollback_limit = 0 applies DEFAULT_SCROLLBACK_LINES (NOT 0, which disables scrollback!):
        let spec_default_sb = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from("i=0; while [ $i -lt 25 ]; do printf 'SB_%02d\\r\\n' $i; i=$((i+1)); done; sleep 10"),
            ],
            cwd: None,
            env: Vec::new(),
            size: GridSize::new(40, 4).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_limit: 0,
            marks_key: None,
            resize_policy: ResizePolicy::LatestActive,
            record_dir: None,
            restore_from: None,
            image_files: false,
            kept: false,
        };
        let (s_sb, _) = client.spawn(spec_default_sb).unwrap();
        assert_eq!(client.wait(s_sb, WaitCondition::Text(String::from("SB_24")), 3_000).unwrap(), WaitOutcome::Met);
        let st_sb = NonZeroStreamId::new(2).unwrap();
        let sb_att = client
            .attach(
                s_sb,
                st_sb,
                SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
                None,
                0,
                FrameCredit { max_frames: 1, max_bytes: 65_536 },
            )
            .unwrap();
        if let BrokerMsg::Attached { scrollback_lines, .. } = sb_att {
            assert!(
                scrollback_lines > 0,
                "scrollback_limit=0 must map to DEFAULT_SCROLLBACK_LINES, not disable scrollback"
            );
        }
        client.detach(st_sb).unwrap();
        client.kill(s_sb).unwrap();

        // Test large 32 KiB write into a delayed-reading PTY so non-blocking write hits EAGAIN and
        // MUST arm Interest::writable to finish draining!
        let spec_bigw = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from(
                    "stty -echo raw; printf 'READY_BW\\r\\n'; sleep 0.08; grep -m1 -o BIG_WRITE_TAIL; sleep 10",
                ),
            ],
            cwd: None,
            env: Vec::new(),
            size: GridSize::new(80, 10).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_limit: 50,
            marks_key: None,
            resize_policy: ResizePolicy::LatestActive,
            record_dir: None,
            restore_from: None,
            image_files: false,
            kept: false,
        };
        let (s_bw, _) = client.spawn(spec_bigw).unwrap();
        assert_eq!(client.wait(s_bw, WaitCondition::Text(String::from("READY_BW")), 3_000).unwrap(), WaitOutcome::Met);
        let st_bw = NonZeroStreamId::new(3).unwrap();
        client
            .attach(
                s_bw,
                st_bw,
                SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
                None,
                0,
                FrameCredit { max_frames: 1, max_bytes: 65_536 },
            )
            .unwrap();
        let mut payload = vec![b'X'; 32 * 1024];
        payload.extend_from_slice(b"\nBIG_WRITE_TAIL\n");
        client.send_raw(st_bw.as_stream_id(), RequestId::PUSH, &ClientMsg::Write { data: payload }).unwrap();
        assert_eq!(
            client.wait(s_bw, WaitCondition::Text(String::from("BIG_WRITE_TAIL")), 5_000).unwrap(),
            WaitOutcome::Met,
            "large PTY write hitting EAGAIN must drain completely once writable"
        );
        client.detach(st_bw).unwrap();
        client.kill(s_bw).unwrap();
    }

    // Verify broker closes the client transport on protocol violation (`close_conn_transport`)!
    {
        let mut raw = apack_os::transport::connect_local_stream(handle.socket_path()).unwrap();
        // Oversized frame length header (> MAX_FRAME_BYTES) forces BrokerCore -> CloseConn.
        raw.write_all(&0xFFFF_FFFFu32.to_le_bytes()).unwrap();
        raw.flush().unwrap();
        let mut drain = [0u8; 256];
        // Drain optional error frame until server closes socket (Ok(0) or Err).
        let mut saw_close = false;
        for _ in 0..8 {
            match raw.read(&mut drain) {
                Ok(0) | Err(_) => {
                    saw_close = true;
                    break;
                }
                Ok(_) => {}
            }
        }
        assert!(saw_close, "broker must close transport immediately on protocol violation");
    }

    // Verify Resize took effect in ListSessions!
    let listed = client.list_sessions().unwrap();
    let s2_sum = listed.iter().find(|s| s.id == s2).unwrap();
    assert_eq!(s2_sum.size, GridSize::new(100, 30).unwrap());

    // Wait timeout check via Tier 2 poll_timers
    assert_eq!(
        client.wait(s2, WaitCondition::Text(String::from("NEVER_APPEARS_XYZ")), 40).unwrap(),
        WaitOutcome::Timeout
    );

    client.kill(s1).unwrap();
    client.kill(s2).unwrap();
    for _ in 0..100 {
        if client.list_sessions().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    client.release_lease(lease_id).unwrap();
    client.send_fin().unwrap();
    drop(client);
    // Broker must auto-exit cleanly after release_lease + disconnect with 0 sessions!
    handle.shutdown().unwrap();
}
