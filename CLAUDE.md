# CLAUDE.md

`apack-terminal` v2 is a sharded, sans-IO terminal broker in Rust paired with the thin TypeScript renderer
`@apackdev/terminal-view`. Read [`docs/architecture/design.md`](docs/architecture/design.md) before changing
anything, and the document that owns the area you are changing — [`protocol.md`](docs/architecture/protocol.md),
[`views.md`](docs/architecture/views.md), [`testing.md`](docs/architecture/testing.md).

## Layout

| Path | What it is |
|---|---|
| `src/main.rs`, `tests/cli.rs` | root `apack-terminal` binary crate (`serve`, `proxy`, `inspect`, `version`) and end-to-end CLI tests |
| `crates/apack-proto/` | `#![no_std]` + `alloc`, `#![forbid(unsafe_code)]`: 14-byte length-prefixed binary framing + `FIN_FRAME`, canonical LEB128 varints, `PROTOCOL_VERSION = 2` messages, `PackedRow`, bounded decode caps |
| `crates/apack-core/` | `#![no_std]` + `alloc`, `#![forbid(unsafe_code)]`: pure sans-IO `BrokerCore` & `SessionMachine` state machines, HMAC-SHA256 handshake (`auth.rs`), leases (`lease.rs`), typestate vouched/claimed shell markers (`marks.rs`), OSC 7501 status table (`status.rs`), adaptive BDP `FrameCredit` + byte-credit flow control (`flow.rs`), `VtEngine` trait (`vt.rs`), `TimerWheel` (`time.rs`) |
| `crates/apack-sim/` | deterministic discrete-event simulator (`SimWorld`), virtual clock, arbitrary byte-chunk fuzzing, slow-consumer/flood chaos harness, mock `VtEngine` |
| `crates/apack-vt-ghostty/` | `libghostty-vt` (`ghostty-vt-rs-sys 0.3.0`) adapter: single-pass `PackedRow` extraction, global 256 MiB content-addressed `ImageStore`, zero file/SHM reads, Kitty graphics placeholders, bounded binary snapshots |
| `crates/apack-os/` | OS boundary: `KillTarget` + POSIX signal guard (`signal.rs`), process trees (`tree.rs`, `tree_windows.rs`), PTYs/ConPTY (`pty.rs`, `pty_windows.rs`), `PtyPoller` (`pty_poller.rs`, `pty_poller_epoll.rs`), ACL-hardened sockets/pipes & single-instance lock (`transport.rs`, `transport_windows.rs`, `security.rs`, `security_windows.rs`), `image_sources/`. All 7 Windows-only files are listed in `.cargo/windows-only` |
| `crates/apack-wal/` | `#![forbid(unsafe_code)]`: immutable segmented WAL (`meta.apkm`, `seg-*.apks`, `blobs/`), Castagnoli CRC32C framing, dual-layer `KeyframeV2`, $O(1)$ budget eviction, crash-safe tail truncation, async writer worker |
| `crates/apack-broker/` | sharded I/O runtime (`runtime.rs`, `transport_loop.rs`, `transport_windows.rs`, `shard.rs`, `shard_windows.rs`), CLI config (`config.rs`), multiplexed `proxy` (`proxy.rs`), live `inspect` (`inspect.rs`), typed Rust client (`client.rs`), golden vector test (`tests/wire_vectors_v2.rs`) |
| `node/` | `@apackdev/terminal` npm package: `brokerBinary()` locator (`index.js`, `index.d.ts`), **generated** `node/wire-vectors-v2.json`, and release packing/smoke scripts (`node/release/`) |
| `view/` | `@apackdev/terminal-view` npm package: v2 binary wire codec (`src/wire/`), multiplexed `TerminalClient` & `BrokerSessionSource` (`src/broker/client.ts`), `RowStore`, `Viewport`, `Input`, `Selection`, `links.ts`, `CanvasPainter`, `WebGL2AtlasPainter`, `ShelfPacker`, `createPainter` |
| `xtask/` | workspace tasks (`pre-push`, `generate`, `check-generated`, `check-safety`, `notices`, `check-notices`, `check-exclusions`, `clear-mutants`, `vt-prebuilt`) |
| `fuzz/` | standalone libFuzzer workspace (`v2_wire`, `v2_wal`, `v2_vt`) run nightly by `.github/workflows/fuzz.yml` |
| `windows/conpty/`, `build.rs` | bundled Microsoft ConPTY (`conpty.dll`, `OpenConsole.exe`) copied beside Windows binaries by `build.rs` |
| `about.toml`, `about.hbs`, `licenses/` | third-party license config and verbatim non-crate notices used by `cargo xtask notices` |
| `THIRD-PARTY-NOTICES`, `COPYRIGHT-rust-library.html` | **generated** by `cargo xtask notices`; shipped beside every platform binary in `binaries.yml` |

## Checks

