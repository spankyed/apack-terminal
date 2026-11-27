import { describe, expect, it } from 'vitest'
import goldenRaw from '../../node/wire-vectors-v2.json'
import {
  FIN_FRAME,
  FrameDecoder,
  MsgType,
  WireError,
  WireReader,
  WireWriter,
  computeClientProof,
  computeServerProof,
  decodeBrokerMsg,
  decodeClientMsg,
  decodePackedRow,
  encodeBrokerFrame,
  encodeClientFrame,
  verifyServerProof,
} from '../src/index.ts'
import {
  SubFlags,
  decodeMarksKey,
  decodeSubFlags,
  encodeGridSize,
  encodeLeaseKind,
  encodeSubFlags,
} from '../src/wire/types.ts'

interface GoldenFrame {
  readonly name: string
  readonly streamId: number
  readonly msgType: number
  readonly reqId: number
  readonly hex: string
}

interface GoldenFile {
  readonly protocolVersion: number
  readonly finFrameHex: string
  readonly auth: {
    readonly secretHex: string
    readonly clientNonceHex: string
    readonly serverNonceHex: string
    readonly clientHmacHex: string
    readonly serverHmacHex: string
  }
  readonly client: readonly GoldenFrame[]
  readonly broker: readonly GoldenFrame[]
  readonly malformed: ReadonlyArray<{
    readonly name: string
    readonly direction: 'frame' | 'client' | 'broker'
    readonly hex: string
    readonly reason: string
  }>
}

function fromHex(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) throw new Error(`odd hex length: ${hex.length}`)
  const out = new Uint8Array(hex.length / 2)
  for (let i = 0; i < out.length; i += 1) {
    out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16)
  }
  return out
}

function toHex(bytes: Uint8Array): string {
  let out = ''
  for (let i = 0; i < bytes.length; i += 1) {
    out += bytes[i]!.toString(16).padStart(2, '0')
  }
  return out
}

const golden = goldenRaw as GoldenFile

