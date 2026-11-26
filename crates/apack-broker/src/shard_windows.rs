//! Windows ConPTY shard I/O edge (`#[cfg(windows)]`-only; mutated on Windows CI).

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use apack_os::pty_windows::{ConPtyBridge, ConPtyOutput};
use apack_proto::types::SessionId;

use crate::shard::{LivePty, LiveSession, PTY_READ_QUANTUM_BYTES, ShardMailboxState, ShardRunner};

const WIN_PTY_WRITE_CHUNK_BYTES: usize = 65_536;

/// Drains up to `budget` bytes of ready ConPTY output from `bridge`, invoking `on_bytes` for each
/// chunk. Returns `true` if `ConPtyOutput::Eof` was observed.
pub(crate) fn drain_conpty_quantum(bridge: &ConPtyBridge, mut budget: usize, mut on_bytes: impl FnMut(&[u8])) -> bool {
    while budget != 0 {
        match bridge.try_recv_output() {
            Some(ConPtyOutput::Data(bytes)) => {
                budget = budget.saturating_sub(bytes.len());
                on_bytes(&bytes);
            }
            Some(ConPtyOutput::Eof) => return true,
            None => break,
        }
    }
    false
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
    pub(crate) fn drain_windows_ptys(&mut self) {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for session_id in ids {
            let mut chunks = Vec::new();
            let hit_eof = match self.sessions.get(&session_id).and_then(|s| s.pty.as_ref()) {
                Some(LivePty::Windows { bridge, eof: false }) => {
                    drain_conpty_quantum(bridge, PTY_READ_QUANTUM_BYTES, |bytes| chunks.push(bytes.to_vec()))
                }
                _ => false,
            };
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
    }
}
