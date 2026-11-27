import { spawn, type ChildProcess } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { createServer, type IncomingMessage, type Server } from 'node:http'
import { createConnection, type Socket } from 'node:net'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const E2E_DIR = dirname(fileURLToPath(import.meta.url))
const VIEW_DIR = resolve(E2E_DIR, '..')
const REPO_ROOT = resolve(VIEW_DIR, '..')
const DIST_INDEX = join(VIEW_DIR, 'dist', 'index.js')
const WS_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'

export interface LiveBrokerBridge {
  readonly baseUrl: string
  readonly socketPath: string
  readonly tmpDir: string
  close(): Promise<void>
}

function resolveBrokerBinary(): string {
  const candidates = [
    process.env.APACK_TERMINAL_BIN,
    join(REPO_ROOT, 'target', 'debug', process.platform === 'win32' ? 'apack-terminal.exe' : 'apack-terminal'),
    join(REPO_ROOT, 'target', 'release', process.platform === 'win32' ? 'apack-terminal.exe' : 'apack-terminal'),
  ].filter((p): p is string => Boolean(p))
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate
  }
  throw new Error(
    `Could not locate apack-terminal binary. Checked:\n  ${candidates.join('\n  ')}\n`
      + 'Build it first with `cargo build --locked --bin apack-terminal` or set APACK_TERMINAL_BIN.',
  )
}

function encodeWsFrame(opcode: number, payload: Buffer): Buffer {
  const len = payload.length
  let header: Buffer
  if (len < 126) {
    header = Buffer.alloc(2)
    header[0] = 0x80 | opcode
    header[1] = len
  } else if (len < 65536) {
    header = Buffer.alloc(4)
    header[0] = 0x80 | opcode
    header[1] = 126
    header.writeUInt16BE(len, 2)
  } else {
    header = Buffer.alloc(10)
    header[0] = 0x80 | opcode
    header[1] = 127
    header.writeBigUInt64BE(BigInt(len), 2)
  }
  return Buffer.concat([header, payload])
}

/**
 * Minimal RFC 6455 binary WebSocket <-> local broker socket bridge using only Node built-ins.
 */
