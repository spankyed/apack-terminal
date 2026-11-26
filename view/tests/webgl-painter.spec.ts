import { describe, expect, it } from 'vitest'
import {
  CanvasPainter,
  DrawMode,
  INSTANCE_FLOATS,
  PackedStyleFlag,
  RunFlag,
  ShelfPacker,
  WebGL2AtlasPainter,
  WebGL2UnavailableError,
  createPainter,
  drawRow,
  packRow,
  type Scene,
  type Theme,
} from '../src/index.ts'
import { recordingContext } from './_canvas.ts'
import { PALETTE, packedRgb, row } from './_support.ts'

const theme: Theme = {
  background: 0x1e1e1e,
  foreground: 0xd4d4d4,
  palette: PALETTE,
  cursor: 0xffffff,
  cursorText: 0x000000,
  selectionBackground: 0x264f78,
  selectionForeground: null,
}

interface MockDrawCall {
  readonly count: number
  readonly instances: Float32Array
  readonly texture: unknown
}

function mockWebGL2Canvas(options: { readonly maxTextureSize?: number } = {}) {
  const uniforms = new Map<string, number | readonly number[]>()
  const drawCalls: MockDrawCall[] = []
  let texSubUploads = 0
  let deletedTextures = 0
  let deletedBuffers = 0
  let deletedVaos = 0
  let deletedPrograms = 0
  let boundTexture: unknown = null
  let lastUploadedBuffer = new Float32Array(0)
  let contextLostFlag = false
  let nextId = 1

  const gl = {
    VERTEX_SHADER: 0x8b31,
    FRAGMENT_SHADER: 0x8b30,
    COMPILE_STATUS: 0x8b81,
    LINK_STATUS: 0x8b82,
    ARRAY_BUFFER: 0x8892,
    STATIC_DRAW: 0x88e4,
    DYNAMIC_DRAW: 0x88e8,
    FLOAT: 0x1406,
    TRIANGLES: 0x0004,
    TEXTURE0: 0x84c0,
    TEXTURE_2D: 0x0de1,
    TEXTURE_MIN_FILTER: 0x2801,
    TEXTURE_MAG_FILTER: 0x2800,
    TEXTURE_WRAP_S: 0x2802,
    TEXTURE_WRAP_T: 0x2803,
    LINEAR: 0x2601,
    CLAMP_TO_EDGE: 0x812f,
    RGBA: 0x1908,
    UNSIGNED_BYTE: 0x1401,
    UNPACK_PREMULTIPLY_ALPHA_WEBGL: 0x9241,
    BLEND: 0x0be2,
    ONE: 1,
    ONE_MINUS_SRC_ALPHA: 0x0303,
    COLOR_BUFFER_BIT: 0x4000,
    MAX_TEXTURE_SIZE: 0x0d33,

    isContextLost: () => contextLostFlag,
    getParameter: (param: number) => (param === 0x0d33 ? (options.maxTextureSize ?? 2048) : 0),
    createShader: () => ({ id: nextId++ }),
    shaderSource: () => {},
    compileShader: () => {},
    getShaderParameter: () => true,
    getShaderInfoLog: () => '',
    deleteShader: () => {},
    createProgram: () => ({ id: nextId++ }),
    attachShader: () => {},
    linkProgram: () => {},
    getProgramParameter: () => true,
    getProgramInfoLog: () => '',
    deleteProgram: () => { deletedPrograms += 1 },
    useProgram: () => {},
    getUniformLocation: (_prog: unknown, name: string) => ({ name }),
    uniform1f: (loc: { name: string } | null, v: number) => { if (loc) uniforms.set(loc.name, v) },
    uniform1i: (loc: { name: string } | null, v: number) => { if (loc) uniforms.set(loc.name, v) },
    uniform2f: (loc: { name: string } | null, x: number, y: number) => { if (loc) uniforms.set(loc.name, [x, y]) },
    createVertexArray: () => ({ id: nextId++ }),
    bindVertexArray: () => {},
    deleteVertexArray: () => { deletedVaos += 1 },
    createBuffer: () => ({ id: nextId++ }),
    bindBuffer: () => {},
    deleteBuffer: () => { deletedBuffers += 1 },
    bufferData: (_target: number, data: ArrayBufferView | null) => {
      if (data instanceof Float32Array) lastUploadedBuffer = data.slice()
    },
    bufferSubData: (_target: number, _off: number, data: ArrayBufferView) => {
      if (data instanceof Float32Array) lastUploadedBuffer = data.slice()
    },
    enableVertexAttribArray: () => {},
    vertexAttribPointer: () => {},
    vertexAttribDivisor: () => {},
    createTexture: () => ({ id: nextId++ }),
    deleteTexture: () => { deletedTextures += 1 },
    activeTexture: () => {},
    bindTexture: (_target: number, tex: unknown) => { boundTexture = tex },
    texParameteri: () => {},
    pixelStorei: () => {},
    texImage2D: () => {},
    texSubImage2D: () => { texSubUploads += 1 },
    enable: () => {},
    blendFunc: () => {},
    viewport: () => {},
    clearColor: () => {},
    clear: () => {},
    drawArraysInstanced: (_mode: number, _first: number, _count: number, instanceCount: number) => {
      drawCalls.push({
        count: instanceCount,
        instances: lastUploadedBuffer.slice(0, instanceCount * INSTANCE_FLOATS),
        texture: boundTexture,
      })
    },
  }

  const canvas = document.createElement('canvas')
  canvas.getContext = ((kind: string) => {
    if (kind === 'webgl2') return gl
    return null
  }) as unknown as HTMLCanvasElement['getContext']

  return {
    canvas,
    gl,
    uniforms,
    drawCalls,
    setContextLost: (lost: boolean) => { contextLostFlag = lost },
    stats: () => ({ texSubUploads, deletedTextures, deletedBuffers, deletedVaos, deletedPrograms }),
  }
}

