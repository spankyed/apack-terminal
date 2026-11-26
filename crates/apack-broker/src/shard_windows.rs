//! Windows ConPTY shard I/O edge (`#[cfg(windows)]`-only; mutated on Windows CI).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use apack_os::pty_windows::{ConPtyBridge, ConPtyOutput};
use apack_proto::types::SessionId;

use crate::shard::{LivePty, PTY_READ_QUANTUM_BYTES, ShardCmd, ShardRunner};

const WIN_PTY_WRITE_CHUNK_BYTES: usize = 65_536;

/// Drains up to `budget` bytes of ready ConPTY output from `bridge`, invoking `on_bytes` for each
/// chunk. Returns `true` if `ConPtyOutput::Eof` was observed.
pub(crate) fn drain_conpty_quantum(bridge: &ConPtyBridge, mut budget: usize, mut on_bytes: impl FnMut(&[u8])) -> bool {
    while budget > 0 {
        match bridge.try_recv_output() {
            Some(ConPtyOutput::Bytes(bytes)) => {
                budget = budget.saturating_sub(bytes.len());
                on_bytes(&bytes);
            }
            Some(ConPtyOutput::Eof) => return true,
            None => break,
        }
    }
    false
}

/// Sleeps on `cond` until either `mailbox` becomes non-empty or `timeout` elapses.
pub(crate) fn wait_for_win_mailbox(mailbox: &Mutex<VecDeque<ShardCmd>>, cond: &Condvar, timeout: Duration) {
    let q = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    if q.is_empty() {
        let _ = cond.wait_timeout(q, timeout);
    }
}

/// Writes pending PTY input bytes into a Windows `ConPtyBridge`.
pub(crate) fn drain_windows_pty_writes(sess: &mut crate::shard::LiveSession) {
    if sess.machine.pty_write_len() > 0 {
        let data = sess.machine.drain_pty_write(WIN_PTY_WRITE_CHUNK_BYTES);
        if let Some(LivePty::Windows { bridge, eof: false }) = &sess.pty {
            let _ = bridge.write(data);
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
        }
    }
}
