//! Session termination, SIGHUP/SIGKILL escalation, child reaping, and Tier-2 idle compression.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use apack_core::time::TimestampMs;
use apack_core::vt::VtEngine;
use apack_os::image_sources::Scanner as ImageScanner;
#[cfg(unix)]
use apack_os::signal::hang_up_session_descendants;
use apack_os::signal::{deliver_signal, kill_session_descendants, signal_name_from_description};
use apack_proto::types::{SessionId, SignalKind, SignalScope};

use crate::now_unix_ms;

#[cfg(unix)]
use super::PTY_READ_BUF_BYTES;
#[cfg(unix)]
use super::pty_io::drain_unix_pty_nonblocking;
use super::{
    COMPRESS_PAGES_PER_TURN, LivePty, PTY_READ_QUANTUM_BYTES, ReactorEvent, ShardRunner, TERMINATE_POLL_INTERVAL,
};

impl ShardRunner {
    pub(super) fn initiate_session_termination(&mut self, session_id: SessionId) {
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if sess.rewound {
            self.finish_and_remove_session(session_id, None, None);
            return;
        }
        if sess.terminating_since.is_some() {
            return;
        }
        sess.terminating_since = Some(Instant::now());
        // If `libghostty-vt` is currently sitting inside an open APC (either awaiting an off-shard
        // image load or mid-scan), abort it with CAN (0x18) so subsequent teardown output is not
        // swallowed inside the unclosed APC string.
        if sess.pending_image.is_some() || sess.image_scanner.as_ref().is_some_and(ImageScanner::reading) {
            let mut evs = Vec::new();
            sess.machine.vt_mut().feed(&[0x18], &mut evs);
        }
        // Cancel any in-flight named-source read immediately so Kill / teardown never stalls on a
        // stuck mount, and stop scanning new named sources while draining teardown bytes.
        sess.image_scanner = None;
        sess.image_ticket = sess.image_ticket.wrapping_add(1);
        sess.pending_image = None;
        sess.pending_image_tail.clear();
        // Unpause PTY reads during teardown so macOS/BSD kernel `ttydrain()` cannot wedge an
        // exiting child in `?Es` state waiting for unread master PTY bytes!
        sess.paused_by_stream = false;
        sess.paused_by_raw_flow = false;
        #[cfg(unix)]
        let raw_pid = sess.pid.map(|p| p.pid() as u32);
        self.sync_pty_interest(session_id);

        #[cfg(unix)]
        {
            if let Some(raw) = raw_pid {
                // Signal non-leader descendants WHILE the leader is still alive, then SIGHUP leader group!
                hang_up_session_descendants(&[raw]);
                let _ = deliver_signal(raw, SignalKind::Hup, SignalScope::Group);
            }
        }
        #[cfg(windows)]
        {
            if let Some(sess) = self.sessions.get_mut(&session_id) {
                // Dropping TreeGuard closes the Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`),
                // terminating the entire process tree atomically; close ConPTY master asynchronously.
                sess.tree = None;
                if let Some(killer) = sess.killer.as_mut() {
                    let _ = killer.kill();
                }
                if let Some(LivePty::Windows { bridge, .. }) = &sess.pty {
                    bridge.close_master_async();
                }
            }
        }

        self.try_reap_session(session_id);
    }

    pub(crate) fn try_reap_session(&mut self, session_id: SessionId) {
        let exit_opt = {
            let Some(sess) = self.sessions.get_mut(&session_id) else {
                return;
            };
            // Defer natural exit while an off-shard Kitty named-source load is still in flight;
            // explicit termination clears `pending_image` before calling `try_reap_session`.
            if sess.pending_image.is_some() {
                return;
            }
            if let Some(exit) = sess.reaped_exit.take() {
                Some(exit)
            } else {
                let Some(child) = sess.child.as_mut() else {
                    return;
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let parsed = parse_exit_status(&status);
                        // Clear reaped child handles immediately; on natural exit also clear `pid`
                        // right away so a PID recycled by the OS during a deferred `pending_image`
                        // load can never be signalled by a late `Kill`/`Signal`/shutdown.
                        sess.child = None;
                        sess.killer = None;
                        if sess.terminating_since.is_none() {
                            sess.pid = None;
                            #[cfg(unix)]
                            {
                                sess.tree = None;
                            }
                        }
                        Some(parsed)
                    }
                    _ => None,
                }
            }
        };

        let Some((code, signal)) = exit_opt else {
            return;
        };

