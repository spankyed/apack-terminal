# Design — apack-terminal v2

`apack-terminal` v2 is a clean-room, sans-IO terminal broker and thin view renderer built around five
structural invariants:

1. **Pure sans-IO core.** All state transitions, protocol framing, flow control, shell marker verification,
   program status tracking, and deadline scheduling live in `#![no_std]` + `alloc`, `#![forbid(unsafe_code)]`
   crates (`apack-proto`, `apack-core`) driven by explicit events and virtual timestamps.
2. **Single VT authority.** `libghostty-vt` (`apack-vt-ghostty`) runs inside the broker and ingests every
   PTY byte before any raw output subscriber or view sees it. Views (`@apackdev/terminal-view`) interpret zero
   VT bytes.
3. **Sharded zero-lock-contention runtime.** The broker (`apack-broker`) partitions sessions across
   `K = min(cores, 8)` (`MAX_DEFAULT_SHARDS = 8`) dedicated shard threads, each owning its sessions'
   `GhosttyEngine` and `SessionMachine` exclusively on one thread with zero per-session PTY reader threads
   on Unix.
4. **Kernel-enforced process lifetimes.** POSIX sessions/process groups (`setsid` + negative-pgid kill via
   `KillTarget`) and Windows Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) guarantee teardown on exit
   or crash without relying on user-space cleanup.
5. **Zero unbounded buffers & crash-safe segmented WAL.** Every queue, ring, image store, and recording
   backlog has a compile-time cap; recordings use immutable CRC32C-framed segments with $O(1)$ eviction and
   dual-layer `KeyframeV2`.

## Workspace Architecture

```
                     ┌──────────────────────────────────────────┐
                     │          src/main.rs (CLI bin)           │
                     │   serve | proxy | inspect | version      │
                     └────────────────────┬─────────────────────┘
                                          ▼
                     ┌──────────────────────────────────────────┐
                     │           crates/apack-broker            │
                     │ Control Reactor · K Session Shards       │
                     │   Proxy · Inspect · Typed Rust Client    │
                     └───┬──────────┬──────────┬──────────┬─────┘
                         ▼          ▼          ▼          ▼
                ┌────────────┐ ┌─────────┐ ┌────────┐ ┌───────────┐
                │ apack-core │ │apack-vt │ │apack-os│ │ apack-wal │
                │ (sans-IO)  │ │-ghostty │ │(PTY/OS)│ │(segmented)│
                └─────┬──────┘ └────┬────┘ └───┬────┘ └─────┬─────┘
                      └─────────────┴──────────┴────────────┘
                                          ▼
                     ┌──────────────────────────────────────────┐
                     │           crates/apack-proto             │
                     │  Zero-unsafe binary framing & v2 schema  │
                     └──────────────────────────────────────────┘
                                          ▲
                                          │  golden vectors
                                          │  (node/wire-vectors-v2.json)
                                          ▼
                     ┌──────────────────────────────────────────┐
                     │         view/ (@apackdev/terminal-view)  │
                     │  v2 TS Client · Canvas 2D · WebGL2 Atlas │
                     └──────────────────────────────────────────┘
```

`crates/apack-sim` sits alongside `apack-core` for deterministic discrete-event simulation without OS I/O.
Repeatable throughput, frame-budget, scrollback, WAL, and proxy-RTT benchmarks live in `benches/v2_bench.rs`
(`cargo xtask bench`), with measured baseline numbers recorded in [benchmarks.md](benchmarks.md).

## Concurrency & Sharded Runtime (`apack-broker`)

- **Control Reactor (`transport_loop.rs`, `transport_windows.rs`):** Owns the listener socket/pipe,
  connections, `BrokerCore`, authentication handshakes, leases, and `StreamId` routing across shards.
- **Session Shards (`shard.rs`, `shard_windows.rs`):** Sessions are placed on the least-loaded shard among
  `K = min(cores, 8)` (overridable via `serve --shards <1..=8>`). Each shard owns its `SessionMachine`s,
  `GhosttyEngine` instances, and `TimerWheel` exclusively on one thread — **zero cross-thread mutexes on the
  hot VT/diff path**:
  - **Unix:** Each shard multiplexes all of its sessions' non-blocking PTY master fds inside one
    `apack_os::pty_poller::PtyPoller` (`epoll` on Linux via `pty_poller_epoll.rs`, `poll(2)` on macOS/BSD),
    draining at most `PTY_READ_QUANTUM_BYTES` (`256 KiB`) per session per tick so one flooding session cannot
    starve peer sessions on the same shard.
  - **Windows:** `shard_windows.rs` (`ConPtyBridge`) bridges blocking ConPTY pipe I/O into the shard's
    bounded command channel.
- **Bounded Foreground-Verification Worker (`shard.rs`):** Blocking `sysinfo`/Win32 foreground-process checks
  for OSC 7501 status confirmation run off the shard thread via a bounded queue (`FG_VERIFY_QUEUE_CAP = 64`),
  posting typed confirmation events back to the owning shard.
