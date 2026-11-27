//! Reproducible throughput, latency, memory, and flood benchmark suite for `apack-terminal` v2.
//!
//! Run via `cargo xtask bench` (builds the release `apack-terminal` binary first so `pty_flood`
//! measures an isolated broker subprocess RSS) or directly via `cargo bench --bench v2_bench`.
//!
//! Supported flags:
//! - `--filter <substr>`: run only benchmarks whose name contains `<substr>`.
//! - `--quick`: reduced iteration/byte counts for fast smoke verification.
//! - `--json`: emit a machine-readable JSON summary block after the human-readable report.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use apack_broker::proxy::run_proxy_over;
use apack_broker::{BrokerClient, BrokerConfig, BrokerHandle, BrokerServer};
use apack_core::auth::AuthConfig;
use apack_core::vt::{CompressStep, VtEngine};
use apack_os::transport::connect_local_stream;
use apack_proto::frame::{DecodedFrame, FrameDecoder};
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SpawnSpec};
use apack_proto::types::{
    CellPx, FrameCredit, GridSize, LeaseKind, NonZeroStreamId, RequestId, ResizePolicy, StreamId, SubFlags,
    WaitCondition, WaitOutcome,
};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine};
use apack_wal::{
    EngineCheckpoint, KeyframeV2, RecordingRegistry, RolloverHint, VisualKeyframe, WalConfig, WalReader, WalWriter,
};

#[derive(Debug, Clone)]
struct BenchArgs {
    filter: Option<String>,
    quick: bool,
    json: bool,
}

fn parse_args() -> BenchArgs {
    let mut filter = None;
    let mut quick = false;
    let mut json = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--json" => json = true,
            "--filter" => {
                filter = it.next();
            }
            "--bench" => {}
            other if other.starts_with("--filter=") => {
                filter = Some(other["--filter=".len()..].to_owned());
            }
            other if !other.starts_with('-') && filter.is_none() => {
                filter = Some(other.to_owned());
            }
            _ => {}
        }
    }
    BenchArgs { filter, quick, json }
}

#[derive(Debug, Clone, Copy)]
struct LatencyStats {
    count: usize,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    max_us: f64,
}

fn summarize_us(mut samples: Vec<f64>) -> LatencyStats {
    assert!(!samples.is_empty(), "need at least one sample");
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    let pct = |p: f64| -> f64 {
        let idx = ((p * (n.saturating_sub(1) as f64)).round() as usize).min(n - 1);
        samples[idx]
    };
    LatencyStats { count: n, p50_us: pct(0.50), p95_us: pct(0.95), p99_us: pct(0.99), max_us: samples[n - 1] }
}

fn unique_endpoint(dir: &Path, label: &str) -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    #[cfg(unix)]
    {
        let _ = label;
        dir.join("p").join(format!("{seq}.sock")).display().to_string()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        format!(r"\\.\pipe\apack-v2-bench-{label}-{}-{seq}", std::process::id())
    }
}

// ---------------------------------------------------------------------------
// RAII guards & OS peak RSS sampling
// ---------------------------------------------------------------------------

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child live").id()
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(c) => c.try_wait(),
            None => Ok(None),
        }
    }

    fn shutdown(mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct RssSamplerGuard {
    stop: Arc<AtomicBool>,
    peak_bytes: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RssSamplerGuard {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_bytes = Arc::new(AtomicU64::new(read_process_peak_rss_bytes(pid).unwrap_or(0)));
        // On Linux (`/proc/{pid}/status` VmHWM) and Windows (`PeakWorkingSet64`), the kernel tracks
        // exact peak RSS natively, so we only need background polling on macOS/other Unix.
        #[cfg(all(unix, not(target_os = "linux")))]
        let thread = {
            let stop_c = Arc::clone(&stop);
            let peak_c = Arc::clone(&peak_bytes);
            Some(thread::spawn(move || {
                while !stop_c.load(Ordering::Relaxed) {
                    if let Some(rss) = read_process_peak_rss_bytes(pid) {
                        peak_c.fetch_max(rss, Ordering::Relaxed);
                    }
                    thread::sleep(Duration::from_millis(15));
                }
            }))
        };
        #[cfg(any(target_os = "linux", windows))]
        let thread: Option<thread::JoinHandle<()>> = None;
        Self { stop, peak_bytes, thread }
    }

    fn finish(mut self, pid: u32) -> u64 {
        if let Some(rss) = read_process_peak_rss_bytes(pid) {
            self.peak_bytes.fetch_max(rss, Ordering::Relaxed);
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.peak_bytes.load(Ordering::Relaxed)
    }
}

impl Drop for RssSamplerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Reads peak (or current, on macOS where `RssSamplerGuard` polls continuously) resident set size
/// in bytes for `pid`.
fn read_process_peak_rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kib.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kib.saturating_mul(1024))
    }
    #[cfg(windows)]
    {
        let script = format!("(Get-Process -Id {pid} -ErrorAction SilentlyContinue).PeakWorkingSet64");
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &script]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

// ---------------------------------------------------------------------------
// Benchmark 1: Sustained PTY flood throughput & peak broker RSS
// ---------------------------------------------------------------------------

struct PtyFloodResult {
    isolated_broker: bool,
    target_bytes: u64,
    pty_bytes_read: u64,
    elapsed_s: f64,
    throughput_mib_s: f64,
    frames_received: u64,
    peak_rss_mib: f64,
}

