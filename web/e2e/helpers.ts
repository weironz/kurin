/**
 * WP-66 真实环境 e2e 的共享工具：环境变量、登录、操作轮询、终端状态。
 *
 * 所有用例都以 `KURIN_E2E_URL` 为开关；未设置时 skip，保证 `pnpm test:e2e`
 * 在没有 tier1/tier2 的机器上仍然全绿。
 */
import { expect, type Page } from '@playwright/test';

export interface E2eEnv {
  /** `kurin serve` 的地址，如 `https://10.0.0.5:7080`。 */
  url: string;
  /** 服务启动时打印的一次性令牌（`/#token=…`）。 */
  token: string;
  /** 本次创建的集群名（默认带时间戳，避免重复）。 */
  cluster: string;
  /** 向导第 3 步的批量节点文本（如 `10.0.0.11-13 control-plane`）。 */
  bulk: string;
  /** 向导模板卡片文案（默认「单节点」）。 */
  template: string;
  /** SSH 认证：key（服务端路径）或 password（环境变量名）。 */
  auth: 'key' | 'password';
  keyPath?: string;
  passwordEnv?: string;
  /** 加 worker 的批量文本；未设置时跳过加节点场景。 */
  addBulk?: string;
  /** 删除节点场景的节点名；未设置时跳过。 */
  deleteNode?: string;
  /** 服务数据目录（secret 扫描用）。 */
  dataDir?: string;
  /** `kurin` 二进制路径（CLI 抢锁场景）。 */
  cli?: string;
  /** CLI 使用的配置文件（数据目录下的 cluster.toml 镜像）。 */
  config?: string;
  /** CLI 与 serve 共用的 XDG_STATE_HOME（锁文件目录）。 */
  xdgStateHome?: string;
  /** 重启 `kurin serve` 的命令（interrupted → retry 场景）。 */
  restartCmd?: string;
  /** 明文密码（secret 扫描断言它不出现在 DB/事件/API 中）。 */
  secret?: string;
}

/** 读取环境；缺少 `KURIN_E2E_URL` 时返回 null（用例 skip）。 */
export function readEnv(): E2eEnv | null {
  const url = process.env.KURIN_E2E_URL;
  if (!url) return null;
  const token = process.env.KURIN_E2E_TOKEN;
  if (!token) throw new Error('KURIN_E2E_TOKEN is required when KURIN_E2E_URL is set');
  const auth = process.env.KURIN_E2E_AUTH === 'password' ? 'password' : 'key';
  return {
    url,
    token,
    cluster: process.env.KURIN_E2E_CLUSTER ?? `e2e-${Date.now()}`,
    bulk: process.env.KURIN_E2E_BULK ?? '192.0.2.11 control-plane',
    template: process.env.KURIN_E2E_TEMPLATE ?? '单节点体验',
    auth,
    keyPath: process.env.KURIN_E2E_KEY_PATH,
    passwordEnv: process.env.KURIN_E2E_PASSWORD_ENV,
    addBulk: process.env.KURIN_E2E_ADD_BULK,
    deleteNode: process.env.KURIN_E2E_DELETE_NODE,
    dataDir: process.env.KURIN_E2E_DATA_DIR,
    cli: process.env.KURIN_E2E_CLI,
    config: process.env.KURIN_E2E_CONFIG,
    xdgStateHome: process.env.KURIN_E2E_XDG_STATE_HOME,
    restartCmd: process.env.KURIN_E2E_RESTART_CMD,
    secret: process.env.KURIN_E2E_SECRET,
  };
}

/** 令牌换 cookie 并等待控制台外壳出现。 */
export async function login(page: Page, env: E2eEnv): Promise<void> {
  await page.goto(`/#token=${encodeURIComponent(env.token)}`);
  await expect(page.getByText('kurin 控制台')).toBeVisible({ timeout: 30_000 });
}

/** `GET /operations/{id}` 的轮询视图。 */
export interface OperationView {
  id: string;
  state: 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted';
  kind: string;
  error?: string | null;
  retry_of?: string | null;
}

