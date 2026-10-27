/**
 * 创建向导的状态模型（WP-62，UI §8.2 的 9 步）。
 *
 * 纯数据与纯函数：模板预填、步骤定义、每步的前进条件。
 * render/validate 一律走服务端（`api/wizard.ts`），这里不复刻校验逻辑，
 * 只做「没填完不让走」的最小本地门控。
 */
import type { AddonCatalogEntry, CapabilityRow, PackageInfo } from '../api/types';
import type { MessageKey } from '../i18n';

/* --------------------------------- 步骤 --------------------------------- */

/** 向导步骤（0 基）。 */
export const STEPS = [
  'template',
  'basic',
  'nodes',
  'auth',
  'network',
  'components',
  'offline',
  'review',
  'confirm',
] as const;

export type WizardStep = (typeof STEPS)[number];
export type WizardStepIndex = number;

/** 步骤标题（i18n 键）。 */
export const STEP_TITLE_KEYS: Record<WizardStep, MessageKey> = {
  template: 'wizard.step.template',
  basic: 'wizard.step.basic',
  nodes: 'wizard.step.nodes',
  auth: 'wizard.step.auth',
  network: 'wizard.step.network',
  components: 'wizard.step.components',
  offline: 'wizard.step.offline',
  review: 'wizard.step.review',
  confirm: 'wizard.step.confirm',
};

/* --------------------------------- 模板 --------------------------------- */

/** 内置模板 id。 */
export type TemplateId = 'single' | 'ha-kubevip' | 'ha-kurinlb' | 'k3s-edge' | 'blank';

/** 模板预填（UI §8.2 第 1 步：「选模板会预填后续步骤，随时可改」）。 */
export interface ClusterTemplate {
  id: TemplateId;
  titleKey: MessageKey;
  descriptionKey: MessageKey;
  /** 预填的表单字段（浅合并进 draft）。 */
  patch: Partial<WizardForm>;
}

/** 单节点体验。 */
const single: ClusterTemplate = {
  id: 'single',
  titleKey: 'wizard.template.single.title',
  descriptionKey: 'wizard.template.single.description',
  patch: {
    distribution: 'kubeadm',
    runtime: 'containerd',
    cni: 'calico',
    haMode: 'none',
  },
};

/** 3 主 HA（kube-vip）。 */
const haKubeVip: ClusterTemplate = {
  id: 'ha-kubevip',
  titleKey: 'wizard.template.haKubeVip.title',
  descriptionKey: 'wizard.template.haKubeVip.description',
  patch: {
    distribution: 'kubeadm',
    runtime: 'containerd',
    cni: 'calico',
    haMode: 'kube-vip',
    controlPlaneCount: 3,
    workerCount: 0,
  },
};

/** 3 主 + LB 节点（kurin-lb）。 */
const haKurinLb: ClusterTemplate = {
  id: 'ha-kurinlb',
  titleKey: 'wizard.template.haKurinLb.title',
  descriptionKey: 'wizard.template.haKurinLb.description',
  patch: {
    distribution: 'kubeadm',
    runtime: 'containerd',
    cni: 'calico',
    haMode: 'kurin-lb',
    controlPlaneCount: 3,
    lbCount: 1,
    workerCount: 0,
  },
};

/** k3s 边缘。 */
const k3sEdge: ClusterTemplate = {
  id: 'k3s-edge',
  titleKey: 'wizard.template.k3sEdge.title',
  descriptionKey: 'wizard.template.k3sEdge.description',
  patch: {
    distribution: 'k3s',
    runtime: 'containerd',
    cni: 'flannel',
    haMode: 'kube-vip',
    controlPlaneCount: 1,
    workerCount: 2,
  },
};

/** 空白。 */
const blank: ClusterTemplate = {
  id: 'blank',
  titleKey: 'wizard.template.blank.title',
  descriptionKey: 'wizard.template.blank.description',
  patch: {},
};

/** 内置模板（顺序即展示顺序）。 */
export const TEMPLATES: ClusterTemplate[] = [single, haKubeVip, haKurinLb, k3sEdge, blank];

/** 模板的节点数量建议：选模板后给出可一键展开的占位行数。 */
export const TEMPLATE_NODE_PLAN: Record<TemplateId, { controlPlane: number; worker: number; lb: number; registry: number }> = {
  single: { controlPlane: 1, worker: 0, lb: 0, registry: 0 },
  'ha-kubevip': { controlPlane: 3, worker: 0, lb: 0, registry: 0 },
  'ha-kurinlb': { controlPlane: 3, worker: 0, lb: 1, registry: 0 },
  'k3s-edge': { controlPlane: 1, worker: 2, lb: 0, registry: 0 },
  blank: { controlPlane: 0, worker: 0, lb: 0, registry: 0 },
};

