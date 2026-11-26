import type { SelectedCells } from './draw.ts'
import { localLineAt, localTextOf, localTextOfRect, localWordAt } from './local-query.ts'
import type { RowStore } from './row-store.ts'
import type { CellPos, CellSpan, TerminalSource } from './source.ts'

/** What a click selects by, which is how many times it was clicked */
export type Granularity = 'cell' | 'word' | 'line'

/**
 * The shape of a selection made by dragging over cells: `linear` is the text between two cells;
 * `block` is the rectangle between two cells (Alt-drag / Option-drag).
 */
export type Shape = 'linear' | 'block'

const before = (a: CellPos, b: CellPos): boolean =>
  a.line < b.line || (a.line === b.line && a.col < b.col)

/**
 * What the user has selected, in absolute lines.
 *
 * v2 optimization:
 * - Resolves `wordAt`, `lineAt`, `textOf`, and `textOfRect` synchronously in 0 ms via `local-query.ts`
 *   whenever the target rows are cached in `RowStore` (99% of interactive clicks & copies).
 * - Falls back to a single broker RPC (`source.wordAt`/`lineAt`/`textOf`/`textOfRect`) only when
 *   the selection crosses uncached scrollback history.
 * - Exposes a monotonic numeric `version` so `TerminalView` dirty-checks `RowDrawing` caches in O(1)
 *   without allocating string cache keys.
 */
export class Selection {
  private anchor: CellSpan | null = null
  private head: CellSpan | null = null
  private headAt: CellPos | null = null
  private granularity: Granularity = 'cell'
  private shape: Shape = 'linear'
  private anchorAt: CellPos | null = null
  private anchorAsked = 0
  private headAsked = 0
  private rev = 0

  constructor(
    private readonly source: TerminalSource,
    private readonly store: RowStore,
    private readonly changed: () => void,
  ) {}

  /** Monotonic integer incremented on every selection state change. */
  get version(): number {
    return this.rev
  }

  /** The rectangle of a block selection, inclusive, or `null` when linear or empty. */
  get block(): { top: number; bottom: number; left: number; right: number } | null {
    if (this.shape !== 'block' || !this.anchorAt || !this.headAt) return null
    return {
      top: Math.min(this.anchorAt.line, this.headAt.line),
      bottom: Math.max(this.anchorAt.line, this.headAt.line),
      left: Math.min(this.anchorAt.col, this.headAt.col),
      right: Math.max(this.anchorAt.col, this.headAt.col),
    }
  }

  /** The selection, start before end, or `null` when empty. */
  get span(): CellSpan | null {
    const rectangle = this.block
    if (rectangle) {
      return {
        start: { line: rectangle.top, col: rectangle.left },
        end: { line: rectangle.bottom, col: rectangle.right },
      }
    }
    if (!this.anchor || !this.head) return null
    const start = before(this.head.start, this.anchor.start) ? this.head.start : this.anchor.start
    const end = before(this.anchor.end, this.head.end) ? this.head.end : this.anchor.end
    return { start, end }
  }

  /** The cells of `line` that are selected, for drawing. */
  on(line: number): SelectedCells | undefined {
    const cols = this.store.columns
    if (cols <= 0) return undefined
    const maxCol = cols - 1
    const rectangle = this.block
    if (rectangle) {
      if (line < rectangle.top || line > rectangle.bottom) return undefined
      const from = Math.max(0, Math.min(rectangle.left, maxCol))
      const to = Math.max(from + 1, Math.min(rectangle.right + 1, cols))
      return { from, to }
    }
    const span = this.span
    if (!span || line < span.start.line || line > span.end.line) return undefined
    const from = line === span.start.line ? Math.max(0, Math.min(span.start.col, maxCol)) : 0
    const to = line === span.end.line ? Math.max(0, Math.min(span.end.col + 1, cols)) : cols
    return to > from ? { from, to } : undefined
  }

  /** Starts a selection at `position`: cell, word, or line. */
  begin(position: CellPos, granularity: Granularity, shape: Shape = 'linear'): void {
    this.granularity = granularity
    this.shape = granularity === 'cell' ? shape : 'linear'
    this.anchorAt = position
    const cell: CellSpan = { start: position, end: position }
    this.anchor = cell
    this.head = cell
    this.headAt = position
    const anchor = (this.anchorAsked += 1)
    const head = (this.headAsked += 1)
    const widened = this.tryLocalWiden(position)
    if (widened) {
      this.anchor = widened
      this.head = widened
      this.notify()
      return
    }
    this.notify()
    this.remoteWiden(position, (span) => {
      if (anchor === this.anchorAsked) this.anchor = span
      if (head === this.headAsked) this.head = span
      return anchor === this.anchorAsked || head === this.headAsked
    })
  }

