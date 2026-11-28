/**
 * Page-chunked terminal row cache (`PAGE_LINES = 256`) holding v2 `PackedRow`s by absolute line.
 *
 * **Why page-chunked:** Furthest-from-viewer eviction scans `Map<pageIdx, RowPage>` (~200 entries
 * for 50,000 lines) instead of 56,000 individual line entries — ~280× faster eviction with zero
 * per-line Map allocation churn!
 */
import { RowFlag, type FrameUpdate, type PackedRow } from './source.ts'

/** Number of consecutive absolute lines stored per page. */
export const PAGE_LINES = 256

/** Target ceiling on retained cached lines before furthest pages are evicted. */
export const KEPT_LINES = 50_000

/** Slack above `KEPT_LINES` before running an $O(\text{pages})$ eviction sweep. */
export const KEPT_SLACK = KEPT_LINES >> 3

const MAX_ABSENT = 16_384

interface RowPage {
  readonly baseLine: number
  readonly slots: Array<PackedRow | undefined>
  count: number
}

export class RowStore {
  /** `-1` until the first frame arrives. */
  epoch = -1
  firstActive = 0
  screenRows = 0
  columns = 0
  scrollback = 0
  imagesGen = 0

  private readonly pages = new Map<number, RowPage>()
  private totalRows = 0
  private readonly picturedLines = new Set<number>()
  private readonly absent = new Set<number>()
  private readonly doubted = new Set<number>()
  /** Increments whenever `imagesGen` changes so in-flight history fetches carrying stale placeholders are rejected. */
  imagesChanged = 0

  /** The oldest absolute line the broker still holds in scrollback. */
  get oldest(): number {
    return Math.max(0, this.firstActive - this.scrollback)
  }

  /** One past the last line of the active screen. */
  get end(): number {
    return this.firstActive + this.screenRows
  }

  /** Total cached `PackedRow`s currently held across all pages. */
  get size(): number {
    return this.totalRows
  }

  /** Number of allocated 256-line pages (useful for diagnostics & O(pages) tests). */
  get pageCount(): number {
    return this.pages.size
  }

  row(line: number): PackedRow | undefined {
    if (line < 0) return undefined
    const pageIdx = Math.floor(line / PAGE_LINES)
    const page = this.pages.get(pageIdx)
    if (!page) return undefined
    return page.slots[line - page.baseLine]
  }

  private setRow(row: PackedRow): void {
    const line = row.line
    const pageIdx = Math.floor(line / PAGE_LINES)
    let page = this.pages.get(pageIdx)
    if (!page) {
      page = { baseLine: pageIdx * PAGE_LINES, slots: new Array<PackedRow | undefined>(PAGE_LINES), count: 0 }
      this.pages.set(pageIdx, page)
    }
    const offset = line - page.baseLine
    const prev = page.slots[offset]
    if (prev === undefined) {
      page.count += 1
      this.totalRows += 1
    }
    page.slots[offset] = row
    if ((row.flags & RowFlag.HAS_PLACEHOLDER) !== 0 && row.pieces && row.pieces.length > 0) {
      this.picturedLines.add(line)
    } else {
      this.picturedLines.delete(line)
    }
    this.absent.delete(line)
    this.doubted.delete(line)
  }

  private deleteLine(line: number): void {
    this.doubted.delete(line)
    const pageIdx = Math.floor(line / PAGE_LINES)
    const page = this.pages.get(pageIdx)
    if (!page) return
    const offset = line - page.baseLine
    if (page.slots[offset] === undefined) return
    page.slots[offset] = undefined
    page.count -= 1
    this.totalRows -= 1
    this.picturedLines.delete(line)
    if (page.count === 0) {
      this.pages.delete(pageIdx)
    }
  }

  private clearAll(): void {
    this.pages.clear()
    this.totalRows = 0
    this.picturedLines.clear()
    this.absent.clear()
    this.doubted.clear()
  }

