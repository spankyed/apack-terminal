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
   `N = min(4, num_cpus)` dedicated I/O shards (`mio` reactors), each owning its sessions' `VtEngine` and
   `SessionCore` exclusively on one thread.
4. **Kernel-enforced process lifetimes.** POSIX sessions/process groups (`setsid` + negative-pgid kill via
   `SignalTarget`) and Windows Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) guarantee teardown on exit
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
                     │ Sharded Reactor · PTY Readers · OS Pool  │
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

## Concurrency & Sharded Runtime (`apack-broker`)

- **Sharded Event Loops (`shard.rs`):** Sessions are assigned to a shard on spawn. Each shard owns its
  `SessionCore`s, `GhosttyEngine` instances, view projections, and per-shard deadline heap (`DeadlineWheel`)
  with **zero cross-thread mutexes on the hot VT/diff path**.
- **Dedicated PTY Reader Threads (`pty_reader.rs`):** One OS reader thread per live PTY drains raw bytes
  into a bounded `mpsc::sync_channel` (`64` chunks × `64 KiB` = `4 MiB` max in-flight per session), waking
  the owning shard immediately via `mio::Waker`. If a session's shard falls behind under a `yes`/`cat` flood,
  the bounded channel applies kernel backpressure directly onto the PTY master fd without blocking any other
  session or shard.
- **OS Blocking Pool (`pool.rs`):** Operations that can block on kernel state — PTY spawning, `sysinfo`/Win32
  foreground-process verification for OSC 7501 statuses, and synchronous process-tree checks — execute on a
  bounded worker pool and post typed completion events back to the shard.
- **Unified Deadline Heap (`apack-core/src/time.rs`):** Nothing polls. Timers exist only for genuine
  wall-clock contracts:
  - DEC 2026 synchronized-output hold cap (`SYNC_HOLD_MAX_MS = 1000 ms`)
  - Incremental `SearchWatch` coalescing (`SEARCH_COALESCE_MS = 150 ms`)
  - Connection mutual-auth handshake timeout (`AUTH_HANDSHAKE_TIMEOUT_MS = 10_000 ms`)
  - Broker and per-session lease reconnect grace timers
  - Idle session scrollback compression and caller `Wait` deadlines

## Process-Tree & Lifetime Model (`apack-os`, `apack-core/src/lease.rs`)

### Lease Ownership (Broker & Per-Session)

Session lifetimes are decoupled from individual TCP/socket/pipe connections via explicit **leases**:

- **Broker Lease (`LeaseKind::Broker`):** Claimed by the host application (e.g. Electron main process) using
  a `LeaseToken`. While a broker lease is held, sessions outlive control-connection drops. When the holder's
  connection drops, the lease enters a bounded reconnect grace window (`--lease-grace-ms`, default 30 s,
  clamped); if no client reclaims the token before expiry, the broker tears down all sessions and exits.
- **Per-Session Lease (`LeaseKind::Session`):** Independent sessions can hold their own lease tokens and
  grace windows (`MAX_SESSION_LEASES_PER_CONN = 64`).
- **Ephemeral / Holder-Bound Failure Mode:** If a broker is started without an active lease or all leases
  expire, failure always errs toward teardown — never orphaned background shells.

### Signal Safety (`SignalTarget`)

`crates/apack-os/src/signal.rs` defines `SignalTarget` with a private `NonZeroI32` and no `Default` impl:

- `SignalTarget::process_group(pgid)` requires `pgid > 0` and stores `-pgid` (POSIX `killpg` semantics).
- `SignalTarget::process(pid)` requires `pid > 0`.
- `0` (own process group) and `-1` (all processes owned by the user) are **unrepresentable**.
- On POSIX (`tree_posix.rs`), every spawned shell calls `setsid()` in `pre_exec`, making the child the leader
  of its own session and process group.
- On Windows (`tree_windows.rs`), every spawned ConPTY process is assigned to a dedicated Win32 Job Object
  configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing the handle terminates the entire process tree
  in the kernel.

### Windows ConPTY

