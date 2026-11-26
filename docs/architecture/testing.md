# Testing & Verification — apack-terminal v2

Every invariant in `apack-terminal` v2 is backed by an automated gate. Correctness is proven at five layers:
deterministic sans-IO simulation, golden cross-language wire vectors, cross-platform OS/PTY integration tests,
mutation testing, and sanitizer/fuzzer coverage.

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

## 2. Golden Cross-Language Wire Vectors

`crates/apack-broker/tests/wire_vectors_v2.rs` encodes every `ClientMsg` and `BrokerMsg` variant (plus mutual
HMAC-SHA256 proofs) into `node/wire-vectors-v2.json`.

- `cargo xtask generate` regenerates `node/wire-vectors-v2.json`.
- `cargo xtask check-generated` fails CI if the Rust codec changed without updating the committed vectors.
- `view/tests/wire-vectors.spec.ts` decodes every broker vector and re-encodes every client vector
  byte-for-byte in TypeScript, proving wire compatibility in every single CI run.

## 3. Unit, Integration & Cross-Platform Suites

Tests run under `cargo nextest run --workspace` (each test in its own process with `.config/nextest.toml`
deadlines so a lost wake or signal fails fast instead of hanging):

- **`apack-proto`, `apack-core`, `apack-sim`:** Protocol framing, LEB128 canon, state machine, simulation.
- **`apack-vt-ghostty`:** `libghostty-vt` row extraction, Unicode/grapheme/wide cells, Kitty inline images,
  blocked file/SHM reads inside the VT core, `ImageStore` LRU eviction, snapshot export/import.
- **`apack-os`:** PTY/ConPTY spawn & resize, `KillTarget` invariants, POSIX process-group kill (`tree.rs`),
  Windows Job Objects (`tree_windows.rs`), socket `0o700`/`0o600` + peer UID checks (`transport.rs`), Windows
  SDDL pipe ACLs and owner SID verification (`security.rs`, `security_windows.rs`, `transport_windows.rs`).
- **`apack-wal`:** CRC32C framing, dual-layer `KeyframeV2`, torn-tail crash recovery, $O(1)$ budget eviction,
  blob sweeping, symlink/foreign-directory refusal, zlib bomb caps, async worker flush barriers.
- **`apack-broker` & root `tests/cli.rs`:** End-to-end sharded broker, `serve` single-instance locking,
  `proxy` autostart & `--no-autostart`, `inspect --json`, lease grace expiry, real shell integration.
- **View (`view/`):** Vitest suite covering `src/wire/*` (`wire-vectors.spec.ts`), `client.ts`, `row-store.ts`,
  `viewport.ts`, `local-query.ts`, `input.ts`, `selection.ts`, `links.ts`, `draw.ts`, `images.ts`,
  `canvas-painter.ts`, `webgl-painter.ts` (including `ShelfPacker` & `createPainter`), and `view.ts`, plus
  strict DOM-only `tsc --noEmit` and Vite bundle build.
- **Node (`node/`):** `brokerBinary()` resolution and release tarball packing/smoke verification.

CI (`.github/workflows/ci.yml`) runs the full suite on **Linux x64**, **macOS arm64**, and **Windows x64**
(with `cargo clippy` also run on Windows to catch `cfg(windows)` lint issues).

## 4. Mutation Testing (`.github/workflows/mutants.yml`)

Every pull request and push runs `cargo mutants --workspace --in-diff` across 8 Linux shards plus a dedicated
Windows runner for `.cargo/windows-only` files.

- **Zero surviving mutants:** Any missed mutant fails CI.
- **Zero dead exclusions:** `cargo xtask check-exclusions` verifies every entry in `.cargo/mutants.toml`
  matches at least one real mutant and every file in `.cargo/windows-only` exists.
- **Process-control safety:** `SignalTarget` and `KillTarget` make `kill(0)`/`kill(-1)` uncompileable under
  mutation, and the OS kill guard is explicitly excluded in `.cargo/mutants.toml`.

## 5. AddressSanitizer & Nightly Fuzzing

- **ASAN (`.github/workflows/asan.yml`):** Every code PR runs the workspace suite under
  `RUSTFLAGS=-Zsanitizer=address` on x86_64 Linux to verify the `libghostty-vt` FFI and OS boundaries.
- **Nightly Fuzzing (`.github/workflows/fuzz.yml`):** Automatically discovers every `[[bin]]` target in
  `fuzz/Cargo.toml` (`v2_wire`, `v2_wal`, `v2_vt`) and fuzzes each with libFuzzer + persisted corpora.

## Local Commands

```sh
cargo xtask pre-push                                  # full pre-push gate + diff mutants
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo xtask check-generated
cargo xtask check-safety
cargo xtask check-notices
cargo xtask check-exclusions
cargo test --locked --workspace --doc
cargo nextest run --locked --workspace --no-fail-fast
cargo build --release --locked --bin apack-terminal
npm --prefix view run typecheck && npm --prefix view test && npm --prefix view run build
npm --prefix node test
```
