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
  encodeHeader,
  encodePackedRow,
  verifyServerProof,
} from '../src/index.ts'
import {
  MAX_BLOB_BYTES,
  MAX_PACKED_ROW_TEXT_BYTES,
  MAX_ROWS_PER_FETCH,
  MAX_SCROLLBACK_LINES,
  MAX_SEARCH_NEEDLE_BYTES,
  MAX_STRING_BYTES,
  MAX_WAIT_STATUSES,
} from '../src/wire/constants.ts'
import { encodeBrokerPayload, encodeClientPayload } from '../src/wire/messages.ts'
import {
  SubFlags,
  decodeImagePlacement,
  decodeMarksKey,
  decodeSubFlags,
  encodeCellSpan,
  encodeGridSize,
  encodeImagePiece,
  encodeImagePlacement,
  encodeKeyInput,
  encodeLeaseKind,
  encodePointerInput,
  encodeRgb24,
  encodeSignalScope,
  encodeStatusRecord,
  encodeSubFlags,
  encodeWaitCondition,
  isValidPackedColor,
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

    // 3. encodeLeaseKind brokerGrace validates resumeToken length & non-zero BEFORE writing tag/graceMs.
    const wLease = new WireWriter()
    expectWireErr(
      () => encodeLeaseKind(wLease, { kind: 'brokerGrace', graceMs: 1000, resumeToken: new Uint8Array(4) }),
      'InvalidValue',
    )
    expect(wLease.length).toBe(0)
    expectWireErr(
      () => encodeLeaseKind(wLease, { kind: 'brokerGrace', graceMs: 1000, resumeToken: new Uint8Array(32) }),
      'InvalidValue',
    )
    expect(wLease.length).toBe(0)

    // 4. decodeMarksKey throws InvalidValue (not InvalidUtf8) on non-UTF-8 bytes, matching Rust.
    const badUtf8 = new Uint8Array(32).fill(0xff)
    expectWireErr(() => decodeMarksKey(new WireReader(badUtf8)), 'InvalidValue')

    // 5. PackedRow MAX_PACKED_ROW_TEXT_BYTES exact pass (65_535) and reject (65_536) on both encode & decode,
    //    plus empty non-zero links rejection.
    const exactText = 'a'.repeat(MAX_PACKED_ROW_TEXT_BYTES)
    const exactRuns = new Uint32Array([0 | (1 << 16), (MAX_PACKED_ROW_TEXT_BYTES - 1) | (1 << 16), 0, 0])
    const exactW = new WireWriter()
    encodePackedRow(exactW, {
      line: 0,
      flags: 0,
      text: exactText,
      runs: exactRuns,
      styles: new Uint32Array(0),
      links: [],
    })
    const exactDecoded = decodePackedRow(new WireReader(exactW.toBytes()))
    expect(exactDecoded.text.length).toBe(MAX_PACKED_ROW_TEXT_BYTES)
    expect(exactDecoded.runs[1]).toBe((MAX_PACKED_ROW_TEXT_BYTES - 1) | (1 << 16))

    const overRow = new WireWriter()
    overRow.varU64(0) // line
    overRow.u8(0) // flags
    overRow.bytes(new Uint8Array(MAX_PACKED_ROW_TEXT_BYTES + 1).fill(0x61))
    expectWireErr(() => decodePackedRow(new WireReader(overRow.toBytes())), 'LimitExceeded')
    expectWireErr(
      () =>
        encodePackedRow(new WireWriter(), {
          line: 0,
          flags: 0,
          text: 'a'.repeat(MAX_PACKED_ROW_TEXT_BYTES + 1),
          runs: new Uint32Array(0),
          styles: new Uint32Array(0),
          links: [],
        }),
      'LimitExceeded',
    )
    expectWireErr(
      () =>
        encodePackedRow(new WireWriter(), {
          line: 0,
          flags: 0,
          text: '',
          runs: new Uint32Array(0),
          styles: new Uint32Array(0),
          links: ['', 'https://ok.test', ''],
        }),
      'InvalidValue',
    )

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

  it('validates encodeHeader ranges, bytesBounded clamp, varU64 safe integer boundary, and symmetric TS encoders', () => {
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

    // 1. encodeHeader rejects out-of-range / non-integer streamId, msgType, reqId.
    expectWireErr(() => encodeHeader(-1, MsgType.OK, 0, 0), 'InvalidValue')
    expectWireErr(() => encodeHeader(0x1_0000_0000, MsgType.OK, 0, 0), 'InvalidValue')
    expectWireErr(() => encodeHeader(1.5, MsgType.OK, 0, 0), 'InvalidValue')
    expectWireErr(() => encodeHeader(0, -1, 0, 0), 'InvalidValue')
    expectWireErr(() => encodeHeader(0, 0x1_0000, 0, 0), 'InvalidValue')
    expectWireErr(() => encodeHeader(0, MsgType.OK, -1, 0), 'InvalidValue')
    expectWireErr(() => encodeHeader(0, MsgType.OK, 0x1_0000_0000, 0), 'InvalidValue')

    // 2. WireReader.bytesBounded clamps maxLen > MAX_BLOB_BYTES to MAX_BLOB_BYTES.
    const overBlob = new WireWriter()
    overBlob.varU32(MAX_BLOB_BYTES + 1)
    expectWireErr(
      () => new WireReader(overBlob.toBytes()).bytesBounded(MAX_BLOB_BYTES + 1024, 'oversized'),
      'LimitExceeded',
    )

    // 3. varU64 exact Number.MAX_SAFE_INTEGER boundary pass + MAX_SAFE_INTEGER + 1 reject.
    const wSafe = new WireWriter()
    wSafe.varU64(Number.MAX_SAFE_INTEGER)
    expect(new WireReader(wSafe.toBytes()).varU64()).toBe(Number.MAX_SAFE_INTEGER)
    expectWireErr(() => new WireWriter().varU64(Number.MAX_SAFE_INTEGER + 1), 'InvalidValue')
    const wBig = new WireWriter()
    wBig.varU64(BigInt(Number.MAX_SAFE_INTEGER) + 1n)
    expect(new WireReader(wBig.toBytes()).varU64Big()).toBe(BigInt(Number.MAX_SAFE_INTEGER) + 1n)
    expectWireErr(() => new WireReader(wBig.toBytes()).varU64(), 'LimitExceeded')

    // 4. Symmetric TS encoder validation matches Rust decode constraints without partial writes.
    const blob = new Uint8Array(32).fill(0x42)

    const wWait = new WireWriter()
    expectWireErr(() => encodeWaitCondition(wWait, { kind: 'status', states: [] }), 'InvalidValue')
    expectWireErr(
      () =>
        encodeWaitCondition(wWait, {
          kind: 'status',
          states: Array.from({ length: MAX_WAIT_STATUSES + 1 }, () => 'idle' as const),
        }),
      'LimitExceeded',
    )
    expect(wWait.length).toBe(0)

    const wSpan = new WireWriter()
    expectWireErr(
      () => encodeCellSpan(wSpan, { start: { line: 5, col: 2 }, end: { line: 4, col: 9 } }),
      'InvalidValue',
    )
    expect(wSpan.length).toBe(0)

    const wKey = new WireWriter()
    expectWireErr(
      () =>
        encodeKeyInput(wKey, {
          code: 'KeyA',
          action: 'press',
          modifiers: 0xff,
          text: 'a',
          unshiftedCodepoint: 97,
          composed: false,
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodeKeyInput(wKey, {
          code: 'KeyA',
          action: 'press',
          modifiers: 0,
          text: '',
          unshiftedCodepoint: 0xd800,
          composed: false,
        }),
      'InvalidValue',
    )
    expect(wKey.length).toBe(0)

    const wPtr = new WireWriter()
    expectWireErr(
      () =>
        encodePointerInput(wPtr, {
          action: 'press',
          button: 99,
          modifiers: 0,
          xPx: 0,
          yPx: 0,
          cellPx: { width: 8, height: 16 },
        }),
      'InvalidValue',
    )
    expect(wPtr.length).toBe(0)

    const wPiece = new WireWriter()
    expectWireErr(
      () => encodeImagePiece(wPiece, { blob, colStart: 0, colLen: 0, srcRect: [0, 0, 1, 1], dstPx: [1, 1] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePiece(wPiece, { blob, colStart: 0, colLen: 1, srcRect: [0, 0, 0, 1], dstPx: [1, 1] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePiece(wPiece, { blob, colStart: 0, colLen: 1, srcRect: [0, 0, 1, 1], dstPx: [0, 1] }),
      'InvalidValue',
    )
    expect(wPiece.length).toBe(0)

    const validPlacement = {
      blob,
      line: 3,
      col: -1,
      sourceRect: [2, 4, 30, 20] as const,
      cellOffset: [1, 3] as const,
      pixelSize: [60, 40] as const,
      cellSpan: [4, 2] as const,
      zIndex: -2,
    }
    const wPlace = new WireWriter()
    encodeImagePlacement(wPlace, validPlacement)
    expect(decodeImagePlacement(new WireReader(wPlace.toBytes()))).toEqual(validPlacement)

    const wBadPlace = new WireWriter()
    expectWireErr(
      () => encodeImagePlacement(wBadPlace, { ...validPlacement, sourceRect: [0, 0, 0, 10] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wBadPlace, { ...validPlacement, sourceRect: [0xffff_ffff, 0, 1, 10] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wBadPlace, { ...validPlacement, pixelSize: [0, 40] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wBadPlace, { ...validPlacement, cellSpan: [4, 0] }),
      'InvalidValue',
    )
    expect(wBadPlace.length).toBe(0)

    const wStatus = new WireWriter()
    expectWireErr(
      () =>
        encodeStatusRecord(wStatus, {
          id: 'agent',
          state: 'working',
          kind: 'permission',
          progress: null,
          app: null,
          title: null,
          message: null,
          changed: 1,
          sinceMs: 1,
          confirmed: 'unknown',
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodeStatusRecord(wStatus, {
          id: 'agent',
          state: 'working',
          kind: null,
          progress: 101,
          app: null,
          title: null,
          message: null,
          changed: 1,
          sinceMs: 1,
          confirmed: 'unknown',
        }),
      'InvalidValue',
    )
    expect(wStatus.length).toBe(0)

    expectWireErr(
      () => encodeClientPayload({ kind: 'fetchRows', epoch: 1, fromLine: 0, count: 0 }),
      'LimitExceeded',
    )
    expectWireErr(
      () => encodeClientPayload({ kind: 'fetchRows', epoch: 1, fromLine: 0, count: -1 }),
      'LimitExceeded',
    )
    expectWireErr(
      () => encodeClientPayload({ kind: 'fetchRows', epoch: 1, fromLine: 0, count: MAX_ROWS_PER_FETCH + 1 }),
      'LimitExceeded',
    )
    expectWireErr(
      () => encodeClientPayload({ kind: 'getImageBlob', blob, offset: 0, maxBytes: 0 }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeClientPayload({ kind: 'getImageBlob', blob, offset: 0, maxBytes: -1 }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeBrokerPayload({ kind: 'progress', state: 'indeterminate', percent: 50 }),
      'InvalidValue',
    )

    // 5. Additional adversarial checks: atomic rollback, surrogate-pair split, rgb24/color validation,
    //    NaN maxLen fail-closed, WireWriter.str/bytes caps, remaining ImagePlacement axes & broker encoders.
    expectWireErr(() => encodeHeader(0, MsgType.OK, 0, -1), 'InvalidValue')
    expectWireErr(() => encodeHeader(0, MsgType.OK, 0, Number.NaN), 'InvalidValue')
    expectWireErr(() => encodeHeader(0, MsgType.OK, 0, MAX_BLOB_BYTES + 1), 'FrameTooLarge')

    expectWireErr(() => new WireReader(new Uint8Array([0])).bytesBounded(Number.NaN, 'nan'), 'InvalidValue')
    expectWireErr(() => new WireWriter().strBounded('a', Number.NaN, 'nan'), 'InvalidValue')
    expectWireErr(() => new WireWriter().str('a'.repeat(MAX_STRING_BYTES + 1)), 'LimitExceeded')
    expectWireErr(() => new WireWriter().bytes(new Uint8Array(MAX_BLOB_BYTES + 1)), 'LimitExceeded')

    const wRgb = new WireWriter()
    expectWireErr(() => encodeRgb24(wRgb, Number.NaN), 'InvalidValue')
    expectWireErr(() => encodeRgb24(wRgb, -1), 'InvalidValue')
    expectWireErr(() => encodeRgb24(wRgb, 1.5), 'InvalidValue')
    expectWireErr(() => encodeRgb24(wRgb, 0x01_00_00_00), 'InvalidValue')
    expect(wRgb.length).toBe(0)
    expect(isValidPackedColor(Number.NaN)).toBe(false)
    expect(isValidPackedColor(1.5)).toBe(false)
    expect(isValidPackedColor(0x1_0000_0001)).toBe(false)

    // Atomic rollback on tagged unions, structs, and PackedRow mid-encode errors.
    const wAtom = new WireWriter()
    expectWireErr(
      () => encodeWaitCondition(wAtom, { kind: 'text', needle: 'a'.repeat(MAX_SEARCH_NEEDLE_BYTES + 1) }),
      'LimitExceeded',
    )
    expectWireErr(() => encodeLeaseKind(wAtom, { kind: 'session', session: 0 }), 'InvalidValue')
    expectWireErr(() => encodeSignalScope(wAtom, { kind: 'pid', pid: 0 }), 'InvalidValue')
    expectWireErr(
      () => encodeCellSpan(wAtom, { start: { line: 0, col: 0 }, end: { line: Number.NaN, col: 0 } }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePiece(wAtom, { blob, colStart: 10, colLen: -5, srcRect: [0, 0, 1, 1], dstPx: [1, 1] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePiece(wAtom, { blob, colStart: 0, colLen: 1, srcRect: [-1, 0, 0x1_0000_0000, 1], dstPx: [1, 1] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePiece(wAtom, { blob, colStart: 0, colLen: 1, srcRect: [0, 0, 1, 1], dstPx: [-1, 1] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, sourceRect: [0, 0, 10, 0] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, sourceRect: [0, 0xffff_ffff, 10, 1] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, sourceRect: [-1, 0, 10, 10] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, pixelSize: [10, 0] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, pixelSize: [-1, 10] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, cellSpan: [0, 2] }),
      'InvalidValue',
    )
    expectWireErr(
      () => encodeImagePlacement(wAtom, { ...validPlacement, zIndex: Number.NaN }),
      'InvalidValue',
    )
    // PackedRow surrogate-pair split ("🚀" has UTF-16 length 2) + invalid run/style atomic rollback.
    expectWireErr(
      () =>
        encodePackedRow(wAtom, {
          line: 0,
          flags: 0,
          text: '🚀',
          runs: new Uint32Array([0 | (2 << 16), 0 | (1 << 16), 0, 1]),
          styles: new Uint32Array(0),
          links: [],
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodePackedRow(wAtom, {
          line: 0,
          flags: 0,
          text: '🚀',
          runs: new Uint32Array([0 | (2 << 16), 1 | (1 << 16), 0, 1]),
          styles: new Uint32Array(0),
          links: [],
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodePackedRow(wAtom, {
          line: 0,
          flags: 0,
          text: 'hi',
          runs: new Uint32Array([0 | (2 << 16), 0 | (2 << 16), 0, 0]),
          styles: new Uint32Array([1, 0, 0, 0]), // styles[0] not default
          links: [],
        }),
      'InvalidValue',
    )
    expect(wAtom.length).toBe(0)

    // Broker message encode invariants (FrameUpdate, Attached, SearchDelta, ImageBlobChunk).
    expectWireErr(
      () =>
        encodeBrokerPayload({
          kind: 'frame',
          frame: {
            seq: 1,
            epoch: 1,
            size: { cols: 80, rows: 24 },
            firstActiveLine: 5,
            scrollbackLines: 6,
            scrollDelta: 0,
            cursor: { col: 0, row: 0, shape: 'block', blinking: false, hidden: false },
            modes: { altScreen: false, mouse: false, reverseVideo: false, bracketedPaste: false },
            palette: null,
            rows: [],
            imagesGen: 0,
            fullSync: true,
          },
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodeBrokerPayload({
          kind: 'attached',
          session: 1,
          streamId: 1,
          size: { cols: 80, rows: 24 },
          epoch: 1,
          firstActiveLine: 5,
          scrollbackLines: 6,
          foreground: null,
          statuses: [],
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodeBrokerPayload({
          kind: 'searchDelta',
          epoch: 1,
          fromLine: 10,
          toLine: 5,
          added: [],
          truncated: false,
        }),
      'InvalidValue',
    )
    expectWireErr(
      () =>
        encodeBrokerPayload({
          kind: 'imageBlobChunk',
          blob,
          encoding: 'png',
          totalBytes: 4,
          offset: 2,
          data: new Uint8Array([1, 2, 3]),
        }),
      'InvalidValue',
    )

    // SpawnSpec.scrollbackLimit enforces 0..=MAX_SCROLLBACK_LINES on both encode and decode.
    const spawnBase = (scrollbackLimit: number) => ({
      kind: 'spawn' as const,
      spec: {
        argv: [],
        cwd: null,
        env: [],
        size: { cols: 80, rows: 24 },
        cellPx: { width: 0, height: 0 },
        scrollbackLimit,
        marksKey: null,
        resizePolicy: 'explicitOverride' as const,
        recordDir: null,
        restoreFrom: null,
        imageFiles: false,
        kept: false,
      },
    })
    const okSpawn = encodeClientPayload(spawnBase(MAX_SCROLLBACK_LINES))
    const decodedSpawn = decodeClientMsg(okSpawn.msgType, okSpawn.payload)
    expect(decodedSpawn.kind).toBe('spawn')
    if (decodedSpawn.kind === 'spawn') expect(decodedSpawn.spec.scrollbackLimit).toBe(MAX_SCROLLBACK_LINES)
    expectWireErr(() => encodeClientPayload(spawnBase(MAX_SCROLLBACK_LINES + 1)), 'LimitExceeded')
    expectWireErr(() => encodeClientPayload(spawnBase(-1)), 'LimitExceeded')
    expectWireErr(() => encodeClientPayload(spawnBase(Number.NaN)), 'LimitExceeded')
    expectWireErr(() => encodeClientPayload(spawnBase(1.5)), 'LimitExceeded')
    // Decode-side boundary rejection on a raw wire payload with scrollbackLimit = MAX_SCROLLBACK_LINES + 1.
    const rawSpawnOver = new WireWriter()
    rawSpawnOver.varU32(0) // argv
    rawSpawnOver.bool(false) // cwd
    rawSpawnOver.varU32(0) // env
    rawSpawnOver.u16(80) // size.cols
    rawSpawnOver.u16(24) // size.rows
    rawSpawnOver.u16(0) // cellPx.width
    rawSpawnOver.u16(0) // cellPx.height
    rawSpawnOver.varU32(MAX_SCROLLBACK_LINES + 1) // scrollbackLimit (over limit)
    expectWireErr(() => decodeClientMsg(MsgType.SPAWN, rawSpawnOver.toBytes()), 'LimitExceeded')
  })
})