function makeScratchCanvas(coloredEmoji = false) {
  const rec = recordingContext(10)
  const ctx = rec.canvas.getContext('2d') as unknown as Record<string, unknown>
  ctx.clearRect = () => {}
  ctx.getImageData = (_x: number, _y: number, w: number, h: number) => {
    const data = new Uint8ClampedArray(Math.max(1, w * h * 4))
    if (coloredEmoji) {
      data[0] = 255
      data[1] = 80
      data[2] = 0
      data[3] = 255
    } else {
      data[0] = 255
      data[1] = 255
      data[2] = 255
      data[3] = 255
    }
    return { data, width: w, height: h }
  }
  return rec.canvas
}

describe('ShelfPacker', () => {
  it('packs rectangles onto shelves with 1px padding and returns null when full', () => {
    const packer = new ShelfPacker(64)
    const a = packer.allocate(20, 20)
    const b = packer.allocate(20, 20)
    // Each padded slot is 22px wide -> 2 fit on row 0 (44px), 3rd wraps to row 1 (y=23)
    const c = packer.allocate(20, 20)
    expect(a).toEqual({ x: 1, y: 1 })
    expect(b).toEqual({ x: 23, y: 1 })
    expect(c).toEqual({ x: 1, y: 23 })

    // Fill remaining space until overflow
    expect(packer.allocate(20, 20)).toEqual({ x: 23, y: 23 })
    expect(packer.allocate(20, 20)).toBeNull()

    packer.reset()
    expect(packer.allocate(20, 20)).toEqual({ x: 1, y: 1 })
  })
})

