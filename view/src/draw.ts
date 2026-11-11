import { PLAIN, StyleFlag, paletteIndex, underlineOf, type Color, type Row, type SessionColors, type Underline } from './source.ts'

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

/**
 * A theme as a session is drawn in it: the host's, with the colours the program set for itself in place
 * of the theme's own, and with the two defaults changed places while the program has the screen in
 * reverse video.
 *
 * **Only the defaults change places.** A character with a colour of its own keeps it, as it does in every
 * terminal: reverse video is about the screen a program did not colour.
 */
export function sessionTheme(theme: Theme, colors: SessionColors | null, reverseVideo: boolean): Theme {
  if (!reverseVideo && (!colors || (colors.foreground === null && colors.background === null && colors.cursor === null && colors.palette.length === 0))) return theme
  let palette = theme.palette
  if (colors && colors.palette.length > 0) {
    const changed = [...theme.palette]
    for (const entry of colors.palette) changed[entry.index] = entry.rgb
    palette = changed
  }
  const foreground = colors?.foreground ?? theme.foreground
  const background = colors?.background ?? theme.background
  return {
    ...theme,
    palette,
    foreground: reverseVideo ? background : foreground,
    background: reverseVideo ? foreground : background,
    cursor: colors?.cursor ?? theme.cursor,
  }
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
 *
 * **A selection is one rectangle over the cells it covers, whatever is in them.** A row lists the characters
 * a program wrote and not the blank cells between or after them, so a background made from characters
 * would have a hole wherever nothing was written. The rectangle is the selection's own; the characters
 * under it give up their backgrounds and take its text colour.
 */
export function drawRow(row: Row, theme: Theme, selected?: SelectedCells): RowDrawing {
  const backgrounds: Array<{ column: number; cells: number; color: number }> = []
  const glyphs: Array<RowDrawing['glyphs'][number]> = []
  const lines: Array<{ kind: LineKind; column: number; cells: number; color: number }> = []

  /** Extends the last run when this one continues it, which is what keeps a coloured line one rectangle */
  const extend = <T extends { column: number; cells: number }>(runs: T[], next: T, same: (last: T) => boolean) => {
    const last = runs.at(-1)
    if (last && last.column + last.cells === next.column && same(last)) last.cells += next.cells
    else runs.push(next)
  }

  // The cells the selection's rectangle covers: what was asked for, widened to the whole of any character
  // it only partly reaches
  let selectedFrom = selected?.from ?? 0
  let selectedTo = selected?.to ?? 0

  for (let index = 0; index < row.text.length; index += 1) {
    const text = row.text[index]!
    const column = row.column[index]!
    const cells = row.width[index]!
    const style = row.styles[row.style[index]!] ?? PLAIN
    const flags = style.flags

    let fg = colorIn(theme, style.fg) ?? theme.foreground
    let bg = colorIn(theme, style.bg)
    if (flags & StyleFlag.inverse) [fg, bg] = [bg ?? theme.background, fg]
    const isSelected = selected !== undefined && column < selected.to && column + cells > selected.from
    if (isSelected) {
      fg = theme.selectionForeground ?? fg
      selectedFrom = Math.min(selectedFrom, column)
      selectedTo = Math.max(selectedTo, column + cells)
    } else if (bg !== null && bg !== theme.background) {
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

  if (selectedTo > selectedFrom) {
    // Among the others in the order of its cells, which is the order they are all in
    const after = backgrounds.findIndex((run) => run.column >= selectedTo)
    backgrounds.splice(after === -1 ? backgrounds.length : after, 0, { column: selectedFrom, cells: selectedTo - selectedFrom, color: theme.selectionBackground })
  }

  return { backgrounds, glyphs, lines }
}
