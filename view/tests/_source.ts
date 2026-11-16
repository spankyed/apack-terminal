import type { Command, Fetched, Frame, Picture, Placements, KeyInput, PasteOutcome, PointerInput, Position, Row, SourceListener, Span, TerminalSource } from '../src/source.ts'

/** A source a case drives by hand: it sends what the case tells it to and records what the view asked of it */
export class FakeSource implements TerminalSource {
  listener: SourceListener | null = null
  attached = 0
  readonly acknowledged: number[] = []
  readonly fetched: Array<[first: number, count: number]> = []
  readonly keys: KeyInput[] = []
  readonly texts: string[] = []
  readonly pointers: PointerInput[] = []
  readonly pastes: Array<{ text: string; allowUnsafe: boolean }> = []
  readonly focus_: boolean[] = []
  /** History the source will answer a fetch from, by line */
  readonly history = new Map<number, Row>()
  epoch = 1
  pasteOutcome: PasteOutcome = 'pasted'

  attach(listener: SourceListener): () => void {
    this.listener = listener
    this.attached += 1
    return () => { if (this.listener === listener) this.listener = null }
  }

  /** Sends the view a frame, as the owner would */
  send(frame: Frame): void {
    this.listener?.frame(frame)
  }

  acknowledge(sequence: number): void { this.acknowledged.push(sequence) }

  /** Answers for every line asked about: one it has no history for is one it does not hold */
  async fetch(first: number, count: number): Promise<Fetched> {
    this.fetched.push([first, count])
    const rows: Row[] = []
    for (let line = first; line < first + count; line += 1) {
      const row = this.history.get(line)
      if (row) rows.push(row)
    }
    return { epoch: this.epoch, first, count, rows }
  }

  key(input: KeyInput): void { this.keys.push(input) }
  text(text: string): void { this.texts.push(text) }
  pointer(input: PointerInput): void { this.pointers.push(input) }
  async paste(text: string, options?: { allowUnsafe?: boolean }): Promise<PasteOutcome> {
    this.pastes.push({ text, allowUnsafe: options?.allowUnsafe ?? false })
    return this.pasteOutcome
  }
  focus(gained: boolean): void { this.focus_.push(gained) }
  async wordAt(_position: Position): Promise<Span | null> { return null }
  async lineAt(_position: Position): Promise<Span | null> { return null }
  async textOf(_span: Span): Promise<string> { return '' }
  async search(_needle: string, _caseSensitive: boolean): Promise<readonly Span[]> { return [] }
  /** What the source answers when asked for its commands, and how many times it was asked */
  commandList: Command[] = []
  commandsAsked = 0
  async commands(): Promise<readonly Command[]> { this.commandsAsked += 1; return this.commandList }
  /** Where the source says its images are, how many times it was asked, and the pixels it answers with */
  placed: Placements = { cellWidth: 0, cellHeight: 0, placements: [] }
  imagesAsked = 0
  readonly pictures = new Map<number, Picture>()
  readonly picturesAsked: number[] = []
  async images(): Promise<Placements> { this.imagesAsked += 1; return this.placed }
  async image(id: number): Promise<Picture | null> { this.picturesAsked.push(id); return this.pictures.get(id) ?? null }
}
