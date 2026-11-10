import { PLAIN, type Frame, type Row, type Rows, type Style } from './source.ts'

/**
 * How many lines a store keeps before it lets the ones furthest from the screen go.
 *
 * A row is what was sent, so keeping one costs memory and letting one go costs a fetch when the user
 * scrolls back to it. Fifty thousand lines is past what anyone scrolls through by hand, and the owner
 * holds the rest.
 */
export const KEPT_LINES = 50_000

/**
 * The rows a view has been sent, by absolute line.
 *
 * **A cache and never the truth.** The terminal's owner holds the screen and its history; this holds
 * whatever of it has arrived, so that scrolling through lines already seen costs nothing and a line not
 * yet seen is something to ask for (`missing`). What keeps it honest is three things the owner says: the
 * epoch, which empties it; where history starts, below which lines are gone; and `staleFrom`, which names
 * history that changed without being resent.
 */
export class RowStore {
  /** `-1` until the first frame: no line number means anything yet */
  epoch = -1
  firstActive = 0
  screenRows = 0
  columns = 0
  scrollback = 0
  /** Every style a kept row uses. `styles[0]` is `PLAIN`, so a row's `0` needs no lookup to mean "unstyled" */
  readonly styles: Style[] = [PLAIN]
  /** Every link a kept row uses; `links[0]` is no link */
  readonly links: string[] = ['']

  private readonly rows = new Map<number, Row>()
  private readonly styleIndex = new Map<string, number>([[styleKey(PLAIN), 0]])
  private readonly linkIndex = new Map<string, number>([['', 0]])

  /** The oldest line the owner still holds */
  get oldest(): number {
    return this.firstActive - this.scrollback
  }

  /** One past the last line of the screen */
  get end(): number {
    return this.firstActive + this.screenRows
  }

  get size(): number {
    return this.rows.size
  }

  row(line: number): Row | undefined {
    return this.rows.get(line)
  }

  /**
   * Takes a frame, answering which lines now differ from what was held.
   *
   * `reset` is true when nothing held before is worth anything: the epoch moved, so a caller redraws all
   * of it rather than the lines named.
   */
  apply(frame: Frame): { reset: boolean; changed: number[] } {
    const reset = frame.epoch !== this.epoch
    if (reset) {
      this.rows.clear()
      this.epoch = frame.epoch
    }
    this.firstActive = frame.firstActive
    this.screenRows = frame.screenRows
    this.columns = frame.columns
    this.scrollback = frame.scrollback

    // A set, because a line can be both let go as stale and resent by the same frame
    const changed = new Set<number>()
    // History the owner changed and did not resend. Before the new rows, so one of them that is in that
    // range is kept
    if (frame.staleFrom !== undefined) {
      for (let line = frame.staleFrom; line < frame.firstActive; line += 1) {
        if (this.rows.delete(line)) changed.add(line)
      }
    }
    for (const line of this.keep(frame)) changed.add(line)
    this.forget()
    return { reset, changed: [...changed].sort((a, b) => a - b) }
  }

  /**
   * Takes rows that were asked for. An answer about another epoch names lines that no longer exist, so it
   * is dropped whole.
   */
  absorb(fetched: Rows & { readonly epoch: number }): number[] {
    if (fetched.epoch !== this.epoch) return []
    // The screen is only ever told by a frame: a fetched copy of a line a program can still write to may
    // be older than the frame that last described it
    const history = fetched.rows.filter((row) => row.line < this.firstActive)
    const kept = this.keep({ ...fetched, rows: history })
    this.forget()
    return kept
  }

  /**
   * The runs of history lines in `first .. first + count` that the owner holds and this does not, each as
   * `[first, count]`.
   *
   * **History only.** The screen is told by frames and by nothing else, so a line of it that is not here
   * is one the owner has yet to send, not one to ask for.
   */
  missing(first: number, count: number): Array<[number, number]> {
    const from = Math.max(first, this.oldest)
    const to = Math.min(first + count, this.firstActive)
    const runs: Array<[number, number]> = []
    let start = -1
    for (let line = from; line < to; line += 1) {
      if (this.rows.has(line)) {
        if (start !== -1) runs.push([start, line - start])
        start = -1
      } else if (start === -1) {
        start = line
      }
    }
    if (start !== -1) runs.push([start, to - start])
    return runs
  }

  /**
   * Lets go of every line from `line` up. For a view that stopped listening: the lines that were on the
   * screen then may have changed and scrolled into history since, and nothing said so.
   */
  forgetFrom(line: number): void {
    for (const held of this.rows.keys()) {
      if (held >= line) this.rows.delete(held)
    }
  }

  /** Keeps `rows`, with their styles and links moved onto this store's own tables */
  private keep(message: Rows): number[] {
    const styles = message.styles.map((style) => this.styleFor(style))
    const links = message.links.map((link) => this.linkFor(link))
    const kept: number[] = []
    for (const row of message.rows) {
      this.rows.set(row.line, {
        ...row,
        style: row.style.map((index) => styles[index] ?? 0),
        link: row.link.map((index) => links[index] ?? 0),
      })
      kept.push(row.line)
    }
    return kept
  }

  /** Lets go of what the owner no longer holds, then of what is furthest from the screen past `KEPT_LINES` */
  private forget(): void {
    const oldest = this.oldest
    for (const line of this.rows.keys()) {
      if (line < oldest || line >= this.end) this.rows.delete(line)
    }
    if (this.rows.size <= KEPT_LINES) return
    // The furthest from the screen are the oldest lines, and a Map keeps insertion order rather than line
    // order, so they are found by sorting
    const lines = [...this.rows.keys()].sort((a, b) => a - b)
    for (const line of lines.slice(0, this.rows.size - KEPT_LINES)) this.rows.delete(line)
  }

  private styleFor(style: Style): number {
    const key = styleKey(style)
    let index = this.styleIndex.get(key)
    if (index === undefined) {
      index = this.styles.length
      this.styles.push(style)
      this.styleIndex.set(key, index)
    }
    return index
  }

  private linkFor(link: string): number {
    let index = this.linkIndex.get(link)
    if (index === undefined) {
      index = this.links.length
      this.links.push(link)
      this.linkIndex.set(link, index)
    }
    return index
  }
}

function styleKey(style: Style): string {
  return `${style.fg}|${style.bg}|${style.underlineColor}|${style.flags}`
}
