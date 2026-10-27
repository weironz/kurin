import { describe, expect, it } from 'vitest';
import type { PackageInfo } from '../api/types';
import {
  CNI_OPTIONS,
  TEMPLATES,
  applyTemplate,
  buildNodeRow,
  checkPackageCoverage,
  emptyForm,
  indexCapabilities,
  optionDisabled,
  stepBlocked,
  addonClosure,
  addonConflictsWith,
} from './model';
import {
  buildDto,
  flattenAddonValues,
  parseLabels,
  parseRoleToForm,
  portOrUndefined,
  tomlLiteral,
} from './dto';
import type { AddonCatalogEntry } from '../api/types';

/** 基础表单：第 2 步之后每步都能通过本地门控。 */
function completeForm() {
  const form = emptyForm();
  form.name = 'prod';
  form.kubernetesVersion = '1.33.1';
  form.cni = 'calico';
  form.nodes = [
    { ...buildNodeRow({ name: 'cp-1', address: '192.168.10.11' }), role: 'control-plane' },
    { ...buildNodeRow({ name: 'worker-1', address: '192.168.10.21' }) },
  ];
  form.keyPath = '/root/.ssh/id_ed25519';
  form.packageMode = 'online';
  return form;
}

describe('向导：每步本地门控（服务端校验之外的输入完整性）', () => {
  it('第 1 步必须选模板', () => {
    expect(stepBlocked('template', emptyForm())).toBe('wizard.blocked.template');
  });

  it('第 2 步：名称为空 / 非法 / 版本缺失', () => {
    const form = emptyForm();
    expect(stepBlocked('basic', form)).toBe('wizard.blocked.name');
    form.name = 'Prod';
    expect(stepBlocked('basic', form)).toBe('wizard.blocked.name');
    form.name = 'prod';
    expect(stepBlocked('basic', form)).toBe('wizard.blocked.k8sVersion');
    form.kubernetesVersion = '1.33.1';
    expect(stepBlocked('basic', form)).toBeNull();
  });

  it('第 3 步：无节点 / 缺字段 / 重名 / 重地址', () => {
    const form = completeForm();
    expect(stepBlocked('nodes', form)).toBeNull();
    expect(stepBlocked('nodes', { ...form, nodes: [] })).toBe('wizard.blocked.nodes');
    expect(
      stepBlocked('nodes', { ...form, nodes: [{ ...form.nodes[0], address: '' }] }),
    ).toBe('wizard.blocked.nodes');
    expect(
      stepBlocked('nodes', {
        ...form,
        nodes: [...form.nodes, { ...form.nodes[0], address: '192.168.10.99' }],
      }),
    ).toBe('wizard.blocked.nodeNames');
    expect(
      stepBlocked('nodes', {
        ...form,
        nodes: [...form.nodes, { ...form.nodes[1], name: 'other' }],
      }),
    ).toBe('wizard.blocked.nodeAddresses');
  });

  it('第 4 步：认证方式对应来源必填', () => {
    const form = completeForm();
    expect(stepBlocked('auth', form)).toBeNull();
    expect(stepBlocked('auth', { ...form, keyPath: '' })).toBe('wizard.blocked.key');
    expect(
      stepBlocked('auth', { ...form, authMethod: 'password-env', passwordEnv: '' }),
    ).toBe('wizard.blocked.password');
    expect(
      stepBlocked('auth', { ...form, authMethod: 'password-file', passwordFile: '' }),
    ).toBe('wizard.blocked.password');
  });

  it('第 5/6/7 步：CNI、外部 registry 地址、包选择', () => {
    const form = completeForm();
    expect(stepBlocked('network', { ...form, cni: '' })).toBe('wizard.blocked.cni');
    expect(
      stepBlocked('components', { ...form, registryChoice: 'external' }),
    ).toBe('wizard.blocked.registryAddress');
    expect(stepBlocked('offline', { ...form, packageMode: 'existing' })).toBe(
      'wizard.blocked.package',
    );
    expect(stepBlocked('offline', { ...form, packageMode: 'uri' })).toBe('wizard.blocked.package');
  });
});

