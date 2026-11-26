/**
 * Correct-by-construction domain types and `PackedRow` typed-array representation matching
 * `crates/apack-proto/src/types.rs`.
 */
import {
  MAX_GRID_COLS,
  MAX_GRID_ROWS,
  MAX_LEASE_GRACE_MS,
  MAX_SEARCH_NEEDLE_BYTES,
  WireError,
  decodeUtf8Strict,
  encodeUtf8,
  type WireReader,
  type WireWriter,
} from './codec.ts'

export const CONTROL_STREAM_ID = 0
export const PUSH_REQ_ID = 0

export const SubFlags = {
  VIEW: 0x01,
  EVENTS: 0x02,
  RAW_OUTPUT: 0x04,
  BACKPRESSURE_PTY: 0x08,
  ALL_MASK: 0x0f,
} as const

export const Modifier = {
  shift: 0x01,
  ctrl: 0x02,
  alt: 0x04,
  super: 0x08,
  caps: 0x10,
  num: 0x20,
} as const

export const RowFlag = {
  WRAPPED: 0x01,
  PROMPT: 0x02,
  HAS_BLINK: 0x04,
  HAS_PLACEHOLDER: 0x08,
} as const

export const RunFlag = {
  /** Wide character or grapheme cluster needing per-cell placement */
  PER_CELL: 0x01,
} as const

export const PackedStyleFlag = {
  BOLD: 0x0001,
  ITALIC: 0x0002,
  UNDERLINE_MASK: 0x001c,
  UNDERLINE_MAX: 0x0014,
  STRIKETHROUGH: 0x0020,
  BLINK: 0x0040,
  INVERSE: 0x0080,
  INVISIBLE: 0x0100,
  FAINT: 0x0200,
  ALL_MASK: 0x03ff,
} as const

export type UnderlineKind = 'none' | 'single' | 'double' | 'curly' | 'dotted' | 'dashed'

const UNDERLINE_NAMES: readonly UnderlineKind[] = ['none', 'single', 'double', 'curly', 'dotted', 'dashed']

export function underlineOfStyleFlags(flags: number): UnderlineKind {
  return UNDERLINE_NAMES[(flags & PackedStyleFlag.UNDERLINE_MASK) >> 2] ?? 'none'
}

export function isValidPackedColor(color: number): boolean {
  const c = color >>> 0
  return c <= 256 || (c >= 0x01_00_00_00 && c <= 0x01_ff_ff_ff)
}

export type SignalKind = 'hup' | 'int' | 'quit' | 'term' | 'kill' | 'usr1' | 'usr2' | 'stop' | 'cont'
const SIGNAL_TO_TAG: Record<SignalKind, number> = {
  hup: 1, int: 2, quit: 3, term: 4, kill: 5, usr1: 6, usr2: 7, stop: 8, cont: 9,
}
const TAG_TO_SIGNAL: readonly (SignalKind | undefined)[] = [
  undefined, 'hup', 'int', 'quit', 'term', 'kill', 'usr1', 'usr2', 'stop', 'cont',
]

export type SignalScope =
  | { readonly kind: 'leader' }
  | { readonly kind: 'group' }
  | { readonly kind: 'pid'; readonly pid: number }

export type UnknownReason = 'noStatus' | 'superseded' | 'sessionEnded'
const UNKNOWN_TO_TAG: Record<UnknownReason, number> = { noStatus: 0, superseded: 1, sessionEnded: 2 }
const TAG_TO_UNKNOWN: readonly (UnknownReason | undefined)[] = ['noStatus', 'superseded', 'sessionEnded']

export type CommandOutcome =
  | { readonly kind: 'running' }
  | { readonly kind: 'exited'; readonly code: number }
  | { readonly kind: 'unknown'; readonly reason: UnknownReason }
  | { readonly kind: 'claimed'; readonly code: number | null }

export type ErrorCode =
  | 'handshake'
  | 'unauthorized'
  | 'malformed'
  | 'noSuchSession'
  | 'noSuchStream'
  | 'staleEpoch'
  | 'pty'
  | 'outsideSession'
  | 'unsupported'
  | 'resourceLimit'
  | 'busy'

const ERROR_CODE_TO_TAG: Record<ErrorCode, number> = {
  handshake: 1, unauthorized: 2, malformed: 3, noSuchSession: 4, noSuchStream: 5,
  staleEpoch: 6, pty: 7, outsideSession: 8, unsupported: 9, resourceLimit: 10, busy: 11,
}
const TAG_TO_ERROR_CODE: readonly (ErrorCode | undefined)[] = [
  undefined, 'handshake', 'unauthorized', 'malformed', 'noSuchSession', 'noSuchStream',
  'staleEpoch', 'pty', 'outsideSession', 'unsupported', 'resourceLimit', 'busy',
]

export interface GridSize {
  readonly cols: number
  readonly rows: number
}

export interface CellPx {
  readonly width: number
  readonly height: number
}

export type LeaseKind =
  | { readonly kind: 'brokerImmediate' }
  | { readonly kind: 'brokerGrace'; readonly graceMs: number; readonly resumeToken: Uint8Array }
  | { readonly kind: 'session'; readonly session: number }

export type ResizePolicy = 'latestActive' | 'minFit' | 'explicitOverride'
const RESIZE_TO_TAG: Record<ResizePolicy, number> = { latestActive: 0, minFit: 1, explicitOverride: 2 }
const TAG_TO_RESIZE: readonly (ResizePolicy | undefined)[] = ['latestActive', 'minFit', 'explicitOverride']

export interface ViewportHint {
  readonly size: GridSize
  readonly cellPx: CellPx
  readonly visible: boolean
  readonly focused: boolean
}

export interface FrameCredit {
  readonly maxFrames: number
  readonly maxBytes: number
}

