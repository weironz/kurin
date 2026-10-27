/**
 * WP-64 e2e 的 mock 后端：addon 管理页与危险区需要的全部端点。
 *
 * 与 wizardServe.ts 同理：静态资源从 dist 读取（含 .gz 直出），API 用
 * Node http 服务；提交记录存进数组供测试断言（addon 安装/卸载、危险操作、
 * archive 的请求体形状——confirm == 集群名的服务端契约在 mock 里同样校验）。
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
export interface AddonSubmission {
  method: string;
  cluster: string;
  body: Record<string, unknown>;
}

export interface AddonsMockServer {
  url: string;
  close: () => void;
  /** 按时间序收到的 /clusters/{name}/addons 与 /operations 提交。 */
  submissions: AddonSubmission[];
  /** archive 是否已被调用（含集群名）。 */
  archived: string[];
}

const CATALOG = [
  {
    name: 'metrics-server',
    version: '0.9.0',
    title: 'Metrics Server',
    description: '资源指标 API（metrics.k8s.io）',
    category: 'monitoring',
    icon: null,
    namespace: 'kube-system',
    requires: [],
    conflicts: [],
    values_schema: {
      type: 'object',
      additionalProperties: false,
      properties: { replicas: { type: 'integer', minimum: 1, maximum: 8, default: 1 } },
    },
    origin: 'builtin',
  },
  {
    name: 'ingress-nginx',
    version: '1.15.1',
    title: 'Ingress NGINX Controller',
    description: 'NGINX Ingress 控制器',
    category: 'ingress',
    icon: null,
    namespace: 'ingress-nginx',
    requires: [],
    conflicts: [],
    values_schema: {},
    origin: 'builtin',
  },
];

/** e2e 用的集群详情（tier1：健康、无活动操作）。 */
const CLUSTER = {
  name: 'tier1',
  distribution: 'kubeadm',
  kubernetesVersion: 'v1.33.1',
  lifecycle: 'active',
  currentRev: 3,
  createdAt: '2026-09-01T08:00:00.000Z',
  updatedAt: '2026-10-20T08:00:00.000Z',
  desiredNodes: [
    { name: 'cp-1', address: '10.0.0.11', role: 'control-plane' },
    { name: 'worker-1', address: '10.0.0.21', role: 'worker' },
  ],
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
  snapshot: null,
  activeOperation: null,
  collectError: null,
  configSummary: {
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.33.1',
    runtime: 'containerd',
    zone: 'global',
    cni: 'calico',
    podCidr: '10.244.0.0/16',
    serviceCidr: '10.96.0.0/12',
    haMode: 'none',
    vip: null,
  },
};

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

/** 启动 WP-64 e2e 的 mock 服务；resolve 的 url 是测试的 baseURL。 */
export function startAddonsMockServer(): Promise<AddonsMockServer> {
  const { promise, resolve } = Promise.withResolvers<AddonsMockServer>();
  const submissions: AddonSubmission[] = [];
  const archived: string[] = [];
  /** 已安装清单（install/remove 会更新它，页面刷新即可见）。 */
  const installed: Record<string, Record<string, unknown>> = {};
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
      const body = req.method === 'POST' || req.method === 'PUT' || req.method === 'DELETE'
        ? await read()
        : {};
      const addonsApi = /^\/api\/v1\/clusters\/([^/]+)\/addons$/.exec(url.pathname);
      if (addonsApi) {
        const cluster = addonsApi[1];
        submissions.push({ method: req.method ?? '', cluster, body });
        if (req.method === 'GET') {
          const empty = Object.keys(installed).length === 0;
          json(200, {
            cluster,
            addons: installed,
            note: empty ? '集群还没有 addon 记录（或尚未采集）' : '',
          });
          return;
        }
        // 卸载：confirm 必须等于集群名（服务端契约）。
        if (req.method === 'DELETE' && body.confirm !== cluster) {
          json(400, {
            code: 'invalid-request',
            message: `卸载 addon 需要 confirm 字段等于集群名 "${cluster}"`,
          });
          return;
        }
        const id = `01JDYADDONE2E${String(opSeq++).padStart(4, '0')}`;
        opRecords.set(id, {
          id,
          cluster,
          kind: req.method === 'DELETE' ? 'addon-remove' : 'addon-install',
          state: 'succeeded',
          params: {},
          actor: 'e2e',
          error: null,
          createdAt: '2026-10-27T08:00:00.000Z',
          startedAt: '2026-10-27T08:00:01.000Z',
          finishedAt: '2026-10-27T08:00:02.000Z',
          logLines: 0,
          result: null,
        });
        if (req.method === 'DELETE') {
          delete installed[String(body.addon ?? '')];
        } else {
          installed[String(body.addon ?? '')] = {
            version: String(body.version ?? '0.0.0'),
            status: 'installed',
          };
        }
        json(202, { operation: id, state: 'queued', cluster });
        return;
      }
      const operationsApi = /^\/api\/v1\/clusters\/([^/]+)\/operations$/.exec(url.pathname);
      if (operationsApi && req.method === 'POST') {
        const cluster = operationsApi[1];
        submissions.push({ method: 'POST', cluster, body });
        const kind = String(body.kind ?? '');
        if (
          (kind === 'delete-node' || kind === 'delete-cluster') &&
          body.confirm !== cluster
        ) {
          json(400, {
            code: 'invalid-request',
            message: `破坏性操作需要 confirm 字段等于集群名 "${cluster}"`,
          });
          return;
        }
        const id = `01JDYADDONE2E${String(opSeq++).padStart(4, '0')}`;
        opRecords.set(id, {
          id,
          cluster,
          kind,
          state: 'succeeded',
          params: (body.params as Record<string, unknown>) ?? {},
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
      const archiveApi = /^\/api\/v1\/clusters\/([^/]+)\/archive$/.exec(url.pathname);
      if (archiveApi && req.method === 'POST') {
        archived.push(archiveApi[1]);
        json(200, { name: archiveApi[1], lifecycle: 'archived' });
        return;
      }
      const op = /^\/api\/v1\/operations\/([^/]+)$/.exec(url.pathname);
      if (op && req.method === 'GET') {
        const record = opRecords.get(op[1]);
        json(record ? 200 : 404, record ?? { code: 'not-found', message: '操作不存在' });
        return;
      }
      switch (url.pathname) {
        case '/api/v1/session':
          json(200, { expiresAt: '2026-12-31T00:00:00.000Z', version: '0.4.0' });
          return;
        case '/api/v1/addons/catalog':
          json(200, { addons: CATALOG });
          return;
        case '/api/v1/clusters':
          json(200, [
            {
              name: 'tier1',
              distribution: 'kubeadm',
              kubernetesVersion: 'v1.33.1',
              lifecycle: 'active',
              currentRev: 3,
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
      if (clusterDetail && req.method === 'GET') {
        json(200, { ...CLUSTER, name: clusterDetail[1] });
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
      archived,
    });
  });
  return promise;
}
