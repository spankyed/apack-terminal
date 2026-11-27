use super::*;

#[test]
fn stdio_proxy_fails_on_missing_socket_without_autostart() {
    let dir = tempdir().unwrap();
    let missing = unique_endpoint(dir.path(), "no-such-broker");
    assert!(run_stdio_proxy(&missing, false).is_err());
}

#[test]
fn connected_client_stays_alive_past_start_grace_and_marks_env_is_distinct_from_auth_env() {
    assert_ne!(AUTH_KEY_ENV, apack_os::pty::MARKS_KEY_ENV);

    let dir = tempdir().unwrap();
    let endpoint = unique_endpoint(dir.path(), "start-grace-live");
    let mut cfg = BrokerConfig::for_socket(&endpoint);
    cfg.start_grace = Duration::from_millis(60);
    let mut server = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    let mut client = BrokerClient::connect(&endpoint, &AuthConfig::None, "grace-test").unwrap();
    // Stay connected well past `start_grace` so `!state.ever_connected` is strictly exercised.
    std::thread::sleep(Duration::from_millis(120));
    let sessions = client.list_sessions().unwrap();
    assert!(sessions.is_empty());

    // Spawn a session with an explicit `APACK_TERMINAL_KEY` in `spec.env` and verify the broker
    // does not strip it when stripping `AUTH_KEY_ENV`.
    #[cfg(unix)]
    let argv = vec![
        String::from("/bin/sh"),
        String::from("-c"),
        String::from("printf 'MK=%s\\r\\n' \"$APACK_TERMINAL_KEY\"; sleep 10"),
    ];
    #[cfg(windows)]
    let argv = vec![
        String::from("cmd.exe"),
        String::from("/C"),
        String::from("echo MK=%APACK_TERMINAL_KEY% & ping -n 10 127.0.0.1 >nul"),
    ];
    let spec = SpawnSpec {
        argv,
        cwd: None,
        env: vec![(String::from(apack_os::pty::MARKS_KEY_ENV), String::from("vouched-secret-value"))],
        size: GridSize::new(80, 24).unwrap(),
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_limit: 100,
        marks_key: None,
        resize_policy: ResizePolicy::LatestActive,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    };
    let (session_id, _) = client.spawn(spec).unwrap();
    let outcome = client.wait(session_id, WaitCondition::Text(String::from("MK=vouched-secret-value")), 5_000).unwrap();
    assert_eq!(outcome, WaitOutcome::Met, "expected marks env preserved, got {outcome:?}");

    drop(client);
    server.shutdown().unwrap();
}

