// Sessions on a broker, as a view sees them: frames routed to the view that attached, questions matched to
// their answers, and everyone told when a session or the connection ends.
import { describe, expect, it } from 'vitest'
import { BrokerViews, Unanswered, type Frame } from '../src/index.ts'
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
      { kind: 'attach', channel: 2, session: 's-2' },
      { kind: 'attach', channel: 1, session: 's-1' },
    ])
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
    source.key({ action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshifted: 97 })
    source.text('日本')
    source.text('')
    source.pointer({ action: 'wheelUp', button: -1, modifiers: 0, x: 4, y: 8, cellWidth: 8, cellHeight: 16 })
    source.focus(false)
    expect(broker.taken()).toEqual([
      { kind: 'acknowledge', channel: 1, sequence: 12 },
      { kind: 'key', channel: 1, action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshifted: 97 },
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
      { kind: 'text', channel: 1, request: 3, text: 'forty\nforty-one' },
      { kind: 'span', channel: 1, request: 2, epoch: 6, span },
      { kind: 'rows', channel: 1, request: 1, epoch: 6, rows: { styles: frame({}).styles, links: [''], rows: [row(40, 'forty')] } },
    )
    expect(await text).toBe('forty\nforty-one')
    expect(await word).toEqual(span)
    expect(await fetched).toEqual({ epoch: 6, styles: frame({}).styles, links: [''], rows: [row(40, 'forty')] })
  })

  it('has no answer when the screen was reflowed while it was on its way', async () => {
    const { broker, source } = attached()
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    const word = source.wordAt({ line: 41, column: 3 })
    const found = source.search('needle')
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
})
