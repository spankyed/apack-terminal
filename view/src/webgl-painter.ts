/**
 * Hardware-accelerated WebGL2 instanced glyph-atlas painter implementing the exact same `Painter`
 * interface as `CanvasPainter`.
 *
 * Architecture:
 * - **Dynamic shelf-packed 2D glyph atlas (`RGBA8`, premultiplied alpha):** rasterizes unique
 *   `(face, cells, text)` glyphs on a small offscreen 2D canvas with overhang bearing margins and
 *   1px transparent padding, uploading via WebGL2 9-arg `texSubImage2D(..., w, h, scratch)` (zero
 *   `getImageData` for ASCII; single scan for non-ASCII detecting both chromatic and grayscale color
 *   emoji). When full, clears and repacks automatically in-frame.
 * - **Single `Float32Array` instanced draw buffer (`drawArraysInstanced`):** packs backgrounds,
 *   gutter marks, glyphs, straight/dotted/dashed/curly decorations (all shader-driven: 1 quad per
 *   run!), and cursor into a reusable growable `Float32Array` (13 floats/instance = 52 bytes),
 *   drawing a normal frame in 1–2 WebGL draw calls with zero per-frame JS allocations.
 * - **Fractional scroll uniform (`uOffsetPx`):** smooth sub-row trackpad scrolling directly in the
 *   vertex shader at exact 1:1 device-pixel scale.
 * - **Bounded GPU image texture LRU (`maxImageTextureBytes = 64 MiB`):** batches consecutive
 *   same-texture Kitty placeholder slices, evicts LRU textures with `gl.deleteTexture`, and guards
 *   against closed/tainted image sources.
 * - **Context loss resilience:** listens to `webglcontextlost` / `webglcontextrestored`, cleans up
 *   partial GPU state on errors, and releases contexts cleanly via `WEBGL_lose_context` on dispose.
 */
import { CanvasPainter } from './canvas-painter.ts'
import type { GlyphRun, LineKind } from './draw.ts'
import { fontOf, measure, type Font, type Metrics, type Painter, type Scene } from './painter.ts'

/** 1px transparent border around every atlas slot to prevent texture bleed. */
const ATLAS_PAD = 1

/** Default glyph atlas dimension in device pixels (clamped to `GL_MAX_TEXTURE_SIZE`). */
const DEFAULT_ATLAS_SIZE = 2048

/** Minimum supported atlas dimension (also used in unit tests exercising overflow/repack). */
const MIN_ATLAS_SIZE = 64

/** Default GPU VRAM cap for cached image placement textures (64 MiB). */
const DEFAULT_MAX_IMAGE_TEX_BYTES = 64 * 1024 * 1024

/** Floats per instance: `aDest(4) + aUv(4) + aColor(4) + aMode(1) = 13`. */
export const INSTANCE_FLOATS = 13
const INSTANCE_STRIDE_BYTES = INSTANCE_FLOATS * 4

/** Shader rendering modes packed into `aMode`. */
export const DrawMode = {
  SOLID: 0,
  MONO_GLYPH: 1,
  COLOR_TEX: 2,
  CURLY_WAVE: 3,
  DASHED_LINE: 4,
} as const

/** Error thrown when `WebGL2AtlasPainter` cannot acquire a WebGL2 context. */
export class WebGL2UnavailableError extends Error {
  constructor(message = 'WebGL2 context is unavailable on this canvas') {
    super(message)
    this.name = 'WebGL2UnavailableError'
  }
}

export interface WebGL2PainterOptions {
  readonly canvas?: HTMLCanvasElement
  readonly pixelRatio?: number
  /** Glyph atlas texture size in pixels (square, clamped to `[64, MAX_TEXTURE_SIZE]`). Default `2048`. */
  readonly atlasSize?: number
  /** Maximum GPU bytes retained across cached image placement textures (default `64 MiB`). */
  readonly maxImageTextureBytes?: number
  /** Optional custom offscreen 2D canvas factory (used for glyph rasterization & font metrics). */
  readonly scratchCanvas?: () => HTMLCanvasElement
  /** Optional callback fired when WebGL2 context is lost. */
  readonly onContextLost?: () => void
}

export interface CreatePainterOptions extends WebGL2PainterOptions {
  /** Prefer `WebGL2AtlasPainter` when available (default `true`). Set `false` to force `CanvasPainter`. */
  readonly preferWebGL?: boolean
}

/**
 * Creates the best available `Painter` for the current environment: `WebGL2AtlasPainter` when WebGL2
 * is available (and `preferWebGL !== false`), seamlessly falling back to `CanvasPainter` on a clean
 * canvas otherwise.
 */
export function createPainter(font: Font, options: CreatePainterOptions = {}): Painter {
  const ratio = options.pixelRatio ?? (globalThis.devicePixelRatio || 1)
  if (options.preferWebGL !== false) {
    const glCanvas = options.canvas ?? document.createElement('canvas')
    try {
      return new WebGL2AtlasPainter(font, { ...options, canvas: glCanvas, pixelRatio: ratio })
    } catch (err) {
      if (!(err instanceof WebGL2UnavailableError)) throw err
      // HTML Canvas spec forbids requesting '2d' on a canvas whose 'webgl2' slot was locked;
      // always allocate a fresh canvas for the 2D fallback.
      return new CanvasPainter(font, document.createElement('canvas'), ratio)
    }
  }
  return new CanvasPainter(font, options.canvas ?? document.createElement('canvas'), ratio)
}

export interface AtlasEntry {
  readonly x: number
  readonly y: number
  readonly w: number
  readonly h: number
  /** Left/top overhang padding in device pixels subtracted from draw position. */
  readonly bearingX: number
  readonly bearingY: number
  readonly u0: number
  readonly v0: number
  readonly u1: number
  readonly v1: number
  readonly colored: boolean
}

interface Shelf {
  readonly y: number
  readonly height: number
  cursorX: number
}

/**
 * Pure shelf-packing allocator for a square `size × size` 2D glyph atlas texture.
 */
export class ShelfPacker {
  private readonly shelves: Shelf[] = []
  private nextY = 0

  constructor(readonly size: number) {}

