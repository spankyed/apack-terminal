/**
 * Single-connection multiplexed v2 binary broker client (`TerminalClient`) and per-session
 * `BrokerSessionSource` implementing `TerminalSource`.
 *
 * Features:
 * - Mutual HMAC-SHA256 handshake (`apack-v2-client` / `apack-v2-server`) with strict downgrade rejection
 * - Universal 4-byte `FIN_FRAME` clean close
 * - Multiplexed `NonZeroStreamId` streams & correlated `RequestId` RPCs
 * - Byte-bounded BDP `FrameCredit` pacing (`maxFrames`, `maxBytes`, dynamic EWMA `ackRttMs`,
 *   `maxFrames = 0` when hidden)
 * - Pipelined `64 KiB` `GetImageBlob` chunk assembly (`window = 8` chunks in flight)
 */
import { BULK_CHUNK_BYTES, PROTOCOL_VERSION, WireError } from '../wire/codec.ts'
import { computeClientProof, ctEq32, randomNonce32, sha256, verifyServerProof } from '../wire/auth.ts'
import { FrameDecoder, encodeFin, type FrameHeader } from '../wire/frame.ts'
import {
  decodeBrokerMsg,
  encodeClientFrame,
  type BrokerMsg,
  type ClientMsg,
  type FrameUpdate,
  type SessionSummary,
  type SpawnSpec,
} from '../wire/messages.ts'
import {
  CONTROL_STREAM_ID,
  DEFAULT_WORD_DELIMITERS,
  PUSH_REQ_ID,
  SubFlags,
  type CellPos,
  type CellPx,
  type CellSpan,
  type CommandRecord,
  type CursorShape,
  type ErrorCode,
  type FrameCredit,
  type GridSize,
  type ImageBlobData,
  type ImageEncoding,
  type ImagePlacement,
  type KeyInput,
  type LeaseKind,
  type MetricsSnapshot,
  type PackedRow,
  type PasteOutcome,
  type PointerInput,
  type SessionEvent,
  type SignalKind,
  type SignalScope,
  type SourceSink,
  type TerminalSource,
  type ViewportHint,
  type WaitCondition,
  type WaitOutcome,
} from '../source.ts'

/** Minimal byte-oriented transport connecting `TerminalClient` to a local socket, IPC bridge, or SSH proxy. */
export interface ByteStream {
  write(bytes: Uint8Array): void
  onData(listener: (chunk: Uint8Array) => void): () => void
  onClose(listener: () => void): () => void
  close(): void
}

/** Structured error returned by the broker for a correlated request or fatal connection error. */
export class BrokerError extends Error {
  readonly code: ErrorCode
  constructor(code: ErrorCode, message: string) {
    super(`${code}: ${message}`)
    this.name = 'BrokerError'
    this.code = code
  }
}

/** Thrown when an in-flight RPC fails because the connection closed or a stream detached. */
export class DisconnectedError extends Error {
  constructor(message = 'connection closed before broker answered') {
    super(message)
    this.name = 'DisconnectedError'
  }
}

export interface ConnectOptions {
  readonly clientName?: string
  /** Optional 32-byte shared secret for mutual HMAC-SHA256 authentication. */
  readonly authSecret?: Uint8Array
  /** Optional deterministic nonce supplier (defaults to `crypto.getRandomValues`). */
  readonly nonce?: () => Uint8Array
  /** Optional monotonic millisecond clock for BDP RTT EWMA (defaults to `performance.now`). */
  readonly nowMs?: () => number
}

export interface AttachOptions {
  readonly flags?: number
  readonly initialViewport?: ViewportHint | null
  readonly initialOutputCredit?: number
  readonly initialFrameCredit?: FrameCredit
  /** Maximum in-flight frames when visible (default `2` local / up to `4` over high-RTT SSH). */
  readonly maxInFlightFrames?: number
  /** Maximum in-flight frame payload bytes (default `262_144` = 256 KiB). */
  readonly maxInFlightBytes?: number
}

interface PendingRpc {
  readonly streamId: number
  readonly resolve: (msg: BrokerMsg) => void
  readonly reject: (err: Error) => void
}

interface AttachedStream {
  readonly hasRawOutput: boolean
  onMessage(msg: BrokerMsg): void
  onClosed(reason: string): void
}

const DEFAULT_MAX_FRAMES = 2
const DEFAULT_MAX_BYTES = 262_144
const IMAGE_PIPELINE_WINDOW = 8
const MAX_IMAGE_BLOB_BYTES = 64 * 1024 * 1024

/**
 * Single-connection multiplexed client speaking the unified v2 binary protocol.
 */
