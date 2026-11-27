# Testing & Verification — apack-terminal v2

Every invariant in `apack-terminal` v2 is backed by an automated gate. Correctness is proven at six layers:
deterministic sans-IO simulation, Rust → TypeScript wire codegen & golden vectors, cross-platform OS/PTY +
real-Chromium WebGL2 E2E suites, mutation testing & Windows cfg hygiene, sanitizer/fuzzing coverage, and
repeatable throughput/latency benchmarks.

## 1. Sans-IO Deterministic Simulation (`crates/apack-sim`)

Because `apack-proto` and `apack-core` are `#![no_std]` + `alloc` with zero OS dependencies, `apack-sim`
drives the entire broker state machine under a virtual clock (`SimWorld`):

- **Arbitrary Byte-Chunk Invariance:** Wire frames, control RPCs, and marker streams are fed whole, 1 byte
  at a time, and in seeded random chunk splits, asserting bit-identical state transitions.
- **Slow-Consumer & Flood Chaos:** Simulates stalled views, credit-starved raw output subscribers, DEC 2026
  sync holds hitting the `1 s` cap, and `yes`-style output floods, asserting bounded queues, zero silent drops,
  and exact `FrameCredit` / `OutputCredit` / `Throttled` / `OutputDropped` accounting.
- **Lease & Reconnect Lifecycle:** Tests `BrokerImmediate`, `BrokerGrace`, and per-session lease handoff,
  expiry across virtual time advances, and concurrent disconnect races deterministically with zero real sleeps.
- **Hostile-PTY Security Tests:** Verifies spoofed `OSC 633;V;...` marks with wrong keys or replayed sequence
  numbers are rejected and counted without ever forging `VouchedExitCode` or `PromptArrived`.

## 2. Rust → TypeScript Wire Codegen & Golden Vectors

`cargo xtask generate` (`xtask/src/wire_codegen.rs` & `xtask/src/generate.rs`) derives both the TypeScript
wire codec and cross-language test vectors directly from `apack-proto`:

- **Generated TS wire layer:** `view/src/wire/constants.ts`, `view/src/wire/types.ts`, and
  `view/src/wire/messages.ts` are generated deterministically from `apack-proto` constants, bounds (such as
  `MAX_SEARCH_NEEDLE_BYTES`), tags, and declarative wire schemas.
- **Golden wire vectors:** `crates/apack-broker/tests/wire_vectors_v2.rs` encodes every `ClientMsg` and
  `BrokerMsg` variant (plus mutual HMAC-SHA256 proofs) into `node/wire-vectors-v2.json`.
- **Drift gate:** `cargo xtask check-generated` fails CI if generated TS wire files or `node/wire-vectors-v2.json`
  drift from Rust. `view/tests/wire-vectors.spec.ts` decodes every broker vector and re-encodes every client
  vector byte-for-byte in TypeScript on every CI run.

## 3. Unit, Integration, Cross-Platform & Real-Chromium E2E Suites

Rust tests run under `cargo nextest run --workspace` (each test in its own process with `.config/nextest.toml`
deadlines so a lost wake or signal fails fast instead of hanging):

- **`apack-proto`, `apack-core`, `apack-sim`:** Protocol framing, LEB128 canon, state machine, simulation.
- **`apack-vt-ghostty`:** `libghostty-vt` row extraction, Unicode/grapheme/wide cells, Kitty inline images,
  blocked file/SHM reads inside the VT core, `ImageStore` LRU eviction, snapshot export/import.
- **`apack-os`:** PTY/ConPTY spawn & resize, `KillTarget` invariants, POSIX process-group kill (`tree.rs`),
  Windows Job Objects (`tree_windows.rs`), socket `0o700`/`0o600` + peer UID checks (`transport.rs`), Windows
  SDDL pipe ACLs and owner SID verification (`security.rs`, `security_windows.rs`, `transport_windows.rs`),
  and off-shard Kitty named-source (`t=f` / `t=s`) security enforcement (`image_sources/`).
- **`apack-wal`:** CRC32C framing, dual-layer `KeyframeV2`, torn-tail crash recovery, $O(1)$ budget eviction,
  blob sweeping, symlink/foreign-directory refusal, zlib bomb caps, async worker flush barriers.
- **`apack-broker` & root `tests/cli.rs`:** End-to-end sharded broker, `serve` single-instance locking,
  `proxy` autostart & `--no-autostart`, `inspect --json`, lease grace expiry, Kitty `t=f`/`t=s` broker wiring,
  and real shell integration.
- **View unit suite (`view/`):** Vitest suite covering `src/wire/*` (`wire-vectors.spec.ts`), `client.ts`,
  `row-store.ts`, `viewport.ts`, `local-query.ts`, `input.ts`, `selection.ts`, `links.ts`, `draw.ts`, `images.ts`,
  `canvas-painter.ts`, `webgl-painter.ts` (including `ShelfPacker` & `createPainter`), and `view.ts`, plus
  strict DOM-only `tsc --noEmit` and Vite bundle build.
