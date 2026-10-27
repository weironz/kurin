/**
 * Addon 管理 API（WP-64，UI §8.1）：已安装清单、安装/升级/卸载，以及
 * 集群详情「危险区」的删除节点 / 销毁集群 / 移除纳管。
 *
 * 服务端契约（§7 v1.1）：
 * - `GET /clusters/{name}/addons` → `{cluster, addons, note}`，`addons` 是
 *   按名字索引的 JSON 对象（`version` / `status` / `values` 可能缺失）；
 * - `POST|PUT|DELETE /clusters/{name}/addons` → 202 `{operation, state, cluster}`；
 * - `POST /clusters/{name}/operations`（delete-node / delete-cluster）→ 202 操作视图；
 * - `POST /clusters/{name}/archive` → 200 `{name, lifecycle}`。
 */
import { apiFetch } from './client';
import type { AddonCatalogEntry } from './types';
import { flattenAddonValues } from '../wizard/dto';

/* --------------------------------- 目录 ---------------------------------- */

/** `GET /addons/catalog`：可用 addon + values JSON Schema（向导同源，避免第二条约定）。 */
export { fetchAddonCatalog } from './wizard';

/* ------------------------------ 已安装清单 -------------------------------- */

/** 防御式解析后的已安装 addon 一行（快照里的记录形状不保证稳定）。 */
export interface InstalledAddonRow {
  name: string;
  version: string | null;
  status: string | null;
  /** values 的点路径摘要（`a.b=1`，逗号分隔）；缺失为 null。 */
  valuesSummary: string | null;
}

/** `GET /clusters/{name}/addons` 的解析结果。 */
export interface ClusterAddonInventory {
  cluster: string;
  addons: InstalledAddonRow[];
  note: string;
}

/** `GET /clusters/{name}/addons` 的原始响应（服务端 ClusterAddonsView）。 */
export interface ClusterAddonsView {
  cluster: string;
  addons: unknown;
  note: string;
}

/**
 * 把服务端的清单 JSON 解析成行：键是 addon 名，值里的 `version` /
 * `status` / `values` 都可能缺失（收集器写入的记录只有部分字段）。
 * 非对象的 `addons` 视为空清单（note 兜底）。
 */
export function parseAddonInventory(view: Partial<ClusterAddonsView>): ClusterAddonInventory {
  const raw = view.addons;
  const rows: InstalledAddonRow[] = [];
  if (raw != null && typeof raw === 'object' && !Array.isArray(raw)) {
    for (const [name, entry] of Object.entries(raw as Record<string, unknown>)) {
      const record =
        entry != null && typeof entry === 'object' && !Array.isArray(entry)
          ? (entry as Record<string, unknown>)
          : {};
      const version = typeof record.version === 'string' ? record.version : null;
      const status = typeof record.status === 'string' ? record.status : null;
      const flat: string[] = [];
      flattenAddonValues('', record.values ?? {}, flat);
      rows.push({ name, version, status, valuesSummary: flat.length > 0 ? flat.join(', ') : null });
    }
  }
  return {
    cluster: typeof view.cluster === 'string' ? view.cluster : '',
    addons: rows,
    note: typeof view.note === 'string' ? view.note : '',
  };
}

/** `GET /clusters/{name}/addons`：已安装清单（无记录时 addons 为空 + note）。 */
export function fetchInstalledAddons(
  name: string,
  signal?: AbortSignal,
): Promise<ClusterAddonInventory> {
  return apiFetch<ClusterAddonsView>(`/clusters/${encodeURIComponent(name)}/addons`, {
    signal,
  }).then(parseAddonInventory);
}

/* --------------------------- 安装 / 升级 / 卸载 --------------------------- */

/** 202 提交回执：addon 端点是 `{operation}`，operations 端点是完整视图 `{id}`。 */
export interface OperationReceipt {
  operation?: string;
  id?: string;
  state: string;
  cluster?: string | null;
}

/** 回执里的操作 id（两种端点形状取一）。 */
export function receiptOperationId(receipt: OperationReceipt): string {
  return receipt.operation ?? receipt.id ?? '';
}

/** 安装/升级请求体（§7：values 是无 addon 前缀的 `key=value` 串）。 */
export interface AddonOpRequest {
  addon: string;
  version?: string;
  values?: string[];
  dry_run?: boolean;
}

/** rjsf 表单值 → `key=value` 串（点路径展开，值按 TOML 字面量序列化）。 */
export function flattenValues(values: Record<string, unknown>): string[] {
  const out: string[] = [];
  flattenAddonValues('', values, out);
  return out;
}

