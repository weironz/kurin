import '@testing-library/jest-dom/vitest';
import { afterAll, afterEach, beforeAll, vi } from 'vitest';
import { server } from '../mocks/server';
import { resetSessionState } from '../mocks/handlers';

// jsdom 未实现 matchMedia，antd 的响应式观察者需要它
if (typeof window.matchMedia !== 'function') {
  window.matchMedia = vi.fn().mockImplementation((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addListener: () => undefined,
    removeListener: () => undefined,
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
    dispatchEvent: () => false,
  }));
}
// 注：jsdom 不实现 EventSource；SSE 测试一律通过 eventSourceFactory 注入假实现，
// 生产代码路径（真 EventSource）仅在浏览器中执行。
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }));
afterEach(() => {
  server.resetHandlers();
  resetSessionState();
  window.history.replaceState(null, '', '/');
});
afterAll(() => server.close());
