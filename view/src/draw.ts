import { ensureContrast } from './contrast.ts'
import {
  PackedStyleFlag,
  RowFlag,
  RunFlag,
  resolvePackedColor,
  underlineOfStyleFlags,
  type PackedRow,
  type PaletteOverrides,
  type UnderlineKind,
} from './source.ts'

/**
 * The colours a view supplies: the defaults a program did not override, the palette its numbered colours
 * name, and what belongs to the view itself.
 */
export interface Theme {
  readonly background: number
  readonly foreground: number
  /** What each of the 256 numbered colours is (`xtermPalette`) */
  readonly palette: readonly number[]
  readonly cursor: number
  /** The colour of a character under a block cursor */
  readonly cursorText: number
  readonly selectionBackground: number
  /** The colour of selected text, or `null` to leave each character its own */
  readonly selectionForeground: number | null
  /** Behind every place a search found its text, and behind the one the person is on. Left out, a view's own */
  readonly matchBackground?: number
  readonly currentMatchBackground?: number
  /**
   * The least WCAG contrast ratio text is drawn at against what it sits on (`contrast.ts`). Left out, or 1, every colour
   * is the program's own; 4.5 is the level WCAG asks for normal text. `setTheme` changes it while a view is up.
   */
  readonly minimumContrast?: number
}

/**
 * A theme as a session is drawn in it: the host's, with any OSC 10/11/12/4 palette overrides the
 * program set for itself, and with the two defaults swapped while DECSCNM reverse video is active.
 */
export function sessionTheme(
  theme: Theme,
  overrides: PaletteOverrides | null,
  reverseVideo: boolean,
): Theme {
  if (
    !reverseVideo
    && (!overrides
      || (overrides.fg === null
        && overrides.bg === null
        && overrides.cursor === null
        && overrides.palette.length === 0))
  ) {
    return theme
  }
  let palette = theme.palette
  if (overrides && overrides.palette.length > 0) {
    const changed = [...theme.palette]
    for (const entry of overrides.palette) changed[entry.index] = entry.rgb
    palette = changed
  }
  const foreground = overrides?.fg ?? theme.foreground
  const background = overrides?.bg ?? theme.background
  return {
    ...theme,
    palette,
    foreground: reverseVideo ? background : foreground,
    background: reverseVideo ? foreground : background,
    cursor: overrides?.cursor ?? theme.cursor,
  }
}

export type LineKind = Exclude<UnderlineKind, 'none'> | 'strikethrough' | 'overline'

/** A background painted over cells after the row's own, such as where a search found its text */
export interface Highlight {
  readonly from: number
  readonly to: number
  readonly color: number
}

/** A run of text (either a batchable printable ASCII span or a single non-ASCII / wide cluster). */
export interface GlyphRun {
  readonly text: string
  readonly column: number
  readonly cells: number
  readonly color: number
  /** `0.5` for faint text, otherwise `1` */
  readonly alpha: number
  readonly bold: boolean
  readonly italic: boolean
  /**
   * True when every character in `text` is printable ASCII (`0x20..=0x7E`) occupying 1 cell each,
   * so `CanvasPainter` can draw the whole span in a single `fillText` call once its font-advance
   * probe passes.
   */
  readonly batchable: boolean
}

/**
 * One row as things to draw, in cells: a painter multiplies by the size of a cell.
 *
 * Built directly from v2 `PackedRow` flat typed arrays with zero per-cell intermediate objects.
 */
export interface RowDrawing {
  /** Runs of cells with a background of their own; cells with the default are not listed */
  readonly backgrounds: ReadonlyArray<{ column: number; cells: number; color: number }>
  readonly glyphs: ReadonlyArray<GlyphRun>
  readonly lines: ReadonlyArray<{ kind: LineKind; column: number; cells: number; color: number }>
}

