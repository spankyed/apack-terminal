// A row as things to draw: the rules a terminal's styles follow, and what a selection does to them.
import { describe, expect, it } from 'vitest'
import { drawRow, type Theme } from '../src/draw.ts'
import { PLAIN, StyleFlag, type Row, type Style } from '../src/source.ts'
import { row } from './_support.ts'

const theme: Theme = {
  background: 0x1e1e1e,
  foreground: 0xd4d4d4,
  cursor: 0xd4d4d4,
  cursorText: 0x1e1e1e,
  selectionBackground: 0x264f78,
  selectionForeground: null,
}
const RED = 0xcd3131
const BLUE = 0x2472c8

/** A row whose characters take the given style indices, one each */
const styled = (text: string, styles: number[]): Row => ({ ...row(0, text), style: Uint16Array.from(styles) })

describe('a plain row', () => {
  it('draws its characters in the default colour and no background at all', () => {
    const drawing = drawRow(row(0, 'ab'), [PLAIN], theme)

    expect(drawing.backgrounds).toEqual([])
    expect(drawing.glyphs.map(({ text, column, color }) => ({ text, column, color }))).toEqual([
      { text: 'a', column: 0, color: theme.foreground },
      { text: 'b', column: 1, color: theme.foreground },
    ])
  })

  it('draws nothing for a space', () => {
    expect(drawRow(row(0, 'a b'), [PLAIN], theme).glyphs.map((glyph) => glyph.text)).toEqual(['a', 'b'])
  })
})

describe('a background', () => {
  it('is one rectangle for a run of cells that share it', () => {
    const onRed: Style = { ...PLAIN, bg: RED }
    const onBlue: Style = { ...PLAIN, bg: BLUE }
    const drawing = drawRow(styled('aabbc', [1, 1, 2, 2, 0]), [PLAIN, onRed, onBlue], theme)

    expect(drawing.backgrounds).toEqual([
      { column: 0, cells: 2, color: RED },
      { column: 2, cells: 2, color: BLUE },
    ])
  })

  it('is drawn under a space, which has no character but still has a colour', () => {
    const drawing = drawRow(styled(' ', [1]), [PLAIN, { ...PLAIN, bg: RED }], theme)

    expect(drawing.backgrounds).toEqual([{ column: 0, cells: 1, color: RED }])
    expect(drawing.glyphs).toEqual([])
  })
})

describe('inverse', () => {
  it('swaps the two colours, defaults included', () => {
    const drawing = drawRow(styled('a', [1]), [PLAIN, { ...PLAIN, flags: StyleFlag.inverse }], theme)

    expect(drawing.backgrounds).toEqual([{ column: 0, cells: 1, color: theme.foreground }])
    expect(drawing.glyphs[0]?.color).toBe(theme.background)
  })

  it('swaps chosen colours too', () => {
    const style: Style = { ...PLAIN, fg: RED, bg: BLUE, flags: StyleFlag.inverse }
    const drawing = drawRow(styled('a', [1]), [PLAIN, style], theme)

    expect(drawing.backgrounds[0]?.color).toBe(RED)
    expect(drawing.glyphs[0]?.color).toBe(BLUE)
  })
})

describe('text attributes', () => {
  it('reach the character: bold, italic, and faint as half strength', () => {
    const style: Style = { ...PLAIN, flags: StyleFlag.bold | StyleFlag.italic | StyleFlag.faint }
    const [glyph] = drawRow(styled('a', [1]), [PLAIN, style], theme).glyphs

    expect(glyph).toMatchObject({ bold: true, italic: true, alpha: 0.5 })
  })

  it('invisible text keeps its background and draws neither character nor line', () => {
    const style: Style = { ...PLAIN, bg: RED, flags: StyleFlag.invisible | StyleFlag.strikethrough | (1 << 8) }
    const drawing = drawRow(styled('a', [1]), [PLAIN, style], theme)

    expect(drawing.backgrounds).toHaveLength(1)
    expect(drawing.glyphs).toEqual([])
    expect(drawing.lines).toEqual([])
  })
})

describe('lines through and under text', () => {
  it('are one line per run, in the underline colour where one was given and the text colour where not', () => {
    const curly: Style = { ...PLAIN, underlineColor: RED, flags: 3 << 8 }
    const struck: Style = { ...PLAIN, fg: BLUE, flags: StyleFlag.strikethrough }
    const drawing = drawRow(styled('aabb', [1, 1, 2, 2]), [PLAIN, curly, struck], theme)

    expect(drawing.lines).toEqual([
      { kind: 'curly', column: 0, cells: 2, color: RED },
      { kind: 'strikethrough', column: 2, cells: 2, color: BLUE },
    ])
  })

  it('an overline and an underline on the same cells are both drawn', () => {
    const style: Style = { ...PLAIN, flags: StyleFlag.overline | (1 << 8) }
    const kinds = drawRow(styled('a', [1]), [PLAIN, style], theme).lines.map((line) => line.kind)

    expect(kinds.sort()).toEqual(['overline', 'single'])
  })
})

describe('a wide character', () => {
  it('is one character over two cells', () => {
    const wide: Row = { ...row(0, '日a'), column: Uint16Array.from([0, 2]), width: Uint8Array.from([2, 1]) }
    const drawing = drawRow(wide, [PLAIN], theme)

    expect(drawing.glyphs.map(({ text, column, cells }) => ({ text, column, cells }))).toEqual([
      { text: '日', column: 0, cells: 2 },
      { text: 'a', column: 2, cells: 1 },
    ])
  })
})

describe('a selection', () => {
  it('replaces the background of what it covers and leaves each character its colour', () => {
    const drawing = drawRow(styled('abcd', [1, 1, 0, 0]), [PLAIN, { ...PLAIN, fg: RED, bg: BLUE }], theme, { from: 1, to: 3 })

    expect(drawing.backgrounds).toEqual([
      { column: 0, cells: 1, color: BLUE },
      { column: 1, cells: 2, color: theme.selectionBackground },
    ])
    expect(drawing.glyphs.map((glyph) => glyph.color)).toEqual([RED, RED, theme.foreground, theme.foreground])
  })

  it('recolours the text when the theme gives a colour for it', () => {
    const drawing = drawRow(row(0, 'ab'), [PLAIN], { ...theme, selectionForeground: 0xffffff }, { from: 0, to: 1 })

    expect(drawing.glyphs.map((glyph) => glyph.color)).toEqual([0xffffff, theme.foreground])
  })

  it('covers the blank cells past the end of the line', () => {
    const drawing = drawRow(row(0, 'ab'), [PLAIN], theme, { from: 1, to: 6 })

    expect(drawing.backgrounds).toEqual([{ column: 1, cells: 5, color: theme.selectionBackground }])
  })

  it('covers a wide character it only half reaches', () => {
    const wide: Row = { ...row(0, '日'), column: Uint16Array.from([0]), width: Uint8Array.from([2]) }

    expect(drawRow(wide, [PLAIN], theme, { from: 1, to: 2 }).backgrounds[0]).toMatchObject({ column: 0, cells: 2 })
  })
})
