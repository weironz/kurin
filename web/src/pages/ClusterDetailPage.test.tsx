import { beforeEach, describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { loginWithToken } from '../api/client';
import ClusterDetailPage from './ClusterDetailPage';

/** 每个用例独立的 QueryClient；路由只挂详情页（页面内部用 useParams/useQuery）。 */
function renderDetail(name: string): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[`/clusters/${name}`]}>
        <Routes>
          <Route path="/clusters/:name" element={<ClusterDetailPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

/**
 * antd Tabs 会保留已访问的面板（display: none）；
 * getByRole('tabpanel') 只返回可访问树中的活动面板，用它限定查询范围。
 */
function activePanel(): HTMLElement {
  return screen.getByRole('tabpanel');
}

beforeEach(async () => {
  // 建立 mock 会话（MSW handler 的 sessionEstablished 标记）
  await loginWithToken('valid-token');
});

describe('集群详情 `/clusters/:name`（WP-61）', () => {
  it('概览 Tab：快照统计、etcd、观测节点表与当前操作横幅', async () => {
    renderDetail('tier2-edge');
    await waitFor(() => expect(screen.getByText('edge-cp')).toBeInTheDocument());
    // 快照统计
    expect(screen.getByText('未运行 Pod')).toBeInTheDocument();
    expect(screen.getByText('90 天')).toBeInTheDocument();
    expect(screen.getByText('正常')).toBeInTheDocument();
    expect(screen.getByText('1/2')).toBeInTheDocument();
    // 观测节点表
    expect(screen.getByText('edge-worker-1')).toBeInTheDocument();
    expect(screen.getByText('未就绪')).toBeInTheDocument();
    expect(screen.getAllByText('arm64').length).toBeGreaterThan(0);
    // 当前操作横幅（queued/running）
    const banner = screen.getByRole('link', { name: '添加节点 · 执行中' });
    expect(banner).toHaveAttribute('href', '/operations/01JDY8Z9X7M2Q3R4S5T6U7V8WB');
  });

  it('概览 Tab：不可达集群显示采集失败且无观测节点', async () => {
    renderDetail('tier3-ha');
    await waitFor(() => expect(screen.getByText('状态采集失败')).toBeInTheDocument());
    expect(screen.getByText(/dial tcp 10\.0\.1\.11:22: i\/o timeout/)).toBeInTheDocument();
    expect(screen.getByText('暂无观测节点')).toBeInTheDocument();
    expect(screen.getByText('0/0')).toBeInTheDocument();
  });

  it('节点 Tab：期望缺失的节点被标记且显示期望地址', async () => {
    renderDetail('tier2-edge');
    await waitFor(() => expect(screen.getByText('edge-cp')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('tab', { name: '节点' }));
    const panel = activePanel();
    // edge-worker-2 在配置里但快照从未观测到
    expect(within(panel).getByText('edge-worker-2')).toBeInTheDocument();
    expect(within(panel).getByText('期望缺失')).toBeInTheDocument();
    expect(within(panel).getByText('10.1.0.22')).toBeInTheDocument();
    // 合并行：角色 / 架构 / OS / kubelet 版本来自观测
    expect(within(panel).getByText('控制面')).toBeInTheDocument();
    expect(within(panel).getAllByText('arm64').length).toBeGreaterThan(0);
    expect(within(panel).getAllByText('v1.33.1+k3s1').length).toBeGreaterThan(0);
  });

  it('节点 Tab：观测到但未声明的节点被标记', async () => {
    renderDetail('lab-old');
    await waitFor(() => expect(screen.getByText('cp-1')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('tab', { name: '节点' }));
    const panel = activePanel();
    expect(within(panel).getByText('manual-node')).toBeInTheDocument();
    expect(within(panel).getByText('未声明')).toBeInTheDocument();
    // 未声明行无期望地址
    expect(within(panel).queryByText('192.168.10.99')).not.toBeInTheDocument();
  });

  it('配置 Tab：只读摘要、修订号与 CodeMirror 占位', async () => {
    renderDetail('tier2-edge');
    await waitFor(() => expect(screen.getByText('edge-cp')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('tab', { name: '配置' }));
    const panel = activePanel();
    expect(within(panel).getByText('配置摘要')).toBeInTheDocument();
    expect(within(panel).getByText('配置修订')).toBeInTheDocument();
    expect(within(panel).getByText('#1')).toBeInTheDocument();
    expect(within(panel).getByText('flannel')).toBeInTheDocument();
    expect(within(panel).getByText('10.42.0.0/16')).toBeInTheDocument();
    expect(within(panel).getByText(/CodeMirror 6 TOML 编辑器/)).toBeInTheDocument();
  });

  it('操作历史 Tab：按集群查询并链接到操作详情', async () => {
    renderDetail('tier2-edge');
    await waitFor(() => expect(screen.getByText('edge-cp')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('tab', { name: '操作历史' }));
    const panel = activePanel();
    const link = await within(panel).findByRole('link', {
      name: '01JDY8Z9X7M2Q3R4S5T6U7V8WB',
    });
    expect(link).toHaveAttribute('href', '/operations/01JDY8Z9X7M2Q3R4S5T6U7V8WB');
    expect(within(panel).getByText('执行中')).toBeInTheDocument();
    expect(within(panel).getByText('证书续期')).toBeInTheDocument();
    // ?cluster=tier2-edge 过滤：不含其他集群的操作
    expect(within(panel).queryByText('预检查')).not.toBeInTheDocument();
  });

  it('不存在的集群：404 与返回链接', async () => {
    renderDetail('nope');
    await waitFor(() => expect(screen.getByText('集群不存在')).toBeInTheDocument());
    expect(screen.getByRole('link', { name: '返回集群列表' })).toHaveAttribute('href', '/');
  });
});
