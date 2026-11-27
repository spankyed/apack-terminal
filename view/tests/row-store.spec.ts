import { describe, expect, it } from 'vitest'
import { KEPT_LINES, KEPT_SLACK, PAGE_LINES, RowFlag, RowStore } from '../src/index.ts'
import { frame, row } from './_support.ts'

describe('page-chunked RowStore', () => {
  it('applies frames, tracks oldest/end/missing, and resets cleanly on epoch change', () => {
    const store = new RowStore()
    const first = store.apply(
      frame({
        epoch: 1,
        firstActiveLine: 100,
        scrollbackLines: 100,
        size: { cols: 20, rows: 4 },
        rows: [row(100, 'a'), row(101, 'b'), row(102, 'c'), row(103, 'd')],
      }),
    )
    expect(first.reset).toBe(true)
    expect(first.changed).toEqual([100, 101, 102, 103])
    expect(store.oldest).toBe(0)
    expect(store.end).toBe(104)
    expect(store.missing(95, 10)).toEqual([{ from: 95, count: 5 }])

    // Full sync clears unmentioned active rows
    const sync = store.apply(
      frame({
        seq: 2,
        epoch: 1,
        firstActiveLine: 100,
        scrollbackLines: 100,
        size: { cols: 20, rows: 4 },
        fullSync: true,
        rows: [row(100, 'only-top')],
      }),
    )
    expect(sync.reset).toBe(false)
    expect(store.row(100)?.text).toBe('only-top')
    expect(store.row(101)).toBeUndefined()

    // Epoch bump clears cache
    const reset = store.apply(
      frame({
        seq: 3,
        epoch: 2,
        firstActiveLine: 0,
        scrollbackLines: 0,
        size: { cols: 20, rows: 4 },
        rows: [row(0, 'fresh')],
      }),
    )
    expect(reset.reset).toBe(true)
    expect(store.size).toBe(1)
    expect(store.row(100)).toBeUndefined()
  })

  it('absorbs fetched history, marks absent lines, and rejects stale epochs or stale image placeholders', () => {
    const store = new RowStore()
    store.apply(
      frame({
        epoch: 1,
        firstActiveLine: 50,
        scrollbackLines: 50,
        size: { cols: 20, rows: 4 },
        imagesGen: 1,
        rows: [row(50, 'live')],
      }),
    )

    // Wrong epoch ignored
    expect(store.absorb(999, 40, 5, [row(40, 'wrong')])).toEqual([])
    expect(store.row(40)).toBeUndefined()

    // Partial response marks unreturned lines as absent so missing() does not re-ask forever
    const absorbed = store.absorb(1, 40, 5, [row(40, 'h40'), row(42, 'h42')])
    expect(absorbed).toEqual([40, 42])
    expect(store.missing(40, 5)).toEqual([])

    // Placeholders are doubted when imagesGen changes
    const blob = new Uint8Array(32).fill(7)
    const picRow = row(30, ' ', {
      pieces: [{ blob, colStart: 0, colLen: 2, srcRect: [0, 0, 10, 10], dstPx: [20, 20] }],
    })
    expect((picRow.flags & RowFlag.HAS_PLACEHOLDER) !== 0).toBe(true)
    store.absorb(1, 30, 1, [picRow])
    expect(store.pictured()).toEqual([30])
    expect(store.missing(30, 1)).toEqual([])

    const genBefore = store.imagesChanged
    store.apply(
      frame({
        seq: 2,
        epoch: 1,
        firstActiveLine: 50,
        scrollbackLines: 50,
        size: { cols: 20, rows: 4 },
        imagesGen: 2,
        rows: [],
      }),
    )
    expect(store.imagesChanged).toBe(genBefore + 1)
    expect(store.missing(30, 1)).toEqual([{ from: 30, count: 1 }])

    // Stale fetch from before imagesGen bump cannot overwrite placeholder row
    expect(store.absorb(1, 30, 1, [picRow], 50, genBefore)).toEqual([])
  })

  it('evicts furthest 256-line pages in O(pages) while keeping active screen and near-viewer pages', () => {
    const store = new RowStore()
    const total = KEPT_LINES + KEPT_SLACK + PAGE_LINES * 4
    const firstActive = total - 4
    store.apply(
      frame({
        epoch: 1,
        firstActiveLine: firstActive,
        scrollbackLines: firstActive,
        size: { cols: 20, rows: 4 },
        rows: [row(firstActive, 's0'), row(firstActive + 1, 's1')],
      }),
    )

    // Populate pages from 0 up to firstActive - 1 with viewer sitting near line 1000
    const batch = []
    for (let line = 0; line < firstActive; line += 1) {
      batch.push(row(line, `L${line}`))
    }
    store.absorb(1, 0, firstActive, batch, 1000)

    expect(store.size).toBeLessThanOrEqual(KEPT_LINES)
    // Active screen preserved
    expect(store.row(firstActive)?.text).toBe('s0')
    // Near-viewer page around line 1000 preserved
    expect(store.row(1000)?.text).toBe('L1000')
    // Furthest history pages evicted
    expect(store.row(firstActive - PAGE_LINES * 2)).toBeUndefined()
  })

  it('invalidates scrolled-off active rows on fullSync and doubts them on incremental scroll', () => {
    const store = new RowStore()
    store.apply(
      frame({
        seq: 1,
        epoch: 1,
        firstActiveLine: 10,
        scrollbackLines: 10,
        size: { cols: 20, rows: 4 },
        rows: [row(10, 'spinner-1'), row(11, 'spinner-2'), row(12, 'c'), row(13, 'd')],
      }),
    )

    // Incremental scroll shifts lines 10..12 into history: kept visible (no blank flash) but doubted!
    store.apply(
      frame({
        seq: 2,
        epoch: 1,
        firstActiveLine: 12,
        scrollbackLines: 12,
        scrollDelta: 2,
        size: { cols: 20, rows: 4 },
        fullSync: false,
        rows: [row(14, 'e'), row(15, 'f')],
      }),
    )
    expect(store.row(10)?.text).toBe('spinner-1')
    expect(store.missing(10, 2)).toEqual([{ from: 10, count: 2 }])

    // Absorbing authoritative history refreshes line 10 and clears line 11 when omitted (blank)
    store.absorb(1, 10, 2, [row(10, 'done-1')])
    expect(store.row(10)?.text).toBe('done-1')
    expect(store.row(11)).toBeUndefined()
    expect(store.missing(10, 2)).toEqual([])

    // fullSync that jumps firstActive forward drops scrolled-off former-active rows immediately
    store.apply(
      frame({
        seq: 3,
        epoch: 1,
        firstActiveLine: 15,
        scrollbackLines: 15,
        size: { cols: 20, rows: 4 },
        fullSync: true,
        rows: [row(15, 'fresh-15')],
      }),
    )
    expect(store.row(12)).toBeUndefined()
    expect(store.row(14)).toBeUndefined()
    expect(store.row(15)?.text).toBe('fresh-15')

    // Huge same-epoch fullSync jump (10M lines) stays O(screenRows + pages) and finishes promptly
    const t0 = performance.now()
    store.apply(
      frame({
        seq: 4,
        epoch: 1,
        firstActiveLine: 10_000_000,
        scrollbackLines: 10_000_000,
        size: { cols: 20, rows: 4 },
        fullSync: true,
        rows: [row(10_000_000, 'far-future')],
      }),
    )
    expect(performance.now() - t0).toBeLessThan(100)
    expect(store.row(15)).toBeUndefined()
    expect(store.row(10_000_000)?.text).toBe('far-future')
  })

  it('clears doubted on uncached stale-image placeholder row that turns blank in absorb()', () => {
    const store = new RowStore()
    store.apply(
      frame({
        seq: 1,
        epoch: 1,
        firstActiveLine: 10,
        scrollbackLines: 10,
        size: { cols: 20, rows: 4 },
        imagesGen: 1,
        fullSync: true,
        rows: [row(10, 'active')],
      }),
    )
    const atGen = store.imagesChanged
    // imagesGen bumps while fetchRows for line 5 is in flight
    store.apply(
      frame({
        seq: 2,
        epoch: 1,
        firstActiveLine: 10,
        scrollbackLines: 10,
        size: { cols: 20, rows: 4 },
        imagesGen: 2,
        rows: [],
      }),
    )
    // Stale fetch returns an uncached row 5 carrying HAS_PLACEHOLDER -> marks doubted without caching
    store.absorb(1, 5, 1, [{ ...row(5, 'img'), flags: RowFlag.HAS_PLACEHOLDER }], 5, atGen)
    expect(store.row(5)).toBeUndefined()
    expect(store.missing(5, 1)).toEqual([{ from: 5, count: 1 }])

    // Fresh refetch omits line 5 (now blank) -> must clear doubted and mark absent so missing() stops!
    const absorbed = store.absorb(1, 5, 1, [], 5, store.imagesChanged)
    expect(absorbed).toEqual([5])
    expect(store.missing(5, 1)).toEqual([])
  })
})
