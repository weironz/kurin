/**
 * WP-62 e2e 的 mock 后端：向导 9 步需要的全部端点。
 *
 * 与 serve.ts 同理：route.fulfill 无法流式推送，但向导流程只走 JSON，
 * 普通 Node http 服务即可；静态资源仍从 dist 读取（含 .gz 直出）。
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

/** 服务端持有的状态（测试断言用）。 */
export interface WizardMockServer {
  url: string;
  close: () => void;
  /** `POST /clusters/{name}/operations` 收到的提交（kind + params）。 */
  submitted: Array<{ cluster: string; kind: string; params: Record<string, unknown> }>;
  /** render 收到的 DTO（最后一份）。 */
  lastRenderBody: () => Record<string, unknown> | null;
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

const PACKAGES = [
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

function renderToml(body: Record<string, unknown>): string {
  const lines = ['version = "v1alpha2"'];
  if (typeof body.name === 'string') lines.push(`name = "${body.name}"`);
  lines.push('', '[kubernetes]', 'version = "1.33.1"', '', '[ssh]', 'user = "root"', '');
  const nodes = (body.nodes as Array<Record<string, unknown>> | undefined) ?? [];
  for (const node of nodes) {
    lines.push(`[nodes.${node.name}]`);
    lines.push(`address = "${node.address}"`);
    lines.push(`role = "${node.role ?? 'worker'}"`, '');
  }
  return lines.join('\n') + '\n';
}

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

/** 启动向导 e2e 的 mock 服务；resolve 的 url 是测试的 baseURL。 */
export function startWizardMockServer(): Promise<WizardMockServer> {
  const { promise, resolve } = Promise.withResolvers<WizardMockServer>();
  const submitted: WizardMockServer['submitted'] = [];
  const renderBodies: Array<Record<string, unknown>> = [];
  let opSeq = 1;
  const opResults = new Map<string, unknown>();

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
      (JSON.parse((await req.toArray().then((c) => Buffer.concat(c).toString())) || '{}') as Record<string, unknown>);

    void (async () => {
      const body = (req.method === 'POST' || req.method === 'PUT') ? await read() : {};
      switch (url.pathname) {
        case '/api/v1/session':
          json(200, { expiresAt: '2026-12-31T00:00:00.000Z', version: '0.4.0' });
          return;
        case '/api/v1/meta':
          json(200, {
            version: '0.4.0',
            components: { kubernetes: '1.33.1', containerd: '1.7.22' },
            capabilities: [
              { id: 'cni/calico', kind: 'cni', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
              { id: 'cni/flannel', kind: 'cni', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
              { id: 'ha/kube-vip', kind: 'ha', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'online-only', zones: ['global', 'cn'], ops: ['create'] },
            ],
          });
          return;
        case '/api/v1/addons/catalog':
          json(200, { addons: CATALOG });
          return;
        case '/api/v1/packages':
          json(200, { packages: PACKAGES });
          return;
        case '/api/v1/configs/render': {
          renderBodies.push(body);
          const toml = renderToml(body);
          json(200, { toml, valid: true, diagnostics: [] });
          return;
        }
        case '/api/v1/configs/validate':
          json(200, { valid: true });
          return;
        case '/api/v1/nodes/parse': {
          const input = String(body.input ?? '');
          const rows: Array<Record<string, string>> = [];
          for (const raw of input.split('\n')) {
            const line = raw.split('#')[0].trim();
            if (line === '') continue;
            const [range, role = 'worker'] = line.split(/\s+/);
            const m = /^(\d+\.\d+\.\d+\.)(\d+)-(\d+)$/.exec(range);
            if (!m) {
              json(422, {
                code: 'validation-failed',
                message: '校验失败',
                diagnostics: [{ message: `invalid IP list: ${range}` }],
              });
              return;
            }
            for (let i = Number(m[2]); i <= Number(m[3]); i += 1) {
              rows.push({
                name: `${role === 'control-plane' || role === 'cp' ? 'cp' : role}-${rows.length + 1}`,
                address: `${m[1]}${i}`,
                role: role === 'control-plane' || role === 'cp' ? 'controlplane' : role,
              });
            }
          }
          json(200, { nodes: rows });
          return;
        }
        case '/api/v1/nodes/probe':
          json(200, {
            facts: {
              arch: 'amd64',
              os: 'ubuntu',
              os_version: '24.04',
              kernel: '6.8.0-45-generic',
              cgroup_v2: true,
              init_systemd: true,
              cpus: 4,
              mem_mib: 8192,
              disk_free_mib: { '/': 102400, '/var/lib': 92160 },
              hostname: 'probe-host',
              user: 'root',
              is_root: true,
              sudo: 'passwordless',
              tcp_forwarding: true,
              curl: true,
              existing_install: 'none',
            },
          });
          return;
        default:
          break;
      }
      // 集合路由：向导的 ensureClusterRegistered 走 POST /clusters。
      if (url.pathname === '/api/v1/clusters' && req.method === 'POST') {
        json(201, {
          cluster: { name: String(body.name ?? ''), lifecycle: 'draft' },
          rev: 1,
          sha256: 'deadbeef',
        });
        return;
      }
      // 参数化路由
      const register = /^\/api\/v1\/clusters\/([^/]+)$/.exec(url.pathname);
      if (register && req.method === 'POST') {
        json(201, { cluster: { name: register[1], lifecycle: 'draft' }, rev: 1, sha256: 'deadbeef' });
        return;
      }
      const config = /^\/api\/v1\/clusters\/([^/]+)\/config$/.exec(url.pathname);
      if (config) {
        if (req.method === 'GET') {
          json(200, { rev: 1, toml: renderToml(renderBodies.at(-1) ?? {}), sha256: 'deadbeef' });
        } else {
          json(200, { rev: 2, sha256: 'deadbeef' });
        }
        return;
      }
      const operations = /^\/api\/v1\/clusters\/([^/]+)\/operations$/.exec(url.pathname);
      if (operations && req.method === 'POST') {
        const cluster = operations[1];
        const kind = String(body.kind ?? 'create');
        submitted.push({ cluster, kind, params: (body.params as Record<string, unknown>) ?? {} });
        const id = `01JDYWIZE2E${String(opSeq++).padStart(5, '0')}`;
        const record = {
          id,
          cluster,
          kind,
          state: 'succeeded',
          params: {},
          actor: 'e2e',
          error: null,
          createdAt: '2026-10-27T08:00:00.000Z',
          startedAt: '2026-10-27T08:00:01.000Z',
          finishedAt: '2026-10-27T08:00:02.000Z',
          logLines: 0,
          result:
            kind === 'precheck'
              ? {
                  checks: [
                    { node: 'cp-1', check: 'ports', status: 'ok', detail: '' },
                    { node: 'cp-1', check: 'sudo', status: 'ok', detail: '' },
                  ],
                  failed: 0,
                  warned: 0,
                }
              : kind === 'plan'
                ? {
                    op: 'create',
                    phases: [
                      {
                        id: 'prepare',
                        label: '准备',
                        tasks: [{ key: 'prepare/sysctl@cp-1', node: 'cp-1', describe: '内核参数' }],
                      },
                    ],
                  }
                : null,
        };
        opResults.set(id, record);
        json(202, record);
        return;
      }
      const op = /^\/api\/v1\/operations\/([^/]+)$/.exec(url.pathname);
      if (op) {
        const record = opResults.get(op[1]);
        json(record ? 200 : 404, record ?? { code: 'not-found', message: '操作不存在' });
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
      submitted,
      lastRenderBody: () => renderBodies.at(-1) ?? null,
    });
  });
  return promise;
}
