import type { GlyphRun, LineKind, RowDrawing } from './draw.ts'
import { css, fontOf, measure, type Font, type Metrics, type Painter, type Scene } from './painter.ts'

/** How many CSS colour strings and measured fallback glyphs are cached before rolling over. */
const REMEMBERED = 4096

/** Maximum allowed pixel drift between `measureText('MMMM').width` and `4 * cellWidth` for ASCII run batching. */
const ADVANCE_TOLERANCE_PX = 0.05

/**
 * Draws a scene on a 2D canvas.
 *
 * v2 optimizations:
 * 1. **Font-advance-probed ASCII run batching:** probes `measureText('MMMM')` & `measureText('iiii')`
 *    once per face on font/DPR change; when the monospace advance matches `4 * cellWidth` within
 *    `0.05 px`, batchable printable ASCII runs draw with a single `fillText` call (automatically
 *    falling back to per-cell `fillText` for non-uniform/proportional fonts).
 * 2. **Zero whole-row `save/clip/restore`:** only individual overhanging glyphs (`isWider`) are clipped.
 * 3. **Scroll-blit fast path (`drawImage(canvas, ...)`):** when `Scene.scrollPosition` shifts by an
 *    exact device-pixel vertical delta smaller than the viewport (integer or exact-pixel fractional
 *    scroll), copies the overlapping pixel band in one blit and repaints only the newly exposed edge
 *    rows and any rows whose cursor or `RowDrawing` identity changed.
 * 4. **Cache preservation across grid resizes:** `colors` is never cleared on resize, and `wider` +
 *    advance probes are preserved whenever font, pixel ratio, and measured cell metrics are unchanged.
 */
export class CanvasPainter implements Painter {
  readonly canvas: HTMLCanvasElement
  metrics: Metrics
  private readonly context: CanvasRenderingContext2D
  private columns = 0
  private rows = 0
  private gutter = 0
  /** The CSS font shorthand of each face: bold is `1`, italic `2`. */
  private faces: readonly string[]
  /** Whether each face's ASCII advance matches `cellWidth` closely enough for batched `fillText`. */
  private batchableFace: boolean[] = [false, false, false, false]
  /** The scene the canvas currently shows, or `null` when cleared/unpainted. */
  private shown: Scene | null = null
  /** Last applied context state to suppress redundant DOM property writes. */
  private face: string | null = null
  private fill: string | null = null
  private readonly colors = new Map<number, string>()
  /** Whether a non-ASCII glyph in a given face is wider than its allocated cells. */
  private readonly wider = new Map<string, boolean>()

  constructor(
    private font: Font,
    canvas = document.createElement('canvas'),
    private pixelRatio = globalThis.devicePixelRatio || 1,
  ) {
    const context = canvas.getContext('2d')
    if (!context) throw new Error('This canvas has no 2D context to draw a terminal on')
    this.pixelRatio = Number.isFinite(pixelRatio) && pixelRatio > 0 ? pixelRatio : 1
    this.canvas = canvas
    this.context = context
    this.faces = facesOf(font)
    this.metrics = measure(context, font, this.pixelRatio)
    this.probeFaces()
    canvas.addEventListener('contextrestored', this.cleared)
  }

  private readonly cleared = () => {
    this.shown = null
    this.repaint?.()
  }

  private repaint: (() => void) | null = null

  whenCleared(repaint: () => void): void {
    this.repaint = repaint
  }

  /** Exposes whether a given face (0..3) passed the monospace advance probe (useful in tests). */
  canBatchFace(bold = false, italic = false): boolean {
    const idx = (bold ? 1 : 0) | (italic ? 2 : 0)
    return this.batchableFace[idx] ?? false
  }