describe('向导：模板预填', () => {
  it('3 主 HA（kube-vip）模板预填发行版 / CNI / HA / 节点数量建议', () => {
    const template = TEMPLATES.find((x) => x.id === 'ha-kubevip');
    expect(template).toBeDefined();
    const form = applyTemplate(emptyForm(), template!);
    expect(form.template).toBe('ha-kubevip');
    expect(form.distribution).toBe('kubeadm');
    expect(form.cni).toBe('calico');
    expect(form.haMode).toBe('kube-vip');
    expect(form.controlPlaneCount).toBe(3);
  });

  it('k3s 边缘模板：flannel + 1 主 2 worker', () => {
    const form = applyTemplate(emptyForm(), TEMPLATES.find((x) => x.id === 'k3s-edge')!);
    expect(form.distribution).toBe('k3s');
    expect(form.cni).toBe('flannel');
    expect(form.haMode).toBe('kube-vip');
  });
});

describe('向导：能力表只禁用并给原因（UI §8.3）', () => {
  const capabilities = indexCapabilities([
    {
      id: 'cni/calico',
      kind: 'cni',
      distributions: ['k8s'],
      archs: ['amd64', 'arm64'],
      offline: 'bundle',
      zones: ['global', 'cn'],
      ops: ['create'],
    },
    {
      id: 'cni/flannel',
      kind: 'cni',
      distributions: ['k8s', 'k3s'],
      archs: ['amd64', 'arm64'],
      offline: 'bundle',
      zones: ['global', 'cn'],
      ops: ['create'],
    },
    {
      id: 'cni/kube-ovn',
      kind: 'cni',
      distributions: ['k8s'],
      archs: ['amd64', 'arm64'],
      offline: 'online-only',
      zones: ['global', 'cn'],
      ops: ['create'],
    },
  ]);

  it('k3s 下 calico 禁用（发行版不支持），flannel 可用', () => {
    expect(optionDisabled(capabilities, 'cni/calico', 'k3s', false)).toBe(
      'wizard.capability.distribution',
    );
    expect(optionDisabled(capabilities, 'cni/flannel', 'k3s', false)).toBeNull();
  });

  it('选了离线包时 kube-ovn 禁用（online-only），在线则可用', () => {
    expect(optionDisabled(capabilities, 'cni/kube-ovn', 'kubeadm', true)).toBe(
      'wizard.capability.offline',
    );
    expect(optionDisabled(capabilities, 'cni/kube-ovn', 'kubeadm', false)).toBeNull();
  });

  it('CNI 选项表与后端组件一一对应', () => {
    expect(CNI_OPTIONS.map((o) => o.value)).toEqual([
      'calico',
      'flannel',
      'cilium',
      'kube-ovn',
      'none',
    ]);
  });
});

