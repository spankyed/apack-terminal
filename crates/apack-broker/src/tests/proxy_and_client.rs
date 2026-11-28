use super::*;

#[test]
fn e2e_dumb_single_channel_proxy_splices_live_client() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-proxy");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    // Build an in-process bidirectional pipe pair connecting BrokerClient <-> run_proxy_over <-> BrokerServer!
    let (c2p_tx, c2p_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let (p2c_tx, p2c_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);

    let sock_clone = handle.socket_path().to_owned();
    let proxy_thread = std::thread::spawn(move || {
        let proxy_in = ChanReader { rx: c2p_rx, buf: Vec::new(), pos: 0 };
        let proxy_out = ChanWriter { tx: p2c_tx };
        run_proxy_over(proxy_in, proxy_out, || apack_os::transport::connect_local_stream(&sock_clone))
    });

    let client_transport =
        ChanDuplex { reader: ChanReader { rx: p2c_rx, buf: Vec::new(), pos: 0 }, writer: ChanWriter { tx: c2p_tx } };
    let mut client = BrokerClient::handshake(client_transport, &AuthConfig::None, "via-proxy").unwrap();
    assert!(client.list_sessions().unwrap().is_empty());
    let metrics = client.metrics().unwrap();
    assert_eq!(metrics.live_sessions, 0);
    // Drop client -> closes stdin to proxy -> proxy writes FIN_FRAME -> clean exit!
    drop(client);

    proxy_thread.join().unwrap().unwrap();

    // Also test proxy pre-first-response replay: attempt #1 connects to a dummy listener that
    // immediately hangs up without sending a byte; attempt #2 connects to the real BrokerServer!
    let dummy_sock = unique_endpoint(tmp.path(), "e2e-proxy-dummy");
    let sock_replay = unique_endpoint(tmp.path(), "e2e-proxy-real2");
    let mut rcfg = BrokerConfig::for_socket(&sock_replay);
    rcfg.shard_count = 1;
    let mut replay_handle = BrokerServer::bind(rcfg).unwrap().spawn().unwrap();

    let dummy_listener = apack_os::transport::bind_local_listener(&dummy_sock).unwrap();
    let dummy_accept = std::thread::spawn(move || {
        use interprocess::local_socket::traits::Listener as _;
        if let Ok(mut s) = dummy_listener.accept() {
            let mut drain = [0u8; 64];
            let _ = s.read(&mut drain);
            let _ = apack_os::transport::hang_up(&s);
        }
    });

    let (c2p_tx2, c2p_rx2) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let (p2c_tx2, p2c_rx2) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let real_sock = replay_handle.socket_path().to_owned();
    let dummy_sock_c = dummy_sock.clone();
    let mut attempt = 0u8;
    let replay_thread = std::thread::spawn(move || {
        let proxy_in = ChanReader { rx: c2p_rx2, buf: Vec::new(), pos: 0 };
        let proxy_out = ChanWriter { tx: p2c_tx2 };
        run_proxy_over(proxy_in, proxy_out, move || {
            attempt += 1;
            if attempt == 1 {
                apack_os::transport::connect_local_stream(&dummy_sock_c)
            } else {
                apack_os::transport::connect_local_stream(&real_sock)
            }
        })
    });
    let client_transport2 =
        ChanDuplex { reader: ChanReader { rx: p2c_rx2, buf: Vec::new(), pos: 0 }, writer: ChanWriter { tx: c2p_tx2 } };
    let mut client2 = BrokerClient::handshake(client_transport2, &AuthConfig::None, "via-proxy-replay").unwrap();
    assert!(client2.list_sessions().unwrap().is_empty());

    // connect_or_autostart(autostart=false) fails cleanly on missing path, succeeds on live broker
    let missing_sock = unique_endpoint(tmp.path(), "does-not-exist");
    assert!(connect_or_autostart(&missing_sock, false).is_err());
    assert!(connect_or_autostart(replay_handle.socket_path(), false).is_ok());

    drop(client2);
    replay_thread.join().unwrap().unwrap();
    let _ = dummy_accept.join();
    replay_handle.shutdown().unwrap();

    // BrokerServer::socket_path + BrokerServer::run serving a real connection + clean idle exit
    let sock_grace = unique_endpoint(tmp.path(), "e2e-grace");
    let mut gcfg = BrokerConfig::for_socket(&sock_grace);
    gcfg.start_grace = Duration::from_secs(5);
    gcfg.shard_count = 1;
    let gserver = BrokerServer::bind(gcfg).unwrap();
    assert_eq!(gserver.socket_path(), sock_grace);
    let run_thread = std::thread::spawn(move || gserver.run());
    let mut gclient = BrokerClient::connect(&sock_grace, &AuthConfig::None, "via-run").unwrap();
    assert!(gclient.list_sessions().unwrap().is_empty());
    gclient.send_fin().unwrap();
    drop(gclient);
    run_thread.join().unwrap().unwrap();

    // connect_or_autostart_with: autostart=false fails without spawning, autostart=true retries
    // across an async broker startup delay until server binds, and times out cleanly if spawner never binds.
    let auto_ok_sock = unique_endpoint(tmp.path(), "e2e-auto-ok");
    let mut spawned_count = 0usize;
    let (bg_tx, bg_rx) = std::sync::mpsc::sync_channel::<crate::BrokerHandle>(1);
    let stream_auto = connect_or_autostart_with(&auto_ok_sock, true, Duration::from_secs(2), |path| {
        spawned_count += 1;
        let p = path.to_owned();
        let tx = bg_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let mut scfg = BrokerConfig::for_socket(&p);
            scfg.shard_count = 1;
            let _ = tx.send(BrokerServer::bind(scfg).unwrap().spawn().unwrap());
        });
        Ok(None)
    })
    .unwrap();
    assert_eq!(spawned_count, 1);
    drop(stream_auto);
    let mut bg_handle = bg_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    bg_handle.shutdown().unwrap();

    let auto_timeout_sock = unique_endpoint(tmp.path(), "e2e-auto-timeout");
    let mut noauto_spawns = 0usize;
    assert!(
        connect_or_autostart_with(&auto_timeout_sock, false, Duration::from_secs(1), |_| {
            noauto_spawns += 1;
            Ok(None)
        })
        .is_err()
    );
    assert_eq!(noauto_spawns, 0, "autostart=false must not invoke spawner");

    let mut noop_spawns = 0usize;
    assert!(
        connect_or_autostart_with(&auto_timeout_sock, true, Duration::from_millis(35), |_| {
            noop_spawns += 1;
            Ok(None)
        })
        .is_err()
    );
    assert_eq!(noop_spawns, 1);

    // Fast-fail autostart (`M9`): when the spawned child exits immediately with stderr,
    // `connect_or_autostart_with` returns promptly (NOT waiting the 5s timeout) and includes stderr!
    let auto_fail_sock = unique_endpoint(tmp.path(), "e2e-auto-fail");
    let fail_started = std::time::Instant::now();
    let fail_err = connect_or_autostart_with(&auto_fail_sock, true, Duration::from_secs(5), |_| {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd.exe");
            c.args(["/C", "echo BROKER_BOOT_FAIL_DIAG 1>&2 & exit /B 1"]);
            c
        } else {
            let mut c = std::process::Command::new("/bin/sh");
            c.args(["-c", "echo BROKER_BOOT_FAIL_DIAG >&2; exit 1"]);
            c
        };
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        Ok(Some(cmd.spawn()?))
    })
    .unwrap_err();
    assert!(fail_started.elapsed() < Duration::from_secs(2), "must fail fast on child exit, not wait 5s");
    assert!(
        fail_err.to_string().contains("BROKER_BOOT_FAIL_DIAG"),
        "must surface captured stderr on startup failure: {fail_err}"
    );

    // Concurrent autostart bind race (`HELD_EXIT`): if spawned child exits with HELD_EXIT while
    // the winning broker binds the socket, `connect_or_autostart_with` still connects cleanly!
    let auto_held_sock = unique_endpoint(tmp.path(), "e2e-auto-held");
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel::<crate::BrokerHandle>(1);
    let stream_held = connect_or_autostart_with(&auto_held_sock, true, Duration::from_secs(2), |path| {
        let p = path.to_owned();
        let tx = held_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            let mut scfg = BrokerConfig::for_socket(&p);
            scfg.shard_count = 1;
            let _ = tx.send(BrokerServer::bind(scfg).unwrap().spawn().unwrap());
        });
        let held_code = apack_os::transport::HELD_EXIT;
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd.exe");
            c.args(["/C", &format!("exit /B {held_code}")]);
            c
        } else {
            let mut c = std::process::Command::new("/bin/sh");
            c.args(["-c", &format!("exit {held_code}")]);
            c
        };
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        Ok(Some(cmd.spawn()?))
    })
    .unwrap();
    drop(stream_held);
    let mut held_handle = held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    held_handle.shutdown().unwrap();

    // Dropping BrokerHandle shuts down the broker and releases its SocketHold so re-binding succeeds!
    let sock_drop = unique_endpoint(tmp.path(), "e2e-drop");
    let mut dcfg = BrokerConfig::for_socket(&sock_drop);
    dcfg.shard_count = 1;
    let dhandle = BrokerServer::bind(dcfg.clone()).unwrap().spawn().unwrap();
    drop(dhandle);
    let mut rebound = BrokerServer::bind(dcfg).unwrap().spawn().unwrap();
    rebound.shutdown().unwrap();

    handle.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn e2e_posix_signal_terminates_session_group() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-sig");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    let mut client = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "sig-test").unwrap();
    let (sid, _) = client.spawn(echo_command_spec("SIGNAL_READY", None, None)).unwrap();
    assert_eq!(client.wait(sid, WaitCondition::Text(String::from("SIGNAL_READY")), 5_000).unwrap(), WaitOutcome::Met);
    let ev_stream = NonZeroStreamId::new(1).unwrap();
    client
        .attach(
            sid,
            ev_stream,
            SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 1, max_bytes: 65_536 },
        )
        .unwrap();
    client.signal(sid, SignalKind::Term, SignalScope::Group).unwrap();
    let exited = loop {
        let frame = client.recv_frame().unwrap();
        if let BrokerMsg::Exited { exit_code, signal } = frame.msg {
            break (exit_code, signal);
        }
    };
    assert!(exited.0.is_some() || exited.1.is_some(), "expected exit code or signal: {exited:?}");
    client.send_fin().unwrap();
    handle.shutdown().unwrap();
}

