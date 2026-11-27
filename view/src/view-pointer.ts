import { modifiersOf } from './input.ts'
import { linkAt, linkRuns, type Detector, type Link, type LinkRun } from './links.ts'
import { type Painter } from './painter.ts'
import { type RowStore } from './row-store.ts'
import { type Granularity, type Selection } from './selection.ts'
import { type CellPos, type FrameUpdate, type PackedRow, type PointerInput, type TerminalSource } from './source.ts'
import { type Viewport } from './viewport.ts'
import { sanitizeTitle, type LinkPreview } from './view-types.ts'

const DRAG_LINES_PER_SECOND = 15
const CLICK_AGAIN_MS = 500
const CLICK_AGAIN_PIXELS = 4
const THUMB_MIN = 20

interface Pressed {
  readonly pointerId: number
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

export interface PointerHost {
  readonly element: HTMLDivElement
  readonly thumb: HTMLDivElement
  readonly painter: Painter
  readonly source: TerminalSource
  readonly store: RowStore
  readonly viewport: Viewport
  readonly selection: Selection
  readonly gutter: number
  readonly mac: boolean
  readonly detectors: readonly Detector[]
  frame(): FrameUpdate | null
  columns(): number
  rows(): number
  height(): number
  layout(): { first: number; lines: number[] }
  copyOnSelect(): boolean
  linkPreview(): LinkPreview
  onLink(link: Link, event: MouseEvent): void
  focus(): void
  copy(): Promise<boolean>
  scrollBy(rows: number): void
  scrollToPosition(position: number): void
  schedule(): void
}

/**
 * Owns mouse/pointer selection, PTY mouse reporting, link hover/open, wheel scrolling,
 * scrollbar thumb dragging, and cached canvas bounding-box coordinates.
 */
export class PointerController {
  private cachedBox: CachedBox | null = null
  private thumbDrawn = { height: 0, top: 0, shown: false }
  private thumbRelease: (() => void) | null = null
  private hovered: { line: number; link: Link; runs: LinkRun[] } | null = null
  private hoverAt: { line: number; col: number; row: PackedRow | undefined } | null = null
  private hoverVer = 0
  private pressed: Pressed | null = null
  private lastPress: { time: number; clientX: number; clientY: number; button: number; count: number } | null = null
  private reported: { col: number; row: number; button: number } | null = null
  private dragScroll: { point: Point; frame: number; at: number | null } | null = null
  private wheelOwed = 0

  constructor(private readonly host: PointerHost) {
    const { element, thumb } = host
    element.addEventListener('wheel', this.wheel, { passive: false })
    element.addEventListener('pointerdown', this.pointerDown)
    element.addEventListener('pointermove', this.pointerMove)
    element.addEventListener('pointerup', this.pointerUp)
    element.addEventListener('pointercancel', this.pointerLost)
    element.addEventListener('lostpointercapture', this.pointerLost)
    element.addEventListener('pointerleave', this.pointerLeave)
    element.addEventListener('contextmenu', this.contextMenu)
    thumb.addEventListener('pointerdown', this.thumbDown)
  }

  get hoverVersion(): number {
    return this.hoverVer
  }

  hoveredRunsOn(line: number): readonly LinkRun[] {
    return this.hovered?.runs.filter((run) => run.line === line) ?? []
  }

  updateLinkPreviewTitle(): void {
    if (this.hovered) {
      this.host.element.title = this.previews(this.hovered.link) ? sanitizeTitle(this.hovered.link.target) : ''
    }
  }

  resetHover(): void {
    this.hovered = null
    this.hoverAt = null
    this.hoverVer += 1
    this.host.element.style.cursor = 'text'
    this.host.element.title = ''
  }

