import { describe, expect, it } from 'vitest'
import { RowFlag, linkAt, linkRuns, packRow, urls, type Detector } from '../src/index.ts'
import { row } from './_support.ts'

describe('links (OSC 8 declared & URL/custom detectors on PackedRow)', () => {
  it('detects URLs and strips trailing sentence punctuation while preserving balanced parens', () => {
    const r = row(0, 'see (https://example.com/a_(b)).')
    const link = linkAt(r, 10, [urls])
    expect(link).toEqual({
      target: 'https://example.com/a_(b)',
      kind: 'url',
      from: 5,
      to: 30,
    })
  })

  it('detects URLs wrapped across consecutive rows', () => {
    const r0 = row(0, 'visit https://exampl', { wrapped: true })
    const r1 = row(1, 'e.com/path/to/page ok')
    const map = new Map([
      [0, r0],
      [1, r1],
    ])
    const link0 = linkAt(r0, 10, [urls], (l) => map.get(l))
    expect(link0).toEqual({
      target: 'https://example.com/path/to/page',
      kind: 'url',
      from: 6,
      to: 20,
    })
    const link1 = linkAt(r1, 3, [urls], (l) => map.get(l))
    expect(link1).toEqual({
      target: 'https://example.com/path/to/page',
      kind: 'url',
      from: 0,
      to: 18,
    })
  })

  it('prefers declared OSC 8 hyperlinks (1-based into links[] with links[0]=""), joins wrapped linkRuns, and blocks unsafe schemes', () => {
    const r0 = packRow({
      line: 0,
      flags: RowFlag.WRAPPED,
      links: ['', 'https://docs.example.dev', 'https://other.example.dev'],
      runs: [
        { colStart: 0, colLen: 5, text: 'click' },
        { colStart: 5, colLen: 5, text: ' here', linkIdx: 1 },
      ],
    })
    const r1 = packRow({
      line: 1,
      links: ['', 'https://docs.example.dev', 'https://other.example.dev'],
      runs: [
        { colStart: 0, colLen: 4, text: 'more', linkIdx: 1 },
        { colStart: 5, colLen: 4, text: 'next', linkIdx: 2 },
      ],
    })
    const rows = new Map([
      [0, r0],
      [1, r1],
    ])

    const link = linkAt(r0, 6, [urls], (l) => rows.get(l))
    expect(link).toEqual({
      target: 'https://docs.example.dev',
      kind: 'declared',
      from: 5,
      to: 10,
    })
    expect(linkRuns(link!, r0, 6, (l) => rows.get(l))).toEqual([
      { line: 0, from: 5, to: 10 },
      { line: 1, from: 0, to: 4 },
    ])
    // Second distinct OSC 8 link on same row resolves to links[2]
    expect(linkAt(r1, 6, [urls], (l) => rows.get(l))).toEqual({
      target: 'https://other.example.dev',
      kind: 'declared',
      from: 5,
      to: 9,
    })

    // Explicit allow-list for OSC 8 declared links: allow http:/https:/mailto:, reject everything else
    const allowedMailto = packRow({
      line: 2,
      links: ['', '  MAILTO:ops@example.com', 'http://example.com/ok'],
      runs: [
        { colStart: 0, colLen: 4, text: 'mail', linkIdx: 1 },
        { colStart: 5, colLen: 4, text: 'http', linkIdx: 2 },
      ],
    })
    expect(linkAt(allowedMailto, 1, [urls])?.target).toBe('  MAILTO:ops@example.com')
    expect(linkAt(allowedMailto, 6, [urls])?.target).toBe('http://example.com/ok')

    for (const target of [
      '  java\tscript:alert(1)',
      'ht tp://evil.example',
      'h\x00ttp://evil.example',
      'http://example.com\r\nfoo',
      'https:relative/path',
      'http:../secret',
      'data:text/html,<script>alert(1)</script>',
      'vbscript:msgbox(1)',
      'file:///etc/passwd',
      'ftp://files.example.com/a',
      'ssh://user@host',
      '/relative/path',
    ]) {
      const bad = packRow({
        line: 3,
        links: ['', target],
        runs: [{ colStart: 0, colLen: 4, text: 'evil', linkIdx: 1 }],
      })
      expect(linkAt(bad, 1, [urls]), `should reject ${target}`).toBeNull()
    }
  })

  it('supports custom host detectors', () => {
    const issueDetector: Detector = {
      kind: 'issue',
      find(text) {
        const out: Array<{ start: number; end: number; target: string }> = []
        for (const m of text.matchAll(/#(\d+)/g)) {
          out.push({ start: m.index, end: m.index + m[0].length, target: `issue:${m[1]}` })
        }
        return out
      },
    }
    const r = row(0, 'fixed #42 today')
    expect(linkAt(r, 7, [urls, issueDetector])).toEqual({
      target: 'issue:42',
      kind: 'issue',
      from: 6,
      to: 9,
    })
  })
})
