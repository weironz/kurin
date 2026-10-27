import { beforeEach, describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter, Route, Routes, useLocation } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { App as AntdApp } from 'antd';
import { http, HttpResponse } from 'msw';
import { server } from '../mocks/server';
import { loginWithToken } from '../api/client';
import PackagesPage from './PackagesPage';

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

function renderPackages(): void {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AntdApp>
        <MemoryRouter initialEntries={['/packages']}>
          <LocationProbe />
          <Routes>
            <Route path="/packages" element={<PackagesPage />} />
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

/** 等待包表格首行出现。 */
async function expectTableLoaded(): Promise<void> {
  await waitFor(() =>
    expect(screen.getByTestId('packages-verify-pkg-base-1.33.1')).toBeInTheDocument(),
  );
}

beforeEach(async () => {
  await loginWithToken('valid-token');
});

describe('包管理页 `/packages`（WP-65）', () => {
  it('表格渲染包行：名称 / 类型 / 版本 / 架构 / 大小 / 来源 / SBOM', async () => {
    renderPackages();
    await expectTableLoaded();
    const table = screen.getByTestId('packages-table');
    expect(within(table).getByText('1.33.1')).toBeInTheDocument();
    expect(within(table).getByText('kubeadm')).toBeInTheDocument();
    expect(within(table).getByText('amd64')).toBeInTheDocument();
    // 1_234_567_890 → 1.1 GB
    expect(within(table).getByText('1.1 GB')).toBeInTheDocument();
    expect(within(table).getByText('导入')).toBeInTheDocument();
    expect(within(table).getAllByText('无').length).toBeGreaterThan(0);
  });

  it('导入对话框：source → POST /packages → 新行出现；非法 source 服务端 400 就地展示', async () => {
    renderPackages();
    await expectTableLoaded();

    // 非法 source：服务端 400 → Alert 就地展示
    fireEvent.click(screen.getByTestId('packages-import'));
    const modal = await screen.findByTestId('packages-import-modal');
    fireEvent.change(within(modal).getByTestId('packages-import-source'), {
      target: { value: 'not-a-uri' },
    });
    fireEvent.click(screen.getByTestId('packages-import-confirm'));
    expect(await screen.findByTestId('packages-import-error')).toHaveTextContent(
      /包来源不合法/,
    );

    // 合法 source：登记成功 → 表格出现新行（id 由 source 派生）
    fireEvent.change(within(modal).getByTestId('packages-import-source'), {
      target: { value: 'file:///media/usb/kurin-base-1.34.0.tar.zst' },
    });
    fireEvent.click(screen.getByTestId('packages-import-confirm'));
    await waitFor(() =>
      expect(
        screen.getByTestId('packages-verify-pkg-urin-base-1.34.0.tar.zst'),
      ).toBeInTheDocument(),
    );
  });

  it('删除：确认框点名包与来源，DELETE 后行消失', async () => {
    renderPackages();
    await expectTableLoaded();

    fireEvent.click(screen.getByTestId('packages-delete-pkg-base-1.33.1'));
    const modal = await screen.findByTestId('packages-delete-modal');
    expect(
      within(modal).getByText(/file:\/\/\/media\/usb\/kurin-base-1.33.1.tar.zst/),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByTestId('packages-delete-confirm'));

    await waitFor(() => expect(screen.getByText('还没有登记的包')).toBeInTheDocument());
    expect(screen.queryByTestId('packages-verify-pkg-base-1.33.1')).not.toBeInTheDocument();
  });

  it('校验：POST verify → 202 跳转操作详情', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/packages/:id/verify', async ({ params }) => {
        bodies.push({ id: params['id'] });
        return HttpResponse.json(
          { operation: '01JDYPKGVERIFY01', state: 'queued', cluster: 'packages' },
          { status: 202 },
        );
      }),
    );
    renderPackages();
    await expectTableLoaded();

    fireEvent.click(screen.getByTestId('packages-verify-pkg-base-1.33.1'));
    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toEqual({ id: 'pkg-base-1.33.1' });
    await expectNavigatedToOperation();
  });

  it('推送：destination 非 oci:// 被表单拦截；合法提交 body 后跳转', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/packages/:id/push', async ({ request, params }) => {
        bodies.push({ id: params['id'], ...((await request.json()) as Record<string, unknown>) });
        return HttpResponse.json(
          { operation: '01JDYPKGPUSH001', state: 'queued', cluster: 'packages' },
          { status: 202 },
        );
      }),
    );
    renderPackages();
    await expectTableLoaded();

    fireEvent.click(screen.getByTestId('packages-push-pkg-base-1.33.1'));
    const modal = await screen.findByTestId('packages-push-modal');
    // 非 oci:// 目标：表单校验拦截（错误文案与 extra 提示区分），不提交
    fireEvent.change(within(modal).getByTestId('packages-push-destination'), {
      target: { value: 'file:///tmp/x' },
    });
    fireEvent.click(screen.getByTestId('packages-push-confirm'));
    await waitFor(() =>
      expect(within(modal).getAllByText(/推送目标必须以 oci/)).toHaveLength(1),
    );
    expect(bodies).toHaveLength(0);

    // 合法目标 + 凭据引用
    fireEvent.change(within(modal).getByTestId('packages-push-destination'), {
      target: { value: 'oci://registry.local:5000/kurin/base' },
    });
    fireEvent.change(within(modal).getByTestId('packages-push-user'), {
      target: { value: 'robot' },
    });
    fireEvent.change(within(modal).getByTestId('packages-push-password-env'), {
      target: { value: 'REGISTRY_PASSWORD' },
    });
    fireEvent.click(screen.getByTestId('packages-push-confirm'));

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toEqual({
      id: 'pkg-base-1.33.1',
      destination: 'oci://registry.local:5000/kurin/base',
      user: 'robot',
      password_env: 'REGISTRY_PASSWORD',
    });
    await expectNavigatedToOperation();
  });

  it('制作 base 包：output 必填；提交 body 原样（缺省字段不送）后跳转', async () => {
    const bodies: Array<Record<string, unknown>> = [];
    server.use(
      http.post('/api/v1/packages/create-base', async ({ request }) => {
        bodies.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json(
          { operation: '01JDYPKGBASE0001', state: 'queued', cluster: 'packages' },
          { status: 202 },
        );
      }),
    );
    renderPackages();
    await expectTableLoaded();

    fireEvent.click(screen.getByTestId('packages-create-base'));
    const modal = await screen.findByTestId('packages-create-base-modal');
    // 空 output：表单必填拦截
    fireEvent.click(screen.getByTestId('packages-create-base-confirm'));
    await waitFor(() => expect(bodies).toHaveLength(0));

    fireEvent.change(within(modal).getByTestId('packages-create-base-kubernetes'), {
      target: { value: '1.34.0' },
    });
    fireEvent.change(within(modal).getByTestId('packages-create-base-arch'), {
      target: { value: 'arm64' },
    });
    fireEvent.change(within(modal).getByTestId('packages-create-base-output'), {
      target: { value: 'oci-layout:///opt/kurin/base' },
    });
    fireEvent.click(screen.getByTestId('packages-create-base-confirm'));

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toEqual({
      kubernetes: '1.34.0',
      distribution: 'kubeadm',
      arch: 'arm64',
      output: 'oci-layout:///opt/kurin/base',
    });
    await expectNavigatedToOperation();
  });

  it('列表加载失败显示错误与重试', async () => {
    server.use(
      http.get('/api/v1/packages', () =>
        HttpResponse.json({ code: 'unavailable', message: '服务正在关闭' }, { status: 503 }),
      ),
    );
    renderPackages();
    expect(await screen.findByText(/操作失败：服务正在关闭/)).toBeInTheDocument();
  });
});