#[test]
fn no_such_session_error_returned_cleanly() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-nosess");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    let mut client = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "err-test").unwrap();
    let missing = SessionId::new(999).unwrap();
    assert!(matches!(client.kill(missing), Err(ClientError::Broker { code: ErrorCode::NoSuchSession, .. })));
    assert!(matches!(
        client.owns(missing, SignalTarget::new(1).unwrap()),
        Err(ClientError::Broker { code: ErrorCode::NoSuchSession, .. })
    ));
    assert!(matches!(client.keep(missing, true), Err(ClientError::Broker { code: ErrorCode::NoSuchSession, .. })));
    assert!(matches!(client.seek(missing, 0), Err(ClientError::Broker { code: ErrorCode::NoSuchSession, .. })));
    client.send_fin().unwrap();
    handle.shutdown().unwrap();
}

#[test]
fn client_mock_transport_fin_req_id_and_multi_frame() {
    struct MockDuplex {
        incoming: io::Cursor<Vec<u8>>,
        outgoing: Vec<u8>,
    }
    impl Read for MockDuplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.incoming.read(buf)
        }
    }
    impl Write for MockDuplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.outgoing.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Encode HelloOk + Ok back-to-back into a single incoming byte slice so read_one_from_wire
    // exercises multi-frame buffer draining in one transport read!
    let mut wire = Vec::new();
    BrokerMsg::HelloOk {
        protocol_version: PROTOCOL_VERSION,
        auth_required: false,
        server_nonce: [0x11; 32],
        word_delimiters: String::from(" ,:;"),
        capabilities: 0x0000_00AB,
    }
    .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(1), &mut wire)
    .unwrap();
    BrokerMsg::Ok.encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(2), &mut wire).unwrap();
    BrokerMsg::Bell.encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(3), &mut wire).unwrap();

    let mock = MockDuplex { incoming: io::Cursor::new(wire), outgoing: Vec::new() };
    let mut client = BrokerClient::handshake(mock, &AuthConfig::None, "mock").unwrap();
    assert_eq!(client.word_delimiters(), " ,:;");
    assert_eq!(client.capabilities(), 0x0000_00AB);

    // Consumes second frame from rx_buf without another transport read.
    client.release_lease(7).unwrap();
    // Third frame is Bell instead of Ok -> expect_ok must reject with UnexpectedReply!
    assert!(matches!(client.release_lease(8), Err(ClientError::UnexpectedReply(_))));
    let req4 = client.alloc_req_id();
    let req5 = client.alloc_req_id();
    assert_eq!(req4.raw(), 4);
    assert_eq!(req5.raw(), 5);

    // send_fin must write the exact 4-byte FIN_FRAME `[0, 0, 0, 0]` to the wire.
    client.send_fin().unwrap();
    let mut dummy_wire = Vec::new();
    BrokerMsg::HelloOk {
        protocol_version: PROTOCOL_VERSION,
        auth_required: false,
        server_nonce: [0x22; 32],
        word_delimiters: String::from(" "),
        capabilities: 0,
    }
    .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(1), &mut dummy_wire)
    .unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let duplex = ChanDuplex {
        reader: ChanReader { rx: std::sync::mpsc::channel().1, buf: dummy_wire, pos: 0 },
        writer: ChanWriter { tx },
    };
    let mut c2 = BrokerClient::handshake(duplex, &AuthConfig::None, "fin-check").unwrap();
    let _hello_bytes = rx.recv().unwrap();
    c2.send_fin().unwrap();
    let fin_bytes = rx.recv().unwrap();
    assert_eq!(fin_bytes, FIN_FRAME);

    // Auth downgrade check: client configured with AuthConfig::Required MUST reject auth_required=false!
    let mut noauth_wire = Vec::new();
    BrokerMsg::HelloOk {
        protocol_version: PROTOCOL_VERSION,
        auth_required: false,
        server_nonce: [0x33; 32],
        word_delimiters: String::from(" "),
        capabilities: 0,
    }
    .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(1), &mut noauth_wire)
    .unwrap();
    let mock_downgrade = MockDuplex { incoming: io::Cursor::new(noauth_wire), outgoing: Vec::new() };
    let required = AuthConfig::Required(derive_auth_secret(b"client-requires-auth"));
    assert!(
        matches!(
            BrokerClient::handshake(mock_downgrade, &required, "downgrade-check"),
            Err(ClientError::AuthFailed(_))
        ),
        "client with AuthConfig::Required must reject server advertising auth_required=false"
    );
}

