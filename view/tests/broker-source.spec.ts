// Sessions on a broker, as a view sees them: frames routed to the view that attached, questions matched to
// their answers, and everyone told when a session or the connection ends.
import { describe, expect, it } from 'vitest'
import { BrokerViews, MAX_MESSAGE_BYTES, TooLarge, Unanswered, type Command, type Frame } from '../src/index.ts'
import { FakeBroker } from './_broker.ts'
import { frame, row } from './_support.ts'

function attached(session = 's-1') {
  const broker = new FakeBroker()
  const views = new BrokerViews(broker)
  const source = views.open(session)
  const frames: Frame[] = []
  let ended = 0
  const detach = source.attach({ frame: (next) => { frames.push(next) }, ended: () => { ended += 1 } })
  return { broker, views, source, frames, detach, ended: () => ended }
}

describe('a session on a broker', () => {
  it('asks for nothing until a view attaches, and then for that session on a channel of its own', () => {
    const broker = new FakeBroker()
    const views = new BrokerViews(broker)
    const one = views.open('s-1')
    const two = views.open('s-2')
    expect(broker.received).toEqual([])

    two.attach({ frame() {}, ended() {} })
    one.attach({ frame() {}, ended() {} })
    expect(broker.taken()).toEqual([
      { kind: 'attach', channel: 1, session: 's-2' },
      { kind: 'attach', channel: 2, session: 's-1' },
    ])
  })

  it('attaches again on a channel never used, so a frame still on its way to the last attachment reaches nobody', () => {
    const { broker, source, detach, frames } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ sequence: 1, rows: [row(0, 'first')] }) })
    detach()
    const again: Frame[] = []
    source.attach({ frame: (next) => { again.push(next) }, ended() {} })
    expect(broker.taken()).toEqual([
      { kind: 'attach', channel: 1, session: 's-1' },
      { kind: 'detach', channel: 1 },
      { kind: 'attach', channel: 2, session: 's-1' },
    ])

    // Sent before the broker read the detach: a change from a frame the view no longer holds
    broker.say({ kind: 'frame', channel: 1, frame: frame({ sequence: 2, rows: [row(1, 'a change')] }) })
    expect(again, 'a frame for the attachment before was taken as one of this').toEqual([])

    broker.say({ kind: 'frame', channel: 2, frame: frame({ sequence: 1, rows: [row(0, 'whole')] }) })
    expect(again.map((next) => next.rows[0]?.text.join(''))).toEqual(['whole'])
    expect(frames).toHaveLength(1)
    source.acknowledge(1)
    expect(broker.taken()).toEqual([{ kind: 'acknowledge', channel: 2, sequence: 1 }])
  })

  it('hands each frame to the view of the session it is for', () => {
    const broker = new FakeBroker()
    const views = new BrokerViews(broker)
    const heard: string[] = []
    views.open('s-1').attach({ frame: (next) => { heard.push(`one:${next.sequence}`) }, ended() {} })
    views.open('s-2').attach({ frame: (next) => { heard.push(`two:${next.sequence}`) }, ended() {} })

    broker.say(
      { kind: 'frame', channel: 2, frame: frame({ sequence: 5, rows: [row(0, 'two')] }) },
      { kind: 'frame', channel: 1, frame: frame({ sequence: 9 }) },
      { kind: 'frame', channel: 7, frame: frame({ sequence: 1 }) },
    )
    expect(heard).toEqual(['two:5', 'one:9'])
  })

  it('carries a frame across the stream exactly', () => {
    const { broker, frames } = attached()
    const sent = frame({
      sequence: 3, epoch: 4, firstActive: 100, scrollback: 100, staleFrom: 90,
      cursor: { column: 2, row: 1, shape: 'bar', blinking: true },
      modes: { alternateScreen: true, mouse: true },
      rows: [row(100, 'hello', { wrapped: true }), row(101, '$ ', { prompt: true })],
    })
    broker.say({ kind: 'frame', channel: 1, frame: sent })
    expect(frames).toEqual([sent])
  })

  it('sends a view\'s acknowledgement and input on its channel', () => {
    const { broker, source } = attached()
    broker.taken()
    source.acknowledge(12)
    source.key({ action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshifted: 97, composed: false })
    source.text('日本')
    source.text('')
    source.pointer({ action: 'wheelUp', button: -1, modifiers: 0, x: 4, y: 8, cellWidth: 8, cellHeight: 16 })
    source.focus(false)
    expect(broker.taken()).toEqual([
      { kind: 'acknowledge', channel: 1, sequence: 12 },
      { kind: 'key', channel: 1, action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshifted: 97, composed: false },
      { kind: 'text', channel: 1, text: '日本' },
      { kind: 'pointer', channel: 1, action: 'wheelUp', button: -1, modifiers: 0, x: 4, y: 8, cellWidth: 8, cellHeight: 16 },
      { kind: 'focus', channel: 1, gained: false },
    ])
  })

  it('stops the frames when the view detaches', () => {
    const { broker, detach, frames } = attached()
    broker.taken()
    detach()
    detach()
    expect(broker.taken()).toEqual([{ kind: 'detach', channel: 1 }])
    broker.say({ kind: 'frame', channel: 1, frame: frame({}) })
    expect(frames).toEqual([])
  })
})

