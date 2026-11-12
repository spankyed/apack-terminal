// What a character is drawn in against what it sits on. The contrast ratio is WCAG 2.x's, so its values are the ones a
// published checker gives for the same colours; a colour the program chose is kept whenever it is already far enough.
import { describe, expect, it } from 'vitest'
import { contrastRatio, ensureContrast, luminance } from '../src/contrast.ts'
import { drawRow, type Theme } from '../src/draw.ts'
import { PLAIN, paletteColor, type Style } from '../src/source.ts'
import { PALETTE, row } from './_support.ts'

const WHITE = 0xffffff
const BLACK = 0x000000

describe('the contrast ratio of two colours', () => {
  it('is 21 for black on white, and 1 for a colour on itself', () => {
    expect(contrastRatio(BLACK, WHITE)).toBeCloseTo(21, 5)
    expect(contrastRatio(0x3366cc, 0x3366cc)).toBeCloseTo(1, 5)
  })

  it('is the same whichever way round the two colours are given', () => {
    expect(contrastRatio(0x777777, WHITE)).toBeCloseTo(contrastRatio(WHITE, 0x777777), 10)
  })

  it('is the value a published checker gives for mid grey on white, 4.48', () => {
    expect(contrastRatio(0x777777, WHITE)).toBeCloseTo(4.48, 2)
  })

  it('has white at the top of the luminance scale and black at the bottom', () => {
    expect(luminance(WHITE)).toBeCloseTo(1, 10)
    expect(luminance(BLACK)).toBeCloseTo(0, 10)
  })
})

describe('the colour a character is drawn in', () => {
  it('is the one the program chose when that is far enough', () => {
    expect(ensureContrast(0xdddddd, BLACK, 4.5)).toBe(0xdddddd)
    expect(ensureContrast(0x123456, WHITE, 1)).toBe(0x123456)
  })

  it('is moved until it reaches the minimum, and no further than it has to go', () => {
    for (const minimum of [2, 3, 4.5, 7]) {
      for (const background of [BLACK, WHITE, 0x3366cc, 0x808080]) {
        for (const foreground of [0x000000, 0x111111, 0x777777, 0xdddddd, 0x3366cc]) {
          const drawn = ensureContrast(foreground, background, minimum)
          const best = Math.max(contrastRatio(BLACK, background), contrastRatio(WHITE, background))
          // Where the minimum is reachable it is met; where it is not, the most contrast black or white can give is
          if (best >= minimum) expect(contrastRatio(drawn, background), `${foreground.toString(16)} on ${background.toString(16)} at ${minimum}`).toBeGreaterThanOrEqual(minimum - 0.01)
          else expect(contrastRatio(drawn, background)).toBeCloseTo(best, 2)
        }
      }
    }
  })

  it('goes to black or white when nothing nearer is far enough, which is the most a colour can do', () => {
    expect(ensureContrast(0x111111, BLACK, 22)).toBe(WHITE)
    expect(ensureContrast(0xeeeeee, WHITE, 22)).toBe(BLACK)
  })

  it('is the same answer each time it is asked for, whether it was cached or not', () => {
    const first = ensureContrast(0x2a2a2a, BLACK, 4.5)
    expect(ensureContrast(0x2a2a2a, BLACK, 4.5)).toBe(first)
  })
})

describe('a character drawn with a minimum contrast', () => {
  const background = 0x1e1e1e
  const theme: Theme = { background, foreground: WHITE, palette: PALETTE, cursor: WHITE, cursorText: background, selectionBackground: 0x264f78, selectionForeground: null }
  // Palette 1 is the program's own dark red: about 2:1 on the theme's background, too close to read
  const darkRed: Style = { ...PLAIN, fg: paletteColor(1), bg: null }

  it('is drawn at the minimum where the program chose a colour too close to its background', () => {
    const glyph = drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), { ...theme, minimumContrast: 4.5 }).glyphs[0]!
    expect(contrastRatio(glyph.color, background)).toBeGreaterThanOrEqual(4.5)
    expect(glyph.color).not.toBe(PALETTE[1])
  })

  it('is drawn as the program chose when no minimum is set, or the minimum is 1', () => {
    expect(drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), theme).glyphs[0]!.color).toBe(PALETTE[1])
    expect(drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), { ...theme, minimumContrast: 1 }).glyphs[0]!.color).toBe(PALETTE[1])
  })

  it('is measured against the selection where the character is selected', () => {
    const selected = { from: 0, to: 1 }
    const glyph = drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), { ...theme, minimumContrast: 4.5, selectionBackground: 0xffffff }, selected).glyphs[0]!
    expect(contrastRatio(glyph.color, 0xffffff)).toBeGreaterThanOrEqual(4.5)
  })
})
