// A view, used: what a click, a drag, a wheel and a pointer over a link each do — and who they are for,
// which changes when a program asks for the mouse.
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { Command, Placements, Span } from '../src/source.ts'
import { framesWaiting, installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled, theme } from './_view.ts'

beforeEach(() => installFrames())

/** A screen of four rows of text, and a hundred lines of history above it when asked for */
function showing(setup: ReturnType<typeof mountedView>, parts: Parameters<typeof frame>[0] = {}) {
  setup.source.send(frame({ rows: [row(parts.firstActive ?? 0, 'alpha beta'), row((parts.firstActive ?? 0) + 1, 'gamma')], ...parts }))
  runFrames()
}

/** The cells a painted row draws as selected */
const selectedOn = (setup: ReturnType<typeof mountedView>, index: number) =>
  setup.painted().rows[index]?.backgrounds.filter((run) => run.color === theme.selectionBackground).map((run) => [run.column, run.column + run.cells])

describe('selecting', () => {
  it('starts with a drag, not a click, and covers the cells between where it went down and where it is', () => {
    const view = mountedView()
    showing(view)
    view.point('pointerdown', 1, 0)
    expect(selectedOn(view, 0), 'a press selected something').toEqual([])
    view.point('pointermove', 4, 0)
    expect(selectedOn(view, 0)).toEqual([[1, 5]])
    view.point('pointermove', 2, 1)
    expect(selectedOn(view, 0), 'to the end of the row it started on').toEqual([[1, 20]])
    expect(selectedOn(view, 1)).toEqual([[0, 3]])
    view.point('pointerup', 2, 1)
    expect(view.view.selection.span).toEqual({ start: { line: 0, column: 1 }, end: { line: 1, column: 2 } })
  })

  it('is cleared by a click', () => {
    const view = mountedView()
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 4, 0)
    view.point('pointerup', 4, 0)
    view.point('pointerdown', 6, 1)
    view.point('pointerup', 6, 1)
    expect(view.view.selection.span).toBeNull()
  })

  it('takes a word on a double click and a line on a triple, as the source says what those are', async () => {
    class Words extends FakeSource {
      override async wordAt(): Promise<Span | null> { return { start: { line: 0, column: 6 }, end: { line: 0, column: 9 } } }
      override async lineAt(): Promise<Span | null> { return { start: { line: 0, column: 0 }, end: { line: 1, column: 4 } } }
    }
    const view = mountedView({}, new Words())
    showing(view)
    // Three presses of one button in one place, each soon after the last: a pointer event says none of that
    view.point('pointerdown', 7, 0, { at: 1000 })
    view.point('pointerup', 7, 0, { at: 1080 })
    expect(view.view.selection.span, 'one press selected something').toBeNull()
    view.point('pointerdown', 7, 0, { at: 1250 })
    await settled()
    expect(selectedOn(view, 0)).toEqual([[6, 10]])
    view.point('pointerup', 7, 0, { at: 1300 })

    view.point('pointerdown', 7, 0, { at: 1500 })
    await settled()
    expect(view.view.selection.span).toEqual({ start: { line: 0, column: 0 }, end: { line: 1, column: 4 } })
    view.point('pointerup', 7, 0, { at: 1550 })

    // A fourth is the first of another round
    view.point('pointerdown', 7, 0, { at: 1700 })
    expect(view.view.selection.span).toBeNull()
  })

  it('counts a press as the same click again only soon after the last, in the same place, with the same button', async () => {
    class Words extends FakeSource {
      override async wordAt(): Promise<Span | null> { return { start: { line: 0, column: 6 }, end: { line: 0, column: 9 } } }
    }
    const pressedTwice = async (second: Parameters<ReturnType<typeof mountedView>['point']>[3], column = 7) => {
      const view = mountedView({}, new Words())
      showing(view)
      view.point('pointerdown', 7, 0, { at: 1000 })
      view.point('pointerup', 7, 0, { at: 1050 })
      view.point('pointerdown', column, 0, second)
      await settled()
      return view.view.selection.span
    }
    expect(await pressedTwice({ at: 1400 }), 'two presses in one place, soon after each other').not.toBeNull()
    expect(await pressedTwice({ at: 1600 }), 'too long after').toBeNull()
    expect(await pressedTwice({ at: 1400 }, 9), 'somewhere else').toBeNull()
    expect(await pressedTwice({ at: 1400, button: 2 }), 'with another button').toBeNull()
  })

  it('is copied as the source says its text is, by the copy shortcut', async () => {
    class Text extends FakeSource {
      override async textOf(): Promise<string> { return 'lpha b' }
    }
    const copied: string[] = []
    const view = mountedView({ onCopy: (text) => { copied.push(text) } }, new Text())
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 6, 0)
    view.point('pointerup', 6, 0)
    const pressed = view.key('keydown', { code: 'KeyC', key: 'c', metaKey: true })
    await settled()
    expect(copied).toEqual(['lpha b'])
    expect(pressed.defaultPrevented).toBe(true)
    expect(view.source.keys, 'the copy shortcut reached the program').toEqual([])
  })

  it('ends on the last line in view when dragged to the bottom edge or below it, and is copied from there', async () => {
    class Text extends FakeSource {
      asked: Span[] = []
      override async textOf(span: Span): Promise<string> { this.asked.push(span); return 'alpha beta\ngamma' }
    }
    const copied: string[] = []
    const view = mountedView({ onCopy: (text) => { copied.push(text) } }, new Text())
    showing(view)
    view.point('pointerdown', 0, 0)
    // The screen is four rows: this is where a fifth would be, and then well below that
    view.point('pointermove', 5, 4)
    expect(view.view.selection.span?.end).toEqual({ line: 3, column: 5 })
    view.point('pointermove', 5, 9)
    view.point('pointerup', 5, 9)
    expect(view.view.selection.span).toEqual({ start: { line: 0, column: 0 }, end: { line: 3, column: 5 } })

    expect(await view.view.copy()).toBe(true)
    expect(copied).toEqual(['alpha beta\ngamma'])
  })

  it('of everything is copied as what the terminal still holds, after history it covered was let go', async () => {
    class Text extends FakeSource {
      asked: Span[] = []
      override async textOf(span: Span): Promise<string> { this.asked.push(span); return 'kept' }
    }
    const source = new Text()
    const view = mountedView({ onCopy: () => {} }, source)
    showing(view, { firstActive: 100, scrollback: 100 })
    view.view.selectAll()
    // The owner trims its history to the last ten lines
    view.source.send(frame({ sequence: 2, firstActive: 100, scrollback: 10 }))

    expect(await view.view.copy()).toBe(true)
    expect(source.asked).toEqual([{ start: { line: 90, column: 0 }, end: { line: 103, column: 19 } }])
  })

  it('stops being dragged when the pointer is taken away, though no release says so', () => {
    const view = mountedView()
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 4, 0)
    view.point('pointercancel', 4, 0)
    view.point('pointermove', 8, 1)
    expect(view.view.selection.span).toEqual({ start: { line: 0, column: 1 }, end: { line: 0, column: 4 } })
  })

  it('scrolls the view while it is dragged past an edge, faster the further past, and stops when it is let go', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 100 })
    view.point('pointerdown', 1, 1)
    // Half a row above the top edge. Nothing scrolls until a frame, and then for as long as frames come
    view.point('pointermove', 1, -1)
    expect(view.view.viewport.top).toBe(100)
    runFrames(0)
    runFrames(100)
    const slow = 100 - view.view.viewport.top
    expect(slow).toBeGreaterThan(0)

    // Four and a half rows above it: nine times as far, over the same tenth of a second
    view.point('pointermove', 1, -5)
    const before = view.view.viewport.top
    runFrames(200)
    const fast = before - view.view.viewport.top
    expect(fast).toBeGreaterThan(slow * 4)
    expect(view.view.selection.span?.start, 'the selection did not follow the view').toEqual({ line: Math.floor(view.view.viewport.top), column: 1 })
    expect(view.view.selection.span?.end).toEqual({ line: 101, column: 1 })

    view.point('pointerup', 1, -5)
    const stopped = view.view.viewport.top
    runFrames(300)
    runFrames(400)
    expect(view.view.viewport.top, 'went on scrolling with the button up').toBe(stopped)
    expect(framesWaiting()).toBe(0)
  })

  it('stops scrolling when the pointer comes back inside', () => {
    const inside = mountedView()
    showing(inside, { firstActive: 100, scrollback: 100 })
    inside.point('pointerdown', 1, 1)
    inside.point('pointermove', 1, -3)
    runFrames(0)
    inside.point('pointermove', 1, 2)
    const held = inside.view.viewport.top
    runFrames(100)
    runFrames(200)
    expect(inside.view.viewport.top).toBe(held)
  })

  it('stops scrolling when the view is disposed mid-drag', () => {
    const disposed = mountedView()
    showing(disposed, { firstActive: 100, scrollback: 100 })
    disposed.point('pointerdown', 1, 1)
    disposed.point('pointermove', 1, -3)
    disposed.view.dispose()
    expect(framesWaiting(), 'a disposed view left a frame asked for').toBe(0)
  })

  it('is forgotten when the screen is reflowed, since its lines are gone', () => {
    const view = mountedView()
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 4, 0)
    view.point('pointerup', 4, 0)
    showing(view, { epoch: 2, sequence: 2 })
    expect(view.view.selection.span).toBeNull()
  })
})

