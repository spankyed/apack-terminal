// Pointer, selection, wheel, mouse reporting, links, and zero getBoundingClientRect() on pointermove.
import { beforeEach, describe, expect, it } from 'vitest'
import { installFrames } from './_frames.ts'
import { frame, row } from './_support.ts'
import { mountedView } from './_view.ts'

beforeEach(() => installFrames())

describe('pointer, selection, wheel, links, and layout caching', () => {
  it('selects words/lines/blocks synchronously in 0 ms and copies on release when copyOnSelect is enabled', async () => {
    const copied: string[] = []
    const { source, view, painted, point } = mountedView({
      copyOnSelect: true,
      onCopy: (t) => copied.push(t),
    })
    source.send(
      frame({
        size: { cols: 20, rows: 3 },
        rows: [row(0, 'hello world'), row(1, 'second line'), row(2, 'third line')],
      }),
    )
    painted()

    // Double-click 'world' (col 7, row 0) -> 0 ms local selection + auto-copy on release
    point('pointerdown', 7, 0, { button: 0, at: 100 })
    point('pointerup', 7, 0, { button: 0, at: 150 })
    point('pointerdown', 7, 0, { button: 0, at: 200 })
    point('pointerup', 7, 0, { button: 0, at: 250 })
    expect(view.selection.span).toEqual({ start: { line: 0, col: 6 }, end: { line: 0, col: 10 } })
    expect(copied.at(-1)).toBe('world')
    expect(source.remoteWordCalls).toEqual([])
    expect(source.remoteTextCalls).toEqual([])

    // Alt-drag block selection
    point('pointerdown', 0, 0, { button: 0, altKey: true, at: 1000 })
    point('pointermove', 3, 1, { button: 0, altKey: true, at: 1050 })
    point('pointerup', 3, 1, { button: 0, altKey: true, at: 1100 })
    expect(view.selection.block).toEqual({ top: 0, bottom: 1, left: 0, right: 3 })
    expect(copied.at(-1)).toBe('hell\nseco')
  })

  it('calls zero getBoundingClientRect() on pointermove (uses ResizeObserver/pointerdown cached box)', () => {
    const { source, painter, painted, point } = mountedView()
    source.send(frame({ size: { cols: 20, rows: 3 }, rows: [row(0, 'https://example.com')] }))
    painted()

    let rectCalls = 0
    const orig = painter.canvas.getBoundingClientRect.bind(painter.canvas)
    painter.canvas.getBoundingClientRect = () => {
      rectCalls += 1
      return orig()
    }

    // Prime pointerdown once, then fire 50 pointermoves across the grid
    for (let i = 0; i < 50; i += 1) {
      point('pointermove', i % 20, i % 3)
    }
    expect(rectCalls).toBe(0)
  })

  it('hovers and Cmd-clicks links, and reports mouse events to programs when modes.mouse is true', () => {
    const opened: string[] = []
    const { source, view, painted, point } = mountedView({
      mac: true,
      linkPreview: 'all',
      onLink: (l) => opened.push(l.target),
    })
    source.send(frame({ size: { cols: 30, rows: 2 }, rows: [row(0, 'open https://apack.dev now')] }))
    painted()

    point('pointermove', 10, 0)
    expect(view.element.style.cursor).toBe('pointer')
    expect(view.element.title).toBe('https://apack.dev')

    point('pointerdown', 10, 0, { button: 0, metaKey: true, at: 1000 })
    point('pointerup', 10, 0, { button: 0, metaKey: true, at: 1050 })
    expect(opened).toEqual(['https://apack.dev'])

    // Switch program to mouse reporting mode
    source.send(
      frame({
        seq: 2,
        size: { cols: 30, rows: 2 },
        modes: { altScreen: false, mouse: true, reverseVideo: false, bracketedPaste: false },
        rows: [row(0, 'mouse app')],
      }),
    )
    painted()
    point('pointerdown', 3, 1, { button: 0, at: 2000 })
    point('pointermove', 4, 1, { button: 0, at: 2050 })
    point('pointerup', 4, 1, { button: 0, at: 2100 })
    // Side buttons (Back=3 / Forward=4) must NOT be sent as invalid PointerInput
    point('pointerdown', 3, 1, { button: 3, at: 2200 })
    point('pointerup', 3, 1, { button: 3, at: 2250 })
    point('pointerdown', 3, 1, { button: 4, at: 2300 })
    point('pointerup', 3, 1, { button: 4, at: 2350 })
    expect(source.pointers.map((p) => [p.action, p.button, p.xPx, p.yPx])).toEqual([
      ['press', 0, 35, 30],
      ['motion', 0, 45, 30],
      ['release', 0, 45, 30],
    ])
  })

  it('scrolls history on wheel, sends wheelUp/wheelDown in mouse mode, and ArrowUp/Down in altScreen', () => {
    const { source, view, painted } = mountedView()
    source.send(
      frame({
        size: { cols: 20, rows: 4 },
        firstActiveLine: 20,
        scrollbackLines: 20,
        rows: [row(20, 's0')],
      }),
    )
    painted()

    view.element.dispatchEvent(new WheelEvent('wheel', { deltaY: -40, deltaMode: 0, cancelable: true }))
    expect(view.viewport.following).toBe(false)
    expect(view.viewport.top).toBe(18)

    // Alt-screen without mouse sends ArrowUp/ArrowDown
    source.send(
      frame({
        seq: 2,
        size: { cols: 20, rows: 4 },
        firstActiveLine: 20,
        scrollbackLines: 0,
        modes: { altScreen: true, mouse: false, reverseVideo: false, bracketedPaste: false },
      }),
    )
    painted()
    view.element.dispatchEvent(new WheelEvent('wheel', { deltaY: -2, deltaMode: 1, cancelable: true }))
    expect(source.keys.map((k) => k.code)).toEqual(['ArrowUp', 'ArrowUp'])
  })
})
