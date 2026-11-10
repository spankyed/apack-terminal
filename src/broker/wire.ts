/**
 * What a view and a broker say to each other, as bytes.
 *
 * This is the view's half of `apack-terminal`'s view protocol (`src/view/wire.rs` there): it decodes what
 * a view is sent and encodes what a view sends, and nothing else. The other half is in Rust, in another
 * repository, so neither suite can see both — what holds them together is `tests/vectors/view-vectors.json`,
 * which the broker writes from its encoder and this reads.
 *
 * **A message is a length, a kind and a payload**: a `u32` little-endian count of what follows, one byte
 * saying which message, and that message's fields in order. Integers are LEB128; a string is its length
 * and its UTF-8.
 */
import {
  CURSOR_SHAPES,
  KEY_ACTIONS,
  POINTER_ACTIONS,
  PLAIN,
  paletteColor,
  type Color,
  type Command,
  type Cursor,
  type Frame,
  type KeyInput,
  type PasteOutcome,
  type Placement,
  type Placements,
  type PointerInput,
  type Position,
  type Row,
  type Rows,
  type Span,
  type Style,
} from '../source.ts'

/** The largest message either side may send, as the broker has it */
export const MAX_MESSAGE_BYTES = 8 << 20

/** What a view sends. `channel` is the number the view gave the session when it attached */
export type ToBroker =
  | { readonly kind: 'attach'; readonly channel: number; readonly session: string }
  | { readonly kind: 'detach'; readonly channel: number }
  | { readonly kind: 'acknowledge'; readonly channel: number; readonly sequence: number }
  | { readonly kind: 'fetch'; readonly channel: number; readonly request: number; readonly epoch: number; readonly first: number; readonly count: number }
  | ({ readonly kind: 'key'; readonly channel: number } & KeyInput)
  | { readonly kind: 'text'; readonly channel: number; readonly text: string }
  | ({ readonly kind: 'pointer'; readonly channel: number } & PointerInput)
  | { readonly kind: 'paste'; readonly channel: number; readonly request: number; readonly allowUnsafe: boolean; readonly text: string }
  | { readonly kind: 'focus'; readonly channel: number; readonly gained: boolean }
  | { readonly kind: 'wordAt' | 'lineAt'; readonly channel: number; readonly request: number; readonly epoch: number; readonly at: Position }
  | { readonly kind: 'textOf'; readonly channel: number; readonly request: number; readonly epoch: number; readonly span: Span }
  | { readonly kind: 'search'; readonly channel: number; readonly request: number; readonly needle: string }
  | { readonly kind: 'commands'; readonly channel: number; readonly request: number }
  | { readonly kind: 'images'; readonly channel: number; readonly request: number }
  | { readonly kind: 'image'; readonly channel: number; readonly request: number; readonly image: number; readonly offset: number }

/** What a view is sent */
export type ToView =
  | { readonly kind: 'frame'; readonly channel: number; readonly frame: Frame }
  | { readonly kind: 'rows'; readonly channel: number; readonly request: number; readonly epoch: number; readonly rows: Rows }
  | { readonly kind: 'ended'; readonly channel: number }
  | { readonly kind: 'pasted'; readonly channel: number; readonly request: number; readonly outcome: PasteOutcome }
  | { readonly kind: 'span'; readonly channel: number; readonly request: number; readonly epoch: number; readonly span: Span | null }
  | { readonly kind: 'text'; readonly channel: number; readonly request: number; readonly text: string }
  | { readonly kind: 'spans'; readonly channel: number; readonly request: number; readonly epoch: number; readonly spans: readonly Span[] }
  | { readonly kind: 'failed'; readonly channel: number; readonly request: number; readonly message: string }
  | { readonly kind: 'commands'; readonly channel: number; readonly request: number; readonly epoch: number; readonly commands: readonly Command[] }
  | ({ readonly kind: 'images'; readonly channel: number; readonly request: number; readonly epoch: number } & Placements)
  | { readonly kind: 'image'; readonly channel: number; readonly request: number; readonly image: number; readonly generation: number; readonly width: number; readonly height: number; readonly channels: number; readonly total: number; readonly offset: number; readonly bytes: Uint8Array }

