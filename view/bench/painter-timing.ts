// The painter under heavy output: a full-screen repaint of coloured text, every row changed on every frame, drawn
// through the real canvas painter, timed per paint and per frame (sent to acknowledged).
import {
  CanvasPainter,
  TerminalView,
  packRow,
  xtermPalette,
  type Font,
  type FrameUpdate,
  type PackedRow,
  type Painter,
  type StyleSpec,
} from '../src/index.ts'
import { FakeSource } from '../tests/_source.ts'

const query = new URLSearchParams(location.search)
const FRAMES = Number(query.get('frames') ?? 300)
const COLUMNS = Number(query.get('columns') ?? 200)
const ROWS = Number(query.get('rows') ?? 50)
const MINIMUM = query.get('minimumContrast') === null ? undefined : Number(query.get('minimumContrast'))
const font: Font = { family: 'monospace', size: 14 }

const styles: StyleSpec[] = [{}, ...Array.from({ length: 16 }, (_, index) => ({ fg: index + 1 }))]

function rowOf(line: number, generation: number): PackedRow {
  const runs = Array.from({ length: COLUMNS }, (_, index) => ({
    colStart: index,
    colLen: 1,
    text: 'x',
    styleIdx: 1 + ((index + line + generation) % 16),
  }))
  return packRow({ line, runs, styles })
}

class Heavy extends FakeSource {
  seq = 0
  sentAt = 0
  readonly frameTimes: number[] = []
  onDone: () => void = () => {}

  next(): void {
    this.seq += 1
    const rows = Array.from({ length: ROWS }, (_, line) => rowOf(line, this.seq))
    const f: FrameUpdate = {
      seq: this.seq,
      epoch: 1,
      size: { cols: COLUMNS, rows: ROWS },
      firstActiveLine: 0,
      scrollbackLines: 0,
      scrollDelta: 0,
      cursor: { col: 0, row: 0, shape: 'block', blinking: false, hidden: true },
      modes: { altScreen: false, mouse: false, reverseVideo: false, bracketedPaste: false },
      palette: null,
      rows,
      imagesGen: 0,
      fullSync: false,
    }
    this.sentAt = performance.now()
    ;(window as unknown as { __sent: number }).__sent = this.seq
    this.send(f)
  }

  override ackFrame(seq: number): void {
    super.ackFrame(seq)
    this.frameTimes.push(performance.now() - this.sentAt)
    ;(window as unknown as { __acked: number }).__acked = this.frameTimes.length
    if (this.frameTimes.length < FRAMES) setTimeout(() => this.next(), 0)
    else this.onDone()
  }
}

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
  const paints = painter!.paints.slice(5)
  const frames = source.frameTimes.slice(5)
  const result = {
    columns: COLUMNS,
    rows: ROWS,
    frames: frames.length,
    minimumContrast: MINIMUM ?? null,
    paint: summary(paints),
    frame: summary(frames),
    userAgent: navigator.userAgent,
  }
  ;(window as unknown as { __result: unknown }).__result = result
  console.log(JSON.stringify(result))
}

const view = new TerminalView(source, {
  font,
  theme: {
    background: 0x1e1e1e,
    foreground: 0xd4d4d4,
    palette: xtermPalette(Array.from({ length: 16 }, (_, index) => 0x808080 | (index * 0x0f0f0f))),
    cursor: 0xd4d4d4,
    cursorText: 0x1e1e1e,
    selectionBackground: 0x264f78,
    selectionForeground: null,
    ...(MINIMUM === undefined ? {} : { minimumContrast: MINIMUM }),
  },
  painter: (fontOf: Font): Painter => {
    painter = new Timed(fontOf)
    return painter
  },
  mac: true,
})
view.mount(document.getElementById('host')!)
source.next()
