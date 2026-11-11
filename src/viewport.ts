import type { RowStore } from './row-store.ts'

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

  constructor(private readonly store: RowStore) {}

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
    if (this.store.scrollback === 0 || this.held >= this.store.firstActive) this.followingOutput = true
  }

  /** The line at the top of the view, fractional while a scroll is part-way between two */
  get top(): number {
    if (this.following) return this.store.firstActive
    return Math.min(Math.max(this.held, this.store.oldest), this.store.firstActive)
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

  /** What a scrollbar draws: how much there is, where the view is in it, and how much it shows */
  get extent(): { total: number; position: number; shown: number } {
    return {
      total: this.store.scrollback + this.store.screenRows,
      position: this.top - this.store.oldest,
      shown: this.store.screenRows,
    }
  }

  /** Moves by `lines`, negative for back. Reaching the screen is following it again */
  scrollBy(lines: number): void {
    this.scrollTo(this.top + lines)
  }

  /** Shows from `line`. At or past the screen, the view follows the output again */
  scrollTo(line: number): void {
    const clamped = Math.max(line, this.store.oldest)
    if (clamped >= this.store.firstActive) {
      this.followingOutput = true
      return
    }
    this.followingOutput = false
    this.held = clamped
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
