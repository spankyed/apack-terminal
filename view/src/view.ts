import { CanvasPainter } from './canvas-painter.ts'
import { systemClock } from './clock.ts'
import { sessionTheme, type Theme } from './draw.ts'
import { ImageCache } from './images.ts'
import { Input } from './input.ts'
import { urls, type Detector } from './links.ts'
import { fontOf, type Font, type Painter } from './painter.ts'
import { RowStore } from './row-store.ts'
import { Selection } from './selection.ts'
import {
  Modifier,
  type CellSpan,
  type CommandRecord,
  type CursorShape,
  type FrameUpdate,
  type PaletteOverrides,
  type SessionEvent,
  type TerminalSource,
} from './source.ts'
import { Viewport } from './viewport.ts'
import { CommandFoldController } from './view-commands.ts'
import { BlinkController, DisplayController, copySelection } from './view-display.ts'
import { PointerController } from './view-pointer.ts'
import { ViewRenderer } from './view-render.ts'
import { SearchController } from './view-search.ts'
import {
  BLINK_MS,
  MATCH_HIGHLIGHTS,
  MIRROR_DEBOUNCE_MS,
  hex,
  type Fold,
  type LinkPreview,
  type ViewOptions,
} from './view-types.ts'

export { BLINK_MS, MATCH_HIGHLIGHTS, MIRROR_DEBOUNCE_MS, type Fold, type LinkPreview, type ViewOptions }

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
  private readonly unsubImageEvict: () => void
  private readonly gutter: number
  private readonly mac: boolean
  private readonly detectors: readonly Detector[]
  private readonly input: Input
  private readonly mirror: HTMLDivElement
  private readonly thumb: HTMLDivElement
  private readonly search: SearchController
  private readonly commandFolds: CommandFoldController
  private readonly display: DisplayController
  private readonly pointer: PointerController
  private readonly renderer: ViewRenderer

  private font: Font
  private theme: Theme
  private hostTheme: Theme
  private paletteOverrides: PaletteOverrides | null = null
  private reverseVideo = false
  private frame: FrameUpdate | null = null
  private columns = 0
  private rows = 0
  private unacknowledgedSeq: number | null = null
  private unsubscribe: (() => void) | null = null
  private listening = false
  private visible = true
  private ended = false
  private scheduled: number | null = null

  private readonly blink: BlinkController
  private focused = false
  private wasFollowing = true
  private disposed = false
  private wasSelected = false
  private copyOnSelect: boolean
  private cursorInactive: CursorShape
  private linkPreview: LinkPreview

  constructor(private readonly source: TerminalSource, private readonly options: ViewOptions) {
    const clock = options.clock ?? systemClock
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
    this.unsubImageEvict = this.imageCache.onEvict((key) => this.painter.releaseImage?.(key))
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

    this.commandFolds = new CommandFoldController(this.viewport, this.gutter, options.onCommands)
    this.search = new SearchController({
      source,
      isDisposed: () => this.disposed,
      epoch: () => this.store.epoch,
      columns: () => this.columns,
      onFound: (found) => this.options.onFound?.(found),
      showMatch: (match) => this.showMatch(match),
      schedule: () => this.schedule(),
    })
    this.display = new DisplayController({
      element: this.element,
      painter: this.painter,
      source,
      gutter: this.gutter,
      font: () => this.font,
      columns: () => this.columns,
      rows: () => this.rows,
      isListening: () => this.listening,
      isDisposed: () => this.disposed,
      isVisible: () => this.visible,
      isFocused: () => this.focused,
      onFit: (fit) => this.options.onFit?.(fit),
      refreshBox: () => this.pointer.refreshBox(),
      schedule: () => this.schedule(),
    })
    this.pointer = new PointerController({
      element: this.element,
      thumb: this.thumb,
      painter: this.painter,
      source,
      store: this.store,
      viewport: this.viewport,
      selection: this.selection,
      gutter: this.gutter,
      mac: this.mac,
      detectors: this.detectors,
      frame: () => this.frame,
      columns: () => this.columns,
      rows: () => this.rows,
      height: () => this.display.height,
      layout: () => this.commandFolds.layout(),
      copyOnSelect: () => this.copyOnSelect,
      linkPreview: () => this.linkPreview,
      onLink: (link, event) => this.options.onLink?.(link, event),
      focus: () => this.focus(),
      copy: () => this.copy(),
      scrollBy: (rows) => this.scrollBy(rows),
      scrollToPosition: (position) => this.scrollToPosition(position),
      schedule: () => this.schedule(),
    })
    this.blink = new BlinkController(
      {
        isActive: () => this.isActive(),
        isFocused: () => this.focused,
        cursorBlinks: () => this.cursorBlinks(),
        schedule: () => this.schedule(),
      },
      clock,
    )
    this.renderer = new ViewRenderer(
      {
        element: this.element,
        mirror: this.mirror,
        painter: this.painter,
        imageCache: this.imageCache,
        input: this.input,
        source,
        store: this.store,
        viewport: this.viewport,
        selection: this.selection,
        search: this.search,
        commands: this.commandFolds,
        pointer: this.pointer,
        gutter: this.gutter,
        theme: () => this.theme,
        isDisposed: () => this.disposed,
        isEnded: () => this.ended,
        isListening: () => this.listening,
        isFocused: () => this.focused,
        isCursorShown: () => this.blink.cursorShown,
        isTextShown: () => this.blink.textShown,
        cursorInactive: () => this.cursorInactive,
        blinkText: (inView) => this.blink.setTextBlinking(inView),
        schedule: () => this.schedule(),
      },
      clock,
    )
    this.themed()
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
    this.pointer.updateLinkPreviewTitle()
  }

  /**
   * Controls whether this view is visible (e.g. active tab vs background tab). Setting `visible: false`
   * sends a declarative `ViewportHint` that pauses broker frame delivery (`maxFrames = 0`) without
   * dropping the session attachment.
   */
  setVisible(visible: boolean): void {
    if (this.visible === visible) return
    this.visible = visible
    if (!visible && this.scheduled !== null) {
      cancelAnimationFrame(this.scheduled)
      this.scheduled = null
    }
    this.blink.syncActive()
    this.display.pushViewportHint()
    if (!visible) {
      this.renderer.cancelMirrorTimer()
      if (this.unacknowledgedSeq !== null && !this.ended) {
        this.source.ackFrame?.(this.unacknowledgedSeq)
        this.unacknowledgedSeq = null
      }
    } else {
      this.schedule()
    }
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
    this.renderer.clearDrawings()
    this.commandFolds.clearFoldDrawings()
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
    return this.search.find(needle, options)
  }

  get found(): { current: number; total: number; more: boolean } {
    return this.search.found
  }

  findNext(direction: -1 | 1 = 1): void {
    this.search.findNext(direction)
  }

  private showMatch(match: CellSpan | undefined): void {
    if (match) {
      const { lines } = this.commandFolds.layout()
      const target = this.commandFolds.hiddenIn(match.start.line)?.first ?? match.start.line
      if (!lines.includes(target)) {
        this.scrollTo(this.viewport.stepRows(target, -Math.floor(this.rows / 2)))
      }
    }
    this.schedule()
  }

  get commands(): readonly CommandRecord[] {
    return this.commandFolds.commands
  }

  setFolds(folds: readonly Fold[]): void {
    this.commandFolds.setFolds(folds)
    this.renderer.clearDrawings()
    this.followingMayHaveChanged()
    this.schedule()
  }

  commandAt(line: number): CommandRecord | null {
    return this.commandFolds.commandAt(line)
  }

  outputOf(command: CommandRecord): CellSpan | null {
    return this.commandFolds.outputOf(command, this.frame !== null, this.store.firstActive, this.rows, this.columns)
  }

  selectOutput(command: CommandRecord): boolean {
    const span = this.outputOf(command)
    if (span) this.selection.select(span)
    return span !== null
  }

  scrollToCommand(direction: -1 | 1): void {
    const target = this.commandFolds.scrollTarget(direction)
    if (target === 'follow') this.follow()
    else if (target !== null) this.scrollTo(target)
  }

  private shortcut(event: KeyboardEvent): boolean {
    const held = this.mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && event.shiftKey
    if (!held || event.altKey || (event.code !== 'ArrowUp' && event.code !== 'ArrowDown')) return false
    this.scrollToCommand(event.code === 'ArrowUp' ? -1 : 1)
    return true
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
    return copySelection(this.selection, this.options.onCopy)
  }

  mount(host: HTMLElement): void {
    if (this.disposed) return
    if (this.display.mounted) {
      if (this.element.parentElement === host) return
      this.unmount()
    }
    host.appendChild(this.element)
    const sized = this.display.attach(host)
    this.resume()
    if (sized) this.schedule()
  }

  unmount(): void {
    this.display.detach()
    this.pointer.releaseOnUnmount()
    this.input.reset()
    this.setFocused(false)
    this.pause()
    this.element.remove()
  }

  fit(): void {
    this.display.fit()
  }

  setFocused(focused: boolean): void {
    if (focused === this.focused) return
    this.focused = focused
    this.blink.resetCursor()
    this.source.sendFocus(focused)
    this.display.pushViewportHint()
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
    this.renderer.flushMirror()
  }

  dispose(): void {
    if (this.disposed) return
    this.unmount()
    this.disposed = true
    this.unsubImageEvict()
    this.pointer.dispose()
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
        onSearchMatches: (snapshot) => this.search.applySnapshot(snapshot, false),
        onSearchDelta: (patch) => this.search.applyDelta(patch),
        onEvent: (event) => this.handleEvent(event),
      },
      { manualAck: true },
    )
    this.display.pushViewportHint()
    if (this.focused) this.source.sendFocus(true)
    this.fetchCommands()
    this.renderer.wantPicturesInView()
    this.blink.resetCursor()
    this.schedule()
  }

  private pause(): void {
    if (this.unacknowledgedSeq !== null && !this.ended) {
      this.source.ackFrame?.(this.unacknowledgedSeq)
    }
    this.unacknowledgedSeq = null
    this.unsubscribe?.()
    this.unsubscribe = null
    this.listening = false
    this.renderer.cancelMirrorTimer()
    if (this.scheduled !== null) cancelAnimationFrame(this.scheduled)
    this.scheduled = null
    this.pointer.stopDragScroll()
    this.blink.resetCursor()
    this.blink.setTextBlinking(false)
  }

  private handleEvent(event: SessionEvent): void {
    if (this.disposed) return
    switch (event.kind) {
      case 'commandStarted':
      case 'commandFinished':
        this.commandFolds.upsertCommand(event.record)
        this.schedule()
        break
      case 'exited':
        if (!this.ended) {
          this.ended = true
          this.options.onEnded?.(event.signal ?? (event.exitCode !== null ? `exited (${event.exitCode})` : undefined))
        }
        break
      case 'streamClosed':
        if (!this.ended) {
          this.ended = true
          this.options.onEnded?.(event.reason)
        }
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
      this.commandFolds.setCommands(commands)
      this.schedule()
    }, () => {})
  }

  private take(frame: FrameUpdate): void {
    const moved = frame.epoch !== this.store.epoch
    const after = moved ? this.viewport.acrossReset() : undefined
    const near = moved || this.viewport.following ? frame.firstActiveLine : this.viewport.top
    const { reset, changed } = this.store.apply(frame, near)

    if (reset) this.commandFolds.clearFolds()
    after?.()
    this.viewport.settle()

    if (reset) {
      this.renderer.resetForEpoch()
      this.selection.reset()
      this.pointer.resetHover()
      this.commandFolds.setCommands([])
      this.search.resetForEpoch()
      this.fetchCommands()
    } else {
      for (const line of changed) this.renderer.deleteDrawing(line)
    }

    this.renderer.syncFrameImages(frame)

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
      if (this.display.size()) this.fit()
      this.pointer.refreshBox()
    }

    this.blink.resetCursor()
    this.followingMayHaveChanged()
    this.schedule()
  }

  private cursorBlinks(): boolean {
    const cursor = this.frame?.cursor
    return cursor !== undefined && !cursor.hidden && cursor.blinking
  }

  private typed(): void {
    if ((this.options.followOnInput ?? true) && !this.viewport.following) this.follow()
    this.blink.resetCursor()
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

  private isActive(): boolean {
    return this.visible && this.listening && !this.disposed
  }

  private schedule(): void {
    if (this.scheduled !== null || !this.frame || !this.isActive()) return
    this.scheduled = requestAnimationFrame(() => {
      this.scheduled = null
      this.paint()
    })
  }

  private paint(): void {
    const frame = this.frame
    if (!frame) return
    try {
      this.renderer.paint(frame)
    } finally {
      if (this.unacknowledgedSeq !== null && !this.ended) {
        this.source.ackFrame?.(this.unacknowledgedSeq)
      }
      this.unacknowledgedSeq = null
    }
  }
}
