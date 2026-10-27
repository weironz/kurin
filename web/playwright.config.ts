import { defineConfig } from '@playwright/test';

/**
 * WP-63 mock-SSE Playwright 配置。
 *
 * 只跑浏览器端（SSE / EventSource 在 jsdom 不可用，单元测试走注入工厂；
 * 这里的重连不丢不重验收需要真实 EventSource 与流式网络层）。
 *
 * 页面由测试内起的 Node 服务提供（静态 dist + /api/v1 mock + SSE 流），
 * 不复用 vite preview：route.fulfill 无法流式推送（响应立即结束，
 * EventSource 不派发 message），必须由真实 HTTP 分块传输。
 */
export default defineConfig({
  testDir: './tests',
  timeout: 60_000,
  retries: 0,
  workers: 1,
  use: {
    headless: true,
    channel: 'chromium-headless-shell',
    trace: 'off',
  },
});