  reset(): void {
    this.shelves.length = 0
    this.nextY = 0
  }

  /** Allocates a `(w, h)` rectangle (including `ATLAS_PAD` border on all sides), or `null` if it doesn't fit. */
  allocate(w: number, h: number): { readonly x: number; readonly y: number } | null {
    const pw = Math.max(1, Math.ceil(w)) + ATLAS_PAD * 2
    const ph = Math.max(1, Math.ceil(h)) + ATLAS_PAD * 2
    if (pw > this.size || ph > this.size) return null

    for (const shelf of this.shelves) {
      if (ph <= shelf.height && shelf.height <= ph * 2 && shelf.cursorX + pw <= this.size) {
        const x = shelf.cursorX + ATLAS_PAD
        const y = shelf.y + ATLAS_PAD
        shelf.cursorX += pw
        return { x, y }
      }
    }

    if (this.nextY + ph > this.size) return null
    const shelf: Shelf = { y: this.nextY, height: ph, cursorX: pw }
    this.shelves.push(shelf)
    this.nextY += ph
    return { x: ATLAS_PAD, y: shelf.y + ATLAS_PAD }
  }
}

const VERTEX_SHADER_SRC = `#version 300 es
precision highp float;

layout(location = 0) in vec2 aCorner;
layout(location = 1) in vec4 aDest;
layout(location = 2) in vec4 aUv;
layout(location = 3) in vec4 aColor;
layout(location = 4) in float aMode;

uniform vec2 uViewportPx;
uniform float uOffsetPx;

out vec2 vUv;
out vec2 vLocal;
out vec4 vColor;
out float vMode;
out vec2 vDestSize;
out vec4 vParams;

void main() {
  vec2 posPx = vec2(aDest.x, aDest.y - uOffsetPx) + aCorner * aDest.zw;
  vec2 clip = (posPx / uViewportPx) * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  vUv = mix(aUv.xy, aUv.zw, aCorner);
  vLocal = aCorner;
  vColor = aColor;
  vMode = aMode;
  vDestSize = aDest.zw;
  vParams = aUv;
}
`

const FRAGMENT_SHADER_SRC = `#version 300 es
precision highp float;

uniform sampler2D uAtlas;
uniform float uCellWidth;
uniform float uPixelRatio;

in vec2 vUv;
in vec2 vLocal;
in vec4 vColor;
in float vMode;
in vec2 vDestSize;
in vec4 vParams;

out vec4 outColor;

void main() {
  if (vMode < 0.5) {
    // Mode 0: Solid color rect (background, gutter mark, straight line, cursor)
    outColor = vec4(vColor.rgb * vColor.a, vColor.a);
  } else if (vMode < 1.5) {
    // Mode 1: Monochrome glyph tinted by aColor (premultiplied alpha)
    float cov = texture(uAtlas, vUv).a;
    float a = cov * vColor.a;
    if (a <= 0.001) discard;
    outColor = vec4(vColor.rgb * a, a);
  } else if (vMode < 2.5) {
    // Mode 2: Full-color premultiplied RGBA texture (color emoji or image placement)
    vec4 tex = texture(uAtlas, vUv);
    float a = tex.a * vColor.a;
    if (a <= 0.001) discard;
    outColor = vec4(tex.rgb * vColor.a, a);
  } else if (vMode < 3.5) {
    // Mode 3: Anti-aliased curly underline wave (vParams.x = amplitudePx, vParams.y = strokeHalfPx)
    float xPx = vLocal.x * vDestSize.x;
    float yPx = vLocal.y * vDestSize.y;
    float freq = 6.2831853 / max(1.0, uCellWidth);
    float phase = xPx * freq;
    float ampPx = vParams.x;
    float halfStroke = vParams.y;
    float centerY = vDestSize.y * 0.5 + ampPx * sin(phase);
    float slope = ampPx * freq * cos(phase);
    float perpDist = abs(yPx - centerY) / sqrt(1.0 + slope * slope);
    float aa = 1.0 / max(1.0, uPixelRatio);
    float cov = 1.0 - smoothstep(halfStroke - aa * 0.5, halfStroke + aa * 0.5, perpDist);
    float a = cov * vColor.a;
    if (a <= 0.001) discard;
    outColor = vec4(vColor.rgb * a, a);
  } else {
    // Mode 4: Dotted / dashed underline (vParams.x = dashPx, vParams.y = periodPx)
    float xPx = vLocal.x * vDestSize.x;
    if (mod(xPx, max(1.0, vParams.y)) >= vParams.x) discard;
    outColor = vec4(vColor.rgb * vColor.a, vColor.a);
  }
}
`

const UNIT_QUAD = new Float32Array([
  0, 0,
  1, 0,
  0, 1,
  0, 1,
  1, 0,
  1, 1,
])

interface ImageTextureEntry {
  readonly texture: WebGLTexture
  readonly width: number
  readonly height: number
  readonly byteSize: number
  /** Weak reference to the uploaded source so closed/GC'd bitmaps can be swept without pinning them. */
  readonly sourceRef: WeakRef<CanvasImageSource> | null
}

export class WebGL2AtlasPainter implements Painter {
  readonly canvas: HTMLCanvasElement
  metrics: Metrics

  private readonly gl: WebGL2RenderingContext
  private readonly scratch: HTMLCanvasElement
  private readonly scratchCtx: CanvasRenderingContext2D
  private readonly maxImageTexBytes: number
  private readonly onContextLostCallback: (() => void) | undefined

  private font: Font
  private faces: readonly string[]
  private pixelRatio: number
  private columns = 0
  private rows = 0
  private gutter = 0
  private atlasSize: number
  private packer: ShelfPacker
  private readonly atlasEntries = new Map<string, AtlasEntry>()

  // WebGL2 GPU objects (rebuilt cleanly on webglcontextrestored)
  private program: WebGLProgram | null = null
  private vao: WebGLVertexArrayObject | null = null
  private quadBuf: WebGLBuffer | null = null
  private instanceBuf: WebGLBuffer | null = null
  private instanceBufCapacityFloats = 0
  private atlasTex: WebGLTexture | null = null
  private uViewportPx: WebGLUniformLocation | null = null
  private uOffsetPx: WebGLUniformLocation | null = null
  private uAtlas: WebGLUniformLocation | null = null
  private uCellWidth: WebGLUniformLocation | null = null
  private uPixelRatio: WebGLUniformLocation | null = null

