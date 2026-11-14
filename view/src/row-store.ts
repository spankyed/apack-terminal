import type { Fetched, Frame, Row } from './source.ts'

/**
 * How many lines a store keeps before it lets the ones furthest from the viewer go.
 *
 * A row is what was sent, so keeping one costs memory and letting one go costs a fetch when the user
 * scrolls back to it. Fifty thousand lines is past what anyone scrolls through by hand, and the owner
 * holds the rest.
 */
export const KEPT_LINES = 50_000

/**
 * How far past `KEPT_LINES` a store grows before it lets lines go, and then it goes back down to
 * `KEPT_LINES`: finding the furthest lines reads every line held, so it is done once for this many arrivals
 * and not once for each.
 */
export const KEPT_SLACK = KEPT_LINES >> 3

/**
 * The rows a view has been sent, by absolute line.
 *
 * **A cache and never the truth.** The terminal's owner holds the screen and its history; this holds
 * whatever of it has arrived, so that scrolling through lines already seen costs nothing and a line not
 * yet seen is something to ask for (`missing`). What keeps it honest is three things the owner says: the
 * epoch, which empties it; where history starts, below which lines are gone; and `staleFrom`, which names
 * history that changed without being resent.
 *
 * **A row is kept as it came.** Its styles and links are its own, so there is no table here for rows to
 * share and none to keep to the size of what is still held: what a row cost is given back when the row is
 * let go, and the row a frame brought is the row that is drawn.
 */
export class RowStore {
  /** `-1` until the first frame: no line number means anything yet */
  epoch = -1
  firstActive = 0
  screenRows = 0
  columns = 0
  scrollback = 0

  private readonly rows = new Map<number, Row>()
  /**
   * History lines the owner answered for with no row: it does not hold them, so they are not missing. True
   * until the owner says what it holds has changed — the epoch, where history starts, or `staleFrom`.
   */
  private readonly absent = new Set<number>()
  /**
   * History lines held that may no longer be what the owner holds, and are asked for again (`doubtPictured`).
   * A doubted row is still drawn: it is the best there is until the answer comes.
   */
  private readonly doubted = new Set<number>()
  /**
   * How many times `doubtPictured` has been called: what an asker notes when it asks, so `absorb` can tell
   * an answer asked for before the images last changed from one asked for since.
   */
  imagesChanged = 0

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
   * of it rather than the lines named. `near` is the line the viewer is at, which is what is kept longest;
   * left out, that is the screen.
   *
   * A frame need not bring every row it owes: a screen too long for one message comes over several, and
   * the lines the first of them leaves out are simply not held until the next.
   */
  apply(frame: Frame, near = frame.firstActive): { reset: boolean; changed: number[] } {
    const reset = frame.epoch !== this.epoch
    const oldest = this.oldest
    if (reset) {
      this.rows.clear()
      this.doubted.clear()
      this.epoch = frame.epoch
    }
    this.firstActive = frame.firstActive
    this.screenRows = frame.screenRows
    this.columns = frame.columns
    this.scrollback = frame.scrollback
    if (reset || frame.staleFrom !== undefined || this.oldest !== oldest) this.absent.clear()

    // A set, because a line can be both let go as stale and resent by the same frame
    const changed = new Set<number>()
    // History the owner changed and did not resend. Before the new rows, so one of them that is in that
    // range is kept
    if (frame.staleFrom !== undefined) {
      for (let line = frame.staleFrom; line < frame.firstActive; line += 1) {
        if (this.rows.delete(line)) changed.add(line)
        this.doubted.delete(line)
      }
    }
    for (const row of frame.rows) {
      this.rows.set(row.line, row)
      this.absent.delete(row.line)
      this.doubted.delete(row.line)
      changed.add(row.line)
    }
    this.forget(near)
    return { reset, changed: [...changed].sort((a, b) => a - b) }
  }