- **Unified `TimerWheel` (`apack-core/src/time.rs`):** Nothing polls. Timers exist only for genuine
  wall-clock contracts:
  - DEC 2026 synchronized-output hold cap (`SYNC_HOLD_MAX_MS = 1_000 ms`)
  - Incremental `SetSearch` coalescing (`SEARCH_COALESCE_MS = 150 ms`)
  - Connection authentication handshake timeout (`AUTH_HANDSHAKE_TIMEOUT_MS = 10_000 ms`)
  - Broker and per-session lease reconnect grace timers
  - Idle session scrollback compression (`DEFAULT_IDLE_COMPRESS = 2 s`), graceful kill escalation
    (`DEFAULT_KILL_ESCALATE = 2 s`), and caller `Wait` deadlines

## Process-Tree & Lifetime Model (`apack-os`, `apack-core/src/lease.rs`)

### Lease Ownership (Broker & Per-Session)

Session lifetimes are decoupled from individual socket/pipe connections via explicit **leases**
(`apack_proto::types::LeaseKind`):

- **`LeaseKind::BrokerImmediate`:** Tears down all unkept sessions immediately when the lease holder
  disconnects.
- **`LeaseKind::BrokerGrace { grace_ms: LeaseGraceMs, resume_token: ResumeToken }`:** Allows sessions to
  survive control-connection drops for up to `grace_ms`. If no client reclaims `resume_token` before expiry,
  the broker tears down all unkept sessions and exits.
- **`LeaseKind::Session(SessionId)`:** Scopes a lease to one session (`MAX_SESSION_LEASES_PER_CONN = 64`).
- **Crash-Only Failure Mode:** Failure always errs toward teardown rather than orphaned shells.

### Signal Safety (`SignalTarget` & `KillTarget`)

- `apack_proto::types::SignalTarget` wraps a strictly positive `NonZeroI32` (`SignalTarget::new`), validated
  at wire decode time.
- `apack_os::signal::KillTarget` wraps a private `i32` constructed exclusively via `KillTarget::at` /
  `kill_target` (neither implements `Default`): `0` (own process group) and `-1` (every process owned by the
  user) are **unrepresentable**.
- On POSIX (`crates/apack-os/src/tree.rs`), every spawned shell calls `setsid()` in `pre_exec`, making the
  child the leader of its own session and process group.
- On Windows (`crates/apack-os/src/tree_windows.rs`), every spawned ConPTY process is assigned to a dedicated
  Win32 Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing the handle terminates the
  entire process tree in the kernel.

### Windows ConPTY

Windows sessions run against the bundled Microsoft ConPTY (`windows/conpty/conpty.dll` + `OpenConsole.exe`),
copied beside the binary by `build.rs`. `crates/apack-os/src/pty_windows.rs` handles ConPTY-specific quirks
explicitly: `ClosePseudoConsole` teardown sequencing, input `\r\n` normalization, and bounded drain behavior
without hanging on missing EOF.

## VT Core, Graphics & Hostile-PTY Security Boundary (`apack-vt-ghostty`)

Everything written into a PTY is treated as **hostile untrusted bytes**:

- **Zero file or shared-memory reads inside the VT core.** Kitty graphics `t=f` (file), `t=t` (temp file),
  and `t=s` (POSIX/Win32 shared memory) transmissions are **never opened** by the VT core (`libghostty-vt`
  never touches the filesystem). When `SpawnSpec.image_files` is enabled, validated file/SHM reads are
  performed strictly outside the VT core by `crates/apack-os/src/image_sources/` under explicit path/size
  rules; otherwise only inline payloads (`t=d`) are accepted.
- **Content-addressed global `ImageStore` (`256 MiB` LRU cap):** Decoded Kitty images are hashed (SHA-256)
  and stored once across all sessions (`SESSION_IMAGE_STORAGE_BYTES = 64 MiB` per session). Views fetch blobs
  on demand via `GetImageBlob` and verify SHA-256 over the reassembled bytes before returning.
- **Typestate shell markers (`apack-core/src/marks.rs`):**
  - In `MarksMode::Claimed` (unkeyed), all `OSC 133`/`OSC 633` markers produce `CommandOutcome::Claimed`.
  - In `MarksMode::Keyed(MarksKey)`, only `OSC 633;V;...` sequences carrying the session's secret key
    (compared in constant time via `ct_eq_32`) AND a strictly increasing sequence number can produce
    `VouchedExitCode` (`CommandOutcome::Exited`) or emit `PromptArrived`. Invalid or replayed keyed marks are
    rejected and counted in `refused`, never downgraded to claims.
  - `MARKS_KEY_ENV` (`APACK_TERMINAL_KEY`) is kept distinct from the broker auth env var
    (`APACK_TERMINAL_AUTH_KEY`) so stripping the broker auth secret never clobbers a caller-supplied marks key.
- **Bounded OSC 7501 program statuses (`apack-core/src/status.rs`):** Capped at `MAX_STATUSES = 256` records
  and `512` bytes per field, with asynchronous non-blocking foreground-process confirmation.

## Immutable Segmented WAL & Dual-Layer Restore (`apack-wal`)

Recordings live in an owner-private directory (`0o700` on Unix, symlink-refused):

