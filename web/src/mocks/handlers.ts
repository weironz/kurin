/**
 * MSW handlers：覆盖 app shell 需要的端点（WP-60）。
 * 在测试中通过 setupServer 启用；dev 模式用 `VITE_USE_MOCKS=1` 启用（见 mocks/browser.ts）。
 */
import { http, HttpResponse } from 'msw';
import type { ClusterSummary, Meta, OperationList, OperationSummary } from '../api/types';

const now = '2026-10-27T08:00:00.000Z';

export const mockMeta: Meta = {
  version: '0.4.0',
  componentVersions: {
    kubernetes: 'v1.33.1',
    containerd: '1.7.22',
    etcd: '3.5.16',
    cni: 'v1.0.1',
  },
  capabilities: {
    'distribution:k3s:cni': 'k3s 只支持 flannel | none',
    'cni:kube-ovn:offline': 'kube-ovn 离线不可用',
    'distribution:k3s:lb': 'k3s × kurin-lb 暂不支持',
    'kubernetes:min': 'K8s ≥ 1.30',
  },
};

export const mockClusters: ClusterSummary[] = [
  {
    name: 'tier1',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.33.1',
    lifecycle: 'active',
    currentRev: 3,
    nodesReady: 3,
    nodesTotal: 3,
    health: 'healthy',
    certMinDays: 210,
    observedAt: now,
    collectError: null,
    activeOperation: null,
  },
  {
    name: 'tier2-edge',
    distribution: 'k3s',
    kubernetesVersion: 'v1.33.1+k3s1',
    lifecycle: 'active',
    currentRev: 1,
    nodesReady: 1,
    nodesTotal: 2,
    health: 'degraded',
    certMinDays: 90,
    observedAt: now,
    collectError: null,
    activeOperation: null,
  },
];

export const mockOperations: OperationSummary[] = [
  {
    id: '01JDY8Z9X7M2Q3R4S5T6U7V8WA',
    cluster: 'tier1',
    kind: 'precheck',
    state: 'succeeded',
    params: {},
    actor: 'session-1',
    error: null,
    createdAt: now,
    startedAt: now,
    finishedAt: now,
    logLines: 42,
  },
  {
    id: '01JDY8Z9X7M2Q3R4S5T6U7V8WB',
    cluster: 'tier2-edge',
    kind: 'add-nodes',
    state: 'running',
    params: { nodes: ['worker-2'] },
    actor: 'session-1',
    error: null,
    createdAt: now,
    startedAt: now,
    finishedAt: null,
    logLines: 128,
  },
];

const mockOperationList: OperationList = {
  items: mockOperations,
  total: mockOperations.length,
  page: 1,
  pageSize: 50,
};

/** 已建立会话的标记（测试与 dev 都从无 cookie 开始）。 */
let sessionEstablished = false;

export function resetSessionState(): void {
  sessionEstablished = false;
}

export const handlers = [
  // 存活
  http.get('/api/v1/healthz', () => HttpResponse.json({ status: 'ok' })),

  // 会话（UI §6.10）
  http.post('/api/v1/session', async ({ request }) => {
    const body = (await request.json()) as { token?: string };
    if (!body.token || body.token === 'wrong-token') {
      return HttpResponse.json({ code: 'unauthorized', message: '令牌无效' }, { status: 401 });
    }
    sessionEstablished = true;
    return HttpResponse.json({
      expiresAt: '2026-10-27T12:00:00.000Z',
      version: mockMeta.version,
    });
  }),
  http.get('/api/v1/session', () => {
    if (!sessionEstablished) {
      return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
    }
    return HttpResponse.json({
      expiresAt: '2026-10-27T12:00:00.000Z',
      version: mockMeta.version,
    });
  }),
  http.delete('/api/v1/session', () => {
    sessionEstablished = false;
    return new HttpResponse(null, { status: 204 });
  }),

  // meta
  http.get('/api/v1/meta', () => {
    if (!sessionEstablished) {
      return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
    }
    return HttpResponse.json(mockMeta);
  }),

  // 集群列表
  http.get('/api/v1/clusters', () => {
    if (!sessionEstablished) {
      return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
    }
    return HttpResponse.json(mockClusters);
  }),

  // 集群详情
  http.get('/api/v1/clusters/:name', ({ params }) => {
    if (!sessionEstablished) {
      return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
    }
    const name = params['name'] as string;
    const summary = mockClusters.find((c) => c.name === name);
    if (!summary) {
      return HttpResponse.json(
        { code: 'not-found', message: `集群 ${name} 不存在` },
        { status: 404 },
      );
    }
    return HttpResponse.json({
      ...summary,
      createdAt: now,
      updatedAt: now,
      desiredNodes: [],
      observedNodes: [],
      snapshot: null,
    });
  }),

  // 操作列表
  http.get('/api/v1/operations', ({ request }) => {
    if (!sessionEstablished) {
      return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
    }
    const url = new URL(request.url);
    const cluster = url.searchParams.get('cluster');
    const state = url.searchParams.get('state');
    let items = mockOperations;
    if (cluster) items = items.filter((op) => op.cluster === cluster);
    if (state) items = items.filter((op) => op.state === state);
    return HttpResponse.json({ ...mockOperationList, items, total: items.length });
  }),

  // 操作详情
  http.get('/api/v1/operations/:id', ({ params }) => {
    if (!sessionEstablished) {
      return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
    }
    const id = params['id'] as string;
    const op = mockOperations.find((o) => o.id === id);
    if (!op) {
      return HttpResponse.json(
        { code: 'not-found', message: `操作 ${id} 不存在` },
        { status: 404 },
      );
    }
    return HttpResponse.json(op);
  }),
];