describe('a question a view asks', () => {
  it('is answered by the message carrying its number, in whatever order answers come', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    broker.taken()

    const fetched = source.fetch(40, 2)
    const word = source.wordAt({ line: 41, column: 3 })
    const text = source.textOf({ start: { line: 40, column: 0 }, end: { line: 41, column: 5 } })
    expect(broker.taken()).toEqual([
      { kind: 'fetch', channel: 1, request: 1, epoch: 6, first: 40, count: 2 },
      { kind: 'wordAt', channel: 1, request: 2, epoch: 6, at: { line: 41, column: 3 } },
      { kind: 'textOf', channel: 1, request: 3, epoch: 6, span: { start: { line: 40, column: 0 }, end: { line: 41, column: 5 } } },
    ])

    const span = { start: { line: 41, column: 0 }, end: { line: 41, column: 4 } }
    broker.say(
      { kind: 'text', channel: 1, request: 3, epoch: 6, text: 'forty\nforty-one' },
      { kind: 'span', channel: 1, request: 2, epoch: 6, span },
      { kind: 'rows', channel: 1, request: 1, epoch: 6, first: 40, count: 2, rows: { rows: [row(40, 'forty')] } },
    )
    expect(await text).toBe('forty\nforty-one')
    expect(await word).toEqual(span)
    expect(await fetched).toEqual({ epoch: 6, first: 40, count: 2, rows: [row(40, 'forty')] })
  })

  it('has no answer when the screen was reflowed while it was on its way', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    const word = source.wordAt({ line: 41, column: 3 })
    const found = source.search('needle', false)
    // The answers name lines of an epoch the view has since been told is over
    broker.say(
      { kind: 'frame', channel: 1, frame: frame({ epoch: 7 }) },
      { kind: 'span', channel: 1, request: 1, epoch: 6, span: { start: { line: 41, column: 0 }, end: { line: 41, column: 4 } } },
      { kind: 'spans', channel: 1, request: 2, epoch: 6, spans: [{ start: { line: 1, column: 0 }, end: { line: 1, column: 5 } }] },
    )
    expect(await word).toBeNull()
    expect(await found).toEqual([])
  })

  it('says whether a paste went or was refused, and asks again when the person allows it', async () => {
    const { broker, source } = attached()
    broker.taken()
    const first = source.paste('rm -rf x\n')
    broker.say({ kind: 'pasted', channel: 1, request: 1, outcome: 'unsafe' })
    expect(await first).toBe('unsafe')

    const second = source.paste('rm -rf x\n', { allowUnsafe: true })
    broker.say({ kind: 'pasted', channel: 1, request: 2, outcome: 'pasted' })
    expect(await second).toBe('pasted')
    expect(broker.taken()).toEqual([
      { kind: 'paste', channel: 1, request: 1, allowUnsafe: false, text: 'rm -rf x\n' },
      { kind: 'paste', channel: 1, request: 2, allowUnsafe: true, text: 'rm -rf x\n' },
    ])
  })

  it('is still answered when the view detached while it was on its way', async () => {
    const { broker, source, detach } = attached()
    const text = source.textOf({ start: { line: 0, column: 0 }, end: { line: 0, column: 4 } })
    detach()
    source.attach({ frame() {}, ended() {} })
    broker.say({ kind: 'text', channel: 1, request: 1, epoch: 0, text: 'asked before' })
    expect(await text).toBe('asked before')
  })

  it('has no answer, and is not sent, while no view is attached', async () => {
    const broker = new FakeBroker()
    const source = new BrokerViews(broker).open('s-1')
    await expect(source.fetch(0, 1)).rejects.toBeInstanceOf(Unanswered)
    source.key({ action: 'press', code: 'KeyA', modifiers: 0, text: 'a', unshifted: 97, composed: false })
    expect(broker.received).toEqual([])
  })

  it('is refused, and the connection kept, when it is a paste too large for one message', async () => {
    const { broker, source, ended } = attached()
    broker.taken()
    await expect(source.paste('x'.repeat(MAX_MESSAGE_BYTES + 1))).rejects.toBeInstanceOf(TooLarge)
    expect(broker.taken(), 'some of it was sent').toEqual([])
    expect(ended()).toBe(0)

    // And the next question is asked and answered as any other
    const pasted = source.paste('small')
    broker.say({ kind: 'pasted', channel: 1, request: 2, outcome: 'pasted' })
    expect(await pasted).toBe('pasted')
  })

  it('says which lines an answer to a fetch covers, which may be fewer than were asked about', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    const fetched = source.fetch(0, 5000)
    broker.say({ kind: 'rows', channel: 1, request: 1, epoch: 6, first: 0, count: 2000, rows: { rows: [row(7, 'seven')] } })
    expect(await fetched).toEqual({ epoch: 6, first: 0, count: 2000, rows: [row(7, 'seven')] })
  })

  it('has no text for an answer read in another numbering than the view is in', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    const text = source.textOf({ start: { line: 40, column: 0 }, end: { line: 41, column: 5 } })
    broker.say(
      { kind: 'frame', channel: 1, frame: frame({ epoch: 7 }) },
      { kind: 'text', channel: 1, request: 1, epoch: 6, text: 'from lines that are other lines by now' },
    )
    await expect(text).rejects.toBeInstanceOf(Unanswered)
  })

  it('names the epoch of the last frame, and has no answer when the broker refuses it as one that is over', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) }, { kind: 'frame', channel: 1, frame: frame({ sequence: 2, epoch: 9 }) })
    broker.taken()
    const asked = [source.fetch(0, 1), source.wordAt({ line: 1, column: 1 }), source.lineAt({ line: 1, column: 1 }), source.textOf({ start: { line: 0, column: 0 }, end: { line: 0, column: 1 } })]
    expect(broker.taken().map((sent) => 'epoch' in sent && sent.epoch)).toEqual([9, 9, 9, 9])
    for (const request of [1, 2, 3, 4]) broker.say({ kind: 'failed', channel: 1, request, message: 'the lines have been numbered again' })
    for (const answer of asked) await expect(answer).rejects.toBeInstanceOf(Unanswered)
  })

  it('fails with what the broker said when the broker could not carry it out', async () => {
    const { broker, source } = attached()
    const fetched = source.fetch(0, 1)
    broker.say({ kind: 'failed', channel: 1, request: 1, message: 'no session s-1' })
    await expect(fetched).rejects.toThrow(new Unanswered('no session s-1'))
  })
})

