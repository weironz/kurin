import { expect, test } from '@playwright/test';
import { startLocaleMockServer, type LocaleMockServer } from './localeServe';

/**
 * WP-78 验收：语言切换（zh-CN ↔ en）不刷新页面，整页文案即时更换并持久化。
 *
 * 1. 默认 zh-CN：集群页中文标题 + 侧边栏中文菜单；
 * 2. 切到 English：标题 / 菜单 / 登出按钮全部变英文，localStorage 记录 kurin.locale=en；
 * 3. reload 后仍是 English（持久化生效）；
 * 4. 切回中文：文案恢复，localStorage 恢复 zh-CN。
 */
test('语言切换：集群页 zh → en（reload 持久化）→ zh', async ({ browser }) => {
  const server: LocaleMockServer = await startLocaleMockServer();
  const context = await browser.newContext({ baseURL: server.url });
  const page = await context.newPage();

  try {
    await page.goto('/');
    await expect(page.getByRole('heading', { name: '集群' })).toBeVisible({ timeout: 20_000 });
    await expect(page.getByRole('menuitem', { name: '操作' })).toBeVisible();
    await expect(page.getByRole('button', { name: '退出登录' })).toBeVisible();

    // 切到 English：无刷新，标题与菜单立即变英文（antd Select 交互沿用 wizard.spec.ts 的定位约定）
    await page.getByTestId('locale-switch').locator('.ant-select-selector').click();
    await page.locator('.ant-select-item-option').filter({ hasText: 'English' }).click();
    await expect(page.getByRole('heading', { name: 'Clusters' })).toBeVisible();
    await expect(page.getByRole('menuitem', { name: 'Operations' })).toBeVisible();
    await expect(page.getByRole('menuitem', { name: 'Packages' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Log out' })).toBeVisible();
    expect(await page.evaluate(() => window.localStorage.getItem('kurin.locale'))).toBe('en');

    // 持久化：刷新后仍是英文
    await page.reload();
    await expect(page.getByRole('heading', { name: 'Clusters' })).toBeVisible({ timeout: 20_000 });
    await expect(page.getByRole('menuitem', { name: 'About' })).toBeVisible();
    expect(await page.evaluate(() => window.localStorage.getItem('kurin.locale'))).toBe('en');

    // 切回中文
    await page.getByTestId('locale-switch').locator('.ant-select-selector').click();
    await page.locator('.ant-select-item-option').filter({ hasText: '中文' }).click();
    await expect(page.getByRole('heading', { name: '集群' })).toBeVisible();
    await expect(page.getByRole('button', { name: '退出登录' })).toBeVisible();
    expect(await page.evaluate(() => window.localStorage.getItem('kurin.locale'))).toBe('zh-CN');
  } finally {
    server.close();
    await context.close();
  }
});