/** Whether any style referenced on `row` has the blink flag (O(1) flag fast-path + O(styles) fallback). */
export function blinks(row: PackedRow): boolean {
  if ((row.flags & RowFlag.HAS_BLINK) !== 0) return true
  const { styles } = row
  for (let i = 3; i < styles.length; i += 4) {
    if ((styles[i]! & PackedStyleFlag.BLINK) !== 0) return true
  }
  return false
}

/** The cells of one row that are selected: from `from` up to but not including `to` */
export interface SelectedCells {
  readonly from: number
  readonly to: number
}

/** Extracts plain visible text of a `PackedRow` with spaces filling blank gaps (for DOM mirror / a11y). */
export function rowPlainText(row: PackedRow): string {
  const { runs, text } = row
  let out = ''
  let col = 0
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const txtWord = runs[i + 1]!
    const colStart = colWord & 0xffff
    const colLen = colWord >>> 16
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    if (colStart > col) out += ' '.repeat(colStart - col)
    out += text.slice(u16Off, u16Off + u16Len)
    col = colStart + colLen
  }
  return out
}

function isPrintableAscii(text: string): boolean {
  for (let i = 0; i < text.length; i += 1) {
    const ch = text.charCodeAt(i)
    if (ch < 0x20 || ch > 0x7e) return false
  }
  return true
}

function isAllSpaces(text: string): boolean {
  for (let i = 0; i < text.length; i += 1) {
    if (text.charCodeAt(i) !== 0x20) return false
  }
  return true
}

interface OrderedHighlight {
  readonly from: number
  readonly to: number
  readonly color: number
  readonly order: number
}

/**
 * Converts a v2 `PackedRow` into a painter-ready `RowDrawing`, preserving run-batched printable
 * ASCII spans wherever style, selection, and WCAG highlight backgrounds are uniform.
 */
