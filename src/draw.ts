import { StyleFlag, paletteIndex, underlineOf, type Color, type Row, type Style, type Underline } from './source.ts'

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
}

export type LineKind = Exclude<Underline, 'none'> | 'strikethrough' | 'overline'

/**
 * One row as things to draw, in cells: a painter multiplies by the size of a cell.
 *
 * **In cells so that it can be kept.** A row's drawing depends on the row, the theme and what is selected
 * on it — not on where it is on the screen — so a painter builds this once per row that changed and reuses
 * it while the view scrolls.
 */
export interface RowDrawing {
  /** Runs of cells with a background of their own; cells with the default are not listed */
  readonly backgrounds: ReadonlyArray<{ column: number; cells: number; color: number }>
  readonly glyphs: ReadonlyArray<{
    text: string
    column: number
    cells: number
    color: number
    /** `0.5` for faint text, otherwise `1` */
    alpha: number
    bold: boolean
    italic: boolean
  }>
  readonly lines: ReadonlyArray<{ kind: LineKind; column: number; cells: number; color: number }>
}

/** The cells of one row that are selected: from `from` up to but not including `to` */
export interface SelectedCells {
  readonly from: number
  readonly to: number
}

/** A colour as the theme has it: a numbered one looked up, any other as it is */
function colorIn(theme: Theme, color: Color): number | null {
  if (color === null) return null
  const index = paletteIndex(color)
  return index === null ? color : (theme.palette[index] ?? theme.foreground)
}

/**
 * What to draw for `row`.
 *
 * The rules a terminal's styles follow, in the order they apply: inverse swaps the two colours, defaults
 * included; a selection then replaces the background and, if the theme says so, the text; invisible text
 * keeps its background and draws no character; faint text is drawn at half strength.
 */
export function drawRow(row: Row, styles: readonly Style[], theme: Theme, selected?: SelectedCells): RowDrawing {
  const backgrounds: Array<{ column: number; cells: number; color: number }> = []
  const glyphs: Array<RowDrawing['glyphs'][number]> = []
  const lines: Array<{ kind: LineKind; column: number; cells: number; color: number }> = []

  /** Extends the last run when this one continues it, which is what keeps a coloured line one rectangle */
  const extend = <T extends { column: number; cells: number }>(runs: T[], next: T, same: (last: T) => boolean) => {
    const last = runs.at(-1)
    if (last && last.column + last.cells === next.column && same(last)) last.cells += next.cells
    else runs.push(next)
  }

  for (let index = 0; index < row.text.length; index += 1) {
    const text = row.text[index]!
    const column = row.column[index]!
    const cells = row.width[index]!
    const style = styles[row.style[index]!] ?? styles[0]!
    const flags = style.flags

    let fg = colorIn(theme, style.fg) ?? theme.foreground
    let bg = colorIn(theme, style.bg)
    if (flags & StyleFlag.inverse) [fg, bg] = [bg ?? theme.background, fg]
    const isSelected = selected !== undefined && column < selected.to && column + cells > selected.from
    if (isSelected) {
      bg = theme.selectionBackground
      fg = theme.selectionForeground ?? fg
    }

    if (bg !== null && bg !== theme.background) {
      extend(backgrounds, { column, cells, color: bg }, (last) => last.color === bg)
    }

    const visible = !(flags & StyleFlag.invisible)
    if (visible && text !== ' ' && text !== '') {
      glyphs.push({
        text,
        column,
        cells,
        color: fg,
        alpha: flags & StyleFlag.faint ? 0.5 : 1,
        bold: (flags & StyleFlag.bold) !== 0,
        italic: (flags & StyleFlag.italic) !== 0,
      })
    }

    if (!visible) continue
    const underline = underlineOf(style)
    if (underline !== 'none') {
      const color = colorIn(theme, style.underlineColor) ?? fg
      extend(lines, { kind: underline, column, cells, color }, (last) => last.kind === underline && last.color === color)
    }
    if (flags & StyleFlag.strikethrough) {
      extend(lines, { kind: 'strikethrough', column, cells, color: fg }, (last) => last.kind === 'strikethrough' && last.color === fg)
    }
    if (flags & StyleFlag.overline) {
      extend(lines, { kind: 'overline', column, cells, color: fg }, (last) => last.kind === 'overline' && last.color === fg)
    }
  }

  // A selection reaches past the last character of a line: the blank cells it covers are selected too
  if (selected !== undefined) {
    const last = row.text.length - 1
    const end = last < 0 ? 0 : row.column[last]! + row.width[last]!
    const from = Math.max(selected.from, end)
    if (selected.to > from) {
      extend(backgrounds, { column: from, cells: selected.to - from, color: theme.selectionBackground },
        (run) => run.color === theme.selectionBackground)
    }
  }

  return { backgrounds, glyphs, lines }
}