#[test]
fn timeout_and_idle_compression_helpers_unit() {
    // Pure timeout helpers for Reactor and Shard + cooperative idle compression helper (cross-platform!).
    let now = apack_core::time::TimestampMs::from_millis(1_000);
    assert_eq!(reactor_wait_timeout(None, now, true, Duration::from_millis(50), false), Duration::from_secs(1));
    assert_eq!(reactor_wait_timeout(None, now, false, Duration::from_millis(50), false), Duration::from_millis(50));
    assert_eq!(
        reactor_wait_timeout(
            Some(apack_core::time::TimestampMs::from_millis(1_120)),
            now,
            true,
            Duration::from_secs(5),
            false
        ),
        Duration::from_millis(120)
    );
    assert_eq!(reactor_wait_timeout(None, now, true, Duration::from_secs(5), true), Duration::from_millis(25));

    assert_eq!(resolve_scrollback_lines(0), 10_000);
    assert_eq!(resolve_scrollback_lines(500), 500);

    assert_eq!(
        shard_session_wait_timeout(None, now, true, false, false, false, Duration::ZERO, Duration::from_secs(2)),
        Duration::from_millis(20)
    );
    assert_eq!(
        shard_session_wait_timeout(None, now, false, true, false, false, Duration::ZERO, Duration::from_secs(2)),
        Duration::from_millis(20)
    );
    assert_eq!(
        shard_session_wait_timeout(None, now, false, false, true, false, Duration::ZERO, Duration::from_secs(2)),
        Duration::from_millis(20)
    );
    assert_eq!(
        shard_session_wait_timeout(
            None,
            now,
            false,
            false,
            false,
            true,
            Duration::from_secs(5),
            Duration::from_secs(2)
        ),
        Duration::from_secs(5)
    );
    assert_eq!(
        shard_session_wait_timeout(
            None,
            now,
            false,
            false,
            false,
            false,
            Duration::from_millis(500),
            Duration::from_secs(2)
        ),
        Duration::from_millis(1_500)
    );
    assert_eq!(
        shard_session_wait_timeout(
            None,
            now,
            false,
            false,
            false,
            false,
            Duration::from_secs(2),
            Duration::from_secs(2)
        ),
        Duration::from_millis(5)
    );
    assert_eq!(
        shard_session_wait_timeout(
            Some(apack_core::time::TimestampMs::from_millis(1_010)),
            now,
            true,
            false,
            false,
            false,
            Duration::ZERO,
            Duration::from_secs(2)
        ),
        Duration::from_millis(10)
    );

    let mut compressed = false;
    let mut calls = 0usize;
    maybe_compress_idle(&mut compressed, Duration::from_millis(10), Duration::from_millis(20), || {
        calls += 1;
        apack_core::vt::CompressStep::Done
    });
    assert_eq!(calls, 0);
    assert!(!compressed);
    maybe_compress_idle(&mut compressed, Duration::from_millis(20), Duration::from_millis(20), || {
        calls += 1;
        apack_core::vt::CompressStep::More
    });
    assert_eq!(calls, 1);
    assert!(!compressed);
    maybe_compress_idle(&mut compressed, Duration::from_millis(25), Duration::from_millis(20), || {
        calls += 1;
        apack_core::vt::CompressStep::Done
    });
    assert_eq!(calls, 2);
    assert!(compressed);
    maybe_compress_idle(&mut compressed, Duration::from_millis(100), Duration::from_millis(20), || {
        calls += 1;
        apack_core::vt::CompressStep::Done
    });
    assert_eq!(calls, 2);

    // Pure proxy replay/reconnect decision helpers
    assert!(should_buffer_replay(false, false));
    assert!(!should_buffer_replay(true, false));
    assert!(!should_buffer_replay(false, true));
    assert!(!should_buffer_replay(true, true));

    assert!(should_reconnect_proxy(true, false, false, false));
    assert!(should_reconnect_proxy(true, false, true, true));
    assert!(!should_reconnect_proxy(true, false, true, false));
    assert!(!should_reconnect_proxy(true, true, false, true));
    assert!(!should_reconnect_proxy(false, false, false, true));

    // Pure shutdown deadline, should_exit, pending_lists prune, and stalled-frame selector helpers
    let t0 = std::time::Instant::now();
    let dl = compute_shutdown_deadline(t0, Duration::from_secs(2));
    assert_eq!(dl.duration_since(t0), Duration::from_secs(7));
    assert!(!reactor_should_exit(false, true, t0, Some(t0)));
    assert!(reactor_should_exit(true, true, t0, Some(dl)));
    assert!(!reactor_should_exit(true, false, t0, Some(dl)));
    assert!(reactor_should_exit(true, false, dl, Some(dl)));

    let c1 = apack_core::lease::ConnId::new(1).unwrap();
    let c2 = apack_core::lease::ConnId::new(2).unwrap();
    let mut plist = std::collections::HashMap::new();
    plist
        .insert(1u64, PendingList { conn: c1, req_id: RequestId::from_raw(1), remaining_shards: 1, collected: vec![] });
    plist
        .insert(2u64, PendingList { conn: c2, req_id: RequestId::from_raw(2), remaining_shards: 1, collected: vec![] });
    prune_pending_lists_for_conn(&mut plist, c1);
    assert!(!plist.contains_key(&1));
    assert!(plist.contains_key(&2));

    let s1 = NonZeroStreamId::new(1).unwrap();
    let s2 = NonZeroStreamId::new(2).unwrap();
    let mut stalled = std::collections::HashMap::new();
    let dummy_frame = Box::new(apack_proto::messages::FrameUpdate {
        seq: 1,
        epoch: 1,
        size: GridSize::new(80, 24).unwrap(),
        first_active_line: 0,
        scrollback_lines: 0,
        scroll_delta: 0,
        cursor: apack_proto::types::CursorState {
            col: 0,
            row: 0,
            shape: apack_proto::types::CursorShape::Block,
            blinking: false,
            hidden: false,
        },
        modes: apack_proto::types::ViewModes::default(),
        palette: None,
        rows: Vec::new(),
        images_gen: 0,
        full_sync: true,
    });
    let a1 = apack_core::broker::StreamAttachmentId::new(10).unwrap();
    let a2 = apack_core::broker::StreamAttachmentId::new(11).unwrap();
    let a3 = apack_core::broker::StreamAttachmentId::new(12).unwrap();
    stalled.insert(
        a1,
        crate::transport_loop::StalledFrame::Buffered {
            stream_id: s1,
            session: SessionId::new(1).unwrap(),
            frame: dummy_frame.clone(),
            charged_bytes: 128,
            reason: apack_core::broker::FrameStallReason::OutboundOrBudget,
        },
    );
    stalled.insert(
        a2,
        crate::transport_loop::StalledFrame::Buffered {
            stream_id: s2,
            session: SessionId::new(2).unwrap(),
            frame: dummy_frame.clone(),
            charged_bytes: 256,
            reason: apack_core::broker::FrameStallReason::OutboundOrBudget,
        },
    );
    stalled.insert(
        a3,
        crate::transport_loop::StalledFrame::Buffered {
            stream_id: s2,
            session: SessionId::new(2).unwrap(),
            frame: dummy_frame,
            charged_bytes: 64,
            reason: apack_core::broker::FrameStallReason::Pacer,
        },
    );
    let mut queued = std::collections::HashMap::new();
    queued.insert(a1, (s1, SessionId::new(1).unwrap()));
    // Only budget-stalled attachments without a queued P1 frame are returned (pacer-stalled `a3` is skipped).
    assert_eq!(ready_stalled_attachments(&stalled, &queued), vec![a2]);
    queued.insert(a2, (s2, SessionId::new(2).unwrap()));
    let freed = prune_ended_session_frames(&mut stalled, &mut queued, SessionId::new(1).unwrap());
    assert_eq!(freed, 128);
    assert!(!stalled.contains_key(&a1) && stalled.contains_key(&a2) && stalled.contains_key(&a3));
    assert!(!queued.contains_key(&a1) && queued.contains_key(&a2));

    // Bounded shard event draining + wait clamping (`H6`)
    let (tx, rx) = std::sync::mpsc::channel::<u32>();
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    tx.send(3).unwrap();
    let mut seen = Vec::new();
    assert!(drain_events_bounded(&rx, 2, |v| seen.push(v)));
    assert_eq!(seen, vec![1, 2]);
    assert_eq!(clamp_reactor_wait(Duration::from_secs(1), true), Duration::ZERO);
    assert_eq!(clamp_reactor_wait(Duration::from_secs(1), false), Duration::from_secs(1));
    assert!(!drain_events_bounded(&rx, 2, |v| seen.push(v)));
    assert_eq!(seen, vec![1, 2, 3]);

    // RAII per-connection ShardCmdPermit (`L7`)
    let inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let p1 = crate::shard::ShardCmdPermit::try_acquire(&inflight, 2).expect("permit 1");
    let p2 = crate::shard::ShardCmdPermit::try_acquire(&inflight, 2).expect("permit 2");
    assert!(crate::shard::ShardCmdPermit::try_acquire(&inflight, 2).is_none());
    assert_eq!(inflight.load(std::sync::atomic::Ordering::Acquire), 2);
    drop(p1);
    assert_eq!(inflight.load(std::sync::atomic::Ordering::Acquire), 1);
    let p3 = crate::shard::ShardCmdPermit::try_acquire(&inflight, 2).expect("permit 3 after drop");
    drop((p2, p3));
    assert_eq!(inflight.load(std::sync::atomic::Ordering::Acquire), 0);

    // Raw-output in-flight high/low-water backpressure helpers (`H6`)
    let sess_lo = crate::shard::RAW_OUTPUT_SESSION_LOW_WATER;
    let shard_lo = crate::shard::RAW_OUTPUT_SHARD_LOW_WATER;
    assert_eq!(sess_lo, 1_048_576);
    assert_eq!(shard_lo, 4_194_304);
    assert!(!crate::shard::should_pause_raw_inflight(0, 0));
    assert!(!crate::shard::should_pause_raw_inflight(2 * 1024 * 1024 - 1, 8 * 1024 * 1024 - 1));
    assert!(crate::shard::should_pause_raw_inflight(2 * 1024 * 1024, 0));
    assert!(crate::shard::should_pause_raw_inflight(0, 8 * 1024 * 1024));
    assert!(crate::shard::crossed_raw_low_water(100, 99, 100));
    assert!(!crate::shard::crossed_raw_low_water(100, 100, 100));
    assert!(!crate::shard::crossed_raw_low_water(99, 50, 100));
    assert!(crate::shard::should_notify_raw_drained(sess_lo, sess_lo - 1, 0, 0));
    assert!(crate::shard::should_notify_raw_drained(0, 0, shard_lo, shard_lo - 1));
    assert!(!crate::shard::should_notify_raw_drained(sess_lo, sess_lo, shard_lo, shard_lo));
}