export type CursorShape = 'block' | 'bar' | 'underline' | 'hollow'
const CURSOR_TO_TAG: Record<CursorShape, number> = { block: 0, bar: 1, underline: 2, hollow: 3 }
const TAG_TO_CURSOR: readonly (CursorShape | undefined)[] = ['block', 'bar', 'underline', 'hollow']

export interface CursorState {
  readonly col: number
  readonly row: number
  readonly shape: CursorShape
  readonly blinking: boolean
  readonly hidden: boolean
}

/** 24-bit `0xRRGGBB` colour integer */
export type Rgb24 = number

export interface PaletteOverrides {
  readonly fg: Rgb24 | null
  readonly bg: Rgb24 | null
  readonly cursor: Rgb24 | null
  readonly palette: ReadonlyArray<{ readonly index: number; readonly rgb: Rgb24 }>
}

export interface ViewModes {
  readonly altScreen: boolean
  readonly mouse: boolean
  readonly reverseVideo: boolean
  readonly bracketedPaste: boolean
}

export type ImageEncoding = 'png' | 'zstdRgba'

export interface ImagePiece {
  /** 32-byte BLAKE3 hash */
  readonly blob: Uint8Array
  readonly colStart: number
  readonly colLen: number
  readonly srcRect: readonly [number, number, number, number]
  readonly dstPx: readonly [number, number]
}

/**
 * Self-contained, zero-per-cell-object style-run encoded terminal row.
 *
 * - `runs` holds 4 `u32` words per run:
 *   - `[i + 0]`: `colStart | (colLen << 16)`
 *   - `[i + 1]`: `utf16Offset | (utf16Len << 16)` (indices directly into JS `row.text`)
 *   - `[i + 2]`: `styleIdx | (linkIdx << 16)`
 *   - `[i + 3]`: `runFlags` (`RunFlag.PER_CELL`)
 * - `styles` holds 4 `u32` words per unique style:
 *   - `[i + 0]`: `fg` (`0` = default, `1..=256` = palette+1, `0x01_RRGGBB` = RGB)
 *   - `[i + 1]`: `bg`
 *   - `[i + 2]`: `underlineColor`
 *   - `[i + 3]`: `flags` (`PackedStyleFlag`)
 */
export interface PackedRow {
  readonly line: number
  readonly flags: number
  readonly text: string
  readonly runs: Uint32Array
  readonly styles: Uint32Array
  readonly links: readonly string[]
  readonly pieces?: readonly ImagePiece[]
}

export interface CellPos {
  readonly line: number
  readonly col: number
}

export interface CellSpan {
  readonly start: CellPos
  readonly end: CellPos
}

export interface ImagePlacement {
  readonly blob: Uint8Array
  readonly line: number
  readonly col: number
  readonly pixelSize: readonly [number, number]
  readonly cellSpan: readonly [number, number]
  readonly zIndex: number
}

export interface CommandRecord {
  readonly id: number
  readonly command: string | null
  readonly cwd: string | null
  readonly startedMs: number | null
  readonly finishedMs: number | null
  readonly outcome: CommandOutcome
  readonly promptLine: number | null
  readonly outputLine: number | null
  readonly endLine: number | null
}

export type StatusState = 'idle' | 'working' | 'blocked' | 'done' | 'error'
const STATUS_TO_TAG: Record<StatusState, number> = { idle: 0, working: 1, blocked: 2, done: 3, error: 4 }
const TAG_TO_STATUS: readonly (StatusState | undefined)[] = ['idle', 'working', 'blocked', 'done', 'error']

export type BlockedKind = 'permission' | 'question' | 'auth'
const BLOCKED_TO_TAG: Record<BlockedKind, number> = { permission: 0, question: 1, auth: 2 }
const TAG_TO_BLOCKED: readonly (BlockedKind | undefined)[] = ['permission', 'question', 'auth']

export type Confirmed = 'matched' | 'mismatched' | 'unknown'
const CONFIRMED_TO_TAG: Record<Confirmed, number> = { matched: 0, mismatched: 1, unknown: 2 }
const TAG_TO_CONFIRMED: readonly (Confirmed | undefined)[] = ['matched', 'mismatched', 'unknown']

export interface StatusRecord {
  readonly id: string
  readonly state: StatusState
  readonly kind: BlockedKind | null
  readonly progress: number | null
  readonly app: string | null
  readonly title: string | null
  readonly message: string | null
  readonly changed: number
  readonly sinceMs: number
  readonly confirmed: Confirmed
}

export type ProgressState = 'none' | 'running' | 'error' | 'indeterminate' | 'paused'
const PROGRESS_TO_TAG: Record<ProgressState, number> = {
  none: 0, running: 1, error: 2, indeterminate: 3, paused: 4,
}
const TAG_TO_PROGRESS: readonly (ProgressState | undefined)[] = [
  'none', 'running', 'error', 'indeterminate', 'paused',
]

export type KeyAction = 'press' | 'repeat' | 'release'
const KEY_ACTION_TO_TAG: Record<KeyAction, number> = { press: 0, repeat: 1, release: 2 }
const TAG_TO_KEY_ACTION: readonly (KeyAction | undefined)[] = ['press', 'repeat', 'release']

export interface KeyInput {
  readonly code: string
  readonly action: KeyAction
  readonly modifiers: number
  readonly text: string
  readonly unshiftedCodepoint: number
  readonly composed: boolean
}

export type PointerAction = 'press' | 'release' | 'motion' | 'wheelUp' | 'wheelDown' | 'wheelLeft' | 'wheelRight'
const POINTER_TO_TAG: Record<PointerAction, number> = {
  press: 0, release: 1, motion: 2, wheelUp: 3, wheelDown: 4, wheelLeft: 5, wheelRight: 6,
}
const TAG_TO_POINTER: readonly (PointerAction | undefined)[] = [
  'press', 'release', 'motion', 'wheelUp', 'wheelDown', 'wheelLeft', 'wheelRight',
]

