/**
 * WP-65 e2e 的 mock 后端：包管理页、升级对话框、证书 Tab 与 TOML 编辑器
 * 需要的全部端点。
 *
 * 与 addonsServe.ts 同理：静态资源从 dist 读取（含 .gz 直出），API 用
 * Node http 服务；提交记录存进数组供测试断言（导入/推送/制作 base、
 * upgrade / certs-renew 的请求体形状，以及 PUT config 的 base_rev）。
 */
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { createServer, type Server, type ServerResponse } from 'node:http';

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2; charset=utf-8',
};

/** 测试断言用的提交记录。 */
export interface PackageSubmission {
  method: string;
  path: string;
  body: Record<string, unknown>;
}

export interface PackagesMockServer {
  url: string;
  close: () => void;
  /** 按时间序收到的提交（import / push / create-base / operations / config PUT）。 */
  submissions: PackageSubmission[];
  /** 服务端持有的集群配置（TOML 编辑器读写它）。 */
  config: { rev: number; toml: string };
}

const BASE_TOML = [
  'version = "v1alpha2"',
  'name = "tier1"',
  '',
  '[kubernetes]',
  'version = "1.33.1"',
  '',
  '[ssh]',
  'user = "root"',
  '',
  '[nodes.cp-1]',
  'address = "10.0.0.11"',
  'role = "control-plane"',
  '',
].join('\n');

async function serveStatic(pathname: string, res: ServerResponse): Promise<void> {
  const dist = path.resolve(import.meta.dirname, '../dist');
  const rel = pathname === '/' ? 'index.html' : pathname.slice(1);
  const file = path.join(dist, rel);
  if (!file.startsWith(dist)) {
    res.writeHead(403).end();
    return;
  }
  // SPA 回退：非资产路径（无扩展名）回落到 index.html，由前端路由接管。
  const candidates = path.extname(file) === '' ? [file, path.join(dist, 'index.html')] : [file];
  const attempts: Array<[string, Record<string, string>]> = [];
  for (const candidate of candidates) {
    attempts.push([`${candidate}.gz`, { 'Content-Encoding': 'gzip' }]);
    attempts.push([candidate, {}]);
  }
  for (const [target, headers] of attempts) {
    try {
      const data = await fs.readFile(target);
      res.writeHead(200, {
        'Content-Type': MIME[path.extname(target.replace(/\.gz$/, ''))] ?? 'application/octet-stream',
        ...headers,
      });
      res.end(data);
      return;
    } catch {
      // 尝试下一个候选
    }
  }
  res.writeHead(404).end();
}

/** 已登记包（import 会追加它；GET 返回当前快照）。 */
const packageRows: Array<Record<string, unknown>> = [
  {
    id: 'pkg-base-1.33.1',
    source: 'file:///media/usb/kurin-base-1.33.1.tar.zst',
    origin: 'imported',
    kind: 'base',
    name: 'base',
    version: '1.33.1',
    distribution: 'kubeadm',
    arch: ['amd64'],
    size: 1234567890,
    index: 'sha256:deadbeef',
    signatures: 0,
    sbom: null,
    created_at: '2026-10-20T08:00:00.000Z',
    verified_at: '2026-10-20T08:01:00.000Z',
  },
];

