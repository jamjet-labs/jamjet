import { defineConfig } from 'tsup'

export default defineConfig({
  entry: ['src/index.ts'],
  format: ['esm'],
  dts: true,
  sourcemap: true,
  clean: true,
  target: 'es2022',
  platform: 'neutral',
  external: ['@jamjet/cloud', 'ai', '@opentelemetry/api', '@opentelemetry/sdk-trace-base'],
})