describe('a program that asked for the mouse', () => {
  const asked = { modes: { alternateScreen: false, mouse: true } }

  it('is sent the press, the drag and the release, in pixels, and nothing is selected', () => {
    const view = mountedView()
    showing(view, asked)
    view.point('pointerdown', 2, 1)
    view.point('pointermove', 3, 1)
    view.point('pointerup', 3, 1)
    expect(view.source.pointers.map((sent) => [sent.action, sent.button, sent.x, sent.y])).toEqual([
      ['press', 0, 25, 30], ['move', 0, 35, 30], ['release', 0, 35, 30],
    ])
    expect(view.source.pointers[0]).toMatchObject({ cellWidth: 10, cellHeight: 20, modifiers: 0 })
    expect(view.view.selection.span).toBeNull()
  })

  it('is sent a move with nothing held, which is how it draws what the pointer is over', () => {
    const view = mountedView()
    showing(view, asked)
    view.point('pointermove', 5, 0)
    expect(view.source.pointers.map((sent) => [sent.action, sent.button])).toEqual([['move', -1]])
  })

  it('is told of a move once for each cell the pointer comes to', () => {
    const view = mountedView()
    showing(view, asked)
    const moveTo = (clientX: number, clientY: number) =>
      view.view.element.dispatchEvent(new MouseEvent('pointermove', { bubbles: true, clientX, clientY }))
    moveTo(52, 4)
    moveTo(55, 9)
    moveTo(58, 16)
    expect(view.source.pointers, 'three moves inside one cell were three reports').toHaveLength(1)
    moveTo(61, 16)
    moveTo(61, 21)
    expect(view.source.pointers.map((sent) => [sent.x, sent.y])).toEqual([[52, 4], [61, 16], [61, 21]])
  })

  it('is sent the release when the pointer is taken away mid-press, once, however that is said', () => {
    const cancelled = mountedView()
    showing(cancelled, asked)
    cancelled.point('pointerdown', 2, 1)
    cancelled.point('pointercancel', 2, 1)
    cancelled.point('lostpointercapture', 2, 1)
    expect(cancelled.source.pointers.map((sent) => [sent.action, sent.button])).toEqual([['press', 0], ['release', 0]])
    // And what follows is a pointer with nothing held, not a drag that never ended
    cancelled.point('pointermove', 6, 1)
    expect(cancelled.source.pointers.at(-1)).toMatchObject({ action: 'move', button: -1 })

    const released = mountedView()
    showing(released, asked)
    released.point('pointerdown', 2, 1)
    released.point('pointerup', 2, 1)
    released.point('lostpointercapture', 2, 1)
    expect(released.source.pointers.map((sent) => sent.action)).toEqual(['press', 'release'])
  })

  it('is told where the pointer is in the cells it draws, when the page shows the view at another size', () => {
    const view = mountedView()
    showing(view, asked)
    // A panel scaled to twice its size, a hundred pixels in and fifty down: the canvas is 200 by 80 of its own pixels
    const canvas = view.painter.canvas
    Object.defineProperty(canvas, 'offsetWidth', { value: 200 })
    Object.defineProperty(canvas, 'offsetHeight', { value: 80 })
    canvas.getBoundingClientRect = () => ({ left: 100, top: 50, width: 400, height: 160 }) as DOMRect
    view.view.element.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, cancelable: true, clientX: 150, clientY: 110 }))
    expect(view.source.pointers[0]).toMatchObject({ action: 'press', x: 25, y: 30, cellWidth: 10, cellHeight: 20 })
  })

  it('has the right button, and so the page shows no menu, only while the screen it draws on is showing', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 100, ...asked })
    const menu = () => view.point('contextmenu', 2, 1).defaultPrevented
    expect(menu()).toBe(true)
    view.view.scrollTo(50)
    expect(menu(), 'a click the program is not sent took the menu with it').toBe(false)
  })

  it('is passed over with shift held, which is how a person selects in spite of it', () => {
    const view = mountedView()
    showing(view, asked)
    view.point('pointerdown', 1, 0, { shiftKey: true })
    view.point('pointermove', 4, 0, { shiftKey: true })
    expect(view.source.pointers).toEqual([])
    expect(selectedOn(view, 0)).toEqual([[1, 5]])
  })
})

