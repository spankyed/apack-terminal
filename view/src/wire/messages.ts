/**
 * Unified v2 binary protocol message encoders & decoders matching
 * `crates/apack-proto/src/messages.rs` byte-for-byte.
 */
import {
  BULK_CHUNK_BYTES,
  MAX_COMMANDS,
  MAX_ROWS_PER_FETCH,
  MAX_SEARCH_MATCHES,
  WireError,
  WireReader,
  WireWriter,
} from './codec.ts'
import { encodeFrame } from './frame.ts'
import {
  decodeBlobHash,
  decodeCellPos,
  decodeCellPx,
  decodeCellSpan,
  decodeCommandRecord,
  decodeCursorShape,
  decodeCursorState,
  decodeErrorCode,
  decodeForegroundProcess,
  decodeFrameCredit,
  decodeGridSize,
  decodeImageEncoding,
  decodeImagePlacement,
  decodeKeyInput,
  decodeLeaseKind,
  decodeMarksKey,
  decodeMetricsSnapshot,
  decodeNonZeroStreamId,
  decodePackedRow,
  decodePaletteOverrides,
  decodePasteOutcome,
  decodePointerInput,
  decodeProgressState,
  decodeResizePolicy,
  decodeSessionId,
  decodeSignalKind,
  decodeSignalScope,
  decodeSignalTarget,
  decodeStatusRecord,
  decodeSubFlags,
  decodeViewModes,
  decodeViewportHint,
  decodeWaitCondition,
  decodeWaitOutcome,
  encodeBlobHash,
  encodeCellPos,
  encodeCellPx,
  encodeCellSpan,
  encodeCommandRecord,
  encodeCursorShape,
  encodeCursorState,
  encodeErrorCode,
  encodeForegroundProcess,
  encodeFrameCredit,
  encodeGridSize,
  encodeImageEncoding,
  encodeImagePlacement,
  encodeKeyInput,
  encodeLeaseKind,
  encodeMarksKey,
  encodeMetricsSnapshot,
  encodeNonZeroStreamId,
  encodePackedRow,
  encodePaletteOverrides,
  encodePasteOutcome,
  encodePointerInput,
  encodeProgressState,
  encodeResizePolicy,
  encodeSessionId,
  encodeSignalKind,
  encodeSignalScope,
  encodeSignalTarget,
  encodeStatusRecord,
  encodeSubFlags,
  encodeViewModes,
  encodeViewportHint,
  encodeWaitCondition,
  encodeWaitOutcome,
  type CellPos,
  type CellPx,
  type CellSpan,
  type CommandRecord,
  type CursorShape,
  type CursorState,
  type ErrorCode,
  type ForegroundProcess,
  type FrameCredit,
  type GridSize,
  type ImageEncoding,
  type ImagePlacement,
  type KeyInput,
  type LeaseKind,
  type MetricsSnapshot,
  type PackedRow,
  type PaletteOverrides,
  type PasteOutcome,
  type PointerInput,
  type ProgressState,
  type ResizePolicy,
  type SignalKind,
  type SignalScope,
  type StatusRecord,
  type ViewModes,
  type ViewportHint,
  type WaitCondition,
  type WaitOutcome,
} from './types.ts'

export const MsgType = {
  // Client -> Broker control (Stream 0)
  HELLO: 0x0001,
  AUTH_PROOF: 0x0002,
  ACQUIRE_LEASE: 0x0003,
  RELEASE_LEASE: 0x0004,
  SPAWN: 0x0005,
  KILL: 0x0006,
  SIGNAL: 0x0007,
  OWNS: 0x0008,
  LIST_SESSIONS: 0x0009,
  REWIND: 0x000a,
  SEEK: 0x000b,
  METRICS: 0x000c,
  ATTACH: 0x000d,
  DETACH: 0x000e,
  KEEP: 0x000f,
  WAIT: 0x0010,

  // Client -> Broker per-session stream (Stream >= 1)
  FRAME_ACK: 0x0100,
  VIEWPORT_HINT: 0x0101,
  EXPLICIT_RESIZE: 0x0102,
  SET_CURSOR_DEFAULTS: 0x0103,
  KEY: 0x0104,
  TEXT: 0x0105,
  POINTER: 0x0106,
  FOCUS: 0x0107,
  PASTE: 0x0108,
  WRITE: 0x0109,
  PAUSE: 0x010a,
  RESUME: 0x010b,
  OUTPUT_CREDIT: 0x010c,
  FETCH_ROWS: 0x010d,
  WORD_AT: 0x010e,
  LINE_AT: 0x010f,
  TEXT_OF: 0x0110,
  TEXT_OF_RECT: 0x0111,
  SET_SEARCH: 0x0112,
  COMMANDS: 0x0113,
  COMMAND_OUTPUT: 0x0114,
  IMAGES: 0x0115,
  GET_IMAGE_BLOB: 0x0116,

  // Broker -> Client control replies
  HELLO_OK: 0x0200,
  AUTH_OK: 0x0201,
  ERROR: 0x0202,
  OK: 0x0203,
  LEASE_ACK: 0x0204,
  SPAWNED: 0x0205,
  OWNS_REPLY: 0x0206,
  SESSIONS_LIST: 0x0207,
  REWOUND: 0x0208,
  SOUGHT: 0x0209,
  METRICS_REPLY: 0x020a,
  ATTACHED: 0x020b,
  WAITED: 0x020c,

  // Broker -> Client per-session replies and push events
  FRAME: 0x0300,
  ROWS: 0x0301,
  PASTED: 0x0302,
  SPAN_REPLY: 0x0303,
  TEXT_REPLY: 0x0304,
  SEARCH_MATCHES: 0x0305,
  SEARCH_DELTA: 0x0306,
  COMMANDS_REPLY: 0x0307,
  COMMAND_OUTPUT_REPLY: 0x0308,
  IMAGES_REPLY: 0x0309,
  IMAGE_BLOB_CHUNK: 0x030a,
  TITLE: 0x0310,
  BELL: 0x0311,
  CLIPBOARD: 0x0312,
  NOTIFICATION: 0x0313,
  PROGRESS: 0x0314,
  PROGRAM_STATUS: 0x0315,
  COMMAND_STARTED: 0x0316,
  COMMAND_FINISHED: 0x0317,
  CWD_CHANGED: 0x0318,
  EXITED: 0x0319,
  OUTPUT: 0x0320,
  THROTTLED: 0x0321,
  OUTPUT_DROPPED: 0x0322,
  STREAM_CLOSED: 0x0323,
} as const