  /** Refreshes cached canvas bounding box (called on mount, ResizeObserver, scroll, resize, pointerdown — never pointermove). */
  refreshBox(): CachedBox {
    const canvas = this.host.painter.canvas
    const box = canvas.getBoundingClientRect()
    const scaleX = canvas.offsetWidth > 0 && box.width > 0 ? box.width / canvas.offsetWidth : 1
    const scaleY = canvas.offsetHeight > 0 && box.height > 0 ? box.height / canvas.offsetHeight : 1
    this.cachedBox = { left: box.left, top: box.top, scaleX, scaleY }
    return this.cachedBox
  }

  releaseOnUnmount(): void {
    this.cachedBox = null
    this.thumbRelease?.()
    const pressed = this.pressed
    this.pressed = null
    if (pressed?.reporting) {
      this.reported = null
      const { cellWidth, cellHeight } = this.host.painter.metrics
      this.host.source.sendPointer({
        action: 'release',
        button: pressed.button,
        modifiers: 0,
        xPx: 0,
        yPx: 0,
        cellPx: { width: Math.max(1, Math.round(cellWidth)), height: Math.max(1, Math.round(cellHeight)) },
      })
    }
  }

  stopDragScroll(): void {
    if (this.dragScroll) cancelAnimationFrame(this.dragScroll.frame)
    this.dragScroll = null
  }

  updateThumb(): void {
    const thumb = this.thumbAt()
    const next = {
      height: thumb?.height ?? 0,
      top: thumb?.top ?? 0,
      shown: thumb !== null && !this.host.viewport.following,
    }
    const last = this.thumbDrawn
    if (next.height !== last.height) this.host.thumb.style.height = `${next.height}px`
    if (next.top !== last.top) this.host.thumb.style.top = `${next.top}px`
    if (next.shown !== last.shown) {
      this.host.thumb.style.opacity = next.shown ? '0.35' : '0'
      this.host.thumb.style.pointerEvents = next.shown ? 'auto' : 'none'
    }
    this.thumbDrawn = next
  }

  dispose(): void {
    const { element, thumb } = this.host
    element.removeEventListener('wheel', this.wheel)
    element.removeEventListener('pointerdown', this.pointerDown)
    element.removeEventListener('pointermove', this.pointerMove)
    element.removeEventListener('pointerup', this.pointerUp)
    element.removeEventListener('pointercancel', this.pointerLost)
    element.removeEventListener('lostpointercapture', this.pointerLost)
    element.removeEventListener('pointerleave', this.pointerLeave)
    element.removeEventListener('contextmenu', this.contextMenu)
    thumb.removeEventListener('pointerdown', this.thumbDown)
    this.thumbRelease?.()
    this.pressed = null
    this.stopDragScroll()
  }

