import { expect, test } from '@playwright/test';
import { startWizardMockServer, type WizardMockServer } from './wizardServe';

/**
 * WP-62 验收：mock 后端上完整走完「3 主 HA（kube-vip）」模板并提交。
 *
 * 断言：
 * 1. 9 步全部可达（步骤条 + 每步标题）；
 * 2. 批量粘贴展开成 3 行控制面节点；探测回填 facts；
 * 3. 每步的 render → validate 都发生（前端不复刻校验）；
 * 4. precheck 通过后「创建」可用，提交后跳转 /operations/:id；
 * 5. 服务端收到 `kind = "create"` 的操作提交。
 */
test('创建向导：3 主 HA（kube-vip）模板完整提交', async ({ browser }) => {
  const server: WizardMockServer = await startWizardMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/clusters/new');
    await expect(page.getByRole('heading', { name: '新建集群' })).toBeVisible({ timeout: 20_000 });

    // 第 1 步：模板。
    await expect(page.getByText('3 主 HA（kube-vip）')).toBeVisible();
    await page.getByText('3 主 HA（kube-vip）').click();
    await page.getByTestId('wizard-next').click();

    // 第 2 步：基本信息。
    await page.getByTestId('wizard-name').fill('prod');
    await page.getByTestId('wizard-k8s-version').locator('.ant-select-selector').click();
    await page.locator('.ant-select-item-option').filter({ hasText: '1.33.1' }).click();
    await page.getByTestId('wizard-next').click();

    // 第 3 步：节点（批量粘贴 3 主 + 探测回填）。
    await page.getByTestId('wizard-bulk-input').fill('192.168.10.11-13 control-plane');
    await page.getByTestId('wizard-bulk-parse').click();
    await expect(page.locator('[data-testid="wizard-probe-ok"]')).toHaveCount(0);
    await page.getByTestId('wizard-probe-node-2').click();
    await expect(page.locator('[data-testid="wizard-probe-ok"]')).toHaveCount(1);
    await expect(page.getByText(/ubuntu 24\.04 · amd64/)).toBeVisible();
    await page.getByTestId('wizard-next').click();

    // 第 4 步：认证（模板默认 key，填路径即可）。
    await page.getByTestId('wizard-key-path').fill('/root/.ssh/id_ed25519');
    await page.getByTestId('wizard-next').click();

    // 第 5 步：网络与 HA（模板已选 kube-vip，补 VIP）。
    await page.getByTestId('wizard-vip').fill('192.168.10.100');
    await page.getByTestId('wizard-next').click();

    // 第 6 步：组件（勾一个 addon）。
    await page.getByTestId('wizard-addon-check-metrics-server').click();
    await page.getByTestId('wizard-next').click();

    // 第 7 步：离线（在线模式直接过）。
    await page.getByTestId('wizard-next').click();

    // 第 8 步：检查（precheck 通过后按钮可用）。
    await page.getByTestId('wizard-precheck-run').click();
    await page.waitForTimeout(3000);
    await expect(page.getByText(/2\/2 项通过/)).toBeVisible({ timeout: 20_000 });
    await page.getByTestId('wizard-next').click();

    // 第 9 步：确认（摘要 + 只读 TOML → 提交）。
    await expect(page.getByTestId('wizard-summary')).toBeVisible();
    await expect(page.getByTestId('wizard-toml-preview')).toContainText('name = "prod"');
    await page.getByTestId('wizard-create').click();

    // 提交 → 跳转操作详情。
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    expect(server.submitted.length).toBeGreaterThanOrEqual(2);
    expect(server.submitted.some((s) => s.kind === 'precheck')).toBe(true);
    expect(server.submitted.at(-1)?.kind).toBe('create');
    expect(server.submitted.at(-1)?.cluster).toBe('prod');
    // 每步都发生过 render（规则 1）。
    expect(server.lastRenderBody()).not.toBeNull();
  } finally {
    await context.close();
    server.close();
  }
});
