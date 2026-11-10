import { PLAIN, type Frame, type KeyInput, type PasteOutcome, type PointerInput, type Position, type Row, type Rows, type SourceListener, type Span, type TerminalSource } from '../src/source.ts'

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

  async fetch(first: number, count: number): Promise<Rows & { epoch: number }> {
    this.fetched.push([first, count])
    const rows: Row[] = []
    for (let line = first; line < first + count; line += 1) {
      const row = this.history.get(line)
      if (row) rows.push(row)
    }
    return { epoch: this.epoch, styles: [PLAIN], links: [''], rows }
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
  async search(_needle: string): Promise<readonly Span[]> { return [] }
}
