import { beforeEach, describe, expect, it } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App as AntdApp } from 'antd';
import { http, HttpResponse } from 'msw';
import { server } from '../mocks/server';
import { loginWithToken } from '../api/client';
import { installAddon } from '../api/addons';
import AddonsPage from './AddonsPage';

/** 命中的路由（提交成功后跳转 /operations/:id 的探针）。 */
let landed: string | null = null;

function renderAddons(name: string): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AntdApp>
        <MemoryRouter initialEntries={[`/clusters/${name}/addons`]}>
          <Routes>
            <Route path="/clusters/:name/addons" element={<AddonsPage />} />
            <Route
              path="/operations/:id"
              element={
                <div data-testid="operation-landing">
                  {(() => {
                    landed = 'hit';
                    return null;
                  })()}
                </div>
              }
            />
          </Routes>
        </MemoryRouter>
      </AntdApp>
    </QueryClientProvider>,
  );
}

/** 预置已安装清单（走 MSW 的 install handler，页面查询即可见）。 */
async function seedInstalled(addon: string, version: string): Promise<void> {
  await installAddon('tier1', { addon, version });
}

beforeEach(async () => {
  landed = null;
  await loginWithToken('valid-token');
});

describe('Addon 管理页 `/clusters/:name/addons`（WP-64）', () => {
  it('空清单显示服务端 note，目录按分类分组渲染', async () => {
    renderAddons('tier1');
    await waitFor(() =>
      expect(screen.getByTestId('addons-catalog-metrics-server')).toBeInTheDocument(),
    );
    // 空清单 + 服务端 note
    expect(screen.getByText(/集群还没有 addon 记录/)).toBeInTheDocument();
    // 分类分组
    expect(screen.getByText('监控')).toBeInTheDocument();
    expect(screen.getByText('存储')).toBeInTheDocument();
    expect(screen.getByText('入口')).toBeInTheDocument();
    // 目录卡片：标题 / 命名空间 / 来源
    const card = screen.getByTestId('addons-catalog-metrics-server');
    expect(within(card).getByText('Metrics Server')).toBeInTheDocument();
    expect(within(card).getByText(/kube-system/)).toBeInTheDocument();
    expect(within(card).getByText(/builtin/)).toBeInTheDocument();
  });

  it('已安装表格渲染版本与状态', async () => {
    await seedInstalled('ingress-nginx', '1.15.1');
    renderAddons('tier1');
    await waitFor(() =>
      expect(screen.getByTestId('addons-status-ingress-nginx')).toBeInTheDocument(),
    );
    const table = screen.getByTestId('addons-installed-table');
    expect(within(table).getByText('ingress-nginx')).toBeInTheDocument();
    expect(within(table).getByText('1.15.1')).toBeInTheDocument();
    expect(screen.getByTestId('addons-status-ingress-nginx')).toHaveTextContent('已安装');
  });

  it('兼容性：同版本 → 已安装徽标；旧版本 → 升级徽标；冲突 → 禁用并给出原因', async () => {
    // local-path 0.0.31（目录同版本）与 metrics-server 0.8.0（目录 0.9.0）
    await seedInstalled('local-path', '0.0.31');
    await installAddon('tier1', { addon: 'metrics-server', version: '0.8.0' });
    renderAddons('tier1');
    await waitFor(() =>
      expect(screen.getByTestId('addons-state-installed-local-path')).toBeInTheDocument(),
    );
    expect(screen.getByTestId('addons-state-upgrade-metrics-server')).toHaveTextContent('0.8.0');
    // nfs-subdir 与已装 local-path 冲突：禁用 + 原因
    const conflictCard = screen.getByTestId('addons-catalog-nfs-subdir');
    expect(within(conflictCard).getByTestId('addons-conflict-nfs-subdir')).toHaveTextContent(
      'local-path',
    );
    expect(within(conflictCard).getByTestId('addons-install-nfs-subdir')).toBeDisabled();
    // 旧版本 → 只有升级入口，无安装按钮
    expect(screen.queryByTestId('addons-install-metrics-server')).not.toBeInTheDocument();
    expect(screen.getByTestId('addons-upgrade-metrics-server')).toBeEnabled();
  });

  it(
    '安装对话框：rjsf values 表单 → key=value 提交 → 跳转操作详情',
    async () => {
      const bodies: Array<Record<string, unknown>> = [];
      server.use(
        http.post('/api/v1/clusters/:name/addons', async ({ request }) => {
          bodies.push((await request.json()) as Record<string, unknown>);
          return HttpResponse.json(
            { operation: '01JDYADDONOP01', state: 'queued', cluster: 'tier1' },
            { status: 202 },
          );
        }),
      );
      const user = userEvent.setup();
      renderAddons('tier1');
      await waitFor(() =>
        expect(screen.getByTestId('addons-install-metrics-server')).toBeEnabled(),
      );
      await user.click(screen.getByTestId('addons-install-metrics-server'));
      // rjsf 表单渲染（复用向导的 AddonValuesForm）
      const modal = await screen.findByTestId('addons-install-modal');
      expect(within(modal).getByTestId('wizard-addon-values')).toBeInTheDocument();
      // 填 replicas = 2
      const input = within(modal).getByLabelText(/replicas/i);
      await user.clear(input);
      await user.type(input, '2');
      await user.click(screen.getByTestId('addons-install-confirm'));
      await waitFor(() => expect(bodies).toHaveLength(1));
      expect(bodies[0]).toMatchObject({
        addon: 'metrics-server',
        version: '0.9.0',
        values: ['replicas=2'],
      });
      await waitFor(() => expect(landed).toBe('hit'));
    },
    15_000,
  );

  it(
    '卸载：确认框点名集群与 addon，确认后发送 DELETE（confirm == 集群名）',
    async () => {
      await seedInstalled('local-path', '0.0.31');
      const bodies: Array<Record<string, unknown>> = [];
      server.use(
        http.delete('/api/v1/clusters/:name/addons', async ({ request }) => {
          bodies.push((await request.json()) as Record<string, unknown>);
          return HttpResponse.json(
            { operation: '01JDYADDONOP02', state: 'queued', cluster: 'tier1' },
            { status: 202 },
          );
        }),
      );
      const user = userEvent.setup();
      renderAddons('tier1');
      await waitFor(() =>
        expect(screen.getByTestId('addons-uninstall-local-path')).toBeInTheDocument(),
      );
      await user.click(screen.getByTestId('addons-uninstall-local-path'));
      const modal = await screen.findByTestId('addons-uninstall-modal');
      // 确认框点名集群（confirm 契约的可视面）
      expect(within(modal).getByText(/tier1/)).toBeInTheDocument();
      expect(within(modal).getByText(/local-path/)).toBeInTheDocument();
      await user.click(screen.getByTestId('addons-uninstall-confirm'));
      await waitFor(() => expect(bodies).toHaveLength(1));
      expect(bodies[0]).toMatchObject({ addon: 'local-path', confirm: 'tier1' });
      await waitFor(() => expect(landed).toBe('hit'));
    },
    15_000,
  );

  it('活动操作期间：安装禁用并显示提示', async () => {
    renderAddons('tier2-edge');
    await waitFor(() =>
      expect(screen.getByTestId('addons-catalog-metrics-server')).toBeInTheDocument(),
    );
    expect(screen.getByText(/集群有活动操作/)).toBeInTheDocument();
    expect(screen.getByTestId('addons-install-metrics-server')).toBeDisabled();
  });

  it('不存在的集群：404 与返回链接', async () => {
    renderAddons('nope');
    await waitFor(() => expect(screen.getByText('集群不存在')).toBeInTheDocument());
    expect(screen.getByRole('link', { name: '返回集群列表' })).toHaveAttribute('href', '/');
  });

  it('返回链接指向集群详情（详情页头部入口的落点）', async () => {
    renderAddons('tier1');
    await waitFor(() =>
      expect(screen.getByTestId('addons-catalog-metrics-server')).toBeInTheDocument(),
    );
    expect(screen.getByRole('link', { name: '返回集群详情' })).toHaveAttribute(
      'href',
      '/clusters/tier1',
    );
  });
});