#[test]
fn review_regressions_env_grace_pipelined_fin_teardown_spawn_and_seek_cell_px() {
    use std::io::{Read, Write};

    // 1. apply_env_overrides(None, None) preserves custom programmatic start_grace.
    let mut custom = BrokerConfig::for_socket("/tmp/ignored");
    custom.start_grace = Duration::from_millis(777);
    let kept_grace = custom.apply_env_overrides(None, None);
    assert_eq!(kept_grace.start_grace, Duration::from_millis(777));

    // 2. Pipelined RPC + FIN_FRAME delivers the queued reply cleanly before EOF.
    let dir = tempdir().unwrap();
    let endpoint = unique_endpoint(dir.path(), "pipelined-fin");
    let mut cfg = BrokerConfig::for_socket(&endpoint);
    cfg.shard_count = 1;
    let mut server = BrokerServer::bind(cfg).unwrap().spawn().unwrap();
    {
        let mut stream = apack_os::transport::connect_local_stream(server.socket_path()).unwrap();
        let mut wire = Vec::new();
        ClientMsg::Hello {
            protocol_version: apack_proto::PROTOCOL_VERSION,
            client_nonce: [7u8; 32],
            client_name: String::from("fin-test"),
        }
        .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(1), &mut wire)
        .unwrap();
        // Send Hello first, wait for HelloOk so connection is Ready, then pipeline Metrics + FIN_FRAME!
        stream.write_all(&wire).unwrap();
        stream.flush().unwrap();

        let mut dec = apack_proto::frame::FrameDecoder::new();
        let mut tmp_buf = [0u8; 4096];
        'hello: loop {
            let n = stream.read(&mut tmp_buf).unwrap();
            assert!(n > 0);
            let mut rest = &tmp_buf[..n];
            while !rest.is_empty() {
                let (used, ev) = dec.step(rest).unwrap();
                rest = &rest[used..];
                if let Some(apack_proto::frame::DecodedFrame::Frame { header, payload }) = ev {
                    assert!(matches!(BrokerMsg::decode(header.msg_type, &payload).unwrap(), BrokerMsg::HelloOk { .. }));
                    break 'hello;
                }
            }
        }
        // Pipeline Metrics + FIN_FRAME in a single write!
        let mut batch = Vec::new();
        ClientMsg::Metrics
            .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(2), &mut batch)
            .unwrap();
        batch.extend_from_slice(&apack_proto::FIN_FRAME);
        stream.write_all(&batch).unwrap();
        stream.flush().unwrap();

        let mut tail = Vec::new();
        let t_fin = std::time::Instant::now();
        stream.read_to_end(&mut tail).unwrap();
        assert!(
            t_fin.elapsed() < Duration::from_millis(250),
            "already-drained ClientFin must close immediately without waiting 500ms drain deadline"
        );
        let (_, ev) = dec.step(&tail).unwrap();
        let ev = ev.expect("expected MetricsReply before EOF");
        let apack_proto::frame::DecodedFrame::Frame { header, payload } = ev else {
            panic!("expected MetricsReply frame");
        };
        assert_eq!(header.req_id, RequestId::from_raw(2));
        assert!(matches!(BrokerMsg::decode(header.msg_type, &payload).unwrap(), BrokerMsg::MetricsReply(_)));
    }
    server.shutdown().unwrap();

    // 3. TeardownAll rejects subsequent Spawn on the shard with Busy + SpawnOrRewindFailed.
    let (tx, rx) = std::sync::mpsc::channel::<ReactorEvent>();
    let shared = ShardShared {
        image_store: std::sync::Arc::new(std::sync::Mutex::new(apack_vt_ghostty::ImageStore::default())),
        wal_registry: apack_wal::RecordingRegistry::new(),
        wal_config: apack_wal::WalConfig::default(),
        idle_compress_after: Duration::from_secs(5),
        kill_escalate_after: Duration::from_millis(50),
        engine_abi_hash: engine_abi_hash(),
    };
    let mut shard = spawn_shard(0, shared, tx, std::sync::Arc::new(|| {})).unwrap();
    shard.send(ShardCmd::TeardownAll);
    shard.send(ShardCmd::Spawn {
        session_id: SessionId::new(1).unwrap(),
        conn: apack_core::lease::ConnId::new(1).unwrap(),
        req_id: RequestId::from_raw(9),
        spec: Box::new(echo_command_spec("SHOULD_NOT_SPAWN", None, None)),
    });
    let mut saw_failed = false;
    let mut saw_busy = false;
    for _ in 0..4 {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(ReactorEvent::SpawnOrRewindFailed) => saw_failed = true,
            Ok(ReactorEvent::SendMsg { msg: BrokerMsg::Error { code: ErrorCode::Busy, .. }, .. }) => saw_busy = true,
            _ => {}
        }
        if saw_failed && saw_busy {
            break;
        }
    }
    assert!(saw_failed && saw_busy, "Spawn after TeardownAll must fail cleanly with Busy");
    shard.shutdown();

    // 4. apply_seek_slice returns effective post-Resize CellPx.
    let init_px = CellPx { width: 8, height: 16 };
    let resized_px = CellPx { width: 11, height: 22 };
    let size = GridSize::new(80, 24).unwrap();
    let mut eng = apack_vt_ghostty::GhosttyEngine::new(apack_vt_ghostty::GhosttyConfig::new(size)).unwrap();
    let machine = apack_core::session::SessionMachine::new(
        SessionId::new(1).unwrap(),
        apack_vt_ghostty::GhosttyEngine::new(apack_vt_ghostty::GhosttyConfig::new(size)).unwrap(),
        ResizePolicy::ExplicitOverride,
        apack_core::marks::MarksMode::Claimed,
        init_px,
    );
    let abi = engine_abi_hash();
    let kf = capture_keyframe(&machine, &abi);
    let slice = apack_wal::SeekSlice {
        segment_id: 1,
        keyframe_at_ms: 0,
        keyframe: kf,
        tail_records: vec![apack_wal::TimedRecord {
            at_ms: 100,
            record: apack_wal::WalRecord::Resize { size, cell_px: resized_px },
        }],
    };
    let effective = apply_seek_slice(&mut eng, &slice, &abi);
    assert_eq!(effective, resized_px);

    // 5. Pure transport/shard helper truth tables
    use crate::transport_loop::{
        all_sessions_drained, drain_deadline_expired, should_fast_poll, should_graceful_drain_unix,
    };
    assert!(all_sessions_drained(true, 0));
    assert!(!all_sessions_drained(false, 0));
    assert!(!all_sessions_drained(true, 1));
    assert!(should_graceful_drain_unix(apack_core::broker::CloseReason::ClientFin, 0, false));
    assert!(should_graceful_drain_unix(apack_core::broker::CloseReason::SlowReader, 1, false));
    assert!(should_graceful_drain_unix(apack_core::broker::CloseReason::SlowReader, 0, true));
    assert!(!should_graceful_drain_unix(apack_core::broker::CloseReason::SlowReader, 0, false));
    assert!(should_fast_poll(true, false));
    assert!(should_fast_poll(false, true));
    assert!(!should_fast_poll(false, false));
    let t0 = std::time::Instant::now();
    let t1 = t0 + Duration::from_millis(10);
    assert!(!drain_deadline_expired(t0, t1));
    assert!(drain_deadline_expired(t1, t1));
    assert!(drain_deadline_expired(t1, t0));
    #[cfg(unix)]
    {
        let i_normal = crate::shard::unix_pty_interest(false, false, 4);
        assert!(i_normal.readable && i_normal.writable);
        let i_broken = crate::shard::unix_pty_interest(false, true, 4);
        assert!(i_broken.readable && !i_broken.writable);
        let i_paused = crate::shard::unix_pty_interest(true, false, 0);
        assert!(!i_paused.readable && !i_paused.writable);
    }

    // 6. Unix non-blocking draining_unix (has_draining_conns keeps broker alive, poll_draining_unix
    //    delivers all replies, reap_draining_conns forcibly hangs up stalled peer after 500ms)
    //    AND close_conn_transport on server shutdown unblocks an open idle peer before session exit!
    #[cfg(unix)]
    {
        let do_hello = |s: &mut interprocess::local_socket::Stream| {
            let mut hello = Vec::new();
            ClientMsg::Hello {
                protocol_version: apack_proto::PROTOCOL_VERSION,
                client_nonce: [9u8; 32],
                client_name: String::from("drain-test"),
            }
            .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(1), &mut hello)
            .unwrap();
            s.write_all(&hello).unwrap();
            s.flush().unwrap();
            let mut dec = apack_proto::frame::FrameDecoder::new();
            let mut buf = [0u8; 4096];
            'wait_ok: loop {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0);
                let mut rest = &buf[..n];
                while !rest.is_empty() {
                    let (used, ev) = dec.step(rest).unwrap();
                    rest = &rest[used..];
                    if ev.is_some() {
                        break 'wait_ok;
                    }
                }
            }
        };

        let mut batch = Vec::new();
        const BURST_N: u32 = 35_000;
        for i in 0..BURST_N {
            ClientMsg::Metrics
                .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(i + 2), &mut batch)
                .unwrap();
        }
        batch.extend_from_slice(&apack_proto::FIN_FRAME);

        // 6a. Sole client pipelines 35,000 Metrics (~770 KiB > Linux & macOS AF_UNIX buffers) + FIN_FRAME.
        //     `has_draining_conns()` MUST keep the broker from auto-exiting while replies drain!
        let dir_solo = tempdir().unwrap();
        let ep_solo = unique_endpoint(dir_solo.path(), "unix-drain-solo");
        let mut cfg_solo = BrokerConfig::for_socket(&ep_solo);
        cfg_solo.shard_count = 1;
        let mut srv_solo = BrokerServer::bind(cfg_solo).unwrap().spawn().unwrap();
        let mut burst = apack_os::transport::connect_local_stream(srv_solo.socket_path()).unwrap();
        do_hello(&mut burst);
        burst.write_all(&batch).unwrap();
        burst.flush().unwrap();
        // Wait 200ms (>> unoptimized parse time of 35k frames, << 500ms GRACEFUL_DRAIN_TIMEOUT) so
        // the reactor processes FIN and would auto-exit if `has_draining_conns` were false!
        std::thread::sleep(Duration::from_millis(200));
        assert!(!srv_solo.is_finished(), "broker must not auto-exit while draining_unix still holds unsent replies");
        let mut all_replies = Vec::new();
        burst.read_to_end(&mut all_replies).unwrap();
        let mut dec = apack_proto::frame::FrameDecoder::new();
        let mut rest = all_replies.as_slice();
        let mut got_replies = 0u32;
        while !rest.is_empty() {
            let (used, ev) = dec.step(rest).unwrap();
            rest = &rest[used..];
            if matches!(ev, Some(apack_proto::frame::DecodedFrame::Frame { .. })) {
                got_replies += 1;
            }
        }
        assert_eq!(got_replies, BURST_N, "draining_unix must keep broker alive and deliver all replies");
        srv_solo.shutdown().unwrap();

        // 6b. Sole stalled peer pipelines BURST_N Metrics + FIN_FRAME and NEVER reads ->
        //     `reap_draining_conns` must forcibly hang up after GRACEFUL_DRAIN_TIMEOUT (500 ms) so
        //     `draining_unix` empties and the broker auto-exits cleanly WITHOUT the peer ever reading!
        let dir_reap = tempdir().unwrap();
        let ep_reap = unique_endpoint(dir_reap.path(), "unix-drain-reap-solo");
        let mut cfg_reap = BrokerConfig::for_socket(&ep_reap);
        cfg_reap.shard_count = 1;
        let mut srv_reap = BrokerServer::bind(cfg_reap).unwrap().spawn().unwrap();
        let mut stalled = apack_os::transport::connect_local_stream(srv_reap.socket_path()).unwrap();
        do_hello(&mut stalled);
        stalled.write_all(&batch).unwrap();
        stalled.flush().unwrap();
        let reap_wait = std::time::Instant::now();
        while (!srv_reap.is_finished() || std::path::Path::new(&ep_reap).exists())
            && reap_wait.elapsed() < Duration::from_millis(1_500)
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            srv_reap.is_finished() && !std::path::Path::new(&ep_reap).exists(),
            "reap_draining_conns must drop stalled draining_unix peer after 500ms so broker auto-exits"
        );
        let mut discard = Vec::new();
        stalled.read_to_end(&mut discard).unwrap();
        assert!(discard.len() < all_replies.len());
        srv_reap.shutdown().unwrap();

        // 6c. Spawn a HUP-ignoring session so `srv.shutdown()` takes ~300ms to escalate SIGKILL,
        //     and verify an open idle peer receives immediate EOF (< 150ms) from `close_conn_transport`!
        let dir2 = tempdir().unwrap();
        let ep2 = unique_endpoint(dir2.path(), "unix-close-conn");
        let mut cfg2 = BrokerConfig::for_socket(&ep2);
        cfg2.shard_count = 1;
        cfg2.kill_escalate_after = Duration::from_millis(300);
        let mut srv = BrokerServer::bind(cfg2).unwrap().spawn().unwrap();
        let mut keeper = BrokerClient::connect(srv.socket_path(), &AuthConfig::None, "keeper").unwrap();
        let trap_spec = SpawnSpec {
            argv: vec![String::from("/bin/sh"), String::from("-c"), String::from("trap '' HUP; sleep 10")],
            cwd: None,
            env: Vec::new(),
            size: GridSize::new(80, 24).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_limit: 100,
            marks_key: None,
            resize_policy: ResizePolicy::LatestActive,
            record_dir: None,
            restore_from: None,
            image_files: false,
            kept: false,
        };
        let _ = keeper.spawn(trap_spec).unwrap();
        std::thread::sleep(Duration::from_millis(40));
        let mut idle = apack_os::transport::connect_local_stream(srv.socket_path()).unwrap();
        do_hello(&mut idle);
        let t_shut = std::time::Instant::now();
        let reader_thread = std::thread::spawn(move || {
            let mut b = [0u8; 16];
            let n = idle.read(&mut b).unwrap_or(0);
            (n, t_shut.elapsed())
        });
        drop(keeper);
        srv.shutdown().unwrap();
        let (eof_n, eof_dt) = reader_thread.join().unwrap();
        assert_eq!(eof_n, 0, "shutdown must close open client connections");
        assert!(
            eof_dt < Duration::from_millis(200),
            "close_conn_transport must close idle clients immediately (took {eof_dt:?}, before 300ms SIGKILL)"
        );
    }
}

