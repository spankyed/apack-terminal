import { type CellSpan, type SearchPatch, type SearchSnapshot, type TerminalSource } from './source.ts'
import { MATCH_HIGHLIGHTS, sortMatchesNewestFirst } from './view-types.ts'

export interface SearchHost {
  readonly source: TerminalSource
  isDisposed(): boolean
  epoch(): number
  columns(): number
  onFound(found: { current: number; total: number; more: boolean }): void
  showMatch(match: CellSpan | undefined): void
  schedule(): void
}

/**
 * Incremental server-side `SearchWatch` state (zero client-side polling).
 */
export class SearchController {
  private needle = ''
  private matchCase = false
  private matches: CellSpan[] = []
  private moreMatches = false
  private currentMatch = -1
  private finding = 0
  private searchPending = false
  private matchVersion = 0

  constructor(private readonly host: SearchHost) {}

  get version(): number {
    return this.matchVersion
  }

  get hasMatches(): boolean {
    return this.matches.length > 0
  }

  get found(): { current: number; total: number; more: boolean } {
    return { current: this.currentMatch + 1, total: this.matches.length, more: this.moreMatches }
  }

  async find(needle: string, options: { matchCase?: boolean } = {}): Promise<number> {
    if (this.host.isDisposed()) return 0
    const asked = (this.finding += 1)
    this.needle = needle
    this.matchCase = options.matchCase ?? false
    if (needle === '') {
      this.searchPending = false
      this.matches = []
      this.moreMatches = false
      this.currentMatch = -1
      this.matchVersion += 1
      this.host.onFound(this.found)
      this.host.schedule()
      void this.host.source.setSearch('', this.matchCase).catch(() => {})
      return 0
    }
    this.searchPending = true
    try {
      const snapshot = await this.host.source.setSearch(needle, this.matchCase)
      if (this.host.isDisposed() || asked !== this.finding) return this.matches.length
      this.searchPending = false
      this.applySnapshot(snapshot, true)
      return this.matches.length
    } catch {
      if (!this.host.isDisposed() && asked === this.finding) {
        this.searchPending = false
        this.matches = []
        this.moreMatches = false
        this.currentMatch = -1
        this.matchVersion += 1
        this.host.onFound(this.found)
        this.host.schedule()
      }
      return 0
    }
  }

  findNext(direction: -1 | 1 = 1): void {
    if (this.matches.length === 0) return
    this.currentMatch = (this.currentMatch + direction + this.matches.length) % this.matches.length
    this.matchVersion += 1
    this.host.showMatch(this.matches[this.currentMatch])
  }

  applySnapshot(snapshot: SearchSnapshot, resetCurrent: boolean): void {
    if (this.needle === '' || this.searchPending) return
    const epoch = this.host.epoch()
    if (epoch >= 0 && snapshot.epoch !== epoch) return
    const prev = this.matches[this.currentMatch]
    const sorted = sortMatchesNewestFirst(snapshot.matches)
    const held = sorted.slice(0, MATCH_HIGHLIGHTS)
    const more = snapshot.truncated || sorted.length > MATCH_HIGHLIGHTS
    let current: number
    if (held.length === 0) {
      current = -1
    } else if (resetCurrent) {
      current = 0
    } else {
      const still = prev
        ? held.findIndex((m) => m.start.line === prev.start.line && m.start.col === prev.start.col)
        : -1
      current = still !== -1 ? still : Math.max(0, Math.min(this.currentMatch, held.length - 1))
    }
    const before = this.found
    this.matches = held
    this.moreMatches = more
    this.currentMatch = current
    this.matchVersion += 1
    const after = this.found
    if (resetCurrent) {
      this.host.showMatch(this.matches[this.currentMatch])
    } else {
      this.host.schedule()
    }
    if (after.current !== before.current || after.total !== before.total || after.more !== before.more) {
      this.host.onFound(after)
    }
  }

  applyDelta(patch: SearchPatch): void {
    if (this.needle === '' || this.searchPending) return
    const epoch = this.host.epoch()
    if (epoch >= 0 && patch.epoch !== epoch) return
    const prev = this.matches[this.currentMatch]
    const kept = this.matches.filter(
      (m) => m.end.line < patch.fromLine || m.start.line >= patch.toLine,
    )
    const merged = sortMatchesNewestFirst([...kept, ...patch.added])
    const held = merged.slice(0, MATCH_HIGHLIGHTS)
    const more = patch.truncated || merged.length > MATCH_HIGHLIGHTS
    let current: number
    if (held.length === 0) {
      current = -1
    } else {
      const still = prev
        ? held.findIndex((m) => m.start.line === prev.start.line && m.start.col === prev.start.col)
        : -1
      current = still !== -1 ? still : Math.max(0, Math.min(this.currentMatch, held.length - 1))
    }
    const before = this.found
    this.matches = held
    this.moreMatches = more
    this.currentMatch = current
    this.matchVersion += 1
    const after = this.found
    if (after.current !== before.current || after.total !== before.total || after.more !== before.more) {
      this.host.onFound(after)
    }
    this.host.schedule()
  }

  resetForEpoch(): void {
    const hadMatches = this.matches.length > 0 || this.moreMatches
    // Keep `this.needle` across epoch resets: the broker preserves the active SearchWatch and
    // immediately pushes a fresh `SearchMatches` for the new epoch.
    this.matches = []
    this.moreMatches = false
    this.currentMatch = -1
    this.matchVersion += 1
    if (hadMatches) this.host.onFound(this.found)
  }

  matchesOn(line: number): Array<{ from: number; to: number; current: boolean }> {
    const matches = this.matches
    let low = 0
    let high = matches.length
    while (low < high) {
      const middle = (low + high) >> 1
      if (matches[middle]!.start.line > line) low = middle + 1
      else high = middle
    }
    const cols = this.host.columns()
    const on: Array<{ from: number; to: number; current: boolean }> = []
    for (let index = low; index < matches.length; index += 1) {
      const match = matches[index]!
      if (match.end.line < line) break
      on.push({
        from: line === match.start.line ? match.start.col : 0,
        to: line === match.end.line ? match.end.col + 1 : cols,
        current: index === this.currentMatch,
      })
    }
    return on
  }
}