Windows sessions run against the bundled Microsoft ConPTY (`windows/conpty/conpty.dll` + `OpenConsole.exe`),
copied beside the binary by `build.rs`. `crates/apack-os/src/pty_windows.rs` handles ConPTY-specific quirks
explicitly: explicit `ClosePseudoConsole` teardown sequencing, input `\r\n` normalization, and bounded drain
behavior without hanging on missing EOF.

## VT Core, Graphics & Hostile-PTY Security Boundary (`apack-vt-ghostty`)

Everything written into a PTY is treated as **hostile untrusted bytes**:

- **Zero file or shared-memory reads inside the VT core.** Kitty graphics `t=f` (file), `t=t` (temp file),
  and `t=s` (POSIX/Win32 shared memory) transmissions are **never opened** by the VT core or broker. Only
  inline payloads (`t=d`) are accepted, eliminating arbitrary file disclosure and path-traversal classes.
- **Content-addressed global `ImageStore` (`256 MiB` LRU cap):** Decoded inline images are hashed (SHA-256)
  and stored once across all sessions (`SESSION_IMAGE_STORAGE_BYTES = 64 MiB` per session). Views fetch blobs
  on demand via `GetImageBlob` and verify SHA-256 before decoding.
- **Typestate shell markers (`apack-core/src/marks.rs`):**
  - In `MarksMode::Claimed` (unkeyed), all `OSC 133`/`OSC 633` markers produce `CommandOutcome::Claimed`.
  - In `MarksMode::Keyed(MarksKey)`, only `OSC 633;V;...` sequences carrying the session's 32-byte secret key
    (compared in constant time via `ct_eq_32`) AND a strictly increasing sequence number can produce
    `VouchedExitCode` (`CommandOutcome::Exited`) or emit `PromptArrived`. Invalid or replayed keyed marks are
    rejected and counted in `refused`, never downgraded to claims.
  - Note: `MarksKey` protects against untrusted terminal *output* (logs, `cat`, `curl`, remote SSH output),
    not against a local process running as the same OS user that can inspect the shell's environment.
- **Bounded OSC 7501 program statuses (`apack-core/src/status.rs`):** Capped at `MAX_STATUSES = 256` records
  and `512` bytes per field, with asynchronous non-blocking foreground-process confirmation via `pool.rs`.

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

- **Unix sockets (`transport_posix.rs`):** Parent directory must be owned by the current UID and mode `0o700`
  (world/group-writable directories like `/tmp` are refused). Socket permissions are `0o600`, and both
  broker and client verify peer UID via `SO_PEERCRED` / `getpeereid`.
- **Windows named pipes (`security_windows.rs`, `transport_windows.rs`):** Protected SDDL DACL
  (`O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)`) plus client-side owner SID verification before sending a byte
  ([windows-pipe-access.md](windows-pipe-access.md)).
- **Mutual HMAC-SHA256 authentication (`apack-core/src/auth.rs`):** Every connection performs a challenge-response
  `Hello` / `HelloChallenge` / `HelloAuth` / `HelloOk` handshake using a per-broker 32-byte secret key stored
  in an owner-private keyfile beside the socket/pipe (or supplied over the transport).
- **Multiplexed `proxy` (`apack-broker/src/proxy.rs`):** `apack-terminal proxy [<socket>]` bridges stdin/stdout
  directly to the local broker (auto-starting it unless `--no-autostart` is set), allowing `ssh host apack-terminal proxy`
  to multiplex any number of remote control RPCs, views, raw feeds, and image fetches over a single SSH channel.

## How a Consumer Gets the Broker

Every `master` commit triggers `.github/workflows/binaries.yml`, building release binaries for `darwin-arm64`,
`linux-x64` (inside `manylinux_2_28` with glibc ≤ 2.28 enforcement), and `win32-x64` (with bundled ConPTY),
generating CycloneDX SBOMs, verifying `THIRD-PARTY-NOTICES`, packing `@apackdev/terminal`,
`@apackdev/terminal-<platform>`, and `@apackdev/terminal-view` tarballs, smoke-testing every platform, and
publishing the immutable GitHub release `bin-<commit>` with `SHA256SUMS`.
