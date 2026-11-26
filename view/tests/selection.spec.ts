import { describe, expect, it } from 'vitest'
import { RowStore, Selection } from '../src/index.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { settled } from './_view.ts'

describe('Selection (0 ms local queries + single-RPC fallback + block selection)', () => {
  it('resolves word, line, linear text, and rectangular block text locally in 0 ms without RPCs', async () => {
    const source = new FakeSource()
    const store = new RowStore()
    store.apply(
      frame({
        size: { cols: 20, rows: 3 },
        rows: [
          row(0, 'alpha beta gamma'),
          row(1, 'delta epsilon zeta'),
          row(2, 'eta   theta   iota'),
        ],
      }),
    )

    let changes = 0
    const sel = new Selection(source, store, () => { changes += 1 })
    const v0 = sel.version

    // Double-click word selection resolves synchronously!
    sel.begin({ line: 0, col: 7 }, 'word')
    expect(sel.version).toBeGreaterThan(v0)
    expect(sel.span).toEqual({ start: { line: 0, col: 6 }, end: { line: 0, col: 9 } })
    expect(sel.syncText()).toBe('beta')
    expect(await sel.text()).toBe('beta')
    expect(source.remoteWordCalls).toEqual([])
    expect(source.remoteTextCalls).toEqual([])
    expect(changes).toBe(1)

    // Triple-click line selection resolves synchronously!
    sel.begin({ line: 1, col: 3 }, 'line')
    expect(sel.span).toEqual({ start: { line: 1, col: 0 }, end: { line: 1, col: 19 } })
    expect(await sel.text()).toBe('delta epsilon zeta')
    expect(source.remoteLineCalls).toEqual([])

    // Block (rectangular Alt-drag) selection resolves synchronously!
    sel.begin({ line: 0, col: 0 }, 'cell', 'block')
    sel.extendTo({ line: 2, col: 4 })
    expect(sel.block).toEqual({ top: 0, bottom: 2, left: 0, right: 4 })
    expect(sel.on(1)).toEqual({ from: 0, to: 5 })
    expect(await sel.text()).toBe('alpha\ndelta\neta')
    expect(source.remoteRectCalls).toEqual([])
  })

  it('falls back to a single broker RPC when crossing uncached history', async () => {
    const source = new FakeSource()
    source.remoteWordAnswer = { start: { line: 10, col: 0 }, end: { line: 10, col: 4 } }
    source.remoteTextAnswer = 'remote-linear'
    source.remoteRectAnswer = 'remote-rect'

    const store = new RowStore()
    store.apply(
      frame({
        epoch: 3,
        firstActiveLine: 10,
        scrollbackLines: 10,
        size: { cols: 20, rows: 2 },
        rows: [row(10, 'hello world'), row(11, 'second')],
      }),
    )

    const sel = new Selection(source, store, () => {})
    // Line 9 is uncached -> localWordAt returns null -> falls back to source.wordAt
    sel.begin({ line: 10, col: 2 }, 'word')
    await settled()
    expect(source.remoteWordCalls).toEqual([{ line: 10, col: 2 }])
    expect(sel.span).toEqual({ start: { line: 10, col: 0 }, end: { line: 10, col: 4 } })

    // Linear selection spanning uncached line 5..10 calls source.textOf once
    sel.select({ start: { line: 5, col: 0 }, end: { line: 10, col: 4 } })
    expect(sel.syncText()).toBeNull()
    expect(await sel.text()).toBe('remote-linear')
    expect(source.remoteTextCalls).toHaveLength(1)

    // Block selection spanning uncached line 5..10 calls source.textOfRect ONCE (not N times!)
    sel.begin({ line: 5, col: 1 }, 'cell', 'block')
    sel.extendTo({ line: 10, col: 4 })
    expect(await sel.text()).toBe('remote-rect')
    expect(source.remoteRectCalls).toHaveLength(1)

    sel.reset()
    expect(sel.span).toBeNull()
  })
})
