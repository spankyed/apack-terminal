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
  sha256,
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
    // 140 KiB blob => 3 chunks (64 KiB + 64 KiB + 12 KiB)
    const blobBytes = new Uint8Array(65_536 * 2 + 12_345)
    for (let i = 0; i < blobBytes.length; i += 1) blobBytes[i] = (i * 17) & 0xff
    const blobHash = sha256(blobBytes)

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
          statuses: [],
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

    // Attaching hidden (`visible: false`, so initialFrameCredit.maxFrames === 0) and later becoming
    // visible BEFORE frame 1 arrives must immediately grant credit via FrameAck(seq=0, maxFrames>0)!
    const hiddenSource = await client.attachSession(30, {
      flags: SubFlags.VIEW | SubFlags.EVENTS,
      initialViewport: {
        size: { cols: 80, rows: 24 },
        cellPx: { width: 10, height: 20 },
        visible: false,
        focused: false,
      },
    })
    const hiddenEvents: string[] = []
    hiddenSource.subscribe({
      onFrame: () => {},
      onSearchMatches: () => {},
      onSearchDelta: () => {},
      onEvent: (ev) => hiddenEvents.push(ev.kind),
    })
    const acksBeforeReveal = pipe.sentToBroker.filter(
      (m) => m.streamId === hiddenSource.streamId && m.msg.kind === 'frameAck',
    ).length
    hiddenSource.sendViewportHint({
      size: { cols: 80, rows: 24 },
      cellPx: { width: 10, height: 20 },
      visible: true,
      focused: true,
    })
    const revealAcks = pipe.sentToBroker.filter(
      (m) => m.streamId === hiddenSource.streamId && m.msg.kind === 'frameAck',
    )
    expect(revealAcks.length).toBe(acksBeforeReveal + 1)
    const revealAck = revealAcks.at(-1)!
    if (revealAck.msg.kind === 'frameAck') {
      expect(revealAck.msg.seq).toBe(0)
      expect(revealAck.msg.credit.maxFrames).toBeGreaterThan(0)
    }
    // programStatusCleared forwards cleanly as a SessionEvent
    pipe.pushBroker(hiddenSource.streamId, PUSH_REQ_ID, { kind: 'programStatusCleared', id: 'agent-1' })
    expect(hiddenEvents).toContain('programStatusCleared')

    // Unsolicited stream-scoped Error push (e.g., PTY write rejected on exited/rewound session)
    // must NOT dispose the stream or close the multiplexed connection!
    pipe.pushBroker(hiddenSource.streamId, PUSH_REQ_ID, { kind: 'error', code: 'pty', message: 'PTY write rejected' })
    expect(client.isClosed).toBe(false)
    expect(hiddenEvents).not.toContain('streamClosed')
    pipe.pushBroker(hiddenSource.streamId, PUSH_REQ_ID, { kind: 'bell' })
    expect(hiddenEvents).toContain('bell')

    // Oversized search / wait needles (> 4 KiB) reject locally BEFORE sending any frame to the broker!
    const sentBeforeOversized = pipe.sentToBroker.length
    const hugeNeedle = 'x'.repeat(4097)
    await expect(hiddenSource.setSearch(hugeNeedle, false)).rejects.toThrow(/limit exceeded/i)
    await expect(client.wait(30, { kind: 'text', needle: hugeNeedle }, 1000)).rejects.toThrow(/limit exceeded/i)
    expect(pipe.sentToBroker.length).toBe(sentBeforeOversized)
    expect(client.isClosed).toBe(false)

    hiddenSource.dispose()

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

  it('rejects 0-byte/short/mismatched-hash image chunks, FIN piggybacked on HelloOk, and post-dispose calls', async () => {
    // 1. FIN arriving in same chunk right after HelloOk marks returned client closed
    const finPipe = new MockBrokerPipe()
    finPipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        const helloOk = encodeBrokerFrame(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: false,
          serverNonce: new Uint8Array(32),
          wordDelimiters: ' ',
          capabilities: 0,
        })
        const combined = new Uint8Array(helloOk.length + FIN_FRAME.length)
        combined.set(helloOk, 0)
        combined.set(FIN_FRAME, helloOk.length)
        // Push both in one read chunk
        finPipe.stream.onData(() => {}) // no-op
        ;(finPipe as unknown as { dataHandlers: Set<(c: Uint8Array) => void> }).dataHandlers.forEach((cb) => cb(combined))
      }
    }
    const cFin = await TerminalClient.connect(finPipe.stream)
    expect(cFin.isClosed).toBe(true)

    // 2. Hostile 0-byte initial chunk for totalBytes = 64 MiB fails fast without OOM/hang
    const imgPipe = new MockBrokerPipe()
    const wantedBlob = new Uint8Array(32).fill(0x42)
    let mode: 'zero' | 'wrongHash' | 'corruptBody' = 'zero'
    imgPipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        imgPipe.pushBroker(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: false,
          serverNonce: new Uint8Array(32),
          wordDelimiters: ' ',
          capabilities: 0,
        })
      } else if (msg.kind === 'attach') {
        imgPipe.pushBroker(CONTROL_STREAM_ID, reqId, {
          kind: 'attached',
          session: msg.session,
          streamId: msg.streamId,
          epoch: 1,
          size: { cols: 80, rows: 24 },
          firstActiveLine: 0,
          scrollbackLines: 0,
          foreground: null,
          statuses: [],
        })
      } else if (msg.kind === 'getImageBlob') {
        imgPipe.pushBroker(streamId, reqId, {
          kind: 'imageBlobChunk',
          blob: mode === 'wrongHash' ? new Uint8Array(32).fill(0x99) : msg.blob,
          encoding: 'png',
          totalBytes: mode === 'zero' ? 64 * 1024 * 1024 : 4,
          offset: 0,
          data: mode === 'zero' ? new Uint8Array(0) : new Uint8Array([1, 2, 3, 4]),
        })
      }
    }
    const client = await TerminalClient.connect(imgPipe.stream)
    const session = await client.attachSession(1)
    await expect(session.getImageBlob(wantedBlob)).rejects.toThrow(/short initial image blob chunk/i)
    mode = 'wrongHash'
    await expect(session.getImageBlob(wantedBlob)).rejects.toThrow(/hash mismatch/i)
    mode = 'corruptBody'
    await expect(session.getImageBlob(wantedBlob)).rejects.toThrow(/SHA-256 mismatch/i)

    session.dispose()
    await expect(session.commands()).rejects.toBeInstanceOf(DisconnectedError)
    client.close()
  })

  it('cleans up stream on exited when !RAW_OUTPUT and enforces MAX_CLIENT_NAME_BYTES', async () => {
    const pipe = new MockBrokerPipe()
    pipe.onClientFrame = (streamId, reqId, msg) => {
      if (msg.kind === 'hello') {
        pipe.pushBroker(streamId, reqId, {
          kind: 'helloOk',
          protocolVersion: PROTOCOL_VERSION,
          authRequired: false,
          serverNonce: new Uint8Array(32),
          wordDelimiters: ' ',
          capabilities: 0,
        })
      } else if (msg.kind === 'attach') {
        pipe.pushBroker(streamId, reqId, {
          kind: 'attached',
          session: msg.session,
          streamId: msg.streamId,
          size: { cols: 80, rows: 24 },
          epoch: 1,
          firstActiveLine: 0,
          scrollbackLines: 0,
          foreground: null,
          statuses:
            msg.session === 1
              ? [
                  {
                    id: '',
                    state: 'working',
                    kind: null,
                    progress: 40,
                    app: 'codex',
                    title: 'Working',
                    message: null,
                    changed: 1,
                    sinceMs: 1000,
                    confirmed: 'matched',
                  },
                ]
              : [],
        })
      }
    }

    // Overlong clientName (> 256 bytes) rejected before connecting
    const badPipe = new MockBrokerPipe()
    await expect(
      TerminalClient.connect(badPipe.stream, { clientName: 'a'.repeat(257) }),
    ).rejects.toThrow(/limit exceeded/i)

    const client = await TerminalClient.connect(pipe.stream, { clientName: 'a'.repeat(256) })
    const viewOnly = await client.attachSession(1, { flags: SubFlags.VIEW | SubFlags.EVENTS })
    const rawAlso = await client.attachSession(2, { flags: SubFlags.VIEW | SubFlags.EVENTS | SubFlags.RAW_OUTPUT })
    expect(viewOnly.initialStatuses).toHaveLength(1)
    expect(viewOnly.initialStatuses[0]?.app).toBe('codex')
    expect(rawAlso.initialStatuses).toHaveLength(0)

    // Pending RPC on non-RAW_OUTPUT stream rejects immediately on Exited (broker suppresses redundant StreamClosed)
    // and marks BrokerSessionSource locally disposed without emitting a synthetic streamClosed.
    const viewEvents: string[] = []
    viewOnly.subscribe({
      onFrame: () => {},
      onSearchMatches: () => {},
      onSearchDelta: () => {},
      onEvent: (ev) => viewEvents.push(ev.kind),
    })
    const pendingView = viewOnly.fetchRows(1, 0, 10)
    pipe.pushBroker(viewOnly.streamId, PUSH_REQ_ID, { kind: 'exited', exitCode: 0, signal: null })
    await expect(pendingView).rejects.toBeInstanceOf(DisconnectedError)
    expect(viewEvents).toEqual(['exited'])
    const sentAfterExit = pipe.sentToBroker.length
    await expect(viewOnly.fetchRows(1, 0, 10)).rejects.toBeInstanceOf(DisconnectedError)
    viewOnly.sendKey({
      code: 'KeyA',
      action: 'press',
      modifiers: 0,
      text: 'a',
      unshiftedCodepoint: 97,
      composed: false,
    })
    expect(pipe.sentToBroker.length).toBe(sentAfterExit)

    // Pending RPC on RAW_OUTPUT stream stays alive across Exited until trailing StreamClosed arrives
    const rawEvents: string[] = []
    rawAlso.subscribe({
      onFrame: () => {},
      onSearchMatches: () => {},
      onSearchDelta: () => {},
      onEvent: (ev) => rawEvents.push(ev.kind),
    })
    const pendingRaw = rawAlso.fetchRows(1, 0, 10)
    pipe.pushBroker(rawAlso.streamId, PUSH_REQ_ID, { kind: 'exited', exitCode: 0, signal: null })
    pipe.pushBroker(rawAlso.streamId, PUSH_REQ_ID, { kind: 'streamClosed', reason: 'session ended' })
    await expect(pendingRaw).rejects.toBeInstanceOf(DisconnectedError)
    expect(rawEvents).toEqual(['exited', 'streamClosed'])

    client.close()
  })
})
