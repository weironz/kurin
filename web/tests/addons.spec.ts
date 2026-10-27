import { expect, test } from '@playwright/test';
import { startAddonsMockServer, type AddonsMockServer } from './addonsServe';

/**
 * WP-64 验收：addon 管理页 + 危险区，全部跑在 mock 后端上。
 *
 * 1. 安装 metrics-server：rjsf values 表单 → key=value 提交 → 跳转操作详情；
 * 2. 卸载：确认框点名集群，DELETE 带 confirm == 集群名；
 * 3. 删除节点：选择节点 → 确认 → delete-node + confirm；
 * 4. 销毁集群：输入集群名才可确认 → delete-cluster + confirm；
 * 5. 移除纳管：文案与销毁区分，POST archive 后回到集群列表。
 */
test.describe.configure({ mode: 'serial' });

test('Addon 管理页：安装、卸载与目录兼容性', async ({ browser }) => {
  const server: AddonsMockServer = await startAddonsMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/clusters/tier1/addons');
    await expect(page.getByRole('heading', { name: 'Addon' })).toBeVisible({
      timeout: 20_000,
    });

    // 空清单：服务端 note；目录分组卡片可见。
    await expect(page.getByText(/集群还没有 addon 记录/)).toBeVisible();
    await expect(page.getByTestId('addons-catalog-metrics-server')).toBeVisible();
    await expect(page.getByTestId('addons-catalog-ingress-nginx')).toBeVisible();

    // 安装 metrics-server：对话框 + rjsf 表单（replicas = 2）。
    await page.getByTestId('addons-install-metrics-server').click();
    const modal = page.getByTestId('addons-install-modal');
    await expect(modal.getByTestId('wizard-addon-values')).toBeVisible();
    const replicas = modal.getByLabel(/replicas/i);
    await replicas.fill('2');
    await page.getByTestId('addons-install-confirm').click();

    // 202 → 跳转 /operations/:id。
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const install = server.submissions.find((s) => s.method === 'POST');
    expect(installs(install)).toMatchObject({
      addon: 'metrics-server',
      version: '0.9.0',
      values: ['replicas=2'],
    });

    // 回到页面：已安装表格 + 已安装徽标；卸载（confirm == 集群名）。
    await page.goto('/clusters/tier1/addons');
    await expect(page.getByTestId('addons-status-metrics-server')).toBeVisible({
      timeout: 20_000,
    });
    await page.getByTestId('addons-uninstall-metrics-server').click();
    const uninstallModal = page.getByTestId('addons-uninstall-modal');
    await expect(uninstallModal).toContainText('tier1');
    await page.getByTestId('addons-uninstall-confirm').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const remove = server.submissions.find((s) => s.method === 'DELETE');
    expect(installs(remove)).toMatchObject({ addon: 'metrics-server', confirm: 'tier1' });
  } finally {
    await context.close();
    server.close();
  }
});

test('危险区：删除节点、销毁集群与移除纳管', async ({ browser }) => {
  const server: AddonsMockServer = await startAddonsMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/clusters/tier1');
    await expect(page.getByRole('heading', { name: '集群详情' })).toBeVisible({
      timeout: 20_000,
    });
    // 头部入口存在（链接到 addon 管理页）。
    await expect(page.getByRole('link', { name: 'Addon' })).toHaveAttribute(
      'href',
      '/clusters/tier1/addons',
    );
    await page.getByRole('tab', { name: '危险区' }).click();

    // 删除节点：选择 worker-1 → 确认 → 提交。
    await page.getByTestId('danger-delete-node-select').click();
    await page.getByTitle('worker-1').click();
    await page.getByTestId('danger-delete-node-open').click();
    await expect(page.getByTestId('danger-delete-node-modal')).toContainText('worker-1');
    await page.getByTestId('danger-delete-node-confirm').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const deleteNode = server.submissions.at(-1);
    expect(deleteNode?.body).toMatchObject({
      kind: 'delete-node',
      params: { node: 'worker-1' },
      confirm: 'tier1',
    });

    // 销毁集群：输入错误名不可确认，输入集群名后提交。
    await page.goto('/clusters/tier1');
    await page.getByRole('tab', { name: '危险区' }).click();
    await page.getByTestId('danger-destroy-open').click();
    const destroyModal = page.getByTestId('danger-destroy-modal');
    await expect(destroyModal).toContainText('不可恢复');
    const confirmButton = page.getByTestId('danger-destroy-confirm');
    await expect(confirmButton).toBeDisabled();
    await page.getByTestId('danger-destroy-input').fill('tier');
    await expect(confirmButton).toBeDisabled();
    await page.getByTestId('danger-destroy-input').fill('tier1');
    await expect(confirmButton).toBeEnabled();
    await confirmButton.click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    expect(server.submissions.at(-1)?.body).toMatchObject({
      kind: 'delete-cluster',
      confirm: 'tier1',
    });

    // 移除纳管：文案强调远端集群不受影响，POST archive 后回到集群列表。
    await page.goto('/clusters/tier1');
    await page.getByRole('tab', { name: '危险区' }).click();
    await page.getByTestId('danger-archive-open').click();
    const archiveModal = page.getByTestId('danger-archive-modal');
    await expect(archiveModal).toContainText('不受影响');
    await expect(archiveModal).not.toContainText('不可恢复');
    await page.getByTestId('danger-archive-confirm').click();
    await expect(page).toHaveURL(/\/$/, { timeout: 20_000 });
    expect(server.archived).toEqual(['tier1']);
  } finally {
    await context.close();
    server.close();
  }
});

/** 非空提交（断言用）；空时返回空对象让 toMatchObject 报清晰错误。 */
function installs(sub: { body: Record<string, unknown> } | undefined): Record<string, unknown> {
  return sub?.body ?? {};
}