/** Bytes that are not a message. A view that reads one cannot trust what follows it either */
export class Malformed extends Error {}

const KIND = {
  attach: 1, detach: 2, acknowledge: 3, fetch: 4, key: 5, text: 6, pointer: 7, paste: 8, focus: 9,
  wordAt: 10, lineAt: 11, textOf: 12, search: 13, commands: 14, images: 15, image: 16,
} as const

const FRAME = 65
const ROWS = 66
const ENDED = 67
const PASTED = 68
const SPAN = 69
const TEXT = 70
const SPANS = 71
const FAILED = 72
const COMMANDS = 73
const IMAGES = 74
const IMAGE = 75

const FRAME_CURSOR = 1
const FRAME_ALTERNATE = 2
const FRAME_MOUSE = 4
const FRAME_STALE = 8
const FRAME_BLINKING = 16

/** The button byte for a move with nothing held, or a wheel */
const NO_BUTTON = 255

const utf8 = new TextEncoder()
const text = new TextDecoder('utf-8', { fatal: true })

class Writer {
  private bytes = new Uint8Array(64)
  private length = 4

  constructor(kind: number) {
    this.byte(kind)
  }

  private room(more: number): void {
    if (this.length + more <= this.bytes.length) return
    const grown = new Uint8Array(Math.max(this.bytes.length * 2, this.length + more))
    grown.set(this.bytes)
    this.bytes = grown
  }

  byte(value: number): void {
    this.room(1)
    this.bytes[this.length++] = value
  }

  flag(value: boolean): void {
    this.byte(value ? 1 : 0)
  }

  /** An integer up to 2^53, seven bits a byte, low bits first */
  number(value: number): void {
    if (!Number.isSafeInteger(value) || value < 0) throw new RangeError(`${value} is not a number a message can carry`)
    let rest = value
    for (;;) {
      const low = rest % 128
      rest = Math.floor(rest / 128)
      if (rest === 0) return this.byte(low)
      this.byte(low | 0x80)
    }
  }

  float(value: number): void {
    this.room(4)
    new DataView(this.bytes.buffer).setFloat32(this.length, value, true)
    this.length += 4
  }

  text(value: string): void {
    const encoded = utf8.encode(value)
    this.number(encoded.length)
    this.room(encoded.length)
    this.bytes.set(encoded, this.length)
    this.length += encoded.length
  }

  position(at: Position): void {
    this.number(at.line)
    this.number(at.column)
  }

  finish(): Uint8Array {
    new DataView(this.bytes.buffer).setUint32(0, this.length - 4, true)
    return this.bytes.subarray(0, this.length)
  }
}

/** A message for the broker, with its length in front */
export function encodeToBroker(message: ToBroker): Uint8Array {
  const out = new Writer(KIND[message.kind])
  out.number(message.channel)
  switch (message.kind) {
    case 'attach':
      out.text(message.session)
      break
    case 'detach':
      break
    case 'acknowledge':
      out.number(message.sequence)
      break
    case 'fetch':
      out.number(message.request)
      out.number(message.epoch)
      out.number(message.first)
      out.number(message.count)
      break
    case 'key':
      out.byte(KEY_ACTIONS.indexOf(message.action))
      out.number(message.modifiers)
      out.text(message.code)
      out.text(message.text)
      out.number(message.unshifted)
      break
    case 'text':
      out.text(message.text)
      break
    case 'pointer':
      out.byte(POINTER_ACTIONS.indexOf(message.action))
      out.byte(message.button < 0 ? NO_BUTTON : message.button)
      out.number(message.modifiers)
      out.float(message.x)
      out.float(message.y)
      out.float(message.cellWidth)
      out.float(message.cellHeight)
      break
    case 'paste':
      out.number(message.request)
      out.flag(message.allowUnsafe)
      out.text(message.text)
      break
    case 'focus':
      out.flag(message.gained)
      break
    case 'wordAt':
    case 'lineAt':
      out.number(message.request)
      out.number(message.epoch)
      out.position(message.at)
      break
    case 'textOf':
      out.number(message.request)
      out.number(message.epoch)
      out.position(message.span.start)
      out.position(message.span.end)
      break
    case 'search':
      out.number(message.request)
      out.text(message.needle)
      break
    case 'commands':
    case 'images':
      out.number(message.request)
      break
    case 'image':
      out.number(message.request)
      out.number(message.image)
      out.number(message.offset)
      break
  }
  return out.finish()
}