describe('an ending', () => {
  it('of the session tells its view once, and fails what was still being asked', async () => {
    const { broker, source, ended } = attached()
    const fetched = source.fetch(0, 1)
    broker.say({ kind: 'ended', channel: 1 }, { kind: 'ended', channel: 1 })
    expect(ended()).toBe(1)
    await expect(fetched).rejects.toBeInstanceOf(Unanswered)
    // And nothing more is asked of a session that is over
    await expect(source.fetch(0, 1)).rejects.toBeInstanceOf(Unanswered)
  })

  it('of the session is the last thing said on its channel: no acknowledgement or input follows it', () => {
    const { broker, source } = attached()
    broker.say({ kind: 'ended', channel: 1 })
    broker.taken()
    source.acknowledge(3)
    source.key({ action: 'press', code: 'KeyA', modifiers: 0, text: 'a', unshifted: 97, composed: false })
    source.text('typed')
    source.pointer({ action: 'press', button: 0, modifiers: 0, x: 1, y: 1, cellWidth: 8, cellHeight: 16 })
    source.focus(true)
    expect(broker.taken()).toEqual([])
  })

  it('of the connection reaches a session no view has attached to yet', () => {
    const broker = new FakeBroker()
    const source = new BrokerViews(broker).open('s-1')
    broker.hangUp()
    let ended = 0
    source.attach({ frame() {}, ended: () => { ended += 1 } })
    expect(ended).toBe(1)
  })

  it('of an attachment the broker could not make tells its view why, and leaves the session to be attached again', async () => {
    const { broker, source, frames } = attached()
    const reasons: Array<string | undefined> = []
    const second = source.attach({ frame: (next) => { frames.push(next) }, ended: (reason) => { reasons.push(reason) } })
    const asked = source.fetch(0, 1)
    broker.taken()
    // Answered as request zero, since an attach has no number of its own
    broker.say({ kind: 'failed', channel: 2, request: 0, message: 'ghostty_render_state_new failed with -1' })
    expect(reasons).toEqual(['ghostty_render_state_new failed with -1'])
    await expect(asked).rejects.toBeInstanceOf(Unanswered)
    source.acknowledge(1)
    // The broker is told the channel is finished with — it may have attached it and failed later — and nothing else
    expect(broker.taken(), 'something was said on the channel of an attachment that is over').toEqual([{ kind: 'detach', channel: 2 }])
    broker.say({ kind: 'frame', channel: 2, frame: frame({ sequence: 2 }) })
    expect(frames, 'a frame on the channel of an attachment that was refused was taken').toEqual([])
    second()

    source.attach({ frame: (next) => { frames.push(next) }, ended: (reason) => { reasons.push(reason) } })
    expect(broker.taken()).toEqual([{ kind: 'attach', channel: 3, session: 's-1' }])
    broker.say({ kind: 'frame', channel: 3, frame: frame({ sequence: 4 }) })
    expect(frames.map((next) => next.sequence)).toEqual([4])
    expect(reasons).toHaveLength(1)
  })

  it('of the connection ends every session on it', () => {
    const broker = new FakeBroker()
    const views = new BrokerViews(broker)
    let ended = 0
    for (const session of ['s-1', 's-2']) views.open(session).attach({ frame() {}, ended: () => { ended += 1 } })
    broker.hangUp()
    expect(ended).toBe(2)

    // A session opened afterwards is one that has already ended
    views.open('s-3').attach({ frame() {}, ended: () => { ended += 1 } })
    expect(ended).toBe(3)
    broker.taken()
    views.open('s-4').acknowledge(1)
    expect(broker.taken()).toEqual([])
  })

  it('comes when the broker sends what is not the protocol, since nothing after it can be trusted', () => {
    const { broker, ended, frames } = attached()
    broker.raw(Uint8Array.of(2, 0, 0, 0, 250, 1))
    expect(ended()).toBe(1)
    broker.say({ kind: 'frame', channel: 1, frame: frame({}) })
    expect(frames).toEqual([])
  })

  it('of the connection is told to whoever holds it, once, whether it was closed or stopped on its own', () => {
    const stopped = attached()
    let heard = 0
    stopped.views.onClose(() => { heard += 1 })
    // Bytes that are not the protocol stop it without its stream having closed
    stopped.broker.raw(Uint8Array.of(2, 0, 0, 0, 250, 1))
    stopped.views.close()
    expect(heard).toBe(1)

    const closed = attached()
    let told = 0
    const unlisten = closed.views.onClose(() => { told += 1 })
    unlisten()
    closed.views.close()
    expect(told, 'a listener that stopped listening was told').toBe(0)
  })
})

describe('the commands of a session', () => {
  const ran: Command = { id: 4, command: 'make', cwd: '/src', startedMs: 10, finishedMs: 20, outcome: { kind: 'exited', code: -2 }, prompt: 7, output: 8, end: 12 }

  it('are answered with the lines each is on', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    broker.taken()
    const asked = source.commands()
    expect(broker.taken()).toEqual([{ kind: 'commands', channel: 1, request: 1 }])
    broker.say({ kind: 'commands', channel: 1, request: 1, epoch: 6, commands: [ran] })
    expect(await asked).toEqual([ran])
  })

  it('keep what is known of them and lose their lines when the answer is from before a reflow', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    const asked = source.commands()
    broker.say(
      { kind: 'frame', channel: 1, frame: frame({ epoch: 7 }) },
      { kind: 'commands', channel: 1, request: 1, epoch: 6, commands: [ran] },
    )
    expect(await asked).toEqual([{ ...ran, prompt: null, output: null, end: null }])
  })
})
