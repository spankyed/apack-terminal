// A view mounted on a page with a painter that records, cells ten pixels by twenty, for the suites that
// drive it the way a person does.
import type { Theme } from '../src/draw.ts'
import type { Painter, Scene } from '../src/painter.ts'
import { TerminalView, type ViewOptions } from '../src/view.ts'
import { runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { PALETTE } from './_support.ts'

export const theme: Theme = {
  background: 0x1e1e1e, palette: PALETTE, foreground: 0xd4d4d4, cursor: 0xffffff, cursorText: 0x000000,
  selectionBackground: 0x264f78, selectionForeground: null,
}

export class RecordingPainter implements Painter {
  readonly canvas = document.createElement('canvas')
  readonly metrics = { cellWidth: 10, cellHeight: 20, baseline: 16 }
  readonly scenes: Scene[] = []
  resize(): void {}
  setFont(): void {}
  paint(scene: Scene): void { this.scenes.push(scene) }
  dispose(): void {}
}

export function mountedView(options: Partial<ViewOptions> = {}, source = new FakeSource()) {
  const painter = new RecordingPainter()
  const view = new TerminalView(source, { font: { family: 'monospace', size: 14 }, theme, painter: () => painter, mac: true, ...options })
  const host = document.createElement('div')
  document.body.appendChild(host)
  view.mount(host)
  /** The last scene painted, after running whatever frames were waiting */
  const painted = () => { runFrames(); return painter.scenes.at(-1)! }
  /**
   * A pointer event over the cell at `column`, `row` of the grid, as a page delivers one: it does not say
   * how many times it was clicked. `at` is when on the page's clock, in milliseconds; left out, it is a
   * second after the event before, which is too long after for any two to be one click.
   */
  let clock = 0
  const point = (type: string, column: number, row: number, init: MouseEventInit & { at?: number } = {}) => {
    const { at, ...rest } = init
    clock = at ?? clock + 1000
    // jsdom has no `PointerEvent`: a mouse event with what a pointer event adds that a view reads
    const event = new MouseEvent(type, { bubbles: true, cancelable: true, clientX: column * 10 + 5, clientY: row * 20 + 10, ...rest, detail: 0 })
    Object.defineProperty(event, 'timeStamp', { value: clock })
    Object.defineProperty(event, 'pointerId', { value: 1 })
    view.element.dispatchEvent(event)
    return event
  }
  const key = (type: 'keydown' | 'keyup', init: KeyboardEventInit) => {
    const event = new KeyboardEvent(type, { bubbles: true, cancelable: true, ...init })
    view.element.querySelector('textarea')!.dispatchEvent(event)
    return event
  }
  return { source, painter, view, host, painted, point, key, field: view.element.querySelector('textarea')! }
}

/** Lets promises that are already settled run their continuations */
export const settled = () => new Promise<void>((resolve) => setTimeout(resolve, 0))