  resize(columns: number, rows: number, pixelRatio: number, gutter = 0): void {
    const ratio = Number.isFinite(pixelRatio) && pixelRatio > 0 ? pixelRatio : 1
    const ratioChanged = ratio !== this.pixelRatio
    const prevWidth = this.metrics.cellWidth
    const prevHeight = this.metrics.cellHeight

    this.columns = columns
    this.rows = rows
    this.pixelRatio = ratio
    this.gutter = gutter
    this.metrics = measure(this.context, this.font, ratio)

    const metricsChanged =
      ratioChanged
      || this.metrics.cellWidth !== prevWidth
      || this.metrics.cellHeight !== prevHeight

    const width = columns * this.metrics.cellWidth + gutter
    const height = rows * this.metrics.cellHeight
    this.canvas.width = Math.round(width * ratio)
    this.canvas.height = Math.round(height * ratio)
    this.canvas.style.width = `${width}px`
    this.canvas.style.height = `${height}px`
    this.shown = null
    if (metricsChanged) {
      this.wider.clear()
      this.probeFaces()
    }
  }

  setFont(font: Font): void {
    this.font = font
    this.faces = facesOf(font)
    this.wider.clear()
    this.metrics = measure(this.context, font, this.pixelRatio)
    this.probeFaces()
    this.resize(this.columns, this.rows, this.pixelRatio, this.gutter)
  }

  paint(scene: Scene): void {
    const { context } = this
    context.setTransform(this.pixelRatio, 0, 0, this.pixelRatio, 0, 0)
    context.textBaseline = 'alphabetic'
    this.face = null
    this.fill = null

    if (this.tryScrollBlit(scene)) {
      this.shown = scene
      return
    }

    const bands = this.changed(scene)
    if (bands === null) {
      this.draw(scene, 0, scene.rows.length)
    } else {
      for (const [first, end] of bands) {
        this.paintBand(scene, first, end)
      }
    }
    this.shown = scene
  }

  dispose(): void {
    this.canvas.removeEventListener('contextrestored', this.cleared)
    this.shown = null
    this.repaint = null
    this.face = null
    this.fill = null
    this.colors.clear()
    this.wider.clear()
    this.canvas.width = 0
    this.canvas.height = 0
    this.canvas.remove()
  }

  /**
   * Probes each font face once after font/DPR change: `measureText('MMMM')` and `measureText('iiii')`
   * must both equal `4 * cellWidth` within `0.05 px` before multi-char ASCII `fillText` batching is enabled.
   */
  private probeFaces(): void {
    const target = this.metrics.cellWidth * 4
    const next = [false, false, false, false]
    if (!(target > 0)) {
      this.batchableFace = next
      return
    }
    for (let i = 0; i < 4; i += 1) {
      this.context.font = this.faces[i]!
      const m4 = this.context.measureText('MMMM').width
      const i4 = this.context.measureText('iiii').width
      next[i] =
        Number.isFinite(m4)
        && Number.isFinite(i4)
        && m4 > 0
        && Math.abs(m4 - target) <= ADVANCE_TOLERANCE_PX
        && Math.abs(i4 - target) <= ADVANCE_TOLERANCE_PX
    }
    this.face = null
    this.batchableFace = next
  }