describe('WebGL2AtlasPainter & createPainter', () => {
  it('batches backgrounds, glyphs, curly underlines, and cursor into 2 instanced draw calls with fractional scroll uniform', () => {
    const mock = mockWebGL2Canvas()
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock.canvas, pixelRatio: 1, scratchCanvas: () => makeScratchCanvas(false) },
    )
    painter.resize(20, 4, 1, 6)

    const r0 = drawRow(
      packRow({
        line: 0,
        styles: [
          {},
          { bg: packedRgb(0x334455), flags: (3 << 2) /* curly underline */ },
        ],
        runs: [{ colStart: 0, colLen: 5, text: 'hello', styleIdx: 1 }],
      }),
      theme,
    )
    const scene: Scene = {
      theme,
      rows: [r0, null, null, null],
      offset: 0.25,
      marks: [{ row: 0, color: 0x00ff00 }],
      cursor: { column: 1, row: 0, cells: 1, shape: 'block', text: { text: 'e', bold: false, italic: false } },
    }
    painter.paint(scene)

    // Pass 1 (gutter mark + row background) + Pass 2 (5 glyphs + curly wave + block cursor + cursor text) = 2 draw calls!
    expect(painter.drawCallsLastFrame).toBe(2)
    expect(mock.drawCalls).toHaveLength(2)

    // Fractional scroll uniform: 0.25 * cellHeight(20) = 5.0px
    expect(mock.uniforms.get('uOffsetPx')).toBe(5)
    expect(mock.uniforms.get('uViewportPx')).toEqual([206, 80])

    // Pass 1: 2 solid rects (1 gutter mark + 1 background run)
    const pass1 = mock.drawCalls[0]!
    expect(pass1.count).toBe(2)
    expect(pass1.instances[12]).toBe(DrawMode.SOLID)

    // Pass 2: 5 ASCII glyphs ('h','e','l','l','o') + 1 curly underline + 1 cursor rect + 1 cursor glyph = 8 instances
    const pass2 = mock.drawCalls[1]!
    expect(pass2.count).toBe(8)
    const modes: number[] = []
    for (let i = 0; i < pass2.count; i += 1) modes.push(pass2.instances[i * INSTANCE_FLOATS + 12]!)
    expect(modes).toEqual([
      DrawMode.MONO_GLYPH,
      DrawMode.MONO_GLYPH,
      DrawMode.MONO_GLYPH,
      DrawMode.MONO_GLYPH,
      DrawMode.MONO_GLYPH,
      DrawMode.CURLY_WAVE,
      DrawMode.SOLID,
      DrawMode.MONO_GLYPH,
    ])

    // Unique glyphs cached in atlas: 'h', 'e', 'l', 'o' = 4 uploads (repeated 'l' and cursor 'e' hit cache!)
    expect(painter.atlasEntryCount).toBe(4)
    expect(mock.stats().texSubUploads).toBe(4)

    // Repainting same scene uploads ZERO new glyphs
    painter.paint(scene)
    expect(mock.stats().texSubUploads).toBe(4)

    painter.dispose()
    const st = mock.stats()
    expect(st.deletedTextures).toBeGreaterThanOrEqual(1)
    expect(st.deletedBuffers).toBe(2)
    expect(st.deletedVaos).toBe(1)
    expect(st.deletedPrograms).toBe(1)
  })

  it('auto-resets and repacks the glyph atlas cleanly when full', () => {
    const mock = mockWebGL2Canvas()
    // Tiny 64x64 atlas with 10x20 cells (12x22 padded) holds 5 cols * 2 rows = 10 glyphs max
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock.canvas, pixelRatio: 1, atlasSize: 64, scratchCanvas: () => makeScratchCanvas(false) },
    )
    painter.resize(20, 2, 1)

    // Frame 1 fills 9 glyphs ('a'..'i') -> fits with 0 resets
    painter.paint({ theme, rows: [drawRow(row(0, 'abcdefghi'), theme), null], offset: 0 })
    expect(painter.atlasResets).toBe(0)
    expect(painter.atlasEntryCount).toBe(9)

    // Frame 2 introduces 5 brand-new glyphs ('1'..'5') -> triggers 1 automatic atlas reset & repacks only '1'..'5'!
    painter.paint({ theme, rows: [drawRow(row(0, '12345'), theme), null], offset: 0 })
    expect(painter.atlasResets).toBe(1)
    expect(painter.atlasEntryCount).toBe(5)
    painter.dispose()
  })

  it('detects color emoji via COLOR_TEX mode and renders image placements above and below text', () => {
    const mock = mockWebGL2Canvas()
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock.canvas, pixelRatio: 1, scratchCanvas: () => makeScratchCanvas(true) },
    )
    painter.resize(10, 2, 1)

    const pic = document.createElement('canvas')
    pic.width = 40
    pic.height = 20

    const r0 = drawRow(
      packRow({
        line: 0,
        styles: [{}, { flags: PackedStyleFlag.BOLD }],
        runs: [{ colStart: 0, colLen: 2, text: '🔥', flags: RunFlag.PER_CELL, styleIdx: 1 }],
      }),
      theme,
    )
    painter.paint({
      theme,
      rows: [r0, null],
      offset: 0,
      images: [
        { picture: pic, x: 0, y: 0, width: 20, height: 20, source: { x: 0, y: 0, width: 40, height: 20 }, above: false },
        { picture: pic, x: 20, y: 0, width: 20, height: 20, source: { x: 0, y: 0, width: 40, height: 20 }, above: true },
      ],
    })

    // Under-text image + glyph pass + above-text image = 3 draw calls
    expect(painter.drawCallsLastFrame).toBe(3)
    // Color emoji rendered with DrawMode.COLOR_TEX
    expect(mock.drawCalls[1]!.instances[12]).toBe(DrawMode.COLOR_TEX)
    // Image texture reused across both placements and freed on dispose
    painter.dispose()
    expect(mock.stats().deletedTextures).toBe(2) // 1 atlas + 1 image texture
  })

  it('handles webglcontextlost / webglcontextrestored cleanly and fires whenCleared', () => {
    const mock = mockWebGL2Canvas()
    let lostNotified = 0
    let clearedNotified = 0
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      {
        canvas: mock.canvas,
        pixelRatio: 1,
        scratchCanvas: () => makeScratchCanvas(false),
        onContextLost: () => { lostNotified += 1 },
      },
    )
    painter.whenCleared(() => { clearedNotified += 1 })
    painter.resize(10, 2, 1)

    const lostEvent = new Event('webglcontextlost', { cancelable: true })
    mock.setContextLost(true)
    mock.canvas.dispatchEvent(lostEvent)
    expect(lostEvent.defaultPrevented).toBe(true)
    expect(painter.isContextLost).toBe(true)
    expect(lostNotified).toBe(1)

    // Painting while context is lost is a safe no-op
    mock.drawCalls.length = 0
    painter.paint({ theme, rows: [drawRow(row(0, 'hi'), theme), null], offset: 0 })
    expect(mock.drawCalls).toHaveLength(0)

    // Context restored rebuilds GPU state and triggers whenCleared repaint callback
    mock.setContextLost(false)
    mock.canvas.dispatchEvent(new Event('webglcontextrestored'))
    expect(painter.isContextLost).toBe(false)
    expect(clearedNotified).toBe(1)

    painter.paint({ theme, rows: [drawRow(row(0, 'hi'), theme), null], offset: 0 })
    expect(mock.drawCalls.length).toBeGreaterThan(0)
    painter.dispose()
  })

  it('throws WebGL2UnavailableError when WebGL2 is absent and createPainter falls back to CanvasPainter', () => {
    const noWebGLCanvas = document.createElement('canvas')
    noWebGLCanvas.getContext = (() => null) as unknown as HTMLCanvasElement['getContext']
    expect(() => new WebGL2AtlasPainter({ family: 'monospace', size: 14 }, { canvas: noWebGLCanvas })).toThrow(
      WebGL2UnavailableError,
    )

    // createPainter falls back cleanly to CanvasPainter when webgl2 returns null
    const origCreate = document.createElement.bind(document)
    const fallbackRec = recordingContext(10)
    let created = 0
    const spyCreate = ((tag: string) => {
      if (tag === 'canvas') {
        created += 1
        // 1st canvas: no webgl2; 2nd canvas: 2D context for CanvasPainter fallback
        return created === 1 ? noWebGLCanvas : fallbackRec.canvas
      }
      return origCreate(tag)
    }) as typeof document.createElement

    const prevCreate = document.createElement
    document.createElement = spyCreate
    try {
      const p = createPainter({ family: 'monospace', size: 14 })
      expect(p).toBeInstanceOf(CanvasPainter)
      p.dispose()
    } finally {
      document.createElement = prevCreate
    }
  })
})
