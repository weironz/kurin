import { beforeEach, describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App as AntdApp } from 'antd';
import type { ClusterDetail } from '../api/types';
import { loginWithToken } from '../api/client';
import { ClusterDangerZone } from './ClusterDangerZone';

/** 命中的路由（提交成功后跳转 /operations/:id 的探针）。 */
let landed: string | null = null;

/** 最小详情夹具：危险区只读 name / desiredNodes / activeOperation。 */
function detail(overrides: Partial<ClusterDetail> = {}): ClusterDetail {
  return {
    name: 'tier1',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.33.1',
    lifecycle: 'active',
    currentRev: 3,
    createdAt: '2026-09-01T08:00:00.000Z',
    updatedAt: '2026-10-20T08:00:00.000Z',
    desiredNodes: [
      { name: 'cp-1', address: '10.0.0.11', role: 'control-plane' },
      { name: 'worker-1', address: '10.0.0.21', role: 'worker' },
    ],
    observedNodes: [],
    snapshot: null,
    activeOperation: null,
    collectError: null,
    configSummary: {
      distribution: 'kubeadm',
      kubernetesVersion: 'v1.33.1',
      runtime: 'containerd',
      zone: 'global',
      cni: 'calico',
      podCidr: '10.244.0.0/16',
      serviceCidr: '10.96.0.0/12',
      haMode: 'none',
      vip: null,
    },
    ...overrides,
  };
}

function renderZone(d: ClusterDetail): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AntdApp>
        <MemoryRouter initialEntries={['/']}>
          <Routes>
            <Route path="/" element={<ClusterDangerZone detail={d} />} />
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

async function pickSelectOption(label: string): Promise<void> {
  fireEvent.mouseDown(
    screen.getByTestId('danger-delete-node-select').querySelector('.ant-select-selector')!,
  );
  // `role=option` 的 aria 节点不带点击处理器，真正的目标是它外层的 item div。
  fireEvent.click(await screen.findByTitle(label));
}

beforeEach(async () => {
  landed = null;
  await loginWithToken('valid-token');
});

describe('危险区（WP-64）', () => {
  it('渲染三块操作：删除节点 / 销毁集群 / 移除纳管', () => {
    renderZone(detail());
    expect(screen.getByTestId('danger-delete-node')).toBeInTheDocument();
    expect(screen.getByTestId('danger-destroy')).toBeInTheDocument();
    expect(screen.getByTestId('danger-archive')).toBeInTheDocument();
    expect(screen.getByTestId('danger-delete-node-open')).toBeDisabled(); // 未选节点
  });

  it(
    '删除节点：选择节点 → 确认框点名集群与节点 → 提交 delete-node + confirm',
    async () => {
      const user = userEvent.setup();
      renderZone(detail());
      await pickSelectOption('worker-1');
      await user.click(screen.getByTestId('danger-delete-node-open'));
      const modal = screen.getByTestId('danger-delete-node-modal');
      expect(within(modal).getByText(/tier1/)).toBeInTheDocument();
      expect(within(modal).getByText(/worker-1/)).toBeInTheDocument();
      // 提交（服务端要求 confirm == 集群名，UI 已带）
      await user.click(screen.getByTestId('danger-delete-node-confirm'));
      await waitFor(() => expect(landed).toBe('hit'), { timeout: 5000 });
    },
    15_000,
  );

  it(
    '销毁集群：输入不匹配时确认键禁用，匹配后才可提交',
    async () => {
      const user = userEvent.setup();
      renderZone(detail());
      await user.click(screen.getByTestId('danger-destroy-open'));
      const modal = screen.getByTestId('danger-destroy-modal');
      expect(within(modal).getByText(/tier1/)).toBeInTheDocument();
      const input = within(modal).getByTestId('danger-destroy-input');
      const confirm = screen.getByTestId('danger-destroy-confirm');
      expect(confirm).toBeDisabled();
      await user.type(input, 'tier');
      expect(confirm).toBeDisabled();
      await user.type(input, '1');
      expect(confirm).toBeEnabled();
      await user.click(confirm);
      await waitFor(() => expect(landed).toBe('hit'), { timeout: 5000 });
    },
    15_000,
  );

  it(
    '移除纳管：无需输入集群名，确认即提交并关闭对话框',
    async () => {
      const user = userEvent.setup();
      renderZone(detail());
      await user.click(screen.getByTestId('danger-archive-open'));
      const modal = screen.getByTestId('danger-archive-modal');
      expect(within(modal).getByText(/tier1/)).toBeInTheDocument();
      expect(screen.getByTestId('danger-archive-confirm')).toBeEnabled();
      await user.click(screen.getByTestId('danger-archive-confirm'));
      // 归档不创建操作：成功后回到集群列表，对话框收起（antd 保留 DOM，
      // 断言它已被隐藏）。
      await waitFor(
        () => {
          const modal = screen.getByTestId('danger-archive-modal').closest('.ant-modal-wrap');
          expect(modal).toHaveStyle({ display: 'none' });
        },
        { timeout: 8000 },
      );
    },
    15_000,
  );

  it('文案区分：销毁强调不可恢复，移除纳管强调远端集群不受影响', async () => {
    const user = userEvent.setup();
    renderZone(detail());
    await user.click(screen.getByTestId('danger-destroy-open'));
    const destroyModal = screen.getByTestId('danger-destroy-modal');
    const destroyCopy = destroyModal.textContent ?? '';
    await user.click(screen.getByRole('button', { name: /取\s*消/ }));
    await user.click(screen.getByTestId('danger-archive-open'));
    const archiveModal = screen.getByTestId('danger-archive-modal');
    const archiveCopy = archiveModal.textContent ?? '';
    // 销毁：远端资源被清除且不可恢复
    expect(destroyCopy).toContain('不可恢复');
    expect(destroyCopy).not.toContain('不受影响');
    // 移除纳管：远端集群保持原样（与销毁相对）
    expect(archiveCopy).toContain('不受影响');
    expect(archiveCopy).not.toContain('不可恢复');
    // 两份文案不是同一段话
    expect(archiveCopy).not.toBe(destroyCopy);
    // 卡片说明同样区分（销毁描述提到清除远端资源）
    expect(screen.getByTestId('danger-destroy').textContent).toContain('远端');
    expect(screen.getByTestId('danger-archive').textContent).toContain('不受影响');
  });

  it('活动操作期间：三个操作全部禁用并提示', () => {
    renderZone(
      detail({
        activeOperation: {
          id: '01JDY8Z9X7M2Q3R4S5T6U7V8WB',
          cluster: 'tier1',
          kind: 'add-nodes',
          state: 'running',
          params: {},
          actor: 'mock',
          error: null,
          createdAt: '2026-10-27T07:00:00.000Z',
          startedAt: '2026-10-27T07:00:01.000Z',
          finishedAt: null,
          logLines: 0,
        },
      }),
    );
    expect(screen.getByText(/集群有活动操作/)).toBeInTheDocument();
    expect(screen.getByTestId('danger-destroy-open')).toBeDisabled();
    expect(screen.getByTestId('danger-archive-open')).toBeDisabled();
    expect(screen.getByTestId('danger-delete-node-open')).toBeDisabled();
  });

  it('失败时展示服务端错误信息（archive 409）', async () => {
    const user = userEvent.setup();
    renderZone(detail({ name: 'tier2-edge' }));
    await user.click(screen.getByTestId('danger-archive-open'));
    await user.click(screen.getByTestId('danger-archive-confirm'));
    const error = await screen.findByTestId('danger-error', {}, { timeout: 8000 });
    expect(error).toHaveTextContent(/活动操作/);
  });
});
