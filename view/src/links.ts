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

/** What closes a bracket, and what opens it */
const OPENS: Record<string, string> = { ')': '(', ']': '[', '}': '{' }

/**
 * URLs with a scheme a person would recognise. What may follow one in prose — a full stop, a bracket that
 * closes one opened before it — is not part of it; a bracket that closes one the URL itself opened is.
 */
export const urls: Detector = {
  kind: 'url',
  find(text) {
    const found: Array<{ start: number; end: number }> = []
    for (const match of text.matchAll(/\b(?:https?|ftp|file):\/\/[^\s<>"'`]+/g)) {
      let url = match[0]
      for (;;) {
        const last = url.at(-1)!
        const opens = OPENS[last]
        const trailing = opens === undefined ? '.,;:!?'.includes(last) : count(url, last) > count(url, opens)
        if (!trailing) break
        url = url.slice(0, -1)
      }
      found.push({ start: match.index, end: match.index + url.length })
    }
    return found
  },
}

function count(text: string, character: string): number {
  let found = 0
  for (const one of text) if (one === character) found += 1
  return found
}

/** How many rows of a wrapped line are read on each side of the one asked about */
const JOINED_ROWS = 16

/**
 * The link at a cell of a row, if there is one.
 *
 * `rowAt` gives the rows above and below, by line. Text a program wrote as one line and the terminal
 * wrapped is read as that one line, so a URL cut by the edge is found whole from any row of it; `from` and
 * `to` are still the cells of it on the row asked about. Left out, a row is read on its own.
 */
export function linkAt(row: Row, column: number, detectors: readonly Detector[], rowAt?: (line: number) => Row | undefined): Link | null {
  const at = glyphAt(row, column)
  if (at === -1) return null

  // What a program declared wins: it said what this text opens
  const declared = row.link[at]!
  const target = row.links[declared]
  if (declared !== 0 && target !== undefined) {
    let first = at
    let last = at
    while (first > 0 && row.link[first - 1] === declared) first -= 1
    while (last + 1 < row.text.length && row.link[last + 1] === declared) last += 1
    return { target, kind: 'declared', from: row.column[first]!, to: row.column[last]! + row.width[last]! }
  }

  // Otherwise, text that looks like one. The rows the line wrapped onto, as far as they are held
  const joined: Row[] = [row]
  if (rowAt) {
    for (let line = row.line - 1; joined.length <= JOINED_ROWS; line -= 1) {
      const above = rowAt(line)
      if (!above?.wrapped) break
      joined.unshift(above)
    }
    const before = joined.length - 1
    for (let below = row; below.wrapped && joined.length <= before + JOINED_ROWS; ) {
      const next = rowAt(below.line + 1)
      if (!next) break
      joined.push(next)
      below = next
    }
  }

  // The line is read as a string, with each offset mapped back to the character it came from: a character
  // can be several code units and can cover two cells. `starts` is of the row asked about only
  let text = ''
  const starts: number[] = []
  for (const part of joined) {
    for (let index = 0; index < part.text.length; index += 1) {
      const gap = index > 0 ? part.column[index]! - (part.column[index - 1]! + part.width[index - 1]!) : part.column[index]!
      text += ' '.repeat(Math.max(0, gap))
      if (part === row) starts.push(text.length)
      text += part.text[index]!
    }
  }
  const offset = starts[at]!
  for (const detector of detectors) {
    for (const match of detector.find(text)) {
      if (offset < match.start || offset >= match.end) continue
      const first = starts.findIndex((start) => start >= match.start)
      if (first === -1) continue
      let last = first
      while (last + 1 < starts.length && starts[last + 1]! < match.end) last += 1
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

/** The cells one link covers on one row, `from` up to but not including `to` */
export interface LinkRun {
  readonly line: number
  readonly from: number
  readonly to: number
}

/**
 * Every run of cells the link at `column` of `row` covers, from the first row it wraps over to the last.
 *
 * A declared link is the same URI on consecutive cells, and each row holds its own table of links, so a
 * link that wraps is two runs with one URI between them: the run at the end of a row that wrapped onto the
 * next, and the run at the start of that next row. The URI is what joins them, since the row has no other
 * name for a link. A link a detector found is one run, `linkAt` gives its cells on this row.
 */
export function linkRuns(link: Link, row: Row, column: number, rowAt: (line: number) => Row | undefined): LinkRun[] {
  const own: LinkRun = { line: row.line, from: link.from, to: link.to }
  if (link.kind !== 'declared') return [own]
  const at = glyphAt(row, column)
  if (at === -1) return [own]
  const target = link.target
  let first = at
  let last = at
  while (first > 0 && targetAt(row, first - 1) === target) first -= 1
  while (last + 1 < row.text.length && targetAt(row, last + 1) === target) last += 1

  // Upward, while the run reaches the start of its row and the row above wrapped onto it
  const above: LinkRun[] = []
  let top = row
  let start = first
  while (start === 0) {
    const previous = rowAt(top.line - 1)
    if (!previous?.wrapped || previous.text.length === 0) break
    const end = previous.text.length - 1
    if (targetAt(previous, end) !== target) break
    let begin = end
    while (begin > 0 && targetAt(previous, begin - 1) === target) begin -= 1
    above.unshift(runOf(previous, begin, end))
    top = previous
    start = begin
  }

  // Downward, while the run reaches the end of its row and the row reaches onto the next
  const below: LinkRun[] = []
  let bottom = row
  let stop = last
  while (stop === bottom.text.length - 1 && bottom.wrapped) {
    const next = rowAt(bottom.line + 1)
    if (!next || next.text.length === 0 || targetAt(next, 0) !== target) break
    let end = 0
    while (end + 1 < next.text.length && targetAt(next, end + 1) === target) end += 1
    below.push(runOf(next, 0, end))
    bottom = next
    stop = end
  }

  return [...above, runOf(row, first, last), ...below]
}

/** The URI a character of a row declared, or `undefined` where it declared none */
function targetAt(row: Row, index: number): string | undefined {
  const declared = row.link[index]!
  return declared === 0 ? undefined : row.links[declared]
}

/** The cells from character `first` to character `last` of a row, inclusive */
function runOf(row: Row, first: number, last: number): LinkRun {
  return { line: row.line, from: row.column[first]!, to: row.column[last]! + row.width[last]! }
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
