import type { SelectedCells } from './draw.ts'
import type { Position, Span, TerminalSource } from './source.ts'

/** What a click selects by, which is how many times it was clicked */
export type Granularity = 'cell' | 'word' | 'line'

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
  /** The other end, likewise */
  private head: Span | null = null
  private granularity: Granularity = 'cell'
  /** Counts the asks, so an answer to one that has been overtaken changes nothing */
  private asked = 0

  constructor(
    private readonly source: TerminalSource,
    private readonly columns: () => number,
    private readonly changed: () => void,
  ) {}

  /** The selection, start before end, or `null` when there is none */
  get span(): Span | null {
    if (!this.anchor || !this.head) return null
    const start = before(this.head.start, this.anchor.start) ? this.head.start : this.anchor.start
    const end = before(this.anchor.end, this.head.end) ? this.head.end : this.anchor.end
    return { start, end }
  }

  /** The cells of `line` that are selected, for drawing */
  on(line: number): SelectedCells | undefined {
    const span = this.span
    if (!span || line < span.start.line || line > span.end.line) return undefined
    return {
      from: line === span.start.line ? span.start.column : 0,
      // A line the selection runs through is selected to its edge, blank cells and all
      to: line === span.end.line ? span.end.column + 1 : this.columns(),
    }
  }

  /** Starts a selection at `position`: a cell, or the word or line there */
  begin(position: Position, granularity: Granularity): void {
    this.granularity = granularity
    const cell = { start: position, end: position }
    this.anchor = cell
    this.head = cell
    this.changed()
    void this.widen(position, (span) => { this.anchor = span; this.head = span })
  }

  /** Moves the far end of the selection to `position` */
  extendTo(position: Position): void {
    if (!this.anchor) return
    this.head = { start: position, end: position }
    this.changed()
    void this.widen(position, (span) => { this.head = span })
  }

  /** Selects exactly these cells, from the first to the last */
  select(span: Span): void {
    this.asked += 1
    this.granularity = 'cell'
    this.anchor = { start: span.start, end: span.start }
    this.head = { start: span.end, end: span.end }
    this.changed()
  }

  clear(): void {
    if (!this.anchor) return
    this.asked += 1
    this.anchor = null
    this.head = null
    this.changed()
  }

  /** The selected text as a program wrote it, or the empty string */
  async text(): Promise<string> {
    const span = this.span
    return span ? this.source.textOf(span) : ''
  }

  /** Line numbers are about to mean something else: a selection made in them is gone */
  reset(): void {
    this.clear()
  }

  /** Grows an end to the word or line it is in, when that is what is being selected by */
  private async widen(position: Position, set: (span: Span) => void): Promise<void> {
    if (this.granularity === 'cell') return
    const ask = (this.asked += 1)
    const span = await (this.granularity === 'word' ? this.source.wordAt(position) : this.source.lineAt(position))
    if (ask !== this.asked || !span) return
    set(span)
    this.changed()
  }
}
