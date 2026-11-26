/**
 * Total, streaming, memory-bounded binary frame encoder & decoder matching
 * `crates/apack-proto/src/frame.rs`.
 */
import {
  FIN_FRAME,
  FRAME_HEADER_LEN,
  INNER_HEADER_LEN,
  MAX_BODY_LEN,
  MAX_FRAME_PAYLOAD,
  WireError,
} from './codec.ts'

export interface FrameHeader {
  readonly streamId: number
  readonly msgType: number
  readonly reqId: number
  readonly payloadLen: number
}

export type DecodedFrame =
  | { readonly kind: 'fin' }
  | { readonly kind: 'frame'; readonly header: FrameHeader; readonly payload: Uint8Array }

export function encodeHeader(
  streamId: number,
  msgType: number,
  reqId: number,
  payloadLen: number,
): Uint8Array {
  if (!Number.isInteger(payloadLen) || payloadLen < 0 || payloadLen > MAX_FRAME_PAYLOAD) {
    throw new WireError('FrameTooLarge', `wire frame payload ${payloadLen} bytes exceeds limit`)
  }
  const header = new Uint8Array(FRAME_HEADER_LEN)
  const view = new DataView(header.buffer)
  view.setUint32(0, INNER_HEADER_LEN + payloadLen, true)
  view.setUint32(4, streamId >>> 0, true)
  view.setUint16(8, msgType & 0xffff, true)
  view.setUint32(10, reqId >>> 0, true)
  return header
}

export function encodeFrame(
  streamId: number,
  msgType: number,
  reqId: number,
  payload: Uint8Array,
): Uint8Array {
  const header = encodeHeader(streamId, msgType, reqId, payload.length)
  if (payload.length === 0) return header
  const out = new Uint8Array(FRAME_HEADER_LEN + payload.length)
  out.set(header, 0)
  out.set(payload, FRAME_HEADER_LEN)
  return out
}

export function encodeFin(): Uint8Array {
  return FIN_FRAME.slice()
}

type DecoderState =
  | { readonly kind: 'length'; readonly buf: Uint8Array; have: number }
  | { readonly kind: 'inner'; readonly payloadLen: number; readonly buf: Uint8Array; have: number }
  | {
      readonly kind: 'payload'
      readonly header: FrameHeader
      /** Grows lazily as chunks arrive so a huge declared length on a stalled chunk allocates 0 bytes */
      readonly chunks: Uint8Array[]
      have: number
    }
  | { readonly kind: 'closed' }

function fillFixed(
  dst: Uint8Array,
  have: number,
  input: Uint8Array,
  offset: number,
): { have: number; offset: number; done: boolean } {
  const take = Math.min(dst.length - have, input.length - offset)
  dst.set(input.subarray(offset, offset + take), have)
  const nextHave = have + take
  return { have: nextHave, offset: offset + take, done: nextHave === dst.length }
}

/**
 * Streaming binary frame decoder invariant across arbitrary 1-byte and random chunk splits.
 */
export class FrameDecoder {
  private state: DecoderState = { kind: 'length', buf: new Uint8Array(4), have: 0 }

  get closed(): boolean {
    return this.state.kind === 'closed'
  }

  /**
   * Feeds `input` and yields either a complete frame/FIN plus `consumed` bytes, or `null` when all
   * of `input` was consumed and more bytes are needed.
   */
  step(input: Uint8Array): { consumed: number; frame: DecodedFrame | null } {
    let offset = 0
    while (offset < input.length) {
      const st = this.state
      if (st.kind === 'closed') {
        return { consumed: input.length, frame: null }
      }
      if (st.kind === 'length') {
        const res = fillFixed(st.buf, st.have, input, offset)
        st.have = res.have
        offset = res.offset
        if (res.done) {
          const view = new DataView(st.buf.buffer, st.buf.byteOffset, 4)
          const bodyLen = view.getUint32(0, true)
          if (bodyLen === 0) {
            this.state = { kind: 'closed' }
            return { consumed: offset, frame: { kind: 'fin' } }
          }
          if (bodyLen < INNER_HEADER_LEN) {
            this.state = { kind: 'closed' }
            throw new WireError('InvalidValue', 'frame shorter than header')
          }
          if (bodyLen > MAX_BODY_LEN) {
            this.state = { kind: 'closed' }
            throw new WireError('FrameTooLarge', `wire frame payload ${bodyLen - INNER_HEADER_LEN} bytes exceeds limit`)
          }
          this.state = {
            kind: 'inner',
            payloadLen: bodyLen - INNER_HEADER_LEN,
            buf: new Uint8Array(INNER_HEADER_LEN),
            have: 0,
          }
        }
        continue
      }
      if (st.kind === 'inner') {
        const res = fillFixed(st.buf, st.have, input, offset)
        st.have = res.have
        offset = res.offset
        if (res.done) {
          const view = new DataView(st.buf.buffer, st.buf.byteOffset, INNER_HEADER_LEN)
          const header: FrameHeader = {
            streamId: view.getUint32(0, true),
            msgType: view.getUint16(4, true),
            reqId: view.getUint32(6, true),
            payloadLen: st.payloadLen,
          }
          if (header.payloadLen === 0) {
            this.state = { kind: 'length', buf: new Uint8Array(4), have: 0 }
            return { consumed: offset, frame: { kind: 'frame', header, payload: new Uint8Array(0) } }
          }
          this.state = { kind: 'payload', header, chunks: [], have: 0 }
        }
        continue
      }
      // payload
      const expected = st.header.payloadLen
      const need = expected - st.have
      const take = Math.min(need, input.length - offset)
      st.chunks.push(input.slice(offset, offset + take))
      st.have += take
      offset += take
      if (st.have === expected) {
        let payload: Uint8Array
        if (st.chunks.length === 1) {
          payload = st.chunks[0]!
        } else {
          payload = new Uint8Array(expected)
          let pos = 0
          for (const chunk of st.chunks) {
            payload.set(chunk, pos)
            pos += chunk.length
          }
        }
        const header = st.header
        this.state = { kind: 'length', buf: new Uint8Array(4), have: 0 }
        return { consumed: offset, frame: { kind: 'frame', header, payload } }
      }
    }
    return { consumed: offset, frame: null }
  }

  /** Convenience helper draining all complete frames contained in `chunk`. */
  push(chunk: Uint8Array): DecodedFrame[] {
    const out: DecodedFrame[] = []
    let rest = chunk
    while (rest.length > 0 && !this.closed) {
      const { consumed, frame } = this.step(rest)
      if (frame !== null) out.push(frame)
      if (consumed === 0) break
      rest = rest.subarray(consumed)
    }
    return out
  }
}
