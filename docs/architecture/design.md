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
  connections, `BrokerCore`, phase-typed decoders (`AwaitingHello` 4 KiB → `AwaitingAuth` 64 B → `Ready` 8 MiB),
  unified per-connection `MemoryBudget` (charging both `OutboundQueue` and `RawOutputFlow::pending_utf8` against
  `max_queue_bytes`), leases, and monotonic `StreamAttachmentId`-keyed routing across shards (preventing stale
  cross-session frame/stall races on `StreamId` reuse). Bounded reactor event draining
  (`MAX_SHARD_EVENTS_PER_TURN`) and per-connection in-flight shard command permits prevent floods from starving
  control RPCs or shutdown.
- **Session Shards (`shard/`, `shard_windows.rs`):** Sessions are placed on the least-loaded shard among
  `K = min(cores, 8)` (overridable via `serve --shards <1..=8>`). Each shard owns its `SessionMachine`s,
  `GhosttyEngine` instances, and `TimerWheel` exclusively on one thread — **zero cross-thread mutexes on the
  hot VT/diff path**:
  - **Unix:** Each shard multiplexes all of its sessions' non-blocking PTY master fds inside one
    `apack_os::pty_poller::PtyPoller` (`epoll` on Linux via `pty_poller_epoll.rs`, `poll(2)` on macOS/BSD),
    draining at most `PTY_READ_QUANTUM_BYTES` (`256 KiB`) per session per tick so one flooding session cannot
    starve peer sessions on the same shard.
  - **Windows:** `shard_windows.rs` (`ConPtyBridge`) bridges blocking ConPTY pipe I/O into the shard's
    bounded command channel, using a two-phase `Option<ExitDrainState>` (`None -> Some(ExitDrainState { code, signal, deadline })`)
    so trailing ConPTY bytes flush before `Exited`.
- **Bounded Foreground-Verification Worker (`shard/mod.rs`):** Blocking `sysinfo`/Win32 foreground-process checks
  for OSC 7501 status confirmation run off the shard thread via a bounded queue (`FG_VERIFY_QUEUE_CAP = 64`)
  with per-session `fg_inflight` coalescing, posting typed confirmation events back to the owning shard.
- **Unified `TimerWheel` (`apack-core/src/time.rs`):** Nothing polls. Timers exist only for genuine
  wall-clock contracts:
  - DEC 2026 synchronized-output hold cap (`SYNC_HOLD_MAX_MS = 1_000 ms`)
  - Incremental `SetSearch` coalescing (`SEARCH_COALESCE_MS = 150 ms`)
  - Connection authentication handshake timeout (`AUTH_HANDSHAKE_TIMEOUT_MS = 10_000 ms`)
  - Broker and per-session lease reconnect grace timers
  - Idle session scrollback compression (`DEFAULT_IDLE_COMPRESS = 2 s`), graceful kill escalation
    (`DEFAULT_KILL_ESCALATE = 2 s`), and caller `Wait` deadlines

## Process-Tree & Lifetime Model (`apack-os`, `apack-core/src/lease.rs`)

### Lease Ownership & `SessionLifetime`

Session lifetimes are explicitly modeled by `SessionLifetime` (`Kept`, `LeaseBound`, `ConnBound(ConnId)` where
`Rewind` sessions are always `ConnBound`) and decoupled from individual socket/pipe connections via explicit
**leases** (`apack_proto::types::LeaseKind`):

- **`LeaseKind::BrokerImmediate`:** Tears down all unkept (`LeaseBound`) sessions immediately when the lease holder
  disconnects (or when the last unleased client disconnects).
- **`LeaseKind::BrokerGrace { grace_ms: LeaseGraceMs, resume_token: ResumeToken }`:** Allows sessions to
  survive control-connection drops for up to `grace_ms`. If no client reclaims `resume_token` before expiry,
  the broker tears down all unkept sessions and exits.
- **`LeaseKind::Session(SessionId)`:** Scopes a lease to one session (`MAX_SESSION_LEASES_PER_CONN = 64`).
- **`SessionLifetime::ConnBound(ConnId)`:** Tears down the session immediately when its owning connection closes
  (used by `Rewind` playback sessions so they never outlive their viewer).
