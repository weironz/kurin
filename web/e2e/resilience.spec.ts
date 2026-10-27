/**
 * WP-66 韧性场景：关闭浏览器、服务重启、CLI 抢锁、密码明文扫描。
 *
 * 这些用例需要真实集群与可控的 `kurin serve` 进程；相关能力未提供时
 * 单个用例 skip（见 README 的环境变量）。
 */
import { execFileSync } from 'node:child_process';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test } from '@playwright/test';
import {
  login,
  operationIdFromUrl,
  readEnv,
  startCreate,
  waitForOperation,
} from './helpers';

/** 递归收集目录下的普通文件（跳过符号链接与过大的二进制）。 */
function walk(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(path, out);
    } else if (entry.isFile()) {
      out.push(path);
    }
  }
  return out;
}

test('创建中关闭浏览器：操作在服务端继续并在重开后可见', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  test.setTimeout(900_000);

  const first = await browser.newContext({ baseURL: env.url });
  const page = await first.newPage();
  await login(page, env);
  const id = await startCreate(page, env);
  await first.close();

  const second = await browser.newContext({ baseURL: env.url });
  const reopened = await second.newPage();
  try {
    await login(reopened, env);
    await reopened.goto(`/operations/${encodeURIComponent(id)}`);
    await expect(reopened.getByTestId('operation-detail')).toBeVisible({ timeout: 30_000 });
    const operation = await waitForOperation(reopened, id, 600_000);
    expect(operation.state, operation.error ?? '').toBe('succeeded');
  } finally {
    await second.close();
  }
});

test('服务重启：活动操作 interrupted → 重新执行', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  test.skip(!env.restartCmd, 'KURIN_E2E_RESTART_CMD is not set');
  test.setTimeout(900_000);

  const context = await browser.newContext({ baseURL: env.url });
  const page = await context.newPage();
  try {
    await login(page, env);
    const id = await startCreate(page, env);

    execFileSync('sh', ['-c', env.restartCmd], { stdio: 'inherit' });

    // 重启后会话可能失效：重新用令牌登录。
    await page.waitForTimeout(2000);
    await login(page, env);

    const interrupted = await waitForOperation(page, id, 120_000);
    expect(interrupted.state).toBe('interrupted');

    await page.goto(`/operations/${encodeURIComponent(id)}`);
    await page.getByRole('button', { name: '重新执行' }).click();
    await page.getByRole('button', { name: '创建并执行' }).click();
    await expect(page).toHaveURL(/\/operations\//, { timeout: 60_000 });
    const retryId = operationIdFromUrl(page);
    expect(retryId).not.toBe(id);
    const retried = await waitForOperation(page, retryId, 600_000);
    expect(retried.retry_of).toBe(id);
  } finally {
    await context.close();
  }
});

test('CLI 抢锁：Web 操作进行中时 CLI 被拒绝', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  test.skip(!env.cli || !env.config, 'KURIN_E2E_CLI / KURIN_E2E_CONFIG are not set');
  test.setTimeout(900_000);

  const context = await browser.newContext({ baseURL: env.url });
  const page = await context.newPage();
  try {
    await login(page, env);
    await startCreate(page, env);

    let output = '';
    let failed = false;
    try {
      output = execFileSync(
        env.cli,
        ['upgrade', '--file', env.config, '--with-kubernetes', 'v1.99.0'],
        {
          encoding: 'utf8',
          env: {
            ...process.env,
            ...(env.xdgStateHome ? { XDG_STATE_HOME: env.xdgStateHome } : {}),
          },
        },
      );
    } catch (error) {
      failed = true;
      const err = error as { stdout?: string; stderr?: string; message?: string };
      output = `${err.stdout ?? ''}\n${err.stderr ?? ''}\n${err.message ?? ''}`;
    }
    expect(failed, `CLI unexpectedly succeeded:\n${output}`).toBe(true);
    expect(output).toMatch(/is busy|held by/);
  } finally {
    await context.close();
  }
});

test('密码明文不出现在数据库、事件与 API 响应中', async ({ browser }) => {
  const env = readEnv();
  test.skip(env == null, 'KURIN_E2E_URL is not set');
  test.skip(!env.dataDir || !env.secret, 'KURIN_E2E_DATA_DIR / KURIN_E2E_SECRET are not set');
  test.setTimeout(900_000);

  const context = await browser.newContext({ baseURL: env.url });
  const page = await context.newPage();
  try {
    await login(page, env);
    const id = await startCreate(page, env);
    const operation = await waitForOperation(page, id, 600_000);
    expect(operation.state, operation.error ?? '').toBe('succeeded');

    // API：配置修订、操作详情、日志。
    const config = await page.request.get(
      `/api/v1/clusters/${encodeURIComponent(env.cluster)}/config`,
    );
    expect(config.ok()).toBe(true);
    expect(await config.text()).not.toContain(env.secret);

    const detail = await page.request.get(`/api/v1/operations/${encodeURIComponent(id)}`);
    expect(detail.ok()).toBe(true);
    expect(await detail.text()).not.toContain(env.secret);

    const logs = await page.request.get(
      `/api/v1/operations/${encodeURIComponent(id)}/logs?limit=5000`,
    );
    expect(logs.ok()).toBe(true);
    expect(await logs.text()).not.toContain(env.secret);

    // 磁盘：数据库与 JSONL 日志。
    const needle = Buffer.from(env.secret);
    const offenders: string[] = [];
    for (const file of walk(env.dataDir)) {
      if (statSync(file).size > 64 * 1024 * 1024) continue;
      if (readFileSync(file).includes(needle)) offenders.push(file);
    }
    expect(offenders, `secret found in ${offenders.join(', ')}`).toEqual([]);
  } finally {
    await context.close();
  }
});