#[cfg(unix)]
#[test]
fn transport_unix_helpers_unit() {
    assert_eq!(classify_nonblocking_err(&io::Error::from(io::ErrorKind::WouldBlock)), NonBlockErr::WouldBlock);
    assert_eq!(classify_nonblocking_err(&io::Error::from(io::ErrorKind::Interrupted)), NonBlockErr::Interrupted);
    assert_eq!(classify_nonblocking_err(&io::Error::from(io::ErrorKind::BrokenPipe)), NonBlockErr::Fatal);

    let mut q = VecDeque::new();
    q.push_back((vec![1, 2, 3, 4, 5], 0));
    q.push_back((vec![6, 7, 8, 9], 0));
    // Partial advance within first buffer (off -> 2, rem = 3)
    advance_write_queue(&mut q, 2);
    assert_eq!(q.len(), 2);
    assert_eq!(q.front().unwrap().1, 2);
    // Advance 5 bytes: consumes rem=3 of first buffer (5 - 3 = 2 != 5 / 3 = 1!) + 2 of second buffer
    advance_write_queue(&mut q, 5);
    assert_eq!(q.len(), 1);
    assert_eq!(q.front().unwrap().1, 2);
    // Exact drain of remaining 2 bytes
    advance_write_queue(&mut q, 2);
    assert!(q.is_empty());
    // Extra advance on empty queue is a clean no-op
    advance_write_queue(&mut q, 5);
    assert!(q.is_empty());

    // Unix connection accept & write_queue watermark boundary helpers
    assert!(can_accept_unix_conn(false, 0, true, true));
    assert!(!can_accept_unix_conn(true, 0, true, true));
    assert!(!can_accept_unix_conn(false, crate::transport_loop::MAX_CONNECTIONS, true, true));
    assert!(!can_accept_unix_conn(false, 0, false, true));
    assert!(!can_accept_unix_conn(false, 0, true, false));

    assert!(write_queue_can_pull(31, 131_071));
    assert!(!write_queue_can_pull(32, 0));
    assert!(!write_queue_can_pull(0, 131_072));
}