/* --------------------------------- 表单 --------------------------------- */

/** 第 3 步的一行节点。 */
export interface NodeRow {
  /** 表内唯一行 id（与节点名无关）。 */
  key: string;
  name: string;
  address: string;
  internalAddress: string;
  role: 'control-plane' | 'worker' | 'lb' | 'registry';
  /** 节点级覆盖（仅 user/port，UI §8.2 第 3 步）。 */
  user: string;
  port: string;
  labels: string;
  /** 连通性测试回填的 facts；探测失败时 error 非空。 */
  probe?: {
    state: 'ok' | 'failed' | 'probing';
    error?: string;
    arch?: string;
    os?: string;
    osVersion?: string;
    cpus?: number;
    memMib?: number;
    diskRootMib?: number;
    sudo?: string;
    isRoot?: boolean;
  };
}

/** 第 6 步一个 addon 的勾选状态。 */
export interface AddonSelection {
  name: string;
  /** rjsf 表单的当前值（`{}` 当无参数）。 */
  values: Record<string, unknown>;
}

/** 向导表单（渲染 DTO 前的原始状态；字符串宽松，转换为 DTO 时收紧）。 */
export interface WizardForm {
  /* 第 1 步 */
  template: TemplateId | null;
  /* 第 2 步 */
  name: string;
  distribution: 'kubeadm' | 'k3s';
  kubernetesVersion: string;
  runtime: 'containerd' | 'crio';
  zone: 'global' | 'cn';
  /* 模板用的节点数量建议（不直接进 DTO）。 */
  controlPlaneCount: number;
  workerCount: number;
  lbCount: number;
  /* 第 3 步 */
  nodes: NodeRow[];
  /* 第 4 步 */
  sshUser: string;
  sshPort: string;
  /** password-env / password-file / key。 */
  authMethod: 'password-env' | 'password-file' | 'key';
  /** 密码来源：env 变量名或管理机文件路径（值本身从不进入前端状态）。 */
  passwordEnv: string;
  passwordFile: string;
  keyPath: string;
  keyPassphraseEnv: string;
  become: boolean;
  becomePasswordEnv: string;
  knownHosts: 'tofu' | 'strict' | 'accept-new';
  /* 第 5 步 */
  podCidr: string;
  serviceCidr: string;
  dnsDomain: string;
  cni: string;
  haMode: string;
  vip: string;
  /* 第 6 步 */
  registryChoice: 'none' | 'zot' | 'docker-registry' | 'harbor' | 'external';
  externalRegistryAddress: string;
  addons: Record<string, AddonSelection>;
  /* 第 7 步 */
  packageMode: 'online' | 'existing' | 'uri';
  /** 已导入包的 id（`GET /packages`）。 */
  packageId: string | null;
  packageUri: string;
  /* 「高级：编辑 TOML」单向开关（第 9 步）。 */
  tomlMode: boolean;
}

/** 行 id 计数（模块级：一次向导会话内唯一即可）。 */
let rowSeq = 0;

/** 新建一行节点（key 自动分配；名称可留空，DTO 组装时交给服务端命名）。 */
export function buildNodeRow(init: Partial<NodeRow> = {}): NodeRow {
  rowSeq += 1;
  return {
    key: `node-${rowSeq}`,
    name: '',
    address: '',
    internalAddress: '',
    role: 'worker',
    user: '',
    port: '',
    labels: '',
    ...init,
  };
}

export const DEFAULT_POD_CIDR = '10.233.64.0/18';
export const DEFAULT_SERVICE_CIDR = '10.233.0.0/18';
export const DEFAULT_DNS_DOMAIN = 'cluster.local';

/** 空白表单（不含模板预填）。 */
export function emptyForm(): WizardForm {
  return {
    template: null,
    name: '',
    distribution: 'kubeadm',
    kubernetesVersion: '',
    runtime: 'containerd',
    zone: 'global',
    controlPlaneCount: 0,
    workerCount: 0,
    lbCount: 0,
    nodes: [],
    sshUser: 'root',
    sshPort: '22',
    authMethod: 'key',
    passwordEnv: '',
    passwordFile: '',
    keyPath: '',
    keyPassphraseEnv: '',
    become: false,
    becomePasswordEnv: '',
    knownHosts: 'tofu',
    podCidr: DEFAULT_POD_CIDR,
    serviceCidr: DEFAULT_SERVICE_CIDR,
    dnsDomain: DEFAULT_DNS_DOMAIN,
    cni: '',
    haMode: 'none',
    vip: '',
    registryChoice: 'none',
    externalRegistryAddress: '',
    addons: {},
    packageMode: 'online',
    packageId: null,
    packageUri: '',
    tomlMode: false,
  };
}