export interface PointerInput {
  readonly action: PointerAction
  /** `0` = left, `1` = middle, `2` = right, `255` = none */
  readonly button: number
  readonly modifiers: number
  readonly xPx: number
  readonly yPx: number
  readonly cellPx: CellPx
}

export type PasteOutcome = 'pasted' | 'unsafe'

export type WaitCondition =
  | { readonly kind: 'idle' }
  | { readonly kind: 'text'; readonly needle: string }
  | { readonly kind: 'status'; readonly states: readonly StatusState[] }
  | { readonly kind: 'exit' }

export type WaitOutcome = 'met' | 'timeout' | 'exited'
const WAIT_OUTCOME_TO_TAG: Record<WaitOutcome, number> = { met: 0, timeout: 1, exited: 2 }
const TAG_TO_WAIT_OUTCOME: readonly (WaitOutcome | undefined)[] = ['met', 'timeout', 'exited']

export interface ForegroundProcess {
  readonly pid: number
  readonly name: string | null
}

export interface MetricsSnapshot {
  readonly liveSessions: number
  readonly rewoundSessions: number
  readonly connections: number
  readonly activeTimers: number
  readonly scrollbackBytes: number
  readonly imageStoreBytes: number
  readonly totalPtyBytes: number
  readonly totalThrottledUs: number
}

// --- Encoders & decoders matching apack-proto/src/types.rs ---

export function encodeSessionId(w: WireWriter, id: number): void {
  if (!Number.isInteger(id) || id < 1) throw new WireError('InvalidValue', 'zero session id')
  w.varU64(id)
}
export function decodeSessionId(r: WireReader): number {
  const id = r.varU64()
  if (id === 0) throw new WireError('InvalidValue', 'zero session id')
  return id
}

export function encodeNonZeroStreamId(w: WireWriter, id: number): void {
  if (!Number.isInteger(id) || id < 1 || id > 0xffff_ffff) {
    throw new WireError('InvalidValue', 'stream id must be non-zero')
  }
  w.u32(id)
}
export function decodeNonZeroStreamId(r: WireReader): number {
  const id = r.u32()
  if (id === 0) throw new WireError('InvalidValue', 'stream id must be non-zero')
  return id
}

export function encodeSignalTarget(w: WireWriter, pid: number): void {
  if (!Number.isInteger(pid) || pid < 1 || pid > 0x7fff_ffff) {
    throw new WireError('InvalidValue', 'signal target pid must be > 0')
  }
  w.i32(pid)
}
export function decodeSignalTarget(r: WireReader): number {
  const pid = r.i32()
  if (pid < 1) throw new WireError('InvalidValue', 'signal target pid must be > 0')
  return pid
}

export function encodeSignalKind(w: WireWriter, kind: SignalKind): void {
  w.u8(SIGNAL_TO_TAG[kind])
}
export function decodeSignalKind(r: WireReader): SignalKind {
  const tag = r.u8()
  const kind = TAG_TO_SIGNAL[tag]
  if (!kind) throw new WireError('InvalidTag', `invalid SignalKind wire tag ${tag}`)
  return kind
}

export function encodeSignalScope(w: WireWriter, scope: SignalScope): void {
  if (scope.kind === 'leader') w.u8(0)
  else if (scope.kind === 'group') w.u8(1)
  else {
    w.u8(2)
    encodeSignalTarget(w, scope.pid)
  }
}
export function decodeSignalScope(r: WireReader): SignalScope {
  const tag = r.u8()
  if (tag === 0) return { kind: 'leader' }
  if (tag === 1) return { kind: 'group' }
  if (tag === 2) return { kind: 'pid', pid: decodeSignalTarget(r) }
  throw new WireError('InvalidTag', `invalid SignalScope wire tag ${tag}`)
}

const HEX_32 = /^[0-9a-f]{32}$/
export function encodeMarksKey(w: WireWriter, key: string): void {
  if (!HEX_32.test(key)) throw new WireError('InvalidValue', 'marks key must be 32 lowercase hex chars')
  w.raw(encodeUtf8(key))
}
export function decodeMarksKey(r: WireReader): string {
  const s = decodeUtf8Strict(r.take(32))
  if (!HEX_32.test(s)) throw new WireError('InvalidValue', 'marks key must be 32 lowercase hex chars')
  return s
}

export function encodeCommandOutcome(w: WireWriter, outcome: CommandOutcome): void {
  switch (outcome.kind) {
    case 'running':
      w.u8(0)
      break
    case 'exited':
      w.u8(1)
      w.i32(outcome.code)
      break
    case 'unknown':
      w.u8(2)
      w.u8(UNKNOWN_TO_TAG[outcome.reason])
      break
    case 'claimed':
      w.u8(3)
      w.opt(outcome.code, (wr, c) => wr.i32(c))
      break
  }
}
export function decodeCommandOutcome(r: WireReader): CommandOutcome {
  const tag = r.u8()
  if (tag === 0) return { kind: 'running' }
  if (tag === 1) return { kind: 'exited', code: r.i32() }
  if (tag === 2) {
    const ut = r.u8()
    const reason = TAG_TO_UNKNOWN[ut]
    if (!reason) throw new WireError('InvalidTag', `invalid UnknownReason wire tag ${ut}`)
    return { kind: 'unknown', reason }
  }
  if (tag === 3) return { kind: 'claimed', code: r.opt((rd) => rd.i32()) }
  throw new WireError('InvalidTag', `invalid CommandOutcome wire tag ${tag}`)
}

export function encodeErrorCode(w: WireWriter, code: ErrorCode): void {
  w.u16(ERROR_CODE_TO_TAG[code])
}
export function decodeErrorCode(r: WireReader): ErrorCode {
  const tag = r.u16()
  const code = TAG_TO_ERROR_CODE[tag]
  if (!code) throw new WireError('InvalidTag', `invalid ErrorCode wire tag ${tag}`)
  return code
}

