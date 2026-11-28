use super::*;

#[test]
fn cli_parsing_config_and_helpers() {
    let args = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    assert!(parse_cli(&[]).unwrap_err().contains("usage: apack-terminal"));
    assert_eq!(
        parse_cli(&args(&["serve"])).unwrap(),
        CliCommand::Serve { socket: None, shards: None, detached: false }
    );
    assert_eq!(
        parse_cli(&args(&["serve", "--socket", "/tmp/a.sock", "--shards", "8", "--detached"])).unwrap(),
        CliCommand::Serve { socket: Some(String::from("/tmp/a.sock")), shards: Some(8), detached: true }
    );
    assert!(parse_cli(&args(&["serve", "--shards", "0"])).is_err());
    assert!(parse_cli(&args(&["serve", "--shards", "9"])).is_err());
    assert!(parse_cli(&args(&["serve", "--shards", "abc"])).is_err());
    assert!(parse_cli(&args(&["serve", "--socket"])).is_err());
    assert!(parse_cli(&args(&["serve", "--unknown"])).is_err());

    assert_eq!(
        parse_cli(&args(&["proxy", "--socket", "/tmp/b.sock", "--no-autostart"])).unwrap(),
        CliCommand::Proxy { socket: Some(String::from("/tmp/b.sock")), autostart: false }
    );
    assert_eq!(parse_cli(&args(&["proxy"])).unwrap(), CliCommand::Proxy { socket: None, autostart: true });
    assert!(parse_cli(&args(&["proxy", "--socket"])).is_err());
    assert!(parse_cli(&args(&["proxy", "--bad"])).is_err());

    assert_eq!(
        parse_cli(&args(&["inspect", "--socket", "/tmp/c.sock", "--json"])).unwrap(),
        CliCommand::Inspect { socket: Some(String::from("/tmp/c.sock")), json: true }
    );
    assert!(parse_cli(&args(&["inspect", "--socket"])).is_err());
    assert!(parse_cli(&args(&["inspect", "--bad"])).is_err());

    assert_eq!(parse_cli(&args(&["--version"])).unwrap(), CliCommand::Version);
    assert_eq!(parse_cli(&args(&["-V"])).unwrap(), CliCommand::Version);
    assert_eq!(parse_cli(&args(&["version"])).unwrap(), CliCommand::Version);
    assert!(parse_cli(&args(&["version", "extra"])).is_err());
    assert!(parse_cli(&args(&["bogus"])).is_err());

    assert_eq!(clamp_shard_count(0), 1);
    assert_eq!(clamp_shard_count(3), 3);
    assert_eq!(clamp_shard_count(64), 8);

    assert_eq!(resolve_socket_path(Some(String::from("/tmp/x"))).unwrap(), "/tmp/x");
    assert!(resolve_socket_path(Some(String::new())).is_err());
    assert!(resolve_socket_path(None).is_ok());

    assert!(now_unix_ms() > 1_000_000_000_000);
    assert_eq!(crate::now_wall_ms().as_millis() / 1_000_000_000_000, now_unix_ms() / 1_000_000_000_000);
    let m1 = crate::now_monotonic_ms();
    let m2 = crate::now_monotonic_ms();
    assert!(m1.as_millis() >= 1 && m2 >= m1, "monotonic clock must be >= 1 and non-decreasing");
    let mut r1 = [0u8; 32];
    let mut r2 = [0u8; 32];
    fill_random(&mut r1).unwrap();
    fill_random(&mut r2).unwrap();
    assert_ne!(r1, r2);

    let abi = engine_abi_hash();
    assert_eq!(abi, engine_abi_hash());
    assert_ne!(abi, [0u8; 32]);
    assert_ne!(abi, [1u8; 32]);
    let mut f_alt = apack_vt_ghostty::features();
    f_alt.kitty_graphics = !f_alt.kitty_graphics;
    assert_ne!(abi, abi_hash_for_features(f_alt));

    // Config apply_env_overrides
    let c_none = BrokerConfig::for_socket("/tmp/x").apply_env_overrides(Some("250"), Some(""));
    assert_eq!(c_none.start_grace, Duration::from_millis(250));
    assert_eq!(c_none.auth, AuthConfig::None);
    let c_auth = BrokerConfig::for_socket("/tmp/x").apply_env_overrides(None, Some("secret-key"));
    assert!(c_auth.auth.is_required());

    assert_eq!(PROXY_REPLAY_CAP, 1_048_576);

    // OSC 7501 foreground confirmation (`confirm_foreground_app` in apack-core):
    let argv = vec![String::from("/usr/local/bin/Claude"), String::from("--flag")];
    assert_eq!(
        apack_core::status::confirm_foreground_app("claude", Some(&argv)),
        apack_proto::types::Confirmed::Matched
    );
    assert_eq!(
        apack_core::status::confirm_foreground_app("claude-code", Some(&argv)),
        apack_proto::types::Confirmed::Matched
    );
    assert_eq!(
        apack_core::status::confirm_foreground_app("vim", Some(&argv)),
        apack_proto::types::Confirmed::Mismatched
    );
    assert_eq!(apack_core::status::confirm_foreground_app("   ", Some(&argv)), apack_proto::types::Confirmed::Unknown);
    // Trailing args must NEVER match (anti-spoofing: `cat claude` != `claude`).
    let cat_argv = vec![String::from("/bin/cat"), String::from("claude")];
    assert_eq!(
        apack_core::status::confirm_foreground_app("claude", Some(&cat_argv)),
        apack_proto::types::Confirmed::Mismatched
    );

    // ReplayBuffer cap & overflow
    let mut rb = ReplayBuffer::new(10);
    assert!(!rb.is_overflowed());
    assert_eq!(rb.replay_slice(), None);
    rb.push(b"hello");
    assert_eq!(rb.replay_slice(), Some(&b"hello"[..]));
    rb.push(b"12345");
    assert_eq!(rb.replay_slice(), Some(&b"hello12345"[..]));
    rb.push(b"!");
    assert!(rb.is_overflowed());
    assert_eq!(rb.replay_slice(), None);
    rb.push(b"x");
    assert_eq!(rb.replay_slice(), None);

    // read_retry_eintr retries Interrupted and returns both Ok and non-Interrupted Err
    struct FlakyReader(u8);
    impl Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            match self.0 {
                1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                2 => {
                    buf[..3].copy_from_slice(b"abc");
                    Ok(3)
                }
                _ => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            }
        }
    }
    let mut fr = FlakyReader(0);
    let mut rbuf = [0u8; 8];
    assert_eq!(read_retry_eintr(&mut fr, &mut rbuf).unwrap(), 3);
    assert_eq!(&rbuf[..3], b"abc");
    assert_eq!(read_retry_eintr(&mut fr, &mut rbuf).unwrap_err().kind(), io::ErrorKind::BrokenPipe);

    let mut rb2 = ReplayBuffer::new(PROXY_REPLAY_CAP);
    rb2.push(b"abc");
    rb2.clear();
    assert_eq!(rb2.replay_slice(), None);

    // ClientError Display coverage
    let errs = vec![
        ClientError::Io(io::Error::other("io")),
        ClientError::Wire(apack_proto::codec::WireError::UnexpectedEof),
        ClientError::UntrustedServer,
        ClientError::AuthFailed(String::from("bad")),
        ClientError::Broker { code: ErrorCode::NoSuchSession, message: String::from("gone") },
        ClientError::UnexpectedReply("nope"),
        ClientError::Closed,
    ];
    for e in errs {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn e2e_spawn_view_frames_queries_owns_metrics_inspect_and_kill() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-core");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 2;
    cfg.start_grace = Duration::from_secs(30);

    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();
    let mut client = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "e2e-test").unwrap();
    assert!(!client.word_delimiters().is_empty());
    assert_ne!(client.word_delimiters(), "xyzzy");
    assert_eq!(client.capabilities(), apack_core::BROKER_CAPABILITIES);

    let (lease_id, resumed) = client.acquire_lease(LeaseKind::BrokerImmediate).unwrap();
    assert!(lease_id > 0);
    assert!(!resumed);

    let (sid, pid) = client.spawn(echo_command_spec("APACK_V2_HELLO_WORLD", None, None)).unwrap();
    assert!(pid.is_some());

    // Wait reactively for the output marker to land in the authoritative VT core!
    let outcome = client.wait(sid, WaitCondition::Text(String::from("APACK_V2_HELLO_WORLD")), 10_000).unwrap();
    assert_eq!(outcome, WaitOutcome::Met);

    // Attach a SUB_VIEW + SUB_EVENTS stream and verify initial full-sync frame + queries.
    let stream_id = NonZeroStreamId::new(1).unwrap();
    let attached = client
        .attach(
            sid,
            stream_id,
            SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
            Some(ViewportHint {
                size: GridSize::new(80, 24).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
                visible: true,
                focused: true,
            }),
            0,
            FrameCredit { max_frames: 4, max_bytes: 256 * 1024 },
        )
        .unwrap();
    let (epoch, attach_fg) = match attached {
        BrokerMsg::Attached { epoch, size, foreground, .. } => {
            assert_eq!(size, GridSize::new(80, 24).unwrap());
            (epoch, foreground)
        }
        other => panic!("unexpected attach reply: {other:?}"),
    };
    #[cfg(unix)]
    assert!(attach_fg.is_some(), "Unix Attached reply should report foreground process");
    #[cfg(windows)]
    let _ = attach_fg;

    // Receive initial FrameUpdate and acknowledge it.
    let frame = loop {
        let f = client.recv_frame().unwrap();
        if let BrokerMsg::Frame(upd) = f.msg {
            break upd;
        }
    };
    assert!(frame.full_sync);
    assert!(
        frame.rows.iter().any(|r| r.text.contains("APACK_V2_HELLO_WORLD")),
        "initial full-sync frame must contain PTY output"
    );
    client
        .send_raw(
            stream_id.as_stream_id(),
            RequestId::PUSH,
            &ClientMsg::FrameAck { seq: frame.seq, credit: FrameCredit { max_frames: 4, max_bytes: 256 * 1024 } },
        )
        .unwrap();

    // Query TextOf, LineAt, WordAt, FetchRows, SetSearch
    let line_reply = client.stream_request(stream_id, &ClientMsg::LineAt { epoch, line: 0 }).unwrap();
    assert!(matches!(line_reply, BrokerMsg::SpanReply { span: Some(_) }));

    let word_reply =
        client.stream_request(stream_id, &ClientMsg::WordAt { epoch, pos: CellPos { line: 0, col: 0 } }).unwrap();
    assert!(matches!(word_reply, BrokerMsg::SpanReply { span: Some(_) }));

    let text_reply = client
        .stream_request(
            stream_id,
            &ClientMsg::TextOf {
                epoch,
                span: CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 0, col: 19 } },
            },
        )
        .unwrap();
    match text_reply {
        BrokerMsg::TextReply { text } => assert!(text.contains("APACK_V2_HELLO_WORLD")),
        other => panic!("unexpected TextReply: {other:?}"),
    }

    let rows_reply = client.stream_request(stream_id, &ClientMsg::FetchRows { epoch, from_line: 0, count: 5 }).unwrap();
    assert!(matches!(rows_reply, BrokerMsg::Rows { .. }));

    let search_reply = client
        .stream_request(
            stream_id,
            &ClientMsg::SetSearch { query: String::from("APACK_V2_HELLO_WORLD"), case_sensitive: true },
        )
        .unwrap();
    match search_reply {
        BrokerMsg::SearchMatches { matches, .. } => assert!(!matches.is_empty()),
        other => panic!("unexpected SearchMatches: {other:?}"),
    }

    // Owns check (leader PID belongs to session; PID 1 does not)
    let leader_pid = pid.unwrap();
    assert!(client.owns(sid, leader_pid).unwrap());
    assert!(!client.owns(sid, SignalTarget::new(1).unwrap()).unwrap());

    // ListSessions, Metrics, and `inspect` report (text + JSON)
    let list = client.list_sessions().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, sid);
    #[cfg(unix)]
    assert!(list[0].foreground.is_some(), "Unix SessionSummary should report foreground process");

    let metrics = client.metrics().unwrap();
    assert_eq!(metrics.live_sessions, 1);
    assert!(metrics.connections >= 1);
    assert!(metrics.total_pty_bytes >= 20, "shard telemetry must report real parsed PTY bytes");

    // Two concurrent ListSessions requests back-to-back must get distinct broker list_ids & both replies!
    let list_req1 = client.alloc_req_id();
    let list_req2 = client.alloc_req_id();
    client.send_raw(apack_proto::types::StreamId::CONTROL, list_req1, &ClientMsg::ListSessions).unwrap();
    client.send_raw(apack_proto::types::StreamId::CONTROL, list_req2, &ClientMsg::ListSessions).unwrap();
    assert!(
        matches!(client.wait_reply(list_req1).unwrap(), BrokerMsg::SessionsList { sessions } if sessions.len() == 1)
    );
    assert!(
        matches!(client.wait_reply(list_req2).unwrap(), BrokerMsg::SessionsList { sessions } if sessions.len() == 1)
    );

    let report = collect_report(handle.socket_path(), &AuthConfig::None).unwrap();
    assert_eq!(report.sessions.len(), 1);
    let text_out = format_report(&report, false);
    assert!(text_out.contains("sessions: 1 live"));
    let json_out = format_report(&report, true);
    assert!(json_out.contains("\"live_sessions\": 1"));
    let _built = build_report(&list, metrics);

    // Detach, verify stream_id can be cleanly re-attached & detached again, then kill session
    client.detach(stream_id).unwrap();
    client
        .attach(
            sid,
            stream_id,
            SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 1, max_bytes: 65_536 },
        )
        .unwrap();
    client.detach(stream_id).unwrap();
    client.kill(sid).unwrap();

    // Wait for session removal
    for _ in 0..100 {
        if client.list_sessions().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(client.list_sessions().unwrap().is_empty());

    client.release_lease(lease_id).unwrap();
    client.send_fin().unwrap();
    drop(client);
    // Explicitly shut down while `handle` is still alive and verify the broker actually stopped!
    handle.shutdown().unwrap();
    assert!(
        BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "after-shutdown").is_err(),
        "broker must stop accepting connections immediately after shutdown()"
    );
}

