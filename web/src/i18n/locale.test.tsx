import { beforeEach, describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App } from '../App';
import { loginWithToken } from '../api/client';
import { setLocale } from './index';

/**
 * 语言切换（WP-78）验收（组件级，跑在 MSW mock 上）：
 * AppShell 头部的下拉切换 zh-CN ↔ en，不刷新页面 —— 集群页标题 / 侧边栏 / 登出按钮
 * 全部即时换语言，document.title 跟随，localStorage 记住选择。
 */

/** 打开语言下拉并点选一个选项（antd Select 在 jsdom 里用 mouseDown 打开）。 */
async function pickLocale(label: 'English' | '中文'): Promise<void> {
  const selector = screen.getByTestId('locale-switch').querySelector('.ant-select-selector');
  expect(selector).not.toBeNull();
  fireEvent.mouseDown(selector as Element);
  const option = await screen.findByText(label, { selector: '.ant-select-item-option-content' });
  fireEvent.click(option);
}

beforeEach(async () => {
  window.localStorage.removeItem('kurin.locale');
  setLocale('zh-CN'); // 模块级 locale 复位（上个用例可能停在 en）
  // 建立 mock 会话（MSW handler 的 sessionEstablished 标记）
  await loginWithToken('valid-token');
});

describe('语言切换（WP-78）', () => {
  it('集群页：默认中文 → 切 English 整页重渲染并持久化 → 切回中文恢复', async () => {
    render(
      <QueryClientProvider
        client={new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } })}
      >
        <MemoryRouter initialEntries={['/']}>
          <App />
        </MemoryRouter>
      </QueryClientProvider>,
    );

    // 默认 zh-CN：页面标题 + 侧边栏菜单
    await waitFor(() => expect(screen.getAllByText('集群').length).toBeGreaterThan(0), {
      timeout: 3_000,
    });
    expect(screen.getAllByText('操作').length).toBeGreaterThan(0);
    expect(screen.getByRole('button', { name: /退出登录/ })).toBeInTheDocument();
    await waitFor(() => expect(document.title).toBe('集群 · kurin'));

    // 切到 English：标题 / 菜单 / 登出按钮全部变英文，无需刷新
    await pickLocale('English');
    await waitFor(() =>
      expect(screen.getByRole('heading', { name: 'Clusters' })).toBeInTheDocument(),
    );
    expect(screen.getAllByText('Operations').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Packages').length).toBeGreaterThan(0);
    expect(screen.getAllByText('About').length).toBeGreaterThan(0);
    expect(screen.getByRole('button', { name: /Log out/ })).toBeInTheDocument();
    await waitFor(() => expect(document.title).toBe('Clusters · kurin'));
    expect(window.localStorage.getItem('kurin.locale')).toBe('en');

    // 切回中文：文案恢复，localStorage 恢复 zh-CN
    await pickLocale('中文');
    await waitFor(() => expect(screen.getByRole('heading', { name: '集群' })).toBeInTheDocument());
    expect(screen.getAllByText('操作').length).toBeGreaterThan(0);
    expect(screen.getByRole('button', { name: /退出登录/ })).toBeInTheDocument();
    expect(window.localStorage.getItem('kurin.locale')).toBe('zh-CN');
  });
});