describe('向导：DTO 组装（表单 → render 请求）', () => {
  it('字段映射：kubeadm 直通、VIP 仅在 HA 模式下传、默认 CIDR带上', () => {
    const form = completeForm();
    form.haMode = 'kube-vip';
    form.vip = '192.168.10.100';
    const dto = buildDto(form, [], null);
    expect(dto.name).toBe('prod');
    expect(dto.kubernetes).toBe('1.33.1');
    expect(dto.distribution).toBe('kubeadm');
    expect(dto.cni).toBe('calico');
    expect(dto.pod_cidr).toBe('10.233.64.0/18');
    expect(dto.ha_mode).toBe('kube-vip');
    expect(dto.vip).toBe('192.168.10.100');
    expect(dto.nodes).toHaveLength(2);
    expect(dto.nodes?.[0]).toMatchObject({ name: 'cp-1', address: '192.168.10.11', role: 'control-plane' });
    // 私钥认证：key 路径 + become 不开启时不带密码引用
    expect(dto.ssh).toMatchObject({ user: 'root', key: '/root/.ssh/id_ed25519' });
    expect(dto.ssh?.become_password_env).toBeUndefined();
  });

  it('单控制面（无 HA）时 vip 不进 DTO', () => {
    const form = completeForm();
    form.haMode = 'none';
    form.vip = '192.168.10.100';
    const dto = buildDto(form, [], null);
    expect(dto.vip).toBeUndefined();
    expect(dto.ha_mode).toBeUndefined();
  });

  it('k3s 下非 flannel/none 的 CNI 收紧为 flannel（服务端只支持这两种）', () => {
    const form = completeForm();
    form.distribution = 'k3s';
    form.cni = 'calico';
    const dto = buildDto(form, [], null);
    expect(dto.cni).toBe('flannel');
  });

  it('认证方式三种映射：env 引用 / 文件引用 / 私钥', () => {
    const form = completeForm();
    const envDto = buildDto(
      { ...form, authMethod: 'password-env', passwordEnv: 'KURIN_SSH_PASSWORD' },
      [],
      null,
    );
    expect(envDto.ssh?.password_env).toBe('KURIN_SSH_PASSWORD');
    const fileDto = buildDto(
      { ...form, authMethod: 'password-file', passwordFile: '/etc/kurin/pass' },
      [],
      null,
    );
    expect(fileDto.ssh?.password_file).toBe('/etc/kurin/pass');
    expect(fileDto.ssh?.password_env).toBeUndefined();
    const keyDto = buildDto(
      { ...form, keyPath: '/root/.ssh/id_ed25519', keyPassphraseEnv: 'KEY_PASS' },
      [],
      null,
    );
    expect(keyDto.ssh).toMatchObject({ key: '/root/.ssh/id_ed25519', key_passphrase_env: 'KEY_PASS' });
  });

  it('节点级覆盖：user/port/labels/内网地址；端口非法不传', () => {
    const form = completeForm();
    form.nodes = [
      {
        ...buildNodeRow({ name: 'w9', address: '10.1.0.9' }),
        internalAddress: '192.168.0.9',
        user: 'ops',
        port: '2222',
        labels: 'zone=a,env=prod',
      },
    ];
    const dto = buildDto(form, [], null);
    expect(dto.nodes?.[0]).toMatchObject({
      internal_address: '192.168.0.9',
      user: 'ops',
      port: 2222,
      labels: { zone: 'a', env: 'prod' },
    });
    form.nodes[0].port = 'not-a-port';
    expect(buildDto(form, [], null).nodes?.[0].port).toBeUndefined();
  });

  it('addon：勾选 + values 扁平化成 NAME.KEY=TOML 字面量', () => {
    const form = completeForm();
    form.addons = {
      'metrics-server': { name: 'metrics-server', values: { replicas: 3 } },
      'ingress-nginx': {
        name: 'ingress-nginx',
        values: { controller: { replicas: 2 } },
      },
    };
    const catalog: AddonCatalogEntry[] = ['metrics-server', 'ingress-nginx'].map((name) => ({
      name,
      version: '1',
      title: null,
      description: '',
      category: 'x',
      icon: null,
      namespace: 'x',
      requires: [],
      conflicts: [],
      values_schema: {},
      origin: 'builtin',
    }));
    const dto = buildDto(form, catalog, null);
    expect(dto.addons).toEqual(['metrics-server', 'ingress-nginx']);
    expect(dto.addon_values).toEqual([
      'metrics-server.replicas=3',
      'ingress-nginx.controller.replicas=2',
    ]);
  });

  it('registry 与离线包：sets + packages 携带包 source URI', () => {
    const form = completeForm();
    form.registryChoice = 'external';
    form.externalRegistryAddress = 'https://registry.example.com:5000';
    const pkg: PackageInfo = {
      id: 'pkg-1',
      source: 'file:///media/usb/base.tar.zst',
      origin: 'imported',
      kind: 'base',
      name: 'base',
      version: '1.33.1',
      distribution: 'kubeadm',
      arch: ['amd64'],
      size: 1,
      index: 'sha256:x',
      signatures: 0,
      sbom: null,
      created_at: '2026-10-20T08:00:00.000Z',
      verified_at: '2026-10-20T08:01:00.000Z',
    };
    const dto = buildDto({ ...form, packageMode: 'existing', packageId: 'pkg-1' }, [], pkg);
    expect(dto.sets).toEqual([
      'registry.kind="external"',
      'registry.address="https://registry.example.com:5000"',
    ]);
    expect(dto.packages).toEqual(['file:///media/usb/base.tar.zst']);
  });
});

