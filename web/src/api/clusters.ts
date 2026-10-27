/**
 * 集群 API 适配层（WP-66）：服务端 DTO 是 snake_case（WP-55 生成的
 * `ClusterView`/`ClusterDetail`），页面沿用 WP-61 的 camelCase 视图。
 * 所有字段映射集中在这里，避免每个页面各自猜形状。
 */
import { apiFetch } from './client';
import type {
  ClusterDetail,
  ClusterSnapshot,
  ClusterSummary,
  NodeSpec,
  ObservedNode,
  OperationSummary,
} from './types';

/** `GET /clusters` 的一行（服务端 `ClusterView`）。 */
interface ClusterViewApi {
  name: string;
  lifecycle: string;
  current_rev: number;
  created_at: string;
  updated_at: string;
  distribution: string;
  kubernetes_version: string;
  nodes_ready: number | null;
  nodes_total: number | null;
  health: string;
  cert_min_days: number | null;
  observed_at: string | null;
  collect_error: string | null;
  active_operation: string | null;
}

interface SnapshotApi {
  attempted_at: string;
  observed_at: string | null;
  report: {
    cert_min_days?: number | null;
    etcd_ok?: boolean;
    healthy?: boolean;
    pods_not_running?: unknown[];
    nodes?: Array<{
      name?: string;
      ready?: boolean;
      roles?: string;
      kubelet_version?: string;
      os_image?: string;
      arch?: string;
      cpu_cores?: number;
      memory_mb?: number;
    }>;
  } | null;
  error: string | null;
  certs_at?: string | null;
  cert_min_days: number | null;
}

interface DesiredNodeApi {
  name: string;
  address: string;
  role: string;
  internal_address?: string;
  arch?: string;
}

interface ObservedNodeApi {
  name: string;
  ready: boolean;
  roles: string;
  version?: string;
  os_image?: string;
  arch?: string;
  cpu_cores?: number;
  memory_mb?: number;
}

interface ConfigSummaryApi {
  distribution: string;
  kubernetes_version: string;
  runtime: string;
  zone?: string;
  cni: string;
  pod_cidr: string;
  service_cidr: string;
  ha_mode: string;
  vip?: string;
}

interface ClusterDetailApi extends ClusterViewApi {
  snapshot: SnapshotApi | null;
  revisions: Array<{ rev: number; sha256: string; created_at: string; note?: string | null }>;
  desired_nodes: DesiredNodeApi[];
  observed_nodes: ObservedNodeApi[];
  config_summary: ConfigSummaryApi;
}

/** 服务端只给活动操作的 id；详情页的横幅与门控只需要「有一个操作在跑」。 */
function activeOperation(id: string | null, cluster: string): OperationSummary | null {
  if (id == null) return null;
  return {
    id,
    cluster,
    kind: 'unknown',
    state: 'running',
    params: {},
    actor: '',
    createdAt: '',
  };
}

function snapshotOf(api: SnapshotApi | null, view: ClusterViewApi): ClusterSnapshot | null {
  if (api == null) return null;
  const report = api.report;
  const nodes = report?.nodes ?? [];
  const ready = nodes.filter((node) => node.ready === true).length;
  return {
    attemptedAt: api.attempted_at,
    observedAt: api.observed_at,
    nodesReady: view.nodes_ready ?? ready,
    nodesTotal: view.nodes_total ?? nodes.length,
    podsNotRunning: report?.pods_not_running?.length,
    etcdHealth: report == null ? undefined : report.etcd_ok ? 'ok' : 'degraded',
    certMinDays: api.cert_min_days ?? view.cert_min_days,
    error: api.error,
  };
}

function summaryOf(view: ClusterViewApi): ClusterSummary {
  return {
    name: view.name,
    distribution: view.distribution === 'k3s' ? 'k3s' : 'kubeadm',
    kubernetesVersion: view.kubernetes_version,
    lifecycle: view.lifecycle as ClusterSummary['lifecycle'],
    currentRev: view.current_rev,
    nodesReady: view.nodes_ready,
    nodesTotal: view.nodes_total,
    health: view.health as ClusterSummary['health'],
    certMinDays: view.cert_min_days,
    observedAt: view.observed_at,
    collectError: view.collect_error,
    activeOperation: activeOperation(view.active_operation, view.name),
  };
}

function desiredNodeOf(node: DesiredNodeApi): NodeSpec {
  return {
    name: node.name,
    address: node.address,
    internalAddress: node.internal_address,
    // 页面只展示 control-plane/worker/lb；registry 节点按 worker 展示。
    role: (node.role === 'registry' ? 'worker' : node.role) as NodeSpec['role'],
  };
}

function observedNodeOf(node: ObservedNodeApi): ObservedNode {
  return {
    name: node.name,
    ready: node.ready,
    roles: node.roles,
    version: node.version ?? '',
    osImage: node.os_image,
    arch: node.arch,
    cpuCores: node.cpu_cores,
    memoryMb: node.memory_mb,
  };
}

function detailOf(api: ClusterDetailApi): ClusterDetail {
  const summary = summaryOf(api);
  const config = api.config_summary;
  return {
    ...summary,
    createdAt: api.created_at,
    updatedAt: api.updated_at,
    desiredNodes: (api.desired_nodes ?? []).map(desiredNodeOf),
    observedNodes: (api.observed_nodes ?? []).map(observedNodeOf),
    snapshot: snapshotOf(api.snapshot, api),
    configSummary: {
      distribution: config.distribution === 'k3s' ? 'k3s' : 'kubeadm',
      kubernetesVersion: config.kubernetes_version,
      runtime: config.runtime === 'cri-o' ? 'cri-o' : 'containerd',
      zone: config.zone === 'cn' ? 'cn' : 'global',
      cni: config.cni,
      podCidr: config.pod_cidr,
      serviceCidr: config.service_cidr,
      haMode:
        config.ha_mode === 'kurin-lb'
          ? 'kurin-lb'
          : config.ha_mode === 'external-endpoint'
            ? 'none'
            : 'kube-vip',
      vip: config.vip,
    },
  };
}

/** `GET /clusters`：服务端 `{clusters: [...]}` → 页面数组。 */
export async function fetchClusters(signal?: AbortSignal): Promise<ClusterSummary[]> {
  const body = await apiFetch<{ clusters: ClusterViewApi[] } | ClusterSummary[]>('/clusters', {
    signal,
  });
  // MSW 的旧夹具直接返回数组；真实服务端返回 `{clusters}`。
  return Array.isArray(body) ? body : (body.clusters ?? []).map(summaryOf);
}

/** `GET /clusters/{name}`：snake_case 详情 → 页面 camelCase。 */
export async function fetchClusterDetail(
  name: string,
  signal?: AbortSignal,
): Promise<ClusterDetail> {
  const body = await apiFetch<ClusterDetailApi | ClusterDetail>(
    `/clusters/${encodeURIComponent(name)}`,
    { signal },
  );
  // MSW 的旧夹具已是 camelCase 视图；真实服务端是 snake_case。
  if ('desiredNodes' in body) return body;
  return detailOf(body);
}
