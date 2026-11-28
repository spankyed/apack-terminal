//! Shard-internal unit tests for Windows ConPTY pumping and Kitty named image sources.

#[cfg(all(test, windows))]
mod win_shard_tests {
    use std::collections::VecDeque;

    use apack_core::marks::MarksMode;
    use apack_core::vt::VtEngine;
    use apack_os::pty::{PtySpawnConfig, spawn_pty};
    use apack_os::pty_windows::ConPtyBridge;
    use apack_proto::types::{CellPos, CellPx, CellSpan, GridSize, ResizePolicy};
    use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
    use apack_wal::{RecordingRegistry, WalConfig, WalReaper};

    use super::super::*;
    use crate::shard_windows::{
        WIN_CONPTY_DRAIN_TIMEOUT, apply_win_write_result, clamp_win_session_write_timeout, clamp_win_write_timeout,
        drain_conpty_quantum, drain_windows_pty_writes, finish_or_begin_win_exit_drain, is_win_write_would_block,
        maybe_wait_win_mailbox,
    };

    #[test]
    fn win_shard_helpers_and_drain_windows_ptys_cover_pause_quantum_and_eof() {
        // 1. clamp_win_write_timeout & is_win_write_would_block
        assert_eq!(clamp_win_write_timeout(Duration::from_secs(1), true, 0), Duration::from_secs(1));
        assert_eq!(clamp_win_write_timeout(Duration::from_secs(1), false, 4), Duration::from_secs(1));
        assert_eq!(clamp_win_write_timeout(Duration::from_secs(1), true, 4), Duration::from_millis(10));
        assert_eq!(clamp_win_write_timeout(Duration::from_millis(3), true, 4), Duration::from_millis(3));
        assert!(is_win_write_would_block(&apack_os::OsError::io(
            "test",
            std::io::Error::from(std::io::ErrorKind::WouldBlock),
        )));
        assert!(!is_win_write_would_block(&apack_os::OsError::io(
            "test",
            std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        )));

        // 2. maybe_wait_win_mailbox skips wait when exhausted_quantum == true, waits when false
        let mb = Arc::new(Mutex::new(ShardMailboxState { queue: VecDeque::new(), woken: false }));
        let cv = Arc::new(Condvar::new());
        let t_skip = Instant::now();
        maybe_wait_win_mailbox(true, &mb, &cv, Duration::from_secs(2));
        assert!(t_skip.elapsed() < Duration::from_millis(200));

        let t_wait = Instant::now();
        maybe_wait_win_mailbox(false, &mb, &cv, Duration::from_millis(40));
        assert!(t_wait.elapsed() >= Duration::from_millis(20));

        // 3. ShardRunner::drain_windows_ptys with 0 sessions returns false
        let (rtx, _rrx) = mpsc::channel();
        let mut runner = ShardRunner {
            shard_idx: 0,
            shared: ShardShared {
                image_store: Arc::new(Mutex::new(ImageStore::default())),
                wal_registry: RecordingRegistry::new(),
                wal_reaper: WalReaper::new(),
                wal_config: WalConfig::default(),
                idle_compress_after: Duration::from_secs(60),
                kill_escalate_after: Duration::from_secs(1),
                engine_abi_hash: [0u8; 32],
            },
            reactor_tx: rtx,
            wake_reactor: Arc::new(|| {}),
            mailbox: Arc::clone(&mb),
            load: Arc::new(AtomicUsize::new(0)),
            telemetry: Arc::new(ShardTelemetryAtomics::default()),
            self_wake: ShardWake::Cond(Arc::clone(&mb), Arc::clone(&cv)),
            fg_tx: None,
            fg_worker: None,
            fg_inflight: HashSet::new(),
            fg_pending: HashMap::new(),
            raw_output_inflight: Arc::new(AtomicUsize::new(0)),
            sessions: HashMap::new(),
            cond: Arc::clone(&cv),
            total_pty_bytes: 0,
            running: true,
            tearing_down_all: false,
        };
        assert!(!runner.drain_windows_ptys(1));

        // 4. Two real ConPTY sessions: paused_id stays paused while live_id exhausts budget=1.
        let make_session = |sid: SessionId, cmd: &str, paused: bool| {
            let cfg = PtySpawnConfig {
                size: GridSize::new(80, 24).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
                shell: Some(String::from("cmd.exe")),
                args: vec![String::from("/C"), cmd.to_owned()],
                env: Vec::new(),
                unset: Vec::new(),
                cwd: None,
            };
            let spawned = spawn_pty(&cfg).expect("spawn conpty");
            let bridge = ConPtyBridge::start(spawned.master, || {}).expect("start bridge");
            let mut vt_cfg = GhosttyConfig::new(cfg.size);
            vt_cfg.cell_px = cfg.cell_px;
            vt_cfg.scrollback_lines = 100;
            let engine = GhosttyEngine::new(vt_cfg).expect("ghostty");
            let machine = SessionMachine::new(sid, engine, ResizePolicy::LatestActive, MarksMode::Claimed, cfg.cell_px);
            LiveSession {
                machine,
                pty: Some(LivePty::Windows { bridge, eof: false, write_broken: false }),
                child: Some(spawned.child),
                killer: Some(spawned.killer),
                tree: spawned.tree,
                pid: None,
                kept: false,
                rewound: false,
                created_ms: 0,
                wal: None,
                wal_has_segment: false,
                wal_skip_prefix: 0,
                pending_wal_rollover: false,
                wal_images_gen: 0,
                wal_written_blobs: HashSet::new(),
                rewind_reader: None,
                rewind_at_ms: 0,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: paused,
                paused_by_raw_flow: false,
                paused_by_raw_inflight: false,
                raw_output_inflight: Arc::new(AtomicUsize::new(0)),
                terminating_since: None,
                escalated_sigkill: false,
                view_slot_free: HashSet::new(),
                raw_output_subs: HashSet::new(),
                image_scanner: None,
                image_worker: None,
                image_ticket: 0,
                pending_image: None,
                pending_image_tail: Vec::new(),
                reaped_exit: None,
                exit_drain: None,
            }
        };

        let paused_id = SessionId::new(1).unwrap();
        let live_id = SessionId::new(2).unwrap();
        runner.sessions.insert(paused_id, make_session(paused_id, "echo WIN_PAUSED_ONLY", true));

        // While only paused_id is present, drain_windows_ptys must ingest zero bytes and return false.
        for _ in 0..15 {
            assert!(!runner.drain_windows_ptys(PTY_READ_QUANTUM_BYTES));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(runner.total_pty_bytes, 0, "paused ConPTY session must not be drained at all");

        // Now add live_id (unpaused) and verify budget=1 exhausts while paused_id stays unread.
        runner.sessions.insert(live_id, make_session(live_id, "echo WIN_LIVE_ONLY && set /p _x=", false));

        let mut saw_exhausted = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if runner.drain_windows_ptys(1) {
                saw_exhausted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_exhausted, "unpaused ConPTY session must exhaust budget=1");
        let full_span = CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 23, col: 79 } };
        let paused_text = runner.sessions[&paused_id].machine.vt().text_of(full_span, 4096);
        assert!(!paused_text.contains("WIN_PAUSED_ONLY"), "paused ConPTY must not be drained: {paused_text:?}");

        // Unpause paused_id and drain until its marker appears, plus test drain_conpty_quantum EOF.
        runner.sessions.get_mut(&paused_id).unwrap().paused_by_stream = false;
        let mut saw_paused_after = false;
        let deadline2 = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline2 {
            let _ = runner.drain_windows_ptys(PTY_READ_QUANTUM_BYTES);
            if runner.sessions[&paused_id].machine.vt().text_of(full_span, 4096).contains("WIN_PAUSED_ONLY") {
                saw_paused_after = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_paused_after, "unpaused ConPTY must drain its output");

        // Direct `begin_win_exit_drain` + `drain_conpty_quantum` check for (true, false) EOF and
        // (false, false) empty using live_id's already-active bridge (no 3rd ConPTY spawn or DSR wait).
        let mut live_sess = runner.sessions.remove(&live_id).expect("live session");
        assert!(live_sess.tree.is_some(), "Windows PTY spawn must attach JobGuard via pinned handle");
        // Verify `drain_windows_pty_writes` loops across multiple 64 KiB chunks in a single call
        // while staying below `ConPtyBridge::MAX_INPUT_QUEUE_BYTES` (256 KiB).
        let multi_chunk = vec![b'a'; 70_000];
        live_sess.machine.enqueue_pty_reply(&multi_chunk, &mut Vec::new());
        assert_eq!(live_sess.machine.pty_write_len(), multi_chunk.len());
        drain_windows_pty_writes(&mut live_sess);
        assert_eq!(
            live_sess.machine.pty_write_len(),
            0,
            "drain_windows_pty_writes must drain multiple 64 KiB chunks in one call"
        );
        if let Some(mut killer) = live_sess.killer.take() {
            let _ = killer.kill();
        }
        if let Some(mut child) = live_sess.child.take() {
            let _ = child.wait();
        }
        let drain_now = Instant::now();
        live_sess.paused_by_stream = true;
        live_sess.paused_by_raw_flow = true;
        live_sess.paused_by_raw_inflight = true;
        assert_eq!(finish_or_begin_win_exit_drain(&mut live_sess, (Some(42), None), drain_now), None);
        assert!(!live_sess.paused_by_stream);
        assert!(!live_sess.paused_by_raw_flow);
        assert!(!live_sess.paused_by_raw_inflight);
        let drain_state = live_sess.exit_drain.as_ref().expect("exit_drain set");
        assert_eq!(drain_state.code, Some(42));
        assert_eq!(drain_state.deadline, drain_now + WIN_CONPTY_DRAIN_TIMEOUT);
        let Some(LivePty::Windows { bridge, .. }) = live_sess.pty.take() else {
            panic!("expected Windows LivePty");
        };
        // Verify rewound (`pty: None`) session never clamps timeout to 10ms and does not consume queued writes.
        live_sess.machine.on_pty_output(
            b"\x1b[6n",
            TimestampMs::from_millis(1),
            apack_core::time::WallMs::from_millis(1),
            &mut Vec::new(),
        );
        let rewound_pending = live_sess.machine.pty_write_len();
        assert!(rewound_pending > 0);
        assert_eq!(clamp_win_session_write_timeout(Duration::from_secs(1), &live_sess), Duration::from_secs(1));
        drain_windows_pty_writes(&mut live_sess);
        assert_eq!(live_sess.machine.pty_write_len(), rewound_pending);
        live_sess.machine.consume_pty_write(rewound_pending);
        // Verify WouldBlock retains bytes without latching write_broken, while closed/broken ConPTY
        // bridge latches `write_broken = true` so timeout is no longer clamped to 10ms.
        live_sess.pty = Some(LivePty::Windows { bridge, eof: false, write_broken: false });
        live_sess.machine.on_pty_output(
            b"\x1b[6n",
            TimestampMs::from_millis(2),
            apack_core::time::WallMs::from_millis(2),
            &mut Vec::new(),
        );
        let pending_before = live_sess.machine.pty_write_len();
        assert!(!apply_win_write_result(
            &mut live_sess,
            pending_before,
            Err(apack_os::OsError::io("full", std::io::Error::from(std::io::ErrorKind::WouldBlock))),
        ));
        assert_eq!(live_sess.machine.pty_write_len(), pending_before);
        assert!(matches!(live_sess.pty, Some(LivePty::Windows { write_broken: false, .. })));
        assert_eq!(clamp_win_session_write_timeout(Duration::from_secs(1), &live_sess), Duration::from_millis(10));
        drain_windows_pty_writes(&mut live_sess);
        assert!(matches!(live_sess.pty, Some(LivePty::Windows { write_broken: true, .. })));
        assert_eq!(clamp_win_session_write_timeout(Duration::from_secs(1), &live_sess), Duration::from_secs(1));
        let Some(LivePty::Windows { bridge, .. }) = live_sess.pty.take() else {
            panic!("expected Windows LivePty");
        };
        assert_eq!(finish_or_begin_win_exit_drain(&mut live_sess, (Some(0), None), drain_now), Some((Some(0), None)));
        let mut saw_eof = false;
        let deadline3 = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline3 {
            let (eof, exhausted) = drain_conpty_quantum(&bridge, PTY_READ_QUANTUM_BYTES, |_| {});
            if eof {
                assert!(!exhausted);
                saw_eof = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_eof, "begin_win_exit_drain must close ConPTY master so drain_conpty_quantum observes EOF");
        assert_eq!(drain_conpty_quantum(&bridge, PTY_READ_QUANTUM_BYTES, |_| {}), (false, false));

        // Clean up remaining test session.
        runner.force_drop_session(paused_id);
    }
}

#[cfg(test)]
mod image_source_shard_tests {
    use apack_core::marks::MarksMode;
    use apack_core::vt::VtEngine;
    use apack_os::image_sources::{Quiet as ImageQuiet, encoded};
    #[cfg(unix)]
    use apack_os::pty::{PtySpawnConfig, spawn_pty};
    use apack_proto::types::Confirmed;
    use apack_proto::types::{CellPos, CellPx, CellSpan, GridSize, ResizePolicy};
    use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
    use apack_wal::{RecordingRegistry, WalConfig, WalReaper};

