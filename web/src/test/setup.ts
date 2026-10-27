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
// CodeMirror 6 用 Range.getClientRects 测量文本；jsdom 没有实现它，
// 缺少时测量循环会抛 TypeError 并让 Vitest 以未捕获异常失败。
if (typeof Range !== 'undefined' && !Range.prototype.getClientRects) {
  Range.prototype.getClientRects = function getClientRects() {
    return {
      length: 0,
      item: () => null,
      [Symbol.iterator]: function* empty() {},
    } as unknown as DOMRectList;
  };
}
if (typeof Range !== 'undefined' && !Range.prototype.getBoundingClientRect) {
  Range.prototype.getBoundingClientRect = () => new DOMRect(0, 0, 0, 0);
}
if (typeof Element !== 'undefined' && !Element.prototype.scrollTo) {
  Element.prototype.scrollTo = () => undefined;
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
