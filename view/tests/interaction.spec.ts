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

  it('tracks pointerId on Pressed and ignores mismatched pointer events during drag/reporting', () => {
    const { source, view, host, painted, point } = mountedView()
    source.send(
      frame({
        size: { cols: 20, rows: 2 },
        rows: [row(0, 'abcdefghij'), row(1, 'klmnopqrst')],
      }),
    )
    painted()

    // Start drag selection with pointerId 1 at col 1, row 0
    point('pointerdown', 1, 0, { button: 0, pointerId: 1, at: 100 })
    // Mismatched pointerId 2 move/up/cancel MUST be completely ignored!
    point('pointermove', 9, 1, { button: 0, pointerId: 2, at: 120 })
    point('pointerup', 9, 1, { button: 0, pointerId: 2, at: 130 })
    point('pointercancel', 9, 1, { button: 0, pointerId: 2, at: 140 })
    expect(view.selection.span).toBeNull()

    // Matching pointerId 1 continues and completes the drag cleanly
    point('pointermove', 4, 0, { button: 0, pointerId: 1, at: 150 })
    point('pointerup', 4, 0, { button: 0, pointerId: 1, at: 160 })
    expect(view.selection.span).toEqual({ start: { line: 0, col: 1 }, end: { line: 0, col: 4 } })

    // Same isolation in PTY mouse-reporting mode
    source.send(
      frame({
        seq: 2,
        size: { cols: 20, rows: 2 },
        modes: { altScreen: false, mouse: true, reverseVideo: false, bracketedPaste: false },
        rows: [row(0, 'mouse')],
      }),
    )
    painted()
    point('pointerdown', 2, 0, { button: 0, pointerId: 10, at: 500 })
    point('pointermove', 8, 1, { button: 0, pointerId: 11, at: 520 })
    point('pointerup', 8, 1, { button: 0, pointerId: 11, at: 530 })
    point('pointerup', 3, 0, { button: 0, pointerId: 10, at: 540 })
    expect(source.pointers.map((p) => [p.action, p.xPx, p.yPx])).toEqual([
      ['press', 25, 10],
      ['release', 35, 10],
    ])

    // Same pointerId isolation for scrollbar thumb drag (ignores mismatched pointerId & secondary pointerdown)
    Object.defineProperty(view.element, 'clientHeight', { value: 200, configurable: true })
    Object.defineProperty(host, 'clientWidth', { value: 200, configurable: true })
    Object.defineProperty(host, 'clientHeight', { value: 200, configurable: true })
    view.fit()
    source.send(
      frame({
        seq: 3,
        size: { cols: 20, rows: 10 },
        firstActiveLine: 90,
        scrollbackLines: 90,
        modes: { altScreen: false, mouse: false, reverseVideo: false, bracketedPaste: false },
        rows: [row(90, 'tail')],
      }),
    )
    painted()
    const thumb = view.element.lastElementChild as HTMLDivElement
    const dispatchPointer = (target: EventTarget, type: string, clientY: number, pointerId: number, button = 0) => {
      const ev = new MouseEvent(type, { bubbles: true, cancelable: true, clientY, button })
      Object.defineProperty(ev, 'pointerId', { value: pointerId })
      target.dispatchEvent(ev)
    }
    const startPos = view.viewport.extent.position
    dispatchPointer(thumb, 'pointerdown', 180, 20, 0)
    // Secondary pointerdown and mismatched pointermove/pointerup MUST be ignored!
    dispatchPointer(thumb, 'pointerdown', 100, 21, 0)
    dispatchPointer(window, 'pointermove', 20, 21, 0)
    dispatchPointer(window, 'pointerup', 20, 21, 0)
    expect(view.viewport.extent.position).toBe(startPos)
    // Matching pointerId 20 drags the scrollbar up cleanly and ends on matching pointerup
    dispatchPointer(window, 'pointermove', 90, 20, 0)
    const draggedPos = view.viewport.extent.position
    expect(draggedPos).toBeLessThan(startPos)
    dispatchPointer(window, 'pointerup', 90, 20, 0)
    // Subsequent move after release does not scroll further
    dispatchPointer(window, 'pointermove', 10, 20, 0)
    expect(view.viewport.extent.position).toBe(draggedPos)

    // Thumb drag ignores non-left pointerup on the same pointerId, and still releases on button=0 or pointercancel
    dispatchPointer(thumb, 'pointerdown', 180, 30, 0)
    const posBeforeRightUp = view.viewport.extent.position
    dispatchPointer(window, 'pointerup', 120, 30, 2)
    dispatchPointer(window, 'pointermove', 120, 30, 0)
    expect(view.viewport.extent.position).not.toBe(posBeforeRightUp)
    const posBeforeCancel = view.viewport.extent.position
    dispatchPointer(window, 'pointercancel', 120, 30, 2)
    dispatchPointer(window, 'pointermove', 20, 30, 0)
    expect(view.viewport.extent.position).toBe(posBeforeCancel)
  })

  it('guards throwing setPointerCapture and ignores non-left pointerdown when not mouse-reporting', () => {
    const { source, view, painted, point } = mountedView()
    source.send(
      frame({
        size: { cols: 20, rows: 2 },
        rows: [row(0, 'abcdefghij'), row(1, 'klmnopqrst')],
      }),
    )
    painted()

    // Simulate browser throwing DOMException from setPointerCapture
    let captureAttempts = 0
    view.element.setPointerCapture = () => {
      captureAttempts += 1
      throw new DOMException('Invalid pointerId', 'NotFoundError')
    }

    // Right-click (button 2) without mouse reporting must return early WITHOUT setting pressed or calling setPointerCapture
    point('pointerdown', 1, 0, { button: 2, pointerId: 40, at: 100 })
    expect(captureAttempts).toBe(0)

    // Subsequent left-click drag still works immediately (not wedged by the un-released right-click!)
    point('pointerdown', 1, 0, { button: 0, pointerId: 41, at: 120 })
    expect(captureAttempts).toBe(1)
    point('pointermove', 5, 0, { button: 0, pointerId: 41, at: 140 })
    point('pointerup', 5, 0, { button: 0, pointerId: 41, at: 160 })
    expect(view.selection.span).toEqual({ start: { line: 0, col: 1 }, end: { line: 0, col: 5 } })
  })
})