export interface SpawnSpec {
  readonly argv: readonly string[]
  readonly cwd: string | null
  readonly env: ReadonlyArray<readonly [string, string]>
  readonly size: GridSize
  readonly cellPx: CellPx
  readonly scrollbackLimit: number
  readonly marksKey: string | null
  readonly resizePolicy: ResizePolicy
  readonly recordDir: string | null
  readonly restoreFrom: string | null
  readonly imageFiles: boolean
  readonly kept: boolean
}

export interface SessionSummary {
  readonly id: number
  readonly size: GridSize
  readonly alive: boolean
  readonly kept: boolean
  readonly rewound: boolean
  readonly title: string
  readonly cwd: string | null
  readonly pid: number | null
  readonly foreground: ForegroundProcess | null
  readonly createdMs: number
}

export interface FrameUpdate {
  readonly seq: number
  readonly epoch: number
  readonly size: GridSize
  readonly firstActiveLine: number
  readonly scrollbackLines: number
  readonly scrollDelta: number
  readonly cursor: CursorState
  readonly modes: ViewModes
  readonly palette: PaletteOverrides | null
  readonly rows: readonly PackedRow[]
  readonly imagesGen: number
  readonly fullSync: boolean
}

export type ClientMsg =
  | { readonly kind: 'hello'; readonly protocolVersion: number; readonly clientNonce: Uint8Array; readonly clientName: string }
  | { readonly kind: 'authProof'; readonly hmac: Uint8Array }
  | { readonly kind: 'acquireLease'; readonly lease: LeaseKind }
  | { readonly kind: 'releaseLease'; readonly leaseId: number }
  | { readonly kind: 'spawn'; readonly spec: SpawnSpec }
  | { readonly kind: 'kill'; readonly session: number }
  | { readonly kind: 'signal'; readonly session: number; readonly signal: SignalKind; readonly scope: SignalScope }
  | { readonly kind: 'owns'; readonly session: number; readonly pid: number }
  | { readonly kind: 'listSessions' }
  | { readonly kind: 'rewind'; readonly recordDir: string; readonly atMs: number | null }
  | { readonly kind: 'seek'; readonly session: number; readonly atMs: number }
  | { readonly kind: 'metrics' }
  | {
      readonly kind: 'attach'
      readonly session: number
      readonly streamId: number
      readonly flags: number
      readonly initialViewport: ViewportHint | null
      readonly initialOutputCredit: number
      readonly initialFrameCredit: FrameCredit
    }
  | { readonly kind: 'detach'; readonly streamId: number }
  | { readonly kind: 'keep'; readonly session: number; readonly kept: boolean }
  | { readonly kind: 'wait'; readonly session: number; readonly condition: WaitCondition; readonly timeoutMs: number }
  | { readonly kind: 'frameAck'; readonly seq: number; readonly credit: FrameCredit }
  | { readonly kind: 'viewportHint'; readonly hint: ViewportHint }
  | { readonly kind: 'explicitResize'; readonly size: GridSize; readonly cellPx: CellPx }
  | { readonly kind: 'setCursorDefaults'; readonly shape: CursorShape; readonly blinking: boolean }
  | { readonly kind: 'key'; readonly input: KeyInput }
  | { readonly kind: 'text'; readonly text: string }
  | { readonly kind: 'pointer'; readonly input: PointerInput }
  | { readonly kind: 'focus'; readonly focused: boolean }
  | { readonly kind: 'paste'; readonly text: string; readonly allowUnsafe: boolean }
  | { readonly kind: 'write'; readonly data: Uint8Array }
  | { readonly kind: 'pause' }
  | { readonly kind: 'resume' }
  | { readonly kind: 'outputCredit'; readonly bytes: number }
  | { readonly kind: 'fetchRows'; readonly epoch: number; readonly fromLine: number; readonly count: number }
  | { readonly kind: 'wordAt'; readonly epoch: number; readonly pos: CellPos }
  | { readonly kind: 'lineAt'; readonly epoch: number; readonly line: number }
  | { readonly kind: 'textOf'; readonly epoch: number; readonly span: CellSpan }
  | { readonly kind: 'textOfRect'; readonly epoch: number; readonly span: CellSpan }
  | { readonly kind: 'setSearch'; readonly query: string; readonly caseSensitive: boolean }
  | { readonly kind: 'commands' }
  | { readonly kind: 'commandOutput'; readonly commandId: number }
  | { readonly kind: 'images' }
  | { readonly kind: 'getImageBlob'; readonly blob: Uint8Array; readonly offset: number; readonly maxBytes: number }