#[test]
fn shard_direct_handle_load_shutdown_drop_and_no_such_session() {
    let tmp = tempdir().unwrap();
    let rec_drop = tmp.path().join("shard-drop-wal");
    let (tx, rx) = std::sync::mpsc::channel::<ReactorEvent>();
    let shared = ShardShared {
        image_store: std::sync::Arc::new(std::sync::Mutex::new(apack_vt_ghostty::ImageStore::default())),
        wal_registry: apack_wal::RecordingRegistry::new(),
        wal_config: apack_wal::WalConfig::default(),
        idle_compress_after: Duration::from_secs(5),
        kill_escalate_after: Duration::from_millis(50),
        engine_abi_hash: engine_abi_hash(),
    };
    let noop_wake: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(|| {});
    let s0 = spawn_shard(0, shared.clone(), tx.clone(), std::sync::Arc::clone(&noop_wake)).unwrap();
    let s1 = spawn_shard(1, shared, tx, noop_wake).unwrap();

    let conn = apack_core::lease::ConnId::new(1).unwrap();
    // Missing session on s1 -> send_no_such_session must emit ErrorCode::NoSuchSession!
    s1.send(ShardCmd::Kill { session_id: SessionId::new(999).unwrap(), conn, req_id: RequestId::from_raw(1) });
    let err_ev = loop {
        let ev = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        if matches!(ev, ReactorEvent::SendMsg { .. }) {
            break ev;
        }
    };
    assert!(matches!(
        err_ev,
        ReactorEvent::SendMsg { msg: BrokerMsg::Error { code: ErrorCode::NoSuchSession, .. }, .. }
    ));

    // Spawn 2 sessions on s0 -> s0.session_load() == 2, s1.session_load() == 0, pick_least_loaded_shard == 1!
    for sid_num in [1u64, 2u64] {
        s0.send(ShardCmd::Spawn {
            session_id: SessionId::new(sid_num).unwrap(),
            conn,
            req_id: RequestId::from_raw(sid_num as u32 + 10),
            spec: Box::new(echo_command_spec("SHARD_LOAD_TEST", None, None)),
        });
    }
    let mut spawned = 0usize;
    while spawned < 2 {
        if matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), ReactorEvent::SessionRegistered { .. }) {
            spawned += 1;
        }
    }
    assert_eq!(s0.session_load(), 2);
    assert_eq!(s1.session_load(), 0);
    assert_eq!(s1.telemetry(), (0, 0));
    let shards = [s0, s1];
    assert_eq!(pick_least_loaded_shard(&shards), 1);
    let [mut s0, s1] = shards;

    // Explicit ShardHandle::shutdown() must stop the shard thread BEFORE Drop!
    s0.shutdown();
    assert_eq!(s0.session_load(), 0);

    // Spawn a recorded session on s1 and drop s1 WITHOUT calling shutdown() -> Drop must seal WAL!
    s1.send(ShardCmd::Spawn {
        session_id: SessionId::new(3).unwrap(),
        conn,
        req_id: RequestId::from_raw(30),
        spec: Box::new(echo_command_spec("SHARD_DROP_SEAL", Some(rec_drop.to_string_lossy().into_owned()), None)),
    });
    while !matches!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), ReactorEvent::SessionRegistered { .. }) {}
    drop(s1);
    let reg = apack_wal::RecordingRegistry::new();
    let reader = apack_wal::WalReader::open(&rec_drop, &reg).unwrap();
    assert!(reader.segments()[0].sealed, "Drop on ShardHandle must shut down shard and seal active WAL");
}

