/**
 * 向导表单 → `POST /configs/render` DTO 的组装（WP-62）。
 *
 * 降级逻辑全部在服务端（WP-54 `build_patch`，与 CLI 快速参数同一条路径）；
 * 这里只做字段映射与勾选展开，保证「一个模型」：同样的 DTO 由 CLI 与
 * 向导共用。秘密只以引用（env 变量名 / 文件路径）出现。
 */
import type {
  AddonCatalogEntry,
  PackageInfo,
  WizardDto,
  WizardNodeDto,
  WizardSshDto,
} from '../api/types';
import type { NodeRow, WizardForm } from './model';

/** TOML 字面量序列化（`--set`/`--addon-set` 的值部分）。 */
export function tomlLiteral(value: unknown): string {
  if (typeof value === 'string') return JSON.stringify(value);
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  if (value == null) return '""';
  return JSON.stringify(JSON.stringify(value));
}

/** `--addon-set` 的点路径展开：`a.b=1` 写进 `addons.<name>.values.a.b`。 */
export function flattenAddonValues(prefix: string, value: unknown, out: string[]): void {
  if (value != null && typeof value === 'object' && !Array.isArray(value)) {
    for (const [key, child] of Object.entries(value)) {
      flattenAddonValues(prefix === '' ? key : `${prefix}.${key}`, child, out);
    }
    return;
  }
  out.push(`${prefix}=${tomlLiteral(value)}`);
}

/** 表单 → render DTO。 */
export function buildDto(
  form: WizardForm,
  catalog: AddonCatalogEntry[],
  selectedPackage: PackageInfo | null,
): WizardDto {
  const distributionApi = form.distribution === 'k3s' ? 'k3s' : 'kubeadm';
  const sets: string[] = [];
  // registry 选择 → sets（quick patch 只认 kebab-case 路径）
  if (form.registryChoice !== 'none') {
    sets.push(`registry.kind=${tomlLiteral(form.registryChoice)}`);
    if (form.registryChoice === 'external' && form.externalRegistryAddress !== '') {
      sets.push(`registry.address=${tomlLiteral(form.externalRegistryAddress)}`);
    }
  }
  // 包来源：已导入包 → source URI；URI 模式 → 原样
  const packages: string[] = [];
  if (form.packageMode === 'existing' && selectedPackage) {
    packages.push(selectedPackage.source);
  } else if (form.packageMode === 'uri' && form.packageUri.trim() !== '') {
    packages.push(form.packageUri.trim());
  }

  const addons: string[] = [];
  const addonValues: string[] = [];
  for (const entry of catalog) {
    const selection = form.addons[entry.name];
    if (selection == null) continue;
    addons.push(entry.name);
    const flat: string[] = [];
    flattenAddonValues('', selection.values, flat);
    for (const assignment of flat) {
      addonValues.push(`${entry.name}.${assignment}`);
    }
  }

  const nodes: WizardNodeDto[] = form.nodes.map((row) => nodeDto(row));
  const ssh: WizardSshDto | undefined = sshDto(form);
  // k3s 的 CNI 只支持 flannel | none；表单可能带着别的值进来（切换发行版），
  // 服务端会拒绝——这里先收紧到 flannel，避免无谓的 422 噪音。
  const cni = distributionApi === 'k3s' && form.cni !== 'flannel' && form.cni !== 'none' ? 'flannel' : form.cni;

  return {
    name: form.name.trim() || undefined,
    kubernetes: form.kubernetesVersion || undefined,
    distribution: distributionApi,
    runtime: form.runtime,
    cni: cni === '' ? undefined : cni,
    pod_cidr: form.podCidr || undefined,
    service_cidr: form.serviceCidr || undefined,
    dns_domain: form.dnsDomain || undefined,
    vip: form.haMode !== 'none' && form.vip !== '' ? form.vip : undefined,
    ha_mode: form.haMode === 'none' ? undefined : form.haMode,
    // `global` 是默认值；artifacts.zone 只接受 "cn"，不要把它写进文档。
    zone: form.zone === 'cn' ? 'cn' : undefined,
    packages: packages.length > 0 ? packages : undefined,
    addons: addons.length > 0 ? addons : undefined,
    addon_values: addonValues.length > 0 ? addonValues : undefined,
    sets: sets.length > 0 ? sets : undefined,
    ssh,
    nodes,
  };
}