    use super::super::images::{ImageRefusal, PendingImageLoad};
    use super::super::*;

    fn make_test_runner() -> (ShardRunner, mpsc::Receiver<ReactorEvent>) {
        let mb = Arc::new(Mutex::new(ShardMailboxState::default()));
        #[cfg(unix)]
        let (poller, self_wake) = {
            let p = PtyPoller::new().expect("poller");
            let w = ShardWake::Poller(p.waker());
            (p, w)
        };
        #[cfg(windows)]
        let (cond, self_wake) = {
            let cv = Arc::new(Condvar::new());
            let w = ShardWake::Cond(Arc::clone(&mb), Arc::clone(&cv));
            (cv, w)
        };
        let (rtx, rrx) = mpsc::channel();
        let runner = ShardRunner {
            shard_idx: 0,
            shared: ShardShared {
                image_store: Arc::new(Mutex::new(ImageStore::default())),
                wal_registry: RecordingRegistry::new(),
                wal_reaper: WalReaper::new(),
                wal_config: WalConfig::default(),
                idle_compress_after: Duration::from_secs(60),
                kill_escalate_after: Duration::from_secs(1),
                engine_abi_hash: [0u8; 32],
            },
            reactor_tx: rtx,
            wake_reactor: Arc::new(|| {}),
            mailbox: mb,
            load: Arc::new(AtomicUsize::new(0)),
            telemetry: Arc::new(ShardTelemetryAtomics::default()),
            self_wake,
            fg_tx: None,
            fg_worker: None,
            fg_inflight: HashSet::new(),
            fg_pending: HashMap::new(),
            raw_output_inflight: Arc::new(AtomicUsize::new(0)),
            sessions: HashMap::new(),
            #[cfg(unix)]
            poller,
            #[cfg(unix)]
            token_to_session: HashMap::new(),
            #[cfg(windows)]
            cond,
            #[cfg(unix)]
            read_buf: vec![0u8; PTY_READ_BUF_BYTES],
            #[cfg(unix)]
            write_scratch: Vec::new(),
            total_pty_bytes: 0,
            running: true,
            tearing_down_all: false,
        };
        (runner, rrx)
    }

    fn insert_image_session(runner: &mut ShardRunner, sid: SessionId, image_files: bool) {
        let size = GridSize::new(80, 24).unwrap();
        let cell_px = CellPx { width: 8, height: 16 };
        let mut vt_cfg = GhosttyConfig::new(size);
        vt_cfg.cell_px = cell_px;
        vt_cfg.scrollback_lines = 100;
        vt_cfg.image_store = Some(Arc::clone(&runner.shared.image_store));
        let engine = GhosttyEngine::new(vt_cfg).expect("ghostty");
        let machine = SessionMachine::new(sid, engine, ResizePolicy::LatestActive, MarksMode::Claimed, cell_px);
        let (image_scanner, image_worker) = if image_files {
            (Some(ImageScanner::default()), Some(Arc::new(ImageWorker::default())))
        } else {
            (None, None)
        };
        runner.sessions.insert(
            sid,
            LiveSession {
                machine,
                pty: None,
                child: None,
                killer: None,
                tree: None,
                pid: None,
                kept: false,
                rewound: false,
                created_ms: 0,
                wal: None,
                wal_has_segment: false,
                wal_skip_prefix: 0,
                pending_wal_rollover: false,
                wal_images_gen: 0,
                wal_written_blobs: HashSet::new(),
                rewind_reader: None,
                rewind_at_ms: 0,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: false,
                paused_by_raw_flow: false,
                paused_by_raw_inflight: false,
                raw_output_inflight: Arc::new(AtomicUsize::new(0)),
                terminating_since: None,
                escalated_sigkill: false,
                view_slot_free: HashSet::new(),
                raw_output_subs: HashSet::new(),
                image_scanner,
                image_worker,
                image_ticket: 0,
                pending_image: None,
                pending_image_tail: Vec::new(),
                reaped_exit: None,
                exit_drain: None,
            },
        );
    }

    fn wait_and_drain_image(runner: &mut ShardRunner, sid: SessionId) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            runner.drain_mailbox();
            if runner.sessions.get(&sid).is_none_or(|s| s.pending_image.is_none()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for off-shard image load");
    }

