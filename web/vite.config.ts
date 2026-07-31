import { tanstackRouter } from '@tanstack/router-plugin/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

export default defineConfig({
  plugins: [tanstackRouter({ autoCodeSplitting: true }), react()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:8765',
      '/asset': 'http://127.0.0.1:8765',
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    include: ['src/**/*.test.{ts,tsx}'],
    pool: 'threads',
    maxWorkers: 3,
    setupFiles: './src/test/setup.ts',
  },
})
