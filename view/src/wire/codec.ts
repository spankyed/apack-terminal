/**
 * Total, bounds-checked binary wire reader and writer matching `crates/apack-proto/src/codec.rs`
 * byte-for-byte. Zero runtime dependencies, strict DOM-compatible ES2022.
 */
import { MAX_BLOB_BYTES, MAX_STRING_BYTES, MAX_VEC_ITEMS } from './constants.ts'

export {
  BULK_CHUNK_BYTES,
  DEFAULT_SSH_GRACE_MS,
  FIN_FRAME,
  FRAME_HEADER_LEN,
  INNER_HEADER_LEN,
  MAX_BLOB_BYTES,
  MAX_BODY_LEN,
  MAX_COMMANDS,
  MAX_FRAME_PAYLOAD,
  MAX_GRID_COLS,
  MAX_GRID_ROWS,
  MAX_LEASE_GRACE_MS,
  MAX_PACKED_ROW_TEXT_BYTES,
  MAX_ROWS_PER_FETCH,
  MAX_SEARCH_MATCHES,
  MAX_SEARCH_NEEDLE_BYTES,
  MAX_STATUSES,
  MAX_STRING_BYTES,
  MAX_VEC_ITEMS,
  PROTOCOL_VERSION,
} from './constants.ts'

export type WireErrorKind =
  | 'UnexpectedEof'
  | 'TrailingBytes'
  | 'FrameTooLarge'
  | 'InvalidTag'
  | 'InvalidUtf8'
  | 'VarintOverflow'
  | 'LimitExceeded'
  | 'InvalidValue'

export class WireError extends Error {
  readonly kind: WireErrorKind
  constructor(kind: WireErrorKind, message: string) {
    super(message)
    this.name = 'WireError'
    this.kind = kind
  }
}

const textEncoder = new TextEncoder()
const textDecoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true })

export class WireWriter {
  private buf: Uint8Array
  private view: DataView
  private len = 0

  constructor(initialCapacity = 128) {
    this.buf = new Uint8Array(Math.max(16, initialCapacity))
    this.view = new DataView(this.buf.buffer)
  }

  get length(): number {
    return this.len
  }

  /** Executes `fn` atomically: if it throws, rewinds `this.len` so no partial bytes remain. */
  atomic(fn: (w: WireWriter) => void): void {
    const start = this.len
    try {
      fn(this)
    } catch (err) {
      this.len = start
      throw err
    }
  }

  private ensure(extra: number): void {
    const needed = this.len + extra
    if (needed <= this.buf.length) return
    let nextCap = this.buf.length * 2
    while (nextCap < needed) nextCap *= 2
    const next = new Uint8Array(nextCap)
    next.set(this.buf.subarray(0, this.len))
    this.buf = next
    this.view = new DataView(next.buffer)
  }

  toBytes(): Uint8Array {
    return this.buf.slice(0, this.len)
  }

  u8(value: number): void {
    if (!Number.isInteger(value) || value < 0 || value > 0xff) {
      throw new WireError('InvalidValue', `u8 out of range: ${value}`)
    }
    this.ensure(1)
    this.buf[this.len++] = value
  }

  bool(value: boolean): void {
    this.ensure(1)
    this.buf[this.len++] = value ? 1 : 0
  }

  u16(value: number): void {
    if (!Number.isInteger(value) || value < 0 || value > 0xffff) {
      throw new WireError('InvalidValue', `u16 out of range: ${value}`)
    }
    this.ensure(2)
    this.view.setUint16(this.len, value, true)
    this.len += 2
  }

  u32(value: number): void {
    if (!Number.isInteger(value) || value < 0 || value > 0xffff_ffff) {
      throw new WireError('InvalidValue', `u32 out of range: ${value}`)
    }
    this.ensure(4)
    this.view.setUint32(this.len, value, true)
    this.len += 4
  }

  i32(value: number): void {
    if (!Number.isInteger(value) || value < -0x8000_0000 || value > 0x7fff_ffff) {
      throw new WireError('InvalidValue', `i32 out of range: ${value}`)
    }
    this.ensure(4)
    this.view.setInt32(this.len, value, true)
    this.len += 4
  }

