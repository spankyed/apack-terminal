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
  KeyInput,
  PasteOutcome,
  PointerInput,
  Position,
  Rows,
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

/** One connection to a broker, and the sessions drawn over it */
export class BrokerViews {
  private readonly messages = new Messages()
  private readonly sessions = new Map<number, SessionSource>()
  private nextChannel = 1
  private closed = false
  private readonly stop: Array<() => void>

  constructor(private readonly stream: ByteStream) {
    this.stop = [stream.onData((chunk) => this.receive(chunk)), stream.onClose(() => this.close())]
  }

  /** A source for the session the broker calls `session`. Nothing is sent until a view attaches to it */
  open(session: string): TerminalSource {
    const channel = this.nextChannel
    this.nextChannel += 1
    const source = new SessionSource(session, channel, (message) => this.send(message), () => this.sessions.delete(channel))
    this.sessions.set(channel, source)
    if (this.closed) source.end()
    return source
  }

  /** Stops listening to the stream. Every session is told it has ended */
  close(): void {
    if (this.closed) return
    this.closed = true
    for (const stop of this.stop) stop()
    for (const session of [...this.sessions.values()]) session.end()
    this.sessions.clear()
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
      this.sessions.get(message.channel)?.receive(message)
    }
  }
}

/** One session, as a view sees it */
class SessionSource implements TerminalSource {
  private listener: SourceListener | null = null
  private ended = false
  /** The epoch of the last frame, which is the one every line number a view holds belongs to */
  private epoch = 0
  private nextRequest = 1
  private readonly waiting = new Map<number, { resolve: (answer: Answer) => void; reject: (error: Error) => void }>()

  constructor(
    private readonly session: string,
    private readonly channel: number,
    private readonly send: (message: ToBroker) => void,
    private readonly forget: () => void,
  ) {}

  attach(listener: SourceListener): () => void {
    this.listener = listener
    if (this.ended) {
      listener.ended()
      return () => {}
    }
    this.send({ kind: 'attach', channel: this.channel, session: this.session })
    return () => {
      if (this.listener !== listener) return
      this.listener = null
      if (!this.ended) this.send({ kind: 'detach', channel: this.channel })
    }
  }

  receive(message: ToView): void {
    switch (message.kind) {
      case 'frame':
        this.epoch = message.frame.epoch
        this.listener?.frame(message.frame)
        break
      case 'ended':
        this.end()
        break
      default: {
        const waiting = this.waiting.get(message.request)
        this.waiting.delete(message.request)
        if (message.kind === 'failed') waiting?.reject(new Unanswered(message.message))
        else waiting?.resolve(message)
      }
    }
  }

  /** The session is over, or the connection to it is: whoever is waiting is told, once */
  end(): void {
    if (this.ended) return
    this.ended = true
    for (const waiting of this.waiting.values()) waiting.reject(new Unanswered('the session ended'))
    this.waiting.clear()
    this.forget()
    this.listener?.ended()
  }

  /** Sends a request and waits for the message that answers it, which is the one carrying its number */
  private ask<K extends Answer['kind']>(kind: K, build: (request: number) => ToBroker): Promise<Extract<Answer, { kind: K }>> {
    if (this.ended) return Promise.reject(new Unanswered('the session ended'))
    const request = this.nextRequest
    this.nextRequest += 1
    return new Promise((resolve, reject) => {
      this.waiting.set(request, {
        resolve: (answer) => {
          if (answer.kind === kind) resolve(answer as Extract<Answer, { kind: K }>)
          else reject(new Unanswered(`asked for ${kind} and was answered ${answer.kind}`))
        },
        reject,
      })
      this.send(build(request))
    })
  }

  acknowledge(sequence: number): void {
    this.send({ kind: 'acknowledge', channel: this.channel, sequence })
  }

  async fetch(first: number, count: number): Promise<Rows & { readonly epoch: number }> {
    const { channel, epoch } = this
    const answer = await this.ask('rows', (request) => ({ kind: 'fetch', channel, request, epoch, first, count }))
    return { ...answer.rows, epoch: answer.epoch }
  }

  key(input: KeyInput): void {
    this.send({ kind: 'key', channel: this.channel, ...input })
  }

  text(text: string): void {
    if (text !== '') this.send({ kind: 'text', channel: this.channel, text })
  }

  pointer(input: PointerInput): void {
    this.send({ kind: 'pointer', channel: this.channel, ...input })
  }

  async paste(text: string, options?: { readonly allowUnsafe?: boolean }): Promise<PasteOutcome> {
    const { channel } = this
    const allowUnsafe = options?.allowUnsafe ?? false
    return (await this.ask('pasted', (request) => ({ kind: 'paste', channel, request, allowUnsafe, text }))).outcome
  }

  focus(gained: boolean): void {
    this.send({ kind: 'focus', channel: this.channel, gained })
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
    const { channel, epoch } = this
    const answer = await this.ask('span', (request) => ({ kind, channel, request, epoch, at }))
    return answer.epoch === this.epoch ? answer.span : null
  }

  async textOf(span: Span): Promise<string> {
    const { channel, epoch } = this
    return (await this.ask('text', (request) => ({ kind: 'textOf', channel, request, epoch, span }))).text
  }

  async search(needle: string): Promise<readonly Span[]> {
    const { channel } = this
    const answer = await this.ask('spans', (request) => ({ kind: 'search', channel, request, needle }))
    return answer.epoch === this.epoch ? answer.spans : []
  }

  /**
   * What is known of each command is true in any epoch; the lines it is on are not, so an answer from
   * before a reflow keeps its commands and loses their lines.
   */
  async commands(): Promise<readonly Command[]> {
    const { channel } = this
    const answer = await this.ask('commands', (request) => ({ kind: 'commands', channel, request }))
    if (answer.epoch === this.epoch) return answer.commands
    return answer.commands.map((command) => ({ ...command, prompt: null, output: null, end: null }))
  }
}
