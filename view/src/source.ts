/**
 * Clean-room v2 `TerminalSource` contract between `@apackdev/terminal-view` and its broker
 * (either `BrokerSessionSource` over a multiplexed v2 binary `TerminalClient`, or an in-memory
 * mock in tests).
 *
 * The view interprets zero VT bytes; all terminal state arrives as `FrameUpdate`s carrying
 * zero-per-cell-object `PackedRow`s and incremental `SearchDelta`s.
 */
import type {
  CellPos,
  CellPx,
  CellSpan,
  CommandRecord,
  CursorShape,
  CursorState,
  ErrorCode,
  ForegroundProcess,
  FrameCredit,
  GridSize,
  ImageEncoding,
  ImagePiece,
  ImagePlacement,
  KeyAction,
  KeyInput,
  LeaseKind,
  MetricsSnapshot,
  PackedRow,
  PaletteOverrides,
  PasteOutcome,
  PointerAction,
  PointerInput,
  ProgressState,
  ResizePolicy,
  Rgb24,
  SignalKind,
  SignalScope,
  StatusRecord,
  StatusState,
  UnderlineKind,
  ViewModes,
  ViewportHint,
  WaitCondition,
  WaitOutcome,
} from './wire/types.ts'
import type { FrameUpdate, SessionSummary, SpawnSpec } from './wire/messages.ts'

export {
  CONTROL_STREAM_ID,
  Modifier,
  PUSH_REQ_ID,
  PackedStyleFlag,
  RowFlag,
  RunFlag,
  SubFlags,
  blobHashToHex,
  isValidPackedColor,
  underlineOfStyleFlags,
} from './wire/types.ts'

export type {
  CellPos,
  CellPx,
  CellSpan,
  CommandRecord,
  CursorShape,
  CursorState,
  ErrorCode,
  ForegroundProcess,
  FrameCredit,
  FrameUpdate,
  GridSize,
  ImageEncoding,
  ImagePiece,
  ImagePlacement,
  KeyAction,
  KeyInput,
  LeaseKind,
  MetricsSnapshot,
  PackedRow,
  PaletteOverrides,
  PasteOutcome,
  PointerAction,
  PointerInput,
  ProgressState,
  ResizePolicy,
  Rgb24,
  SessionSummary,
  SignalKind,
  SignalScope,
  SpawnSpec,
  StatusRecord,
  StatusState,
  UnderlineKind,
  ViewModes,
  ViewportHint,
  WaitCondition,
  WaitOutcome,
}

/** Default word delimiters used when a standalone mock source does not override `wordDelimiters`. */
export const DEFAULT_WORD_DELIMITERS = " \t`'\"()[]{}<>:;,"

/** Reassembled content-addressed image blob returned by `TerminalSource.getImageBlob`. */
export interface ImageBlobData {
  readonly blob: Uint8Array
  readonly encoding: ImageEncoding
  readonly bytes: Uint8Array
}

/** Full match snapshot pushed after `setSearch` or epoch change. */
export interface SearchSnapshot {
  readonly epoch: number
  readonly matches: readonly CellSpan[]
  readonly truncated: boolean
}

/** Incremental match replacement pushed when dirty lines change within the same epoch. */
export interface SearchPatch {
  readonly epoch: number
  readonly fromLine: number
  readonly toLine: number
  readonly added: readonly CellSpan[]
  readonly truncated: boolean
}

/** Side-effect event emitted on a session stream (`SubFlags.EVENTS`). */
export type SessionEvent =
  | { readonly kind: 'title'; readonly title: string }
  | { readonly kind: 'bell' }
  | { readonly kind: 'clipboard'; readonly selection: string; readonly data: string }
  | { readonly kind: 'notification'; readonly title: string; readonly body: string }
  | { readonly kind: 'progress'; readonly state: ProgressState; readonly percent: number | null }
  | { readonly kind: 'programStatus'; readonly record: StatusRecord }
  | { readonly kind: 'programStatusCleared'; readonly id: string }
  | { readonly kind: 'commandStarted'; readonly record: CommandRecord }
  | { readonly kind: 'commandFinished'; readonly record: CommandRecord }
  | { readonly kind: 'cwdChanged'; readonly cwd: string }
  | { readonly kind: 'exited'; readonly exitCode: number | null; readonly signal: string | null }
  | { readonly kind: 'streamClosed'; readonly reason: string }

/** Callbacks installed by `TerminalView` when attaching to a `TerminalSource`. */
export interface SourceSink {
  onFrame(frame: FrameUpdate): void
  onSearchMatches(snapshot: SearchSnapshot): void
  onSearchDelta(patch: SearchPatch): void
  onEvent?(event: SessionEvent): void
}

/**
 * The v2 view-facing interface of one attached terminal session.
 *
 * Notice what is absent compared to v1: no client-side search polling, no `resize()` races (only
 * declarative `sendViewportHint`), and `wordDelimiters` is provided up-front from `HelloOk` so
 * 99% of `wordAt`/`lineAt`/`textOf`/`textOfRect` calls resolve synchronously in 0 ms locally.
 */
export interface TerminalSource {
  /** Word-selection delimiters advertised by the broker in `HelloOk`. */
  readonly wordDelimiters: string

