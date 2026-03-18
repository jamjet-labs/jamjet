import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  server: {
    port: 4261,
    proxy: {
      '/api': { target: 'http://localhost:7700', changeOrigin: true },
      '/executions': { target: 'http://localhost:7700', changeOrigin: true },
      '/agents': { target: 'http://localhost:7700', changeOrigin: true },
      '/health': { target: 'http://localhost:7700', changeOrigin: true },
      '/ws': { target: 'ws://localhost:7700', ws: true },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
  },
})