  // Bounded LRU cache of image placement textures keyed by content-addressed string key (never pinning
  // `CanvasImageSource` / closed `ImageBitmap`s in a strong Map); unkeyed callers get a WeakMap token.
  private readonly imageTextures = new Map<string, ImageTextureEntry>()
  private readonly fallbackImageKeys = new WeakMap<CanvasImageSource, string>()
  private nextFallbackImageKey = 1
  private imageTexBytes = 0

  // Reusable growable instanced CPU scratch buffer (zero per-frame allocations)
  private instances = new Float32Array(4096 * INSTANCE_FLOATS)
  private instanceCount = 0

  private contextLost = false
  private disposed = false
  private repaint: (() => void) | null = null

  /** Diagnostic counters exposed for tests & telemetry. */
  atlasResets = 0
  drawCallsLastFrame = 0

  constructor(font: Font, options: WebGL2PainterOptions = {}) {
    const canvas = options.canvas ?? document.createElement('canvas')
    const gl = canvas.getContext('webgl2', {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      preserveDrawingBuffer: false,
      powerPreference: 'high-performance',
    })
    if (!gl) {
      throw new WebGL2UnavailableError()
    }

    const scratch = options.scratchCanvas ? options.scratchCanvas() : document.createElement('canvas')
    const scratchCtx = scratch.getContext('2d', { willReadFrequently: true })
    if (!scratchCtx) {
      gl.getExtension?.('WEBGL_lose_context')?.loseContext()
      throw new WebGL2UnavailableError('Offscreen 2D rasterization context unavailable')
    }

    const ratio = options.pixelRatio ?? (globalThis.devicePixelRatio || 1)
    this.pixelRatio = Number.isFinite(ratio) && ratio > 0 ? ratio : 1
    this.canvas = canvas
    this.gl = gl
    this.scratch = scratch
    this.scratchCtx = scratchCtx
    this.font = font
    this.faces = facesOf(font)
    this.metrics = measure(scratchCtx, font, this.pixelRatio)
    this.maxImageTexBytes = Math.max(
      1024,
      Number.isFinite(options.maxImageTextureBytes) ? options.maxImageTextureBytes! : DEFAULT_MAX_IMAGE_TEX_BYTES,
    )
    this.onContextLostCallback = options.onContextLost

    const requested = Number.isFinite(options.atlasSize) ? options.atlasSize! : DEFAULT_ATLAS_SIZE
    const maxTexRaw = Number(gl.getParameter(gl.MAX_TEXTURE_SIZE))
    const maxTex = Number.isFinite(maxTexRaw) && maxTexRaw > 0 ? maxTexRaw : DEFAULT_ATLAS_SIZE
    this.atlasSize = Math.max(MIN_ATLAS_SIZE, Math.min(Math.floor(requested), maxTex))
    this.packer = new ShelfPacker(this.atlasSize)

    try {
      this.initGpu()
    } catch (err) {
      this.releaseGpuObjects()
      gl.getExtension?.('WEBGL_lose_context')?.loseContext()
      throw err
    }

    canvas.addEventListener('webglcontextlost', this.handleContextLost)
    canvas.addEventListener('webglcontextrestored', this.handleContextRestored)
  }

  get isContextLost(): boolean {
    return this.contextLost
  }

  get atlasEntryCount(): number {
    return this.atlasEntries.size
  }

  get imageTextureCount(): number {
    return this.imageTextures.size
  }

  get imageTextureBytes(): number {
    return this.imageTexBytes
  }

  whenCleared(repaint: () => void): void {
    if (this.disposed) return
    this.repaint = repaint
  }

  /** Explicitly deletes any GPU texture cached for content-addressed image `key` (`H14`). */
  releaseImage(key: string): void {
    if (this.disposed) return
    const entry = this.imageTextures.get(key)
    if (!entry) return
    this.imageTextures.delete(key)
    this.imageTexBytes -= entry.byteSize
    if (!this.contextLost) {
      this.gl.deleteTexture(entry.texture)
    }
  }

  resize(columns: number, rows: number, pixelRatio: number, gutter = 0): void {
    if (this.disposed) return
    const ratio = Number.isFinite(pixelRatio) && pixelRatio > 0 ? pixelRatio : 1
    const ratioChanged = ratio !== this.pixelRatio
    const prevW = this.metrics.cellWidth
    const prevH = this.metrics.cellHeight

    this.columns = columns
    this.rows = rows
    this.pixelRatio = ratio
    this.gutter = gutter
    this.metrics = measure(this.scratchCtx, this.font, ratio)

    const width = columns * this.metrics.cellWidth + gutter
    const height = rows * this.metrics.cellHeight
    this.canvas.width = Math.max(1, Math.round(width * ratio))
    this.canvas.height = Math.max(1, Math.round(height * ratio))
    this.canvas.style.width = `${width}px`
    this.canvas.style.height = `${height}px`

    if (!this.contextLost) {
      this.gl.viewport(0, 0, this.canvas.width, this.canvas.height)
    }

    if (ratioChanged || this.metrics.cellWidth !== prevW || this.metrics.cellHeight !== prevH) {
      this.resetAtlas()
    }
  }

  setFont(font: Font): void {
    if (this.disposed) return
    this.font = font
    this.faces = facesOf(font)
    this.metrics = measure(this.scratchCtx, font, this.pixelRatio)
    this.resetAtlas()
    // Recompute canvas CSS/device dimensions with the already-measured font metrics
    const width = this.columns * this.metrics.cellWidth + this.gutter
    const height = this.rows * this.metrics.cellHeight
    this.canvas.width = Math.max(1, Math.round(width * this.pixelRatio))
    this.canvas.height = Math.max(1, Math.round(height * this.pixelRatio))
    this.canvas.style.width = `${width}px`
    this.canvas.style.height = `${height}px`
    if (!this.contextLost) {
      this.gl.viewport(0, 0, this.canvas.width, this.canvas.height)
    }
  }

