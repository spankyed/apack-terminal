# apack-terminal (v2)

A sharded, sans-IO terminal broker: one process that owns PTYs and their process trees, runs `libghostty-vt`
as the single VT authority, records sessions to an immutable segmented WAL, and serves a single multiplexed
binary protocol — paired with the thin TypeScript renderer `@apackdev/terminal-view`.

Written for [apack](https://github.com/spankyed/apack), whose backend is a separate Node process. Sessions
survive backend restarts via broker-wide and per-session leases with reconnect grace windows, and tear down
deterministically through kernel-enforced ownership when the app exits.

| Path | What it is |
|---|---|
| `src/`, `tests/` | the `apack-terminal` v2 CLI binary (`serve`, `proxy`, `inspect`, `version`) and CLI integration tests |
| `crates/apack-proto/` | zero-`unsafe` `#![no_std]` + `alloc` binary framing, canonical LEB128 codec, and `PROTOCOL_VERSION = 2` wire schema |
| `crates/apack-core/` | zero-`unsafe` `#![no_std]` + `alloc` sans-IO deterministic broker & session state machine |
| `crates/apack-sim/` | deterministic discrete-event simulator (`SimWorld`, virtual clock, arbitrary chunking, slow-consumer chaos) |
| `crates/apack-vt-ghostty/` | `libghostty-vt` adapter implementing `VtEngine`, global content-addressed image store, bounded snapshots |
| `crates/apack-os/` | OS boundary: PTYs/ConPTY, `SignalTarget` process-tree control, Windows Job Objects, ACL-enforced sockets/pipes |
| `crates/apack-wal/` | zero-`unsafe` immutable segmented WAL (`meta.apkm`, `seg-*.apks`, `blobs/`), dual-layer `KeyframeV2`, $O(1)$ eviction |
| `crates/apack-broker/` | sharded I/O reactor (`mio`), per-session reader threads, OS blocking pool, `proxy`, `inspect`, v2 Rust client |
| `xtask/` | repository tasks (`pre-push`, `generate`, `check-generated`, `check-safety`, `notices`, `check-exclusions`) |
| [`node/`](node) | the `@apackdev/terminal` npm package: `brokerBinary()` runtime locator and golden `wire-vectors-v2.json` |
| [`view/`](view) | the `@apackdev/terminal-view` npm package: Canvas 2D + WebGL2 atlas renderer and v2 binary client ([its README](view/README.md)) |

## Core Guarantees

- **Correct by construction.** Illegal states are unrepresentable at compile time: `SignalTarget` makes
  `kill(0)`/`kill(-1)` impossible to construct; `VouchedExitCode` separates authenticated `MarksKey` shell
  facts from untrusted PTY claims; `apack-core` enforces sans-IO purity via `#![no_std]` + `#![forbid(unsafe_code)]`.
- **Single authority & unidirectional replication.** The broker's `libghostty-vt` core is the sole authority
  on terminal state (screen, scrollback, modes, cursor, images, markers, commands, program statuses).
  `@apackdev/terminal-view` interprets zero VT bytes and renders style-run `PackedRow` diffs directly.
- **Unified multiplexed binary protocol (v2).** One length-prefixed binary wire over Unix sockets, Windows
  named pipes, or `ssh host apack-terminal proxy` carries control RPCs, raw output feeds, incremental search
  watches, view frames, input events, and image blobs over independent `stream_id` channels with mutual
  HMAC-SHA256 authentication.
- **Explicit backpressure & principled load shedding.** Zero unbounded buffers anywhere. Conflatable view
  frames use adaptive BDP credit flow control and dirty-row coalescing; non-conflatable control streams use
  explicit byte credits and bounded queues; overload always reports measured metrics rather than dropping
  silently.
- **Owned process trees & crash-only safety.** POSIX sessions/process groups (`setsid` + negative-pgid kill)
  and Windows Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) ensure no orphaned shells survive teardown
  or crashes.
- **Immutable segmented WAL & dual-layer restore.** Crash-safe CRC32C-framed segments with $O(1)$ budget
  eviction and `KeyframeV2` carrying both a version-stable `VisualKeyframe` and an optional ABI-hashed
  `libghostty-vt` binary checkpoint.

