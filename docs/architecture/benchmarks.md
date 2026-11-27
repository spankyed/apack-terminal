# Benchmarks — apack-terminal v2

`apack-terminal` ships a zero-external-dependency, reproducible release benchmark harness
(`benches/v2_bench.rs`) invoked via `cargo xtask bench`. It measures the five core hot paths end-to-end
against real `libghostty-vt`, real OS PTYs, real segmented WAL files, and a real isolated broker process.

## Running the Suite

```sh
# Full release suite (builds target/release/apack-terminal first for isolated broker RSS):
cargo xtask bench

# Run a single benchmark by substring filter:
cargo xtask bench --filter tui_frames

# Emit machine-readable JSON summary alongside the formatted table:
cargo xtask bench --json

# Fast reduced-scale smoke run:
cargo xtask bench --quick
```

Browser-side Canvas 2D vs WebGL2 glyph atlas timing lives separately in `view/bench/painter-timing.ts`
(`node view/bench/run-painter-timing.mjs`).

## Baseline Results

Measured on **Apple M1 Pro (10 cores), macOS 26.3.1 (`aarch64`), `rustc 1.99.1`, `--release`** on the
`v2` integration branch (`c3d0aab` + `v2_bench`).

### 1. Sustained PTY Flood Throughput & Peak Broker RSS (`pty_flood`)

Streams `32.0 MiB` of 70-byte lines through a real OS PTY (`cat` loop on a 1 MiB fixture file, zero
pipe/`SIGPIPE` races) into an isolated `apack-terminal serve` broker subprocess (`scrollback_limit = 50,000`)
while a second client connection holds a live `SUB_VIEW` attachment acknowledging `FrameUpdate` packets
(`FrameCredit { max_frames: 8, max_bytes: 512 KiB }`) and an external sampler polls broker RSS every 15 ms.

| Metric | Measured Baseline |
| --- | ---: |
| Target flood payload | `32.0 MiB` |
| Broker PTY bytes ingested (incl. PTY `ONLCR` expansion) | `32.46 MiB` (`34,032,971 B`) |
| Wall time to clean exit | `1.355 s` |
| Sustained PTY ingestion throughput | **`23.96 MiB/s`** |
| Live differential `FrameUpdate`s delivered & ACKed | `4,476` frames |
| Peak isolated broker RSS (`50,000`-line scrollback cap) | **`58.86 MiB`** |

### 2. 60/120Hz TUI Frame Diffing + Wire Encode/Decode (`tui_frames`)

Drives a `120 × 40` `GhosttyEngine` across `240` measured frames (after `30` warmup frames) with
synchronized-output (`DEC 2026`) redraws: 24-bit RGB status header, 12 colored updating content rows,
footer bar, and cursor movement. Times `feed` + dirty viewport extraction + `BrokerMsg::Frame` wire encode
+ `FrameDecoder` / `BrokerMsg::decode`.

| Stage (`120 × 40`, `240` frames) | p50 | p95 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| VT feed + dirty diff + `PackedRow` pack | `54.5 µs` | `58.1 µs` | `64.9 µs` | `78.7 µs` |
| Wire encode (`BrokerMsg::Frame`, avg `2,355 B`, max `2,449 B`) | `1.2 µs` | `1.7 µs` | `1.9 µs` | `8.8 µs` |
| Wire decode (`FrameDecoder` + `BrokerMsg::decode`) | `2.7 µs` | `2.9 µs` | `3.2 µs` | `4.0 µs` |
| **Combined total per frame** | **`58.4 µs`** | **`62.7 µs`** | **`70.0 µs`** | **`84.9 µs`** |

At p99 (`70.0 µs`), the broker VT-diff + wire round-trip consumes **0.42% of a 60 Hz frame budget
(`16.67 ms`)** and **0.84% of a 120 Hz frame budget (`8.33 ms`)**.

### 3. Large Scrollback Search & Cooperative Idle Compression (`scrollback_search_compress`)

Populates a `120 × 40` `GhosttyEngine` (`scrollback_limit = 60,000`) with **`55,001` lines** of realistic
ANSI-styled build output containing `55` embedded match tokens, measures full-scrollback search before
compression, runs cooperative `compress_step(1)` to completion while recording every single-step pause,
and re-measures full-scrollback search on the compressed cold scrollback.