export type BrokerMsg =
  | {
      readonly kind: 'helloOk'
      readonly protocolVersion: number
      readonly authRequired: boolean
      readonly serverNonce: Uint8Array
      readonly wordDelimiters: string
      readonly capabilities: number
    }
  | { readonly kind: 'authOk'; readonly serverHmac: Uint8Array }
  | { readonly kind: 'error'; readonly code: ErrorCode; readonly message: string }
  | { readonly kind: 'ok' }
  | { readonly kind: 'leaseAck'; readonly leaseId: number; readonly resumed: boolean }
  | { readonly kind: 'spawned'; readonly session: number; readonly pid: number | null }
  | { readonly kind: 'ownsReply'; readonly owned: boolean }
  | { readonly kind: 'sessionsList'; readonly sessions: readonly SessionSummary[] }
  | { readonly kind: 'rewound'; readonly session: number; readonly durationMs: number; readonly atMs: number }
  | { readonly kind: 'sought'; readonly atMs: number }
  | { readonly kind: 'metricsReply'; readonly metrics: MetricsSnapshot }
  | {
      readonly kind: 'attached'
      readonly session: number
      readonly streamId: number
      readonly size: GridSize
      readonly epoch: number
      readonly firstActiveLine: number
      readonly scrollbackLines: number
      readonly foreground: ForegroundProcess | null
    }
  | { readonly kind: 'waited'; readonly outcome: WaitOutcome }
  | { readonly kind: 'frame'; readonly frame: FrameUpdate }
  | { readonly kind: 'rows'; readonly epoch: number; readonly rows: readonly PackedRow[] }
  | { readonly kind: 'pasted'; readonly outcome: PasteOutcome }
  | { readonly kind: 'spanReply'; readonly span: CellSpan | null }
  | { readonly kind: 'textReply'; readonly text: string }
  | { readonly kind: 'searchMatches'; readonly epoch: number; readonly matches: readonly CellSpan[]; readonly truncated: boolean }
  | {
      readonly kind: 'searchDelta'
      readonly epoch: number
      readonly fromLine: number
      readonly toLine: number
      readonly added: readonly CellSpan[]
      readonly truncated: boolean
    }
  | { readonly kind: 'commandsReply'; readonly commands: readonly CommandRecord[] }
  | { readonly kind: 'commandOutputReply'; readonly text: string | null }
  | { readonly kind: 'imagesReply'; readonly imagesGen: number; readonly placements: readonly ImagePlacement[] }
  | {
      readonly kind: 'imageBlobChunk'
      readonly blob: Uint8Array
      readonly encoding: ImageEncoding
      readonly totalBytes: number
      readonly offset: number
      readonly data: Uint8Array
    }
  | { readonly kind: 'title'; readonly title: string }
  | { readonly kind: 'bell' }
  | { readonly kind: 'clipboard'; readonly selection: string; readonly data: string }
  | { readonly kind: 'notification'; readonly title: string; readonly body: string }
  | { readonly kind: 'progress'; readonly state: ProgressState; readonly percent: number | null }
  | { readonly kind: 'programStatus'; readonly record: StatusRecord }
  | { readonly kind: 'commandStarted'; readonly record: CommandRecord }
  | { readonly kind: 'commandFinished'; readonly record: CommandRecord }
  | { readonly kind: 'cwdChanged'; readonly cwd: string }
  | { readonly kind: 'exited'; readonly exitCode: number | null; readonly signal: string | null }
  | { readonly kind: 'output'; readonly text: string }
  | { readonly kind: 'throttled'; readonly waitedUs: number }
  | { readonly kind: 'outputDropped'; readonly bytes: number }
  | { readonly kind: 'streamClosed'; readonly reason: string }

function encodeSpawnSpec(w: WireWriter, s: SpawnSpec): void {
  w.slice(s.argv, (wr, a) => wr.str(a))
  w.opt(s.cwd, (wr, v) => wr.str(v))
  w.slice(s.env, (wr, [k, v]) => {
    wr.str(k)
    wr.str(v)
  })
  encodeGridSize(w, s.size)
  encodeCellPx(w, s.cellPx)
  w.varU32(s.scrollbackLimit)
  w.opt(s.marksKey, encodeMarksKey)
  encodeResizePolicy(w, s.resizePolicy)
  w.opt(s.recordDir, (wr, v) => wr.str(v))
  w.opt(s.restoreFrom, (wr, v) => wr.str(v))
  w.bool(s.imageFiles)
  w.bool(s.kept)
}

function decodeSpawnSpec(r: WireReader): SpawnSpec {
  return {
    argv: r.vecBounded(1024, 'spawn argv', (rd) => rd.str()),
    cwd: r.opt((rd) => rd.str()),
    env: r.vecBounded(1024, 'spawn env', (rd) => [rd.str(), rd.str()] as const),
    size: decodeGridSize(r),
    cellPx: decodeCellPx(r),
    scrollbackLimit: r.varU32(),
    marksKey: r.opt(decodeMarksKey),
    resizePolicy: decodeResizePolicy(r),
    recordDir: r.opt((rd) => rd.str()),
    restoreFrom: r.opt((rd) => rd.str()),
    imageFiles: r.bool(),
    kept: r.bool(),
  }
}

function encodeSessionSummary(w: WireWriter, s: SessionSummary): void {
  encodeSessionId(w, s.id)
  encodeGridSize(w, s.size)
  w.bool(s.alive)
  w.bool(s.kept)
  w.bool(s.rewound)
  w.str(s.title)
  w.opt(s.cwd, (wr, v) => wr.str(v))
  w.opt(s.pid, encodeSignalTarget)
  w.opt(s.foreground, encodeForegroundProcess)
  w.varU64(s.createdMs)
}

function decodeSessionSummary(r: WireReader): SessionSummary {
  return {
    id: decodeSessionId(r),
    size: decodeGridSize(r),
    alive: r.bool(),
    kept: r.bool(),
    rewound: r.bool(),
    title: r.str(),
    cwd: r.opt((rd) => rd.str()),
    pid: r.opt(decodeSignalTarget),
    foreground: r.opt(decodeForegroundProcess),
    createdMs: r.varU64(),
  }
}