  varU32(value: number): void {
    if (!Number.isInteger(value) || value < 0 || value > 0xffff_ffff) {
      throw new WireError('InvalidValue', `varU32 out of range: ${value}`)
    }
    let v = value >>> 0
    this.ensure(5)
    while (true) {
      const byte = v & 0x7f
      v >>>= 7
      if (v === 0) {
        this.buf[this.len++] = byte
        return
      }
      this.buf[this.len++] = byte | 0x80
    }
  }

  varU64(value: number | bigint): void {
    let v: bigint
    if (typeof value === 'bigint') {
      if (value < 0n || value > 0xffff_ffff_ffff_ffffn) {
        throw new WireError('InvalidValue', `varU64 out of range: ${value}`)
      }
      v = value
    } else {
      if (!Number.isInteger(value) || value < 0 || !Number.isSafeInteger(value)) {
        throw new WireError('InvalidValue', `varU64 out of safe range: ${value}`)
      }
      v = BigInt(value)
    }
    this.ensure(10)
    while (true) {
      const byte = Number(v & 0x7fn)
      v >>= 7n
      if (v === 0n) {
        this.buf[this.len++] = byte
        return
      }
      this.buf[this.len++] = byte | 0x80
    }
  }

  raw(bytes: Uint8Array): void {
    this.ensure(bytes.length)
    this.buf.set(bytes, this.len)
    this.len += bytes.length
  }

  bytes(bytes: Uint8Array): void {
    if (bytes.length > MAX_BLOB_BYTES) {
      throw new WireError('LimitExceeded', 'wire limit exceeded: blob bytes')
    }
    this.varU32(bytes.length)
    this.raw(bytes)
  }

  str(text: string): void {
    this.strBounded(text, MAX_STRING_BYTES, 'string bytes')
  }

  strBounded(text: string, maxLen: number, label: string): void {
    if (!Number.isFinite(maxLen) || maxLen < 0) {
      throw new WireError('InvalidValue', `invalid string max length: ${maxLen}`)
    }
    const encoded = textEncoder.encode(text)
    const cap = Math.min(maxLen, MAX_STRING_BYTES)
    if (!(encoded.length <= cap)) {
      throw new WireError('LimitExceeded', `wire limit exceeded: ${label}`)
    }
    this.bytes(encoded)
  }

  opt<T>(value: T | null | undefined, encode: (w: WireWriter, inner: T) => void): void {
    if (value === null || value === undefined) {
      this.u8(0)
    } else {
      this.u8(1)
      encode(this, value)
    }
  }

  slice<T>(items: readonly T[], encode: (w: WireWriter, item: T) => void): void {
    this.varU32(items.length)
    for (let i = 0; i < items.length; i += 1) {
      encode(this, items[i]!)
    }
  }
}

export class WireReader {
  private readonly view: DataView
  private offset = 0

  constructor(private readonly bytesBuf: Uint8Array) {
    this.view = new DataView(bytesBuf.buffer, bytesBuf.byteOffset, bytesBuf.byteLength)
  }

  get remaining(): number {
    return this.bytesBuf.length - this.offset
  }

  finish(): void {
    if (this.offset !== this.bytesBuf.length) {
      throw new WireError('TrailingBytes', `${this.bytesBuf.length - this.offset} trailing wire bytes after message`)
    }
  }

  take(len: number): Uint8Array {
    if (len < 0 || this.offset + len > this.bytesBuf.length) {
      throw new WireError('UnexpectedEof', 'unexpected end of wire input')
    }
    const slice = this.bytesBuf.subarray(this.offset, this.offset + len)
    this.offset += len
    return slice
  }

  fixedCopy(len: number): Uint8Array {
    return this.take(len).slice()
  }

  u8(): number {
    if (this.offset >= this.bytesBuf.length) {
      throw new WireError('UnexpectedEof', 'unexpected end of wire input')
    }
    return this.bytesBuf[this.offset++]!
  }

  bool(): boolean {
    const b = this.u8()
    if (b === 0) return false
    if (b === 1) return true
    throw new WireError('InvalidTag', `invalid bool wire tag ${b}`)
  }

  u16(): number {
    if (this.offset + 2 > this.bytesBuf.length) {
      throw new WireError('UnexpectedEof', 'unexpected end of wire input')
    }
    const v = this.view.getUint16(this.offset, true)
    this.offset += 2
    return v
  }