export class TerminalClient {
  readonly wordDelimiters: string
  readonly capabilities: number
  private readonly stream: ByteStream
  private readonly decoder = new FrameDecoder()
  private readonly pending = new Map<number, PendingRpc>()
  private readonly streams = new Map<number, AttachedStream>()
  private readonly unsubData: () => void
  private readonly unsubClose: () => void
  private readonly nowMs: () => number
  private nextReqId = 1
  private nextStreamId = 1
  private closed = false

  private constructor(
    stream: ByteStream,
    wordDelimiters: string,
    capabilities: number,
    nowMs: () => number,
    decoder: FrameDecoder,
    unsubData: () => void,
    unsubClose: () => void,
  ) {
    this.stream = stream
    this.wordDelimiters = wordDelimiters
    this.capabilities = capabilities
    this.nowMs = nowMs
    this.decoder = decoder
    this.unsubData = unsubData
    this.unsubClose = unsubClose
  }

  /** Connects over `stream`, completes `Hello`/`HelloOk` (+ mutual HMAC if required), and returns a live client. */
  static async connect(stream: ByteStream, options: ConnectOptions = {}): Promise<TerminalClient> {
    const clientName = options.clientName ?? 'terminal-view-v2'
    const authSecret = options.authSecret
    if (authSecret !== undefined && authSecret.length !== 32) {
      throw new Error('authSecret must be exactly 32 bytes')
    }
    const clientNonce = (options.nonce ?? randomNonce32)()
    if (clientNonce.length !== 32) throw new Error('clientNonce must be 32 bytes')
    const nowMs = options.nowMs ?? (() => (typeof performance !== 'undefined' ? performance.now() : Date.now()))

    const decoder = new FrameDecoder()
    let instance: TerminalClient | null = null
    type HandshakeItem =
      | { readonly kind: 'msg'; readonly header: FrameHeader; readonly msg: BrokerMsg }
      | { readonly kind: 'fin' }
    const handshakeQueue: HandshakeItem[] = []
    let handshakeWaiter: ((item: HandshakeItem) => void) | null = null
    let handshakeReject: ((err: Error) => void) | null = null
    let earlyError: Error | null = null

    const failEarly = (err: Error) => {
      if (earlyError !== null) return
      earlyError = err
      if (handshakeReject) {
        const rej = handshakeReject
        handshakeWaiter = null
        handshakeReject = null
        rej(err)
      }
    }

    const pushHandshakeItem = (item: HandshakeItem) => {
      if (handshakeWaiter) {
        const w = handshakeWaiter
        handshakeWaiter = null
        handshakeReject = null
        w(item)
      } else {
        handshakeQueue.push(item)
      }
    }

    const unsubData = stream.onData((chunk) => {
      if (instance !== null) {
        instance.onBytes(chunk)
        return
      }
      try {
        const frames = decoder.push(chunk)
        for (const f of frames) {
          if (f.kind === 'fin') {
            pushHandshakeItem({ kind: 'fin' })
            return
          }
          const msg = decodeBrokerMsg(f.header.msgType, f.payload)
          pushHandshakeItem({ kind: 'msg', header: f.header, msg })
        }
      } catch (err) {
        failEarly(err instanceof Error ? err : new Error(String(err)))
        stream.close()
      }
    })

    const unsubClose = stream.onClose(() => {
      if (instance) {
        instance.handleClosed('transport closed')
      } else {
        failEarly(new DisconnectedError('transport closed during handshake'))
      }
    })

    const nextHandshakeMsg = (expectedReqId: number): Promise<BrokerMsg> => {
      const consume = (item: HandshakeItem): BrokerMsg => {
        if (item.kind === 'fin') {
          throw new DisconnectedError('broker sent FIN during handshake')
        }
        if (item.header.streamId !== CONTROL_STREAM_ID || item.header.reqId !== expectedReqId) {
          throw new WireError(
            'InvalidValue',
            `unexpected handshake header (stream=${item.header.streamId}, req=${item.header.reqId})`,
          )
        }
        return item.msg
      }
      const queued = handshakeQueue.shift()
      if (queued) {
        try {
          return Promise.resolve(consume(queued))
        } catch (err) {
          return Promise.reject(err)
        }
      }
      if (earlyError !== null) return Promise.reject(earlyError)
      return new Promise((resolve, reject) => {
        handshakeWaiter = (item) => {
          try {
            resolve(consume(item))
          } catch (err) {
            reject(err instanceof Error ? err : new Error(String(err)))
          }
        }
        handshakeReject = reject
      })
    }

    try {
      // 1. Send Hello (reqId = 1)
      stream.write(
        encodeClientFrame(CONTROL_STREAM_ID, 1, {
          kind: 'hello',
          protocolVersion: PROTOCOL_VERSION,
          clientNonce,
          clientName,
        }),
      )
      const first = await nextHandshakeMsg(1)
      if (first.kind === 'error') throw new BrokerError(first.code, first.message)
      if (first.kind !== 'helloOk') {
        throw new WireError('InvalidTag', `expected helloOk, got ${first.kind}`)
      }
      if (first.protocolVersion !== PROTOCOL_VERSION) {
        throw new WireError('InvalidValue', `protocol version mismatch: ${first.protocolVersion}`)
      }
      // Reject auth downgrade: if caller configured a secret, broker MUST require auth.
      if (authSecret !== undefined && !first.authRequired) {
        throw new BrokerError('unauthorized', 'broker downgraded authentication to none')
      }
      if (first.authRequired) {
        if (authSecret === undefined) {
          throw new BrokerError('unauthorized', 'broker requires authentication but no authSecret was provided')
        }
        const proof = computeClientProof(authSecret, clientNonce, first.serverNonce)
        stream.write(encodeClientFrame(CONTROL_STREAM_ID, 2, { kind: 'authProof', hmac: proof }))
        const second = await nextHandshakeMsg(2)
        if (second.kind === 'error') throw new BrokerError(second.code, second.message)
        if (second.kind !== 'authOk') {
          throw new WireError('InvalidTag', `expected authOk, got ${second.kind}`)
        }
        if (!verifyServerProof(authSecret, first.serverNonce, clientNonce, second.serverHmac)) {
          throw new BrokerError('unauthorized', 'broker failed mutual HMAC verification')
        }
      }

      if (earlyError !== null) throw earlyError

      const client = new TerminalClient(
        stream,
        first.wordDelimiters || DEFAULT_WORD_DELIMITERS,
        first.capabilities,
        nowMs,
        decoder,
        unsubData,
        unsubClose,
      )
      client.nextReqId = 3
      instance = client
      // Deliver any frames/FIN that arrived in the same read chunk right after HelloOk/AuthOk
      while (handshakeQueue.length > 0 && !client.closed) {
        const extra = handshakeQueue.shift()!
        if (extra.kind === 'fin') {
          client.handleClosed('broker sent FIN')
          break
        }
        client.dispatchFrame(extra.header.streamId, extra.header.reqId, extra.msg)
      }
      return client
    } catch (err) {
      unsubData()
      unsubClose()
      stream.close()
      throw err
    }
  }