function encodeFrameUpdate(w: WireWriter, f: FrameUpdate): void {
  w.varU64(f.seq)
  w.varU64(f.epoch)
  encodeGridSize(w, f.size)
  w.varU64(f.firstActiveLine)
  w.varU64(f.scrollbackLines)
  w.i32(f.scrollDelta)
  encodeCursorState(w, f.cursor)
  encodeViewModes(w, f.modes)
  w.opt(f.palette, encodePaletteOverrides)
  w.slice(f.rows, encodePackedRow)
  w.varU64(f.imagesGen)
  w.bool(f.fullSync)
}

function decodeFrameUpdate(r: WireReader): FrameUpdate {
  const seq = r.varU64()
  const epoch = r.varU64()
  const size = decodeGridSize(r)
  const firstActiveLine = r.varU64()
  const scrollbackLines = r.varU64()
  if (scrollbackLines > firstActiveLine) {
    throw new WireError('InvalidValue', 'FrameUpdate.scrollback_lines > first_active_line')
  }
  return {
    seq,
    epoch,
    size,
    firstActiveLine,
    scrollbackLines,
    scrollDelta: r.i32(),
    cursor: decodeCursorState(r),
    modes: decodeViewModes(r),
    palette: r.opt(decodePaletteOverrides),
    rows: r.vecBounded(MAX_ROWS_PER_FETCH, 'frame rows', decodePackedRow),
    imagesGen: r.varU64(),
    fullSync: r.bool(),
  }
}

export function clientMsgType(msg: ClientMsg): number {
  switch (msg.kind) {
    case 'hello': return MsgType.HELLO
    case 'authProof': return MsgType.AUTH_PROOF
    case 'acquireLease': return MsgType.ACQUIRE_LEASE
    case 'releaseLease': return MsgType.RELEASE_LEASE
    case 'spawn': return MsgType.SPAWN
    case 'kill': return MsgType.KILL
    case 'signal': return MsgType.SIGNAL
    case 'owns': return MsgType.OWNS
    case 'listSessions': return MsgType.LIST_SESSIONS
    case 'rewind': return MsgType.REWIND
    case 'seek': return MsgType.SEEK
    case 'metrics': return MsgType.METRICS
    case 'attach': return MsgType.ATTACH
    case 'detach': return MsgType.DETACH
    case 'keep': return MsgType.KEEP
    case 'wait': return MsgType.WAIT
    case 'frameAck': return MsgType.FRAME_ACK
    case 'viewportHint': return MsgType.VIEWPORT_HINT
    case 'explicitResize': return MsgType.EXPLICIT_RESIZE
    case 'setCursorDefaults': return MsgType.SET_CURSOR_DEFAULTS
    case 'key': return MsgType.KEY
    case 'text': return MsgType.TEXT
    case 'pointer': return MsgType.POINTER
    case 'focus': return MsgType.FOCUS
    case 'paste': return MsgType.PASTE
    case 'write': return MsgType.WRITE
    case 'pause': return MsgType.PAUSE
    case 'resume': return MsgType.RESUME
    case 'outputCredit': return MsgType.OUTPUT_CREDIT
    case 'fetchRows': return MsgType.FETCH_ROWS
    case 'wordAt': return MsgType.WORD_AT
    case 'lineAt': return MsgType.LINE_AT
    case 'textOf': return MsgType.TEXT_OF
    case 'textOfRect': return MsgType.TEXT_OF_RECT
    case 'setSearch': return MsgType.SET_SEARCH
    case 'commands': return MsgType.COMMANDS
    case 'commandOutput': return MsgType.COMMAND_OUTPUT
    case 'images': return MsgType.IMAGES
    case 'getImageBlob': return MsgType.GET_IMAGE_BLOB
  }
}

