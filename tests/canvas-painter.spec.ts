// The 2D painter: what it draws for a scene, and where.
import { describe, expect, it } from 'vitest'
import { CanvasPainter } from '../src/canvas-painter.ts'
import { drawRow, type Theme } from '../src/draw.ts'
import { css } from '../src/painter.ts'
import { PLAIN, type Style } from '../src/source.ts'
import { recordingContext } from './_canvas.ts'
import { row, PALETTE } from './_support.ts'

const theme: Theme = {
  background: 0x1e1e1e, palette: PALETTE, foreground: 0xd4d4d4, cursor: 0xffffff, cursorText: 0x000000,
  selectionBackground: 0x264f78, selectionForeground: null,
}
const font = { family: 'monospace', size: 14 }

/** A painter over a recording canvas, sized 20 columns by 4 rows: cells are 10 wide and 20 tall */
function painter() {
  const { canvas, drawn } = recordingContext()
  const made = new CanvasPainter(font, canvas)
  made.resize(20, 4, 1)
  return { painter: made, drawn }
}

describe('one cell', () => {
  it('is measured from the font, in whole device pixels', () => {
    const { painter: made } = painter()
    expect(made.metrics).toEqual({ cellWidth: 10, cellHeight: 20, baseline: 16 })
    expect(made.canvas.style.width).toBe('200px')
    expect(made.canvas.height).toBe(80)
  })

  it('has as many device pixels as the display gives it', () => {
    const { painter: made } = painter()
    made.resize(20, 4, 2)
    expect([made.canvas.width, made.canvas.height]).toEqual([400, 160])
    expect(made.canvas.style.height, 'the canvas grew on the page with the pixel ratio').toBe('80px')
  })
})

describe('a scene', () => {
  it('is the background, then each row where it belongs', () => {
    const { painter: made, drawn } = painter()
    const red: Style = { ...PLAIN, bg: 0xcd3131 }
    const first = drawRow(row(0, 'hi'), [PLAIN], theme)
    const second = drawRow({ ...row(1, 'x'), style: Uint16Array.from([1]) }, [PLAIN, red], theme)

    made.paint({ theme, rows: [first, second], offset: 0 })

    expect(drawn[0]).toMatchObject({ kind: 'rect', x: 0, y: 0, width: 200, height: 80, color: css(theme.background) })
    expect(drawn.find((call) => call.color === css(0xcd3131))).toMatchObject({ x: 0, y: 20, width: 10, height: 20 })
    expect(drawn.filter((call) => call.kind === 'text').map(({ text, x, y }) => ({ text, x, y }))).toEqual([
      { text: 'h', x: 0, y: 16 },
      { text: 'i', x: 10, y: 16 },
      { text: 'x', x: 0, y: 36 },
    ])
  })

  it('draws every background before any character, so a tall one is not cut by the row below', () => {
    const { painter: made, drawn } = painter()
    const onRed: Style = { ...PLAIN, bg: 0xcd3131 }
    const styled = (line: number) => drawRow({ ...row(line, 'a'), style: Uint16Array.from([1]) }, [PLAIN, onRed], theme)

    made.paint({ theme, rows: [styled(0), styled(1)], offset: 0 })

    const kinds = drawn.slice(1).map((call) => call.kind)
    expect(kinds).toEqual(['rect', 'rect', 'text', 'text'])
  })

  it('is shifted up by the fraction of a row the view is scrolled between two lines', () => {
    const { painter: made, drawn } = painter()
    made.paint({ theme, rows: [drawRow(row(0, 'a'), [PLAIN], theme), drawRow(row(1, 'b'), [PLAIN], theme)], offset: 0.5 })

    expect(drawn.filter((call) => call.kind === 'text').map((call) => call.y)).toEqual([6, 26])
  })

  it('leaves a row it does not hold blank', () => {
    const { painter: made, drawn } = painter()
    made.paint({ theme, rows: [null, drawRow(row(1, 'b'), [PLAIN], theme)], offset: 0 })

    expect(drawn.filter((call) => call.kind === 'text')).toHaveLength(1)
  })

  it('uses the bold and italic faces a character asks for', () => {
    const { painter: made, drawn } = painter()
    const strong: Style = { ...PLAIN, flags: 1 | 2 }
    made.paint({ theme, rows: [drawRow({ ...row(0, 'a'), style: Uint16Array.from([1]) }, [PLAIN, strong], theme)], offset: 0 })

    expect(drawn.find((call) => call.kind === 'text')?.font).toBe('italic bold 14px monospace')
  })
})

describe('the cursor', () => {
  const scene = (shape: 'block' | 'bar' | 'underline' | 'hollow') => ({
    theme, rows: [drawRow(row(0, 'ab'), [PLAIN], theme)], offset: 0,
    cursor: { column: 1, row: 0, cells: 1, shape, text: { text: 'b', bold: false, italic: false } },
  })

  it('as a block covers its cell and redraws the character over itself', () => {
    const { painter: made, drawn } = painter()
    made.paint(scene('block'))

    const [block, text] = drawn.slice(-2)
    expect(block).toMatchObject({ kind: 'rect', x: 10, y: 0, width: 10, height: 20, color: css(theme.cursor) })
    expect(text).toMatchObject({ kind: 'text', text: 'b', x: 10, color: css(theme.cursorText) })
  })

  it('as a bar is a thin line at the left of its cell, and as an underline one at the bottom', () => {
    const bar = painter()
    bar.painter.paint(scene('bar'))
    expect(bar.drawn.at(-1)).toMatchObject({ x: 10, y: 0, width: 2, height: 20 })

    const underline = painter()
    underline.painter.paint(scene('underline'))
    expect(underline.drawn.at(-1)).toMatchObject({ x: 10, y: 18, width: 10, height: 2 })
  })

  it('hollow is an outline, which is what an unfocused terminal shows', () => {
    const { painter: made, drawn } = painter()
    made.paint(scene('hollow'))
    expect(drawn.at(-1)?.kind).toBe('strokeRect')
  })
})

describe('a gutter', () => {
  it('is a strip left of the first column that rows are drawn clear of, with a bar beside each marked row', () => {
    const { canvas, drawn } = recordingContext()
    const made = new CanvasPainter(font, canvas)
    made.resize(20, 4, 1, 6)
    expect(made.canvas.style.width, 'the gutter is not part of the canvas').toBe('206px')

    made.paint({ theme, rows: [drawRow(row(0, 'hi'), [PLAIN], theme), null, drawRow(row(2, 'x'), [PLAIN], theme)], offset: 0, marks: [{ row: 2, color: 0x00ff00 }] })

    expect(drawn[0]).toMatchObject({ kind: 'rect', x: 0, width: 206 })
    expect(drawn.find((call) => call.color === css(0x00ff00))).toMatchObject({ kind: 'rect', x: 1, y: 42, width: 3, height: 16 })
    expect(drawn.find((call) => call.text === 'h')).toMatchObject({ x: 6 })
    expect(drawn.find((call) => call.text === 'x')).toMatchObject({ x: 6, y: 56 })
  })

  it('is not there unless asked for, and then nothing is marked', () => {
    const { painter: made, drawn } = painter()
    made.paint({ theme, rows: [drawRow(row(0, 'hi'), [PLAIN], theme)], offset: 0, marks: [{ row: 0, color: 0x00ff00 }] })
    expect(drawn.some((call) => call.color === css(0x00ff00))).toBe(false)
    expect(drawn.find((call) => call.text === 'h')).toMatchObject({ x: 0 })
  })
})