  /** Registers the view's frame/search/event sink and starts delivery. Returns an unsubscribe fn. */
  subscribe(sink: SourceSink, options?: { readonly manualAck?: boolean }): () => void

  /** Acknowledges a painted frame sequence for end-to-end paint-paced credit replenishment. */
  ackFrame?(seq: number): void

  /** Reports updated viewport geometry, visibility (`visible: false` pauses frames), and focus. */
  sendViewportHint(hint: ViewportHint): void

  /** Sends an explicit resize (used when session uses `explicitOverride` policy). */
  explicitResize?(size: GridSize, cellPx: CellPx): Promise<void>

  /** Updates fallback cursor defaults when the running program has not set DECSCUSR. */
  setCursorDefaults?(shape: CursorShape, blinking: boolean): void

  sendKey(input: KeyInput): void
  sendText(text: string): void
  sendPointer(input: PointerInput): void
  sendFocus(focused: boolean): void
  paste(text: string, allowUnsafe?: boolean): Promise<PasteOutcome>

  /** Fetches uncached history rows `[fromLine, fromLine + count)` for `epoch`. */
  fetchRows(epoch: number, fromLine: number, count: number): Promise<readonly PackedRow[]>

  /** Broker RPC fallbacks used ONLY when a selection/click crosses uncached history. */
  wordAt(epoch: number, pos: CellPos): Promise<CellSpan | null>
  lineAt(epoch: number, line: number): Promise<CellSpan | null>
  textOf(epoch: number, span: CellSpan): Promise<string>
  textOfRect(epoch: number, span: CellSpan): Promise<string>

  /** Installs or clears (`query === ''`) an incremental server-side `SearchWatch`. */
  setSearch(query: string, caseSensitive: boolean): Promise<SearchSnapshot>

  /** Fetches the bounded command history ring and recorded output for a command. */
  commands(): Promise<readonly CommandRecord[]>
  commandOutput?(commandId: number): Promise<string | null>

  /** Fetches active image placements and content-addressed compressed image blobs. */
  images(): Promise<{ readonly imagesGen: number; readonly placements: readonly ImagePlacement[] }>
  getImageBlob(blob: Uint8Array): Promise<ImageBlobData>

  /** Detaches this stream cleanly. */
  dispose?(): void
}

/** Extracts the RGB integer or palette index from a v2 `PackedStyle` colour word. */
export function resolvePackedColor(
  packed: number,
  palette: readonly number[],
  fallback: number | null,
): number | null {
  const c = packed >>> 0
  if (c === 0) return fallback
  if (c <= 256) return palette[c - 1] ?? fallback
  if (c >= 0x01_00_00_00 && c <= 0x01_ff_ff_ff) return c & 0x00ff_ffff
  return fallback
}

/** Convenience helper building a `PackedRow` from structured runs (great for tests & fold headers). */
export interface RunSpec {
  readonly colStart: number
  readonly colLen: number
  readonly text: string
  readonly styleIdx?: number
  readonly linkIdx?: number
  readonly flags?: number
}

export interface StyleSpec {
  readonly fg?: number
  readonly bg?: number
  readonly underlineColor?: number
  readonly flags?: number
}

export function packRow(init: {
  readonly line: number
  readonly flags?: number
  readonly runs?: readonly RunSpec[]
  readonly styles?: readonly StyleSpec[]
  readonly links?: readonly string[]
  readonly pieces?: readonly ImagePiece[]
}): PackedRow {
  const runSpecs = init.runs ?? []
  const styleSpecs = init.styles ?? []
  let text = ''
  const runs = new Uint32Array(runSpecs.length * 4)
  for (let i = 0; i < runSpecs.length; i += 1) {
    const r = runSpecs[i]!
    const u16Off = text.length
    text += r.text
    const u16Len = r.text.length
    const base = i * 4
    runs[base] = ((r.colStart & 0xffff) | ((r.colLen & 0xffff) << 16)) >>> 0
    runs[base + 1] = ((u16Off & 0xffff) | ((u16Len & 0xffff) << 16)) >>> 0
    runs[base + 2] = (((r.styleIdx ?? 0) & 0xffff) | (((r.linkIdx ?? 0) & 0xffff) << 16)) >>> 0
    runs[base + 3] = (r.flags ?? 0) & 0xff
  }
  const styles = new Uint32Array(styleSpecs.length * 4)
  for (let i = 0; i < styleSpecs.length; i += 1) {
    const s = styleSpecs[i]!
    const base = i * 4
    styles[base] = (s.fg ?? 0) >>> 0
    styles[base + 1] = (s.bg ?? 0) >>> 0
    styles[base + 2] = (s.underlineColor ?? 0) >>> 0
    styles[base + 3] = (s.flags ?? 0) >>> 0
  }
  const links = init.links ?? []
  const flags = (init.flags ?? 0) | (init.pieces && init.pieces.length > 0 ? 0x08 : 0)
  return init.pieces && init.pieces.length > 0
    ? { line: init.line, flags, text, runs, styles, links, pieces: init.pieces }
    : { line: init.line, flags, text, runs, styles, links }
}
