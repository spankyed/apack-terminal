# Protocol — apack-terminal v2

`apack-terminal` v2 uses **one unified, multiplexed binary protocol** (`PROTOCOL_VERSION = 2`) for control
RPCs, raw output feeds, incremental search watches, differential view frames, input events, and image blob
transfers.

The single source of truth is `crates/apack-proto` (`#![no_std]` + `alloc`, `#![forbid(unsafe_code)]`). The
TypeScript codec lives in `view/src/wire/`. Both halves are locked together by the golden wire test vectors
in `node/wire-vectors-v2.json` (`cargo xtask generate` / `cargo xtask check-generated`).

## Framing (`crates/apack-proto/src/frame.rs`)

Every regular frame on the wire has a 14-byte little-endian header followed by `payload` (`body_len - 10` bytes):

```text
 0           4           8         10          14
┌───────────┬───────────┬─────────┬───────────┬──────────────────────────┐
│ body_len  │ stream_id │ msg_type│  req_id   │         payload          │
│  u32 LE   │  u32 LE   │ u16 LE  │  u32 LE   │   [u8; body_len - 10]    │
└───────────┴───────────┴─────────┴───────────┴──────────────────────────┘
```

- `FIN_FRAME = [0, 0, 0, 0]` (`body_len == 0`): universal 4-byte clean EOF sentinel across Unix sockets,
  Windows named pipes, and SSH `proxy` links.
- `body_len`: byte length of `[stream_id..]` (`10 <= body_len <= MAX_FRAME_PAYLOAD + 10`, where
  `MAX_FRAME_PAYLOAD = 8 MiB = 8_388_608`; values in `1..10` or above the cap fail decoding cleanly).
- `stream_id`: `0` (`CONTROL_STREAM_ID`) for connection-level control RPCs; `>= 1` (`NonZeroStreamId`) for
  multiplexed per-session streams.
- `msg_type`: explicit `u16` wire tag (`crates/apack-proto/src/messages.rs::msg_type`):
  - `0x0001..=0x0010`: Client → Broker control (`stream_id == 0`)
  - `0x0100..=0x0116`: Client → Broker per-session (`stream_id >= 1`)
  - `0x0200..=0x020C`: Broker → Client control replies
  - `0x0300..=0x0323`: Broker → Client per-session replies and push events (`req_id == 0` / `PUSH_REQ_ID` for pushes)
- `req_id`: client-assigned `u32` request correlation ID (`0` for unsolicited pushes / fire-and-forget messages).
- **Scalar & collection encoding (`crates/apack-proto/src/codec.rs`):**
  - Unsigned integers use **canonical unsigned LEB128** (`var_u32`, `var_u64`); non-canonical overlong
    encodings and trailing bytes in any decoded message payload are rejected.
  - Strings are UTF-8 length-prefixed with hard per-field decode caps.
  - Vectors use `vec_bounded` with explicit compile-time maximums before allocation (preventing OOM bombs).
  - `FrameDecoder` is strictly streaming and chunk-boundary invariant across every 1-byte and arbitrary split.

## Handshake & Mutual Authentication (`crates/apack-core/src/auth.rs`)

Every new connection starts unauthenticated under a hard `AUTH_HANDSHAKE_TIMEOUT_MS` (`10 s`) deadline:

```text
Client                                          Broker
  │ ─── Hello { protocol_version: 2,        ──▶  │  verifies protocol_version == 2
  │             client_nonce: [u8; 32] }         │  generates server_nonce: [u8; 32]
  │ ◀── HelloOk { protocol_version: 2,      ───  │  sets auth_required (true if APACK_TERMINAL_AUTH_KEY set)
  │               auth_required, server_nonce,   │
  │               word_delimiters, capabilities }│
  │                                              │
  │   [if auth_required == true:]                │
  │   client_hmac = HMAC-SHA256(secret,          │
  │     "apack-v2-client" || c_nonce || s_nonce) │
  │ ─── AuthProof { hmac: client_hmac }     ──▶  │  verifies client_hmac (constant-time)
  │                                              │  server_hmac = HMAC-SHA256(secret,
  │                                              │    "apack-v2-server" || s_nonce || c_nonce)
  │ ◀── AuthOk { server_hmac }              ───  │  transitions connection to Authenticated
  │   verifies server_hmac (constant-time)       │
```

If a client was configured with `authSecret` and the server replies `auth_required == false`, the client
rejects the connection immediately (downgrade protection). Any wrong protocol version, out-of-order message,
bad HMAC, or timeout closes the connection.

## Message Catalog (`crates/apack-proto/src/messages.rs`)

### Client → Broker (`ClientMsg`)

