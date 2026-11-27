import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { expect, test } from '@playwright/test'
import { startLiveBrokerBridge, type LiveBrokerBridge } from './broker-bridge.ts'
import { countMatchingPixels, countNonBackgroundPixels, decodePng } from './png.ts'

const BG_RGB: readonly [number, number, number] = [10, 13, 18]

interface E2EHarnessApi {
  mountLiveSession(opts: {
    mode: 'webgl2' | 'canvas2d'
    argv: string[]
    cols?: number
    rows?: number
    widthPx?: number
    heightPx?: number
    scrollbackLimit?: number
    imageFiles?: boolean
  }): Promise<{
    session: number
    painterKind: 'webgl2' | 'canvas2d' | 'unknown'
    metrics: { cellWidth: number; cellHeight: number } | null
  }>
  waitForText(needle: string, timeoutMs?: number): Promise<string>
  waitAnimationFrames(n?: number): Promise<void>
  mirrorText(): string
  visibleLines(): string[]
  painterStats(): {
    kind: 'webgl2' | 'canvas2d'
    isContextLost: boolean
    atlasEntryCount: number
    imageTextureCount: number
    contextLostCount: number
    lastFit: { columns: number; rows: number; cellWidth: number; cellHeight: number } | null
    lastFound: { current: number; total: number; more: boolean } | null
    lastCopied: string
    metrics: { cellWidth: number; cellHeight: number }
  } | null
  scrollTo(line: number): void
  follow(): void
  find(needle: string, matchCase?: boolean): Promise<number>
  copy(): Promise<{ ok: boolean; text: string }>
  resizeHost(widthPx: number, heightPx: number): void
  redraw(): void
  loseAndRestoreWebGLContext(): Promise<{
    duringLoss: { isContextLost: boolean; contextLostCount: number }
    afterRestore: { isContextLost: boolean; atlasEntryCount: number; imageTextureCount: number }
  }>
  teardown(): Promise<void>
}

declare global {
  interface Window {
    __apackE2E: E2EHarnessApi
  }
}

/** Builds a Kitty inline `t=d` RGBA graphics escape sequence for a solid-color WxH image. */
function makeKittyInlineImage(
  widthPx: number,
  heightPx: number,
  cols: number,
  rows: number,
  rgba: readonly [number, number, number, number],
  imageId: number,
): string {
  const raw = Buffer.alloc(widthPx * heightPx * 4)
  for (let i = 0; i < raw.length; i += 4) {
    raw[i] = rgba[0]
    raw[i + 1] = rgba[1]
    raw[i + 2] = rgba[2]
    raw[i + 3] = rgba[3]
  }
  const b64 = raw.toString('base64')
  return `\x1b_Ga=T,f=32,s=${widthPx},v=${heightPx},c=${cols},r=${rows},i=${imageId},t=d;${b64}\x1b\\`
}

let bridge: LiveBrokerBridge

test.beforeEach(async () => {
  bridge = await startLiveBrokerBridge()
})

test.afterEach(async ({ page }) => {
  await page.evaluate(() => window.__apackE2E?.teardown()).catch(() => {})
  await bridge?.close()
})