fn bench_pty_flood(quick: bool) -> PtyFloodResult {
    let target_bytes: u64 = if quick { 4 * 1024 * 1024 } else { 32 * 1024 * 1024 };
    let tmp = tempfile::tempdir().expect("tempdir");
    let endpoint = unique_endpoint(tmp.path(), "flood");

    // Prefer isolated release broker subprocess when available (set by `cargo xtask bench`).
    let bin_path = std::env::var_os("APACK_TERMINAL_BIN").map(std::path::PathBuf::from).filter(|p| p.is_file());

    let mut broker_child: Option<ChildGuard> = None;
    let mut inproc_server: Option<BrokerHandle> = None;

    let broker_pid: u32 = if let Some(bin) = bin_path {
        let child = Command::new(bin)
            .args(["serve", "--socket", &endpoint])
            .env_remove(apack_broker::config::AUTH_KEY_ENV)
            .env_remove(apack_os::transport::START_GRACE_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn apack-terminal serve");
        let guard = ChildGuard::new(child);
        let pid = guard.id();
        broker_child = Some(guard);
        pid
    } else {
        let mut cfg = BrokerConfig::for_socket(&endpoint);
        cfg.start_grace = Duration::from_secs(30);
        let server = BrokerServer::bind(cfg).expect("bind in-process broker").spawn().expect("spawn in-process broker");
        inproc_server = Some(server);
        std::process::id()
    };

    // Wait for broker socket readiness (bounded 5s deadline), failing immediately if the child exits.
    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let mut control = loop {
        if let Some(guard) = broker_child.as_mut()
            && let Ok(Some(status)) = guard.try_wait()
        {
            panic!("isolated apack-terminal broker exited prematurely with status {status}");
        }
        match BrokerClient::connect(&endpoint, &AuthConfig::None, "bench-control") {
            Ok(c) => break c,
            Err(e) => {
                assert!(Instant::now() < connect_deadline, "broker did not accept connection within 5s: {e}");
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let (lease_id, _) = control.acquire_lease(LeaseKind::BrokerImmediate).expect("acquire lease");

    // Connect the VIEW client BEFORE spawning so attach is synchronous and race-free.
    let mut view = BrokerClient::connect(&endpoint, &AuthConfig::None, "bench-view").expect("connect view");

    let sampler = RssSamplerGuard::start(broker_pid);

    // Bounded `cat` PTY flood generator that prints `READY` once the shell/PowerShell runtime has
    // booted and blocks on a single stdin line before writing byte 0, ensuring:
    //  1. Shell/PowerShell cold-start (and Windows ConPTY DSR init) is excluded from throughput.
    //  2. The live `VIEW | EVENTS` subscriber is attached and synced before the flood begins.
    let chunk_bytes: usize = 1024 * 1024;
    let repeats: u64 = (target_bytes / (chunk_bytes as u64)).max(1);
    let fixture_path = tmp.path().join("flood-1m.txt");
    {
        let mut block = Vec::with_capacity(chunk_bytes);
        let line = b"apack-v2-flood-bench-0123456789abcdef0123456789abcdef0123456789abcdef\n";
        while block.len() + line.len() <= chunk_bytes {
            block.extend_from_slice(line);
        }
        std::fs::write(&fixture_path, &block).expect("write flood fixture");
    }
    let env = vec![
        (String::from("APACK_BENCH_FILE"), fixture_path.display().to_string()),
        (String::from("APACK_BENCH_REPEATS"), repeats.to_string()),
    ];
    #[cfg(unix)]
    let argv = vec![
        String::from("/bin/sh"),
        String::from("-c"),
        String::from(
            "printf 'READY\\n'; IFS= read -r _; i=0; while [ \"$i\" -lt \"$APACK_BENCH_REPEATS\" ]; do cat \"$APACK_BENCH_FILE\"; i=$((i + 1)); done",
        ),
    ];
    #[cfg(windows)]
    let argv = vec![
        String::from("powershell"),
        String::from("-NoProfile"),
        String::from("-Command"),
        String::from(
            "[Console]::WriteLine('READY'); \
             [void][Console]::ReadLine(); \
             $bytes = [IO.File]::ReadAllBytes($env:APACK_BENCH_FILE); \
             $out = [Console]::OpenStandardOutput(); \
             $n = [int]$env:APACK_BENCH_REPEATS; \
             for ($i = 0; $i -lt $n; $i++) { $out.Write($bytes, 0, $bytes.Length) }; \
             $out.Flush()",
        ),
    ];

    let spec = SpawnSpec {
        argv,
        cwd: None,
        env,
        size: GridSize::new(120, 40).unwrap(),
        cell_px: CellPx { width: 8, height: 16 },
        scrollback_limit: 50_000,
        marks_key: None,
        resize_policy: ResizePolicy::LatestActive,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    };

    let (session_id, _) = control.spawn(spec).expect("spawn flood session");

    // Wait for the child runtime to finish cold-start and block on stdin (`READY`).
    let ready = control.wait(session_id, WaitCondition::Text(String::from("READY")), 15_000).expect("wait READY");
    assert_eq!(ready, WaitOutcome::Met, "flood shell did not print READY: {ready:?}");

    // Synchronously attach VIEW | EVENTS and ACK the initial full-sync frame before releasing the flood.
    let stream_id = NonZeroStreamId::new(1).unwrap();
    let credit = FrameCredit { max_frames: 8, max_bytes: 512 * 1024 };
    view.attach(
        session_id,
        stream_id,
        SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
        None,
        0,
        credit,
    )
    .expect("attach view");
    loop {
        let init = view.recv_frame().expect("recv initial sync frame");
        if let BrokerMsg::Frame(f) = init.msg {
            view.send_raw(stream_id.as_stream_id(), RequestId::PUSH, &ClientMsg::FrameAck { seq: f.seq, credit })
                .expect("ack initial sync frame");
            break;
        }
    }

    // Snapshot baseline metrics and release the gated flood via platform newline (`\r\n` on ConPTY).
    let metrics_before = control.metrics().expect("metrics before");
    #[cfg(unix)]
    let go_bytes: &[u8] = b"go\n";
    #[cfg(windows)]
    let go_bytes: &[u8] = b"go\r\n";

    let start = Instant::now();
    view.send_raw(stream_id.as_stream_id(), RequestId::PUSH, &ClientMsg::Write { data: go_bytes.to_vec() })
        .expect("send flood start signal");

    let mut frames_received = 0u64;
    let mut saw_exit = false;
    loop {
        match view.recv_frame() {
            Ok(frame) => match frame.msg {
                BrokerMsg::Frame(f) => {
                    frames_received += 1;
                    view.send_raw(
                        stream_id.as_stream_id(),
                        RequestId::PUSH,
                        &ClientMsg::FrameAck { seq: f.seq, credit },
                    )
                    .expect("send FrameAck");
                }
                BrokerMsg::Exited { .. } => {
                    saw_exit = true;
                    break;
                }
                BrokerMsg::StreamClosed { .. } => {
                    break;
                }
                _ => {}
            },
            Err(e) => panic!("view stream error during flood: {e}"),
        }
    }
    let elapsed = start.elapsed();
    let metrics_after = control.metrics().expect("metrics after");
    let peak_rss_bytes = sampler.finish(broker_pid);

    assert!(saw_exit, "flood session did not deliver Exited event");
    assert!(frames_received > 0, "expected live VIEW frames during flood, got 0");

    // Clean teardown: close view, release lease, shut down broker.
    let _ = view.send_fin();
    drop(view);
    let _ = control.release_lease(lease_id);
    let _ = control.send_fin();
    drop(control);
    let isolated_broker = broker_child.is_some();
    if let Some(guard) = broker_child {
        guard.shutdown();
    }
    if let Some(mut server) = inproc_server {
        let _ = server.shutdown();
    }

    let pty_bytes_read = metrics_after.total_pty_bytes.saturating_sub(metrics_before.total_pty_bytes);
    assert!(
        pty_bytes_read >= target_bytes / 2,
        "expected broker to read at least half of target flood ({target_bytes} B), got {pty_bytes_read} B"
    );
    let elapsed_s = elapsed.as_secs_f64().max(1e-9);
    let throughput_mib_s = (pty_bytes_read as f64) / (1024.0 * 1024.0) / elapsed_s;
    let peak_rss_mib = (peak_rss_bytes as f64) / (1024.0 * 1024.0);

    PtyFloodResult {
        isolated_broker,
        target_bytes,
        pty_bytes_read,
        elapsed_s,
        throughput_mib_s,
        frames_received,
        peak_rss_mib,
    }
}

// ---------------------------------------------------------------------------
// Benchmark 2: 60/120Hz TUI frame diffing + wire encode/decode time
// ---------------------------------------------------------------------------

struct TuiFramesResult {
    frames: usize,
    diff_pack: LatencyStats,
    wire_encode: LatencyStats,
    wire_decode: LatencyStats,
    total: LatencyStats,
    avg_frame_bytes: f64,
    max_frame_bytes: usize,
}

fn build_tui_frame_bytes(frame_idx: usize, cols: u16, rows: u16) -> Vec<u8> {
    let spinner = ['|', '/', '-', '\\'][frame_idx & 3];
    let mut out = Vec::with_capacity(4096);
    // Synchronized output + home cursor + styled status header.
    out.extend_from_slice(b"\x1b[?2026h\x1b[H\x1b[1;38;2;255;255;255;48;2;30;64;175m");
    let header = format!(" APACK TUI BENCH  frame={frame_idx:04}  [{spinner}]  cpu=42.7%  mem=128MiB ");
    out.extend_from_slice(header.as_bytes());
    if header.len() < usize::from(cols) {
        out.resize(out.len() + usize::from(cols) - header.len(), b' ');
    }
    out.extend_from_slice(b"\x1b[0m\r\n");

    // Update 12 active table/log rows with mixed SGR colors & highlights (realistic TUI diff).
    let active_rows = usize::from(rows).saturating_sub(2);
    for r in 0..12.min(active_rows) {
        let row_num = 2 + ((frame_idx + r * 3) % active_rows);
        let color = 31 + ((frame_idx + r) % 6);
        let line = format!(
            "\x1b[{row_num};1H\x1b[{color}m[{frame_idx:04}.{r:02}]\x1b[0m worker-{r:02} \x1b[1mOK\x1b[0m processed={} lat={}.{}ms\x1b[K",
            frame_idx * 17 + r,
            (frame_idx + r) % 9,
            (frame_idx * 7 + r) % 10
        );
        out.extend_from_slice(line.as_bytes());
    }

    // Bottom footer + cursor park + end synchronized output.
    let footer = format!(
        "\x1b[{rows};1H\x1b[38;2;203;213;225;48;2;15;23;42m F1 Help  F2 Filter  q Quit  tick={frame_idx:04}\x1b[K\x1b[0m\x1b[{};1H\x1b[?2026l",
        rows.saturating_sub(1)
    );
    out.extend_from_slice(footer.as_bytes());
    out
}

fn bench_tui_frames(quick: bool) -> TuiFramesResult {
    let cols = 120u16;
    let rows = 40u16;
    let warmup = if quick { 10 } else { 30 };
    let measured = if quick { 60 } else { 240 };

    let mut cfg = GhosttyConfig::new(GridSize::new(cols, rows).unwrap());
    cfg.scrollback_lines = 10_000;
    let mut engine = GhosttyEngine::new(cfg).expect("init GhosttyEngine");
    let mut events = Vec::new();
    let mut packed_rows = Vec::new();
    let mut wire_buf = Vec::with_capacity(64 * 1024);
    let mut decoder = FrameDecoder::new();

    // Seed initial full screen.
    let seed = build_tui_frame_bytes(0, cols, rows);
    engine.feed(&seed, &mut events);
    let _ = engine.take_dirty_viewport();
    events.clear();

    let mut diff_us = Vec::with_capacity(measured);
    let mut enc_us = Vec::with_capacity(measured);
    let mut dec_us = Vec::with_capacity(measured);
    let mut total_us = Vec::with_capacity(measured);
    let mut total_bytes: usize = 0;
    let mut max_frame_bytes: usize = 0;

    let stream_id = StreamId::from_raw(1);
    let mut last_palette_gen = engine.generations().palette;

    for i in 1..=(warmup + measured) {
        let payload = build_tui_frame_bytes(i, cols, rows);

        let t0 = Instant::now();
        events.clear();
        engine.feed(&payload, &mut events);
        let dirty = engine.take_dirty_viewport();
        let meta = engine.meta();
        let gens = engine.generations();
        packed_rows.clear();
        if dirty.full_dirty {
            let all_rows: Vec<u16> = (0..rows).collect();
            engine.extract_viewport_rows(&all_rows, &mut packed_rows);
        } else {
            engine.extract_viewport_rows(&dirty.dirty_rows, &mut packed_rows);
        }
        let palette = if gens.palette != last_palette_gen {
            last_palette_gen = gens.palette;
            Some(engine.palette_overrides())
        } else {
            None
        };
        let t1 = Instant::now();

        let row_count = packed_rows.len();
        assert!(row_count >= 12, "expected at least 12 dirty TUI rows per frame, got {row_count}");
        let update = FrameUpdate {
            seq: i as u64,
            epoch: meta.epoch,
            size: meta.size,
            first_active_line: meta.first_active_line,
            scrollback_lines: meta.scrollback_lines,
            scroll_delta: dirty.scroll_delta,
            cursor: meta.cursor,
            modes: meta.modes,
            palette,
            rows: std::mem::take(&mut packed_rows),
            images_gen: meta.images_gen,
            full_sync: dirty.full_dirty,
        };
        let msg = BrokerMsg::Frame(Box::new(update));
        wire_buf.clear();
        msg.encode_to_frame(stream_id, RequestId::PUSH, &mut wire_buf).expect("encode frame");
        let t2 = Instant::now();

        let (consumed, maybe_frame) = decoder.step(&wire_buf).expect("frame decode");
        assert_eq!(consumed, wire_buf.len());
        let Some(DecodedFrame::Frame { header, payload: frame_payload }) = maybe_frame else {
            panic!("expected Frame");
        };
        let decoded = BrokerMsg::decode(header.msg_type, &frame_payload).expect("BrokerMsg::decode");
        let BrokerMsg::Frame(ref decoded_frame) = decoded else {
            panic!("expected decoded Frame");
        };
        assert_eq!(decoded_frame.rows.len(), row_count);
        black_box(&decoded);
        let t3 = Instant::now();

        if i > warmup {
            let d = (t1 - t0).as_secs_f64() * 1e6;
            let e = (t2 - t1).as_secs_f64() * 1e6;
            let c = (t3 - t2).as_secs_f64() * 1e6;
            diff_us.push(d);
            enc_us.push(e);
            dec_us.push(c);
            total_us.push(d + e + c);
            total_bytes += wire_buf.len();
            max_frame_bytes = max_frame_bytes.max(wire_buf.len());
        }
    }

    TuiFramesResult {
        frames: measured,
        diff_pack: summarize_us(diff_us),
        wire_encode: summarize_us(enc_us),
        wire_decode: summarize_us(dec_us),
        total: summarize_us(total_us),
        avg_frame_bytes: (total_bytes as f64) / (measured as f64),
        max_frame_bytes,
    }
}

// ---------------------------------------------------------------------------
// Benchmark 3: Large scrollback search (55k lines) & cooperative compression
// ---------------------------------------------------------------------------

struct ScrollbackResult {
    total_lines: u64,
    expected_matches: usize,
    hot_bytes: u64,
    cold_bytes: u64,
    compression_ratio: f64,
    compress_steps: usize,
    compress_total_ms: f64,
    compress_pause: LatencyStats,
    hot_search_cs: LatencyStats,
    hot_search_ci: LatencyStats,
    cold_search_cs: LatencyStats,
    cold_search_ci: LatencyStats,
}

fn bench_scrollback_search_compress(quick: bool) -> ScrollbackResult {
    let line_count: usize = if quick { 6_000 } else { 55_000 };
    let search_iters: usize = if quick { 4 } else { 10 };
    let needle = "APACK_NEEDLE_MATCH_TOKEN";

    let mut cfg = GhosttyConfig::new(GridSize::new(120, 40).unwrap());
    cfg.scrollback_lines = line_count + 5_000;
    let mut engine = GhosttyEngine::new(cfg).expect("init GhosttyEngine");
    let mut events = Vec::new();

    // Feed styled build/log lines in 16 KiB batches.
    let mut batch = Vec::with_capacity(32 * 1024);
    let mut expected_matches = 0usize;
    for i in 0..line_count {
        if i % 1000 == 500 {
            expected_matches += 1;
            let s = format!(
                "\x1b[33m2026-11-27T02:00:{:02}.{:03}Z\x1b[0m \x1b[1;31mWARN\x1b[0m crates/apack-broker/src/shard.rs:{i} {needle} seq={i}\r\n",
                i % 60,
                i % 1000
            );
            batch.extend_from_slice(s.as_bytes());
        } else {
            let s = format!(
                "\x1b[32m2026-11-27T02:00:{:02}.{:03}Z\x1b[0m \x1b[36mINFO\x1b[0m compiling target crate=apack_core module=session_{:03} line={i} status=ok\r\n",
                i % 60,
                i % 1000,
                i % 128
            );
            batch.extend_from_slice(s.as_bytes());
        }
        if batch.len() >= 16 * 1024 {
            engine.feed(&batch, &mut events);
            events.clear();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        engine.feed(&batch, &mut events);
        events.clear();
    }

    let meta = engine.meta();
    let from = meta.oldest_line();
    let to = meta.total_lines();
    let total_lines = to.saturating_sub(from);
    let hot_bytes = engine.scrollback_bytes();

    let run_search = |eng: &GhosttyEngine, case_sensitive: bool| -> LatencyStats {
        let mut samples = Vec::with_capacity(search_iters);
        let mut matches = Vec::new();
        for _ in 0..search_iters {
            matches.clear();
            let t0 = Instant::now();
            let truncated = eng.search_lines(from, to, needle, case_sensitive, 2_000, &mut matches);
            let dt = t0.elapsed().as_secs_f64() * 1e6;
            assert!(!truncated, "unexpected search truncation");
            assert_eq!(matches.len(), expected_matches, "search match count mismatch");
            samples.push(dt);
        }
        summarize_us(samples)
    };

    let hot_search_cs = run_search(&engine, true);
    let hot_search_ci = run_search(&engine, false);

    // Cooperative idle compression: 1 page per step, recording every pause duration.
    let mut step_pauses_us = Vec::new();
    let compress_start = Instant::now();
    loop {
        let t0 = Instant::now();
        let progress = engine.compress_step(1);
        let dt = t0.elapsed().as_secs_f64() * 1e6;
        step_pauses_us.push(dt);
        if progress == CompressStep::Done {
            break;
        }
    }
    let compress_total_ms = compress_start.elapsed().as_secs_f64() * 1e3;
    let cold_bytes = engine.scrollback_bytes();
    let compression_ratio = if cold_bytes > 0 { (hot_bytes as f64) / (cold_bytes as f64) } else { 1.0 };

    let cold_search_cs = run_search(&engine, true);
    let cold_search_ci = run_search(&engine, false);

    ScrollbackResult {
        total_lines,
        expected_matches,
        hot_bytes,
        cold_bytes,
        compression_ratio,
        compress_steps: step_pauses_us.len(),
        compress_total_ms,
        compress_pause: summarize_us(step_pauses_us),
        hot_search_cs,
        hot_search_ci,
        cold_search_cs,
        cold_search_ci,
    }
}

// ---------------------------------------------------------------------------
// Benchmark 4: WAL streaming write, rollover, and seek-to-timestamp latency
// ---------------------------------------------------------------------------

struct WalBenchResult {
    bytes_written: u64,
    records_written: usize,
    write_mib_s: f64,
    write_records_s: f64,
    rollovers: usize,
    rollover: LatencyStats,
    open_us: f64,
    seek_samples: usize,
    seek: LatencyStats,
}

fn capture_keyframe(engine: &GhosttyEngine, size: GridSize, cell_px: CellPx) -> KeyframeV2 {
    let meta = engine.meta();
    let all_rows: Vec<u16> = (0..size.rows.get()).collect();
    let mut primary_rows = Vec::with_capacity(all_rows.len());
    engine.extract_viewport_rows(&all_rows, &mut primary_rows);
    let visual = VisualKeyframe {
        size,
        cell_px,
        active_alt_screen: meta.modes.alt_screen,
        cursor: meta.cursor,
        modes: meta.modes,
        palette: engine.palette_overrides(),
        primary_rows,
        alt_rows: vec![],
        images: engine.images(),
        marks_key: None,
        marks_last_seq: 0,
        marks_refused: 0,
        commands: vec![],
    };
    let snapshot_bytes =
        engine.export_snapshot(apack_wal::MAX_ENGINE_SNAPSHOT_BYTES).expect("GhosttyEngine::export_snapshot");
    let engine_checkpoint = Some(EngineCheckpoint { engine_abi_hash: apack_broker::engine_abi_hash(), snapshot_bytes });
    KeyframeV2 { visual, engine: engine_checkpoint }
}

fn bench_wal_stream_rollover_seek(quick: bool) -> WalBenchResult {
    let total_bytes: usize = if quick { 1024 * 1024 } else { 8 * 1024 * 1024 };
    let chunk_size: usize = 4096;
    let records = total_bytes / chunk_size;
    let segment_target: u64 = if quick { 256 * 1024 } else { 512 * 1024 };
    let seek_count: usize = if quick { 24 } else { 64 };

    let tmp = tempfile::tempdir().expect("tempdir");
    let rec_dir = tmp.path().join("wal-bench");
    let registry = RecordingRegistry::new();
    let wal_cfg =
        WalConfig { segment_target_bytes: segment_target, budget_bytes: 64 * 1024 * 1024, ..WalConfig::default() };

    let size = GridSize::new(120, 40).unwrap();
    let cell_px = CellPx { width: 8, height: 16 };
    let mut engine = GhosttyEngine::new(GhosttyConfig::new(size)).expect("GhosttyEngine");
    let mut events = Vec::new();
    engine.feed(b"\x1b[1;32mWAL benchmark session started\x1b[0m\r\n", &mut events);

    let mut writer = WalWriter::open_or_create(&rec_dir, [0x42; 16], 1_700_000_000_000, wal_cfg, registry.clone())
        .expect("open WAL");
    let mut at_ms: u64 = 1_000;
    let initial_kf = capture_keyframe(&engine, size, cell_px);
    writer.start_segment_with_keyframe(at_ms, &initial_kf).expect("start initial segment");

    // Realistic repetitive-yet-varying 4 KiB PTY payload chunk.
    let mut chunk = Vec::with_capacity(chunk_size);
    while chunk.len() < chunk_size {
        chunk.extend_from_slice(b"\x1b[36m[wal-stream]\x1b[0m payload-line-0123456789abcdef0123456789abcdef\r\n");
    }
    chunk.truncate(chunk_size);

    let mut rollover_us = Vec::new();
    let mut pure_write_dur = Duration::ZERO;

    for i in 0..records {
        at_ms += 10;
        let tw0 = Instant::now();
        let hint = writer.record_output(at_ms, &chunk, true).expect("record_output");
        pure_write_dur += tw0.elapsed();

        if hint == RolloverHint::NeedKeyframe {
            // Feed realistic screen content into the VT engine before capturing the dual-layer keyframe.
            engine.feed(&chunk, &mut events);
            let line = format!("segment boundary tick={i}\r\n");
            engine.feed(line.as_bytes(), &mut events);
            events.clear();
            let tr0 = Instant::now();
            let kf = capture_keyframe(&engine, size, cell_px);
            assert!(kf.engine.is_some(), "expected dual-layer EngineCheckpoint");
            writer.start_segment_with_keyframe(at_ms, &kf).expect("rollover segment");
            rollover_us.push(tr0.elapsed().as_secs_f64() * 1e6);
        }
    }
    writer.seal_active().expect("seal active");
    let segment_count = writer.segment_count();
    assert!(segment_count >= 2, "expected multiple WAL segments, got {segment_count}");
    drop(writer);

    let t_open = Instant::now();
    let reader = WalReader::open(&rec_dir, &registry).expect("WalReader::open");
    let open_us = t_open.elapsed().as_secs_f64() * 1e6;
    let (first_ms, last_ms) = reader.span().expect("WAL span");
    let span = last_ms.saturating_sub(first_ms).max(1);

    let mut seek_us = Vec::with_capacity(seek_count);
    for i in 0..seek_count {
        let target_ms = first_ms + (span * (i as u64)) / ((seek_count.saturating_sub(1).max(1)) as u64);
        let ts0 = Instant::now();
        let slice = reader.records_for_moment(target_ms).expect("records_for_moment").expect("slice");
        black_box(&slice);
        seek_us.push(ts0.elapsed().as_secs_f64() * 1e6);
    }

    let write_s = pure_write_dur.as_secs_f64().max(1e-9);
    WalBenchResult {
        bytes_written: (records * chunk_size) as u64,
        records_written: records,
        write_mib_s: ((records * chunk_size) as f64) / (1024.0 * 1024.0) / write_s,
        write_records_s: (records as f64) / write_s,
        rollovers: rollover_us.len(),
        rollover: summarize_us(rollover_us),
        open_us,
        seek_samples: seek_count,
        seek: summarize_us(seek_us),
    }
}

// ---------------------------------------------------------------------------
// Benchmark 5: `proxy` latency overhead under simulated RTT
// ---------------------------------------------------------------------------

struct DelayedPipeState {
    queue: VecDeque<(Instant, Vec<u8>)>,
    buf: Vec<u8>,
    buf_pos: usize,
    closed: bool,
    actual_hold_ns: u64,
}

struct DelayedPipeWriter {
    state: Arc<(Mutex<DelayedPipeState>, Condvar)>,
    delay: Duration,
}

struct DelayedPipeReader {
    state: Arc<(Mutex<DelayedPipeState>, Condvar)>,
}

fn delayed_pipe(delay: Duration) -> (DelayedPipeWriter, DelayedPipeReader, Arc<(Mutex<DelayedPipeState>, Condvar)>) {
    let state = Arc::new((
        Mutex::new(DelayedPipeState {
            queue: VecDeque::new(),
            buf: Vec::new(),
            buf_pos: 0,
            closed: false,
            actual_hold_ns: 0,
        }),
        Condvar::new(),
    ));
    (DelayedPipeWriter { state: Arc::clone(&state), delay }, DelayedPipeReader { state: Arc::clone(&state) }, state)
}

impl Write for DelayedPipeWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        if guard.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "delayed pipe closed"));
        }
        guard.queue.push_back((Instant::now() + self.delay, data.to_vec()));
        cv.notify_all();
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for DelayedPipeWriter {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        guard.closed = true;
        cv.notify_all();
    }
}

impl Drop for DelayedPipeReader {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        guard.closed = true;
        cv.notify_all();
    }
}

