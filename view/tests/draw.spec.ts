// A row as things to draw: the rules a terminal's styles follow, and what a selection does to them.
import { describe, expect, it } from 'vitest'
import { drawRow, type SelectedCells, type Theme } from '../src/draw.ts'
import { xtermPalette } from '../src/palette.ts'
import { PLAIN, StyleFlag, paletteColor, type Row, type Style } from '../src/source.ts'
import { row, PALETTE } from './_support.ts'

/** A row drawn with `styles` as its own: the plain one, then whatever its characters name */
const drawStyled = (row: Row, styles: Style[], theme: Theme, selected?: SelectedCells) => drawRow({ ...row, styles }, theme, selected)

const theme: Theme = {
  background: 0x1e1e1e, palette: PALETTE,
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
    const drawing = drawStyled(row(0, 'ab'), [PLAIN], theme)

    expect(drawing.backgrounds).toEqual([])
    expect(drawing.glyphs.map(({ text, column, color }) => ({ text, column, color }))).toEqual([
      { text: 'a', column: 0, color: theme.foreground },
      { text: 'b', column: 1, color: theme.foreground },
    ])
  })

  it('draws nothing for a space', () => {
    expect(drawStyled(row(0, 'a b'), [PLAIN], theme).glyphs.map((glyph) => glyph.text)).toEqual(['a', 'b'])
  })
})

describe('a background', () => {
  it('is one rectangle for a run of cells that share it', () => {
    const onRed: Style = { ...PLAIN, bg: RED }
    const onBlue: Style = { ...PLAIN, bg: BLUE }
    const drawing = drawStyled(styled('aabbc', [1, 1, 2, 2, 0]), [PLAIN, onRed, onBlue], theme)

    expect(drawing.backgrounds).toEqual([
      { column: 0, cells: 2, color: RED },
      { column: 2, cells: 2, color: BLUE },
    ])
  })

  it('is drawn under a space, which has no character but still has a colour', () => {
    const drawing = drawStyled(styled(' ', [1]), [PLAIN, { ...PLAIN, bg: RED }], theme)

    expect(drawing.backgrounds).toEqual([{ column: 0, cells: 1, color: RED }])
    expect(drawing.glyphs).toEqual([])
  })
})

describe('inverse', () => {
  it('swaps the two colours, defaults included', () => {
    const drawing = drawStyled(styled('a', [1]), [PLAIN, { ...PLAIN, flags: StyleFlag.inverse }], theme)

    expect(drawing.backgrounds).toEqual([{ column: 0, cells: 1, color: theme.foreground }])
    expect(drawing.glyphs[0]?.color).toBe(theme.background)
  })

  it('swaps chosen colours too', () => {
    const style: Style = { ...PLAIN, fg: RED, bg: BLUE, flags: StyleFlag.inverse }
    const drawing = drawStyled(styled('a', [1]), [PLAIN, style], theme)

    expect(drawing.backgrounds[0]?.color).toBe(RED)
    expect(drawing.glyphs[0]?.color).toBe(BLUE)
  })
})

describe('text attributes', () => {
  it('reach the character: bold, italic, and faint as half strength', () => {
    const style: Style = { ...PLAIN, flags: StyleFlag.bold | StyleFlag.italic | StyleFlag.faint }
    const [glyph] = drawStyled(styled('a', [1]), [PLAIN, style], theme).glyphs

    expect(glyph).toMatchObject({ bold: true, italic: true, alpha: 0.5 })
  })

  it('invisible text keeps its background and draws neither character nor line', () => {
    const style: Style = { ...PLAIN, bg: RED, flags: StyleFlag.invisible | StyleFlag.strikethrough | (1 << 8) }
    const drawing = drawStyled(styled('a', [1]), [PLAIN, style], theme)

    expect(drawing.backgrounds).toHaveLength(1)
    expect(drawing.glyphs).toEqual([])
    expect(drawing.lines).toEqual([])
  })

  it('text that blinks is drawn like any other, and for the half of a blink it is off keeps its background and draws nothing else', () => {
    const blinking: Style = { ...PLAIN, bg: RED, flags: StyleFlag.blink | StyleFlag.strikethrough }
    const both = { ...styled('ab', [1, 0]), styles: [PLAIN, blinking] }

    const on = drawRow(both, theme)
    expect(on.glyphs.map((glyph) => glyph.text)).toEqual(['a', 'b'])
    expect(on.lines).toHaveLength(1)

    const off = drawRow(both, theme, undefined, true)
    expect(off.glyphs.map((glyph) => glyph.text), 'the text beside it does not blink').toEqual(['b'])
    expect(off.lines).toEqual([])
    expect(off.backgrounds).toEqual(on.backgrounds)
  })
})

describe('lines through and under text', () => {
  it('are one line per run, in the underline colour where one was given and the text colour where not', () => {
    const curly: Style = { ...PLAIN, underlineColor: RED, flags: 3 << 8 }
    const struck: Style = { ...PLAIN, fg: BLUE, flags: StyleFlag.strikethrough }
    const drawing = drawStyled(styled('aabb', [1, 1, 2, 2]), [PLAIN, curly, struck], theme)

    expect(drawing.lines).toEqual([
      { kind: 'curly', column: 0, cells: 2, color: RED },
      { kind: 'strikethrough', column: 2, cells: 2, color: BLUE },
    ])
  })

  it('an overline and an underline on the same cells are both drawn', () => {
    const style: Style = { ...PLAIN, flags: StyleFlag.overline | (1 << 8) }
    const kinds = drawStyled(styled('a', [1]), [PLAIN, style], theme).lines.map((line) => line.kind)

    expect(kinds.sort()).toEqual(['overline', 'single'])
  })
})