export function encodeClientPayload(msg: ClientMsg): Uint8Array {
  const w = new WireWriter()
  switch (msg.kind) {
    case 'hello':
      w.u16(msg.protocolVersion)
      if (msg.clientNonce.length !== 32) throw new WireError('InvalidValue', 'clientNonce must be 32 bytes')
      w.raw(msg.clientNonce)
      w.str(msg.clientName)
      break
    case 'authProof':
      if (msg.hmac.length !== 32) throw new WireError('InvalidValue', 'hmac must be 32 bytes')
      w.raw(msg.hmac)
      break
    case 'acquireLease':
      encodeLeaseKind(w, msg.lease)
      break
    case 'releaseLease':
      w.varU64(msg.leaseId)
      break
    case 'spawn':
      encodeSpawnSpec(w, msg.spec)
      break
    case 'kill':
      encodeSessionId(w, msg.session)
      break
    case 'signal':
      encodeSessionId(w, msg.session)
      encodeSignalKind(w, msg.signal)
      encodeSignalScope(w, msg.scope)
      break
    case 'owns':
      encodeSessionId(w, msg.session)
      encodeSignalTarget(w, msg.pid)
      break
    case 'listSessions':
    case 'metrics':
    case 'pause':
    case 'resume':
    case 'commands':
    case 'images':
      break
    case 'rewind':
      w.str(msg.recordDir)
      w.opt(msg.atMs, (wr, ms) => wr.varU64(ms))
      break
    case 'seek':
      encodeSessionId(w, msg.session)
      w.varU64(msg.atMs)
      break
    case 'attach':
      encodeSessionId(w, msg.session)
      encodeNonZeroStreamId(w, msg.streamId)
      encodeSubFlags(w, msg.flags)
      w.opt(msg.initialViewport, encodeViewportHint)
      w.varU32(msg.initialOutputCredit)
      encodeFrameCredit(w, msg.initialFrameCredit)
      break
    case 'detach':
      encodeNonZeroStreamId(w, msg.streamId)
      break
    case 'keep':
      encodeSessionId(w, msg.session)
      w.bool(msg.kept)
      break
    case 'wait':
      encodeSessionId(w, msg.session)
      encodeWaitCondition(w, msg.condition)
      w.varU32(msg.timeoutMs)
      break
    case 'frameAck':
      w.varU64(msg.seq)
      encodeFrameCredit(w, msg.credit)
      break
    case 'viewportHint':
      encodeViewportHint(w, msg.hint)
      break
    case 'explicitResize':
      encodeGridSize(w, msg.size)
      encodeCellPx(w, msg.cellPx)
      break
    case 'setCursorDefaults':
      encodeCursorShape(w, msg.shape)
      w.bool(msg.blinking)
      break
    case 'key':
      encodeKeyInput(w, msg.input)
      break
    case 'text':
      w.str(msg.text)
      break
    case 'pointer':
      encodePointerInput(w, msg.input)
      break
    case 'focus':
      w.bool(msg.focused)
      break
    case 'paste':
      w.str(msg.text)
      w.bool(msg.allowUnsafe)
      break
    case 'write':
      w.bytes(msg.data)
      break
    case 'outputCredit':
      w.varU32(msg.bytes)
      break
    case 'fetchRows':
      w.varU64(msg.epoch)
      w.varU64(msg.fromLine)
      w.varU32(msg.count)
      break
    case 'wordAt':
      w.varU64(msg.epoch)
      encodeCellPos(w, msg.pos)
      break
    case 'lineAt':
      w.varU64(msg.epoch)
      w.varU64(msg.line)
      break
    case 'textOf':
    case 'textOfRect':
      w.varU64(msg.epoch)
      encodeCellSpan(w, msg.span)
      break
    case 'setSearch':
      w.str(msg.query)
      w.bool(msg.caseSensitive)
      break
    case 'commandOutput':
      w.varU64(msg.commandId)
      break
    case 'getImageBlob':
      encodeBlobHash(w, msg.blob)
      w.varU32(msg.offset)
      w.varU32(msg.maxBytes)
      break
  }
  return w.toBytes()
}

export function encodeClientFrame(streamId: number, reqId: number, msg: ClientMsg): Uint8Array {
  return encodeFrame(streamId, clientMsgType(msg), reqId, encodeClientPayload(msg))
}

export function decodeClientMsg(msgType: number, payload: Uint8Array): ClientMsg {
  const r = new WireReader(payload)
  let msg: ClientMsg
  switch (msgType) {
    case MsgType.HELLO:
      msg = { kind: 'hello', protocolVersion: r.u16(), clientNonce: r.fixedCopy(32), clientName: r.str() }
      break
    case MsgType.AUTH_PROOF:
      msg = { kind: 'authProof', hmac: r.fixedCopy(32) }
      break
    case MsgType.ACQUIRE_LEASE:
      msg = { kind: 'acquireLease', lease: decodeLeaseKind(r) }
      break
    case MsgType.RELEASE_LEASE:
      msg = { kind: 'releaseLease', leaseId: r.varU64() }
      break
    case MsgType.SPAWN:
      msg = { kind: 'spawn', spec: decodeSpawnSpec(r) }
      break
    case MsgType.KILL:
      msg = { kind: 'kill', session: decodeSessionId(r) }
      break
    case MsgType.SIGNAL:
      msg = { kind: 'signal', session: decodeSessionId(r), signal: decodeSignalKind(r), scope: decodeSignalScope(r) }
      break
    case MsgType.OWNS:
      msg = { kind: 'owns', session: decodeSessionId(r), pid: decodeSignalTarget(r) }
      break
    case MsgType.LIST_SESSIONS:
      msg = { kind: 'listSessions' }
      break
    case MsgType.REWIND:
      msg = { kind: 'rewind', recordDir: r.str(), atMs: r.opt((rd) => rd.varU64()) }
      break
    case MsgType.SEEK:
      msg = { kind: 'seek', session: decodeSessionId(r), atMs: r.varU64() }
      break
    case MsgType.METRICS:
      msg = { kind: 'metrics' }
      break
    case MsgType.ATTACH:
      msg = {
        kind: 'attach',
        session: decodeSessionId(r),
        streamId: decodeNonZeroStreamId(r),
        flags: decodeSubFlags(r),
        initialViewport: r.opt(decodeViewportHint),
        initialOutputCredit: r.varU32(),
        initialFrameCredit: decodeFrameCredit(r),
      }
      break
    case MsgType.DETACH:
      msg = { kind: 'detach', streamId: decodeNonZeroStreamId(r) }
      break
    case MsgType.KEEP:
      msg = { kind: 'keep', session: decodeSessionId(r), kept: r.bool() }
      break
    case MsgType.WAIT:
      msg = { kind: 'wait', session: decodeSessionId(r), condition: decodeWaitCondition(r), timeoutMs: r.varU32() }
      break
    case MsgType.FRAME_ACK:
      msg = { kind: 'frameAck', seq: r.varU64(), credit: decodeFrameCredit(r) }
      break
    case MsgType.VIEWPORT_HINT:
      msg = { kind: 'viewportHint', hint: decodeViewportHint(r) }
      break
    case MsgType.EXPLICIT_RESIZE:
      msg = { kind: 'explicitResize', size: decodeGridSize(r), cellPx: decodeCellPx(r) }
      break
    case MsgType.SET_CURSOR_DEFAULTS:
      msg = { kind: 'setCursorDefaults', shape: decodeCursorShape(r), blinking: r.bool() }
      break
    case MsgType.KEY:
      msg = { kind: 'key', input: decodeKeyInput(r) }
      break
    case MsgType.TEXT:
      msg = { kind: 'text', text: r.str() }
      break
    case MsgType.POINTER:
      msg = { kind: 'pointer', input: decodePointerInput(r) }
      break
    case MsgType.FOCUS:
      msg = { kind: 'focus', focused: r.bool() }
      break
    case MsgType.PASTE:
      msg = { kind: 'paste', text: r.str(), allowUnsafe: r.bool() }
      break
    case MsgType.WRITE:
      msg = { kind: 'write', data: r.bytes() }
      break
    case MsgType.PAUSE:
      msg = { kind: 'pause' }
      break
    case MsgType.RESUME:
      msg = { kind: 'resume' }
      break
    case MsgType.OUTPUT_CREDIT:
      msg = { kind: 'outputCredit', bytes: r.varU32() }
      break
    case MsgType.FETCH_ROWS: {
      const epoch = r.varU64()
      const fromLine = r.varU64()
      const count = r.varU32()
      if (count === 0 || count > MAX_ROWS_PER_FETCH) throw new WireError('LimitExceeded', 'fetch rows count')
      msg = { kind: 'fetchRows', epoch, fromLine, count }
      break
    }
    case MsgType.WORD_AT:
      msg = { kind: 'wordAt', epoch: r.varU64(), pos: decodeCellPos(r) }
      break
    case MsgType.LINE_AT:
      msg = { kind: 'lineAt', epoch: r.varU64(), line: r.varU64() }
      break
    case MsgType.TEXT_OF:
      msg = { kind: 'textOf', epoch: r.varU64(), span: decodeCellSpan(r) }
      break
    case MsgType.TEXT_OF_RECT:
      msg = { kind: 'textOfRect', epoch: r.varU64(), span: decodeCellSpan(r) }
      break
    case MsgType.SET_SEARCH:
      msg = { kind: 'setSearch', query: r.str(), caseSensitive: r.bool() }
      break
    case MsgType.COMMANDS:
      msg = { kind: 'commands' }
      break
    case MsgType.COMMAND_OUTPUT:
      msg = { kind: 'commandOutput', commandId: r.varU64() }
      break
    case MsgType.IMAGES:
      msg = { kind: 'images' }
      break
    case MsgType.GET_IMAGE_BLOB: {
      const blob = decodeBlobHash(r)
      const offset = r.varU32()
      const maxBytes = r.varU32()
      if (maxBytes === 0) throw new WireError('InvalidValue', 'get_image_blob max_bytes must be non-zero')
      msg = { kind: 'getImageBlob', blob, offset, maxBytes }
      break
    }
    default:
      throw new WireError('InvalidTag', `invalid ClientMsg wire tag ${msgType}`)
  }
  r.finish()
  return msg
}