impl Read for DelayedPipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().unwrap();
        loop {
            if guard.buf_pos < guard.buf.len() {
                let avail = guard.buf.len() - guard.buf_pos;
                let n = avail.min(out.len());
                out[..n].copy_from_slice(&guard.buf[guard.buf_pos..guard.buf_pos + n]);
                guard.buf_pos += n;
                return Ok(n);
            }
            if let Some(&(deliver_at, _)) = guard.queue.front() {
                let now = Instant::now();
                if now >= deliver_at {
                    let (_, chunk) = guard.queue.pop_front().unwrap();
                    guard.buf = chunk;
                    guard.buf_pos = 0;
                    continue;
                }
                let wait_for = deliver_at.saturating_duration_since(now);
                let sleep_start = Instant::now();
                let (next_guard, _) = cv.wait_timeout(guard, wait_for).unwrap();
                guard = next_guard;
                let slept = u64::try_from(sleep_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                guard.actual_hold_ns = guard.actual_hold_ns.saturating_add(slept);
                continue;
            }
            if guard.closed {
                return Ok(0);
            }
            guard = cv.wait(guard).unwrap();
        }
    }
}

struct DuplexChannel {
    reader: DelayedPipeReader,
    writer: DelayedPipeWriter,
}

impl Read for DuplexChannel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for DuplexChannel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct ProxyRttEntry {
    label: &'static str,
    injected_rtt_ms: f64,
    observed: LatencyStats,
    p50_overhead_us: f64,
}