  private readonly wheel = (event: WheelEvent) => {
    const frame = this.host.frame()
    if (!frame || event.ctrlKey) return
    const { cellHeight } = this.host.painter.metrics
    const lines =
      event.deltaMode === 1
        ? event.deltaY
        : event.deltaMode === 2
          ? event.deltaY * this.host.rows()
          : event.deltaY / cellHeight
    const toProgram = frame.modes.mouse && !event.shiftKey && this.host.viewport.following
    if (!toProgram && !frame.modes.altScreen) {
      if (this.host.store.scrollback === 0) return
      event.preventDefault()
      this.host.scrollBy(lines)
      return
    }
    event.preventDefault()
    if (this.wheelOwed !== 0 && Math.sign(lines) !== Math.sign(this.wheelOwed)) this.wheelOwed = 0
    this.wheelOwed += lines
    const notches = Math.trunc(this.wheelOwed)
    this.wheelOwed -= notches
    for (let sent = 0; sent < Math.min(Math.abs(notches), 20); sent += 1) {
      if (toProgram) {
        this.host.source.sendPointer(this.pointerInput(event, notches < 0 ? 'wheelUp' : 'wheelDown', 255))
      } else {
        this.host.source.sendKey({
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

  private pointIn(point: Point): { x: number; y: number } {
    const box = this.cachedBox ?? this.refreshBox()
    return {
      x: (point.clientX - box.left) / box.scaleX - this.host.gutter,
      y: (point.clientY - box.top) / box.scaleY,
    }
  }

  private positionOf(point: Point): CellPos {
    const { cellWidth, cellHeight } = this.host.painter.metrics
    const { x, y } = this.pointIn(point)
    const { lines } = this.host.layout()
    const row = Math.floor(y / cellHeight + this.host.viewport.offset)
    const line = lines.length > 0 ? lines[Math.max(0, Math.min(row, lines.length - 1))]! : this.host.store.firstActive
    return {
      line,
      col: Math.max(0, Math.min(Math.floor(x / cellWidth), Math.max(0, this.host.columns() - 1))),
    }
  }

  private beyond(point: Point): number {
    const { cellHeight } = this.host.painter.metrics
    const { y } = this.pointIn(point)
    const bottom = this.host.rows() * cellHeight
    return y < 0 ? y / cellHeight : y > bottom ? (y - bottom) / cellHeight : 0
  }

  private pointerInput(event: MouseEvent, action: PointerInput['action'], button: number): PointerInput {
    const { cellWidth, cellHeight } = this.host.painter.metrics
    const { x, y } = this.pointIn(event)
    const maxX = this.host.columns() * cellWidth
    const maxY = this.host.rows() * cellHeight
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
    this.host.source.sendPointer(input)
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
    if (event.target === this.host.thumb) return
    if (this.pressed) {
      event.preventDefault()
      return
    }
    this.refreshBox()
    this.host.focus()
    event.preventDefault()
    const at = this.positionOf(event)
    const validButton = event.button >= 0 && event.button <= 2
    const reporting =
      validButton && this.host.frame()?.modes.mouse === true && !event.shiftKey && this.host.viewport.following
    this.pressed = { pointerId: event.pointerId, button: event.button, reporting, at, moved: false, block: event.altKey }
    this.host.element.setPointerCapture?.(event.pointerId)
    if (reporting) {
      const input = this.pointerInput(event, 'press', event.button)
      this.reported = {
        col: Math.floor(input.xPx / input.cellPx.width),
        row: Math.floor(input.yPx / input.cellPx.height),
        button: event.button,
      }
      this.host.source.sendPointer(input)
      return
    }
    if (event.button !== 0) return
    if (event.shiftKey && this.host.selection.span) {
      this.host.selection.extendTo(at)
      return
    }
    const clicks = this.clicks(event)
    const granularity: Granularity = clicks === 3 ? 'line' : clicks === 2 ? 'word' : 'cell'
    if (granularity === 'cell') this.host.selection.clear()
    else this.host.selection.begin(at, granularity)
  }

  private readonly pointerMove = (event: PointerEvent) => {
    const pressed = this.pressed
    if (pressed && event.pointerId !== pressed.pointerId) return
    if (pressed?.reporting) {
      this.reportMove(event, pressed.button)
      return
    }
    if (!pressed && this.host.frame()?.modes.mouse && !event.shiftKey && this.host.viewport.following) {
      this.reportMove(event, 255)
    }
    const at = this.positionOf(event)
    if (pressed?.button === 0) {
      if (!pressed.moved && (at.line !== pressed.at.line || at.col !== pressed.at.col)) {
        pressed.moved = true
        if (!this.host.selection.span) {
          this.host.selection.begin(pressed.at, 'cell', pressed.block ? 'block' : 'linear')
        }
      }
      if (pressed.moved) {
        this.host.selection.extendTo(at)
        this.dragScrollTo(event)
      }
      return
    }
    this.hover(at)
  }

  private readonly pointerUp = (event: PointerEvent) => {
    const pressed = this.released(event)
    if (!pressed || pressed.reporting) return
    if (pressed.button === 0 && this.host.copyOnSelect() && this.host.selection.span) void this.host.copy()
    const opening = this.host.mac ? event.metaKey : event.ctrlKey
    if (pressed.button === 0 && !pressed.moved && opening) {
      const link = this.linkOn(this.positionOf(event))
      if (link) this.host.onLink(link, event)
    }
  }

  private readonly pointerLost = (event: PointerEvent) => {
    this.released(event)
  }

  private released(event: PointerEvent): Pressed | null {
    const pressed = this.pressed
    if (!pressed) return null
    if (event.pointerId !== pressed.pointerId) return null
    if (event.type === 'pointerup' && event.button !== pressed.button) return null
    this.pressed = null
    this.stopDragScroll()
    if (this.host.element.hasPointerCapture?.(event.pointerId)) {
      this.host.element.releasePointerCapture(event.pointerId)
    }
    if (pressed.reporting) {
      this.reported = null
      this.host.source.sendPointer(this.pointerInput(event, 'release', pressed.button))
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
    const top = this.host.viewport.top
    this.host.scrollBy((this.beyond(drag.point) * elapsed * DRAG_LINES_PER_SECOND) / 1000)
    this.host.selection.extendTo(this.positionOf(drag.point))
    if (elapsed > 0 && this.host.viewport.top === top) {
      this.dragScroll = null
      return
    }
    drag.frame = requestAnimationFrame(this.dragScrollStep)
  }

  private readonly pointerLeave = () => this.hover(null)

  private readonly contextMenu = (event: MouseEvent) => {
    if (this.host.frame()?.modes.mouse && !event.shiftKey && this.host.viewport.following) event.preventDefault()
  }

  private linkOn(at: CellPos): Link | null {
    const row = this.host.store.row(at.line)
    return row ? linkAt(row, at.col, this.host.detectors, (line) => this.host.store.row(line)) : null
  }

  private hover(at: CellPos | null): void {
    const over = at ? { line: at.line, col: at.col, row: this.host.store.row(at.line) } : null
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
    const row = at ? this.host.store.row(at.line) : undefined
    this.hovered =
      link && at && row
        ? { line: at.line, link, runs: linkRuns(link, row, at.col, (line) => this.host.store.row(line)) }
        : null
    this.hoverVer += 1
    this.host.element.style.cursor = link ? 'pointer' : 'text'
    this.host.element.title = link && this.previews(link) ? sanitizeTitle(link.target) : ''
    this.host.schedule()
  }

  private previews(link: Link): boolean {
    const mode = this.host.linkPreview()
    return mode === 'all' || (mode === 'osc8' && link.kind === 'declared')
  }

  private thumbAt(): { height: number; top: number; travel: number } | null {
    const { total, position, shown } = this.host.viewport.extent
    const h = this.host.height()
    if (total <= shown || h <= 0) return null
    const height = Math.min(h, Math.max(THUMB_MIN, (shown / total) * h))
    const travel = h - height
    return { height, top: (position / (total - shown)) * travel, travel }
  }

  private readonly thumbDown = (event: PointerEvent) => {
    if (event.button !== 0 || this.thumbRelease !== null) return
    event.preventDefault()
    event.stopPropagation()
    const pointerId = event.pointerId
    try {
      this.host.thumb.setPointerCapture?.(pointerId)
    } catch {
      // Synthetic pointer events in unit tests do not register active browser pointer IDs.
    }
    const start = { y: event.clientY, position: this.host.viewport.extent.position }
    const move = (moved: PointerEvent) => {
      if (moved.pointerId !== pointerId) return
      const { total, shown } = this.host.viewport.extent
      const travel = this.thumbAt()?.travel ?? 0
      if (travel > 0) this.host.scrollToPosition(start.position + ((moved.clientY - start.y) / travel) * (total - shown))
    }
    const up = (ended?: PointerEvent) => {
      if (ended && ended.pointerId !== pointerId) return
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      window.removeEventListener('pointercancel', up)
      this.host.thumb.removeEventListener('lostpointercapture', up)
      this.thumbRelease = null
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
    window.addEventListener('pointercancel', up)
    this.host.thumb.addEventListener('lostpointercapture', up)
    this.thumbRelease = () => up()
  }
}