```text
<recording-dir>/
├── meta.apkm                # 40B CRC32C-verified metadata header (magic APKREC2\0, format v2, UUID)
├── blobs/<sha256>.png|zst   # Immutable content-addressed image blobs (verified on read)
├── seg-000001.apks          # Sealed immutable segment (KeyframeV2 + records + 40B APKFTR2\0 footer)
└── seg-000002.apks          # Active append-only tail segment
```

- **Crash-only safety:** Every record carries a 16-byte header and trailing Castagnoli CRC32C. Sealing a
  segment calls `sync_data()`, appends the 40-byte `SegmentFooter` (committing `records_end_offset` so
  untrusted PTY bytes inside a record can never spoof EOF), and syncs the directory. On open after a crash,
  sealed segments open in $O(1)$ via their footer; an unsealed active tail is scanned sequentially and
  truncated cleanly at `valid_records_end` via `set_len()`.
- **$O(1)$ budget eviction:** When total size exceeds `DEFAULT_BUDGET_BYTES` (`32 MiB`), oldest unpinned
  sealed segments are unlinked directly (zero full-file rewrites, zero Windows rename-sharing failures) and
  unreferenced blobs are swept.
- **Dual-layer `KeyframeV2`:** Record 0 of every segment is a zlib-compressed `KeyframeV2` (capped at
  `32 MiB` uncompressed; multi-keyframe amplification bombs after record 0 are rejected before inflation):
  1. `VisualKeyframe` — broker-owned, version-stable snapshot of screen rows, up to `1_000` history rows,
     cursor, modes, palette, image placements, `MarksKey` state, and command history.
  2. Optional `EngineCheckpoint` — ABI-hashed `libghostty-vt` binary snapshot (`<= 8 MiB`) enabling lossless
     restore on matching builds with automatic graceful fallback to `VisualKeyframe` across VT upgrades.
- **Principled overload accounting:** Continuous non-ground escape runs (`DEFAULT_MAX_NON_GROUND_BYTES = 4 MiB`)
  or slow-disk backlogs (`DEFAULT_MAX_BACKLOG_BYTES = 16 MiB`) emit explicit `WalRecord::Gap` records rather
  than silently dropping bytes or stalling PTY ingestion.

## Transport Security & Remote `proxy`

- **Unix sockets (`crates/apack-os/src/transport.rs`):** Parent directory must be owned by the current UID
  and mode `0o700` (world/group-writable directories like `/tmp` are refused). Socket permissions are `0o600`,
  and both broker and client verify peer UID via `SO_PEERCRED` / `getpeereid`.
- **Windows named pipes (`crates/apack-os/src/security.rs`, `security_windows.rs`, `transport_windows.rs`):**
  Protected SDDL DACL (`O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)`), server-side client PID SID verification
  (`verify_client_is_current_user` via `GetNamedPipeClientProcessId` → `OpenProcess` → `GetTokenInformation`),
  and client-side pipe owner SID verification (`owned_by_this_user`) before sending a byte ([windows-pipe-access.md](windows-pipe-access.md)).
- **Graceful process shutdown (`crates/apack-os/src/transport.rs`, `crates/apack-broker/src/runtime.rs`):**
  `BrokerServer::run` installs async-signal-safe `SIGINT`/`SIGTERM` handlers (self-pipe on Unix) and a
  Windows `SetConsoleCtrlHandler` hook so operator termination triggers graceful `TeardownAll`, seals active
  WAL segments, and removes the broker socket cleanly.
- **Optional mutual HMAC-SHA256 authentication (`apack-core/src/auth.rs`, `apack-broker/src/config.rs`):**
  Local transport defaults to `AuthConfig::None` (relying on OS peer UID / pipe owner verification); setting
  `APACK_TERMINAL_AUTH_KEY` derives a 32-byte `AuthSecret` via SHA-256 and enforces the `Hello -> HelloOk -> AuthProof -> AuthOk`
  mutual challenge-response handshake.
- **Zero-sniff multiplexed `proxy` (`crates/apack-broker/src/proxy.rs`):**
  `apack-terminal proxy [--socket <path>] [--no-autostart]` splices stdin/stdout directly to the local broker
  (auto-starting a detached `apack-terminal serve` unless `--no-autostart` is set, with a `1 MiB` replay
  buffer for idle-exit races and universal 4-byte `FIN_FRAME` on stdin EOF), allowing
  `ssh host apack-terminal proxy` to multiplex any number of remote control RPCs, views, raw feeds, and image
  fetches over a single SSH channel.

## How a Consumer Gets the Broker

Every `master` commit triggers `.github/workflows/binaries.yml`, building release binaries for `darwin-arm64`,
`linux-x64` (inside `manylinux_2_28` with glibc ≤ 2.28 enforcement), and `win32-x64` (with bundled ConPTY),
generating CycloneDX SBOMs (`apack-terminal_bin.cdx.json`), verifying `THIRD-PARTY-NOTICES`, packing
`@apackdev/terminal`, `@apackdev/terminal-<platform>`, and `@apackdev/terminal-view` tarballs, smoke-testing
every platform, and publishing the immutable GitHub release `bin-<commit>` with `SHA256SUMS`.
