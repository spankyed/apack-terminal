# Protocol — apack-terminal v2

`apack-terminal` v2 uses **one unified, multiplexed binary protocol** (`PROTOCOL_VERSION = 2`) for control
RPCs, raw output feeds, incremental search watches, differential view frames, input events, and image blob
transfers.

The single source of truth is `crates/apack-proto` (`#![no_std]` + `alloc`, `#![forbid(unsafe_code)]`). The
TypeScript codec lives in `view/src/wire/`. Both halves are locked together by the golden wire test
vectors in `node/wire-vectors-v2.json` (`cargo xtask generate` / `cargo xtask check-generated`).

## Framing

Every frame on the wire has a 12-byte little-endian header followed by `payload_len` bytes:

```text
 0       4       6       8               12
┌───────┬───────┬───────┬───────────────┬──────────────────────┐
│  len  │ type  │ flags │   stream_id   │       payload        │
│  u32  │  u16  │  u16  │      u32      │     [u8; len]        │
└───────┴───────┴───────┴───────────────┴──────────────────────┘
```

- `len`: payload byte length (`<= MAX_FRAME_PAYLOAD = 16 MiB`; frames exceeding this close the connection with
  `WireError::FrameTooLarge`).
- `type`: `u16` message tag distinguishing client (`0x0001..`) and broker (`0x8001..`) messages.
- `flags`: reserved `u16` (`0`; unknown non-zero flags are rejected).
- `stream_id`: `0` for connection-level control RPCs and lifecycle events; `>= 1` for multiplexed per-session
  streams (`AttachView`, `SubscribeOutput`, `SubscribeSearch`, etc.).
- **Scalar & collection encoding (`crates/apack-proto/src/codec.rs`):**
  - Unsigned integers use **canonical unsigned LEB128** (`var_u32`, `var_u64`); non-canonical overlong encodings
    and trailing bytes are rejected on decode.
  - Strings are UTF-8 length-prefixed (`var_u32` byte count + UTF-8 bytes) with hard per-field decode caps.
  - Vectors use `vec_bounded` with explicit compile-time maximums before allocation (preventing OOM bombs).
  - `FrameDecoder` is strictly streaming and chunk-boundary invariant across every 1-byte and arbitrary split.

## Handshake & Mutual Authentication

Every new connection starts in `Unauthenticated` state under a hard `AUTH_HANDSHAKE_TIMEOUT_MS` (`10 s`)
deadline and completes a 4-step mutual HMAC-SHA256 handshake (`crates/apack-core/src/auth.rs`):

```text
Client                                          Broker
  │ ─── Hello { protocol: 2, client_nonce, ──▶   │  verifies protocol == 2
  │             client_name, capabilities }      │  generates server_nonce
  │ ◀── HelloChallenge { server_nonce,      ───  │  MAC_s = HMAC(key, "apack-v2-srv" || c_nonce || s_nonce)
  │                      server_mac }            │
  │   verifies server_mac (constant-time)        │
  │   MAC_c = HMAC(key, "apack-v2-cli" || ...)   │
  │ ─── HelloAuth { client_mac }            ──▶  │  verifies client_mac (constant-time)
  │ ◀── HelloOk { broker_version,           ───  │  transitions connection to Authenticated
  │               capabilities, max_frame,       │
  │               word_delimiters }              │
```

Any message other than the expected handshake step, wrong protocol version, bad MAC, or timeout immediately
closes the connection.

## Message Catalog (`crates/apack-proto/src/messages.rs`)

### Client → Broker (`ClientMsg`)

