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

/**
 * `GET /meta` 能力表的一行（WP-54 `CapabilityRow`）：一个内置组件的支持面。
 * 前端只用它禁用选项并解释原因（UI §8.3），最终结论以 validate 为准。
 */
export interface CapabilityRow {
  /** 组件 id：`<kind>/<name>`（如 `cni/calico`、`distro/k3s`）。 */
  id: string;
  /** 所属层：cni / distro / runtime / ha / registry / addon / post。 */
  kind: string;
  /** 支持的发行版：`k8s` / `k3s`。 */
  distributions: string[];
  /** 有制品钉定的架构：amd64 / arm64。 */
  archs: string[];
  /** 离线就绪：`bundle` | `rejected` | `online-only`。 */
  offline: string;
  /** 可用下载区域。 */
  zones: string[];
  /** 参与的操作。 */
  ops: string[];
}

/** `GET /meta`：版本、组件版本表（`create config --print-versions`）、能力表。 */
export interface Meta {
  version: string;
  /** 组件版本表：组件名 → 版本（如 `kubernetes: v1.33.1`）。 */
  components: Record<string, string>;
  /** 能力表（UI §8.3）：每个内置组件一行，禁用选项的唯一依据。 */
  capabilities: CapabilityRow[];
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

/**
 * `GET /operations/{id}`：快照 + 进度聚合 + 事件游标（UI §7「状态 + 按阶段/节点聚合的进度」）。
 * WP-63 对临时 DTO 的扩展；列表项不需要的字段只在详情响应中出现。
 */
export type OperationDetail = OperationSummary & {
  /** 已持久化事件的最大 seq；历史分页（`/logs?after=`）与 SSE `?after=` 的起点。无事件为 0。 */
  lastSeq: number;
  /**
   * 计划内全部任务的当前状态（含未开始的 `pending`），按计划顺序。
   * 进度网格的初始状态；事件流在其上增量更新（乱序按状态等级合并，不降级）。
   * 老操作可缺失，此时退化为纯事件流聚合（没有「待执行」格子）。
   */
  progress?: OperationProgress;
  /**
   * 终态操作携带的结果载荷（WP-62 扩展）：precheck 报表、plan 的 PlanView 等。
   * 后端已把 result 存进 operations 表，序列化到 API 的字段由 WP-55 绑定收口。
   */
  result?: PrecheckReportResult | PlanResult | null;
};

/** `precheck` 操作的 `result` 载荷：逐节点检查行 + 计数。 */
export interface PrecheckReportResult {
  checks: Array<{
    node: string;
    check: string;
    /** ok / warn / fail。 */
    status: string;
    detail: string;
  }>;
  failed: number;
  warned: number;
}

/** 进度网格单元格状态（UI §8.1：待执行/执行中/跳过/完成/已核对/告警/失败）。 */
export type TaskCellState =
  | 'pending'
  | 'running'
  | 'skipped'
  | 'reconciled'
  | 'done'
  | 'warn'
  | 'failed';

/** 进度聚合里的一个任务（`GET /operations/{id}` 的 `progress.tasks` 项）。 */
export interface OperationTaskSnapshot {
  /** 任务 key：`<stage>/<kind>@<node>`（与事件里的 task id 同一命名）。 */
  id: string;
  node: string | null;
  stage: string;
  state: TaskCellState;
  /** 终态耗时（ms）。 */
  ms?: number;
  /** 跳过/失败/告警原因。 */
  reason?: string;
}

/** `GET /operations/{id}` 的进度聚合。 */
export interface OperationProgress {
  tasks: OperationTaskSnapshot[];
}

/** `POST /operations/{id}/cancel`：202 + 更新后的操作快照（状态变为 cancelled）。 */
export type CancelOperationResponse = OperationSummary;

/** `POST /operations/{id}/retry`：202 + 新创建的操作（`retryOf` 指向原操作）。 */
export type RetryOperationResponse = OperationSummary;

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

/**
 * 信封 body —— 现有 executor 事件原文（`kurin --json` 的 v1 形状，kebab-case 与
 * `crates/kurin-core/src/engine.rs` 的 `Event` 枚举一致：`task-skipped`/`task-failed`/
 * `task-reconciled`，`node`/`stage` 非 Option）+ v1.1 新增的 warn/log/resource。
 */
export type OperationEventBody =
  | { type: 'task-start'; id: string; node: string; stage: string }
  | { type: 'task-skipped'; id: string; node: string; stage: string; reason?: string }
  | { type: 'task-done'; id: string; node: string; stage: string; ms: number }
  | { type: 'task-failed'; id: string; node: string; stage: string; reason: string }
  | { type: 'task-reconciled'; id: string; node: string; stage: string }
  /** BestEffort 失败等：任务带告警结束（architecture/02 §3.10 `TaskWarn`）。 */
  | { type: 'task-warn'; id: string; node: string; stage: string; message: string }
  | { type: 'stage-done'; stage: string }
  | { type: 'cluster-ready' }
  | { type: 'step'; node: string; step: string; ok: boolean; detail: string }
  | { type: 'note'; level: LogLevel; message: string }
  | { type: 'state'; state: OperationState; error?: string }
  | {
      type: 'log';
      node?: string | null;
      stage?: string | null;
      /** 任务 key；远端输出（stdout/stderr）折叠在该任务组下（UI §8.5）。 */
      task?: string | null;
      level: LogLevel;
      stream: LogStream;
      line: string;
      elapsedMs?: number;
    }
  | {
      type: 'resource';
      stage: string;
      kind: string;
      name: string;
      namespace?: string | null;
      ready: boolean;
      /** 就绪状态文本（如 `1/2 ready`）。 */
      detail?: string;
    };

/** 日志级别（note/log 事件）。 */
export type LogLevel = 'debug' | 'info' | 'warn' | 'error';

/** 日志流：kurin 自身 / 远端命令 stdout / stderr。 */
export type LogStream = 'kurin' | 'stdout' | 'stderr';

/* --------------------- 日志分页查询 (§7 v1.1, WP-63) ---------------------- */

/** `GET /operations/{id}/logs` 查询参数（UI §7：分页与筛选）。 */
export interface OperationLogsQuery {
  /** 只返回 seq 严格大于该值的事件（升序游标）。 */
  after?: number;
  /** 页大小上限（默认 500，服务端封顶）。 */
  limit?: number;
  node?: string;
  stage?: string;
  task?: string;
  level?: LogLevel;
  stream?: LogStream;
  /** 全文搜索。 */
  q?: string;
}

/** `GET /operations/{id}/logs` 响应：按 seq 升序的事件页 + 当前最大 seq。 */
export interface OperationLogsPage {
  items: OperationEventEnvelope[];
  /** 服务端已持久化事件的最大 seq（判断是否到达末尾、SSE 续读起点）。 */
  lastSeq: number;
}

/** `GET /operations/{id}/logs.download?format=` 的下载格式（UI §8.5）。 */
export type LogDownloadFormat = 'log' | 'jsonl';

/* ------------------------------ PlanView (§7) ----------------------------- */

/**
 * `plan` 操作的 `result` 载荷（kurin-ops `PlanView`，snake_case）：
 * 阶段 → 任务，带稳定的任务 key 与可选的漂移标注。
 */
export interface PlanResult {
  /** 操作类型（create / certs-renew / …）。 */
  op: string;
  phases: Array<{
    /** 阶段 id（prepare / bootstrap / …）。 */
    id: string;
    /** 展示名。 */
    label: string;
    tasks: Array<{
      /** 稳定任务标识 `<stage>/<kind>@<node>`。 */
      key: string;
      node: string;
      describe: string;
      /** 只读漂移探测（`?check=true`）：`"unchanged"` 或 `{ "would-change": { reason } }`。 */
      drift?: unknown;
    }>;
  }>;
}

/* ----------------------------- Addon (§7, M2) ----------------------------- */

/**
 * `GET /addons/catalog` 的一行（WP-54 `CatalogAddon`，snake_case）。
 * 向导第 6 步的勾选列表与 values 参数表单都由它驱动。
 */
export interface AddonCatalogEntry {
  name: string;
  version: string;
  title?: string | null;
  description: string;
  /** 分类：monitoring / ingress / security / storage / networking / dashboard。 */
  category: string;
  icon?: string | null;
  namespace: string;
  /** 依赖的 addon 名（勾选时自动勾上，UI §8.2 第 6 步）。 */
  requires: string[];
  /** 冲突的 addon 名（互斥）。 */
  conflicts: string[];
  /** values JSON Schema（`{}` 表示不接受参数）。 */
  values_schema: Record<string, unknown>;
  /** 来源说明（`builtin …` / `dir …` / `package …`）。 */
  origin: string;
}

/** `GET /addons/catalog` 响应。 */
export interface AddonCatalogResponse {
  addons: AddonCatalogEntry[];
}

/** `GET /clusters/{name}/addons` 已装项。 */
export interface InstalledAddon {
  name: string;
  version: string;
  status: 'installed' | 'failed' | 'unknown';
  valuesSummary?: string;
}

/* ------------------------------ 包 (§7, M3) ------------------------------- */

/** `GET /packages` 的一行（WP-54 `PackageView`，snake_case）。 */
export interface PackageInfo {
  id: string;
  /** 包来源 URI（选中已导入包时，它就是要写进 DTO `packages` 的值）。 */
  source: string;
  /** 登记来源：imported / create-base。 */
  origin: string;
  /** base / addon / os / bundle / custom。 */
  kind: string;
  name: string;
  /** base 包钉定的 Kubernetes 版本。 */
  version: string;
  /** kubeadm / k3s。 */
  distribution: string;
  /** 覆盖的架构。 */
  arch: string[];
  /** 载荷总字节数。 */
  size: number;
  /** 索引摘要（sha256:…）。 */
  index: string;
  signatures: number;
  sbom?: string | null;
  created_at: Timestamp;
  verified_at: Timestamp;
}

/** `GET /packages` 响应。 */
export interface PackageListResponse {
  packages: PackageInfo[];
}

/* --------------------------- 创建向导 DTO (§8.2, WP-62) --------------------------- */

/**
 * 向导 DTO：`POST /configs/render` 的请求体（WP-54 `RenderBody`，snake_case）。
 * 与 CLI 快速参数共用同一套 `ConfigPatch` 降级逻辑；秘密只以引用出现
 * （env / 文件路径），明文密码从不进入 DTO。
 */
export interface WizardDto {
  name?: string;
  /** Kubernetes 版本（`v` 前缀可有可无）。 */
  kubernetes?: string;
  /** kubeadm / k3s。 */
  distribution?: string;
  /** containerd / crio。 */
  runtime?: string;
  /** calico / flannel / cilium / kube-ovn / none。 */
  cni?: string;
  pod_cidr?: string;
  service_cidr?: string;
  dns_domain?: string;
  vip?: string;
  /** kube-vip / kurin-lb / external-endpoint。 */
  ha_mode?: string;
  /** global / cn。 */
  zone?: string;
  /** 包来源 URI（base 与 os 包按 kind 归类）。 */
  packages?: string[];
  /** 兼容旧参数：裸目录 = 离线 bundle 目录；URI 归入 packages。 */
  offline?: string;
  /** `NAME` 或 `NAME=VERSION`。 */
  addons?: string[];
  /** `NAME.dotted.key=TOML 字面量`。 */
  addon_values?: string[];
  /** 任意 kebab-case 路径覆盖（registry.kind 等），最后应用。 */
  sets?: string[];
  /** role-index / hostname / ip。 */
  node_name?: string;
  ssh?: WizardSshDto;
  nodes?: WizardNodeDto[];
}

/** 向导 DTO 的集群级 SSH 段。 */
export interface WizardSshDto {
  user?: string;
  port?: number;
  /** 私钥路径（管理机路径）。 */
  key?: string;
  key_passphrase_env?: string;
  password_env?: string;
  password_file?: string;
  become?: boolean;
  become_password_env?: string;
  /** tofu / strict / accept-new。 */
  known_hosts?: string;
}

/** 向导 DTO 的一个节点。 */
export interface WizardNodeDto {
  name?: string;
  address: string;
  /** control-plane / worker / lb / registry。 */
  role?: string;
  internal_address?: string;
  /** 节点级覆盖：仅 user / port。 */
  user?: string;
  port?: number;
  labels?: Record<string, string>;
}

/** `POST /configs/render` 响应：规范 TOML + 一次顺带校验的诊断。 */
export interface RenderedConfig {
  toml: string;
  valid: boolean;
  diagnostics?: Diagnostic[];
}

/** `POST /nodes/parse` 请求体（批量录入）。 */
export interface ParseNodesRequest {
  input: string;
  /** 已占用的节点名（避免自动命名冲突）。 */
  existing?: string[];
}

/** `POST /nodes/parse` 的一行。 */
export interface ParsedNodeRow {
  name: string;
  address: string;
  /** controlplane / worker / lb / registry（注意 controlplane 无连字符）。 */
  role: string;
}

/** `POST /nodes/parse` 响应。 */
export interface ParsedNodesResponse {
  nodes: ParsedNodeRow[];
}

/**
 * `POST /nodes/probe` 请求体（逐节点「测试连接」）。
 * 凭据只接受引用：env 变量名 / 管理机上的文件路径。
 */
export interface ProbeRequest {
  address: string;
  user?: string;
  port?: number;
  key?: string;
  key_passphrase_env?: string;
  password_env?: string;
  password_file?: string;
  become?: boolean;
  become_password_env?: string;
  known_hosts?: string;
}

/** `POST /nodes/probe` 返回的 facts（回填节点表格；不含任何凭据材料）。 */
export interface ProbeFacts {
  arch: string;
  os: string;
  os_version: string;
  kernel: string;
  cgroup_v2: boolean;
  init_systemd: boolean;
  cpus: number;
  /** MiB。 */
  mem_mib: number;
  /** 挂载点 → 空闲 MiB（`/`、`/var/lib`）。 */
  disk_free_mib: Record<string, number>;
  hostname: string;
  user: string;
  is_root: boolean;
  /** none / passwordless / needs-password / unknown。 */
  sudo: string;
  tcp_forwarding?: boolean | null;
  curl?: boolean | null;
  /** none / kubeadm / k3s。 */
  existing_install: string;
}

/** `POST /nodes/probe` 响应。 */
export interface ProbeResponse {
  facts: ProbeFacts;
}

/* ------------------------------- 会话 (§6.10) ------------------------------ */

/** `POST /session` 成功响应。 */
export interface SessionInfo {
  /** 会话过期时间（cookie 为 HttpOnly; SameSite=Strict，前端只拿元数据）。 */
  expiresAt: Timestamp;
  version: string;
}