#[test]
fn kitty_image_sources_e2e_file_shm_security_and_teardown() {
    use apack_os::image_sources::Quiet as ImageQuiet;

    assert!(should_forward_probe_reply("\x1b_Gi=1;OK\x1b\\", ImageQuiet::No));
    assert!(!should_forward_probe_reply("\x1b_Gi=1;OK\x1b\\", ImageQuiet::Ok));
    assert!(!should_forward_probe_reply("\x1b_Gi=1;EINVAL: x\x1b\\", ImageQuiet::Failures));

    let tmp = tempdir().unwrap();
    let endpoint = unique_endpoint(tmp.path(), "kitty-img-src");
    let mut cfg = BrokerConfig::for_socket(&endpoint);
    cfg.shard_count = 1;
    let mut srv = BrokerServer::bind(cfg).unwrap().spawn().unwrap();
    let mut client = BrokerClient::connect(srv.socket_path(), &AuthConfig::None, "img-e2e").unwrap();
    let (_lease, _) = client.acquire_lease(LeaseKind::BrokerImmediate).unwrap();

    #[cfg(unix)]
    {
        use apack_os::image_sources::encoded;
        use std::os::unix::ffi::OsStrExt;
        use std::process::Command;

        let pixels: Vec<u8> =
            vec![0xde, 0xad, 0xbe, 0xff, 0x10, 0x20, 0x30, 0xff, 0x40, 0x50, 0x60, 0xff, 0x70, 0x80, 0x90, 0xff];
        let img_path = tmp.path().join("ok.rgba");
        std::fs::write(&img_path, &pixels).unwrap();
        let b64_ok = String::from_utf8(encoded(img_path.as_os_str().as_bytes())).unwrap();

        // 1. `image_files: false` MUST refuse `t=f` (hostile-PTY boundary).
        let seq_disabled = tmp.path().join("seq-disabled.bin");
        std::fs::write(&seq_disabled, format!("\x1b_Ga=T,f=32,s=2,v=2,i=1,t=f;{b64_ok}\x1b\\DISABLED_DONE\r\n"))
            .unwrap();
        let mut spec_disabled = echo_command_spec("x", None, None);
        spec_disabled.argv =
            vec![String::from("/bin/sh"), String::from("-c"), format!("cat '{}'; sleep 10", seq_disabled.display())];
        spec_disabled.image_files = false;
        let (s_dis, _) = client.spawn(spec_disabled).unwrap();
        assert_eq!(
            client.wait(s_dis, WaitCondition::Text(String::from("DISABLED_DONE")), 5_000).unwrap(),
            WaitOutcome::Met
        );
        let st_dis = NonZeroStreamId::new(1).unwrap();
        client
            .attach(
                s_dis,
                st_dis,
                SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                None,
                0,
                FrameCredit { max_frames: 8, max_bytes: 1 << 20 },
            )
            .unwrap();
        let BrokerMsg::ImagesReply { placements: dis_placements, .. } =
            client.stream_request(st_dis, &ClientMsg::Images).unwrap()
        else {
            panic!("expected ImagesReply");
        };
        assert!(dis_placements.is_empty(), "t=f must be blocked when image_files=false");
        client.kill(s_dis).unwrap();

        // 2. `image_files: true`: `t=f` file load + `t=s` POSIX shared memory + symlink/FIFO refusals!
        struct ShmCleanup(String);
        impl Drop for ShmCleanup {
            fn drop(&mut self) {
                let _ = Command::new("python3")
                    .args([
                        "-c",
                        "import sys; from multiprocessing.shared_memory import SharedMemory\n\
                         try:\n  s = SharedMemory(name=sys.argv[1])\n  s.close()\n  s.unlink()\n\
                         except FileNotFoundError:\n  pass",
                        &self.0,
                    ])
                    .status();
            }
        }
        let shm_bare = format!("apack-img-e2e-{}", std::process::id());
        let shm_name = format!("/{shm_bare}");
        let _shm_guard = ShmCleanup(shm_bare.clone());
        let shm_pixels: Vec<u8> =
            vec![0x01, 0x02, 0x03, 0xff, 0x04, 0x05, 0x06, 0xff, 0x07, 0x08, 0x09, 0xff, 0x0a, 0x0b, 0x0c, 0xff];
        let shm_status = Command::new("python3")
            .args([
                "-c",
                "import sys; from multiprocessing.shared_memory import SharedMemory\n\
                 from multiprocessing import resource_tracker\n\
                 s = SharedMemory(name=sys.argv[1], create=True, size=16)\n\
                 resource_tracker.unregister(s._name, 'shared_memory')\n\
                 s.buf[:16] = bytes.fromhex('010203ff040506ff070809ff0a0b0cff')\n\
                 s.close()",
                &shm_bare,
            ])
            .status()
            .expect("python3 shm setup");
        assert!(shm_status.success(), "python3 shared_memory creation failed");
        let b64_shm = String::from_utf8(encoded(shm_name.as_bytes())).unwrap();

        // Hostile sources: symlink to `/dev/null`, symlink loop, and a FIFO (named pipe).
        let dev_link = tmp.path().join("to-dev-null");
        std::os::unix::fs::symlink("/dev/null", &dev_link).unwrap();
        let loop_a = tmp.path().join("loop-a");
        let loop_b = tmp.path().join("loop-b");
        std::os::unix::fs::symlink(&loop_b, &loop_a).unwrap();
        std::os::unix::fs::symlink(&loop_a, &loop_b).unwrap();
        let fifo_path = tmp.path().join("trap.fifo");
        assert!(Command::new("mkfifo").arg(&fifo_path).status().expect("mkfifo").success());

        let b64_dev = String::from_utf8(encoded(dev_link.as_os_str().as_bytes())).unwrap();
        let b64_loop = String::from_utf8(encoded(loop_a.as_os_str().as_bytes())).unwrap();
        let b64_fifo = String::from_utf8(encoded(fifo_path.as_os_str().as_bytes())).unwrap();

        let seq_enabled = tmp.path().join("seq-enabled.bin");
        let stream = format!(
            "\x1b_Ga=T,f=32,s=2,v=2,i=2,t=f;{b64_ok}\x1b\\\
             \x1b_Ga=T,f=32,s=2,v=2,i=3,t=s;{b64_shm}\x1b\\\
             \x1b_Ga=T,f=32,s=2,v=2,i=4,t=f;{b64_dev}\x1b\\\
             \x1b_Ga=T,f=32,s=2,v=2,i=5,t=f;{b64_loop}\x1b\\\
             \x1b_Ga=T,f=32,s=2,v=2,i=6,t=f;{b64_fifo}\x1b\\\
             ALL_IMAGES_DONE\r\n"
        );
        std::fs::write(&seq_enabled, stream).unwrap();

        let mut spec_ok = echo_command_spec("x", None, None);
        spec_ok.argv =
            vec![String::from("/bin/sh"), String::from("-c"), format!("cat '{}'; sleep 10", seq_enabled.display())];
        spec_ok.image_files = true;
        let (s_ok, _) = client.spawn(spec_ok).unwrap();
        assert_eq!(
            client.wait(s_ok, WaitCondition::Text(String::from("ALL_IMAGES_DONE")), 5_000).unwrap(),
            WaitOutcome::Met
        );
        let st_ok = NonZeroStreamId::new(2).unwrap();
        client
            .attach(
                s_ok,
                st_ok,
                SubFlags::from_bits(SubFlags::VIEW).unwrap(),
                None,
                0,
                FrameCredit { max_frames: 8, max_bytes: 1 << 20 },
            )
            .unwrap();
        let BrokerMsg::ImagesReply { placements, .. } = client.stream_request(st_ok, &ClientMsg::Images).unwrap()
        else {
            panic!("expected ImagesReply");
        };
        assert_eq!(placements.len(), 2, "expected exactly 2 valid images (t=f + t=s), got {placements:?}");
        let mut blobs = Vec::new();
        for p in &placements {
            assert_eq!(p.pixel_size, [2, 2]);
            let BrokerMsg::ImageBlobChunk { encoding, data, .. } = client
                .stream_request(st_ok, &ClientMsg::GetImageBlob { blob: p.blob, offset: 0, max_bytes: 1024 })
                .unwrap()
            else {
                panic!("expected ImageBlobChunk");
            };
            assert_eq!(encoding, apack_proto::types::ImageEncoding::Png);
            assert!(data.starts_with(b"\x89PNG\r\n\x1a\n"));
            blobs.push(data);
        }
        assert!(
            blobs.iter().any(|b| b.windows(8).any(|w| w == &pixels[..8]) && b.windows(8).any(|w| w == &pixels[8..])),
            "missing t=f PNG pixel blob"
        );
        assert!(
            blobs
                .iter()
                .any(|b| b.windows(8).any(|w| w == &shm_pixels[..8]) && b.windows(8).any(|w| w == &shm_pixels[8..])),
            "missing t=s PNG pixel blob"
        );

        // 3. Explicit `Kill` tears down an `image_files: true` session cleanly and promptly.
        let t_kill = std::time::Instant::now();
        client.kill(s_ok).unwrap();
        assert!(t_kill.elapsed() < Duration::from_secs(2));

        // 4a. Path B: short-lived child printing ONLY a refused `t=f` (`q=0`, so `write_broken`
        //     stays `false` until `handle_image_source_loaded` enqueues its refusal reply and hits
        //     Unix `EIO`) + trailing text, exiting immediately -> must NOT reap before `FAST_EXIT_B`!
        let b64_missing =
            String::from_utf8(encoded(tmp.path().join("no-such-img.rgba").as_os_str().as_bytes())).unwrap();
        for (idx, (seq_name, payload, marker, stream_num)) in [
            (
                "seq-fast-b.bin",
                format!("\x1b_Ga=T,f=32,s=2,v=2,i=30,q=0,t=f;{b64_missing}\x1b\\FAST_EXIT_B\r\n"),
                "FAST_EXIT_B",
                3u32,
            ),
            (
                "seq-fast-a.bin",
                format!("\x1b[6n\x1b_Ga=T,f=32,s=2,v=2,i=31,t=f;{b64_ok}\x1b\\FAST_EXIT_A\r\n"),
                "FAST_EXIT_A",
                4u32,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let seq_fast = tmp.path().join(seq_name);
            std::fs::write(&seq_fast, payload).unwrap();
            let ready_tok = format!("FAST_READY_{idx}");
            let mut spec_fast = echo_command_spec("x", None, None);
            spec_fast.argv = vec![
                String::from("/bin/sh"),
                String::from("-c"),
                format!("printf '{ready_tok}\\r\\n'; read _go; cat '{}'", seq_fast.display()),
            ];
            spec_fast.image_files = true;
            let (s_fast, _) = client.spawn(spec_fast).unwrap();
            assert_eq!(client.wait(s_fast, WaitCondition::Text(ready_tok), 5_000).unwrap(), WaitOutcome::Met);
            let st_fast = NonZeroStreamId::new(stream_num).unwrap();
            client
                .attach(
                    s_fast,
                    st_fast,
                    SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
                    None,
                    0,
                    FrameCredit { max_frames: 16, max_bytes: 1 << 20 },
                )
                .unwrap();
            let wait_req = client.alloc_req_id();
            client
                .send_raw(
                    apack_proto::types::StreamId::CONTROL,
                    wait_req,
                    &ClientMsg::Wait {
                        session: s_fast,
                        condition: WaitCondition::Text(String::from(marker)),
                        timeout_ms: 5_000,
                    },
                )
                .unwrap();
            client
                .send_raw(st_fast.as_stream_id(), RequestId::PUSH, &ClientMsg::Write { data: b"go\n".to_vec() })
                .unwrap();
            assert_eq!(
                client.wait_reply(wait_req).unwrap(),
                BrokerMsg::Waited { outcome: WaitOutcome::Met },
                "deferred natural exit must complete off-shard t=f load and drain {marker} before Exited"
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if !client.list_sessions().unwrap().iter().any(|s| s.id == s_fast) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !client.list_sessions().unwrap().iter().any(|s| s.id == s_fast),
                "short-lived image_files session must exit cleanly after deferred load completes ({marker})"
            );
        }
    }

    #[cfg(windows)]
    {
        let mut spec_win = echo_command_spec("WIN_IMG_READY", None, None);
        spec_win.image_files = true;
        let (s_win, _) = client.spawn(spec_win).unwrap();
        assert_eq!(
            client.wait(s_win, WaitCondition::Text(String::from("WIN_IMG_READY")), 5_000).unwrap(),
            WaitOutcome::Met
        );
        client.kill(s_win).unwrap();
    }

    drop(client);
    srv.shutdown().unwrap();
}
