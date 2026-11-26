// What a screen reader is given by a view: the accessible document, lazy 200ms Clock-coalesced
// updates + synchronous flushMirror(), and the last drawn frame sequence.
import { beforeEach, describe, expect, it } from 'vitest'
import { MIRROR_DEBOUNCE_MS } from '../src/index.ts'
import { TestClock } from './_clock.ts'
import { installFrames, runFrames } from './_frames.ts'
import { frame, row } from './_support.ts'
import { mountedView } from './_view.ts'

beforeEach(() => installFrames())

const documentOf = (element: HTMLElement) => element.querySelector<HTMLElement>('[role="document"]')

describe('the accessible document a view publishes', () => {
  it('is a document named Terminal, found in the view\'s own element', () => {
    const view = mountedView()
    const doc = documentOf(view.view.element)
    expect(doc, 'no document in the view').not.toBeNull()
    expect(doc!.getAttribute('aria-label')).toBe('Terminal')
    expect(doc!.dataset.terminalText).toBe('')
  })

  it('populates on initial paint and has one text element per screen row', () => {
    const view = mountedView()
    view.source.send(frame({ size: { cols: 20, rows: 3 }, rows: [row(0, 'alpha beta'), row(1, 'gamma'), row(2, '$ ')] }))
    runFrames()
    const lines = [...documentOf(view.view.element)!.children].map((child) => child.textContent)
    expect(lines).toEqual(['alpha beta', 'gamma', '$ '])
  })

  it('coalesces subsequent mirror mutations via Clock and rewrites only changed rows', async () => {
    const clock = new TestClock()
    const view = mountedView({ clock })
    view.source.send(frame({ seq: 1, size: { cols: 20, rows: 2 }, rows: [row(0, 'steady'), row(1, 'first')] }))
    runFrames()

    const published = documentOf(view.view.element)!
    const touched: Node[] = []
    const watcher = new MutationObserver((records) => {
      for (const record of records) {
        touched.push(record.target.nodeType === Node.TEXT_NODE ? record.target.parentNode! : record.target)
      }
    })
    watcher.observe(published, { childList: true, characterData: true, subtree: true })

    view.source.send(frame({ seq: 2, size: { cols: 20, rows: 2 }, rows: [row(1, 'intermediate')] }))
    runFrames()
    view.source.send(frame({ seq: 3, size: { cols: 20, rows: 2 }, rows: [row(1, 'second')] }))
    runFrames()
    // Not flushed yet before 200ms debounce
    expect(published.children[1]!.textContent).toBe('first')

    clock.advance(MIRROR_DEBOUNCE_MS)
    await new Promise((resolve) => setTimeout(resolve, 0))
    const changed = touched.flatMap((node) => (node.parentNode === published ? [node] : []))
    watcher.disconnect()
    expect(changed, 'a row that did not change was rewritten').toEqual([published.children[1]])
    expect(published.children[0]!.textContent).toBe('steady')
    expect(published.children[1]!.textContent).toBe('second')

    // Synchronous flushMirror() updates immediately without waiting for debounce
    view.source.send(frame({ seq: 4, size: { cols: 20, rows: 2 }, rows: [row(1, 'flushed')] }))
    runFrames()
    expect(published.children[1]!.textContent).toBe('second')
    view.view.flushMirror()
    expect(published.children[1]!.textContent).toBe('flushed')
  })

  it('says which frame was last drawn, so a test or reader can tell the screen has caught up', () => {
    const view = mountedView()
    view.source.send(frame({ seq: 7, size: { cols: 20, rows: 1 }, rows: [row(0, 'ready')] }))
    runFrames()
    expect(view.view.element.dataset.terminalDrawn).toBe('7')
  })
})