for (const mode of ['webgl2', 'canvas2d'] as const) {
  test.describe(`TerminalView live broker E2E (${mode})`, () => {
    test('renders real glyphs, RGB colors, cursor, emoji vs monochrome tinting, and Kitty inline image', async ({
      page,
    }) => {
      const pageErrors: string[] = []
      page.on('pageerror', (err) => pageErrors.push(err.message))

      // Write exact terminal escape sequences to a file inside the bridge tmp dir so shell quoting
      // never alters escapes or UTF-8 emoji bytes.
      const seqFile = join(bridge.tmpDir, `render-${mode}.bin`)
      const magentaImg = makeKittyInlineImage(16, 16, 6, 3, [255, 0, 255, 255], 7)
      const stream = [
        '\x1b[38;2;255;40;40mRED_ROW_GLYPHS\x1b[0m\r\n',
        '\x1b[38;2;40;255;40mGREEN_ROW_GLYPHS\x1b[0m\r\n',
        '\x1b[38;2;40;120;255mBLUE_ROW_GLYPHS\x1b[0m\r\n',
        '\x1b[48;2;240;220;30m\x1b[38;2;0;0;0mYELLOW_BG_BLOCK\x1b[0m\r\n',
        '\x1b[38;2;0;120;255m######\x1b[0m  🟥🟥  \x1b[38;2;255;255;255mEMOJI_ROW\x1b[0m\r\n',
        `${magentaImg}\r\n\r\n\r\n`,
        'RENDER_READY_OK ',
      ].join('')
      writeFileSync(seqFile, stream, 'utf8')

      await page.goto(bridge.baseUrl)
      const mounted = await page.evaluate(
        ({ m, file }) =>
          window.__apackE2E.mountLiveSession({
            mode: m,
            argv: ['/bin/sh', '-c', `cat '${file}'; sleep 15`],
            cols: 80,
            rows: 20,
            widthPx: 720,
            heightPx: 360,
          }),
        { m: mode, file: seqFile },
      )
      expect(mounted.painterKind).toBe(mode)
      expect(mounted.metrics).not.toBeNull()
      const cellW = mounted.metrics!.cellWidth
      const cellH = mounted.metrics!.cellHeight

      expect(await page.evaluate(() => window.__apackE2E.waitForText('RENDER_READY_OK', 8000))).toBe('met')

      // Wait until the Kitty inline image blob has been fetched, decoded, and painted.
      if (mode === 'webgl2') {
        await expect
          .poll(async () => (await page.evaluate(() => window.__apackE2E.painterStats()))?.imageTextureCount ?? 0, {
            timeout: 8000,
          })
          .toBeGreaterThan(0)
      } else {
        await page.waitForTimeout(300)
      }
      await page.evaluate(async () => {
        window.__apackE2E.redraw()
        await window.__apackE2E.waitAnimationFrames(4)
      })

      const canvas = page.locator('#terminal-host canvas').first()
      const png = decodePng(await canvas.screenshot())

      // Row 0: red glyphs (`r > 180, g < 100, b < 100`)
      const redPixels = countMatchingPixels(
        png,
        { x: 0, y: 0, w: cellW * 16, h: cellH },
        (r, g, b) => r > 170 && g < 100 && b < 100,
      )
      expect(redPixels).toBeGreaterThan(20)

      // Row 1: green glyphs (`g > 180, r < 100, b < 100`)
      const greenPixels = countMatchingPixels(
        png,
        { x: 0, y: cellH, w: cellW * 18, h: cellH },
        (r, g, b) => g > 170 && r < 100 && b < 100,
      )
      expect(greenPixels).toBeGreaterThan(20)

      // Row 2: blue glyphs (`b > 170, r < 100`)
      const bluePixels = countMatchingPixels(
        png,
        { x: 0, y: cellH * 2, w: cellW * 18, h: cellH },
        (r, g, b) => b > 170 && r < 100 && g < 160,
      )
      expect(bluePixels).toBeGreaterThan(20)

      // Row 3: bright yellow background block (`r > 200, g > 180, b < 80`)
      const yellowPixels = countMatchingPixels(
        png,
        { x: 0, y: cellH * 3, w: cellW * 16, h: cellH },
        (r, g, b) => r > 200 && g > 180 && b < 80,
      )
      expect(yellowPixels).toBeGreaterThan(200)

      // Row 4: monochrome blue `######` vs color emoji `🟥🟥` (red square, NOT tinted blue!)
      const monoBluePixels = countMatchingPixels(
        png,
        { x: 0, y: cellH * 4, w: cellW * 7, h: cellH },
        (r, _g, b) => b > 150 && r < 90,
      )
      expect(monoBluePixels).toBeGreaterThan(15)

      const emojiBand = { x: cellW * 7, y: cellH * 4, w: cellW * 8, h: cellH }
      const emojiNonBg = countNonBackgroundPixels(png, emojiBand, BG_RGB)
      expect(emojiNonBg).toBeGreaterThan(15)
      // On hosts with a color emoji font (macOS Apple Color Emoji & Ubuntu Noto Color Emoji),
      // `🟥` renders strongly red-dominant pixels proving COLOR_TEX preserves native RGB channels.
      const emojiRedPixels = countMatchingPixels(png, emojiBand, (r, _g, b) => r > 150 && r > b + 60)
      expect(emojiRedPixels).toBeGreaterThan(15)

      // Rows 5..8: Kitty inline magenta image texture compositing (`r > 210, g < 50, b > 210`)
      const magentaPixels = countMatchingPixels(
        png,
        { x: 0, y: cellH * 5, w: cellW * 10, h: cellH * 4 },
        (r, g, b) => r > 210 && g < 50 && b > 210,
      )
      expect(magentaPixels).toBeGreaterThan(200)

      // Cursor: bright cyan (`0x00ffff` -> `r < 40, g > 200, b > 200`) on the `RENDER_READY_OK ` row
      const cursorPixels = countMatchingPixels(
        png,
        { x: 0, y: cellH * 5, w: png.width, h: png.height - cellH * 5 },
        (r, g, b) => r < 40 && g > 200 && b > 200,
      )
      expect(cursorPixels).toBeGreaterThan(20)

      if (mode === 'webgl2') {
        const stats = await page.evaluate(() => window.__apackE2E.painterStats())
        expect(stats?.atlasEntryCount).toBeGreaterThan(10)
        expect(stats?.imageTextureCount).toBeGreaterThan(0)
      }

      expect(pageErrors).toEqual([])
      await page.evaluate(() => window.__apackE2E.teardown())
    })

    test('supports scrollback history fetch, mouse selection/copy, search highlights, and resize/reflow', async ({
      page,
    }) => {
      const pageErrors: string[] = []
      page.on('pageerror', (err) => pageErrors.push(err.message))

      // Emit 120 lines so early lines scroll well into broker scrollback; include known selection
      // and search targets near the bottom viewport.
      const script = [
        'i=1',
        'while [ $i -le 120 ]; do',
        '  if [ $i -eq 112 ]; then printf "HIST_LINE_%04d FIND_ME_99 ALPHA\\r\\n" $i;',
        '  elif [ $i -eq 115 ]; then printf "HIST_LINE_%04d COPY_TARGET_42 BETA\\r\\n" $i;',
        '  elif [ $i -eq 117 ]; then printf "HIST_LINE_%04d FIND_ME_99 GAMMA\\r\\n" $i;',
        '  else printf "HIST_LINE_%04d_PADDING_TEXT\\r\\n" $i; fi',
        '  i=$((i+1))',
        'done',
        'sleep 15',
      ].join('\n')

      await page.goto(bridge.baseUrl)
      const mounted = await page.evaluate(
        ({ m, sh }) =>
          window.__apackE2E.mountLiveSession({
            mode: m,
            argv: ['/bin/sh', '-c', sh],
            cols: 80,
            rows: 20,
            widthPx: 720,
            heightPx: 360,
          }),
        { m: mode, sh: script },
      )
      expect(mounted.painterKind).toBe(mode)
      expect(await page.evaluate(() => window.__apackE2E.waitForText('HIST_LINE_0120', 8000))).toBe('met')
      await page.evaluate(() => window.__apackE2E.waitAnimationFrames(4))

      const canvas = page.locator('#terminal-host canvas').first()
      const bottomPng = decodePng(await canvas.screenshot())

      // 1. Scrollback history fetch: scroll to top (line 0) and confirm broker FetchRows populates early lines.
      await page.evaluate(() => window.__apackE2E.scrollTo(0))
      await expect
        .poll(async () => page.evaluate(() => window.__apackE2E.mirrorText()), { timeout: 6000 })
        .toContain('HIST_LINE_0001')
      await page.evaluate(() => window.__apackE2E.waitAnimationFrames(3))
      const topPng = decodePng(await canvas.screenshot())
      expect(Buffer.from(topPng.rgba).equals(Buffer.from(bottomPng.rgba))).toBe(false)

      // Return to live bottom viewport.
      await page.evaluate(() => window.__apackE2E.follow())
      await expect
        .poll(async () => page.evaluate(() => window.__apackE2E.mirrorText()), { timeout: 6000 })
        .toContain('HIST_LINE_0120')
      await page.evaluate(() => window.__apackE2E.waitAnimationFrames(3))

      // 2. Mouse selection + copy on the row containing `COPY_TARGET_42`.
      const visLines = await page.evaluate(() => window.__apackE2E.visibleLines())
      const copyRowIdx = visLines.findIndex((l) => l.includes('COPY_TARGET_42'))
      expect(copyRowIdx).toBeGreaterThanOrEqual(0)

      const box = await canvas.boundingBox()
      expect(box).not.toBeNull()
      const metrics = mounted.metrics!
      const dragY = box!.y + (copyRowIdx + 0.5) * metrics.cellHeight
      const dragXStart = box!.x + 12 * metrics.cellWidth
      const dragXEnd = box!.x + 34 * metrics.cellWidth

      await page.mouse.move(dragXStart, dragY)
      await page.mouse.down()
      await page.mouse.move(dragXEnd, dragY, { steps: 6 })
      await page.mouse.up()
      await page.evaluate(() => window.__apackE2E.waitAnimationFrames(3))

      const selectedPng = decodePng(await canvas.screenshot())
      // Selection highlight uses `selectionBackground: 0x3b82f6` (`b > 200, g > 100, r < 100`).
      const selPixels = countMatchingPixels(
        selectedPng,
        { x: 0, y: copyRowIdx * metrics.cellHeight, w: selectedPng.width, h: metrics.cellHeight + 2 },
        (r, g, b) => b > 190 && g > 95 && r < 100,
      )
      expect(selPixels).toBeGreaterThan(80)

      const copied = await page.evaluate(() => window.__apackE2E.copy())
      expect(copied.ok).toBe(true)
      expect(copied.text).toContain('COPY_TARGET_42')

      // 3. Live search highlights (`FIND_ME_99` appears on lines 112 & 117).
      const matchTotal = await page.evaluate(() => window.__apackE2E.find('FIND_ME_99', true))
      expect(matchTotal).toBeGreaterThanOrEqual(2)
      await page.evaluate(() => window.__apackE2E.waitAnimationFrames(3))
      const searchPng = decodePng(await canvas.screenshot())
      // `matchBackground: 0xff8800` (orange) and `currentMatchBackground: 0xff00ff` (magenta)
      const highlightPixels = countMatchingPixels(
        searchPng,
        { x: 0, y: 0, w: searchPng.width, h: searchPng.height },
        (r, g, b) => (r > 210 && g > 100 && g < 175 && b < 40) || (r > 210 && g < 40 && b > 210),
      )
      expect(highlightPixels).toBeGreaterThan(100)

      // 4. Resize / reflow: shrink container and verify fit updates + non-blank canvas.
      const fitBefore = (await page.evaluate(() => window.__apackE2E.painterStats()))?.lastFit
      expect(fitBefore).not.toBeNull()
      await page.evaluate(() => window.__apackE2E.resizeHost(420, 220))
      await expect
        .poll(async () => (await page.evaluate(() => window.__apackE2E.painterStats()))?.lastFit?.columns ?? 999, {
          timeout: 5000,
        })
        .toBeLessThan(fitBefore!.columns)
      await page.evaluate(() => window.__apackE2E.waitAnimationFrames(3))
      const resizedPng = decodePng(await canvas.screenshot())
      expect(resizedPng.width).toBeLessThan(bottomPng.width)
      expect(
        countNonBackgroundPixels(resizedPng, { x: 0, y: 0, w: resizedPng.width, h: resizedPng.height }, BG_RGB),
      ).toBeGreaterThan(200)

      expect(pageErrors).toEqual([])
      await page.evaluate(() => window.__apackE2E.teardown())
    })
  })
}

