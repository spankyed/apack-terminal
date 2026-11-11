// The 2D painter: what it draws for a scene, and where.
import { describe, expect, it } from 'vitest'
import { CanvasPainter } from '../src/canvas-painter.ts'
import { drawRow, type SelectedCells, type Theme } from '../src/draw.ts'
import { css } from '../src/painter.ts'
import { PLAIN, type Row, type Style } from '../src/source.ts'
import { recordingContext } from './_canvas.ts'
import { row, PALETTE } from './_support.ts'

/** A row drawn with `styles` as its own: the plain one, then whatever its characters name */
const drawStyled = (row: Row, styles: Style[], theme: Theme, selected?: SelectedCells) => drawRow({ ...row, styles }, theme, selected)

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

  it('is measured for the display it is on from the start, before anything says how many cells there are', () => {
    // A character 8.4 pixels wide is 8 on a display of whole pixels, and 8.5 on one with two to a pixel
    expect(new CanvasPainter(font, recordingContext(8.4).canvas, 1).metrics.cellWidth).toBe(8)
    expect(new CanvasPainter(font, recordingContext(8.4).canvas, 2).metrics.cellWidth).toBe(8.5)
  })

  it('is measured again for another typeface, and the canvas is sized for the cells it has now', () => {
    const { canvas, cell } = recordingContext()
    const made = new CanvasPainter(font, canvas)
    made.resize(20, 4, 1)
    cell.width = 12
    made.setFont({ family: 'serif', size: 18 })
    expect(made.metrics.cellWidth).toBe(12)
    expect(made.canvas.style.width).toBe('240px')
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
    const first = drawStyled(row(0, 'hi'), [PLAIN], theme)
    const second = drawStyled({ ...row(1, 'x'), style: Uint16Array.from([1]) }, [PLAIN, red], theme)

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
    const styled = (line: number) => drawStyled({ ...row(line, 'a'), style: Uint16Array.from([1]) }, [PLAIN, onRed], theme)

    made.paint({ theme, rows: [styled(0), styled(1)], offset: 0 })

    const kinds = drawn.slice(1).map((call) => call.kind)
    expect(kinds).toEqual(['rect', 'rect', 'text', 'text'])
  })

  it('is shifted up by the fraction of a row the view is scrolled between two lines', () => {
    const { painter: made, drawn } = painter()
    made.paint({ theme, rows: [drawStyled(row(0, 'a'), [PLAIN], theme), drawStyled(row(1, 'b'), [PLAIN], theme)], offset: 0.5 })

    expect(drawn.filter((call) => call.kind === 'text').map((call) => call.y)).toEqual([6, 26])
  })

  it('leaves a row it does not hold blank', () => {
    const { painter: made, drawn } = painter()
    made.paint({ theme, rows: [null, drawStyled(row(1, 'b'), [PLAIN], theme)], offset: 0 })

    expect(drawn.filter((call) => call.kind === 'text')).toHaveLength(1)
  })

  it('uses the bold and italic faces a character asks for', () => {
    const { painter: made, drawn } = painter()
    const strong: Style = { ...PLAIN, flags: 1 | 2 }
    made.paint({ theme, rows: [drawStyled({ ...row(0, 'a'), style: Uint16Array.from([1]) }, [PLAIN, strong], theme)], offset: 0 })

    expect(drawn.find((call) => call.kind === 'text')?.font).toBe('italic bold 14px monospace')
  })
})

describe('what the canvas is told', () => {
  it('is a font and a colour when they change, not for every character', () => {
    const { canvas, given } = recordingContext()
    const made = new CanvasPainter(font, canvas)
    made.resize(20, 4, 1)
    const before = { ...given }
    made.paint({ theme, rows: [drawStyled(row(0, 'hello world'), [PLAIN], theme), drawStyled(row(1, 'again'), [PLAIN], theme)], offset: 0 })

    expect(given.font - before.font, 'the font was set again for characters in the same face').toBe(1)
    expect(given.fillStyle - before.fillStyle, 'the background, then one colour for every character').toBe(2)
  })
})

describe('a character wider than its cells', () => {
  it('is kept to them from side to side, and one that fits is drawn as it is', () => {
    const { painter: made, drawn } = painter()
    // One cell each; the recording canvas measures a character by its code units, so the second is two cells wide
    const narrow = { ...row(0, 'ab'), text: ['a', 'e\u0301'] }
    // And an emoji that has the two cells it measures
    const emoji = { ...row(1, 'x'), text: ['\u{1f600}'], width: Uint8Array.from([2]) }
    made.paint({ theme, rows: [drawStyled(narrow, [PLAIN], theme), drawStyled(emoji, [PLAIN], theme)], offset: 0 })

    // Each inside its row and the two beside it, as every character not of the font's own is; the one
    // wider than its cell inside that cell's width as well
    const text = (character: string) => drawn.find((call) => call.text === character)!
    expect(text('a').clips).toEqual([[0, -20, 200, 60]])
    expect(text('e\u0301').clips, 'left to cover the cell beside it').toEqual([[0, -20, 200, 60], [10, -20, 10, 60]])
    expect(text('\u{1f600}').clips).toEqual([[0, 0, 200, 60]])
  })
})