  get isClosed(): boolean {
    return this.closed
  }

  private allocReqId(): number {
    for (let tries = 0; tries < 65_536; tries += 1) {
      const id = this.nextReqId
      this.nextReqId = id === 0xffff_ffff ? 1 : id + 1
      if (!this.pending.has(id)) return id
    }
    throw new WireError('LimitExceeded', 'too many concurrent in-flight requests')
  }

  private allocStreamId(): number {
    for (let tries = 0; tries < 65_536; tries += 1) {
      const id = this.nextStreamId
      this.nextStreamId = id === 0xffff_ffff ? 1 : id + 1
      if (!this.streams.has(id)) return id
    }
    throw new WireError('LimitExceeded', 'too many concurrent attached streams')
  }

  /** Sends a fire-and-forget frame (`reqId = 0`). */
  sendPush(streamId: number, msg: ClientMsg): void {
    if (this.closed) return
    this.stream.write(encodeClientFrame(streamId, PUSH_REQ_ID, msg))
  }

  /** Sends a correlated RPC and resolves with its reply `BrokerMsg` (rejecting on `BrokerError` or close). */
  request(streamId: number, msg: ClientMsg): Promise<BrokerMsg> {
    if (this.closed) return Promise.reject(new DisconnectedError())
    const reqId = this.allocReqId()
    return new Promise<BrokerMsg>((resolve, reject) => {
      this.pending.set(reqId, { streamId, resolve, reject })
      try {
        this.stream.write(encodeClientFrame(streamId, reqId, msg))
      } catch (err) {
        this.pending.delete(reqId)
        reject(err instanceof Error ? err : new Error(String(err)))
      }
    })
  }

  private onBytes(chunk: Uint8Array): void {
    if (this.closed) return
    try {
      const frames = this.decoder.push(chunk)
      for (const f of frames) {
        if (f.kind === 'fin') {
          this.handleClosed('broker sent FIN')
          return
        }
        const msg = decodeBrokerMsg(f.header.msgType, f.payload)
        this.dispatchFrame(f.header.streamId, f.header.reqId, msg)
      }
    } catch (err) {
      this.handleClosed(err instanceof Error ? err.message : String(err))
      this.stream.close()
    }
  }

