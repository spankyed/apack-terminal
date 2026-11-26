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

function mockWebGL2Canvas(options: { readonly maxTextureSize?: number; readonly throwOnImageUpload?: boolean } = {}) {
  const uniforms = new Map<string, number | readonly number[]>()
  const drawCalls: MockDrawCall[] = []
  const premultiplyAtUpload: number[] = []
  let texImage2dCalls = 0
  let texSubUploads = 0
  let deletedTextures = 0
  let deletedBuffers = 0
  let deletedVaos = 0
  let deletedPrograms = 0
  let loseContextCalls = 0
  let unpackPremultiply = 0
  let boundTexture: unknown = null
  let lastUploadedBuffer = new Float32Array(0)
  let contextLostFlag = false
  let failCreateProgram = false
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
    getExtension: (name: string) =>
      name === 'WEBGL_lose_context' ? { loseContext: () => { loseContextCalls += 1 } } : null,
    createShader: () => ({ id: nextId++ }),
    shaderSource: () => {},
    compileShader: () => {},
    getShaderParameter: () => true,
    getShaderInfoLog: () => '',
    deleteShader: () => {},
    createProgram: () => (failCreateProgram ? null : { id: nextId++ }),
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
    pixelStorei: (pname: number, param: number) => {
      if (pname === 0x9241) unpackPremultiply = param
    },
    texImage2D: (...args: unknown[]) => {
      texImage2dCalls += 1
      if (args.length === 6) {
        if (options.throwOnImageUpload) throw new Error('InvalidStateError: closed ImageBitmap')
        premultiplyAtUpload.push(unpackPremultiply)
      }
    },
    texSubImage2D: () => {
      texSubUploads += 1
      premultiplyAtUpload.push(unpackPremultiply)
    },
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
    premultiplyAtUpload,
    setContextLost: (lost: boolean) => { contextLostFlag = lost },
    setFailCreateProgram: (fail: boolean) => { failCreateProgram = fail },
    stats: () => ({ texImage2dCalls, texSubUploads, deletedTextures, deletedBuffers, deletedVaos, deletedPrograms, loseContextCalls }),
  }
}

function makeScratchCanvas(pixelMode: 'white' | 'chromatic' | 'grayscaleEmoji' = 'white') {
  const rec = recordingContext(10)
  let getImageDataCalls = 0
  const ctx = rec.canvas.getContext('2d') as unknown as Record<string, unknown>
  ctx.clearRect = () => {}
  ctx.getImageData = (_x: number, _y: number, w: number, h: number) => {
    getImageDataCalls += 1
    const data = new Uint8ClampedArray(Math.max(1, w * h * 4))
    if (pixelMode === 'chromatic') {
      data[0] = 255
      data[1] = 80
      data[2] = 0
      data[3] = 255
    } else if (pixelMode === 'grayscaleEmoji') {
      // Dark gray/black emoji like 🐼 or ⚫ (r===g===b < 245)
      data[0] = 40
      data[1] = 40
      data[2] = 40
      data[3] = 255
    } else {
      data[0] = 255
      data[1] = 255
      data[2] = 255
      data[3] = 255
    }
    return { data, width: w, height: h }
  }
  return { canvas: rec.canvas, getImageDataCalls: () => getImageDataCalls }
}

describe('ShelfPacker', () => {
  it('packs rectangles with 1px padding and rejects oversized rectangles cleanly', () => {
    const packer = new ShelfPacker(64)
    // Oversized rectangle (needs 64 + 2 = 66 > 64) must return null, never clamp OOB!
    expect(packer.allocate(64, 20)).toBeNull()
    expect(packer.allocate(20, 64)).toBeNull()

    const a = packer.allocate(20, 20)
    const b = packer.allocate(20, 20)
    const c = packer.allocate(20, 20)
    expect(a).toEqual({ x: 1, y: 1 })
    expect(b).toEqual({ x: 23, y: 1 })
    expect(c).toEqual({ x: 1, y: 23 })
    expect(packer.allocate(20, 20)).toEqual({ x: 23, y: 23 })
    expect(packer.allocate(20, 20)).toBeNull()

    packer.reset()
    expect(packer.allocate(20, 20)).toEqual({ x: 1, y: 1 })
  })
})

