//! Bounded-channel Windows ConPTY bridge with non-blocking off-shard teardown.
//!
//! ConPTY handles do not support `poll(2)`/`epoll` and closing a ConPTY master handle can block
//! synchronously while flushing the pseudo-console. This bridge isolates each Windows PTY behind
//! thin reader/writer threads communicating over bounded `sync_channel`s and drops the master
//! handle on a short-lived background thread so session shards never stall.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and Windows CI mutates it.

#![cfg(windows)]

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use apack_proto::types::{CellPx, GridSize};
use portable_pty::MasterPty;

use crate::OsError;
use crate::pty::{PtyMaster, to_pty_size};

/// Read chunk size for the ConPTY reader thread (64 KiB, matching Unix shard reads).
pub const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Bounded output queue depth between a ConPTY reader thread and its owning shard.
pub const OUTPUT_CHANNEL_DEPTH: usize = 16;

/// Maximum buffered input bytes allowed before rejecting additional input writes.
pub const MAX_INPUT_QUEUE_BYTES: usize = 256 * 1024;

/// Event produced by a ConPTY reader thread for the owning session shard.
#[derive(Debug)]
pub enum ConPtyOutput {
    /// Non-empty chunk of bytes read from the ConPTY output pipe.
    Data(Vec<u8>),
    /// Output pipe closed (EOF or error).
    Eof,
}

/// Flow-control pause gate shared between the shard and the ConPTY reader thread.
#[derive(Default)]
struct PauseGate {
    state: Mutex<PauseState>,
    changed: Condvar,
}

#[derive(Default)]
struct PauseState {
    paused: bool,
    draining: bool,
}