function attachWsBrokerBridge(req: IncomingMessage, wsSocket: Socket, head: Buffer, brokerSocketPath: string): void {
  const key = req.headers['sec-websocket-key']
  if (typeof key !== 'string' || key.length === 0) {
    wsSocket.destroy()
    return
  }
  const accept = createHash('sha1').update(`${key}${WS_GUID}`).digest('base64')
  wsSocket.write(
    'HTTP/1.1 101 Switching Protocols\r\n'
      + 'Upgrade: websocket\r\n'
      + 'Connection: Upgrade\r\n'
      + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  )

  const broker = createConnection(brokerSocketPath)
  let buf: Buffer = head.length > 0 ? Buffer.from(head) : Buffer.alloc(0)
  let fragChunks: Buffer[] = []
  let closed = false

  const closeBoth = () => {
    if (closed) return
    closed = true
    wsSocket.destroy()
    broker.destroy()
  }

  broker.on('data', (chunk: Buffer) => {
    if (!closed && wsSocket.writable) {
      if (!wsSocket.write(encodeWsFrame(0x2, chunk))) {
        broker.pause()
        wsSocket.once('drain', () => {
          if (!closed) broker.resume()
        })
      }
    }
  })
  broker.on('close', () => {
    if (!closed && wsSocket.writable) {
      wsSocket.end(encodeWsFrame(0x8, Buffer.alloc(0)))
    }
    closed = true
  })
  broker.on('error', closeBoth)

  const parseFrames = () => {
    while (buf.length >= 2) {
      const b0 = buf[0] ?? 0
      const b1 = buf[1] ?? 0
      const fin = (b0 & 0x80) !== 0
      const opcode = b0 & 0x0f
      const masked = (b1 & 0x80) !== 0
      let payloadLen = b1 & 0x7f
      let offset = 2
      if (payloadLen === 126) {
        if (buf.length < 4) return
        payloadLen = buf.readUInt16BE(2)
        offset = 4
      } else if (payloadLen === 127) {
        if (buf.length < 10) return
        const bigLen = buf.readBigUInt64BE(2)
        if (bigLen > 64n * 1024n * 1024n) {
          closeBoth()
          return
        }
        payloadLen = Number(bigLen)
        offset = 10
      }
      const maskLen = masked ? 4 : 0
      if (buf.length < offset + maskLen + payloadLen) return

      const payload = Buffer.allocUnsafe(payloadLen)
      if (masked) {
        const m0 = buf[offset] ?? 0
        const m1 = buf[offset + 1] ?? 0
        const m2 = buf[offset + 2] ?? 0
        const m3 = buf[offset + 3] ?? 0
        const mask = [m0, m1, m2, m3]
        const start = offset + 4
        for (let i = 0; i < payloadLen; i++) {
          payload[i] = (buf[start + i] ?? 0) ^ (mask[i & 3] ?? 0)
        }
      } else {
        buf.copy(payload, 0, offset, offset + payloadLen)
      }
      buf = buf.subarray(offset + maskLen + payloadLen)

      if (opcode === 0x8) {
        // RFC 6455 §5.5.1: echo Close frame cleanly before closing transport.
        if (!closed && wsSocket.writable) {
          wsSocket.end(encodeWsFrame(0x8, Buffer.alloc(0)))
        }
        closed = true
        broker.destroy()
        return
      }
      if (opcode === 0x9) {
        // Ping -> Pong
        if (!closed && wsSocket.writable) wsSocket.write(encodeWsFrame(0xa, payload))
        continue
      }
      if (opcode === 0xa) {
        continue
      }
      if (opcode === 0x2 || opcode === 0x1 || opcode === 0x0) {
        fragChunks.push(payload)
        if (fin) {
          const message = fragChunks.length === 1 ? fragChunks[0]! : Buffer.concat(fragChunks)
          fragChunks = []
          if (!closed && broker.writable) {
            if (!broker.write(message)) {
              wsSocket.pause()
              broker.once('drain', () => {
                if (!closed) wsSocket.resume()
              })
            }
          }
        }
      }
    }
  }

  wsSocket.on('data', (chunk: Buffer) => {
    buf = buf.length === 0 ? chunk : Buffer.concat([buf, chunk])
    parseFrames()
  })
  wsSocket.on('close', closeBoth)
  wsSocket.on('error', closeBoth)
  if (buf.length > 0) parseFrames()
}

const HARNESS_HTML = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>apack-terminal-view E2E</title>
  <style>
    * { box-sizing: border-box; }
    html, body {
      margin: 0;
      padding: 0;
      background: #0a0d12;
      overflow: hidden;
      font-family: monospace;
    }
    #terminal-host {
      position: relative;
      width: 720px;
      height: 360px;
      background: #0a0d12;
      overflow: hidden;
    }
  </style>