describe('the wheel', () => {
  const wheel = (view: ReturnType<typeof mountedView>, deltaY: number, init: WheelEventInit = {}) => {
    const event = new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaY, clientX: 15, clientY: 10, ...init })
    view.view.element.dispatchEvent(event)
    return event
  }

  it('scrolls history where there is some', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 100 })
    const event = wheel(view, -60)
    expect(event.defaultPrevented).toBe(true)
    expect(view.view.viewport.top).toBe(97)
    expect(view.source.pointers).toEqual([])
  })

  it('goes to a program that asked for the mouse, a whole notch at a time', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 100, modes: { alternateScreen: false, mouse: true } })
    wheel(view, -8)
    expect(view.source.pointers, 'less than a notch was sent').toEqual([])
    wheel(view, -14)
    wheel(view, 40)
    expect(view.source.pointers.map((sent) => [sent.action, sent.button])).toEqual([['wheelUp', -1], ['wheelDown', -1], ['wheelDown', -1]])
    expect(view.view.viewport.following).toBe(true)
  })

  it('turned the other way is a notch of its own, not paid out of what was owed the first way', () => {
    const view = mountedView()
    showing(view, { modes: { alternateScreen: false, mouse: true } })
    // Nine tenths of a notch up, then one whole notch down
    wheel(view, -18)
    wheel(view, 20)
    expect(view.source.pointers.map((sent) => sent.action)).toEqual(['wheelDown'])
  })

  it('scrolls the history of a view that is scrolled back, whatever the program asked for', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 100, modes: { alternateScreen: false, mouse: true } })
    view.view.scrollTo(50)
    wheel(view, -60)
    expect(view.source.pointers, 'a program was sent a wheel over lines it is not drawing').toEqual([])
    expect(view.view.viewport.top).toBe(47)
  })

  it('with control held is the page\'s, which is how a pinch zooms it', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 100 })
    expect(wheel(view, -60, { ctrlKey: true }).defaultPrevented).toBe(false)
    expect(view.view.viewport.top).toBe(100)
  })

  it('is arrow keys to a full-screen program that did not ask, which is what scrolls a pager', () => {
    const view = mountedView()
    showing(view, { modes: { alternateScreen: true, mouse: false } })
    wheel(view, 40)
    wheel(view, -20)
    expect(view.source.keys.map((sent) => sent.code)).toEqual(['ArrowDown', 'ArrowDown', 'ArrowUp'])
  })

  it('does nothing, and is left to the page, where there is nothing to scroll', () => {
    const view = mountedView()
    showing(view)
    expect(wheel(view, -60).defaultPrevented).toBe(false)
  })
})

