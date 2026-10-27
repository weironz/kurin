/**
 * MSW handlers：覆盖 app shell 与集群页面需要的端点（WP-60 / WP-61）。
 * 在测试中通过 setupServer 启用；dev 模式用 `VITE_USE_MOCKS=1` 启用（见 mocks/browser.ts）。
 *
 * 集群矩阵（UI §8.1 五种健康状态 + 一个期望缺失节点）：
 * - tier1      kubeadm，healthy（4/4 节点全部就绪）
 * - tier2-edge k3s，    degraded（worker-1 未就绪 + edge-worker-2 期望缺失，正在 add-nodes）
 * - tier3-ha   kubeadm，unreachable（SSH 全部超时，无观测）
 * - lab-old    kubeadm，stale（快照超过一天，且有一个未声明节点 manual-node）
 * - lab-draft  k3s，    unknown（draft，从未采集）
 */
import { http, HttpResponse } from 'msw';
import type {
  ClusterDetail,
  ClusterSummary,
  Meta,
  OperationList,
  OperationSummary,
} from '../api/types';

const now = '2026-10-27T08:00:00.000Z';
const anHourAgo = '2026-10-27T07:00:00.000Z';
const yesterday = '2026-10-26T01:00:00.000Z';

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

/* --------------------------------- 操作 ---------------------------------- */

const succeededPrecheck: OperationSummary = {
  id: '01JDY8Z9X7M2Q3R4S5T6U7V8WA',
  cluster: 'tier1',
  kind: 'precheck',
  state: 'succeeded',
  params: {},
  actor: 'session-1',
  error: null,
  createdAt: anHourAgo,
  startedAt: anHourAgo,
  finishedAt: anHourAgo,
  logLines: 42,
};

const runningAddNodes: OperationSummary = {
  id: '01JDY8Z9X7M2Q3R4S5T6U7V8WB',
  cluster: 'tier2-edge',
  kind: 'add-nodes',
  state: 'running',
  params: { nodes: ['edge-worker-2'] },
  actor: 'session-1',
  error: null,
  createdAt: now,
  startedAt: now,
  finishedAt: null,
  logLines: 128,
};

const succeededCertsRenew: OperationSummary = {
  id: '01JDY8Z9X7M2Q3R4S5T6U7V8WC',
  cluster: 'tier2-edge',
  kind: 'certs-renew',
  state: 'succeeded',
  params: {},
  actor: 'cli',
  error: null,
  createdAt: yesterday,
  startedAt: yesterday,
  finishedAt: yesterday,
  logLines: 87,
};

export const mockOperations: OperationSummary[] = [
  succeededPrecheck,
  runningAddNodes,
  succeededCertsRenew,
];

/* --------------------------------- 集群 ---------------------------------- */

export const mockClusters: ClusterSummary[] = [
  {
    name: 'tier1',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.33.1',
    lifecycle: 'active',
    currentRev: 3,
    nodesReady: 4,
    nodesTotal: 4,
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
    activeOperation: runningAddNodes,
  },
  {
    name: 'tier3-ha',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.31.8',
    lifecycle: 'active',
    currentRev: 5,
    nodesReady: null,
    nodesTotal: null,
    health: 'unreachable',
    certMinDays: null,
    observedAt: null,
    collectError: '所有控制面节点 SSH 连接失败：dial tcp 10.0.1.11:22: i/o timeout',
    activeOperation: null,
  },
  {
    name: 'lab-old',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.30.5',
    lifecycle: 'active',
    currentRev: 2,
    nodesReady: 3,
    nodesTotal: 3,
    health: 'stale',
    certMinDays: 45,
    observedAt: yesterday,
    collectError: null,
    activeOperation: null,
  },
  {
    name: 'lab-draft',
    distribution: 'k3s',
    kubernetesVersion: 'v1.33.1+k3s1',
    lifecycle: 'draft',
    currentRev: 1,
    nodesReady: null,
    nodesTotal: null,
    health: 'unknown',
    certMinDays: null,
    observedAt: null,
    collectError: null,
    activeOperation: null,
  },
];