class Reader {
  private at = 0

  constructor(private readonly bytes: Uint8Array) {}

  byte(): number {
    if (this.at >= this.bytes.length) throw new Malformed('the message ended before its last field')
    return this.bytes[this.at++]!
  }

  flag(): boolean {
    const value = this.byte()
    if (value > 1) throw new Malformed('a flag is outside what the format defines')
    return value === 1
  }

  take(count: number): Uint8Array {
    if (this.bytes.length - this.at < count) throw new Malformed('the message ended before its last field')
    const taken = this.bytes.subarray(this.at, this.at + count)
    this.at += count
    return taken
  }

  /** An integer of up to 2^53: a view's line numbers are JavaScript numbers, and past that they are not exact */
  number(): number {
    let value = 0
    let scale = 1
    for (let read = 0; read < 8; read += 1) {
      const byte = this.byte()
      value += (byte & 0x7f) * scale
      if (!(byte & 0x80)) {
        if (!Number.isSafeInteger(value)) break
        return value
      }
      scale *= 128
    }
    throw new Malformed('an integer did not fit its field')
  }

  /** A count of things that each take at least a byte, refused before anything is made for it */
  count(): number {
    const count = this.number()
    if (count > this.bytes.length - this.at) throw new Malformed('the message ended before its last field')
    return count
  }

  text(): string {
    const bytes = this.take(this.count())
    try {
      return text.decode(bytes)
    } catch {
      throw new Malformed('a string was not UTF-8')
    }
  }

  position(): Position {
    return { line: this.number(), column: this.number() }
  }

  span(): Span {
    return { start: this.position(), end: this.position() }
  }

  /** A command: a byte saying which of its fields it has, then those fields in order */
  command(): Command {
    const id = this.number()
    const has = this.byte()
    const text = (bit: number) => (has & bit ? this.text() : null)
    const number = (bit: number) => (has & bit ? this.number() : null)
    const command = text(1)
    const cwd = text(2)
    const startedMs = number(4)
    const finishedMs = number(8)
    let exitStatus: number | null = null
    if (has & 16) {
      // Zigzag, as the broker writes it: the low bit is the sign
      const zigzag = this.number()
      if (zigzag > 0xffffffff) throw new Malformed('an integer did not fit its field')
      exitStatus = zigzag % 2 === 0 ? zigzag / 2 : -(zigzag + 1) / 2
    }
    return { id, command, cwd, startedMs, finishedMs, exitStatus, prompt: number(32), output: number(64), end: number(128) }
  }

  color(): Color {
    switch (this.byte()) {
      case 0:
        return null
      case 1:
        return paletteColor(this.byte())
      case 2: {
        const [red, green, blue] = this.take(3)
        return (red! << 16) | (green! << 8) | blue!
      }
      default:
        throw new Malformed("a colour's tag is outside what the format defines")
    }
  }