**Run `cargo xtask pre-push` before every push.** It runs every check CI fails a change on, including
`cargo mutants` on the diff, cheapest first, and refuses a dirty working tree. Any surviving mutant must be
killed by a test or excluded in `.cargo/mutants.toml` with a concrete reason verified by `cargo xtask check-exclusions`.

**Process-control mutation safety.** Because the test suite executes real process signals, mutating a signal
target could broadcast `kill(0)` or `kill(-1)` to the host workstation. Three invariants prevent this and
must be preserved across any change to `crates/apack-os`:

- **`SignalTarget` and `KillTarget` make `0` and `-1` unrepresentable.** `apack_proto::types::SignalTarget`
  wraps a strictly positive `NonZeroI32` (`SignalTarget::new`), and `apack_os::signal::KillTarget` wraps a
  private `i32` constructed only through `KillTarget::at` / `kill_target` (neither implements `Default`) — so
  mutants returning `Ok(Default::default())` fail to compile.
- **The OS kill guard (`kill_target` / `send_signal`) is excluded in `.cargo/mutants.toml`** with an explicit
  comment so a mutant cannot strip the guard.
- **Integration brokers run in isolated sessions (`setsid`)**, so no group signal can escape to the test runner.

```sh
cargo xtask pre-push                                     # full pre-push gate + diff mutants
cargo nextest run --workspace                            # full Rust suite (isolated processes)
cargo clippy --workspace --all-targets -- -D warnings    # zero warnings across all targets
cargo fmt --all --check                                  # formatting pinned by rustfmt.toml
cargo xtask generate                                     # regenerate node/wire-vectors-v2.json
cargo xtask check-generated                              # fail if wire vectors drifted
cargo xtask check-safety                                 # fail if any unsafe lacks a SAFETY comment
cargo xtask notices                                      # regenerate THIRD-PARTY-NOTICES & stdlib HTML
cargo xtask check-notices                                # fail if notices drifted
cargo xtask check-exclusions                             # fail if any mutants exclusion matches nothing
cargo xtask clear-mutants                                # clean temp dirs from aborted mutant runs
npm --prefix view test                                   # view unit & golden wire vector suite
npm --prefix view run typecheck && npm --prefix view run build
npm --prefix node test                                   # @apackdev/terminal & release packing tests
```

- **Changed the v2 wire protocol?** Update `crates/apack-proto` AND `view/src/wire/` in the same
  commit, run `cargo xtask generate`, and commit the updated `node/wire-vectors-v2.json`. Both Rust
  (`crates/apack-broker/tests/wire_vectors_v2.rs`) and TypeScript (`view/tests/wire-vectors.spec.ts`) verify
  every vector in that file.
- **Changed a dependency, `rust-toolchain.toml`, or `ghostty-vt-rs-sys`?** Run `cargo xtask notices` and
  commit `THIRD-PARTY-NOTICES` and `COPYRIGHT-rust-library.html`.
- **`PROTOCOL_VERSION` is `2`.** Keep it at `2` until an independently published consumer cannot move in
  lockstep with this repository.

## Rules

- **Preserve sans-IO purity in `apack-proto` and `apack-core`.** Both crates are `#![no_std]` + `alloc` and
  `#![forbid(unsafe_code)]` (with `crates/apack-core/clippy.toml` forbidding wall clocks, sockets, files, and
  env reads). All new protocol logic, state transitions, coalescing, and deadlines belong in `apack-core`
  first and must be tested deterministically in `apack-sim`.
- **A claim is not a fact.** Everything coming out of a PTY is hostile untrusted bytes. Unvouched marks remain
  `CommandOutcome::Claimed`; only `OSC 633;V;...` verified in constant time against `MarksKey` with a strictly
  monotonic sequence number may produce `VouchedExitCode` (`CommandOutcome::Exited`).
- **Feed the authority before bytes leave.** The broker's `VtEngine` ingests PTY bytes before raw output
  subscribers or view projections observe them. Views never parse VT bytes.
- **Zero unbounded buffers & nothing polls.** Every queue, ring, image cache, marker buffer, and WAL backlog
  has an explicit compile-time cap. Conditions fire on state transitions; timers exist only for real wall-clock
  contracts (DEC 2026 holds, idle history compression, lease grace, caller deadlines) in one `TimerWheel`.
- **Every `unsafe` site requires a `SAFETY:` comment** (enforced by `cargo xtask check-safety`).
- **A platform claim is made on that platform.** Windows ConPTY, Job Objects, SDDL pipe ACLs, and
  `.cargo/windows-only` mutants are verified on Windows CI (`ci.yml`, `mutants.yml`).
- **Kill only by captured PID / `KillTarget`**, never by process name.

## Toolchain Alignment

`rust-toolchain.toml` pins the same Rust channel as apack's other Rust repositories (such as `elmdb`) so CI
and local workspace builds share one toolchain.