  /**
   * Attempts an O(1) vertical pixel-band scroll blit via `drawImage(canvas, ...)` when the viewport
   * shifted uniformly without images/marks interfering. Returns `true` if handled.
   */
  private tryScrollBlit(scene: Scene): boolean {
    const shown = this.shown
    if (!shown) return false
    if (shown.theme !== scene.theme) return false
    if (shown.epoch !== scene.epoch) return false
    if (scene.scrollPosition === undefined || shown.scrollPosition === undefined) return false
    if ((scene.images?.length ?? 0) > 0 || (shown.images?.length ?? 0) > 0) return false
    if ((scene.marks?.length ?? 0) > 0 || (shown.marks?.length ?? 0) > 0) return false
    if (this.rows <= 2 || this.canvas.width <= 0 || this.canvas.height <= 0) return false

    const deltaRows = scene.scrollPosition - shown.scrollPosition
    if (deltaRows === 0 || Math.abs(deltaRows) >= this.rows - 1) return false

    const { cellHeight } = this.metrics
    const rawDyDev = deltaRows * cellHeight * this.pixelRatio
    const dyDev = Math.round(rawDyDev)
    if (dyDev === 0 || Math.abs(dyDev - rawDyDev) > 0.05) return false

    const absDy = Math.abs(dyDev)
    const copyH = this.canvas.height - absDy
    if (copyH <= 0) return false

    // When `deltaRows > 0` (scrolling down / new output at bottom), content moves UP (`dstY = 0, srcY = absDy`).
    // When `deltaRows < 0` (scrolling up / into scrollback), content moves DOWN (`dstY = absDy, srcY = 0`).
    const srcY = dyDev > 0 ? absDy : 0
    const dstY = dyDev > 0 ? 0 : absDy

    const { context } = this
    context.save()
    context.setTransform(1, 0, 0, 1, 0, 0)
    context.drawImage(this.canvas, 0, srcY, this.canvas.width, copyH, 0, dstY, this.canvas.width, copyH)
    context.restore()
    context.setTransform(this.pixelRatio, 0, 0, this.pixelRatio, 0, 0)
    context.textBaseline = 'alphabetic'
    this.face = null
    this.fill = null

    const count = scene.rows.length
    const dirty = new Uint8Array(count)

    // Mark newly exposed rows at the entering edge (with 1-row neighbor margin for vertical glyph overhang)
    const exposed = Math.ceil(Math.abs(deltaRows)) + 1
    if (deltaRows > 0) {
      const start = Math.max(0, count - exposed)
      for (let i = start; i < count; i += 1) dirty[i] = 1
    } else {
      const stop = Math.min(count, exposed)
      for (let i = 0; i < stop; i += 1) dirty[i] = 1
    }

    // Detect any row in the blitted band whose drawing identity changed relative to the shifted
    // `shown.rows` (for BOTH integer and fractional smooth scrolls so newly fetched history rows,
    // search highlights, selections, or blinking text never leave stale blitted pixels!).
    const rowShift = Math.floor(scene.scrollPosition) - Math.floor(shown.scrollPosition)
    for (let i = 0; i < count; i += 1) {
      const oldIdx = i + rowShift
      if (oldIdx < 0 || oldIdx >= shown.rows.length || scene.rows[i] !== shown.rows[oldIdx]) {
        dirty[i] = 1
      }
    }

    const intShift = Math.round(deltaRows)
    const isIntegerShift = Math.abs(deltaRows - intShift) < 1e-6 && scene.offset === 0 && shown.offset === 0
    if (isIntegerShift) {
      if (shown.cursor) {
        const landed = shown.cursor.row - intShift
        if (landed >= 0 && landed < count) dirty[landed] = 1
      }
      if (scene.cursor && scene.cursor.row >= 0 && scene.cursor.row < count) {
        dirty[scene.cursor.row] = 1
      }
    } else {
      if (shown.cursor) {
        const landed = Math.floor(shown.cursor.row - deltaRows)
        for (let r = landed - 1; r <= landed + 1; r += 1) {
          if (r >= 0 && r < count) dirty[r] = 1
        }
      }
      if (scene.cursor) {
        for (let r = scene.cursor.row - 1; r <= scene.cursor.row + 1; r += 1) {
          if (r >= 0 && r < count) dirty[r] = 1
        }
      }
    }

    // Expand dirty rows by +-1 for vertical glyph overhang consistency with `changed()`
    const expanded = new Uint8Array(count)
    for (let i = 0; i < count; i += 1) {
      if (dirty[i] || dirty[i - 1] || dirty[i + 1]) expanded[i] = 1
    }
    // Also clean the opposite clip-edge row itself (without expanding into its interior neighbor)
    // so any ascender/descender overhang copied from the scrolled-off row is cleanly cleared.
    if (count > 0) {
      if (deltaRows > 0) expanded[0] = 1
      else expanded[count - 1] = 1
    }

    const bands = collectBands(expanded)
    for (const [first, end] of bands) {
      this.paintBand(scene, first, end)
    }
    return true
  }