  private dispatchFrame(streamId: number, reqId: number, msg: BrokerMsg): void {
    if (reqId !== PUSH_REQ_ID) {
      const entry = this.pending.get(reqId)
      if (entry) {
        if (entry.streamId !== streamId) {
          this.handleClosed(`protocol error: reply reqId ${reqId} arrived on stream ${streamId}, expected ${entry.streamId}`)
          this.stream.close()
          return
        }
        this.pending.delete(reqId)
        if (msg.kind === 'error') {
          entry.reject(new BrokerError(msg.code, msg.message))
        } else {
          entry.resolve(msg)
        }
        return
      }
    }
    if (streamId !== CONTROL_STREAM_ID) {
      const st = this.streams.get(streamId)
      if (st) {
        st.onMessage(msg)
        if (msg.kind === 'streamClosed') {
          this.streams.delete(streamId)
          this.rejectPendingForStream(streamId, new DisconnectedError(msg.reason))
        } else if (msg.kind === 'exited' && !st.hasRawOutput) {
          this.streams.delete(streamId)
          const reason = msg.signal ?? (msg.exitCode !== null ? `exited (${msg.exitCode})` : 'session ended')
          this.rejectPendingForStream(streamId, new DisconnectedError(reason))
        }
      }
      return
    }
    if (msg.kind === 'error') {
      this.handleClosed(`${msg.code}: ${msg.message}`)
      this.stream.close()
    }
  }

  private rejectPendingForStream(streamId: number, err: Error): void {
    for (const [reqId, entry] of this.pending) {
      if (entry.streamId === streamId) {
        this.pending.delete(reqId)
        entry.reject(err)
      }
    }
  }

  private handleClosed(reason: string): void {
    if (this.closed) return
    this.closed = true
    this.unsubData()
    this.unsubClose()
    const err = new DisconnectedError(reason)
    for (const entry of this.pending.values()) entry.reject(err)
    this.pending.clear()
    for (const st of this.streams.values()) st.onClosed(reason)
    this.streams.clear()
  }

  /** Cleanly ends the connection by writing the universal 4-byte `FIN_FRAME` before closing. */
  close(): void {
    if (this.closed) return
    try {
      this.stream.write(encodeFin())
    } catch {
      // Transport may already be closed
    }
    this.handleClosed('client closed')
    this.stream.close()
  }

  // --- Control-plane RPCs (Stream 0) ---

