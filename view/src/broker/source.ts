/**
 * Sessions on a broker, as sources a view is drawn from.
 *
 * One connection carries every session a window shows. Each is given a channel of its own when a view
 * attaches, and from then on the broker's frames for it arrive here and are handed to that view.
 *
 * **This knows nothing about sockets.** It is given a stream of bytes that is already past the broker's
 * handshake — whatever carries them is the caller's — so it runs the same over a message port, a socket
 * in a test, or a recording played back.
 */
import type {
  Command,
  Fetched,
  KeyInput,
  PasteOutcome,
  Picture,
  Placements,
  PointerInput,
  Position,
  SourceListener,
  Span,
  TerminalSource,
} from '../source.ts'
import { Messages, decodeToView, encodeToBroker, type ToBroker, type ToView } from './wire.ts'

/** A two-way stream of bytes to a broker's view connection, after its handshake */
export interface ByteStream {
  send(bytes: Uint8Array): void
  /** Bytes as they arrive, in pieces of any size. The returned function stops them */
  onData(listener: (chunk: Uint8Array) => void): () => void
  /** The stream is over and nothing more will arrive */
  onClose(listener: () => void): () => void
}

/** A request a session ended before answering, or that the broker could not carry out */
export class Unanswered extends Error {}

type Answer = Exclude<ToView, { kind: 'frame' | 'ended' }>

/** What a session is given of the connection it is on */
interface Connection {
  send(message: ToBroker): void
  /** A channel no session has used, on which this one now hears what the broker says */
  channel(session: SessionSource): number
  /** The session is finished with a channel: nothing more is expected on it */
  release(channel: number): void
  /** The session has ended */
  forget(session: SessionSource): void
}

/** One connection to a broker, and the sessions drawn over it */
export class BrokerViews {
  private readonly messages = new Messages()
  /** Every session opened that has not ended, and the one each channel in use belongs to */
  private readonly sessions = new Set<SessionSource>()
  private readonly channels = new Map<number, SessionSource>()
  private nextChannel = 1
  private closed = false
  private readonly stop: Array<() => void>
  private readonly closing = new Set<() => void>()

  constructor(private readonly stream: ByteStream) {
    this.stop = [stream.onData((chunk) => this.receive(chunk)), stream.onClose(() => this.close())]
  }

  /** A source for the session the broker calls `session`. Nothing is sent until a view attaches to it */
  open(session: string): TerminalSource {
    const source = new SessionSource(session, {
      send: (message) => this.send(message),
      channel: (owner) => {
        const channel = this.nextChannel
        this.nextChannel += 1
        this.channels.set(channel, owner)
        return channel
      },
      release: (channel) => { this.channels.delete(channel) },
      forget: (ended) => { this.sessions.delete(ended) },
    })
    this.sessions.add(source)
    if (this.closed) source.end()
    return source
  }

  /**
   * Hears that this has stopped, once, whatever stopped it: `close`, the stream ending, or bytes arriving
   * that are not the protocol. An owner that keeps one of these per broker learns here to make another.
   * The returned function stops listening.
   */
  onClose(listener: () => void): () => void {
    this.closing.add(listener)
    return () => { this.closing.delete(listener) }
  }

  /** Stops listening to the stream. Every session is told it has ended */
  close(): void {
    if (this.closed) return
    this.closed = true
    for (const stop of this.stop) stop()
    for (const session of [...this.sessions]) session.end()
    this.sessions.clear()
    this.channels.clear()
    for (const listener of [...this.closing]) listener()
    this.closing.clear()
  }

  private send(message: ToBroker): void {
    if (!this.closed) this.stream.send(encodeToBroker(message))
  }

  private receive(chunk: Uint8Array): void {
    let bodies: Uint8Array[]
    try {
      bodies = this.messages.push(chunk)
    } catch {
      return this.close()
    }
    for (const body of bodies) {
      let message: ToView
      try {
        message = decodeToView(body)
      } catch {
        // Not the protocol: where the next message starts can no longer be trusted either
        return this.close()
      }
      this.channels.get(message.channel)?.receive(message)
    }
  }
}

/**
 * One session, as a view sees it.
 *
 * **A channel is used for one attachment and never again.** The broker goes on sending on a channel until
 * it has read the detach, so a frame can be on its way when a view stops listening. Were the next
 * attachment on the same channel, that frame — a change from a frame this view no longer holds — would
 * arrive looking like the first of the new one. On a channel of its own it is addressed to nobody.
 */
class SessionSource implements TerminalSource {
  private listener: SourceListener | null = null
  private ended = false
  /** The channel of the attachment there is now, or `null` with none */
  private channel: number | null = null
  /** The epoch of the last frame, which is the one every line number a view holds belongs to */
  private epoch = 0
  private nextRequest = 1
  private readonly waiting = new Map<number, { channel: number; resolve: (answer: Answer) => void; reject: (error: Error) => void }>()

  constructor(private readonly session: string, private readonly connection: Connection) {}

