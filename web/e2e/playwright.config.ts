import { defineConfig } from '@playwright/test';

/**
 * WP-66：对接真实 `kurin serve` 的端到端验收。
 *
 * 与 `web/playwright.config.ts`（mock 后端）分开：这里不启动任何
 * webServer，地址、令牌与测试集群全部由环境变量提供（见 README）。
 * 未设置 `KURIN_E2E_URL` 时所有用例自动 skip。
 */
export default defineConfig({
  testDir: '.',
  timeout: 180_000,
  expect: { timeout: 30_000 },
  retries: 0,
  // 破坏性场景串行执行，避免多个用例争同一个集群锁。
  workers: 1,
  reporter: [['list']],
  use: {
    baseURL: process.env.KURIN_E2E_URL,
    headless: true,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
});
