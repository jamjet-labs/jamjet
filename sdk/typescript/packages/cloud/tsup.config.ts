import { defineConfig } from 'tsup'

export default defineConfig({
  entry: {
    index: 'src/index.ts',
    node: 'src/node.ts',
    testing: 'src/testing.ts',
  },
  format: ['esm'],
  dts: true,
  splitting: true,
  sourcemap: true,
  clean: true,
  target: 'es2022',
  platform: 'neutral',
  treeshake: true,
  external: ['msw', 'msw/node', 'node:async_hooks', 'async_hooks'],
})
