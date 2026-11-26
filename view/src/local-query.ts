/**
 * Zero-RTT local selection & text extraction over cached `PackedRow`s in `RowStore`.
 *
 * When a double-click, triple-click, or copy falls entirely inside cached rows (99% of
 * interactive UX), these functions resolve exact boundaries and text synchronously in 0 ms using
 * the broker-supplied `wordDelimiters` from `HelloOk` — zero SSH round-trip lag and synchronous
 * clipboard writes. Returns `null` when crossing an uncached line so the caller falls back to a
 * single broker RPC (`wordAt`/`lineAt`/`textOf`/`textOfRect`).
 */
import { RowFlag, RunFlag, type CellPos, type CellSpan, type PackedRow } from './source.ts'
import type { RowStore } from './row-store.ts'

interface RowCell {
  readonly col: number
  readonly width: number
  readonly text: string
}

/** Materializes the non-blank cell clusters of a `PackedRow` (only called on click/copy, never on paint). */
function cellsOfRow(row: PackedRow): RowCell[] {
  const out: RowCell[] = []
  const { runs, text } = row
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const txtWord = runs[i + 1]!
    const runFlags = runs[i + 3]!
    const colStart = colWord & 0xffff
    const colLen = colWord >>> 16
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    const slice = text.slice(u16Off, u16Off + u16Len)

    if ((runFlags & RunFlag.PER_CELL) !== 0) {
      // Ghostty emits one PER_CELL run per cell (wide glyph or multi-codepoint grapheme cluster).
      out.push({ col: colStart, width: colLen, text: slice })
    } else if (slice.length === colLen) {
      // Pure 1-cell ASCII/BMP run: 1 UTF-16 code unit per column
      for (let k = 0; k < colLen; k += 1) {
        out.push({ col: colStart + k, width: 1, text: slice[k]!, })
      }
    } else {
      // Narrow non-BMP codepoints (surrogate pairs): 1 codepoint per column
      const cps = Array.from(slice)
      for (let k = 0; k < cps.length && k < colLen; k += 1) {
        out.push({ col: colStart + k, width: 1, text: cps[k]! })
      }
    }
  }
  return out
}

const EMPTY_ROW: PackedRow = {
  line: 0,
  flags: 0,
  text: '',
  runs: new Uint32Array(0),
  styles: new Uint32Array(0),
  links: [''],
}

/**
 * Returns the cached row for `line`, treating omitted blank active-screen lines (`>= firstActive`)
 * as empty non-wrapped rows so `fullSync` blank active lines never force a broker RPC fallback.
 */
function resolveLocalRow(store: RowStore, line: number): PackedRow | undefined {
  const found = store.row(line)
  if (found !== undefined) return found
  if (line >= store.firstActive && line < store.end) return EMPTY_ROW
  return undefined
}

/**
 * Resolves the wrapped logical line containing `line` synchronously when every row of that wrapped
 * line (plus the preceding row to prove `start` is not a continuation) is cached. Returns `null`
 * if an uncached history boundary is encountered (requiring broker `lineAt` RPC).
 */
export function localLineAt(store: RowStore, line: number): CellSpan | null {
  if (line < store.oldest || line >= store.end || store.columns <= 0) return null
  if (resolveLocalRow(store, line) === undefined) return null

  let startLine = line
  while (startLine > store.oldest) {
    const prev = resolveLocalRow(store, startLine - 1)
    if (prev === undefined) return null
    if ((prev.flags & RowFlag.WRAPPED) === 0) break
    startLine -= 1
  }

  let endLine = line
  while (endLine + 1 < store.end) {
    const cur = resolveLocalRow(store, endLine)
    if (cur === undefined) return null
    if ((cur.flags & RowFlag.WRAPPED) === 0) break
    endLine += 1
    if (resolveLocalRow(store, endLine) === undefined) return null
  }

  return {
    start: { line: startLine, col: 0 },
    end: { line: endLine, col: Math.max(0, store.columns - 1) },
  }
}

/**
 * Resolves word boundaries at `pos` synchronously using broker-supplied `wordDelimiters` when the
 * containing wrapped line is cached. Returns `null` if an uncached boundary is crossed.
 */
