/**
 * 临时 DTO —— 手写草稿，镜像 UI 文档 §7 的 API 形状。
 *
 * ⚠️ PROVISIONAL：这些类型将在 WP-55 由 ts-rs 从 `crates/kurin-server` 的
 * DTO 派生生成（`web/src/api/generated/`），届时本文件将被替换。
 * 字段歧义处均以 UI 文档 §7 的示例为准，见各类型的行内注释。
 */

/* ---------------------------------- 公共 ---------------------------------- */

/** 时间戳：UTC、毫秒精度（UI §6.8 事件信封的 `ts` 形状）。 */
export type Timestamp = string;

/** ULID（operations 表主键）。 */
export type OperationId = string;

/** 集群唯一键 = name（UI 文档 D7，v1 不支持改名）。 */
export type ClusterName = string;

/* ------------------------------ 错误信封 (§7) ----------------------------- */

/** `POST /configs/validate` 返回的诊断；labels 把错误映射回 TOML 行列（F11）。 */
export interface Diagnostic {
  message: string;
  help?: string;
  labels?: Array<{
    offset: number;
    len: number;
    text: string;
  }>;
}

/** 400/401/404/409/422/503 的统一错误体。 */
export interface ApiErrorEnvelope {
  /** 机读错误码，如 `invalid-request` / `unauthorized` / `not-found` / `conflict` / `validation-failed` / `unavailable`。 */
  code: string;
  /** 人类可读消息（中文）。 */
  message: string;
  /** 422：字段级诊断（含 span）；409：冲突原因（如正在运行的操作 ID）。 */
  diagnostics?: Diagnostic[];
}

/* ------------------------------- meta (§7) -------------------------------- */

/** `GET /meta`：版本、组件版本表（`create config --print-versions`）、能力表。 */
export interface Meta {
  version: string;
  /** 组件版本表：组件名 → 版本（如 `kubernetes: v1.33.1`）。 */
  componentVersions: Record<string, string>;
  /**
   * 能力表（UI §8.3）：`"<subject>:<constraint>"` → 原因。
   * 例如 `"cni:kube-ovn:offline": "离线不可用"`。前端只用它禁用选项并解释原因，
   * 不在 TS 复刻校验逻辑。
   */
  capabilities: Record<string, string>;
}

/* -------------------------------- 集群 (§7) ------------------------------- */

/** 集群生命周期（clusters 表 lifecycle 列）。 */
export type ClusterLifecycle =
  'draft' | 'provisioning' | 'active' | 'deleting' | 'deleted' | 'archived';

/** 健康徽标五态（UI §8.1：健康 / 异常 / 不可达 / 过期 / 无快照）。 */
export type ClusterHealth = 'healthy' | 'degraded' | 'unreachable' | 'stale' | 'unknown';

/** `GET /clusters` 列表项：集群 + 最近快照 + 当前活动操作。 */
export interface ClusterSummary {
  name: ClusterName;
  /** 发行版：kubeadm / k3s。 */
  distribution: 'kubeadm' | 'k3s';
  kubernetesVersion: string;
  lifecycle: ClusterLifecycle;
  /** 当前修订号（config_revisions.rev）。 */
  currentRev: number;
  /** 节点 Ready 计数（来自最近一次成功快照；无快照时为 null）。 */
  nodesReady: number | null;
  nodesTotal: number | null;
  /** 健康徽标状态（由快照派生，服务端计算）。 */
  health: ClusterHealth;
  /** 证书最早到期剩余天数（≥6 小时才刷新一次，可为 null）。 */
  certMinDays: number | null;
  /** 最近一次成功采集时间；不可达/过期判断的依据。 */
  observedAt: Timestamp | null;
  /** 最近一次失败原因（成功时为空）。 */
  collectError: string | null;
  /** 当前活动操作（queued/running 时存在）。 */
  activeOperation: OperationSummary | null;
}

