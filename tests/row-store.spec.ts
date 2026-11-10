// What a view keeps of what it has been sent, and what it knows to ask for.
import { describe, expect, it } from 'vitest'
import { KEPT_LINES, RowStore } from '../src/row-store.ts'
import { PLAIN, StyleFlag, type Style } from '../src/source.ts'
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

describe('the styles and links of kept rows', () => {
  it('are moved onto the store, so two messages numbering them differently agree', () => {
    const store = new RowStore()
    store.apply(frame({ styles: [PLAIN, bold], rows: [row(0, 'b', { style: 1 })] }))
    store.apply(frame({ sequence: 2, styles: [PLAIN, red, bold], rows: [row(1, 'r', { style: 1 }), row(2, 'b', { style: 2 })] }))

    const styleOf = (line: number) => store.styles[store.row(line)!.style[0]!]
    expect(styleOf(0)).toEqual(bold)
    expect(styleOf(1)).toEqual(red)
    expect(styleOf(2), 'the same style arrived under another number and was kept twice').toBe(styleOf(0))
    expect(store.styles[0], 'index zero is the plain style whatever was sent').toEqual(PLAIN)
  })
})

describe('what a view has to ask for', () => {
  it('is every held line it does not have, as runs', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10, rows: [row(3, 'x'), row(4, 'y'), row(10, 's')] }))

    expect(store.missing(0, 14)).toEqual([[0, 3], [5, 5], [11, 3]])
    expect(store.missing(3, 2), 'lines it holds were asked for').toEqual([])
  })

  it('is never a line the owner does not hold', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 2 }))

    expect(store.missing(0, 100), 'asked below the oldest line, or past the screen').toEqual([[8, 6]])
  })
})

describe('rows that were asked for', () => {
  it('are kept when they are history of this epoch', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10 }))

    const kept = store.absorb({ epoch: 1, styles: [PLAIN], links: [''], rows: [row(2, 'old')] })

    expect(kept).toEqual([2])
    expect(store.row(2)?.text.join('')).toBe('old')
  })

  it('are dropped when they answer for another epoch', () => {
    const store = new RowStore()
    store.apply(frame({ epoch: 2, firstActive: 10, scrollback: 10 }))

    expect(store.absorb({ epoch: 1, styles: [PLAIN], links: [''], rows: [row(2, 'stale')] })).toEqual([])
    expect(store.row(2)).toBeUndefined()
  })

  it('never replace a line of the screen, which only a frame describes', () => {
    const store = new RowStore()
    store.apply(frame({ firstActive: 10, scrollback: 10, rows: [row(10, 'live')] }))

    store.absorb({ epoch: 1, styles: [PLAIN], links: [''], rows: [row(10, 'fetched')] })

    expect(store.row(10)?.text.join('')).toBe('live')
  })
})

describe('how much is kept', () => {
  it('is bounded, and what goes is the oldest', () => {
    const store = new RowStore()
    const lines = KEPT_LINES + 10
    store.apply(frame({ firstActive: lines, scrollback: lines }))
    store.absorb({ epoch: 1, styles: [PLAIN], links: [''], rows: Array.from({ length: lines }, (_, line) => row(line, 'x')) })

    expect(store.size).toBe(KEPT_LINES)
    expect(store.row(0), 'the oldest line was kept past the bound').toBeUndefined()
    expect(store.row(lines - 1)).toBeDefined()
  })
})
