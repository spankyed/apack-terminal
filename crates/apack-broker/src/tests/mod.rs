mod core_e2e;
mod interactive_e2e;
mod lifecycle_e2e;
mod proxy_and_client;
mod regressions_and_images;

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
