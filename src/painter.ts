import type { RowDrawing, Theme } from './draw.ts'
import type { CursorShape } from './source.ts'

/** The typeface a view draws in */
export interface Font {
  readonly family: string
  /** In CSS pixels */
  readonly size: number
}

/** One cell, in CSS pixels. Whole device pixels, so that a column of cells has no seams */
export interface Metrics {
  readonly cellWidth: number
  readonly cellHeight: number
  /** From the top of a cell down to where a character sits */
  readonly baseline: number
}

/** Everything one frame of drawing needs */
export interface Scene {
  readonly theme: Theme
  /** The rows to draw, top to bottom. `null` is a line the view does not hold yet: it is drawn blank */
  readonly rows: ReadonlyArray<RowDrawing | null>
  /** How far the grid is shifted up, as a fraction of a row: the view is between two lines */
  readonly offset: number
  /**
   * Rows marked in the gutter, each in a colour: where a command began, coloured by how it ended. Drawn
   * only where the painter was given a gutter to draw them in.
   */
  readonly marks?: ReadonlyArray<{ readonly row: number; readonly color: number }>
  /**
   * Images a program placed, in the order to draw them. Positions and sizes are in CSS pixels from the
   * top-left of the first row, before `offset` is applied; `source` is the part of the picture drawn.
   */
  readonly images?: ReadonlyArray<{
    readonly picture: CanvasImageSource
    readonly x: number
    readonly y: number
    readonly width: number
    readonly height: number
    readonly source: { readonly x: number; readonly y: number; readonly width: number; readonly height: number }
    /** Drawn under the text when false: the program put it behind what it prints */
    readonly above: boolean
  }>
  /** Absent when there is none to draw: hidden by the program, off the view, or in the off half of a blink */
  readonly cursor?: {
    readonly column: number
    /** Among `rows` */
    readonly row: number
    readonly cells: number
    readonly shape: CursorShape
    /** The character under a block cursor, drawn again over it in the cursor's text colour */
    readonly text?: { readonly text: string; readonly bold: boolean; readonly italic: boolean }
  }
}

/**
 * What draws a scene. Two of these exist — one on the GPU, one on a 2D canvas for where there is none —
 * and a view does not know which it holds.
 */
export interface Painter {
  readonly canvas: HTMLCanvasElement
  readonly metrics: Metrics
  /** The view is `columns` by `rows` cells, on a display with `pixelRatio` device pixels to a CSS pixel */
  /** `gutter` is a strip of that many pixels left of the first column, which rows are not drawn in */
  resize(columns: number, rows: number, pixelRatio: number, gutter?: number): void
  paint(scene: Scene): void
  dispose(): void
}

/** `0xRRGGBB` as a CSS colour */
export function css(color: number, alpha = 1): string {
  const r = (color >> 16) & 0xff
  const g = (color >> 8) & 0xff
  const b = color & 0xff
  return alpha === 1 ? `rgb(${r} ${g} ${b})` : `rgb(${r} ${g} ${b} / ${alpha})`
}

/** The CSS font shorthand for a character of this weight and slant */
export function fontOf(font: Font, bold: boolean, italic: boolean): string {
  return `${italic ? 'italic ' : ''}${bold ? 'bold ' : ''}${font.size}px ${font.family}`
}

/**
 * One cell of `font`, snapped to whole device pixels.
 *
 * **Snapped because a fraction accumulates.** A cell 8.4 pixels wide puts the hundredth column forty
 * pixels from where a rounded one would, and a background drawn per cell shows a seam wherever two of them
 * meet between pixels. The width a browser measures is rounded to the device grid and every cell is that.
 */
export function measure(context: CanvasRenderingContext2D, font: Font, pixelRatio: number): Metrics {
  context.font = fontOf(font, false, false)
  const measured = context.measureText('M')
  const snap = (value: number) => Math.max(1, Math.round(value * pixelRatio)) / pixelRatio
  const ascent = measured.fontBoundingBoxAscent ?? measured.actualBoundingBoxAscent ?? font.size * 0.8
  const descent = measured.fontBoundingBoxDescent ?? measured.actualBoundingBoxDescent ?? font.size * 0.2
  return {
    cellWidth: snap(measured.width),
    cellHeight: snap(ascent + descent),
    baseline: snap(ascent),
  }
}
