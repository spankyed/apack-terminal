import {
  DEFAULT_WORD_DELIMITERS,
  type CellPos,
  type CellSpan,
  type CommandRecord,
  type CursorShape,
  type FrameUpdate,
  type ImageBlobData,
  type ImagePlacement,
  type KeyInput,
  type PackedRow,
  type PasteOutcome,
  type PointerInput,
  type SearchPatch,
  type SearchSnapshot,
  type SessionEvent,
  type SourceSink,
  type TerminalSource,
  type ViewportHint,
} from '../src/source.ts'
import { blobHashToHex } from '../src/wire/types.ts'

/** In-memory v2 `TerminalSource` test double recording every view call and pushing frames/deltas/events. */
export class FakeSource implements TerminalSource {
  wordDelimiters = DEFAULT_WORD_DELIMITERS
  sink: SourceSink | null = null
  subscribed = 0
  readonly acknowledged: number[] = []
  readonly hints: ViewportHint[] = []
  readonly fetched: Array<{ readonly epoch: number; readonly fromLine: number; readonly count: number }> = []
  readonly keys: KeyInput[] = []
  readonly texts: string[] = []
  readonly pointers: PointerInput[] = []
  readonly pastes: Array<{ readonly text: string; readonly allowUnsafe: boolean }> = []
  readonly focus_: boolean[] = []
  readonly searches: Array<{ readonly query: string; readonly caseSensitive: boolean }> = []
  readonly remoteWordCalls: CellPos[] = []
  readonly remoteLineCalls: number[] = []
  readonly remoteTextCalls: CellSpan[] = []
  readonly remoteRectCalls: CellSpan[] = []

  readonly history = new Map<number, PackedRow>()
  epoch = 1
  pasteOutcome: PasteOutcome = 'pasted'
  commandList: CommandRecord[] = []
  commandsAsked = 0
  imagePlacements: ImagePlacement[] = []
  imagesGen = 0
  imagesAsked = 0
  readonly blobs = new Map<string, ImageBlobData>()
  readonly blobsAsked: string[] = []
  searchResult: SearchSnapshot = { epoch: 1, matches: [], truncated: false }
  remoteTextAnswer: string | ((span: CellSpan) => string) = ''
  remoteRectAnswer: string | ((span: CellSpan) => string) = ''
  remoteWordAnswer: CellSpan | null = null
  remoteLineAnswer: CellSpan | null = null

  subscribe(sink: SourceSink): () => void {
    this.sink = sink
    this.subscribed += 1
    return () => {
      if (this.sink === sink) this.sink = null
    }
  }

  send(frame: FrameUpdate): void {
    this.epoch = frame.epoch
    this.sink?.onFrame(frame)
  }

  pushSearchMatches(snapshot: SearchSnapshot): void {
    this.sink?.onSearchMatches(snapshot)
  }

  pushSearchDelta(patch: SearchPatch): void {
    this.sink?.onSearchDelta(patch)
  }

  pushEvent(event: SessionEvent): void {
    this.sink?.onEvent?.(event)
  }

  ackFrame(seq: number): void {
    this.acknowledged.push(seq)
  }

  sendViewportHint(hint: ViewportHint): void {
    this.hints.push(hint)
  }

  setCursorDefaults(_shape: CursorShape, _blinking: boolean): void {}

  sendKey(input: KeyInput): void {
    this.keys.push(input)
  }

  sendText(text: string): void {
    this.texts.push(text)
  }

  sendPointer(input: PointerInput): void {
    this.pointers.push(input)
  }

  sendFocus(focused: boolean): void {
    this.focus_.push(focused)
  }

  async paste(text: string, allowUnsafe = false): Promise<PasteOutcome> {
    this.pastes.push({ text, allowUnsafe })
    return this.pasteOutcome
  }

  async fetchRows(epoch: number, fromLine: number, count: number): Promise<readonly PackedRow[]> {
    this.fetched.push({ epoch, fromLine, count })
    const rows: PackedRow[] = []
    for (let line = fromLine; line < fromLine + count; line += 1) {
      const row = this.history.get(line)
      if (row) rows.push(row)
    }
    return rows
  }

  async wordAt(_epoch: number, pos: CellPos): Promise<CellSpan | null> {
    this.remoteWordCalls.push(pos)
    return this.remoteWordAnswer
  }

  async lineAt(_epoch: number, line: number): Promise<CellSpan | null> {
    this.remoteLineCalls.push(line)
    return this.remoteLineAnswer
  }

  async textOf(_epoch: number, span: CellSpan): Promise<string> {
    this.remoteTextCalls.push(span)
    return typeof this.remoteTextAnswer === 'function' ? this.remoteTextAnswer(span) : this.remoteTextAnswer
  }

  async textOfRect(_epoch: number, span: CellSpan): Promise<string> {
    this.remoteRectCalls.push(span)
    return typeof this.remoteRectAnswer === 'function' ? this.remoteRectAnswer(span) : this.remoteRectAnswer
  }

  async setSearch(query: string, caseSensitive: boolean): Promise<SearchSnapshot> {
    this.searches.push({ query, caseSensitive })
    return { ...this.searchResult, epoch: this.epoch }
  }

  async commands(): Promise<readonly CommandRecord[]> {
    this.commandsAsked += 1
    return this.commandList
  }

  async images(): Promise<{ readonly imagesGen: number; readonly placements: readonly ImagePlacement[] }> {
    this.imagesAsked += 1
    return { imagesGen: this.imagesGen, placements: this.imagePlacements }
  }

  async getImageBlob(blob: Uint8Array): Promise<ImageBlobData> {
    const key = blobHashToHex(blob)
    this.blobsAsked.push(key)
    const found = this.blobs.get(key)
    if (!found) throw new Error(`missing blob ${key}`)
    return found
  }
}
