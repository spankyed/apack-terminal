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
    use apack_wal::{RecordingRegistry, WalConfig};

    use super::super::*;
    use crate::shard_windows::{clamp_win_write_timeout, drain_conpty_quantum, maybe_wait_win_mailbox};

    #[test]
    fn win_shard_helpers_and_drain_windows_ptys_cover_pause_quantum_and_eof() {
        // 1. clamp_win_write_timeout
        assert_eq!(clamp_win_write_timeout(Duration::from_secs(1), 0), Duration::from_secs(1));
        assert_eq!(clamp_win_write_timeout(Duration::from_secs(1), 4), Duration::from_millis(10));
        assert_eq!(clamp_win_write_timeout(Duration::from_millis(3), 4), Duration::from_millis(3));

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
                pty: Some(LivePty::Windows { bridge, eof: false }),
                child: Some(spawned.child),
                killer: Some(spawned.killer),
                tree: spawned.tree,
                pid: None,
                kept: false,
                rewound: false,
                created_ms: 0,
                wal: None,
                pending_wal_rollover: false,
                rewind_reader: None,
                rewind_at_ms: 0,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: paused,
                paused_by_raw_flow: false,
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

        // Direct drain_conpty_quantum check for (true, false) EOF and (false, false) empty using
        // live_id's already-active bridge (no 3rd ConPTY spawn or DSR wait).
        let mut live_sess = runner.sessions.remove(&live_id).expect("live session");
        if let Some(mut killer) = live_sess.killer.take() {
            let _ = killer.kill();
        }
        if let Some(mut child) = live_sess.child.take() {
            let _ = child.wait();
        }
        let Some(LivePty::Windows { bridge, .. }) = live_sess.pty.take() else {
            panic!("expected Windows LivePty");
        };
        bridge.close_master_async();
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
        assert!(saw_eof, "drain_conpty_quantum must report (true, false) on EOF");
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
    use apack_proto::types::{CellPos, CellPx, CellSpan, GridSize, ResizePolicy};
    use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
    use apack_wal::{RecordingRegistry, WalConfig};

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
                pending_wal_rollover: false,
                rewind_reader: None,
                rewind_at_ms: 0,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: false,
                paused_by_raw_flow: false,
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
}
