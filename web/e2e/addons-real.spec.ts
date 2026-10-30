/**
 * WP-64 真实 UI 验收：在已注册的 tier1 集群上通过 Addon 页面安装/卸载
 * metrics-server。需要 `KURIN_E2E_URL`/`KURIN_E2E_TOKEN` 与
 * `KURIN_E2E_ADDON_CLUSTER`（serve 中已注册的集群名）。
 */
import { expect, test } from '@playwright/test';
import { login, operationIdFromUrl, readEnv, waitForOperation } from './helpers';

test('Addon 页面：真实安装并卸载 metrics-server', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  const cluster = process.env.KURIN_E2E_ADDON_CLUSTER;
  test.skip(!cluster, 'KURIN_E2E_ADDON_CLUSTER is not set');
  test.setTimeout(900_000);

  const context = await browser.newContext({ baseURL: env!.url });
  const page = await context.newPage();
  try {
    await login(page, env!);

    await page.goto(`/clusters/${encodeURIComponent(cluster!)}/addons`);
    await expect(page.getByRole('heading', { name: 'Addon' })).toBeVisible({
      timeout: 30_000,
    });
    await expect(page.getByTestId('addons-catalog-metrics-server')).toBeVisible();

    await page.getByTestId('addons-install-metrics-server').click();
    const modal = page.getByTestId('addons-install-modal');
    await expect(modal.getByTestId('wizard-addon-values')).toBeVisible();
    await page.getByTestId('addons-install-confirm').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 60_000 });
    const install = await waitForOperation(page, operationIdFromUrl(page), 600_000);
    expect(install.state, install.error ?? '').toBe('succeeded');

    // The collector refreshes the installed inventory on its own cycle;
    // reload until the status row appears.
    await expect
      .poll(
        async () => {
          await page.goto(`/clusters/${encodeURIComponent(cluster!)}/addons`);
          return page.getByTestId('addons-status-metrics-server').count();
        },
        { timeout: 180_000, intervals: [2_000] },
      )
      .toBeGreaterThan(0);

    await page.getByTestId('addons-uninstall-metrics-server').click();
    const uninstall = page.getByTestId('addons-uninstall-modal');
    await expect(uninstall).toContainText(cluster!);
    await page.getByTestId('addons-uninstall-confirm').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 60_000 });
    const removed = await waitForOperation(page, operationIdFromUrl(page), 600_000);
    expect(removed.state, removed.error ?? '').toBe('succeeded');
  } finally {
    await context.close();
  }
});