#[test]
fn shard_keyframe_seek_fallback_and_exit_status_unit() {
    let ok0 = portable_pty::ExitStatus::with_exit_code(0);
    assert_eq!(parse_exit_status(&ok0), (Some(0), None));
    let ok42 = portable_pty::ExitStatus::with_exit_code(42);
    assert_eq!(parse_exit_status(&ok42), (Some(42), None));
    let sig = portable_pty::ExitStatus::with_signal("Terminated");
    let (code, sig_name) = parse_exit_status(&sig);
    assert_eq!(code, None);
    assert!(sig_name.is_some());

    // Capture keyframe with non-zero scrollback history, then restore via BOTH matching ABI
    // (lossless snapshot) AND mismatched ABI (visual text fallback with multiple + empty rows).
    let size = GridSize::new(40, 4).unwrap();
    let cell_px = CellPx { width: 8, height: 16 };
    let mut cfg = GhosttyConfig::new(size);
    cfg.scrollback_lines = 100;
    let mut engine = GhosttyEngine::new(cfg).unwrap();
    let mut evs = Vec::new();
    engine.feed(b"HIST_LINE_1\r\nHIST_LINE_2\r\n\r\nVP_LINE_A\r\nVP_LINE_B\r\nVP_LINE_C", &mut evs);
    assert!(engine.meta().scrollback_lines > 0);

    let machine = SessionMachine::new(
        SessionId::new(1).unwrap(),
        engine,
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        cell_px,
    );
    let abi = engine_abi_hash();
    let kf = capture_keyframe(&machine, &abi);
    assert!(kf.visual.primary_rows.iter().any(|r| r.text.contains("HIST_LINE_1")));
    assert!(kf.visual.primary_rows.iter().any(|r| r.text.contains("VP_LINE_C")));

    let slice = apack_wal::SeekSlice { segment_id: 1, keyframe_at_ms: 0, keyframe: kf, tail_records: Vec::new() };

    let img_store = std::sync::Arc::new(std::sync::Mutex::new(apack_vt_ghostty::ImageStore::default()));
    // 1. Lossless restore on matching ABI
    let eng_same = build_keyframe_engine(&slice, &abi, &img_store, 100).unwrap();
    let mut hits = Vec::new();
    let m1 = eng_same.meta();
    eng_same.search_lines(0, m1.first_active_line + u64::from(m1.size.rows.get()), "HIST_LINE_1", true, 16, &mut hits);
    assert!(!hits.is_empty());

    // 2. Visual fallback on mismatched ABI: verify exact row separation (no missing or extra leading CRLF!)
    let wrong_abi = [0xEE; 32];
    let eng_fallback = build_keyframe_engine(&slice, &wrong_abi, &img_store, 100).unwrap();
    let m2 = eng_fallback.meta();
    let mut fb_rows = Vec::new();
    eng_fallback.extract_history_rows(0, m2.first_active_line as u32, usize::MAX, &mut fb_rows);
    let vp_idx: Vec<u16> = (0..m2.size.rows.get()).collect();
    eng_fallback.extract_viewport_rows(&vp_idx, &mut fb_rows);
    let lines: Vec<&str> = fb_rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(lines[0], "HIST_LINE_1", "row 0 must not have a leading blank line: {lines:?}");
    assert_eq!(lines[1], "HIST_LINE_2", "row 1 must be separated by CRLF: {lines:?}");
    assert_eq!(lines[2], "", "blank row 2 must be preserved: {lines:?}");
    assert_eq!(lines[3], "VP_LINE_A", "row 3 must follow blank row: {lines:?}");

    // 3. Alt-screen capture populates `alt_rows` and ABI-mismatch fallback restores alt screen cleanly!
    let mut alt_eng = GhosttyEngine::new(GhosttyConfig::new(size)).unwrap();
    let mut alt_evs = Vec::new();
    alt_eng.feed(b"\x1b[?1049hALT_SCREEN_BODY\x1b[2;5H", &mut alt_evs);
    assert!(alt_eng.meta().modes.alt_screen);
    let alt_machine = SessionMachine::new(
        SessionId::new(2).unwrap(),
        alt_eng,
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        cell_px,
    );
    let alt_kf = capture_keyframe(&alt_machine, &abi);
    assert!(alt_kf.visual.active_alt_screen);
    assert!(alt_kf.visual.alt_rows.iter().any(|r| r.text.contains("ALT_SCREEN_BODY")));
    let alt_slice =
        apack_wal::SeekSlice { segment_id: 1, keyframe_at_ms: 0, keyframe: alt_kf, tail_records: Vec::new() };
    let alt_fallback = build_keyframe_engine(&alt_slice, &wrong_abi, &img_store, 100).unwrap();
    assert!(alt_fallback.meta().modes.alt_screen, "alt-screen fallback must enter alt screen");
    assert_eq!(alt_fallback.meta().cursor.row, 1);
    assert_eq!(alt_fallback.meta().cursor.col, 4);
    let mut alt_fb_rows = Vec::new();
    alt_fallback.extract_viewport_rows(&[0], &mut alt_fb_rows);
    assert!(alt_fb_rows[0].text.contains("ALT_SCREEN_BODY"));
    assert_eq!(lines[4], "VP_LINE_B", "row 4 must follow row 3: {lines:?}");
    assert_eq!(lines[5], "VP_LINE_C", "row 5 must follow row 4: {lines:?}");

    // 4. CLI `-h`/`--help` (`M16`) and `inspect --json` trailing newline:
    use crate::config::{CLI_HELP, CliCommand, INSPECT_HELP, PROXY_HELP, SERVE_HELP, VERSION_HELP, parse_cli};
    assert_eq!(parse_cli(&["-h".into()]), Ok(CliCommand::Help(CLI_HELP)));
    assert_eq!(parse_cli(&["--help".into()]), Ok(CliCommand::Help(CLI_HELP)));
    assert!(parse_cli(&["--help".into(), "--bogus".into()]).is_err());
    assert_eq!(parse_cli(&["help".into()]), Ok(CliCommand::Help(CLI_HELP)));
    assert_eq!(parse_cli(&["help".into(), "serve".into()]), Ok(CliCommand::Help(SERVE_HELP)));
    assert_eq!(parse_cli(&["help".into(), "proxy".into()]), Ok(CliCommand::Help(PROXY_HELP)));
    assert_eq!(parse_cli(&["help".into(), "inspect".into()]), Ok(CliCommand::Help(INSPECT_HELP)));
    assert_eq!(parse_cli(&["help".into(), "version".into()]), Ok(CliCommand::Help(VERSION_HELP)));
    assert!(parse_cli(&["help".into(), "bogus".into()]).is_err());
    assert!(parse_cli(&["help".into(), "serve".into(), "extra".into()]).is_err());
    assert_eq!(parse_cli(&["serve".into(), "--help".into()]), Ok(CliCommand::Help(SERVE_HELP)));
    assert_eq!(parse_cli(&["serve".into(), "-h".into()]), Ok(CliCommand::Help(SERVE_HELP)));
    assert_eq!(parse_cli(&["proxy".into(), "--help".into()]), Ok(CliCommand::Help(PROXY_HELP)));
    assert_eq!(parse_cli(&["inspect".into(), "-h".into()]), Ok(CliCommand::Help(INSPECT_HELP)));
    assert_eq!(parse_cli(&["version".into(), "--help".into()]), Ok(CliCommand::Help(VERSION_HELP)));
    assert_eq!(parse_cli(&["version".into(), "-h".into()]), Ok(CliCommand::Help(VERSION_HELP)));
    assert!(parse_cli(&["version".into(), "--bogus".into(), "--help".into()]).is_err());
    assert!(parse_cli(&["--version".into(), "--help".into()]).is_err());
    let sample_report = crate::inspect::build_report(&[], apack_proto::types::MetricsSnapshot::default());
    assert!(crate::inspect::format_report(&sample_report, true).ends_with('\n'));

    // `reap_child_in_background` must actually wait/reap an exited child so it never lingers as a zombie.
    #[cfg(unix)]
    {
        let child = std::process::Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().expect("spawn short child");
        let pid = child.id();
        crate::proxy::reap_child_in_background(child);
        let dl = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < dl {
            if apack_os::signal::probe_child_exit_unreaped(pid) == apack_os::signal::ChildExitProbe::NotChild {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            apack_os::signal::probe_child_exit_unreaped(pid),
            apack_os::signal::ChildExitProbe::NotChild,
            "reap_child_in_background must reap the child process"
        );
    }
}

#[test]
fn pr6_proxy_large_full_duplex_reconnect_replay_never_deadlocks() {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempdir().unwrap();
    let sock1 = unique_endpoint(tmp.path(), "pr6-dead-1");
    let sock2 = unique_endpoint(tmp.path(), "pr6-duplex-2");
    #[cfg(unix)]
    {
        if let Some(p) = std::path::Path::new(&sock1).parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        if let Some(p) = std::path::Path::new(&sock2).parent() {
            std::fs::create_dir_all(p).unwrap();
        }
    }

    // Listener 1 accepts and immediately closes without sending a single byte (simulating idle broker exit).
    let l1 = apack_os::transport::bind_local_listener(&sock1).unwrap();
    let t1 = std::thread::spawn(move || {
        use interprocess::local_socket::traits::Listener as _;
        if let Ok(s) = l1.accept() {
            drop(s);
        }
    });

    // Listener 2 echoes back 256 KiB of responses concurrently while reading 512 KiB of replayed client bytes!
    // If the proxy wrote the 512 KiB replay synchronously on the sole reader thread, socket buffers would deadlock.
    let l2 = apack_os::transport::bind_local_listener(&sock2).unwrap();
    let total_payload = 512 * 1024;
    let t2 = std::thread::spawn(move || {
        use interprocess::local_socket::traits::Listener as _;
        use std::io::Read;
        let s = std::sync::Arc::new(l2.accept().expect("accept l2"));
        let sw = std::sync::Arc::clone(&s);
        let reply_thread = std::thread::spawn(move || {
            let mut tx: &interprocess::local_socket::Stream = &sw;
            tx.write_all(&vec![b'R'; 256 * 1024]).expect("write 256 KiB concurrently");
            tx.flush().expect("flush");
        });
        let mut rx: &interprocess::local_socket::Stream = &s;
        let mut received = Vec::with_capacity(total_payload + 4);
        let mut buf = [0u8; 16_384];
        while received.len() < total_payload + 4 {
            let n = rx.read(&mut buf).expect("read replayed bytes");
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }
        reply_thread.join().expect("reply thread");
        received
    });

    let input_bytes = vec![b'C'; total_payload];
    let attempts = AtomicUsize::new(0);
    let mut stdout_buf = Vec::new();
    let s1_clone = sock1.clone();
    let s2_clone = sock2.clone();
    run_proxy_over(std::io::Cursor::new(input_bytes.clone()), &mut stdout_buf, move || {
        if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
            apack_os::transport::connect_local_stream(&s1_clone)
        } else {
            apack_os::transport::connect_local_stream(&s2_clone)
        }
    })
    .expect("512 KiB full-duplex reconnect replay must succeed without deadlock");

    let _ = t1.join();
    let server_got = t2.join().expect("l2 thread");
    assert_eq!(stdout_buf.len(), 256 * 1024);
    assert!(stdout_buf.iter().all(|&b| b == b'R'));
    assert_eq!(&server_got[..total_payload], input_bytes.as_slice());
    assert_eq!(&server_got[total_payload..], &apack_proto::FIN_FRAME);

    // Also verify consistent `BrokenPipe` error when replay buffer overflows (> 1 MiB) before
    // `stream1` hits EOF without sending any bytes.
    let sock_ovf = unique_endpoint(tmp.path(), "pr6-overflow-1");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_ovf).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let l_ovf = apack_os::transport::bind_local_listener(&sock_ovf).unwrap();
    let t_ovf = std::thread::spawn(move || {
        use interprocess::local_socket::traits::Listener as _;
        use std::io::Read;
        if let Ok(mut s) = l_ovf.accept() {
            let mut sink = [0u8; 32_768];
            let mut got = 0usize;
            while got < 1_200_000 {
                match s.read(&mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n,
                }
            }
            drop(s);
        }
    });
    let big_input = std::io::Cursor::new(vec![b'X'; 1_200_000]);
    let mut ovf_out = Vec::new();
    let ovf_err = run_proxy_over(big_input, &mut ovf_out, move || apack_os::transport::connect_local_stream(&sock_ovf))
        .expect_err("overflowed replay buffer before first broker byte must return BrokenPipe");
    assert_eq!(ovf_err.kind(), std::io::ErrorKind::BrokenPipe);
    t_ovf.join().expect("join t_ovf");
}

