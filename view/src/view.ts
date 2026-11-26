import { CanvasPainter } from './canvas-painter.ts'
import { Timer, systemClock, type Clock } from './clock.ts'
import { blinks, drawRow, rowPlainText, sessionTheme, type RowDrawing, type SelectedCells, type Theme } from './draw.ts'
import { ImageCache, type ImageCacheOptions } from './images.ts'
import { Input, modifiersOf } from './input.ts'
import { linkAt, linkRuns, urls, type Detector, type Link, type LinkRun } from './links.ts'
import { fontOf, type Font, type Painter, type Scene } from './painter.ts'
import { PAGE_LINES, RowStore } from './row-store.ts'
import { Selection, type Granularity } from './selection.ts'
import {
  Modifier,
  PackedStyleFlag,
  RunFlag,
  packRow,
  type CellPos,
  type CellSpan,
  type CommandRecord,
  type CursorShape,
  type FrameUpdate,
  type ImagePlacement,
  type PackedRow,
  type PaletteOverrides,
  type PointerInput,
  type SearchPatch,
  type SearchSnapshot,
  type SessionEvent,
  type TerminalSource,
  type ViewportHint,
} from './source.ts'
import { Viewport } from './viewport.ts'
import { cellsOfCodePoint } from './width.ts'

/**
 * A run of lines a host folds away. The first line stays on screen as a header, which stands for
 * the lines after it up to `last`: those are neither drawn nor pointed at, and what is copied
 * from the fold is still all of its text.
 */
export interface Fold {
  readonly first: number
  readonly last: number
  /** What the header says the fold holds, after the count of lines. Left out, the header says only the count. */
  readonly label?: string
}

/** How long the cursor is shown, and then hidden, while it blinks (ms). */
export const BLINK_MS = 530

/** Maximum search matches retained & highlighted in the view. */
export const MATCH_HIGHLIGHTS = 1000

/** Debounce interval (ms) for coalescing updates to the hidden `[data-terminal-text]` DOM mirror. */
export const MIRROR_DEBOUNCE_MS = 200

const DRAG_LINES_PER_SECOND = 15
const CLICK_AGAIN_MS = 500
const CLICK_AGAIN_PIXELS = 4
const THUMB_MIN = 20

const blinkingRows = new WeakMap<PackedRow, boolean>()
function blinksIn(row: PackedRow): boolean {
  let answer = blinkingRows.get(row)
  if (answer === undefined) {
    answer = blinks(row)
    blinkingRows.set(row, answer)
  }
  return answer
}

/**
 * What hovering a link shows of where it opens, as the pointer's title: `osc8` for a link a
 * program declared with a hyperlink sequence, `all` for any link, and `off` for none.
 */
export type LinkPreview = 'off' | 'osc8' | 'all'

export interface ViewOptions {
  /** The typeface a view starts in; `setFont` changes it. */
  readonly font: Font
  /** The colours a view starts in; `setTheme` changes them. */
  readonly theme: Theme
  /** Optional custom `ImageCache` or decode/cap options (e.g. for tests or zstdRgba decoders). */
  readonly imageCache?: ImageCache
  readonly imageCacheOptions?: ImageCacheOptions
  /** Custom painter factory. Defaults to `CanvasPainter`. */
  readonly painter?: (font: Font) => Painter
  /** Reports measured cell fit whenever container dimensions or font metrics change. */
  readonly onFit?: (fit: { columns: number; rows: number; cellWidth: number; cellHeight: number }) => void
  /** Fires whenever whether the view is following live output changes. */
  readonly onFollowingChange?: (following: boolean) => void
  /** Fires when the session exits or the stream closes. */
  readonly onEnded?: (reason?: string) => void
  /** Optional raw event callback for title, bell, clipboard, notification, progress, status, cwd, etc. */
  readonly onEvent?: (event: SessionEvent) => void
  /** Host keyboard shortcut interceptor. Returning `true` consumes the key. */
  readonly onKey?: (event: KeyboardEvent) => boolean
  /** Confirmation callback when the broker flags a paste as `'unsafe'`. */
  readonly confirmPaste?: (text: string) => Promise<boolean>
  /** Fires if a paste RPC fails (e.g. oversized payload). */
  readonly onPasteError?: (error: unknown) => void
  /** Whether typing/pasting jumps a scrolled-back view back to the live screen. Default `true`. */
  readonly followOnInput?: boolean
  /** Cursor shape when unfocused. Default `'hollow'`. */
  readonly cursorInactive?: CursorShape
  /** Whether Option acts as Alt on macOS. Default `false`. */
  readonly optionIsAlt?: boolean
  /** Fires when the user Cmd/Ctrl-clicks a link. */
  readonly onLink?: (link: Link, event: MouseEvent) => void
  /** Which links display a tooltip preview on hover. Default `'osc8'`. */
  readonly linkPreview?: LinkPreview
  /** Additional custom link detectors beyond `urls`. */
  readonly detectors?: readonly Detector[]
  /** Fires whenever selection presence flips between empty and non-empty. */
  readonly onSelectionChange?: (selected: boolean) => void
  /** Fires when live incremental `SearchWatch` updates change match count or current index. */
  readonly onFound?: (found: { current: number; total: number; more: boolean }) => void
  /** Custom copy handler. Defaults to writing to `navigator.clipboard`. */
  readonly onCopy?: (text: string) => void
  /** Fires whenever the session's command list updates. */
  readonly onCommands?: (commands: readonly CommandRecord[]) => void
  /** Automatically copy selections on pointer release. Default `false`. */
  readonly copyOnSelect?: boolean
  /** Override macOS modifier heuristics. Defaults to `navigator.platform` detection. */
  readonly mac?: boolean
  /** Gutter width in CSS px for command prompt status marks. Default `0` (disabled). */
  readonly gutter?: number
  /** Structural timer source (compatible with `@apackdev/clock`). Defaults to `systemClock`. */
  readonly clock?: Clock
}

interface KeptDrawing {
  readonly row: PackedRow
  readonly selVer: number
  readonly matchVer: number
  readonly hoverVer: number
  readonly blinkedOff: boolean
  readonly drawing: RowDrawing
}

interface Pressed {
  readonly button: number
  readonly reporting: boolean
  readonly at: CellPos
  moved: boolean
  readonly block: boolean
}

interface Point {
  readonly clientX: number
  readonly clientY: number
}

interface CachedBox {
  readonly left: number
  readonly top: number
  readonly scaleX: number
  readonly scaleY: number
}

interface CommandLine {
  readonly line: number
  readonly command: CommandRecord
}

/**
 * Clean-room v2 `@apackdev/terminal-view`.
 *
 * Key architectural properties:
 * - Zero VT interpretation: renders authoritative `PackedRow`s from `FrameUpdate`s.
 * - Declarative `ViewportHint` resize & visibility (`setVisible(false)` pauses broker frames).
 * - Zero search polling: driven entirely by push `SearchMatches` / `SearchDelta` from `SearchWatch`.
 * - 0 ms synchronous local `wordAt` / `lineAt` / `textOf` / `textOfRect` via `RowStore` + `local-query.ts`.
 * - Zero `getBoundingClientRect()` on `pointermove` (cached via `ResizeObserver` / resize / scroll / pointerdown).
 * - Numeric dirty-checked `RowDrawing` cache (zero per-row string key allocations).
 * - Lazy `Clock`-coalesced `[data-terminal-text]` DOM mirror (`MIRROR_DEBOUNCE_MS = 200` + `flushMirror()`).
 * - Bounded LRU `ImageCache` with off-main-thread `createImageBitmap` decoding.
 */
export class TerminalView {
  readonly element: HTMLDivElement
  readonly store = new RowStore()
  readonly viewport = new Viewport(this.store)
  readonly selection: Selection

  private readonly painter: Painter
  private readonly imageCache: ImageCache
  private readonly ownsImageCache: boolean
  private font: Font
  private theme: Theme
  private hostTheme: Theme
  private paletteOverrides: PaletteOverrides | null = null
  private reverseVideo = false

  private readonly drawings = new Map<number, KeptDrawing>()
  private readonly asking = new Set<number>()
  private frame: FrameUpdate | null = null
  private columns = 0
  private rows = 0
  private unacknowledgedSeq: number | null = null
  private unsubscribe: (() => void) | null = null
  private listening = false
  private visible = true
  private ended = false
  private scheduled: number | null = null
  private observer: ResizeObserver | null = null
  private unwatch: (() => void) | null = null
  private stopDisplay: (() => void) | null = null
  private pixelRatio: number | null = null
  private height = 0
  private cachedBox: CachedBox | null = null