</head>
<body>
  <div id="terminal-host"></div>
  <script type="module">
    import {
      CanvasPainter,
      ImageCache,
      TerminalClient,
      TerminalView,
      WebGL2AtlasPainter,
      createPainter,
      rowPlainText,
      xtermPalette,
    } from '/dist/index.js'

    function createWsByteStream(url) {
      return new Promise((resolve, reject) => {
        const ws = new WebSocket(url)
        ws.binaryType = 'arraybuffer'
        const dataListeners = new Set()
        const closeListeners = new Set()
        let closed = false
        const notifyClose = () => {
          if (closed) return
          closed = true
          for (const fn of closeListeners) fn()
        }
        ws.addEventListener('open', () => {
          resolve({
            write(bytes) {
              if (!closed && ws.readyState === WebSocket.OPEN) {
                ws.send(bytes)
              }
            },
            onData(listener) {
              dataListeners.add(listener)
              return () => dataListeners.delete(listener)
            },
            onClose(listener) {
              closeListeners.add(listener)
              return () => closeListeners.delete(listener)
            },
            close() {
              notifyClose()
              if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) {
                ws.close()
              }
            },
          })
        })
        ws.addEventListener('message', (ev) => {
          const chunk = new Uint8Array(ev.data)
          for (const fn of dataListeners) fn(chunk)
        })
        ws.addEventListener('close', notifyClose)
        ws.addEventListener('error', (err) => {
          reject(new Error('WebSocket error: ' + String(err)))
          notifyClose()
        })
      })
    }

    const ANSI_16 = [
      0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5,
      0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ]
    const E2E_THEME = {
      background: 0x0a0d12,
      foreground: 0xe6edf3,
      palette: xtermPalette(ANSI_16),
      cursor: 0x00ffff,
      cursorText: 0x000000,
      selectionBackground: 0x3b82f6,
      selectionForeground: null,
      matchBackground: 0xff8800,
      currentMatchBackground: 0xff00ff,
    }

    let state = null

    window.__apackE2E = {
      async mountLiveSession(opts) {
        if (state) {
          await window.__apackE2E.teardown()
        }
        const host = document.getElementById('terminal-host')
        host.style.width = (opts.widthPx ?? 720) + 'px'
        host.style.height = (opts.heightPx ?? 360) + 'px'
        host.innerHTML = ''

        const wsUrl = (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws'
        const stream = await createWsByteStream(wsUrl)
        const client = await TerminalClient.connect(stream, { clientName: 'playwright-e2e' })
        await client.acquireLease({ kind: 'brokerImmediate' })

        const { session } = await client.spawn({
          argv: opts.argv,
          cwd: null,
          env: [],
          size: { cols: opts.cols ?? 80, rows: opts.rows ?? 20 },
          cellPx: { width: 9, height: 18 },
          scrollbackLimit: opts.scrollbackLimit ?? 2000,
          marksKey: null,
          resizePolicy: 'latestActive',
          recordDir: null,
          restoreFrom: null,
          imageFiles: Boolean(opts.imageFiles),
          kept: false,
        })

        const source = await client.attachSession(session)
        const imageCache = new ImageCache()
        let painterInstance = null
        let contextLostCount = 0
        let lastFit = null
        let lastFound = null
        let lastCopied = ''

        const view = new TerminalView(source, {
          font: {
            family: 'Menlo, Monaco, Consolas, "DejaVu Sans Mono", "Noto Color Emoji", "Apple Color Emoji", monospace',
            size: 14,
            lineHeight: 1.25,
          },
          theme: E2E_THEME,
          imageCache,
          painter: (font) => {
            painterInstance = createPainter(font, {
              preferWebGL: opts.mode === 'webgl2',
              onContextLost: () => {
                contextLostCount++
              },
            })
            return painterInstance
          },
          onFit: (fit) => {
            lastFit = fit
          },
          onFound: (found) => {
            lastFound = found
          },
          onCopy: (text) => {
            lastCopied = text
          },
        })
        view.mount(host)
        view.focus()

        state = {
          host,
          stream,
          client,
          session,
          source,
          imageCache,
          view,
          get painter() {
            return painterInstance
          },
          get contextLostCount() {
            return contextLostCount
          },
          get lastFit() {
            return lastFit
          },
          get lastFound() {
            return lastFound
          },
          get lastCopied() {
            return lastCopied
          },
        }

        return {
          session,
          painterKind:
            painterInstance instanceof WebGL2AtlasPainter
              ? 'webgl2'
              : painterInstance instanceof CanvasPainter
                ? 'canvas2d'
                : 'unknown',
          metrics: painterInstance ? painterInstance.metrics : null,
        }
      },

      async waitForText(needle, timeoutMs = 8000) {
        if (!state) throw new Error('not mounted')
        return await state.client.wait(state.session, { kind: 'text', needle }, timeoutMs)
      },

      async waitAnimationFrames(n = 3) {
        for (let i = 0; i < n; i++) {
          await new Promise((r) => requestAnimationFrame(() => r()))
        }
      },

      mirrorText() {
        if (!state) return ''
        state.view.flushMirror()
        const el = state.host.querySelector('[data-terminal-text]')
        return el ? (el.textContent ?? '') : ''
      },

      visibleLines() {
        if (!state) return []
        const vp = state.view.viewport
        const store = state.view.store
        const rows = state.lastFit?.rows ?? 20
        const out = []
        for (let r = 0; r < rows; r++) {
          const line = vp.stepRows(vp.top, r)
          const row = store.row(line)
          out.push(row ? rowPlainText(row) : '')
        }
        return out
      },

      painterStats() {
        if (!state || !state.painter) return null
        const p = state.painter
        const isGl = p instanceof WebGL2AtlasPainter
        return {
          kind: isGl ? 'webgl2' : 'canvas2d',
          isContextLost: isGl ? p.isContextLost : false,
          atlasEntryCount: isGl ? p.atlasEntryCount : 0,
          imageTextureCount: isGl ? p.imageTextureCount : 0,
          decodedImageCount: state.imageCache.size,
          canvasWidth: p.canvas.width,
          canvasHeight: p.canvas.height,
          storeEpoch: state.view.store.epoch,
          contextLostCount: state.contextLostCount,
          lastFit: state.lastFit,
          lastFound: state.lastFound,
          lastCopied: state.lastCopied,
          metrics: p.metrics,
        }
      },

      scrollTo(line) {
        if (!state) throw new Error('not mounted')
        state.view.scrollTo(line)
      },

      follow() {
        if (!state) throw new Error('not mounted')
        state.view.follow()
      },

      async find(needle, matchCase = false) {
        if (!state) throw new Error('not mounted')
        return await state.view.find(needle, { matchCase })
      },

      async copy() {
        if (!state) throw new Error('not mounted')
        const ok = await state.view.copy()
        return { ok, text: state.lastCopied }
      },

      resizeHost(widthPx, heightPx) {
        if (!state) throw new Error('not mounted')
        state.host.style.width = widthPx + 'px'
        state.host.style.height = heightPx + 'px'
        state.view.fit()
        state.view.redraw()
      },

      redraw() {
        if (!state) return
        state.view.redraw()
      },

      async loseAndRestoreWebGLContext() {
        if (!state || !(state.painter instanceof WebGL2AtlasPainter)) {
          throw new Error('WebGL2AtlasPainter not active')
        }
        const canvas = state.painter.canvas
        const gl = canvas.getContext('webgl2')
        if (!gl) throw new Error('missing webgl2 context')
        const ext = gl.getExtension('WEBGL_lose_context')
        if (!ext) throw new Error('WEBGL_lose_context extension unavailable')

        const lostPromise = new Promise((resolve) => {
          canvas.addEventListener('webglcontextlost', () => resolve(true), { once: true })
        })
        ext.loseContext()
        await lostPromise
        // Yield a macrotask so Chromium finishes transitioning the context into restorable-lost state.
        await new Promise((r) => setTimeout(r, 50))
        const duringLoss = {
          isContextLost: state.painter.isContextLost,
          contextLostCount: state.contextLostCount,
        }

        const restoredPromise = new Promise((resolve) => {
          canvas.addEventListener('webglcontextrestored', () => resolve(true), { once: true })
        })
        ext.restoreContext()
        await restoredPromise
        // Do NOT call state.view.redraw() manually: verify WebGL2AtlasPainter.handleContextRestored
        // drives automatic repaint via TerminalView whenCleared callback!
        await window.__apackE2E.waitAnimationFrames(4)

        return {
          duringLoss,
          afterRestore: {
            isContextLost: state.painter.isContextLost,
            atlasEntryCount: state.painter.atlasEntryCount,
            imageTextureCount: state.painter.imageTextureCount,
          },
        }
      },

      async teardown() {
        if (!state) return
        const s = state
        state = null
        try {
          s.view.dispose()
        } catch {}
        try {
          await s.client.kill(s.session)
        } catch {}
        try {
          s.client.close()
        } catch {}
        try {
          s.stream.close()
        } catch {}
      },
    }
  </script>
</body>
</html>
`

function isChildAlive(child: ChildProcess): boolean {
  return child.exitCode === null && child.signalCode === null
}

function makeShortSocketTmpDir(): string {
  let base = tmpdir()
  if (process.platform !== 'win32' && Buffer.byteLength(join(base, 'apack-e2e-XXXXXX', 'b.sock')) >= 100) {
    base = '/tmp'
  }
  return mkdtempSync(join(base, 'apack-e2e-'))
}

async function waitForSocket(socketPath: string, child: ChildProcess, stderrChunks: string[]): Promise<void> {
  const deadline = Date.now() + 10_000
  while (Date.now() < deadline) {
    if (!isChildAlive(child)) {
      throw new Error(
        `apack-terminal broker exited early (code=${child.exitCode}, signal=${child.signalCode}): ${stderrChunks.join('')}`,
      )
    }
    if (existsSync(socketPath)) {
      // Confirm listener accepts connections cleanly.
      const ok = await new Promise<boolean>((resolveConn) => {
        const probe = createConnection(socketPath)
        probe.once('connect', () => {
          probe.destroy()
          resolveConn(true)
        })
        probe.once('error', () => {
          probe.destroy()
          resolveConn(false)
        })
      })
      if (ok) return
    }
    await new Promise((r) => setTimeout(r, 25))
  }
  throw new Error(`Timed out waiting for broker socket at ${socketPath}: ${stderrChunks.join('')}`)
}

export async function startLiveBrokerBridge(): Promise<LiveBrokerBridge> {
  if (!existsSync(DIST_INDEX)) {
    throw new Error(`Missing built view bundle at ${DIST_INDEX}. Run \`npm run build\` in view/ first.`)
  }
  const binary = resolveBrokerBinary()
  const tmpDir = makeShortSocketTmpDir()
  const socketPath = join(tmpDir, 'b.sock')

  const childEnv: NodeJS.ProcessEnv = { ...process.env }
  delete childEnv.APACK_TERMINAL_AUTH_KEY
  const stderrChunks: string[] = []
  const brokerChild = spawn(binary, ['serve', '--socket', socketPath, '--shards', '1'], {
    env: childEnv,
    stdio: ['ignore', 'ignore', 'pipe'],
  })
  brokerChild.stderr?.on('data', (d: Buffer) => stderrChunks.push(d.toString('utf8')))

  const onProcessExit = () => {
    if (isChildAlive(brokerChild)) {
      try {
        brokerChild.kill('SIGKILL')
      } catch {}
    }
    try {
      rmSync(tmpDir, { recursive: true, force: true })
    } catch {}
  }
  process.once('exit', onProcessExit)

  let server: Server | null = null
  const openSockets = new Set<Socket>()

  try {
    await waitForSocket(socketPath, brokerChild, stderrChunks)
    const distBundle = readFileSync(DIST_INDEX)

    server = createServer((req, res) => {
      if (req.url === '/' || req.url === '/index.html') {
        res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
        res.end(HARNESS_HTML)
        return
      }
      if (req.url === '/dist/index.js') {
        res.writeHead(200, { 'content-type': 'application/javascript; charset=utf-8' })
        res.end(distBundle)
        return
      }
      res.writeHead(404)
      res.end('not found')
    })

    server.on('upgrade', (req, socket, head) => {
      const netSock = socket as Socket
      if (req.url !== '/ws') {
        netSock.destroy()
        return
      }
      openSockets.add(netSock)
      netSock.on('close', () => openSockets.delete(netSock))
      attachWsBrokerBridge(req, netSock, head, socketPath)
    })

    await new Promise<void>((resolveListen, rejectListen) => {
      server!.once('error', rejectListen)
      server!.listen(0, '127.0.0.1', () => resolveListen())
    })
    const addr = server.address()
    if (!addr || typeof addr === 'string') {
      throw new Error('Failed to bind HTTP bridge port')
    }
    const baseUrl = `http://127.0.0.1:${addr.port}`
    const liveServer = server

    return {
      baseUrl,
      socketPath,
      tmpDir,
      async close() {
        process.off('exit', onProcessExit)
        for (const s of openSockets) s.destroy()
        openSockets.clear()
        await new Promise<void>((r) => liveServer.close(() => r()))
        if (isChildAlive(brokerChild)) {
          brokerChild.kill('SIGTERM')
          await new Promise<void>((r) => {
            const timer = setTimeout(() => {
              if (isChildAlive(brokerChild)) brokerChild.kill('SIGKILL')
              r()
            }, 1500)
            brokerChild.once('exit', () => {
              clearTimeout(timer)
              r()
            })
          })
        }
        rmSync(tmpDir, { recursive: true, force: true })
      },
    }
  } catch (err) {
    process.off('exit', onProcessExit)
    for (const s of openSockets) s.destroy()
    if (server) {
      await new Promise<void>((r) => server!.close(() => r()))
    }
    if (isChildAlive(brokerChild)) {
      brokerChild.kill('SIGKILL')
    }
    rmSync(tmpDir, { recursive: true, force: true })
    throw err
  }
}
