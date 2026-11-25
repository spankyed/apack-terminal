import type { RowStore } from './row-store.ts'

/** A run of lines folded to one header row: `first` is the header, and the lines after it up to `last` are hidden */
export interface FoldRange {
  readonly first: number
  readonly last: number
}

/**
 * Which lines a view is showing.
 *
 * **Either following the output or holding a line, and never anything in between.** Following, the view
 * shows the screen. Scrolled back, it shows from an absolute line and goes on showing from that line
 * whatever a program writes — there is nothing to compensate for, because the line a user is reading keeps
 * its number while new ones are added below it. What can move a held view is the owner letting that line
 * go, and then it shows the oldest line there is.
 */
export class Viewport {
  /** The line at the top while scrolled back, which may be between two lines mid-scroll. Unused while following */
  private held = 0
  private followingOutput = true
  /** The folded runs, sorted and not overlapping. Their hidden lines take no row, so scrolling counts rows past them */
  private folds: readonly FoldRange[] = []
  /** Prefix sums of hidden line counts before each fold: `hiddenBefore[i]` is hidden lines in `folds[0..i-1]` */
  private hiddenBefore: number[] = [0]

  constructor(private readonly store: RowStore) {}

  /** Sets the folded runs the view draws; a fold's hidden lines are passed over by every scroll */
  setFolds(folds: readonly FoldRange[]): void {
    this.folds = folds
    this.hiddenBefore = new Array<number>(folds.length + 1)
    this.hiddenBefore[0] = 0
    for (let i = 0; i < folds.length; i += 1) {
      this.hiddenBefore[i + 1] = this.hiddenBefore[i]! + (folds[i]!.last - folds[i]!.first)
    }
    if (!this.followingOutput) {
      const min = this.visibleOldest()
      if (this.held < min) {
        this.held = min
      } else {
        const hidden = this.hiddenIn(Math.floor(this.held))
        if (hidden) this.held = hidden.first < this.store.oldest ? hidden.last + 1 : hidden.first
      }
      if (this.held >= this.store.firstActive) this.followingOutput = true
    }
  }

  /** The oldest non-hidden line in the store: advances past any fold covering `store.oldest` */
  private visibleOldest(): number {
    const oldest = this.store.oldest
    const hidden = this.hiddenIn(oldest)
    return hidden ? hidden.last + 1 : oldest
  }

  /** The fold a line is hidden in: a header is not, and neither is a line outside every fold */
  hiddenIn(line: number): FoldRange | undefined {
    const folds = this.folds
    let low = 0
    let high = folds.length
    while (low < high) {
      const mid = (low + high) >> 1
      if (folds[mid]!.first < line) low = mid + 1
      else high = mid
    }
    const fold = folds[low - 1]
    return fold && line <= fold.last ? fold : undefined
  }

  /**
   * Whether the view shows the screen and moves with it.
   *
   * **One fact, and only a scroll or the history going makes it false or true.** It is not worked out from
   * how much history there is: a view whose history was erased under it is following from then on, and
   * stays following when the next line scrolls into a history that is no longer empty.
   */
  get following(): boolean {
    return this.followingOutput
  }

  /**
   * The store took a frame. With no history left, or none at or above the line held, there is nothing to
   * hold: the view is following.
   */
  settle(): void {
    if (this.store.scrollback === 0 || Math.max(this.held, this.visibleOldest()) >= this.store.firstActive) this.followingOutput = true
  }

  /** The line at the top of the view, fractional while a scroll is part-way between two */
  get top(): number {
    if (this.following) return this.store.firstActive
    return Math.min(Math.max(this.held, this.visibleOldest()), this.store.firstActive)
  }

  /** The whole lines the view needs to draw: `[first, count]`, one more than the screen when between two */
  get visible(): [first: number, count: number] {
    const top = this.top
    const first = Math.floor(top)
    return [first, this.store.screenRows + (top === first ? 0 : 1)]
  }

  /** How far the grid is shifted up, as a fraction of a row */
  get offset(): number {
    const top = this.top
    return top - Math.floor(top)
  }

  /**
   * What a scrollbar draws, in rows: how many there are, where the view is among them, and how many it shows. A fold is one
   * row, so the thumb moves a notch's worth for each row that is drawn, not each line.
   */
  get extent(): { total: number; position: number; shown: number } {
    const { oldest, firstActive, screenRows } = this.store
    return {
      total: this.rowsBetween(oldest, firstActive) + screenRows,
      position: this.rowsBetween(oldest, this.top),
      shown: screenRows,
    }
  }