  private paintBand(scene: Scene, first: number, end: number): void {
    const { context, metrics } = this
    const bandTop = Math.max(0, (first - scene.offset) * metrics.cellHeight)
    const bandBottom = Math.min(this.rows * metrics.cellHeight, (end - scene.offset) * metrics.cellHeight)
    if (bandBottom <= bandTop) return
    context.save()
    context.beginPath()
    context.rect(0, bandTop, this.columns * metrics.cellWidth + this.gutter, bandBottom - bandTop)
    context.clip()
    this.draw(scene, first - 1, end + 1, [bandTop, bandBottom])
    context.restore()
    this.face = null
    this.fill = null
  }

  private changed(scene: Scene): Array<[number, number]> | null {
    const shown = this.shown
    if (!shown || shown.theme !== scene.theme || scene.offset !== 0 || shown.offset !== 0) return null
    if (shown.rows.length !== scene.rows.length) return null
    if (!same(shown.marks, scene.marks, sameMark) || !same(shown.images, scene.images, sameImage)) return null

    const count = scene.rows.length
    const differs = new Uint8Array(count)
    for (let index = 0; index < count; index += 1) {
      if (scene.rows[index] !== shown.rows[index]) differs[index] = 1
    }
    if (!sameCursor(shown.cursor, scene.cursor)) {
      for (const cursor of [shown.cursor, scene.cursor]) {
        if (cursor && cursor.row >= 0 && cursor.row < count) differs[cursor.row] = 1
      }
    }

    const expanded = new Uint8Array(count)
    let drawn = 0
    for (let index = 0; index < count; index += 1) {
      if (!differs[index] && !differs[index - 1] && !differs[index + 1]) continue
      expanded[index] = 1
      drawn += 1
    }
    return drawn * 2 > count ? null : collectBands(expanded)
  }

  private draw(scene: Scene, first: number, end: number, clipBand?: readonly [number, number]): void {
    const { context, metrics } = this
    const { cellWidth, cellHeight } = metrics
    const from = Math.max(0, first)
    const to = Math.min(scene.rows.length, end)
    this.fillStyle(this.color(scene.theme.background))
    context.fillRect(0, 0, this.columns * cellWidth + this.gutter, this.rows * cellHeight)

    if (this.gutter > 0) {
      for (const mark of scene.marks ?? []) {
        if (mark.row < from || mark.row >= to) continue
        this.fillStyle(this.color(mark.color))
        context.fillRect(1, (mark.row - scene.offset) * cellHeight + 2, Math.max(2, this.gutter - 3), cellHeight - 4)
      }
    }

    context.translate(this.gutter, 0)
    for (let index = from; index < to; index += 1) {
      const row = scene.rows[index]
      if (row) this.backgrounds(row, (index - scene.offset) * cellHeight)
    }
    this.images(scene, false, clipBand)
    for (let index = from; index < to; index += 1) {
      const row = scene.rows[index]
      if (row) this.foreground(row, (index - scene.offset) * cellHeight)
    }
    this.images(scene, true, clipBand)
    if (scene.cursor && scene.cursor.row >= from && scene.cursor.row < to) this.cursor(scene)
    context.translate(-this.gutter, 0)
  }

  private fontFace(bold: boolean, italic: boolean): number {
    const face = (bold ? 1 : 0) | (italic ? 2 : 0)
    const font = this.faces[face]!
    if (font !== this.face) {
      this.context.font = font
      this.face = font
    }
    return face
  }

  private fillStyle(style: string): void {
    if (style === this.fill) return
    this.context.fillStyle = style
    this.fill = style
  }

  private color(color: number, alpha = 1): string {
    const key = color + Math.round(alpha * 255) * 0x1000000
    let written = this.colors.get(key)
    if (written === undefined) {
      if (this.colors.size >= REMEMBERED) this.colors.clear()
      written = css(color, alpha)
      this.colors.set(key, written)
    }
    return written
  }

