import { describe, expect, it } from 'vitest'
import { CanvasPainter, drawRow, type Scene, type Theme } from '../src/index.ts'
import { recordingContext } from './_canvas.ts'
import { PALETTE, row } from './_support.ts'

const theme: Theme = {
  background: 0x1e1e1e,
  foreground: 0xd4d4d4,
  palette: PALETTE,
  cursor: 0xffffff,
  cursorText: 0x000000,
  selectionBackground: 0x264f78,
  selectionForeground: null,
}

describe('CanvasPainter v2', () => {
  it('batches printable ASCII runs into one fillText when font-advance probe passes, and falls back per-cell when proportional', () => {
    // 1. Uniform monospace font (measureText('MMMM') === 4 * cellWidth) -> batched!
    const mono = recordingContext(10)
    const painter = new CanvasPainter({ family: 'monospace', size: 14 }, mono.canvas, 1)
    painter.resize(20, 2, 1)
    expect(painter.canBatchFace(false, false)).toBe(true)

    const r0 = drawRow(row(0, 'hello world'), theme)
    painter.paint({ theme, rows: [r0, null], offset: 0 })
    const texts = mono.drawn.filter((d) => d.kind === 'text')
    expect(texts).toHaveLength(1)
    expect(texts[0]).toMatchObject({ text: 'hello world', x: 0, y: 16 })
    // Zero whole-row clips!
    expect(texts[0]!.clips).toEqual([])
    painter.dispose()

    // 2. Proportional font where 'iiii' measures narrower than 4 * cellWidth -> per-cell fallback!
    const prop = recordingContext(10)
    const ctx = prop.canvas.getContext('2d')!
    ctx.measureText = ((text: string) => ({
      width: text === 'iiii' ? 20 : text.length * 10,
      fontBoundingBoxAscent: 16,
      fontBoundingBoxDescent: 4,
    })) as CanvasRenderingContext2D['measureText']

    const propPainter = new CanvasPainter({ family: 'proportional', size: 14 }, prop.canvas, 1)
    propPainter.resize(20, 2, 1)
    expect(propPainter.canBatchFace(false, false)).toBe(false)
    propPainter.paint({ theme, rows: [r0, null], offset: 0 })
    const propTexts = prop.drawn.filter((d) => d.kind === 'text')
    expect(propTexts.map((d) => [d.text, d.x])).toEqual([
      ['h', 0], ['e', 10], ['l', 20], ['l', 30], ['o', 40],
      ['w', 60], ['o', 70], ['r', 80], ['l', 90], ['d', 100],
    ])
    propPainter.dispose()
  })

  it('clips ONLY individual overhanging glyphs and never wraps entire rows in save/clip/restore', () => {
    const rec = recordingContext(10)
    const ctx = rec.canvas.getContext('2d')!
    ctx.measureText = ((text: string) => ({
      width: text === '⚡' ? 25 : text.length * 10,
      fontBoundingBoxAscent: 16,
      fontBoundingBoxDescent: 4,
    })) as CanvasRenderingContext2D['measureText']

    const painter = new CanvasPainter({ family: 'monospace', size: 14 }, rec.canvas, 1)
    painter.resize(10, 2, 1)
    const r0 = drawRow(row(0, 'a⚡b'), theme)
    painter.paint({ theme, rows: [r0, null], offset: 0 })

    const texts = rec.drawn.filter((d) => d.kind === 'text')
    const normalA = texts.find((t) => t.text === 'a')!
    const wideBolt = texts.find((t) => t.text === '⚡')!
    const normalB = texts.find((t) => t.text === 'b')!
    expect(normalA.clips).toEqual([])
    expect(wideBolt.clips.length).toBe(1)
    expect(normalB.clips).toEqual([])
    painter.dispose()
  })

  it('uses O(1) drawImage scroll-blit fast path on uniform scroll shifts and repaints only newly exposed edge rows', () => {
    const rec = recordingContext(10)
    const painter = new CanvasPainter({ family: 'monospace', size: 14 }, rec.canvas, 1)
    painter.resize(10, 6, 1)

    const rows = Array.from({ length: 7 }, (_, i) => drawRow(row(i, `line-${i}`), theme))
    const scene1: Scene = { theme, rows: rows.slice(0, 6), offset: 0, scrollPosition: 0 }
    painter.paint(scene1)
    rec.drawn.length = 0

    // Scroll down by 1 row (scrollPosition: 0 -> 1)
    const scene2: Scene = { theme, rows: rows.slice(1, 7), offset: 0, scrollPosition: 1 }
    painter.paint(scene2)

    // Must blit self canvas once with srcY=20, dstY=0, height=100 (6*20 - 20)
    const blits = rec.drawn.filter((d) => d.kind === 'image' && d.picture === rec.canvas)
    expect(blits).toHaveLength(1)
    expect(blits[0]!.source).toEqual([0, 20, 100, 100])
    expect(blits[0]!.y).toBe(0)
    expect(blits[0]!.height).toBe(100)

    // Top unchanged lines (line-1, line-2) must NOT be redrawn!
    const redrawnTexts = rec.drawn.filter((d) => d.kind === 'text').map((d) => d.text)
    expect(redrawnTexts).toContain('line-6')
    expect(redrawnTexts).not.toContain('line-1')
    expect(redrawnTexts).not.toContain('line-2')

    // Fractional scroll shift (+0.5 row = 10px) where a middle row mutated MUST repaint the dirty row!
    rec.drawn.length = 0
    const mutatedMid = [...rows.slice(1, 7), drawRow(row(7, 'line-7'), theme)]
    mutatedMid[2] = drawRow(row(3, 'mid-updated'), theme)
    painter.paint({ theme, rows: mutatedMid, offset: 0.5, scrollPosition: 1.5 })
    const fracTexts = rec.drawn.filter((d) => d.kind === 'text').map((d) => d.text)
    expect(fracTexts).toContain('mid-updated')
    painter.dispose()
  })

  it('preserves colour and glyph-width caches across grid resizes when font and DPR are unchanged', () => {
    const rec = recordingContext(10)
    let measureCalls = 0
    const ctx = rec.canvas.getContext('2d')!
    const origMeasure = ctx.measureText
    ctx.measureText = ((text: string) => {
      if (text === 'é') measureCalls += 1
      return origMeasure(text)
    }) as CanvasRenderingContext2D['measureText']

    const painter = new CanvasPainter({ family: 'monospace', size: 14 }, rec.canvas, 1)
    painter.resize(10, 2, 1)
    const r0 = drawRow(row(0, 'é'), theme)
    painter.paint({ theme, rows: [r0, null], offset: 0 })
    expect(measureCalls).toBe(1)

    // Resize grid columns/rows with SAME DPR & font -> wider cache preserved!
    painter.resize(20, 4, 1)
    painter.paint({ theme, rows: [r0, null, null, null], offset: 0 })
    expect(measureCalls).toBe(1)

    // Changing DPR invalidates wider cache
    painter.resize(20, 4, 2)
    painter.paint({ theme, rows: [r0, null, null, null], offset: 0 })
    expect(measureCalls).toBe(2)
    painter.dispose()
  })

  it('repaints dirty bands only when a single row changes and fires whenCleared on contextrestored', () => {
    const rec = recordingContext(10)
    const painter = new CanvasPainter({ family: 'monospace', size: 14 }, rec.canvas, 1)
    painter.resize(10, 6, 1)

    const r = Array.from({ length: 6 }, (_, i) => drawRow(row(i, `r${i}`), theme))
    painter.paint({ theme, rows: r, offset: 0 })
    rec.drawn.length = 0

    // Mutate only row 5
    const next = [...r]
    next[5] = drawRow(row(5, 'changed'), theme)
    painter.paint({ theme, rows: next, offset: 0 })

    const drawnTexts = rec.drawn.filter((d) => d.kind === 'text').map((d) => d.text)
    expect(drawnTexts).toContain('changed')
    expect(drawnTexts).not.toContain('r0')
    expect(drawnTexts).not.toContain('r1')

    let cleared = 0
    painter.whenCleared(() => { cleared += 1 })
    rec.canvas.dispatchEvent(new Event('contextrestored'))
    expect(cleared).toBe(1)
    painter.dispose()
  })
})
