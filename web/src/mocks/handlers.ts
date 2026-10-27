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
  components: {
    kubernetes: '1.33.1',
    containerd: '1.7.22',
    etcd: '3.5.16',
    'kubernetes.k3s': 'v1.33.1+k3s1',
  },
  // 与 kurin_components::component::capabilities_report() 同构（snake_case 行）
  capabilities: [
    { id: 'distro/kubeadm', kind: 'distro', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'distro/k3s', kind: 'distro', distributions: ['k3s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'runtime/containerd', kind: 'runtime', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'runtime/crio', kind: 'runtime', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'cni/calico', kind: 'cni', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'cni/flannel', kind: 'cni', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'cni/cilium', kind: 'cni', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'cni/kube-ovn', kind: 'cni', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'online-only', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'cni/none', kind: 'cni', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'ha/kube-vip', kind: 'ha', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'online-only', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'ha/kurin-lb', kind: 'ha', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'online-only', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'ha/external-endpoint', kind: 'ha', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'registry/zot', kind: 'registry', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'registry/docker-registry', kind: 'registry', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'registry/harbor', kind: 'registry', distributions: ['k8s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
    { id: 'registry/external', kind: 'registry', distributions: ['k8s', 'k3s'], archs: ['amd64', 'arm64'], offline: 'bundle', zones: ['global', 'cn'], ops: ['create'] },
  ],
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

/* ------------------------------ 创建向导（WP-62） ------------------------------ */

/** 向导目录：内置 addon 的镜像（values schema 同 addons/ 下的真实形状）。 */
export const mockAddonCatalog = [
  {
    name: 'metrics-server',
    version: '0.9.0',
    title: 'Metrics Server',
    description: '资源指标 API（metrics.k8s.io），kubectl top 与 HPA 依赖它',
    category: 'monitoring',
    icon: null,
    namespace: 'kube-system',
    requires: [],
    conflicts: [],
    values_schema: {
      type: 'object',
      additionalProperties: false,
      properties: {
        replicas: { type: 'integer', minimum: 1, maximum: 8, default: 1 },
      },
    },
    origin: 'builtin',
  },
  {
    name: 'ingress-nginx',
    version: '1.15.1',
    title: 'Ingress NGINX Controller',
    description: 'NGINX Ingress 控制器，通过 Ingress 暴露集群服务',
    category: 'ingress',
    icon: null,
    namespace: 'ingress-nginx',
    requires: [],
    conflicts: [],
    values_schema: {
      type: 'object',
      additionalProperties: false,
      properties: {
        controller: {
          type: 'object',
          additionalProperties: false,
          properties: {
            replicas: { type: 'integer', minimum: 1, maximum: 8, default: 1 },
          },
        },
      },
    },
    origin: 'builtin',
  },
  {
    name: 'local-path',
    version: '0.0.31',
    title: 'Local Path Provisioner',
    description: '用节点本地磁盘提供 StorageClass kurin-local-path 的轻量存储',
    category: 'storage',
    icon: null,
    namespace: 'local-path-storage',
    requires: [],
    conflicts: ['nfs-subdir'],
    values_schema: {},
    origin: 'builtin',
  },
  {
    name: 'nfs-subdir',
    version: '4.0.2',
    title: 'NFS subdir external provisioner',
    description: '对接外部 NFS 服务器的动态存储（StorageClass kurin-nfs）',
    category: 'storage',
    icon: null,
    namespace: 'default',
    requires: [],
    conflicts: ['local-path'],
    values_schema: {
      type: 'object',
      additionalProperties: false,
      properties: {
        server: { type: 'string', minLength: 1 },
        path: { type: 'string', minLength: 1 },
      },
    },
    origin: 'builtin',
  },
] as const;

/** 已登记的包源。 */
export const mockPackages = [
  {
    id: 'pkg-base-1.33.1',
    source: 'file:///media/usb/kurin-base-1.33.1.tar.zst',
    origin: 'imported',
    kind: 'base',
    name: 'base',
    version: '1.33.1',
    distribution: 'kubeadm',
    arch: ['amd64'],
    size: 1_234_567_890,
    index: 'sha256:deadbeef',
    signatures: 0,
    sbom: null,
    created_at: '2026-10-20T08:00:00.000Z',
    verified_at: '2026-10-20T08:01:00.000Z',
  },
] as const;

/** 向导提交状态（resetSessionState 清空，测试间互不泄漏）。 */
const mockRegistered = new Set<string>();
const mockConfigs = new Map<string, string>();
const mockRevs = new Map<string, number>();
const mockWizardOps = new Map<string, Record<string, unknown>>();
let mockOpSeq = 1;

/** 已建立会话的标记（测试与 dev 都从无 cookie 开始）。 */
let sessionEstablished = false;

export function resetSessionState(): void {
  sessionEstablished = false;
  mockRegistered.clear();
  mockConfigs.clear();
  mockRevs.clear();
  mockWizardOps.clear();
  mockOpSeq = 1;
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

  // 操作详情：WP-63 场景详情（含 progress/lastSeq）在 operationHandlers.ts 注册；
  // MSW 按「先注册先匹配」，这里只兜底 mockOperations 的摘要形状（无网格种子）。
  http.get('/api/v1/operations/:id', ({ params }) => {
    if (!sessionEstablished) {
      return unauthorized();
    }
    const id = params['id'] as string;
    const wizard = mockWizardOps.get(id);
    if (wizard) {
      return HttpResponse.json(wizard);
    }
    const op = mockOperations.find((o) => o.id === id);
    if (!op) {
      return HttpResponse.json(
        { code: 'not-found', message: `操作 ${id} 不存在` },
        { status: 404 },
      );
    }
    return HttpResponse.json(op);
  }),

  // ── 创建向导（WP-62）：render / validate / parse / probe / catalog / packages ──

  // POST /configs/render：把 DTO 组装成 v1alpha2 文档；诊断从 mockRenderDiagnostics 查。
  http.post('/api/v1/configs/render', async ({ request }) => {
    if (!sessionEstablished) return unauthorized();
    const body = (await request.json()) as {
      name?: string;
      nodes?: Array<{ name?: string; address: string; role?: string }>;
      vip?: string;
      pod_cidr?: string;
      service_cidr?: string;
      ssh?: { user?: string };
    };
    const diagnostics: Array<{ message: string; labels?: unknown[] }> = [];
    // 模拟服务端规则：节点地址落在 Pod CIDR 内时报错（前端不复刻校验）。
    if (body.pod_cidr && body.nodes?.some((n) => n.address.startsWith('10.233.'))) {
      diagnostics.push({
        message: `node address falls inside pod-cidr ${body.pod_cidr}`,
      });
    }
    const lines = [
      'version = "v1alpha2"',
      body.name ? `name = "${body.name}"` : '',
      '',
      '[kubernetes]',
      body.nodes?.some((n) => (n.role ?? 'worker') === 'control-plane')
        ? 'version = "1.33.1"'
        : '',
      '',
      '[ssh]',
      `user = "${body.ssh?.user ?? 'root'}"`,
      '',
      ...(body.nodes ?? []).map(
        (n) =>
          `[nodes.${n.name ?? 'node'}]\naddress = "${n.address}"\nrole = "${n.role ?? 'worker'}"\n`,
      ),
    ].filter(Boolean);
    return HttpResponse.json({
      toml: lines.join('\n'),
      valid: diagnostics.length === 0,
      diagnostics,
    });
  }),

  // POST /configs/validate：true / 422 + 诊断。
  http.post('/api/v1/configs/validate', async ({ request }) => {
    if (!sessionEstablished) return unauthorized();
    const body = (await request.json()) as { toml?: string };
    if ((body.toml ?? '').includes('[nodes.')) {
      return HttpResponse.json({ valid: true });
    }
    return HttpResponse.json(
      {
        code: 'validation-failed',
        message: '校验失败',
        diagnostics: [{ message: '需要至少一个 [nodes.*] 条目' }],
      },
      { status: 422 },
    );
  }),

  // POST /nodes/parse：与 CLI 同一解析器的 mock（末段范围展开）。
  http.post('/api/v1/nodes/parse', async ({ request }) => {
    if (!sessionEstablished) return unauthorized();
    const body = (await request.json()) as { input?: string };
    const rows: Array<{ name: string; address: string; role: string }> = [];
    for (const raw of (body.input ?? '').split('\n')) {
      const line = raw.split('#')[0].trim();
      if (line === '') continue;
      const [range, role = 'worker'] = line.split(/\s+/);
      const match = /^(\d+\.\d+\.\d+\.)(\d+)-(\d+)$/.exec(range);
      if (!match) {
        return HttpResponse.json(
          {
            code: 'validation-failed',
            message: '校验失败',
            diagnostics: [{ message: `invalid IP list: ${range}` }],
          },
          { status: 422 },
        );
      }
      const [, prefix, from, to] = match;
      for (let i = Number(from); i <= Number(to); i += 1) {
        rows.push({
          name: `${role === 'control-plane' || role === 'cp' ? 'cp' : role}-${rows.length + 1}`,
          address: `${prefix}${i}`,
          role: role === 'control-plane' || role === 'cp' ? 'controlplane' : role,
        });
      }
    }
    return HttpResponse.json({ nodes: rows });
  }),

  // POST /nodes/probe：192.0.2.* 不可达，其余返回 facts。
  http.post('/api/v1/nodes/probe', async ({ request }) => {
    if (!sessionEstablished) return unauthorized();
    const body = (await request.json()) as { address?: string };
    if ((body.address ?? '').startsWith('192.0.2.')) {
      return HttpResponse.json(
        { code: 'invalid-request', message: '探测失败：连接超时' },
        { status: 400 },
      );
    }
    return HttpResponse.json({
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
  }),

  // GET /addons/catalog：内置目录的镜像（含 values schema）。
  http.get('/api/v1/addons/catalog', () => {
    if (!sessionEstablished) return unauthorized();
    return HttpResponse.json({ addons: mockAddonCatalog });
  }),

  // GET /packages：已登记包。
  http.get('/api/v1/packages', () => {
    if (!sessionEstablished) return unauthorized();
    return HttpResponse.json({ packages: mockPackages });
  }),

  // POST /clusters：登记；mockRegistered 集合去重，重复登记 409。
  http.post('/api/v1/clusters', async ({ request }) => {
    if (!sessionEstablished) return unauthorized();
    const body = (await request.json()) as { name?: string; toml?: string };
    const name = body.name ?? '';
    if (mockRegistered.has(name)) {
      return HttpResponse.json(
        { code: 'conflict', message: `集群 ${name} 已有配置` },
        { status: 409 },
      );
    }
    mockRegistered.add(name);
    return HttpResponse.json(
      { cluster: { name, lifecycle: 'draft' }, rev: 1, sha256: 'deadbeef' },
      { status: 201 },
    );
  }),

  // GET /clusters/{name}/config：已登记集群返回最后提交的 TOML。
  http.get('/api/v1/clusters/:name/config', ({ params }) => {
    if (!sessionEstablished) return unauthorized();
    const name = params['name'] as string;
    const toml = mockConfigs.get(name);
    if (toml == null) {
      return HttpResponse.json(
        { code: 'conflict', message: `集群 ${name} 还没有配置` },
        { status: 409 },
      );
    }
    return HttpResponse.json({ rev: mockRevs.get(name) ?? 1, toml, sha256: 'deadbeef' });
  }),

  // PUT /clusters/{name}/config：追加修订。
  http.put('/api/v1/clusters/:name/config', async ({ request, params }) => {
    if (!sessionEstablished) return unauthorized();
    const name = params['name'] as string;
    const body = (await request.json()) as { toml?: string; base_rev?: number };
    mockConfigs.set(name, body.toml ?? '');
    mockRevs.set(name, (body.base_rev ?? 1) + 1);
    return HttpResponse.json({ rev: (body.base_rev ?? 1) + 1, sha256: 'deadbeef' });
  }),

  // POST /clusters/{name}/operations：202 + 操作（precheck / plan / create）。
  http.post('/api/v1/clusters/:name/operations', async ({ request, params }) => {
    if (!sessionEstablished) return unauthorized();
    const name = params['name'] as string;
    const body = (await request.json()) as { kind?: string };
    const id = `01JDYWIZARD${String(mockOpSeq++).padStart(6, '0')}`;
    const record = {
      id,
      cluster: name,
      kind: body.kind ?? 'create',
      state: 'succeeded',
      params: {},
      actor: 'mock',
      error: null,
      createdAt: '2026-10-27T08:00:00.000Z',
      startedAt: '2026-10-27T08:00:01.000Z',
      finishedAt: '2026-10-27T08:00:02.000Z',
      logLines: 0,
      result:
        body.kind === 'precheck'
          ? {
              checks: [
                { node: 'cp-1', check: 'ports', status: 'ok', detail: '' },
                { node: 'cp-1', check: 'sudo', status: 'ok', detail: '' },
              ],
              failed: 0,
              warned: 0,
            }
          : body.kind === 'plan'
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
    mockWizardOps.set(id, record);
    return HttpResponse.json(record, { status: 202 });
  }),

  // POST /clusters/{name}/plan：202 + 操作 id。
  http.post('/api/v1/clusters/:name/plan', ({ params }) => {
    if (!sessionEstablished) return unauthorized();
    const name = params['name'] as string;
    const id = `01JDYWIZARD${String(mockOpSeq++).padStart(6, '0')}`;
    mockWizardOps.set(id, {
      id,
      cluster: name,
      kind: 'plan',
      state: 'succeeded',
      params: {},
      actor: 'mock',
      error: null,
      createdAt: '2026-10-27T08:00:00.000Z',
      startedAt: '2026-10-27T08:00:01.000Z',
      finishedAt: '2026-10-27T08:00:02.000Z',
      logLines: 0,
      result: {
        op: 'create',
        phases: [
          {
            id: 'prepare',
            label: '准备',
            tasks: [{ key: 'prepare/sysctl@cp-1', node: 'cp-1', describe: '内核参数' }],
          },
        ],
      },
    });
    return HttpResponse.json({ operation: id, state: 'succeeded', cluster: name }, { status: 202 });
  }),
];