export function brokerMsgType(msg: BrokerMsg): number {
  switch (msg.kind) {
    case 'helloOk': return MsgType.HELLO_OK
    case 'authOk': return MsgType.AUTH_OK
    case 'error': return MsgType.ERROR
    case 'ok': return MsgType.OK
    case 'leaseAck': return MsgType.LEASE_ACK
    case 'spawned': return MsgType.SPAWNED
    case 'ownsReply': return MsgType.OWNS_REPLY
    case 'sessionsList': return MsgType.SESSIONS_LIST
    case 'rewound': return MsgType.REWOUND
    case 'sought': return MsgType.SOUGHT
    case 'metricsReply': return MsgType.METRICS_REPLY
    case 'attached': return MsgType.ATTACHED
    case 'waited': return MsgType.WAITED
    case 'frame': return MsgType.FRAME
    case 'rows': return MsgType.ROWS
    case 'pasted': return MsgType.PASTED
    case 'spanReply': return MsgType.SPAN_REPLY
    case 'textReply': return MsgType.TEXT_REPLY
    case 'searchMatches': return MsgType.SEARCH_MATCHES
    case 'searchDelta': return MsgType.SEARCH_DELTA
    case 'commandsReply': return MsgType.COMMANDS_REPLY
    case 'commandOutputReply': return MsgType.COMMAND_OUTPUT_REPLY
    case 'imagesReply': return MsgType.IMAGES_REPLY
    case 'imageBlobChunk': return MsgType.IMAGE_BLOB_CHUNK
    case 'title': return MsgType.TITLE
    case 'bell': return MsgType.BELL
    case 'clipboard': return MsgType.CLIPBOARD
    case 'notification': return MsgType.NOTIFICATION
    case 'progress': return MsgType.PROGRESS
    case 'programStatus': return MsgType.PROGRAM_STATUS
    case 'commandStarted': return MsgType.COMMAND_STARTED
    case 'commandFinished': return MsgType.COMMAND_FINISHED
    case 'cwdChanged': return MsgType.CWD_CHANGED
    case 'exited': return MsgType.EXITED
    case 'output': return MsgType.OUTPUT
    case 'throttled': return MsgType.THROTTLED
    case 'outputDropped': return MsgType.OUTPUT_DROPPED
    case 'streamClosed': return MsgType.STREAM_CLOSED
  }
}

