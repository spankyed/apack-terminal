// The painter under heavy output: a full-screen repaint of coloured text, every row changed on every frame, drawn
// through the real canvas painter, timed per paint and per frame (sent to acknowledged). Reports percentiles to
// `window.__result` and the console. The page reads its settings from the query string:
//   ?frames=300&columns=200&rows=50&minimumContrast=4.5
import { CanvasPainter, TerminalView, xtermPalette, type Font, type Painter, type Row, type Style, type Frame, PLAIN, paletteColor } from '../src/index.ts'
import { FakeSource } from '../tests/_source.ts'

const query = new URLSearchParams(location.search)
const FRAMES = Number(query.get('frames') ?? 300)
const COLUMNS = Number(query.get('columns') ?? 200)
const ROWS = Number(query.get('rows') ?? 50)
const MINIMUM = query.get('minimumContrast') === null ? undefined : Number(query.get('minimumContrast'))
const font: Font = { family: 'monospace', size: 14 }

// Sixteen colours, so every cell of a row can differ from its neighbour
const styles: Style[] = [PLAIN, ...Array.from({ length: 16 }, (_, index) => ({ ...PLAIN, fg: paletteColor(index) }))]

/** One full row: each character in one of the sixteen colours, shifted by the frame and the line so nothing repeats */
function rowOf(line: number, generation: number): Row {
  const text = Array.from({ length: COLUMNS }, () => 'x')
  return {
    line,
    wrapped: false,
    prompt: false,
    text,
    column: Uint16Array.from(text, (_, index) => index),
    width: Uint8Array.from(text, () => 1),
    style: Uint16Array.from(text, (_, index) => 1 + ((index + line + generation) % 16)),
    link: new Uint16Array(COLUMNS),
    styles,
    links: [''],
  }
}

/** A source that sends the next full frame when the view has painted the last */
class Heavy extends FakeSource {
  sequence = 0
  sentAt = 0
  readonly frameTimes: number[] = []
  onDone: () => void = () => {}

  next(): void {
    this.sequence += 1
    const rows = Array.from({ length: ROWS }, (_, line) => rowOf(line, this.sequence))
    const frame: Frame = {
      sequence: this.sequence, epoch: 1, firstActive: 0, columns: COLUMNS, screenRows: ROWS, scrollback: 0, commands: 0,
      images: 0, cursor: null, modes: { alternateScreen: false, mouse: false }, reverseVideo: false, rows,
    }
    this.sentAt = performance.now()
    ;(window as unknown as { __sent: number }).__sent = this.sequence
    this.send(frame)
  }

  override acknowledge(sequence: number): void {
    super.acknowledge(sequence)
    this.frameTimes.push(performance.now() - this.sentAt)
    ;(window as unknown as { __acked: number }).__acked = this.frameTimes.length
    // Deferred: the view is still inside the paint that acknowledged this, and a frame sent from here would be dropped
    // when it returns. A broker answers later in any case.
    if (this.frameTimes.length < FRAMES) setTimeout(() => this.next(), 0)
    else this.onDone()
  }
}

/** The canvas painter, with each paint timed */
class Timed extends CanvasPainter {
  readonly paints: number[] = []
  override paint(scene: Parameters<CanvasPainter['paint']>[0]): void {
    const started = performance.now()
    super.paint(scene)
    this.paints.push(performance.now() - started)
  }
}

function percentile(values: number[], p: number): number {
  const sorted = [...values].sort((a, b) => a - b)
  return sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))] ?? 0
}

function summary(values: number[]) {
  return { n: values.length, p50: percentile(values, 50), p95: percentile(values, 95), max: Math.max(...values) }
}

let painter: Timed | undefined
const source = new Heavy()
source.onDone = () => {
  // The first frames build the cache and the canvas: measured as they are, from frame 2
  const paints = painter!.paints.slice(5)
  const frames = source.frameTimes.slice(5)
  const result = {
    columns: COLUMNS, rows: ROWS, frames: frames.length, minimumContrast: MINIMUM ?? null,
    paint: summary(paints), frame: summary(frames), userAgent: navigator.userAgent,
  }
  ;(window as unknown as { __result: unknown }).__result = result
  console.log(JSON.stringify(result))
}

const view = new TerminalView(source, {
  font,
  theme: {
    background: 0x1e1e1e, foreground: 0xd4d4d4, palette: xtermPalette(Array.from({ length: 16 }, (_, index) => 0x808080 | (index * 0x0f0f0f))),
    cursor: 0xd4d4d4, cursorText: 0x1e1e1e, selectionBackground: 0x264f78, selectionForeground: null,
    ...(MINIMUM === undefined ? {} : { minimumContrast: MINIMUM }),
  },
  painter: (fontOf: Font): Painter => {
    painter = new Timed(fontOf)
    return painter
  },
  mac: true,
})
view.mount(document.getElementById('host')!)
// The view is sized by its element; the source waits for the first ask for a frame, which the view makes on mount
source.next()
