import type { SelectedCells } from './draw.ts'
import type { Position, Row, Span, TerminalSource } from './source.ts'

/** What a click selects by, which is how many times it was clicked */
export type Granularity = 'cell' | 'word' | 'line'

/**
 * The shape of a selection made by dragging over cells: `linear` is the text between two cells, as a program would have it;
 * `block` is the rectangle between two cells, which is each line's own columns (Alt-drag, Option-drag on a Mac).
 */
export type Shape = 'linear' | 'block'

/**
 * The first (`start`) or last (`end`) column of the character that covers `column` in `row`, or `column` itself where
 * no character does: a blank cell is its own column.
 */
function glyphCovering(row: Row, column: number, edge: 'start' | 'end'): number {
  for (let index = 0; index < row.column.length; index += 1) {
    const from = row.column[index]!
    const to = from + row.width[index]! - 1
    if (column >= from && column <= to) return edge === 'start' ? from : to
  }
  return column
}

const before = (a: Position, b: Position): boolean => a.line < b.line || (a.line === b.line && a.column < b.column)

/**
 * What the user has selected, in absolute lines.
 *
 * **Kept here and drawn at once; only its meaning is asked of the source.** Where a selection is, the view
 * knows without asking, so dragging one never waits on anything. What it cannot know from the rows it
 * holds is where a word ends, where a wrapped line began, or what the text is with wrapped rows joined —
 * those are the terminal's facts, and the source answers them.
 *
 * In absolute lines, so a selection stays on the text it was made on while output arrives and while the
 * view scrolls.
 */
export class Selection {
  /** Where the selection was started, as the span that start selected: one cell, a word or a line */
  private anchor: Span | null = null
  /** The other end, likewise, and the cell it was last moved to */
  private head: Span | null = null
  private headAt: Position | null = null
  private granularity: Granularity = 'cell'
  /** Whether the selection is the text between its ends, or the rectangle between them. Only a cell selection is a block. */
  private shape: Shape = 'linear'
  /** Where a block selection began, in cells: the corner its rectangle is measured from with `headAt` */
  private anchorAt: Position | null = null
  /**
   * Counts the asks about each end, so an answer to one that has been overtaken changes nothing. One each:
   * the far end moving does not make what the word under the start is any less the answer.
   */
  private anchorAsked = 0
  private headAsked = 0

  constructor(
    private readonly source: TerminalSource,
    private readonly columns: () => number,
    private readonly changed: () => void,
    /** The lines the terminal still holds, `oldest` up to but not including `end`. Left out, every line */
    private readonly held?: () => { readonly oldest: number; readonly end: number },
    /** The row a line is drawn from, when the view has it: what its wide characters span, for a block's copy */
    private readonly rowOf?: (line: number) => Row | null,
  ) {}

  /**
   * The rectangle of a block selection, inclusive, or `null` when the selection is not one. Its corners are the cells the
   * drag began and ended in, whichever way round.
   */
  get block(): { top: number; bottom: number; left: number; right: number } | null {
    if (this.shape !== 'block' || !this.anchorAt || !this.headAt) return null
    return {
      top: Math.min(this.anchorAt.line, this.headAt.line),
      bottom: Math.max(this.anchorAt.line, this.headAt.line),
      left: Math.min(this.anchorAt.column, this.headAt.column),
      right: Math.max(this.anchorAt.column, this.headAt.column),
    }
  }

  /** The selection, start before end, or `null` when there is none. A block's is the span of its rectangle. */
  get span(): Span | null {
    const rectangle = this.block
    if (rectangle) return { start: { line: rectangle.top, column: rectangle.left }, end: { line: rectangle.bottom, column: rectangle.right } }
    if (!this.anchor || !this.head) return null
    const start = before(this.head.start, this.anchor.start) ? this.head.start : this.anchor.start
    const end = before(this.anchor.end, this.head.end) ? this.head.end : this.anchor.end
    return { start, end }
  }

  /** The cells of `line` that are selected, for drawing */
  on(line: number): SelectedCells | undefined {
    const cols = this.columns()
    if (cols <= 0) return undefined
    const maxCol = cols - 1
    const rectangle = this.block
    if (rectangle) {
      if (line < rectangle.top || line > rectangle.bottom) return undefined
      const from = Math.max(0, Math.min(rectangle.left, maxCol))
      const to = Math.max(from + 1, Math.min(rectangle.right + 1, cols))
      return { from, to }
    }
    const span = this.span
    if (!span || line < span.start.line || line > span.end.line) return undefined
    const from = line === span.start.line ? Math.max(0, Math.min(span.start.column, maxCol)) : 0
    // A line the selection runs through is selected to its edge, blank cells and all
    const to = line === span.end.line ? Math.max(0, Math.min(span.end.column + 1, cols)) : cols
    return to > from ? { from, to } : undefined
  }