    fn drain_pty_reply_text(runner: &mut ShardRunner, sid: SessionId) -> String {
        let sess = runner.sessions.get_mut(&sid).expect("session");
        let mut out = Vec::new();
        while sess.machine.pty_write_len() != 0 {
            let mut chunk = Vec::new();
            sess.machine.peek_pty_write(4096, &mut chunk);
            let n = chunk.len();
            out.extend_from_slice(&chunk);
            sess.machine.consume_pty_write(n);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn shard_image_sources_cover_file_query_refusals_ordering_and_cancel() {
        // 1. `should_forward_probe_reply` truth table.
        assert!(should_forward_probe_reply("\x1b_Gi=1;OK\x1b\\", ImageQuiet::No));
        assert!(should_forward_probe_reply("\x1b_Gi=1;EINVAL: bad\x1b\\", ImageQuiet::No));
        assert!(!should_forward_probe_reply("\x1b_Gi=1;OK\x1b\\", ImageQuiet::Ok));
        assert!(should_forward_probe_reply("\x1b_Gi=1;EINVAL: bad\x1b\\", ImageQuiet::Ok));
        assert!(!should_forward_probe_reply("\x1b_Gi=1;OK\x1b\\", ImageQuiet::Failures));
        assert!(!should_forward_probe_reply("\x1b_Gi=1;EINVAL: bad\x1b\\", ImageQuiet::Failures));

        let dir = tempfile::tempdir().expect("tempdir");
        let img_path = dir.path().join("px.rgba");
        let pixels: Vec<u8> =
            vec![0xaa, 0x11, 0x22, 0xff, 0x33, 0x44, 0x55, 0xff, 0x66, 0x77, 0x88, 0xff, 0x99, 0xbb, 0xcc, 0xff];
        std::fs::write(&img_path, &pixels).expect("write rgba");
        let b64_ok = String::from_utf8(encoded(img_path.to_string_lossy().as_bytes())).unwrap();
        let b64_missing =
            String::from_utf8(encoded(dir.path().join("missing.rgba").to_string_lossy().as_bytes())).unwrap();

        let (mut runner, rrx) = make_test_runner();
        let sid = SessionId::new(1).unwrap();
        insert_image_session(&mut runner, sid, true);

        // 2. Askable `t=f` load + inline trailing bytes + queued follow-up chunk while `pending_image` is active.
        let apc1 = format!("\x1b_Ga=T,f=32,s=2,v=2,i=7,t=f;{b64_ok}\x1b\\TAIL_ONE ");
        runner.ingest_pty_bytes(sid, apc1.as_bytes());
        assert!(runner.sessions[&sid].is_pty_paused(), "pending_image must pause PTY reads");
        runner.ingest_pty_bytes(sid, b"TAIL_TWO");
        wait_and_drain_image(&mut runner, sid);
        assert!(!runner.sessions[&sid].is_pty_paused());
        assert_eq!(runner.total_pty_bytes as usize, apc1.len() + b"TAIL_TWO".len());
        while let Ok(ev) = rrx.try_recv() {
            assert!(
                !matches!(ev, ReactorEvent::PublishRawOutput { .. }),
                "must not publish raw output when raw_output_subs is empty"
            );
        }

        let images = runner.sessions[&sid].machine.vt().images();
        assert_eq!(images.len(), 1, "expected 1 placed image, got {images:?}");
        assert_eq!(images[0].pixel_size, [2, 2]);
        let blob_chunk = runner.shared.image_store.lock().unwrap().get_chunk(images[0].blob, 0, 1024).expect("blob");
        assert_eq!(blob_chunk.encoding, apack_proto::types::ImageEncoding::Png);
        assert!(blob_chunk.data.starts_with(b"\x89PNG\r\n\x1a\n"), "expected PNG blob");
        assert!(blob_chunk.data.windows(8).any(|w| w == &pixels[..8]));
        assert!(blob_chunk.data.windows(8).any(|w| w == &pixels[8..]));

        let full_span = CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 23, col: 79 } };
        let screen_text = runner.sessions[&sid].machine.vt().text_of(full_span, 4096);
        assert!(screen_text.contains("TAIL_ONE TAIL_TWO"), "inline ordering broken: {screen_text:?}");
        let reply1 = drain_pty_reply_text(&mut runner, sid);
        assert!(reply1.contains("i=7;OK"), "expected OK reply for i=7, got {reply1:?}");

        // 3. `a=q` query (`query_bytes().is_some()`) succeeds with OK reply and adds no placement.
        let apc_q = format!("\x1b_Ga=q,f=32,s=2,v=2,i=8,t=f;{b64_ok}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_q.as_bytes());
        wait_and_drain_image(&mut runner, sid);
        let reply_q = drain_pty_reply_text(&mut runner, sid);
        assert!(reply_q.contains("i=8;OK"), "expected query OK reply, got {reply_q:?}");
        assert_eq!(runner.sessions[&sid].machine.vt().images().len(), 1);

        // 4. Unaskable `read_unasked()` (`!askable()`, `a=T`) succeeds AND failing unaskable hits `ByCore`.
        let apc_unasked_ok = format!("\x1b_Ga=T,f=32,s=2,v=2,t=f;{b64_ok}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_unasked_ok.as_bytes());
        wait_and_drain_image(&mut runner, sid);
        assert_eq!(runner.sessions[&sid].machine.vt().images().len(), 2);
        let apc_unasked_fail = format!("\x1b_Ga=T,f=32,s=2,v=2,t=f;{b64_missing}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_unasked_fail.as_bytes());
        wait_and_drain_image(&mut runner, sid);
        assert_eq!(runner.sessions[&sid].machine.vt().images().len(), 2);

        // 5. Refused askable loads: `q=0` emits `EINVAL: invalid data`, `q=2` suppresses reply.
        let apc_fail_loud = format!("\x1b_Ga=T,f=32,s=2,v=2,i=9,q=0,t=f;{b64_missing}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_fail_loud.as_bytes());
        wait_and_drain_image(&mut runner, sid);
        let reply_fail_loud = drain_pty_reply_text(&mut runner, sid);
        assert!(reply_fail_loud.contains("i=9;EINVAL: invalid data"), "got {reply_fail_loud:?}");

        let apc_fail_quiet = format!("\x1b_Ga=T,f=32,s=2,v=2,i=10,q=2,t=f;{b64_missing}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_fail_quiet.as_bytes());
        wait_and_drain_image(&mut runner, sid);
        assert!(drain_pty_reply_text(&mut runner, sid).is_empty(), "q=2 must suppress refusal reply");

        // 6. Invalid-key askable probe (`f=99`) is answered directly without off-shard read, and
        //    `!askable && !read_unasked` (`a=q` without `i`/`I`) feeds refusal cleanly without stalling.
        let apc_bad_fmt = format!("\x1b_Ga=T,f=99,s=2,v=2,i=11,q=0,t=f;{b64_ok}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_bad_fmt.as_bytes());
        assert!(runner.sessions[&sid].pending_image.is_none());
        let reply_bad_fmt = drain_pty_reply_text(&mut runner, sid);
        assert!(reply_bad_fmt.contains("i=11;"), "expected core error reply, got {reply_bad_fmt:?}");

        let apc_unask_q = format!("\x1b_Ga=q,f=32,s=2,v=2,t=f;{b64_ok}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_unask_q.as_bytes());
        assert!(runner.sessions[&sid].pending_image.is_none());

        // 7. Non-named inline `t=d` passthrough still works when `image_files == true`.
        let inline_b64 = String::from_utf8(encoded(&pixels)).unwrap();
        let apc_inline = format!("\x1b_Ga=T,f=32,s=2,v=2,i=12,t=d;{inline_b64}\x1b\\");
        runner.ingest_pty_bytes(sid, apc_inline.as_bytes());
        assert!(runner.sessions[&sid].pending_image.is_none());
        assert_eq!(runner.sessions[&sid].machine.vt().images().len(), 3);
        let _ = drain_pty_reply_text(&mut runner, sid);

        // 8. Platform security refusals: Windows NTFS junction / UNC / pipe; Unix `/dev/null` symlink.
        #[cfg(windows)]
        {
            let target_dir = dir.path().join("real-target");
            std::fs::create_dir(&target_dir).expect("mkdir target");
            std::fs::write(target_dir.join("secret.rgba"), &pixels).expect("write target");
            let junction_dir = dir.path().join("junc");
            let st = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&junction_dir)
                .arg(&target_dir)
                .status()
                .expect("mklink");
            assert!(st.success(), "mklink /J failed");
            for (idx, bad) in [
                junction_dir.join("secret.rgba").to_string_lossy().into_owned(),
                String::from(r"\\127.0.0.1\share\x.rgba"),
                String::from(r"\\.\pipe\apack-refused-pipe"),
            ]
            .into_iter()
            .enumerate()
            {
                let b64 = String::from_utf8(encoded(bad.as_bytes())).unwrap();
                let id = 40 + idx;
                runner.ingest_pty_bytes(sid, format!("\x1b_Ga=T,f=32,s=2,v=2,i={id},t=f;{b64}\x1b\\").as_bytes());
                wait_and_drain_image(&mut runner, sid);
                let reply = drain_pty_reply_text(&mut runner, sid);
                assert!(reply.contains("EINVAL: invalid data"), "expected refusal for {bad:?}, got {reply:?}");
            }
            assert_eq!(runner.sessions[&sid].machine.vt().images().len(), 3);
        }

        // 9. Stale ticket rejection + `None` (timeout/circuit-breaker) refusal + `initiate_session_termination` cancel.
        {
            let sess = runner.sessions.get_mut(&sid).unwrap();
            sess.image_ticket = 500;
            sess.pending_image = Some(PendingImageLoad {
                ticket: 500,
                refusal: ImageRefusal::Said {
                    reply: String::from("\x1b_Gi=77;EINVAL: invalid data\x1b\\"),
                    quiet: ImageQuiet::No,
                },
            });
            sess.pending_image_tail = b"\x1b\\AFTER_TIMEOUT".to_vec();
        }
        // Stale ticket 499 must be ignored completely.
        runner.handle_image_source_loaded(sid, 499, None);
        assert!(runner.sessions[&sid].pending_image.is_some());
        // Matching ticket 500 with `None` emits refusal reply and drains tail cleanly.
        runner.handle_image_source_loaded(sid, 500, None);
        assert!(runner.sessions[&sid].pending_image.is_none());
        let timeout_reply = drain_pty_reply_text(&mut runner, sid);
        assert!(timeout_reply.contains("i=77;EINVAL: invalid data"), "got {timeout_reply:?}");
        assert!(runner.sessions[&sid].machine.vt().text_of(full_span, 4096).contains("AFTER_TIMEOUT"));

        // Explicit termination clears pending_image, image_scanner, and tail immediately AND aborts
        // any open APC in `libghostty-vt` via CAN (0x18) so subsequent teardown text still renders!
        runner.ingest_pty_bytes(sid, b"\x1b_Ga=T,f=32,s=2,v=2,t=f;L2E=");
        assert!(runner.sessions[&sid].image_scanner.as_ref().is_some_and(ImageScanner::reading));
        runner.initiate_session_termination(sid);
        {
            let sess = &runner.sessions[&sid];
            assert!(sess.pending_image.is_none());
            assert!(sess.image_scanner.is_none());
            assert!(sess.pending_image_tail.is_empty());
        }
        runner.ingest_pty_bytes(sid, b"TEARDOWN_AFTER_SCAN");
        assert!(runner.sessions[&sid].machine.vt().text_of(full_span, 4096).contains("TEARDOWN_AFTER_SCAN"));

        // Also verify CAN abort when `pending_image.is_some()` (with scanner NOT mid-read).
        let sid2 = SessionId::new(2).unwrap();
        insert_image_session(&mut runner, sid2, true);
        {
            let sess = runner.sessions.get_mut(&sid2).unwrap();
            let mut evs = Vec::new();
            sess.machine.vt_mut().feed(b"\x1b_", &mut evs);
            sess.image_ticket = 600;
            sess.pending_image = Some(PendingImageLoad {
                ticket: 600,
                refusal: ImageRefusal::Said {
                    reply: String::from("\x1b_Gi=88;EINVAL: invalid data\x1b\\"),
                    quiet: ImageQuiet::No,
                },
            });
            sess.pending_image_tail = b"DROPPED".to_vec();
        }
        runner.initiate_session_termination(sid2);
        {
            let sess = &runner.sessions[&sid2];
            assert!(sess.pending_image.is_none());
            assert!(sess.image_scanner.is_none());
            assert!(sess.pending_image_tail.is_empty());
            assert_ne!(sess.image_ticket, 600);
        }
        runner.ingest_pty_bytes(sid2, b"TEARDOWN_AFTER_PENDING");
        assert!(runner.sessions[&sid2].machine.vt().text_of(full_span, 4096).contains("TEARDOWN_AFTER_PENDING"));

        // Direct check of `drain_tail_pty_bytes` on a real Unix PTY master (no poller reads first).
        #[cfg(unix)]
        {
            let sid3 = SessionId::new(3).unwrap();
            insert_image_session(&mut runner, sid3, false);
            let pty_cfg = PtySpawnConfig {
                size: GridSize::new(80, 24).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
                shell: Some(String::from("/bin/sh")),
                args: vec![String::from("-c"), String::from("printf 'DRAIN_TAIL_UNIT_OK'")],
                env: Vec::new(),
                unset: Vec::new(),
                cwd: None,
            };
            let mut spawned = spawn_pty(&pty_cfg).expect("spawn pty");
            runner.sessions.get_mut(&sid3).unwrap().pty = Some(LivePty::Unix {
                master: spawned.master,
                token: PollToken(sid3.get()),
                eof: false,
                write_broken: false,
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                runner.drain_tail_pty_bytes(sid3);
                if runner.sessions[&sid3].machine.vt().text_of(full_span, 4096).contains("DRAIN_TAIL_UNIT_OK") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(runner.sessions[&sid3].machine.vt().text_of(full_span, 4096).contains("DRAIN_TAIL_UNIT_OK"));
            runner.sessions.get_mut(&sid3).unwrap().pty = None;
            let _ = spawned.child.wait();
        }
    }

    #[test]
    fn shard_fg_inflight_gating_attachment_isolation_and_raw_inflight_backpressure() {
        let (mut runner, rrx) = make_test_runner();
        let sid = SessionId::new(10).unwrap();
        insert_image_session(&mut runner, sid, false);

        // 1. Batched per-session fg_inflight gating + multi-slot preservation (`B1`) + Full backpressure retry (`M2`):
        let (fg_tx, fg_rx) = mpsc::sync_channel::<FgVerifyReq>(1);
        runner.fg_tx = Some(fg_tx);
        // Dispatch slot-a immediately -> enters fg_tx (capacity 1, now FULL!).
        runner.fg_pending.entry(sid).or_default().insert(String::from("slot-a"), (1, String::from("app-a")));
        runner.flush_pending_fg_verify(sid);
        assert!(runner.fg_inflight.contains(&sid));

        // While sid is in flight, enqueue root "" + "slot-b" (updated twice) + "slot-c":
        // NONE of the distinct slots may overwrite each other!
        runner.fg_pending.entry(sid).or_default().insert(String::new(), (2, String::from("claude")));
        runner.fg_pending.entry(sid).or_default().insert(String::from("slot-b"), (3, String::from("old")));
        runner.fg_pending.entry(sid).or_default().insert(String::from("slot-b"), (4, String::from("new")));
        runner.fg_pending.entry(sid).or_default().insert(String::from("slot-c"), (5, String::from("cargo")));
        runner.flush_pending_fg_verify(sid);

        // Second session while fg_tx is FULL must stay in fg_pending (NEVER degrade to Confirmed::Unknown)!
        let sid2 = SessionId::new(11).unwrap();
        insert_image_session(&mut runner, sid2, false);
        runner.fg_pending.entry(sid2).or_default().insert(String::new(), (1, String::from("codex")));
        runner.flush_pending_fg_verify(sid2);
        assert!(
            runner.fg_pending.contains_key(&sid2),
            "Full fg_tx must retain pending items instead of faking Unknown"
        );
        assert!(runner.mailbox.lock().unwrap().queue.is_empty(), "Full fg_tx must NOT queue fallback Unknown");

        // Drain slot-a from fg_rx:
        let first = fg_rx.try_recv().expect("first fg verify queued");
        assert_eq!(first.session_id, sid);
        assert_eq!(first.items, vec![(String::from("slot-a"), 1, String::from("app-a"))]);

        // Local fallback completion (from_worker: false) must NOT clear fg_inflight!
        runner.handle_cmd(ShardCmd::ForegroundVerified { session_id: sid, results: vec![], from_worker: false }, None);
        assert!(runner.fg_inflight.contains(&sid));

        // Worker completion (from_worker: true) frees sid's slot AND flushes BOTH sid's batched items AND sid2!
        let (fg_tx2, fg_rx2) = mpsc::sync_channel::<FgVerifyReq>(4);
        runner.fg_tx = Some(fg_tx2);
        runner.handle_cmd(
            ShardCmd::ForegroundVerified {
                session_id: sid,
                results: vec![(String::from("slot-a"), 1, Confirmed::Unknown)],
                from_worker: true,
            },
            None,
        );
        let mut dispatched = Vec::new();
        while let Ok(req) = fg_rx2.try_recv() {
            dispatched.push(req);
        }
        let sid_batch = dispatched.iter().find(|r| r.session_id == sid).expect("sid batch flushed");
        assert_eq!(
            sid_batch.items,
            vec![
                (String::new(), 2, String::from("claude")),
                (String::from("slot-b"), 4, String::from("new")),
                (String::from("slot-c"), 5, String::from("cargo")),
            ],
            "all distinct status slots (including root) must be verified in one batch without overwriting"
        );
        let sid2_batch = dispatched.iter().find(|r| r.session_id == sid2).expect("backlogged sid2 flushed");
        assert_eq!(sid2_batch.items, vec![(String::new(), 1, String::from("codex"))]);

        // Complete both batches cleanly.
        runner.handle_cmd(ShardCmd::ForegroundVerified { session_id: sid, results: vec![], from_worker: true }, None);
        runner.handle_cmd(ShardCmd::ForegroundVerified { session_id: sid2, results: vec![], from_worker: true }, None);
        assert!(!runner.fg_inflight.contains(&sid));
        assert!(!runner.fg_inflight.contains(&sid2));

        // Disconnected / absent fg_tx resolves via mailbox fallback (from_worker: false) so waits never deadlock!
        drop(fg_rx2);
        runner.fg_pending.entry(sid).or_default().insert(String::new(), (9, String::from("sh")));
        runner.flush_pending_fg_verify(sid);
        let fb = runner.mailbox.lock().unwrap().queue.pop_front().expect("disconnected fg_tx queues fallback").0;
        assert!(matches!(
            fb,
            ShardCmd::ForegroundVerified { session_id, ref results, from_worker: false }
            if session_id == sid && results == &vec![(String::new(), 9, Confirmed::Unknown)]
        ));
        runner.fg_tx = None;
        runner.sessions.remove(&sid2);

        // 2. Stale StreamAttachmentId on Detach / ViewSlotFree / StreamMsg cannot affect newer attachment (`B2`):
        let conn = ConnId::new(1).unwrap();
        let st = apack_proto::types::NonZeroStreamId::new(1).unwrap();
        let a1 = StreamAttachmentId::new(1).unwrap();
        let a2 = StreamAttachmentId::new(2).unwrap();
        runner.handle_cmd(
            ShardCmd::Attach {
                session_id: sid,
                conn,
                req_id: RequestId::from_raw(1),
                stream_id: st,
                attachment_id: a1,
                flags: apack_proto::types::SubFlags::from_bits(
                    apack_proto::types::SubFlags::VIEW | apack_proto::types::SubFlags::RAW_OUTPUT,
                )
                .unwrap(),
                initial_viewport: None,
            },
            None,
        );
        // Re-attach same (conn, st) with a2 (simulating Detach + Re-Attach on same session)
        runner.handle_cmd(
            ShardCmd::Attach {
                session_id: sid,
                conn,
                req_id: RequestId::from_raw(2),
                stream_id: st,
                attachment_id: a2,
                flags: apack_proto::types::SubFlags::from_bits(
                    apack_proto::types::SubFlags::VIEW | apack_proto::types::SubFlags::RAW_OUTPUT,
                )
                .unwrap(),
                initial_viewport: None,
            },
            None,
        );
        // Consume a2's initial view slot, then send stale ViewSlotFree(a1) and stale Detach(a1)
        runner.sessions.get_mut(&sid).unwrap().view_slot_free.remove(&a2);
        runner.handle_cmd(
            ShardCmd::ViewSlotFree { session_id: sid, conn, stream_id: st, attachment_id: a1, force_full_sync: false },
            None,
        );
        assert!(!runner.sessions[&sid].view_slot_free.contains(&a1));
        assert!(!runner.sessions[&sid].view_slot_free.contains(&a2));
        runner.handle_cmd(ShardCmd::Detach { session_id: sid, conn, stream_id: st, attachment_id: a1 }, None);
        assert_eq!(runner.sessions[&sid].machine.attachment_of((conn, st)), Some(a2));

        // Matching ViewSlotFree(a2) re-arms a2!
        runner.handle_cmd(
            ShardCmd::ViewSlotFree { session_id: sid, conn, stream_id: st, attachment_id: a2, force_full_sync: false },
            None,
        );
        assert!(runner.sessions[&sid].view_slot_free.contains(&a2));

        // 3. PublishRawOutput in-flight backpressure pauses PTY at high-water and resumes via RawOutputDrained / Tier 2 (`H6`):
        while rrx.try_recv().is_ok() {}
        // Small chunk below high-water must NOT pause PTY!
        runner.ingest_pty_bytes(sid, b"small");
        assert!(!runner.sessions[&sid].paused_by_raw_inflight, "small chunk must not pause PTY");
        while let Ok(ev) = rrx.try_recv() {
            if let ReactorEvent::PublishRawOutput { bytes, session_inflight, shard_inflight, .. } = ev {
                session_inflight.fetch_sub(bytes.len(), Ordering::AcqRel);
                shard_inflight.fetch_sub(bytes.len(), Ordering::AcqRel);
            }
        }
        // High-water chunk during termination must NOT pause PTY (prevents ttydrain deadlock)!
        runner.sessions.get_mut(&sid).unwrap().terminating_since = Some(Instant::now());
        let big_chunk = vec![b'X'; protocol::RAW_OUTPUT_SESSION_HIGH_WATER];
        runner.ingest_pty_bytes(sid, &big_chunk);
        assert!(!runner.sessions[&sid].paused_by_raw_inflight, "terminating session must not pause PTY");
        while let Ok(ev) = rrx.try_recv() {
            if let ReactorEvent::PublishRawOutput { bytes, session_inflight, shard_inflight, .. } = ev {
                session_inflight.fetch_sub(bytes.len(), Ordering::AcqRel);
                shard_inflight.fetch_sub(bytes.len(), Ordering::AcqRel);
            }
        }
        runner.sessions.get_mut(&sid).unwrap().terminating_since = None;

        // Live session hitting high-water pauses PTY, stays paused under RawOutputDrained & Tier 2 while still >= high-water,
        // and resumes once counters drop below cap!
        runner.ingest_pty_bytes(sid, &big_chunk);
        assert!(runner.sessions[&sid].paused_by_raw_inflight, "must pause PTY at raw high-water");
        assert!(runner.sessions[&sid].is_pty_paused());
        runner.handle_cmd(ShardCmd::RawOutputDrained, None);
        assert!(runner.sessions[&sid].paused_by_raw_inflight, "must stay paused while inflight >= high-water");
        runner.run_tier2(TimestampMs::from_millis(5));
        assert!(runner.sessions[&sid].paused_by_raw_inflight, "Tier 2 must not unpause while inflight >= high-water");

        let mut saw_raw = false;
        while let Ok(ev) = rrx.try_recv() {
            if let ReactorEvent::PublishRawOutput { session, bytes, session_inflight, shard_inflight } = ev {
                assert_eq!(session, sid);
                assert_eq!(bytes.len(), big_chunk.len());
                session_inflight.fetch_sub(bytes.len(), Ordering::AcqRel);
                shard_inflight.fetch_sub(bytes.len(), Ordering::AcqRel);
                saw_raw = true;
            }
        }
        assert!(saw_raw);
        runner.handle_cmd(ShardCmd::RawOutputDrained, None);
        assert!(!runner.sessions[&sid].paused_by_raw_inflight, "RawOutputDrained must unpause PTY");

        // Also verify Tier 2 safety net unpauses if counter dropped below cap.
        runner.sessions.get_mut(&sid).unwrap().paused_by_raw_inflight = true;
        runner.run_tier2(TimestampMs::from_millis(10));
        assert!(!runner.sessions[&sid].paused_by_raw_inflight, "Tier 2 must unpause drained raw inflight");
    }

    #[test]
    fn exit_flush_forces_full_sync_when_view_slot_inflight_while_live_respects_slot() {
        use apack_proto::types::{NonZeroStreamId, SubFlags};

        let (mut runner, rrx) = make_test_runner();
        let sid = SessionId::new(100).unwrap();
        let conn = ConnId::new(1).unwrap();
        let stream_id = NonZeroStreamId::new(1).unwrap();
        let att = StreamAttachmentId::new(77).unwrap();
        insert_image_session(&mut runner, sid, false);
        runner.handle_cmd(
            ShardCmd::Attach {
                session_id: sid,
                conn,
                req_id: RequestId::from_raw(1),
                stream_id,
                attachment_id: att,
                flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
                initial_viewport: None,
            },
            None,
        );
        // Drain initial Attached + Frame(seq=1); `view_slot_free` is now EMPTY (simulating in-flight token).
        let mut init_frames = 0usize;
        while let Ok(ev) = rrx.try_recv() {
            if let ReactorEvent::PublishFrame { final_frame, .. } = ev {
                assert!(!final_frame, "initial attach frame must not be marked final_frame");
                init_frames += 1;
            }
        }
        assert_eq!(init_frames, 1);
        assert!(!runner.sessions[&sid].view_slot_free.contains(&att));

        // While live, new PTY output must NOT bypass `view_slot_free` (no duplicate in-flight frame).
        runner.ingest_pty_bytes(sid, b"LIVE_HELD\r\n");
        let live_frames: Vec<_> = std::iter::from_fn(|| rrx.try_recv().ok())
            .filter(|e| matches!(e, ReactorEvent::PublishFrame { .. }))
            .collect();
        assert!(live_frames.is_empty(), "live session must respect view_slot_free");

        // ViewSlotFree with `force_full_sync = true` must force a self-contained full_sync frame.
        runner.handle_cmd(
            ShardCmd::ViewSlotFree { session_id: sid, conn, stream_id, attachment_id: att, force_full_sync: true },
            None,
        );
        let forced_frames: Vec<_> = std::iter::from_fn(|| rrx.try_recv().ok())
            .filter_map(|e| match e {
                ReactorEvent::PublishFrame { frame, final_frame, .. } => Some((frame, final_frame)),
                _ => None,
            })
            .collect();
        assert_eq!(forced_frames.len(), 1);
        assert!(forced_frames[0].0.full_sync, "force_full_sync ViewSlotFree must emit full_sync=true");
        assert!(!forced_frames[0].1, "live forced full_sync must not be marked final_frame");

        // On exit, final dirty view MUST flush strictly BEFORE Exited as a self-contained full_sync=true final_frame!
        runner.ingest_pty_bytes(sid, b"\x1b[?2026hEXIT_FINAL\r\n");
        runner.finish_and_remove_session(sid, Some(0), None);
        let events: Vec<_> = std::iter::from_fn(|| rrx.try_recv().ok()).collect();
        let frame_pos = events.iter().position(|e| {
            matches!(
                e,
                ReactorEvent::PublishFrame { frame, final_frame: true, .. }
                    if frame.full_sync && frame.rows.iter().any(|r| r.text.contains("EXIT_FINAL"))
            )
        });
        let exit_pos =
            events.iter().position(|e| matches!(e, ReactorEvent::SendMsg { msg: BrokerMsg::Exited { .. }, .. }));
        assert!(frame_pos.is_some(), "expected full_sync final frame containing EXIT_FINAL");
        assert!(exit_pos.is_some(), "expected Exited message");
        assert!(frame_pos.unwrap() < exit_pos.unwrap(), "final frame must precede Exited");

        // Also verify exiting while `view_slot_free` IS free still forces `full_sync = true` & `final_frame = true`.
        let sid2 = SessionId::new(101).unwrap();
        let att2 = StreamAttachmentId::new(78).unwrap();
        insert_image_session(&mut runner, sid2, false);
        runner.handle_cmd(
            ShardCmd::Attach {
                session_id: sid2,
                conn,
                req_id: RequestId::from_raw(2),
                stream_id,
                attachment_id: att2,
                flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
                initial_viewport: None,
            },
            None,
        );
        while rrx.try_recv().is_ok() {}
        runner.handle_cmd(
            ShardCmd::ViewSlotFree { session_id: sid2, conn, stream_id, attachment_id: att2, force_full_sync: false },
            None,
        );
        assert!(runner.sessions[&sid2].view_slot_free.contains(&att2));
        runner.ingest_pty_bytes(sid2, b"\x1b[?2026hFREE_SLOT_FINAL\r\n");
        runner.finish_and_remove_session(sid2, Some(0), None);
        let events2: Vec<_> = std::iter::from_fn(|| rrx.try_recv().ok()).collect();
        assert!(
            events2.iter().any(|e| matches!(
                e,
                ReactorEvent::PublishFrame { frame, final_frame: true, .. }
                    if frame.full_sync && frame.rows.iter().any(|r| r.text.contains("FREE_SLOT_FINAL"))
            )),
            "final exit frame must force full_sync=true even when view_slot_free was free"
        );
    }

    #[test]
    fn pr5_review_fixes_visual_fallback_sanitization_virtual_blobs_and_multichunk_tf_wal() {
        use apack_proto::types::{CursorShape, CursorState, PackedRow, PackedRun, ViewModes};
        use apack_wal::{KeyframeV2, SeekSlice, VisualKeyframe, WalConfig, WalReader, WalRecord, WalWorker, WalWriter};

        // 1. `apply_visual_fallback` (`H1`): hostile C0/C1 escapes in PackedRow.text cannot lock up VT
        //    or disable alt-screen; `col_start` indentation preserved; truncated FLAG_WRAPPED does NOT collapse rows!
        let size = GridSize::new(20, 4).unwrap();
        let cell_px = CellPx { width: 8, height: 16 };
        let visual = VisualKeyframe {
            size,
            cell_px,
            active_alt_screen: true,
            cursor: CursorState { col: 3, row: 2, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes { alt_screen: true, ..ViewModes::default() },
            palette: Default::default(),
            primary_rows: vec![
                PackedRow {
                    line: 0,
                    text: String::from("\x1b]HOSTILE\u{009d}P0"),
                    runs: vec![PackedRun {
                        col_start: 2,
                        col_len: 10,
                        byte_offset: 0,
                        byte_len: "\x1b]HOSTILE\u{009d}P0".len() as u32,
                        style_idx: 0,
                        link_idx: 0,
                        flags: 0,
                    }],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: PackedRow::FLAG_WRAPPED, // end_col=12 < cols=20 -> must NOT collapse next row!
                },
                PackedRow {
                    line: 1,
                    text: String::from("01234567890123456789"),
                    runs: vec![PackedRun {
                        col_start: 0,
                        col_len: 20,
                        byte_offset: 0,
                        byte_len: 20,
                        style_idx: 0,
                        link_idx: 0,
                        flags: 0,
                    }],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: PackedRow::FLAG_WRAPPED, // full 20-col wrapped row -> MUST soft-wrap into row 2!
                },
                PackedRow {
                    line: 2,
                    text: String::from("P2_CONT_FULL_20_COLS"),
                    runs: vec![PackedRun {
                        col_start: 0,
                        col_len: 20,
                        byte_offset: 0,
                        byte_len: 20,
                        style_idx: 0,
                        link_idx: 0,
                        flags: 0,
                    }],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: 0, // full 20-col row WITHOUT FLAG_WRAPPED -> must hard-break (NOT soft-wrap)!
                },
                PackedRow {
                    line: 3,
                    text: String::from("P3_HARD"),
                    runs: vec![],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: 0,
                },
            ],
            alt_rows: vec![
                PackedRow {
                    line: 0,
                    text: String::from("\x1b[?1049lALT0"),
                    runs: vec![],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: 0,
                },
                PackedRow {
                    line: 1,
                    text: String::from("ALT1"),
                    runs: vec![],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: 0,
                },
                PackedRow {
                    line: 2,
                    text: String::new(),
                    runs: vec![],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: 0,
                },
                // Bottom alt row (row 3 of 4-row grid) with wide chars + overlong run must NOT wrap/scroll ALT0 off!
                PackedRow {
                    line: 3,
                    text: "中".repeat(20),
                    runs: vec![PackedRun {
                        col_start: 0,
                        col_len: 1,
                        byte_offset: 0,
                        byte_len: ("中".repeat(20)).len() as u32,
                        style_idx: 0,
                        link_idx: 0,
                        flags: 0,
                    }],
                    styles: vec![],
                    links: vec![],
                    pieces: vec![],
                    flags: 0,
                },
            ],
            images: vec![],
            marks_key: None,
            marks_last_seq: 0,
            marks_active_vouched_seq: None,
            marks_refused: 0,
            commands: vec![],
        };
        let slice = SeekSlice {
            segment_id: 1,
            keyframe_at_ms: 0,
            keyframe: KeyframeV2 { visual, engine: None },
            tail_records: vec![],
        };
        let store = Arc::new(Mutex::new(ImageStore::default()));
        let mut eng = crate::shard::replay::build_keyframe_engine(&slice, &[0u8; 32], &store, 100).unwrap();
        assert!(eng.at_ground(), "sanitized visual fallback must end cleanly at ground state");
        assert!(eng.meta().modes.alt_screen, "hostile \\x1b[?1049l in row text must NOT exit alt screen");
        assert_eq!((eng.meta().cursor.row, eng.meta().cursor.col), (2, 3));
        let mut alt_out = Vec::new();
        eng.extract_viewport_rows(&[0, 1], &mut alt_out);
        assert!(alt_out[0].text.contains("ALT0") && alt_out[1].text.contains("ALT1"));
        // Switch back to primary screen to verify col_start indentation + non-collapsed P1_NEXT row!
        let mut ev_pri = Vec::new();
        eng.feed(b"\x1b[?1049l", &mut ev_pri);
        let mut pri_out = Vec::new();
        eng.extract_viewport_rows(&[0, 1, 2, 3], &mut pri_out);
        assert_eq!(
            eng.text_of(
                apack_proto::types::CellSpan {
                    start: apack_proto::types::CellPos { line: 0, col: 2 },
                    end: apack_proto::types::CellPos { line: 0, col: 11 },
                },
                64
            ),
            "]HOSTILEP0",
            "col_start indentation must place text at exact column 2"
        );
        assert_eq!(pri_out[0].flags & PackedRow::FLAG_WRAPPED, 0, "truncated FLAG_WRAPPED must NOT soft-wrap row 0");
        assert!(pri_out[1].text.contains("01234567890123456789"));
        assert_ne!(pri_out[1].flags & PackedRow::FLAG_WRAPPED, 0, "full-width FLAG_WRAPPED row must soft-wrap");
        assert!(pri_out[2].text.contains("P2_CONT"));
        assert_eq!(pri_out[2].flags & PackedRow::FLAG_WRAPPED, 0);
        assert!(pri_out[3].text.contains("P3_HARD"));

        // Direct unit check on `split_st_ground_chunk` (both leading ST and mid-chunk ST + no-ST).
        assert_eq!(
            crate::shard::images::split_st_ground_chunk(b"\x1b\\\x1b_Gm=0;AA=="),
            Some((&b"\x1b\\"[..], &b"\x1b_Gm=0;AA=="[..]))
        );
        assert_eq!(
            crate::shard::images::split_st_ground_chunk(b"Ga=t\x1b\\\x1b_Grest"),
            Some((&b"Ga=t\x1b\\"[..], &b"\x1b_Grest"[..]))
        );
        assert_eq!(crate::shard::images::split_st_ground_chunk(b"no-terminator"), None);

        // Adversarial `reconstruct_visual_row` bounds: unclamped `col_start = u16::MAX`, overlapping/OOB runs,
        // multiplicative run-slice amplification, control-char column suppression, and runs-empty `cols` + byte budget!
        let hostile_amplified = PackedRow {
            line: 0,
            text: "A".repeat(32),
            runs: vec![
                // Valid run at col 1 (len 4) -> " AAAA" (5 bytes, cur_col = 5)
                PackedRun {
                    col_start: 1,
                    col_len: 4,
                    byte_offset: 0,
                    byte_len: 4,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                // Overlapping/out-of-order run (col_start 3 < cur_col 5) -> MUST be skipped!
                PackedRun {
                    col_start: 3,
                    col_len: 4,
                    byte_offset: 0,
                    byte_len: 4,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                // Unclamped col_start = u16::MAX (>= cols=10) -> MUST be skipped!
                PackedRun {
                    col_start: u16::MAX,
                    col_len: 4,
                    byte_offset: 0,
                    byte_len: 4,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                // Out-of-bounds byte slice -> MUST be skipped!
                PackedRun {
                    col_start: 6,
                    col_len: 2,
                    byte_offset: 99,
                    byte_len: 4,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                // Valid run hitting exact `max_bytes = 8` budget during padding/text -> stops cleanly at <= 8 bytes!
                PackedRun {
                    col_start: 6,
                    col_len: 4,
                    byte_offset: 0,
                    byte_len: 4,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
            ],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        let (out_cap, end_col_cap) = crate::shard::replay::reconstruct_visual_row(&hostile_amplified, 10, 8);
        assert_eq!(out_cap, " AAAA AA");
        assert!(out_cap.len() <= 8);
        assert_eq!(end_col_cap, 6);
        // Padding itself exceeding `max_bytes` breaks cleanly without exceeding budget:
        let pad_overflow = PackedRow {
            line: 0,
            text: String::from("Z"),
            runs: vec![PackedRun {
                col_start: 9,
                col_len: 1,
                byte_offset: 0,
                byte_len: 1,
                style_idx: 0,
                link_idx: 0,
                flags: 0,
            }],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&pad_overflow, 10, 4), (String::new(), 0));
        // Exact padding fit (`pad == max_bytes`) succeeds (kills `>` -> `>=`):
        let pad_exact = PackedRow {
            line: 0,
            text: String::new(),
            runs: vec![PackedRun {
                col_start: 4,
                col_len: 0,
                byte_offset: 0,
                byte_len: 0,
                style_idx: 0,
                link_idx: 0,
                flags: 0,
            }],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&pad_exact, 10, 4), (String::from("    "), 4));
        // Control chars stripped inside a run still advance `cur_col` (rejecting overlapping runs & aligning
        // subsequent padding) while returning `end_col == 0` so wrap suppression never misfires, AND total
        // scanned control bytes count toward `max_bytes` (defeating control-only CPU amplification DoS)!
        let ctrl_multi_row = PackedRow {
            line: 0,
            text: String::from("ab\x07cdXYZ"),
            runs: vec![
                // Run 0 at col 0, len 5 -> emits "abcd", advances cur_col to 5!
                PackedRun {
                    col_start: 0,
                    col_len: 5,
                    byte_offset: 0,
                    byte_len: 5,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                // Overlapping run at col 0 (< cur_col 5) -> MUST be skipped!
                PackedRun {
                    col_start: 0,
                    col_len: 5,
                    byte_offset: 0,
                    byte_len: 5,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                // Run 2 at col 5 (== cur_col 5) -> NO spurious 5-space padding, emits "XYZ"!
                PackedRun {
                    col_start: 5,
                    col_len: 3,
                    byte_offset: 5,
                    byte_len: 3,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
            ],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&ctrl_multi_row, 10, 64), (String::from("abcdXYZ"), 0));
        // Control-only slices count against `max_bytes`, stopping before trailing run past budget:
        let ctrl_budget_row = PackedRow {
            line: 0,
            text: String::from("\x00\x00\x00\x00\x00OK"),
            runs: vec![
                PackedRun {
                    col_start: 0,
                    col_len: 1,
                    byte_offset: 0,
                    byte_len: 5,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
                PackedRun {
                    col_start: 1,
                    col_len: 2,
                    byte_offset: 5,
                    byte_len: 2,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                },
            ],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&ctrl_budget_row, 10, 4), (String::new(), 0));
        // Runs-empty row truncated strictly to `cols` AND scanned `max_bytes` (including control bytes!):
        let long_plain = PackedRow {
            line: 0,
            text: "é".repeat(20), // 2 bytes per char
            runs: vec![],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&long_plain, 5, 64).0.chars().count(), 5);
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&long_plain, 20, 4).0.len(), 4);
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&long_plain, 20, 5).0.len(), 4);
        let plain_ctrl_budget = PackedRow {
            line: 0,
            text: String::from("\x00\x00\x00\x00\x00OK"),
            runs: vec![],
            styles: vec![],
            links: vec![],
            pieces: vec![],
            flags: 0,
        };
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&plain_ctrl_budget, 20, 4).0, "");
        assert_eq!(crate::shard::replay::reconstruct_visual_row(&plain_ctrl_budget, 20, 7).0, "OK");

        // 2. Virtual `U=1` Kitty image persisted via `active_image_blobs` even after `ImageStore` eviction (`H2` + `H3`),
        //    WAL retry leaves `wal_images_gen` unlatched when `write_blob` fails, and `hydrate_slice_blobs` loads it!
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = dir.path().join("virt-wal");
        let (mut runner, _rrx) = make_test_runner();
        let sid = SessionId::new(42).unwrap();
        insert_image_session(&mut runner, sid, true);
        {
            let sess = runner.sessions.get_mut(&sid).unwrap();
            let writer = WalWriter::open_or_create(
                &wal_dir,
                [0x42; 16],
                1_000,
                WalConfig { max_non_ground_bytes: 8_192, ..WalConfig::default() },
                runner.shared.wal_registry.clone(),
            )
            .unwrap();
            let kf = crate::shard::replay::capture_keyframe(&sess.machine, &runner.shared.engine_abi_hash);
            let wal = WalWorker::spawn(writer);
            assert!(wal.start_segment(1_000, kf));
            sess.wal = Some(wal);
            sess.wal_has_segment = true;
        }
        let virt_png = apack_vt_ghostty::encode_png_rgba(2, 2, &[77u8; 16]).unwrap();
        let virt_b64 = String::from_utf8(apack_os::image_sources::encoded(&virt_png)).unwrap();
        let virt_seq = format!(
            "\x1b_Ga=T,U=1,f=100,i=21,p=1,c=1,r=1,q=2;{virt_b64}\x1b\\\x1b[38;2;0;0;21;58;2;0;0;1m\u{10EEEE}\x1b[0m"
        );
        {
            let sess = runner.sessions.get_mut(&sid).unwrap();
            let mut evs = Vec::new();
            sess.machine.vt_mut().feed(virt_seq.as_bytes(), &mut evs);
            let blobs = sess.machine.vt().active_image_blobs();
            assert_eq!(blobs.len(), 1, "active_image_blobs must include virtual U=1 images");
            *runner.shared.image_store.lock().unwrap() = ImageStore::default();
            assert!(runner.shared.image_store.lock().unwrap().get_blob(blobs[0]).is_none());
            crate::shard::replay::sync_wal_image_blobs(sess, &runner.shared.image_store, &runner.shared.wal_reaper);
            assert!(sess.wal_written_blobs.contains(&blobs[0]));
            assert_eq!(sess.wal_images_gen, sess.machine.vt().generations().images);
            let kf2 = crate::shard::replay::capture_keyframe(&sess.machine, &runner.shared.engine_abi_hash);
            assert!(kf2.referenced_blobs().contains(&blobs[0]), "keyframe referenced_blobs must include virtual piece");
            if let Some(wal) = sess.wal.take() {
                let _ = wal.seal();
                let _ = wal.shutdown();
            }
            let reader = WalReader::open(&wal_dir, &runner.shared.wal_registry).unwrap();
            let fresh_store = Arc::new(Mutex::new(ImageStore::default()));
            let fake_slice = SeekSlice { segment_id: 1, keyframe_at_ms: 1_000, keyframe: kf2, tail_records: vec![] };
            crate::shard::replay::hydrate_slice_blobs(&reader, &fake_slice, &fresh_store);
            assert!(
                fresh_store.lock().unwrap().get_blob(blobs[0]).is_some(),
                "hydrate_slice_blobs must hydrate virtual U=1 blobs from <dir>/blobs/"
            );
        }

        // 3. Multi-chunk `t=f` image exceeding `max_non_ground_bytes` (1.5 MiB) writes without
        //    `NonGroundCap` AND never leaks the host file path into WAL (`H4`)!
        let tf_wal_dir = dir.path().join("tf-wal");
        let secret_file = dir.path().join("SUPER_SECRET_HOST_PATH_PR5.raw");
        // 1.6 MiB raw RGBA (800x512x4) -> 3 chunks (~2.18 MiB base64 > 1.5 MiB max_non_ground_bytes!).
        let big_rgba = vec![0x5Au8; 800 * 512 * 4];
        std::fs::write(&secret_file, &big_rgba).unwrap();
        let sid_tf = SessionId::new(43).unwrap();
        insert_image_session(&mut runner, sid_tf, true);
        {
            let sess = runner.sessions.get_mut(&sid_tf).unwrap();
            let writer = WalWriter::open_or_create(
                &tf_wal_dir,
                [0x43; 16],
                1_000,
                WalConfig { max_non_ground_bytes: 1_500_000, ..WalConfig::default() },
                runner.shared.wal_registry.clone(),
            )
            .unwrap();
            let kf = crate::shard::replay::capture_keyframe(&sess.machine, &runner.shared.engine_abi_hash);
            let wal = WalWorker::spawn(writer);
            assert!(wal.start_segment(1_000, kf));
            sess.wal = Some(wal);
            sess.wal_has_segment = true;
        }
        let secret_b64 =
            String::from_utf8(apack_os::image_sources::encoded(secret_file.as_os_str().as_encoded_bytes())).unwrap();
        let tf_apc = format!("\x1b_Ga=T,f=32,s=800,v=512,i=30,t=f,q=2;{secret_b64}\x1b\\AFTER_TF");
        runner.ingest_pty_bytes(sid_tf, tf_apc.as_bytes());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            runner.drain_mailbox();
            if runner.sessions[&sid_tf].pending_image.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(runner.sessions[&sid_tf].pending_image.is_none());
        assert_eq!(runner.sessions[&sid_tf].machine.vt().images().len(), 1);
        if let Some(wal) = runner.sessions.get_mut(&sid_tf).unwrap().wal.take() {
            let _ = wal.seal();
            let _ = wal.shutdown();
        }
        let tf_reader = WalReader::open(&tf_wal_dir, &runner.shared.wal_registry).unwrap();
        let tf_slice = tf_reader.latest_segment_records().unwrap().unwrap();
        assert!(
            !tf_slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Gap { .. })),
            "multi-chunk t=f exceeding max_non_ground_bytes must NOT trigger NonGroundCap Gap!"
        );
        let all_wal_out: Vec<u8> = tf_slice
            .tail_records
            .iter()
            .filter_map(|r| match &r.record {
                WalRecord::Output(b) => Some(b.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        let all_wal_str = String::from_utf8_lossy(&all_wal_out);
        assert!(
            !all_wal_str.contains(&secret_b64) && !all_wal_str.contains("SUPER_SECRET_HOST_PATH_PR5"),
            "WAL must NEVER leak raw intercepted t=f host paths"
        );

        // 4. `take_ready_exit_drain` (`M10` 2-phase ConPTY drain helper) & `ShardCmd::TeardownSession` (`H12`):
        let base = Instant::now();
        let mut drain =
            Some(super::super::ExitDrainState { code: Some(7), signal: None, deadline: base + Duration::from_secs(2) });
        assert_eq!(super::super::lifecycle::take_ready_exit_drain(&mut drain, false, base), None);
        assert!(drain.is_some());
        assert_eq!(
            super::super::lifecycle::take_ready_exit_drain(&mut drain, false, base + Duration::from_secs(2)),
            Some((Some(7), None))
        );
        assert!(drain.is_none());
        let mut drain_eof =
            Some(super::super::ExitDrainState { code: Some(0), signal: None, deadline: base + Duration::from_secs(2) });
        assert_eq!(super::super::lifecycle::take_ready_exit_drain(&mut drain_eof, true, base), Some((Some(0), None)));

        // 5. `is_pty_paused`, `compute_wait_timeout` & `run_tier2` cover `exit_drain.is_some()`
        //    and `terminating_since.is_some()` when `child` is `None` (post-reap ConPTY drain state).
        let sid_drain = SessionId::new(44).unwrap();
        insert_image_session(&mut runner, sid_drain, false);
        {
            let sess = runner.sessions.get_mut(&sid_drain).unwrap();
            sess.paused_by_stream = true;
            assert!(sess.is_pty_paused());
            sess.terminating_since = Some(Instant::now());
            assert!(!sess.is_pty_paused(), "terminating_since must override flow-control pause");
            sess.terminating_since = None;
            assert!(sess.is_pty_paused());
            sess.exit_drain = Some(super::super::ExitDrainState {
                code: Some(9),
                signal: None,
                deadline: Instant::now() + Duration::from_secs(1),
            });
            assert!(!sess.is_pty_paused(), "exit_drain must override flow-control pause");
            sess.paused_by_stream = false;
        }
        let wait_t = runner.compute_wait_timeout(TimestampMs::from_millis(2_000));
        assert_eq!(
            wait_t, TERMINATE_POLL_INTERVAL,
            "exit_drain must clamp shard wait timeout to TERMINATE_POLL_INTERVAL, got {wait_t:?}"
        );
        runner.sessions.get_mut(&sid_drain).unwrap().exit_drain =
            Some(super::super::ExitDrainState { code: Some(9), signal: None, deadline: Instant::now() });
        runner.run_tier2(TimestampMs::from_millis(2_000));
        assert!(
            !runner.sessions.contains_key(&sid_drain),
            "run_tier2 must finalize and remove a session whose exit_drain deadline elapsed"
        );
    }

    #[test]
    fn pr4_shard_wal_failed_detach_blob_gated_rollover_seal_and_atomic_tf_overflow() {
        use apack_wal::{GapReason, WalReader, WalRecord, WalWorker, WalWriter};

        // 0. Exact boundary checks for `leading_st_len` and `should_shed_wal_image_stream`.
        assert_eq!(super::super::images::leading_st_len(b"\x1b\\rest"), 2);
        assert_eq!(super::super::images::leading_st_len(b"\x9crest"), 1);
        assert_eq!(super::super::images::leading_st_len(b"plain"), 0);
        assert!(!super::super::images::should_shed_wal_image_stream(100, 100));
        assert!(super::super::images::should_shed_wal_image_stream(101, 100));
        assert!(super::super::images::should_shed_wal_image_stream(200, 100));

        let (mut runner, _rrx) = make_test_runner();
        let tmp = tempfile::tempdir().expect("tempdir");

        // 1. Sticky failed WAL handle is detached cleanly by `sync_wal_image_blobs`, `maybe_rollover_wal`,
        //    and `ResizePty` so hot shards never spin locking `ImageStore` or capturing keyframes.
        let sid_fail = SessionId::new(71).unwrap();
        insert_image_session(&mut runner, sid_fail, false);
        let fail_dir = tmp.path().join("failed-wal");
        let w = WalWriter::open_or_create(
            &fail_dir,
            [0x71; 16],
            100,
            WalConfig::default(),
            runner.shared.wal_registry.clone(),
        )
        .unwrap();
        let handle = WalWorker::spawn(w);
        assert!(handle.write_output(100, b"no-active-segment".to_vec(), true));
        assert!(handle.flush_barrier().is_err());
        assert!(handle.is_failed());
        {
            let sess = runner.sessions.get_mut(&sid_fail).unwrap();
            sess.wal = Some(handle);
            sess.wal_has_segment = true;
            sess.pending_wal_rollover = true;
            sess.wal_written_blobs.insert(apack_proto::types::BlobHash([0xAA; 32]));
            crate::shard::replay::sync_wal_image_blobs(sess, &runner.shared.image_store, &runner.shared.wal_reaper);
            assert!(sess.wal.is_none(), "failed WAL handle must be detached on sync_wal_image_blobs");
            assert!(!sess.wal_has_segment);
            assert!(!sess.pending_wal_rollover);
            assert!(sess.wal_written_blobs.is_empty());
        }
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(2)));

        // Verify `maybe_rollover_wal` also detaches a failed WAL handle directly.
        let fail_dir2 = tmp.path().join("failed-wal-2");
        let w2 = WalWriter::open_or_create(
            &fail_dir2,
            [0x72; 16],
            100,
            WalConfig::default(),
            runner.shared.wal_registry.clone(),
        )
        .unwrap();
        let handle2 = WalWorker::spawn(w2);
        assert!(handle2.write_output(100, b"boom".to_vec(), true));
        assert!(handle2.flush_barrier().is_err());
        {
            let sess = runner.sessions.get_mut(&sid_fail).unwrap();
            sess.wal = Some(handle2);
            sess.wal_has_segment = true;
            sess.pending_wal_rollover = true;
            crate::shard::replay::maybe_rollover_wal(
                sess,
                200,
                true,
                &runner.shared.engine_abi_hash,
                &runner.shared.image_store,
                &runner.shared.wal_reaper,
            );
            assert!(sess.wal.is_none(), "failed WAL handle must be detached on maybe_rollover_wal");
            assert!(!sess.wal_has_segment);
            assert!(!sess.pending_wal_rollover);
        }
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(2)));

        // Verify `ResizePty` also detaches a failed WAL handle cleanly.
        let fail_dir3 = tmp.path().join("failed-wal-3");
        let w3 = WalWriter::open_or_create(
            &fail_dir3,
            [0x75; 16],
            100,
            WalConfig::default(),
            runner.shared.wal_registry.clone(),
        )
        .unwrap();
        let handle3 = WalWorker::spawn(w3);
        assert!(handle3.write_output(100, b"boom".to_vec(), true));
        assert!(handle3.flush_barrier().is_err());
        {
            let sess = runner.sessions.get_mut(&sid_fail).unwrap();
            sess.wal = Some(handle3);
            sess.wal_has_segment = true;
        }
        runner.apply_effects_inner(
            sid_fail,
            vec![apack_core::session::SessionEffect::ResizePty {
                size: GridSize::new(90, 30).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
            }],
            false,
            None,
        );
        assert!(runner.sessions[&sid_fail].wal.is_none(), "ResizePty must detach failed WAL handle");
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(2)));

        // 2. Blob-gated rollover & seal refusal when an active blob cannot be persisted, plus
        //    `wal_has_segment == false` deferral (must not brick WAL on output/resize) & idle timeout clamp.
        let sid_gate = SessionId::new(72).unwrap();
        insert_image_session(&mut runner, sid_gate, false);
        let gate_dir = tmp.path().join("blob-gated-wal");
        let gate_w = WalWriter::open_or_create(
            &gate_dir,
            [0x73; 16],
            100,
            WalConfig::default(),
            runner.shared.wal_registry.clone(),
        )
        .unwrap();
        let gate_h = WalWorker::spawn(gate_w);
        {
            let sess = runner.sessions.get_mut(&sid_gate).unwrap();
            sess.compressed_idle = true;
            sess.pending_wal_rollover = true;
        }
        assert_eq!(
            runner.compute_wait_timeout(TimestampMs::from_millis(10)),
            Duration::from_secs(5),
            "pending_wal_rollover without wal must not clamp wait timeout"
        );
        {
            let sess = runner.sessions.get_mut(&sid_gate).unwrap();
            sess.wal = Some(gate_h);
            sess.pending_wal_rollover = false;
        }
        assert_eq!(
            runner.compute_wait_timeout(TimestampMs::from_millis(10)),
            Duration::from_secs(5),
            "live wal without pending_wal_rollover must not clamp wait timeout"
        );
        // With `wal_has_segment == false`, output & resize must NOT send records before segment 0 starts!
        runner.sessions.get_mut(&sid_gate).unwrap().pending_wal_rollover = true;
        assert_eq!(
            runner.compute_wait_timeout(TimestampMs::from_millis(10)),
            TERMINATE_POLL_INTERVAL,
            "pending_wal_rollover with live wal must clamp idle shard wait timeout to TERMINATE_POLL_INTERVAL"
        );
        runner.apply_effects_inner(
            sid_gate,
            vec![apack_core::session::SessionEffect::ResizePty {
                size: GridSize::new(82, 25).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
            }],
            false,
            None,
        );
        {
            let sess = runner.sessions.get_mut(&sid_gate).unwrap();
            // Now let `maybe_rollover_wal` start segment 0 cleanly.
            crate::shard::replay::maybe_rollover_wal(
                sess,
                100,
                true,
                &runner.shared.engine_abi_hash,
                &runner.shared.image_store,
                &runner.shared.wal_reaper,
            );
            assert!(sess.wal_has_segment, "maybe_rollover_wal must start segment 0 and set wal_has_segment");
            assert!(!sess.pending_wal_rollover);
            sess.wal.as_ref().unwrap().flush_barrier().unwrap();
            assert!(!sess.wal.as_ref().unwrap().is_failed(), "ResizePty while !wal_has_segment must not brick WAL");

            // Place a real inline 1x1 RGBA Kitty image into VT, then simulate an unpersistable blob
            // (empty ImageStore + image deleted from VT's rehydration? Actually easier: remove the
            // active image blob from an unrelated store passed to `maybe_rollover_wal` while leaving
            // `sess.machine` with `wal_images_gen != cur_gen` via a small closed/unpersisted check).
            let b64_px = String::from_utf8(encoded(&[0xffu8, 0x00, 0x00, 0xff])).unwrap();
            let seq = format!("\x1b_Ga=T,f=32,s=1,v=1,i=72,q=2;{b64_px}\x1b\\");
            sess.machine.on_pty_output(
                seq.as_bytes(),
                TimestampMs::from_millis(10),
                apack_core::time::WallMs::from_millis(10),
                &mut Vec::new(),
            );
            assert_ne!(sess.wal_images_gen, sess.machine.vt().generations().images);
            // Calling `maybe_rollover_wal` with a different empty store where rehydrate puts into
            // `runner.shared.image_store` (NOT `alt_store`) proves rollover REFUSES while `wal_images_gen != cur_gen`!
            let empty_store = Arc::new(Mutex::new(ImageStore::default()));
            sess.pending_wal_rollover = true;
            crate::shard::replay::maybe_rollover_wal(
                sess,
                150,
                true,
                &runner.shared.engine_abi_hash,
                &empty_store,
                &runner.shared.wal_reaper,
            );
            assert!(sess.pending_wal_rollover, "rollover must refuse while active image blob is unpersisted");
            assert_ne!(sess.wal_images_gen, sess.machine.vt().generations().images);

            // Now with the real image store, `maybe_rollover_wal` persists the blob and rolls over cleanly!
            crate::shard::replay::maybe_rollover_wal(
                sess,
                200,
                true,
                &runner.shared.engine_abi_hash,
                &runner.shared.image_store,
                &runner.shared.wal_reaper,
            );
            assert!(!sess.pending_wal_rollover, "once active blob persists, rollover succeeds");
            assert_eq!(sess.wal_images_gen, sess.machine.vt().generations().images);
        }
        // Exercise `ResizePty` effect against a live WAL (verifies `!detach_failed_wal && wal_has_segment`).
        runner.apply_effects_inner(
            sid_gate,
            vec![apack_core::session::SessionEffect::ResizePty {
                size: GridSize::new(90, 30).unwrap(),
                cell_px: CellPx { width: 8, height: 16 },
            }],
            false,
            None,
        );
        runner.finish_and_remove_session(sid_gate, Some(0), None);
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(3)));
        let gate_reader = WalReader::open(&gate_dir, &runner.shared.wal_registry).unwrap();
        assert_eq!(gate_reader.segments().len(), 2, "rollover created segment 2 after persisting blob");
        assert!(gate_reader.segments()[1].sealed, "segment 2 sealed cleanly once active blobs persisted");
        let gate_slice = gate_reader.latest_segment_records().unwrap().unwrap();
        assert!(
            gate_slice.tail_records.iter().any(|r| matches!(r.record, WalRecord::Resize { .. })),
            "ResizePty on live WAL must persist WalRecord::Resize"
        );
        for blob in gate_slice.keyframe.referenced_blobs() {
            assert!(
                gate_reader.read_blob(blob, apack_proto::types::ImageEncoding::Png).is_ok(),
                "every blob referenced by keyframe must exist in <dir>/blobs/"
            );
        }
        drop(gate_reader);

        // 2b. Verify `close_session_wal` skips `wal.seal()` when active blobs remain unpersisted.
        let sid_unsealed = SessionId::new(74).unwrap();
        insert_image_session(&mut runner, sid_unsealed, false);
        let unsealed_dir = tmp.path().join("unsealed-blob-gate");
        let uw = WalWriter::open_or_create(
            &unsealed_dir,
            [0x76; 16],
            100,
            WalConfig::default(),
            runner.shared.wal_registry.clone(),
        )
        .unwrap();
        let uh = WalWorker::spawn(uw);
        {
            let sess = runner.sessions.get_mut(&sid_unsealed).unwrap();
            let kf0 = crate::shard::replay::capture_keyframe(&sess.machine, &runner.shared.engine_abi_hash);
            assert!(uh.start_segment(100, kf0));
            uh.flush_barrier().unwrap();
            sess.wal = Some(uh);
            sess.wal_has_segment = true;
            let b64_px = String::from_utf8(encoded(&[0x00u8, 0xff, 0x00, 0xff])).unwrap();
            let seq = format!("\x1b_Ga=T,f=32,s=1,v=1,i=74,q=2;{b64_px}\x1b\\");
            sess.machine.on_pty_output(
                seq.as_bytes(),
                TimestampMs::from_millis(10),
                apack_core::time::WallMs::from_millis(10),
                &mut Vec::new(),
            );
            sess.pending_wal_rollover = true;
            let empty_store = Arc::new(Mutex::new(ImageStore::default()));
            crate::shard::replay::close_session_wal(
                sess,
                &runner.shared.engine_abi_hash,
                &empty_store,
                &runner.shared.wal_reaper,
            );
        }
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(3)));
        let unsealed_reader = WalReader::open(&unsealed_dir, &runner.shared.wal_registry).unwrap();
        assert_eq!(
            unsealed_reader.segments().len(),
            1,
            "must not start a new segment when active blobs did not persist"
        );
        assert!(!unsealed_reader.segments()[0].sealed, "wal.seal() must be skipped when active blobs did not persist");
        drop(unsealed_reader);