/** 应用模板预填（浅合并；不清空用户已改的字段——「随时可改」同样适用于反向）。 */
export function applyTemplate(form: WizardForm, template: ClusterTemplate): WizardForm {
  return { ...form, ...template.patch, template: template.id };
}

/* ------------------------------ 本地门控 ------------------------------ */

/** 第 2 步：名称为 DNS-1123 子域风格的粗检（最终结论在服务端 validate）。 */
export const NAME_PATTERN = /^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$/;

/** 每步的本地前进条件（服务端校验之外的纯输入完整性）。 */
export function stepBlocked(step: WizardStep, form: WizardForm): MessageKey | null {
  switch (step) {
    case 'template':
      return form.template == null ? 'wizard.blocked.template' : null;
    case 'basic':
      if (form.name === '' || !NAME_PATTERN.test(form.name)) return 'wizard.blocked.name';
      if (form.kubernetesVersion === '') return 'wizard.blocked.k8sVersion';
      return null;
    case 'nodes': {
      if (form.nodes.length === 0) return 'wizard.blocked.nodes';
      const bad = form.nodes.find((n) => n.address === '' || n.name === '');
      if (bad) return 'wizard.blocked.nodes';
      const names = form.nodes.map((n) => n.name);
      if (new Set(names).size !== names.length) return 'wizard.blocked.nodeNames';
      const addresses = form.nodes.map((n) => n.address);
      if (new Set(addresses).size !== addresses.length) return 'wizard.blocked.nodeAddresses';
      return null;
    }
    case 'auth': {
      if (form.sshUser === '') return 'wizard.blocked.sshUser';
      if (form.authMethod === 'password-env' && form.passwordEnv === '')
        return 'wizard.blocked.password';
      if (form.authMethod === 'password-file' && form.passwordFile === '')
        return 'wizard.blocked.password';
      if (form.authMethod === 'key' && form.keyPath === '') return 'wizard.blocked.key';
      return null;
    }
    case 'network':
      return form.cni === '' ? 'wizard.blocked.cni' : null;
    case 'components':
      if (form.registryChoice === 'external' && form.externalRegistryAddress === '')
        return 'wizard.blocked.registryAddress';
      return null;
    case 'offline':
      if (form.packageMode === 'existing' && form.packageId == null)
        return 'wizard.blocked.package';
      if (form.packageMode === 'uri' && form.packageUri === '') return 'wizard.blocked.package';
      return null;
    default:
      return null;
  }
}

/* ------------------------------ 能力约束 ------------------------------ */

/** `/meta` 能力表按 id 的索引。 */
export type CapabilityIndex = Record<string, CapabilityRow>;

export function indexCapabilities(rows: CapabilityRow[]): CapabilityIndex {
  const index: CapabilityIndex = {};
  for (const row of rows) index[row.id] = row;
  return index;
}

/** 发行版的表单拼写 ↔ 能力表拼写（`kubeadm` 是 UI 词，`k8s` 是后端词）。 */
const DISTRIBUTION_API: Record<string, string> = { kubeadm: 'k8s', k3s: 'k3s' };

/**
 * 某个组件选项在当前发行版下是否可用（UI §8.3：只用能力表禁用并给原因）。
 * 返回 null 表示可用，否则是原因文案键。
 */
export function componentBlocked(
  index: CapabilityIndex,
  componentId: string,
  distribution: string,
): MessageKey | null {
  const row = index[componentId];
  if (!row) return null;
  const api = DISTRIBUTION_API[distribution] ?? distribution;
  if (!row.distributions.includes(api)) return 'wizard.capability.distribution';
  return null;
}

/** CNI 选项列表（第 5 步的卡片）。 */
export const CNI_OPTIONS = [
  { value: 'calico', componentId: 'cni/calico', titleKey: 'wizard.cni.calico' as MessageKey },
  { value: 'flannel', componentId: 'cni/flannel', titleKey: 'wizard.cni.flannel' as MessageKey },
  { value: 'cilium', componentId: 'cni/cilium', titleKey: 'wizard.cni.cilium' as MessageKey },
  {
    value: 'kube-ovn',
    componentId: 'cni/kube-ovn',
    titleKey: 'wizard.cni.kubeOvn' as MessageKey,
  },
  { value: 'none', componentId: 'cni/none', titleKey: 'wizard.cni.none' as MessageKey },
] as const;