export function encodeGridSize(w: WireWriter, size: GridSize): void {
  if (
    !Number.isInteger(size.cols) ||
    !Number.isInteger(size.rows) ||
    size.cols < 1 ||
    size.cols > MAX_GRID_COLS ||
    size.rows < 1 ||
    size.rows > MAX_GRID_ROWS
  ) {
    throw new WireError('InvalidValue', 'grid dimensions out of range')
  }
  w.u16(size.cols)
  w.u16(size.rows)
}
export function decodeGridSize(r: WireReader): GridSize {
  const cols = r.u16()
  const rows = r.u16()
  if (cols < 1 || cols > MAX_GRID_COLS || rows < 1 || rows > MAX_GRID_ROWS) {
    throw new WireError('InvalidValue', 'grid dimensions out of range')
  }
  return { cols, rows }
}

export function encodeCellPx(w: WireWriter, px: CellPx): void {
  w.u16(px.width)
  w.u16(px.height)
}
export function decodeCellPx(r: WireReader): CellPx {
  return { width: r.u16(), height: r.u16() }
}

export function encodeSubFlags(w: WireWriter, flags: number): void {
  if (flags < 0 || flags > SubFlags.ALL_MASK || ((flags & SubFlags.BACKPRESSURE_PTY) !== 0 && (flags & SubFlags.RAW_OUTPUT) === 0)) {
    throw new WireError('InvalidValue', 'unknown SubFlags bits')
  }
  w.u8(flags)
}
export function decodeSubFlags(r: WireReader): number {
  const flags = r.u8()
  if (flags > SubFlags.ALL_MASK || ((flags & SubFlags.BACKPRESSURE_PTY) !== 0 && (flags & SubFlags.RAW_OUTPUT) === 0)) {
    throw new WireError('InvalidValue', 'unknown SubFlags bits')
  }
  return flags
}

export function encodeLeaseKind(w: WireWriter, kind: LeaseKind): void {
  switch (kind.kind) {
    case 'brokerImmediate':
      w.u8(0)
      break
    case 'brokerGrace':
      if (!Number.isInteger(kind.graceMs) || kind.graceMs < 1 || kind.graceMs > MAX_LEASE_GRACE_MS) {
        throw new WireError('LimitExceeded', 'lease grace_ms')
      }
      if (kind.resumeToken.length !== 32) {
        throw new WireError('InvalidValue', 'resumeToken must be 32 bytes')
      }
      w.u8(1)
      w.varU32(kind.graceMs)
      w.raw(kind.resumeToken)
      break
    case 'session':
      w.u8(2)
      encodeSessionId(w, kind.session)
      break
  }
}
export function decodeLeaseKind(r: WireReader): LeaseKind {
  const tag = r.u8()
  if (tag === 0) return { kind: 'brokerImmediate' }
  if (tag === 1) {
    const graceMs = r.varU32()
    if (graceMs < 1 || graceMs > MAX_LEASE_GRACE_MS) throw new WireError('LimitExceeded', 'lease grace_ms')
    const resumeToken = r.fixedCopy(32)
    return { kind: 'brokerGrace', graceMs, resumeToken }
  }
  if (tag === 2) return { kind: 'session', session: decodeSessionId(r) }
  throw new WireError('InvalidTag', `invalid LeaseKind wire tag ${tag}`)
}

export function encodeResizePolicy(w: WireWriter, policy: ResizePolicy): void {
  w.u8(RESIZE_TO_TAG[policy])
}
export function decodeResizePolicy(r: WireReader): ResizePolicy {
  const tag = r.u8()
  const policy = TAG_TO_RESIZE[tag]
  if (!policy) throw new WireError('InvalidTag', `invalid ResizePolicy wire tag ${tag}`)
  return policy
}

export function encodeViewportHint(w: WireWriter, hint: ViewportHint): void {
  encodeGridSize(w, hint.size)
  encodeCellPx(w, hint.cellPx)
  w.bool(hint.visible)
  w.bool(hint.focused)
}
export function decodeViewportHint(r: WireReader): ViewportHint {
  return {
    size: decodeGridSize(r),
    cellPx: decodeCellPx(r),
    visible: r.bool(),
    focused: r.bool(),
  }
}

export function encodeFrameCredit(w: WireWriter, credit: FrameCredit): void {
  w.u16(credit.maxFrames)
  w.varU32(credit.maxBytes)
}
export function decodeFrameCredit(r: WireReader): FrameCredit {
  return { maxFrames: r.u16(), maxBytes: r.varU32() }
}

export function encodeCursorShape(w: WireWriter, shape: CursorShape): void {
  w.u8(CURSOR_TO_TAG[shape])
}
export function decodeCursorShape(r: WireReader): CursorShape {
  const tag = r.u8()
  const shape = TAG_TO_CURSOR[tag]
  if (!shape) throw new WireError('InvalidTag', `invalid CursorShape wire tag ${tag}`)
  return shape
}

export function encodeCursorState(w: WireWriter, state: CursorState): void {
  w.u16(state.col)
  w.u16(state.row)
  encodeCursorShape(w, state.shape)
  w.u8((state.blinking ? 1 : 0) | (state.hidden ? 2 : 0))
}
export function decodeCursorState(r: WireReader): CursorState {
  const col = r.u16()
  const row = r.u16()
  const shape = decodeCursorShape(r)
  const flags = r.u8()
  if (flags > 0x03) throw new WireError('InvalidValue', 'unknown CursorState flag bits')
  return { col, row, shape, blinking: (flags & 1) !== 0, hidden: (flags & 2) !== 0 }
}

