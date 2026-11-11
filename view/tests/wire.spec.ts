// The view protocol's bytes, read against what the broker's own encoder wrote.
//
// `node/view-vectors.json` is written by the broker's suite (`tests/view_vectors.rs`): each kind of message
// as the bytes its Rust encoder produces and as the value they mean. It is read where it is written, so
// there is one file and no copy to fall behind. This is the only place the two halves of the protocol
// meet, so every vector is used: one for a view is decoded and compared, one for the broker is encoded
// and compared.
import { describe, expect, it } from 'vitest'
import recorded from '../../node/view-vectors.json'
import { decodeToBroker, encodeToView } from './_broker.ts'
import { MAX_MESSAGE_BYTES, Malformed, Messages, decodeToView, encodeToBroker, type ToBroker, type ToView } from '../src/broker/wire.ts'
import { TooLarge, paletteIndex, type Color, type Rows } from '../src/source.ts'

interface Vector {
  readonly name: string
  readonly to: 'view' | 'broker'
  readonly hex: string
  readonly message: Record<string, unknown>
}

const vectors = recorded as Vector[]

const bytesOf = (hex: string) => Uint8Array.from(hex.match(/../g) ?? [], (pair) => Number.parseInt(pair, 16))
const hexOf = (bytes: Uint8Array) => Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')

/** A colour as the vectors write one */
function color(value: Color): unknown {
  if (value === null) return null
  const index = paletteIndex(value)
  return index === null ? { rgb: value } : { palette: index }
}

/** Rows as the vectors write them: a list of characters rather than arrays side by side */
function rows(block: Rows): Record<string, unknown> {
  return {
    rows: block.rows.map((row) => ({
      line: row.line,
      wrapped: row.wrapped,
      prompt: row.prompt,
      // Without the plain style and the empty link a view puts in front, which the bytes do not carry
      styles: row.styles.slice(1).map((style) => ({ fg: color(style.fg), bg: color(style.bg), underline: color(style.underlineColor), flags: style.flags })),
      links: row.links.slice(1),
      glyphs: row.text.map((text, index) => ({
        column: row.column[index], width: row.width[index], text, style: row.style[index], link: row.link[index],
      })),
    })),
  }
}

/** A decoded message as the vectors write it */
function written(message: ToView): Record<string, unknown> {
  switch (message.kind) {
    case 'frame': {
      const { frame } = message
      return {
        kind: 'frame', channel: message.channel, sequence: frame.sequence, epoch: frame.epoch,
        firstActive: frame.firstActive, columns: frame.columns, screenRows: frame.screenRows,
        scrollback: frame.scrollback, commands: frame.commands, images: frame.images, cursor: frame.cursor, alternateScreen: frame.modes.alternateScreen,
        mouse: frame.modes.mouse, staleFrom: frame.staleFrom ?? null, reverseVideo: frame.reverseVideo, colors: frame.colors ?? null, ...rows(frame),
      }
    }
    case 'rows':
      return { kind: 'rows', channel: message.channel, request: message.request, epoch: message.epoch, first: message.first, count: message.count, ...rows(message.rows) }
    case 'image':
      return { ...message, bytes: Array.from(message.bytes) }
    default:
      return { ...message }
  }
}

describe('the bytes a view and a broker exchange', () => {
  it('covers every kind of message the protocol has', () => {
    const kinds = new Set(vectors.map((vector) => `${vector.to}:${String(vector.message.kind)}`))
    expect(kinds.size).toBe(27)
  })

  for (const vector of vectors.filter((vector) => vector.to === 'view')) {
    it(`reads what the broker sends: ${vector.name}`, () => {
      const [body, ...rest] = new Messages().push(bytesOf(vector.hex))
      expect(rest).toEqual([])
      expect(written(decodeToView(body!))).toEqual(vector.message)
    })
  }

  for (const vector of vectors.filter((vector) => vector.to === 'broker')) {
    it(`writes what the broker reads: ${vector.name}`, () => {
      expect(hexOf(encodeToBroker(vector.message as unknown as ToBroker))).toBe(vector.hex)
    })
  }
})

describe('the fake broker the rest of this suite is driven by', () => {
  it('writes the bytes the real one does', () => {
    for (const vector of vectors.filter((vector) => vector.to === 'view')) {
      const [body] = new Messages().push(bytesOf(vector.hex))
      expect(hexOf(encodeToView(decodeToView(body!))), vector.name).toBe(vector.hex)
    }
  })

  it('reads what the real one would', () => {
    for (const vector of vectors.filter((vector) => vector.to === 'broker')) {
      expect(decodeToBroker(bytesOf(vector.hex).subarray(4)), vector.name).toEqual(vector.message)
    }
  })
})