fn measure_proxied_rtt(endpoint: &str, one_way: Duration, iters: usize) -> (LatencyStats, LatencyStats) {
    let (client_to_proxy_w, client_to_proxy_r, up_state) = delayed_pipe(one_way);
    let (proxy_to_client_w, proxy_to_client_r, down_state) = delayed_pipe(one_way);

    let ep = endpoint.to_owned();
    let proxy_thread =
        thread::spawn(move || run_proxy_over(client_to_proxy_r, proxy_to_client_w, || connect_local_stream(&ep)));

    let transport = DuplexChannel { reader: proxy_to_client_r, writer: client_to_proxy_w };
    let mut client = BrokerClient::handshake(transport, &AuthConfig::None, "bench-proxy").expect("proxy handshake");

    // Warm up 2 RPCs.
    for _ in 0..2 {
        let _ = client.metrics().expect("warmup metrics");
    }

    let mut samples_us = Vec::with_capacity(iters);
    let mut overhead_us = Vec::with_capacity(iters);
    for _ in 0..iters {
        let up_before = up_state.0.lock().unwrap().actual_hold_ns;
        let down_before = down_state.0.lock().unwrap().actual_hold_ns;
        let t0 = Instant::now();
        let snap = client.metrics().expect("proxied metrics");
        let total_us = t0.elapsed().as_secs_f64() * 1e6;
        black_box(snap);
        let up_held = up_state.0.lock().unwrap().actual_hold_ns.saturating_sub(up_before);
        let down_held = down_state.0.lock().unwrap().actual_hold_ns.saturating_sub(down_before);
        let actual_pipe_us = ((up_held.saturating_add(down_held)) as f64) / 1e3;
        samples_us.push(total_us);
        overhead_us.push((total_us - actual_pipe_us).max(0.0));
    }

    let _ = client.send_fin();
    drop(client);
    proxy_thread.join().expect("proxy thread panicked").expect("proxy clean exit");
    (summarize_us(samples_us), summarize_us(overhead_us))
}