describe('a wide character', () => {
  it('is one character over two cells', () => {
    const wide: Row = { ...row(0, '日a'), column: Uint16Array.from([0, 2]), width: Uint8Array.from([2, 1]) }
    const drawing = drawStyled(wide, [PLAIN], theme)

    expect(drawing.glyphs.map(({ text, column, cells }) => ({ text, column, cells }))).toEqual([
      { text: '日', column: 0, cells: 2 },
      { text: 'a', column: 2, cells: 1 },
    ])
  })
})

describe('a selection', () => {
  it('replaces the background of what it covers and leaves each character its colour', () => {
    const drawing = drawStyled(styled('abcd', [1, 1, 0, 0]), [PLAIN, { ...PLAIN, fg: RED, bg: BLUE }], theme, { from: 1, to: 3 })

    expect(drawing.backgrounds).toEqual([
      { column: 0, cells: 1, color: BLUE },
      { column: 1, cells: 2, color: theme.selectionBackground },
    ])
    expect(drawing.glyphs.map((glyph) => glyph.color)).toEqual([RED, RED, theme.foreground, theme.foreground])
  })

  it('recolours the text when the theme gives a colour for it', () => {
    const drawing = drawStyled(row(0, 'ab'), [PLAIN], { ...theme, selectionForeground: 0xffffff }, { from: 0, to: 1 })

    expect(drawing.glyphs.map((glyph) => glyph.color)).toEqual([0xffffff, theme.foreground])
  })

  it('covers the blank cells past the end of the line', () => {
    const drawing = drawStyled(row(0, 'ab'), [PLAIN], theme, { from: 1, to: 6 })

    expect(drawing.backgrounds).toEqual([{ column: 1, cells: 5, color: theme.selectionBackground }])
  })

  it('covers the cells nothing was written in, between characters as well as after them', () => {
    // `a`, six cells a program moved the cursor over without writing, then `b`
    const gapped: Row = { ...row(0, 'ab'), column: Uint16Array.from([0, 7]) }
    const drawing = drawStyled(gapped, [PLAIN], theme, { from: 0, to: 12 })

    expect(drawing.backgrounds).toEqual([{ column: 0, cells: 12, color: theme.selectionBackground }])
  })

  it('is one rectangle among the backgrounds either side of it, in the order of their cells', () => {
    const drawing = drawStyled(styled('abcdef', [1, 1, 1, 1, 1, 1]), [PLAIN, { ...PLAIN, bg: BLUE }], theme, { from: 2, to: 4 })

    expect(drawing.backgrounds).toEqual([
      { column: 0, cells: 2, color: BLUE },
      { column: 2, cells: 2, color: theme.selectionBackground },
      { column: 4, cells: 2, color: BLUE },
    ])
  })

  it('covers a wide character it only half reaches', () => {
    const wide: Row = { ...row(0, '日'), column: Uint16Array.from([0]), width: Uint8Array.from([2]) }

    expect(drawStyled(wide, [PLAIN], theme, { from: 1, to: 2 }).backgrounds[0]).toMatchObject({ column: 0, cells: 2 })
  })
})

describe('a numbered colour', () => {
  it('is looked up in the theme when the row is drawn, so a theme decides what red is', () => {
    const styles: Style[] = [PLAIN, { fg: paletteColor(1), bg: paletteColor(4), underlineColor: paletteColor(200), flags: 1 << 8 }]
    const drawing = drawStyled(row(0, 'a', { style: 1 }), styles, theme)
    expect(drawing.glyphs[0]!.color).toBe(PALETTE[1])
    expect(drawing.backgrounds[0]!.color).toBe(PALETTE[4])
    expect(drawing.lines[0]!.color).toBe(PALETTE[200])

    const other = { ...theme, palette: PALETTE.map((color) => color ^ 0xffffff) }
    expect(drawStyled(row(0, 'a', { style: 1 }), styles, other).glyphs[0]!.color).toBe(PALETTE[1]! ^ 0xffffff)
  })

  it('keeps a colour a program gave exactly, whatever the theme', () => {
    const styles: Style[] = [PLAIN, { fg: 0x010203, bg: null, underlineColor: null, flags: 0 }]
    expect(drawStyled(row(0, 'a', { style: 1 }), styles, theme).glyphs[0]!.color).toBe(0x010203)
  })
})

describe('the 256 colours', () => {
  it('are the theme\'s sixteen, then the cube and the greys every terminal shares', () => {
    const palette = xtermPalette([0x111111, 0x222222])
    expect(palette).toHaveLength(256)
    expect(palette.slice(0, 3)).toEqual([0x111111, 0x222222, 0])
    expect([palette[16], palette[21], palette[196], palette[231]]).toEqual([0x000000, 0x0000ff, 0xff0000, 0xffffff])
    expect([palette[17], palette[232], palette[255]]).toEqual([0x00005f, 0x080808, 0xeeeeee])
  })
})