  /** Applies an authoritative v2 `FrameUpdate`, returning whether the epoch reset and which lines changed. */
  apply(frame: FrameUpdate, near = frame.firstActiveLine): { readonly reset: boolean; readonly changed: number[] } {
    const reset = frame.epoch !== this.epoch
    const prevOldest = this.oldest
    const prevEnd = this.end
    const prevFirstActive = this.firstActive

    const imagesBumped = !reset && frame.imagesGen !== this.imagesGen
    if (reset) {
      this.clearAll()
      this.epoch = frame.epoch
      this.imagesGen = frame.imagesGen
    } else if (imagesBumped) {
      this.imagesGen = frame.imagesGen
    }

    this.firstActive = Math.max(0, frame.firstActiveLine)
    this.screenRows = Math.max(0, frame.size.rows)
    this.columns = Math.max(0, frame.size.cols)
    this.scrollback = Math.max(0, Math.min(frame.scrollbackLines, this.firstActive))

    // Doubt pictured history rows AFTER updating `firstActive` so active rows that just scrolled
    // into history on the same frame `imagesGen` changed are properly doubted too.
    if (imagesBumped) {
      this.doubtPictured()
    }

    if (reset || this.oldest !== prevOldest || this.firstActive < prevFirstActive) {
      this.absent.clear()
    }

    const changed: number[] = []
    if (!reset) {
      if (frame.fullSync) {
        // Drop any rows that were on the previous active screen and scrolled off during skipped
        // frames (O(prevScreenRows)), plus any current active-screen rows replaced by blank rows
        // omitted from `frame.rows` (O(screenRows)). Lines `< this.oldest` are dropped in O(pages)
        // by `trimBelow(this.oldest)` below.
        const stalePrevStart = Math.max(this.oldest, prevFirstActive)
        const stalePrevEnd = Math.min(prevEnd, this.firstActive)
        for (let line = stalePrevStart; line < stalePrevEnd; line += 1) {
          if (this.row(line) !== undefined) {
            this.deleteLine(line)
            changed.push(line)
          }
        }
        for (let line = this.firstActive; line < this.end; line += 1) {
          if (this.row(line) !== undefined) {
            this.deleteLine(line)
            changed.push(line)
          }
        }
        // If `firstActive` moved backward within the same epoch (e.g. reflow/shrink), doubt older
        // cached history by iterating allocated pages (O(cached rows)), never O(scrollback).
        if (this.firstActive < prevFirstActive) {
          for (const page of this.pages.values()) {
            if (page.baseLine >= this.firstActive) continue
            const limit = Math.min(PAGE_LINES, this.firstActive - page.baseLine)
            for (let i = 0; i < limit; i += 1) {
              if (page.slots[i] !== undefined) this.doubted.add(page.baseLine + i)
            }
          }
        }
      } else if (this.firstActive > prevFirstActive && frame.scrollDelta > 0) {
        // Rows that just scrolled off the active screen into scrollback may have been mutated
        // (e.g. spinner/progress overwrite) right before scrolling; mark them doubted so scrolling
        // back re-verifies them against broker history without causing blank flashes while live.
        const scrolledEnd = Math.min(prevEnd, this.firstActive)
        for (let line = prevFirstActive; line < scrolledEnd; line += 1) {
          if (this.row(line) !== undefined) this.doubted.add(line)
        }
      }
    }

    for (const row of frame.rows) {
      if (row.line < this.oldest || row.line >= this.end) continue
      this.setRow(row)
      changed.push(row.line)
    }

    // Trim cached pages/lines that fell off scrollback or beyond the new screen end
    if (!reset) {
      if (this.oldest > prevOldest) this.trimBelow(this.oldest)
      if (this.end < prevEnd) this.trimAtOrAbove(this.end)
    }

    if (this.totalRows > KEPT_LINES + KEPT_SLACK) {
      this.evictFurthestPages(near)
    }
    return { reset, changed }
  }

  /** Absorbs history rows returned by `fetchRows(epoch, fromLine, count)`, returning absorbed line numbers. */
  absorb(
    epoch: number,
    fromLine: number,
    count: number,
    rows: readonly PackedRow[],
    near = this.firstActive,
    atImagesChanged = this.imagesChanged,
  ): number[] {
    if (epoch !== this.epoch || count <= 0) return []
    const seen = new Set<number>()
    const absorbed: number[] = []
    const staleImages = atImagesChanged !== this.imagesChanged

    for (const row of rows) {
      if (row.line < this.oldest || row.line >= this.end) continue
      // Never overwrite live screen rows or newer doubted placeholder rows with stale history
      if (row.line >= this.firstActive && this.row(row.line) !== undefined) continue
      if (staleImages && (row.flags & RowFlag.HAS_PLACEHOLDER) !== 0) {
        // Never mark a stale-image placeholder line as `absent`; mark it doubted so it refetches.
        seen.add(row.line)
        if (row.line < this.firstActive) this.doubted.add(row.line)
        continue
      }
      this.setRow(row)
      seen.add(row.line)
      absorbed.push(row.line)
    }

    const start = Math.max(this.oldest, fromLine)
    const stop = Math.min(this.firstActive, fromLine + count)
    for (let line = start; line < stop; line += 1) {
      if (!seen.has(line)) {
        if (this.doubted.has(line)) {
          this.deleteLine(line)
          absorbed.push(line)
        }
        if (this.row(line) === undefined) {
          if (this.absent.size >= MAX_ABSENT) this.absent.clear()
          this.absent.add(line)
        }
      }
    }

    if (this.totalRows > KEPT_LINES + KEPT_SLACK) {
      this.evictFurthestPages(near)
    }
    return absorbed
  }

