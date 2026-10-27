/**
 * WP-66 M2 主链路：创建 → 状态 → 加 worker → 删 worker → 销毁。
 *
 * 需要真实 `kurin serve` 与可 SSH 的节点；环境变量见 README。
 * 未设置 `KURIN_E2E_URL` 时 skip。
 */
import { expect, test } from '@playwright/test';
import {
  login,
  operationIdFromUrl,
  readEnv,
  startCreate,
  waitForOperation,
} from './helpers';

test('创建 → 状态 → 加 worker → 删 worker → 销毁', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  test.setTimeout(1_800_000);
  const context = await browser.newContext({ baseURL: env.url });
  const page = await context.newPage();
  try {
    await login(page, env);

    // ── 创建：9 步向导 ──────────────────────────────────────────────
    const createId = await startCreate(page, env);
    const created = await waitForOperation(page, createId, 600_000);
    expect(created.state, created.error ?? '').toBe('succeeded');

    // ── 状态：集群详情为 active ────────────────────────────────────
    await page.goto(`/clusters/${encodeURIComponent(env.cluster)}`);
    await expect(page.getByTestId('cluster-lifecycle')).toHaveText('运行中', { timeout: 60_000 });

    // ── 加 worker ──────────────────────────────────────────────────
    if (env.addBulk) {
      await page.getByRole('tab', { name: /节点/ }).click();
      await page.getByTestId('cluster-add-nodes-open').click();
      await page.getByTestId('add-nodes-input').fill(env.addBulk);
      await page.getByTestId('add-nodes-parse').click();
      await expect(page.getByTestId('add-nodes-table')).toBeVisible();
      await page.getByTestId('add-nodes-submit').click();
      await expect(page).toHaveURL(/\/operations\//, { timeout: 60_000 });
      const addId = operationIdFromUrl(page);
      const added = await waitForOperation(page, addId, 600_000);
      expect(added.state, added.error ?? '').toBe('succeeded');
    }

    // ── 删 worker ──────────────────────────────────────────────────
    if (env.deleteNode) {
      await page.goto(`/clusters/${encodeURIComponent(env.cluster)}`);
      await page.getByRole('tab', { name: /危险区/ }).click();
      await page.getByTestId('danger-delete-node-select').click();
      await page.getByTitle(env.deleteNode).click();
      await page.getByTestId('danger-delete-node-open').click();
      await page.getByTestId('danger-delete-node-confirm').click();
      await expect(page).toHaveURL(/\/operations\//, { timeout: 60_000 });
      const deleteId = operationIdFromUrl(page);
      const deleted = await waitForOperation(page, deleteId, 600_000);
      expect(deleted.state, deleted.error ?? '').toBe('succeeded');
    }

    // ── 销毁：输入集群名确认 ────────────────────────────────────────
    await page.goto(`/clusters/${encodeURIComponent(env.cluster)}`);
    await page.getByRole('tab', { name: /危险区/ }).click();
    await page.getByTestId('danger-destroy-open').click();
    await page.getByTestId('danger-destroy-input').fill(env.cluster);
    await page.getByTestId('danger-destroy-confirm').click();
    await expect(page).toHaveURL(/\/operations\//, { timeout: 60_000 });
    const destroyId = operationIdFromUrl(page);
    const destroyed = await waitForOperation(page, destroyId, 900_000);
    expect(destroyed.state, destroyed.error ?? '').toBe('succeeded');
  } finally {
    await context.close();
  }
});
