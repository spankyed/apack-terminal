import fs from 'node:fs'
import path from 'node:path'

// TypeScript preserves `.ts` relative specifiers in emitted `.d.ts` files when `allowImportingTsExtensions`
// is enabled. Rewrite relative `.ts` specifiers in `dist/**/*.d.ts` to `.js` so consumers using NodeNext /
// Node16 / standard bundler resolution without `allowImportingTsExtensions` resolve declarations cleanly.
const dist = path.resolve(import.meta.dirname, '..', 'dist')

function rewriteDir(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      rewriteDir(full)
      continue
    }
    if (!entry.name.endsWith('.d.ts')) continue
    const before = fs.readFileSync(full, 'utf8')
    const after = before.replace(/((?:from|import\s*\(?)\s*['"]\.\.?\/[^'"]+?)\.ts(['"])/g, '$1.js$2')
    if (/['"]\.\.?\/[^'"]+?\.ts['"]/.test(after)) {
      throw new Error(`rewrite-dts: leftover relative .ts specifier in ${path.relative(dist, full)}`)
    }
    if (after !== before) {
      fs.writeFileSync(full, after, 'utf8')
    }
  }
}

rewriteDir(dist)