/** 运行时选项（k3s 固定 containerd，见 distro/k3s 校验）。 */
export const RUNTIME_OPTIONS = [
  { value: 'containerd', componentId: 'runtime/containerd' },
  { value: 'crio', componentId: 'runtime/crio' },
] as const;

/** HA 模式选项。 */
export const HA_OPTIONS = [
  { value: 'none', componentId: 'ha/external-endpoint' },
  { value: 'kube-vip', componentId: 'ha/kube-vip' },
  { value: 'kurin-lb', componentId: 'ha/kurin-lb' },
] as const;

/** registry 选项（none 不对应组件）。 */
export const REGISTRY_OPTIONS = [
  { value: 'none', componentId: null },
  { value: 'zot', componentId: 'registry/zot' },
  { value: 'docker-registry', componentId: 'registry/docker-registry' },
  { value: 'harbor', componentId: 'registry/harbor' },
  { value: 'external', componentId: 'registry/external' },
] as const;

/**
 * 计算某个选项是否应禁用：能力表说该发行版不支持，或该组件 offline-only
 * 而用户已选离线包（kube-ovn 离线不可用，UI §8.3 的例子）。
 */
export function optionDisabled(
  index: CapabilityIndex,
  componentId: string | null,
  distribution: string,
  offlineSelected: boolean,
): MessageKey | null {
  if (componentId == null) return null;
  const byDistribution = componentBlocked(index, componentId, distribution);
  if (byDistribution != null) return byDistribution;
  if (offlineSelected && index[componentId]?.offline === 'online-only') {
    return 'wizard.capability.offline';
  }
  return null;
}

/** 离线包覆盖性检查（UI §8.2 第 7 步）：缺什么列什么。 */
export interface PackageCoverageIssue {
  severity: 'error' | 'warn';
  messageKey: MessageKey;
  /** 插值 `{name}` / `{have}` / `{want}`。 */
  vars?: Record<string, string | number>;
}

/**
 * 检查选中的包是否覆盖当前选择（版本、发行版、架构、离线组件）。
 * 只做「已选包 vs 当前选择」的对照，是否可用最终仍由 validate 决定。
 */
export function checkPackageCoverage(
  pkg: PackageInfo | null,
  form: WizardForm,
  archs: string[],
): PackageCoverageIssue[] {
  if (pkg == null) return [];
  const issues: PackageCoverageIssue[] = [];
  const wantedVersion = form.kubernetesVersion.replace(/^v/, '');
  if (pkg.version !== '' && pkg.version !== wantedVersion) {
    issues.push({
      severity: 'error',
      messageKey: 'wizard.package.versionMismatch',
      vars: { have: pkg.version, want: wantedVersion },
    });
  }
  // 包清单用 UI 拼写（kubeadm/k3s），能力表才用 k8s。
  const wantedDistribution = form.distribution;
  if (pkg.distribution !== '' && pkg.distribution !== wantedDistribution) {
    issues.push({
      severity: 'error',
      messageKey: 'wizard.package.distributionMismatch',
      vars: { have: pkg.distribution, want: wantedDistribution },
    });
  }
  if (archs.length > 0) {
    const missing = archs.filter((a) => !pkg.arch.includes(a));
    if (missing.length > 0) {
      issues.push({
        severity: 'error',
        messageKey: 'wizard.package.archMismatch',
        vars: { arch: missing.join(', ') },
      });
    }
  }
  return issues;
}

/** addon 依赖闭包：勾选 name 时还需要勾上哪些（UI §8.2「依赖自动勾选并提示」）。 */
export function addonClosure(catalog: AddonCatalogEntry[], selected: string[]): Set<string> {
  const byName = new Map(catalog.map((a) => [a.name, a]));
  const needed = new Set<string>();
  const queue = [...selected];
  while (queue.length > 0) {
    const name = queue.pop() as string;
    if (needed.has(name)) continue;
    needed.add(name);
    for (const dep of byName.get(name)?.requires ?? []) {
      if (!needed.has(dep)) queue.push(dep);
    }
  }
  return needed;
}

/** 与已勾选集合冲突的 addon（conflicts 是双向的）。 */
export function addonConflictsWith(
  catalog: AddonCatalogEntry[],
  selected: string[],
  name: string,
): string[] {
  const byName = new Map(catalog.map((a) => [a.name, a]));
  const mine = byName.get(name)?.conflicts ?? [];
  const active = selected.filter((s) => s !== name);
  return mine.filter((m) => active.includes(m)).concat(
    active.filter((a) => (byName.get(a)?.conflicts ?? []).includes(name)),
  );
}
