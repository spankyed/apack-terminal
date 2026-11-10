// What on a row can be opened: what a program declared, and text that looks like something to open.
import { describe, expect, it } from 'vitest'
import { linkAt, urls, type Detector } from '../src/links.ts'
import type { Row } from '../src/source.ts'
import { row } from './_support.ts'

const links = ['', 'https://declared.example/x']

describe('a link on a row', () => {
  it('is what a program declared, across every character that shares the declaration', () => {
    const declared: Row = { ...row(0, 'see docs here'), link: Uint16Array.from([0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0, 0]) }
    expect(linkAt(declared, links, 5, [urls])).toEqual({ target: 'https://declared.example/x', kind: 'declared', from: 4, to: 8 })
    expect(linkAt(declared, links, 2, [urls])).toBeNull()
  })

  it('is a URL in the text, without what follows it in a sentence', () => {
    const prose = row(0, 'go to https://example.com/a?b=1). then')
    const found = linkAt(prose, links, 10, [urls])
    expect(found).toEqual({ target: 'https://example.com/a?b=1', kind: 'url', from: 6, to: 31 })
    expect(linkAt(prose, links, 31, [urls]), 'the closing bracket is not part of it').toBeNull()
    expect(linkAt(prose, links, 0, [urls])).toBeNull()
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
    expect(linkAt(wide, links, 5, [urls])).toEqual({ target: 'http://a.b', kind: 'url', from: 3, to: 13 })
    expect(linkAt(wide, links, 2, [urls]), 'a blank cell is not a link').toBeNull()
  })

  it('is whatever a host taught the view to find, with the target the host gave it', () => {
    const paths: Detector = {
      kind: 'path',
      find: (text) => [...text.matchAll(/[\w./-]+\.ts:\d+/g)].map((match) => ({ start: match.index, end: match.index + match[0].length, target: `open ${match[0]}` })),
    }
    const output = row(0, 'error at src/view.ts:42 here')
    expect(linkAt(output, links, 12, [urls, paths])).toEqual({ target: 'open src/view.ts:42', kind: 'path', from: 9, to: 23 })
  })
})