describe('the scrollbar', () => {
  /** A view eighty pixels tall over a hundred lines of history, and how many times its height was read */
  function tall() {
    const view = mountedView()
    let reads = 0
    Object.defineProperty(view.view.element, 'clientHeight', { get: () => { reads += 1; return 80 }, configurable: true })
    view.view.fit()
    showing(view, { firstActive: 100, scrollback: 100 })
    const thumb = view.view.element.lastElementChild as HTMLElement
    const edges = () => { view.painted(); return [Number.parseFloat(thumb.style.top), Number.parseFloat(thumb.style.top) + Number.parseFloat(thumb.style.height)] }
    return { ...view, thumb, edges, reads: () => reads }
  }

  it('is not there to be pressed while it is not there to be seen', () => {
    const view = tall()
    view.painted()
    expect([view.thumb.style.opacity, view.thumb.style.pointerEvents]).toEqual(['0', 'none'])
    view.view.scrollTo(50)
    view.painted()
    expect([view.thumb.style.opacity, view.thumb.style.pointerEvents]).toEqual(['0.35', 'auto'])
    view.view.follow()
    view.painted()
    expect(view.thumb.style.pointerEvents).toBe('none')
  })

  it('stays on its track from the oldest line to the newest, at the least height a hand can take hold of', () => {
    const view = tall()
    view.view.scrollTo(0)
    expect(view.edges()).toEqual([0, 20])
    view.view.scrollTo(50)
    expect(view.edges()).toEqual([30, 50])
    view.view.scrollTo(99)
    const [top, bottom] = view.edges()
    expect(top).toBeGreaterThan(55)
    expect(bottom, 'the thumb ran off the end of its track').toBeLessThanOrEqual(80)
  })

  it('moves the view in proportion when dragged, from one end of its track to the other', () => {
    const view = tall()
    view.view.scrollTo(50)
    view.painted()
    view.thumb.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, cancelable: true, clientY: 40 }))
    // The track is sixty pixels of travel for a hundred lines
    window.dispatchEvent(new MouseEvent('pointermove', { clientY: 25 }))
    expect(view.view.viewport.top).toBe(25)
    window.dispatchEvent(new MouseEvent('pointermove', { clientY: 10 }))
    expect(view.view.viewport.top).toBe(0)
    window.dispatchEvent(new MouseEvent('pointerup'))
    window.dispatchEvent(new MouseEvent('pointermove', { clientY: 40 }))
    expect(view.view.viewport.top, 'went on dragging with the button up').toBe(0)
  })

  it('lets go of a drag that was under way when the view is disposed, or the pointer is taken away', () => {
    for (const ending of ['dispose', 'pointercancel'] as const) {
      const view = tall()
      view.view.scrollTo(50)
      view.painted()
      view.thumb.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, cancelable: true, clientY: 40 }))
      if (ending === 'dispose') view.view.dispose()
      else window.dispatchEvent(new MouseEvent('pointercancel'))
      window.dispatchEvent(new MouseEvent('pointermove', { clientY: 10 }))
      expect(view.view.viewport.top, `after ${ending}`).toBe(50)
    }
  })

  it('is drawn from a height measured when the view is sized, not on every paint', () => {
    const view = tall()
    const measured = view.reads()
    for (let paint = 0; paint < 3; paint += 1) {
      view.view.scrollBy(-1)
      view.painted()
    }
    expect(view.reads()).toBe(measured)
  })
})

describe('copying to the page\'s clipboard', () => {
  class Text extends FakeSource {
    answer: (text: string) => void = () => {}
    override textOf(): Promise<string> { return new Promise((resolve) => { this.answer = resolve }) }
  }
  const textIn = (blob: Blob) => new Promise<string>((resolve) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result))
    reader.readAsText(blob)
  })
  /** A view with a cell dragged over, and the clipboard a page would have */
  function selected(clipboard: object | undefined) {
    Object.defineProperty(navigator, 'clipboard', { value: clipboard, configurable: true })
    const view = mountedView({}, new Text())
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 6, 0)
    view.point('pointerup', 6, 0)
    return view as ReturnType<typeof mountedView> & { source: Text }
  }
  afterEach(() => {
    Reflect.deleteProperty(navigator, 'clipboard')
    Reflect.deleteProperty(globalThis, 'ClipboardItem')
  })

  it('hands the clipboard the text before the text has come, while the page is still handling what the person did', async () => {
    Object.assign(globalThis, { ClipboardItem: class { constructor(readonly items: Record<string, Promise<Blob>>) {} } })
    const written: Array<Record<string, Promise<Blob>>> = []
    const view = selected({ write: async ([item]: Array<{ items: Record<string, Promise<Blob>> }>) => { written.push(item!.items); await item!.items['text/plain'] } })

    const copying = view.view.copy()
    expect(written, 'the clipboard was not written inside the gesture').toHaveLength(1)
    view.source.answer('lpha b')
    expect(await copying).toBe(true)
    expect(await textIn(await written[0]!['text/plain']!)).toBe('lpha b')
  })

  it('says it did not copy when the page would not let it, or there was nothing to', async () => {
    Object.assign(globalThis, { ClipboardItem: class { constructor(readonly items: Record<string, Promise<Blob>>) {} } })
    const refused = selected({ write: async () => { throw new DOMException('not allowed', 'NotAllowedError') } })
    const copying = refused.view.copy()
    refused.source.answer('lpha b')
    expect(await copying).toBe(false)

    const empty = selected({ write: async ([item]: Array<{ items: Record<string, Promise<Blob>> }>) => { await item!.items['text/plain'] } })
    const nothing = empty.view.copy()
    empty.source.answer('')
    expect(await nothing).toBe(false)

    const none = selected(undefined)
    expect(await none.view.copy(), 'a page with no clipboard copied').toBe(false)
  })

  it('says it did not copy when the source has no text to give: the lines were numbered again, or there was too much of it', async () => {
    class Refusing extends FakeSource {
      override async textOf(): Promise<string> { throw new Error('a text was longer than a message may be') }
    }
    const written: string[] = []
    Object.defineProperty(navigator, 'clipboard', { value: { writeText: async (text: string) => { written.push(text) } }, configurable: true })
    const view = mountedView({}, new Refusing())
    showing(view, { firstActive: 100, scrollback: 100 })
    view.view.selectAll()
    expect(await view.view.copy()).toBe(false)
    expect(written).toEqual([])
  })

  it('writes the text once it has come where the clipboard takes nothing else', async () => {
    const written: string[] = []
    const view = selected({ writeText: async (text: string) => { written.push(text) } })
    const copying = view.view.copy()
    view.source.answer('lpha b')
    expect(await copying).toBe(true)
    expect(written).toEqual(['lpha b'])
  })
})

