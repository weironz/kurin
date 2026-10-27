/// <reference types="vitest" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import viteCompression from 'vite-plugin-compression';

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    react(),
    // 产物输出 gzip 版本（rust-embed 只嵌入 .gz，见 UI 文档 §9）
    viteCompression({
      algorithm: 'gzip',
      // 全量压缩：rust-embed 只嵌入 .gz（UI §9），index.html 也必须有 .gz
      threshold: 0,
      // 只保留 .gz：rust-embed 只嵌入 gzip 副本（UI §9）
      deleteOriginFile: true,
    }),
  ],
  server: {
    port: 5173,
    // /api 代理到 kurin serve（dev 时）；MSW 模式下不会走到这里
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:7080',
        changeOrigin: false,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: false,
    rollupOptions: {
      output: {
        manualChunks: {
          react: ['react', 'react-dom', 'react-router-dom'],
          antd: ['antd', '@ant-design/icons'],
          query: ['@tanstack/react-query'],
        },
      },
    },
  },
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    css: false,
    include: ['src/**/*.test.{ts,tsx}'],
  },
});