  u32(): number {
    if (this.offset + 4 > this.bytesBuf.length) {
      throw new WireError('UnexpectedEof', 'unexpected end of wire input')
    }
    const v = this.view.getUint32(this.offset, true)
    this.offset += 4
    return v
  }

  i32(): number {
    if (this.offset + 4 > this.bytesBuf.length) {
      throw new WireError('UnexpectedEof', 'unexpected end of wire input')
    }
    const v = this.view.getInt32(this.offset, true)
    this.offset += 4
    return v
  }

  varU32(): number {
    let acc = 0
    for (let i = 0; i < 5; i += 1) {
      const byte = this.u8()
      if (i === 4 && byte > 0x0f) {
        throw new WireError('VarintOverflow', 'LEB128 u32 varint overflow')
      }
      if (i > 0 && byte === 0) {
        throw new WireError('InvalidValue', 'non-canonical LEB128 u32 varint')
      }
      acc = (acc + ((byte & 0x7f) * 2 ** (i * 7))) >>> 0
      if ((byte & 0x80) === 0) return acc
    }
    throw new WireError('VarintOverflow', 'LEB128 u32 varint overflow')
  }

  varU64Big(): bigint {
    let acc = 0n
    for (let i = 0; i < 10; i += 1) {
      const byte = this.u8()
      if (i === 9 && byte > 0x01) {
        throw new WireError('VarintOverflow', 'LEB128 u64 varint overflow')
      }
      if (i > 0 && byte === 0) {
        throw new WireError('InvalidValue', 'non-canonical LEB128 u64 varint')
      }
      acc |= BigInt(byte & 0x7f) << BigInt(i * 7)
      if ((byte & 0x80) === 0) return acc
    }
    throw new WireError('VarintOverflow', 'LEB128 u64 varint overflow')
  }

  /** Reads a `var_u64` into a JS safe integer (`<= Number.MAX_SAFE_INTEGER`). */
  varU64(): number {
    const v = this.varU64Big()
    if (v > BigInt(Number.MAX_SAFE_INTEGER)) {
      throw new WireError('LimitExceeded', 'u64 exceeds JS safe integer range')
    }
    return Number(v)
  }

  bytesBounded(maxLen: number, label: string): Uint8Array {
    if (!Number.isFinite(maxLen) || maxLen < 0) {
      throw new WireError('InvalidValue', `invalid bytes max length: ${maxLen}`)
    }
    const cap = Math.min(maxLen, MAX_BLOB_BYTES)
    const len = this.varU32()
    if (!(len <= cap)) {
      throw new WireError('LimitExceeded', `wire limit exceeded: ${label}`)
    }
    return this.take(len)
  }

  bytes(): Uint8Array {
    return this.bytesBounded(MAX_BLOB_BYTES, 'blob bytes').slice()
  }

  strBytes(): Uint8Array {
    return this.bytesBounded(MAX_STRING_BYTES, 'string bytes')
  }

  str(): string {
    return this.strBounded(MAX_STRING_BYTES, 'string bytes')
  }

  strBounded(maxLen: number, label: string): string {
    if (!Number.isFinite(maxLen) || maxLen < 0) {
      throw new WireError('InvalidValue', `invalid string max length: ${maxLen}`)
    }
    const raw = this.bytesBounded(Math.min(maxLen, MAX_STRING_BYTES), label)
    try {
      return textDecoder.decode(raw)
    } catch {
      throw new WireError('InvalidUtf8', 'invalid UTF-8 in wire string')
    }
  }

  opt<T>(decode: (r: WireReader) => T): T | null {
    const present = this.bool()
    return present ? decode(this) : null
  }

  vecBounded<T>(maxItems: number, label: string, decode: (r: WireReader) => T): T[] {
    const count = this.varU32()
    if (count > maxItems || count > MAX_VEC_ITEMS) {
      throw new WireError('LimitExceeded', `wire limit exceeded: ${label}`)
    }
    const out: T[] = []
    for (let i = 0; i < count; i += 1) {
      out.push(decode(this))
    }
    return out
  }
}

export function decodeUtf8Strict(bytes: Uint8Array): string {
  try {
    return textDecoder.decode(bytes)
  } catch {
    throw new WireError('InvalidUtf8', 'invalid UTF-8 in wire string')
  }
}

export function encodeUtf8(text: string): Uint8Array {
  return textEncoder.encode(text)
}
