import { CanvasPainter } from './canvas-painter.ts'
import { drawRow, type RowDrawing, type SelectedCells, type Theme } from './draw.ts'
import type { Font, Painter, Scene } from './painter.ts'
import { RowStore } from './row-store.ts'
import type { Frame, Row, TerminalSource } from './source.ts'
import { Viewport } from './viewport.ts'

/** How long the cursor is shown, and then hidden, while it blinks */
export const BLINK_MS = 530

export interface ViewOptions {
  readonly font: Font
  readonly theme: Theme
  /**
   * Makes the painter. Left out, a view draws on a 2D canvas; a host that has a GPU painter passes it, and
   * a case passes one that records.
   */
  readonly painter?: (font: Font) => Painter
  /**
   * The view measured how many cells fit where it is mounted. The grid a session runs at is the owner's to
   * decide — several views may be showing it — so this is a report, and the view goes on drawing the grid
   * its frames describe.
   */
  readonly onFit?: (fit: { columns: number; rows: number; cellWidth: number; cellHeight: number }) => void
  /** Whether the view is following the output changed, which a host draws a "jump to bottom" control from */
  readonly onFollowingChange?: (following: boolean) => void
  /** The session ended */
  readonly onEnded?: () => void
}

interface Kept {
  readonly row: Row
  readonly selected: string
  readonly drawing: RowDrawing
}

/**
 * A terminal, drawn.
 *
 * **It draws when something changed and at no other time.** A frame arrives, the user scrolls, a
 * selection moves, the cursor blinks: each asks for one paint on the next animation frame, and with
 * nothing asking there is no loop running. A frame is acknowledged once it has been painted, which is what
 * lets the source send the next — so a view that is slow to draw is sent fewer frames rather than more
 * than it can use, and a view that is not mounted is sent none.
 */
export class TerminalView {
  readonly element: HTMLDivElement
  readonly store = new RowStore()
  readonly viewport = new Viewport(this.store)

  private readonly painter: Painter
  private readonly drawings = new Map<number, Kept>()
  private readonly asked = new Set<number>()
  private frame: Frame | null = null
  /** The frame to acknowledge once it is on the screen */
  private unacknowledged: number | null = null
  private detach: (() => void) | null = null
  private scheduled: number | null = null
  private observer: ResizeObserver | null = null
  private blink: ReturnType<typeof setInterval> | null = null
  private cursorShown = true
  private focused = false
  private wasFollowing = true
  private disposed = false
  /** What is selected on each line, for `selectionOn`. Set by the selection, which is the only writer */
  selectionOn: (line: number) => SelectedCells | undefined = () => undefined

  constructor(private readonly source: TerminalSource, private readonly options: ViewOptions) {
    this.element = document.createElement('div')
    this.element.style.cssText = 'position:relative;width:100%;height:100%;overflow:hidden'
    this.element.style.background = `#${options.theme.background.toString(16).padStart(6, '0')}`
    this.painter = (options.painter ?? ((font) => new CanvasPainter(font)))(options.font)
    this.painter.canvas.style.display = 'block'
    this.element.appendChild(this.painter.canvas)
    this.element.addEventListener('wheel', this.wheel, { passive: false })
  }

  /** Puts the view in `host` and starts its frames */
  mount(host: HTMLElement): void {
    host.appendChild(this.element)
    this.observer = new ResizeObserver(() => this.fit())
    this.observer.observe(host)
    this.fit()
    this.resume()
  }

  /**
   * Takes the view off the page and stops its frames. What it has been sent is kept, so mounting it again
   * draws at once and asks only for what changed meanwhile.
   */
  unmount(): void {
    this.observer?.disconnect()
    this.observer = null
    this.pause()
    this.element.remove()
  }

  /** Measures how many cells fit, and reports it */
  fit(): void {
    const host = this.element.parentElement
    if (!host) return
    const { cellWidth, cellHeight } = this.painter.metrics
    const columns = Math.max(1, Math.floor(host.clientWidth / cellWidth))
    const rows = Math.max(1, Math.floor(host.clientHeight / cellHeight))
    this.options.onFit?.({ columns, rows, cellWidth, cellHeight })
  }

  /** The view has, or has lost, the keyboard: an unfocused terminal shows a hollow cursor that does not blink */
  setFocused(focused: boolean): void {
    this.focused = focused
    this.cursorShown = true
    if (this.blink) clearInterval(this.blink)
    this.blink = null
    if (focused && this.frame?.cursor?.blinking) this.startBlinking()
    this.source.focus(focused)
    this.schedule()
  }

  scrollBy(lines: number): void {
    this.viewport.scrollBy(lines)
    this.scrolled()
  }

  scrollTo(line: number): void {
    this.viewport.scrollTo(line)
    this.scrolled()
  }

  /** Shows the screen again and follows it */
  follow(): void {
    this.viewport.follow()
    this.scrolled()
  }

  /** Something outside the rows changed what a row looks like — a selection moved */
  redraw(): void {
    this.schedule()
  }