  private readonly blink: Timer
  private cursorShown = true
  private readonly textBlink: Timer
  private textBlinking = false
  private textShown = true
  private readonly mirrorTimer: Timer
  private mirrorDirty = false
  private lastLayoutLines: number[] = []

  private focused = false
  private wasFollowing = true
  private disposed = false
  private wasSelected = false
  private drawnSeq = -1

  // Incremental SearchWatch state (zero polling timers!)
  private needle = ''
  private matchCase = false
  private matches: CellSpan[] = []
  private moreMatches = false
  private currentMatch = -1
  private finding = 0
  private matchVersion = 0
  private hoverVersion = 0

  private readonly gutter: number
  private commandList: CommandRecord[] = []
  private commandStarts: readonly CommandLine[] = []
  private commandPrompts: readonly CommandLine[] = []
  private folds: readonly Fold[] = []
  private readonly foldRows = new Map<string, PackedRow>()
  private readonly foldDrawings = new Map<PackedRow, RowDrawing>()
  private lastFit: { columns: number; rows: number; cellWidth: number; cellHeight: number } | null = null
  private lastHintSent: ViewportHint | null = null

  // v2 content-addressed image placements
  private placements: readonly ImagePlacement[] = []
  private imagesGenSeen = -1

  private readonly mac: boolean
  private readonly detectors: readonly Detector[]
  private readonly input: Input
  private readonly mirror: HTMLDivElement
  private readonly mirrored: Array<PackedRow | null | undefined> = []
  private readonly thumb: HTMLDivElement
  private thumbDrawn = { height: 0, top: 0, shown: false }
  private thumbRelease: (() => void) | null = null
  private hovered: { line: number; link: Link; runs: LinkRun[] } | null = null
  private hoverAt: { line: number; col: number; row: PackedRow | undefined } | null = null
  private pressed: Pressed | null = null
  private lastPress: { time: number; clientX: number; clientY: number; button: number; count: number } | null = null
  private reported: { col: number; row: number; button: number } | null = null
  private dragScroll: { point: Point; frame: number; at: number | null } | null = null
  private wheelOwed = 0
  private copyOnSelect: boolean
  private cursorInactive: CursorShape
  private linkPreview: LinkPreview

  constructor(private readonly source: TerminalSource, private readonly options: ViewOptions) {
    const clock = options.clock ?? systemClock
    this.blink = new Timer(clock)
    this.textBlink = new Timer(clock)
    this.mirrorTimer = new Timer(clock)
    this.mac = options.mac ?? /Mac|iPhone|iPad/.test(navigator.platform)
    this.gutter = options.gutter ?? 0
    this.copyOnSelect = options.copyOnSelect ?? false
    this.cursorInactive = options.cursorInactive ?? 'hollow'
    this.linkPreview = options.linkPreview ?? 'osc8'
    this.font = options.font
    this.theme = options.theme
    this.hostTheme = options.theme
    this.detectors = [urls, ...(options.detectors ?? [])]

    if (options.imageCache) {
      this.imageCache = options.imageCache
      this.ownsImageCache = false
    } else {
      this.imageCache = new ImageCache(options.imageCacheOptions)
      this.ownsImageCache = true
    }

    this.element = document.createElement('div')
    this.element.style.cssText = 'position:relative;width:100%;height:100%;overflow:hidden;cursor:text'
    this.painter = (options.painter ?? ((font) => new CanvasPainter(font)))(options.font)
    this.painter.canvas.style.display = 'block'
    this.painter.whenCleared?.(() => this.schedule())
    this.element.appendChild(this.painter.canvas)

    this.selection = new Selection(source, this.store, () => {
      const selected = this.selection.span !== null
      if (selected !== this.wasSelected) {
        this.wasSelected = selected
        this.options.onSelectionChange?.(selected)
      }
      this.schedule()
    })

    this.input = new Input(source, {
      mac: this.mac,
      optionIsAlt: options.optionIsAlt,
      intercept: (event) => this.shortcut(event) || (options.onKey?.(event) ?? false),
      paste: (text) => {
        void this.paste(text).catch((error: unknown) => {
          this.options.onPasteError?.(error)
        })
      },
      copy: () => { void this.copy() },
      focused: (focused) => this.setFocused(focused),
      typed: () => this.typed(),
      compositionStarted: () => this.selection.clear(),
      clock,
    })
    this.element.appendChild(this.input.field)

    this.mirror = document.createElement('div')
    this.mirror.setAttribute('role', 'document')
    this.mirror.setAttribute('aria-label', 'Terminal')
    this.mirror.dataset.terminalText = ''
    this.mirror.style.cssText = 'position:absolute;left:0;top:0;width:1px;height:1px;overflow:hidden;'
      + 'clip-path:inset(50%);white-space:pre;pointer-events:none;user-select:none'
    this.element.appendChild(this.mirror)

    this.thumb = document.createElement('div')
    this.thumb.style.cssText = 'position:absolute;right:2px;width:6px;border-radius:3px;top:0;height:0;'
      + 'background:currentColor;opacity:0;pointer-events:none;transition:opacity 120ms;cursor:default;z-index:2'
    this.element.appendChild(this.thumb)
    this.themed()

    this.element.addEventListener('wheel', this.wheel, { passive: false })
    this.element.addEventListener('pointerdown', this.pointerDown)
    this.element.addEventListener('pointermove', this.pointerMove)
    this.element.addEventListener('pointerup', this.pointerUp)
    this.element.addEventListener('pointercancel', this.pointerLost)
    this.element.addEventListener('lostpointercapture', this.pointerLost)
    this.element.addEventListener('pointerleave', this.pointerLeave)
    this.element.addEventListener('contextmenu', this.contextMenu)
    this.thumb.addEventListener('pointerdown', this.thumbDown)
  }

  setCopyOnSelect(on: boolean): void {
    this.copyOnSelect = on
  }

  setCursorInactive(style: CursorShape): void {
    this.cursorInactive = style
    this.schedule()
  }

  setLinkPreview(mode: LinkPreview): void {
    this.linkPreview = mode
    if (this.hovered) {
      this.element.title = this.previews(this.hovered.link) ? sanitizeTitle(this.hovered.link.target) : ''
    }
  }

  /**
   * Controls whether this view is visible (e.g. active tab vs background tab). Setting `visible: false`
   * sends a declarative `ViewportHint` that pauses broker frame delivery (`maxFrames = 0`) without
   * dropping the session attachment.
   */
  setVisible(visible: boolean): void {
    if (this.visible === visible) return
    this.visible = visible
    this.pushViewportHint()
    if (visible) this.schedule()
  }

  setTheme(theme: Theme): void {
    this.hostTheme = { ...theme, palette: [...theme.palette] }
    this.recolour()
  }

  private recolour(): void {
    const theme = sessionTheme(this.hostTheme, this.paletteOverrides, this.reverseVideo)
    if (theme === this.theme) return
    this.theme = theme
    this.themed()
    this.drawings.clear()
    this.foldDrawings.clear()
    this.schedule()
  }

  setFont(font: Font): void {
    const before = this.painter.metrics
    this.font = font
    this.painter.setFont(font)
    this.themed()
    const { cellWidth, cellHeight } = this.painter.metrics
    if (cellWidth !== before.cellWidth || cellHeight !== before.cellHeight) this.fit()
    this.schedule()
  }

  private themed(): void {
    const { background, foreground } = this.theme
    this.element.style.background = hex(background)
    this.thumb.style.color = hex(foreground)
    this.input.appearance({
      font: fontOf(this.font, false, false),
      color: hex(foreground),
      background: hex(background),
    })
  }

  selectAll(): void {
    if (!this.frame || this.rows <= 0 || this.columns <= 0) return
    const endLine = this.store.firstActive + this.rows - 1
    if (endLine < this.store.oldest) return
    this.selection.select({
      start: { line: this.store.oldest, col: 0 },
      end: { line: endLine, col: Math.max(0, this.columns - 1) },
    })
  }

  /**
   * Installs or clears (`needle === ''`) an incremental server-side `SearchWatch`.
   * Zero client-side polling: subsequent terminal mutations push `SearchDelta` patches automatically.
   */
  async find(needle: string, options: { matchCase?: boolean } = {}): Promise<number> {
    if (this.disposed) return 0
    const asked = (this.finding += 1)
    this.needle = needle
    this.matchCase = options.matchCase ?? false
    if (needle === '') {
      this.matches = []
      this.moreMatches = false
      this.currentMatch = -1
      this.matchVersion += 1
      this.options.onFound?.(this.found)
      this.schedule()
      void this.source.setSearch('', this.matchCase).catch(() => {})
      return 0
    }
    try {
      const snapshot = await this.source.setSearch(needle, this.matchCase)
      if (this.disposed || asked !== this.finding) return this.matches.length
      this.applySearchSnapshot(snapshot, true)
      return this.matches.length
    } catch {
      return 0
    }
  }

