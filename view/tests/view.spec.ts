// TerminalView v2 lifecycle: ViewportHint, setVisible(false) pause, incremental SearchWatch,
// blinking cursor/text on TestClock, CommandRecord gutter marks & jumping, and numeric cache keys.
import { beforeEach, describe, expect, it } from 'vitest'
import { BLINK_MS, PackedStyleFlag, type CommandRecord } from '../src/index.ts'
import { TestClock } from './_clock.ts'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'

beforeEach(() => installFrames())

describe('TerminalView v2 core lifecycle', () => {
  it('sends declarative ViewportHint on fit/focus/visibility and pauses paints when hidden', () => {
    const source = new FakeSource()
    const { view, host, painted, painter } = mountedView({}, source)
    Object.defineProperty(host, 'clientWidth', { value: 200, configurable: true })
    Object.defineProperty(host, 'clientHeight', { value: 100, configurable: true })
    view.fit()

    expect(source.hints.at(-1)).toEqual({
      size: { cols: 20, rows: 5 },
      cellPx: { width: 10, height: 20 },
      visible: true,
      focused: false,
    })

    // Hiding sends visible: false (which makes BrokerSessionSource send maxFrames = 0)
    view.setVisible(false)
    expect(source.hints.at(-1)?.visible).toBe(false)

    // Frames arriving while hidden do not schedule canvas paints
    const paintsBefore = painter.scenes.length
    source.send(frame({ seq: 1, size: { cols: 20, rows: 5 }, rows: [row(0, 'hidden')] }))
    runFrames()
    expect(painter.scenes.length).toBe(paintsBefore)

    // Making visible again sends visible: true and paints immediately
    view.setVisible(true)
    expect(source.hints.at(-1)?.visible).toBe(true)
    const scene = painted()
    expect(scene.rows[0]!.glyphs[0]!.text).toBe('hidden')
    expect(source.acknowledged).toContain(1)
  })

  it('drives search incrementally via setSearch + SearchDelta without polling timers', async () => {
    const source = new FakeSource()
    const foundUpdates: Array<{ current: number; total: number; more: boolean }> = []
    source.searchResult = {
      epoch: 1,
      matches: [
        { start: { line: 0, col: 0 }, end: { line: 0, col: 2 } },
        { start: { line: 2, col: 4 }, end: { line: 2, col: 6 } },
      ],
      truncated: false,
    }
    const { view, painted } = mountedView({ onFound: (f) => foundUpdates.push(f) }, source)
    source.send(
      frame({
        seq: 1,
        size: { cols: 20, rows: 4 },
        rows: [row(0, 'foo one'), row(1, 'bar'), row(2, 'baz foo'), row(3, 'qux')],
      }),
    )
    painted()

    const count = await view.find('foo')
    expect(count).toBe(2)
    expect(source.searches).toEqual([{ query: 'foo', caseSensitive: false }])
    // Newest match first -> line 2 is match 1 of 2
    expect(view.found).toEqual({ current: 1, total: 2, more: false })

    // Step to older match -> line 0
    view.findNext(1)
    expect(view.found).toEqual({ current: 2, total: 2, more: false })

    // Push incremental SearchDelta replacing line 2..3 with two new matches (zero extra setSearch RPCs!)
    source.pushSearchDelta({
      epoch: 1,
      fromLine: 2,
      toLine: 4,
      added: [
        { start: { line: 2, col: 0 }, end: { line: 2, col: 2 } },
        { start: { line: 3, col: 1 }, end: { line: 3, col: 3 } },
      ],
      truncated: false,
    })
    expect(view.found.total).toBe(3)
    expect(source.searches).toHaveLength(1)
    const scene = painted()
    // Row 0 still highlighted
    expect(scene.rows[0]!.backgrounds.length).toBeGreaterThan(0)

    // Epoch reset preserves active SearchWatch needle so broker's new-epoch SearchMatches + SearchDelta work!
    source.send(frame({ seq: 2, epoch: 2, size: { cols: 20, rows: 4 }, rows: [row(0, 'foo resized')] }))
    source.pushSearchMatches({
      epoch: 2,
      matches: [{ start: { line: 0, col: 0 }, end: { line: 0, col: 2 } }],
      truncated: false,
    })
    expect(view.found).toEqual({ current: 1, total: 1, more: false })
    source.pushSearchDelta({
      epoch: 2,
      fromLine: 1,
      toLine: 2,
      added: [{ start: { line: 1, col: 0 }, end: { line: 1, col: 2 } }],
      truncated: false,
    })
    expect(view.found.total).toBe(2)

    // Clearing search sends empty query once
    await view.find('')
    expect(view.found).toEqual({ current: 0, total: 0, more: false })
  })

  it('blinks focused cursor and blinking text on injected TestClock and preserves unchanged RowDrawing identity', () => {
    const clock = new TestClock()
    const { source, view, painted } = mountedView({ clock })
    view.setFocused(true)
    source.send(
      frame({
        seq: 1,
        size: { cols: 20, rows: 2 },
        cursor: { col: 1, row: 0, shape: 'bar', blinking: true, hidden: false },
        rows: [
          row(0, 'steady'),
          row(1, 'blink', { style: 1, hasBlink: true, styles: [{ flags: PackedStyleFlag.BLINK }] }),
        ],
      }),
    )
    const s1 = painted()
    expect(s1.cursor?.shape).toBe('bar')
    expect(s1.rows[1]!.glyphs).toHaveLength(1)

    // Advance BLINK_MS -> cursor hides and blinking row hides, while steady row keeps exact object identity!
    clock.advance(BLINK_MS)
    const s2 = painted()
    expect(s2.cursor).toBeUndefined()
    expect(s2.rows[1]!.glyphs).toHaveLength(0)
    expect(s2.rows[0]).toBe(s1.rows[0])

    // Unfocus shows cursorInactive shape ('hollow') without blinking
    view.setFocused(false)
    const s3 = painted()
    expect(s3.cursor?.shape).toBe('hollow')
    clock.advance(BLINK_MS * 3)
    expect(painted().cursor?.shape).toBe('hollow')
  })

  it('renders CommandRecord gutter marks from live events and jumps between prompts via scrollToCommand', async () => {
    const source = new FakeSource()
    const c1: CommandRecord = {
      id: 1,
      command: 'ok',
      cwd: '/tmp',
      startedMs: 100,
      finishedMs: 150,
      outcome: { kind: 'exited', code: 0 },
      promptLine: 0,
      outputLine: 1,
      endLine: 2,
    }
    const c2: CommandRecord = {
      id: 2,
      command: 'fail',
      cwd: '/tmp',
      startedMs: 200,
      finishedMs: 250,
      outcome: { kind: 'claimed', code: 0 },
      promptLine: 3,
      outputLine: 4,
      endLine: 5,
    }
    source.commandList = [c1]
    const { view, painted } = mountedView({ gutter: 6 }, source)
    source.send(
      frame({
        seq: 1,
        size: { cols: 20, rows: 4 },
        firstActiveLine: 4,
        scrollbackLines: 4,
        rows: [row(4, 'out'), row(5, 'next'), row(6, 'a'), row(7, 'b')],
      }),
    )
    await settled()
    source.pushEvent({ kind: 'commandFinished', record: c2 })

    // Jump back to prompt 3, then prompt 0
    view.scrollToCommand(-1)
    expect(view.viewport.top).toBe(3)
    const s = painted()
    // Claimed status always renders in yellow (palette[3]), never green
    expect(s.marks).toEqual([{ row: 0, color: s.theme.palette[3] }])

    view.scrollToCommand(-1)
    expect(view.viewport.top).toBe(0)
    expect(view.commandAt(1)?.id).toBe(1)
    expect(view.commandAt(4)?.id).toBe(2)

    view.dispose()
  })

  it('cancels and suppresses blink timers while setVisible(false) and fires onEnded at most once', () => {
    const clock = new TestClock()
    const endedReasons: Array<string | undefined> = []
    const { source, view, painted, painter } = mountedView(
      { clock, onEnded: (reason) => endedReasons.push(reason) },
      undefined,
    )
    view.setFocused(true)
    source.send(
      frame({
        seq: 1,
        size: { cols: 20, rows: 2 },
        cursor: { col: 0, row: 0, shape: 'bar', blinking: true, hidden: false },
        rows: [
          row(0, 'steady'),
          row(1, 'blink', { style: 1, hasBlink: true, styles: [{ flags: PackedStyleFlag.BLINK }] }),
        ],
      }),
    )
    painted()
    expect(clock.pending).toBeGreaterThan(0)

    // Advance to mid-blink-off so we also verify hiding resets both cursor & text blink back to visible!
    clock.advance(BLINK_MS)
    const midBlinkOff = painted()
    expect(midBlinkOff.cursor).toBeUndefined()
    expect(midBlinkOff.rows[1]!.glyphs).toHaveLength(0)

    // M12: hiding cancels both cursor and text blink timers immediately (clock.pending === 0),
    // and neither focus changes nor incoming frames re-arm blink timers while hidden!
    view.setVisible(false)
    expect(clock.pending).toBe(0)
    source.send(
      frame({
        seq: 2,
        size: { cols: 20, rows: 2 },
        cursor: { col: 1, row: 0, shape: 'bar', blinking: true, hidden: false },
        rows: [
          row(0, 'steady2'),
          row(1, 'blink2', { style: 1, hasBlink: true, styles: [{ flags: PackedStyleFlag.BLINK }] }),
        ],
      }),
    )
    view.setFocused(false)
    view.setFocused(true)
    const paintsWhileHidden = painter.scenes.length
    clock.advance(BLINK_MS * 5)
    expect(clock.pending).toBe(0)
    expect(painter.scenes.length).toBe(paintsWhileHidden)

    // Showing again resets both cursor and blinking text to visible, then resumes both blink timers cleanly
    view.setVisible(true)
    const sShown = painted()
    expect(sShown.cursor?.shape).toBe('bar')
    expect(sShown.rows[1]!.glyphs.length).toBeGreaterThan(0)
    clock.advance(BLINK_MS)
    const sBlinked = painted()
    expect(sBlinked.cursor).toBeUndefined()
    expect(sBlinked.rows[1]!.glyphs).toHaveLength(0)

    // M11: receiving both exited and streamClosed invokes onEnded at most once
    source.pushEvent({ kind: 'exited', exitCode: 0, signal: null })
    source.pushEvent({ kind: 'streamClosed', reason: 'session ended' })
    expect(endedReasons).toEqual(['exited (0)'])

    view.dispose()
  })
})