export function drawRow(
  row: PackedRow,
  theme: Theme,
  selected?: SelectedCells,
  blinkedOff = false,
  found: readonly Highlight[] = [],
): RowDrawing {
  const backgrounds: Array<{ column: number; cells: number; color: number }> = []
  const glyphs: GlyphRun[] = []
  const lines: Array<{ kind: LineKind; column: number; cells: number; color: number }> = []

  const extend = <T extends { column: number; cells: number }>(
    list: T[],
    next: T,
    same: (last: T) => boolean,
  ) => {
    const last = list.at(-1)
    if (last && last.column + last.cells === next.column && same(last)) last.cells += next.cells
    else list.push(next)
  }

  const hasSelection = selected !== undefined && selected.to > selected.from
  let selectedFrom = hasSelection ? selected.from : 0
  let selectedTo = hasSelection ? selected.to : 0

  const orderedHighlights: OrderedHighlight[] = found.length === 0 ? [] : orderedByColumn(found)
  const useContrast = theme.minimumContrast !== undefined && theme.minimumContrast > 1

  const emitSegment = (
    segText: string,
    column: number,
    cells: number,
    batchable: boolean,
    styleFg: number,
    styleBg: number | null,
    styleUnderlineColor: number | null,
    flags: number,
    isSelected: boolean,
  ): void => {
    if (cells <= 0) return

    let fg = styleFg
    let bg = styleBg
    if ((flags & PackedStyleFlag.INVERSE) !== 0) {
      const nextFg = bg ?? theme.background
      bg = fg
      fg = nextFg
    }

    if (isSelected) {
      fg = theme.selectionForeground ?? fg
      selectedFrom = Math.min(selectedFrom, column)
      selectedTo = Math.max(selectedTo, column + cells)
    } else if (bg !== null && bg !== theme.background) {
      extend(backgrounds, { column, cells, color: bg }, (last) => last.color === bg)
    }

    const visible =
      (flags & PackedStyleFlag.INVISIBLE) === 0
      && !(blinkedOff && (flags & PackedStyleFlag.BLINK) !== 0)
    if (!visible) return

    if (useContrast) {
      let behind = isSelected ? theme.selectionBackground : (bg ?? theme.background)
      let bestOrder = -1
      for (let h = 0; h < orderedHighlights.length; h += 1) {
        const hl = orderedHighlights[h]!
        if (hl.from >= column + cells) break
        if (hl.to > column && hl.order > bestOrder) {
          bestOrder = hl.order
          behind = hl.color
        }
      }
      fg = ensureContrast(fg, behind, theme.minimumContrast!)
    }

    if (segText.length > 0 && !isAllSpaces(segText)) {
      const alpha = (flags & PackedStyleFlag.FAINT) !== 0 ? 0.5 : 1
      const bold = (flags & PackedStyleFlag.BOLD) !== 0
      const italic = (flags & PackedStyleFlag.ITALIC) !== 0
      const last = glyphs.at(-1)
      if (
        batchable
        && last !== undefined
        && last.batchable
        && last.column + last.cells === column
        && last.color === fg
        && last.alpha === alpha
        && last.bold === bold
        && last.italic === italic
      ) {
        glyphs[glyphs.length - 1] = {
          text: last.text + segText,
          column: last.column,
          cells: last.cells + cells,
          color: fg,
          alpha,
          bold,
          italic,
          batchable: true,
        }
      } else {
        glyphs.push({ text: segText, column, cells, color: fg, alpha, bold, italic, batchable })
      }
    }

    const underline = underlineOfStyleFlags(flags)
    if (underline !== 'none') {
      const color = styleUnderlineColor ?? fg
      extend(lines, { kind: underline, column, cells, color }, (last) => last.kind === underline && last.color === color)
    }
    if ((flags & PackedStyleFlag.STRIKETHROUGH) !== 0) {
      extend(lines, { kind: 'strikethrough', column, cells, color: fg }, (last) => last.kind === 'strikethrough' && last.color === fg)
    }
  }

  const { runs, styles, text } = row
  for (let i = 0; i < runs.length; i += 4) {
    const colWord = runs[i]!
    const txtWord = runs[i + 1]!
    const metaWord = runs[i + 2]!
    const runFlags = runs[i + 3]!

    const colStart = colWord & 0xffff
    const colLen = colWord >>> 16
    const u16Off = txtWord & 0xffff
    const u16Len = txtWord >>> 16
    const styleIdx = metaWord & 0xffff

    const styleBase = styleIdx * 4
    const fgRaw = styleBase < styles.length ? styles[styleBase]! : 0
    const bgRaw = styleBase + 1 < styles.length ? styles[styleBase + 1]! : 0
    const ulRaw = styleBase + 2 < styles.length ? styles[styleBase + 2]! : 0
    const flags = styleBase + 3 < styles.length ? styles[styleBase + 3]! : 0

    const styleFg = resolvePackedColor(fgRaw, theme.palette, theme.foreground) ?? theme.foreground
    const styleBg = resolvePackedColor(bgRaw, theme.palette, null)
    const styleUl = resolvePackedColor(ulRaw, theme.palette, null)

    const slice = text.slice(u16Off, u16Off + u16Len)
    const perCell = (runFlags & RunFlag.PER_CELL) !== 0

    if (perCell) {
      // Ghostty emits one PER_CELL run per cell (wide glyph or multi-codepoint grapheme cluster).
      const isSel = hasSelection && colStart < selected!.to && colStart + colLen > selected!.from
      emitSegment(slice, colStart, colLen, false, styleFg, styleBg, styleUl, flags, isSel)
      continue
    }

    // Narrow non-BMP codepoints (surrogate pairs, `slice.length !== colLen`): emit 1 codepoint per column
    if (slice.length !== colLen) {
      const cps = Array.from(slice)
      for (let k = 0; k < cps.length && k < colLen; k += 1) {
        const c = colStart + k
        const isSel = hasSelection && c < selected!.to && c + 1 > selected!.from
        emitSegment(cps[k]!, c, 1, false, styleFg, styleBg, styleUl, flags, isSel)
      }
      continue
    }

    // 1-cell-per-UTF-16-code-unit fast path (ASCII or 1-cell BMP run)
    const batchable = isPrintableAscii(slice)
    const runEnd = colStart + colLen

    // Collect any interior split points caused by selection edges or WCAG highlight boundaries
    const splits: number[] = []
    if (hasSelection) {
      if (selected!.from > colStart && selected!.from < runEnd) splits.push(selected!.from)
      if (selected!.to > colStart && selected!.to < runEnd) splits.push(selected!.to)
    }
    if (useContrast && orderedHighlights.length > 0) {
      for (const hl of orderedHighlights) {
        if (hl.from >= runEnd) break
        if (hl.to <= colStart) continue
        if (hl.from > colStart && hl.from < runEnd) splits.push(hl.from)
        if (hl.to > colStart && hl.to < runEnd) splits.push(hl.to)
      }
    }

    if (splits.length === 0) {
      const isSel = hasSelection && colStart < selected!.to && runEnd > selected!.from
      if (batchable || slice.length === 0) {
        emitSegment(slice, colStart, colLen, batchable, styleFg, styleBg, styleUl, flags, isSel)
      } else {
        for (let k = 0; k < slice.length && k < colLen; k += 1) {
          emitSegment(slice[k]!, colStart + k, 1, false, styleFg, styleBg, styleUl, flags, isSel)
        }
      }
    } else {
      splits.sort((a, b) => a - b)
      let cursor = colStart
      for (let s = 0; s <= splits.length; s += 1) {
        const edge = s < splits.length ? splits[s]! : runEnd
        if (edge <= cursor) continue
        const segOff = cursor - colStart
        const segLen = edge - cursor
        const segText = slice.slice(segOff, segOff + segLen)
        const isSel = hasSelection && cursor < selected!.to && edge > selected!.from
        if (batchable || segText.length === 0) {
          emitSegment(segText, cursor, segLen, batchable, styleFg, styleBg, styleUl, flags, isSel)
        } else {
          for (let k = 0; k < segText.length && k < segLen; k += 1) {
            emitSegment(segText[k]!, cursor + k, 1, false, styleFg, styleBg, styleUl, flags, isSel)
          }
        }
        cursor = edge
      }
    }
  }

  if (selectedTo > selectedFrom) {
    const after = backgrounds.findIndex((run) => run.column >= selectedTo)
    backgrounds.splice(after === -1 ? backgrounds.length : after, 0, {
      column: selectedFrom,
      cells: selectedTo - selectedFrom,
      color: theme.selectionBackground,
    })
  }
  for (const highlight of found) {
    backgrounds.push({
      column: highlight.from,
      cells: highlight.to - highlight.from,
      color: highlight.color,
    })
  }

  return { backgrounds, glyphs, lines }
}

function orderedByColumn(found: readonly Highlight[]): OrderedHighlight[] {
  const n = found.length
  let ascending = true
  let descending = true
  for (let i = 1; i < n; i += 1) {
    const prev = found[i - 1]!.from
    const curr = found[i]!.from
    if (curr < prev) ascending = false
    if (curr > prev) descending = false
  }
  const result = new Array<OrderedHighlight>(n)
  if (ascending) {
    for (let i = 0; i < n; i += 1) {
      const h = found[i]!
      result[i] = { from: h.from, to: h.to, color: h.color, order: i }
    }
    return result
  }
  if (descending) {
    for (let i = 0; i < n; i += 1) {
      const src = n - 1 - i
      const h = found[src]!
      result[i] = { from: h.from, to: h.to, color: h.color, order: src }
    }
    return result
  }
  for (let i = 0; i < n; i += 1) {
    const h = found[i]!
    result[i] = { from: h.from, to: h.to, color: h.color, order: i }
  }
  return result.sort((a, b) => a.from - b.from || a.order - b.order)
}
