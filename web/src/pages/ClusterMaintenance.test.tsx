import { beforeEach, describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App as AntdApp } from 'antd';
import { http, HttpResponse } from 'msw';
import { server } from '../mocks/server';
import { loginWithToken } from '../api/client';
import ClusterDetailPage from './ClusterDetailPage';

/**
 * 导航探针：读当前 pathname（不能用「渲染即执行」的 IIFE 标记变量——
 * JSX 求值时它就跑掉了，断言「未跳转」永远失败）。
 * 输入一律 fireEvent.change：userEvent 的逐键模拟在 antd Modal 里极慢。
 */
function LocationProbe() {
  const location = useLocation();
  return (
    <div data-testid="nav-location" style={{ display: 'none' }}>
      {location.pathname}
    </div>
  );
}

function renderDetail(name: string): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AntdApp>
        <MemoryRouter initialEntries={[`/clusters/${name}`]}>
          <LocationProbe />
          <Routes>
            <Route path="/clusters/:name" element={<ClusterDetailPage />} />
            <Route path="/operations/:id" element={<div data-testid="operation-landing" />} />
          </Routes>
        </MemoryRouter>
      </AntdApp>
    </QueryClientProvider>,
  );
}

/** 等待跳转到操作详情页。 */
async function expectNavigatedToOperation(): Promise<void> {
  await waitFor(() =>
    expect(screen.getByTestId('nav-location').textContent).toMatch(/\/operations\//),
  );
}

/** 等待集群详情加载完成（观测节点表出现首行）。 */
async function expectDetailLoaded(node: string): Promise<void> {
  await waitFor(() => expect(screen.getByText(node)).toBeInTheDocument());
}

beforeEach(async () => {
  await loginWithToken('valid-token');
});

describe('升级对话框（WP-65）', () => {
  it('semver 与批大小校验：非法输入不提交，合法预览提交 dry_run: true', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/clusters/:name/operations', async ({ request }) => {
        bodies.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json(
          { id: '01JDYUPGRADE0001', state: 'succeeded', cluster: 'tier1' },
          { status: 202 },
        );
      }),
    );
    renderDetail('tier1');
    await expectDetailLoaded('cp-1');

    fireEvent.click(screen.getByTestId('cluster-upgrade-open'));
    const modal = await screen.findByTestId('cluster-upgrade-modal');
    expect(within(modal).getByTestId('cluster-upgrade-current')).toHaveTextContent('1.33.1');

    // 空 target：必填拦截
    fireEvent.click(screen.getByTestId('cluster-upgrade-preview'));
    await waitFor(() => expect(within(modal).getByText('目标版本必填')).toBeInTheDocument());

    // 非 semver target：形状拦截
    fireEvent.change(within(modal).getByTestId('cluster-upgrade-target'), {
      target: { value: 'latest' },
    });
    fireEvent.click(screen.getByTestId('cluster-upgrade-preview'));
    await waitFor(() => expect(within(modal).getByText(/语义化版本/)).toBeInTheDocument());
    expect(bodies).toHaveLength(0);
    expect(screen.getByTestId('nav-location').textContent).toBe('/clusters/tier1');

    // 合法 target：预览提交 dry_run: true（batch 默认 1，force 默认 false）
    fireEvent.change(within(modal).getByTestId('cluster-upgrade-target'), {
      target: { value: '1.34.0' },
    });
    fireEvent.click(screen.getByTestId('cluster-upgrade-preview'));
    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toEqual({
      kind: 'upgrade',
      params: { target: '1.34.0', batch: 1, dry_run: true, force_drain_failure: false },
    });
    await expectNavigatedToOperation();
  });

  it('执行升级：不带 dry_run；batch 与 force 透传；批大小 0 被拦截', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/clusters/:name/operations', async ({ request }) => {
        bodies.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json(
          { id: '01JDYUPGRADE0002', state: 'queued', cluster: 'tier1' },
          { status: 202 },
        );
      }),
    );
    renderDetail('tier1');
    await expectDetailLoaded('cp-1');

    fireEvent.click(screen.getByTestId('cluster-upgrade-open'));
    const modal = await screen.findByTestId('cluster-upgrade-modal');
    fireEvent.change(within(modal).getByTestId('cluster-upgrade-target'), {
      target: { value: '1.34.0' },
    });

    // 批大小 0：拦截
    fireEvent.change(within(modal).getByTestId('cluster-upgrade-batch'), {
      target: { value: '0' },
    });
    fireEvent.click(screen.getByTestId('cluster-upgrade-execute'));
    await waitFor(() => expect(within(modal).getByText(/至少为 1/)).toBeInTheDocument());
    expect(bodies).toHaveLength(0);
    expect(screen.getByTestId('nav-location').textContent).toBe('/clusters/tier1');

    // batch = 3 + force 勾选：真实提交（无 dry_run）
    fireEvent.change(within(modal).getByTestId('cluster-upgrade-batch'), {
      target: { value: '3' },
    });
    fireEvent.click(within(modal).getByTestId('cluster-upgrade-force-drain'));
    fireEvent.click(screen.getByTestId('cluster-upgrade-execute'));

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toEqual({
      kind: 'upgrade',
      params: { target: '1.34.0', batch: 3, dry_run: false, force_drain_failure: true },
    });
    await expectNavigatedToOperation();
  });

  it('活动操作期间升级入口禁用（tier2-edge 有 running 操作）', async () => {
    renderDetail('tier2-edge');
    await expectDetailLoaded('edge-cp');
    expect(screen.getByTestId('cluster-upgrade-open')).toBeDisabled();
  });

  it('服务端拒绝非法 target：400 就地展示且不跳转', async () => {
    renderDetail('tier1');
    await expectDetailLoaded('cp-1');

    fireEvent.click(screen.getByTestId('cluster-upgrade-open'));
    const modal = await screen.findByTestId('cluster-upgrade-modal');
    // v 前缀过前端形状检查，但服务端（默认 handler）只认 x.y.z 开头 → 400
    fireEvent.change(within(modal).getByTestId('cluster-upgrade-target'), {
      target: { value: 'v1.34.0' },
    });
    fireEvent.click(screen.getByTestId('cluster-upgrade-preview'));

    expect(await screen.findByTestId('cluster-upgrade-error')).toHaveTextContent(
      /目标版本不合法/,
    );
    // 未跳转：仍停在集群详情，对话框保持打开
    expect(screen.getByTestId('nav-location').textContent).toBe('/clusters/tier1');
    expect(screen.getByTestId('cluster-upgrade-modal')).toBeInTheDocument();
  });
});

