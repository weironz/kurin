import { beforeEach, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { server } from '../mocks/server';
import { ApiError, apiFetch, loginWithToken } from './client';
import {
  addonCompatibility,
  archiveCluster,
  deleteClusterNode,
  fetchInstalledAddons,
  flattenValues,
  groupCatalogByCategory,
  parseAddonInventory,
  receiptOperationId,
  removeAddon,
  type InstalledAddonRow,
} from './addons';
import type { AddonCatalogEntry } from './types';

/** 目录夹具：与 mockAddonCatalog 同构的最小集（冲突/依赖关系覆盖分支）。 */
const catalog: AddonCatalogEntry[] = [
  {
    name: 'metrics-server',
    version: '0.9.0',
    title: 'Metrics Server',
    description: '资源指标 API',
    category: 'monitoring',
    icon: null,
    namespace: 'kube-system',
    requires: [],
    conflicts: [],
    values_schema: {},
    origin: 'builtin',
  },
  {
    name: 'local-path',
    version: '0.0.31',
    title: 'Local Path Provisioner',
    description: '本地路径存储',
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
    description: '外部 NFS 动态存储',
    category: 'storage',
    icon: null,
    namespace: 'default',
    requires: [],
    conflicts: ['local-path'],
    values_schema: {},
    origin: 'builtin',
  },
  {
    name: 'kube-prometheus-stack',
    version: '1.2.3',
    title: null,
    description: '监控套件',
    category: 'monitoring',
    icon: null,
    namespace: 'monitoring',
    requires: ['metrics-server'],
    conflicts: [],
    values_schema: {},
    origin: 'builtin',
  },
];

/** 已安装清单夹具。 */
const installed: InstalledAddonRow[] = [
  { name: 'local-path', version: '0.0.31', status: 'installed', valuesSummary: null },
  { name: 'metrics-server', version: '0.8.0', status: 'installed', valuesSummary: null },
];

beforeEach(async () => {
  await loginWithToken('valid-token');
});

describe('parseAddonInventory（防御式解析）', () => {
  it('version / status / values 缺失时降级为 null', () => {
    const inv = parseAddonInventory({
      cluster: 'tier1',
      addons: { 'metrics-server': { version: '0.9.0' }, 'bare-addon': {} },
      note: '',
    });
    expect(inv.cluster).toBe('tier1');
    expect(inv.addons).toHaveLength(2);
    const metrics = inv.addons.find((a) => a.name === 'metrics-server');
    expect(metrics?.version).toBe('0.9.0');
    expect(metrics?.status).toBeNull();
    const bare = inv.addons.find((a) => a.name === 'bare-addon');
    expect(bare?.version).toBeNull();
    expect(bare?.valuesSummary).toBeNull();
  });

  it('values 对象展开为点路径摘要', () => {
    const inv = parseAddonInventory({
      cluster: 'tier1',
      addons: { 'ingress-nginx': { version: '1.15.1', values: { controller: { replicas: 2 } } } },
      note: '',
    });
    expect(inv.addons[0]?.valuesSummary).toBe('controller.replicas=2');
  });

  it('addons 非对象（数组/标量）时按空清单处理并保留 note', () => {
    for (const addons of [[], 'oops', null, 42]) {
      const inv = parseAddonInventory({ cluster: 'tier1', addons, note: '尚未采集' });
      expect(inv.addons).toEqual([]);
      expect(inv.note).toBe('尚未采集');
    }
  });

  it('条目值非对象（标量）时字段全部降级', () => {
    const inv = parseAddonInventory({ cluster: 'tier1', addons: { weird: 'nope' }, note: '' });
    expect(inv.addons).toEqual([
      { name: 'weird', version: null, status: null, valuesSummary: null },
    ]);
  });
});

describe('flattenValues（表单值 → key=value）', () => {
  it('嵌套对象展开为点路径，标量按 TOML 字面量序列化', () => {
    expect(flattenValues({ replicas: 2 })).toEqual(['replicas=2']);
    expect(flattenValues({ a: 1, b: { c: 'x' }, d: true })).toEqual([
      'a=1',
      'b.c="x"',
      'd=true',
    ]);
  });

  it('空对象 → 空（不发送 values 字段）', () => {
    expect(flattenValues({})).toEqual([]);
  });
});

describe('addonCompatibility（兼容性 / 禁用原因）', () => {
  it('未安装 → installable', () => {
    const compat = addonCompatibility(catalog, installed, 'local-path');
    expect(compat.installed).toBe(true);
    expect(addonCompatibility(catalog, installed, 'nfs-subdir').state).toBe('installable');
  });

  it('已安装且版本一致 → installed', () => {
    const compat = addonCompatibility(catalog, installed, 'local-path');
    expect(compat.state).toBe('installed');
    expect(compat.installedVersion).toBe('0.0.31');
  });

  it('已安装但版本不同 → upgrade（带已装版本）', () => {
    const compat = addonCompatibility(catalog, installed, 'metrics-server');
    expect(compat.state).toBe('upgrade');
    expect(compat.installedVersion).toBe('0.8.0');
  });

  it('与已安装 addon 冲突（目录声明方向）→ conflicts 给出禁用原因', () => {
    const compat = addonCompatibility(catalog, installed, 'nfs-subdir');
    expect(compat.conflicts).toContain('local-path');
  });

  it('与已安装 addon 冲突（已装一侧声明的反向）→ 同样命中', () => {
    const reverse: InstalledAddonRow[] = [
      { name: 'nfs-subdir', version: '4.0.2', status: 'installed', valuesSummary: null },
    ];
    const compat = addonCompatibility(catalog, reverse, 'local-path');
    expect(compat.conflicts).toContain('nfs-subdir');
  });

  it('依赖未安装 → missingRequires 列出', () => {
    const withoutMetrics = installed.filter((a) => a.name !== 'metrics-server');
    const compat = addonCompatibility(catalog, withoutMetrics, 'kube-prometheus-stack');
    expect(compat.missingRequires).toEqual(['metrics-server']);
    expect(compat.state).toBe('installable');
  });

  it('依赖已安装 → missingRequires 为空', () => {
    const withMetrics: InstalledAddonRow[] = [
      ...installed,
      { name: 'metrics-server', version: '0.9.0', status: 'installed', valuesSummary: null },
    ];
    expect(addonCompatibility(catalog, withMetrics, 'kube-prometheus-stack').missingRequires).toEqual(
      [],
    );
  });

  it('不在目录里的已安装 addon 不产生冲突', () => {
    const extra: InstalledAddonRow[] = [
      ...installed,
      { name: 'mystery', version: '1.0.0', status: 'installed', valuesSummary: null },
    ];
    expect(addonCompatibility(catalog, extra, 'metrics-server').conflicts).toEqual([]);
  });
});

describe('groupCatalogByCategory（目录分组）', () => {
  it('按既定顺序分组，未知分类按字典序追加', () => {
    const groups = groupCatalogByCategory([
      ...catalog,
      { ...catalog[0], name: 'zzz', category: 'weird' },
    ]);
    expect(groups.map((g) => g.category)).toEqual(['monitoring', 'storage', 'weird']);
    expect(groups[0].addons.map((a) => a.name)).toEqual([
      'metrics-server',
      'kube-prometheus-stack',
    ]);
    expect(groups[1].addons.map((a) => a.name)).toEqual(['local-path', 'nfs-subdir']);
  });

  it('空分类回退 other', () => {
    const groups = groupCatalogByCategory([{ ...catalog[0], category: '' }]);
    expect(groups.map((g) => g.category)).toEqual(['other']);
  });
});

describe('receiptOperationId（202 回执）', () => {
  it('addon 端点的 {operation} 与 operations 端点的 {id} 都能取到', () => {
    expect(receiptOperationId({ operation: 'op-a', state: 'queued' })).toBe('op-a');
    expect(receiptOperationId({ id: 'op-b', state: 'queued' })).toBe('op-b');
    expect(receiptOperationId({ state: 'queued' })).toBe('');
  });
});

describe('confirm 门控（MSW 镜像服务端契约）', () => {
  it('delete-node 携带 confirm == 集群名 → 202', async () => {
    const receipt = await deleteClusterNode('tier1', 'worker-1');
    expect(receiptOperationId(receipt)).toMatch(/^01JDY/);
    expect(receipt.cluster).toBe('tier1');
  });

  it('delete-cluster 的 confirm 不等于集群名 → 400（服务端错误信息透出）', async () => {
    const wrong = apiFetch('/clusters/tier1/operations', {
      method: 'POST',
      body: { kind: 'delete-cluster', params: {}, confirm: 'typo' },
    });
    const err = (await wrong.catch((e) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(400);
    expect(err.message).toContain('tier1');
  });

  it('卸载 addon 携带 confirm == 集群名 → 202', async () => {
    await apiFetch('/clusters/tier1/addons', {
      method: 'POST',
      body: { addon: 'metrics-server' },
    });
    const receipt = await removeAddon('tier1', 'metrics-server');
    expect(receipt.cluster).toBe('tier1');
    const after = await fetchInstalledAddons('tier1');
    expect(after.addons.map((a) => a.name)).not.toContain('metrics-server');
  });

  it('卸载 addon 的 confirm 不等于集群名 → 400', async () => {
    const err = (await apiFetch('/clusters/tier1/addons', {
      method: 'DELETE',
      body: { addon: 'metrics-server', confirm: 'nope' },
    }).catch((e) => e)) as ApiError;
    expect(err.status).toBe(400);
  });
});

describe('archive（移除纳管）', () => {
  it('无活动操作的集群 → 200 且详情 lifecycle 变为 archived', async () => {
    const result = await archiveCluster('tier1');
    expect(result).toEqual({ name: 'tier1', lifecycle: 'archived' });
    const detail = await apiFetch<{ lifecycle: string }>('/clusters/tier1');
    expect(detail.lifecycle).toBe('archived');
  });

  it('有活动操作的集群 → 409（服务端错误信息透出）', async () => {
    const err = (await archiveCluster('tier2-edge').catch((e) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBe(409);
    expect(err.message).toContain('活动操作');
  });

  it('不存在的集群 → 404', async () => {
    const err = (await archiveCluster('ghost').catch((e) => e)) as ApiError;
    expect(err.status).toBe(404);
  });
});

describe('清单端点（MSW）', () => {
  it('安装后清单反映新 addon（values 摘要 + 状态）', async () => {
    await apiFetch('/clusters/tier1/addons', {
      method: 'POST',
      body: { addon: 'ingress-nginx', version: '1.15.1' },
    });
    const inv = await fetchInstalledAddons('tier1');
    const row = inv.addons.find((a) => a.name === 'ingress-nginx');
    expect(row?.version).toBe('1.15.1');
    expect(row?.status).toBe('installed');
  });

  it('空清单带服务端 note', async () => {
    const inv = await fetchInstalledAddons('tier1');
    expect(inv.addons).toEqual([]);
    expect(inv.note).toContain('addon');
  });

  it('服务端 500 时错误向上传播', async () => {
    server.use(
      http.get('/api/v1/clusters/:name/addons', () =>
        HttpResponse.json({ code: 'internal', message: 'boom' }, { status: 500 }),
      ),
    );
    const err = (await fetchInstalledAddons('tier1').catch((e) => e)) as ApiError;
    expect(err.status).toBe(500);
  });
});
