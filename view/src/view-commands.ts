import { drawRow, type RowDrawing, type Theme } from './draw.ts'
import { type Scene } from './painter.ts'
import { type CellSpan, type CommandRecord, type PackedRow } from './source.ts'
import { type Viewport } from './viewport.ts'
import { buildFoldRow, firstAfter, markColor, type Fold } from './view-types.ts'

interface CommandLine {
  readonly line: number
  readonly command: CommandRecord
}

/**
 * Owns shell command prompt indexing/navigation and folded-line header synthesis/caching.
 */
export class CommandFoldController {
  private commandList: CommandRecord[] = []
  private commandStarts: readonly CommandLine[] = []
  private commandPrompts: readonly CommandLine[] = []
  private foldList: readonly Fold[] = []
  private readonly foldRows = new Map<string, PackedRow>()
  private readonly foldDrawings = new Map<PackedRow, RowDrawing>()

  constructor(
    private readonly viewport: Viewport,
    private readonly gutter: number,
    private readonly onCommands?: (commands: readonly CommandRecord[]) => void,
  ) {}

  get commands(): readonly CommandRecord[] {
    return this.commandList
  }

  setFolds(folds: readonly Fold[]): void {
    const sorted = folds.filter((fold) => fold.last > fold.first).sort((a, b) => a.first - b.first)
    for (let index = 1; index < sorted.length; index += 1) {
      if (sorted[index]!.first <= sorted[index - 1]!.last) throw new RangeError('folds overlap')
    }
    this.foldList = sorted
    this.viewport.setFolds(sorted)
    this.foldRows.clear()
    this.foldDrawings.clear()
  }

  clearFolds(): void {
    this.foldList = []
    this.viewport.setFolds([])
    this.foldRows.clear()
    this.foldDrawings.clear()
  }

  clearFoldDrawings(): void {
    this.foldDrawings.clear()
  }

  foldStartingAt(line: number): Fold | undefined {
    return this.foldList.find((known) => known.first === line)
  }

  hiddenIn(line: number): Fold | undefined {
    return this.viewport.hiddenIn(line)
  }

  layout(): { first: number; lines: number[] } {
    const [top, count] = this.viewport.visible
    const hidden = this.hiddenIn(top)
    const first = hidden ? hidden.first : top
    const lines: number[] = []
    let line = first
    while (lines.length < count) {
      const fold = this.hiddenIn(line)
      if (fold) line = fold.last + 1
      else {
        lines.push(line)
        line += 1
      }
    }
    return { first, lines }
  }

  foldRow(fold: Fold): PackedRow {
    const label = `▸ ${fold.label ?? `${fold.last - fold.first} lines folded`}`
    const key = `${fold.first}:${fold.last}:${label}`
    const known = this.foldRows.get(key)
    if (known) return known
    const row = buildFoldRow(fold)
    this.foldRows.set(key, row)
    return row
  }

  foldDrawing(fold: Fold, theme: Theme): RowDrawing {
    const row = this.foldRow(fold)
    let drawing = this.foldDrawings.get(row)
    if (!drawing) {
      drawing = drawRow(row, theme)
      this.foldDrawings.set(row, drawing)
    }
    return drawing
  }

  setCommands(commands: readonly CommandRecord[]): void {
    this.commandList = [...commands]
    this.rebuildCommandIndex()
    this.onCommands?.(this.commandList)
  }

  upsertCommand(record: CommandRecord): void {
    const idx = this.commandList.findIndex((c) => c.id === record.id)
    if (idx === -1) this.commandList.push(record)
    else this.commandList[idx] = record
    this.rebuildCommandIndex()
    this.onCommands?.(this.commandList)
  }

  private rebuildCommandIndex(): void {
    const starts: CommandLine[] = []
    const prompts: CommandLine[] = []
    for (const command of this.commandList) {
      const begins = command.promptLine ?? command.outputLine
      if (begins !== null) starts.push({ line: begins, command })
      if (command.promptLine !== null) prompts.push({ line: command.promptLine, command })
    }
    starts.sort((a, b) => a.line - b.line)
    prompts.sort((a, b) => a.line - b.line)
    this.commandStarts = starts
    this.commandPrompts = prompts
  }

  commandAt(line: number): CommandRecord | null {
    return this.commandStarts[firstAfter(this.commandStarts, line) - 1]?.command ?? null
  }

  outputOf(command: CommandRecord, hasFrame: boolean, firstActive: number, rows: number, columns: number): CellSpan | null {
    if (command.outputLine === null || !hasFrame) return null
    const last = command.endLine !== null ? command.endLine - 1 : firstActive + rows - 1
    if (last < command.outputLine) return null
    return {
      start: { line: command.outputLine, col: 0 },
      end: { line: last, col: Math.max(0, columns - 1) },
    }
  }

  scrollTarget(direction: -1 | 1): number | 'follow' | null {
    const top = Math.floor(this.viewport.top)
    const prompts = this.commandPrompts
    const target = direction === -1
      ? prompts[firstAfter(prompts, top - 1) - 1]
      : prompts[firstAfter(prompts, top)]
    if (target !== undefined) return target.line
    if (direction === 1) return 'follow'
    return null
  }

  marksIn(first: number, lines: number[], theme: Theme): NonNullable<Scene['marks']> {
    if (this.gutter === 0 || lines.length === 0) return []
    const prompts = this.commandPrompts
    const last = lines[lines.length - 1]!
    const marks: Array<{ row: number; color: number }> = []
    for (let index = firstAfter(prompts, first - 1); index < prompts.length && prompts[index]!.line <= last; index += 1) {
      const { line, command } = prompts[index]!
      if (this.hiddenIn(line)) continue
      marks.push({
        row: this.viewport.rowsBetween(first, line),
        color: theme.palette[markColor(command)] ?? theme.foreground,
      })
    }
    return marks
  }
}
