// What a character is drawn in against what it sits on. The contrast ratio is WCAG 2.x's.
import { describe, expect, it } from 'vitest'
import { contrastRatio, ensureContrast, luminance } from '../src/contrast.ts'
import { drawRow, type Theme } from '../src/draw.ts'
import type { StyleSpec } from '../src/source.ts'
import { PALETTE, packedPalette, packedRgb, row } from './_support.ts'

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

  it('is moved to the nearest colour that reaches the minimum, not one further along', () => {
    const along = (foreground: number, target: number, amount: number) => {
      const mix = (shift: number) => Math.round(((foreground >> shift) & 0xff) * (1 - amount) + ((target >> shift) & 0xff) * amount)
      return (mix(16) << 16) | (mix(8) << 8) | mix(0)
    }
    const nearest = (foreground: number, background: number, minimum: number) => {
      const target = contrastRatio(WHITE, background) >= contrastRatio(BLACK, background) ? WHITE : BLACK
      for (let step = 0; step <= 1000; step += 1) {
        const colour = along(foreground, target, step / 1000)
        if (contrastRatio(colour, background) >= minimum) return { colour, amount: step / 1000, target }
      }
      return null
    }
    for (const minimum of [2, 3, 4.5, 7]) {
      for (const background of [BLACK, WHITE, 0x3366cc, 0x808080]) {
        for (const foreground of [0x000000, 0x111111, 0x777777, 0xdddddd, 0x3366cc]) {
          const label = `${foreground.toString(16)} on ${background.toString(16)} at ${minimum}`
          const drawn = ensureContrast(foreground, background, minimum)
          if (contrastRatio(foreground, background) >= minimum) {
            expect(drawn, label).toBe(foreground)
            continue
          }
          const reach = nearest(foreground, background, minimum)
          if (reach === null) {
            const best = Math.max(contrastRatio(BLACK, background), contrastRatio(WHITE, background))
            expect(contrastRatio(drawn, background), label).toBeCloseTo(best, 2)
            continue
          }
          expect(contrastRatio(drawn, background), label).toBeGreaterThanOrEqual(minimum)
          for (const shift of [16, 8, 0]) {
            expect(Math.abs(((drawn >> shift) & 0xff) - ((reach.colour >> shift) & 0xff)), label).toBeLessThanOrEqual(1)
          }
          if (reach.amount >= 0.01) {
            expect(contrastRatio(along(foreground, reach.target, reach.amount - 0.01), background), label).toBeLessThan(minimum)
          }
        }
      }
    }
  })

  it('goes to black or white when nothing nearer is far enough', () => {
    expect(ensureContrast(0x111111, BLACK, 22)).toBe(WHITE)
    expect(ensureContrast(0xeeeeee, WHITE, 22)).toBe(BLACK)
  })

  it('is the same answer each time it is asked for, whether cached or not', () => {
    const first = ensureContrast(0x2a2a2a, BLACK, 4.5)
    expect(ensureContrast(0x2a2a2a, BLACK, 4.5)).toBe(first)
  })
})

describe('a character drawn with a minimum contrast', () => {
  const background = 0x1e1e1e
  const theme: Theme = {
    background,
    foreground: WHITE,
    palette: PALETTE,
    cursor: WHITE,
    cursorText: background,
    selectionBackground: 0x264f78,
    selectionForeground: null,
  }
  const darkRed: StyleSpec = { fg: packedPalette(1) }

  it('is drawn at the minimum where the program chose a colour too close to its background', () => {
    const glyph = drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), { ...theme, minimumContrast: 4.5 }).glyphs[0]!
    expect(contrastRatio(glyph.color, background)).toBeGreaterThanOrEqual(4.5)
    expect(glyph.color).not.toBe(PALETTE[1])
  })

  it('is drawn as the program chose when no minimum is set, or the minimum is 1', () => {
    expect(drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), theme).glyphs[0]!.color).toBe(PALETTE[1])
    expect(drawRow(row(0, 'a', { style: 1, styles: [darkRed] }), { ...theme, minimumContrast: 1 }).glyphs[0]!.color).toBe(PALETTE[1])
  })

  it('is measured against the search match it is drawn over, where a search found it', () => {
    const grey: StyleSpec = { fg: packedRgb(0x7a7a7a) }
    const black = { ...theme, background: BLACK, minimumContrast: 4.5 }
    const current = 0x9e6a03
    expect(drawRow(row(0, 'a', { style: 1, styles: [grey] }), black).glyphs[0]!.color).toBe(0x7a7a7a)
    const found = drawRow(row(0, 'a', { style: 1, styles: [grey] }), black, undefined, false, [{ from: 0, to: 1, color: current }])
    expect(contrastRatio(found.glyphs[0]!.color, current)).toBeGreaterThanOrEqual(4.5)
  })

  it('is measured against the selection where the character is selected', () => {
    const selected = { from: 0, to: 1 }
    const glyph = drawRow(
      row(0, 'a', { style: 1, styles: [darkRed] }),
      { ...theme, minimumContrast: 4.5, selectionBackground: 0xffffff },
      selected,
    ).glyphs[0]!
    expect(contrastRatio(glyph.color, 0xffffff)).toBeGreaterThanOrEqual(4.5)
  })
})