export function encodeBrokerPayload(msg: BrokerMsg): Uint8Array {
  const w = new WireWriter()
  switch (msg.kind) {
    case 'helloOk':
      w.u16(msg.protocolVersion)
      w.bool(msg.authRequired)
      if (msg.serverNonce.length !== 32) throw new WireError('InvalidValue', 'serverNonce must be 32 bytes')
      w.raw(msg.serverNonce)
      w.str(msg.wordDelimiters)
      w.u32(msg.capabilities)
      break
    case 'authOk':
      if (msg.serverHmac.length !== 32) throw new WireError('InvalidValue', 'serverHmac must be 32 bytes')
      w.raw(msg.serverHmac)
      break
    case 'error':
      encodeErrorCode(w, msg.code)
      w.str(msg.message)
      break
    case 'ok':
    case 'bell':
      break
    case 'leaseAck':
      w.varU64(msg.leaseId)
      w.bool(msg.resumed)
      break
    case 'spawned':
      encodeSessionId(w, msg.session)
      w.opt(msg.pid, encodeSignalTarget)
      break
    case 'ownsReply':
      w.bool(msg.owned)
      break
    case 'sessionsList':
      w.slice(msg.sessions, encodeSessionSummary)
      break
    case 'rewound':
      encodeSessionId(w, msg.session)
      w.varU64(msg.durationMs)
      w.varU64(msg.atMs)
      break
    case 'sought':
      w.varU64(msg.atMs)
      break
    case 'metricsReply':
      encodeMetricsSnapshot(w, msg.metrics)
      break
    case 'attached':
      encodeSessionId(w, msg.session)
      encodeNonZeroStreamId(w, msg.streamId)
      encodeGridSize(w, msg.size)
      w.varU64(msg.epoch)
      w.varU64(msg.firstActiveLine)
      w.varU64(msg.scrollbackLines)
      w.opt(msg.foreground, encodeForegroundProcess)
      break
    case 'waited':
      encodeWaitOutcome(w, msg.outcome)
      break
    case 'frame':
      encodeFrameUpdate(w, msg.frame)
      break
    case 'rows':
      w.varU64(msg.epoch)
      w.slice(msg.rows, encodePackedRow)
      break
    case 'pasted':
      encodePasteOutcome(w, msg.outcome)
      break
    case 'spanReply':
      w.opt(msg.span, encodeCellSpan)
      break
    case 'textReply':
      w.str(msg.text)
      break
    case 'searchMatches':
      w.varU64(msg.epoch)
      w.slice(msg.matches, encodeCellSpan)
      w.bool(msg.truncated)
      break
    case 'searchDelta':
      w.varU64(msg.epoch)
      w.varU64(msg.fromLine)
      w.varU64(msg.toLine)
      w.slice(msg.added, encodeCellSpan)
      w.bool(msg.truncated)
      break
    case 'commandsReply':
      w.slice(msg.commands, encodeCommandRecord)
      break
    case 'commandOutputReply':
      w.opt(msg.text, (wr, s) => wr.str(s))
      break
    case 'imagesReply':
      w.varU64(msg.imagesGen)
      w.slice(msg.placements, encodeImagePlacement)
      break
    case 'imageBlobChunk':
      encodeBlobHash(w, msg.blob)
      encodeImageEncoding(w, msg.encoding)
      w.varU32(msg.totalBytes)
      w.varU32(msg.offset)
      w.bytes(msg.data)
      break
    case 'title':
      w.str(msg.title)
      break
    case 'clipboard':
      w.str(msg.selection)
      w.str(msg.data)
      break
    case 'notification':
      w.str(msg.title)
      w.str(msg.body)
      break
    case 'progress':
      encodeProgressState(w, msg.state)
      w.opt(msg.percent, (wr, p) => wr.u8(p))
      break
    case 'programStatus':
      encodeStatusRecord(w, msg.record)
      break
    case 'commandStarted':
    case 'commandFinished':
      encodeCommandRecord(w, msg.record)
      break
    case 'cwdChanged':
      w.str(msg.cwd)
      break
    case 'exited':
      w.opt(msg.exitCode, (wr, c) => wr.i32(c))
      w.opt(msg.signal, (wr, s) => wr.str(s))
      break
    case 'output':
      w.str(msg.text)
      break
    case 'throttled':
      w.varU64(msg.waitedUs)
      break
    case 'outputDropped':
      w.varU64(msg.bytes)
      break
    case 'streamClosed':
      w.str(msg.reason)
      break
  }
  return w.toBytes()
}

export function encodeBrokerFrame(streamId: number, reqId: number, msg: BrokerMsg): Uint8Array {
  return encodeFrame(streamId, brokerMsgType(msg), reqId, encodeBrokerPayload(msg))
}