describe('向导：辅助序列化', () => {
  it('tomlLiteral：字符串加引号、数字/布尔直出', () => {
    expect(tomlLiteral('a b')).toBe('"a b"');
    expect(tomlLiteral(3)).toBe('3');
    expect(tomlLiteral(true)).toBe('true');
  });

  it('flattenAddonValues：嵌套对象展开为点路径', () => {
    const out: string[] = [];
    flattenAddonValues('', { a: 1, b: { c: 'x' } }, out);
    expect(out).toEqual(['a=1', 'b.c="x"']);
  });

  it('parseLabels / parseRoleToForm / portOrUndefined', () => {
    expect(parseLabels('zone=a, env=prod;bad')).toEqual({ zone: 'a', env: 'prod' });
    expect(parseRoleToForm('controlplane')).toBe('control-plane');
    expect(parseRoleToForm('lb')).toBe('lb');
    expect(parseRoleToForm('registry')).toBe('registry');
    expect(parseRoleToForm('worker')).toBe('worker');
    expect(portOrUndefined('22')).toBe(22);
    expect(portOrUndefined('')).toBeUndefined();
    expect(portOrUndefined('70000')).toBeUndefined();
  });
});

describe('向导：离线包覆盖性检查（UI §8.2 第 7 步）', () => {
  const pkg: PackageInfo = {
    id: 'pkg-1',
    source: 'file:///base.tar.zst',
    origin: 'imported',
    kind: 'base',
    name: 'base',
    version: '1.33.1',
    distribution: 'kubeadm',
    arch: ['amd64'],
    size: 1,
    index: 'sha256:x',
    signatures: 0,
    sbom: null,
    created_at: '2026-10-20T08:00:00.000Z',
    verified_at: '2026-10-20T08:01:00.000Z',
  };

  it('版本 / 发行版 / 架构不一致各成一条 error', () => {
    const form = completeForm();
    expect(checkPackageCoverage(pkg, form, [])).toEqual([]);
    const versionIssues = checkPackageCoverage(
      { ...pkg, version: '1.32.0' },
      form,
      [],
    );
    expect(versionIssues.map((i) => i.messageKey)).toEqual(['wizard.package.versionMismatch']);
    expect(versionIssues[0].vars).toEqual({ have: '1.32.0', want: '1.33.1' });
    const distroIssues = checkPackageCoverage(
      { ...pkg, distribution: 'k3s' },
      form,
      [],
    );
    expect(distroIssues.map((i) => i.messageKey)).toEqual(['wizard.package.distributionMismatch']);
    const archIssues = checkPackageCoverage(pkg, form, ['arm64']);
    expect(archIssues.map((i) => i.messageKey)).toEqual(['wizard.package.archMismatch']);
    expect(archIssues[0].vars).toEqual({ arch: 'arm64' });
  });
});

describe('向导：addon 依赖与冲突', () => {
  const catalog: AddonCatalogEntry[] = [
    {
      name: 'a',
      version: '1',
      title: null,
      description: '',
      category: 'x',
      icon: null,
      namespace: 'x',
      requires: ['b'],
      conflicts: [],
      values_schema: {},
      origin: 'builtin',
    },
    {
      name: 'b',
      version: '1',
      title: null,
      description: '',
      category: 'x',
      icon: null,
      namespace: 'x',
      requires: [],
      conflicts: [],
      values_schema: {},
      origin: 'builtin',
    },
    {
      name: 'c',
      version: '1',
      title: null,
      description: '',
      category: 'x',
      icon: null,
      namespace: 'x',
      requires: [],
      conflicts: ['a'],
      values_schema: {},
      origin: 'builtin',
    },
  ];

  it('依赖闭包：勾 a 自动带 b', () => {
    expect([...addonClosure(catalog, ['a'])].sort()).toEqual(['a', 'b']);
  });

  it('冲突是双向的', () => {
    expect(addonConflictsWith(catalog, ['a'], 'c')).toEqual(['a']);
    expect(addonConflictsWith(catalog, ['c'], 'a')).toEqual(['c']);
    expect(addonConflictsWith(catalog, ['b'], 'a')).toEqual([]);
  });
});