describe('a link', () => {
  const linked = () => {
    const opened: string[] = []
    const view = mountedView({ onLink: (link) => { opened.push(link.target) } })
    view.source.send(frame({ rows: [row(0, 'see https://example.com now')] }))
    runFrames()
    return { view, opened }
  }

  it('is underlined, and the pointer changes, while the pointer is over it', () => {
    const { view } = linked()
    view.point('pointermove', 8, 0)
    expect(view.painted().rows[0]!.lines).toEqual([{ kind: 'single', column: 4, cells: 19, color: theme.foreground }])
    expect(view.view.element.style.cursor).toBe('pointer')
    expect(view.view.element.title).toBe('https://example.com')

    view.point('pointermove', 1, 0)
    expect(view.painted().rows[0]!.lines).toEqual([])
    expect(view.view.element.style.cursor).toBe('text')
  })

  it('is worked out once for each cell the pointer comes to, not for every move inside one', () => {
    let looked = 0
    const view = mountedView({ detectors: [{ kind: 'counted', find: () => { looked += 1; return [] } }] })
    view.source.send(frame({ rows: [row(0, 'nothing to open here')] }))
    runFrames()
    const moveTo = (clientX: number) => view.view.element.dispatchEvent(new MouseEvent('pointermove', { bubbles: true, clientX, clientY: 10 }))
    moveTo(51)
    moveTo(54)
    moveTo(58)
    expect(looked).toBe(1)
    moveTo(63)
    expect(looked).toBe(2)

    // The row under a pointer that has not moved is another row: what it opens is worked out again
    view.source.send(frame({ sequence: 2, rows: [row(0, 'see https://example.com now')] }))
    runFrames()
    moveTo(64)
    expect(view.view.element.title).toBe('https://example.com')
  })

  it('stops being pointed at when the screen is reflowed from under the pointer', () => {
    const { view } = linked()
    view.point('pointermove', 8, 0)
    expect(view.view.element.style.cursor).toBe('pointer')
    view.source.send(frame({ sequence: 2, epoch: 2, rows: [row(0, 'nothing to open')] }))
    runFrames()
    expect([view.view.element.style.cursor, view.view.element.title]).toEqual(['text', ''])
  })

  it('opens on a click with the platform\'s modifier held, and not on a plain click or a drag', () => {
    const { view, opened } = linked()
    view.point('pointerdown', 8, 0)
    view.point('pointerup', 8, 0)
    expect(opened, 'a plain click opened it').toEqual([])

    view.point('pointerdown', 8, 0, { metaKey: true })
    view.point('pointermove', 12, 0, { metaKey: true })
    view.point('pointerup', 12, 0, { metaKey: true })
    expect(opened, 'a drag opened it').toEqual([])

    view.point('pointerdown', 8, 0, { metaKey: true })
    view.point('pointerup', 8, 0, { metaKey: true })
    expect(opened).toEqual(['https://example.com'])
  })
})

describe('what a page can read of a view', () => {
  it('is the text on screen, a row an element, kept to what is shown', () => {
    const view = mountedView()
    showing(view)
    const text = () => [...view.view.element.querySelectorAll('[data-terminal-text] > div')].map((line) => line.textContent)
    expect(text()).toEqual(['alpha beta', 'gamma', '', ''])

    view.source.send(frame({ sequence: 2, rows: [row(1, 'delta   end')] }))
    runFrames()
    expect(text()).toEqual(['alpha beta', 'delta   end', '', ''])
  })

  it('says which frame has been drawn, once it has', () => {
    const view = mountedView()
    view.source.send(frame({ sequence: 41 }))
    expect(view.view.element.dataset.terminalDrawn).toBeUndefined()
    runFrames()
    expect(view.view.element.dataset.terminalDrawn).toBe('41')
  })
})

describe('what a host is told about the selection', () => {
  it('is whether there is one, each time that changes and not otherwise', () => {
    const told: boolean[] = []
    const view = mountedView({ onSelectionChange: (selected) => { told.push(selected) } })
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 4, 0)
    view.point('pointermove', 6, 0)
    view.point('pointerup', 6, 0)
    expect(told).toEqual([true])
    view.point('pointerdown', 2, 1)
    view.point('pointerup', 2, 1)
    expect(told).toEqual([true, false])
  })

  it('covers everything held, history included, when the person selects all', () => {
    const view = mountedView()
    showing(view, { firstActive: 100, scrollback: 40 })
    view.view.selectAll()
    expect(view.view.selection.span).toEqual({ start: { line: 60, column: 0 }, end: { line: 103, column: 19 } })
  })
})

