/**
 * A 2D context that records what was drawn on it.
 *
 * jsdom has no canvas, and a painter's cases are about *what* it draws where — which a list of calls says
 * exactly, and which pixels would only say after a rasteriser nobody here is testing.
 */
export interface Drawn {
  readonly kind: 'rect' | 'text' | 'stroke' | 'strokeRect' | 'image'
  readonly x: number
  readonly y: number
  readonly width?: number
  readonly height?: number
  readonly text?: string
  readonly color: string
  readonly font?: string
  /** For an image: the picture drawn and the part of it, as left, top, width, height */
  readonly picture?: unknown
  readonly source?: readonly [number, number, number, number]
  /** What it was drawn inside, innermost last, each as left, top, width, height on the canvas: empty for all of it */
  readonly clips: ReadonlyArray<readonly [number, number, number, number]>
}

/**
 * `cellWidth` is how wide the font's own characters measure. A string of several code units measures that
 * for each, which is how a case has a character wider than its cell.
 */
export function recordingContext(cellWidth = 10) {
  const drawn: Drawn[] = []
  /** How many times the context was given each of these, which costs a browser whether or not it changed */
  const given = { font: 0, fillStyle: 0 }
  const cell = { width: cellWidth }
  let path: Array<[number, number]> = []
  let rects: Array<[number, number, number, number]> = []
  let state = { shift: [0, 0] as [number, number], clips: [] as Array<readonly [number, number, number, number]>, fillStyle: '', strokeStyle: '', font: '' }
  const saved: Array<typeof state> = []
  const context = {
    get fillStyle() { return state.fillStyle },
    set fillStyle(value: string) { given.fillStyle += 1; state.fillStyle = value },
    get strokeStyle() { return state.strokeStyle },
    set strokeStyle(value: string) { state.strokeStyle = value },
    get font() { return state.font },
    set font(value: string) { given.font += 1; state.font = value },
    lineWidth: 1,
    textBaseline: 'alphabetic',
    // Where the origin has been moved to, which every call after it is recorded relative to the canvas by
    setTransform() { state.shift = [0, 0] },
    translate(x: number, y: number) { state.shift = [state.shift[0] + x, state.shift[1] + y] },
    save() { saved.push({ ...state, shift: [...state.shift], clips: [...state.clips] }) },
    restore() { state = saved.pop() ?? state },
    setLineDash() {},
    // The font box is twenty tall with sixteen above the baseline
    measureText: (text: string) => ({ width: text.length * cell.width, fontBoundingBoxAscent: 16, fontBoundingBoxDescent: 4 }),
    fillRect(x: number, y: number, width: number, height: number) {
      drawn.push({ kind: 'rect', x: x + state.shift[0], y: y + state.shift[1], width, height, color: state.fillStyle, clips: state.clips })
    },
    strokeRect(x: number, y: number, width: number, height: number) {
      drawn.push({ kind: 'strokeRect', x: x + state.shift[0], y: y + state.shift[1], width, height, color: state.strokeStyle, clips: state.clips })
    },
    fillText(text: string, x: number, y: number) {
      drawn.push({ kind: 'text', x: x + state.shift[0], y: y + state.shift[1], text, color: state.fillStyle, font: state.font, clips: state.clips })
    },
    drawImage(picture: unknown, sx: number, sy: number, sw: number, sh: number, x: number, y: number, width: number, height: number) {
      drawn.push({ kind: 'image', x: x + state.shift[0], y: y + state.shift[1], width, height, color: '', picture, source: [sx, sy, sw, sh], clips: state.clips })
    },
    beginPath() { path = []; rects = [] },
    rect(x: number, y: number, width: number, height: number) { rects.push([x + state.shift[0], y + state.shift[1], width, height]) },
    clip() { state.clips = [...state.clips, ...rects] },
    moveTo(x: number, y: number) { path.push([x, y]) },
    lineTo(x: number, y: number) { path.push([x, y]) },
    quadraticCurveTo(_cx: number, _cy: number, x: number, y: number) { path.push([x, y]) },
    stroke() {
      const [first] = path
      const last = path.at(-1)
      if (first && last) drawn.push({ kind: 'stroke', x: first[0], y: first[1], width: last[0] - first[0], color: state.strokeStyle, clips: state.clips })
    },
  }
  const canvas = document.createElement('canvas')
  canvas.getContext = (() => context) as unknown as HTMLCanvasElement['getContext']
  return { canvas, drawn, given, cell }
}