  paint(scene: Scene): void {
    this.drawCallsLastFrame = 0
    if (this.disposed || this.contextLost || this.gl.isContextLost?.()) return
    if (!this.program || !this.vao || !this.atlasTex) return

    const gl = this.gl
    const { cellWidth, cellHeight } = this.metrics
    // Derive CSS viewport from actual device buffer dimensions so 1 CSS px maps 1:1 to pixelRatio device px
    const viewportW = Math.max(1, this.canvas.width / this.pixelRatio)
    const viewportH = Math.max(1, this.canvas.height / this.pixelRatio)

    gl.viewport(0, 0, this.canvas.width, this.canvas.height)
    const [bgR, bgG, bgB] = rgbFloats(scene.theme.background)
    gl.clearColor(bgR, bgG, bgB, 1)
    gl.clear(gl.COLOR_BUFFER_BIT)

    gl.useProgram(this.program)
    gl.bindVertexArray(this.vao)
    gl.uniform2f(this.uViewportPx, viewportW, viewportH)
    gl.uniform1f(this.uOffsetPx, scene.offset * cellHeight)
    gl.uniform1f(this.uCellWidth, cellWidth)
    gl.uniform1f(this.uPixelRatio, this.pixelRatio)
    gl.uniform1i(this.uAtlas, 0)

    this.ensureSceneGlyphs(scene)
    if (this.imageTextures.size > 0) this.sweepDeadImageTextures()

    // --- Pass 1: Backgrounds + gutter marks ---
    this.instanceCount = 0
    if (this.gutter > 0 && scene.marks) {
      const barW = Math.max(2, this.gutter - 3)
      const barH = Math.max(1, cellHeight - 4)
      for (const mark of scene.marks) {
        const [r, g, b] = rgbFloats(mark.color)
        this.pushSolid(1, mark.row * cellHeight + 2, barW, barH, r, g, b, 1)
      }
    }
    for (let rowIdx = 0; rowIdx < scene.rows.length; rowIdx += 1) {
      const row = scene.rows[rowIdx]
      if (!row) continue
      const y = rowIdx * cellHeight
      for (const bg of row.backgrounds) {
        const [r, g, b] = rgbFloats(bg.color)
        this.pushSolid(this.gutter + bg.column * cellWidth, y, bg.cells * cellWidth, cellHeight, r, g, b, 1)
      }
    }
    if (this.instanceCount > 0) {
      this.flushBatch(this.atlasTex)
    }

    // --- Pass 1b: Images drawn under text (`above === false`) ---
    this.drawImages(scene, false)

    const hasAboveImages =
      scene.images?.some((img) => img.above && img.source.width > 0 && img.source.height > 0) ?? false

    // --- Pass 2: Glyphs + text decorations (+ cursor when no above-text images exist) ---
    this.instanceCount = 0
    for (let rowIdx = 0; rowIdx < scene.rows.length; rowIdx += 1) {
      const row = scene.rows[rowIdx]
      if (!row) continue
      const y = rowIdx * cellHeight
      for (const run of row.glyphs) {
        this.emitGlyphRun(run, y)
      }
      for (const line of row.lines) {
        this.emitDecoration(line.kind, this.gutter + line.column * cellWidth, y, line.cells * cellWidth, line.color)
      }
    }
    if (scene.cursor && !hasAboveImages) {
      this.emitCursor(scene)
    }
    if (this.instanceCount > 0) {
      this.flushBatch(this.atlasTex)
    }

    // --- Pass 3: Images drawn above text (`above === true`) + cursor on top (matching CanvasPainter z-order) ---
    if (hasAboveImages) {
      this.drawImages(scene, true)
      if (scene.cursor) {
        this.instanceCount = 0
        this.emitCursor(scene)
        if (this.instanceCount > 0) {
          this.flushBatch(this.atlasTex)
        }
      }
    }

    gl.bindVertexArray(null)
  }

  dispose(): void {
    if (this.disposed) return
    this.disposed = true
    this.canvas.removeEventListener('webglcontextlost', this.handleContextLost)
    this.canvas.removeEventListener('webglcontextrestored', this.handleContextRestored)
    this.repaint = null
    const wasLost = this.contextLost
    this.releaseGpuObjects()
    if (!wasLost) {
      this.gl.getExtension?.('WEBGL_lose_context')?.loseContext()
    }
    this.atlasEntries.clear()
    this.packer.reset()
    this.scratch.width = 0
    this.scratch.height = 0
    this.canvas.width = 0
    this.canvas.height = 0
    this.canvas.remove()
  }

  private readonly handleContextLost = (event: Event) => {
    event.preventDefault()
    this.contextLost = true
    this.program = null
    this.vao = null
    this.quadBuf = null
    this.instanceBuf = null
    this.instanceBufCapacityFloats = 0
    this.atlasTex = null
    this.atlasEntries.clear()
    this.packer.reset()
    this.imageTextures.clear()
    this.imageTexBytes = 0
    this.onContextLostCallback?.()
  }

  private readonly handleContextRestored = () => {
    if (this.disposed) return
    // Clear `contextLost` BEFORE `initGpu()` so `resetAtlas()` allocates `atlasTex` immediately.
    this.contextLost = false
    try {
      this.initGpu()
    } catch {
      // Context lost again or driver refused rebuild; remain in contextLost state safely
      this.releaseGpuObjects()
      this.contextLost = true
      return
    }
    if (this.columns > 0 && this.rows > 0) {
      this.gl.viewport(0, 0, this.canvas.width, this.canvas.height)
    }
    this.repaint?.()
  }