- **Crash-Only Failure Mode:** Failure always errs toward teardown rather than orphaned shells.

### Signal Safety (`SignalTarget` & `KillTarget`)

- `apack_proto::types::SignalTarget` wraps a strictly positive `NonZeroI32` (`SignalTarget::new`), validated
  at wire decode time.
- `apack_os::signal::KillTarget` wraps a private `i32` constructed exclusively via `KillTarget::at` /
  `kill_target` (neither implements `Default`): `0` (own process group) and `-1` (every process owned by the
  user) are **unrepresentable**.
- On POSIX (`crates/apack-os/src/tree.rs`, `signal.rs`), every spawned shell calls `setsid()` in `pre_exec`,
  making the child the leader of its own session and process group. Teardown probes exit via
  `waitid(P_PID, pid, ..., WEXITED | WNOHANG | WNOWAIT)` so the zombie pins the PID while signaling both
  `sid == leader` processes and deepest-first `setsid()` descendants still reachable via the live parent chain,
  then reaps once. (As with standard POSIX process-tree tracking without OS-specific cgroups or subreapers,
  a grandchild that calls `setsid()` and whose intermediate parent exits before discovery—reparenting it to
  PID 1 under a new SID—cannot be traced back to the session.)
- On Windows (`crates/apack-os/src/tree_windows.rs`), every spawned ConPTY process is attached to a dedicated
  Win32 Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` via its OS child `HANDLE`. Closing
  the handle terminates the entire process tree in the kernel.

### Windows ConPTY

Windows sessions run against the bundled Microsoft ConPTY (`windows/conpty/conpty.dll` + `OpenConsole.exe`),
copied beside built binaries and test executables by `crates/apack-os/build.rs`. `crates/apack-os/src/pty_windows.rs` handles ConPTY-specific quirks
explicitly: `ClosePseudoConsole` teardown sequencing, input `\r\n` normalization, and bounded drain behavior
without hanging on missing EOF.

## VT Core, Graphics & Hostile-PTY Security Boundary (`apack-vt-ghostty`)

Everything written into a PTY is treated as **hostile untrusted bytes**:

- **Zero file or shared-memory reads inside the VT core.** Kitty graphics `t=f` (file), `t=t` (temp file),
  and `t=s` (POSIX/Win32 shared memory) transmissions are **never opened** by the VT core (`libghostty-vt`
  never touches the filesystem; probe validation uses the sealed `NamedImageProbe` type and all `extern "C"`
  callbacks route through a `catch_unwind` trampoline). When `SpawnSpec.image_files` is enabled, validated
  file/SHM reads run off-shard in `crates/apack-os/src/image_sources/` using handle-relative traversal:
  Windows (`NtCreateFile` + `FILE_OPEN_REPARSE_POINT`) refuses all reparse points (symlinks and NTFS
  junctions), UNC shares, devices, and named pipes; POSIX (`openat2` / `readlinkat` + `dir_stack`) resolves
  safe symlinks (e.g., macOS `/var -> /private/var`) while refusing `/proc`/`/sys`/`/dev` (except `/dev/shm`),
  symlink loops, special filesystems, and non-regular files. Otherwise only inline payloads (`t=d`) are
  accepted.
- **Push-synced image cache & content-addressed `ImageStore` (`256 MiB` LRU cap):** `GhosttyEngine` pushes
  decoded Kitty image metadata into its bounded local cache on `&mut self` mutations (`feed` / `restore_snapshot`)
  so viewport/history/placement reads are lock-free. Views (`@apackdev/terminal-view`) key WebGL2 GPU textures
  by content-addressed `BlobHash` string, releasing textures deterministically on `ImageCache.onEvict`, and
  verify SHA-256 over reassembled `GetImageBlob` chunks.
- **Typestate shell markers (`apack-core/src/marks.rs`):**
  - In `MarksMode::Claimed` (unkeyed), all `OSC 133`/`OSC 633` markers produce `CommandOutcome::Claimed`.
  - In `MarksMode::Keyed(MarksKey)`, only `OSC 633;V;...` sequences carrying the session's secret key
    (compared in constant time via `ct_eq_32`) AND a strictly increasing sequence number can produce
    `VouchedExitCode` (`CommandOutcome::Exited`) or emit `PromptArrived`. Invalid or replayed keyed marks are
    rejected and counted in `refused`, never downgraded to claims.
  - `MARKS_KEY_ENV` (`APACK_TERMINAL_KEY`) is kept distinct from the broker auth env var
    (`APACK_TERMINAL_AUTH_KEY`), which `serve` reads and removes from its process environment before spawning
    any worker thread or shell.
- **Bounded OSC 7501 program statuses (`apack-core/src/status.rs`):** Capped at `MAX_STATUSES = 256` records
  and `512` bytes per field, with hierarchical prefix `Clear`, ancestor `app` inheritance, pure sans-IO
  `argv[0]`/interpreter/alias verification against off-shard foreground process checks, late-attach status
  hydration, and `WaitCondition::Status` deferral until foreground confirmation succeeds (never waking on
  `Confirmed::Mismatched`).

## Immutable Segmented WAL & Dual-Layer Restore (`apack-wal`)

Recordings live in an owner-private directory (`0o700` on Unix, symlink-refused) with every file open routed
through `safe_fs` (`O_NOFOLLOW | O_NONBLOCK` + regular-file verification):

```text
<recording-dir>/
├── meta.apkm                # 40B CRC32C-verified metadata header (magic APKREC2\0, format v2, UUID)
├── blobs/<sha256>.png|zst   # Immutable content-addressed image blobs (written live & verified on read)
├── seg-000001.apks          # Sealed immutable segment (KeyframeV2 + records + 72B APKFTR2\0 SHA-256 footer)
└── seg-000002.apks          # Active append-only tail segment
```

- **Crash-only safety & SHA-256 footer authenticity:** Every record carries a 16-byte header and trailing
  Castagnoli CRC32C. Sealing a segment calls `sync_data()`, appends the 72-byte `SegmentFooter` (committing
  `records_end_offset` plus the incremental SHA-256 digest over `[0..records_end_offset]` so untrusted PTY
  bytes inside a record can never spoof a sealed footer), and syncs the directory. Sealed historical segments
  verify via their SHA-256 footer; the tail/active segment and any live claimed directory always verify
  sequentially and truncate torn tails cleanly at `valid_records_end`.
- **$O(1)$ budget eviction:** When total size exceeds `DEFAULT_BUDGET_BYTES` (`32 MiB`), oldest unpinned
  sealed segments are unlinked directly (zero full-file rewrites, zero Windows rename-sharing failures) and
  unreferenced blobs are swept.
- **Dual-layer `KeyframeV2` & transactional replay:** Record 0 of every segment is a zlib-compressed
  `KeyframeV2` (capped at `32 MiB` uncompressed; multi-keyframe amplification bombs after record 0 are
  rejected before inflation). Live shards persist image blobs (`write_blob`) and vouched marks (`write_marks`)
  alongside output, and `Rewind`/`Seek`/`restore_from` hydrate referenced blobs from `<dir>/blobs/` and restore
  through `restore_machine_from_slice` -> `SessionMachine::restore_from_replay` (monotonically advancing `epoch`,
  resetting/replaying marks & statuses, refreshing `SearchWatch`es, and forcing full sync):
  1. `VisualKeyframe` — broker-owned, version-stable snapshot of screen rows, up to `1_000` history rows,
     cursor, modes, palette, image placements, `MarksKey` state, and command history (sanitized on fallback).
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
`@apackdev/terminal`, `@apackdev/terminal-<platform>`, and `@apackdev/terminal-view` (all version-locked by
`cargo xtask check-versions` and stamped with `gitHead`), smoke-testing both the broker binary and
`@apackdev/terminal-view` ESM imports on every platform (`smoke` and post-publish `consume`), and publishing
the immutable GitHub release `bin-<commit>` with `SHA256SUMS`.
