/**
 * WP-78 语言切换 e2e 的 mock 后端：session + 集群列表两个端点即可渲染集群总览。
 *
 * 与 packagesServe.ts 同理：静态资源从 dist 读取（含 .gz 直出），API 用
 * Node http 服务；这里不需要可编程 SSE，只求最小可用页面。
 */
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { createServer, type ServerResponse } from 'node:http';

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

export interface LocaleMockServer {
  url: string;
  close: () => void;
}

const DIST = path.resolve(import.meta.dirname, '../dist');

/** 一行集群（服务端 snake_case 的 `ClusterView` 形状，见 src/api/clusters.ts）。 */
const CLUSTER = {
  name: 'tier1',
  lifecycle: 'active',
  current_rev: 3,
  created_at: '2026-10-01T08:00:00.000Z',
  updated_at: '2026-10-27T08:00:00.000Z',
  distribution: 'kubeadm',
  kubernetes_version: '1.33.1',
  nodes_ready: 3,
  nodes_total: 3,
  health: 'healthy',
  cert_min_days: 210,
  observed_at: '2026-10-27T08:00:00.000Z',
  collect_error: null,
  active_operation: null,
};

/** 启动语言切换 e2e 的 mock 服务；resolve 的 url 是测试的 baseURL。 */
export function startLocaleMockServer(): Promise<LocaleMockServer> {
  const { promise, resolve } = Promise.withResolvers<LocaleMockServer>();

  const server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');

    if (!url.pathname.startsWith('/api/')) {
      void serveStatic(url.pathname, res);
      return;
    }

    if (url.pathname === '/api/v1/session') {
      if (req.method === 'DELETE') {
        res.writeHead(204).end();
        return;
      }
      json(res, 200, { expiresAt: '2026-12-31T00:00:00.000Z', version: '0.4.0' });
      return;
    }

    if (url.pathname === '/api/v1/clusters') {
      json(res, 200, { clusters: [CLUSTER] });
      return;
    }

    json(res, 404, { code: 'not-found', message: 'no mock' });
  });

  server.listen(0, '127.0.0.1', () => {
    const addr = server.address();
    const port = typeof addr === 'object' && addr != null ? addr.port : 0;
    resolve({ url: `http://127.0.0.1:${port}`, close: () => server.close() });
  });
  return promise;
}

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { 'Content-Type': 'application/json' }).end(JSON.stringify(body));
}

/** dist 只含 .gz（UI §9），直出 gzip（Content-Encoding）。 */
async function serveStatic(pathname: string, res: ServerResponse): Promise<void> {
  const clean = pathname === '/' ? '/index.html' : pathname;
  for (const candidate of [clean, '/index.html']) {
    const gzPath = path.join(DIST, `${candidate}.gz`);
    try {
      const file = await fs.readFile(gzPath);
      const ext = path.extname(candidate);
      res.writeHead(200, {
        'Content-Type': MIME[ext] ?? 'application/octet-stream',
        'Content-Encoding': 'gzip',
      });
      res.end(file);
      return;
    } catch {
      // 该资源无 .gz，尝试下一个候选。
    }
  }
  res.writeHead(404).end('not found');
}
