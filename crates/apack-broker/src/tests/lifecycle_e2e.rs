use super::*;

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

    // 2. Cross-platform WAL rollover (> 64 KiB output -> >= 2 segments), cooperative idle scrollback
    //    compression, and Rewind `Signal` rejection (`ErrorCode::Malformed`) on Unix AND Windows!
    {
        let rec_dir = tmp.path().join("wal-roll-xplat");
        let roll_file = tmp.path().join("rollover-lines.txt");
        let mut roll_buf = String::with_capacity(1400 * 64);
        for i in 0..1400u32 {
            roll_buf.push_str(&format!("ROLLOVER_LINE_{i:04}_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n"));
        }
        std::fs::write(&roll_file, &roll_buf).unwrap();

        #[cfg(unix)]
        let roll_argv =
            vec![String::from("/bin/sh"), String::from("-c"), String::from("cat rollover-lines.txt; sleep 10")];
        #[cfg(windows)]
        let roll_argv = vec![
            String::from("cmd.exe"),
            String::from("/C"),
            String::from("type rollover-lines.txt & ping -n 10 127.0.0.1 >nul"),
        ];

        let spec_roll = SpawnSpec {
            argv: roll_argv,
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
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

        // Let idle compression (25 ms) run cooperatively on the 500 scrollback lines.
        let mut compressed_ok = false;
        let idle_dl = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < idle_dl {
            if c2.metrics().unwrap().scrollback_bytes > 0 {
                compressed_ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(compressed_ok, "expected scrollback_bytes > 0 after rollover output & idle compression");

        c2.kill(s_roll).unwrap();
        for _ in 0..150 {
            if !c2.list_sessions().unwrap().iter().any(|s| s.id == s_roll) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!c2.list_sessions().unwrap().iter().any(|s| s.id == s_roll));

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
    }

    // 2b. Unix foreground verify (OSC 7501) + SIGKILL escalation + graceful SIGHUP + paused-PTY kill!
    #[cfg(unix)]
    {
        let script = concat!(
            "printf 'FG_READY\\r\\n'; ",
            "read _go; ",
            "printf '\\033]7501;id=st_ok:app=sh:state=working\\007'; ",
            "printf '\\033]7501;id=st_bad:app=nonexistent_app_xyz:state=working\\007'; ",
            "trap '' HUP; sleep 10"
        );
        let spec_fg = SpawnSpec {
            argv: vec![String::from("/bin/sh"), String::from("-c"), String::from(script)],
            cwd: None,
            env: Vec::new(),
            size: GridSize::new(80, 10).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            scrollback_limit: 200,
            marks_key: None,
            resize_policy: ResizePolicy::LatestActive,
            record_dir: None,
            restore_from: None,
            image_files: false,
            kept: false,
        };
        let (s_fg, _) = c2.spawn(spec_fg).unwrap();
        assert_eq!(c2.wait(s_fg, WaitCondition::Text(String::from("FG_READY")), 5_000).unwrap(), WaitOutcome::Met);
        let st = NonZeroStreamId::new(1).unwrap();
        c2.attach(
            s_fg,
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

        // Kill s_fg (which traps SIGHUP) -> escalates to SIGKILL after `kill_escalate_after` and reaps!
        c2.kill(s_fg).unwrap();
        for _ in 0..150 {
            let ids: Vec<SessionId> = c2.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
            if !ids.contains(&s_fg) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!c2.list_sessions().unwrap().iter().any(|s| s.id == s_fg));

        // Verify graceful SIGHUP runs BEFORE SIGKILL escalation (`kill_escalate_after = 400ms`):
        // a shell trapping SIGHUP and exiting with code 7 must exit cleanly with Some(7) (not SIGKILL)!
        // Use a bounded loop with a HUP-ignoring subshell so Ubuntu `dash` never exits 129 (`128+HUP`)
        // from reaping a HUP-killed background `sleep` as the final command before running the trap.
        let hup_spec = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from(
                    "trap 'sleep 0.04; exit 7' HUP; printf 'HUP_TRAP_READY\\r\\n'; i=0; while [ $i -lt 10 ]; do i=$((i+1)); (trap '' HUP; exec sleep 1) & wait $!; done",
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
        let tail_written_flag = tmp.path().join("tail-written.flag");
        let tail_spec = SpawnSpec {
            argv: vec![
                String::from("/bin/sh"),
                String::from("-c"),
                format!(
                    "printf 'TAIL_ARMED\\r\\n'; read _go; printf 'TAIL_BEFORE_EXIT_99\\r\\n'; : > '{}'; sleep 10",
                    tail_written_flag.display()
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
        // Deterministic barrier: wait for the child to create `tail_written_flag` after writing to PTY stdout.
        let flag_dl = std::time::Instant::now() + Duration::from_secs(3);
        while !tail_written_flag.exists() && std::time::Instant::now() < flag_dl {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(tail_written_flag.exists(), "child must write buffered tail before kill()");
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

    // 2c. drain_win_events_bounded + clamp_win_wait
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<u32>();
    let mut seen = Vec::new();
    assert!(!crate::transport_windows::drain_win_events_bounded(&ev_rx, 2, |v| seen.push(v)));
    assert!(seen.is_empty());
    ev_tx.send(10).unwrap();
    ev_tx.send(20).unwrap();
    assert!(!crate::transport_windows::drain_win_events_bounded(&ev_rx, 2, |v| seen.push(v)));
    assert_eq!(seen, vec![10, 20]);
    seen.clear();
    for v in [1, 2, 3, 4] {
        ev_tx.send(v).unwrap();
    }
    assert!(crate::transport_windows::drain_win_events_bounded(&ev_rx, 2, |v| seen.push(v)));
    assert_eq!(seen, vec![1, 2, 3]);
    assert_eq!(ev_rx.try_recv().unwrap(), 4);
    assert_eq!(crate::transport_windows::clamp_win_wait(Duration::from_secs(1), false), Duration::from_secs(1));
    assert_eq!(crate::transport_windows::clamp_win_wait(Duration::from_secs(1), true), Duration::ZERO);

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
    //     Use a 5s configured drain timeout and assert prompt auto-exit (< 2s) so noisy Windows CI
    //     cannot flake while still proving `writer_done` short-circuits `graceful_drain_timeout`!
    let dir_solo = tempdir().unwrap();
    let ep_solo = unique_endpoint(dir_solo.path(), "win-drain-solo");
    let mut cfg_solo = BrokerConfig::for_socket(&ep_solo);
    cfg_solo.shard_count = 1;
    cfg_solo.graceful_drain_timeout = Duration::from_secs(5);
    let mut srv_solo = BrokerServer::bind(cfg_solo).unwrap().spawn().unwrap();
    let mut burst = apack_os::transport::connect_local_stream(srv_solo.socket_path()).unwrap();
    do_hello(&mut burst);
    let t_fin = std::time::Instant::now();
    burst.write_all(&batch).unwrap();
    burst.flush().unwrap();
    // Wait 100ms (<< 5s configured graceful_drain_timeout): broker MUST stay alive while replies
    // wait to drain (kills `Instant::now() - graceful_drain_timeout` mutant cleanly!).
    std::thread::sleep(Duration::from_millis(100));
    assert!(!srv_solo.is_finished(), "draining_win must keep broker alive before graceful_drain_timeout expires");
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
    while !srv_solo.is_finished() && t_fin.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        srv_solo.is_finished() && t_fin.elapsed() < Duration::from_secs(2),
        "broker must auto-exit promptly once writer_done is true (before 5s timeout): elapsed={:?}",
        t_fin.elapsed()
    );
    srv_solo.shutdown().unwrap();

    // 4b. Sole stalled peer pipelines WIN_BURST_N Metrics + FIN_FRAME and never reads ->
    //     reap_draining_win_conns must forcibly close_abrupt after 250ms so broker auto-exits!
    let dir_reap = tempdir().unwrap();
    let ep_reap = unique_endpoint(dir_reap.path(), "win-drain-reap");
    let mut cfg_reap = BrokerConfig::for_socket(&ep_reap);
    cfg_reap.shard_count = 1;
    cfg_reap.graceful_drain_timeout = Duration::from_millis(250);
    let mut srv_reap = BrokerServer::bind(cfg_reap).unwrap().spawn().unwrap();
    let mut stalled = apack_os::transport::connect_local_stream(srv_reap.socket_path()).unwrap();
    do_hello(&mut stalled);
    stalled.write_all(&batch).unwrap();
    stalled.flush().unwrap();
    let reap_wait = std::time::Instant::now();
    while !srv_reap.is_finished() && reap_wait.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        srv_reap.is_finished(),
        "reap_draining_win_conns must drop stalled peer after graceful_drain_timeout so broker auto-exits"
    );
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
    while !srv_small.is_finished() && wait_small.elapsed() < Duration::from_secs(5) {
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

#[test]
fn pr6_session_lifetime_conn_bound_rewind_and_unleased_disconnect_e2e() {
    let tmp = tempdir().unwrap();
    let sock = unique_endpoint(tmp.path(), "pr6-lifetime");
    let rec_dir = tmp.path().join("rec-rewind");

    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    cfg.kill_escalate_after = Duration::from_millis(250);
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();

    // 1. Record a short session to WAL so we have a valid recording to open in `Rewind` mode.
    let mut rec_client = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "recorder").unwrap();
    let rec_dir_str = rec_dir.to_string_lossy().into_owned();
    let (rec_sid, _) = rec_client.spawn(echo_command_spec("PR6_REWIND_DATA", Some(rec_dir_str.clone()), None)).unwrap();
    assert_eq!(
        rec_client.wait(rec_sid, WaitCondition::Text(String::from("PR6_REWIND_DATA")), 5_000).unwrap(),
        WaitOutcome::Met
    );
    rec_client.kill(rec_sid).unwrap();
    for _ in 0..100 {
        if rec_client.list_sessions().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(rec_client.list_sessions().unwrap().is_empty());

    // 2. Two unleased clients connect:
    //    - `owner` opens a `Rewind` session (`ConnBound(owner)`), a live `LeaseBound` session,
    //      and a `Kept` session.
    //    - `observer` stays connected without any broker lease.
    let mut owner = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "owner").unwrap();
    let mut observer = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "observer").unwrap();
    drop(rec_client);

    let mut spec_kept = echo_command_spec("PR6_KEPT", None, None);
    spec_kept.kept = true;
    let (s_kept, _) = owner.spawn(spec_kept).unwrap();
    let (s_lease_bound, _) = owner.spawn(echo_command_spec("PR6_LEASE_BOUND", None, None)).unwrap();
    let (s_rewind, _, _) = owner.rewind(rec_dir_str, None).unwrap();
    assert_eq!(
        owner.wait(s_lease_bound, WaitCondition::Text(String::from("PR6_LEASE_BOUND")), 5_000).unwrap(),
        WaitOutcome::Met
    );

    // 3. `owner` disconnects while `observer` stays connected:
    //    - `s_rewind` (`ConnBound(owner)`) MUST be torn down immediately!
    //    - `s_lease_bound` (`LeaseBound`) and `s_kept` (`Kept`) MUST stay alive while `observer` is connected!
    drop(owner);
    for _ in 0..100 {
        let ids: Vec<SessionId> = observer.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
        if !ids.contains(&s_rewind) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let after_owner_drop: Vec<SessionId> = observer.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
    assert!(!after_owner_drop.contains(&s_rewind), "ConnBound Rewind session must close when its owner disconnects");
    assert!(
        after_owner_drop.contains(&s_lease_bound),
        "LeaseBound session must survive while another client is connected"
    );
    assert!(after_owner_drop.contains(&s_kept), "Kept session must survive");

    // 4. Last unleased client (`observer`) disconnects:
    //    - `s_lease_bound` (`LeaseBound`) MUST tear down immediately (`TeardownNonKeptSessions`),
    //      while `s_kept` keeps the broker alive and survives for a fresh client to inspect!
    drop(observer);
    // Let the reactor observe `observer`'s socket EOF while `conns` is genuinely empty before
    // `inspector` connects (otherwise `inspector` could accept before `observer`'s FIN is polled).
    std::thread::sleep(Duration::from_millis(80));
    let mut inspector = BrokerClient::connect(handle.socket_path(), &AuthConfig::None, "inspector").unwrap();
    for _ in 0..150 {
        let ids: Vec<SessionId> = inspector.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
        if !ids.contains(&s_lease_bound) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let after_last_drop: Vec<SessionId> = inspector.list_sessions().unwrap().into_iter().map(|s| s.id).collect();
    assert!(
        !after_last_drop.contains(&s_lease_bound),
        "non-kept LeaseBound session must tear down when the last unleased client disconnects"
    );
    assert!(after_last_drop.contains(&s_kept), "Kept session must survive last unleased client disconnect");

    drop(inspector);
    handle.shutdown().unwrap();
}