  rows(): Rows {
    const styles: Style[] = [PLAIN]
    for (let more = this.count(); more > 0; more -= 1) {
      styles.push({ fg: this.color(), bg: this.color(), underlineColor: this.color(), flags: this.number() })
    }
    const links: string[] = ['']
    for (let more = this.count(); more > 0; more -= 1) links.push(this.text())
    const rows: Row[] = []
    for (let more = this.count(); more > 0; more -= 1) rows.push(this.row(styles.length, links.length))
    return { styles, links, rows }
  }

  /**
   * A row, which arrives as runs: characters side by side that share a style and a link. A run of plain
   * characters is one string, each code point a cell; any other says each character's width.
   */
  private row(styles: number, links: number): Row {
    const line = this.number()
    const flags = this.byte()
    if (flags > 3) throw new Malformed("a row's flags are outside what the format defines")
    const characters: string[] = []
    const columns: number[] = []
    const widths: number[] = []
    const style: number[] = []
    const link: number[] = []
    for (let runs = this.count(); runs > 0; runs -= 1) {
      let column = this.number()
      const runStyle = this.number()
      const runLink = this.number()
      if (runStyle >= styles) throw new Malformed("a style's index is outside its block")
      if (runLink >= links) throw new Malformed("a link's index is outside its block")
      const push = (character: string, width: number) => {
        characters.push(character)
        columns.push(column)
        widths.push(width)
        style.push(runStyle)
        link.push(runLink)
        column += width
      }
      switch (this.byte()) {
        case 0:
          for (const character of this.text()) push(character, 1)
          break
        case 1:
          for (let more = this.count(); more > 0; more -= 1) {
            const width = this.byte()
            push(this.text(), width)
          }
          break
        default:
          throw new Malformed("a run's kind is outside what the format defines")
      }
      if (column > 0xffff) throw new Malformed('an integer did not fit its field')
    }
    return {
      line,
      wrapped: (flags & 1) !== 0,
      prompt: (flags & 2) !== 0,
      text: characters,
      column: Uint16Array.from(columns),
      width: Uint8Array.from(widths),
      style: Uint16Array.from(style),
      link: Uint16Array.from(link),
    }
  }

  finish<T>(value: T): T {
    if (this.at !== this.bytes.length) throw new Malformed("bytes followed the message's last field")
    return value
  }
}

