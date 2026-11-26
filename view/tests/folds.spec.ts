// Folded line runs: header row rendering, cached header drawings, cursor clamping, epoch reset, and scrolling.
import { beforeEach, describe, expect, it } from 'vitest'
import { installFrames } from './_frames.ts'
import { frame, row } from './_support.ts'
import { mountedView } from './_view.ts'

beforeEach(() => installFrames())

describe('folded line runs', () => {
  it('draws a single header row for folded lines, hides folded rows, and caches fold RowDrawing identity', () => {
    const { source, view, painted } = mountedView()
    source.send(
      frame({
        size: { cols: 30, rows: 4 },
        rows: [row(0, 'cmd'), row(1, 'out-1'), row(2, 'out-2'), row(3, 'after')],
      }),
    )
    view.setFolds([{ first: 0, last: 2, label: 'build output' }])
    const scene1 = painted()
    const headerText = scene1.rows[0]!.glyphs.map((g) => g.text).join('')
    expect(headerText).toContain('build output')
    // Second drawn row is line 3 ('after'), skipping hidden lines 1 and 2
    expect(scene1.rows[1]!.glyphs.map((g) => g.text).join('')).toBe('after')

    // Repaint preserves exact RowDrawing object identity for unchanged fold header
    view.redraw()
    const scene2 = painted()
    expect(scene2.rows[0]).toBe(scene1.rows[0])

    // Overlapping folds throw RangeError
    expect(() => view.setFolds([{ first: 0, last: 3 }, { first: 2, last: 4 }])).toThrow(RangeError)

    // Epoch reset clears folds cleanly
    source.send(frame({ seq: 2, epoch: 2, size: { cols: 30, rows: 4 }, rows: [row(0, 'fresh')] }))
    const scene3 = painted()
    expect(scene3.rows[0]!.glyphs.map((g) => g.text).join('')).toBe('fresh')
  })

  it('positions input field at fold header when cursor is inside folded lines and copies full folded text', async () => {
    const copied: string[] = []
    const { source, view, painted, field } = mountedView({ onCopy: (t) => copied.push(t) })
    source.send(
      frame({
        size: { cols: 20, rows: 4 },
        cursor: { col: 2, row: 1, shape: 'block', blinking: false, hidden: false },
        rows: [row(0, 'head'), row(1, 'mid'), row(2, 'tail'), row(3, 'next')],
      }),
    )
    view.setFolds([{ first: 0, last: 2 }])
    painted()
    // Cursor on hidden line 1 places IME textarea at fold header row 0 (top = 0px)
    expect(field.style.top).toBe('0px')

    // Selecting all copies the actual folded lines too!
    view.selectAll()
    expect(await view.copy()).toBe(true)
    expect(copied).toEqual(['head\nmid\ntail\nnext'])
  })

  it('uses default count label when label is omitted and steps over folded lines when scrolling', () => {
    const { source, view, painted } = mountedView()
    source.send(
      frame({
        size: { cols: 30, rows: 3 },
        firstActiveLine: 6,
        scrollbackLines: 6,
        rows: [row(6, 's0'), row(7, 's1'), row(8, 's2')],
      }),
    )
    for (let i = 0; i < 6; i += 1) source.history.set(i, row(i, `h${i}`))
    view.store.absorb(1, 0, 6, Array.from({ length: 6 }, (_, i) => row(i, `h${i}`)))
    view.setFolds([{ first: 1, last: 4 }])
    view.scrollTo(0)
    const scene = painted()
    expect(scene.rows[0]!.glyphs.map((g) => g.text).join('')).toBe('h0')
    expect(scene.rows[1]!.glyphs.map((g) => g.text).join('')).toContain('3 lines folded')
    expect(scene.rows[2]!.glyphs.map((g) => g.text).join('')).toBe('h5')
  })
})