test('WebGL2AtlasPainter recovers cleanly from WEBGL_lose_context -> webglcontextrestored', async ({ page }) => {
  const pageErrors: string[] = []
  page.on('pageerror', (err) => pageErrors.push(err.message))

  const seqFile = join(bridge.tmpDir, 'webgl-loss.bin')
  const magentaImg = makeKittyInlineImage(16, 16, 6, 3, [255, 0, 255, 255], 19)
  writeFileSync(
    seqFile,
    `\x1b[38;2;255;40;40mBEFORE_AND_AFTER_LOSS\x1b[0m\r\n${magentaImg}\r\n\r\n\r\nCTX_READY\r\n`,
    'utf8',
  )

  await page.goto(bridge.baseUrl)
  const mounted = await page.evaluate(
    ({ file }) =>
      window.__apackE2E.mountLiveSession({
        mode: 'webgl2',
        argv: ['/bin/sh', '-c', `cat '${file}'; sleep 15`],
        cols: 80,
        rows: 20,
        widthPx: 720,
        heightPx: 360,
      }),
    { file: seqFile },
  )
  expect(mounted.painterKind).toBe('webgl2')
  expect(await page.evaluate(() => window.__apackE2E.waitForText('CTX_READY', 8000))).toBe('met')
  await expect
    .poll(async () => (await page.evaluate(() => window.__apackE2E.painterStats()))?.imageTextureCount ?? 0, {
      timeout: 8000,
    })
    .toBeGreaterThan(0)
  await page.evaluate(() => window.__apackE2E.waitAnimationFrames(4))

  const canvas = page.locator('#terminal-host canvas').first()
  const beforePng = decodePng(await canvas.screenshot())
  const beforeRed = countMatchingPixels(
    beforePng,
    { x: 0, y: 0, w: beforePng.width, h: 40 },
    (r, g, b) => r > 170 && g < 100 && b < 100,
  )
  const beforeMagenta = countMatchingPixels(
    beforePng,
    { x: 0, y: 12, w: 120, h: 100 },
    (r, g, b) => r > 210 && g < 50 && b > 210,
  )
  expect(beforeRed).toBeGreaterThan(20)
  expect(beforeMagenta).toBeGreaterThan(200)

  // Trigger real WebGL2 context loss & restoration via WEBGL_lose_context.
  const recovery = await page.evaluate(() => window.__apackE2E.loseAndRestoreWebGLContext())
  expect(recovery.duringLoss.isContextLost).toBe(true)
  expect(recovery.duringLoss.contextLostCount).toBe(1)
  expect(recovery.afterRestore.isContextLost).toBe(false)
  expect(recovery.afterRestore.atlasEntryCount).toBeGreaterThan(5)
  expect(recovery.afterRestore.imageTextureCount).toBeGreaterThan(0)

  // Verify real composited pixels AFTER restoration contain BOTH the red glyphs AND magenta image!
  const afterPng = decodePng(await canvas.screenshot())
  const afterRed = countMatchingPixels(
    afterPng,
    { x: 0, y: 0, w: afterPng.width, h: 40 },
    (r, g, b) => r > 170 && g < 100 && b < 100,
  )
  const afterMagenta = countMatchingPixels(
    afterPng,
    { x: 0, y: 12, w: 120, h: 100 },
    (r, g, b) => r > 210 && g < 50 && b > 210,
  )
  expect(afterRed).toBeGreaterThan(20)
  expect(afterMagenta).toBeGreaterThan(200)

  expect(pageErrors).toEqual([])
  await page.evaluate(() => window.__apackE2E.teardown())
})