/** 一行节点 → DTO（端口为空不传，交给默认值）。 */
function nodeDto(row: NodeRow): WizardNodeDto {
  const dto: WizardNodeDto = {
    name: row.name === '' ? undefined : row.name,
    address: row.address,
    role: row.role,
  };
  if (row.internalAddress !== '') dto.internal_address = row.internalAddress;
  if (row.user !== '') dto.user = row.user;
  const port = Number(row.port);
  if (row.port !== '' && Number.isInteger(port) && port > 0) dto.port = port;
  const labels = parseLabels(row.labels);
  if (Object.keys(labels).length > 0) dto.labels = labels;
  return dto;
}

/** `zone=a,env=prod` → `{ zone: 'a', env: 'prod' }`；非法片段忽略（validate 兜底）。 */
export function parseLabels(text: string): Record<string, string> {
  const labels: Record<string, string> = {};
  for (const pair of text.split(/[;,\s]+/).filter(Boolean)) {
    const eq = pair.indexOf('=');
    if (eq <= 0) continue;
    const key = pair.slice(0, eq);
    const value = pair.slice(eq + 1);
    if (key !== '' && value !== '') labels[key] = value;
  }
  return labels;
}

/** 第 4 步表单 → DTO 的 ssh 段。 */
function sshDto(form: WizardForm): WizardSshDto | undefined {
  const dto: WizardSshDto = {
    user: form.sshUser === '' ? undefined : form.sshUser,
    port: portOrUndefined(form.sshPort),
    become: form.become,
    known_hosts: form.knownHosts,
  };
  switch (form.authMethod) {
    case 'password-env':
      if (form.passwordEnv !== '') dto.password_env = form.passwordEnv.trim();
      break;
    case 'password-file':
      if (form.passwordFile !== '') dto.password_file = form.passwordFile.trim();
      break;
    default:
      if (form.keyPath !== '') {
        dto.key = form.keyPath.trim();
        if (form.keyPassphraseEnv !== '') {
          dto.key_passphrase_env = form.keyPassphraseEnv.trim();
        }
      }
      break;
  }
  if (form.become && form.becomePasswordEnv !== '') {
    dto.become_password_env = form.becomePasswordEnv.trim();
  }
  return dto;
}

/** 端口字符串 → 数字（空/非法返回 undefined）。 */
export function portOrUndefined(text: string): number | undefined {
  const port = Number(text);
  return text !== '' && Number.isInteger(port) && port > 0 && port <= 65535 ? port : undefined;
}

/** 单节点探测请求（第 3 步「测试连接」）：优先用行级覆盖，回退全局。 */
export function probeRequestFor(row: NodeRow, form: WizardForm) {
  return {
    address: row.address,
    user: row.user !== '' ? row.user : form.sshUser === '' ? undefined : form.sshUser,
    port: portOrUndefined(row.port) ?? portOrUndefined(form.sshPort),
    key: form.authMethod === 'key' && form.keyPath !== '' ? form.keyPath : undefined,
    key_passphrase_env:
      form.authMethod === 'key' && form.keyPassphraseEnv !== ''
        ? form.keyPassphraseEnv
        : undefined,
    password_env:
      form.authMethod === 'password-env' && form.passwordEnv !== '' ? form.passwordEnv : undefined,
    password_file:
      form.authMethod === 'password-file' && form.passwordFile !== ''
        ? form.passwordFile
        : undefined,
    become: form.become,
    become_password_env:
      form.become && form.becomePasswordEnv !== '' ? form.becomePasswordEnv : undefined,
    known_hosts: form.knownHosts,
  };
}

/** `POST /nodes/parse` 行（role 为 `controlplane` 无连字符）→ 表单 role。 */
export function parseRoleToForm(role: string): NodeRow['role'] {
  if (role.startsWith('controlplane') || role === 'cp') return 'control-plane';
  if (role === 'lb') return 'lb';
  if (role === 'registry') return 'registry';
  return 'worker';
}