/** `GET /clusters/{name}`：详情（配置摘要、节点期望 vs 观测、快照）。 */
export interface ClusterDetail {
  name: ClusterName;
  distribution: 'kubeadm' | 'k3s';
  kubernetesVersion: string;
  lifecycle: ClusterLifecycle;
  currentRev: number;
  createdAt: Timestamp;
  updatedAt: Timestamp;
  /** 期望节点（来自配置修订）。 */
  desiredNodes: NodeSpec[];
  /** 观测节点（来自最近快照；无快照为空数组）。 */
  observedNodes: ObservedNode[];
  snapshot: ClusterSnapshot | null;
  activeOperation: OperationSummary | null;
  /** 最近一次采集失败原因（成功时为 null；与 ClusterSummary.collectError 同源）。 */
  collectError: string | null;
  /** 配置摘要（配置 Tab 的只读表单视图；完整 TOML 走 `GET /clusters/{name}/config`）。 */
  configSummary: ClusterConfigSummary;
}

/**
 * `GET /clusters/{name}` 的配置摘要（UI §8.1 配置 Tab；与 §8.2 向导第 9 步的摘要同源）。
 * 只覆盖向导展示的字段，完整配置以 `GET /clusters/{name}/config` 的 TOML 为准。
 */
export interface ClusterConfigSummary {
  distribution: 'kubeadm' | 'k3s';
  kubernetesVersion: string;
  /** 容器运行时（k3s 固定 containerd）。 */
  runtime: 'containerd' | 'cri-o';
  /** 镜像区域。 */
  zone: 'global' | 'cn';
  /** CNI 名称（calico / flannel / cilium / kube-ovn / none…）。 */
  cni: string;
  podCidr: string;
  serviceCidr: string;
  /** 高可用模式（UI §8.2 第 5 步）。 */
  haMode: 'none' | 'kube-vip' | 'kurin-lb';
  /** kube-vip 模式下的 VIP。 */
  vip?: string | null;
}

/** 配置中的期望节点。 */
export interface NodeSpec {
  name: string;
  address: string;
  internalAddress?: string;
  role: 'control-plane' | 'worker' | 'lb';
  labels?: Record<string, string>;
}

/** 快照中观测到的节点。 */
export interface ObservedNode {
  name: string;
  ready: boolean;
  roles: string;
  version: string;
  osImage?: string;
  /** CPU 架构（WP-04 facts：amd64 / arm64），混合架构集群各节点不同。 */
  arch?: string;
  cpuCores?: number;
  memoryMb?: number;
}

/** Collector 采集的集群快照（snapshots 表 report 列的 JSON）。 */
export interface ClusterSnapshot {
  attemptedAt: Timestamp;
  observedAt: Timestamp | null;
  nodesReady: number;
  nodesTotal: number;
  podsRunning?: number;
  podsFailed?: number;
  /** 未处于 Running 状态的 Pod 数（UI §8.1 详情概览「未运行 Pod」）。 */
  podsNotRunning?: number;
  etcdHealth?: 'ok' | 'degraded' | 'unreachable';
  certMinDays?: number | null;
  error?: string | null;
}

/* -------------------------------- 操作 (§7) ------------------------------- */

/** 操作类型（operations 表 kind 列）。 */
export type OperationKind =
  | 'create'
  | 'add-nodes'
  | 'delete-node'
  | 'delete-cluster'
  | 'upgrade'
  | 'certs-renew'
  | 'precheck'
  | 'artifact'
  | 'package';

/** 操作状态（operations 表 state 列）。 */
export type OperationState =
  'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'interrupted';

/** `GET /operations` 列表项与 `GET /operations/{id}` 快照。 */
export interface OperationSummary {
  id: OperationId;
  cluster: ClusterName | null;
  kind: OperationKind;
  state: OperationState;
  configRev?: number;
  /** 操作参数 JSON（目标版本、节点名、batch…）。 */
  params: Record<string, unknown>;
  retryOf?: OperationId | null;
  actor: string;
  error?: string | null;
  createdAt: Timestamp;
  startedAt?: Timestamp | null;
  finishedAt?: Timestamp | null;
  /** 日志文件行数（v1.1：日志行存 JSONL 文件）。 */
  logLines?: number;
}