  get found(): { current: number; total: number; more: boolean } {
    return { current: this.currentMatch + 1, total: this.matches.length, more: this.moreMatches }
  }

  findNext(direction: -1 | 1 = 1): void {
    if (this.matches.length === 0) return
    this.currentMatch = (this.currentMatch + direction + this.matches.length) % this.matches.length
    this.matchVersion += 1
    this.showMatch()
  }

  private applySearchSnapshot(snapshot: SearchSnapshot, resetCurrent: boolean): void {
    if (this.store.epoch >= 0 && snapshot.epoch !== this.store.epoch) return
    const prev = this.matches[this.currentMatch]
    const sorted = sortMatchesNewestFirst(snapshot.matches)
    const held = sorted.slice(0, MATCH_HIGHLIGHTS)
    const more = snapshot.truncated || sorted.length > MATCH_HIGHLIGHTS
    let current: number
    if (held.length === 0) {
      current = -1
    } else if (resetCurrent) {
      current = 0
    } else {
      const still = prev
        ? held.findIndex((m) => m.start.line === prev.start.line && m.start.col === prev.start.col)
        : -1
      current = still !== -1 ? still : Math.max(0, Math.min(this.currentMatch, held.length - 1))
    }
    const before = this.found
    this.matches = held
    this.moreMatches = more
    this.currentMatch = current
    this.matchVersion += 1
    const after = this.found
    if (resetCurrent) {
      this.showMatch()
    } else {
      this.schedule()
    }
    if (after.current !== before.current || after.total !== before.total || after.more !== before.more) {
      this.options.onFound?.(after)
    }
  }

  private applySearchDelta(patch: SearchPatch): void {
    if (this.needle === '') return
    if (this.store.epoch >= 0 && patch.epoch !== this.store.epoch) return
    const prev = this.matches[this.currentMatch]
    const kept = this.matches.filter(
      (m) => m.end.line < patch.fromLine || m.start.line >= patch.toLine,
    )
    const merged = sortMatchesNewestFirst([...kept, ...patch.added])
    const held = merged.slice(0, MATCH_HIGHLIGHTS)
    const more = patch.truncated || merged.length > MATCH_HIGHLIGHTS
    let current: number
    if (held.length === 0) {
      current = -1
    } else {
      const still = prev
        ? held.findIndex((m) => m.start.line === prev.start.line && m.start.col === prev.start.col)
        : -1
      current = still !== -1 ? still : Math.max(0, Math.min(this.currentMatch, held.length - 1))
    }
    const before = this.found
    this.matches = held
    this.moreMatches = more
    this.currentMatch = current
    this.matchVersion += 1
    const after = this.found
    if (after.current !== before.current || after.total !== before.total || after.more !== before.more) {
      this.options.onFound?.(after)
    }
    this.schedule()
  }

  private showMatch(): void {
    const match = this.matches[this.currentMatch]
    if (match) {
      const { lines } = this.layout()
      const target = this.hiddenIn(match.start.line)?.first ?? match.start.line
      if (!lines.includes(target)) {
        this.scrollTo(this.viewport.stepRows(target, -Math.floor(this.rows / 2)))
      }
    }
    this.schedule()
  }

  private matchesOn(line: number): Array<{ from: number; to: number; current: boolean }> {
    const matches = this.matches
    let low = 0
    let high = matches.length
    while (low < high) {
      const middle = (low + high) >> 1
      if (matches[middle]!.start.line > line) low = middle + 1
      else high = middle
    }
    const on: Array<{ from: number; to: number; current: boolean }> = []
    for (let index = low; index < matches.length; index += 1) {
      const match = matches[index]!
      if (match.end.line < line) break
      on.push({
        from: line === match.start.line ? match.start.col : 0,
        to: line === match.end.line ? match.end.col + 1 : this.columns,
        current: index === this.currentMatch,
      })
    }
    return on
  }

  get commands(): readonly CommandRecord[] {
    return this.commandList
  }

  setFolds(folds: readonly Fold[]): void {
    const sorted = folds.filter((fold) => fold.last > fold.first).sort((a, b) => a.first - b.first)
    for (let index = 1; index < sorted.length; index += 1) {
      if (sorted[index]!.first <= sorted[index - 1]!.last) throw new RangeError('folds overlap')
    }
    this.folds = sorted
    this.viewport.setFolds(sorted)
    this.foldRows.clear()
    this.foldDrawings.clear()
    this.drawings.clear()
    this.followingMayHaveChanged()
    this.schedule()
  }

  private hiddenIn(line: number): Fold | undefined {
    return this.viewport.hiddenIn(line)
  }

  private layout(): { first: number; lines: number[] } {
    const [top, count] = this.viewport.visible
    const hidden = this.hiddenIn(top)
    const first = hidden ? hidden.first : top
    const lines: number[] = []
    let line = first
    while (lines.length < count) {
      const fold = this.hiddenIn(line)
      if (fold) line = fold.last + 1
      else {
        lines.push(line)
        line += 1
      }
    }
    return { first, lines }
  }

  private rowsBetween(from: number, to: number): number {
    return this.viewport.rowsBetween(from, to)
  }

  private foldRow(fold: Fold): PackedRow {
    const label = `▸ ${fold.label ?? `${fold.last - fold.first} lines folded`}`
    const key = `${fold.first}:${fold.last}:${label}`
    const known = this.foldRows.get(key)
    if (known) return known
    const chars = Array.from(label)
    const runs: Array<{ colStart: number; colLen: number; text: string; flags: number }> = []
    let col = 0
    for (const ch of chars) {
      const cp = ch.codePointAt(0) ?? 0
      const w = Math.max(1, cellsOfCodePoint(cp))
      const isAscii = ch.length === 1 && cp >= 0x20 && cp <= 0x7e && w === 1
      const last = runs.at(-1)
      if (isAscii && last && last.flags === 0 && last.colStart + last.colLen === col) {
        last.text += ch
        last.colLen += 1
      } else {
        runs.push({ colStart: col, colLen: w, text: ch, flags: isAscii ? 0 : RunFlag.PER_CELL })
      }
      col += w
    }
    const row = packRow({ line: fold.first, runs })
    this.foldRows.set(key, row)
    return row
  }

  private foldDrawing(fold: Fold): RowDrawing {
    const row = this.foldRow(fold)
    let drawing = this.foldDrawings.get(row)
    if (!drawing) {
      drawing = drawRow(row, this.theme)
      this.foldDrawings.set(row, drawing)
    }
    return drawing
  }

  private setCommands(commands: readonly CommandRecord[]): void {
    this.commandList = [...commands]
    this.rebuildCommandIndex()
    this.options.onCommands?.(this.commandList)
  }

  private upsertCommand(record: CommandRecord): void {
    const idx = this.commandList.findIndex((c) => c.id === record.id)
    if (idx === -1) this.commandList.push(record)
    else this.commandList[idx] = record
    this.rebuildCommandIndex()
    this.options.onCommands?.(this.commandList)
    this.schedule()
  }

  private rebuildCommandIndex(): void {
    const starts: CommandLine[] = []
    const prompts: CommandLine[] = []
    for (const command of this.commandList) {
      const begins = command.promptLine ?? command.outputLine
      if (begins !== null) starts.push({ line: begins, command })
      if (command.promptLine !== null) prompts.push({ line: command.promptLine, command })
    }
    starts.sort((a, b) => a.line - b.line)
    prompts.sort((a, b) => a.line - b.line)
    this.commandStarts = starts
    this.commandPrompts = prompts
  }

  commandAt(line: number): CommandRecord | null {
    return this.commandStarts[firstAfter(this.commandStarts, line) - 1]?.command ?? null
  }

  outputOf(command: CommandRecord): CellSpan | null {
    if (command.outputLine === null || !this.frame) return null
    const last = command.endLine !== null ? command.endLine - 1 : this.store.firstActive + this.rows - 1
    if (last < command.outputLine) return null
    return {
      start: { line: command.outputLine, col: 0 },
      end: { line: last, col: Math.max(0, this.columns - 1) },
    }
  }

  selectOutput(command: CommandRecord): boolean {
    const span = this.outputOf(command)
    if (span) this.selection.select(span)
    return span !== null
  }