/** A message for a view, from its kind and payload: what `Messages` hands back */
export function decodeToView(body: Uint8Array): ToView {
  const read = new Reader(body)
  const kind = read.byte()
  const channel = read.number()
  switch (kind) {
    case FRAME: {
      const sequence = read.number()
      const epoch = read.number()
      const firstActive = read.number()
      const columns = read.number()
      const screenRows = read.number()
      const scrollback = read.number()
      const commands = read.number()
      const images = read.number()
      const flags = read.byte()
      if (flags >= 32) throw new Malformed("a frame's flags are outside what the format defines")
      let cursor: Cursor | null = null
      if (flags & FRAME_CURSOR) {
        const column = read.number()
        const row = read.number()
        const shape = CURSOR_SHAPES[read.byte()]
        if (shape === undefined) throw new Malformed("a cursor's shape is outside what the format defines")
        cursor = { column, row, shape, blinking: (flags & FRAME_BLINKING) !== 0 }
      }
      const staleFrom = flags & FRAME_STALE ? read.number() : undefined
      const frame: Frame = {
        sequence, epoch, firstActive, columns, screenRows, scrollback, commands, images, cursor,
        modes: { alternateScreen: (flags & FRAME_ALTERNATE) !== 0, mouse: (flags & FRAME_MOUSE) !== 0 },
        ...(staleFrom === undefined ? {} : { staleFrom }),
        ...read.rows(),
      }
      return read.finish({ kind: 'frame', channel, frame })
    }
    case ROWS: {
      const request = read.number()
      const epoch = read.number()
      return read.finish({ kind: 'rows', channel, request, epoch, rows: read.rows() })
    }
    case ENDED:
      return read.finish({ kind: 'ended', channel })
    case PASTED: {
      const request = read.number()
      const outcome = (['pasted', 'unsafe'] as const)[read.byte()]
      if (outcome === undefined) throw new Malformed("a paste's outcome is outside what the format defines")
      return read.finish({ kind: 'pasted', channel, request, outcome })
    }
    case SPAN: {
      const request = read.number()
      const epoch = read.number()
      return read.finish({ kind: 'span', channel, request, epoch, span: read.flag() ? read.span() : null })
    }
    case TEXT: {
      const request = read.number()
      return read.finish({ kind: 'text', channel, request, text: read.text() })
    }
    case SPANS: {
      const request = read.number()
      const epoch = read.number()
      const spans: Span[] = []
      for (let more = read.count(); more > 0; more -= 1) spans.push(read.span())
      return read.finish({ kind: 'spans', channel, request, epoch, spans })
    }
    case FAILED: {
      const request = read.number()
      return read.finish({ kind: 'failed', channel, request, message: read.text() })
    }
    case COMMANDS: {
      const request = read.number()
      const epoch = read.number()
      const commands: Command[] = []
      for (let more = read.count(); more > 0; more -= 1) commands.push(read.command())
      return read.finish({ kind: 'commands', channel, request, epoch, commands })
    }
    case IMAGES: {
      const request = read.number()
      const epoch = read.number()
      const cellWidth = read.number()
      const cellHeight = read.number()
      const placements: Placement[] = []
      for (let more = read.count(); more > 0; more -= 1) {
        const image = read.number()
        const generation = read.number()
        const zigzag = read.number()
        if (zigzag > 0xffffffff) throw new Malformed('an integer did not fit its field')
        placements.push({
          image, generation, z: zigzag % 2 === 0 ? zigzag / 2 : -(zigzag + 1) / 2,
          line: read.number(), column: read.number(), columns: read.number(), rows: read.number(),
          offsetX: read.number(), offsetY: read.number(), width: read.number(), height: read.number(),
          sourceX: read.number(), sourceY: read.number(), sourceWidth: read.number(), sourceHeight: read.number(),
        })
      }
      return read.finish({ kind: 'images', channel, request, epoch, cellWidth, cellHeight, placements })
    }
    case IMAGE: {
      const request = read.number()
      const image = read.number()
      const generation = read.number()
      const width = read.number()
      const height = read.number()
      const channels = read.byte()
      if (channels < 1 || channels > 4) throw new Malformed("an image's channels are outside what the format defines")
      const total = read.number()
      const offset = read.number()
      // Copied, because the bytes it was read from are the stream's and are reused
      const bytes = read.take(read.count()).slice()
      return read.finish({ kind: 'image', channel, request, image, generation, width, height, channels, total, offset, bytes })
    }
    default:
      throw new Malformed(`no message has kind ${kind}`)
  }
}

/**
 * Finds messages in a stream of bytes that arrives in pieces of any size.
 *
 * A socket delivers what it has: half a length, three messages and the start of a fourth. Each piece is
 * pushed as it comes and every message that is now whole is handed back as its kind and payload.
 */
export class Messages {
  private held = new Uint8Array(0)

  push(chunk: Uint8Array): Uint8Array[] {
    let bytes = chunk
    if (this.held.length > 0) {
      bytes = new Uint8Array(this.held.length + chunk.length)
      bytes.set(this.held)
      bytes.set(chunk, this.held.length)
    }
    const whole: Uint8Array[] = []
    let at = 0
    while (bytes.length - at >= 4) {
      const length = new DataView(bytes.buffer, bytes.byteOffset + at, 4).getUint32(0, true)
      if (length > MAX_MESSAGE_BYTES) throw new Malformed('a message was longer than a message may be')
      if (bytes.length - at - 4 < length) break
      whole.push(bytes.subarray(at + 4, at + 4 + length))
      at += 4 + length
    }
    // What is left is copied, so the next push does not keep this chunk's whole buffer alive for a tail
    this.held = bytes.slice(at)
    return whole
  }
}
