import type { LineKind, RowDrawing } from './draw.ts'
import { css, fontOf, measure, type Font, type Metrics, type Painter, type Scene } from './painter.ts'

/** How many colours and measured characters are remembered before the memory is started again */
const REMEMBERED = 4096

/**
 * Draws a scene on a 2D canvas.
 *
 * The painter for where there is no GPU to draw with: a remote desktop, a blocked driver, a lost context
 * that did not come back. It draws every character with `fillText`, which is slower than a GPU and exactly
 * as correct.
 *
 * **It draws what changed.** The scene on the canvas is kept, and a row's drawing is the same object for
 * as long as the row looks the same, so the rows that differ from what is shown are known without reading
 * them. Those are drawn again, each band of them inside a clip of its own rows, and the canvas ends up
 * exactly as drawing the whole scene would leave it; a scene that differs in anything but its rows and
 * cursor is drawn whole.
 */
export class CanvasPainter implements Painter {
  readonly canvas: HTMLCanvasElement
  metrics: Metrics
  private readonly context: CanvasRenderingContext2D
  private columns = 0
  private rows = 0
  private gutter = 0
  /** The font shorthand of each face: bold is `1`, italic `2` */
  private faces: readonly string[]
  /** The scene the canvas shows, or `null` when it shows none: never painted, or sized since */
  private shown: Scene | null = null
  /** What the context was last given, so that it is given a font or a colour only when that changes */
  private face: string | null = null
  private fill: string | null = null
  private readonly colors = new Map<number, string>()
  private readonly reaching = new WeakMap<RowDrawing, boolean>()
  /** Whether a character, in a face, is wider than the cells it has */
  private readonly wider = new Map<string, boolean>()

