/**
 * A clock, handed in: `after` runs `run` once after `ms` milliseconds unless the returned cancel function
 * is called first. Matches `@apackdev/clock`, `@apackdev/link`, and `@apackdev/state` structurally so a
 * host can share one timer queue across the renderer with no runtime dependency here.
 */
export interface Clock {
  after(ms: number, run: () => void): () => void
}

/** The default clock, backed by the host's own `setTimeout` and `clearTimeout`. */
export const systemClock: Clock = {
  after(ms, run) {
    const id = globalThis.setTimeout(run, ms)
    return () => { globalThis.clearTimeout(id) }
  },
}

/**
 * One pending timer slot backed by a `Clock`.
 *
 * Cancels any previous firing before arming a new one, clears its handle before invoking `run`, ignores
 * stale or cancelled firings, and handles clocks that run `after(0, run)` synchronously without keeping
 * an already-fired cancel handle.
 */
export class Timer {
  private stop: (() => void) | null = null
  private token = 0

  constructor(private readonly clock: Clock) {}

  get active(): boolean {
    return this.stop !== null
  }

  /** Arms `run` after `ms` milliseconds, cancelling any pending timer in this slot first. */
  arm(ms: number, run: () => void): void {
    this.cancel()
    const mine = ++this.token
    let fired = false
    const stop = this.clock.after(ms, () => {
      if (fired || this.token !== mine) return
      fired = true
      this.stop = null
      run()
    })
    if (!fired) {
      if (this.token === mine) this.stop = stop
      else stop()
    }
  }

  /** Cancels any pending timer in this slot. Safe to call repeatedly or re-entrantly. */
  cancel(): void {
    this.token += 1
    const stop = this.stop
    this.stop = null
    stop?.()
  }
}
