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
    if let Some(LivePty::Windows { bridge, eof: false, .. }) = &sess.pty {
        // Clear flow-control pauses so `drain_windows_ptys` flushes trailing ConPTY output through
        // EOF promptly instead of stalling behind a paused stream for the full 2s drain window.
        sess.paused_by_stream = false;
        sess.paused_by_raw_flow = false;
        sess.paused_by_raw_inflight = false;
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

/// Clamps shard wait timeout when a writable Windows ConPTY session has pending input bytes to flush.
pub(crate) fn clamp_win_write_timeout(
    timeout: Duration,
    has_writable_pty: bool,
    pending_write_bytes: usize,
) -> Duration {
    if has_writable_pty && pending_write_bytes != 0 { timeout.min(Duration::from_millis(10)) } else { timeout }
}

/// Clamps shard wait timeout for `sess`, avoiding a 100 Hz idle spin on rewound sessions or broken
/// ConPTY input pipes.
pub(crate) fn clamp_win_session_write_timeout(timeout: Duration, sess: &LiveSession) -> Duration {
    let has_writable_pty = matches!(sess.pty, Some(LivePty::Windows { eof: false, write_broken: false, .. }));
    clamp_win_write_timeout(timeout, has_writable_pty, sess.machine.pty_write_len())
}

/// Returns `true` iff a Windows ConPTY write failure is transient backpressure (`WouldBlock`)
/// rather than a broken/closed pipe.
pub(crate) fn is_win_write_would_block(err: &apack_os::OsError) -> bool {
    matches!(err, apack_os::OsError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock)
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

/// Applies one `ConPtyBridge::write` outcome to `sess`, returning `true` when the write loop
/// should continue flushing additional chunks.
pub(crate) fn apply_win_write_result(
    sess: &mut LiveSession,
    written_len: usize,
    res: Result<(), apack_os::OsError>,
) -> bool {
    match res {
        Ok(()) => {
            sess.machine.consume_pty_write(written_len);
            true
        }
        Err(err) if is_win_write_would_block(&err) => false, // WouldBlock: retain bytes for retry
        Err(_) => {
            if let Some(LivePty::Windows { write_broken, .. }) = sess.pty.as_mut() {
                *write_broken = true;
            }
            false
        }
    }
}

/// Writes pending PTY input bytes into a Windows `ConPtyBridge`, retrying on `WouldBlock` and
/// latching `write_broken` on broken pipes (matching Unix `drain_pty_writes_for` without consuming
/// undelivered bytes when `pty` is absent).
pub(crate) fn drain_windows_pty_writes(sess: &mut LiveSession) {
    let mut scratch = Vec::new();
    while sess.machine.pty_write_len() != 0 {
        sess.machine.peek_pty_write(WIN_PTY_WRITE_CHUNK_BYTES, &mut scratch);
        let written_len = scratch.len();
        let res = match &sess.pty {
            Some(LivePty::Windows { bridge, eof: false, write_broken: false }) => bridge.write(scratch.clone()),
            _ => break,
        };
        if !apply_win_write_result(sess, written_len, res) {
            break;
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
                    Some(LivePty::Windows { bridge, eof: false, .. }) => {
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