  /** `pixelRatio` is the display's, so that a cell measured before the first `resize` is one of its cells */
  constructor(private font: Font, canvas = document.createElement('canvas'), private pixelRatio = globalThis.devicePixelRatio || 1) {
    const context = canvas.getContext('2d')
    if (!context) throw new Error('This canvas has no 2D context to draw a terminal on')
    this.canvas = canvas
    this.context = context
    this.faces = facesOf(font)
    this.metrics = measure(context, font, pixelRatio)
    // A context the browser took away comes back blank, whatever was on it
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

  resize(columns: number, rows: number, pixelRatio: number, gutter = 0): void {
    this.columns = columns
    this.rows = rows
    this.pixelRatio = pixelRatio
    this.gutter = gutter
    this.metrics = measure(this.context, this.font, pixelRatio)
    const width = columns * this.metrics.cellWidth + gutter
    const height = rows * this.metrics.cellHeight
    // Sizing a canvas clears it, and a cell may no longer be the size the widths were measured against
    this.canvas.width = Math.round(width * pixelRatio)
    this.canvas.height = Math.round(height * pixelRatio)
    this.canvas.style.width = `${width}px`
    this.canvas.style.height = `${height}px`
    this.shown = null
    this.wider.clear()
  }

  setFont(font: Font): void {
    this.font = font
    this.faces = facesOf(font)
    this.resize(this.columns, this.rows, this.pixelRatio, this.gutter)
  }

  paint(scene: Scene): void {
    const { context, metrics } = this
    // Sizing a canvas resets its transform and everything else it was given, so both are set per paint
    context.setTransform(this.pixelRatio, 0, 0, this.pixelRatio, 0, 0)
    context.textBaseline = 'alphabetic'
    this.face = null
    this.fill = null

    const bands = this.changed(scene)
    if (bands === null) {
      this.draw(scene, 0, scene.rows.length)
    } else {
      for (const [first, end] of bands) {
        context.save()
        context.beginPath()
        context.rect(0, first * metrics.cellHeight, this.columns * metrics.cellWidth + this.gutter, (end - first) * metrics.cellHeight)
        context.clip()
        // A character can reach into the rows beside its own, so those are drawn too, and the clip keeps
        // of them only what falls in the band
        this.draw(scene, first - 1, end + 1)
        context.restore()
        this.face = null
        this.fill = null
      }
    }
    this.shown = scene
  }

  dispose(): void {
    this.canvas.removeEventListener('contextrestored', this.cleared)
    this.canvas.remove()
  }

  /**
   * The rows that differ from what the canvas shows, as bands of `[first, end)`, or `null` when the whole
   * scene is to be drawn: nothing is shown, more than rows and the cursor changed, the view is between two
   * lines — where a band's edges are not on whole pixels — or most of the rows changed anyway.
   */
  private changed(scene: Scene): Array<[number, number]> | null {
    const shown = this.shown
    if (!shown || shown.theme !== scene.theme || scene.offset !== 0 || shown.offset !== 0) return null
    if (shown.rows.length !== scene.rows.length) return null
    if (!same(shown.marks, scene.marks, sameMark) || !same(shown.images, scene.images, sameImage)) return null

    const count = scene.rows.length
    const differs = new Uint8Array(count)
    for (let index = 0; index < count; index += 1) if (scene.rows[index] !== shown.rows[index]) differs[index] = 1
    if (!sameCursor(shown.cursor, scene.cursor)) {
      for (const cursor of [shown.cursor, scene.cursor]) if (cursor && cursor.row >= 0 && cursor.row < count) differs[cursor.row] = 1
    }
    // What a row's characters reached of the rows beside it changed with it
    const bands: Array<[number, number]> = []
    let drawn = 0
    for (let index = 0; index < count; index += 1) {
      if (!differs[index] && !differs[index - 1] && !differs[index + 1]) continue
      drawn += 1
      const last = bands.at(-1)
      if (last && last[1] === index) last[1] = index + 1
      else bands.push([index, index + 1])
    }
    return drawn * 2 > count ? null : bands
  }

  /** Draws the scene's rows from `first` up to `end`, and everything of it that is not a row */
  private draw(scene: Scene, first: number, end: number): void {
    const { context, metrics } = this
    const { cellWidth, cellHeight } = metrics
    const from = Math.max(0, first)
    const to = Math.min(scene.rows.length, end)
    this.fillStyle(this.color(scene.theme.background))
    context.fillRect(0, 0, this.columns * cellWidth + this.gutter, this.rows * cellHeight)

    // In the gutter, before the rows are moved clear of it: a bar beside each marked row
    if (this.gutter > 0) {
      for (const mark of scene.marks ?? []) {
        if (mark.row < from || mark.row >= to) continue
        this.fillStyle(this.color(mark.color))
        context.fillRect(1, (mark.row - scene.offset) * cellHeight + 2, Math.max(2, this.gutter - 3), cellHeight - 4)
      }
    }
    // Everything after this is drawn in cells, the first of which starts where the gutter ends
    context.translate(this.gutter, 0)

    // Backgrounds first across every row, so a tall character is never cut by the row below being filled
    for (let index = from; index < to; index += 1) {
      const row = scene.rows[index]
      if (row) this.backgrounds(row, (index - scene.offset) * cellHeight)
    }
    this.images(scene, false)
    for (let index = from; index < to; index += 1) {
      const row = scene.rows[index]
      if (row) this.foreground(row, (index - scene.offset) * cellHeight)
    }
    this.images(scene, true)
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

  /** A colour as CSS, written once for each colour and strength */
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

  /**
   * Whether a character drawn in the face the context has is wider than its cells. A character of the
   * font's own — one ASCII letter — is a cell by definition; any other may come from whichever font the
   * browser fell back to, at whatever width that font gave it, and is measured once.
   */
  private isWider(text: string, cells: number, face: number): boolean {
    if (text.length === 1 && text.charCodeAt(0) < 0x80) return false
    const key = `${face}${cells}${text}`
    let wider = this.wider.get(key)
    if (wider === undefined) {
      if (this.wider.size >= REMEMBERED) this.wider.clear()
      wider = this.context.measureText(text).width > cells * this.metrics.cellWidth + 0.5
      this.wider.set(key, wider)
    }
    return wider
  }

  /** The images on one side of the text: over the cell backgrounds and under the characters, or over both */
  private images(scene: Scene, above: boolean): void {
    for (const image of scene.images ?? []) {
      if (image.above !== above || image.source.width <= 0 || image.source.height <= 0) continue
      this.context.drawImage(
        image.picture,
        image.source.x, image.source.y, image.source.width, image.source.height,
        image.x, image.y - scene.offset * this.metrics.cellHeight, image.width, image.height,
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

  /**
   * A row's characters and lines. **A character reaches no further than the rows beside its own**: one
   * that could — a character not of the font's own, or one of several code points, which is where marks
   * stacked on a letter are — is drawn inside a clip of those three rows. That is what lets a row that
   * changed be drawn again with only its neighbours and come out as drawing everything would.
   */
  private foreground(row: RowDrawing, y: number): void {
    const { context, metrics } = this
    const reaches = this.reaches(row)
    if (reaches) {
      context.save()
      context.beginPath()
      context.rect(-this.gutter, y - metrics.cellHeight, this.columns * metrics.cellWidth + this.gutter, metrics.cellHeight * 3)
      context.clip()
    }
    this.glyphs(row, y)
    if (reaches) {
      context.restore()
      this.face = null
      this.fill = null
    }
    for (const line of row.lines) {
      this.line(line.kind, line.column * metrics.cellWidth, y, line.cells * metrics.cellWidth, line.color)
    }
  }

  /** Whether any character of a row is one that might be taller than a cell: asked once for each drawing */
  private reaches(row: RowDrawing): boolean {
    let reaches = this.reaching.get(row)
    if (reaches === undefined) {
      reaches = row.glyphs.some((glyph) => glyph.text.length !== 1 || glyph.text.charCodeAt(0) >= 0x80)
      this.reaching.set(row, reaches)
    }
    return reaches
  }

  private glyphs(row: RowDrawing, y: number): void {
    const { context, metrics } = this
    for (const glyph of row.glyphs) {
      const face = this.fontFace(glyph.bold, glyph.italic)
      this.fillStyle(this.color(glyph.color, glyph.alpha))
      const x = glyph.column * metrics.cellWidth
      if (!this.isWider(glyph.text, glyph.cells, face)) {
        context.fillText(glyph.text, x, y + metrics.baseline)
        continue
      }
      // Kept to its own cells from side to side, so it does not cover the characters beside it; up and
      // down it may reach as far as any other character does
      context.save()
      context.beginPath()
      context.rect(x, y - metrics.cellHeight, glyph.cells * metrics.cellWidth, metrics.cellHeight * 3)
      context.clip()
      context.fillText(glyph.text, x, y + metrics.baseline)
      context.restore()
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
        // A wave one cell long, so it meets itself at every cell boundary whatever the run's length
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
          this.fontFace(cursor.text.bold, cursor.text.italic)
          this.fillStyle(this.color(scene.theme.cursorText))
          context.fillText(cursor.text.text, x, y + metrics.baseline)
        }
        break
    }
  }
}

function facesOf(font: Font): string[] {
  return [fontOf(font, false, false), fontOf(font, true, false), fontOf(font, false, true), fontOf(font, true, true)]
}

/** Whether two lists, either of which may be left out, hold the same things in the same order */
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