describe('clearing', () => {
  it('asks the program, with the key a person would press', () => {
    const view = mountedView()
    showing(view)
    view.view.clear()
    expect(view.source.keys).toEqual([{ action: 'press', code: 'KeyL', modifiers: 2, text: '', unshifted: 108, composed: false }])
  })
})

describe('where a session\'s images are', () => {
  it('is asked again by the next frame when the answer never came', async () => {
    const source = new (class extends FakeSource {
      failing = true
      override async images(): Promise<Placements> {
        this.imagesAsked += 1
        if (this.failing) throw new Error('unanswered')
        return this.placed
      }
    })()
    const view = mountedView({}, source)
    showing(view, { images: 1 })
    await settled()
    source.failing = false
    view.source.send(frame({ sequence: 2, images: 1 }))
    await settled()
    expect(source.imagesAsked).toBe(2)
    view.source.send(frame({ sequence: 3, images: 1 }))
    await settled()
    expect(source.imagesAsked, 'asked again though it had been answered').toBe(2)
  })
})

describe('the commands a shell ran', () => {
  const command = (id: number, prompt: number, parts: Partial<Command> = {}): Command => ({
    id, command: `cmd ${id}`, cwd: '/src', startedMs: 1000, finishedMs: 2000, outcome: { kind: 'exited', code: 0 }, prompt, output: prompt + 1, end: prompt + 3, ...parts,
  })
  /** A view over a screen at lines 100 to 103 with a hundred lines of history, and three commands in it */
  async function withCommands(options: Parameters<typeof mountedView>[0] = {}) {
    const view = mountedView({ gutter: 6, ...options })
    view.source.commandList = [
      command(1, 20),
      command(2, 60, { outcome: { kind: 'exited', code: 2 } }),
      command(3, 101, { finishedMs: null, outcome: { kind: 'running' }, end: null }),
    ]
    showing(view, { firstActive: 100, scrollback: 100, commands: 1 })
    await settled()
    return view
  }

  it('are asked for when a frame says there is something new, and not for a frame that does not', async () => {
    const view = await withCommands()
    expect(view.source.commandsAsked).toBe(1)
    view.source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, commands: 1 }))
    await settled()
    expect(view.source.commandsAsked, 'asked again though nothing was new').toBe(1)
    view.source.send(frame({ sequence: 3, firstActive: 100, scrollback: 100, commands: 2 }))
    await settled()
    expect(view.source.commandsAsked).toBe(2)
  })

  it('are asked for again after a reflow, since the lines they were on are gone', async () => {
    const view = await withCommands()
    view.source.send(frame({ sequence: 2, epoch: 2, firstActive: 100, scrollback: 100, commands: 1 }))
    await settled()
    expect(view.source.commandsAsked).toBe(2)
  })

  it('are asked for again by the next frame when the answer never came', async () => {
    const source = new (class extends FakeSource {
      failing = true
      override async commands(): Promise<readonly Command[]> {
        this.commandsAsked += 1
        if (this.failing) throw new Error('unanswered')
        return this.commandList
      }
    })()
    source.commandList = [command(1, 101)]
    const view = mountedView({ gutter: 6 }, source)
    showing(view, { firstActive: 100, scrollback: 100, commands: 1 })
    await settled()
    expect(view.view.commands).toEqual([])

    source.failing = false
    view.source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, commands: 1 }))
    await settled()
    expect(source.commandsAsked).toBe(2)
    expect(view.view.commands.map((ran) => ran.id)).toEqual([1])
  })

  it('are found among thousands by the line asked about, whichever it is', async () => {
    const view = mountedView({ gutter: 6 })
    view.source.commandList = Array.from({ length: 5000 }, (_, index) => command(index + 1, index * 4))
    showing(view, { firstActive: 20000, scrollback: 20000, commands: 1 })
    await settled()

    expect(view.view.commandAt(0)?.id).toBe(1)
    expect(view.view.commandAt(4001)?.id).toBe(1001)
    expect(view.view.commandAt(19999)?.id).toBe(5000)
    view.view.scrollTo(8001)
    // Lines 8001 to 8004 are in view, and one prompt is on them
    expect(view.painted().marks).toEqual([{ row: 3, color: theme.palette[2] }])
    view.view.scrollToCommand(-1)
    expect(view.view.viewport.top).toBe(8000)
    view.view.scrollToCommand(1)
    expect(view.view.viewport.top).toBe(8004)
  })

  it('are marked beside their prompts in the gutter, coloured by how each ended', async () => {
    const view = await withCommands()
    // On screen: only the third, which is still running
    expect(view.painted().marks).toEqual([{ row: 1, color: theme.palette[4] }])

    view.view.scrollTo(58)
    expect(view.painted().marks, 'a command that failed').toEqual([{ row: 2, color: theme.palette[1] }])
    view.view.scrollTo(20)
    expect(view.painted().marks, 'a command that succeeded').toEqual([{ row: 0, color: theme.palette[2] }])
  })

  it('are marked as having succeeded only where a status of zero was vouched for', async () => {
    const view = mountedView({ gutter: 6 })
    const ended: Array<[Command['outcome'], number | undefined, string]> = [
      [{ kind: 'exited', code: 0 }, theme.palette[2], 'an exit of zero'],
      [{ kind: 'claimed', code: 0 }, theme.palette[3], 'a claim of zero'],
      [{ kind: 'claimed', code: 2 }, theme.palette[1], 'a claim of a failure'],
      [{ kind: 'claimed', code: null }, theme.palette[8], 'a claim of an end with no status'],
      [{ kind: 'unknown', why: 'no_status' }, theme.palette[8], 'an end with no status'],
      [{ kind: 'unknown', why: 'superseded' }, theme.palette[8], 'an end nothing reported'],
      [{ kind: 'unknown', why: 'session_ended' }, theme.palette[8], 'a session that ended'],
    ]
    view.source.commandList = ended.map(([outcome], index) => command(index + 1, 100 + index, { outcome }))
    showing(view, { firstActive: 100, scrollback: 100, commands: 1, screenRows: ended.length })
    await settled()

    const marks = view.painted().marks ?? []
    expect(marks.map((mark) => mark.row)).toEqual(ended.map((_, index) => index))
    for (const [index, [, color, what]] of ended.entries()) expect(marks[index]?.color, what).toBe(color)
    const succeeded = marks.filter((mark) => mark.color === theme.palette[2])
    expect(succeeded.map((mark) => mark.row), 'only the vouched exit of zero is drawn as success').toEqual([0])
  })

  it('are not marked where the view was given no gutter to mark them in', async () => {
    const view = await withCommands({ gutter: 0 })
    expect(view.painted().marks).toEqual([])
  })

  it('are stepped between, from prompt to prompt, and past the newest back to the screen', async () => {
    const view = await withCommands()
    view.view.scrollToCommand(-1)
    expect(view.view.viewport.top).toBe(60)
    view.view.scrollToCommand(-1)
    expect(view.view.viewport.top).toBe(20)
    view.view.scrollToCommand(-1)
    expect(view.view.viewport.top, 'there is none before the oldest').toBe(20)

    view.view.scrollToCommand(1)
    expect(view.view.viewport.top).toBe(60)
    view.view.scrollToCommand(1)
    view.view.scrollToCommand(1)
    expect(view.view.viewport.following, 'forward from the newest did not return to the screen').toBe(true)
  })

  it('are stepped between by the keyboard, with a key the program is not sent', async () => {
    const view = await withCommands()
    const pressed = view.key('keydown', { code: 'ArrowUp', key: 'ArrowUp', metaKey: true })
    expect(view.view.viewport.top).toBe(60)
    expect(pressed.defaultPrevented).toBe(true)
    expect(view.source.keys).toEqual([])
  })

  it('are found from any line of theirs, and their output is what lies between where it began and ended', async () => {
    const view = await withCommands()
    expect(view.view.commandAt(10), 'above the first command').toBeNull()
    expect(view.view.commandAt(20)?.id).toBe(1)
    expect(view.view.commandAt(59)?.id, 'the lines after a command, up to the next, are its').toBe(1)
    expect(view.view.commandAt(60)?.id).toBe(2)

    const second = view.view.commandAt(61)!
    expect(view.view.outputOf(second)).toEqual({ start: { line: 61, column: 0 }, end: { line: 62, column: 19 } })
    expect(view.view.selectOutput(second)).toBe(true)
    expect(view.view.selection.span).toEqual({ start: { line: 61, column: 0 }, end: { line: 62, column: 19 } })

    // One still running has printed down to the bottom of the screen so far
    expect(view.view.outputOf(view.view.commandAt(103)!)).toEqual({ start: { line: 102, column: 0 }, end: { line: 103, column: 19 } })
    // And one that printed nothing has nothing to select
    expect(view.view.selectOutput(command(9, 30, { end: 31 }))).toBe(false)
  })
})

