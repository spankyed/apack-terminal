import { describe, expect, it } from 'vitest'
import {
  PackedStyleFlag,
  RunFlag,
  blinks,
  drawRow,
  packRow,
  rowPlainText,
  sessionTheme,
  type Theme,
} from '../src/index.ts'
import { PALETTE, packedPalette, packedRgb, row } from './_support.ts'

const theme: Theme = {
  background: 0x1e1e1e,
  foreground: 0xd4d4d4,
  palette: PALETTE,
  cursor: 0xffffff,
  cursorText: 0x000000,
  selectionBackground: 0x264f78,
  selectionForeground: 0xffffff,
}

describe('drawRow & sessionTheme (v2 PackedRow)', () => {
  it('batches contiguous printable ASCII runs and splits cleanly at selection boundaries', () => {
    const r = row(0, 'hello world')
    const plain = drawRow(r, theme)
    expect(plain.glyphs).toHaveLength(1)
    expect(plain.glyphs[0]).toMatchObject({
      text: 'hello world',
      column: 0,
      cells: 11,
      batchable: true,
      color: theme.foreground,
    })

    // Selection over "lo wo" (cols 3..8) splits the ASCII run into 3 batchable pieces
    const sel = drawRow(r, theme, { from: 3, to: 8 })
    expect(sel.glyphs.map((g) => [g.text, g.column, g.cells, g.color, g.batchable])).toEqual([
      ['hel', 0, 3, theme.foreground, true],
      ['lo wo', 3, 5, theme.selectionForeground, true],
      ['rld', 8, 3, theme.foreground, true],
    ])
    expect(sel.backgrounds).toEqual([{ column: 3, cells: 5, color: theme.selectionBackground }])
  })

  it('applies inverse, faint, invisible, blink, underlines, and strikethrough from PackedStyleFlag', () => {
    const r = packRow({
      line: 0,
      styles: [
        {},
        { fg: packedPalette(1), bg: packedRgb(0x112233), flags: PackedStyleFlag.INVERSE | PackedStyleFlag.BOLD },
        { flags: PackedStyleFlag.FAINT | PackedStyleFlag.ITALIC | (1 << 2) | PackedStyleFlag.STRIKETHROUGH, underlineColor: packedRgb(0xff0000) },
        { flags: PackedStyleFlag.INVISIBLE, bg: packedRgb(0x334455) },
        { flags: PackedStyleFlag.BLINK },
      ],
      runs: [
        { colStart: 0, colLen: 2, text: 'AB', styleIdx: 1 },
        { colStart: 2, colLen: 2, text: 'CD', styleIdx: 2 },
        { colStart: 4, colLen: 1, text: 'X', styleIdx: 3 },
        { colStart: 5, colLen: 1, text: '!', styleIdx: 4 },
      ],
    })

    expect(blinks(r)).toBe(true)
    expect(rowPlainText(r)).toBe('ABCDX!')

    const shown = drawRow(r, theme, undefined, false)
    expect(shown.glyphs.map((g) => g.text)).toEqual(['AB', 'CD', '!'])
    expect(shown.glyphs[0]).toMatchObject({ color: 0x112233, bold: true, italic: false })
    expect(shown.glyphs[1]).toMatchObject({ alpha: 0.5, bold: false, italic: true })
    expect(shown.backgrounds).toEqual([
      { column: 0, cells: 2, color: PALETTE[1] },
      { column: 4, cells: 1, color: 0x334455 },
    ])
    expect(shown.lines).toEqual([
      { kind: 'single', column: 2, cells: 2, color: 0xff0000 },
      { kind: 'strikethrough', column: 2, cells: 2, color: theme.foreground },
    ])

    // Blinked off hides '!'
    const hidden = drawRow(r, theme, undefined, true)
    expect(hidden.glyphs.map((g) => g.text)).toEqual(['AB', 'CD'])
  })

  it('handles wide PER_CELL grapheme clusters without splitting and narrow non-BMP runs without breaking surrogate pairs', () => {
    const r = packRow({
      line: 0,
      styles: [{}],
      runs: [
        { colStart: 0, colLen: 2, text: '👨‍👩‍👧‍👦', flags: RunFlag.PER_CELL },
        { colStart: 2, colLen: 1, text: 'e\u0301', flags: RunFlag.PER_CELL },
        { colStart: 3, colLen: 2, text: '𝕏𝕐', flags: 0 },
      ],
    })
    const drawn = drawRow(r, theme, { from: 1, to: 2 })
    // Family emoji stays ONE 2-cell glyph, combining accent stays ONE 1-cell glyph, and 𝕏/𝕐 stay intact surrogate pairs!
    expect(drawn.glyphs.map((g) => [g.text, g.column, g.cells, g.batchable])).toEqual([
      ['👨‍👩‍👧‍👦', 0, 2, false],
      ['e\u0301', 2, 1, false],
      ['𝕏', 3, 1, false],
      ['𝕐', 4, 1, false],
    ])
    expect(drawn.backgrounds).toEqual([{ column: 0, cells: 2, color: theme.selectionBackground }])
  })

  it('merges session palette overrides and swaps defaults on reverse video', () => {
    expect(sessionTheme(theme, null, false)).toBe(theme)
    const rev = sessionTheme(theme, null, true)
    expect(rev.foreground).toBe(theme.background)
    expect(rev.background).toBe(theme.foreground)

    const custom = sessionTheme(
      theme,
      { fg: 0x111111, bg: 0xeeeeee, cursor: 0x00ff00, palette: [{ index: 2, rgb: 0xabcdef }] },
      true,
    )
    expect(custom.foreground).toBe(0xeeeeee)
    expect(custom.background).toBe(0x111111)
    expect(custom.cursor).toBe(0x00ff00)
    expect(custom.palette[2]).toBe(0xabcdef)
  })
})