  scrollToCommand(direction: -1 | 1): void {
    const top = Math.floor(this.viewport.top)
    const prompts = this.commandPrompts
    const target = direction === -1
      ? prompts[firstAfter(prompts, top - 1) - 1]
      : prompts[firstAfter(prompts, top)]
    if (target !== undefined) this.scrollTo(target.line)
    else if (direction === 1) this.follow()
  }

  private shortcut(event: KeyboardEvent): boolean {
    const held = this.mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && event.shiftKey
    if (!held || event.altKey || (event.code !== 'ArrowUp' && event.code !== 'ArrowDown')) return false
    this.scrollToCommand(event.code === 'ArrowUp' ? -1 : 1)
    return true
  }

  private wantPicturesInView(): void {
    const { first, lines } = this.layout()
    if (lines.length === 0) return
    this.wantPictures(first, lines[lines.length - 1]! - first + 1)
  }

  private wantPictures(first: number, count: number): void {
    if (this.ended || !this.listening || count <= 0) return
    const onReady = () => this.schedule()
    for (const placement of this.placements) {
      const pRows = Math.max(1, placement.cellSpan[1])
      if (placement.line + pRows <= first - count || placement.line >= first + count * 2) continue
      void this.imageCache.ensure(this.source, placement.blob, onReady)
    }
    for (let line = first - count; line < first + count * 2; line += 1) {
      const pieces = this.store.row(line)?.pieces
      if (!pieces) continue
      for (const piece of pieces) {
        void this.imageCache.ensure(this.source, piece.blob, onReady)
      }
    }
  }

  private imagesIn(first: number, span: number, lines: number[]): NonNullable<Scene['images']> {
    const { cellWidth, cellHeight } = this.painter.metrics
    const images: Array<NonNullable<Scene['images']>[number] & { z: number }> = []

    // 1. Placeholder pieces on visible rows (drawn under text at z = -1)
    for (const line of lines) {
      const row = this.store.row(line)
      if (!row?.pieces || this.folds.some((fold) => fold.first === line)) continue
      const top = this.rowsBetween(first, line) * cellHeight
      for (const piece of row.pieces) {
        const decoded = this.imageCache.get(piece.blob)
        if (!decoded) continue
        const [sx, sy, sw, sh] = piece.srcRect
        const [dstW, dstH] = piece.dstPx
        images.push({
          picture: decoded.source,
          x: piece.colStart * cellWidth,
          y: top,
          width: dstW > 0 ? dstW : piece.colLen * cellWidth,
          height: dstH > 0 ? dstH : cellHeight,
          source: {
            x: sx,
            y: sy,
            width: sw > 0 ? sw : decoded.width,
            height: sh > 0 ? sh : decoded.height,
          },
          above: false,
          z: -1,
        })
      }
    }

    // 2. Anchored placements from `source.images()`
    for (const placement of this.placements) {
      const pCols = Math.max(1, placement.cellSpan[0])
      const pRows = Math.max(1, placement.cellSpan[1])
      if (placement.line + pRows <= first || placement.line >= first + span) continue
      if (this.hiddenIn(placement.line)) continue
      const decoded = this.imageCache.get(placement.blob)
      if (!decoded) continue
      const [pxW, pxH] = placement.pixelSize
      images.push({
        picture: decoded.source,
        x: placement.col * cellWidth,
        y: this.rowsBetween(first, placement.line) * cellHeight,
        width: pxW > 0 ? pxW : pCols * cellWidth,
        height: pxH > 0 ? pxH : pRows * cellHeight,
        source: { x: 0, y: 0, width: decoded.width, height: decoded.height },
        above: placement.zIndex >= 0,
        z: placement.zIndex,
      })
    }

    return images.sort((a, b) => a.z - b.z)
  }

  private marksIn(first: number, lines: number[]): NonNullable<Scene['marks']> {
    if (this.gutter === 0 || lines.length === 0) return []
    const prompts = this.commandPrompts
    const last = lines[lines.length - 1]!
    const marks: Array<{ row: number; color: number }> = []
    for (let index = firstAfter(prompts, first - 1); index < prompts.length && prompts[index]!.line <= last; index += 1) {
      const { line, command } = prompts[index]!
      if (this.hiddenIn(line)) continue
      marks.push({
        row: this.rowsBetween(first, line),
        color: this.theme.palette[markColor(command)] ?? this.theme.foreground,
      })
    }
    return marks
  }

  clear(): void {
    this.source.sendKey({
      action: 'press',
      code: 'KeyL',
      modifiers: Modifier.ctrl,
      text: '',
      unshiftedCodepoint: 108,
      composed: false,
    })
  }

  focus(): void {
    this.input.field.focus({ preventScroll: true })
  }

  async copyCommand(command: CommandRecord): Promise<boolean> {
    return this.selectOutput(command) ? this.copy() : false
  }

  typeText(text: string): void {
    this.source.sendText(text)
    this.typed()
  }

  async paste(text: string): Promise<void> {
    if (this.disposed) return
    let outcome = await this.source.paste(text, false)
    if (this.disposed) return
    if (outcome === 'unsafe' && (await this.options.confirmPaste?.(text))) {
      if (this.disposed) return
      outcome = await this.source.paste(text, true)
    }
    if (!this.disposed && outcome === 'pasted') this.typed()
  }

  /**
   * Copies selected text. Resolves synchronously in 0 ms when all selected rows are cached in
   * `RowStore`, falling back to async broker RPC + Promise `ClipboardItem` only for uncached lines.
   */
  async copy(): Promise<boolean> {
    if (!this.selection.span) return false
    const sync = this.selection.syncText()
    if (sync !== null) {
      if (sync === '') return false
      if (this.options.onCopy) {
        this.options.onCopy(sync)
        return true
      }
      const clipboard = navigator.clipboard
      if (!clipboard) return false
      try {
        await clipboard.writeText(sync)
        return true
      } catch {
        return false
      }
    }

    const text = this.selection.text()
    if (this.options.onCopy) {
      const copied = await text.catch(() => '')
      if (copied !== '') this.options.onCopy(copied)
      return copied !== ''
    }
    text.catch(() => {})
    const clipboard = navigator.clipboard
    if (!clipboard) return false
    try {
      if (typeof ClipboardItem === 'function' && typeof clipboard.write === 'function') {
        const plain = text.then((copied) => {
          if (copied === '') throw new Error('nothing is selected')
          return new Blob([copied], { type: 'text/plain' })
        })
        plain.catch(() => {})
        await clipboard.write([new ClipboardItem({ 'text/plain': plain })])
        return true
      }
      const copied = await text
      if (copied === '') return false
      await clipboard.writeText(copied)
      return true
    } catch {
      return false
    }
  }

  mount(host: HTMLElement): void {
    if (this.disposed) return
    if (this.observer) {
      if (this.element.parentElement === host) return
      this.unmount()
    }
    host.appendChild(this.element)
    this.observer = new ResizeObserver(() => {
      this.refreshBox()
      if ((window.devicePixelRatio || 1) !== this.pixelRatio) this.size()
      this.fit()
      this.schedule()
    })
    this.observer.observe(host)
    this.watch()
    const sized = (window.devicePixelRatio || 1) !== this.pixelRatio
    if (sized) this.size()
    this.refreshBox()
    this.fit()
    this.resume()
    if (sized) this.schedule()
  }

  unmount(): void {
    this.observer?.disconnect()
    this.observer = null
    this.unwatch?.()
    this.unwatch = null
    this.lastFit = null
    this.lastHintSent = null
    this.cachedBox = null
    this.thumbRelease?.()
    this.input.clearKeyText()
    const pressed = this.pressed
    this.pressed = null
    if (pressed?.reporting) {
      this.reported = null
      const { cellWidth, cellHeight } = this.painter.metrics
      this.source.sendPointer({
        action: 'release',
        button: pressed.button,
        modifiers: 0,
        xPx: 0,
        yPx: 0,
        cellPx: { width: Math.max(1, Math.round(cellWidth)), height: Math.max(1, Math.round(cellHeight)) },
      })
    }
    this.setFocused(false)
    this.pause()
    this.element.remove()
  }