describe('证书 Tab（WP-65）', () => {
  it('显示快照证书剩余天数；续期提交 force 并跳转', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/clusters/:name/operations', async ({ request }) => {
        bodies.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json(
          { operation: '01JDYCERTS000001', state: 'queued', cluster: 'tier1' },
          { status: 202 },
        );
      }),
    );
    renderDetail('tier1');
    await expectDetailLoaded('cp-1');

    fireEvent.click(screen.getByRole('tab', { name: '证书' }));
    const panel = screen.getByRole('tabpanel');
    expect(within(panel).getByTestId('certs-min-days')).toHaveTextContent('210 天');

    // force 勾选后提交
    fireEvent.click(within(panel).getByTestId('certs-force'));
    fireEvent.click(within(panel).getByTestId('certs-renew'));

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toEqual({ kind: 'certs-renew', params: { force: true } });
    await expectNavigatedToOperation();
  });

  it('活动操作期间续期禁用（快照有证书天数）', async () => {
    renderDetail('tier2-edge');
    await expectDetailLoaded('edge-cp');
    fireEvent.click(screen.getByRole('tab', { name: '证书' }));
    const panel = screen.getByRole('tabpanel');
    // tier2-edge 快照有 certMinDays = 90，但有 running 操作
    expect(within(panel).getByTestId('certs-min-days')).toHaveTextContent('90 天');
    expect(within(panel).getByTestId('certs-renew')).toBeDisabled();
  });

  it('无快照显示「未采集」且可续期（lab-draft）', async () => {
    renderDetail('lab-draft');
    await waitFor(() => expect(screen.getByText('暂无快照')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('tab', { name: '证书' }));
    const panel = screen.getByRole('tabpanel');
    expect(within(panel).getByTestId('certs-min-days')).toHaveTextContent('未采集');
    expect(within(panel).getByTestId('certs-renew')).toBeEnabled();
  });
});

describe('添加节点（WP-66 的加 worker 流程）', () => {
  it('批量粘贴 → 解析 → 提交结构化 add-nodes 并跳转操作', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/clusters/:name/nodes', async ({ request }) => {
        bodies.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json(
          { operation: '01JDYADDNODES0001', state: 'queued', cluster: 'tier1' },
          { status: 202 },
        );
      }),
    );
    renderDetail('tier1');
    await expectDetailLoaded('cp-1');
    fireEvent.click(screen.getByRole('tab', { name: /节点/ }));

    fireEvent.click(screen.getByTestId('cluster-add-nodes-open'));
    const modal = await screen.findByTestId('add-nodes-modal');
    fireEvent.change(within(modal).getByTestId('add-nodes-input'), {
      target: { value: '10.0.0.22-22 registry' },
    });
    fireEvent.click(within(modal).getByTestId('add-nodes-parse'));
    await waitFor(() => expect(within(modal).getByTestId('add-nodes-table')).toBeInTheDocument());
    fireEvent.click(screen.getByTestId('add-nodes-submit'));

    await expectNavigatedToOperation();
    expect(bodies).toHaveLength(1);
    expect(bodies[0]).toEqual({
      nodes: [{ name: 'registry-1', address: '10.0.0.22', role: 'registry' }],
    });
  });
});
