#[cfg(unix)]
use std::collections::VecDeque;
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use apack_core::auth::AuthConfig;
use apack_core::marks::MarksMode;
use apack_core::session::SessionMachine;
use apack_core::vt::VtEngine;
use apack_proto::messages::{BrokerMsg, ClientMsg, SpawnSpec};
use apack_proto::types::{
    CellPos, CellPx, CellSpan, ErrorCode, FrameCredit, GridSize, LeaseGraceMs, LeaseKind, NonZeroStreamId, RequestId,
    ResizePolicy, ResumeToken, SessionId, SignalTarget, SubFlags, ViewportHint, WaitCondition, WaitOutcome,
};
#[cfg(unix)]
use apack_proto::types::{Confirmed, SignalKind, SignalScope};
use apack_proto::{FIN_FRAME, PROTOCOL_VERSION};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine};
use tempfile::tempdir;

use crate::client::{BrokerClient, ClientError};
use crate::config::{BrokerConfig, CliCommand, clamp_shard_count, derive_auth_secret, parse_cli, resolve_socket_path};
use crate::inspect::{build_report, collect_report, format_report};
use crate::proxy::{
    PROXY_REPLAY_CAP, ReplayBuffer, connect_or_autostart, connect_or_autostart_with, run_proxy_over,
    should_buffer_replay, should_reconnect_proxy,
};
use crate::runtime::BrokerServer;
#[cfg(unix)]
use crate::shard::drain_unix_pty_nonblocking;
use crate::shard::{
    ReactorEvent, ShardCmd, ShardShared, apply_seek_slice, argv_matches_app, capture_keyframe, maybe_compress_idle,
    parse_exit_status, resolve_scrollback_lines, shard_session_wait_timeout, spawn_shard,
};
#[cfg(unix)]
use crate::transport_loop::{
    NonBlockErr, advance_write_queue, can_accept_unix_conn, classify_nonblocking_err, write_queue_can_pull,
};
use crate::transport_loop::{
    PendingList, compute_shutdown_deadline, pick_least_loaded_shard, prune_pending_lists_for_conn, reactor_should_exit,
    reactor_wait_timeout, ready_stalled_stream_ids,
};
use crate::{abi_hash_for_features, engine_abi_hash, fill_random, now_unix_ms, read_retry_eintr};

#[cfg(windows)]
static PIPE_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_endpoint(dir: &std::path::Path, label: &str) -> String {
    #[cfg(unix)]
    {
        dir.join(format!("{label}.sock")).to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        let id = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
        format!(r"\\.\pipe\apack-v2-test-{label}-{}-{id}", std::process::id())
    }
}

fn echo_command_spec(text: &str, record_dir: Option<String>, restore_from: Option<String>) -> SpawnSpec {
    #[cfg(unix)]
    let argv = vec![String::from("/bin/sh"), String::from("-c"), format!("printf '{text}\\r\\n'; sleep 30")];
    #[cfg(windows)]
    let argv = vec![String::from("cmd.exe"), String::from("/C"), format!("echo {text} & ping -n 30 127.0.0.1 >nul")];
    SpawnSpec {
        argv,
        cwd: None,
        env: Vec::new(),
        size: GridSize::new(80, 24).unwrap(),
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_limit: 500,
        marks_key: None,
        resize_policy: ResizePolicy::LatestActive,
        record_dir,
        restore_from,
        image_files: false,
        kept: false,
    }
}