const mockClusterDetails: Record<string, ClusterDetail> = {
  tier1: {
    name: 'tier1',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.33.1',
    lifecycle: 'active',
    currentRev: 3,
    createdAt: '2026-09-01T08:00:00.000Z',
    updatedAt: '2026-10-20T08:00:00.000Z',
    desiredNodes: [
      { name: 'cp-1', address: '10.0.0.11', role: 'control-plane' },
      { name: 'cp-2', address: '10.0.0.12', role: 'control-plane' },
      { name: 'cp-3', address: '10.0.0.13', role: 'control-plane' },
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
      {
        name: 'cp-2',
        ready: true,
        roles: 'control-plane',
        version: 'v1.33.1',
        osImage: 'Ubuntu 24.04.1 LTS',
        arch: 'amd64',
        cpuCores: 4,
        memoryMb: 8192,
      },
      {
        name: 'cp-3',
        ready: true,
        roles: 'control-plane',
        version: 'v1.33.1',
        osImage: 'Ubuntu 24.04.1 LTS',
        arch: 'amd64',
        cpuCores: 4,
        memoryMb: 8192,
      },
      {
        name: 'worker-1',
        ready: true,
        roles: '<none>',
        version: 'v1.33.1',
        osImage: 'Ubuntu 24.04.1 LTS',
        arch: 'amd64',
        cpuCores: 8,
        memoryMb: 16384,
      },
    ],
    snapshot: {
      attemptedAt: now,
      observedAt: now,
      nodesReady: 4,
      nodesTotal: 4,
      podsRunning: 86,
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
      kubernetesVersion: 'v1.33.1',
      runtime: 'containerd',
      zone: 'global',
      cni: 'calico',
      podCidr: '10.244.0.0/16',
      serviceCidr: '10.96.0.0/12',
      haMode: 'kube-vip',
      vip: '10.0.0.100',
    },
  },
  'tier2-edge': {
    name: 'tier2-edge',
    distribution: 'k3s',
    kubernetesVersion: 'v1.33.1+k3s1',
    lifecycle: 'active',
    currentRev: 1,
    createdAt: '2026-10-10T08:00:00.000Z',
    updatedAt: '2026-10-10T08:00:00.000Z',
    // edge-worker-2 在配置里，但快照从未观测到（期望缺失）
    desiredNodes: [
      { name: 'edge-cp', address: '10.1.0.11', role: 'control-plane' },
      { name: 'edge-worker-1', address: '10.1.0.21', role: 'worker' },
      { name: 'edge-worker-2', address: '10.1.0.22', role: 'worker' },
    ],
    observedNodes: [
      {
        name: 'edge-cp',
        ready: true,
        roles: 'control-plane,master',
        version: 'v1.33.1+k3s1',
        osImage: 'Debian GNU/Linux 12 (bookworm)',
        arch: 'arm64',
        cpuCores: 4,
        memoryMb: 8192,
      },
      {
        name: 'edge-worker-1',
        ready: false,
        roles: '<none>',
        version: 'v1.33.1+k3s1',
        osImage: 'Debian GNU/Linux 12 (bookworm)',
        arch: 'arm64',
        cpuCores: 4,
        memoryMb: 8192,
      },
    ],
    snapshot: {
      attemptedAt: now,
      observedAt: now,
      nodesReady: 1,
      nodesTotal: 2,
      podsRunning: 23,
      podsFailed: 2,
      podsNotRunning: 3,
      etcdHealth: 'ok',
      certMinDays: 90,
      error: null,
    },
    activeOperation: runningAddNodes,
    collectError: null,
    configSummary: {
      distribution: 'k3s',
      kubernetesVersion: 'v1.33.1+k3s1',
      runtime: 'containerd',
      zone: 'cn',
      cni: 'flannel',
      podCidr: '10.42.0.0/16',
      serviceCidr: '10.43.0.0/16',
      haMode: 'none',
      vip: null,
    },
  },
  'tier3-ha': {
    name: 'tier3-ha',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.31.8',
    lifecycle: 'active',
    currentRev: 5,
    createdAt: '2026-08-15T08:00:00.000Z',
    updatedAt: '2026-10-05T08:00:00.000Z',
    desiredNodes: [
      { name: 'lb-1', address: '10.0.1.10', role: 'lb' },
      { name: 'cp-1', address: '10.0.1.11', role: 'control-plane' },
      { name: 'cp-2', address: '10.0.1.12', role: 'control-plane' },
      { name: 'cp-3', address: '10.0.1.13', role: 'control-plane' },
      { name: 'worker-1', address: '10.0.1.21', role: 'worker' },
    ],
    observedNodes: [],
    snapshot: {
      attemptedAt: now,
      observedAt: null,
      nodesReady: 0,
      nodesTotal: 0,
      error: 'SSH 连接超时（所有节点）',
    },
    activeOperation: null,
    collectError: '所有控制面节点 SSH 连接失败：dial tcp 10.0.1.11:22: i/o timeout',
    configSummary: {
      distribution: 'kubeadm',
      kubernetesVersion: 'v1.31.8',
      runtime: 'containerd',
      zone: 'global',
      cni: 'calico',
      podCidr: '10.244.0.0/16',
      serviceCidr: '10.96.0.0/12',
      haMode: 'kurin-lb',
      vip: null,
    },
  },
  'lab-old': {
    name: 'lab-old',
    distribution: 'kubeadm',
    kubernetesVersion: 'v1.30.5',
    lifecycle: 'active',
    currentRev: 2,
    createdAt: '2026-06-01T08:00:00.000Z',
    updatedAt: '2026-09-01T08:00:00.000Z',
    // manual-node 被手工 kubeadm join，未出现在配置里（未声明）
    desiredNodes: [
      { name: 'cp-1', address: '192.168.10.11', role: 'control-plane' },
      { name: 'worker-1', address: '192.168.10.21', role: 'worker' },
    ],
    observedNodes: [
      {
        name: 'cp-1',
        ready: true,
        roles: 'control-plane',
        version: 'v1.30.5',
        osImage: 'Rocky Linux 9.4 (Blue Onyx)',
        arch: 'amd64',
        cpuCores: 4,
        memoryMb: 8192,
      },
      {
        name: 'worker-1',
        ready: true,
        roles: '<none>',
        version: 'v1.30.5',
        osImage: 'Rocky Linux 9.4 (Blue Onyx)',
        arch: 'amd64',
        cpuCores: 4,
        memoryMb: 8192,
      },
      {
        name: 'manual-node',
        ready: true,
        roles: '<none>',
        version: 'v1.30.5',
        osImage: 'Rocky Linux 9.4 (Blue Onyx)',
        arch: 'amd64',
        cpuCores: 2,
        memoryMb: 4096,
      },
    ],
    snapshot: {
      attemptedAt: yesterday,
      observedAt: yesterday,
      nodesReady: 3,
      nodesTotal: 3,
      podsRunning: 41,
      podsFailed: 0,
      podsNotRunning: 1,
      etcdHealth: 'ok',
      certMinDays: 45,
      error: null,
    },
    activeOperation: null,
    collectError: null,
    configSummary: {
      distribution: 'kubeadm',
      kubernetesVersion: 'v1.30.5',
      runtime: 'cri-o',
      zone: 'global',
      cni: 'cilium',
      podCidr: '10.244.0.0/16',
      serviceCidr: '10.96.0.0/12',
      haMode: 'none',
      vip: null,
    },
  },
  'lab-draft': {
    name: 'lab-draft',
    distribution: 'k3s',
    kubernetesVersion: 'v1.33.1+k3s1',
    lifecycle: 'draft',
    currentRev: 1,
    createdAt: '2026-10-26T08:00:00.000Z',
    updatedAt: '2026-10-26T08:00:00.000Z',
    desiredNodes: [{ name: 'edge-1', address: '10.9.0.11', role: 'control-plane' }],
    observedNodes: [],
    snapshot: null,
    activeOperation: null,
    collectError: null,
    configSummary: {
      distribution: 'k3s',
      kubernetesVersion: 'v1.33.1+k3s1',
      runtime: 'containerd',
      zone: 'global',
      cni: 'flannel',
      podCidr: '10.42.0.0/16',
      serviceCidr: '10.43.0.0/16',
      haMode: 'none',
      vip: null,
    },
  },
};

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

function unauthorized() {
  return HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 });
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
      return unauthorized();
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
      return unauthorized();
    }
    return HttpResponse.json(mockMeta);
  }),

  // 集群列表
  http.get('/api/v1/clusters', () => {
    if (!sessionEstablished) {
      return unauthorized();
    }
    return HttpResponse.json(mockClusters);
  }),

  // 集群详情
  http.get('/api/v1/clusters/:name', ({ params }) => {
    if (!sessionEstablished) {
      return unauthorized();
    }
    const name = params['name'] as string;
    const detail = mockClusterDetails[name];
    if (!detail) {
      return HttpResponse.json(
        { code: 'not-found', message: `集群 ${name} 不存在` },
        { status: 404 },
      );
    }
    return HttpResponse.json(detail);
  }),

  // 操作列表
  http.get('/api/v1/operations', ({ request }) => {
    if (!sessionEstablished) {
      return unauthorized();
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
      return unauthorized();
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