/** 启动 WP-65 e2e 的 mock 服务；resolve 的 url 是测试的 baseURL。 */
export function startPackagesMockServer(): Promise<PackagesMockServer> {
  const { promise, resolve } = Promise.withResolvers<PackagesMockServer>();
  const submissions: PackageSubmission[] = [];
  const config = { rev: 7, toml: BASE_TOML };
  let opSeq = 1;
  const opRecords = new Map<string, Record<string, unknown>>();

  const server: Server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    if (!url.pathname.startsWith('/api/')) {
      void serveStatic(url.pathname, res);
      return;
    }
    const json = (status: number, body: unknown) => {
      res.writeHead(status, { 'Content-Type': 'application/json' }).end(JSON.stringify(body));
    };
    const read = async () =>
      JSON.parse(
        (await req.toArray().then((c) => Buffer.concat(c).toString())) || '{}',
      ) as Record<string, unknown>;

    void (async () => {
      const body =
        req.method === 'POST' || req.method === 'PUT' || req.method === 'DELETE'
          ? await read()
          : {};
      const method = req.method ?? '';

      // ── 包端点 ──
      if (url.pathname === '/api/v1/packages' && method === 'GET') {
        json(200, { packages: packageRows });
        return;
      }
      if (url.pathname === '/api/v1/packages' && method === 'POST') {
        submissions.push({ method, path: url.pathname, body });
        const source = String(body.source ?? '');
        if (!/^(file|oci|oci-layout|https?):\/\/.+/.test(source)) {
          json(400, { code: 'invalid-request', message: `包来源不合法：${source}` });
          return;
        }
        const importedRow = {
          id: 'pkg-imported-e2e',
          source,
          origin: 'imported',
          kind: 'base',
          name: 'base',
          version: '1.34.0',
          distribution: 'kubeadm',
          arch: ['amd64'],
          size: 987654321,
          index: 'sha256:feedface',
          signatures: 0,
          sbom: null,
          created_at: '2026-10-27T08:00:00.000Z',
          verified_at: '2026-10-27T08:00:00.000Z',
        };
        packageRows.push(importedRow);
        json(201, importedRow);
        return;
      }
      const verifyApi = /^\/api\/v1\/packages\/([^/]+)\/verify$/.exec(url.pathname);
      if (verifyApi && method === 'POST') {
        submissions.push({ method, path: url.pathname, body: {} });
        json(202, { operation: `01JDYPKGE2E${String(opSeq++).padStart(4, '0')}`, state: 'queued', cluster: 'packages' });
        return;
      }
      const pushApi = /^\/api\/v1\/packages\/([^/]+)\/push$/.exec(url.pathname);
      if (pushApi && method === 'POST') {
        submissions.push({ method, path: url.pathname, body });
        if (!String(body.destination ?? '').startsWith('oci://')) {
          json(400, { code: 'invalid-request', message: 'push 的目标必须是 oci:// 注册表地址' });
          return;
        }
        json(202, { operation: `01JDYPKGE2E${String(opSeq++).padStart(4, '0')}`, state: 'queued', cluster: 'packages' });
        return;
      }
      if (url.pathname === '/api/v1/packages/create-base' && method === 'POST') {
        submissions.push({ method, path: url.pathname, body });
        json(202, { operation: `01JDYPKGE2E${String(opSeq++).padStart(4, '0')}`, state: 'queued', cluster: 'packages' });
        return;
      }

      // ── 集群操作（upgrade / certs-renew）──
      const operationsApi = /^\/api\/v1\/clusters\/([^/]+)\/operations$/.exec(url.pathname);
      if (operationsApi && method === 'POST') {
        submissions.push({ method, path: url.pathname, body });
        const cluster = operationsApi[1];
        const kind = String(body.kind ?? '');
        const params = (body.params as Record<string, unknown>) ?? {};
        const id = `01JDYOPSE2E${String(opSeq++).padStart(4, '0')}`;
        opRecords.set(id, {
          id,
          cluster,
          kind,
          state: 'succeeded',
          params,
          actor: 'e2e',
          error: null,
          createdAt: '2026-10-27T08:00:00.000Z',
          startedAt: '2026-10-27T08:00:01.000Z',
          finishedAt: '2026-10-27T08:00:02.000Z',
          logLines: 0,
          result: null,
        });
        json(202, opRecords.get(id));
        return;
      }

      // ── 集群配置（TOML 编辑器）──
      const configApi = /^\/api\/v1\/clusters\/([^/]+)\/config$/.exec(url.pathname);
      if (configApi) {
        if (method === 'GET') {
          json(200, { rev: config.rev, toml: config.toml, sha256: 'deadbeef' });
        } else {
          submissions.push({ method, path: url.pathname, body });
          const baseRev = Number(body.base_rev ?? 0);
          if (baseRev !== config.rev) {
            json(409, {
              code: 'conflict',
              message: `配置已被修改（当前修订 ${config.rev}）`,
            });
            return;
          }
          config.toml = String(body.toml ?? '');
          config.rev += 1;
          json(200, { rev: config.rev, sha256: 'feedface' });
        }
        return;
      }

      const op = /^\/api\/v1\/operations\/([^/]+)$/.exec(url.pathname);
      if (op && method === 'GET') {
        const record = opRecords.get(op[1]);
        json(record ? 200 : 404, record ?? { code: 'not-found', message: '操作不存在' });
        return;
      }

      // ── 校验（TOML 编辑器防抖调用）──
      if (url.pathname === '/api/v1/configs/validate' && method === 'POST') {
        const toml = String(body.toml ?? '');
        if (toml.includes('[nodes.')) {
          json(200, { valid: true });
        } else {
          json(422, {
            code: 'validation-failed',
            message: '校验失败',
            diagnostics: [
              { message: '需要至少一个 [nodes.*] 条目', labels: [{ offset: 0, len: 0, text: '' }] },
            ],
          });
        }
        return;
      }

      switch (url.pathname) {
        case '/api/v1/session':
          json(200, { expiresAt: '2026-12-31T00:00:00.000Z', version: '0.4.0' });
          return;
        case '/api/v1/clusters':
          json(200, [
            {
              name: 'tier1',
              distribution: 'kubeadm',
              kubernetesVersion: '1.33.1',
              lifecycle: 'active',
              currentRev: 7,
              nodesReady: 1,
              nodesTotal: 1,
              health: 'healthy',
              certMinDays: 210,
              observedAt: '2026-10-27T08:00:00.000Z',
              collectError: null,
              activeOperation: null,
            },
          ]);
          return;
        case '/api/v1/operations':
          json(200, { items: [], total: 0, page: 1, pageSize: 50 });
          return;
        default:
          break;
      }
      const clusterDetail = /^\/api\/v1\/clusters\/([^/]+)$/.exec(url.pathname);
      if (clusterDetail && method === 'GET') {
        json(200, {
          name: clusterDetail[1],
          distribution: 'kubeadm',
          kubernetesVersion: '1.33.1',
          lifecycle: 'active',
          currentRev: 7,
          createdAt: '2026-09-01T08:00:00.000Z',
          updatedAt: '2026-10-20T08:00:00.000Z',
          desiredNodes: [{ name: 'cp-1', address: '10.0.0.11', role: 'control-plane' }],
          observedNodes: [
            {
              name: 'cp-1',
              ready: true,
              roles: 'control-plane',
              version: 'v1.33.1',
              osImage: 'Ubuntu 24.04.1 LTS',
              arch: 'amd64',
              cpuCores: 4,
              memoryMb: 8192,
            },
          ],
          snapshot: {
            attemptedAt: '2026-10-27T08:00:00.000Z',
            observedAt: '2026-10-27T08:00:00.000Z',
            nodesReady: 1,
            nodesTotal: 1,
            podsRunning: 12,
            podsFailed: 0,
            podsNotRunning: 0,
            etcdHealth: 'ok',
            certMinDays: 210,
            error: null,
          },
          activeOperation: null,
          collectError: null,
          configSummary: {
            distribution: 'kubeadm',
            kubernetesVersion: '1.33.1',
            runtime: 'containerd',
            zone: 'global',
            cni: 'calico',
            podCidr: '10.244.0.0/16',
            serviceCidr: '10.96.0.0/12',
            haMode: 'none',
            vip: null,
          },
        });
        return;
      }
      json(404, { code: 'not-found', message: 'no mock' });
    })();
  });

  server.listen(0, '127.0.0.1', () => {
    const addr = server.address();
    const port = typeof addr === 'object' && addr != null ? addr.port : 0;
    resolve({
      url: `http://127.0.0.1:${port}`,
      close: () => server.close(),
      submissions,
      config,
    });
  });
  return promise;
}