  async acquireLease(lease: LeaseKind): Promise<{ readonly leaseId: number; readonly resumed: boolean }> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'acquireLease', lease })
    if (reply.kind !== 'leaseAck') throw new WireError('InvalidTag', `expected leaseAck, got ${reply.kind}`)
    return { leaseId: reply.leaseId, resumed: reply.resumed }
  }

  async releaseLease(leaseId: number): Promise<void> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'releaseLease', leaseId })
    if (reply.kind !== 'ok') throw new WireError('InvalidTag', `expected ok, got ${reply.kind}`)
  }

  async spawn(spec: SpawnSpec): Promise<{ readonly session: number; readonly pid: number | null }> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'spawn', spec })
    if (reply.kind !== 'spawned') throw new WireError('InvalidTag', `expected spawned, got ${reply.kind}`)
    return { session: reply.session, pid: reply.pid }
  }

  async kill(session: number): Promise<void> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'kill', session })
    if (reply.kind !== 'ok') throw new WireError('InvalidTag', `expected ok, got ${reply.kind}`)
  }

  async signal(session: number, signal: SignalKind, scope: SignalScope): Promise<void> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'signal', session, signal, scope })
    if (reply.kind !== 'ok') throw new WireError('InvalidTag', `expected ok, got ${reply.kind}`)
  }

  async owns(session: number, pid: number): Promise<boolean> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'owns', session, pid })
    if (reply.kind !== 'ownsReply') throw new WireError('InvalidTag', `expected ownsReply, got ${reply.kind}`)
    return reply.owned
  }

  async listSessions(): Promise<readonly SessionSummary[]> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'listSessions' })
    if (reply.kind !== 'sessionsList') throw new WireError('InvalidTag', `expected sessionsList, got ${reply.kind}`)
    return reply.sessions
  }

  async rewind(recordDir: string, atMs: number | null = null): Promise<{ readonly session: number; readonly durationMs: number; readonly atMs: number }> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'rewind', recordDir, atMs })
    if (reply.kind !== 'rewound') throw new WireError('InvalidTag', `expected rewound, got ${reply.kind}`)
    return { session: reply.session, durationMs: reply.durationMs, atMs: reply.atMs }
  }

  async seek(session: number, atMs: number): Promise<number> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'seek', session, atMs })
    if (reply.kind !== 'sought') throw new WireError('InvalidTag', `expected sought, got ${reply.kind}`)
    return reply.atMs
  }

  async metrics(): Promise<MetricsSnapshot> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'metrics' })
    if (reply.kind !== 'metricsReply') throw new WireError('InvalidTag', `expected metricsReply, got ${reply.kind}`)
    return reply.metrics
  }

  async keep(session: number, kept: boolean): Promise<void> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'keep', session, kept })
    if (reply.kind !== 'ok') throw new WireError('InvalidTag', `expected ok, got ${reply.kind}`)
  }

  async wait(session: number, condition: WaitCondition, timeoutMs: number): Promise<WaitOutcome> {
    const reply = await this.request(CONTROL_STREAM_ID, { kind: 'wait', session, condition, timeoutMs })
    if (reply.kind !== 'waited') throw new WireError('InvalidTag', `expected waited, got ${reply.kind}`)
    return reply.outcome
  }

  /** Attaches a multiplexed stream to `session` and returns a v2 `BrokerSessionSource`. */
  async attachSession(session: number, options: AttachOptions = {}): Promise<BrokerSessionSource> {
    const streamId = this.allocStreamId()
    const flags = options.flags ?? (SubFlags.VIEW | SubFlags.EVENTS)
    const maxFrames = options.maxInFlightFrames ?? options.initialFrameCredit?.maxFrames ?? DEFAULT_MAX_FRAMES
    const maxBytes = options.maxInFlightBytes ?? options.initialFrameCredit?.maxBytes ?? DEFAULT_MAX_BYTES
    const visible = options.initialViewport?.visible ?? true
    const initialFrameCredit: FrameCredit = { maxFrames: visible ? maxFrames : 0, maxBytes }

    const source = new BrokerSessionSource(
      this,
      session,
      streamId,
      maxFrames,
      maxBytes,
      visible,
      this.nowMs,
    )
    this.streams.set(streamId, source.handler((flags & SubFlags.RAW_OUTPUT) !== 0))

    try {
      const reply = await this.request(CONTROL_STREAM_ID, {
        kind: 'attach',
        session,
        streamId,
        flags,
        initialViewport: options.initialViewport ?? null,
        initialOutputCredit: options.initialOutputCredit ?? 0,
        initialFrameCredit,
      })
      if (reply.kind !== 'attached') throw new WireError('InvalidTag', `expected attached, got ${reply.kind}`)
      return source
    } catch (err) {
      this.streams.delete(streamId)
      throw err
    }
  }

  /** Internal helper called by `BrokerSessionSource.dispose()`. */
  detachStream(streamId: number): void {
    if (!this.streams.delete(streamId)) return
    this.rejectPendingForStream(streamId, new DisconnectedError('stream detached'))
    if (!this.closed) {
      void this.request(CONTROL_STREAM_ID, { kind: 'detach', streamId }).catch(() => {})
    }
  }
}

const MAX_BUFFERED_FRAMES = 8

/**
 * One multiplexed session attachment implementing `TerminalSource` with byte-bounded BDP
 * `FrameCredit` pacing and pipelined `64 KiB` `GetImageBlob` chunk assembly.
 */
export class BrokerSessionSource implements TerminalSource {
  private readonly sinks = new Set<SourceSink>()
  private readonly manualAckSinks = new Set<SourceSink>()
  private readonly bufferedFrames: FrameUpdate[] = []
  private lastSeq = 0
  private lastAckedSeq = 0
  private lastFrameAtMs: number | null = null
  private ewmaIntervalMs = 16
  private visible: boolean
  private everGrantedCredit: boolean
  private disposed = false

  constructor(
    private readonly client: TerminalClient,
    readonly sessionId: number,
    readonly streamId: number,
    private readonly maxFrames: number,
    private readonly maxBytes: number,
    visible: boolean,
    private readonly nowMs: () => number,
  ) {
    this.visible = visible
    this.everGrantedCredit = visible && maxFrames > 0
  }

  get wordDelimiters(): string {
    return this.client.wordDelimiters
  }

  /** Smoothed frame inter-arrival / ack interval estimate in milliseconds. */
  get estimatedFrameIntervalMs(): number {
    return this.ewmaIntervalMs
  }

  /** Current byte-bounded BDP credit advertised in `FrameAck` (`maxFrames = 0` when hidden). */
  currentCredit(): FrameCredit {
    if (!this.visible) return { maxFrames: 0, maxBytes: this.maxBytes }
    // Scale frames smoothly between 1 (tight local) and configured maxFrames (high-RTT SSH)
    const desiredFrames = this.ewmaIntervalMs >= 40 ? Math.min(4, Math.max(2, this.maxFrames)) : this.maxFrames
    return { maxFrames: desiredFrames, maxBytes: this.maxBytes }
  }