fn bench_proxy_rtt_overhead(quick: bool) -> Vec<ProxyRttEntry> {
    let iters = if quick { 10 } else { 30 };
    let tmp = tempfile::tempdir().expect("tempdir");
    let endpoint = unique_endpoint(tmp.path(), "proxy-rtt");

    let mut cfg = BrokerConfig::for_socket(&endpoint);
    cfg.start_grace = Duration::from_secs(30);
    let mut server = BrokerServer::bind(cfg).expect("bind broker").spawn().expect("spawn broker");

    // Hold a BrokerImmediate lease on a dedicated keepalive connection so the broker stays up
    // across all proxy scenarios even when individual benchmark clients disconnect.
    let mut keepalive = BrokerClient::connect(&endpoint, &AuthConfig::None, "bench-keepalive").expect("keepalive");
    let (lease_id, _) = keepalive.acquire_lease(LeaseKind::BrokerImmediate).expect("acquire keepalive lease");

    // 1. Direct local socket baseline.
    let mut direct = BrokerClient::connect(&endpoint, &AuthConfig::None, "bench-direct").expect("direct connect");
    for _ in 0..5 {
        let _ = direct.metrics().expect("direct warmup");
    }
    let mut direct_us = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let snap = direct.metrics().expect("direct metrics");
        black_box(snap);
        direct_us.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    let _ = direct.send_fin();
    drop(direct);
    let direct_stats = summarize_us(direct_us);

    // 2. Proxied at 0 ms, 20 ms, and 60 ms simulated RTT.
    let mut results = vec![ProxyRttEntry {
        label: "direct_local_socket",
        injected_rtt_ms: 0.0,
        observed: direct_stats,
        p50_overhead_us: 0.0,
    }];

    let scenarios: &[(&'static str, u64)] = if quick {
        &[("proxy_0ms_rtt", 0), ("proxy_20ms_rtt", 10)]
    } else {
        &[("proxy_0ms_rtt", 0), ("proxy_20ms_rtt", 10), ("proxy_60ms_rtt", 30)]
    };

    for &(label, one_way_ms) in scenarios {
        let (observed, non_pipe) = measure_proxied_rtt(&endpoint, Duration::from_millis(one_way_ms), iters);
        let injected_rtt_ms = (one_way_ms * 2) as f64;
        let p50_overhead_us = (non_pipe.p50_us - direct_stats.p50_us).max(0.0);
        results.push(ProxyRttEntry { label, injected_rtt_ms, observed, p50_overhead_us });
    }

    let _ = keepalive.release_lease(lease_id);
    let _ = keepalive.send_fin();
    drop(keepalive);
    let _ = server.shutdown();
    results
}

// ---------------------------------------------------------------------------
// Entry point & formatted reporting
// ---------------------------------------------------------------------------

fn should_run(args: &BenchArgs, name: &str) -> bool {
    args.filter.as_deref().is_none_or(|f| name.contains(f))
}

fn main() {
    let args = parse_args();
    println!("=== apack-terminal v2 Benchmark Suite ({}) ===", if args.quick { "quick" } else { "full" });
    println!("os={} arch={}", std::env::consts::OS, std::env::consts::ARCH);
    println!();

    let mut json_entries: Vec<String> = Vec::new();

    if should_run(&args, "pty_flood") {
        let r = bench_pty_flood(args.quick);
        println!("[1/5] pty_flood (isolated_broker={})", r.isolated_broker);
        println!(
            "  target={:.1} MiB | broker_read={:.2} MiB | elapsed={:.3}s | throughput={:.1} MiB/s",
            (r.target_bytes as f64) / (1024.0 * 1024.0),
            (r.pty_bytes_read as f64) / (1024.0 * 1024.0),
            r.elapsed_s,
            r.throughput_mib_s
        );
        println!("  live_view_frames={} | peak_broker_rss={:.2} MiB", r.frames_received, r.peak_rss_mib);
        println!();
        json_entries.push(format!(
            "\"pty_flood\":{{\"isolated_broker\":{},\"pty_bytes_read\":{},\"elapsed_s\":{:.4},\"throughput_mib_s\":{:.2},\"frames_received\":{},\"peak_rss_mib\":{:.2}}}",
            r.isolated_broker, r.pty_bytes_read, r.elapsed_s, r.throughput_mib_s, r.frames_received, r.peak_rss_mib
        ));
    }

    if should_run(&args, "tui_frames") {
        let r = bench_tui_frames(args.quick);
        let pct_60hz = (r.total.p99_us / 16_666.7) * 100.0;
        let pct_120hz = (r.total.p99_us / 8_333.3) * 100.0;
        println!("[2/5] tui_frames (120x40, {} frames)", r.frames);
        println!(
            "  diff+pack:   p50={:.1}us  p95={:.1}us  p99={:.1}us  max={:.1}us",
            r.diff_pack.p50_us, r.diff_pack.p95_us, r.diff_pack.p99_us, r.diff_pack.max_us
        );
        println!(
            "  wire_encode: p50={:.1}us  p95={:.1}us  p99={:.1}us  max={:.1}us  (avg={:.0}B, max={}B)",
            r.wire_encode.p50_us,
            r.wire_encode.p95_us,
            r.wire_encode.p99_us,
            r.wire_encode.max_us,
            r.avg_frame_bytes,
            r.max_frame_bytes
        );
        println!(
            "  wire_decode: p50={:.1}us  p95={:.1}us  p99={:.1}us  max={:.1}us",
            r.wire_decode.p50_us, r.wire_decode.p95_us, r.wire_decode.p99_us, r.wire_decode.max_us
        );
        println!(
            "  total/frame: p50={:.1}us  p95={:.1}us  p99={:.1}us  max={:.1}us  (p99 = {:.2}% of 60Hz, {:.2}% of 120Hz)",
            r.total.p50_us, r.total.p95_us, r.total.p99_us, r.total.max_us, pct_60hz, pct_120hz
        );
        println!();
        json_entries.push(format!(
            "\"tui_frames\":{{\"frames\":{},\"diff_pack_p50_us\":{:.1},\"diff_pack_p99_us\":{:.1},\"encode_p50_us\":{:.1},\"decode_p50_us\":{:.1},\"total_p50_us\":{:.1},\"total_p99_us\":{:.1},\"avg_frame_bytes\":{:.1}}}",
            r.frames, r.diff_pack.p50_us, r.diff_pack.p99_us, r.wire_encode.p50_us, r.wire_decode.p50_us, r.total.p50_us, r.total.p99_us, r.avg_frame_bytes
        ));
    }

    if should_run(&args, "scrollback_search_compress") {
        let r = bench_scrollback_search_compress(args.quick);
        println!("[3/5] scrollback_search_compress ({} lines, {} matches)", r.total_lines, r.expected_matches);
        println!(
            "  memory: hot={:.2} MiB -> cold={:.2} MiB (ratio={:.2}x)",
            (r.hot_bytes as f64) / (1024.0 * 1024.0),
            (r.cold_bytes as f64) / (1024.0 * 1024.0),
            r.compression_ratio
        );
        println!(
            "  compress_step(1): steps={} total={:.2}ms | pause p50={:.1}us p95={:.1}us max={:.1}us",
            r.compress_steps,
            r.compress_total_ms,
            r.compress_pause.p50_us,
            r.compress_pause.p95_us,
            r.compress_pause.max_us
        );
        println!(
            "  hot_search:  case_sens p50={:.2}ms (max={:.2}ms) | case_insens p50={:.2}ms (max={:.2}ms)",
            r.hot_search_cs.p50_us / 1e3,
            r.hot_search_cs.max_us / 1e3,
            r.hot_search_ci.p50_us / 1e3,
            r.hot_search_ci.max_us / 1e3
        );
        println!(
            "  cold_search: case_sens p50={:.2}ms (max={:.2}ms) | case_insens p50={:.2}ms (max={:.2}ms)",
            r.cold_search_cs.p50_us / 1e3,
            r.cold_search_cs.max_us / 1e3,
            r.cold_search_ci.p50_us / 1e3,
            r.cold_search_ci.max_us / 1e3
        );
        println!();
        json_entries.push(format!(
            "\"scrollback_search_compress\":{{\"lines\":{},\"hot_bytes\":{},\"cold_bytes\":{},\"ratio\":{:.2},\"compress_steps\":{},\"compress_total_ms\":{:.2},\"pause_p50_us\":{:.1},\"pause_max_us\":{:.1},\"hot_search_cs_p50_ms\":{:.2},\"hot_search_ci_p50_ms\":{:.2},\"cold_search_cs_p50_ms\":{:.2},\"cold_search_ci_p50_ms\":{:.2}}}",
            r.total_lines, r.hot_bytes, r.cold_bytes, r.compression_ratio, r.compress_steps, r.compress_total_ms, r.compress_pause.p50_us, r.compress_pause.max_us, r.hot_search_cs.p50_us / 1e3, r.hot_search_ci.p50_us / 1e3, r.cold_search_cs.p50_us / 1e3, r.cold_search_ci.p50_us / 1e3
        ));
    }

    if should_run(&args, "wal_stream_rollover_seek") {
        let r = bench_wal_stream_rollover_seek(args.quick);
        println!(
            "[4/5] wal_stream_rollover_seek ({:.1} MiB, {} records, {} rollovers)",
            (r.bytes_written as f64) / (1024.0 * 1024.0),
            r.records_written,
            r.rollovers
        );
        println!("  streaming_write: {:.1} MiB/s ({:.0} records/s)", r.write_mib_s, r.write_records_s);
        println!(
            "  rollover+kf+sync: p50={:.2}ms  p95={:.2}ms  max={:.2}ms",
            r.rollover.p50_us / 1e3,
            r.rollover.p95_us / 1e3,
            r.rollover.max_us / 1e3
        );
        println!(
            "  open_footer={:.1}us | seek ({} pts): p50={:.1}us  p95={:.1}us  p99={:.1}us  max={:.1}us",
            r.open_us, r.seek_samples, r.seek.p50_us, r.seek.p95_us, r.seek.p99_us, r.seek.max_us
        );
        println!();
        json_entries.push(format!(
            "\"wal_stream_rollover_seek\":{{\"bytes_written\":{},\"records\":{},\"write_mib_s\":{:.1},\"rollovers\":{},\"rollover_p50_ms\":{:.2},\"rollover_max_ms\":{:.2},\"open_us\":{:.1},\"seek_p50_us\":{:.1},\"seek_p99_us\":{:.1}}}",
            r.bytes_written, r.records_written, r.write_mib_s, r.rollovers, r.rollover.p50_us / 1e3, r.rollover.max_us / 1e3, r.open_us, r.seek.p50_us, r.seek.p99_us
        ));
    }

    if should_run(&args, "proxy_rtt_overhead") {
        let entries = bench_proxy_rtt_overhead(args.quick);
        println!("[5/5] proxy_rtt_overhead ({} RPCs/scenario)", entries[0].observed.count);
        let mut arr = Vec::new();
        for e in &entries {
            println!(
                "  {:20} injected={:4.0}ms | p50={:8.2}ms  p95={:8.2}ms  max={:8.2}ms | p50_overhead={:+.1}us",
                e.label,
                e.injected_rtt_ms,
                e.observed.p50_us / 1e3,
                e.observed.p95_us / 1e3,
                e.observed.max_us / 1e3,
                e.p50_overhead_us
            );
            arr.push(format!(
                "{{\"label\":\"{}\",\"injected_rtt_ms\":{:.1},\"p50_us\":{:.1},\"p95_us\":{:.1},\"p50_overhead_us\":{:.1}}}",
                e.label, e.injected_rtt_ms, e.observed.p50_us, e.observed.p95_us, e.p50_overhead_us
            ));
        }
        println!();
        json_entries.push(format!("\"proxy_rtt_overhead\":[{}]", arr.join(",")));
    }

    if args.json {
        println!("JSON={{{}}}", json_entries.join(","));
    }
}
