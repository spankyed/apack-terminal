import { Timer, type Clock } from './clock.ts'
import { type Font, type Painter } from './painter.ts'
import { type Selection } from './selection.ts'
import { type TerminalSource, type ViewportHint } from './source.ts'
import { BLINK_MS } from './view-types.ts'
import { MAX_GRID_COLS, MAX_GRID_ROWS } from './wire/codec.ts'

export interface FitSize {
  readonly columns: number
  readonly rows: number
  readonly cellWidth: number
  readonly cellHeight: number
}

export interface DisplayHost {
  readonly element: HTMLDivElement
  readonly painter: Painter
  readonly source: TerminalSource
  readonly gutter: number
  font(): Font
  columns(): number
  rows(): number
  isListening(): boolean
  isDisposed(): boolean
  isEnded(): boolean
  isVisible(): boolean
  isFocused(): boolean
  onFit(fit: FitSize): void
  refreshBox(): void
  schedule(): void
}

/**
 * Owns container fit measurement, `ResizeObserver`, `devicePixelRatio` (`matchMedia`) & webfont
 * re-measurement, and deduplicated `ViewportHint` transmission.
 */
export class DisplayController {
  private observer: ResizeObserver | null = null
  private unwatch: (() => void) | null = null
  private stopDisplay: (() => void) | null = null
  private pixelRatio: number | null = null
  private containerHeight = 0
  private lastFit: FitSize | null = null
  private lastHintSent: ViewportHint | null = null

  constructor(private readonly host: DisplayHost) {}

  get height(): number {
    return this.containerHeight
  }

  get mounted(): boolean {
    return this.observer !== null
  }

  attach(container: HTMLElement): boolean {
    this.observer = new ResizeObserver(() => {
      this.host.refreshBox()
      if ((window.devicePixelRatio || 1) !== this.pixelRatio) this.size()
      this.fit()
      this.host.schedule()
    })
    this.observer.observe(container)
    this.watch()
    const sized = (window.devicePixelRatio || 1) !== this.pixelRatio
    if (sized) this.size()
    this.host.refreshBox()
    this.fit()
    return sized
  }

  detach(): void {
    this.observer?.disconnect()
    this.observer = null
    this.unwatch?.()
    this.unwatch = null
    this.lastFit = null
    this.lastHintSent = null
  }

  fit(): void {
    const parent = this.host.element.parentElement
    if (!parent) return
    this.containerHeight = this.host.element.clientHeight
    if (parent.clientWidth <= 0 || parent.clientHeight <= 0) return
    const { cellWidth, cellHeight } = this.host.painter.metrics
    if (cellWidth <= 0 || cellHeight <= 0) return
    const columns = Math.min(MAX_GRID_COLS, Math.max(1, Math.floor((parent.clientWidth - this.host.gutter) / cellWidth)))
    const rows = Math.min(MAX_GRID_ROWS, Math.max(1, Math.floor(parent.clientHeight / cellHeight)))
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
    this.host.onFit({ columns, rows, cellWidth, cellHeight })
    this.pushViewportHint()
  }

