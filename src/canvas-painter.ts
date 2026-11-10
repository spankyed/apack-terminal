import type { LineKind, RowDrawing } from './draw.ts'
import { css, fontOf, measure, type Font, type Metrics, type Painter, type Scene } from './painter.ts'

/**
 * Draws a scene on a 2D canvas.
 *
 * The painter for where there is no GPU to draw with: a remote desktop, a blocked driver, a lost context
 * that did not come back. It draws every character with `fillText`, which is slower than a GPU and exactly
 * as correct.
 */
export class CanvasPainter implements Painter {
  readonly canvas: HTMLCanvasElement
  metrics: Metrics
  private readonly context: CanvasRenderingContext2D
  private pixelRatio = 1
  private columns = 0
  private rows = 0
  private gutter = 0

  constructor(private readonly font: Font, canvas = document.createElement('canvas')) {
    const context = canvas.getContext('2d')
    if (!context) throw new Error('This canvas has no 2D context to draw a terminal on')
    this.canvas = canvas
    this.context = context
    this.metrics = measure(context, font, this.pixelRatio)
  }

  resize(columns: number, rows: number, pixelRatio: number, gutter = 0): void {
    this.columns = columns
    this.rows = rows
    this.pixelRatio = pixelRatio
    this.gutter = gutter
    this.metrics = measure(this.context, this.font, pixelRatio)
    const width = columns * this.metrics.cellWidth + gutter
    const height = rows * this.metrics.cellHeight
    this.canvas.width = Math.round(width * pixelRatio)
    this.canvas.height = Math.round(height * pixelRatio)
    this.canvas.style.width = `${width}px`
    this.canvas.style.height = `${height}px`
  }

  paint(scene: Scene): void {
    const { context, metrics } = this
    const { cellWidth, cellHeight } = metrics
    // Sizing a canvas resets its transform, so the scale is set per frame rather than once
    context.setTransform(this.pixelRatio, 0, 0, this.pixelRatio, 0, 0)
    context.fillStyle = css(scene.theme.background)
    context.fillRect(0, 0, this.columns * cellWidth + this.gutter, this.rows * cellHeight)

    // In the gutter, before the rows are moved clear of it: a bar beside each marked row
    if (this.gutter > 0) {
      for (const mark of scene.marks ?? []) {
        context.fillStyle = css(mark.color)
        context.fillRect(1, (mark.row - scene.offset) * cellHeight + 2, Math.max(2, this.gutter - 3), cellHeight - 4)
      }
    }
    // Everything after this is drawn in cells, the first of which starts where the gutter ends
    context.translate(this.gutter, 0)

    // Backgrounds first across every row, so a tall character is never cut by the row below being filled
    scene.rows.forEach((row, index) => {
      if (row) this.backgrounds(row, (index - scene.offset) * cellHeight)
    })
    scene.rows.forEach((row, index) => {
      if (row) this.foreground(row, (index - scene.offset) * cellHeight)
    })
    if (scene.cursor) this.cursor(scene)
  }

  dispose(): void {
    this.canvas.remove()
  }

  private backgrounds(row: RowDrawing, y: number): void {
    const { context, metrics } = this
    for (const run of row.backgrounds) {
      context.fillStyle = css(run.color)
      context.fillRect(run.column * metrics.cellWidth, y, run.cells * metrics.cellWidth, metrics.cellHeight)
    }
  }

  private foreground(row: RowDrawing, y: number): void {
    const { context, metrics } = this
    context.textBaseline = 'alphabetic'
    for (const glyph of row.glyphs) {
      context.font = fontOf(this.font, glyph.bold, glyph.italic)
      context.fillStyle = css(glyph.color, glyph.alpha)
      context.fillText(glyph.text, glyph.column * metrics.cellWidth, y + metrics.baseline)
    }
    for (const line of row.lines) {
      this.line(line.kind, line.column * metrics.cellWidth, y, line.cells * metrics.cellWidth, line.color)
    }
  }

  private line(kind: LineKind, x: number, y: number, width: number, color: number): void {
    const { context, metrics } = this
    const thickness = Math.max(1, Math.round(metrics.cellHeight / 16))
    const under = y + Math.min(metrics.baseline + thickness + 1, metrics.cellHeight - thickness)
    context.fillStyle = css(color)
    context.strokeStyle = css(color)
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
    context.fillStyle = css(scene.theme.cursor)
    switch (cursor.shape) {
      case 'bar': context.fillRect(x, y, thickness, metrics.cellHeight); break
      case 'underline': context.fillRect(x, y + metrics.cellHeight - thickness, width, thickness); break
      case 'hollow':
        context.strokeStyle = css(scene.theme.cursor)
        context.lineWidth = 1
        context.strokeRect(x + 0.5, y + 0.5, width - 1, metrics.cellHeight - 1)
        break
      case 'block':
        context.fillRect(x, y, width, metrics.cellHeight)
        if (cursor.text) {
          context.font = fontOf(this.font, cursor.text.bold, cursor.text.italic)
          context.fillStyle = css(scene.theme.cursorText)
          context.fillText(cursor.text.text, x, y + metrics.baseline)
        }
        break
    }
  }
}