  /** Marks cached history rows carrying image placeholders as doubted so they refetch when visible. */
  doubtPictured(): void {
    this.imagesChanged += 1
    for (const line of this.picturedLines) {
      if (line < this.firstActive) this.doubted.add(line)
    }
  }

  /** Returns contiguous uncached (or doubted) history spans inside `[fromLine, fromLine + count)`. */
  missing(fromLine: number, count: number): Array<{ readonly from: number; readonly count: number }> {
    const start = Math.max(this.oldest, Math.floor(fromLine))
    const stop = Math.min(this.firstActive, Math.ceil(fromLine + count))
    if (stop <= start) return []
    const ranges: Array<{ from: number; count: number }> = []
    for (let line = start; line < stop; line += 1) {
      const need = (this.row(line) === undefined && !this.absent.has(line)) || this.doubted.has(line)
      if (!need) continue
      const last = ranges.at(-1)
      if (last && last.from + last.count === line) last.count += 1
      else ranges.push({ from: line, count: 1 })
    }
    return ranges
  }

  /** Returns all cached lines currently carrying `ImagePiece` placeholders. */
  pictured(): number[] {
    return [...this.picturedLines]
  }

  private trimBelow(oldest: number): void {
    const minPage = Math.floor(oldest / PAGE_LINES)
    for (const [pageIdx, page] of this.pages) {
      if (pageIdx < minPage) {
        this.dropPage(pageIdx, page)
      } else if (pageIdx === minPage) {
        const cutoff = oldest - page.baseLine
        for (let i = 0; i < cutoff; i += 1) {
          if (page.slots[i] !== undefined) {
            page.slots[i] = undefined
            page.count -= 1
            this.totalRows -= 1
            const line = page.baseLine + i
            this.picturedLines.delete(line)
            this.doubted.delete(line)
          }
        }
        if (page.count === 0) this.pages.delete(pageIdx)
      }
    }
  }

  private trimAtOrAbove(end: number): void {
    const maxPage = Math.floor(Math.max(0, end - 1) / PAGE_LINES)
    for (const [pageIdx, page] of this.pages) {
      if (pageIdx > maxPage || end === 0) {
        this.dropPage(pageIdx, page)
      } else if (pageIdx === maxPage) {
        const startOffset = end - page.baseLine
        for (let i = Math.max(0, startOffset); i < PAGE_LINES; i += 1) {
          if (page.slots[i] !== undefined) {
            page.slots[i] = undefined
            page.count -= 1
            this.totalRows -= 1
            const line = page.baseLine + i
            this.picturedLines.delete(line)
            this.doubted.delete(line)
          }
        }
        if (page.count === 0) this.pages.delete(pageIdx)
      }
    }
  }

  private dropPage(pageIdx: number, page: RowPage): void {
    this.pages.delete(pageIdx)
    this.totalRows -= page.count
    for (let i = 0; i < PAGE_LINES; i += 1) {
      if (page.slots[i] !== undefined) {
        const line = page.baseLine + i
        this.picturedLines.delete(line)
        this.doubted.delete(line)
      }
    }
  }

  /**
   * $O(\text{pages} \log \text{pages})$ eviction: drops entire 256-line pages furthest from `near`
   * (never evicting active-screen pages) until `totalRows <= KEPT_LINES`.
   */
  private evictFurthestPages(near: number): void {
    const activeStartPage = Math.floor(this.firstActive / PAGE_LINES)
    const activeEndPage = Math.floor(Math.max(this.firstActive, this.end - 1) / PAGE_LINES)
    const nearPage = Math.floor(near / PAGE_LINES)

    const candidates: Array<{ readonly pageIdx: number; readonly dist: number; readonly page: RowPage }> = []
    for (const [pageIdx, page] of this.pages) {
      if (pageIdx >= activeStartPage && pageIdx <= activeEndPage) continue
      candidates.push({ pageIdx, dist: Math.abs(pageIdx - nearPage), page })
    }
    candidates.sort((a, b) => b.dist - a.dist)

    for (const c of candidates) {
      if (this.totalRows <= KEPT_LINES) break
      this.dropPage(c.pageIdx, c.page)
    }
  }
}