describe('messages in a stream', () => {
  const stream = Uint8Array.from(vectors.filter((vector) => vector.to === 'view').flatMap((vector) => [...bytesOf(vector.hex)]))
  const expected = vectors.filter((vector) => vector.to === 'view').map((vector) => vector.message)

  it('are found however the stream is cut', () => {
    for (const size of [1, 2, 3, 5, 64, 1000, stream.length]) {
      const messages = new Messages()
      const found: unknown[] = []
      for (let at = 0; at < stream.length; at += size) {
        for (const body of messages.push(stream.subarray(at, at + size))) found.push(written(decodeToView(body)))
      }
      expect(found, `in pieces of ${size}`).toEqual(expected)
    }
  })

  it('does not hold on to a piece it has finished with', () => {
    const messages = new Messages()
    const whole = bytesOf(vectors[0]!.hex)
    const [body] = messages.push(whole)
    // The message is a view of the piece it came in; nothing more is copied or kept
    expect(body!.buffer).toBe(whole.buffer)
    expect(messages.push(new Uint8Array(0))).toEqual([])
  })

  it('are whole however many pieces one arrived in, the length included', () => {
    const body = Uint8Array.from({ length: 5000 }, (_, index) => index % 251)
    const sent = new Uint8Array(4 + body.length + 4 + 3)
    new DataView(sent.buffer).setUint32(0, body.length, true)
    sent.set(body, 4)
    new DataView(sent.buffer).setUint32(4 + body.length, 3, true)
    sent.set([7, 8, 9], 8 + body.length)

    for (const size of [1, 3, 7, 4096]) {
      const messages = new Messages()
      const found: Uint8Array[] = []
      for (let at = 0; at < sent.length; at += size) found.push(...messages.push(sent.subarray(at, at + size)))
      expect(found.map((one) => [...one]), `in pieces of ${size}`).toEqual([[...body], [7, 8, 9]])
    }
  })

  it('include one with nothing in it', () => {
    expect(new Messages().push(Uint8Array.of(0, 0, 0, 0, 1, 0, 0, 0, 9)).map((one) => [...one])).toEqual([[], [9]])
  })

  it('refuses a message longer than a message may be', () => {
    const length = new Uint8Array(4)
    new DataView(length.buffer).setUint32(0, (8 << 20) + 1, true)
    expect(() => new Messages().push(length)).toThrow(Malformed)
  })
})

describe('bytes that are not a message', () => {
  const frame = bytesOf(vectors[0]!.hex).subarray(4)

  it('are refused when a message is cut short, wherever it is cut', () => {
    for (let length = 0; length < frame.length; length += 1) {
      expect(() => decodeToView(frame.subarray(0, length)), `cut to ${length}`).toThrow(Malformed)
    }
  })

  it('are refused when anything follows the last field', () => {
    const longer = new Uint8Array(frame.length + 1)
    longer.set(frame)
    expect(() => decodeToView(longer)).toThrow(Malformed)
  })

  it('are refused, not trusted, with any one byte changed', () => {
    for (let at = 0; at < frame.length; at += 1) {
      const changed = frame.slice()
      changed[at] = changed[at]! ^ 0xff
      try {
        decodeToView(changed)
      } catch (error) {
        expect(error, `byte ${at}`).toBeInstanceOf(Malformed)
      }
    }
  })

  it('names a kind no message has', () => {
    expect(() => decodeToView(Uint8Array.of(200, 0))).toThrow('no message has kind 200')
  })

  it('refuses a count the payload cannot hold before making anything for it', () => {
    // A `rows` answer claiming two hundred million styles in four bytes
    expect(() => decodeToView(Uint8Array.of(66, 0, 0, 0, 0xff, 0xff, 0xff, 0x5f))).toThrow(Malformed)
  })
})

describe('a string', () => {
  it('keeps a byte order mark a program wrote at its start', () => {
    const [body] = new Messages().push(encodeToView({ kind: 'text', channel: 1, request: 1, epoch: 1, text: '\ufeffsigned' }))
    expect(decodeToView(body!)).toMatchObject({ text: '\ufeffsigned' })
  })
})

describe('what a view cannot say', () => {
  it('is refused where it is written when it is longer than a message may be, which the broker would end the stream over', () => {
    const paste = (text: string): ToBroker => ({ kind: 'paste', channel: 1, request: 1, allowUnsafe: false, text })
    expect(() => encodeToBroker(paste('x'.repeat(MAX_MESSAGE_BYTES + 1)))).toThrow(TooLarge)
    // Under the bound as characters and over it as bytes
    expect(() => encodeToBroker(paste('日'.repeat(MAX_MESSAGE_BYTES / 3)))).toThrow(TooLarge)
    expect(encodeToBroker(paste('x'.repeat(1000))).length).toBeLessThan(1100)
  })

  it('is refused where it is written', () => {
    expect(() => encodeToBroker({ kind: 'acknowledge', channel: 1, sequence: -1 })).toThrow(RangeError)
    expect(() => encodeToBroker({ kind: 'acknowledge', channel: 1, sequence: 2 ** 60 })).toThrow(RangeError)
    expect(() => encodeToBroker({ kind: 'acknowledge', channel: 1.5, sequence: 1 })).toThrow(RangeError)
  })
})
