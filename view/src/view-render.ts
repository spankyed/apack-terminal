import { Timer, type Clock } from './clock.ts'
import { drawRow, rowPlainText, type RowDrawing, type SelectedCells, type Theme } from './draw.ts'
import { type ImageCache } from './images.ts'
import { type Input } from './input.ts'
import { type Painter, type Scene } from './painter.ts'
import { PAGE_LINES, type RowStore } from './row-store.ts'
import { type Selection } from './selection.ts'
import {
  PackedStyleFlag,
  type CursorShape,
  type FrameUpdate,
  type ImagePlacement,
  type PackedRow,
  type TerminalSource,
} from './source.ts'
import { type Viewport } from './viewport.ts'
import { type CommandFoldController } from './view-commands.ts'
import { type PointerController } from './view-pointer.ts'
import { type SearchController } from './view-search.ts'
import { MIRROR_DEBOUNCE_MS, blinksIn, cellAtColumn } from './view-types.ts'

interface KeptDrawing {
  readonly row: PackedRow
  readonly selVer: number
  readonly matchVer: number
  readonly hoverVer: number
  readonly blinkedOff: boolean
  readonly drawing: RowDrawing
}

export interface RenderHost {
  readonly element: HTMLDivElement
  readonly mirror: HTMLDivElement
  readonly painter: Painter
  readonly imageCache: ImageCache
  readonly input: Input
  readonly source: TerminalSource
  readonly store: RowStore
  readonly viewport: Viewport
  readonly selection: Selection
  readonly search: SearchController
  readonly commands: CommandFoldController
  readonly pointer: PointerController
  readonly gutter: number
  theme(): Theme
  isDisposed(): boolean
  isEnded(): boolean
  isListening(): boolean
  isFocused(): boolean
  isCursorShown(): boolean
  isTextShown(): boolean
  cursorInactive(): CursorShape
  blinkText(inView: boolean): void
  schedule(): void
}

/**
 * Owns the dirty-checked `RowDrawing` cache, paged history prefetching, Kitty image texture
 * demand/compositing, lazy DOM mirror coalescing, and per-frame `Scene` assembly.
 */
export class ViewRenderer {
  private readonly drawings = new Map<number, KeptDrawing>()
  private readonly asking = new Set<number>()
  private readonly mirrorTimer: Timer
  private mirrorDirty = false
  private lastLayoutLines: number[] = []
  private readonly mirrored: Array<PackedRow | null | undefined> = []
  private drawnSeq = -1
  private placements: readonly ImagePlacement[] = []
  private imagesGenSeen = -1
  private readonly schedulePaint: () => void

  constructor(private readonly host: RenderHost, clock: Clock) {
    this.mirrorTimer = new Timer(clock)
    this.schedulePaint = () => this.host.schedule()
  }

  clearDrawings(): void {
    this.drawings.clear()
  }

  deleteDrawing(line: number): void {
    this.drawings.delete(line)
  }

  cancelMirrorTimer(): void {
    this.mirrorTimer.cancel()
  }

  resetForEpoch(): void {
    this.drawings.clear()
    this.asking.clear()
    this.placements = []
    this.imagesGenSeen = -1
  }

  syncFrameImages(frame: FrameUpdate): void {
    if (frame.imagesGen === this.imagesGenSeen) return
    const targetGen = (this.imagesGenSeen = frame.imagesGen)
    const epoch = frame.epoch
    void this.host.source.images().then((reply) => {
      if (this.host.isDisposed() || epoch !== this.host.store.epoch || targetGen !== this.imagesGenSeen) return
      this.placements = reply.placements
      this.wantPicturesInView()
      this.host.schedule()
    }, () => {
      if (epoch === this.host.store.epoch && this.imagesGenSeen === targetGen) this.imagesGenSeen = -1
    })
  }

  wantPicturesInView(): void {
    const { first, lines } = this.host.commands.layout()
    if (lines.length === 0) return
    this.wantPictures(first, lines[lines.length - 1]! - first + 1)
  }