export function localWordAt(store: RowStore, pos: CellPos, wordDelimiters: string): CellSpan | null {
  const logical = localLineAt(store, pos.line)
  if (!logical) return null

  const delims = new Set(Array.from(wordDelimiters))
  const cols = store.columns

  // Flatten the wrapped logical line into an array of cell tokens
  interface FlatCell {
    readonly line: number
    readonly col: number
    readonly width: number
    readonly text: string
  }
  const flat: FlatCell[] = []
  let targetIdx = -1

  for (let line = logical.start.line; line <= logical.end.line; line += 1) {
    const row = store.row(line)
    const cells = row ? cellsOfRow(row) : []
    let cursor = 0
    let cIdx = 0
    while (cursor < cols) {
      const next = cells[cIdx]
      if (next && next.col === cursor) {
        if (line === pos.line && pos.col >= next.col && pos.col < next.col + next.width) {
          targetIdx = flat.length
        }
        flat.push({ line, col: next.col, width: next.width, text: next.text })
        cursor += next.width
        cIdx += 1
      } else {
        if (line === pos.line && pos.col === cursor) {
          targetIdx = flat.length
        }
        flat.push({ line, col: cursor, width: 1, text: ' ' })
        cursor += 1
      }
    }
  }

  if (targetIdx < 0 || targetIdx >= flat.length) return null

  const classify = (ch: string): string => {
    if (ch === ' ' || ch === '\t' || ch === '') return 'space'
    if (delims.has(ch)) return `delim:${ch}`
    return 'word'
  }

  const targetClass = classify(flat[targetIdx]!.text)
  let lo = targetIdx
  let hi = targetIdx
  if (targetClass === 'word' || targetClass === 'space') {
    while (lo > 0 && classify(flat[lo - 1]!.text) === targetClass) lo -= 1
    while (hi + 1 < flat.length && classify(flat[hi + 1]!.text) === targetClass) hi += 1
  }

  const first = flat[lo]!
  const last = flat[hi]!
  return {
    start: { line: first.line, col: first.col },
    end: { line: last.line, col: last.col + last.width - 1 },
  }
}

function sliceRowText(row: PackedRow, fromCol: number, toColInclusive: number, trimTrailingSpaces: boolean): string {
  const cells = cellsOfRow(row)
  let out = ''
  let col = fromCol
  for (const c of cells) {
    if (c.col + c.width <= fromCol) continue
    if (c.col > toColInclusive) break
    if (c.col > col) {
      out += ' '.repeat(c.col - col)
      col = c.col
    }
    out += c.text
    col = c.col + c.width
  }
  return trimTrailingSpaces ? out.replace(/[ ]+$/, '') : out
}

/**
 * Extracts linear selection text across `span` synchronously when every line in `[start.line, end.line]`
 * is cached in `store`. Joins wrapped lines without inserting `\n` and trims trailing whitespace on
 * non-wrapped lines. Returns `null` if any line in the span is missing from `store`.
 */
export function localTextOf(store: RowStore, span: CellSpan): string | null {
  if (span.start.line < store.oldest || span.end.line >= store.end) return null
  const parts: string[] = []
  const maxCol = Math.max(0, store.columns - 1)
  for (let line = span.start.line; line <= span.end.line; line += 1) {
    const row = resolveLocalRow(store, line)
    if (row === undefined) return null
    const fromCol = line === span.start.line ? span.start.col : 0
    const toCol = line === span.end.line ? span.end.col : maxCol
    const wrapped = (row.flags & RowFlag.WRAPPED) !== 0 && line < span.end.line
    parts.push(sliceRowText(row, fromCol, toCol, !wrapped))
    if (line < span.end.line && !wrapped) {
      parts.push('\n')
    }
  }
  return parts.join('')
}

/**
 * Extracts rectangular (Alt-drag) block selection text across `span` synchronously when every line
 * in `[start.line, end.line]` is cached in `store`. Returns `null` if any line is uncached.
 */
export function localTextOfRect(store: RowStore, span: CellSpan): string | null {
  if (span.start.line < store.oldest || span.end.line >= store.end) return null
  const left = Math.min(span.start.col, span.end.col)
  const right = Math.max(span.start.col, span.end.col)
  const lines: string[] = []
  for (let line = span.start.line; line <= span.end.line; line += 1) {
    const row = resolveLocalRow(store, line)
    if (row === undefined) return null
    lines.push(sliceRowText(row, left, right, true))
  }
  return lines.join('\n')
}