/** `GET /operations?cluster=&state=&kind=` 分页信封。 */
export interface OperationList {
  items: OperationSummary[];
  total: number;
  page: number;
  pageSize: number;
}

/* ------------------------- 事件信封 (§6.8, D12) --------------------------- */

/**
 * SSE 事件信封：现有 `--json` 的 `Event` 不改动（F13），Web 事件外包一层信封（D12）。
 * 结构化事件（task/step/note/state）存 `op_events`，日志行（log）存 JSONL 文件，
 * 两者共用同一 `seq` 序列。
 */
export interface OperationEventEnvelope {
  /** 单调递增序号；SSE `Last-Event-ID` / `?after=` 的依据。 */
  seq: number;
  /** UTC 毫秒时间戳。 */
  ts: Timestamp;
  kind: 'task' | 'step' | 'note' | 'state' | 'log' | 'resource';
  payload: OperationEventBody;
}

/** 信封 body —— 现有 executor 事件原文或 v1.1 新增的 step/log/resource。 */
export type OperationEventBody =
  | {
      type: 'task-start' | 'task-done' | 'task-skip' | 'task-fail';
      id: string;
      node?: string;
      stage?: string;
      ms?: number;
    }
  | { type: 'step'; node: string; step: string; ok: boolean; detail: string }
  | { type: 'note'; level: 'debug' | 'info' | 'warn' | 'error'; message: string }
  | { type: 'state'; state: OperationState; error?: string }
  | {
      type: 'log';
      node?: string | null;
      stage?: string | null;
      task?: string | null;
      level: 'debug' | 'info' | 'warn' | 'error';
      stream: 'kurin' | 'stdout' | 'stderr';
      line: string;
      elapsedMs?: number;
    }
  | {
      type: 'resource';
      stage: string;
      kind: string;
      name: string;
      ready: boolean;
      detail?: string;
    };

/* ------------------------------ PlanView (§7) ----------------------------- */

/** `POST /clusters/{name}/plan` 返回的计划视图（F6：`Plan` 不可序列化）。 */
export interface PlanView {
  stages: Array<{
    stage: string;
    tasks: Array<{
      /** 任务 key：`<stage>/<kind>@<node>`。 */
      id: string;
      node?: string | null;
      describe: string;
      /** 只读漂移探测（`?check=true`）：如 `would-change: net.ipv4.ip_forward=0`。 */
      drift?: string;
    }>;
  }>;
}

/* ----------------------------- Addon (§7, M2) ----------------------------- */

/** `GET /addons/catalog` 目录项。 */
export interface AddonCatalogEntry {
  name: string;
  category: string;
  version: string;
  /** 离线支持标记（UI §8.2 第 6 步）。 */
  offline: boolean;
  /** values JSON Schema（rjsf 参数表单来源）。 */
  valuesSchema?: Record<string, unknown>;
  /** 依赖的 addon 名（自动勾选）。 */
  requires?: string[];
  /** 兼容性说明：不兼容时置灰并显示原因（UI §8.1 addon 页）。 */
  incompatibleWith?: string[];
}

/** `GET /clusters/{name}/addons` 已装项。 */
export interface InstalledAddon {
  name: string;
  version: string;
  status: 'installed' | 'failed' | 'unknown';
  valuesSummary?: string;
}

/* ------------------------------ 包 (§7, M3) ------------------------------- */

/** `GET /packages` 包条目。 */
export interface PackageInfo {
  id: string;
  kind: 'base' | 'kubernetes' | 'addon' | 'image';
  name: string;
  version: string;
  arch: 'amd64' | 'arm64';
  sizeBytes: number;
  /** 签名状态。 */
  signed: boolean;
  verified?: boolean;
  sbomUrl?: string;
  importedAt: Timestamp;
}

/* ------------------------------- 会话 (§6.10) ------------------------------ */

/** `POST /session` 成功响应。 */
export interface SessionInfo {
  /** 会话过期时间（cookie 为 HttpOnly; SameSite=Strict，前端只拿元数据）。 */
  expiresAt: Timestamp;
  version: string;
}