  fit(): void {
    const host = this.element.parentElement
    if (!host) return
    this.height = this.element.clientHeight
    if (host.clientWidth <= 0 || host.clientHeight <= 0) return
    const { cellWidth, cellHeight } = this.painter.metrics
    if (cellWidth <= 0 || cellHeight <= 0) return
    const columns = Math.max(1, Math.floor((host.clientWidth - this.gutter) / cellWidth))
    const rows = Math.max(1, Math.floor(host.clientHeight / cellHeight))
    const last = this.lastFit
    if (
      last
      && last.columns === columns
      && last.rows === rows
      && last.cellWidth === cellWidth
      && last.cellHeight === cellHeight
    ) {
      return
    }
    this.lastFit = { columns, rows, cellWidth, cellHeight }
    this.options.onFit?.({ columns, rows, cellWidth, cellHeight })
    this.pushViewportHint()
  }

  private pushViewportHint(): void {
    if (!this.listening || this.disposed) return
    const fit = this.lastFit
    const cols = fit?.columns ?? this.columns
    const rows = fit?.rows ?? this.rows
    if (cols <= 0 || rows <= 0) return
    const { cellWidth, cellHeight } = this.painter.metrics
    const hint: ViewportHint = {
      size: { cols, rows },
      cellPx: {
        width: Math.max(1, Math.round(cellWidth)),
        height: Math.max(1, Math.round(cellHeight)),
      },
      visible: this.visible,
      focused: this.focused,
    }
    const prev = this.lastHintSent
    if (
      prev
      && prev.size.cols === hint.size.cols
      && prev.size.rows === hint.size.rows
      && prev.cellPx.width === hint.cellPx.width
      && prev.cellPx.height === hint.cellPx.height
      && prev.visible === hint.visible
      && prev.focused === hint.focused
    ) {
      return
    }
    this.lastHintSent = hint
    this.source.sendViewportHint(hint)
  }

  private size(): boolean {
    const before = this.painter.metrics
    const nextRatio = window.devicePixelRatio || 1
    const ratioChanged = nextRatio !== this.pixelRatio
    this.pixelRatio = nextRatio
    this.painter.resize(this.columns, this.rows, this.pixelRatio, this.gutter)
    if (ratioChanged && this.listening) this.display()
    const { cellWidth, cellHeight } = this.painter.metrics
    return cellWidth !== before.cellWidth || cellHeight !== before.cellHeight
  }

  private readonly remeasure = () => {
    if (this.size()) this.fit()
    this.refreshBox()
    this.schedule()
  }