  private isWider(text: string, cells: number, face: number): boolean {
    if (text.length === 1 && text.charCodeAt(0) < 0x80) return false
    const key = `${face}:${cells}:${text}`
    let wider = this.wider.get(key)
    if (wider === undefined) {
      if (this.wider.size >= REMEMBERED) this.wider.clear()
      wider = this.context.measureText(text).width > cells * this.metrics.cellWidth + 0.5
      this.wider.set(key, wider)
    }
    return wider
  }

  private images(scene: Scene, above: boolean, clipBand?: readonly [number, number]): void {
    for (const image of scene.images ?? []) {
      if (image.above !== above || image.source.width <= 0 || image.source.height <= 0) continue
      const y = image.y - scene.offset * this.metrics.cellHeight
      if (clipBand && (y + image.height <= clipBand[0] || y >= clipBand[1])) continue
      this.context.drawImage(
        image.picture,
        image.source.x, image.source.y, image.source.width, image.source.height,
        image.x, y, image.width, image.height,
      )
    }
  }

  private backgrounds(row: RowDrawing, y: number): void {
    const { context, metrics } = this
    for (const run of row.backgrounds) {
      this.fillStyle(this.color(run.color))
      context.fillRect(run.column * metrics.cellWidth, y, run.cells * metrics.cellWidth, metrics.cellHeight)
    }
  }

  private foreground(row: RowDrawing, y: number): void {
    this.glyphs(row.glyphs, y)
    const { cellWidth } = this.metrics
    for (const line of row.lines) {
      this.line(line.kind, line.column * cellWidth, y, line.cells * cellWidth, line.color)
    }
  }

  private glyphs(glyphs: readonly GlyphRun[], y: number): void {
    const { context, metrics } = this
    const baselineY = y + metrics.baseline
    for (const glyph of glyphs) {
      const face = this.fontFace(glyph.bold, glyph.italic)
      this.fillStyle(this.color(glyph.color, glyph.alpha))
      const x = glyph.column * metrics.cellWidth

      if (glyph.batchable) {
        if (glyph.text.length === 1 || this.batchableFace[face]) {
          context.fillText(glyph.text, x, baselineY)
        } else {
          // Proportional/drifting font fallback: snap each ASCII glyph to its exact column cell
          for (let k = 0; k < glyph.text.length; k += 1) {
            const ch = glyph.text[k]!
            if (ch !== ' ') context.fillText(ch, (glyph.column + k) * metrics.cellWidth, baselineY)
          }
        }
        continue
      }

      if (!this.isWider(glyph.text, glyph.cells, face)) {
        context.fillText(glyph.text, x, baselineY)
        continue
      }

      // Clip ONLY this individual overhanging glyph horizontally to its own cells (never whole-row clip)
      context.save()
      context.beginPath()
      context.rect(x, y - metrics.cellHeight, glyph.cells * metrics.cellWidth, metrics.cellHeight * 3)
      context.clip()
      context.fillText(glyph.text, x, baselineY)
      context.restore()
      this.face = null
      this.fill = null
    }
  }

  private line(kind: LineKind, x: number, y: number, width: number, color: number): void {
    const { context, metrics } = this
    const thickness = Math.max(1, Math.round(metrics.cellHeight / 16))
    const under = y + Math.min(metrics.baseline + thickness + 1, metrics.cellHeight - thickness)
    context.strokeStyle = this.color(color)
    context.lineWidth = thickness
    const stroke = (at: number, dash: number[] = []) => {
      context.setLineDash(dash)
      context.beginPath()
      context.moveTo(x, at + thickness / 2)
      context.lineTo(x + width, at + thickness / 2)
      context.stroke()
      context.setLineDash([])
    }
    switch (kind) {
      case 'single': stroke(under); break
      case 'double': stroke(under - thickness); stroke(under + thickness); break
      case 'dotted': stroke(under, [thickness, thickness * 2]); break
      case 'dashed': stroke(under, [thickness * 4, thickness * 2]); break
      case 'strikethrough': stroke(y + Math.round(metrics.baseline * 0.65)); break
      case 'overline': stroke(y); break
      case 'curly': {
        const amplitude = thickness * 1.5
        context.beginPath()
        context.moveTo(x, under)
        for (let at = 0; at < width; at += metrics.cellWidth / 2) {
          const up = Math.round(at / (metrics.cellWidth / 2)) % 2 === 0
          context.quadraticCurveTo(
            x + at + metrics.cellWidth / 4, under + (up ? -amplitude : amplitude),
            x + Math.min(at + metrics.cellWidth / 2, width), under,
          )
        }
        context.stroke()
        break
      }
    }
  }