  private disposeLocal(): SourceSink[] {
    if (this.disposed) return []
    this.disposed = true
    const currentSinks = [...this.sinks]
    this.sinks.clear()
    this.manualAckSinks.clear()
    this.bufferedFrames.length = 0
    return currentSinks
  }

  private markClosed(reason: string): void {
    const currentSinks = this.disposeLocal()
    for (const sink of currentSinks) sink.onEvent?.({ kind: 'streamClosed', reason })
  }

  handler(hasRawOutput: boolean): AttachedStream {
    return {
      hasRawOutput,
      onMessage: (msg) => this.handleStreamMessage(msg, hasRawOutput),
      onClosed: (reason) => this.markClosed(reason),
    }
  }

  private handleStreamMessage(msg: BrokerMsg, hasRawOutput: boolean): void {
    switch (msg.kind) {
      case 'frame': {
        const now = this.nowMs()
        if (this.lastFrameAtMs !== null) {
          const dt = Math.max(1, now - this.lastFrameAtMs)
          this.ewmaIntervalMs = this.ewmaIntervalMs * 0.75 + dt * 0.25
        }
        this.lastFrameAtMs = now
        this.lastSeq = msg.frame.seq
        if (this.sinks.size === 0) {
          // Conflate & cap buffered frames while unsubscribed; hold ack until a sink subscribes
          if (msg.frame.fullSync) this.bufferedFrames.length = 0
          if (this.bufferedFrames.length >= MAX_BUFFERED_FRAMES) this.bufferedFrames.shift()
          this.bufferedFrames.push(msg.frame)
        } else {
          for (const sink of this.sinks) sink.onFrame(msg.frame)
          // When a view subscribes with `manualAck`, it calls `ackFrame(seq)` after painting so
          // frame credit paces cleanly to actual render speed. Otherwise auto-ack immediately.
          if (this.manualAckSinks.size === 0) {
            this.ackFrame(msg.frame.seq)
          }
        }
        break
      }
      case 'searchMatches':
        for (const sink of this.sinks) {
          sink.onSearchMatches({ epoch: msg.epoch, matches: msg.matches, truncated: msg.truncated })
        }
        break
      case 'searchDelta':
        for (const sink of this.sinks) {
          sink.onSearchDelta({
            epoch: msg.epoch,
            fromLine: msg.fromLine,
            toLine: msg.toLine,
            added: msg.added,
            truncated: msg.truncated,
          })
        }
        break
      case 'title':
      case 'bell':
      case 'clipboard':
      case 'notification':
      case 'progress':
      case 'programStatus':
      case 'programStatusCleared':
      case 'commandStarted':
      case 'commandFinished':
      case 'cwdChanged':
        for (const sink of this.sinks) sink.onEvent?.(msg as SessionEvent)
        break
      case 'exited': {
        // Without SUB_RAW_OUTPUT, `exited` is the final frame on this stream (no trailing
        // `streamClosed` follows): dispose locally so subsequent API calls reject cleanly and
        // sinks/buffers do not leak, without emitting a duplicate synthetic `streamClosed`.
        const currentSinks = hasRawOutput ? [...this.sinks] : this.disposeLocal()
        for (const sink of currentSinks) sink.onEvent?.(msg as SessionEvent)
        break
      }
      case 'streamClosed':
        this.markClosed(msg.reason)
        break
      case 'error':
        // Unsolicited stream-scoped errors (reqId === 0, e.g., fire-and-forget Key/Text/Pointer
        // writes rejected on an exited/rewound session or transient PTY stdin backpressure) are
        // non-fatal: the broker keeps the stream attached until an explicit `streamClosed`.
        break
      default:
        break
    }
  }

  subscribe(sink: SourceSink, options?: { readonly manualAck?: boolean }): () => void {
    if (this.disposed) return () => {}
    this.sinks.add(sink)
    if (options?.manualAck) this.manualAckSinks.add(sink)
    if (this.bufferedFrames.length > 0) {
      const frames = this.bufferedFrames.splice(0, this.bufferedFrames.length)
      for (const f of frames) sink.onFrame(f)
      if (this.manualAckSinks.size === 0 && this.lastSeq > this.lastAckedSeq) {
        this.ackFrame(this.lastSeq)
      }
    }
    return () => {
      this.sinks.delete(sink)
      const wasManual = this.manualAckSinks.delete(sink)
      if (wasManual && this.manualAckSinks.size === 0 && this.lastSeq > this.lastAckedSeq) {
        this.ackFrame(this.lastSeq)
      }
    }
  }

