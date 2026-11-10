// A view, used: what a click, a drag, a wheel and a pointer over a link each do — and who they are for,
// which changes when a program asks for the mouse.
import { beforeEach, describe, expect, it } from 'vitest'
import type { Span } from '../src/source.ts'
import { installFrames, runFrames } from './_frames.ts'
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
    view.point('pointerdown', 7, 0, { detail: 2 })
    await settled()
    expect(selectedOn(view, 0)).toEqual([[6, 10]])

    view.point('pointerdown', 7, 0, { detail: 3 })
    await settled()
    expect(view.view.selection.span).toEqual({ start: { line: 0, column: 0 }, end: { line: 1, column: 4 } })
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
    expect(view.source.pointers.map((sent) => [sent.action, sent.button])).toEqual([['wheelUp', -1], ['wheelDown', -1]])
    expect(view.view.viewport.following).toBe(true)
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