| Metric (`55,001` lines, `55` matches) | Measured Baseline |
| --- | ---: |
| Hot scrollback memory | `54.30 MiB` (`56,934,400 B`) |
| Cold scrollback memory (after Tier-2 zstd) | **`2.39 MiB`** (`2,507,340 B`) |
| Compression ratio | **`22.71×`** |
| Cooperative `compress_step(1)` steps / total time | `156` steps / **`11.91 ms`** |
| Single-step pause latency (p50 / p95 / max) | **`81.5 µs` / `98.3 µs` / `224.3 µs`** |
| Hot full-scrollback search — case-sensitive (p50 / max) | `67.30 ms` / `67.81 ms` |
| Hot full-scrollback search — case-insensitive (p50 / max) | `67.80 ms` / `71.28 ms` |
| Cold full-scrollback search — case-sensitive (p50 / max) | `71.17 ms` / `84.08 ms` |
| Cold full-scrollback search — case-insensitive (p50 / max) | `72.42 ms` / `178.90 ms` |

> **Observed optimization note:** `GhosttyEngine::search_lines` delegates to `libghostty-vt`'s full-scrollback
> search engine regardless of the requested `[from_line, to_line)` slice (~`67 ms` at 55k lines). During an
> active PTY flood, registering a reactive `WaitCondition::Text` triggers incremental dirty-range checks on
> every PTY read quantum — a narrow-range fast path for small dirty slices is a high-leverage future
> optimization.

### 4. WAL Streaming Write, Rollover & Seek-to-Timestamp (`wal_stream_rollover_seek`)

Streams **`8.0 MiB`** (`2,048 × 4 KiB` CRC32C-framed records) into a segmented `WalWriter`
(`segment_target_bytes = 512 KiB`, `budget_bytes = 64 MiB`), capturing dual-layer `KeyframeV2` snapshots
(`VisualKeyframe` + `GhosttyEngine::export_snapshot`) and running `sync_data()` + footer seal + directory
sync on all `16` segment rollovers, then times footer-indexed `WalReader::open` and `64` evenly distributed
`WalReader::records_for_moment(at_ms)` seeks across the entire recording span.

| Metric (`8.0 MiB`, `2,048` records, `16` rollovers) | Measured Baseline |
| --- | ---: |
| Streaming CRC32C-framed write throughput | **`173.2 MiB/s`** (`44,349 records/s`) |
| Segment rollover + `KeyframeV2` + `sync_data` (p50 / p95 / max) | `17.47 ms` / `20.07 ms` / `21.71 ms` |
| Footer-indexed `WalReader::open` (all 17 segments) | **`544.2 µs`** |
| Seek-to-timestamp `records_for_moment` (p50 / p95 / p99 / max) | **`1.55 ms` / `2.06 ms` / `2.33 ms` / `2.60 ms`** |

### 5. Zero-Sniff `proxy` Latency Overhead under Simulated RTT (`proxy_rtt_overhead`)

Measures `30` consecutive `Metrics` RPC round-trips against a live `BrokerServer` directly over the local
socket versus routed through `apack_broker::proxy::run_proxy_over` over bidirectional delay pipes injecting
`0 ms`, `20 ms` (`10 ms` each way), and `60 ms` (`30 ms` each way) simulated WAN RTT. Non-pipe overhead
subtracts exact measured delay-pipe hold time and direct local socket p50 to isolate pure proxy splice
cost from OS timer coalescing.

| Scenario (`30` RPCs each) | Target RTT | Observed p50 | Observed p95 | Pure Proxy Overhead (p50) |
| --- | ---: | ---: | ---: | ---: |
| `direct_local_socket` | `0 ms` | `23.0 µs` | `31.5 µs` | `0.0 µs` (baseline) |
| `proxy_0ms_rtt` | `0 ms` | `51.1 µs` | `78.7 µs` | **`+28.1 µs`** |
| `proxy_20ms_rtt` | `20 ms` | `25.15 ms` | `31.15 ms` | **`+110.8 µs`** |
| `proxy_60ms_rtt` | `60 ms` | `68.92 ms` | `74.06 ms` | **`+117.6 µs`** |

Even under 60 ms WAN RTT, zero-sniff proxy splicing adds **~0.12 ms (~0.2%)** overhead.