  ackFrame(seq: number): void {
    if (this.disposed) return
    const nextSeq = Math.max(this.lastAckedSeq, seq)
    this.lastAckedSeq = nextSeq
    const credit = this.currentCredit()
    if (credit.maxFrames > 0) this.everGrantedCredit = true
    this.client.sendPush(this.streamId, {
      kind: 'frameAck',
      seq: nextSeq,
      credit,
    })
  }

  sendViewportHint(hint: ViewportHint): void {
    if (this.disposed) return
    const visibilityChanged = this.visible !== hint.visible
    this.visible = hint.visible
    this.client.sendPush(this.streamId, { kind: 'viewportHint', hint })
    // Send an out-of-band FrameAck when visibility changes if either:
    // - at least one frame has been observed (`lastSeq > 0`), or
    // - we attached hidden (`!everGrantedCredit`, so the broker cannot have sent frame 1 yet)
    //   and just became visible for the first time, granting initial credit safely with `seq = 0`.
    if (visibilityChanged && (this.lastSeq > 0 || (hint.visible && !this.everGrantedCredit))) {
      this.ackFrame(this.lastSeq)
    }
  }

  private ensureActive(): void {
    if (this.disposed) throw new DisconnectedError('stream disposed')
  }

  async explicitResize(size: GridSize, cellPx: CellPx): Promise<void> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'explicitResize', size, cellPx })
    if (reply.kind !== 'ok') throw new WireError('InvalidTag', `expected ok, got ${reply.kind}`)
  }

  setCursorDefaults(shape: CursorShape, blinking: boolean): void {
    if (this.disposed) return
    this.client.sendPush(this.streamId, { kind: 'setCursorDefaults', shape, blinking })
  }

  sendKey(input: KeyInput): void {
    if (this.disposed) return
    this.client.sendPush(this.streamId, { kind: 'key', input })
  }

  sendText(text: string): void {
    if (this.disposed) return
    this.client.sendPush(this.streamId, { kind: 'text', text })
  }

  sendPointer(input: PointerInput): void {
    if (this.disposed) return
    this.client.sendPush(this.streamId, { kind: 'pointer', input })
  }

  sendFocus(focused: boolean): void {
    if (this.disposed) return
    this.client.sendPush(this.streamId, { kind: 'focus', focused })
  }

  async paste(text: string, allowUnsafe = false): Promise<PasteOutcome> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'paste', text, allowUnsafe })
    if (reply.kind !== 'pasted') throw new WireError('InvalidTag', `expected pasted, got ${reply.kind}`)
    return reply.outcome
  }

  async fetchRows(epoch: number, fromLine: number, count: number): Promise<readonly PackedRow[]> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'fetchRows', epoch, fromLine, count })
    if (reply.kind !== 'rows') throw new WireError('InvalidTag', `expected rows, got ${reply.kind}`)
    return reply.rows
  }

  async wordAt(epoch: number, pos: CellPos): Promise<CellSpan | null> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'wordAt', epoch, pos })
    if (reply.kind !== 'spanReply') throw new WireError('InvalidTag', `expected spanReply, got ${reply.kind}`)
    return reply.span
  }

  async lineAt(epoch: number, line: number): Promise<CellSpan | null> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'lineAt', epoch, line })
    if (reply.kind !== 'spanReply') throw new WireError('InvalidTag', `expected spanReply, got ${reply.kind}`)
    return reply.span
  }

  async textOf(epoch: number, span: CellSpan): Promise<string> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'textOf', epoch, span })
    if (reply.kind !== 'textReply') throw new WireError('InvalidTag', `expected textReply, got ${reply.kind}`)
    return reply.text
  }

  async textOfRect(epoch: number, span: CellSpan): Promise<string> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'textOfRect', epoch, span })
    if (reply.kind !== 'textReply') throw new WireError('InvalidTag', `expected textReply, got ${reply.kind}`)
    return reply.text
  }

  async setSearch(query: string, caseSensitive: boolean): Promise<{ readonly epoch: number; readonly matches: readonly CellSpan[]; readonly truncated: boolean }> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'setSearch', query, caseSensitive })
    if (reply.kind !== 'searchMatches') throw new WireError('InvalidTag', `expected searchMatches, got ${reply.kind}`)
    return { epoch: reply.epoch, matches: reply.matches, truncated: reply.truncated }
  }

  async commands(): Promise<readonly CommandRecord[]> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'commands' })
    if (reply.kind !== 'commandsReply') throw new WireError('InvalidTag', `expected commandsReply, got ${reply.kind}`)
    return reply.commands
  }

  async commandOutput(commandId: number): Promise<string | null> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'commandOutput', commandId })
    if (reply.kind !== 'commandOutputReply') {
      throw new WireError('InvalidTag', `expected commandOutputReply, got ${reply.kind}`)
    }
    return reply.text
  }

  async images(): Promise<{ readonly imagesGen: number; readonly placements: readonly ImagePlacement[] }> {
    this.ensureActive()
    const reply = await this.client.request(this.streamId, { kind: 'images' })
    if (reply.kind !== 'imagesReply') throw new WireError('InvalidTag', `expected imagesReply, got ${reply.kind}`)
    return { imagesGen: reply.imagesGen, placements: reply.placements }
  }

  /**
   * Downloads a content-addressed image blob in pipelined `64 KiB` chunks (up to 8 requests in
   * flight simultaneously so high-RTT SSH links stream images in 1–2 RTTs!).
   */
  async getImageBlob(blob: Uint8Array): Promise<ImageBlobData> {
    this.ensureActive()
    if (blob.length !== 32) throw new WireError('InvalidValue', 'image blob hash must be 32 bytes')
    const first = await this.requestImageChunk(blob, 0, BULK_CHUNK_BYTES)
    if (!ctEq32(first.blob, blob)) {
      throw new WireError('InvalidValue', 'image blob chunk hash mismatch')
    }
    const total = first.totalBytes
    if (total > MAX_IMAGE_BLOB_BYTES) {
      throw new WireError('LimitExceeded', `image blob ${total} bytes exceeds safety cap`)
    }
    const encoding: ImageEncoding = first.encoding
    if (first.offset !== 0) {
      throw new WireError('InvalidValue', `unexpected initial image chunk offset ${first.offset}`)
    }
    if (total === 0) {
      const empty = new Uint8Array(0)
      if (!ctEq32(sha256(empty), blob)) {
        throw new WireError('InvalidValue', 'image blob SHA-256 mismatch')
      }
      return { blob, encoding, bytes: empty }
    }
    const expectedFirst = Math.min(BULK_CHUNK_BYTES, total)
    if (first.data.length !== expectedFirst) {
      throw new WireError(
        'UnexpectedEof',
        `short initial image blob chunk: got ${first.data.length}, expected ${expectedFirst}`,
      )
    }
    if (first.data.length === total) {
      const single = first.data.slice()
      if (!ctEq32(sha256(single), blob)) {
        throw new WireError('InvalidValue', 'image blob SHA-256 mismatch')
      }
      return { blob, encoding, bytes: single }
    }

    const out = new Uint8Array(total)
    out.set(first.data, 0)

    const offsets: number[] = []
    for (let off = first.data.length; off < total; off += BULK_CHUNK_BYTES) {
      offsets.push(off)
    }

    let cursor = 0
    let aborted = false
    const worker = async (): Promise<void> => {
      try {
        while (!aborted && cursor < offsets.length) {
          const off = offsets[cursor++]!
          const expectedLen = Math.min(BULK_CHUNK_BYTES, total - off)
          const chunk = await this.requestImageChunk(blob, off, expectedLen)
          if (aborted) return
          if (
            !ctEq32(chunk.blob, blob)
            || chunk.encoding !== encoding
            || chunk.totalBytes !== total
            || chunk.offset !== off
          ) {
            throw new WireError('InvalidValue', 'inconsistent image blob chunk metadata')
          }
          if (chunk.data.length !== expectedLen) {
            throw new WireError(
              'UnexpectedEof',
              `short image blob chunk at ${off}: got ${chunk.data.length}, expected ${expectedLen}`,
            )
          }
          out.set(chunk.data, off)
        }
      } catch (err) {
        aborted = true
        throw err
      }
    }

    const workers: Promise<void>[] = []
    const concurrency = Math.min(IMAGE_PIPELINE_WINDOW, offsets.length)
    for (let i = 0; i < concurrency; i += 1) workers.push(worker())
    await Promise.all(workers)
    if (!ctEq32(sha256(out), blob)) {
      throw new WireError('InvalidValue', 'image blob SHA-256 mismatch')
    }
    return { blob, encoding, bytes: out }
  }

  private async requestImageChunk(blob: Uint8Array, offset: number, maxBytes: number) {
    const reply = await this.client.request(this.streamId, {
      kind: 'getImageBlob',
      blob,
      offset,
      maxBytes,
    })
    if (reply.kind !== 'imageBlobChunk') {
      throw new WireError('InvalidTag', `expected imageBlobChunk, got ${reply.kind}`)
    }
    return reply
  }

  dispose(): void {
    if (this.disposed) return
    this.disposed = true
    this.sinks.clear()
    this.manualAckSinks.clear()
    this.bufferedFrames.length = 0
    this.client.detachStream(this.streamId)
  }
}