export function encodeRgb24(w: WireWriter, rgb: Rgb24): void {
  w.u8((rgb >> 16) & 0xff)
  w.u8((rgb >> 8) & 0xff)
  w.u8(rgb & 0xff)
}
export function decodeRgb24(r: WireReader): Rgb24 {
  const b = r.take(3)
  return ((b[0]! << 16) | (b[1]! << 8) | b[2]!) >>> 0
}

export function encodePaletteOverrides(w: WireWriter, p: PaletteOverrides): void {
  w.opt(p.fg, encodeRgb24)
  w.opt(p.bg, encodeRgb24)
  w.opt(p.cursor, encodeRgb24)
  w.slice(p.palette, (wr, entry) => {
    wr.u8(entry.index)
    encodeRgb24(wr, entry.rgb)
  })
}
export function decodePaletteOverrides(r: WireReader): PaletteOverrides {
  return {
    fg: r.opt(decodeRgb24),
    bg: r.opt(decodeRgb24),
    cursor: r.opt(decodeRgb24),
    palette: r.vecBounded(256, 'palette overrides', (rd) => ({ index: rd.u8(), rgb: decodeRgb24(rd) })),
  }
}

export function encodeViewModes(w: WireWriter, m: ViewModes): void {
  w.u8((m.altScreen ? 1 : 0) | (m.mouse ? 2 : 0) | (m.reverseVideo ? 4 : 0) | (m.bracketedPaste ? 8 : 0))
}
export function decodeViewModes(r: WireReader): ViewModes {
  const bits = r.u8()
  if (bits > 0x0f) throw new WireError('InvalidValue', 'unknown ViewModes flag bits')
  return {
    altScreen: (bits & 1) !== 0,
    mouse: (bits & 2) !== 0,
    reverseVideo: (bits & 4) !== 0,
    bracketedPaste: (bits & 8) !== 0,
  }
}

export function encodeBlobHash(w: WireWriter, hash: Uint8Array): void {
  if (hash.length !== 32) throw new WireError('InvalidValue', 'blob hash must be 32 bytes')
  w.raw(hash)
}
export function decodeBlobHash(r: WireReader): Uint8Array {
  return r.fixedCopy(32)
}

export function blobHashToHex(hash: Uint8Array): string {
  let out = ''
  for (let i = 0; i < hash.length; i += 1) {
    out += hash[i]!.toString(16).padStart(2, '0')
  }
  return out
}

export function encodeImageEncoding(w: WireWriter, enc: ImageEncoding): void {
  w.u8(enc === 'png' ? 0 : 1)
}
export function decodeImageEncoding(r: WireReader): ImageEncoding {
  const tag = r.u8()
  if (tag === 0) return 'png'
  if (tag === 1) return 'zstdRgba'
  throw new WireError('InvalidTag', `invalid ImageEncoding wire tag ${tag}`)
}

export function encodeImagePiece(w: WireWriter, p: ImagePiece): void {
  encodeBlobHash(w, p.blob)
  w.u16(p.colStart)
  w.u16(p.colLen)
  for (let i = 0; i < 4; i += 1) w.varU32(p.srcRect[i]!)
  for (let i = 0; i < 2; i += 1) w.varU32(p.dstPx[i]!)
}
export function decodeImagePiece(r: WireReader): ImagePiece {
  const blob = decodeBlobHash(r)
  const colStart = r.u16()
  const colLen = r.u16()
  if (colLen === 0 || colStart + colLen > 0xffff) {
    throw new WireError('InvalidValue', 'invalid ImagePiece column span')
  }
  const sx = r.varU32(), sy = r.varU32(), sw = r.varU32(), sh = r.varU32()
  if (sw === 0 || sh === 0 || sx + sw > 0xffff_ffff || sy + sh > 0xffff_ffff) {
    throw new WireError('InvalidValue', 'invalid ImagePiece src_rect')
  }
  const dw = r.varU32(), dh = r.varU32()
  if (dw === 0 || dh === 0) throw new WireError('InvalidValue', 'invalid ImagePiece dst_px')
  return { blob, colStart, colLen, srcRect: [sx, sy, sw, sh], dstPx: [dw, dh] }
}

/**
 * Maps UTF-8 byte indices `0..=utf8.length` to UTF-16 code-unit offsets in `text`, setting
 * non-boundary interior bytes to `-1` so mid-UTF-8 run offsets fail fast on decode.
 */
function buildUtf8ToUtf16Map(utf8: Uint8Array): Int32Array {
  const map = new Int32Array(utf8.length + 1)
  map.fill(-1)
  let bytePos = 0
  let utf16Pos = 0
  while (bytePos < utf8.length) {
    map[bytePos] = utf16Pos
    const b0 = utf8[bytePos]!
    if (b0 < 0x80) {
      bytePos += 1
      utf16Pos += 1
    } else if (b0 < 0xe0) {
      bytePos += 2
      utf16Pos += 1
    } else if (b0 < 0xf0) {
      bytePos += 3
      utf16Pos += 1
    } else {
      bytePos += 4
      utf16Pos += 2 // surrogate pair in UTF-16
    }
  }
  map[utf8.length] = utf16Pos
  return map
}

/** Maps UTF-16 code-unit offsets `0..=text.length` back to UTF-8 byte offsets for encoding. */
function buildUtf16ToUtf8Map(text: string): Uint32Array {
  const map = new Uint32Array(text.length + 1)
  let bytePos = 0
  let i = 0
  while (i < text.length) {
    map[i] = bytePos
    const cp = text.codePointAt(i)!
    if (cp <= 0x7f) {
      bytePos += 1
      i += 1
    } else if (cp <= 0x7ff) {
      bytePos += 2
      i += 1
    } else if (cp <= 0xffff) {
      bytePos += 3
      i += 1
    } else {
      bytePos += 4
      map[i + 1] = bytePos // shouldn't be indexed mid-surrogate
      i += 2
    }
  }
  map[text.length] = bytePos
  return map
}