## CLI

```sh
apack-terminal serve [--socket <path>] [--lease-grace-ms <ms>] [--max-sessions <n>]
apack-terminal proxy [<socket>] [--no-autostart]
apack-terminal inspect <recording-dir> [--json]
apack-terminal version
```

With no socket named, the broker binds the user's private runtime path (`$XDG_RUNTIME_DIR/apack-terminal/broker.sock`
or `~/.apack-terminal/broker.sock` on Unix; `\\.\pipe\apack-terminal-<sid>-default` on Windows). Shared
directories writable by other users (such as `/tmp`) are refused. Running `apack-terminal` with no arguments
exits with code `2` and prints usage to stderr.

## Status & Building

```sh
git clone https://github.com/spankyed/apack-terminal.git && cd apack-terminal
cargo build
cargo nextest run --workspace
npm --prefix view ci && npm --prefix view test && npm --prefix view run build
npm --prefix node test
```

| | |
|---|---|
| Rust | pinned by `rust-toolchain.toml` |
| Node | 24.12 or later, for `view/` and `node/` |
| Platforms | macOS arm64, Linux x64 (glibc ≥ 2.28), Windows x64 (with bundled Microsoft ConPTY in `windows/conpty/`) |
| Transport | Unix domain socket (owner-only `0o700` dir + `0o600` socket + peer UID check) or Windows named pipe (protected SDDL DACL + owner SID check) |
| Crash model | Sessions end when the broker stops; durable sessions restore from segmented WAL recordings (`KeyframeV2`) |

**Distribution.** Every `master` commit is built for macOS arm64, Linux x64 (manylinux_2_28), and Windows x64
and published as a GitHub release `bin-<commit>` (`.github/workflows/binaries.yml`) containing raw binaries,
Windows ConPTY assets, `THIRD-PARTY-NOTICES`, `COPYRIGHT-rust-library.html`, CycloneDX SBOMs, `SHA256SUMS`,
and pre-packed `.tgz` tarballs for `@apackdev/terminal`, `@apackdev/terminal-<platform>`, and
`@apackdev/terminal-view`. Consumers pin a release commit by URL and never need a Rust toolchain.

## Docs

See the [docs map](docs/README.md):

- [design.md](docs/architecture/design.md): workspace layers, sharded runtime, process lifetime, WAL, and security boundaries
- [protocol.md](docs/architecture/protocol.md): v2 binary framing, mutual auth, multiplexed streams, messages, and flow control
- [views.md](docs/architecture/views.md): differential view replication, Canvas 2D and WebGL2 atlas painters, and `@apackdev/terminal-view`
- [testing.md](docs/architecture/testing.md): deterministic simulation, mutation gates, ASAN, fuzzing, and cross-platform CI
- [windows-pipe-access.md](docs/architecture/windows-pipe-access.md): Windows named-pipe SDDL security descriptor and owner verification
- [background.md](docs/background.md): why the broker exists

## Develop

```sh
cargo xtask pre-push                         # every CI check + mutation testing on the diff
cargo nextest run --workspace                # full Rust test suite (each test in its own process)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo xtask generate                         # regenerate node/wire-vectors-v2.json
cargo xtask check-generated                  # verify wire vectors match Rust encoder
cargo xtask check-safety                     # verify every unsafe site has a SAFETY justification
cargo xtask check-notices                    # verify THIRD-PARTY-NOTICES and Rust stdlib notices
cargo xtask check-exclusions                 # verify every .cargo/mutants.toml exclusion is live
npm --prefix view test                       # view suite (also: run typecheck, run build)
npm --prefix node test                       # node package & release packing tests
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), both npm packages included. The view's
input handling draws on [coder/ghostty-web](https://github.com/coder/ghostty-web) under MIT ([`view/NOTICE`](view/NOTICE)).
Release binaries ship `THIRD-PARTY-NOTICES`, `COPYRIGHT-rust-library.html`, and a CycloneDX SBOM alongside
each platform artifact.
