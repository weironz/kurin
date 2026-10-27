/**
 * dev 模式启用 MSW：`pnpm dev:mocks`（VITE_USE_MOCKS=1）。
 * `msw init public/` 已生成 `public/mockServiceWorker.js`。
 */
import { setupWorker } from 'msw/browser';
import { handlers } from './handlers';

export const worker = setupWorker(...handlers);