        // 2c. Verify a resolved `t=f` load while `wal_has_segment == false` never writes to WAL or bricks it.
        let sid_noseg = SessionId::new(75).unwrap();
        insert_image_session(&mut runner, sid_noseg, true);
        let noseg_dir = tmp.path().join("tf-no-segment");
        let nsw = WalWriter::open_or_create(
            &noseg_dir,
            [0x77; 16],
            100,
            WalConfig::default(),
            runner.shared.wal_registry.clone(),
        )
        .unwrap();
        let nsh = WalWorker::spawn(nsw);
        runner.sessions.get_mut(&sid_noseg).unwrap().wal = Some(nsh);
        let noseg_img = tmp.path().join("noseg.rgba");
        std::fs::write(&noseg_img, [0x11u8; 16]).unwrap();
        let noseg_b64 = String::from_utf8(encoded(noseg_img.to_string_lossy().as_bytes())).unwrap();
        let noseg_cmd = format!("\x1b_Ga=T,f=32,s=2,v=2,i=75,t=f,q=2;{noseg_b64}\x1b\\");
        runner.ingest_pty_bytes(sid_noseg, noseg_cmd.as_bytes());
        wait_and_drain_image(&mut runner, sid_noseg);
        let ns_wal = runner.sessions[&sid_noseg].wal.as_ref().expect("WAL must not detach/fail");
        ns_wal.flush_barrier().unwrap();
        assert!(!ns_wal.is_failed(), "t=f load while wal_has_segment == false must not brick WalWorker");
        runner.finish_and_remove_session(sid_noseg, Some(0), None);
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(3)));

        // 3. Short-lived multi-chunk resolved `t=f` exceeding `max_backlog_bytes` (with NO trailing
        //    bytes after `\x1b\\` and immediate exit without manual `flush_barrier`/`run_tier2`!):
        //    must shed inline APC atomically as ONE `BacklogOverflow` gap (ZERO partial `m=1`),
        //    still preserve the image in segment 2 via `start_segment_on_close`, and seal cleanly!
        let sid_tf = SessionId::new(73).unwrap();
        insert_image_session(&mut runner, sid_tf, true);
        let tf_dir = tmp.path().join("atomic-tf-overflow");
        let tf_cfg = WalConfig { max_backlog_bytes: 128, ..WalConfig::default() };
        let tf_w =
            WalWriter::open_or_create(&tf_dir, [0x74; 16], 100, tf_cfg, runner.shared.wal_registry.clone()).unwrap();
        let tf_h = WalWorker::spawn(tf_w);
        {
            let sess = runner.sessions.get_mut(&sid_tf).unwrap();
            let kf0 = crate::shard::replay::capture_keyframe(&sess.machine, &runner.shared.engine_abi_hash);
            assert!(tf_h.start_segment(100, kf0));
            tf_h.flush_barrier().unwrap();
            sess.wal = Some(tf_h);
            sess.wal_has_segment = true;
        }
        let img_path = tmp.path().join("tf_overflow.rgba");
        let pixels: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
        std::fs::write(&img_path, &pixels).unwrap();
        let path_b64 = String::from_utf8(encoded(img_path.to_string_lossy().as_bytes())).unwrap();
        // End right at `\x1b\\` (no trailing text after ST) to verify `close_session_wal`'s
        // `start_segment_on_close` preserves the image on immediate exit without a manual flush!
        let cmd = format!("\x1b_Ga=T,f=32,s=8,v=8,i=73,t=f,q=2;{path_b64}\x1b\\");
        runner.ingest_pty_bytes(sid_tf, cmd.as_bytes());
        wait_and_drain_image(&mut runner, sid_tf);

        assert!(
            !runner.sessions[&sid_tf].machine.vt().images().is_empty(),
            "live VT must still receive resolved t=f image even when WAL sheds inline APC"
        );
        runner.finish_and_remove_session(sid_tf, Some(0), None);
        assert!(runner.shared.wal_reaper.drain_bounded(Duration::from_secs(3)));

        let tf_reader = WalReader::open(&tf_dir, &runner.shared.wal_registry).unwrap();
        assert!(
            tf_reader.segments().len() >= 2,
            "immediate exit after atomic t=f overflow must commit rollover keyframe, got {} segments",
            tf_reader.segments().len()
        );
        let seg1_bytes = std::fs::read(tf_dir.join("seg-000001.apks")).unwrap();
        let seg1_scan = apack_wal::format::scan_segment(&seg1_bytes);
        assert!(
            seg1_scan.records.iter().any(|r| matches!(
                r.record,
                WalRecord::Gap { reason: GapReason::BacklogOverflow, dropped_bytes } if dropped_bytes > 128
            )),
            "expected BacklogOverflow gap in seg 1, got {:?}",
            seg1_scan.records
        );
        for rec in &seg1_scan.records {
            if let WalRecord::Output(bytes) = &rec.record {
                let s = String::from_utf8_lossy(bytes);
                assert!(!s.contains("m=1;"), "must never leave partial m=1 APC chunk in WAL: {s:?}");
            }
        }
        let latest = tf_reader.latest_segment_records().unwrap().unwrap();
        assert!(!latest.keyframe.visual.images.is_empty(), "rollover keyframe must preserve the resolved image");
        for blob in latest.keyframe.referenced_blobs() {
            assert!(tf_reader.read_blob(blob, apack_proto::types::ImageEncoding::Png).is_ok());
        }
    }
}