#[test]
fn pr3_proxy_autostart_retries_held_exit_clean_exit_timeout_stderr_and_stdin_err() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let tmp = tempdir().unwrap();

    // 1. `connect_or_autostart_with` retries spawning when attempt 1 exits with HELD_EXIT and no winner binds.
    let sock_held = unique_endpoint(tmp.path(), "pr3-held-retry");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_held).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let held_attempts = AtomicUsize::new(0);
    let mut held_listener: Option<interprocess::local_socket::Listener> = None;
    let stream = crate::proxy::connect_or_autostart_with(&sock_held, true, Duration::from_secs(3), |path| {
        let n = held_attempts.fetch_add(1, Ordering::AcqRel);
        if n == 0 {
            #[cfg(unix)]
            let mut cmd = {
                let mut c = Command::new("/bin/sh");
                c.args(["-c", &format!("exit {}", apack_os::transport::HELD_EXIT)]);
                c
            };
            #[cfg(windows)]
            let mut cmd = {
                let mut c = Command::new("cmd.exe");
                c.args(["/C", &format!("exit /B {}", apack_os::transport::HELD_EXIT)]);
                c
            };
            Ok(Some(cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?))
        } else {
            held_listener = Some(apack_os::transport::bind_local_listener(path).expect("bind retry listener"));
            Ok(None)
        }
    })
    .expect("autostart must retry after HELD_EXIT when winner is absent");
    assert_eq!(held_attempts.load(Ordering::Acquire), 2);
    drop((stream, held_listener));

    // 1b. `connect_or_autostart_with` waits HELD_CONNECT_POLLS for a concurrent winner before re-spawning,
    //     and caps retries at MAX_AUTOSTART_SPAWNS when brokers keep exiting 0.
    let sock_winner = unique_endpoint(tmp.path(), "pr3-held-winner");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_winner).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let winner_spawns = AtomicUsize::new(0);
    let sock_winner_bg = sock_winner.clone();
    let (spawned_tx, spawned_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let winner_thread = std::thread::spawn(move || {
        let _ = spawned_rx.recv();
        // Wait long enough for the first `exit HELD_EXIT` attempt to be observed and enter the
        // `held_polls_left > 0` backoff loop (~15ms), while still binding well before the 4*15ms
        // `HELD_CONNECT_POLLS` window expires (~60ms).
        std::thread::sleep(Duration::from_millis(35));
        let l = apack_os::transport::bind_local_listener(&sock_winner_bg).expect("winner bind");
        use interprocess::local_socket::traits::Listener as _;
        let _ = l.accept();
    });
    let stream_w = crate::proxy::connect_or_autostart_with(&sock_winner, true, Duration::from_secs(3), |_| {
        winner_spawns.fetch_add(1, Ordering::AcqRel);
        let _ = spawned_tx.try_send(());
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", &format!("exit {}", apack_os::transport::HELD_EXIT)]);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", &format!("exit /B {}", apack_os::transport::HELD_EXIT)]);
            c
        };
        Ok(Some(cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?))
    })
    .expect("must connect to concurrent winner without re-spawning");
    assert_eq!(
        winner_spawns.load(Ordering::Acquire),
        1,
        "must poll winner during HELD_CONNECT_POLLS before re-spawning"
    );
    drop(stream_w);
    winner_thread.join().unwrap();

    let sock_cap = unique_endpoint(tmp.path(), "pr3-zero-cap");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_cap).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let cap_spawns = AtomicUsize::new(0);
    let _ = crate::proxy::connect_or_autostart_with(&sock_cap, true, Duration::from_millis(1500), |_| {
        cap_spawns.fetch_add(1, Ordering::AcqRel);
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "exit 0"]);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", "exit /B 0"]);
            c
        };
        Ok(Some(cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?))
    })
    .expect_err("exhausting retries must fail on timeout");
    assert_eq!(cap_spawns.load(Ordering::Acquire), 5, "autostart retries must stop at MAX_AUTOSTART_SPAWNS (5)");

    // 1c. Pure proxy helpers: `should_send_replay_fin` & `mark_broker_byte_and_clear_replay`.
    assert!(crate::proxy::should_send_replay_fin(true, true, false));
    assert!(!crate::proxy::should_send_replay_fin(true, true, true));
    assert!(!crate::proxy::should_send_replay_fin(false, true, false));
    assert!(!crate::proxy::should_send_replay_fin(true, false, false));

    let sock_clr = unique_endpoint(tmp.path(), "pr3-clr");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_clr).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let l_clr = apack_os::transport::bind_local_listener(&sock_clr).unwrap();
    let s_clr = std::sync::Arc::new(apack_os::transport::connect_local_stream(&sock_clr).unwrap());
    let mut rb = crate::proxy::ReplayBuffer::new(1024);
    rb.push(b"hello");
    let link_clr = std::sync::Mutex::new(crate::proxy::ActiveLink {
        stream: s_clr,
        replay: rb,
        stdin_eof: false,
        reconnected: false,
        replaying: false,
    });
    let saw_flag = AtomicBool::new(false);
    assert!(crate::proxy::mark_broker_byte_and_clear_replay(&saw_flag, &link_clr));
    assert!(link_clr.lock().unwrap().replay.replay_slice().is_none());
    link_clr.lock().unwrap().replay.push(b"after");
    assert!(!crate::proxy::mark_broker_byte_and_clear_replay(&saw_flag, &link_clr));
    assert_eq!(link_clr.lock().unwrap().replay.replay_slice(), Some(&b"after"[..]));
    drop((link_clr, l_clr));

    // 2. `connect_or_autostart_with` retries spawning when attempt 1 exits cleanly with status 0.
    let sock_zero = unique_endpoint(tmp.path(), "pr3-zero-retry");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_zero).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let zero_attempts = AtomicUsize::new(0);
    let mut zero_listener: Option<interprocess::local_socket::Listener> = None;
    let stream = crate::proxy::connect_or_autostart_with(&sock_zero, true, Duration::from_secs(3), |path| {
        let n = zero_attempts.fetch_add(1, Ordering::AcqRel);
        if n == 0 {
            #[cfg(unix)]
            let mut cmd = {
                let mut c = Command::new("/bin/sh");
                c.args(["-c", "exit 0"]);
                c
            };
            #[cfg(windows)]
            let mut cmd = {
                let mut c = Command::new("cmd.exe");
                c.args(["/C", "exit /B 0"]);
                c
            };
            Ok(Some(cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?))
        } else {
            zero_listener = Some(apack_os::transport::bind_local_listener(path).expect("bind zero retry listener"));
            Ok(None)
        }
    })
    .expect("autostart must retry after clean exit 0");
    assert_eq!(zero_attempts.load(Ordering::Acquire), 2);
    drop((stream, zero_listener));

    // 3. `connect_or_autostart_with` attaches captured stderr on timeout without blocking on live child.
    let sock_timeout = unique_endpoint(tmp.path(), "pr3-timeout-stderr");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_timeout).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let err = crate::proxy::connect_or_autostart_with(&sock_timeout, true, Duration::from_millis(800), |_| {
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "echo PR3_TIMEOUT_DIAG >&2; sleep 2"]);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", "echo PR3_TIMEOUT_DIAG 1>&2 & ping -n 3 127.0.0.1 >nul"]);
            c
        };
        Ok(Some(cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?))
    })
    .expect_err("timeout without listener must fail");
    assert!(err.to_string().contains("PR3_TIMEOUT_DIAG"), "timeout error must surface captured stderr, got: {err}");

    // 4. `run_proxy_over` sends FIN_FRAME on stdin read error so broker closes cleanly without hanging.
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated stdin I/O failure"))
        }
    }
    let sock_err = unique_endpoint(tmp.path(), "pr3-stdin-err");
    #[cfg(unix)]
    if let Some(p) = std::path::Path::new(&sock_err).parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let l_err = apack_os::transport::bind_local_listener(&sock_err).unwrap();
    let t_err = std::thread::spawn(move || {
        use interprocess::local_socket::traits::Listener as _;
        let mut s = l_err.accept().expect("accept err listener");
        let mut got = [0u8; 4];
        s.read_exact(&mut got).expect("must receive FIN_FRAME on stdin read error");
        got
    });
    let mut out_err = Vec::new();
    run_proxy_over(FailingReader, &mut out_err, move || apack_os::transport::connect_local_stream(&sock_err))
        .expect("proxy must finish cleanly after sending FIN on stdin error");
    assert_eq!(t_err.join().expect("join t_err"), apack_proto::FIN_FRAME);

    // 5. `is_interactive_input_msg` & `ShardCmdPermit::try_acquire_with_wake` fin_pending wakeup.
    assert!(crate::transport_loop::is_interactive_input_msg(&ClientMsg::Text { text: String::from("a") }));
    assert!(crate::transport_loop::is_interactive_input_msg(&ClientMsg::Focus { focused: true }));
    assert!(crate::transport_loop::is_interactive_input_msg(&ClientMsg::ViewportHint(
        apack_proto::types::ViewportHint {
            size: GridSize::new(80, 24).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            visible: true,
            focused: true,
        }
    )));
    assert!(!crate::transport_loop::is_interactive_input_msg(&ClientMsg::Commands));
    assert!(!crate::transport_loop::is_interactive_input_msg(&ClientMsg::Write { data: b"hi".to_vec() }));

    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let fin_pending = std::sync::Arc::new(AtomicBool::new(false));
    let wakes = std::sync::Arc::new(AtomicUsize::new(0));
    let wakes_cb = std::sync::Arc::clone(&wakes);
    let wake_fn: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        wakes_cb.fetch_add(1, Ordering::AcqRel);
    });
    let p1 = crate::shard::ShardCmdPermit::try_acquire_with_wake(
        &counter,
        2,
        Some(std::sync::Arc::clone(&fin_pending)),
        Some(std::sync::Arc::clone(&wake_fn)),
    )
    .unwrap();
    assert!(format!("{p1:?}").contains("ShardCmdPermit"));
    let p2 = crate::shard::ShardCmdPermit::try_acquire_with_wake(
        &counter,
        2,
        Some(std::sync::Arc::clone(&fin_pending)),
        Some(wake_fn),
    )
    .unwrap();
    fin_pending.store(true, Ordering::Release);
    drop(p1);
    assert_eq!(wakes.load(Ordering::Acquire), 0, "must not wake until final permit drops");
    drop(p2);
    assert_eq!(wakes.load(Ordering::Acquire), 1, "final permit drop while fin_pending must wake reactor");
}
