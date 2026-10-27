/**
 * 测试内 HTTP 服务（WP-63 e2e）：静态 dist + /api/v1 mock + 可编程 SSE 流。
 *
 * Playwright 的 route.fulfill 无法流式推送（响应立即结束，EventSource 不派发
 * message 事件），因此整个「后端」由该 Node 服务提供：
 * - 静态资源：只嵌 .gz 的 dist（UI §9），按 Accept-Encoding 直出 gzip；
 * - API：session / 操作详情 / 日志分页（空页，数据全走 SSE）；
 * - SSE：首连推 firstBatch 条后销毁 socket；重连按 `?after=` 补读并保持连接。
 */
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { createServer, type Server, type ServerResponse } from 'node:http';
import type { OperationEventEnvelope } from '../src/api/types';

export interface MockServer {
  url: string;
  close: () => void;
  /** 每条 SSE 连接的 after 游标（按连接顺序）。 */
  seenAfter: number[];
}

const OP_ID = '01JDY8Z9X7M2Q3R4S5T6U7V8WD';
const DIST = path.resolve(import.meta.dirname, '../dist');

function sseFrame(ev: OperationEventEnvelope): string {
  return `id: ${ev.seq}\ndata: ${JSON.stringify(ev)}\n\n`;
}

export function buildEvents(count: number): OperationEventEnvelope[] {
  const base = Date.parse('2026-10-27T08:00:00.000Z');
  const events: OperationEventEnvelope[] = [
    { seq: 1, ts: new Date(base + 100).toISOString(), kind: 'state', payload: { type: 'state', state: 'running' } },
    {
      seq: 2,
      ts: new Date(base + 200).toISOString(),
      kind: 'task',
      payload: { type: 'task-start', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare' },
    },
  ];
  for (let i = 3; i <= count - 1; i += 1) {
    events.push({
      seq: i,
      ts: new Date(base + i * 100).toISOString(),
      kind: 'log',
      payload: {
        type: 'log',
        node: 'cp-1',
        stage: 'prepare',
        task: 'prepare/sysctl@cp-1',
        level: 'info',
        stream: 'stdout',
        line: `mock-line-${String(i).padStart(4, '0')}`,
      },
    });
  }
  events.push({
    seq: count,
    ts: new Date(base + count * 100).toISOString(),
    kind: 'task',
    payload: { type: 'task-done', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', ms: 1000 },
  });
  return events;
}

/** 启动 mock 服务；resolve 的 url 即测试用例的 baseURL。 */
export function startMockServer(
  events: OperationEventEnvelope[],
  firstBatch: number,
): Promise<MockServer> {
  const { promise, resolve } = Promise.withResolvers<MockServer>();
  let connections = 0;
  const seenAfter: number[] = [];

  const server: Server = createServer((req, res) => {
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

    if (url.pathname === `/api/v1/operations/${OP_ID}`) {
      json(res, 200, {
        id: OP_ID,
        cluster: 'tier1',
        kind: 'create',
        state: 'running',
        params: {},
        retryOf: null,
        actor: 'e2e',
        error: null,
        createdAt: '2026-10-27T07:50:00.000Z',
        startedAt: '2026-10-27T07:50:02.000Z',
        finishedAt: null,
        logLines: events.length,
        lastSeq: 0,
        progress: {
          tasks: [{ id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', state: 'pending' }],
        },
      });
      return;
    }
    if (url.pathname === `/api/v1/operations/${OP_ID}/logs`) {
      json(res, 200, { items: [], lastSeq: 0 });
      return;
    }

    if (url.pathname === `/api/v1/operations/${OP_ID}/events`) {
      // 客户端断线重连：浏览器原生重连带 Last-Event-ID 头（sse.ts 依赖它），
      // 手动重连带 ?after= —— 两者取最大值作为补读游标。
      const headerId = Number(req.headers['last-event-id'] ?? '0');
      const after = Math.max(Number(url.searchParams.get('after') ?? '0'), headerId || 0);
      seenAfter.push(after);
      connections += 1;
      res.writeHead(200, {
        'Content-Type': 'text/event-stream',
        'Cache-Control': 'no-cache',
        Connection: 'keep-alive',
      });
      if (connections === 1) {
        // 首连：推前 firstBatch 条，然后销毁连接（模拟网络断开 / Lagged）。
        res.write(events.slice(0, firstBatch).map(sseFrame).join(''));
        res.socket?.destroy();
        return;
      }
      // 重连：补读 after 之后的所有事件（模拟服务端按 seq 从库补读 + 广播）。
      const backfill = events.filter((e) => e.seq > after);
      res.write(backfill.map(sseFrame).join(''));
      // 保持连接打开（不再断），让页面停留在已连接状态。
      return;
    }

    json(res, 404, { code: 'not-found', message: 'no mock' });
  });

  server.listen(0, '127.0.0.1', () => {
    const addr = server.address();
    const port = typeof addr === 'object' && addr != null ? addr.port : 0;
    resolve({
      url: `http://127.0.0.1:${port}`,
      close: () => server.close(),
      seenAfter,
    });
  });
  return promise;
}

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { 'Content-Type': 'application/json' }).end(JSON.stringify(body));
}

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
};

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