export function decodeBrokerMsg(msgType: number, payload: Uint8Array): BrokerMsg {
  const r = new WireReader(payload)
  let msg: BrokerMsg
  switch (msgType) {
    case MsgType.HELLO_OK:
      msg = {
        kind: 'helloOk',
        protocolVersion: r.u16(),
        authRequired: r.bool(),
        serverNonce: r.fixedCopy(32),
        wordDelimiters: r.str(),
        capabilities: r.u32(),
      }
      break
    case MsgType.AUTH_OK:
      msg = { kind: 'authOk', serverHmac: r.fixedCopy(32) }
      break
    case MsgType.ERROR:
      msg = { kind: 'error', code: decodeErrorCode(r), message: r.str() }
      break
    case MsgType.OK:
      msg = { kind: 'ok' }
      break
    case MsgType.LEASE_ACK:
      msg = { kind: 'leaseAck', leaseId: r.varU64(), resumed: r.bool() }
      break
    case MsgType.SPAWNED:
      msg = { kind: 'spawned', session: decodeSessionId(r), pid: r.opt(decodeSignalTarget) }
      break
    case MsgType.OWNS_REPLY:
      msg = { kind: 'ownsReply', owned: r.bool() }
      break
    case MsgType.SESSIONS_LIST:
      msg = { kind: 'sessionsList', sessions: r.vecBounded(4096, 'sessions list', decodeSessionSummary) }
      break
    case MsgType.REWOUND:
      msg = { kind: 'rewound', session: decodeSessionId(r), durationMs: r.varU64(), atMs: r.varU64() }
      break
    case MsgType.SOUGHT:
      msg = { kind: 'sought', atMs: r.varU64() }
      break
    case MsgType.METRICS_REPLY:
      msg = { kind: 'metricsReply', metrics: decodeMetricsSnapshot(r) }
      break
    case MsgType.ATTACHED: {
      const session = decodeSessionId(r)
      const streamId = decodeNonZeroStreamId(r)
      const size = decodeGridSize(r)
      const epoch = r.varU64()
      const firstActiveLine = r.varU64()
      const scrollbackLines = r.varU64()
      if (scrollbackLines > firstActiveLine) {
        throw new WireError('InvalidValue', 'Attached.scrollback_lines > first_active_line')
      }
      const foreground = r.opt(decodeForegroundProcess)
      msg = { kind: 'attached', session, streamId, size, epoch, firstActiveLine, scrollbackLines, foreground }
      break
    }
    case MsgType.WAITED:
      msg = { kind: 'waited', outcome: decodeWaitOutcome(r) }
      break
    case MsgType.FRAME:
      msg = { kind: 'frame', frame: decodeFrameUpdate(r) }
      break
    case MsgType.ROWS:
      msg = { kind: 'rows', epoch: r.varU64(), rows: r.vecBounded(MAX_ROWS_PER_FETCH, 'rows reply', decodePackedRow) }
      break
    case MsgType.PASTED:
      msg = { kind: 'pasted', outcome: decodePasteOutcome(r) }
      break
    case MsgType.SPAN_REPLY:
      msg = { kind: 'spanReply', span: r.opt(decodeCellSpan) }
      break
    case MsgType.TEXT_REPLY:
      msg = { kind: 'textReply', text: r.str() }
      break
    case MsgType.SEARCH_MATCHES:
      msg = {
        kind: 'searchMatches',
        epoch: r.varU64(),
        matches: r.vecBounded(MAX_SEARCH_MATCHES, 'search matches', decodeCellSpan),
        truncated: r.bool(),
      }
      break
    case MsgType.SEARCH_DELTA: {
      const epoch = r.varU64()
      const fromLine = r.varU64()
      const toLine = r.varU64()
      if (toLine < fromLine) throw new WireError('InvalidValue', 'search delta to_line < from_line')
      const added = r.vecBounded(MAX_SEARCH_MATCHES, 'search delta', decodeCellSpan)
      for (const span of added) {
        if (span.start.line < fromLine || span.end.line >= toLine) {
          throw new WireError('InvalidValue', 'search delta span outside [from_line, to_line)')
        }
      }
      msg = { kind: 'searchDelta', epoch, fromLine, toLine, added, truncated: r.bool() }
      break
    }
    case MsgType.COMMANDS_REPLY:
      msg = { kind: 'commandsReply', commands: r.vecBounded(MAX_COMMANDS, 'commands', decodeCommandRecord) }
      break
    case MsgType.COMMAND_OUTPUT_REPLY:
      msg = { kind: 'commandOutputReply', text: r.opt((rd) => rd.str()) }
      break
    case MsgType.IMAGES_REPLY:
      msg = {
        kind: 'imagesReply',
        imagesGen: r.varU64(),
        placements: r.vecBounded(4096, 'image placements', decodeImagePlacement),
      }
      break
    case MsgType.IMAGE_BLOB_CHUNK: {
      const blob = decodeBlobHash(r)
      const encoding = decodeImageEncoding(r)
      const totalBytes = r.varU32()
      const offset = r.varU32()
      const data = r.bytesBounded(BULK_CHUNK_BYTES, 'image blob chunk').slice()
      if (offset + data.length > totalBytes) {
        throw new WireError('InvalidValue', 'image blob chunk exceeds total_bytes')
      }
      msg = { kind: 'imageBlobChunk', blob, encoding, totalBytes, offset, data }
      break
    }
    case MsgType.TITLE:
      msg = { kind: 'title', title: r.str() }
      break
    case MsgType.BELL:
      msg = { kind: 'bell' }
      break
    case MsgType.CLIPBOARD:
      msg = { kind: 'clipboard', selection: r.str(), data: r.str() }
      break
    case MsgType.NOTIFICATION:
      msg = { kind: 'notification', title: r.str(), body: r.str() }
      break
    case MsgType.PROGRESS: {
      const state = decodeProgressState(r)
      const percent = r.opt((rd) => {
        const p = rd.u8()
        if (p > 100) throw new WireError('InvalidValue', 'progress percent > 100')
        return p
      })
      if (percent !== null && (state === 'none' || state === 'indeterminate')) {
        throw new WireError('InvalidValue', 'progress percent invalid for state')
      }
      msg = { kind: 'progress', state, percent }
      break
    }
    case MsgType.PROGRAM_STATUS:
      msg = { kind: 'programStatus', record: decodeStatusRecord(r) }
      break
    case MsgType.COMMAND_STARTED:
      msg = { kind: 'commandStarted', record: decodeCommandRecord(r) }
      break
    case MsgType.COMMAND_FINISHED:
      msg = { kind: 'commandFinished', record: decodeCommandRecord(r) }
      break
    case MsgType.CWD_CHANGED:
      msg = { kind: 'cwdChanged', cwd: r.str() }
      break
    case MsgType.EXITED:
      msg = { kind: 'exited', exitCode: r.opt((rd) => rd.i32()), signal: r.opt((rd) => rd.str()) }
      break
    case MsgType.OUTPUT:
      msg = { kind: 'output', text: r.str() }
      break
    case MsgType.THROTTLED:
      msg = { kind: 'throttled', waitedUs: r.varU64() }
      break
    case MsgType.OUTPUT_DROPPED:
      msg = { kind: 'outputDropped', bytes: r.varU64() }
      break
    case MsgType.STREAM_CLOSED:
      msg = { kind: 'streamClosed', reason: r.str() }
      break
    default:
      throw new WireError('InvalidTag', `invalid BrokerMsg wire tag ${msgType}`)
  }
  r.finish()
  return msg
}
