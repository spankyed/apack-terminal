// What a view keeps of what it has been sent, and what it knows to ask for.
import { describe, expect, it } from 'vitest'
import { KEPT_LINES, KEPT_SLACK, RowStore } from '../src/row-store.ts'
import { PLAIN, StyleFlag, type Fetched, type Style } from '../src/source.ts'
import { frame, row } from './_support.ts'

const bold: Style = { ...PLAIN, flags: StyleFlag.bold }
const red: Style = { ...PLAIN, fg: 0xcd3131 }

describe('a frame', () => {
  it('is kept by line, and says which lines it changed', () => {
    const store = new RowStore()
    const first = store.apply(frame({ rows: [row(0, 'one'), row(1, 'two')] }))

    expect(first).toEqual({ reset: true, changed: [0, 1] })
    expect(store.row(1)?.text.join('')).toBe('two')

    const second = store.apply(frame({ sequence: 2, rows: [row(1, 'TWO')] }))
    expect(second, 'a frame of the same epoch was treated as a new screen').toEqual({ reset: false, changed: [1] })
    expect(store.row(0)?.text.join(''), 'a line the frame did not name was lost').toBe('one')
    expect(store.row(1)?.text.join('')).toBe('TWO')
  })

  it('of another epoch leaves nothing of the last', () => {
    const store = new RowStore()
    store.apply(frame({ rows: [row(0, 'before')] }))

    const after = store.apply(frame({ epoch: 2, rows: [row(1, 'after')] }))

    expect(after.reset).toBe(true)
    expect(store.row(0), 'a line numbered before the reflow survived it').toBeUndefined()
    expect(store.row(1)?.text.join('')).toBe('after')
  })

  it('keeps a row that scrolled into history, and drops what the owner no longer holds', () => {
    const store = new RowStore()
    store.apply(frame({ rows: [row(0, 'a'), row(1, 'b'), row(2, 'c'), row(3, 'd')] }))

    // Two lines scrolled off, and the owner keeps only one line of history
    store.apply(frame({ sequence: 2, firstActive: 2, scrollback: 1, rows: [row(4, 'e'), row(5, 'f')] }))

    expect(store.oldest).toBe(1)
    expect(store.row(0), 'a line the owner dropped was kept').toBeUndefined()
    expect(store.row(1)?.text.join(''), 'history already seen has to be asked for again').toBe('b')
    expect(store.row(5)?.text.join('')).toBe('f')
  })

  it('lets go of history it is told changed without being resent', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 4, scrollback: 4, rows: [row(1, 'old'), row(2, 'older'), row(3, 'kept')] }))

    const told = store.apply(frame({ sequence: 2, firstActive: 4, scrollback: 4, staleFrom: 1, rows: [row(3, 'resent')] }))

    expect(store.row(1), 'a stale line was kept').toBeUndefined()
    expect(store.row(2)).toBeUndefined()
    expect(store.row(3)?.text.join(''), 'a line the same frame carried was dropped as stale').toBe('resent')
    expect(told.changed, 'each line once, in order').toEqual([1, 2, 3])
  })
})

describe('the styles and links of a kept row', () => {
  it('are its own, so rows that number theirs differently each keep what they were sent with', () => {
    const store = new RowStore()
    const linked = { ...row(2, 'b', { style: 2, styles: [red, bold], links: ['https://example.com'] }), link: Uint16Array.from([1]) }
    store.apply(frame({ rows: [row(0, 'b', { style: 1, styles: [bold] }), row(1, 'r', { style: 1, styles: [red] }), linked] }))

    const styleOf = (line: number) => { const kept = store.row(line)!; return kept.styles[kept.style[0]!] }
    expect(styleOf(0)).toEqual(bold)
    expect(styleOf(1)).toEqual(red)
    expect(styleOf(2)).toEqual(bold)
    expect(store.row(2)!.links[store.row(2)!.link[0]!]).toBe('https://example.com')
    expect(store.row(0)!.styles[0], 'index zero is the plain style on every row').toEqual(PLAIN)
  })

  it('are let go with the row, so a store holds no more of them than its rows do', () => {
    const store = new RowStore()
    const coloured = (sequence: number) => row(0, 'x', { style: 1, styles: [{ ...PLAIN, fg: sequence }] })
    // One line rewritten ten thousand times, a new colour each time
    for (let sequence = 1; sequence <= 10_000; sequence += 1) store.apply(frame({ sequence, rows: [coloured(sequence)] }))

    expect(store.size).toBe(1)
    expect(store.row(0)!.styles).toEqual([PLAIN, { ...PLAIN, fg: 10_000 }])
  })
})