/** 轮询一个操作直到终态；超时抛错并带上最后一次快照。 */
export async function waitForOperation(
  page: Page,
  id: string,
  timeoutMs = 150_000,
): Promise<OperationView> {
  const deadline = Date.now() + timeoutMs;
  let last: OperationView | null = null;
  while (Date.now() < deadline) {
    try {
      const response = await page.request.get(`/api/v1/operations/${id}`);
      if (response.ok()) {
        last = (await response.json()) as OperationView;
        if (['succeeded', 'failed', 'cancelled', 'interrupted'].includes(last.state)) {
          return last;
        }
      }
    } catch {
      // 服务重启期间连接会拒绝：继续轮询。
    }
    await page.waitForTimeout(1000);
  }
  throw new Error(`operation ${id} did not finish: ${JSON.stringify(last)}`);
}

/** 从 `/operations/{id}` 地址里取操作 id。 */
export function operationIdFromUrl(page: Page): string {
  const match = /\/operations\/([^/?#]+)/.exec(page.url());
  if (!match) throw new Error(`not on an operation page: ${page.url()}`);
  return decodeURIComponent(match[1]);
}

/** 向导第 2 步：填名称与版本（选第一个可用版本）。 */
export async function fillBasic(page: Page, env: E2eEnv): Promise<void> {
  await page.getByTestId('wizard-name').fill(env.cluster);
  await page.getByTestId('wizard-k8s-version').locator('.ant-select-selector').click();
  await page.locator('.ant-select-item-option').first().click();
}

/** 向导第 4 步：按环境选择 key 路径或密码环境变量。 */
export async function fillAuth(page: Page, env: E2eEnv): Promise<void> {
  if (env.auth === 'password') {
    await page
      .getByTestId('wizard-auth-method')
      .getByText('密码（环境变量）')
      .click();
    await page.getByTestId('wizard-password-env').fill(env.passwordEnv ?? 'KURIN_E2E_SSH_PASSWORD');
  } else {
    // 默认就是私钥方式；只填管理机上的路径。
    await page.getByTestId('wizard-key-path').fill(env.keyPath ?? '/root/.ssh/id_ed25519');
  }
}

/**
 * 走完 9 步向导并提交 create，返回操作 id。
 * 前置：已登录；模板/节点/认证来自环境变量。
 */
export async function startCreate(page: Page, env: E2eEnv): Promise<string> {
  await page.goto('/clusters/new');
  await page.getByText(env.template).click();
  await page.getByTestId('wizard-next').click();

  await fillBasic(page, env);
  await page.getByTestId('wizard-next').click();

  await page.getByTestId('wizard-bulk-input').fill(env.bulk);
  await page.getByTestId('wizard-bulk-parse').click();
  await expect(page.getByTestId('wizard-bulk-card')).toBeVisible();
  await page.getByTestId('wizard-next').click();

  await fillAuth(page, env);
  await page.getByTestId('wizard-next').click();

  // 网络与高可用、组件、离线：使用模板默认值。
  await page.getByTestId('wizard-next').click();
  await page.getByTestId('wizard-next').click();
  await page.getByTestId('wizard-next').click();

  // 检查：等去抖 render/validate 产出 TOML 后再跑 precheck。
  await expect(page.getByTestId('wizard-precheck-run')).toBeEnabled({ timeout: 60_000 });
  await page.getByTestId('wizard-precheck-run').click();
  await expect(page.getByText(/项通过|项告警|项失败/)).toBeVisible({ timeout: 300_000 });
  if (await page.getByText(/项失败/).isVisible()) {
    throw new Error('precheck reported failures; the tier is not clean');
  }
  // 有告警时必须显式确认，否则「下一步」保持禁用。
  const ack = page.getByTestId('wizard-ack-warnings');
  if ((await ack.count()) > 0) await ack.check();
  await page.getByTestId('wizard-next').click();

  // 确认：提交 create。
  await page.getByTestId('wizard-create').click();
  await expect(page).toHaveURL(/\/operations\//, { timeout: 60_000 });
  return operationIdFromUrl(page);
}