  attach(listener: SourceListener): () => void {
    this.detach()
    this.listener = listener
    if (this.ended) {
      listener.ended()
      return () => {}
    }
    this.channel = this.connection.channel(this)
    this.connection.send({ kind: 'attach', channel: this.channel, session: this.session })
    return () => {
      if (this.listener === listener) this.detach()
    }
  }

  /** Stops the frames of the attachment there is, and lets its channel go once nothing is owed on it */
  private detach(): void {
    const channel = this.channel
    this.listener = null
    this.channel = null
    if (channel === null) return
    this.connection.send({ kind: 'detach', channel })
    this.settle(channel)
  }

  /** Lets go of a channel no view is attached on once every question asked on it has its answer */
  private settle(channel: number): void {
    if (channel === this.channel) return
    for (const waiting of this.waiting.values()) if (waiting.channel === channel) return
    this.connection.release(channel)
  }

  receive(message: ToView): void {
    switch (message.kind) {
      case 'frame':
        // One for an attachment that is over describes a change from a frame nobody holds
        if (message.channel !== this.channel) break
        this.epoch = message.frame.epoch
        this.listener?.frame(message.frame)
        break
      case 'ended':
        if (message.channel === this.channel) this.end()
        break
      default: {
        // Requests are numbered from one, so a failure of request zero answers the one thing a view sends
        // that has no number: the attach
        if (message.kind === 'failed' && message.request === 0) {
          if (message.channel === this.channel) this.refused(message.message)
          break
        }
        const waiting = this.waiting.get(message.request)
        if (!waiting) break
        this.waiting.delete(message.request)
        this.settle(waiting.channel)
        if (message.kind === 'failed') waiting.reject(new Unanswered(message.message))
        else waiting.resolve(message)
      }
    }
  }

  /**
   * The broker could not show the session to this attachment, and said why. The attachment is over — its
   * view is told so, with the reason, and what it was asking has no answer — and the session is not: it
   * is still there, and attaching again asks again. The broker is told the channel is finished with, since
   * one that failed part-way through — it could not build a frame — is still holding it.
   */
  private refused(reason: string): void {
    const channel = this.channel
    if (channel === null) return
    this.channel = null
    for (const [request, waiting] of this.waiting) {
      if (waiting.channel !== channel) continue
      this.waiting.delete(request)
      waiting.reject(new Unanswered(reason))
    }
    this.connection.send({ kind: 'detach', channel })
    this.connection.release(channel)
    this.listener?.ended(reason)
  }

  /** The session is over, or the connection to it is: whoever is waiting is told, once */
  end(): void {
    if (this.ended) return
    this.ended = true
    const waiting = [...this.waiting.values()]
    this.waiting.clear()
    for (const channel of new Set([this.channel, ...waiting.map((one) => one.channel)])) {
      if (channel !== null) this.connection.release(channel)
    }
    this.channel = null
    this.connection.forget(this)
    for (const one of waiting) one.reject(new Unanswered('the session ended'))
    this.listener?.ended()
  }

  /**
   * What a view says that is not a question: sent on the channel it is attached on, and dropped where
   * there is none — before a view attaches, after it detaches, and once the session has ended.
   */
  private tell(build: (channel: number) => ToBroker): void {
    if (this.channel !== null) this.connection.send(build(this.channel))
  }

  /**
   * Sends a request and waits for the message that answers it, which is the one carrying its number. With
   * no view attached there is no channel to ask on, so there is no answer.
   */
  private ask<K extends Answer['kind']>(kind: K, build: (channel: number, request: number) => ToBroker): Promise<Extract<Answer, { kind: K }>> {
    if (this.ended) return Promise.reject(new Unanswered('the session ended'))
    const channel = this.channel
    if (channel === null) return Promise.reject(new Unanswered('no view is attached to the session'))
    const request = this.nextRequest
    this.nextRequest += 1
    return new Promise((resolve, reject) => {
      this.waiting.set(request, {
        channel,
        resolve: (answer) => {
          if (answer.kind === kind) resolve(answer as Extract<Answer, { kind: K }>)
          else reject(new Unanswered(`asked for ${kind} and was answered ${answer.kind}`))
        },
        reject,
      })
      try {
        this.connection.send(build(channel, request))
      } catch (error) {
        // What could not be written was not asked, so nothing will answer it
        this.waiting.delete(request)
        throw error
      }
    })
  }

  acknowledge(sequence: number): void {
    this.tell((channel) => ({ kind: 'acknowledge', channel, sequence }))
  }

  /**
   * Asked in the epoch of the last frame, which is the one the view's line numbers are in. The broker
   * refuses a question about an epoch that is over, and that refusal is the rejection a view takes as no
   * answer.
   */
  async fetch(first: number, count: number): Promise<Fetched> {
    const { epoch } = this
    const answer = await this.ask('rows', (channel, request) => ({ kind: 'fetch', channel, request, epoch, first, count }))
    return { rows: answer.rows.rows, epoch: answer.epoch, first: answer.first, count: answer.count }
  }

  key(input: KeyInput): void {
    this.tell((channel) => ({ kind: 'key', channel, ...input }))
  }