describe('v2 golden wire vectors (Rust <-> TypeScript parity)', () => {
  it('matches FIN_FRAME and mutual HMAC-SHA256 golden vector byte-for-byte', () => {
    expect(toHex(FIN_FRAME)).toBe(golden.finFrameHex)
    const secret = fromHex(golden.auth.secretHex)
    const clientNonce = fromHex(golden.auth.clientNonceHex)
    const serverNonce = fromHex(golden.auth.serverNonceHex)
    const clientProof = computeClientProof(secret, clientNonce, serverNonce)
    const serverProof = computeServerProof(secret, serverNonce, clientNonce)
    expect(toHex(clientProof)).toBe(golden.auth.clientHmacHex)
    expect(toHex(serverProof)).toBe(golden.auth.serverHmacHex)
    expect(verifyServerProof(secret, serverNonce, clientNonce, serverProof)).toBe(true)
    const tampered = new Uint8Array(serverProof)
    tampered[0] = (tampered[0]! ^ 0x01) & 0xff
    expect(verifyServerProof(secret, serverNonce, clientNonce, tampered)).toBe(false)
  })

  it('decodes and re-encodes every ClientMsg golden vector byte-for-byte', () => {
    expect(golden.client.length).toBeGreaterThanOrEqual(40)
    for (const vec of golden.client) {
      const wire = fromHex(vec.hex)
      const decoder = new FrameDecoder()
      const frames = decoder.push(wire)
      expect(frames.length, vec.name).toBe(1)
      const f = frames[0]!
      expect(f.kind, vec.name).toBe('frame')
      if (f.kind !== 'frame') continue
      expect(f.header.streamId, vec.name).toBe(vec.streamId)
      expect(f.header.reqId, vec.name).toBe(vec.reqId)
      expect(f.header.msgType, vec.name).toBe(vec.msgType)

      const decoded = decodeClientMsg(f.header.msgType, f.payload)
      const reencoded = encodeClientFrame(vec.streamId, vec.reqId, decoded)
      expect(toHex(reencoded), `round-trip mismatch for ${vec.name}`).toBe(vec.hex)
    }
  })

  it('decodes and re-encodes every BrokerMsg golden vector byte-for-byte', () => {
    expect(golden.broker.length).toBeGreaterThanOrEqual(35)
    for (const vec of golden.broker) {
      const wire = fromHex(vec.hex)
      const decoder = new FrameDecoder()
      const frames = decoder.push(wire)
      expect(frames.length, vec.name).toBe(1)
      const f = frames[0]!
      expect(f.kind, vec.name).toBe('frame')
      if (f.kind !== 'frame') continue
      expect(f.header.streamId, vec.name).toBe(vec.streamId)
      expect(f.header.reqId, vec.name).toBe(vec.reqId)
      expect(f.header.msgType, vec.name).toBe(vec.msgType)

      const decoded = decodeBrokerMsg(f.header.msgType, f.payload)
      const reencoded = encodeBrokerFrame(vec.streamId, vec.reqId, decoded)
      expect(toHex(reencoded), `round-trip mismatch for ${vec.name}`).toBe(vec.hex)
    }
  })

  it('rejects every malformed frame/payload vector with WireError', () => {
    expect(golden.malformed.length).toBeGreaterThanOrEqual(10)
    for (const vec of golden.malformed) {
      const bytes = fromHex(vec.hex)
      if (vec.direction === 'frame') {
        expect(() => {
          const dec = new FrameDecoder()
          dec.push(bytes)
        }, vec.name).toThrow(WireError)
      } else {
        expect(() => {
          const dec = new FrameDecoder()
          const frames = dec.push(bytes)
          for (const f of frames) {
            if (f.kind !== 'frame') continue
            if (vec.direction === 'client') decodeClientMsg(f.header.msgType, f.payload)
            else decodeBrokerMsg(f.header.msgType, f.payload)
          }
        }, vec.name).toThrow(WireError)
      }
    }
  })

  it('is chunk-boundary invariant across 1-byte and pseudo-random multi-chunk splits', () => {
    const chunks: Uint8Array[] = []
    for (const v of golden.client) chunks.push(fromHex(v.hex))
    for (const v of golden.broker) chunks.push(fromHex(v.hex))
    chunks.push(FIN_FRAME)

    const totalLen = chunks.reduce((acc, c) => acc + c.length, 0)
    const stream = new Uint8Array(totalLen)
    let off = 0
    for (const c of chunks) {
      stream.set(c, off)
      off += c.length
    }

    // 1. 1-byte split
    const oneByte = new FrameDecoder()
    const gotOneByte = []
    for (let i = 0; i < stream.length; i += 1) {
      gotOneByte.push(...oneByte.push(stream.subarray(i, i + 1)))
    }
    expect(gotOneByte.length).toBe(golden.client.length + golden.broker.length + 1)
    expect(gotOneByte.at(-1)?.kind).toBe('fin')

    // 2. Deterministic pseudo-random chunk sizes (1..73 bytes)
    let seed = 0x1234_5678
    const rand = () => {
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0
      return 1 + (seed % 73)
    }
    const randDec = new FrameDecoder()
    const gotRand = []
    for (let pos = 0; pos < stream.length; ) {
      const step = Math.min(rand(), stream.length - pos)
      gotRand.push(...randDec.push(stream.subarray(pos, pos + step)))
      pos += step
    }
    expect(gotRand.length).toBe(gotOneByte.length)
    for (let i = 0; i < gotOneByte.length; i += 1) {
      const a = gotOneByte[i]!
      const b = gotRand[i]!
      expect(b.kind).toBe(a.kind)
      if (a.kind === 'frame' && b.kind === 'frame') {
        expect(b.header).toEqual(a.header)
        expect(toHex(b.payload)).toBe(toHex(a.payload))
      }
    }
  })

  it('enforces generated wire invariants: SubFlags, GridSize, MarksKey, PackedRow u16, and fail-fast headers', () => {
    const expectWireErr = (fn: () => void, kind: WireError['kind']) => {
      let caught: unknown
      try {
        fn()
      } catch (e) {
        caught = e
      }
      expect(caught).toBeInstanceOf(WireError)
      expect((caught as WireError).kind).toBe(kind)
    }

    // 1. SubFlags rejects BACKPRESSURE_PTY without RAW_OUTPUT on both encode and decode, and rejects floats.
    const wSub = new WireWriter()
    expectWireErr(() => encodeSubFlags(wSub, SubFlags.BACKPRESSURE_PTY), 'InvalidValue')
    expectWireErr(() => encodeSubFlags(wSub, 1.5), 'InvalidValue')
    expect(wSub.length).toBe(0)
    expectWireErr(() => decodeSubFlags(new WireReader(new Uint8Array([SubFlags.BACKPRESSURE_PTY]))), 'InvalidValue')
    encodeSubFlags(wSub, SubFlags.RAW_OUTPUT | SubFlags.BACKPRESSURE_PTY)
    expect(decodeSubFlags(new WireReader(wSub.toBytes()))).toBe(SubFlags.RAW_OUTPUT | SubFlags.BACKPRESSURE_PTY)

    // 2. encodeGridSize validates integer + bounds BEFORE mutating WireWriter.
    const wGrid = new WireWriter()
    expectWireErr(() => encodeGridSize(wGrid, { cols: 80, rows: 0 }), 'InvalidValue')
    expectWireErr(() => encodeGridSize(wGrid, { cols: 80.5, rows: 24 }), 'InvalidValue')
    expect(wGrid.length).toBe(0)

    // 3. encodeLeaseKind brokerGrace validates resumeToken length before writing tag/graceMs.
    const wLease = new WireWriter()
    expectWireErr(
      () => encodeLeaseKind(wLease, { kind: 'brokerGrace', graceMs: 1000, resumeToken: new Uint8Array(4) }),
      'InvalidValue',
    )
    expect(wLease.length).toBe(0)

    // 4. decodeMarksKey throws InvalidValue (not InvalidUtf8) on non-UTF-8 bytes, matching Rust.
    const badUtf8 = new Uint8Array(32).fill(0xff)
    expectWireErr(() => decodeMarksKey(new WireReader(badUtf8)), 'InvalidValue')

    // 5. decodePackedRow rejects UTF-16 run offset/length > 0xffff with LimitExceeded.
    const bigRow = new WireWriter()
    bigRow.varU64(0) // line
    bigRow.u8(0) // flags
    bigRow.bytes(new Uint8Array(70_000).fill(0x61)) // 70k ASCII 'a'
    bigRow.varU32(1) // 1 run
    bigRow.u16(0) // colStart
    bigRow.u16(1) // colLen
    bigRow.varU32(66_000) // byteOffset > 0xffff
    bigRow.varU32(1) // byteLen
    bigRow.u16(0) // styleIdx
    bigRow.u16(0) // linkIdx
    bigRow.u8(0) // runFlags
    expectWireErr(() => decodePackedRow(new WireReader(bigRow.toBytes())), 'LimitExceeded')

    // 6. Fail-fast header validation throws InvalidValue even on truncated trailing payloads.
    const badFrame = new WireWriter()
    badFrame.varU64(1) // seq
    badFrame.varU64(1) // epoch
    badFrame.u16(80)
    badFrame.u16(24)
    badFrame.varU64(10) // firstActiveLine
    badFrame.varU64(11) // scrollbackLines > firstActiveLine (truncated immediately after!)
    expectWireErr(() => decodeBrokerMsg(MsgType.FRAME, badFrame.toBytes()), 'InvalidValue')

    const badAttached = new WireWriter()
    badAttached.varU64(1) // session
    badAttached.u32(1) // streamId
    badAttached.u16(80)
    badAttached.u16(24)
    badAttached.varU64(1) // epoch
    badAttached.varU64(5) // firstActiveLine
    badAttached.varU64(6) // scrollbackLines > firstActiveLine (truncated before foreground!)
    expectWireErr(() => decodeBrokerMsg(MsgType.ATTACHED, badAttached.toBytes()), 'InvalidValue')

    const badDelta = new WireWriter()
    badDelta.varU64(1) // epoch
    badDelta.varU64(10) // fromLine
    badDelta.varU64(9) // toLine < fromLine (truncated before added!)
    expectWireErr(() => decodeBrokerMsg(MsgType.SEARCH_DELTA, badDelta.toBytes()), 'InvalidValue')
  })
})