#[test]
fn cli_parsing_config_and_helpers() {
    let args = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    assert_eq!(parse_cli(&[]).unwrap(), CliCommand::Serve { socket: None, shards: None });
    assert_eq!(
        parse_cli(&args(&["serve", "--socket", "/tmp/a.sock", "--shards", "8"])).unwrap(),
        CliCommand::Serve { socket: Some(String::from("/tmp/a.sock")), shards: Some(8) }
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

    // argv_matches_app
    let argv = vec![String::from("/usr/local/bin/Claude"), String::from("--flag")];
    assert!(argv_matches_app(&argv, "claude"));
    assert!(!argv_matches_app(&argv, "vim"));
    assert!(!argv_matches_app(&argv, "   "));

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
    let token = ResumeToken::new([0x7A; 32]);
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
    let mut c2 = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "ssh-2").unwrap();
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
        Ok(())
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
            Ok(())
        })
        .is_err()
    );
    assert_eq!(noauto_spawns, 0, "autostart=false must not invoke spawner");

    let mut noop_spawns = 0usize;
    assert!(
        connect_or_autostart_with(&auto_timeout_sock, true, Duration::from_millis(35), |_| {
            noop_spawns += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(noop_spawns, 1);

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
    stalled.insert(s1, (SessionId::new(1).unwrap(), dummy_frame.clone()));
    stalled.insert(s2, (SessionId::new(1).unwrap(), dummy_frame));
    let mut queued = std::collections::HashMap::new();
    queued.insert(s1, SessionId::new(1).unwrap());
    assert_eq!(ready_stalled_stream_ids(&stalled, &queued), vec![s2]);
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

    // 1. Lossless restore on matching ABI
    let mut eng_same = GhosttyEngine::new(GhosttyConfig::new(size)).unwrap();
    apply_seek_slice(&mut eng_same, &slice, &abi);
    let mut hits = Vec::new();
    let m1 = eng_same.meta();
    eng_same.search_lines(0, m1.first_active_line + u64::from(m1.size.rows.get()), "HIST_LINE_1", true, 16, &mut hits);
    assert!(!hits.is_empty());

    // 2. Visual fallback on mismatched ABI: verify exact row separation (no missing or extra leading CRLF!)
    let wrong_abi = [0xEE; 32];
    let mut eng_fallback = GhosttyEngine::new(GhosttyConfig::new(size)).unwrap();
    apply_seek_slice(&mut eng_fallback, &slice, &wrong_abi);
    let m2 = eng_fallback.meta();
    let mut fb_rows = Vec::new();
    eng_fallback.extract_history_rows(0, m2.first_active_line as u32, &mut fb_rows);
    let vp_idx: Vec<u16> = (0..m2.size.rows.get()).collect();
    eng_fallback.extract_viewport_rows(&vp_idx, &mut fb_rows);
    let lines: Vec<&str> = fb_rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(lines[0], "HIST_LINE_1", "row 0 must not have a leading blank line: {lines:?}");
    assert_eq!(lines[1], "HIST_LINE_2", "row 1 must be separated by CRLF: {lines:?}");
    assert_eq!(lines[2], "", "blank row 2 must be preserved: {lines:?}");
    assert_eq!(lines[3], "VP_LINE_A", "row 3 must follow blank row: {lines:?}");
    assert_eq!(lines[4], "VP_LINE_B", "row 4 must follow row 3: {lines:?}");
    assert_eq!(lines[5], "VP_LINE_C", "row 5 must follow row 4: {lines:?}");
}

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
    std::thread::sleep(Duration::from_millis(25));

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
        std::thread::sleep(Duration::from_millis(25));
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
                String::from("i=0; while [ $i -lt 25 ]; do printf 'SB_%02d\\r\\n' $i; i=$((i+1)); done; sleep 30"),
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
                    "stty -echo raw; printf 'READY_BW\\r\\n'; sleep 0.08; grep -m1 -o BIG_WRITE_TAIL; sleep 30",
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

#[test]
fn e2e_leases_kept_pinning_tier2_compression_foreground_verify_and_kill_escalation() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-tier2");
    let force_rec_dir = tmp.path().join("wal-force");

    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    cfg.idle_compress_after = Duration::from_millis(25);
    cfg.kill_escalate_after = Duration::from_millis(200);
    cfg.wal_config.segment_target_bytes = apack_wal::MIN_SEGMENT_TARGET_BYTES;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    // 1. Holder client acquires BrokerImmediate lease and spawns:
    //    - s_kept (kept = true, survives lease drop)
    //    - s_pinned (kept = false, pinned by a Session lease from client2, dies when client2 releases)
    //    - s_ephemeral (kept = false, unpinned, dies when holder releases broker lease)
    let mut holder = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "holder").unwrap();
    let (broker_lease, _) = holder.acquire_lease(LeaseKind::BrokerImmediate).unwrap();

    let mut spec_kept = echo_command_spec("KEPT_ALIVE", None, None);
    spec_kept.kept = true;
    let (s_kept, _) = holder.spawn(spec_kept).unwrap();
    let (s_pinned, _) = holder.spawn(echo_command_spec("PINNED_ALIVE", None, None)).unwrap();
    #[cfg(unix)]
    let spec_ephem = SpawnSpec {
        argv: vec![
            String::from("/bin/sh"),
            String::from("-c"),
            String::from("trap 'exit 0' HUP; printf 'EPHEM_ALIVE\\r\\n'; sleep 30 & wait"),
        ],
        cwd: None,
        env: Vec::new(),
        size: GridSize::new(80, 24).unwrap(),
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_limit: 500,
        marks_key: None,
        resize_policy: ResizePolicy::LatestActive,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    };
    #[cfg(windows)]
    let spec_ephem = echo_command_spec("EPHEM_ALIVE", None, None);
    let (s_ephem, _) = holder.spawn(spec_ephem).unwrap();
    assert_eq!(
        holder.wait(s_ephem, WaitCondition::Text(String::from("EPHEM_ALIVE")), 5_000).unwrap(),
        WaitOutcome::Met
    );

    let mut c2 = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "pinner").unwrap();
    let (sess_lease, _) = c2.acquire_lease(LeaseKind::Session(s_pinned)).unwrap();
    // Acquire AND release a session lease on s_ephem WHILE broker_lease is still active:
    // SessionUnpinned must NOT kill s_ephem because broker_lease_active == true!
    let (ephem_lease, _) = c2.acquire_lease(LeaseKind::Session(s_ephem)).unwrap();
    c2.release_lease(ephem_lease).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    let still_alive: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
    assert!(still_alive.contains(&s_ephem), "unpinning a session while broker lease is active must NOT kill it");

    // Release broker lease -> TeardownNonKept kills s_ephem, keeps s_kept AND s_pinned!
    holder.release_lease(broker_lease).unwrap();
    for _ in 0..100 {
        let ids: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
        if !ids.contains(&s_ephem) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let after_broker_drop: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
    assert!(after_broker_drop.contains(&s_kept));
    assert!(after_broker_drop.contains(&s_pinned));
    assert!(!after_broker_drop.contains(&s_ephem));

    // Release session lease -> SessionUnpinned terminates s_pinned while s_kept survives!
    c2.release_lease(sess_lease).unwrap();
    for _ in 0..100 {
        let ids: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
        if !ids.contains(&s_pinned) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let after_pin_drop: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
    assert!(after_pin_drop.contains(&s_kept));
    assert!(!after_pin_drop.contains(&s_pinned));

    // Re-acquire broker lease for remaining tests; leave `s_kept` ALIVE through `handle.shutdown()`
    // below to prove explicit broker shutdown terminates kept sessions cleanly without deadlocking!
    let (_bl2, _) = c2.acquire_lease(LeaseKind::BrokerImmediate).unwrap();

    // `SpawnSpec { image_files: true }` must be rejected cleanly (and not leak `pending_spawns`)!
    let mut bad_img_spec = echo_command_spec("NO_IMG", None, None);
    bad_img_spec.image_files = true;
    assert!(matches!(c2.spawn(bad_img_spec), Err(ClientError::Broker { code: ErrorCode::Malformed, .. })));

    // 2. WAL rollover (> 64 KiB output) + idle compression (25 ms) + Unix foreground verify + SIGKILL escalation!
    #[cfg(unix)]
    {
        let rec_dir = tmp.path().join("wal-roll");
        // Emit > 80 KiB output (exceeding MIN_SEGMENT_TARGET_BYTES = 64 KiB for deterministic WAL
        // rollover + scrollback lines for idle compression), then wait for stdin before OSC 7501!
        let script = concat!(
            "i=0; while [ $i -lt 1400 ]; do ",
            "printf 'ROLLOVER_LINE_%04d_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\\r\\n' $i; ",
            "i=$((i+1)); done; ",
            "read _go; ",
            "printf '\\033]7501;id=st_ok:app=sh:state=working\\007'; ",
            "printf '\\033]7501;id=st_bad:app=nonexistent_app_xyz:state=working\\007'; ",
            "trap '' HUP; sleep 30"
        );
        let spec_roll = SpawnSpec {
            argv: vec![String::from("/bin/sh"), String::from("-c"), String::from(script)],
            cwd: None,
            env: Vec::new(),
            size: GridSize::new(80, 10).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_limit: 500,
            marks_key: None,
            resize_policy: ResizePolicy::LatestActive,
            record_dir: Some(rec_dir.to_string_lossy().into_owned()),
            restore_from: None,
            image_files: false,
            kept: false,
        };
        let (s_roll, _) = c2.spawn(spec_roll).unwrap();
        assert_eq!(
            c2.wait(s_roll, WaitCondition::Text(String::from("ROLLOVER_LINE_1399")), 5_000).unwrap(),
            WaitOutcome::Met
        );
        std::thread::sleep(Duration::from_millis(30));
        let st = NonZeroStreamId::new(1).unwrap();
        c2.attach(
            s_roll,
            st,
            SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 1, max_bytes: 65_536 },
        )
        .unwrap();
        c2.send_raw(st.as_stream_id(), RequestId::PUSH, &ClientMsg::Write { data: b"go\n".to_vec() }).unwrap();

        // Wait for both OSC 7501 foreground verification confirmations (`Matched` and `Mismatched`).
        let mut saw_matched = false;
        let mut saw_mismatched = false;
        for _ in 0..64 {
            let f = c2.recv_frame().unwrap();
            if let BrokerMsg::ProgramStatus(entry) = f.msg {
                if entry.id == "st_ok" && entry.confirmed == Confirmed::Matched {
                    saw_matched = true;
                }
                if entry.id == "st_bad" && entry.confirmed == Confirmed::Mismatched {
                    saw_mismatched = true;
                }
            }
            if saw_matched && saw_mismatched {
                break;
            }
        }
        assert!(saw_matched, "verify_foreground must confirm Matched for sh");
        assert!(saw_mismatched, "verify_foreground must confirm Mismatched for nonexistent_app_xyz");

        // Let idle compression (25 ms) run cooperatively on the 50+ scrollback lines.
        std::thread::sleep(Duration::from_millis(80));
        let metrics = c2.metrics().unwrap();
        assert!(metrics.scrollback_bytes > 0);

        // Kill s_roll (which traps SIGHUP) -> Tier 2 escalates to SIGKILL after 200 ms and reaps!
        c2.kill(s_roll).unwrap();
        for _ in 0..100 {
            let ids: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
            if !ids.contains(&s_roll) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!c2.list_sessions().unwrap().iter().any(|s| s.id == s_roll));

        // Verify WAL rolled over into >= 2 segments and Rewind rejects Signal with Malformed!
        let reg_check = apack_wal::RecordingRegistry::new();
        let wal_check = apack_wal::WalReader::open(&rec_dir, &reg_check).unwrap();
        assert!(
            wal_check.segments().len() >= 2,
            "expected WAL segment rollover (>=2 segments), found {}",
            wal_check.segments().len()
        );
        drop(wal_check);

        let (rew_sid, _, _) = c2.rewind(rec_dir.to_string_lossy().into_owned(), Some(0)).unwrap();
        assert!(matches!(
            c2.signal(rew_sid, SignalKind::Term, SignalScope::Group),
            Err(ClientError::Broker { code: ErrorCode::Malformed, .. })
        ));
        c2.kill(rew_sid).unwrap();

        // Verify graceful SIGHUP runs BEFORE SIGKILL escalation (`kill_escalate_after = 120ms`):
        // a shell trapping SIGHUP and exiting with code 7 must exit cleanly with Some(7) (not SIGKILL)!
        let hup_spec = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from("trap 'sleep 0.04; exit 7' HUP; printf 'HUP_TRAP_READY\\r\\n'; sleep 30 & wait"),
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
        let (s_hup, _) = c2.spawn(hup_spec).unwrap();
        assert_eq!(
            c2.wait(s_hup, WaitCondition::Text(String::from("HUP_TRAP_READY")), 3_000).unwrap(),
            WaitOutcome::Met
        );
        let st_hup = NonZeroStreamId::new(2).unwrap();
        c2.attach(
            s_hup,
            st_hup,
            SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 1, max_bytes: 65_536 },
        )
        .unwrap();
        c2.kill(s_hup).unwrap();
        let hup_exit = loop {
            let frame = c2.recv_frame().unwrap();
            if frame.stream_id == st_hup.as_stream_id()
                && let BrokerMsg::Exited { exit_code, signal } = frame.msg
            {
                break (exit_code, signal);
            }
        };
        assert_eq!(
            hup_exit,
            (Some(7), None),
            "kill() must deliver graceful SIGHUP and wait kill_escalate_after before SIGKILL"
        );

        // Verify `kill()` on a Paused session with buffered output unpauses reads so macOS/BSD
        // `ttydrain()` does not wedge the exiting child, AND unit-test `drain_unix_pty_nonblocking`!
        let tail_spec = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from("printf 'TAIL_ARMED\\r\\n'; sleep 0.04; printf 'TAIL_BEFORE_EXIT_99\\r\\n'; sleep 30"),
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
        let (s_tail, _) = c2.spawn(tail_spec).unwrap();
        assert_eq!(c2.wait(s_tail, WaitCondition::Text(String::from("TAIL_ARMED")), 3_000).unwrap(), WaitOutcome::Met);
        let st_tail = NonZeroStreamId::new(3).unwrap();
        c2.attach(
            s_tail,
            st_tail,
            SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 4, max_bytes: 256 * 1024 },
        )
        .unwrap();
        // Drain initial full-sync frame and grant credit before pausing.
        let init_tail = loop {
            if let BrokerMsg::Frame(upd) = c2.recv_frame().unwrap().msg {
                break upd;
            }
        };
        c2.send_raw(
            st_tail.as_stream_id(),
            RequestId::PUSH,
            &ClientMsg::FrameAck { seq: init_tail.seq, credit: FrameCredit { max_frames: 4, max_bytes: 256 * 1024 } },
        )
        .unwrap();
        c2.send_raw(st_tail.as_stream_id(), RequestId::PUSH, &ClientMsg::Pause).unwrap();
        std::thread::sleep(Duration::from_millis(70));
        c2.kill(s_tail).unwrap();
        let mut saw_tail = false;
        let mut saw_exited = false;
        for _ in 0..64 {
            let f = c2.recv_frame().unwrap();
            match f.msg {
                BrokerMsg::Frame(upd) if f.stream_id == st_tail.as_stream_id() => {
                    if upd.rows.iter().any(|r| r.text.contains("TAIL_BEFORE_EXIT_99")) {
                        saw_tail = true;
                    }
                    c2.send_raw(
                        st_tail.as_stream_id(),
                        RequestId::PUSH,
                        &ClientMsg::FrameAck {
                            seq: upd.seq,
                            credit: FrameCredit { max_frames: 4, max_bytes: 256 * 1024 },
                        },
                    )
                    .unwrap();
                }
                BrokerMsg::Exited { .. } if f.stream_id == st_tail.as_stream_id() => {
                    saw_exited = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_tail, "killing a paused session must unpause & drain buffered tail bytes cleanly");
        assert!(saw_exited, "killed paused session must deliver Exited cleanly without ttydrain deadlock");

        // Direct unit check of `drain_unix_pty_nonblocking` on a real Unix PTY master.
        let pty_cfg = apack_os::pty::PtySpawnConfig {
            size: GridSize::new(80, 10).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            shell: Some(String::from("/bin/sh")),
            args: vec![String::from("-c"), String::from("printf 'DIRECT_DRAIN_OK'")],
            env: Vec::new(),
            unset: Vec::new(),
            cwd: None,
        };
        let mut spawned = apack_os::pty::spawn_pty(&pty_cfg).unwrap();
        let mut drained = Vec::new();
        let mut scratch = [0u8; 256];
        for _ in 0..50 {
            drain_unix_pty_nonblocking(&spawned.master, &mut scratch, 256, |chunk| drained.extend_from_slice(chunk));
            if String::from_utf8_lossy(&drained).contains("DIRECT_DRAIN_OK") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(String::from_utf8_lossy(&drained).contains("DIRECT_DRAIN_OK"));
        let _ = spawned.child.wait();
    }

    // 3. Force-drop live recorded session on shutdown() -> seals WAL and kills child cleanly!
    let (s_force, _) = c2
        .spawn(echo_command_spec("FORCE_DROP_SEAL", Some(force_rec_dir.to_string_lossy().into_owned()), None))
        .unwrap();
    assert_eq!(
        c2.wait(s_force, WaitCondition::Text(String::from("FORCE_DROP_SEAL")), 5_000).unwrap(),
        WaitOutcome::Met
    );
    drop(holder);
    drop(c2);
    handle.shutdown().unwrap();

    // Sealed recording from force_drop_session must open cleanly with WalReader AND be sealed!
    let reg = apack_wal::RecordingRegistry::new();
    let reader = apack_wal::WalReader::open(&force_rec_dir, &reg).unwrap();
    assert!(reader.span().is_some());
    assert!(reader.segments()[0].sealed, "force_drop_session must seal active WAL on shutdown");
}

#[cfg(windows)]
#[test]
fn windows_edge_helpers_and_natural_child_exit() {
    use std::sync::{Arc, Condvar, Mutex};

    // 1. is_graceful_win_close + wait_for_win_wake resets *woken = false and returns immediately
    assert!(crate::transport_windows::is_graceful_win_close(apack_core::broker::CloseReason::ClientFin, false));
    assert!(crate::transport_windows::is_graceful_win_close(apack_core::broker::CloseReason::SlowReader, true));
    assert!(!crate::transport_windows::is_graceful_win_close(apack_core::broker::CloseReason::SlowReader, false));
    let cond = Arc::new((Mutex::new(true), Condvar::new()));
    let t0 = std::time::Instant::now();
    crate::transport_windows::wait_for_win_wake(&cond, Duration::from_secs(2));
    assert!(t0.elapsed() < Duration::from_millis(500));
    assert!(!*cond.0.lock().unwrap());

    // 2. wait_for_win_mailbox returns immediately when `woken == true` and clears `woken`
    let mb = Mutex::new(crate::shard::ShardMailboxState { queue: std::collections::VecDeque::new(), woken: true });
    let cv = Condvar::new();
    let t1 = std::time::Instant::now();
    crate::shard_windows::wait_for_win_mailbox(&mb, &cv, Duration::from_secs(2));
    assert!(t1.elapsed() < Duration::from_millis(500));
    assert!(!mb.lock().unwrap().woken);

    // 3. Naturally-exiting Windows child is reaped cleanly without kill(), emitting Exited!
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "win-nat-exit");
    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();
    let mut client = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "win-exit").unwrap();
    let spec = SpawnSpec {
        argv: vec![String::from("cmd.exe"), String::from("/C"), String::from("echo WIN_NATURAL_EXIT")],
        cwd: None,
        env: Vec::new(),
        size: GridSize::new(80, 24).unwrap(),
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_limit: 200,
        marks_key: None,
        resize_policy: ResizePolicy::LatestActive,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    };
    let (sid, _) = client.spawn(spec).unwrap();
    assert_eq!(client.wait(sid, WaitCondition::Exit, 5_000).unwrap(), WaitOutcome::Met);
    client.send_fin().unwrap();
    drop(client);
    handle.shutdown().unwrap();
}

struct ChanReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let avail = self.buf.len() - self.pos;
        let take = avail.min(out.len());
        out[..take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

struct ChanWriter {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}

impl Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.tx.send(buf.to_vec()).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "closed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ChanDuplex {
    reader: ChanReader,
    writer: ChanWriter,
}

impl Read for ChanDuplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for ChanDuplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