  paint(frame: FrameUpdate): void {
    const { first, lines } = this.host.commands.layout()
    const span = lines.length > 0 ? lines[lines.length - 1]! - first + 1 : 0

    let blinking = false
    for (const line of lines) {
      const row = this.host.store.row(line)
      if (row !== undefined && blinksIn(row)) {
        blinking = true
        break
      }
    }
    this.host.blinkText(blinking)

    const theme = this.host.theme()
    const rows: Array<RowDrawing | null> = lines.map((line) => {
      const fold = this.host.commands.foldStartingAt(line)
      return fold ? this.host.commands.foldDrawing(fold, theme) : this.drawingOf(line)
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
      theme,
      epoch: this.host.store.epoch,
      rows,
      offset: this.host.viewport.offset,
      scrollPosition: this.host.viewport.rowsBetween(0, this.host.viewport.top),
      cursor,
      marks: this.host.commands.marksIn(first, lines, theme),
      images: this.imagesIn(first, span, lines),
    }
    this.host.painter.paint(scene)
    this.afterPaint(frame, first, lines)
  }

  /** Synchronously flushes any pending lazy DOM mirror updates (useful for a11y/tests). */
  flushMirror(): void {
    this.mirrorTimer.cancel()
    this.mirrorDirty = false
    const lines = this.lastLayoutLines
    const count = lines.length
    const mirror = this.host.mirror
    while (mirror.childElementCount < count) mirror.appendChild(document.createElement('div'))
    while (mirror.childElementCount > count) mirror.lastElementChild!.remove()
    this.mirrored.length = count
    for (let index = 0; index < count; index += 1) {
      const line = lines[index]!
      const fold = this.host.commands.foldStartingAt(line)
      const row = fold ? this.host.commands.foldRow(fold) : (this.host.store.row(line) ?? null)
      if (this.mirrored[index] === row) continue
      this.mirrored[index] = row
      mirror.children[index]!.textContent = row ? rowPlainText(row) : ''
    }
  }

  private wantPictures(first: number, count: number): void {
    if (this.host.isEnded() || !this.host.isListening() || count <= 0) return
    for (const placement of this.placements) {
      const pRows = Math.max(1, placement.cellSpan[1])
      if (placement.line + pRows <= first - count || placement.line >= first + count * 2) continue
      void this.host.imageCache.ensure(this.host.source, placement.blob, this.schedulePaint)
    }
    for (let line = first - count; line < first + count * 2; line += 1) {
      const pieces = this.host.store.row(line)?.pieces
      if (!pieces) continue
      for (const piece of pieces) {
        void this.host.imageCache.ensure(this.host.source, piece.blob, this.schedulePaint)
      }
    }
  }

  private imagesIn(first: number, span: number, lines: number[]): NonNullable<Scene['images']> {
    const { cellWidth, cellHeight } = this.host.painter.metrics
    const images: Array<NonNullable<Scene['images']>[number] & { z: number }> = []

    // 1. Placeholder pieces on visible rows (drawn under text at z = -1)
    for (const line of lines) {
      const row = this.host.store.row(line)
      if (!row?.pieces || this.host.commands.foldStartingAt(line)) continue
      const top = this.host.viewport.rowsBetween(first, line) * cellHeight
      for (const piece of row.pieces) {
        const decoded = this.host.imageCache.get(piece.blob)
        if (!decoded) continue
        if (!Number.isFinite(piece.colStart) || piece.colStart < 0) continue
        const colLen = Number.isFinite(piece.colLen) && piece.colLen > 0 ? piece.colLen : 0
        const [rawSx, rawSy, rawSw, rawSh] = piece.srcRect
        const sx = Number.isFinite(rawSx) ? Math.min(Math.max(0, rawSx), decoded.width) : 0
        const sy = Number.isFinite(rawSy) ? Math.min(Math.max(0, rawSy), decoded.height) : 0
        const availW = decoded.width - sx
        const availH = decoded.height - sy
        const sw = Number.isFinite(rawSw) && rawSw > 0 ? Math.min(rawSw, availW) : availW
        const sh = Number.isFinite(rawSh) && rawSh > 0 ? Math.min(rawSh, availH) : availH
        if (!(sw > 0) || !(sh > 0)) continue
        const [rawDstW, rawDstH] = piece.dstPx
        const maxDstW = colLen > 0 ? colLen * cellWidth : 0
        const dstW = Number.isFinite(rawDstW) && rawDstW > 0 ? rawDstW : 0
        const dstH = Number.isFinite(rawDstH) && rawDstH > 0 ? rawDstH : 0
        const width = maxDstW > 0 ? (dstW > 0 ? Math.min(dstW, maxDstW) : maxDstW) : dstW
        const height = dstH > 0 ? Math.min(dstH, cellHeight) : cellHeight
        if (!(width > 0) || !(height > 0)) continue
        images.push({
          key: decoded.key,
          picture: decoded.source,
          x: piece.colStart * cellWidth,
          y: top,
          width,
          height,
          source: { x: sx, y: sy, width: sw, height: sh },
          above: false,
          z: -1,
        })
      }
    }

    // 2. Anchored placements from `source.images()`
    for (const placement of this.placements) {
      const [spanCols, spanRows] = placement.cellSpan
      const pRows = Math.max(1, spanRows)
      if (placement.line + pRows <= first || placement.line >= first + span) continue
      if (this.host.commands.hiddenIn(placement.line)) continue
      const decoded = this.host.imageCache.get(placement.blob)
      if (!decoded) continue
      const [rawSx, rawSy, rawSw, rawSh] = placement.sourceRect
      const sx = Number.isFinite(rawSx) ? Math.min(Math.max(0, rawSx), decoded.width) : 0
      const sy = Number.isFinite(rawSy) ? Math.min(Math.max(0, rawSy), decoded.height) : 0
      const availW = decoded.width - sx
      const availH = decoded.height - sy
      const sw = Number.isFinite(rawSw) && rawSw > 0 ? Math.min(rawSw, availW) : availW
      const sh = Number.isFinite(rawSh) && rawSh > 0 ? Math.min(rawSh, availH) : availH
      if (!(sw > 0) || !(sh > 0)) continue
      const [rawOffX, rawOffY] = placement.cellOffset
      const maxOffX = Math.max(0, cellWidth - 1)
      const maxOffY = Math.max(0, cellHeight - 1)
      const offX = Number.isFinite(rawOffX) ? Math.min(Math.max(0, rawOffX), maxOffX) : 0
      const offY = Number.isFinite(rawOffY) ? Math.min(Math.max(0, rawOffY), maxOffY) : 0
      const [rawPxW, rawPxH] = placement.pixelSize
      const pxW = Number.isFinite(rawPxW) && rawPxW > 0 ? rawPxW : 0
      const pxH = Number.isFinite(rawPxH) && rawPxH > 0 ? rawPxH : 0
      const maxW = spanCols > 0 ? Math.max(0, spanCols * cellWidth - offX) : pxW
      const maxH = spanRows > 0 ? Math.max(0, spanRows * cellHeight - offY) : pxH
      const width = spanCols > 0 ? (pxW > 0 ? Math.min(pxW, maxW) : maxW) : pxW
      const height = spanRows > 0 ? (pxH > 0 ? Math.min(pxH, maxH) : maxH) : pxH
      if (!(width > 0) || !(height > 0)) continue
      images.push({
        key: decoded.key,
        picture: decoded.source,
        x: placement.col * cellWidth + offX,
        y: this.host.viewport.rowsBetween(first, placement.line) * cellHeight + offY,
        width,
        height,
        source: { x: sx, y: sy, width: sw, height: sh },
        above: placement.zIndex >= 0,
        z: placement.zIndex,
      })
    }

    return images.sort((a, b) => a.z - b.z)
  }

  private afterPaint(frame: FrameUpdate, first: number, lines: number[]): void {
    this.lastLayoutLines = lines
    // Populate DOM mirror synchronously on initial empty paint, then coalesce at 200 ms via Clock
    if (this.host.mirror.childElementCount === 0 && lines.length > 0) {
      this.flushMirror()
    } else {
      this.mirrorDirty = true
      if (!this.mirrorTimer.active) {
        this.mirrorTimer.arm(MIRROR_DEBOUNCE_MS, () => {
          if (this.mirrorDirty && !this.host.isDisposed()) this.flushMirror()
        })
      }
    }

    this.host.pointer.updateThumb()

    const { cellWidth, cellHeight } = this.host.painter.metrics
    const cursorLine = frame.firstActiveLine + frame.cursor.row
    const row = this.host.viewport.rowsBetween(first, this.host.commands.hiddenIn(cursorLine)?.first ?? cursorLine)
    this.host.input.moveTo(
      this.host.gutter + frame.cursor.col * cellWidth,
      Math.max(0, row - this.host.viewport.offset) * cellHeight,
      cellHeight,
    )

    if (frame.seq !== this.drawnSeq) {
      this.drawnSeq = frame.seq
      this.host.element.dataset.terminalDrawn = String(frame.seq)
    }
  }

  private drawingOf(line: number): RowDrawing | null {
    const row = this.host.store.row(line)
    if (!row) return null
    const selVer = this.host.selection.version
    const matchVer = this.host.search.version
    const hoverVer = this.host.pointer.hoverVersion
    const blinkedOff = !this.host.isTextShown() && blinksIn(row)

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

    const theme = this.host.theme()
    const selected: SelectedCells | undefined = this.host.selection.on(line)
    const hovered = this.host.pointer.hoveredRunsOn(line)
    const matched = this.host.search.hasMatches ? this.host.search.matchesOn(line) : []
    const { matchBackground = 0x623315, currentMatchBackground = 0x9e6a03 } = theme
    const found = matched.map((m) => ({
      from: m.from,
      to: m.to,
      color: m.current ? currentMatchBackground : matchBackground,
    }))
    let drawing = drawRow(row, theme, selected, blinkedOff, found)
    if (hovered.length > 0) {
      drawing = {
        ...drawing,
        lines: [
          ...drawing.lines,
          ...hovered.map((run) => ({
            kind: 'single' as const,
            column: run.from,
            cells: run.to - run.from,
            color: theme.foreground,
          })),
        ],
      }
    }
    this.drawings.set(line, { row, selVer, matchVer, hoverVer, blinkedOff, drawing })
    return drawing
  }

  private ask(first: number, count: number): void {
    if (this.host.isEnded()) return
    for (const span of this.host.store.missing(first - count, count * 3)) {
      for (let page = Math.floor(span.from / PAGE_LINES); page * PAGE_LINES < span.from + span.count; page += 1) {
        this.askPage(page)
      }
    }
  }

  private askPage(page: number): void {
    if (this.asking.has(page) || this.host.store.epoch < 0) return
    const runs = this.host.store.missing(page * PAGE_LINES, PAGE_LINES)
    const last = runs.at(-1)
    if (!last) return
    const from = runs[0]!.from
    const to = last.from + last.count
    this.asking.add(page)
    const epoch = this.host.store.epoch
    const atImagesChanged = this.host.store.imagesChanged
    void this.host.source.fetchRows(epoch, from, to - from).then((fetched) => {
      if (this.host.isDisposed() || epoch !== this.host.store.epoch) return
      this.asking.delete(page)
      const absorbed = this.host.store.absorb(epoch, from, to - from, fetched, this.host.viewport.top, atImagesChanged)
      for (const line of absorbed) this.drawings.delete(line)
      if (absorbed.length > 0 || atImagesChanged !== this.host.store.imagesChanged) this.host.schedule()
    }, () => {
      if (epoch === this.host.store.epoch) this.asking.delete(page)
    })
  }

  private cursorIn(frame: FrameUpdate, lines: number[]): Scene['cursor'] {
    const cursor = frame.cursor
    if (cursor.hidden || !this.host.isCursorShown()) return undefined
    const line = frame.firstActiveLine + cursor.row
    const index = lines.indexOf(line)
    if (index === -1) return undefined
    const fold = this.host.commands.foldStartingAt(line)
    const row = fold ? this.host.commands.foldRow(fold) : this.host.store.row(line)
    const cell = row ? cellAtColumn(row, cursor.col) : null
    const visible =
      cell !== null
      && (cell.flags & PackedStyleFlag.INVISIBLE) === 0
      && !(!this.host.isTextShown() && (cell.flags & PackedStyleFlag.BLINK) !== 0)
    return {
      column: cell ? cell.col : cursor.col,
      row: index,
      cells: cell ? cell.width : 1,
      shape: this.host.isFocused() ? cursor.shape : this.host.cursorInactive(),
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