describe('WebGL2AtlasPainter & createPainter', () => {
  it('batches backgrounds, ASCII glyphs (0 getImageData), shader-driven curly/dotted lines, and cursor in 2 draw calls', () => {
    const mock = mockWebGL2Canvas()
    const scratch = makeScratchCanvas('white')
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock.canvas, pixelRatio: 1, scratchCanvas: () => scratch.canvas },
    )
    painter.resize(20, 4, 1, 6)

    const r0 = drawRow(
      packRow({
        line: 0,
        styles: [
          {},
          { bg: packedRgb(0x334455), flags: (3 << 2) /* curly */ },
          { flags: (4 << 2) /* dotted */ },
        ],
        runs: [
          { colStart: 0, colLen: 5, text: 'hello', styleIdx: 1 },
          { colStart: 5, colLen: 10, text: '0123456789', styleIdx: 2 },
        ],
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

    // 2 draw calls total: Pass 1 (gutter+bg) + Pass 2 (glyphs + 1 curly + 1 dotted + cursor + cursor glyph)
    expect(painter.drawCallsLastFrame).toBe(2)
    expect(mock.drawCalls).toHaveLength(2)
    // ZERO getImageData calls for pure ASCII!
    expect(scratch.getImageDataCalls()).toBe(0)
    // All atlas uploads used UNPACK_PREMULTIPLY_ALPHA_WEBGL = 1
    expect(mock.premultiplyAtUpload.every((v) => v === 1)).toBe(true)
    expect(mock.uniforms.get('uOffsetPx')).toBe(5)
    expect(mock.uniforms.get('uPixelRatio')).toBe(1)
    expect(mock.uniforms.get('uViewportPx')).toEqual([206, 80])

    // Pass 2: 15 glyphs + 1 curly + 1 dotted (1 quad, NOT per-dot!) + 1 cursor + 1 cursor glyph = 19 instances
    const pass2 = mock.drawCalls[1]!
    expect(pass2.count).toBe(19)
    const modes: number[] = []
    for (let i = 0; i < pass2.count; i += 1) modes.push(pass2.instances[i * INSTANCE_FLOATS + 12]!)
    expect(modes.slice(15)).toEqual([
      DrawMode.CURLY_WAVE,
      DrawMode.DASHED_LINE,
      DrawMode.SOLID,
      DrawMode.MONO_GLYPH,
    ])

    // Repainting same scene uploads ZERO new glyphs
    const uploadsBefore = mock.stats().texSubUploads
    painter.paint(scene)
    expect(mock.stats().texSubUploads).toBe(uploadsBefore)

    painter.dispose()
    const st = mock.stats()
    expect(st.deletedTextures).toBeGreaterThanOrEqual(1)
    expect(st.deletedBuffers).toBe(2)
    expect(st.deletedVaos).toBe(1)
    expect(st.deletedPrograms).toBe(1)
    expect(st.loseContextCalls).toBe(1)
    expect(scratch.canvas.width).toBe(0)
    expect(scratch.canvas.height).toBe(0)
  })

  it('auto-resets atlas when stale entries overflow, and avoids thrashing on an already-empty atlas', () => {
    const mock = mockWebGL2Canvas()
    const scratch = makeScratchCanvas('white')
    // 64x64 atlas with padded slots (~18x28) holds ~6 glyphs max
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock.canvas, pixelRatio: 1, atlasSize: 64, scratchCanvas: () => scratch.canvas },
    )
    painter.resize(20, 2, 1)

    // Frame 1 with 3 glyphs fits with 0 resets
    painter.paint({ theme, rows: [drawRow(row(0, 'abc'), theme), null], offset: 0 })
    expect(painter.atlasResets).toBe(0)
    expect(painter.atlasEntryCount).toBe(3)

    // Frame 2 with 5 new glyphs overflows stale entries -> 1 reset & repacks!
    painter.paint({ theme, rows: [drawRow(row(0, '12345'), theme), null], offset: 0 })
    expect(painter.atlasResets).toBe(1)

    // Single frame exceeding atlas capacity flushes + resets in batches so ZERO glyphs are dropped,
    // without thrashing per-glyph (26 glyphs in a 6-slot atlas => ~4 resets, not 20!).
    const mock2 = mockWebGL2Canvas()
    const painter2 = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock2.canvas, pixelRatio: 1, atlasSize: 64, scratchCanvas: () => makeScratchCanvas('white').canvas },
    )
    painter2.resize(30, 2, 1)
    painter2.paint({ theme, rows: [drawRow(row(0, 'abcdefghijklmnopqrstuvwxyz'), theme), null], offset: 0 })
    expect(painter2.atlasResets).toBeGreaterThanOrEqual(3)
    expect(painter2.atlasResets).toBeLessThanOrEqual(6)
    expect(mock2.drawCalls.length).toBeGreaterThanOrEqual(painter2.atlasResets + 1)

    painter.dispose()
    painter2.dispose()
  })

  it('detects grayscale color emoji, batches same-texture Kitty slices, evicts image textures LRU, and draws cursor above above:true images', () => {
    const mock = mockWebGL2Canvas()
    const scratch = makeScratchCanvas('grayscaleEmoji')
    // Cap image texture cache to 2048 bytes (each 16x16 RGBA canvas = 1024 bytes => holds at most 2)
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      {
        canvas: mock.canvas,
        pixelRatio: 1,
        maxImageTextureBytes: 2048,
        scratchCanvas: () => scratch.canvas,
      },
    )
    painter.resize(10, 2, 1)

    const mkPic = () => {
      const c = document.createElement('canvas')
      c.width = 16
      c.height = 16
      return c
    }
    const p1 = mkPic()
    const p2 = mkPic()
    const p3 = mkPic()

    const r0 = drawRow(
      packRow({
        line: 0,
        styles: [{}, { flags: PackedStyleFlag.BOLD }],
        runs: [{ colStart: 0, colLen: 2, text: '🐼', flags: RunFlag.PER_CELL, styleIdx: 1 }],
      }),
      theme,
    )

    // Two consecutive under-text slices of p1 MUST batch into 1 draw call; cursor draws AFTER above-text p2!
    painter.paint({
      theme,
      rows: [r0, null],
      offset: 0,
      cursor: { column: 0, row: 0, cells: 1, shape: 'bar' },
      images: [
        { picture: p1, x: 0, y: 0, width: 16, height: 10, source: { x: 0, y: 0, width: 16, height: 8 }, above: false },
        { picture: p1, x: 0, y: 10, width: 16, height: 10, source: { x: 0, y: 8, width: 16, height: 8 }, above: false },
        { picture: p2, x: 20, y: 0, width: 16, height: 16, source: { x: 0, y: 0, width: 16, height: 16 }, above: true },
      ],
    })

    // Draw calls: [0] 2 batched p1 slices, [1] grayscale emoji 🐼 (COLOR_TEX), [2] above p2, [3] cursor on top!
    expect(painter.drawCallsLastFrame).toBe(4)
    expect(mock.drawCalls[0]!.count).toBe(2)
    expect(mock.drawCalls[1]!.instances[12]).toBe(DrawMode.COLOR_TEX)
    expect(mock.drawCalls[3]!.instances[12]).toBe(DrawMode.SOLID) // cursor drawn AFTER above:true image!

    // Adding p3 evicts oldest texture (p1) via gl.deleteTexture to stay <= 2048 bytes
    painter.paint({
      theme,
      rows: [r0, null],
      offset: 0,
      images: [
        { picture: p3, x: 0, y: 0, width: 16, height: 16, source: { x: 0, y: 0, width: 16, height: 16 }, above: false },
      ],
    })
    expect(painter.imageTextureCount).toBe(2)
    expect(painter.imageTextureBytes).toBeLessThanOrEqual(2048)
    expect(mock.stats().deletedTextures).toBe(1)

    painter.dispose()
  })

  it('survives texImage2D exceptions without leaking texture or aborting text rendering', () => {
    const mock = mockWebGL2Canvas({ throwOnImageUpload: true })
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      { canvas: mock.canvas, pixelRatio: 1, scratchCanvas: () => makeScratchCanvas('white').canvas },
    )
    painter.resize(10, 2, 1)
    const pic = document.createElement('canvas')
    pic.width = 16
    pic.height = 16

    painter.paint({
      theme,
      rows: [drawRow(row(0, 'safe'), theme), null],
      offset: 0,
      images: [
        { picture: pic, x: 0, y: 0, width: 16, height: 16, source: { x: 0, y: 0, width: 16, height: 16 }, above: false },
      ],
    })
    // Failed image texture was deleted immediately and Pass 2 text ('safe') still drew!
    expect(mock.stats().deletedTextures).toBe(1)
    expect(painter.drawCallsLastFrame).toBe(1)
    expect(mock.drawCalls[0]!.count).toBe(4)
    painter.dispose()
  })

  it('handles webglcontextlost, failed restore retry, and successful webglcontextrestored cleanly', () => {
    const mock = mockWebGL2Canvas()
    let lostNotified = 0
    let clearedNotified = 0
    const painter = new WebGL2AtlasPainter(
      { family: 'monospace', size: 14 },
      {
        canvas: mock.canvas,
        pixelRatio: 1,
        scratchCanvas: () => makeScratchCanvas('white').canvas,
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

    // Simulate failed restore (driver loses context mid-init): stays contextLost, does NOT throw uncaught
    mock.setFailCreateProgram(true)
    mock.setContextLost(false)
    mock.canvas.dispatchEvent(new Event('webglcontextrestored'))
    expect(painter.isContextLost).toBe(true)
    expect(clearedNotified).toBe(0)

    // Subsequent successful restore recovers, allocates atlas texture immediately, and fires whenCleared
    mock.setFailCreateProgram(false)
    const texUploadsBefore = mock.stats().texImage2dCalls
    mock.canvas.dispatchEvent(new Event('webglcontextrestored'))
    expect(painter.isContextLost).toBe(false)
    expect(clearedNotified).toBe(1)
    expect(mock.stats().texImage2dCalls).toBeGreaterThan(texUploadsBefore)

    painter.paint({ theme, rows: [drawRow(row(0, 'hi'), theme), null], offset: 0 })
    expect(mock.drawCalls.length).toBeGreaterThan(0)
    expect(painter.atlasEntryCount).toBe(2)
    painter.dispose()
  })

  it('throws WebGL2UnavailableError when WebGL2 is absent and createPainter falls back to CanvasPainter', () => {
    const noWebGLCanvas = document.createElement('canvas')
    noWebGLCanvas.getContext = (() => null) as unknown as HTMLCanvasElement['getContext']
    expect(() => new WebGL2AtlasPainter({ family: 'monospace', size: 14 }, { canvas: noWebGLCanvas })).toThrow(
      WebGL2UnavailableError,
    )

    const origCreate = document.createElement.bind(document)
    const fallbackRec = recordingContext(10)
    let created = 0
    const spyCreate = ((tag: string) => {
      if (tag === 'canvas') {
        created += 1
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
