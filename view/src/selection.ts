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
  /** The other end, likewise, and the cell it was last moved to */
  private head: Span | null = null
  private headAt: Position | null = null
  private granularity: Granularity = 'cell'
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
    this.changed()
  }

  /**
   * The selected text as a program wrote it, or the empty string. Only of lines the terminal still holds:
   * a selection made before its history was trimmed can name lines that are gone, and a source asked for
   * the text of those has none to give for any of it.
   */
  async text(): Promise<string> {
    const span = this.span
    if (!span) return ''
    const held = this.held?.()
    if (!held) return this.source.textOf(span)
    const start = span.start.line < held.oldest ? { line: held.oldest, column: 0 } : span.start
    const end = span.end.line >= held.end ? { line: held.end - 1, column: Math.max(0, this.columns() - 1) } : span.end
    return before(end, start) ? '' : this.source.textOf({ start, end })
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