  private cursor(scene: Scene): void {
    const { context, metrics } = this
    const cursor = scene.cursor!
    const x = cursor.column * metrics.cellWidth
    const y = (cursor.row - scene.offset) * metrics.cellHeight
    const width = cursor.cells * metrics.cellWidth
    const thickness = Math.max(1, Math.round(metrics.cellWidth / 6))
    this.fillStyle(this.color(scene.theme.cursor))
    switch (cursor.shape) {
      case 'bar': context.fillRect(x, y, thickness, metrics.cellHeight); break
      case 'underline': context.fillRect(x, y + metrics.cellHeight - thickness, width, thickness); break
      case 'hollow':
        context.strokeStyle = this.color(scene.theme.cursor)
        context.lineWidth = 1
        context.strokeRect(x + 0.5, y + 0.5, width - 1, metrics.cellHeight - 1)
        break
      case 'block':
        context.fillRect(x, y, width, metrics.cellHeight)
        if (cursor.text) {
          const face = this.fontFace(cursor.text.bold, cursor.text.italic)
          this.fillStyle(this.color(scene.theme.cursorText))
          if (!this.isWider(cursor.text.text, cursor.cells, face)) {
            context.fillText(cursor.text.text, x, y + metrics.baseline)
          } else {
            context.save()
            context.beginPath()
            context.rect(x, y - metrics.cellHeight, width, metrics.cellHeight * 3)
            context.clip()
            context.fillText(cursor.text.text, x, y + metrics.baseline)
            context.restore()
            this.face = null
            this.fill = null
          }
        }
        break
    }
  }
}

function collectBands(flags: Uint8Array): Array<[number, number]> {
  const bands: Array<[number, number]> = []
  for (let i = 0; i < flags.length; i += 1) {
    if (!flags[i]) continue
    const last = bands.at(-1)
    if (last && last[1] === i) last[1] = i + 1
    else bands.push([i, i + 1])
  }
  return bands
}

function facesOf(font: Font): string[] {
  return [fontOf(font, false, false), fontOf(font, true, false), fontOf(font, false, true), fontOf(font, true, true)]
}

function same<T>(one: readonly T[] | undefined, other: readonly T[] | undefined, equal: (a: T, b: T) => boolean): boolean {
  const a = one ?? []
  const b = other ?? []
  return a.length === b.length && a.every((item, index) => equal(item, b[index]!))
}

type Mark = NonNullable<Scene['marks']>[number]
type Image = NonNullable<Scene['images']>[number]

const sameMark = (a: Mark, b: Mark): boolean => a.row === b.row && a.color === b.color

const sameImage = (a: Image, b: Image): boolean =>
  a.picture === b.picture && a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height && a.above === b.above
  && a.source.x === b.source.x && a.source.y === b.source.y && a.source.width === b.source.width && a.source.height === b.source.height

function sameCursor(a: Scene['cursor'], b: Scene['cursor']): boolean {
  if (!a || !b) return a === b
  return a.column === b.column && a.row === b.row && a.cells === b.cells && a.shape === b.shape
    && a.text?.text === b.text?.text && a.text?.bold === b.text?.bold && a.text?.italic === b.text?.italic
}