export function encodePackedRow(w: WireWriter, row: PackedRow): void {
  w.varU64(row.line)
  w.u8(row.flags)
  const utf8 = encodeUtf8(row.text)
  w.bytes(utf8)
  const isAscii = utf8.length === row.text.length
  const utf16ToUtf8 = isAscii ? null : buildUtf16ToUtf8Map(row.text)

  const runCount = row.runs.length >>> 2
  w.varU32(runCount)
  for (let i = 0; i < row.runs.length; i += 4) {
    const colWord = row.runs[i]!
    const txtWord = row.runs[i + 1]!
    const slWord = row.runs[i + 2]!
    const runFlags = row.runs[i + 3]!
    const colStart = colWord & 0xffff
    const colLen = colWord >>> 16
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    const byteOffset = utf16ToUtf8 ? utf16ToUtf8[u16Off]! : u16Off
    const byteEnd = utf16ToUtf8 ? utf16ToUtf8[u16Off + u16Len]! : u16Off + u16Len
    const styleIdx = slWord & 0xffff
    const linkIdx = slWord >>> 16
    w.u16(colStart)
    w.u16(colLen)
    w.varU32(byteOffset)
    w.varU32(byteEnd - byteOffset)
    w.u16(styleIdx)
    w.u16(linkIdx)
    w.u8(runFlags)
  }

  const styleCount = row.styles.length >>> 2
  w.varU32(styleCount)
  for (let i = 0; i < row.styles.length; i += 4) {
    w.u32(row.styles[i]!)
    w.u32(row.styles[i + 1]!)
    w.u32(row.styles[i + 2]!)
    w.u32(row.styles[i + 3]!)
  }

  w.slice(row.links, (wr, l) => wr.str(l))
  const pieces = row.pieces ?? []
  w.slice(pieces, encodeImagePiece)
}

export function decodePackedRow(r: WireReader): PackedRow {
  const line = r.varU64()
  const flags = r.u8()
  if (flags > 0x0f) throw new WireError('InvalidValue', 'unknown PackedRow flag bits')

  const textBytes = r.strBytes()
  const text = decodeUtf8Strict(textBytes)
  const isAscii = textBytes.length === text.length
  const byteToUtf16 = isAscii ? null : buildUtf8ToUtf16Map(textBytes)

  const runCount = r.varU32()
  if (runCount > 4096) throw new WireError('LimitExceeded', 'wire limit exceeded: packed runs')
  if (runCount * 11 > r.remaining) throw new WireError('UnexpectedEof', 'truncated packed runs')
  const runs = new Uint32Array(runCount * 4)
  for (let i = 0; i < runCount; i += 1) {
    const colStart = r.u16()
    const colLen = r.u16()
    if (colLen === 0 || colStart + colLen > 0xffff) {
      throw new WireError('InvalidValue', 'invalid PackedRun column span')
    }
    const byteOffset = r.varU32()
    const byteLen = r.varU32()
    const styleIdx = r.u16()
    const linkIdx = r.u16()
    const runFlags = r.u8()
    if (runFlags > 0x01) throw new WireError('InvalidValue', 'unknown PackedRun flag bits')

    const byteEnd = byteOffset + byteLen
    if (byteEnd > textBytes.length) {
      throw new WireError('InvalidValue', 'run byte range out of bounds or mid-UTF-8')
    }
    let u16Start: number
    let u16End: number
    if (byteToUtf16 === null) {
      u16Start = byteOffset
      u16End = byteEnd
    } else {
      u16Start = byteToUtf16[byteOffset]!
      u16End = byteToUtf16[byteEnd]!
      if (u16Start < 0 || u16End < 0) {
        throw new WireError('InvalidValue', 'run byte range out of bounds or mid-UTF-8')
      }
    }
    const u16Len = u16End - u16Start
    if (u16Start > 0xffff || u16Len > 0xffff) {
      throw new WireError('LimitExceeded', 'packed row text offset exceeds u16')
    }
    const base = i * 4
    runs[base] = (colStart | (colLen << 16)) >>> 0
    runs[base + 1] = (u16Start | (u16Len << 16)) >>> 0
    runs[base + 2] = (styleIdx | (linkIdx << 16)) >>> 0
    runs[base + 3] = runFlags
  }

  const styleCount = r.varU32()
  if (styleCount > 4096) throw new WireError('LimitExceeded', 'wire limit exceeded: packed styles')
  if (styleCount * 16 > r.remaining) throw new WireError('UnexpectedEof', 'truncated packed styles')
  const styles = new Uint32Array(styleCount * 4)
  for (let i = 0; i < styleCount; i += 1) {
    const fg = r.u32()
    const bg = r.u32()
    const ul = r.u32()
    const sFlags = r.u32()
    if (!isValidPackedColor(fg) || !isValidPackedColor(bg) || !isValidPackedColor(ul)) {
      throw new WireError('InvalidValue', 'invalid PackedStyle colour encoding')
    }
    if ((sFlags & ~PackedStyleFlag.ALL_MASK) !== 0 || (sFlags & PackedStyleFlag.UNDERLINE_MASK) > PackedStyleFlag.UNDERLINE_MAX) {
      throw new WireError('InvalidValue', 'invalid PackedStyle flags')
    }
    if (i === 0 && (fg !== 0 || bg !== 0 || ul !== 0 || sFlags !== 0)) {
      throw new WireError('InvalidValue', 'styles[0] must be default style')
    }
    const base = i * 4
    styles[base] = fg
    styles[base + 1] = bg
    styles[base + 2] = ul
    styles[base + 3] = sFlags
  }

  const links = r.vecBounded(1024, 'packed links', (rd) => rd.str())
  if (links.length > 0 && links[0] !== '') {
    throw new WireError('InvalidValue', 'links[0] must be empty')
  }

  const pieces = r.vecBounded(1024, 'image pieces', decodeImagePiece)
  if (pieces.length > 0 && (flags & RowFlag.HAS_PLACEHOLDER) === 0) {
    throw new WireError('InvalidValue', 'pieces require FLAG_HAS_PLACEHOLDER')
  }

  // Validate styleIdx / linkIdx bounds up front (parse, don't validate later!)
  for (let i = 0; i < runs.length; i += 4) {
    const slWord = runs[i + 2]!
    const styleIdx = slWord & 0xffff
    const linkIdx = slWord >>> 16
    if (styleIdx >= styleCount && styleIdx !== 0) {
      throw new WireError('InvalidValue', 'run style_idx out of bounds')
    }
    if (linkIdx >= links.length && linkIdx !== 0) {
      throw new WireError('InvalidValue', 'run link_idx out of bounds')
    }
  }

  return pieces.length > 0
    ? { line, flags, text, runs, styles, links, pieces }
    : { line, flags, text, runs, styles, links }
}