  private initGpu(): void {
    const gl = this.gl
    let vs: WebGLShader | null = null
    let fs: WebGLShader | null = null
    try {
      vs = compileShader(gl, gl.VERTEX_SHADER, VERTEX_SHADER_SRC)
      fs = compileShader(gl, gl.FRAGMENT_SHADER, FRAGMENT_SHADER_SRC)
      const program = gl.createProgram()
      if (!program) throw new WebGL2UnavailableError('Failed to create WebGL2 program')
      this.program = program
      gl.attachShader(program, vs)
      gl.attachShader(program, fs)
      gl.linkProgram(program)
      if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
        const log = gl.getProgramInfoLog(program) ?? 'link error'
        throw new WebGL2UnavailableError(`WebGL2 program link failed: ${log}`)
      }
      this.uViewportPx = gl.getUniformLocation(program, 'uViewportPx')
      this.uOffsetPx = gl.getUniformLocation(program, 'uOffsetPx')
      this.uAtlas = gl.getUniformLocation(program, 'uAtlas')
      this.uCellWidth = gl.getUniformLocation(program, 'uCellWidth')
      this.uPixelRatio = gl.getUniformLocation(program, 'uPixelRatio')

      const vao = gl.createVertexArray()
      const quadBuf = gl.createBuffer()
      const instanceBuf = gl.createBuffer()
      this.vao = vao
      this.quadBuf = quadBuf
      this.instanceBuf = instanceBuf
      this.instanceBufCapacityFloats = 0
      if (!vao || !quadBuf || !instanceBuf) {
        throw new WebGL2UnavailableError('Failed to allocate WebGL2 VAO/buffers')
      }

      gl.bindVertexArray(vao)

      gl.bindBuffer(gl.ARRAY_BUFFER, quadBuf)
      gl.bufferData(gl.ARRAY_BUFFER, UNIT_QUAD, gl.STATIC_DRAW)
      gl.enableVertexAttribArray(0)
      gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0)

      gl.bindBuffer(gl.ARRAY_BUFFER, instanceBuf)
      gl.enableVertexAttribArray(1)
      gl.vertexAttribPointer(1, 4, gl.FLOAT, false, INSTANCE_STRIDE_BYTES, 0)
      gl.vertexAttribDivisor(1, 1)

      gl.enableVertexAttribArray(2)
      gl.vertexAttribPointer(2, 4, gl.FLOAT, false, INSTANCE_STRIDE_BYTES, 16)
      gl.vertexAttribDivisor(2, 1)

      gl.enableVertexAttribArray(3)
      gl.vertexAttribPointer(3, 4, gl.FLOAT, false, INSTANCE_STRIDE_BYTES, 32)
      gl.vertexAttribDivisor(3, 1)

      gl.enableVertexAttribArray(4)
      gl.vertexAttribPointer(4, 1, gl.FLOAT, false, INSTANCE_STRIDE_BYTES, 48)
      gl.vertexAttribDivisor(4, 1)

      gl.bindVertexArray(null)

