import { beforeEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { http, HttpResponse } from 'msw';
import dayjs from 'dayjs';
import relativeTime from 'dayjs/plugin/relativeTime';
import { server } from '../mocks/server';
import { mockClusters } from '../mocks/handlers';
import { clustersQuery } from '../api/queries';
import { loginWithToken } from '../api/client';
import ClustersPage from './ClustersPage';

dayjs.extend(relativeTime);

const observedAt = '2026-10-27T08:00:00.000Z';

/** 每个用例独立的 QueryClient + MemoryRouter（页面需要 useLocation/useNavigate）。 */
function renderOverview(): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter>
        <ClustersPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  // 建立 mock 会话（MSW handler 的 sessionEstablished 标记）
  await loginWithToken('valid-token');
});

describe('集群总览 `/`（WP-61）', () => {
  it('渲染五种健康徽标、证书/节点列与当前操作链接', async () => {
    renderOverview();
    await waitFor(() => expect(screen.getByText('tier1')).toBeInTheDocument());
    // 五种健康徽标（mockClusters 覆盖全部状态；按行限定避免与表头「健康」撞文本）
    const badgeInRow = (cluster: string, badge: string): HTMLElement => {
      const row = screen.getByText(cluster).closest('tr');
      expect(row).not.toBeNull();
      return within(row!).getByText(badge);
    };
    expect(badgeInRow('tier1', '健康')).toBeInTheDocument();
    expect(badgeInRow('tier2-edge', '异常')).toBeInTheDocument();
    expect(badgeInRow('tier3-ha', '不可达')).toBeInTheDocument();
    expect(badgeInRow('lab-old', '数据过期')).toBeInTheDocument();
    expect(badgeInRow('lab-draft', '无快照')).toBeInTheDocument();
    // 证书最早到期 / 节点 Ready
    expect(screen.getByText('210 天')).toBeInTheDocument();
    expect(screen.getByText('1/2')).toBeInTheDocument();
    // 当前操作：仅 queued/running 的 activeOperation 渲染链接
    const opLink = screen.getByRole('link', { name: '添加节点' });
    expect(opLink).toHaveAttribute('href', '/operations/01JDY8Z9X7M2Q3R4S5T6U7V8WB');
    // 名称链接到详情页
    expect(screen.getByRole('link', { name: 'tier2-edge' })).toHaveAttribute(
      'href',
      '/clusters/tier2-edge',
    );
  });
  it('最近观测列：相对时间 + Tooltip 绝对时间', async () => {
    renderOverview();
    await waitFor(() => expect(screen.getByText('tier1')).toBeInTheDocument());
    const relText = dayjs(observedAt).fromNow();
    const cells = screen.getAllByText(relText);
    expect(cells.length).toBeGreaterThan(0);
    fireEvent.mouseEnter(cells[0]!);
    const absolute = dayjs(observedAt).format('YYYY-MM-DD HH:mm:ss');
    await waitFor(() => expect(screen.getAllByText(absolute).length).toBeGreaterThan(0));
  });

  it('15 秒自动刷新：fake timers 推进后重取', async () => {
    // 查询选项本身断言轮询间隔
    expect(clustersQuery.refetchInterval).toBe(15_000);
    let calls = 0;
    server.use(
      http.get('/api/v1/clusters', () => {
        calls += 1;
        return HttpResponse.json(mockClusters);
      }),
    );
    vi.useFakeTimers();
    try {
      renderOverview();
      // 初次请求完成并渲染（MSW 在真实 I/O 上解析，逐段推进保证冲刷）
      for (let i = 0; i < 10 && calls === 0; i += 1) {
        await act(async () => {
          await vi.advanceTimersByTimeAsync(10);
        });
      }
      expect(screen.getByText('tier1')).toBeInTheDocument();
      expect(calls).toBe(1);
      // 推进 10 秒：不到 15 秒间隔，不应重取
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10_000);
      });
      expect(calls).toBe(1);
      // 推进到 15 秒整点之后：恰好重取一次
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_200);
      });
      expect(calls).toBe(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('空状态：无集群时显示创建引导', async () => {
    server.use(http.get('/api/v1/clusters', () => HttpResponse.json([])));
    renderOverview();
    await waitFor(() => expect(screen.getByText('还没有集群')).toBeInTheDocument());
    expect(screen.getByText('通过导入 TOML 或创建向导登记第一个集群')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /新建集群/ })).toBeInTheDocument();
  });

  it('错误状态：显示错误并可在后端恢复后重试', async () => {
    server.use(
      http.get('/api/v1/clusters', () =>
        HttpResponse.json({ code: 'unavailable', message: '服务正在关闭' }, { status: 503 }),
      ),
    );
    renderOverview();
    await waitFor(() => expect(screen.getByText('集群列表加载失败')).toBeInTheDocument());
    expect(screen.getByText('服务正在关闭')).toBeInTheDocument();
    // 后端恢复后点击重试
    server.use(http.get('/api/v1/clusters', () => HttpResponse.json(mockClusters)));
    fireEvent.click(screen.getByRole('button', { name: /重\s*试/ }));
    await waitFor(() => expect(screen.getByText('tier1')).toBeInTheDocument());
  });
});