export function encodeCellPos(w: WireWriter, pos: CellPos): void {
  w.varU64(pos.line)
  w.u16(pos.col)
}
export function decodeCellPos(r: WireReader): CellPos {
  return { line: r.varU64(), col: r.u16() }
}

export function encodeCellSpan(w: WireWriter, span: CellSpan): void {
  encodeCellPos(w, span.start)
  encodeCellPos(w, span.end)
}
export function decodeCellSpan(r: WireReader): CellSpan {
  const start = decodeCellPos(r)
  const end = decodeCellPos(r)
  if (end.line < start.line || (end.line === start.line && end.col < start.col)) {
    throw new WireError('InvalidValue', 'span end before start')
  }
  return { start, end }
}

export function encodeImagePlacement(w: WireWriter, p: ImagePlacement): void {
  encodeBlobHash(w, p.blob)
  w.varU64(p.line)
  w.i32(p.col)
  w.varU32(p.pixelSize[0])
  w.varU32(p.pixelSize[1])
  w.u16(p.cellSpan[0])
  w.u16(p.cellSpan[1])
  w.i32(p.zIndex)
}
export function decodeImagePlacement(r: WireReader): ImagePlacement {
  return {
    blob: decodeBlobHash(r),
    line: r.varU64(),
    col: r.i32(),
    pixelSize: [r.varU32(), r.varU32()],
    cellSpan: [r.u16(), r.u16()],
    zIndex: r.i32(),
  }
}

export function encodeCommandRecord(w: WireWriter, c: CommandRecord): void {
  w.varU64(c.id)
  w.opt(c.command, (wr, s) => wr.str(s))
  w.opt(c.cwd, (wr, s) => wr.str(s))
  w.opt(c.startedMs, (wr, t) => wr.varU64(t))
  w.opt(c.finishedMs, (wr, t) => wr.varU64(t))
  encodeCommandOutcome(w, c.outcome)
  w.opt(c.promptLine, (wr, l) => wr.varU64(l))
  w.opt(c.outputLine, (wr, l) => wr.varU64(l))
  w.opt(c.endLine, (wr, l) => wr.varU64(l))
}
export function decodeCommandRecord(r: WireReader): CommandRecord {
  return {
    id: r.varU64(),
    command: r.opt((rd) => rd.str()),
    cwd: r.opt((rd) => rd.str()),
    startedMs: r.opt((rd) => rd.varU64()),
    finishedMs: r.opt((rd) => rd.varU64()),
    outcome: decodeCommandOutcome(r),
    promptLine: r.opt((rd) => rd.varU64()),
    outputLine: r.opt((rd) => rd.varU64()),
    endLine: r.opt((rd) => rd.varU64()),
  }
}

export function encodeStatusState(w: WireWriter, s: StatusState): void {
  w.u8(STATUS_TO_TAG[s])
}
export function decodeStatusState(r: WireReader): StatusState {
  const tag = r.u8()
  const st = TAG_TO_STATUS[tag]
  if (!st) throw new WireError('InvalidTag', `invalid StatusState wire tag ${tag}`)
  return st
}

export function encodeStatusRecord(w: WireWriter, s: StatusRecord): void {
  w.str(s.id)
  encodeStatusState(w, s.state)
  w.opt(s.kind, (wr, k) => wr.u8(BLOCKED_TO_TAG[k]))
  w.opt(s.progress, (wr, p) => wr.u8(p))
  w.opt(s.app, (wr, v) => wr.str(v))
  w.opt(s.title, (wr, v) => wr.str(v))
  w.opt(s.message, (wr, v) => wr.str(v))
  w.varU64(s.changed)
  w.varU64(s.sinceMs)
  w.u8(CONFIRMED_TO_TAG[s.confirmed])
}
export function decodeStatusRecord(r: WireReader): StatusRecord {
  const id = r.str()
  const state = decodeStatusState(r)
  const kind = r.opt((rd) => {
    const t = rd.u8()
    const k = TAG_TO_BLOCKED[t]
    if (!k) throw new WireError('InvalidTag', `invalid BlockedKind wire tag ${t}`)
    return k
  })
  if (state !== 'blocked' && kind !== null) {
    throw new WireError('InvalidValue', 'status kind requires Blocked state')
  }
  const progress = r.opt((rd) => {
    const p = rd.u8()
    if (p > 100) throw new WireError('InvalidValue', 'progress must be 0..=100')
    return p
  })
  const app = r.opt((rd) => rd.str())
  const title = r.opt((rd) => rd.str())
  const message = r.opt((rd) => rd.str())
  const changed = r.varU64()
  const sinceMs = r.varU64()
  const ct = r.u8()
  const confirmed = TAG_TO_CONFIRMED[ct]
  if (!confirmed) throw new WireError('InvalidTag', `invalid Confirmed wire tag ${ct}`)
  return { id, state, kind, progress, app, title, message, changed, sinceMs, confirmed }
}

