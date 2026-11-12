// Runs the painter timing page (painter-timing.ts) in Chromium and prints its result as one JSON line.
//
//   PLAYWRIGHT=<path to a playwright module> [CHROME=<a chromium binary>] \
//     node bench/run-painter-timing.mjs [frames=300] [minimumContrast=4.5]
//
// The page is served by a vite dev server this script does not start: run `npx vite --port 5199` in view/ first, and set
// PAGE=http://127.0.0.1:5199 if it is elsewhere. The page's settings are the query string: see painter-timing.ts.
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT ?? 'playwright')
const page = process.env.PAGE ?? 'http://127.0.0.1:5199'
const query = process.argv.slice(2).join('&')
const browser = await chromium.launch({ headless: true, executablePath: process.env.CHROME || undefined })
const tab = await browser.newPage({ viewport: { width: 1700, height: 1000 } })
tab.on('pageerror', (error) => console.error('pageerror', String(error)))
await tab.goto(`${page}/bench/painter-timing.html?${query}`)
await tab.waitForFunction(() => window.__result !== undefined, null, { timeout: 900000, polling: 250 })
console.log(JSON.stringify(await tab.evaluate(() => window.__result)))
await browser.close()