describe('a scene painted over the one before it', () => {
  /** Twelve rows on a painter sized for them, each drawn once and kept, as a view keeps them */
  function painted() {
    const { canvas, drawn } = recordingContext()
    const made = new CanvasPainter(font, canvas)
    made.resize(20, 12, 1)
    const rows = Array.from({ length: 12 }, (_, line) => drawStyled(row(line, `line ${line}`), [PLAIN], theme))
    made.paint({ theme, rows, offset: 0 })
    drawn.length = 0
    return { painter: made, drawn, rows }
  }
  const texts = (drawn: ReturnType<typeof painted>['drawn']) => [...new Set(drawn.filter((call) => call.kind === 'text').map((call) => call.y))]

  it('draws nothing where nothing changed', () => {
    const { painter: made, drawn, rows } = painted()
    made.paint({ theme, rows: [...rows], offset: 0 })
    expect(drawn).toEqual([])
  })

  it('draws the row that changed and the rows a character of it could reach, and nothing outside them', () => {
    const { painter: made, drawn, rows } = painted()
    const next = [...rows]
    next[5] = drawStyled(row(5, 'changed'), [PLAIN], theme)
    made.paint({ theme, rows: next, offset: 0 })

    // Rows four to six, and nothing drawn but inside them
    expect(drawn.length).toBeGreaterThan(0)
    for (const call of drawn) expect(call.clips[0], `a ${call.kind} at ${call.y}`).toEqual([0, 80, 200, 60])
    expect(drawn.filter((call) => call.kind === 'text').map((call) => call.text).join('')).toContain('changed')
    // The rows beside those are drawn too, since their characters may reach in: three to seven
    expect(texts(drawn)).toEqual([76, 96, 116, 136, 156])
  })

  it('draws where the cursor was and where it is, when the cursor is all that moved', () => {
    const { painter: made, drawn, rows } = painted()
    const cursor = { column: 2, row: 9, cells: 1, shape: 'block' as const }
    made.paint({ theme, rows, offset: 0, cursor })
    expect(drawn.at(-1)).toMatchObject({ kind: 'rect', x: 20, y: 180, color: css(theme.cursor), clips: [[0, 160, 200, 60]] })

    // The off half of a blink: the cell is drawn again without it
    drawn.length = 0
    made.paint({ theme, rows, offset: 0 })
    expect(drawn.some((call) => call.color === css(theme.cursor))).toBe(false)
    expect(texts(drawn)).toEqual([156, 176, 196, 216, 236])
  })

  it('is drawn whole when the canvas was sized, the colours changed, or the view is between two lines', () => {
    const whole = (drawn: ReturnType<typeof painted>['drawn']) =>
      drawn[0]?.kind === 'rect' && drawn[0].clips.length === 0 && drawn.filter((call) => call.kind === 'text').length > 12

    const sized = painted()
    sized.painter.resize(20, 12, 2)
    sized.painter.paint({ theme, rows: sized.rows, offset: 0 })
    expect(whole(sized.drawn), 'after the canvas was sized, which clears it').toBe(true)

    const themed = painted()
    themed.painter.paint({ theme: { ...theme, background: 0xffffff }, rows: themed.rows, offset: 0 })
    expect(whole(themed.drawn), 'in other colours').toBe(true)

    const between = painted()
    between.painter.paint({ theme, rows: between.rows, offset: 0.5 })
    expect(whole(between.drawn), 'between two lines').toBe(true)

    const marked = painted()
    marked.painter.paint({ theme, rows: marked.rows, offset: 0, images: [{ picture: {} as CanvasImageSource, x: 0, y: 0, width: 10, height: 10, source: { x: 0, y: 0, width: 1, height: 1 }, above: true }] })
    expect(whole(marked.drawn), 'with an image it did not have').toBe(true)
  })

  it('is drawn again when the browser gives back a context it took, which comes back blank', () => {
    const { painter: made, drawn, rows } = painted()
    let asked = 0
    made.whenCleared(() => { asked += 1 })
    made.canvas.dispatchEvent(new Event('contextrestored'))
    expect(asked, 'nothing was asked to paint the blank canvas').toBe(1)

    made.paint({ theme, rows, offset: 0 })
    expect(drawn.filter((call) => call.kind === 'text').length, 'the scene it thought was showing was not drawn').toBeGreaterThan(12)
  })

  it('is drawn whole when most of it changed, which is cheaper than piece by piece', () => {
    const { painter: made, drawn } = painted()
    made.paint({ theme, rows: Array.from({ length: 12 }, (_, line) => drawStyled(row(line, 'new'), [PLAIN], theme)), offset: 0 })
    expect(drawn.every((call) => call.clips.length === 0)).toBe(true)
  })
})

describe('the cursor', () => {
  const scene = (shape: 'block' | 'bar' | 'underline' | 'hollow') => ({
    theme, rows: [drawStyled(row(0, 'ab'), [PLAIN], theme)], offset: 0,
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

    made.paint({ theme, rows: [drawStyled(row(0, 'hi'), [PLAIN], theme), null, drawStyled(row(2, 'x'), [PLAIN], theme)], offset: 0, marks: [{ row: 2, color: 0x00ff00 }] })

    expect(drawn[0]).toMatchObject({ kind: 'rect', x: 0, width: 206 })
    expect(drawn.find((call) => call.color === css(0x00ff00))).toMatchObject({ kind: 'rect', x: 1, y: 42, width: 3, height: 16 })
    expect(drawn.find((call) => call.text === 'h')).toMatchObject({ x: 6 })
    expect(drawn.find((call) => call.text === 'x')).toMatchObject({ x: 6, y: 56 })
  })

  it('is not there unless asked for, and then nothing is marked', () => {
    const { painter: made, drawn } = painter()
    made.paint({ theme, rows: [drawStyled(row(0, 'hi'), [PLAIN], theme)], offset: 0, marks: [{ row: 0, color: 0x00ff00 }] })
    expect(drawn.some((call) => call.color === css(0x00ff00))).toBe(false)
    expect(drawn.find((call) => call.text === 'h')).toMatchObject({ x: 0 })
  })
})
