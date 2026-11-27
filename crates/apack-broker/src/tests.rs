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
use crate::config::{
    AUTH_KEY_ENV, BrokerConfig, CliCommand, clamp_shard_count, derive_auth_secret, parse_cli, resolve_socket_path,
};
use crate::inspect::{build_report, collect_report, format_report};
use crate::proxy::{
    PROXY_REPLAY_CAP, ReplayBuffer, connect_or_autostart, connect_or_autostart_with, run_proxy_over, run_stdio_proxy,
    should_buffer_replay, should_reconnect_proxy,
};
use crate::runtime::BrokerServer;
#[cfg(unix)]
use crate::shard::drain_unix_pty_nonblocking;
use crate::shard::{
    ReactorEvent, ShardCmd, ShardShared, apply_seek_slice, argv_matches_app, capture_keyframe, maybe_compress_idle,
    parse_exit_status, resolve_scrollback_lines, shard_session_wait_timeout, should_forward_probe_reply, spawn_shard,
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
    let argv = vec![String::from("/bin/sh"), String::from("-c"), format!("printf '{text}\\r\\n'; sleep 10")];
    #[cfg(windows)]
    let argv = vec![String::from("cmd.exe"), String::from("/C"), format!("echo {text} & ping -n 10 127.0.0.1 >nul")];
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

    assert!(parse_cli(&[]).unwrap_err().contains("usage: apack-terminal"));
    assert_eq!(parse_cli(&args(&["serve"])).unwrap(), CliCommand::Serve { socket: None, shards: None });
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

#[test]
fn e2e_leases_kept_pinning_tier2_compression_foreground_verify_and_kill_escalation() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "e2e-tier2");
    let force_rec_dir = tmp.path().join("wal-force");

    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    cfg.idle_compress_after = Duration::from_millis(25);
    cfg.kill_escalate_after = Duration::from_millis(400);
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
            String::from("trap 'exit 0' HUP; printf 'EPHEM_ALIVE\\r\\n'; sleep 10 & wait"),
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

    // Failed `Spawn` (missing `restore_from` directory) must return an error cleanly without leaking `pending_spawns`!
    let bad_restore_spec = echo_command_spec(
        "NO_RESTORE",
        None,
        Some(tmp.path().join("does-not-exist-wal").to_string_lossy().into_owned()),
    );
    assert!(matches!(c2.spawn(bad_restore_spec), Err(ClientError::Broker { code: ErrorCode::Pty, .. })));

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
            "trap '' HUP; sleep 10"
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
                String::from("trap 'sleep 0.04; exit 7' HUP; printf 'HUP_TRAP_READY\\r\\n'; sleep 10 & wait $!"),
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

        // Verify that killing a session whose leader obeys SIGHUP immediately ALSO terminates a
        // SIGHUP-ignoring background job in its own process group (`finish_and_remove_session`).
        struct KillPidOnDrop(u32);
        impl Drop for KillPidOnDrop {
            fn drop(&mut self) {
                if self.0 > 1 {
                    let _ = std::process::Command::new("kill")
                        .args(["-9", &self.0.to_string()])
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
        let bg_pid_file = tmp.path().join("bg-job.pid");
        let bg_spec = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                format!(
                    "set -m; /bin/sh -c 'trap \"\" HUP; echo $$ > \"$1\"; exec sleep 10' _ '{}' & while [ ! -s '{}' ]; do sleep 0.01; done; printf 'BG_JOB_READY\\r\\n'; sleep 10 & wait",
                    bg_pid_file.display(),
                    bg_pid_file.display(),
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
        let (s_bg, _) = c2.spawn(bg_spec).unwrap();
        assert_eq!(c2.wait(s_bg, WaitCondition::Text(String::from("BG_JOB_READY")), 3_000).unwrap(), WaitOutcome::Met);
        let bg_pid: u32 = std::fs::read_to_string(&bg_pid_file).unwrap().trim().parse().unwrap();
        let _bg_guard = KillPidOnDrop(bg_pid);
        let bg_alive = || {
            std::process::Command::new("kill")
                .args(["-0", &bg_pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(bg_pid > 1 && bg_alive(), "background job must be alive before kill()");
        let st_bg = NonZeroStreamId::new(4).unwrap();
        c2.attach(
            s_bg,
            st_bg,
            SubFlags::from_bits(SubFlags::EVENTS).unwrap(),
            None,
            0,
            FrameCredit { max_frames: 1, max_bytes: 65_536 },
        )
        .unwrap();
        c2.kill(s_bg).unwrap();
        loop {
            let frame = c2.recv_frame().unwrap();
            if frame.stream_id == st_bg.as_stream_id() && matches!(frame.msg, BrokerMsg::Exited { .. }) {
                break;
            }
        }
        let bg_gone = (0..40).any(|_| {
            if !bg_alive() {
                true
            } else {
                std::thread::sleep(Duration::from_millis(20));
                false
            }
        });
        assert!(bg_gone, "kill() must terminate HUP-ignoring background job in its own process group");

        // Verify `kill()` on a Paused session with buffered output unpauses reads so macOS/BSD
        // `ttydrain()` does not wedge the exiting child, AND unit-test `drain_unix_pty_nonblocking`!
        let tail_spec = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from("printf 'TAIL_ARMED\\r\\n'; read _go; printf 'TAIL_BEFORE_EXIT_99\\r\\n'; sleep 10"),
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
        // Barrier: `list_sessions` roundtrips through the shard, ensuring `Pause` is active first.
        let _ = c2.list_sessions().unwrap();
        c2.send_raw(st_tail.as_stream_id(), RequestId::PUSH, &ClientMsg::Write { data: b"go\n".to_vec() }).unwrap();
        std::thread::sleep(Duration::from_millis(60));
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
        // Empty scratch slice must return immediately without spinning on `Bytes(0)`.
        drain_unix_pty_nonblocking(&spawned.master, &mut [], 256, |_| panic!("empty buffer must not emit chunks"));
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

    // 1. is_graceful_win_close + can_accept_win_conn + wait_for_win_wake resets *woken = false and returns immediately
    assert!(crate::transport_windows::can_accept_win_conn(false, 0));
    assert!(!crate::transport_windows::can_accept_win_conn(true, 0));
    assert!(!crate::transport_windows::can_accept_win_conn(false, crate::transport_loop::MAX_CONNECTIONS));
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

    // 2b. try_push_win_write handles Ok(true), bounded-channel Full -> Ok(false)+pending, and Disconnected -> Err(())
    let (wtx, wrx) = std::sync::mpsc::sync_channel::<crate::transport_windows::WinWriteMsg>(1);
    let mut pending = None;
    assert_eq!(crate::transport_windows::try_push_win_write(&wtx, &mut pending, vec![1, 2]), Ok(true));
    assert!(pending.is_none());
    assert_eq!(crate::transport_windows::try_push_win_write(&wtx, &mut pending, vec![3, 4]), Ok(false));
    assert_eq!(pending, Some(vec![3, 4]));
    drop(wrx);
    assert_eq!(crate::transport_windows::try_push_win_write(&wtx, &mut pending, vec![5]), Err(()));

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

    // 4. Windows non-blocking draining_win: clean sole-client pipelined Metrics + FIN_FRAME
    //    delivers all replies and auto-exits promptly (< 350ms, proving writer_done short-circuits
    //    GRACEFUL_DRAIN_TIMEOUT), while a stalled sole peer is reaped after 500ms!
    let do_hello = |s: &mut interprocess::local_socket::Stream| {
        let mut hello = Vec::new();
        ClientMsg::Hello {
            protocol_version: apack_proto::PROTOCOL_VERSION,
            client_nonce: [7u8; 32],
            client_name: String::from("win-drain"),
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
    const WIN_BURST_N: u32 = 1_500;
    for i in 0..WIN_BURST_N {
        ClientMsg::Metrics
            .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(i + 2), &mut batch)
            .unwrap();
    }
    batch.extend_from_slice(&apack_proto::FIN_FRAME);

    // 4a. Sole client pipelines 1,500 Metrics (>> WIN_WRITE_WINDOW) + FIN_FRAME.
    let dir_solo = tempdir().unwrap();
    let ep_solo = unique_endpoint(dir_solo.path(), "win-drain-solo");
    let mut cfg_solo = BrokerConfig::for_socket(&ep_solo);
    cfg_solo.shard_count = 1;
    let mut srv_solo = BrokerServer::bind(cfg_solo).unwrap().spawn().unwrap();
    let mut burst = apack_os::transport::connect_local_stream(srv_solo.socket_path()).unwrap();
    do_hello(&mut burst);
    let t_fin = std::time::Instant::now();
    burst.write_all(&batch).unwrap();
    burst.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let mut all_replies = Vec::new();
    let _ = burst.read_to_end(&mut all_replies);
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
    assert_eq!(got_replies, WIN_BURST_N, "draining_win must keep broker alive and deliver all replies");
    while !srv_solo.is_finished() && t_fin.elapsed() < Duration::from_millis(1_000) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        srv_solo.is_finished() && t_fin.elapsed() < Duration::from_millis(450),
        "broker must auto-exit promptly once writer_done is true (before 500ms timeout): elapsed={:?}",
        t_fin.elapsed()
    );
    srv_solo.shutdown().unwrap();

    // 4b. Sole stalled peer pipelines WIN_BURST_N Metrics + FIN_FRAME and never reads ->
    //     reap_draining_win_conns must forcibly close_abrupt after 500ms so broker auto-exits!
    let dir_reap = tempdir().unwrap();
    let ep_reap = unique_endpoint(dir_reap.path(), "win-drain-reap");
    let mut cfg_reap = BrokerConfig::for_socket(&ep_reap);
    cfg_reap.shard_count = 1;
    let mut srv_reap = BrokerServer::bind(cfg_reap).unwrap().spawn().unwrap();
    let mut stalled = apack_os::transport::connect_local_stream(srv_reap.socket_path()).unwrap();
    do_hello(&mut stalled);
    stalled.write_all(&batch).unwrap();
    stalled.flush().unwrap();
    let reap_wait = std::time::Instant::now();
    while !srv_reap.is_finished() && reap_wait.elapsed() < Duration::from_millis(2_000) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(srv_reap.is_finished(), "reap_draining_win_conns must drop stalled peer after 500ms so broker auto-exits");
    let mut discard = Vec::new();
    let _ = stalled.read_to_end(&mut discard);
    assert!(discard.len() < all_replies.len());
    srv_reap.shutdown().unwrap();

    // 4c. Small pipelined Metrics + FIN_FRAME where writer_done becomes true and reap_draining_win_conns
    //     runs BEFORE the client reads: reap must NOT call close_abrupt (DisconnectNamedPipe), so the
    //     unread reply remains preserved in the pipe buffer!
    let dir_small = tempdir().unwrap();
    let ep_small = unique_endpoint(dir_small.path(), "win-drain-small");
    let mut cfg_small = BrokerConfig::for_socket(&ep_small);
    cfg_small.shard_count = 1;
    let mut srv_small = BrokerServer::bind(cfg_small).unwrap().spawn().unwrap();
    let mut small = apack_os::transport::connect_local_stream(srv_small.socket_path()).unwrap();
    do_hello(&mut small);
    let mut one_req = Vec::new();
    ClientMsg::Metrics
        .encode_to_frame(apack_proto::types::StreamId::CONTROL, RequestId::from_raw(2), &mut one_req)
        .unwrap();
    one_req.extend_from_slice(&apack_proto::FIN_FRAME);
    small.write_all(&one_req).unwrap();
    small.flush().unwrap();
    let wait_small = std::time::Instant::now();
    while !srv_small.is_finished() && wait_small.elapsed() < Duration::from_millis(1_000) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(srv_small.is_finished(), "broker must finish reaping clean small draining_win conn");
    let mut small_reply = Vec::new();
    let _ = small.read_to_end(&mut small_reply);
    let mut small_dec = apack_proto::frame::FrameDecoder::new();
    let mut small_rest = small_reply.as_slice();
    let mut small_frames = 0u32;
    while !small_rest.is_empty() {
        let (used, ev) = small_dec.step(small_rest).unwrap();
        small_rest = &small_rest[used..];
        if matches!(ev, Some(apack_proto::frame::DecodedFrame::Frame { .. })) {
            small_frames += 1;
        }
    }
    assert_eq!(small_frames, 1, "reap_draining_win_conns must preserve unread reply when writer_done is true");
    srv_small.shutdown().unwrap();
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
    let argv =
        vec![String::from("/bin/sh"), String::from("-c"), String::from("printf 'MK=%s\\r\\n' \"$APACK_TERMINAL_KEY\"")];
    #[cfg(windows)]
    let argv = vec![String::from("cmd.exe"), String::from("/C"), String::from("echo MK=%APACK_TERMINAL_KEY%")];
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
