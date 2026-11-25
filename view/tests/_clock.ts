import type { Clock } from '../src/clock.ts'

interface Entry {
  readonly id: number
  readonly at: number
  readonly run: () => void
  cancelled: boolean
}

/** Deterministic in-memory `Clock` for singleton-free tests. */
export class TestClock implements Clock {
  private now = 0
  private nextId = 1
  private readonly queue: Entry[] = []

  get pending(): number {
    return this.queue.filter((entry) => !entry.cancelled).length
  }

  after(ms: number, run: () => void): () => void {
    const entry: Entry = { id: this.nextId++, at: this.now + Math.max(0, ms), run, cancelled: false }
    this.queue.push(entry)
    return () => {
      entry.cancelled = true
    }
  }

  /** Advances virtual time by `ms`, firing due timers in chronological (then FIFO) order. */
  advance(ms: number): void {
    const target = this.now + ms
    while (true) {
      let bestIndex = -1
      for (let i = 0; i < this.queue.length; i += 1) {
        const entry = this.queue[i]!
        if (entry.cancelled || entry.at > target) continue
        if (bestIndex === -1) {
          bestIndex = i
          continue
        }
        const best = this.queue[bestIndex]!
        if (entry.at < best.at || (entry.at === best.at && entry.id < best.id)) bestIndex = i
      }
      if (bestIndex === -1) break
      const [entry] = this.queue.splice(bestIndex, 1)
      if (!entry || entry.cancelled) continue
      this.now = entry.at
      entry.run()
    }
    this.now = target
    for (let i = this.queue.length - 1; i >= 0; i -= 1) {
      if (this.queue[i]!.cancelled) this.queue.splice(i, 1)
    }
  }

  /** Advances virtual time and flushes microtasks so async callbacks settle cleanly. */
  async advanceAsync(ms: number): Promise<void> {
    this.advance(ms)
    await Promise.resolve()
    await Promise.resolve()
  }
}
