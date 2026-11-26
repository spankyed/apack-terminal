import { describe, expect, it } from 'vitest'
import {
  BrokerError,
  CONTROL_STREAM_ID,
  DisconnectedError,
  FIN_FRAME,
  FrameDecoder,
  PROTOCOL_VERSION,
  PUSH_REQ_ID,
  SubFlags,
  TerminalClient,
  computeClientProof,
  computeServerProof,
  decodeClientMsg,
  encodeBrokerFrame,
  packRow,
  type BrokerMsg,
  type ByteStream,
  type ClientMsg,
  type FrameUpdate,
} from '../src/index.ts'

class MockBrokerPipe {
  readonly sentToBroker: Array<{ readonly streamId: number; readonly reqId: number; readonly msg: ClientMsg }> = []
  finSeen = false
  closed = false

  private readonly decoder = new FrameDecoder()
  private readonly dataHandlers = new Set<(chunk: Uint8Array) => void>()
  private readonly closeHandlers = new Set<() => void>()
  onClientFrame?: (streamId: number, reqId: number, msg: ClientMsg) => void

  readonly stream: ByteStream = {
    write: (bytes) => {
      if (this.closed) return
      const frames = this.decoder.push(bytes)
      for (const f of frames) {
        if (f.kind === 'fin') {
          this.finSeen = true
          continue
        }
        const msg = decodeClientMsg(f.header.msgType, f.payload)
        this.sentToBroker.push({ streamId: f.header.streamId, reqId: f.header.reqId, msg })
        this.onClientFrame?.(f.header.streamId, f.header.reqId, msg)
      }
    },
    onData: (cb) => {
      this.dataHandlers.add(cb)
      return () => this.dataHandlers.delete(cb)
    },
    onClose: (cb) => {
      this.closeHandlers.add(cb)
      return () => this.closeHandlers.delete(cb)
    },
    close: () => {
      if (this.closed) return
      this.closed = true
      for (const cb of this.closeHandlers) cb()
    },
  }

  pushBroker(streamId: number, reqId: number, msg: BrokerMsg): void {
    const wire = encodeBrokerFrame(streamId, reqId, msg)
    for (const cb of this.dataHandlers) cb(wire)
  }

  pushFin(): void {
    for (const cb of this.dataHandlers) cb(FIN_FRAME)
  }
}

function sampleFrame(seq: number, lineText = 'hello'): FrameUpdate {
  return {
    seq,
    epoch: 1,
    size: { cols: 80, rows: 24 },
    firstActiveLine: 0,
    scrollbackLines: 0,
    scrollDelta: 0,
    cursor: { col: 0, row: 0, shape: 'block', blinking: false, hidden: false },
    modes: { altScreen: false, mouse: false, reverseVideo: false, bracketedPaste: false },
    palette: null,
    rows: [packRow({ line: 0, runs: [{ colStart: 0, colLen: lineText.length, text: lineText }] })],
    imagesGen: 0,
    fullSync: seq === 1,
  }
}

