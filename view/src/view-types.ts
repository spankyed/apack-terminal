import { type Clock } from './clock.ts'
import { blinks, type Theme } from './draw.ts'
import { type ImageCache, type ImageCacheOptions } from './images.ts'
import { type Detector, type Link } from './links.ts'
import { type Font, type Painter } from './painter.ts'
import {
  RunFlag,
  packRow,
  type CellSpan,
  type CommandRecord,
  type CursorShape,
  type PackedRow,
  type SessionEvent,
} from './source.ts'
import { cellsOfCodePoint } from './width.ts'

/**
 * A run of lines a host folds away. The first line stays on screen as a header, which stands for
 * the lines after it up to `last`: those are neither drawn nor pointed at, and what is copied
 * from the fold is still all of its text.
 */
export interface Fold {
  readonly first: number
  readonly last: number
  /** What the header says the fold holds, after the count of lines. Left out, the header says only the count. */
  readonly label?: string
}

/** How long the cursor is shown, and then hidden, while it blinks (ms). */
export const BLINK_MS = 530

/** Maximum search matches retained & highlighted in the view. */
export const MATCH_HIGHLIGHTS = 1000

/** Debounce interval (ms) for coalescing updates to the hidden `[data-terminal-text]` DOM mirror. */
export const MIRROR_DEBOUNCE_MS = 200

/**
 * What hovering a link shows of where it opens, as the pointer's title: `osc8` for a link a
 * program declared with a hyperlink sequence, `all` for any link, and `off` for none.
 */
export type LinkPreview = 'off' | 'osc8' | 'all'

export interface ViewOptions {
  /** The typeface a view starts in; `setFont` changes it. */
  readonly font: Font
  /** The colours a view starts in; `setTheme` changes them. */
  readonly theme: Theme
  /** Optional custom `ImageCache` or decode/cap options (e.g. for tests or zstdRgba decoders). */
  readonly imageCache?: ImageCache
  readonly imageCacheOptions?: ImageCacheOptions
  /** Custom painter factory. Defaults to `CanvasPainter`. */
  readonly painter?: (font: Font) => Painter
  /** Reports measured cell fit whenever container dimensions or font metrics change. */
  readonly onFit?: (fit: { columns: number; rows: number; cellWidth: number; cellHeight: number }) => void
  /** Fires whenever whether the view is following live output changes. */
  readonly onFollowingChange?: (following: boolean) => void
  /** Fires when the session exits or the stream closes. */
  readonly onEnded?: (reason?: string) => void
  /** Optional raw event callback for title, bell, clipboard, notification, progress, status, cwd, etc. */
  readonly onEvent?: (event: SessionEvent) => void
  /** Host keyboard shortcut interceptor. Returning `true` consumes the key. */
  readonly onKey?: (event: KeyboardEvent) => boolean
  /** Confirmation callback when the broker flags a paste as `'unsafe'`. */
  readonly confirmPaste?: (text: string) => Promise<boolean>
  /** Fires if a paste RPC fails (e.g. oversized payload). */
  readonly onPasteError?: (error: unknown) => void
  /** Whether typing/pasting jumps a scrolled-back view back to the live screen. Default `true`. */
  readonly followOnInput?: boolean
  /** Cursor shape when unfocused. Default `'hollow'`. */
  readonly cursorInactive?: CursorShape
  /** Whether Option acts as Alt on macOS. Default `false`. */
  readonly optionIsAlt?: boolean
  /** Fires when the user Cmd/Ctrl-clicks a link. */
  readonly onLink?: (link: Link, event: MouseEvent) => void
  /** Which links display a tooltip preview on hover. Default `'osc8'`. */
  readonly linkPreview?: LinkPreview
  /** Additional custom link detectors beyond `urls`. */
  readonly detectors?: readonly Detector[]
  /** Fires whenever selection presence flips between empty and non-empty. */
  readonly onSelectionChange?: (selected: boolean) => void
  /** Fires when live incremental `SearchWatch` updates change match count or current index. */
  readonly onFound?: (found: { current: number; total: number; more: boolean }) => void
  /** Custom copy handler. Defaults to writing to `navigator.clipboard`. */
  readonly onCopy?: (text: string) => void
  /** Fires whenever the session's command list updates. */
  readonly onCommands?: (commands: readonly CommandRecord[]) => void
  /** Automatically copy selections on pointer release. Default `false`. */
  readonly copyOnSelect?: boolean
  /** Override macOS modifier heuristics. Defaults to `navigator.platform` detection. */
  readonly mac?: boolean
  /** Gutter width in CSS px for command prompt status marks. Default `0` (disabled). */
  readonly gutter?: number
  /** Structural timer source (compatible with `@apackdev/clock`). Defaults to `systemClock`. */
  readonly clock?: Clock
}

