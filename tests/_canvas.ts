/**
 * A 2D context that records what was drawn on it.
 *
 * jsdom has no canvas, and a painter's cases are about *what* it draws where — which a list of calls says
 * exactly, and which pixels would only say after a rasteriser nobody here is testing.
 */
export interface Drawn {
  readonly kind: 'rect' | 'text' | 'stroke' | 'strokeRect'
  readonly x: number
  readonly y: number
  readonly width?: number
  readonly height?: number
  readonly text?: string
  readonly color: string
  readonly font?: string
}

export function recordingContext(cellWidth = 10) {
  const drawn: Drawn[] = []
  let path: Array<[number, number]> = []
  const context = {
    fillStyle: '',
    strokeStyle: '',
    font: '',
    lineWidth: 1,
    textBaseline: 'alphabetic',
    setTransform() {},
    setLineDash() {},
    // Every character is one cell wide, and the font box is twenty tall with sixteen above the baseline
    measureText: (text: string) => ({ width: text.length * cellWidth, fontBoundingBoxAscent: 16, fontBoundingBoxDescent: 4 }),
    fillRect(x: number, y: number, width: number, height: number) {
      drawn.push({ kind: 'rect', x, y, width, height, color: context.fillStyle })
    },
    strokeRect(x: number, y: number, width: number, height: number) {
      drawn.push({ kind: 'strokeRect', x, y, width, height, color: context.strokeStyle })
    },
    fillText(text: string, x: number, y: number) {
      drawn.push({ kind: 'text', x, y, text, color: context.fillStyle, font: context.font })
    },
    beginPath() { path = [] },
    moveTo(x: number, y: number) { path.push([x, y]) },
    lineTo(x: number, y: number) { path.push([x, y]) },
    quadraticCurveTo(_cx: number, _cy: number, x: number, y: number) { path.push([x, y]) },
    stroke() {
      const [first] = path
      const last = path.at(-1)
      if (first && last) drawn.push({ kind: 'stroke', x: first[0], y: first[1], width: last[0] - first[0], color: context.strokeStyle })
    },
  }
  const canvas = document.createElement('canvas')
  canvas.getContext = (() => context) as unknown as HTMLCanvasElement['getContext']
  return { canvas, drawn }
}