- **Real-Chromium Playwright E2E suite (`view/e2e/`):** Boots a real `apack-terminal serve` v2 broker binary
  over a local WebSocket bridge and mounts `TerminalView` in real Chromium for both `WebGL2AtlasPainter` and
  `CanvasPainter`, asserting real rendered pixels (non-blank glyphs, RGB colors, cursor), color emoji vs
  monochrome tinting, Kitty inline image texture compositing, scrollback `FetchRows`, mouse-drag selection/copy,
  live search highlights, resize/reflow, and `WEBGL_lose_context` → `webglcontextrestored` recovery. Run
  locally with `npm --prefix view run test:e2e` and enforced in CI (`view-e2e (Linux x64)`).
- **Node (`node/`):** `brokerBinary()` resolution and release tarball packing/smoke verification.

CI (`.github/workflows/ci.yml`) runs the full suite on **Linux x64**, **macOS arm64**, and **Windows x64**
(with `cargo clippy` also run on Windows to catch `cfg(windows)` lint issues).

## 4. Mutation Testing, Windows Cross-Check & `#[cfg]` Hygiene

Every pull request and push runs `cargo mutants --workspace --in-diff` across 8 Linux shards plus a dedicated
Windows runner for `.cargo/windows-only` files (`.github/workflows/mutants.yml`):

- **Zero surviving mutants:** Any missed mutant fails CI.
- **Zero dead exclusions:** `cargo xtask check-exclusions` verifies every entry in `.cargo/mutants.toml`
  matches at least one real mutant and every file in `.cargo/windows-only` exists.
- **`#[cfg]` mutant-hygiene gate (`cargo xtask check-cfg-hygiene`):** Enforced in `checks (Linux x64)` and
  `pre-push`. Fails if mutable `#[cfg(windows)]` / `#[cfg(not(unix))]` logic (Windows-only functions, `impl`
  blocks, inline modules, or conditional/arithmetic/match logic inside Windows-gated blocks) is placed outside
  `.cargo/windows-only` files, preventing silent blind spots between the Linux and Windows mutation gates.
- **Fast local Windows cross-check (`cargo xtask check-windows`):** Runs
  `cargo clippy --locked --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings` locally on
  macOS/Linux (using an isolated metadata-only `ghostty-vt-rs-sys` stub under `target/windows-check` so no
  MSVC SDK is required for type/lint checks; pass `--xwin`, `--target`, `--check-only`, or `--no-stub` as
  needed). Requires `rustup target add x86_64-pc-windows-msvc`.
- **Process-control safety:** `SignalTarget` and `KillTarget` make `kill(0)`/`kill(-1)` uncompileable under
  mutation, and the OS kill guard is explicitly excluded in `.cargo/mutants.toml`.

## 5. AddressSanitizer, Fuzz Corpora & Continuous Fuzzing

- **ASAN (`.github/workflows/asan.yml`):** Every code PR runs the workspace suite under
  `RUSTFLAGS=-Zsanitizer=address` on x86_64 Linux to verify the `libghostty-vt` FFI and OS boundaries.
- **Committed Fuzz Seed Corpora (`fuzz/seeds/`):** Compact seed corpora committed for all four fuzz targets:
  `v2_wire`, `v2_wal`, `v2_vt`, and `v2_kitty_apc`. Every seed is deterministically replayed on stable Rust
  across Linux, macOS, and Windows via `tests/fuzz_corpus.rs` (part of `cargo nextest run --workspace`).
- **PR Fuzz Smoke Check (`cargo xtask fuzz-smoke` / CI `fuzz-smoke (Linux x64)`):** Replays all committed
  seed corpora and runs a bounded nightly ASAN libFuzzer smoke pass across every target on every PR, failing
  on any crash, panic, sanitizer violation, or OOM.
- **Nightly Fuzzing (`.github/workflows/fuzz.yml`):** Automatically discovers every `[[bin]]` target in
  `fuzz/Cargo.toml` (`v2_wire`, `v2_wal`, `v2_vt`, `v2_kitty_apc`) and fuzzes each with libFuzzer + persisted
  corpora.

## 6. Repeatable Performance & Flood Benchmarks (`cargo xtask bench`)

`cargo xtask bench` builds the release `apack-terminal` binary and executes `benches/v2_bench.rs` covering
five repeatable scenarios (sustained PTY flood throughput & isolated broker peak RSS, 60/120Hz TUI frame diff
+ wire encode/decode latency, 55k-line scrollback search + cooperative idle compression ratio/pause, WAL
streaming/rollover/seek latency, and stdio `proxy` RTT overhead). Recorded baselines and methodology live in
[benchmarks.md](benchmarks.md).

## Local Commands

```sh
cargo xtask pre-push                                  # full pre-push gate + diff mutants
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo xtask check-generated
cargo xtask check-safety
cargo xtask check-cfg-hygiene
cargo xtask check-windows                             # local Windows clippy cross-check
cargo xtask check-notices
cargo xtask check-exclusions
cargo test --locked --workspace --doc
cargo nextest run --locked --workspace --no-fail-fast
cargo build --release --locked --bin apack-terminal
cargo xtask fuzz-smoke --runs 1500                    # local seed replay + bounded fuzz smoke
cargo xtask bench                                     # release throughput/latency benchmarks
npm --prefix view run typecheck && npm --prefix view test && npm --prefix view run build
npm --prefix view run test:e2e                        # real-Chromium WebGL2 & Canvas E2E
npm --prefix node test
```