describe('TerminalClient & BrokerSessionSource', () => {
  it('completes mutual HMAC-SHA256 handshake and rejects auth downgrade or wrong server HMAC', async () => {
    const secret = new Uint8Array(32).fill(0x5a)
    const clientNonce = new Uint8Array(32).fill(0x11)
    const serverNonce = new Uint8Array(32).fill(0x22)

    // 1. Happy mutual HMAC handshake
    const pipe = new MockBrokerPipe()
    pipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        pipe.pushBroker(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: true,
          serverNonce,
          wordDelimiters: ' /:',
          capabilities: 0x07,
        })
      } else if (msg.kind === 'authProof') {
        expect(msg.hmac).toEqual(computeClientProof(secret, clientNonce, serverNonce))
        pipe.pushBroker(streamId, reqId, {
          kind: 'authOk',
          serverHmac: computeServerProof(secret, serverNonce, clientNonce),
        })
      }
    }

    const client = await TerminalClient.connect(pipe.stream, {
      authSecret: secret,
      nonce: () => clientNonce,
    })
    expect(client.wordDelimiters).toBe(' /:')
    expect(client.capabilities).toBe(0x07)
    client.close()
    expect(pipe.finSeen).toBe(true)

    // 2. Rejects auth downgrade when client configured authSecret but server says authRequired: false
    const downgradePipe = new MockBrokerPipe()
    downgradePipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        downgradePipe.pushBroker(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: false,
          serverNonce,
          wordDelimiters: ' ',
          capabilities: 0,
        })
      }
    }
    await expect(
      TerminalClient.connect(downgradePipe.stream, { authSecret: secret, nonce: () => clientNonce }),
    ).rejects.toThrow(/downgraded authentication/i)

    // 3. Rejects forged server HMAC
    const forgedPipe = new MockBrokerPipe()
    forgedPipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        forgedPipe.pushBroker(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: true,
          serverNonce,
          wordDelimiters: ' ',
          capabilities: 0,
        })
      } else if (msg.kind === 'authProof') {
        forgedPipe.pushBroker(streamId, reqId, {
          kind: 'authOk',
          serverHmac: new Uint8Array(32).fill(0xff),
        })
      }
    }
    await expect(
      TerminalClient.connect(forgedPipe.stream, { authSecret: secret, nonce: () => clientNonce }),
    ).rejects.toThrow(/mutual HMAC/i)
  })

  it('multiplexes multiple sessions, paces BDP FrameCredit, pauses when hidden, and pipelines 64 KiB image blobs', async () => {
    const pipe = new MockBrokerPipe()
    let now = 100
    const blobHash = new Uint8Array(32).fill(0xab)
    // 140 KiB blob => 3 chunks (64 KiB + 64 KiB + 12 KiB)
    const blobBytes = new Uint8Array(65_536 * 2 + 12_345)
    for (let i = 0; i < blobBytes.length; i += 1) blobBytes[i] = (i * 17) & 0xff

    pipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        pipe.pushBroker(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: false,
          serverNonce: new Uint8Array(32),
          wordDelimiters: ' \t',
          capabilities: 1,
        })
      } else if (msg.kind === 'attach') {
        pipe.pushBroker(CONTROL_STREAM_ID, reqId, {
          kind: 'attached',
          session: msg.session,
          streamId: msg.streamId,
          epoch: 1,
          size: { cols: 80, rows: 24 },
          firstActiveLine: 0,
          scrollbackLines: 0,
          foreground: null,
        })
      } else if (msg.kind === 'detach') {
        pipe.pushBroker(CONTROL_STREAM_ID, reqId, { kind: 'ok' })
      } else if (msg.kind === 'getImageBlob') {
        const end = Math.min(blobBytes.length, msg.offset + msg.maxBytes)
        pipe.pushBroker(streamId, reqId, {
          kind: 'imageBlobChunk',
          blob: msg.blob,
          encoding: 'png',
          totalBytes: blobBytes.length,
          offset: msg.offset,
          data: blobBytes.subarray(msg.offset, end),
        })
      } else if (msg.kind === 'paste') {
        pipe.pushBroker(streamId, reqId, {
          kind: 'pasted',
          outcome: msg.allowUnsafe ? 'pasted' : 'unsafe',
        })
      } else if (msg.kind === 'listSessions') {
        pipe.pushBroker(CONTROL_STREAM_ID, reqId, { kind: 'error', code: 'busy', message: 'busy' })
      }
    }

    const client = await TerminalClient.connect(pipe.stream, { nowMs: () => now })
    const s1 = await client.attachSession(10, { flags: SubFlags.VIEW | SubFlags.EVENTS })
    const s2 = await client.attachSession(20, { flags: SubFlags.VIEW })
    expect(s1.streamId).not.toBe(s2.streamId)

    // Subscribe with manualAck like TerminalView
    const received: FrameUpdate[] = []
    const unsub = s1.subscribe(
      {
        onFrame: (f) => received.push(f),
        onSearchMatches: () => {},
        onSearchDelta: () => {},
      },
      { manualAck: true },
    )

    pipe.pushBroker(s1.streamId, PUSH_REQ_ID, { kind: 'frame', frame: sampleFrame(1, 'alpha') })
    expect(received).toHaveLength(1)
    // No FrameAck sent yet while manualAck subscriber hasn't painted
    expect(pipe.sentToBroker.filter((m) => m.msg.kind === 'frameAck')).toHaveLength(0)

    s1.ackFrame(1)
    const acks1 = pipe.sentToBroker.filter((m) => m.msg.kind === 'frameAck')
    expect(acks1).toHaveLength(1)
    if (acks1[0]!.msg.kind === 'frameAck') {
      expect(acks1[0]!.msg.seq).toBe(1)
      expect(acks1[0]!.msg.credit.maxFrames).toBeGreaterThan(0)
    }

    // Hiding the viewport immediately sends FrameAck with maxFrames = 0 to pause broker emission
    s1.sendViewportHint({
      size: { cols: 80, rows: 24 },
      cellPx: { width: 10, height: 20 },
      visible: false,
      focused: false,
    })
    const lastAck = pipe.sentToBroker.filter((m) => m.msg.kind === 'frameAck').at(-1)!
    if (lastAck.msg.kind === 'frameAck') {
      expect(lastAck.msg.credit.maxFrames).toBe(0)
    }

    // Pipelined 64 KiB image blob reassembly
    const blob = await s1.getImageBlob(blobHash)
    expect(blob.encoding).toBe('png')
    expect(blob.bytes.length).toBe(blobBytes.length)
    expect(blob.bytes[0]).toBe(blobBytes[0])
    expect(blob.bytes.at(-1)).toBe(blobBytes.at(-1))

    // Error propagation as typed BrokerError
    await expect(client.listSessions()).rejects.toBeInstanceOf(BrokerError)

    // Paste unsafe/allowUnsafe round-trip
    expect(await s1.paste('rm -rf /\n', false)).toBe('unsafe')
    expect(await s1.paste('rm -rf /\n', true)).toBe('pasted')

    // DisconnectedError when broker sends FIN
    unsub()
    const pending = s2.fetchRows(1, 0, 10)
    pipe.pushFin()
    await expect(pending).rejects.toBeInstanceOf(DisconnectedError)
  })
})