describe('what a view has to ask for', () => {
  it('is every line of history it does not have, as runs', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10, rows: [row(3, 'x'), row(4, 'y'), row(10, 's')] }))

    expect(store.missing(0, 14), 'history it lacks, and none of the screen').toEqual([[0, 3], [5, 5]])
    expect(store.missing(3, 2), 'lines it holds were asked for').toEqual([])
  })

  it('is never a line the owner does not hold', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 2 }))

    expect(store.missing(0, 100), 'asked below the oldest line, or for the screen').toEqual([[8, 2]])
  })
})

describe('rows that were asked for', () => {
  it('are kept when they are history of this epoch', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10 }))

    const kept = store.absorb({ epoch: 1, first: 2, count: 1, rows: [row(2, 'old')] })

    expect(kept).toEqual([2])
    expect(store.row(2)?.text.join('')).toBe('old')
  })

  it('say which lines they answer for: one of those with no row is not held by the owner, and is not asked for again', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 100, scrollback: 100 }))
    expect(store.missing(0, 100)).toEqual([[0, 100]])

    // Sixty lines were asked about; the answer got as far as forty of them, and has rows for two
    const told = store.absorb({ epoch: 1, first: 20, count: 40, rows: [row(30, 'held'), row(31, 'held too')] })

    expect(told, 'every line the answer said something about').toHaveLength(40)
    expect(store.missing(0, 100), 'what it covered is settled; what it did not reach is still to ask for').toEqual([[0, 20], [60, 40]])
  })

  it('that the owner did not hold are asked for again once the owner says its history changed', () => {
    const none = (store: RowStore) => store.absorb({ epoch: 1, first: 0, count: 10, rows: [] })

    const stale = new RowStore()
    stale.apply(frame({ firstActive: 10, scrollback: 10 }))
    none(stale)
    expect(stale.missing(0, 10)).toEqual([])
    stale.apply(frame({ sequence: 2, firstActive: 10, scrollback: 10, staleFrom: 4 }))
    expect(stale.missing(0, 10), 'history that changed without being resent').toEqual([[0, 10]])

    const trimmed = new RowStore()
    trimmed.apply(frame({ firstActive: 10, scrollback: 10 }))
    none(trimmed)
    trimmed.apply(frame({ sequence: 2, firstActive: 12, scrollback: 6 }))
    expect(trimmed.missing(0, 12), 'where history starts moved').toEqual([[6, 6]])

    const written = new RowStore()
    written.apply(frame({ firstActive: 10, scrollback: 10 }))
    none(written)
    written.apply(frame({ sequence: 2, firstActive: 10, scrollback: 10, rows: [row(3, 'resent')] }))
    expect(written.row(3)?.text.join(''), 'a frame brought a line the owner had said it did not hold').toBe('resent')
    expect(written.missing(0, 10)).toEqual([])
  })

  it('are dropped when they answer for another epoch', () => {
    const store = new RowStore()
    store.apply(frame({ epoch: 2, firstActive: 10, scrollback: 10 }))

    expect(store.absorb({ epoch: 1, first: 2, count: 1, rows: [row(2, 'stale')] })).toEqual([])
    expect(store.row(2)).toBeUndefined()
  })

  it('never replace a line of the screen, which only a frame describes', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10, rows: [row(10, 'live')] }))

    store.absorb({ epoch: 1, first: 10, count: 1, rows: [row(10, 'fetched')] })

    expect(store.row(10)?.text.join('')).toBe('live')
  })
})

