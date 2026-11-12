// What a screen reader is given by a view, and nothing a person has to see to know it: the page's accessible document, the
// text of each screen row in it, and which frame was last drawn. A screen reader reads these, so their shape is the contract.
// What a real screen reader says of them is a checklist for a person (docs/references/evidence/terminal-parity.md, Phase 6).
import { beforeEach, describe, expect, it } from 'vitest'
import { installFrames, runFrames } from './_frames.ts'
import { frame, row } from './_support.ts'
import { mountedView } from './_view.ts'

beforeEach(() => installFrames())

/** The accessible document a view publishes inside its element */
const documentOf = (element: HTMLElement) => element.querySelector<HTMLElement>('[role="document"]')

describe('the accessible document a view publishes', () => {
  it('is a document named Terminal, found in the view\'s own element', () => {
    const view = mountedView()
    const document = documentOf(view.view.element)
    expect(document, 'no document in the view').not.toBeNull()
    expect(document!.getAttribute('aria-label')).toBe('Terminal')
    expect(document!.dataset.terminalText).toBe('')
  })

  it('has one text element a screen row, holding that row\'s text', () => {
    const view = mountedView()
    view.source.send(frame({ screenRows: 3, columns: 20, rows: [row(0, 'alpha beta'), row(1, 'gamma'), row(2, '$ ')] }))
    runFrames()
    const lines = [...documentOf(view.view.element)!.children].map((child) => child.textContent)
    expect(lines).toEqual(['alpha beta', 'gamma', '$ '])
  })

  it('is rewritten only where a row changed: a row that did not change is not touched at all', async () => {
    const view = mountedView()
    view.source.send(frame({ sequence: 1, screenRows: 2, columns: 20, rows: [row(0, 'steady'), row(1, 'first')] }))
    runFrames()
    const published = documentOf(view.view.element)!
    const touched: Node[] = []
    const watcher = new MutationObserver((records) => {
      for (const record of records) touched.push(record.target.nodeType === Node.TEXT_NODE ? record.target.parentNode! : record.target)
    })
    watcher.observe(published, { childList: true, characterData: true, subtree: true })
    view.source.send(frame({ sequence: 2, screenRows: 2, columns: 20, rows: [row(1, 'second')] }))
    runFrames()
    await new Promise((resolve) => setTimeout(resolve, 0))
    const changed = touched.flatMap((node) => (node.parentNode === published ? [node] : []))
    watcher.disconnect()
    expect(changed, 'a row that did not change was rewritten').toEqual([published.children[1]])
    expect(published.children[0]!.textContent).toBe('steady')
    expect(published.children[1]!.textContent).toBe('second')
  })

  it('says which frame was last drawn, so a test or a reader can tell the screen has caught up', () => {
    const view = mountedView()
    view.source.send(frame({ sequence: 7, screenRows: 1, columns: 20, rows: [row(0, 'ready')] }))
    runFrames()
    expect(view.view.element.dataset.terminalDrawn).toBe('7')
  })
})