  /**
   * Starts a selection at `position`: a cell, or the word or line there. A cell may start a block: a word or a line is
   * always linear.
   */
  begin(position: Position, granularity: Granularity, shape: Shape = 'linear'): void {
    this.granularity = granularity
    this.shape = granularity === 'cell' ? shape : 'linear'
    this.anchorAt = position
    const cell = { start: position, end: position }
    this.anchor = cell
    this.head = cell
    this.headAt = position
    const anchor = (this.anchorAsked += 1)
    const head = (this.headAsked += 1)
    this.changed()
    // One answer is both ends, for as long as the far one has not moved off this cell
    this.widen(position, (span) => {
      if (anchor === this.anchorAsked) this.anchor = span
      if (head === this.headAsked) this.head = span
      return anchor === this.anchorAsked || head === this.headAsked
    })
  }

  /** Moves the far end of the selection to `position` */
  extendTo(position: Position): void {
    if (!this.anchor) return
    // Still in the cell it was last moved to: nothing changed, and there is nothing new to ask
    if (this.headAt && this.headAt.line === position.line && this.headAt.column === position.column) return
    this.head = { start: position, end: position }
    this.headAt = position
    const head = (this.headAsked += 1)
    this.changed()
    this.widen(position, (span) => {
      if (head === this.headAsked) this.head = span
      return head === this.headAsked
    })
  }

  /** Selects exactly these cells, from the first to the last */
  select(span: Span): void {
    this.anchorAsked += 1
    this.headAsked += 1
    this.granularity = 'cell'
    this.shape = 'linear'
    this.anchorAt = null
    this.anchor = { start: span.start, end: span.start }
    this.head = { start: span.end, end: span.end }
    this.headAt = span.end
    this.changed()
  }

  clear(): void {
    if (!this.anchor) return
    this.anchorAsked += 1
    this.headAsked += 1
    this.anchor = null
    this.head = null
    this.headAt = null
    this.anchorAt = null
    this.shape = 'linear'
    this.changed()
  }

  /**
   * The selected text as a program wrote it, or the empty string. Only of lines the terminal still holds:
   * a selection made before its history was trimmed can name lines that are gone, and a source asked for
   * the text of those has none to give for any of it.
   */
  async text(): Promise<string> {
    const cols = this.columns()
    if (cols <= 0) return ''
    const maxCol = cols - 1
    const clamp = (pos: Position): Position => ({ line: pos.line, column: Math.max(0, Math.min(pos.column, maxCol)) })
    const rectangle = this.block
    if (rectangle) return this.blockText(rectangle)
    const span = this.span
    if (!span) return ''
    const held = this.held?.()
    if (!held) {
      const start = clamp(span.start)
      const end = clamp(span.end)
      return before(end, start) ? '' : this.source.textOf({ start, end })
    }
    const start = clamp(span.start.line < held.oldest ? { line: held.oldest, column: 0 } : span.start)
    const end = clamp(span.end.line >= held.end ? { line: held.end - 1, column: maxCol } : span.end)
    return before(end, start) ? '' : this.source.textOf({ start, end })
  }

  /**
   * A block's text is each line's own columns, joined by newlines. Each line is asked for on its own, which is what a
   * source has: a span of text that runs from one line's column to the next is a different text.
   *
   * **A character the rectangle touches is in it, as it is highlighted.** A wide character takes two cells, and the
   * rectangle may begin on its second, or end on its first; the span asked for widens to take the whole character, the
   * same rule `drawRow` highlights by.
   */
  private async blockText(rectangle: { top: number; bottom: number; left: number; right: number }): Promise<string> {
    const held = this.held?.()
    const top = held ? Math.max(rectangle.top, held.oldest) : rectangle.top
    const bottom = held ? Math.min(rectangle.bottom, held.end - 1) : rectangle.bottom
    if (bottom < top) return ''
    const maxCol = Math.max(0, this.columns() - 1)
    const rectLeft = Math.max(0, Math.min(rectangle.left, maxCol))
    const rectRight = Math.max(rectLeft, Math.min(rectangle.right, maxCol))
    const BATCH = 32
    const lines: string[] = []
    for (let start = top; start <= bottom; start += BATCH) {
      const end = Math.min(bottom, start + BATCH - 1)
      const batch: Array<Promise<string>> = []
      for (let line = start; line <= end; line += 1) {
        const row = this.rowOf?.(line) ?? null
        const left = Math.max(0, Math.min(row ? glyphCovering(row, rectLeft, 'start') : rectLeft, maxCol))
        const right = Math.max(left, Math.min(row ? glyphCovering(row, rectRight, 'end') : rectRight, maxCol))
        batch.push(this.source.textOf({ start: { line, column: left }, end: { line, column: right } }))
      }
      lines.push(...(await Promise.all(batch)))
    }
    return lines.join('\n')
  }

  /** Line numbers are about to mean something else: a selection made in them is gone */
  reset(): void {
    this.clear()
  }

  /**
   * Grows an end to the word or line it is in, when that is what is being selected by. `set` answers
   * whether it took the span. A source that cannot say leaves the end the one cell it is.
   */
  private widen(position: Position, set: (span: Span) => boolean): void {
    if (this.granularity === 'cell') return
    const asked = this.granularity === 'word' ? this.source.wordAt(position) : this.source.lineAt(position)
    void asked.then((span) => {
      if (span && set(span)) this.changed()
    }, () => {})
  }
}