  text(text: string): void {
    if (text !== '') this.tell((channel) => ({ kind: 'text', channel, text }))
  }

  pointer(input: PointerInput): void {
    this.tell((channel) => ({ kind: 'pointer', channel, ...input }))
  }

  async paste(text: string, options?: { readonly allowUnsafe?: boolean }): Promise<PasteOutcome> {
    const allowUnsafe = options?.allowUnsafe ?? false
    return (await this.ask('pasted', (channel, request) => ({ kind: 'paste', channel, request, allowUnsafe, text }))).outcome
  }

  focus(gained: boolean): void {
    this.tell((channel) => ({ kind: 'focus', channel, gained }))
  }

  wordAt(position: Position): Promise<Span | null> {
    return this.spanAt('wordAt', position)
  }

  lineAt(position: Position): Promise<Span | null> {
    return this.spanAt('lineAt', position)
  }

  /**
   * An answer in another epoch names lines that are not the ones the view asked about — the screen was
   * reflowed while the question was on its way — so it is no answer.
   */
  private async spanAt(kind: 'wordAt' | 'lineAt', at: Position): Promise<Span | null> {
    const { epoch } = this
    const answer = await this.ask('span', (channel, request) => ({ kind, channel, request, epoch, at }))
    return answer.epoch === this.epoch ? answer.span : null
  }

  /**
   * Text read in another epoch than the view is in is text from other lines than the ones it selected —
   * the screen was reflowed while the answer was on its way — so it is no answer.
   */
  async textOf(span: Span): Promise<string> {
    const { epoch } = this
    const answer = await this.ask('text', (channel, request) => ({ kind: 'textOf', channel, request, epoch, span }))
    if (answer.epoch !== this.epoch) throw new Unanswered('the lines were numbered again while their text was on its way')
    return answer.text
  }

  async search(needle: string, caseSensitive: boolean): Promise<readonly Span[]> {
    const answer = await this.ask('spans', (channel, request) => ({ kind: 'search', channel, request, needle, caseSensitive }))
    return answer.epoch === this.epoch ? answer.spans : []
  }

  /**
   * What is known of each command is true in any epoch; the lines it is on are not, so an answer from
   * before a reflow keeps its commands and loses their lines.
   */
  async commands(): Promise<readonly Command[]> {
    const answer = await this.ask('commands', (channel, request) => ({ kind: 'commands', channel, request }))
    if (answer.epoch === this.epoch) return answer.commands
    return answer.commands.map((command) => ({ ...command, prompt: null, output: null, end: null }))
  }

  /** An answer from before a reflow names lines that are no longer where the images are: it is none */
  async images(): Promise<Placements> {
    const answer = await this.ask('images', (channel, request) => ({ kind: 'images', channel, request }))
    const { cellWidth, cellHeight } = answer
    return { cellWidth, cellHeight, placements: answer.epoch === this.epoch ? answer.placements : [] }
  }

  /**
   * An image arrives in parts, each asked for from where the last ended. A part of another generation is
   * a different picture — the program replaced it between two answers — so the parts so far are dropped
   * and it is read again from its start.
   */
  async image(id: number): Promise<Picture | null> {
    let whole: Uint8Array | null = null
    let generation = -1
    let read = 0
    for (;;) {
      const offset = read
      let part
      try {
        part = await this.ask('image', (channel, request) => ({ kind: 'image', channel, request, image: id, offset }))
      } catch (error) {
        if (error instanceof Unanswered) return null
        throw error
      }
      if (whole && (part.generation !== generation || part.total !== whole.length)) {
        whole = null
        read = 0
        continue
      }
      generation = part.generation
      whole ??= new Uint8Array(part.total)
      whole.set(part.bytes.subarray(0, Math.max(0, whole.length - part.offset)), part.offset)
      read = part.offset + part.bytes.length
      // An empty part of a picture not yet whole would be asked for again for ever
      if (read >= whole.length || part.bytes.length === 0) {
        return { generation, width: part.width, height: part.height, pixels: rgbaOf(part.channels, whole, part.width * part.height) }
      }
    }
  }
}

/** Pixels of one to four bytes each — grey, grey and alpha, colour, colour and alpha — as colour and alpha */
export function rgbaOf(channels: number, bytes: Uint8Array, pixels: number): Uint8ClampedArray {
  const rgba = new Uint8ClampedArray(pixels * 4)
  if (channels === 4) {
    rgba.set(bytes.subarray(0, rgba.length))
    return rgba
  }
  const count = Math.min(pixels, Math.floor(bytes.length / channels))
  for (let pixel = 0; pixel < count; pixel += 1) {
    const from = pixel * channels
    const to = pixel * 4
    const grey = channels < 3
    rgba[to] = bytes[from]!
    rgba[to + 1] = grey ? bytes[from]! : bytes[from + 1]!
    rgba[to + 2] = grey ? bytes[from]! : bytes[from + 2]!
    rgba[to + 3] = channels === 2 ? bytes[from + 1]! : 255
  }
  return rgba
}
