/**
 * WP-65 真实 UI 验收：在已注册的 tier2 集群上通过详情页升级对话框
 * 执行一次小版本升级。需要 `KURIN_E2E_URL`/`KURIN_E2E_TOKEN`、
 * `KURIN_E2E_UPGRADE_CLUSTER` 与 `KURIN_E2E_UPGRADE_TARGET`
 * （默认 1.34.1）。
 */
import { expect, test } from '@playwright/test';
import { login, operationIdFromUrl, readEnv, waitForOperation } from './helpers';

test('升级对话框：真实执行 tier2 小版本升级', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  const cluster = process.env.KURIN_E2E_UPGRADE_CLUSTER;
  test.skip(!cluster, 'KURIN_E2E_UPGRADE_CLUSTER is not set');
  const target = process.env.KURIN_E2E_UPGRADE_TARGET ?? '1.34.1';
  test.setTimeout(1_800_000);

  const context = await browser.newContext({ baseURL: env!.url });
  const page = await context.newPage();
  try {
    await login(page, env!);
    await page.goto(`/clusters/${encodeURIComponent(cluster!)}`);
    await expect(page.getByTestId('cluster-lifecycle')).toHaveText('运行中', {
      timeout: 60_000,
    });

    await page.getByTestId('cluster-upgrade-open').click();
    const modal = page.getByTestId('cluster-upgrade-modal');
    await expect(modal).toBeVisible();
    await page.getByTestId('cluster-upgrade-target').fill(target);
    await page.getByTestId('cluster-upgrade-execute').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 60_000 });
    const op = await waitForOperation(page, operationIdFromUrl(page), 1_500_000);
    expect(op.state, op.error ?? '').toBe('succeeded');

    await page.goto(`/clusters/${encodeURIComponent(cluster!)}`);
    await expect(page.getByText(target, { exact: false }).first()).toBeVisible({
      timeout: 60_000,
    });
  } finally {
    await context.close();
  }
});