  /**
   * Moves by `rows`, negative for back. A row is a drawn one: a fold's hidden lines are passed over without counting, so
   * every step changes what is on screen. Reaching the screen is following it again.
   */
  scrollBy(rows: number): void {
    this.scrollTo(this.stepRows(this.top, rows))
  }

  /** Shows from the row `position` counted from the oldest line, as the scrollbar measures it */
  scrollToPosition(position: number): void {
    this.scrollTo(this.stepRows(this.visibleOldest(), position))
  }

  /** Shows from `line`. At or past the screen, the view follows the output again */
  scrollTo(line: number): void {
    if (!Number.isFinite(line)) return
    let clamped = Math.max(line, this.visibleOldest())
    if (clamped >= this.store.firstActive) {
      this.followingOutput = true
      return
    }
    const hidden = this.hiddenIn(Math.floor(clamped))
    if (hidden) {
      clamped = hidden.first < this.store.oldest ? hidden.last + 1 : hidden.first
      if (clamped >= this.store.firstActive) {
        this.followingOutput = true
        return
      }
    }
    this.followingOutput = false
    this.held = clamped
  }

  /**
   * The line `rows` drawn rows from `line`. A line that is hidden starts from its fold's header, and a fraction stays a
   * fraction of the row it lands in: a whole row steps over a fold as one row, and a part-row never lands on a hidden line.
   */
  stepRows(line: number, rows: number): number {
    if (!Number.isFinite(line)) return this.store.firstActive
    const whole = Math.floor(line)
    const hidden = this.hiddenIn(whole)
    const startRow = hidden ? hidden.first : whole
    const part = hidden ? 0 : line - whole
    if (!Number.isFinite(rows) || rows === 0) return startRow + part
    const total = part + rows
    const wholeStep = Math.floor(total)
    const frac = total - wholeStep
    return this.lineAtDrawn(this.drawnOf(startRow) + wholeStep) + frac
  }

  /** Drawn-row coordinate of a non-hidden integer line */
  private drawnOf(line: number): number {
    const folds = this.folds
    let low = 0
    let high = folds.length
    while (low < high) {
      const mid = (low + high) >> 1
      if (folds[mid]!.first < line) low = mid + 1
      else high = mid
    }
    return line - this.hiddenBefore[low]!
  }

  /** Non-hidden integer line whose drawn-row coordinate is `drawn` */
  private lineAtDrawn(drawn: number): number {
    const folds = this.folds
    let low = 0
    let high = folds.length
    while (low < high) {
      const mid = (low + high) >> 1
      if (folds[mid]!.first - this.hiddenBefore[mid]! < drawn) low = mid + 1
      else high = mid
    }
    return drawn + this.hiddenBefore[low]!
  }

  /** Total hidden lines at or below integer line `limit` */
  private hiddenUpTo(limit: number): number {
    const folds = this.folds
    let low = 0
    let high = folds.length
    while (low < high) {
      const mid = (low + high) >> 1
      if (folds[mid]!.first < limit) low = mid + 1
      else high = mid
    }
    if (low === 0) return 0
    const fold = folds[low - 1]!
    return this.hiddenBefore[low - 1]! + Math.min(fold.last, limit) - fold.first
  }

  /** The number of drawn rows from line `from` up to `to`, a fold's hidden lines taking none: negative when `to` is above `from` */
  rowsBetween(from: number, to: number): number {
    if (to < from) return -this.rowsBetween(to, from)
    let rows = to - from
    const through = Math.ceil(to) - 1
    if (through >= from && this.folds.length > 0) {
      rows -= this.hiddenUpTo(through) - this.hiddenUpTo(Math.floor(from) - 1)
    }
    return rows
  }

  /** Shows the screen and follows it */
  follow(): void {
    this.followingOutput = true
  }

  /**
   * The line numbers are about to change (an epoch moved): hold the same distance from the screen rather
   * than a number that now names something else.
   *
   * Call before the store takes the new frame, and call the function it returns after.
   */
  acrossReset(): () => void {
    const back = this.following ? 0 : this.store.firstActive - this.top
    return () => {
      if (back > 0) this.scrollTo(this.store.firstActive - back)
    }
  }
}