describe('how much is kept', () => {
  const lines = KEPT_LINES + KEPT_SLACK + 10
  const fetched = (first: number, count: number): Fetched =>
    ({ epoch: 1, first, count, rows: Array.from({ length: count }, (_, index) => row(first + index, 'x')) })

  it('is bounded, and what goes is furthest from the screen for a view that is showing it', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: lines, scrollback: lines }))
    store.absorb(fetched(0, lines))

    expect(store.size).toBeLessThanOrEqual(KEPT_LINES)
    expect(store.size, 'far more went than had to').toBeGreaterThan(KEPT_LINES - 2)
    expect(store.row(0), 'the line furthest from the screen was kept past the bound').toBeUndefined()
    expect(store.row(lines - 1)).toBeDefined()
  })

  it('is what is nearest the viewer, so lines fetched for someone reading far back are not let go as they arrive', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: lines * 2, scrollback: lines * 2 }))
    // Everything near the screen is held, right up to where lines start being let go
    store.absorb(fetched(lines * 2 - KEPT_LINES - KEPT_SLACK, KEPT_LINES + KEPT_SLACK))

    // And the viewer is at the oldest line there is, where a page arrives
    const kept = store.absorb(fetched(0, 256), 0)

    expect(kept).toHaveLength(256)
    expect(store.row(0), 'the lines just fetched were the ones let go').toBeDefined()
    expect(store.row(255)).toBeDefined()
    expect(store.missing(0, 256), 'what was fetched has to be asked for again').toEqual([])
    expect(store.row(lines * 2 - 1), 'the line furthest from the viewer was kept').toBeUndefined()
    expect(store.size).toBeLessThanOrEqual(KEPT_LINES)
  })

  it('is never short of the screen, which no fetch brings back, however far from it the viewer is', () => {
    const store = new RowStore()
    const screen = [0, 1, 2, 3].map((index) => row(lines * 2 + index, 'live'))
    store.apply(frame({ firstActive: lines * 2, scrollback: lines * 2, rows: screen }))
    // The viewer is at the oldest line, and more than may be kept arrives around it
    store.absorb(fetched(0, lines), 0)

    expect(store.size).toBeLessThanOrEqual(KEPT_LINES + 1)
    expect(screen.map((live) => store.row(live.line)?.text.join('')), 'a line of the screen was let go').toEqual(['live', 'live', 'live', 'live'])
    expect(store.row(0)).toBeDefined()
  })

  it('is not gone through again for every line that arrives past the bound', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: lines, scrollback: lines }))
    store.absorb(fetched(0, KEPT_LINES + 10))

    expect(store.size, 'a few lines over the bound cost a pass over all of them').toBe(KEPT_LINES + 10)
  })
})

describe('history that shows part of an image', () => {
  const pictured = (line: number, generation: number) => ({
    ...row(line, '    '),
    images: {
      cellWidth: 10, cellHeight: 20,
      pieces: [{ image: 7, generation, column: 0, columns: 4, offsetX: 0, offsetY: 0, width: 40, height: 20, sourceX: 0, sourceY: 0, sourceWidth: 40, sourceHeight: 20 }],
    },
  })

  it('is asked for again once doubted, kept until it is brought, and taken as it then is', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10, rows: [pictured(10, 1)] }))
    store.absorb({ epoch: 1, first: 0, count: 10, rows: [row(3, 'text'), pictured(4, 1), pictured(5, 1)] })
    expect(store.missing(0, 10)).toEqual([])
    expect([...store.pictured()]).toEqual([7])

    store.doubtPictured()
    expect(store.missing(0, 20), 'only the history that shows an image is asked for, and never the screen').toEqual([[4, 2]])
    expect(store.row(4)?.images?.pieces[0]?.generation, 'a doubted row was let go before it was brought again').toBe(1)

    // One of them brought as it now is; the other the owner answers for and no longer holds
    const told = store.absorb({ epoch: 1, first: 4, count: 2, rows: [pictured(4, 2)] })
    expect(told).toEqual([4, 5])
    expect(store.row(4)?.images?.pieces[0]?.generation).toBe(2)
    expect(store.row(5)).toBeUndefined()
    expect(store.missing(0, 10)).toEqual([])
  })

  it('is doubted no longer once a frame brings it or names it stale', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10 }))
    store.absorb({ epoch: 1, first: 0, count: 10, rows: [pictured(4, 1), pictured(8, 1)] })
    store.doubtPictured()

    store.apply(frame({ sequence: 2, firstActive: 10, scrollback: 10, staleFrom: 8, rows: [pictured(4, 2)] }))
    expect(store.missing(4, 1), 'a row a frame brought is still asked for').toEqual([])
    expect(store.row(4)?.images?.pieces[0]?.generation).toBe(2)
    // Named stale, so let go: missing as any line not held is, and brought back as whatever it now is
    expect(store.row(8)).toBeUndefined()
    store.absorb({ epoch: 1, first: 8, count: 1, rows: [row(8, 'text')] })
    expect(store.row(8)?.text.join('')).toBe('text')
  })
})