  pushViewportHint(): void {
    if (!this.host.isListening() || this.host.isDisposed() || this.host.isEnded()) return
    const fit = this.lastFit
    const cols = fit?.columns ?? this.host.columns()
    const rows = fit?.rows ?? this.host.rows()
    if (cols <= 0 || rows <= 0) return
    const { cellWidth, cellHeight } = this.host.painter.metrics
    const hint: ViewportHint = {
      size: { cols, rows },
      cellPx: {
        width: Math.max(1, Math.round(cellWidth)),
        height: Math.max(1, Math.round(cellHeight)),
      },
      visible: this.host.isVisible(),
      focused: this.host.isFocused(),
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
    this.host.source.sendViewportHint(hint)
  }

  size(): boolean {
    const before = this.host.painter.metrics
    const nextRatio = window.devicePixelRatio || 1
    const ratioChanged = nextRatio !== this.pixelRatio
    this.pixelRatio = nextRatio
    this.host.painter.resize(this.host.columns(), this.host.rows(), this.pixelRatio, this.host.gutter)
    if (ratioChanged && this.host.isListening()) this.display()
    const { cellWidth, cellHeight } = this.host.painter.metrics
    return cellWidth !== before.cellWidth || cellHeight !== before.cellHeight
  }

  private readonly remeasure = () => {
    if (this.size()) this.fit()
    this.host.refreshBox()
    this.host.schedule()
  }

  private readonly fontsLoaded = (event: Event) => {
    const faces = (event as Event & { fontfaces?: readonly FontFace[] }).fontfaces
    const named = (family: string) => family.trim().replace(/^["']|["']$/g, '').toLowerCase()
    const own = new Set(this.host.font().family.split(',').map(named))
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
      if (this.pixelRatio === before && this.host.isListening()) this.display()
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
      this.host.refreshBox()
      if ((window.devicePixelRatio || 1) === this.pixelRatio) return
      this.remeasure()
    }
    const onScroll = () => {
      this.host.refreshBox()
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
}

/**
 * Copies selected text. Resolves synchronously in 0 ms when all selected rows are cached in
 * `RowStore`, falling back to async broker RPC + Promise `ClipboardItem` only for uncached lines.
 */
export async function copySelection(selection: Selection, onCopy?: (text: string) => void): Promise<boolean> {
  if (!selection.span) return false
  const sync = selection.syncText()
  if (sync !== null) {
    if (sync === '') return false
    if (onCopy) {
      onCopy(sync)
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

  const text = selection.text()
  if (onCopy) {
    const copied = await text.catch(() => '')
    if (copied !== '') onCopy(copied)
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

export interface BlinkHost {
  isActive(): boolean
  isFocused(): boolean
  cursorBlinks(): boolean
  schedule(): void
}

/**
 * Owns cursor blink and SGR-blink text phase timers via the view's `Clock`.
 */
export class BlinkController {
  private readonly cursorTimer: Timer
  private readonly textTimer: Timer
  private cursorVisible = true
  private textBlinking = false
  private textVisible = true

  constructor(private readonly host: BlinkHost, clock: Clock) {
    this.cursorTimer = new Timer(clock)
    this.textTimer = new Timer(clock)
  }

  get cursorShown(): boolean {
    return this.cursorVisible
  }

  get textShown(): boolean {
    return this.textVisible
  }

  /** Synchronizes blink timers with `host.isActive()` (`visible && listening && !disposed`). */
  syncActive(): void {
    if (!this.host.isActive()) {
      this.cursorTimer.cancel()
      this.textTimer.cancel()
      this.cursorVisible = true
      this.textVisible = true
      return
    }
    this.resetCursor()
    if (this.textBlinking && !this.textTimer.active) {
      this.armText()
    }
  }

  resetCursor(): void {
    this.cursorTimer.cancel()
    if (!this.cursorVisible) {
      this.cursorVisible = true
      this.host.schedule()
    }
    if (!this.host.isActive() || !this.host.isFocused() || !this.host.cursorBlinks()) return
    this.armCursor()
  }

  setTextBlinking(inView: boolean): void {
    if (inView && this.host.isActive()) {
      this.textBlinking = true
      if (!this.textTimer.active) this.armText()
      return
    }
    this.textBlinking = false
    this.textTimer.cancel()
    if (!this.textVisible) {
      this.textVisible = true
      this.host.schedule()
    }
  }

  private armCursor(): void {
    this.cursorTimer.arm(BLINK_MS, () => {
      if (!this.host.isActive() || !this.host.isFocused() || !this.host.cursorBlinks()) {
        return
      }
      this.cursorVisible = !this.cursorVisible
      this.host.schedule()
      if (this.host.isActive() && this.host.isFocused() && this.host.cursorBlinks() && !this.cursorTimer.active) {
        this.armCursor()
      }
    })
  }

  private armText(): void {
    this.textTimer.arm(BLINK_MS, () => {
      if (!this.textBlinking || !this.host.isActive()) return
      this.textVisible = !this.textVisible
      this.host.schedule()
      if (this.textBlinking && this.host.isActive() && !this.textTimer.active) {
        this.armText()
      }
    })
  }
}
