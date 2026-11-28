/**
 * What on a row can be opened.
 *
 * Two kinds of thing are a link: an explicit OSC 8 hyperlink declared by the program (`row.links` +
 * `linkIdx` in `PackedRow.runs`), or text matched by a `Detector` (such as `urls`) across wrapped
 * rows.
 */
import { RowFlag, RunFlag, type PackedRow } from './source.ts'

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

/** Explicit allow-list of URI schemes permitted for declared OSC 8 hyperlinks */
const ALLOWED_SCHEME = /^(?:https?|mailto):/i

/** Whether a declared link target uses an allowed scheme (stripping ASCII controls/whitespace that URL parsers ignore) */
function isSafeTarget(target: string): boolean {
  const normalized = target.replace(/[\u0000-\u0020\u007f]+/g, '')
  return ALLOWED_SCHEME.test(normalized)
}

/**
 * URLs with a scheme a person would recognise. What may follow one in prose — a full stop, a bracket that
 * closes one opened before it — is not part of it; a bracket that closes one the URL itself opened is.
 */
export const urls: Detector = {
  kind: 'url',
  find(text) {
    const found: Array<{ start: number; end: number }> = []
    for (const match of text.matchAll(/\b(?:https?|ftp|file):\/\/[^\s<>"'`]+/g)) {
      const raw = match[0]
      let openParen = 0
      let closeParen = 0
      let openBracket = 0
      let closeBracket = 0
      let openBrace = 0
      let closeBrace = 0
      for (let i = 0; i < raw.length; i += 1) {
        switch (raw[i]) {
          case '(': openParen += 1; break
          case ')': closeParen += 1; break
          case '[': openBracket += 1; break
          case ']': closeBracket += 1; break
          case '{': openBrace += 1; break
          case '}': closeBrace += 1; break
        }
      }
      let end = raw.length
      while (end > 0) {
        const last = raw[end - 1]!
        if (last === ')') {
          if (closeParen <= openParen) break
          closeParen -= 1
        } else if (last === ']') {
          if (closeBracket <= openBracket) break
          closeBracket -= 1
        } else if (last === '}') {
          if (closeBrace <= openBrace) break
          closeBrace -= 1
        } else if (!'.,;:!?'.includes(last)) {
          break
        }
        end -= 1
      }
      if (end > 0) found.push({ start: match.index, end: match.index + end })
    }
    return found
  },
}

/** How many rows of a wrapped line are read on each side of the one asked about */
const JOINED_ROWS = 16

interface CellInfo {
  readonly col: number
  readonly width: number
  readonly text: string
  readonly linkIdx: number
}

function rowCells(row: PackedRow): CellInfo[] {
  const out: CellInfo[] = []
  const { runs, text } = row
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const txtWord = runs[i + 1]!
    const metaWord = runs[i + 2]!
    const runFlags = runs[i + 3]!
    const colStart = colWord & 0xffff
    const colLen = colWord >>> 16
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    const linkIdx = metaWord >>> 16
    const slice = text.slice(u16Off, u16Off + u16Len)

    if ((runFlags & RunFlag.PER_CELL) !== 0) {
      // Ghostty emits one PER_CELL run per cell (wide char or multi-codepoint grapheme cluster).
      out.push({ col: colStart, width: colLen, text: slice, linkIdx })
    } else if (slice.length === colLen) {
      for (let k = 0; k < colLen; k += 1) {
        out.push({ col: colStart + k, width: 1, text: slice[k]!, linkIdx })
      }
    } else {
      // Narrow non-BMP codepoints (e.g. Nerd Font PUA-A/B surrogate pairs): 1 codepoint per column.
      const cps = Array.from(slice)
      for (let k = 0; k < cps.length && k < colLen; k += 1) {
        out.push({ col: colStart + k, width: 1, text: cps[k]!, linkIdx })
      }
    }
  }
  return out
}

function isWrapped(row: PackedRow): boolean {
  return (row.flags & RowFlag.WRAPPED) !== 0
}

function targetOf(row: PackedRow, linkIdx: number): string | undefined {
  return linkIdx === 0 ? undefined : row.links[linkIdx]
}

function runIndexAt(row: PackedRow, column: number): number {
  const { runs } = row
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const start = colWord & 0xffff
    const len = colWord >>> 16
    if (column >= start && column < start + len) return i
    if (start > column) break
  }
  return -1
}

function runTarget(row: PackedRow, runByteIndex: number): string | undefined {
  const linkIdx = row.runs[runByteIndex + 2]! >>> 16
  return targetOf(row, linkIdx)
}

function runsContiguous(row: PackedRow, leftIdx: number, rightIdx: number): boolean {
  const leftCol = row.runs[leftIdx]!
  const rightCol = row.runs[rightIdx]!
  return (leftCol & 0xffff) + (leftCol >>> 16) === (rightCol & 0xffff)
}

/**
 * The link at a cell of a `PackedRow`, if there is one.
 *
 * `rowAt` gives the rows above and below, by line, so wrapped URLs are detected whole across rows.
 */
