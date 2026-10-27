import { expect, test } from '@playwright/test';
import { startPackagesMockServer, type PackagesMockServer } from './packagesServe';

/**
 * WP-65 验收：包管理页 + 升级对话框 + 证书 Tab + TOML 编辑器，全部跑在 mock 后端上。
 *
 * 1. 导入包：source → POST /packages → 表格出现新行；
 * 2. 校验 / 推送 / 制作 base：202 → 跳转 /operations/:id；
 * 3. 升级：预览计划（dry_run: true）与执行（dry_run: false）的请求体形状；
 * 4. 证书 Tab：certMinDays 展示 + certs-renew（force）提交；
 * 5. TOML 编辑器：CodeMirror 懒加载、编辑、校验通过后 PUT（base_rev）保存成功。
 */
test.describe.configure({ mode: 'serial' });

test('包管理页：导入、校验、推送与制作 base', async ({ browser }) => {
  const server: PackagesMockServer = await startPackagesMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/packages');
    await expect(page.getByTestId('packages-verify-pkg-base-1.33.1')).toBeVisible({
      timeout: 20_000,
    });
    // 表格行：版本 / 架构 / 大小（1_234_567_890 → 1.1 GB）
    await expect(page.getByTestId('packages-table').getByText('1.33.1')).toBeVisible();
    await expect(page.getByTestId('packages-table').getByText('1.1 GB')).toBeVisible();

    // 导入：source → POST /packages（201）→ 新行
    await page.getByTestId('packages-import').click();
    const importModal = page.getByTestId('packages-import-modal');
    await importModal.getByTestId('packages-import-source').fill('file:///media/usb/base-1.34.0.tar.zst');
    await page.getByTestId('packages-import-confirm').click();
    await expect(page.getByTestId('packages-verify-pkg-imported-e2e')).toBeVisible({
      timeout: 10_000,
    });

    // 校验：202 → 跳转操作详情
    await page.getByTestId('packages-verify-pkg-base-1.33.1').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const verify = server.submissions.find((s) => s.path.endsWith('/verify'));
    expect(verify).toBeDefined();

    // 推送：destination / user / password_env → 202 → 跳转
    await page.goto('/packages');
    await expect(page.getByTestId('packages-verify-pkg-base-1.33.1')).toBeVisible({
      timeout: 20_000,
    });
    await page.getByTestId('packages-push-pkg-base-1.33.1').click();
    const pushModal = page.getByTestId('packages-push-modal');
    await pushModal.getByTestId('packages-push-destination').fill('oci://registry.local:5000/kurin/base');
    await pushModal.getByTestId('packages-push-user').fill('robot');
    await pushModal.getByTestId('packages-push-password-env').fill('REGISTRY_PASSWORD');
    await page.getByTestId('packages-push-confirm').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const push = server.submissions.find((s) => s.path.endsWith('/push'));
    expect(push?.body).toMatchObject({
      destination: 'oci://registry.local:5000/kurin/base',
      user: 'robot',
      password_env: 'REGISTRY_PASSWORD',
    });

    // 制作 base 包：kubernetes / arch / output → 202 → 跳转
    await page.goto('/packages');
    await expect(page.getByTestId('packages-verify-pkg-base-1.33.1')).toBeVisible({
      timeout: 20_000,
    });
    await page.getByTestId('packages-create-base').click();
    const baseModal = page.getByTestId('packages-create-base-modal');
    await baseModal.getByTestId('packages-create-base-kubernetes').fill('1.34.0');
    await baseModal.getByTestId('packages-create-base-arch').fill('arm64');
    await baseModal.getByTestId('packages-create-base-output').fill('oci-layout:///opt/kurin/base');
    await page.getByTestId('packages-create-base-confirm').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const createBase = server.submissions.find((s) => s.path.endsWith('/create-base'));
    expect(createBase?.body).toMatchObject({
      kubernetes: '1.34.0',
      distribution: 'kubeadm',
      arch: 'arm64',
      output: 'oci-layout:///opt/kurin/base',
    });
  } finally {
    await context.close();
    server.close();
  }
});