impl PauseGate {
    fn set_paused(&self, paused: bool) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if st.draining {
            return;
        }
        st.paused = paused;
        if !paused {
            self.changed.notify_all();
        }
    }

    /// Permanently unpauses the reader so it drains all remaining ConPTY output through true EOF.
    fn begin_drain(&self) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.draining = true;
        st.paused = false;
        self.changed.notify_all();
    }

    /// Blocks while paused (and not draining).
    fn wait_unpaused(&self) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while st.paused && !st.draining {
            st = self.changed.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// Bounded input queue shared between the shard and the ConPTY writer thread.
struct InputQueue {
    state: Mutex<InputState>,
    changed: Condvar,
}

struct InputState {
    chunks: std::collections::VecDeque<Vec<u8>>,
    buffered_bytes: usize,
    closed: bool,
    broken: bool,
}

impl InputQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(InputState {
                chunks: std::collections::VecDeque::new(),
                buffered_bytes: 0,
                closed: false,
                broken: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn push(&self, data: Vec<u8>) -> Result<(), OsError> {
        if data.is_empty() {
            return Ok(());
        }
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if st.closed || st.broken {
            return Err(OsError::io(
                "writing to conpty",
                io::Error::new(io::ErrorKind::BrokenPipe, "conpty input closed"),
            ));
        }
        if st.buffered_bytes.saturating_add(data.len()) > MAX_INPUT_QUEUE_BYTES {
            return Err(OsError::io(
                "writing to conpty",
                io::Error::new(io::ErrorKind::WouldBlock, "conpty input queue full"),
            ));
        }
        st.buffered_bytes += data.len();
        st.chunks.push_back(data);
        self.changed.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(chunk) = st.chunks.pop_front() {
                st.buffered_bytes = st.buffered_bytes.saturating_sub(chunk.len());
                return Some(chunk);
            }
            if st.closed {
                return None;
            }
            st = self.changed.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn mark_broken(&self) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.broken = true;
        st.chunks.clear();
        st.buffered_bytes = 0;
    }

    fn close(&self) {
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.closed = true;
        self.changed.notify_all();
    }
}

/// Non-blocking shard-facing facade over a Windows ConPTY master.
pub struct ConPtyBridge {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input: Arc<InputQueue>,
    pause: Arc<PauseGate>,
    output_rx: mpsc::Receiver<ConPtyOutput>,
    closed: AtomicBool,
}

impl ConPtyBridge {
    /// Wraps a spawned [`PtyMaster`] in bounded reader/writer threads, invoking `on_output_ready`
    /// whenever a new [`ConPtyOutput`] item is pushed into the output channel.
    pub fn start(master: PtyMaster, on_output_ready: impl Fn() + Send + 'static) -> Result<Self, OsError> {
        let raw_master = master.master;
        let mut reader = raw_master
            .try_clone_reader()
            .map_err(|err| OsError::io("cloning conpty reader", io::Error::other(err.to_string())))?;
        let mut writer = raw_master
            .take_writer()
            .map_err(|err| OsError::io("taking conpty writer", io::Error::other(err.to_string())))?;

        let (output_tx, output_rx) = mpsc::sync_channel::<ConPtyOutput>(OUTPUT_CHANNEL_DEPTH);
        let pause = Arc::new(PauseGate::default());
        let input = Arc::new(InputQueue::new());

        let reader_pause = Arc::clone(&pause);
        std::thread::Builder::new()
            .name("conpty-reader".to_owned())
            .spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK_BYTES];
                loop {
                    reader_pause.wait_unpaused();
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if output_tx.send(ConPtyOutput::Data(buf[..n].to_vec())).is_err() {
                                return;
                            }
                            on_output_ready();
                        }
                    }
                }
                let _ = output_tx.send(ConPtyOutput::Eof);
                on_output_ready();
            })
            .map_err(|err| OsError::io("spawning conpty reader thread", err))?;

        let writer_input = Arc::clone(&input);
        std::thread::Builder::new()
            .name("conpty-writer".to_owned())
            .spawn(move || {
                while let Some(chunk) = writer_input.pop() {
                    if writer.write_all(&chunk).is_err() {
                        writer_input.mark_broken();
                        break;
                    }
                }
            })
            .map_err(|err| OsError::io("spawning conpty writer thread", err))?;

        Ok(Self { master: Mutex::new(Some(raw_master)), input, pause, output_rx, closed: AtomicBool::new(false) })
    }

    /// Resizes the ConPTY dimensions if still open.
    pub fn resize(&self, size: GridSize, cell_px: CellPx) -> Result<(), OsError> {
        let guard = self.master.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(master) = guard.as_ref() {
            master
                .resize(to_pty_size(size, cell_px))
                .map_err(|err| OsError::io("resizing conpty", io::Error::other(err.to_string())))?;
        }
        Ok(())
    }

    /// Enqueues input bytes for the ConPTY writer thread without blocking.
    pub fn write(&self, data: Vec<u8>) -> Result<(), OsError> {
        self.input.push(data)
    }

    /// Pauses or resumes ConPTY reading for `BACKPRESSURE_PTY` flow control.
    pub fn set_paused(&self, paused: bool) {
        self.pause.set_paused(paused);
    }

    /// Drains the next available output chunk without blocking.
    pub fn try_recv_output(&self) -> Option<ConPtyOutput> {
        self.output_rx.try_recv().ok()
    }

    /// Releases the ConPTY master handle on a background thread so closing the pseudo-console
    /// flushes remaining output to the reader thread without stalling the calling shard thread.
    pub fn close_master_async(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.input.close();
        self.pause.begin_drain();
        let master = self.master.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(master) = master {
            let _ = std::thread::Builder::new().name("conpty-drop".to_owned()).spawn(move || drop(master));
        }
    }
}

impl Drop for ConPtyBridge {
    fn drop(&mut self) {
        self.close_master_async();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{PtySpawnConfig, spawn_pty};
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    #[test]
    fn pause_gate_blocks_unblocks_and_drains_permanently() {
        let gate = Arc::new(PauseGate::default());
        gate.set_paused(true);
        let gate_bg = Arc::clone(&gate);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            gate_bg.wait_unpaused();
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_millis(40)).is_err(), "paused gate must block");
        gate.set_paused(false);
        rx.recv_timeout(Duration::from_secs(2)).expect("unpaused gate must wake");

