//! Windows ConPTY shard I/O edge (`#[cfg(windows)]`-only; mutated on Windows CI).

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use apack_os::pty_windows::{ConPtyBridge, ConPtyOutput};
use apack_proto::types::SessionId;

use crate::shard::{ExitDrainState, LivePty, LiveSession, ShardMailboxState, ShardRunner};

const WIN_PTY_WRITE_CHUNK_BYTES: usize = 65_536;

/// Maximum wall-clock duration to drain trailing ConPTY bytes after child process exit and
/// `close_master_async()` before emitting `BrokerMsg::Exited`.
pub(crate) const WIN_CONPTY_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Transitions a naturally exited Windows session into `DrainingConPtyToEof(deadline)` by closing
/// the ConPTY master off-shard so `conhost.exe` flushes all trailing output through EOF before
/// `BrokerMsg::Exited` is emitted. Returns `None` when exit finalization should wait for EOF, or
/// `Some(parsed)` when the PTY is already at EOF / absent.
pub(crate) fn finish_or_begin_win_exit_drain(
    sess: &mut LiveSession,
    parsed: (Option<i32>, Option<String>),
    now: Instant,
) -> Option<(Option<i32>, Option<String>)> {
    if let Some(LivePty::Windows { bridge, eof: false }) = &sess.pty {
        bridge.close_master_async();
        sess.exit_drain =
            Some(ExitDrainState { code: parsed.0, signal: parsed.1, deadline: now + WIN_CONPTY_DRAIN_TIMEOUT });
        return None;
    }
    Some(parsed)
}

/// Drains up to `budget` bytes of ready ConPTY output from `bridge`, invoking `on_bytes` for each
/// chunk. Returns `(hit_eof, exhausted_quantum)`.
pub(crate) fn drain_conpty_quantum(
    bridge: &ConPtyBridge,
    mut budget: usize,
    mut on_bytes: impl FnMut(&[u8]),
) -> (bool, bool) {
    while budget != 0 {
        match bridge.try_recv_output() {
            Some(ConPtyOutput::Data(bytes)) => {
                budget = budget.saturating_sub(bytes.len());
                on_bytes(&bytes);
            }
            Some(ConPtyOutput::Eof) => return (true, false),
            None => return (false, false),
        }
    }
    (false, true)
}

/// Sleeps on `cond` until either `mailbox` becomes non-empty, `woken` was signalled (eliminating
/// lost wakeups from `ConPtyBridge` output pushes), or `timeout` elapses.
pub(crate) fn wait_for_win_mailbox(mailbox: &Mutex<ShardMailboxState>, cond: &Condvar, timeout: Duration) {
    let mut state = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    if state.queue.is_empty() && !state.woken {
        let (guard, _) = cond.wait_timeout(state, timeout).unwrap_or_else(|e| e.into_inner());
        state = guard;
    }
    state.woken = false;
}

/// Clamps shard wait timeout when a Windows ConPTY session has pending input bytes to flush.
pub(crate) fn clamp_win_write_timeout(timeout: Duration, pending_write_bytes: usize) -> Duration {
    if pending_write_bytes != 0 { timeout.min(Duration::from_millis(10)) } else { timeout }
}

/// Waits on the Windows shard mailbox unless a ConPTY session exhausted its per-turn output quantum.
pub(crate) fn maybe_wait_win_mailbox(
    exhausted_quantum: bool,
    mailbox: &Mutex<ShardMailboxState>,
    cond: &Condvar,
    timeout: Duration,
) {
    if !exhausted_quantum {
        wait_for_win_mailbox(mailbox, cond, timeout);
    }
}

/// Writes pending PTY input bytes into a Windows `ConPtyBridge` without dropping on `WouldBlock`.
pub(crate) fn drain_windows_pty_writes(sess: &mut LiveSession) {
    let mut scratch = Vec::new();
    while sess.machine.pty_write_len() != 0 {
        sess.machine.peek_pty_write(WIN_PTY_WRITE_CHUNK_BYTES, &mut scratch);
        let written_len = scratch.len();
        let res = match &sess.pty {
            Some(LivePty::Windows { bridge, eof: false }) => bridge.write(scratch.clone()),
            _ => break,
        };
        match res {
            Ok(()) => sess.machine.consume_pty_write(written_len),
            Err(_) => break, // WouldBlock / closed: retain unconsumed bytes for retry
        }
    }
}

impl ShardRunner {
    /// Drains ready Windows ConPTY output (respecting `is_pty_paused`) and pending input writes.
    /// Returns `true` when at least one unpaused session exhausted its per-turn read quantum,
    /// signalling the shard loop to immediately spin another turn without sleeping.
    pub(crate) fn drain_windows_ptys(&mut self, budget: usize) -> bool {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let mut any_exhausted = false;
        for session_id in ids {
            let mut chunks = Vec::new();
            let (hit_eof, exhausted) = match self.sessions.get(&session_id) {
                Some(sess) if !sess.is_pty_paused() => match sess.pty.as_ref() {
                    Some(LivePty::Windows { bridge, eof: false }) => {
                        drain_conpty_quantum(bridge, budget, |bytes| chunks.push(bytes.to_vec()))
                    }
                    _ => (false, false),
                },
                _ => (false, false),
            };
            any_exhausted |= exhausted;
            for chunk in chunks {
                self.ingest_pty_bytes(session_id, &chunk);
            }
            if hit_eof {
                if let Some(sess) = self.sessions.get_mut(&session_id)
                    && let Some(LivePty::Windows { eof, .. }) = sess.pty.as_mut()
                {
                    *eof = true;
                }
                self.try_reap_session(session_id);
            }
            if let Some(sess) = self.sessions.get_mut(&session_id) {
                drain_windows_pty_writes(sess);
            }
        }
        any_exhausted
    }
}