describe('finding text', () => {
  class Searchable extends FakeSource {
    asked: string[] = []
    found: Span[] = []
    override async search(needle: string): Promise<readonly Span[]> { this.asked.push(needle); return this.found }
  }
  const span = (line: number, from: number, to: number, endLine = line): Span => ({ start: { line, column: from }, end: { line: endLine, column: to } })
  /** The cells a painted row draws behind a match, with the colour of each */
  const matchedOn = (view: ReturnType<typeof mountedView>, index: number) =>
    view.painted().rows[index]?.backgrounds.filter((run) => run.color === 0x623315 || run.color === 0x9e6a03).map((run) => [run.column, run.column + run.cells, run.color === 0x9e6a03 ? 'current' : 'other'])

  function searching(found: Span[]) {
    const source = new Searchable()
    source.found = found
    const view = mountedView({}, source)
    showing(view, { firstActive: 100, scrollback: 100 })
    return { view, source }
  }

  it('asks the source, which searches the history as well, and says how many there are', async () => {
    const { view, source } = searching([span(101, 0, 4), span(100, 6, 9), span(30, 2, 5)])
    expect(await view.view.find('beta')).toBe(3)
    expect(source.asked).toEqual(['beta'])
    expect(view.view.found).toEqual({ current: 1, total: 3 })
  })

  it('draws every match on screen, and the one the person is on differently', async () => {
    const { view } = searching([span(101, 0, 4), span(100, 6, 9)])
    await view.view.find('a')
    expect(matchedOn(view, 1)).toEqual([[0, 5, 'current']])
    expect(matchedOn(view, 0)).toEqual([[6, 10, 'other']])
  })

  it('draws a match that runs across the end of a row on both rows', async () => {
    const { view } = searching([span(100, 17, 2, 101)])
    await view.view.find('wrapped')
    expect(matchedOn(view, 0)).toEqual([[17, 20, 'current']])
    expect(matchedOn(view, 1)).toEqual([[0, 3, 'current']])
  })

  it('steps to older matches, scrolling one that is out of view into the middle, and round again', async () => {
    const { view } = searching([span(101, 0, 4), span(100, 6, 9), span(30, 2, 5)])
    await view.view.find('a')
    expect(view.view.viewport.following, 'a match already on screen scrolled the view').toBe(true)

    view.view.findNext()
    view.view.findNext()
    expect(view.view.found.current).toBe(3)
    expect(view.view.viewport.top, 'the oldest match is not in the middle of the view').toBe(28)

    view.view.findNext()
    expect(view.view.found.current, 'past the oldest is the newest again').toBe(1)
    view.view.findNext(-1)
    expect(view.view.found.current).toBe(3)
  })

  it('that found nothing has a match to be on once output brings one', async () => {
    const { view, source } = searching([])
    expect(await view.view.find('later')).toBe(0)
    source.found = [span(101, 0, 4), span(100, 6, 9)]
    view.source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, rows: [row(101, 'later')] }))
    await settled()
    expect(view.view.found).toEqual({ current: 1, total: 2 })
    expect(matchedOn(view, 1)).toEqual([[0, 5, 'current']])
  })

  it('draws nothing again when rows changed and what was found did not', async () => {
    const { view } = searching([span(101, 0, 4)])
    await view.view.find('a')
    const before = view.painted().rows[1]
    // Output on a line with no match on it: the search is made again and says the same
    view.source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, rows: [row(103, 'unrelated')] }))
    runFrames()
    await settled()
    expect(framesWaiting(), 'a search that found the same was a reason to paint').toBe(0)
    expect(view.painted().rows[1], 'a row whose match did not move was drawn afresh').toBe(before)
  })

  it('is cleared by an empty needle, and forgotten when the screen is reflowed', async () => {
    const { view, source } = searching([span(101, 0, 4)])
    await view.view.find('a')
    expect(await view.view.find('')).toBe(0)
    expect(source.asked, 'an empty needle was searched for').toEqual(['a'])
    expect(matchedOn(view, 1)).toEqual([])

    await view.view.find('a')
    showing(view, { epoch: 2, sequence: 5, firstActive: 100, scrollback: 100 })
    expect(view.view.found).toEqual({ current: 0, total: 0 })
  })

  it('is searched for again when rows change, one search at a time, and the person stays on the match they were on', async () => {
    const source = new (class extends Searchable {
      waiting: Array<(found: Span[]) => void> = []
      override search(needle: string): Promise<readonly Span[]> {
        this.asked.push(needle)
        return new Promise((resolve) => { this.waiting.push(resolve) })
      }
    })()
    const view = mountedView({}, source)
    showing(view, { firstActive: 100, scrollback: 100 })
    const finding = view.view.find('a')
    source.waiting[0]!([span(101, 0, 4), span(100, 6, 9)])
    await finding
    view.view.findNext()
    expect(view.view.found).toEqual({ current: 2, total: 2 })

    // A frame that changed no row is not a reason to search
    view.source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, cursor: { column: 0, row: 0, shape: 'block', blinking: false } }))
    expect(source.asked).toEqual(['a'])

    // Three frames of new output while the first of their searches is on its way: one search, then one more
    for (const sequence of [3, 4, 5]) view.source.send(frame({ sequence, firstActive: 100, scrollback: 100, rows: [row(102, `more a ${sequence}`)] }))
    expect(source.asked).toEqual(['a', 'a'])
    source.waiting[1]!([span(102, 5, 5), span(101, 0, 4), span(100, 6, 9)])
    await settled()
    expect(source.asked).toEqual(['a', 'a', 'a'])
    source.waiting[2]!([span(102, 5, 5), span(101, 0, 4), span(100, 6, 9)])
    await settled()
    expect(source.asked, 'searched again with nothing changed since').toHaveLength(3)

    expect(view.view.found, 'the match the person was on is still the one they are on').toEqual({ current: 3, total: 3 })
    expect(matchedOn(view, 2)).toEqual([[5, 6, 'other']])
    expect(matchedOn(view, 0)).toEqual([[6, 10, 'current']])
  })

  it('draws the matches on a line out of very many, and only those', async () => {
    // Two matches on every line from the screen's second back through ten thousand lines, newest first
    const source = new Searchable()
    source.found = Array.from({ length: 20000 }, (_, index) => {
      const line = 10001 - Math.floor(index / 2)
      return index % 2 === 0 ? span(line, 10, 12) : span(line, 2, 4)
    })
    const view = mountedView({}, source)
    showing(view, { firstActive: 10000, scrollback: 10000 })
    await view.view.find('a')
    expect(matchedOn(view, 1)).toEqual([[10, 13, 'current'], [2, 5, 'other']])
    expect(matchedOn(view, 0)).toEqual([[10, 13, 'other'], [2, 5, 'other']])
    expect(matchedOn(view, 2), 'a line below the newest match').toBeUndefined()
  })

  it('takes the answer to the latest search, whichever answer arrives last', async () => {
    const source = new (class extends Searchable {
      waiting: Array<(found: Span[]) => void> = []
      override search(needle: string): Promise<readonly Span[]> {
        this.asked.push(needle)
        return new Promise((resolve) => { this.waiting.push(resolve) })
      }
    })()
    const view = mountedView({}, source)
    showing(view, { firstActive: 100, scrollback: 100 })
    const first = view.view.find('a')
    const second = view.view.find('ab')
    source.waiting[1]!([span(100, 0, 1)])
    source.waiting[0]!([span(100, 0, 1), span(101, 0, 1), span(102, 0, 1)])
    await Promise.all([first, second])
    expect(view.view.found.total, 'an earlier search overwrote a later one').toBe(1)
  })
})