        // Drain tail PTY bytes BEFORE finalizing exit so both ordinary tail output and short-lived
        // commands that print a `t=f` / `t=s` APC and exit immediately are ingested before `Exited`.
        // Explicit termination clears `image_scanner` upfront so it never starts a new image load.
        self.drain_tail_pty_bytes(session_id);
        if let Some(sess) = self.sessions.get_mut(&session_id)
            && sess.pending_image.is_some()
        {
            sess.reaped_exit = Some((code, signal));
            return;
        }

        self.finish_and_remove_session(session_id, code, signal);
    }

    pub(super) fn drain_tail_pty_bytes(&mut self, session_id: SessionId) {
        #[cfg(unix)]
        {
            // Use a fresh buffer instead of taking `self.read_buf` so re-entrant calls from
            // `read_unix_pty_quantum -> apply_effects -> drain_pty_writes_for -> try_reap_session`
            // never see a 0-length `self.read_buf`.
            let mut buf = vec![0u8; PTY_READ_BUF_BYTES];
            let mut chunks = Vec::new();
            if let Some(LivePty::Unix { master, eof: false, .. }) =
                self.sessions.get(&session_id).and_then(|s| s.pty.as_ref())
            {
                drain_unix_pty_nonblocking(master, &mut buf, PTY_READ_QUANTUM_BYTES, |chunk| {
                    chunks.push(chunk.to_vec());
                });
            }
            for chunk in chunks {
                self.ingest_pty_bytes(session_id, &chunk);
            }
        }
        #[cfg(windows)]
        {
            let mut chunks = Vec::new();
            if let Some(LivePty::Windows { bridge, eof: false }) =
                self.sessions.get(&session_id).and_then(|s| s.pty.as_ref())
            {
                let _ = crate::shard_windows::drain_conpty_quantum(bridge, PTY_READ_QUANTUM_BYTES, |b| {
                    chunks.push(b.to_vec())
                });
            }
            for chunk in chunks {
                self.ingest_pty_bytes(session_id, &chunk);
            }
        }
    }

    pub(super) fn finish_and_remove_session(
        &mut self,
        session_id: SessionId,
        code: Option<i32>,
        signal: Option<String>,
    ) {
        let mut effects = Vec::new();
        if let Some(mut sess) = self.sessions.remove(&session_id) {
            #[cfg(unix)]
            {
                if let Some(LivePty::Unix { token, .. }) = sess.pty {
                    let _ = self.poller.deregister(token);
                    self.token_to_session.remove(&token);
                }
                // If this session was explicitly terminated (Kill / teardown) and the leader exited
                // on SIGHUP before `kill_escalate_after`, SIGKILL any remaining HUP-ignoring
                // background jobs in separate process groups belonging to the same POSIX session.
                if sess.terminating_since.is_some()
                    && let Some(pid) = sess.pid
                {
                    kill_session_descendants(&[pid.pid() as u32]);
                }
            }
            #[cfg(windows)]
            if let Some(LivePty::Windows { ref bridge, .. }) = sess.pty {
                bridge.close_master_async();
            }
            if let Some(wal) = sess.wal.take() {
                let _ = wal.seal();
                let _ = wal.shutdown();
            }
            let now = TimestampMs::from_millis(now_unix_ms());
            if !sess.rewound {
                sess.machine.on_process_exit(code, signal, now, &mut effects);
            }
            drop(sess);
        }
        self.load.store(self.sessions.len(), Ordering::Relaxed);
        self.apply_effects(session_id, effects);
        self.emit(ReactorEvent::SessionEnded { session: session_id });
    }

    pub(super) fn force_drop_session(&mut self, session_id: SessionId) {
        if let Some(mut sess) = self.sessions.remove(&session_id) {
            #[cfg(unix)]
            {
                if let Some(LivePty::Unix { token, .. }) = sess.pty {
                    let _ = self.poller.deregister(token);
                    self.token_to_session.remove(&token);
                }
                if let Some(pid) = sess.pid {
                    let raw = pid.pid() as u32;
                    // SIGKILL non-leader descendants BEFORE killing the leader!
                    kill_session_descendants(&[raw]);
                    let _ = deliver_signal(raw, SignalKind::Kill, SignalScope::Group);
                }
            }
            #[cfg(windows)]
            {
                sess.tree = None;
                if let Some(LivePty::Windows { ref bridge, .. }) = sess.pty {
                    bridge.close_master_async();
                }
            }
            if let Some(mut killer) = sess.killer.take() {
                let _ = killer.kill();
            }
            // Drop the master PTY fd BEFORE `child.wait()` so macOS/BSD kernel `ttydrain()`
            // releases any child waiting for unread master bytes instead of wedging `wait()`.
            sess.pty = None;
            if let Some(mut child) = sess.child.take() {
                let _ = child.wait();
            }
            if let Some(wal) = sess.wal.take() {
                let _ = wal.seal();
                let _ = wal.shutdown();
            }
        }
    }

    pub(super) fn run_tier2(&mut self, now: TimestampMs) {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let now_instant = Instant::now();

        for id in ids {
            // 1. Child reap & 2-second SIGKILL escalation BEFORE firing timers so tail PTY bytes
            //    from an exited child satisfy pending `Wait` conditions before any deadline expires.
            let is_live = self.sessions.get(&id).is_some_and(|s| s.child.is_some() || s.reaped_exit.is_some());
            if is_live {
                if let Some(sess) = self.sessions.get_mut(&id)
                    && let Some(since) = sess.terminating_since
                    && !sess.escalated_sigkill
                    && now_instant.saturating_duration_since(since) >= self.shared.kill_escalate_after
                {
                    sess.escalated_sigkill = true;
                    if let Some(pid) = sess.pid {
                        let raw = pid.pid() as u32;
                        // SIGKILL non-leader descendants BEFORE killing the leader!
                        kill_session_descendants(&[raw]);
                        let _ = deliver_signal(raw, SignalKind::Kill, SignalScope::Group);
                    }
                    if let Some(killer) = sess.killer.as_mut() {
                        let _ = killer.kill();
                    }
                }
                self.try_reap_session(id);
            }

            // 2. Session timers (DEC 2026 hold cap, search debounce, Wait deadlines).
            let mut effects = Vec::new();
            if let Some(sess) = self.sessions.get_mut(&id) {
                sess.machine.poll_timers(now, &mut effects);
            }
            self.apply_effects(id, effects);

            // 3. Cooperative idle scrollback compression (4 pages/turn after idle_compress_after).
            if let Some(sess) = self.sessions.get_mut(&id) {
                maybe_compress_idle(
                    &mut sess.compressed_idle,
                    now_instant.saturating_duration_since(sess.last_activity),
                    self.shared.idle_compress_after,
                    || sess.machine.vt_mut().compress_step(COMPRESS_PAGES_PER_TURN),
                );
            }
        }
    }

    pub(super) fn refresh_telemetry_atomics(&self) {
        let scrollback_bytes: u64 = self.sessions.values().map(|s| s.machine.vt().scrollback_bytes()).sum();
        self.telemetry.scrollback_bytes.store(scrollback_bytes, Ordering::Relaxed);
        self.telemetry.total_pty_bytes.store(self.total_pty_bytes, Ordering::Relaxed);
    }
}