export function linkAt(
  row: PackedRow,
  column: number,
  detectors: readonly Detector[],
  rowAt?: (line: number) => PackedRow | undefined,
): Link | null {
  const rIdx = runIndexAt(row, column)
  if (rIdx === -1) return null

  // 1. Declared OSC 8 link takes precedence
  const target = runTarget(row, rIdx)
  if (target !== undefined && target !== '') {
    if (!isSafeTarget(target)) return null
    let first = rIdx
    let last = rIdx
    while (first >= 4 && runTarget(row, first - 4) === target && runsContiguous(row, first - 4, first)) {
      first -= 4
    }
    while (last + 4 < row.runs.length && runTarget(row, last + 4) === target && runsContiguous(row, last, last + 4)) {
      last += 4
    }
    const firstWord = row.runs[first]!
    const lastWord = row.runs[last]!
    return {
      target,
      kind: 'declared',
      from: firstWord & 0xffff,
      to: (lastWord & 0xffff) + (lastWord >>> 16),
    }
  }

  if (detectors.length === 0) return null

  // 2. Join wrapped logical line across up to JOINED_ROWS on each side
  const joined: PackedRow[] = [row]
  if (rowAt) {
    for (let line = row.line - 1; joined.length <= JOINED_ROWS; line -= 1) {
      const above = rowAt(line)
      if (!above || !isWrapped(above)) break
      joined.unshift(above)
    }
    const before = joined.length - 1
    for (let below = row; isWrapped(below) && joined.length <= before + JOINED_ROWS; ) {
      const next = rowAt(below.line + 1)
      if (!next) break
      joined.push(next)
      below = next
    }
  }

  let text = ''
  const targetCells = rowCells(row)
  let targetCellIdx = -1
  for (let i = 0; i < targetCells.length; i += 1) {
    const c = targetCells[i]!
    if (column >= c.col && column < c.col + c.width) {
      targetCellIdx = i
      break
    }
  }
  if (targetCellIdx === -1) return null

  const starts: number[] = []
  for (const part of joined) {
    const cells = part === row ? targetCells : rowCells(part)
    let col = 0
    for (let i = 0; i < cells.length; i += 1) {
      const c = cells[i]!
      if (c.col > col) text += ' '.repeat(c.col - col)
      if (part === row) starts.push(text.length)
      text += c.text
      col = c.col + c.width
    }
  }

  const offset = starts[targetCellIdx]!
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
        from: targetCells[first]!.col,
        to: targetCells[last]!.col + targetCells[last]!.width,
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

function linkRunOf(row: PackedRow, firstIdx: number, lastIdx: number): LinkRun {
  const firstWord = row.runs[firstIdx]!
  const lastWord = row.runs[lastIdx]!
  return {
    line: row.line,
    from: firstWord & 0xffff,
    to: (lastWord & 0xffff) + (lastWord >>> 16),
  }
}

/**
 * Every run of cells the link at `column` of `row` covers, across wrapped rows when declared via OSC 8.
 */
export function linkRuns(
  link: Link,
  row: PackedRow,
  column: number,
  rowAt: (line: number) => PackedRow | undefined,
): LinkRun[] {
  const own: LinkRun = { line: row.line, from: link.from, to: link.to }
  if (link.kind !== 'declared') return [own]
  const at = runIndexAt(row, column)
  if (at === -1) return [own]

  const target = link.target
  let first = at
  let last = at
  while (first >= 4 && runTarget(row, first - 4) === target && runsContiguous(row, first - 4, first)) {
    first -= 4
  }
  while (last + 4 < row.runs.length && runTarget(row, last + 4) === target && runsContiguous(row, last, last + 4)) {
    last += 4
  }

  const above: LinkRun[] = []
  let top = row
  let start = first
  while (start === 0 && top.runs.length > 0 && (top.runs[0]! & 0xffff) === 0) {
    const previous = rowAt(top.line - 1)
    if (!previous || !isWrapped(previous) || previous.runs.length === 0) break
    const end = previous.runs.length - 4
    if (runTarget(previous, end) !== target) break
    let begin = end
    while (begin >= 4 && runTarget(previous, begin - 4) === target && runsContiguous(previous, begin - 4, begin)) {
      begin -= 4
    }
    above.unshift(linkRunOf(previous, begin, end))
    top = previous
    start = begin
  }

  const below: LinkRun[] = []
  let bottom = row
  let stop = last
  while (stop === bottom.runs.length - 4 && isWrapped(bottom)) {
    const next = rowAt(bottom.line + 1)
    if (!next || next.runs.length === 0 || (next.runs[0]! & 0xffff) !== 0 || runTarget(next, 0) !== target) break
    let end = 0
    while (end + 4 < next.runs.length && runTarget(next, end + 4) === target && runsContiguous(next, end, end + 4)) {
      end += 4
    }
    below.push(linkRunOf(next, 0, end))
    bottom = next
    stop = end
  }

  return [...above, linkRunOf(row, first, last), ...below]
}
