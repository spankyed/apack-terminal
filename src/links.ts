/**
 * What on a row can be opened.
 *
 * Two kinds of thing are a link. A program can say so itself, with a hyperlink sequence, and that arrives
 * on the row; and text can simply look like one — a URL, or whatever else a host knows how to open, which
 * a host teaches a view with a `Detector`.
 */
import type { Row } from './source.ts'

/** A run of cells on one row that opens something */
export interface Link {
  /** What to open: a URL a program declared, or the text a detector matched */
  readonly target: string
  /** `declared` by the program, or the name of the detector that found it */
  readonly kind: string
  /** The cells it covers, `from` up to but not including `to` */
  readonly from: number
  readonly to: number
}

/** Finds things to open in a row's text. Offsets are into the text it is given */
export interface Detector {
  readonly kind: string
  find(text: string): ReadonlyArray<{ readonly start: number; readonly end: number; readonly target?: string }>
}

/**
 * URLs with a scheme a person would recognise. What may follow one in prose — a full stop, a closing
 * bracket — is not part of it.
 */
export const urls: Detector = {
  kind: 'url',
  find(text) {
    const found: Array<{ start: number; end: number }> = []
    for (const match of text.matchAll(/\b(?:https?|ftp|file):\/\/[^\s<>"'`]+/g)) {
      const url = match[0].replace(/[.,;:!?)\]}>]+$/, '')
      found.push({ start: match.index, end: match.index + url.length })
    }
    return found
  },
}

/** The link at a cell of a row, if there is one */
export function linkAt(row: Row, links: readonly string[], column: number, detectors: readonly Detector[]): Link | null {
  const at = glyphAt(row, column)
  if (at === -1) return null

  // What a program declared wins: it said what this text opens
  const declared = row.link[at]!
  if (declared !== 0) {
    let first = at
    let last = at
    while (first > 0 && row.link[first - 1] === declared) first -= 1
    while (last + 1 < row.text.length && row.link[last + 1] === declared) last += 1
    return { target: links[declared] ?? '', kind: 'declared', from: row.column[first]!, to: row.column[last]! + row.width[last]! }
  }

  // Otherwise, text that looks like one. The row is read as a string, with each offset mapped back to
  // the character it came from: a character can be several code units and can cover two cells
  let text = ''
  const starts: number[] = []
  for (let index = 0; index < row.text.length; index += 1) {
    const gap = index > 0 ? row.column[index]! - (row.column[index - 1]! + row.width[index - 1]!) : row.column[index]!
    text += ' '.repeat(Math.max(0, gap))
    starts.push(text.length)
    text += row.text[index]!
  }
  const offset = starts[at]!
  for (const detector of detectors) {
    for (const match of detector.find(text)) {
      if (offset < match.start || offset >= match.end) continue
      const first = starts.findIndex((start) => start >= match.start)
      let last = first
      while (last + 1 < starts.length && starts[last + 1]! < match.end) last += 1
      if (first === -1) continue
      return {
        target: match.target ?? text.slice(match.start, match.end),
        kind: detector.kind,
        from: row.column[first]!,
        to: row.column[last]! + row.width[last]!,
      }
    }
  }
  return null
}

/** The index of the character covering a cell, or `-1` where the cell is blank */
function glyphAt(row: Row, column: number): number {
  for (let index = 0; index < row.text.length; index += 1) {
    const start = row.column[index]!
    if (column >= start && column < start + row.width[index]!) return index
    if (start > column) break
  }
  return -1
}
