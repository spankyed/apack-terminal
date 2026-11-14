// The broker's half of the protocol, for a suite that has no broker: what it would send, encoded, and what
// a view sent it, decoded. `wire.spec.ts` holds both to the vectors the real one wrote, so a fake built on
// them speaks the bytes the real one does.
import { KEY_ACTIONS, POINTER_ACTIONS, CURSOR_SHAPES, paletteIndex, type Color, type Position, type Rows } from '../src/source.ts'
import { Messages, type ByteStream, type ToBroker, type ToView } from '../src/index.ts'

const utf8 = new TextEncoder()

class Out {
  readonly bytes: number[] = [0, 0, 0, 0]
  byte(value: number): void { this.bytes.push(value) }
  flag(value: boolean): void { this.bytes.push(value ? 1 : 0) }
  number(value: number): void {
    let rest = value
    for (;;) {
      const low = rest % 128
      rest = Math.floor(rest / 128)
      if (rest === 0) return this.byte(low)
      this.byte(low | 0x80)
    }
  }
  text(value: string): void {
    const encoded = utf8.encode(value)
    this.number(encoded.length)
    this.bytes.push(...encoded)
  }
  position(at: Position): void { this.number(at.line); this.number(at.column) }
  color(color: Color): void {
    if (color === null) return this.byte(0)
    const index = paletteIndex(color)
    if (index !== null) { this.byte(1); this.byte(index); return }
    this.byte(2)
    this.bytes.push((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff)
  }
  rows(block: Rows): void {
    this.number(block.rows.length)
    for (const row of block.rows) {
      this.number(row.line)
      this.byte((row.wrapped ? 1 : 0) | (row.prompt ? 2 : 0))
      // The row's own tables, less the plain style and the empty link every row has in front
      this.number(row.styles.length - 1)
      for (const style of row.styles.slice(1)) {
        this.color(style.fg); this.color(style.bg); this.color(style.underlineColor); this.number(style.flags)
      }
      this.number(row.links.length - 1)
      for (const link of row.links.slice(1)) this.text(link)
      // Characters side by side that share a style and a link, and are all plain or all not
      const runs: Array<{ start: number; end: number; simple: boolean }> = []
      row.text.forEach((text, index) => {
        const simple = row.width[index] === 1 && [...text].length === 1
        const last = runs.at(-1)
        const before = index - 1
        if (last && row.column[before]! + row.width[before]! === row.column[index] && row.style[before] === row.style[index]
          && row.link[before] === row.link[index] && last.simple === simple) last.end = index + 1
        else runs.push({ start: index, end: index + 1, simple })
      })
      this.number(runs.length)
      for (const run of runs) {
        this.number(row.column[run.start]!); this.number(row.style[run.start]!); this.number(row.link[run.start]!)
        const texts = row.text.slice(run.start, run.end)
        if (run.simple) { this.byte(0); this.text(texts.join('')) }
        else {
          this.byte(1); this.number(texts.length)
          texts.forEach((text, offset) => { this.byte(row.width[run.start + offset]!); this.text(text) })
        }
      }
      // The parts of images the row shows: how many, and for a row that shows any, their cell and each
      const pieces = row.images?.pieces ?? []
      this.number(pieces.length)
      if (row.images && pieces.length > 0) { this.number(row.images.cellWidth); this.number(row.images.cellHeight) }
      for (const piece of pieces) {
        for (const value of [piece.image, piece.generation, piece.column, piece.columns, piece.offsetX, piece.offsetY,
          piece.width, piece.height, piece.sourceX, piece.sourceY, piece.sourceWidth, piece.sourceHeight]) this.number(value)
      }
    }
  }
  finish(): Uint8Array {
    const bytes = Uint8Array.from(this.bytes)
    new DataView(bytes.buffer).setUint32(0, bytes.length - 4, true)
    return bytes
  }
}

/** A message for a view, as the broker encodes one */
export function encodeToView(message: ToView): Uint8Array {
  const out = new Out()
  const kinds = { frame: 65, rows: 66, ended: 67, pasted: 68, span: 69, text: 70, spans: 71, failed: 72, commands: 73, images: 74, image: 75 } as const
  out.byte(kinds[message.kind])
  out.number(message.channel)
  switch (message.kind) {
    case 'frame': {
      const { frame } = message
      for (const value of [frame.sequence, frame.epoch, frame.firstActive, frame.columns, frame.screenRows, frame.scrollback, frame.commands, frame.images]) out.number(value)
      out.byte((frame.cursor ? 1 : 0) | (frame.modes.alternateScreen ? 2 : 0) | (frame.modes.mouse ? 4 : 0)
        | (frame.staleFrom === undefined ? 0 : 8) | (frame.cursor?.blinking ? 16 : 0) | (frame.reverseVideo ? 32 : 0) | (frame.colors ? 64 : 0)
        | (frame.cursor?.hidden ? 128 : 0))
      if (frame.cursor) { out.number(frame.cursor.column); out.number(frame.cursor.row); out.byte(CURSOR_SHAPES.indexOf(frame.cursor.shape)) }
      if (frame.staleFrom !== undefined) out.number(frame.staleFrom)
      if (frame.colors) {
        const set = [frame.colors.foreground, frame.colors.background, frame.colors.cursor]
        out.byte(set.reduce<number>((bits, color, bit) => bits | (color === null ? 0 : 1 << bit), 0))
        const rgb = (color: number) => { out.byte((color >> 16) & 0xff); out.byte((color >> 8) & 0xff); out.byte(color & 0xff) }
        for (const color of set) if (color !== null) rgb(color)
        out.number(frame.colors.palette.length)
        for (const entry of frame.colors.palette) { out.byte(entry.index); rgb(entry.rgb) }
      }
      out.rows(frame)
      break
    }
    case 'rows': for (const value of [message.request, message.epoch, message.first, message.count]) out.number(value); out.rows(message.rows); break
    case 'ended': break
    case 'pasted': out.number(message.request); out.byte(message.outcome === 'pasted' ? 0 : 1); break
    case 'span':
      out.number(message.request); out.number(message.epoch); out.flag(message.span !== null)
      if (message.span) { out.position(message.span.start); out.position(message.span.end) }
      break
    case 'text': out.number(message.request); out.number(message.epoch); out.text(message.text); break
    case 'spans':
      out.number(message.request); out.number(message.epoch); out.number(message.spans.length)
      for (const span of message.spans) { out.position(span.start); out.position(span.end) }
      break
    case 'failed': out.number(message.request); out.text(message.message); break
    case 'commands':
      out.number(message.request); out.number(message.epoch); out.number(message.commands.length)
      for (const command of message.commands) {
        out.number(command.id)
        const fields = [command.command, command.cwd, command.startedMs, command.finishedMs, command.prompt, command.output, command.end]
        out.byte(fields.reduce<number>((flags, field, bit) => flags | (field === null ? 0 : 1 << bit), 0))
        if (command.command !== null) out.text(command.command)
        if (command.cwd !== null) out.text(command.cwd)
        for (const time of [command.startedMs, command.finishedMs]) if (time !== null) out.number(time)
        const status = (code: number) => out.number(code >= 0 ? code * 2 : -code * 2 - 1)
        const { outcome } = command
        switch (outcome.kind) {
          case 'running': out.byte(0); break
          case 'exited': out.byte(1); status(outcome.code); break
          case 'unknown': out.byte(2); out.byte(['no_status', 'superseded', 'session_ended'].indexOf(outcome.why)); break
          case 'claimed':
            if (outcome.code === null) out.byte(3)
            else { out.byte(4); status(outcome.code) }
            break
        }
        for (const line of [command.prompt, command.output, command.end]) if (line !== null) out.number(line)
      }
      break
    case 'images':
      for (const value of [message.request, message.epoch, message.cellWidth, message.cellHeight, message.placements.length]) out.number(value)
      for (const placed of message.placements) {
        out.number(placed.image); out.number(placed.generation)
        out.number(placed.z >= 0 ? placed.z * 2 : -placed.z * 2 - 1)
        for (const value of [
          placed.line, placed.column, placed.columns, placed.rows, placed.offsetX, placed.offsetY, placed.width, placed.height,
          placed.sourceX, placed.sourceY, placed.sourceWidth, placed.sourceHeight,
        ]) out.number(value)
      }
      break
    case 'image':
      for (const value of [message.request, message.image, message.generation, message.width, message.height]) out.number(value)
      out.byte(message.channels)
      for (const value of [message.total, message.offset, message.bytes.length]) out.number(value)
      for (const byte of message.bytes) out.byte(byte)
      break
  }
  return out.finish()
}

/** A message from a view, as the broker decodes one */
export function decodeToBroker(body: Uint8Array): ToBroker {
  let at = 0
  const byte = () => body[at++]!
  const number = () => {
    let value = 0
    for (let scale = 1; ; scale *= 128) {
      const next = byte()
      value += (next & 0x7f) * scale
      if (!(next & 0x80)) return value
    }
  }
  const text = () => {
    const length = number()
    at += length
    return new TextDecoder().decode(body.subarray(at - length, at))
  }
  const float = () => {
    at += 4
    return new DataView(body.buffer, body.byteOffset + at - 4, 4).getFloat32(0, true)
  }
  const position = (): Position => ({ line: number(), column: number() })
  const kind = byte()
  const channel = number()
  switch (kind) {
    case 1: return { kind: 'attach', channel, session: text() }
    case 2: return { kind: 'detach', channel }
    case 3: return { kind: 'acknowledge', channel, sequence: number() }
    case 4: return { kind: 'fetch', channel, request: number(), epoch: number(), first: number(), count: number() }
    case 5: return { kind: 'key', channel, action: KEY_ACTIONS[byte()]!, modifiers: number(), code: text(), text: text(), unshifted: number(), composed: byte() === 1 }
    case 6: return { kind: 'text', channel, text: text() }
    case 7: {
      const action = POINTER_ACTIONS[byte()]!
      const button = byte()
      return { kind: 'pointer', channel, action, button: button === 255 ? -1 : button, modifiers: number(), x: float(), y: float(), cellWidth: float(), cellHeight: float() }
    }
    case 8: return { kind: 'paste', channel, request: number(), allowUnsafe: byte() === 1, text: text() }
    case 9: return { kind: 'focus', channel, gained: byte() === 1 }
    case 10: return { kind: 'wordAt', channel, request: number(), epoch: number(), at: position() }
    case 11: return { kind: 'lineAt', channel, request: number(), epoch: number(), at: position() }
    case 12: return { kind: 'textOf', channel, request: number(), epoch: number(), span: { start: position(), end: position() } }
    case 13: return { kind: 'search', channel, request: number(), needle: text(), caseSensitive: byte() === 1 }
    case 14: return { kind: 'commands', channel, request: number() }
    case 15: return { kind: 'images', channel, request: number() }
    case 16: return { kind: 'image', channel, request: number(), image: number(), offset: number() }
    default: throw new Error(`a view sent kind ${kind}`)
  }
}

/** The stream a view is given, with a broker's end a case can hold: what the view sent, and a way to answer */
export class FakeBroker implements ByteStream {
  /** Everything the view has sent, in order */
  readonly received: ToBroker[] = []
  private readonly messages = new Messages()
  private data: ((chunk: Uint8Array) => void) | null = null
  private closing: (() => void) | null = null

  send(bytes: Uint8Array): void {
    for (const body of this.messages.push(bytes)) this.received.push(decodeToBroker(body))
  }
  onData(listener: (chunk: Uint8Array) => void): () => void {
    this.data = listener
    return () => { this.data = null }
  }
  onClose(listener: () => void): () => void {
    this.closing = listener
    return () => { this.closing = null }
  }

  /** The broker sends a view these messages, in one piece */
  say(...messages: ToView[]): void {
    const pieces = messages.map(encodeToView)
    const whole = new Uint8Array(pieces.reduce((length, piece) => length + piece.length, 0))
    pieces.reduce((at, piece) => (whole.set(piece, at), at + piece.length), 0)
    this.data?.(whole)
  }
  /** Bytes exactly as given, for what is not a message */
  raw(bytes: Uint8Array): void { this.data?.(bytes) }
  hangUp(): void { this.closing?.() }
  /** What the view has sent since this was last asked */
  taken(): ToBroker[] { return this.received.splice(0) }
}