      const atlasTex = gl.createTexture()
      if (!atlasTex) throw new WebGL2UnavailableError('Failed to create WebGL2 atlas texture')
      this.atlasTex = atlasTex
      // Keep UNPACK_PREMULTIPLY_ALPHA_WEBGL = 1 globally for both atlas (color emoji) and image uploads
      gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, 1)
      this.allocateAtlasTexture()

      gl.enable(gl.BLEND)
      gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA)
    } catch (err) {
      this.releaseGpuObjects()
      throw err
    } finally {
      if (vs) gl.deleteShader(vs)
      if (fs) gl.deleteShader(fs)
    }
  }

  private allocateAtlasTexture(): void {
    if (!this.atlasTex || this.contextLost) return
    const gl = this.gl
    gl.activeTexture(gl.TEXTURE0)
    gl.bindTexture(gl.TEXTURE_2D, this.atlasTex)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE)
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, 1)
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, this.atlasSize, this.atlasSize, 0, gl.RGBA, gl.UNSIGNED_BYTE, null)
  }

  private resetAtlas(): void {
    this.atlasEntries.clear()
    this.packer.reset()
    this.atlasResets += 1
    if (!this.contextLost && this.atlasTex) {
      this.allocateAtlasTexture()
    }
  }

  private releaseGpuObjects(): void {
    const gl = this.gl
    for (const entry of this.imageTextures.values()) gl.deleteTexture(entry.texture)
    this.imageTextures.clear()
    this.imageTexBytes = 0
    if (this.atlasTex) gl.deleteTexture(this.atlasTex)
    if (this.quadBuf) gl.deleteBuffer(this.quadBuf)
    if (this.instanceBuf) gl.deleteBuffer(this.instanceBuf)
    if (this.vao) gl.deleteVertexArray(this.vao)
    if (this.program) gl.deleteProgram(this.program)
    this.atlasTex = null
    this.quadBuf = null
    this.instanceBuf = null
    this.instanceBufCapacityFloats = 0
    this.vao = null
    this.program = null
  }

  private ensureSceneGlyphs(scene: Scene): void {
    const hadPriorEntries = this.atlasEntries.size > 0
    let overflowed = false

    const ensureChar = (text: string, cells: number, bold: boolean, italic: boolean): boolean => {
      if (text === ' ' || text === '') return true
      const key = glyphKey(text, cells, bold, italic)
      if (this.atlasEntries.has(key)) return true
      if (!this.rasterizeAndUpload(key, text, cells, bold, italic)) {
        overflowed = true
        return false
      }
      return true
    }

    const walk = () => {
      for (const row of scene.rows) {
        if (!row) continue
        for (const run of row.glyphs) {
          if (run.batchable) {
            for (let k = 0; k < run.text.length; k += 1) {
              if (!ensureChar(run.text[k]!, 1, run.bold, run.italic)) return
            }
          } else {
            if (!ensureChar(run.text, run.cells, run.bold, run.italic)) return
          }
        }
      }
      if (scene.cursor?.shape === 'block' && scene.cursor.text) {
        ensureChar(scene.cursor.text.text, scene.cursor.cells, scene.cursor.text.bold, scene.cursor.text.italic)
      }
    }

    walk()
    if (overflowed && hadPriorEntries) {
      // Atlas filled up with stale glyphs from prior frames: clear once and repack only this frame's glyphs.
      this.resetAtlas()
      overflowed = false
      walk()
    }
  }

  private rasterizeAndUpload(
    key: string,
    text: string,
    cells: number,
    bold: boolean,
    italic: boolean,
  ): boolean {
    const { metrics, pixelRatio, scratch, scratchCtx, gl, atlasTex, atlasSize } = this
    if (!atlasTex) return false

    const cellPxW = Math.max(1, Math.ceil(cells * metrics.cellWidth * pixelRatio))
    const cellPxH = Math.max(1, Math.ceil(metrics.cellHeight * pixelRatio))
    // Small bearing padding so italic/bold overhangs and tall accents aren't clipped to the cell box
    const padX = Math.max(1, Math.ceil(0.25 * metrics.cellWidth * pixelRatio))
    const padY = Math.max(1, Math.ceil(0.15 * metrics.cellHeight * pixelRatio))
    const maxSlot = Math.max(1, atlasSize - ATLAS_PAD * 2)
    const bearingX = cellPxW + padX * 2 <= maxSlot ? padX : 0
    const bearingY = cellPxH + padY * 2 <= maxSlot ? padY : 0
    const w = Math.min(cellPxW + bearingX * 2, maxSlot)
    const h = Math.min(cellPxH + bearingY * 2, maxSlot)

    const pos = this.packer.allocate(w, h)
    if (!pos) return false

    if (scratch.width < w || scratch.height < h) {
      scratch.width = Math.max(scratch.width, w)
      scratch.height = Math.max(scratch.height, h)
    }

    const faceIdx = (bold ? 1 : 0) | (italic ? 2 : 0)
    scratchCtx.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0)
    scratchCtx.clearRect(0, 0, w / pixelRatio + 1, h / pixelRatio + 1)
    scratchCtx.font = this.faces[faceIdx]!
    scratchCtx.textBaseline = 'alphabetic'
    scratchCtx.fillStyle = '#ffffff'
    scratchCtx.fillText(text, bearingX / pixelRatio, metrics.baseline + bearingY / pixelRatio)

    // Detect color emoji (both chromatic and dark/grayscale like 🐼/⚫) ONLY for non-ASCII clusters,
    // ignoring thin subpixel-antialiased fringes on monochrome non-ASCII text (CJK, box-drawing, accents).
    let colored = false
    if (text.length > 1 || text.charCodeAt(0) >= 0x80) {
      const img = scratchCtx.getImageData?.(0, 0, w, h)
      if (img) {
        const d = img.data
        for (let i = 0; i < d.length; i += 4) {
          const a = d[i + 3]!
          if (a < 64) continue
          const r = d[i]!
          const g = d[i + 1]!
          const b = d[i + 2]!
          const chroma = Math.max(r, g, b) - Math.min(r, g, b)
          if (chroma > 24 || (a >= 200 && r < 220)) {
            colored = true
            break
          }
        }
      }
    }

    // WebGL2 9-arg texSubImage2D natively copies top-left `w × h` from `scratch` canvas AND applies
    // `UNPACK_PREMULTIPLY_ALPHA_WEBGL = 1` with zero `getImageData` readback!
    gl.activeTexture(gl.TEXTURE0)
    gl.bindTexture(gl.TEXTURE_2D, atlasTex)
    gl.texSubImage2D(gl.TEXTURE_2D, 0, pos.x, pos.y, w, h, gl.RGBA, gl.UNSIGNED_BYTE, scratch)

    this.atlasEntries.set(key, {
      x: pos.x,
      y: pos.y,
      w,
      h,
      bearingX,
      bearingY,
      u0: pos.x / atlasSize,
      v0: pos.y / atlasSize,
      u1: (pos.x + w) / atlasSize,
      v1: (pos.y + h) / atlasSize,
      colored,
    })
    return true
  }

  private resolveGlyphForEmit(text: string, cells: number, bold: boolean, italic: boolean): AtlasEntry | undefined {
    const key = glyphKey(text, cells, bold, italic)
    const existing = this.atlasEntries.get(key)
    if (existing) return existing
    if (!this.atlasTex) return undefined
    // First try packing into the current atlas (handles subsequent glyphs after a mid-pass reset
    // without thrashing); only flush + reset when the atlas is genuinely full.
    if (this.rasterizeAndUpload(key, text, cells, bold, italic)) {
      return this.atlasEntries.get(key)
    }
    if (this.instanceCount > 0) {
      this.flushBatch(this.atlasTex)
      this.instanceCount = 0
    }
    this.resetAtlas()
    this.rasterizeAndUpload(key, text, cells, bold, italic)
    return this.atlasEntries.get(key)
  }

  private emitGlyphRun(run: GlyphRun, y: number): void {
    const { cellWidth } = this.metrics
    const [r, g, b] = rgbFloats(run.color)
    if (run.batchable) {
      for (let k = 0; k < run.text.length; k += 1) {
        const ch = run.text[k]!
        if (ch === ' ') continue
        const entry = this.resolveGlyphForEmit(ch, 1, run.bold, run.italic)
        if (!entry) continue
        this.pushGlyph(this.gutter + (run.column + k) * cellWidth, y, entry, r, g, b, run.alpha)
      }
      return
    }
    const entry = this.resolveGlyphForEmit(run.text, run.cells, run.bold, run.italic)
    if (!entry) return
    this.pushGlyph(this.gutter + run.column * cellWidth, y, entry, r, g, b, run.alpha)
  }

  private emitDecoration(kind: LineKind, x: number, y: number, width: number, color: number): void {
    const { cellHeight, baseline } = this.metrics
    const thickness = Math.max(1, Math.round(cellHeight / 16))
    const under = y + Math.min(baseline + thickness + 1, cellHeight - thickness)
    const [r, g, b] = rgbFloats(color)
    switch (kind) {
      case 'single':
        this.pushSolid(x, under, width, thickness, r, g, b, 1)
        break
      case 'double':
        this.pushSolid(x, under - thickness, width, thickness, r, g, b, 1)
        this.pushSolid(x, under + thickness, width, thickness, r, g, b, 1)
        break
      case 'strikethrough':
        this.pushSolid(x, y + Math.round(baseline * 0.65), width, thickness, r, g, b, 1)
        break
      case 'overline':
        this.pushSolid(x, y, width, thickness, r, g, b, 1)
        break
      case 'dotted':
        // Single shader-driven quad per run (zero per-dot instance explosion!)
        this.pushInstance(x, under, width, thickness, thickness, thickness * 3, 0, 0, r, g, b, 1, DrawMode.DASHED_LINE)
        break
      case 'dashed':
        this.pushInstance(x, under, width, thickness, thickness * 4, thickness * 6, 0, 0, r, g, b, 1, DrawMode.DASHED_LINE)
        break
      case 'curly': {
        const ampPx = thickness * 1.35
        const halfStroke = thickness * 0.5
        const aaMargin = 1.5 / Math.max(1, this.pixelRatio)
        const h = (ampPx + halfStroke + aaMargin) * 2
        this.pushInstance(x, under - h * 0.5, width, h, ampPx, halfStroke, 0, 0, r, g, b, 1, DrawMode.CURLY_WAVE)
        break
      }
    }
  }

  private emitCursor(scene: Scene): void {
    const cursor = scene.cursor!
    const { cellWidth, cellHeight } = this.metrics
    const x = this.gutter + cursor.column * cellWidth
    const y = cursor.row * cellHeight
    const w = cursor.cells * cellWidth
    const thickness = Math.max(1, Math.round(cellWidth / 6))
    const [r, g, b] = rgbFloats(scene.theme.cursor)

    switch (cursor.shape) {
      case 'bar':
        this.pushSolid(x, y, thickness, cellHeight, r, g, b, 1)
        break
      case 'underline':
        this.pushSolid(x, y + cellHeight - thickness, w, thickness, r, g, b, 1)
        break
      case 'hollow':
        this.pushSolid(x, y, w, 1, r, g, b, 1)
        this.pushSolid(x, y + cellHeight - 1, w, 1, r, g, b, 1)
        this.pushSolid(x, y, 1, cellHeight, r, g, b, 1)
        this.pushSolid(x + w - 1, y, 1, cellHeight, r, g, b, 1)
        break
      case 'block':
        this.pushSolid(x, y, w, cellHeight, r, g, b, 1)
        if (cursor.text) {
          const entry = this.resolveGlyphForEmit(cursor.text.text, cursor.cells, cursor.text.bold, cursor.text.italic)
          if (entry) {
            const [tr, tg, tb] = rgbFloats(scene.theme.cursorText)
            this.pushGlyph(x, y, entry, tr, tg, tb, 1)
          }
        }
        break
    }
  }

  private drawImages(scene: Scene, above: boolean): void {
    const list = scene.images
    if (!list || list.length === 0) return

    // Batch consecutive slices sharing the same content-addressed texture key into 1 draw call.
    // Flush BEFORE calling `ensureImageTexture` on a new key so LRU eviction inside
    // `ensureImageTexture` can never delete `currentTex` while its quads are still queued.
    let currentKey: string | null = null
    let currentPicture: CanvasImageSource | null = null
    let currentTex: WebGLTexture | null = null
    this.instanceCount = 0

    for (const img of list) {
      if (img.above !== above || img.source.width <= 0 || img.source.height <= 0) continue
      const texKey = this.imageKeyOf(img.key, img.picture)
      if ((texKey !== currentKey || img.picture !== currentPicture) && currentTex !== null && this.instanceCount > 0) {
        this.flushBatch(currentTex)
        this.instanceCount = 0
        currentTex = null
        currentKey = null
        currentPicture = null
      }
      const texEntry = this.ensureImageTexture(texKey, img.picture)
      if (!texEntry || texEntry.width <= 0 || texEntry.height <= 0) continue
      if (currentTex !== null && currentTex !== texEntry.texture && this.instanceCount > 0) {
        this.flushBatch(currentTex)
        this.instanceCount = 0
      }
      currentKey = texKey
      currentPicture = img.picture
      currentTex = texEntry.texture
      const u0 = img.source.x / texEntry.width
      const v0 = img.source.y / texEntry.height
      const u1 = (img.source.x + img.source.width) / texEntry.width
      const v1 = (img.source.y + img.source.height) / texEntry.height
      this.pushInstance(
        this.gutter + img.x,
        img.y,
        img.width,
        img.height,
        u0, v0, u1, v1,
        1, 1, 1, 1,
        DrawMode.COLOR_TEX,
      )
    }

    if (currentTex !== null && this.instanceCount > 0) {
      this.flushBatch(currentTex)
      this.instanceCount = 0
    }
  }

  private imageKeyOf(explicitKey: string | undefined, picture: CanvasImageSource): string {
    if (explicitKey) return explicitKey
    let token = this.fallbackImageKeys.get(picture)
    if (token === undefined) {
      token = `__anon_${this.nextFallbackImageKey}`
      this.nextFallbackImageKey += 1
      this.fallbackImageKeys.set(picture, token)
    }
    return token
  }

  private sweepDeadImageTextures(): void {
    for (const [key, entry] of this.imageTextures) {
      if (!entry.sourceRef) continue
      const src = entry.sourceRef.deref()
      if (src === undefined) {
        this.imageTextures.delete(key)
        this.imageTexBytes -= entry.byteSize
        this.gl.deleteTexture(entry.texture)
        continue
      }
      const dims = sourceDimensions(src)
      if (dims.width <= 0 || dims.height <= 0) {
        this.imageTextures.delete(key)
        this.imageTexBytes -= entry.byteSize
        this.gl.deleteTexture(entry.texture)
      }
    }
  }

  private ensureImageTexture(key: string, picture: CanvasImageSource): ImageTextureEntry | null {
    const dims = sourceDimensions(picture)
    const cached = this.imageTextures.get(key)
    if (cached) {
      if (dims.width <= 0 || dims.height <= 0) {
        // Source bitmap was closed externally; delete GPU texture immediately
        this.gl.deleteTexture(cached.texture)
        this.imageTextures.delete(key)
        this.imageTexBytes -= cached.byteSize
        return null
      }
      if (
        cached.width === dims.width
        && cached.height === dims.height
        && (cached.sourceRef === null || cached.sourceRef.deref() === picture)
      ) {
        // Refresh LRU recency
        this.imageTextures.delete(key)
        this.imageTextures.set(key, cached)
        return cached
      }
      // Same key re-used with a different bitmap/dimensions: replace the stale GPU texture cleanly.
      this.gl.deleteTexture(cached.texture)
      this.imageTextures.delete(key)
      this.imageTexBytes -= cached.byteSize
    }
    if (typeof SVGImageElement !== 'undefined' && picture instanceof SVGImageElement) return null
    if (dims.width <= 0 || dims.height <= 0) return null

    // Reclaim any closed or GC'd image textures before allocating/evicting live GPU textures.
    this.sweepDeadImageTextures()

    const gl = this.gl
    const tex = gl.createTexture()
    if (!tex) return null
    try {
      gl.activeTexture(gl.TEXTURE0)
      gl.bindTexture(gl.TEXTURE_2D, tex)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE)
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE)
      gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, 1)
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, picture as TexImageSource)
    } catch {
      gl.deleteTexture(tex)
      return null
    }

    const byteSize = Math.max(4, dims.width * dims.height * 4)
    const sourceRef = typeof WeakRef === 'function' ? new WeakRef(picture) : null
    const entry: ImageTextureEntry = { texture: tex, width: dims.width, height: dims.height, byteSize, sourceRef }
    this.imageTextures.set(key, entry)
    this.imageTexBytes += byteSize
    while (this.imageTexBytes > this.maxImageTexBytes && this.imageTextures.size > 1) {
      const oldestKey = this.imageTextures.keys().next().value
      if (oldestKey === undefined) break
      const oldest = this.imageTextures.get(oldestKey)!
      this.imageTextures.delete(oldestKey)
      this.imageTexBytes -= oldest.byteSize
      gl.deleteTexture(oldest.texture)
    }
    return entry
  }

  private pushSolid(x: number, y: number, w: number, h: number, r: number, g: number, b: number, a: number): void {
    this.pushInstance(x, y, w, h, 0, 0, 0, 0, r, g, b, a, DrawMode.SOLID)
  }

  private pushGlyph(
    x: number,
    y: number,
    entry: AtlasEntry,
    r: number,
    g: number,
    b: number,
    a: number,
  ): void {
    this.pushInstance(
      x - entry.bearingX / this.pixelRatio,
      y - entry.bearingY / this.pixelRatio,
      entry.w / this.pixelRatio,
      entry.h / this.pixelRatio,
      entry.u0, entry.v0, entry.u1, entry.v1,
      r, g, b, a,
      entry.colored ? DrawMode.COLOR_TEX : DrawMode.MONO_GLYPH,
    )
  }

  private pushInstance(
    x: number, y: number, w: number, h: number,
    u0: number, v0: number, u1: number, v1: number,
    r: number, g: number, b: number, a: number,
    mode: number,
  ): void {
    const needed = (this.instanceCount + 1) * INSTANCE_FLOATS
    if (needed > this.instances.length) {
      let nextLen = this.instances.length
      while (nextLen < needed) nextLen *= 2
      const grown = new Float32Array(nextLen)
      grown.set(this.instances)
      this.instances = grown
    }
    const base = this.instanceCount * INSTANCE_FLOATS
    this.instances[base] = x
    this.instances[base + 1] = y
    this.instances[base + 2] = w
    this.instances[base + 3] = h
    this.instances[base + 4] = u0
    this.instances[base + 5] = v0
    this.instances[base + 6] = u1
    this.instances[base + 7] = v1
    this.instances[base + 8] = r
    this.instances[base + 9] = g
    this.instances[base + 10] = b
    this.instances[base + 11] = a
    this.instances[base + 12] = mode
    this.instanceCount += 1
  }

  private flushBatch(texture: WebGLTexture): void {
    if (this.instanceCount === 0 || !this.instanceBuf) return
    const gl = this.gl
    const floatLen = this.instanceCount * INSTANCE_FLOATS
    gl.activeTexture(gl.TEXTURE0)
    gl.bindTexture(gl.TEXTURE_2D, texture)
    gl.bindBuffer(gl.ARRAY_BUFFER, this.instanceBuf)
    const view = this.instances.subarray(0, floatLen)
    if (floatLen > this.instanceBufCapacityFloats) {
      gl.bufferData(gl.ARRAY_BUFFER, this.instances, gl.DYNAMIC_DRAW)
      this.instanceBufCapacityFloats = this.instances.length
    } else {
      gl.bufferSubData(gl.ARRAY_BUFFER, 0, view)
    }
    gl.drawArraysInstanced(gl.TRIANGLES, 0, 6, this.instanceCount)
    this.drawCallsLastFrame += 1
  }
}