  dispose(): void {
    if (this.disposed) return
    this.disposed = true
    this.unmount()
    this.element.removeEventListener('wheel', this.wheel)
    if (this.blink) clearInterval(this.blink)
    this.painter.dispose()
  }

  private resume(): void {
    if (this.detach || this.disposed) return
    // Rows that were on the screen when the frames stopped may have changed since and scrolled away, and
    // nothing said so. History older than that cannot have changed
    this.store.forgetFrom(this.store.firstActive)
    this.detach = this.source.attach({
      frame: (frame) => this.take(frame),
      ended: () => this.options.onEnded?.(),
    })
  }

  private pause(): void {
    this.detach?.()
    this.detach = null
    this.unacknowledged = null
    if (this.scheduled !== null) cancelAnimationFrame(this.scheduled)
    this.scheduled = null
  }

  private take(frame: Frame): void {
    const after = frame.epoch !== this.store.epoch ? this.viewport.acrossReset() : undefined
    const { reset, changed } = this.store.apply(frame)
    after?.()
    if (reset) {
      this.drawings.clear()
      this.asked.clear()
    } else {
      for (const line of changed) this.drawings.delete(line)
    }
    const blinking = frame.cursor?.blinking === true
    if (blinking !== (this.frame?.cursor?.blinking === true) && this.focused) {
      if (this.blink) clearInterval(this.blink)
      this.blink = null
      this.cursorShown = true
      if (blinking) this.startBlinking()
    }
    this.frame = frame
    this.unacknowledged = frame.sequence
    if (frame.columns !== this.columns || frame.screenRows !== this.rows) {
      this.columns = frame.columns
      this.rows = frame.screenRows
      this.painter.resize(frame.columns, frame.screenRows, window.devicePixelRatio || 1)
    }
    this.followingMayHaveChanged()
    this.schedule()
  }

  private columns = 0
  private rows = 0

  private startBlinking(): void {
    this.blink = setInterval(() => {
      this.cursorShown = !this.cursorShown
      this.schedule()
    }, BLINK_MS)
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
    if (this.store.scrollback === 0) return
    event.preventDefault()
    const { cellHeight } = this.painter.metrics
    // A wheel reports pixels, lines or pages; a trackpad reports pixels, which is what makes the view move
    // by fractions of a row
    const lines = event.deltaMode === 1 ? event.deltaY : event.deltaMode === 2 ? event.deltaY * this.rows : event.deltaY / cellHeight
    this.scrollBy(lines)
  }

  private schedule(): void {
    if (this.scheduled !== null || this.disposed || !this.frame) return
    this.scheduled = requestAnimationFrame(() => {
      this.scheduled = null
      this.paint()
    })
  }

  private paint(): void {
    const frame = this.frame
    if (!frame) return
    const [first, count] = this.viewport.visible
    const rows: Array<RowDrawing | null> = []
    for (let line = first; line < first + count; line += 1) rows.push(this.drawingOf(line))
    this.ask(first, count)

    const scene: Scene = { theme: this.options.theme, rows, offset: this.viewport.offset, cursor: this.cursorIn(frame, first, count) }
    this.painter.paint(scene)

    if (this.unacknowledged !== null) {
      this.source.acknowledge(this.unacknowledged)
      this.unacknowledged = null
    }
  }

  private drawingOf(line: number): RowDrawing | null {
    const row = this.store.row(line)
    if (!row) return null
    const selected = this.selectionOn(line)
    const key = selected ? `${selected.from}-${selected.to}` : ''
    const kept = this.drawings.get(line)
    if (kept && kept.row === row && kept.selected === key) return kept.drawing
    const drawing = drawRow(row, this.store.styles, this.options.theme, selected)
    this.drawings.set(line, { row, selected: key, drawing })
    return drawing
  }

  /** Asks for the lines in view that are not held, once each */
  private ask(first: number, count: number): void {
    for (const [from, length] of this.store.missing(first, count)) {
      if (this.asked.has(from)) continue
      this.asked.add(from)
      const epoch = this.store.epoch
      void this.source.fetch(from, length).then((fetched) => {
        this.asked.delete(from)
        if (this.disposed || epoch !== this.store.epoch) return
        for (const line of this.store.absorb(fetched)) this.drawings.delete(line)
        this.schedule()
      }, () => { this.asked.delete(from) })
    }
  }

  private cursorIn(frame: Frame, first: number, count: number): Scene['cursor'] {
    const cursor = frame.cursor
    if (!cursor || !this.cursorShown) return undefined
    const line = frame.firstActive + cursor.row
    if (line < first || line >= first + count) return undefined
    const row = this.store.row(line)
    const at = row ? row.column.indexOf(cursor.column) : -1
    const style = row && at !== -1 ? this.store.styles[row.style[at]!] : undefined
    return {
      column: cursor.column,
      row: line - first,
      cells: row && at !== -1 ? row.width[at]! : 1,
      shape: this.focused ? cursor.shape : 'hollow',
      text: row && at !== -1 ? { text: row.text[at]!, bold: ((style?.flags ?? 0) & 1) !== 0, italic: ((style?.flags ?? 0) & 2) !== 0 } : undefined,
    }
  }
}