  /**
   * Takes the answer to a fetch, answering the lines it changed what is known of: the ones it brought, and
   * the ones it says the owner does not hold. An answer about another epoch names lines that no longer
   * exist, so it is dropped whole. `near` is the line the viewer is at, as for `apply`.
   *
   * **What the answer covers and has no row for is not held by the owner**, and is remembered as that, so
   * it is not missing. What it does not cover it says nothing about.
   *
   * **An answer asked for before the images last changed may be of either side of the change.** `asked` is
   * `imagesChanged` when the fetch was made. An answer and the frame that says the images changed can
   * arrive together, and the frame is taken first whichever was sent first, so there is no telling from
   * the order which of them the answer's rows are older than. A row of placeholder cells brought by such
   * an answer is taken, and stays doubted: asked for once more, by a fetch made after the change.
   */
  absorb(fetched: Fetched, near = this.firstActive, asked = this.imagesChanged): number[] {
    if (fetched.epoch !== this.epoch) return []
    const told: number[] = []
    // The screen is only ever told by a frame: a fetched copy of a line a program can still write to may
    // be older than the frame that last described it
    const from = Math.max(fetched.first, this.oldest)
    const to = Math.min(fetched.first + fetched.count, this.firstActive)
    const brought = new Set<number>()
    for (const row of fetched.rows) {
      if (row.line < from || row.line >= to) continue
      this.rows.set(row.line, row)
      this.absent.delete(row.line)
      if (row.images !== undefined && asked !== this.imagesChanged) this.doubted.add(row.line)
      else this.doubted.delete(row.line)
      brought.add(row.line)
      told.push(row.line)
    }
    for (let line = from; line < to; line += 1) {
      if (brought.has(line) || this.absent.has(line)) continue
      // A row asked for again and not brought is one the owner no longer holds
      if (this.rows.has(line) && !this.doubted.delete(line)) continue
      this.rows.delete(line)
      this.absent.add(line)
      told.push(line)
    }
    this.forget(near)
    return told
  }

  /**
   * The runs of history lines in `first .. first + count` that the owner holds, as far as it has said, and
   * this does not: each as `[first, count]`.
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
      if ((this.rows.has(line) && !this.doubted.has(line)) || this.absent.has(line)) {
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
   * The images changed: a history row of placeholder cells may show something else now — another picture
   * under the same number, nothing, or a picture where it showed none — and the owner sends history again
   * only when asked. So each such row is doubted: kept and drawn, and among what `missing` names until it
   * is brought again. The screen's rows are not: a frame brings those.
   */
  doubtPictured(): void {
    this.imagesChanged += 1
    for (const [line, row] of this.rows) {
      if (line < this.firstActive && row.images !== undefined) this.doubted.add(line)
    }
  }

  /** The images the rows held show part of, by number */
  pictured(): Set<number> {
    const images = new Set<number>()
    for (const row of this.rows.values()) {
      for (const piece of row.images?.pieces ?? []) images.add(piece.image)
    }
    return images
  }

  /**
   * Lets go of what the owner no longer holds, then, past `KEPT_LINES` and its slack, of the history that
   * is furthest from `near`. Never of the screen: a line of it is told by a frame and by nothing else, so
   * one let go would stay blank until a program wrote to it again.
   *
   * **Furthest from the viewer, not from the screen.** Someone reading far back in a long history is sent
   * exactly the lines furthest from the screen; letting those go first would let go of what was just
   * fetched for them, and it would be fetched again for ever.
   */
  private forget(near: number): void {
    const oldest = this.oldest
    for (const line of this.rows.keys()) {
      if (line < oldest || line >= this.end) this.rows.delete(line)
    }
    for (const line of this.doubted) if (!this.rows.has(line)) this.doubted.delete(line)
    if (this.rows.size <= KEPT_LINES + KEPT_SLACK) return
    // The widest reach around `near` that holds no more than may be kept, found by halving: at most two
    // lines are any one distance away, so the count is within one of the bound
    const within = (reach: number) => {
      let count = 0
      for (const line of this.rows.keys()) if (line >= this.firstActive || Math.abs(line - near) <= reach) count += 1
      return count
    }
    let reach = 0
    let beyond = Math.max(near - oldest, this.end - near, 0) + 1
    while (beyond - reach > 1) {
      const middle = reach + Math.floor((beyond - reach) / 2)
      if (within(middle) <= KEPT_LINES) reach = middle
      else beyond = middle
    }
    for (const line of this.rows.keys()) {
      if (line < this.firstActive && Math.abs(line - near) > reach) {
        this.rows.delete(line)
        this.doubted.delete(line)
      }
    }
  }
}
