import { describe, expect, it } from 'vitest'
import {
  DEFAULT_WORD_DELIMITERS,
  RowStore,
  RunFlag,
  localLineAt,
  localTextOf,
  localTextOfRect,
  localWordAt,
  packRow,
} from '../src/index.ts'
import { frame, row } from './_support.ts'

describe('0 ms local selection & text queries over cached RowStore', () => {
  it('resolves wrapped logical lines synchronously and returns null across uncached boundaries', () => {
    const store = new RowStore()
    store.apply(
      frame({
        firstActiveLine: 10,
        scrollbackLines: 10,
        size: { cols: 10, rows: 4 },
        rows: [
          row(10, '0123456789', { wrapped: true }),
          row(11, 'abcdefghij', { wrapped: true }),
          row(12, 'end', { wrapped: false }),
          row(13, 'next', { wrapped: false }),
        ],
      }),
    )

    // Line 10 sits right after uncached line 9, so localLineAt(10) cannot prove line 9 wasn't wrapped -> null!
    expect(localLineAt(store, 10)).toBeNull()
    expect(localLineAt(store, 11)).toBeNull()

    // Once line 9 is cached as non-wrapped, lines 10..12 resolve synchronously!
    store.absorb(1, 9, 1, [row(9, 'above', { wrapped: false })])
    expect(localLineAt(store, 10)).toEqual({ start: { line: 10, col: 0 }, end: { line: 12, col: 9 } })
    expect(localLineAt(store, 11)).toEqual({ start: { line: 10, col: 0 }, end: { line: 12, col: 9 } })
    expect(localLineAt(store, 12)).toEqual({ start: { line: 10, col: 0 }, end: { line: 12, col: 9 } })
    expect(localLineAt(store, 13)).toEqual({ start: { line: 13, col: 0 }, end: { line: 13, col: 9 } })
  })

  it('resolves word boundaries across wrapped rows using broker-supplied wordDelimiters', () => {
    const store = new RowStore()
    store.apply(
      frame({
        firstActiveLine: 0,
        scrollbackLines: 0,
        size: { cols: 10, rows: 3 },
        rows: [
          row(0, 'echo hello', { wrapped: true }),
          row(1, '_world(ok)', { wrapped: false }),
          row(2, '日本語 test', { wrapped: false }),
        ],
      }),
    )

    // "hello_world" wraps across line 0..1
    expect(localWordAt(store, { line: 0, col: 7 }, DEFAULT_WORD_DELIMITERS)).toEqual({
      start: { line: 0, col: 5 },
      end: { line: 1, col: 5 },
    })

    // Custom delimiter '_' splits hello and world
    expect(localWordAt(store, { line: 0, col: 7 }, `${DEFAULT_WORD_DELIMITERS}_`)).toEqual({
      start: { line: 0, col: 5 },
      end: { line: 0, col: 9 },
    })

    // Clicking on spaces selects the space run
    expect(localWordAt(store, { line: 0, col: 4 }, DEFAULT_WORD_DELIMITERS)).toEqual({
      start: { line: 0, col: 4 },
      end: { line: 0, col: 4 },
    })

    // CJK wide word spans columns 0..5 (3 wide glyphs * 2 cells)
    expect(localWordAt(store, { line: 2, col: 3 }, DEFAULT_WORD_DELIMITERS)).toEqual({
      start: { line: 2, col: 0 },
      end: { line: 2, col: 5 },
    })
  })

  it('extracts linear and rectangular block text joining wrapped lines and trimming trailing spaces', () => {
    const store = new RowStore()
    store.apply(
      frame({
        firstActiveLine: 0,
        scrollbackLines: 0,
        size: { cols: 10, rows: 4 },
        rows: [
          row(0, '0123456789', { wrapped: true }),
          row(1, 'abcdefgh  ', { wrapped: false }),
          row(2, 'row-two   ', { wrapped: false }),
          row(3, 'row-three ', { wrapped: false }),
        ],
      }),
    )

    expect(
      localTextOf(store, { start: { line: 0, col: 8 }, end: { line: 2, col: 9 } }),
    ).toBe('89abcdefgh\nrow-two')

    expect(
      localTextOfRect(store, { start: { line: 0, col: 2 }, end: { line: 3, col: 5 } }),
    ).toBe('2345\ncdef\nw-tw\nw-th')

    // Missing line inside span returns null to trigger single broker RPC fallback
    const sparse = new RowStore()
    sparse.apply(
      frame({
        firstActiveLine: 5,
        scrollbackLines: 5,
        size: { cols: 10, rows: 2 },
        rows: [row(5, 'five'), row(6, 'six')],
      }),
    )
    expect(localTextOf(sparse, { start: { line: 3, col: 0 }, end: { line: 6, col: 2 } })).toBeNull()
    expect(localTextOfRect(sparse, { start: { line: 3, col: 0 }, end: { line: 6, col: 2 } })).toBeNull()
  })

  it('handles omitted blank active rows, PER_CELL grapheme clusters, narrow non-BMP runs, and single-delimiter selection', () => {
    const store = new RowStore()
    store.apply(
      frame({
        firstActiveLine: 0,
        scrollbackLines: 0,
        size: { cols: 12, rows: 4 },
        fullSync: true,
        rows: [
          row(0, 'foo()/bar'),
          // line 1 intentionally omitted (blank active screen row after fullSync)
          packRow({
            line: 2,
            runs: [
              { colStart: 0, colLen: 2, text: '👨‍👩‍👧‍👦', flags: RunFlag.PER_CELL },
              { colStart: 2, colLen: 1, text: '𝕏', flags: 0 },
              { colStart: 3, colLen: 3, text: ' ok', flags: 0 },
            ],
          }),
          row(3, 'end'),
        ],
      }),
    )

    // Omitted blank active row resolves synchronously as empty line
    expect(localLineAt(store, 1)).toEqual({ start: { line: 1, col: 0 }, end: { line: 1, col: 11 } })
    expect(localTextOf(store, { start: { line: 0, col: 0 }, end: { line: 3, col: 2 } })).toBe('foo()/bar\n\n👨‍👩‍👧‍👦𝕏 ok\nend')

    // Distinct delimiters '(' and ')' do not merge together
    expect(localWordAt(store, { line: 0, col: 3 }, DEFAULT_WORD_DELIMITERS)).toEqual({
      start: { line: 0, col: 3 },
      end: { line: 0, col: 3 },
    })
  })
})