export function encodeProgressState(w: WireWriter, s: ProgressState): void {
  w.u8(PROGRESS_TO_TAG[s])
}
export function decodeProgressState(r: WireReader): ProgressState {
  const tag = r.u8()
  const st = TAG_TO_PROGRESS[tag]
  if (!st) throw new WireError('InvalidTag', `invalid ProgressState wire tag ${tag}`)
  return st
}

export function encodeKeyInput(w: WireWriter, k: KeyInput): void {
  w.str(k.code)
  w.u8(KEY_ACTION_TO_TAG[k.action])
  w.u8(k.modifiers)
  w.str(k.text)
  w.varU32(k.unshiftedCodepoint)
  w.bool(k.composed)
}
export function decodeKeyInput(r: WireReader): KeyInput {
  const code = r.str()
  const at = r.u8()
  const action = TAG_TO_KEY_ACTION[at]
  if (!action) throw new WireError('InvalidTag', `invalid KeyAction wire tag ${at}`)
  const modifiers = r.u8()
  if (modifiers > 0x3f) throw new WireError('InvalidValue', 'unknown KeyInput modifier bits')
  const text = r.str()
  const unshiftedCodepoint = r.varU32()
  if (unshiftedCodepoint !== 0 && (unshiftedCodepoint > 0x10_ffff || (unshiftedCodepoint >= 0xd800 && unshiftedCodepoint <= 0xdfff))) {
    throw new WireError('InvalidValue', 'invalid Unicode scalar codepoint')
  }
  const composed = r.bool()
  return { code, action, modifiers, text, unshiftedCodepoint, composed }
}

export function encodePointerInput(w: WireWriter, p: PointerInput): void {
  w.u8(POINTER_TO_TAG[p.action])
  w.u8(p.button)
  w.u8(p.modifiers)
  w.varU32(p.xPx)
  w.varU32(p.yPx)
  encodeCellPx(w, p.cellPx)
}
export function decodePointerInput(r: WireReader): PointerInput {
  const at = r.u8()
  const action = TAG_TO_POINTER[at]
  if (!action) throw new WireError('InvalidTag', `invalid PointerAction wire tag ${at}`)
  const button = r.u8()
  if (button > 2 && button !== 255) throw new WireError('InvalidValue', 'invalid PointerInput button')
  const modifiers = r.u8()
  if (modifiers > 0x3f) throw new WireError('InvalidValue', 'unknown PointerInput modifier bits')
  return { action, button, modifiers, xPx: r.varU32(), yPx: r.varU32(), cellPx: decodeCellPx(r) }
}

export function encodePasteOutcome(w: WireWriter, o: PasteOutcome): void {
  w.u8(o === 'pasted' ? 0 : 1)
}
export function decodePasteOutcome(r: WireReader): PasteOutcome {
  const tag = r.u8()
  if (tag === 0) return 'pasted'
  if (tag === 1) return 'unsafe'
  throw new WireError('InvalidTag', `invalid PasteOutcome wire tag ${tag}`)
}

export function encodeWaitCondition(w: WireWriter, c: WaitCondition): void {
  switch (c.kind) {
    case 'idle':
      w.u8(0)
      break
    case 'text':
      w.u8(1)
      w.str(c.needle)
      break
    case 'status':
      w.u8(2)
      w.slice(c.states, encodeStatusState)
      break
    case 'exit':
      w.u8(3)
      break
  }
}
export function decodeWaitCondition(r: WireReader): WaitCondition {
  const tag = r.u8()
  if (tag === 0) return { kind: 'idle' }
  if (tag === 1) {
    const needle = r.strBounded(MAX_SEARCH_NEEDLE_BYTES, 'wait text needle')
    if (needle.length === 0) throw new WireError('InvalidValue', 'wait text needle must be non-empty')
    return { kind: 'text', needle }
  }
  if (tag === 2) {
    const states = r.vecBounded(8, 'wait states', decodeStatusState)
    if (states.length === 0) throw new WireError('InvalidValue', 'wait status list must be non-empty')
    return { kind: 'status', states }
  }
  if (tag === 3) return { kind: 'exit' }
  throw new WireError('InvalidTag', `invalid WaitCondition wire tag ${tag}`)
}

export function encodeWaitOutcome(w: WireWriter, o: WaitOutcome): void {
  w.u8(WAIT_OUTCOME_TO_TAG[o])
}
export function decodeWaitOutcome(r: WireReader): WaitOutcome {
  const tag = r.u8()
  const out = TAG_TO_WAIT_OUTCOME[tag]
  if (!out) throw new WireError('InvalidTag', `invalid WaitOutcome wire tag ${tag}`)
  return out
}

export function encodeForegroundProcess(w: WireWriter, f: ForegroundProcess): void {
  encodeSignalTarget(w, f.pid)
  w.opt(f.name, (wr, n) => wr.str(n))
}
export function decodeForegroundProcess(r: WireReader): ForegroundProcess {
  return { pid: decodeSignalTarget(r), name: r.opt((rd) => rd.str()) }
}

export function encodeMetricsSnapshot(w: WireWriter, m: MetricsSnapshot): void {
  w.varU32(m.liveSessions)
  w.varU32(m.rewoundSessions)
  w.varU32(m.connections)
  w.varU32(m.activeTimers)
  w.varU64(m.scrollbackBytes)
  w.varU64(m.imageStoreBytes)
  w.varU64(m.totalPtyBytes)
  w.varU64(m.totalThrottledUs)
}
export function decodeMetricsSnapshot(r: WireReader): MetricsSnapshot {
  return {
    liveSessions: r.varU32(),
    rewoundSessions: r.varU32(),
    connections: r.varU32(),
    activeTimers: r.varU32(),
    scrollbackBytes: r.varU64(),
    imageStoreBytes: r.varU64(),
    totalPtyBytes: r.varU64(),
    totalThrottledUs: r.varU64(),
  }
}