function glyphKey(text: string, cells: number, bold: boolean, italic: boolean): string {
  const face = (bold ? 1 : 0) | (italic ? 2 : 0)
  return `${face}:${cells}:${text}`
}

function facesOf(font: Font): string[] {
  return [fontOf(font, false, false), fontOf(font, true, false), fontOf(font, false, true), fontOf(font, true, true)]
}

function rgbFloats(color: number): readonly [number, number, number] {
  return [
    ((color >> 16) & 0xff) / 255,
    ((color >> 8) & 0xff) / 255,
    (color & 0xff) / 255,
  ]
}

function compileShader(gl: WebGL2RenderingContext, type: number, source: string): WebGLShader {
  const shader = gl.createShader(type)
  if (!shader) throw new WebGL2UnavailableError('Failed to create WebGL2 shader')
  gl.shaderSource(shader, source)
  gl.compileShader(shader)
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    const log = gl.getShaderInfoLog(shader) ?? 'compile error'
    gl.deleteShader(shader)
    throw new WebGL2UnavailableError(`WebGL2 shader compile failed: ${log}`)
  }
  return shader
}

function sourceDimensions(src: CanvasImageSource): { readonly width: number; readonly height: number } {
  if ('naturalWidth' in src && typeof src.naturalWidth === 'number') {
    return { width: src.naturalWidth, height: (src as HTMLImageElement).naturalHeight }
  }
  if ('videoWidth' in src && typeof src.videoWidth === 'number') {
    return { width: src.videoWidth, height: (src as HTMLVideoElement).videoHeight }
  }
  if ('width' in src && typeof src.width === 'number' && 'height' in src && typeof src.height === 'number') {
    return { width: src.width, height: src.height }
  }
  return { width: 0, height: 0 }
}