#[test]
fn e2e_mutual_hmac_authentication_enforced() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-auth");
    let secret = b"super-secret-v2-hmac-key";
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.auth = AuthConfig::Required(derive_auth_secret(secret));
    cfg.shard_count = 1;

    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    // No key -> rejected
    assert!(BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "no-key").is_err());

    // Wrong key -> Unauthorized
    let wrong = AuthConfig::Required(derive_auth_secret(b"wrong-key"));
    let err = BrokerClient::connect(handle.socket_path(), &wrong, "bad-key").err().expect("must fail");
    assert!(matches!(err, ClientError::Broker { code: ErrorCode::Unauthorized, .. } | ClientError::Closed));

    // Correct key -> mutual AuthOk succeeds!
    let right = AuthConfig::Required(derive_auth_secret(secret));
    let mut client = BrokerClient::connect(handle.socket_path(), &right, "good-key").unwrap();
    assert!(client.list_sessions().unwrap().is_empty());
    client.send_fin().unwrap();

    handle.shutdown().unwrap();
}

#[test]
fn e2e_remote_lease_grace_resume_and_recording_rewind_seek_restore() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-rec");
    let rec_dir = tmp.path().join("session-wal");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;

    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    // 1. Acquire a grace-windowed remote lease, spawn a recorded session, disconnect, and resume!
    let token = ResumeToken::new([0x7A; 32]).unwrap();
    let grace = LeaseGraceMs::new(15_000).unwrap();
    let sid = {
        let mut c1 = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "ssh-1").unwrap();
        let (_, resumed) = c1.acquire_lease(LeaseKind::BrokerGrace { grace_ms: grace, resume_token: token }).unwrap();
        assert!(!resumed);

        let (sid, _) = c1
            .spawn(echo_command_spec("RECORDED_MARKER_99", Some(rec_dir.to_string_lossy().into_owned()), None))
            .unwrap();
        let wait = c1.wait(sid, WaitCondition::Text(String::from("RECORDED_MARKER_99")), 10_000).unwrap();
        assert_eq!(wait, WaitOutcome::Met);
        sid
        // Drop c1 without releasing lease -> enters 15s grace window!
    };

    // Reconnect within grace using ResumeToken -> seamlessly resumes without losing session!
    // Wait briefly for the broker reactor to process `c1`'s transport EOF before resuming.
    let mut c2 = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "ssh-2").unwrap();
    for _ in 0..50 {
        if c2.metrics().unwrap().connections == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (_, resumed) = c2.acquire_lease(LeaseKind::BrokerGrace { grace_ms: grace, resume_token: token }).unwrap();
    assert!(resumed, "reconnecting with matching ResumeToken must resume in-grace lease");
    assert_eq!(c2.list_sessions().unwrap().len(), 1);

    // Keep & unkeep toggle works
    c2.keep(sid, true).unwrap();
    c2.keep(sid, false).unwrap();

    // Kill the recorded session so its WAL seals cleanly on disk!
    c2.kill(sid).unwrap();
    for _ in 0..100 {
        if c2.list_sessions().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(c2.list_sessions().unwrap().is_empty());

    // 2. Rewind the recording (must have stayed in a single segment for small output) and Seek!
    let reg_single = apack_wal::RecordingRegistry::new();
    let wal_single = apack_wal::WalReader::open(&rec_dir, &reg_single).unwrap();
    assert_eq!(wal_single.segments().len(), 1, "small output below target must stay in 1 WAL segment");
    drop(wal_single);

    let (rew_sid, duration_ms, at_ms) = c2.rewind(rec_dir.to_string_lossy().into_owned(), None).unwrap();
    assert_eq!(at_ms, duration_ms);
    let sought = c2.seek(rew_sid, duration_ms).unwrap();
    assert_eq!(sought, duration_ms);

    let rew_stream = NonZeroStreamId::new(2).unwrap();
    let rew_attached = c2
        .attach(
            rew_sid,
            rew_stream,
            SubFlags::from_bits(SubFlags::VIEW).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 4, max_bytes: 256 * 1024 },
        )
        .unwrap();
    let rew_epoch = match rew_attached {
        BrokerMsg::Attached { epoch, .. } => epoch,
        other => panic!("unexpected attach: {other:?}"),
    };
    let rew_search = c2
        .stream_request(
            rew_stream,
            &ClientMsg::SetSearch { query: String::from("RECORDED_MARKER_99"), case_sensitive: true },
        )
        .unwrap();
    match rew_search {
        BrokerMsg::SearchMatches { matches, .. } => {
            assert!(!matches.is_empty(), "rewound session must contain recorded output");
        }
        other => panic!("unexpected rewound search reply: {other:?}"),
    }
    let _ = rew_epoch;
    c2.kill(rew_sid).unwrap();

    // 3. Restore from the recording into a brand-new live session with a DIFFERENT size (100x30 != 80x24)
    //    so restore aligns VT geometry to the live PTY!
    let mut restore_spec = echo_command_spec("LIVE_AFTER_RESTORE", None, Some(rec_dir.to_string_lossy().into_owned()));
    restore_spec.size = GridSize::new(100, 30).unwrap();
    let (restored_sid, _) = c2.spawn(restore_spec).unwrap();
    assert_eq!(
        c2.wait(restored_sid, WaitCondition::Text(String::from("LIVE_AFTER_RESTORE")), 10_000).unwrap(),
        WaitOutcome::Met
    );
    let res_stream = NonZeroStreamId::new(3).unwrap();
    let res_att = c2
        .attach(
            restored_sid,
            res_stream,
            SubFlags::from_bits(SubFlags::VIEW).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 4, max_bytes: 256 * 1024 },
        )
        .unwrap();
    if let BrokerMsg::Attached { size, .. } = res_att {
        assert_eq!(size, GridSize::new(100, 30).unwrap(), "restored session must resize VT to match SpawnSpec.size");
    }
    let res_search = c2
        .stream_request(
            res_stream,
            &ClientMsg::SetSearch { query: String::from("RECORDED_MARKER_99"), case_sensitive: true },
        )
        .unwrap();
    match res_search {
        BrokerMsg::SearchMatches { matches, .. } => {
            assert!(!matches.is_empty(), "restored session must retain prior recorded output");
        }
        other => panic!("unexpected restore search reply: {other:?}"),
    }
    c2.kill(restored_sid).unwrap();
    c2.send_fin().unwrap();
    handle.shutdown().unwrap();
}