test('集群详情：升级预览与执行、证书续期', async ({ browser }) => {
  const server: PackagesMockServer = await startPackagesMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/clusters/tier1');
    await expect(page.getByText('cp-1')).toBeVisible({ timeout: 20_000 });

    // 升级对话框：预览计划 → dry_run: true
    await page.getByTestId('cluster-upgrade-open').click();
    const modal = page.getByTestId('cluster-upgrade-modal');
    await expect(modal.getByTestId('cluster-upgrade-current')).toHaveText('1.33.1');
    await modal.getByTestId('cluster-upgrade-target').fill('1.34.0');
    await modal.getByTestId('cluster-upgrade-batch').fill('2');
    await modal.getByTestId('cluster-upgrade-force-drain').check();
    await page.getByTestId('cluster-upgrade-preview').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const preview = server.submissions.find(
      (s) => s.path.endsWith('/operations') && s.body.kind === 'upgrade',
    );
    expect(preview?.body).toEqual({
      kind: 'upgrade',
      params: {
        target: '1.34.0',
        batch: 2,
        dry_run: true,
        force_drain_failure: true,
      },
    });

    // 执行升级：dry_run: false
    await page.goto('/clusters/tier1');
    await expect(page.getByText('cp-1')).toBeVisible({ timeout: 20_000 });
    await page.getByTestId('cluster-upgrade-open').click();
    const modal2 = page.getByTestId('cluster-upgrade-modal');
    await modal2.getByTestId('cluster-upgrade-target').fill('1.34.0');
    await page.getByTestId('cluster-upgrade-execute').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const execute = server.submissions.filter(
      (s) => s.path.endsWith('/operations') && s.body.kind === 'upgrade',
    );
    expect(execute).toHaveLength(2);
    expect(execute[1].body).toEqual({
      kind: 'upgrade',
      params: { target: '1.34.0', batch: 1, dry_run: false, force_drain_failure: false },
    });

    // 证书 Tab：certMinDays 展示 + 续期（force）
    await page.goto('/clusters/tier1');
    await expect(page.getByText('cp-1')).toBeVisible({ timeout: 20_000 });
    await page.getByRole('tab', { name: '证书' }).click();
    await expect(page.getByTestId('certs-min-days')).toHaveText('210 天');
    await page.getByTestId('certs-force').check();
    await page.getByTestId('certs-renew').click();
    await expect(page).toHaveURL(/\/operations\/.+/, { timeout: 20_000 });
    const renew = server.submissions.find((s) => s.body.kind === 'certs-renew');
    expect(renew?.body).toEqual({ kind: 'certs-renew', params: { force: true } });
  } finally {
    await context.close();
    server.close();
  }
});

test('配置 Tab：高级编辑（CodeMirror）加载、校验与保存', async ({ browser }) => {
  const server: PackagesMockServer = await startPackagesMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/clusters/tier1');
    await expect(page.getByText('cp-1')).toBeVisible({ timeout: 20_000 });

    // 默认是只读表单视图；「高级编辑」显式进入
    await page.getByRole('tab', { name: '配置' }).click();
    await expect(page.getByTestId('cluster-config-advanced')).toBeVisible();
    await page.getByTestId('cluster-config-advanced').click();

    // CodeMirror 懒加载 chunk 挂载：正文出现 + 基准修订
    const editor = page.getByTestId('toml-editor');
    await expect(editor.locator('.cm-content')).toBeVisible({ timeout: 20_000 });
    await expect(editor.getByText('v1alpha2')).toBeVisible();
    await expect(page.getByTestId('toml-rev')).toHaveText('基准修订 #7');

    // 防抖校验（500ms）：mock 对含 [nodes. 的文档放行
    await expect(page.getByText('校验通过')).toBeVisible({ timeout: 10_000 });

    // 保存：PUT 带 base_rev=7 → 新修订 #8
    await page.getByTestId('toml-save').click();
    await expect(page.getByTestId('toml-rev')).toHaveText('基准修订 #8', { timeout: 10_000 });
    const put = server.submissions.find((s) => s.method === 'PUT');
    expect(put?.body).toMatchObject({ base_rev: 7 });
    expect(String(put?.body.toml ?? '')).toContain('[nodes.cp-1]');
  } finally {
    await context.close();
    server.close();
  }
});
