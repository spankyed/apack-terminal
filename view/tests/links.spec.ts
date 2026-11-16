// What on a row can be opened: what a program declared, and text that looks like something to open.
import { describe, expect, it } from 'vitest'
import { linkAt, linkRuns, urls, type Detector } from '../src/links.ts'
import type { Row } from '../src/source.ts'
import { row } from './_support.ts'

describe('a link on a row', () => {
  it('is what a program declared, across every character that shares the declaration', () => {
    const declared: Row = { ...row(0, 'see docs here', { links: ['https://declared.example/x'] }), link: Uint16Array.from([0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0, 0]) }
    expect(linkAt(declared, 5, [urls])).toEqual({ target: 'https://declared.example/x', kind: 'declared', from: 4, to: 8 })
    expect(linkAt(declared, 2, [urls])).toBeNull()
  })

  it('is a URL in the text, without what follows it in a sentence', () => {
    const prose = row(0, 'go to https://example.com/a?b=1). then')
    const found = linkAt(prose, 10, [urls])
    expect(found).toEqual({ target: 'https://example.com/a?b=1', kind: 'url', from: 6, to: 31 })
    expect(linkAt(prose, 31, [urls]), 'the closing bracket is not part of it').toBeNull()
    expect(linkAt(prose, 0, [urls])).toBeNull()
  })

  it('covers the cells its characters cover, where some are wide and some sit after a gap', () => {
    // 世 covers two cells, then a blank cell, then a URL starting at the fourth
    const wide: Row = {
      ...row(0, ''),
      text: ['世', ...'http://a.b'],
      column: Uint16Array.from([0, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
      width: Uint8Array.from([2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]),
      style: new Uint16Array(11),
      link: new Uint16Array(11),
    }
    expect(linkAt(wide, 5, [urls])).toEqual({ target: 'http://a.b', kind: 'url', from: 3, to: 13 })
    expect(linkAt(wide, 2, [urls]), 'a blank cell is not a link').toBeNull()
  })

  it('is whatever a host taught the view to find, with the target the host gave it', () => {
    const paths: Detector = {
      kind: 'path',
      find: (text) => [...text.matchAll(/[\w./-]+\.ts:\d+/g)].map((match) => ({ start: match.index, end: match.index + match[0].length, target: `open ${match[0]}` })),
    }
    const output = row(0, 'error at src/view.ts:42 here')
    expect(linkAt(output, 12, [urls, paths])).toEqual({ target: 'open src/view.ts:42', kind: 'path', from: 9, to: 23 })
  })

  it('keeps a bracket the URL itself opened, and leaves one that closes what came before it', () => {
    const wiki = 'see https://en.wikipedia.org/wiki/Rust_(programming_language) for more'
    expect(linkAt(row(0, wiki), 10, [urls])?.target).toBe('https://en.wikipedia.org/wiki/Rust_(programming_language)')

    const aside = '(at https://example.com/a_(b)), and on'
    expect(linkAt(row(0, aside), 10, [urls])?.target).toBe('https://example.com/a_(b)')
  })

  it('is found whole from any row of a line the terminal wrapped, and covers its own cells of each', () => {
    const rows = new Map([
      [4, row(4, 'not part of it')],
      [5, row(5, 'docs: https://exampl', { wrapped: true })],
      [6, row(6, 'e.com/a/very/long/pa', { wrapped: true })],
      [7, row(7, 'th.html and then')],
      [8, row(8, 'e.com/another')],
    ])
    const at = (line: number, column: number) => linkAt(rows.get(line)!, column, [urls], (other) => rows.get(other))
    const whole = 'https://example.com/a/very/long/path.html'

    expect(at(5, 8)).toEqual({ target: whole, kind: 'url', from: 6, to: 20 })
    expect(at(6, 3)).toEqual({ target: whole, kind: 'url', from: 0, to: 20 })
    expect(at(7, 2)).toEqual({ target: whole, kind: 'url', from: 0, to: 7 })
    expect(at(7, 9), 'what followed the URL was taken for part of it').toBeNull()
    expect(at(8, 2), 'a row was joined to one that did not wrap onto it').toBeNull()
  })

  it('stops at the edge of a row where the row it wrapped onto is not held', () => {
    const cut = row(5, 'docs: https://exampl', { wrapped: true })
    expect(linkAt(cut, 8, [urls], () => undefined)?.target).toBe('https://exampl')
  })
})

describe('the cells a declared link covers across a wrap', () => {
  /** A row whose characters from `from` up to `to` declare `target` */
  const declaredRow = (line: number, text: string, from: number, to: number, target: string, wrapped = false): Row => {
    const base = row(line, text, { links: [target], wrapped })
    return { ...base, link: Uint16Array.from(base.text, (_, index) => (index >= from && index < to ? 1 : 0)) }
  }
  const rowsOf = (...rows: Row[]) => new Map(rows.map((one) => [one.line, one]))

  it('is one run on each row it wraps over, joined by its URI, from any of them', () => {
    const rows = rowsOf(declaredRow(0, 'see the link', 4, 12, 'https://x.example/a', true), declaredRow(1, 'continues here', 0, 9, 'https://x.example/a'))
    const runs = (line: number, column: number) => linkRuns(linkAt(rows.get(line)!, column, [])!, rows.get(line)!, column, (other) => rows.get(other))
    const both = [{ line: 0, from: 4, to: 12 }, { line: 1, from: 0, to: 9 }]

    expect(runs(0, 6)).toEqual(both)
    expect(runs(1, 3)).toEqual(both)
  })

  it('is not joined to a row below when the link stops before the edge of its row', () => {
    const rows = rowsOf(declaredRow(0, 'see the link', 4, 8, 'https://x.example/a', true), declaredRow(1, 'continues here', 0, 9, 'https://x.example/a'))
    expect(linkRuns(linkAt(rows.get(0)!, 5, [])!, rows.get(0)!, 5, (other) => rows.get(other))).toEqual([{ line: 0, from: 4, to: 8 }])
  })

  it('is not joined to a row the first row did not wrap onto, or to one with a different URI', () => {
    const unwrapped = rowsOf(declaredRow(0, 'see the link', 4, 12, 'https://x.example/a'), declaredRow(1, 'continues here', 0, 9, 'https://x.example/a'))
    expect(linkRuns(linkAt(unwrapped.get(0)!, 6, [])!, unwrapped.get(0)!, 6, (other) => unwrapped.get(other))).toEqual([{ line: 0, from: 4, to: 12 }])

    const other = rowsOf(declaredRow(0, 'see the link', 4, 12, 'https://x.example/a', true), declaredRow(1, 'continues here', 0, 9, 'https://x.example/b'))
    expect(linkRuns(linkAt(other.get(0)!, 6, [])!, other.get(0)!, 6, (line) => other.get(line))).toEqual([{ line: 0, from: 4, to: 12 }])
  })

  it('is one run for a link a detector found, which a wrap does not extend', () => {
    const wrapped = rowsOf(row(0, 'docs: https://exampl', { wrapped: true }), row(1, 'e.com/path'))
    const found = linkAt(wrapped.get(0)!, 8, [urls], (line) => wrapped.get(line))!
    expect(linkRuns(found, wrapped.get(0)!, 8, (line) => wrapped.get(line))).toEqual([{ line: 0, from: 6, to: 20 }])
  })
})
