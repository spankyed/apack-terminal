import { defineConfig } from 'vite'

// A library build: one ES module, nothing bundled in that a consumer would also carry.
export default defineConfig({
  build: {
    lib: { entry: 'src/index.ts', formats: ['es'], fileName: 'index' },
    sourcemap: true,
    target: 'es2022',
    emptyOutDir: true,
  },
})
