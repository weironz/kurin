import { beforeEach, describe, expect, it } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App as AntdApp } from 'antd';
import { loginWithToken } from '../api/client';
import { buildMockEvents, mockOperationDetail } from '../mocks/operationHandlers';
import OperationDetailPage from './OperationDetailPage';

function renderDetail(id: string): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AntdApp>
        <MemoryRouter initialEntries={[`/operations/${id}`]}>
          <Routes>
            <Route path="/operations/:id" element={<OperationDetailPage />} />
            <Route path="/operations" element={<div>操作列表</div>} />
          </Routes>
        </MemoryRouter>
      </AntdApp>
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  await loginWithToken('valid-token');
});

describe('操作详情 `/operations/:id`（WP-63）', () => {
  it(
    '渲染快照信息 + 进度网格（种子 + 事件聚合）+ 日志',
    async () => {
      renderDetail(mockOperationDetail.id);
      await waitFor(() => expect(screen.getByText('失败')).toBeInTheDocument());
      expect(screen.getByText('创建集群')).toBeInTheDocument();
      await waitFor(
        () => expect(document.querySelector('[data-testid="progress-grid"]')).toBeInTheDocument(),
      );
      expect(screen.getByText('prepare')).toBeInTheDocument();
      expect(screen.getByText('bootstrap')).toBeInTheDocument();
      await waitFor(
        () => expect(screen.getAllByText(/kubeadm-join/i).length).toBeGreaterThan(0),
        { timeout: 6000 },
      );
    },
    15_000,
  );

  it(
    '资源就绪表：Resource 事件聚合（最新一条）',
    async () => {
      renderDetail(mockOperationDetail.id);
      await waitFor(
        () => expect(document.querySelector('[data-testid="resource-table"]')).toBeInTheDocument(),
        { timeout: 6000 },
      );
      expect(screen.getByText('ingress-nginx-controller')).toBeInTheDocument();
      expect(screen.getByText('已就绪')).toBeInTheDocument();
    },
    15_000,
  );

  it(
    '点击格子 → 日志按「阶段 + 节点」过滤，再点取消',
    async () => {
      const user = userEvent.setup();
      renderDetail(mockOperationDetail.id);
      await waitFor(
        () =>
          expect(
            document.querySelector('[data-grid-stage="join"][data-grid-node="worker-1"]'),
          ).toBeInTheDocument(),
        { timeout: 6000 },
      );
      const cell = document.querySelector(
        '[data-grid-stage="join"][data-grid-node="worker-1"]',
      ) as HTMLElement;
      await user.click(cell);
      await waitFor(() => expect(screen.getByText(/日志已按/)).toBeInTheDocument());
      await user.click(screen.getByRole('button', { name: /清除格子筛选/ }));
      await waitFor(() => expect(screen.queryByText(/日志已按/)).not.toBeInTheDocument());
    },
    15_000,
  );

  it(
    '搜索：输入关键词 → 高亮 + 命中计数',
    async () => {
      const user = userEvent.setup();
      renderDetail(mockOperationDetail.id);
      await waitFor(
        () => expect(screen.getAllByText(/kubeadm-join/i).length).toBeGreaterThan(0),
        { timeout: 6000 },
      );
      await user.type(screen.getByPlaceholderText('搜索日志…'), 'FileAvailable');
      await waitFor(() => expect(screen.getByText(/个命中/)).toBeInTheDocument());
      expect(document.querySelectorAll('mark[data-highlight="true"]').length).toBeGreaterThan(0);
    },
    15_000,
  );

  it(
    '时间戳切换：绝对 → 相对',
    async () => {
      const user = userEvent.setup();
      renderDetail(mockOperationDetail.id);
      await waitFor(
        () => expect(document.querySelectorAll('[data-log-time]').length).toBeGreaterThan(0),
        { timeout: 6000 },
      );
      const first = document.querySelector('[data-log-time]') as HTMLElement;
      expect(first.textContent).toMatch(/^\d{2}:\d{2}:\d{2}\.\d{3}$/);
      await user.click(screen.getByTestId('time-mode-button'));
      await waitFor(() =>
        expect(document.querySelector('[data-log-time]')!.textContent).toMatch(
          /^\+\d{2}:\d{2}\.\d{3}$/,
        ),
      );
    },
    15_000,
  );

  it(
    '跟随按钮：暂停与回到最新',
    async () => {
      const user = userEvent.setup();
      renderDetail(mockOperationDetail.id);
      await waitFor(() => expect(screen.getByTestId('follow-button')).toBeInTheDocument(), {
        timeout: 6000,
      });
      expect(screen.getByTestId('follow-button').textContent).toContain('跟随');
      await user.click(screen.getByTestId('follow-button'));
      expect(screen.getByTestId('follow-button').textContent).toContain('回到最新');
      await user.click(screen.getByTestId('follow-button'));
      expect(screen.getByTestId('follow-button').textContent).toContain('跟随');
    },
    15_000,
  );

  it(
    '失败任务组自动展开（stderr 行可见）',
    async () => {
      renderDetail(mockOperationDetail.id);
      await waitFor(
        () => expect(screen.getAllByText(/ERROR FileAvailable/i).length).toBeGreaterThan(0),
        { timeout: 6000 },
      );
    },
    15_000,
  );

  it(
    '终态操作：无取消按钮，重试按钮存在',
    async () => {
      renderDetail(mockOperationDetail.id);
      await waitFor(() => expect(screen.getByText('失败')).toBeInTheDocument(), { timeout: 6000 });
      expect(screen.queryByRole('button', { name: /取消操作/ })).not.toBeInTheDocument();
      expect(screen.getByRole('button', { name: /重新执行/ })).toBeInTheDocument();
    },
    15_000,
  );

  it(
    '重试：确认 → 新操作 ID → 跳转（新 ID 404）',
    async () => {
      const user = userEvent.setup();
      renderDetail(mockOperationDetail.id);
      await waitFor(
        () => expect(screen.getByRole('button', { name: /重新执行/ })).toBeInTheDocument(),
        { timeout: 6000 },
      );
      await user.click(screen.getByRole('button', { name: /重新执行/ }));
      // antd Modal.confirm 的标题会渲染两份内部节点；断言至少一处出现。
      await waitFor(() =>
        expect(screen.getAllByText(/确认重新执行/).length).toBeGreaterThan(0),
      );
      await user.click(screen.getByRole('button', { name: /创建并执行/ }));
      await waitFor(() => expect(screen.getByText('操作不存在')).toBeInTheDocument(), {
        timeout: 8000,
      });
    },
    20_000,
  );

  it('不存在的操作：404 与返回链接', async () => {
    renderDetail('01JNOPE');
    await waitFor(() => expect(screen.getByText('操作不存在')).toBeInTheDocument());
    expect(screen.getByRole('link', { name: /返回操作列表/ })).toHaveAttribute(
      'href',
      '/operations',
    );
  });

  it('mock 场景事件自检：seq 连续', () => {
    const events = buildMockEvents();
    expect(events.map((e) => e.seq)).toEqual(
      Array.from({ length: events.length }, (_, i) => i + 1),
    );
    expect(events.length).toBeGreaterThanOrEqual(17);
  });
});