| Scope | Messages (`msg_type`) | Purpose |
|---|---|---|
| Control (`0x0001..=0x0010`) | `Hello`, `AuthProof`, `AcquireLease`, `ReleaseLease`, `Spawn`, `Kill`, `Signal`, `Owns`, `ListSessions`, `Rewind`, `Seek`, `Metrics`, `Attach`, `Detach`, `Keep`, `Wait` | Handshake/auth, leases (`BrokerImmediate`, `BrokerGrace`, `Session`), session spawn/kill/signal/ownership/list/keep, WAL `Rewind`/`Seek`, telemetry `Metrics`, stream `Attach`/`Detach`, event-driven `Wait` |
| Per-Session Stream (`0x0100..=0x0116`) | `FrameAck`, `ViewportHint`, `ExplicitResize`, `SetCursorDefaults`, `Key`, `Text`, `Pointer`, `Focus`, `Paste`, `Write`, `Pause`, `Resume`, `OutputCredit`, `FetchRows`, `WordAt`, `LineAt`, `TextOf`, `TextOfRect`, `SetSearch`, `Commands`, `CommandOutput`, `Images`, `GetImageBlob` | Adaptive BDP view pacing, resize arbitration, structured input/paste/raw write, PTY pause/resume, raw output byte credits, scrollback `FetchRows`, authoritative word/line/text selection, incremental `SetSearch`, shell command history & output, Kitty image placements & `64 KiB` chunked blob reads |

### Broker → Client (`BrokerMsg`)

| Scope | Messages (`msg_type`) | Purpose |
|---|---|---|
| Control Replies (`0x0200..=0x020C`) | `HelloOk`, `AuthOk`, `Error`, `Ok`, `LeaseAck`, `Spawned`, `OwnsReply`, `SessionsList`, `Rewound`, `Sought`, `MetricsReply`, `Attached`, `Waited` | Handshake/auth completion, typed `ErrorCode` replies, lease/session/rewind/metrics/wait RPC replies |
| Per-Session Replies (`0x0300..=0x030A`) | `Frame`, `Rows`, `Pasted`, `SpanReply`, `TextReply`, `SearchMatches`, `SearchDelta`, `CommandsReply`, `CommandOutputReply`, `ImagesReply`, `ImageBlobChunk` | Differential `FrameUpdate` pushes, scrollback rows, paste outcome, word/line/text queries, incremental search matches/deltas, shell commands & output, Kitty image placements & chunks |
| Per-Session Push Events (`0x0310..=0x0323`) | `Title`, `Bell`, `Clipboard`, `Notification`, `Progress`, `ProgramStatus`, `CommandStarted`, `CommandFinished`, `CwdChanged`, `Exited`, `Output`, `Throttled`, `OutputDropped`, `StreamClosed` | Terminal metadata/events, vouched/claimed shell lifecycle, OSC 7501 status updates, credit-bounded raw PTY `Output`, explicit backpressure notices (`Throttled`, `OutputDropped`), stream close |

## Flow Control & Backpressure (`crates/apack-core/src/flow.rs`)

`apack-terminal` v2 strictly separates **conflatable visual state** from **ordered byte/event streams**:

1. **View Frames (`SubFlags::VIEW`, Conflatable Adaptive BDP Credits):**
   - Each view stream grants `FrameCredit { max_frames: u16, max_bytes: u32 }` inside `FrameAck { seq, credit }`.
   - The broker clamps byte budgets to `[MIN_BDP_FRAME_BYTES = 32 KiB, MAX_BDP_FRAME_BYTES = 1 MiB]` (default
     `256 KiB`) and emits no frames when `max_frames == 0` (e.g. hidden tab).
   - While credit is exhausted, dirty rows accumulate in the session's dirty bitmap with zero queued
     intermediate frames; the next `FrameAck` immediately flushes one coalesced `Frame(FrameUpdate)`.
2. **Raw Output (`SubFlags::RAW_OUTPUT`, Ordered Byte Credits):**
   - Clients grant byte credits via `OutputCredit { bytes }` (capped at `MAX_RAW_OUTPUT_CREDIT = 4 MiB`).
   - Overload is explicitly surfaced via `Throttled` / `OutputDropped` rather than silently discarding bytes
     or unbounded buffering.
3. **Outbound & Stdin Caps:**
   - Non-conflatable outbound bytes per connection are capped at `MAX_CONTROL_QUEUE_BYTES = 16 MiB`.
   - Per-session PTY stdin write backlog is capped at `MAX_PTY_WRITE_BYTES = 16 MiB`.

## Error Codes (`crates/apack-proto/src/types.rs::ErrorCode`)

Every RPC error returns one of 11 closed `ErrorCode` variants:
`Handshake` (`1`), `Unauthorized` (`2`), `Malformed` (`3`), `NoSuchSession` (`4`), `NoSuchStream` (`5`),
`StaleEpoch` (`6`), `Pty` (`7`), `OutsideSession` (`8`), `Unsupported` (`9`), `ResourceLimit` (`10`),
`Busy` (`11`).