/** `POST /clusters/{name}/addons`：安装 → 202 + 操作。 */
export function installAddon(name: string, request: AddonOpRequest): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/clusters/${encodeURIComponent(name)}/addons`, {
    method: 'POST',
    body: request,
  });
}

/** `PUT /clusters/{name}/addons`：升级 → 202 + 操作。 */
export function upgradeAddon(name: string, request: AddonOpRequest): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/clusters/${encodeURIComponent(name)}/addons`, {
    method: 'PUT',
    body: request,
  });
}

/**
 * `DELETE /clusters/{name}/addons`：卸载。服务端要求 `confirm` 等于
 * 集群名（否则 400），这里由调用方保证（UI 的确认框都带着集群名）。
 */
export function removeAddon(
  name: string,
  addon: string,
  keepCrds = false,
): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/clusters/${encodeURIComponent(name)}/addons`, {
    method: 'DELETE',
    body: { addon, keep_crds: keepCrds, confirm: name },
  });
}

/* ------------------------------ 危险区操作 -------------------------------- */

/** `POST /clusters/{name}/operations`：破坏性操作需要 confirm == 集群名。 */
function submitDangerousOperation(
  name: string,
  kind: 'delete-node' | 'delete-cluster',
  params: Record<string, unknown>,
): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/clusters/${encodeURIComponent(name)}/operations`, {
    method: 'POST',
    body: { kind, params, confirm: name },
  });
}

/** `delete-node`：从集群移除一个节点（params `{node}` + confirm）。 */
export function deleteClusterNode(name: string, node: string): Promise<OperationReceipt> {
  return submitDangerousOperation(name, 'delete-node', { node });
}

/** `delete-cluster`：销毁远端集群的全部资源（confirm == 集群名）。 */
export function destroyCluster(name: string): Promise<OperationReceipt> {
  return submitDangerousOperation(name, 'delete-cluster', {});
}

/** `POST /clusters/{name}/archive`：移除纳管（不动远端集群）。 */
export function archiveCluster(name: string): Promise<{ name: string; lifecycle: string }> {
  return apiFetch<{ name: string; lifecycle: string }>(
    `/clusters/${encodeURIComponent(name)}/archive`,
    { method: 'POST' },
  );
}

/* ------------------------------ 兼容性判定 -------------------------------- */

/** 目录卡片对某个 addon 的兼容结论（WP-64：禁用原因 / 已安装 / 可升级）。 */
export interface AddonCompatibility {
  installed: boolean;
  installedVersion: string | null;
  /** 与已安装 addon 冲突的名字（conflicts 双向）；非空 → 禁用安装。 */
  conflicts: string[];
  /** requires 里尚未安装的依赖。 */
  missingRequires: string[];
  /** installed & 版本一致 → installed；installed & 版本不同 → upgrade。 */
  state: 'installable' | 'installed' | 'upgrade';
}

/**
 * 目录里某个 addon 相对已安装清单的兼容性：
 * - 冲突（双向）→ 禁用并给出原因；
 * - 已安装：与目录版本一致 → 已安装，不同 → 可升级。
 */
export function addonCompatibility(
  catalog: AddonCatalogEntry[],
  inventory: InstalledAddonRow[],
  name: string,
): AddonCompatibility {
  const byName = new Map(catalog.map((a) => [a.name, a]));
  const entry = byName.get(name);
  const installed = new Map(inventory.map((a) => [a.name, a]));
  const mine = entry?.conflicts ?? [];
  const conflicts = [...installed.keys()].filter(
    (other) => mine.includes(other) || (byName.get(other)?.conflicts ?? []).includes(name),
  );
  const missingRequires = (entry?.requires ?? []).filter((dep) => !installed.has(dep));
  const installedVersion = installed.get(name)?.version ?? null;
  const state: AddonCompatibility['state'] = !installed.has(name)
    ? 'installable'
    : installedVersion != null && installedVersion === entry?.version
      ? 'installed'
      : 'upgrade';
  return { installed: installed.has(name), installedVersion, conflicts, missingRequires, state };
}

/* -------------------------------- 目录分组 -------------------------------- */

/** 分类展示顺序（未知分类按字典序追加在后面）。 */
const CATEGORY_ORDER = ['monitoring', 'ingress', 'security', 'storage', 'networking', 'dashboard'];

/** 目录按 category 分组（保持展示顺序稳定）。 */
export function groupCatalogByCategory(
  catalog: AddonCatalogEntry[],
): Array<{ category: string; addons: AddonCatalogEntry[] }> {
  const groups = new Map<string, AddonCatalogEntry[]>();
  for (const addon of catalog) {
    const key = addon.category || 'other';
    const list = groups.get(key);
    if (list) {
      list.push(addon);
    } else {
      groups.set(key, [addon]);
    }
  }
  const known = CATEGORY_ORDER.filter((c) => groups.has(c));
  const rest = [...groups.keys()].filter((c) => !known.includes(c)).sort();
  return [...known, ...rest].map((category) => ({ category, addons: groups.get(category)! }));
}