const blinkingRows = new WeakMap<PackedRow, boolean>()

export function blinksIn(row: PackedRow): boolean {
  let answer = blinkingRows.get(row)
  if (answer === undefined) {
    answer = blinks(row)
    blinkingRows.set(row, answer)
  }
  return answer
}

export function buildFoldRow(fold: Fold): PackedRow {
  const label = `▸ ${fold.label ?? `${fold.last - fold.first} lines folded`}`
  const chars = Array.from(label)
  const runs: Array<{ colStart: number; colLen: number; text: string; flags: number }> = []
  let col = 0
  for (const ch of chars) {
    const cp = ch.codePointAt(0) ?? 0
    const w = Math.max(1, cellsOfCodePoint(cp))
    const isAscii = ch.length === 1 && cp >= 0x20 && cp <= 0x7e && w === 1
    const last = runs.at(-1)
    if (isAscii && last && last.flags === 0 && last.colStart + last.colLen === col) {
      last.text += ch
      last.colLen += 1
    } else {
      runs.push({ colStart: col, colLen: w, text: ch, flags: isAscii ? 0 : RunFlag.PER_CELL })
    }
    col += w
  }
  return packRow({ line: fold.first, runs })
}

export function cellAtColumn(
  row: PackedRow,
  col: number,
): { readonly col: number; readonly width: number; readonly text: string; readonly flags: number } | null {
  const { runs, styles, text } = row
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const start = colWord & 0xffff
    const len = colWord >>> 16
    if (col < start) break
    if (col >= start + len) continue
    const txtWord = runs[i + 1]!
    const metaWord = runs[i + 2]!
    const runFlags = runs[i + 3]!
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    const styleIdx = metaWord & 0xffff
    const styleBase = styleIdx * 4
    const flags = styleBase + 3 < styles.length ? styles[styleBase + 3]! : 0
    const slice = text.slice(u16Off, u16Off + u16Len)

    if ((runFlags & RunFlag.PER_CELL) !== 0) {
      return { col: start, width: len, text: slice, flags }
    }

    const offset = col - start
    if (slice.length === len) {
      if (offset >= 0 && offset < slice.length) {
        return { col, width: 1, text: slice[offset]!, flags }
      }
      return null
    }
    const cps = Array.from(slice)
    if (offset >= 0 && offset < cps.length) {
      return { col, width: 1, text: cps[offset]!, flags }
    }
    return null
  }
  return null
}

export function sortMatchesNewestFirst(spans: readonly CellSpan[]): CellSpan[] {
  return [...spans].sort((a, b) => b.start.line - a.start.line || b.start.col - a.start.col)
}

export function hex(color: number): string {
  return `#${color.toString(16).padStart(6, '0')}`
}

export function markColor(command: CommandRecord): number {
  if (command.startedMs === null) return 8
  const { outcome } = command
  switch (outcome.kind) {
    case 'running': return 4
    case 'exited': return outcome.code === 0 ? 2 : 1
    case 'unknown': return 8
    case 'claimed': return 3
  }
}

export function firstAfter(items: ReadonlyArray<{ readonly line: number }>, line: number): number {
  let low = 0
  let high = items.length
  while (low < high) {
    const middle = (low + high) >> 1
    if (items[middle]!.line <= line) low = middle + 1
    else high = middle
  }
  return low
}

const UNSAFE_TITLE_CHARS = /[\u0000-\u001f\u007f-\u009f\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/g

export function sanitizeTitle(text: string): string {
  return text.replace(UNSAFE_TITLE_CHARS, '')
}