  private readonly fontsLoaded = (event: Event) => {
    const faces = (event as Event & { fontfaces?: readonly FontFace[] }).fontfaces
    const named = (family: string) => family.trim().replace(/^["']|["']$/g, '').toLowerCase()
    const own = new Set(this.font.family.split(',').map(named))
    if (!faces || faces.some((face) => own.has(named(face.family)))) this.remeasure()
  }

  private display(): void {
    this.stopDisplay?.()
    this.stopDisplay = null
    if (typeof matchMedia !== 'function') return
    const ratio = window.devicePixelRatio || 1
    let query = matchMedia(`(resolution: ${ratio}dppx)`)
    if (query.matches === false) {
      const min = Math.max(0.001, ratio - 0.001)
      const max = ratio + 0.001
      query = matchMedia(`(min-resolution: ${min}dppx) and (max-resolution: ${max}dppx)`)
    }
    const changed = () => {
      query.removeEventListener?.('change', changed)
      this.stopDisplay = null
      const before = this.pixelRatio
      this.remeasure()
      if (this.pixelRatio === before && this.listening) this.display()
    }
    query.addEventListener?.('change', changed)
    this.stopDisplay = () => {
      query.removeEventListener?.('change', changed)
      this.stopDisplay = null
    }
  }

  private watch(): void {
    this.unwatch?.()
    const stop: Array<() => void> = []
    this.display()
    stop.push(() => this.stopDisplay?.())
    const onResize = () => {
      this.refreshBox()
      if ((window.devicePixelRatio || 1) === this.pixelRatio) return
      this.remeasure()
    }
    const onScroll = () => {
      this.refreshBox()
    }
    window.addEventListener('resize', onResize)
    window.addEventListener('scroll', onScroll, { passive: true, capture: true })
    stop.push(() => window.removeEventListener('resize', onResize))
    stop.push(() => window.removeEventListener('scroll', onScroll, true))
    const fonts = document.fonts
    if (fonts && typeof fonts.addEventListener === 'function') {
      fonts.addEventListener('loadingdone', this.fontsLoaded)
      stop.push(() => fonts.removeEventListener('loadingdone', this.fontsLoaded))
    }
    this.unwatch = () => { for (const one of stop) one() }
  }

  setFocused(focused: boolean): void {
    if (focused === this.focused) return
    this.focused = focused
    this.blinkAgain()
    this.source.sendFocus(focused)
    this.pushViewportHint()
    this.schedule()
  }

  scrollBy(rows: number): void {
    this.viewport.scrollBy(rows)
    this.scrolled()
  }

  private scrollToPosition(position: number): void {
    this.viewport.scrollToPosition(position)
    this.scrolled()
  }

  scrollTo(line: number): void {
    this.viewport.scrollTo(line)
    this.scrolled()
  }

  follow(): void {
    this.viewport.follow()
    this.scrolled()
  }

  redraw(): void {
    this.schedule()
  }

  /** Synchronously flushes any pending lazy DOM mirror updates (useful for a11y/tests). */
  flushMirror(): void {
    this.mirrorTimer.cancel()
    this.mirrorDirty = false
    const lines = this.lastLayoutLines
    const count = lines.length
    while (this.mirror.childElementCount < count) this.mirror.appendChild(document.createElement('div'))
    while (this.mirror.childElementCount > count) this.mirror.lastElementChild!.remove()
    this.mirrored.length = count
    for (let index = 0; index < count; index += 1) {
      const line = lines[index]!
      const fold = this.folds.find((known) => known.first === line)
      const row = fold ? this.foldRow(fold) : (this.store.row(line) ?? null)
      if (this.mirrored[index] === row) continue
      this.mirrored[index] = row
      this.mirror.children[index]!.textContent = row ? rowPlainText(row) : ''
    }
  }

  dispose(): void {
    if (this.disposed) return
    this.unmount()
    this.disposed = true
    this.element.removeEventListener('wheel', this.wheel)
    this.element.removeEventListener('pointerdown', this.pointerDown)
    this.element.removeEventListener('pointermove', this.pointerMove)
    this.element.removeEventListener('pointerup', this.pointerUp)
    this.element.removeEventListener('pointercancel', this.pointerLost)
    this.element.removeEventListener('lostpointercapture', this.pointerLost)
    this.element.removeEventListener('pointerleave', this.pointerLeave)
    this.element.removeEventListener('contextmenu', this.contextMenu)
    this.thumb.removeEventListener('pointerdown', this.thumbDown)
    this.thumbRelease?.()
    this.pressed = null
    this.stopDragScroll()
    if (this.ownsImageCache) this.imageCache.dispose()
    this.input.dispose()
    this.painter.dispose()
  }

  private resume(): void {
    if (this.unsubscribe || this.disposed) return
    this.ended = false
    this.listening = true
    this.unsubscribe = this.source.subscribe(
      {
        onFrame: (frame) => this.take(frame),
        onSearchMatches: (snapshot) => this.applySearchSnapshot(snapshot, false),
        onSearchDelta: (patch) => this.applySearchDelta(patch),
        onEvent: (event) => this.handleEvent(event),
      },
      { manualAck: true },
    )
    this.pushViewportHint()
    if (this.focused) this.source.sendFocus(true)
    this.fetchCommands()
    this.wantPicturesInView()
    this.blinkAgain()
  }

  private pause(): void {
    this.unsubscribe?.()
    this.unsubscribe = null
    this.listening = false
    this.unacknowledgedSeq = null
    this.mirrorTimer.cancel()
    if (this.scheduled !== null) cancelAnimationFrame(this.scheduled)
    this.scheduled = null
    this.stopDragScroll()
    this.blinkAgain()
    this.blinkText(false)
  }

  private handleEvent(event: SessionEvent): void {
    if (this.disposed) return
    switch (event.kind) {
      case 'commandStarted':
      case 'commandFinished':
        this.upsertCommand(event.record)
        break
      case 'exited':
        this.ended = true
        this.options.onEnded?.(event.signal ?? (event.exitCode !== null ? `exited (${event.exitCode})` : undefined))
        break
      case 'streamClosed':
        this.ended = true
        this.options.onEnded?.(event.reason)
        break
      default:
        break
    }
    this.options.onEvent?.(event)
  }

  private fetchCommands(): void {
    if (this.disposed || this.ended) return
    const epoch = this.store.epoch
    void this.source.commands().then((commands) => {
      if (this.disposed || (epoch >= 0 && epoch !== this.store.epoch)) return
      this.setCommands(commands)
      this.schedule()
    }, () => {})
  }

  private blinkText(inView: boolean): void {
    if (inView && this.listening) {
      this.textBlinking = true
      if (!this.textBlink.active) this.scheduleTextBlink()
      return
    }
    this.textBlinking = false
    this.textBlink.cancel()
    if (!this.textShown) {
      this.textShown = true
      this.schedule()
    }
  }

  private scheduleTextBlink(): void {
    this.textBlink.arm(BLINK_MS, () => {
      if (!this.textBlinking || !this.listening || this.disposed) return
      this.textShown = !this.textShown
      this.schedule()
      if (this.textBlinking && this.listening && !this.disposed && !this.textBlink.active) {
        this.scheduleTextBlink()
      }
    })
  }

  private take(frame: FrameUpdate): void {
    const moved = frame.epoch !== this.store.epoch
    const after = moved ? this.viewport.acrossReset() : undefined
    const near = moved || this.viewport.following ? frame.firstActiveLine : this.viewport.top
    const { reset, changed } = this.store.apply(frame, near)

    if (reset) {
      this.folds = []
      this.viewport.setFolds([])
      this.foldRows.clear()
      this.foldDrawings.clear()
    }
    after?.()
    this.viewport.settle()

    if (reset) {
      this.drawings.clear()
      this.asking.clear()
      this.selection.reset()
      this.hovered = null
      this.hoverAt = null
      this.hoverVersion += 1
      this.element.style.cursor = 'text'
      this.element.title = ''
      this.setCommands([])
      this.placements = []
      this.imagesGenSeen = -1
      const hadSearch = this.needle !== '' || this.matches.length > 0 || this.moreMatches
      this.needle = ''
      this.matches = []
      this.moreMatches = false
      this.currentMatch = -1
      this.finding += 1
      this.matchVersion += 1
      if (hadSearch) this.options.onFound?.(this.found)
      this.fetchCommands()
    } else {
      for (const line of changed) this.drawings.delete(line)
    }

    if (frame.imagesGen !== this.imagesGenSeen) {
      const targetGen = (this.imagesGenSeen = frame.imagesGen)
      const epoch = frame.epoch
      void this.source.images().then((reply) => {
        if (this.disposed || epoch !== this.store.epoch || targetGen !== this.imagesGenSeen) return
        this.placements = reply.placements
        this.wantPicturesInView()
        this.schedule()
      }, () => {
        if (epoch === this.store.epoch && this.imagesGenSeen === targetGen) this.imagesGenSeen = -1
      })
    }

    if (frame.palette !== null || frame.modes.reverseVideo !== this.reverseVideo) {
      if (frame.palette !== null) this.paletteOverrides = frame.palette
      this.reverseVideo = frame.modes.reverseVideo
      this.recolour()
    }

    this.frame = frame
    this.unacknowledgedSeq = frame.seq

    if (frame.size.cols !== this.columns || frame.size.rows !== this.rows) {
      this.columns = frame.size.cols
      this.rows = frame.size.rows
      if (this.size()) this.fit()
      this.refreshBox()
    }

    this.blinkAgain()
    this.followingMayHaveChanged()
    this.schedule()
  }

  private cursorBlinks(): boolean {
    const cursor = this.frame?.cursor
    return cursor !== undefined && !cursor.hidden && cursor.blinking
  }

  private blinkAgain(): void {
    this.blink.cancel()
    if (!this.cursorShown) {
      this.cursorShown = true
      this.schedule()
    }
    if (!this.focused || !this.listening || !this.cursorBlinks()) return
    this.scheduleBlink()
  }

  private scheduleBlink(): void {
    this.blink.arm(BLINK_MS, () => {
      if (!this.focused || !this.listening || this.disposed || !this.cursorBlinks()) return
      this.cursorShown = !this.cursorShown
      this.schedule()
      if (this.focused && this.listening && !this.disposed && this.cursorBlinks() && !this.blink.active) {
        this.scheduleBlink()
      }
    })
  }

  private typed(): void {
    if ((this.options.followOnInput ?? true) && !this.viewport.following) this.follow()
    this.blinkAgain()
  }

  private scrolled(): void {
    this.followingMayHaveChanged()
    this.schedule()
  }

  private followingMayHaveChanged(): void {
    const following = this.viewport.following
    if (following === this.wasFollowing) return
    this.wasFollowing = following
    this.options.onFollowingChange?.(following)
  }

  private readonly wheel = (event: WheelEvent) => {
    const frame = this.frame
    if (!frame || event.ctrlKey) return
    const { cellHeight } = this.painter.metrics
    const lines =
      event.deltaMode === 1
        ? event.deltaY
        : event.deltaMode === 2
          ? event.deltaY * this.rows
          : event.deltaY / cellHeight
    const toProgram = frame.modes.mouse && !event.shiftKey && this.viewport.following
    if (!toProgram && !frame.modes.altScreen) {
      if (this.store.scrollback === 0) return
      event.preventDefault()
      this.scrollBy(lines)
      return
    }
    event.preventDefault()
    if (this.wheelOwed !== 0 && Math.sign(lines) !== Math.sign(this.wheelOwed)) this.wheelOwed = 0
    this.wheelOwed += lines
    const notches = Math.trunc(this.wheelOwed)
    this.wheelOwed -= notches
    for (let sent = 0; sent < Math.min(Math.abs(notches), 20); sent += 1) {
      if (toProgram) {
        this.source.sendPointer(this.pointerInput(event, notches < 0 ? 'wheelUp' : 'wheelDown', 255))
      } else {
        this.source.sendKey({
          action: 'press',
          code: notches < 0 ? 'ArrowUp' : 'ArrowDown',
          modifiers: 0,
          text: '',
          unshiftedCodepoint: 0,
          composed: false,
        })
      }
    }
  }

  /** Refreshes cached canvas bounding box (called on mount, ResizeObserver, scroll, resize, pointerdown — never pointermove). */
  private refreshBox(): CachedBox {
    const canvas = this.painter.canvas
    const box = canvas.getBoundingClientRect()
    const scaleX = canvas.offsetWidth > 0 && box.width > 0 ? box.width / canvas.offsetWidth : 1
    const scaleY = canvas.offsetHeight > 0 && box.height > 0 ? box.height / canvas.offsetHeight : 1
    this.cachedBox = { left: box.left, top: box.top, scaleX, scaleY }
    return this.cachedBox
  }

  private pointIn(point: Point): { x: number; y: number } {
    const box = this.cachedBox ?? this.refreshBox()
    return {
      x: (point.clientX - box.left) / box.scaleX - this.gutter,
      y: (point.clientY - box.top) / box.scaleY,
    }
  }

  private positionOf(point: Point): CellPos {
    const { cellWidth, cellHeight } = this.painter.metrics
    const { x, y } = this.pointIn(point)
    const { lines } = this.layout()
    const row = Math.floor(y / cellHeight + this.viewport.offset)
    const line = lines.length > 0 ? lines[Math.max(0, Math.min(row, lines.length - 1))]! : this.store.firstActive
    return {
      line,
      col: Math.max(0, Math.min(Math.floor(x / cellWidth), Math.max(0, this.columns - 1))),
    }
  }

  private beyond(point: Point): number {
    const { cellHeight } = this.painter.metrics
    const { y } = this.pointIn(point)
    const bottom = this.rows * cellHeight
    return y < 0 ? y / cellHeight : y > bottom ? (y - bottom) / cellHeight : 0
  }

  private pointerInput(event: MouseEvent, action: PointerInput['action'], button: number): PointerInput {
    const { cellWidth, cellHeight } = this.painter.metrics
    const { x, y } = this.pointIn(event)
    const maxX = this.columns * cellWidth
    const maxY = this.rows * cellHeight
    return {
      action,
      button,
      modifiers: modifiersOf(event),
      xPx: Math.round(Math.max(0, Math.min(x, maxX))),
      yPx: Math.round(Math.max(0, Math.min(y, maxY))),
      cellPx: {
        width: Math.max(1, Math.round(cellWidth)),
        height: Math.max(1, Math.round(cellHeight)),
      },
    }
  }

  private reportMove(event: MouseEvent, button: number): void {
    const input = this.pointerInput(event, 'motion', button)
    const col = Math.floor(input.xPx / input.cellPx.width)
    const row = Math.floor(input.yPx / input.cellPx.height)
    const last = this.reported
    if (last && last.col === col && last.row === row && last.button === button) return
    this.reported = { col, row, button }
    this.source.sendPointer(input)
  }

  private clicks(event: PointerEvent): number {
    const last = this.lastPress
    const again =
      last !== null
      && last.button === event.button
      && event.timeStamp - last.time <= CLICK_AGAIN_MS
      && Math.abs(event.clientX - last.clientX) <= CLICK_AGAIN_PIXELS
      && Math.abs(event.clientY - last.clientY) <= CLICK_AGAIN_PIXELS
    const count = again && last.count < 3 ? last.count + 1 : 1
    this.lastPress = {
      time: event.timeStamp,
      clientX: event.clientX,
      clientY: event.clientY,
      button: event.button,
      count,
    }
    return count
  }

  private readonly pointerDown = (event: PointerEvent) => {
    if (event.target === this.thumb) return
    if (this.pressed) {
      event.preventDefault()
      return
    }
    this.refreshBox()
    this.focus()
    event.preventDefault()
    const at = this.positionOf(event)
    const reporting = this.frame?.modes.mouse === true && !event.shiftKey && this.viewport.following
    this.pressed = { button: event.button, reporting, at, moved: false, block: event.altKey }
    this.element.setPointerCapture?.(event.pointerId)
    if (reporting) {
      const input = this.pointerInput(event, 'press', event.button)
      this.reported = {
        col: Math.floor(input.xPx / input.cellPx.width),
        row: Math.floor(input.yPx / input.cellPx.height),
        button: event.button,
      }
      this.source.sendPointer(input)
      return
    }
    if (event.button !== 0) return
    if (event.shiftKey && this.selection.span) {
      this.selection.extendTo(at)
      return
    }
    const clicks = this.clicks(event)
    const granularity: Granularity = clicks === 3 ? 'line' : clicks === 2 ? 'word' : 'cell'
    if (granularity === 'cell') this.selection.clear()
    else this.selection.begin(at, granularity)
  }

  private readonly pointerMove = (event: PointerEvent) => {
    const pressed = this.pressed
    if (pressed?.reporting) {
      this.reportMove(event, pressed.button)
      return
    }
    if (!pressed && this.frame?.modes.mouse && !event.shiftKey && this.viewport.following) {
      this.reportMove(event, 255)
    }
    const at = this.positionOf(event)
    if (pressed?.button === 0) {
      if (!pressed.moved && (at.line !== pressed.at.line || at.col !== pressed.at.col)) {
        pressed.moved = true
        if (!this.selection.span) this.selection.begin(pressed.at, 'cell', pressed.block ? 'block' : 'linear')
      }
      if (pressed.moved) {
        this.selection.extendTo(at)
        this.dragScrollTo(event)
      }
      return
    }
    this.hover(at)
  }

  private readonly pointerUp = (event: PointerEvent) => {
    const pressed = this.released(event)
    if (!pressed || pressed.reporting) return
    if (pressed.button === 0 && this.copyOnSelect && this.selection.span) void this.copy()
    const opening = this.mac ? event.metaKey : event.ctrlKey
    if (pressed.button === 0 && !pressed.moved && opening) {
      const link = this.linkOn(this.positionOf(event))
      if (link) this.options.onLink?.(link, event)
    }
  }

  private readonly pointerLost = (event: PointerEvent) => {
    this.released(event)
  }

  private released(event: PointerEvent): Pressed | null {
    const pressed = this.pressed
    if (!pressed) return null
    if (event.type === 'pointerup' && event.button !== pressed.button) return null
    this.pressed = null
    this.stopDragScroll()
    if (this.element.hasPointerCapture?.(event.pointerId)) this.element.releasePointerCapture(event.pointerId)
    if (pressed.reporting) {
      this.reported = null
      this.source.sendPointer(this.pointerInput(event, 'release', pressed.button))
    }
    return pressed
  }

  private dragScrollTo(point: Point): void {
    if (this.beyond(point) === 0) return this.stopDragScroll()
    const kept = { clientX: point.clientX, clientY: point.clientY }
    if (this.dragScroll) this.dragScroll.point = kept
    else this.dragScroll = { point: kept, at: null, frame: requestAnimationFrame(this.dragScrollStep) }
  }

  private readonly dragScrollStep = (time: number) => {
    const drag = this.dragScroll
    if (!drag) return
    const elapsed = drag.at === null ? 16 : Math.min(Math.max(time - drag.at, 0), 100)
    drag.at = time
    const top = this.viewport.top
    this.scrollBy((this.beyond(drag.point) * elapsed * DRAG_LINES_PER_SECOND) / 1000)
    this.selection.extendTo(this.positionOf(drag.point))
    if (elapsed > 0 && this.viewport.top === top) {
      this.dragScroll = null
      return
    }
    drag.frame = requestAnimationFrame(this.dragScrollStep)
  }

  private stopDragScroll(): void {
    if (this.dragScroll) cancelAnimationFrame(this.dragScroll.frame)
    this.dragScroll = null
  }

  private readonly pointerLeave = () => this.hover(null)

  private readonly contextMenu = (event: MouseEvent) => {
    if (this.frame?.modes.mouse && !event.shiftKey && this.viewport.following) event.preventDefault()
  }

  private linkOn(at: CellPos): Link | null {
    const row = this.store.row(at.line)
    return row ? linkAt(row, at.col, this.detectors, (line) => this.store.row(line)) : null
  }

  private hover(at: CellPos | null): void {
    const over = at ? { line: at.line, col: at.col, row: this.store.row(at.line) } : null
    const last = this.hoverAt
    this.hoverAt = over
    if (over && last && over.line === last.line && over.col === last.col && over.row === last.row) return
    const link = at ? this.linkOn(at) : null
    const before = this.hovered
    const same =
      before !== null
      && link !== null
      && at !== null
      && before.line === at.line
      && before.link.from === link.from
      && before.link.to === link.to
    if (same || (!before && !link)) return
    const row = at ? this.store.row(at.line) : undefined
    this.hovered =
      link && at && row
        ? { line: at.line, link, runs: linkRuns(link, row, at.col, (line) => this.store.row(line)) }
        : null
    this.hoverVersion += 1
    this.element.style.cursor = link ? 'pointer' : 'text'
    this.element.title = link && this.previews(link) ? sanitizeTitle(link.target) : ''
    this.schedule()
  }

  private previews(link: Link): boolean {
    return this.linkPreview === 'all' || (this.linkPreview === 'osc8' && link.kind === 'declared')
  }

  private thumbAt(): { height: number; top: number; travel: number } | null {
    const { total, position, shown } = this.viewport.extent
    if (total <= shown || this.height <= 0) return null
    const height = Math.min(this.height, Math.max(THUMB_MIN, (shown / total) * this.height))
    const travel = this.height - height
    return { height, top: (position / (total - shown)) * travel, travel }
  }

  private readonly thumbDown = (event: PointerEvent) => {
    event.preventDefault()
    event.stopPropagation()
    this.thumbRelease?.()
    const start = { y: event.clientY, position: this.viewport.extent.position }
    const move = (moved: PointerEvent) => {
      const { total, shown } = this.viewport.extent
      const travel = this.thumbAt()?.travel ?? 0
      if (travel > 0) this.scrollToPosition(start.position + ((moved.clientY - start.y) / travel) * (total - shown))
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      window.removeEventListener('pointercancel', up)
      this.thumbRelease = null
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
    window.addEventListener('pointercancel', up)
    this.thumbRelease = up
  }

  private schedule(): void {
    if (this.scheduled !== null || this.disposed || !this.frame || !this.listening || !this.visible) return
    this.scheduled = requestAnimationFrame(() => {
      this.scheduled = null
      this.paint()
    })
  }

  private paint(): void {
    const frame = this.frame
    if (!frame) return
    try {
      const { first, lines } = this.layout()
      const span = lines.length > 0 ? lines[lines.length - 1]! - first + 1 : 0

      let blinking = false
      for (const line of lines) {
        const row = this.store.row(line)
        if (row !== undefined && blinksIn(row)) {
          blinking = true
          break
        }
      }
      this.blinkText(blinking)

      const rows: Array<RowDrawing | null> = lines.map((line) => {
        const fold = this.folds.find((known) => known.first === line)
        return fold ? this.foldDrawing(fold) : this.drawingOf(line)
      })

      if (span > 0) {
        for (const line of this.drawings.keys()) {
          if (line < first - span || line >= first + span * 2) this.drawings.delete(line)
        }
        this.ask(first, span)
        this.wantPictures(first, span)
      }

      const cursor = this.cursorIn(frame, lines)
      const scene: Scene = {
        theme: this.theme,
        rows,
        offset: this.viewport.offset,
        scrollPosition: this.viewport.extent.position,
        cursor,
        marks: this.marksIn(first, lines),
        images: this.imagesIn(first, span, lines),
      }
      this.painter.paint(scene)
      this.afterPaint(frame, first, lines)
    } finally {
      if (this.unacknowledgedSeq !== null && !this.ended) {
        this.source.ackFrame?.(this.unacknowledgedSeq)
      }
      this.unacknowledgedSeq = null
    }
  }

  private afterPaint(frame: FrameUpdate, first: number, lines: number[]): void {
    this.lastLayoutLines = lines
    // Populate DOM mirror synchronously on initial empty paint, then coalesce at 200 ms via Clock
    if (this.mirror.childElementCount === 0 && lines.length > 0) {
      this.flushMirror()
    } else {
      this.mirrorDirty = true
      if (!this.mirrorTimer.active) {
        this.mirrorTimer.arm(MIRROR_DEBOUNCE_MS, () => {
          if (this.mirrorDirty && !this.disposed) this.flushMirror()
        })
      }
    }

    const thumb = this.thumbAt()
    const next = {
      height: thumb?.height ?? 0,
      top: thumb?.top ?? 0,
      shown: thumb !== null && !this.viewport.following,
    }
    const last = this.thumbDrawn
    if (next.height !== last.height) this.thumb.style.height = `${next.height}px`
    if (next.top !== last.top) this.thumb.style.top = `${next.top}px`
    if (next.shown !== last.shown) {
      this.thumb.style.opacity = next.shown ? '0.35' : '0'
      this.thumb.style.pointerEvents = next.shown ? 'auto' : 'none'
    }
    this.thumbDrawn = next

    const { cellWidth, cellHeight } = this.painter.metrics
    const cursorLine = frame.firstActiveLine + frame.cursor.row
    const row = this.rowsBetween(first, this.hiddenIn(cursorLine)?.first ?? cursorLine)
    this.input.moveTo(
      this.gutter + frame.cursor.col * cellWidth,
      Math.max(0, row - this.viewport.offset) * cellHeight,
      cellHeight,
    )

    if (frame.seq !== this.drawnSeq) {
      this.drawnSeq = frame.seq
      this.element.dataset.terminalDrawn = String(frame.seq)
    }
  }

  private drawingOf(line: number): RowDrawing | null {
    const row = this.store.row(line)
    if (!row) return null
    const selVer = this.selection.version
    const matchVer = this.matchVersion
    const hoverVer = this.hoverVersion
    const blinkedOff = !this.textShown && blinksIn(row)

    const kept = this.drawings.get(line)
    if (
      kept
      && kept.row === row
      && kept.selVer === selVer
      && kept.matchVer === matchVer
      && kept.hoverVer === hoverVer
      && kept.blinkedOff === blinkedOff
    ) {
      return kept.drawing
    }

    const selected: SelectedCells | undefined = this.selection.on(line)
    const hovered = this.hovered?.runs.filter((run) => run.line === line) ?? []
    const matched = this.matches.length > 0 ? this.matchesOn(line) : []
    const { matchBackground = 0x623315, currentMatchBackground = 0x9e6a03 } = this.theme
    const found = matched.map((m) => ({
      from: m.from,
      to: m.to,
      color: m.current ? currentMatchBackground : matchBackground,
    }))
    let drawing = drawRow(row, this.theme, selected, blinkedOff, found)
    if (hovered.length > 0) {
      drawing = {
        ...drawing,
        lines: [
          ...drawing.lines,
          ...hovered.map((run) => ({
            kind: 'single' as const,
            column: run.from,
            cells: run.to - run.from,
            color: this.theme.foreground,
          })),
        ],
      }
    }
    this.drawings.set(line, { row, selVer, matchVer, hoverVer, blinkedOff, drawing })
    return drawing
  }

  private ask(first: number, count: number): void {
    if (this.ended) return
    for (const span of this.store.missing(first - count, count * 3)) {
      for (let page = Math.floor(span.from / PAGE_LINES); page * PAGE_LINES < span.from + span.count; page += 1) {
        this.askPage(page)
      }
    }
  }

  private askPage(page: number): void {
    if (this.asking.has(page) || this.store.epoch < 0) return
    const runs = this.store.missing(page * PAGE_LINES, PAGE_LINES)
    const last = runs.at(-1)
    if (!last) return
    const from = runs[0]!.from
    const to = last.from + last.count
    this.asking.add(page)
    const epoch = this.store.epoch
    const atImagesChanged = this.store.imagesChanged
    void this.source.fetchRows(epoch, from, to - from).then((fetched) => {
      if (this.disposed || epoch !== this.store.epoch) return
      this.asking.delete(page)
      const absorbed = this.store.absorb(epoch, from, to - from, fetched, this.viewport.top, atImagesChanged)
      for (const line of absorbed) this.drawings.delete(line)
      if (absorbed.length > 0) this.schedule()
    }, () => {
      if (epoch === this.store.epoch) this.asking.delete(page)
    })
  }

  private cursorIn(frame: FrameUpdate, lines: number[]): Scene['cursor'] {
    const cursor = frame.cursor
    if (cursor.hidden || !this.cursorShown) return undefined
    const line = frame.firstActiveLine + cursor.row
    const index = lines.indexOf(line)
    if (index === -1) return undefined
    const fold = this.folds.find((known) => known.first === line)
    const row = fold ? this.foldRow(fold) : this.store.row(line)
    const cell = row ? cellAtColumn(row, cursor.col) : null
    const visible =
      cell !== null
      && (cell.flags & PackedStyleFlag.INVISIBLE) === 0
      && !(!this.textShown && (cell.flags & PackedStyleFlag.BLINK) !== 0)
    return {
      column: cell ? cell.col : cursor.col,
      row: index,
      cells: cell ? cell.width : 1,
      shape: this.focused ? cursor.shape : this.cursorInactive,
      text:
        cell && visible && cell.text !== ' ' && cell.text !== ''
          ? {
              text: cell.text,
              bold: (cell.flags & PackedStyleFlag.BOLD) !== 0,
              italic: (cell.flags & PackedStyleFlag.ITALIC) !== 0,
            }
          : undefined,
    }
  }
}

function cellAtColumn(
  row: PackedRow,
  col: number,
): { readonly col: number; readonly width: number; readonly text: string; readonly flags: number } | null {
  const { runs, styles, text } = row
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const start = colWord & 0xffff
    const len = colWord >>> 16
    if (col < start) break
    if (col >= start + len) continue
    const txtWord = runs[i + 1]!
    const metaWord = runs[i + 2]!
    const runFlags = runs[i + 3]!
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    const styleIdx = metaWord & 0xffff
    const styleBase = styleIdx * 4
    const flags = styleBase + 3 < styles.length ? styles[styleBase + 3]! : 0
    const slice = text.slice(u16Off, u16Off + u16Len)

    if ((runFlags & RunFlag.PER_CELL) !== 0) {
      const chars = Array.from(slice)
      if (chars.length <= 1) return { col: start, width: len, text: slice, flags }
      const per = Math.max(1, Math.floor(len / chars.length))
      let cur = start
      for (let k = 0; k < chars.length; k += 1) {
        const w = k + 1 === chars.length ? Math.max(1, start + len - cur) : per
        if (col >= cur && col < cur + w) return { col: cur, width: w, text: chars[k]!, flags }
        cur += w
      }
      return null
    }

    const offset = col - start
    if (offset >= 0 && offset < slice.length) {
      return { col, width: 1, text: slice[offset]!, flags }
    }
    return null
  }
  return null
}

function sortMatchesNewestFirst(spans: readonly CellSpan[]): CellSpan[] {
  return [...spans].sort((a, b) => b.start.line - a.start.line || b.start.col - a.start.col)
}

function hex(color: number): string {
  return `#${color.toString(16).padStart(6, '0')}`
}

function markColor(command: CommandRecord): number {
  if (command.startedMs === null) return 8
  const { outcome } = command
  switch (outcome.kind) {
    case 'running': return 4
    case 'exited': return outcome.code === 0 ? 2 : 1
    case 'unknown': return 8
    case 'claimed': return 3
  }
}

function firstAfter(items: ReadonlyArray<{ readonly line: number }>, line: number): number {
  let low = 0
  let high = items.length
  while (low < high) {
    const middle = (low + high) >> 1
    if (items[middle]!.line <= line) low = middle + 1
    else high = middle
  }
  return low
}

const UNSAFE_TITLE_CHARS = /[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/g
function sanitizeTitle(text: string): string {
  return text.replace(UNSAFE_TITLE_CHARS, '')
}