  /** Moves the far end of the selection to `position`. */
  extendTo(position: CellPos): void {
    if (!this.anchor) return
    if (this.headAt && this.headAt.line === position.line && this.headAt.col === position.col) return
    this.head = { start: position, end: position }
    this.headAt = position
    const head = (this.headAsked += 1)
    const widened = this.tryLocalWiden(position)
    if (widened) {
      this.head = widened
      this.notify()
      return
    }
    this.notify()
    this.remoteWiden(position, (span) => {
      if (head === this.headAsked) this.head = span
      return head === this.headAsked
    })
  }

  /** Selects exactly these cells, from start to end inclusive. */
  select(span: CellSpan): void {
    this.anchorAsked += 1
    this.headAsked += 1
    this.granularity = 'cell'
    this.shape = 'linear'
    this.anchorAt = null
    this.anchor = { start: span.start, end: span.start }
    this.head = { start: span.end, end: span.end }
    this.headAt = span.end
    this.notify()
  }

  clear(): void {
    if (!this.anchor) return
    this.anchorAsked += 1
    this.headAsked += 1
    this.anchor = null
    this.head = null
    this.headAt = null
    this.anchorAt = null
    this.shape = 'linear'
    this.notify()
  }

  /** Line numbers are about to change (epoch moved): drop selection. */
  reset(): void {
    this.clear()
  }

  /**
   * Synchronously extracts the selected text from cached `RowStore` rows when all lines are local,
   * or returns `null` if an uncached scrollback line requires a broker RPC.
   */
  syncText(): string | null {
    const clamped = this.clampedSpan()
    if (!clamped) return ''
    return clamped.rect
      ? localTextOfRect(this.store, clamped.span)
      : localTextOf(this.store, clamped.span)
  }

  /**
   * Returns the selected text as written by the terminal: 0 ms local resolution when cached,
   * otherwise a single broker RPC (`textOf` or `textOfRect`).
   */
  async text(): Promise<string> {
    const clamped = this.clampedSpan()
    if (!clamped) return ''
    const local = clamped.rect
      ? localTextOfRect(this.store, clamped.span)
      : localTextOf(this.store, clamped.span)
    if (local !== null) return local
    if (this.store.epoch < 0) return ''
    return clamped.rect
      ? this.source.textOfRect(this.store.epoch, clamped.span)
      : this.source.textOf(this.store.epoch, clamped.span)
  }

  private clampedSpan(): { readonly span: CellSpan; readonly rect: boolean } | null {
    const cols = this.store.columns
    if (cols <= 0 || this.store.end <= this.store.oldest) return null
    const maxCol = cols - 1
    const clampCol = (col: number) => Math.max(0, Math.min(col, maxCol))
    const rectangle = this.block
    if (rectangle) {
      const top = Math.max(rectangle.top, this.store.oldest)
      const bottom = Math.min(rectangle.bottom, this.store.end - 1)
      if (bottom < top) return null
      const left = clampCol(rectangle.left)
      const right = Math.max(left, clampCol(rectangle.right))
      return {
        rect: true,
        span: { start: { line: top, col: left }, end: { line: bottom, col: right } },
      }
    }
    const span = this.span
    if (!span) return null
    const start: CellPos =
      span.start.line < this.store.oldest
        ? { line: this.store.oldest, col: 0 }
        : { line: span.start.line, col: clampCol(span.start.col) }
    const end: CellPos =
      span.end.line >= this.store.end
        ? { line: this.store.end - 1, col: maxCol }
        : { line: span.end.line, col: clampCol(span.end.col) }
    if (before(end, start)) return null
    return { rect: false, span: { start, end } }
  }

  private notify(): void {
    this.rev += 1
    this.changed()
  }

  private tryLocalWiden(position: CellPos): CellSpan | null {
    if (this.granularity === 'cell') return null
    return this.granularity === 'word'
      ? localWordAt(this.store, position, this.source.wordDelimiters)
      : localLineAt(this.store, position.line)
  }

  private remoteWiden(position: CellPos, set: (span: CellSpan) => boolean): void {
    if (this.granularity === 'cell' || this.store.epoch < 0) return
    const epoch = this.store.epoch
    const asked =
      this.granularity === 'word'
        ? this.source.wordAt(epoch, position)
        : this.source.lineAt(epoch, position.line)
    void asked.then((span) => {
      if (span && this.store.epoch === epoch && set(span)) this.notify()
    }, () => {})
  }
}
