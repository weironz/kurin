/**
 * WP-63 的 MSW handlers：操作详情、日志分页、取消/重试、SSE 事件流（node 场景）。
 * SSE 在 jsdom 中没有 EventSource，测试一律注入 eventSourceFactory；
 * dev 模式（浏览器）走 handlers.ts 的主集合（不含本文件，MSW 拦不了流）。
 */
import { http, HttpResponse } from 'msw';
import type {
  OperationDetail,
  OperationEventEnvelope,
  OperationLogsPage,
} from '../api/types';
import { mockOperations } from './handlers';

/**
 * 场景事件（一个完成的 create：2 阶段 × 3 节点 + 日志 + 资源 + 失败任务）。
 * seq 连续；测试用它做网格聚合、去重、乱序与断言。
 */
export function buildMockEvents(): OperationEventEnvelope[] {
  const events: OperationEventEnvelope[] = [];
  const base = Date.parse('2026-10-27T08:00:00.000Z');
  let seq = 0;
  let ms = 0;
  const push = (kind: OperationEventEnvelope['kind'], payload: unknown) => {
    ms += 120;
    events.push({
      seq: (seq += 1),
      ts: new Date(base + ms).toISOString(),
      kind,
      payload: payload as OperationEventEnvelope['payload'],
    });
  };
  push('state', { type: 'state', state: 'running' });
  // prepare 阶段：cp-1 / cp-2
  push('task', { type: 'task-start', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare' });
  push('task', { type: 'task-start', id: 'prepare/sysctl@cp-2', node: 'cp-2', stage: 'prepare' });
  push('log', {
    type: 'log',
    node: 'cp-1',
    stage: 'prepare',
    task: 'prepare/sysctl@cp-1',
    level: 'info',
    stream: 'stdout',
    line: '[prepare] sysctl applied',
  });
  push('task', { type: 'task-done', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', ms: 812 });
  push('task', { type: 'task-skipped', id: 'prepare/sysctl@cp-2', node: 'cp-2', stage: 'prepare' });
  // bootstrap 阶段
  push('task', {
    type: 'task-start',
    id: 'bootstrap/kubeadm-init@cp-1',
    node: 'cp-1',
    stage: 'bootstrap',
  });
  push('log', {
    type: 'log',
    node: 'cp-1',
    stage: 'bootstrap',
    task: 'bootstrap/kubeadm-init@cp-1',
    level: 'info',
    stream: 'stdout',
    line: '[init] Using Kubernetes version: v1.33.1',
  });
  push('log', {
    type: 'log',
    node: 'cp-1',
    stage: 'bootstrap',
    task: 'bootstrap/kubeadm-init@cp-1',
    level: 'warn',
    stream: 'stderr',
    line: '[preflight] swap is enabled',
  });
  push('task', {
    type: 'task-done',
    id: 'bootstrap/kubeadm-init@cp-1',
    node: 'cp-1',
    stage: 'bootstrap',
    ms: 102_381,
  });
  // join 阶段：worker-1 失败
  push('task', { type: 'task-start', id: 'join/kubeadm-join@worker-1', node: 'worker-1', stage: 'join' });
  push('log', {
    type: 'log',
    node: 'worker-1',
    stage: 'join',
    task: 'join/kubeadm-join@worker-1',
    level: 'error',
    stream: 'stderr',
    line: 'ERROR FileAvailable--etc-kubernetes-kubelet.conf',
  });
  push('task', {
    type: 'task-failed',
    id: 'join/kubeadm-join@worker-1',
    node: 'worker-1',
    stage: 'join',
    reason: 'exit 1: [ERROR FileAvailable--etc-kubernetes-kubelet.conf]',
  });
  // addons 阶段：资源就绪表
  push('task', {
    type: 'task-warn',
    id: 'addons/ingress-nginx@cp-1',
    node: 'cp-1',
    stage: 'addons',
    message: 'helm upgrade --wait 超时一次，重试成功',
  });
  push('resource', {
    type: 'resource',
    stage: 'addons',
    kind: 'Deployment',
    namespace: 'ingress-nginx',
    name: 'ingress-nginx-controller',
    ready: false,
    detail: '0/2 ready',
  });
  push('resource', {
    type: 'resource',
    stage: 'addons',
    kind: 'Deployment',
    namespace: 'ingress-nginx',
    name: 'ingress-nginx-controller',
    ready: true,
    detail: '2/2 ready',
  });
  push('state', { type: 'state', state: 'failed', error: 'join/kubeadm-join@worker-1 失败' });
  return events;
}

export const mockOperationDetail: OperationDetail = {
  id: '01JDY8Z9X7M2Q3R4S5T6U7V8WD',
  cluster: 'tier1',
  kind: 'create',
  state: 'failed',
  params: {},
  retryOf: null,
  actor: 'session-1',
  error: 'join/kubeadm-join@worker-1 失败',
  createdAt: '2026-10-27T07:50:00.000Z',
  startedAt: '2026-10-27T07:50:02.000Z',
  finishedAt: '2026-10-27T08:02:11.000Z',
  logLines: 18,
  lastSeq: 18,
  progress: {
    tasks: [
      { id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', state: 'pending' },
      { id: 'prepare/sysctl@cp-2', node: 'cp-2', stage: 'prepare', state: 'pending' },
      { id: 'bootstrap/kubeadm-init@cp-1', node: 'cp-1', stage: 'bootstrap', state: 'pending' },
      { id: 'join/kubeadm-join@worker-1', node: 'worker-1', stage: 'join', state: 'pending' },
      { id: 'addons/ingress-nginx@cp-1', node: 'cp-1', stage: 'addons', state: 'pending' },
    ],
  },
};

const events = buildMockEvents();

/** 测试助手：取事件子集。 */
function eventsBetween(after: number, limit: number): OperationEventEnvelope[] {
  return events.filter((e) => e.seq > after).slice(0, limit);
}

export const operationDetailHandlers = [
  // 操作详情：注册在 handlers.ts 的通用版之前（MSW 先注册先匹配），
  // 因此这里要覆盖「全部已知操作」——WP-63 场景返回带 progress 的完整详情，
  // 列表 mock 的三个操作返回摘要（progress/lastSeq 缺省，页面走事件流聚合）。
  http.get('/api/v1/operations/:id', ({ params }) => {
    const id = params['id'] as string;
    if (id === mockOperationDetail.id) {
      return HttpResponse.json(mockOperationDetail);
    }
    const listed = mockOperations.find((o) => o.id === id);
    if (listed) {
      return HttpResponse.json({ ...listed, lastSeq: 0, progress: undefined });
    }
    return HttpResponse.json(
      { code: 'not-found', message: `操作 ${id} 不存在` },
      { status: 404 },
    );
  }),

  // 任意操作的日志页：WP-63 场景返回事件；其他（列表 mock）返回空页，
  // 详情页对 404 的 /logs 会走错误分支，空页让页面正常渲染。
  http.get('/api/v1/operations/:id/logs', ({ request, params }) => {
    const url = new URL(request.url);
    if (params['id'] !== mockOperationDetail.id) {
      const empty: OperationLogsPage = { items: [], lastSeq: 0 };
      return HttpResponse.json(empty);
    }
    const after = Number(url.searchParams.get('after') ?? '0');
    const limit = Number(url.searchParams.get('limit') ?? '500');
    const items = eventsBetween(
      Number.isFinite(after) ? after : 0,
      Number.isFinite(limit) ? limit : 500,
    );
    const page: OperationLogsPage = {
      items,
      lastSeq: events.length > 0 ? events[events.length - 1]!.seq : 0,
    };
    return HttpResponse.json(page);
  }),

  http.post('/api/v1/operations/:id/cancel', ({ params }) => {
    if (params['id'] !== mockOperationDetail.id) {
      return HttpResponse.json({ code: 'not-found', message: '操作不存在' }, { status: 404 });
    }
    return HttpResponse.json({ ...mockOperationDetail, state: 'cancelled' }, { status: 202 });
  }),

  http.post('/api/v1/operations/:id/retry', () =>
    HttpResponse.json(
      {
        ...mockOperationDetail,
        id: '01JDY8Z9X7M2Q3R4S5T6U7V8WE',
        state: 'queued',
        retryOf: mockOperationDetail.id,
        error: null,
      },
      { status: 202 },
    ),
  ),
];
