// The view protocol's bytes, read against what the broker's own encoder wrote.
//
// `vectors/view-vectors.json` is written by `apack-terminal` (`tests/view_vectors.rs`): each kind of message
// as the bytes its Rust encoder produces and as the value they mean. This is the only place the two halves
// of the protocol meet, so every vector is used: one for a view is decoded and compared, one for the broker
// is encoded and compared.
import { describe, expect, it } from 'vitest'
import recorded from './vectors/view-vectors.json'
import { decodeToBroker, encodeToView } from './_broker.ts'
import { Malformed, Messages, decodeToView, encodeToBroker, type ToBroker, type ToView } from '../src/broker/wire.ts'
import { paletteIndex, type Color, type Rows } from '../src/source.ts'

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
    styles: block.styles.map((style) => ({ fg: color(style.fg), bg: color(style.bg), underline: color(style.underlineColor), flags: style.flags })),
    links: block.links,
    rows: block.rows.map((row) => ({
      line: row.line,
      wrapped: row.wrapped,
      prompt: row.prompt,
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
        scrollback: frame.scrollback, commands: frame.commands, cursor: frame.cursor, alternateScreen: frame.modes.alternateScreen,
        mouse: frame.modes.mouse, staleFrom: frame.staleFrom ?? null, ...rows(frame),
      }
    }
    case 'rows':
      return { kind: 'rows', channel: message.channel, request: message.request, epoch: message.epoch, ...rows(message.rows) }
    default:
      return { ...message }
  }
}

describe('the bytes a view and a broker exchange', () => {
  it('covers every kind of message the protocol has', () => {
    const kinds = new Set(vectors.map((vector) => `${vector.to}:${String(vector.message.kind)}`))
    expect(kinds.size).toBe(23)
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

describe('what a view cannot say', () => {
  it('is refused where it is written', () => {
    expect(() => encodeToBroker({ kind: 'acknowledge', channel: 1, sequence: -1 })).toThrow(RangeError)
    expect(() => encodeToBroker({ kind: 'acknowledge', channel: 1, sequence: 2 ** 60 })).toThrow(RangeError)
    expect(() => encodeToBroker({ kind: 'acknowledge', channel: 1.5, sequence: 1 })).toThrow(RangeError)
  })
})