/// Runs one cooperative idle-compression step once `idle_elapsed >= idle_after`.
pub(crate) fn maybe_compress_idle(
    compressed_idle: &mut bool,
    idle_elapsed: Duration,
    idle_after: Duration,
    compress_fn: impl FnOnce() -> apack_core::vt::CompressStep,
) {
    if !*compressed_idle && idle_elapsed >= idle_after && compress_fn() == apack_core::vt::CompressStep::Done {
        *compressed_idle = true;
    }
}

/// Computes the next shard wait timeout contribution for a single session.
#[allow(clippy::too_many_arguments)]
pub(crate) fn shard_session_wait_timeout(
    next_deadline: Option<TimestampMs>,
    now: TimestampMs,
    terminating: bool,
    pty_paused: bool,
    pty_eof: bool,
    compressed_idle: bool,
    idle_elapsed: Duration,
    idle_compress_after: Duration,
) -> Duration {
    let mut timeout = Duration::from_secs(5);
    if let Some(dl) = next_deadline {
        timeout = timeout.min(Duration::from_millis(dl.saturating_sub(now)));
    }
    if terminating || pty_paused || pty_eof {
        timeout = timeout.min(TERMINATE_POLL_INTERVAL);
    } else if !compressed_idle {
        if idle_elapsed >= idle_compress_after {
            timeout = timeout.min(Duration::from_millis(5));
        } else {
            timeout = timeout.min(idle_compress_after - idle_elapsed);
        }
    }
    timeout
}

pub(crate) fn parse_exit_status(status: &portable_pty::ExitStatus) -> (Option<i32>, Option<String>) {
    if let Some(desc) = status.signal() {
        (None, Some(signal_name_from_description(desc)))
    } else {
        (Some(status.exit_code() as i32), None)
    }
}

pub(crate) fn argv_matches_app(argv: &[String], app: &str) -> bool {
    let needle = app.trim();
    if needle.is_empty() {
        return false;
    }
    argv.iter().any(|arg| {
        let base = Path::new(arg).file_name().and_then(|s| s.to_str()).unwrap_or(arg.as_str());
        base.eq_ignore_ascii_case(needle)
    })
}
