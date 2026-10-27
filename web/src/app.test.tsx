import { describe, expect, it, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App } from './App';
import { loginWithToken, logout } from './api/client';

/** 每个用例独立的 QueryClient + MemoryRouter（lazy chunk 需要 waitFor）。 */
function renderAt(path: string): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[path]}>
        <App />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

/** 侧边栏 / 页面标题可能同文案，统一用 getAllByText 断言至少出现一次。 */
function expectTextVisible(text: string): void {
  expect(screen.getAllByText(text).length).toBeGreaterThan(0);
}

beforeEach(async () => {
  // 建立 mock 会话（MSW handler 的 sessionEstablished 标记）
  await loginWithToken('valid-token');
});

describe('app shell 渲染（所有路由，无真实后端）', () => {
  const cases: Array<{ path: string; heading: string }> = [
    { path: '/', heading: '集群' },
    { path: '/clusters/new', heading: '新建集群' },
    { path: '/clusters/tier1', heading: '集群详情' },
    { path: '/operations', heading: '操作' },
    { path: '/operations/01JDY8Z9X7M2Q3R4S5T6U7V8WA', heading: '操作详情' },
    { path: '/packages', heading: '包' },
    { path: '/about', heading: '关于' },
  ];

  for (const { path, heading } of cases) {
    it(`${path} 渲染「${heading}」`, async () => {
      renderAt(path);
      await waitFor(() => expectTextVisible(heading), { timeout: 3_000 });
      // 侧边栏四项（WP-60：集群 / 操作 / 包 / 关于）
      expectTextVisible('集群');
      expectTextVisible('操作');
      expectTextVisible('包');
      expectTextVisible('关于');
      // 头部登出按钮
      expect(screen.getByRole('button', { name: /退出登录/ })).toBeInTheDocument();
    });
  }

  it('集群总览表格渲染 mock 数据（tier1 / tier2-edge）', async () => {
    renderAt('/');
    await waitFor(() => expect(screen.getByText('tier1')).toBeInTheDocument());
    expect(screen.getByText('tier2-edge')).toBeInTheDocument();
    expect(screen.getByText('kubeadm')).toBeInTheDocument();
    expect(screen.getAllByText('健康').length).toBeGreaterThan(0);
  });

  it('关于页渲染版本与能力表', async () => {
    renderAt('/about');
    await waitFor(() => expect(screen.getByText('0.4.0')).toBeInTheDocument(), {
      timeout: 3_000,
    });
    expect(screen.getByText('组件版本')).toBeInTheDocument();
    expect(screen.getByText('能力约束')).toBeInTheDocument();
  });

  it('未登录时渲染登录页', async () => {
    await logout(); // 撤销 beforeEach 建立的会话
    renderAt('/');
    await waitFor(() => expect(screen.getByText('登录 kurin')).toBeInTheDocument());
    expect(screen.getByLabelText('访问令牌')).toBeInTheDocument();
  });

  it('未知路由重定向到 /', async () => {
    renderAt('/nonexistent');
    await waitFor(() => expect(screen.getByText('tier1')).toBeInTheDocument(), {
      timeout: 3_000,
    });
  });
});