        // Once draining begins, `set_paused(true)` is ignored so reader never stalls before EOF.
        gate.begin_drain();
        gate.set_paused(true);
        gate.wait_unpaused();
    }

    #[test]
    fn input_queue_enforces_bounds_fifo_broken_and_close() {
        let q = InputQueue::new();
        q.push(Vec::new()).expect("empty push is a no-op");
        q.push(b"abc".to_vec()).expect("push 1");
        q.push(vec![0u8; MAX_INPUT_QUEUE_BYTES - 3]).expect("fill to exact cap");
        let full_err = q.push(vec![1u8]).expect_err("over cap fails");
        assert!(matches!(full_err, OsError::Io { source, .. } if source.kind() == io::ErrorKind::WouldBlock));

        assert_eq!(q.pop(), Some(b"abc".to_vec()));
        assert_eq!(q.pop().expect("big chunk").len(), MAX_INPUT_QUEUE_BYTES - 3);

        // Broken queue clears buffered data and rejects further writes with BrokenPipe.
        q.push(b"pending".to_vec()).expect("push before broken");
        q.mark_broken();
        let broken_err = q.push(b"x".to_vec()).expect_err("broken rejects");
        assert!(matches!(broken_err, OsError::Io { source, .. } if source.kind() == io::ErrorKind::BrokenPipe));

        q.close();
        assert_eq!(q.pop(), None);
        let closed_err = q.push(b"y".to_vec()).expect_err("closed rejects");
        assert!(matches!(closed_err, OsError::Io { source, .. } if source.kind() == io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn conpty_bridge_runs_interactive_write_pause_resume_and_drains_on_close() {
        let config = PtySpawnConfig {
            size: GridSize::new(80, 24).expect("grid"),
            cell_px: CellPx { width: 8, height: 16 },
            shell: Some("cmd.exe".to_owned()),
            args: vec![
                "/V:ON".to_owned(),
                "/C".to_owned(),
                "echo READY && set /p line= && echo APACK_WIN_GOT:!line!".to_owned(),
            ],
            env: vec![],
            unset: vec![],
            cwd: None,
        };
        let mut spawned = spawn_pty(&config).expect("spawn conpty");
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_bg = Arc::clone(&wakes);
        let bridge = ConPtyBridge::start(spawned.master, move || {
            wakes_bg.fetch_add(1, Ordering::Relaxed);
        })
        .expect("start bridge");

        bridge.resize(GridSize::new(90, 25).expect("grid"), CellPx { width: 8, height: 16 }).expect("resize");
        bridge.set_paused(true);
        bridge.set_paused(false);

        // Wait until `cmd.exe` prints `READY` so ConPTY has attached stdin before we write `HELLO\r\n`.
        let mut collected = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&collected).contains("READY") {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for READY: {:?}",
                String::from_utf8_lossy(&collected)
            );
            match bridge.try_recv_output() {
                Some(ConPtyOutput::Data(bytes)) => collected.extend_from_slice(&bytes),
                Some(ConPtyOutput::Eof) => panic!("unexpected ConPTY EOF before READY"),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }

        bridge.write(b"HELLO\r\n".to_vec()).expect("write input");

        let status = spawned.child.wait().expect("wait child");
        assert!(status.success());
        // Closing master flushes ConPTY output and terminates the reader stream without losing output.
        bridge.close_master_async();
        // Second call is idempotent.
        bridge.close_master_async();

        loop {
            assert!(Instant::now() < deadline, "timed out waiting for ConPTY EOF");
            match bridge.try_recv_output() {
                Some(ConPtyOutput::Data(bytes)) => collected.extend_from_slice(&bytes),
                Some(ConPtyOutput::Eof) => break,
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("APACK_WIN_GOT:HELLO"), "unexpected ConPTY output: {text:?}");
        assert!(wakes.load(Ordering::Relaxed) >= 1);
    }
}