| Group | Messages | Purpose |
|---|---|---|
| Handshake | `Hello`, `HelloAuth`, `Ping` | Protocol negotiation, mutual HMAC auth, RTT probing |
| Leases | `AcquireLease`, `ReleaseLease`, `RenewLease` | Claim/release/extend broker-wide or per-session reconnect grace leases |
| Session Lifecycle | `Spawn`, `ListSessions`, `CloseSession`, `Signal`, `Resize`, `Write` | Create/list/terminate sessions, send POSIX signals (`SignalKind`), resize PTY/VT, write raw stdin |
| Streams & Views | `Attach`, `Detach`, `FrameAck`, `OutputCredit`, `KeyInput`, `MouseInput`, `Paste` | Open/close multiplexed streams (`SUB_VIEW`, `SUB_RAW_OUTPUT`, `SUB_EVENTS`), acknowledge rendered view frames, grant raw output byte credits, send structured input |
| Queries & Waits | `ReadScreen`, `ReadHistory`, `GetFacts`, `GetCommands`, `GetImages`, `GetImageBlob`, `WordAt`, `SelectText`, `Search`, `SearchWatchStart`, `SearchWatchStop`, `Wait`, `CancelWait` | Authoritative reads, scrollback paging, shell/status facts, content-addressed image blob streaming, word/selection resolution, one-shot & incremental search, event-driven waits |
| Recordings | `RecordingStatus`, `RecordingFlush`, `RewindOpen`, `RewindSeek`, `RewindClose` | Inspect WAL status, flush barrier, and deterministic recording rewind/seek |

### Broker → Client (`BrokerMsg`)

| Group | Messages | Purpose |
|---|---|---|
| Handshake | `HelloChallenge`, `HelloOk`, `Pong`, `Error` | Auth challenge/completion, RTT reply, typed `ErrorCode` replies (`rpc_id`) |
| Leases | `LeaseAcquired`, `LeaseReleased` | Lease confirmation and expiry/release notice |
| Lifecycle & Replies | `Spawned`, `SessionList`, `Ok`, `SessionExited` | RPC replies and asynchronous session exit notifications (exit code/signal) |
| Streaming Data | `ViewFrame`, `RawOutput`, `SessionEvent`, `SearchDelta` | Coalesced differential view frames, credit-bounded raw PTY bytes, async shell/title/CWD/status/bell events, debounced search match updates |
| Query Replies | `ScreenReply`, `HistoryReply`, `FactsReply`, `CommandsReply`, `ImagesReply`, `ImageBlobChunk`, `WordReply`, `TextReply`, `SearchReply`, `WaitReply`, `RecordingStatusReply`, `RewindReply` | Bounded authoritative query answers |

## Flow Control & Backpressure (`crates/apack-core/src/flow.rs`)

`apack-terminal` v2 strictly separates **conflatable visual state** from **ordered byte/event streams**:

1. **View Frames (Conflatable, Adaptive BDP Window):**
   - Each attached view stream tracks in-flight frame bytes against an adaptive bandwidth-delay-product
     window clamped to `[MIN_BDP_FRAME_BYTES = 32 KiB, MAX_BDP_FRAME_BYTES = 1 MiB]` (default `256 KiB`).
   - When the window is full, the shard marks dirty rows in the view's `ViewProjection` without queueing
     intermediate frames. The next `FrameAck { frame_seq, paint_us }` frees credit, adjusts the BDP window
     using observed RTT/paint time, and immediately flushes a single coalesced delta frame carrying all
     accumulated changes plus `coalesced_frames` accounting.
2. **Raw Output (`SUB_RAW_OUTPUT`, Ordered Byte Credits):**
   - Clients explicitly grant byte credits via `OutputCredit { bytes }` (capped at `MAX_RAW_OUTPUT_CREDIT = 4 MiB`).
   - The broker never transmits raw bytes beyond granted credit; if a consumer stalls, overflow is tracked
     explicitly rather than growing memory unboundedly.
3. **Control & Event Outbound Queue:**
   - Non-conflatable outbound bytes per connection are capped at `MAX_CONTROL_QUEUE_BYTES = 16 MiB`. Exceeding
     this terminates the stalled connection cleanly so one dead client cannot starve the shard.
   - Per-session PTY stdin write backlog is capped at `MAX_PTY_WRITE_BYTES = 16 MiB`.

## Error Codes (`ErrorCode`)

Every RPC error returns a closed, typed `ErrorCode` enum (never ad-hoc error strings for control flow):
`ProtocolVersion`, `AuthFailed`, `InvalidRequest`, `SessionNotFound`, `StreamNotFound`, `LimitExceeded`,
`Backpressure`, `LeaseConflict`, `NotSupported`, `IoFailed`, `RecordingError`, `Cancelled`, `TimedOut`,
`Internal`.
