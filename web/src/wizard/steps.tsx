import { useMemo, useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Checkbox,
  Descriptions,
  Form,
  Input,
  InputNumber,
  Modal,
  Radio,
  Select,
  Space,
  Spin,
  Table,
  Tag,
  Typography,
} from 'antd';
import { DownloadOutlined } from '@ant-design/icons';
import type {
  AddonCatalogEntry,
  Meta,
  PackageInfo,
  PlanResult,
  PrecheckReportResult,
} from '../api/types';
import {
  ensureClusterRegistered,
  parseNodes,
  probeNode,
  submitOperation,
  submitPlan,
  waitForOperation,
} from '../api/wizard';
import { t } from '../i18n/zh-CN';
import { AddonValuesForm } from './AddonValuesForm';
import {
  CNI_OPTIONS,
  HA_OPTIONS,
  NAME_PATTERN,
  REGISTRY_OPTIONS,
  RUNTIME_OPTIONS,
  TEMPLATES,
  addonClosure,
  addonConflictsWith,
  applyTemplate,
  buildNodeRow,
  checkPackageCoverage,
  optionDisabled,
  type CapabilityIndex,
  type NodeRow,
  type WizardForm,
} from './model';
import { parseRoleToForm, probeRequestFor } from './dto';

/* --------------------------------- 步骤 props --------------------------------- */

/** 每步共享的上下文（向导页面传入）。 */
export interface StepProps {
  form: WizardForm;
  updateForm: (patch: Partial<WizardForm>) => void;
  capabilities: CapabilityIndex;
  catalog: AddonCatalogEntry[];
  meta?: Meta;
  packages?: PackageInfo[];
  /** 当前离线模式（供能力表 offline-only 检查）。 */
  offlineSelected: boolean;
}

/* -------------------------------- 第 1 步：模板 -------------------------------- */

export function TemplateStep({ form, updateForm }: StepProps) {
  return (
    <Card>
      <Radio.Group
        value={form.template}
        onChange={(e) => {
          const template = TEMPLATES.find((x) => x.id === e.target.value);
          if (template) {
            const patched = applyTemplate(form, template);
            updateForm(patched);
          }
        }}
      >
        <Space direction="vertical" size={12} style={{ width: '100%' }}>
          {TEMPLATES.map((template) => (
            <Radio key={template.id} value={template.id}>
              <Card size="small" style={{ marginTop: 8 }}>
                <Typography.Text strong>{t(template.titleKey)}</Typography.Text>
                <Typography.Paragraph type="secondary" style={{ marginBottom: 0 }}>
                  {t(template.descriptionKey)}
                </Typography.Paragraph>
              </Card>
            </Radio>
          ))}
        </Space>
      </Radio.Group>
    </Card>
  );
}

/* ------------------------------ 第 2 步：基本信息 ------------------------------ */

export function BasicStep({ form, updateForm, meta, capabilities }: StepProps) {
  const k8sVersions = useMemo(() => {
    const entries = Object.entries(meta?.components ?? {});
    return entries.filter(([name]) => name === 'kubernetes' || name === 'kubernetes.k3s');
  }, [meta]);
  const isK3s = form.distribution === 'k3s';
  return (
    <Card>
      <Form layout="vertical">
        <Form.Item
          label={t('wizard.basic.name')}
          extra={t('wizard.basic.nameExtra')}
          validateStatus={form.name !== '' && !NAME_PATTERN.test(form.name) ? 'error' : undefined}
        >
          <Input
            value={form.name}
            onChange={(e) => updateForm({ name: e.target.value })}
            placeholder="prod"
            data-testid="wizard-name"
          />
        </Form.Item>
        <Form.Item label={t('wizard.basic.distribution')}>
          <Radio.Group
            value={form.distribution}
            onChange={(e) => updateForm({ distribution: e.target.value })}
          >
            <Radio.Button value="kubeadm">{t('wizard.distribution.kubeadm')}</Radio.Button>
            <Radio.Button value="k3s">{t('wizard.distribution.k3s')}</Radio.Button>
          </Radio.Group>
        </Form.Item>
        <Form.Item label={t('wizard.basic.k8sVersion')} extra={t('wizard.basic.k8sVersionExtra')}>
          <Select
            value={form.kubernetesVersion === '' ? undefined : form.kubernetesVersion}
            onChange={(v) => updateForm({ kubernetesVersion: v })}
            options={k8sVersions.map(([name, version]) => ({
              value: version,
              label: `${name === 'kubernetes.k3s' ? 'k3s' : 'kubeadm'} ${version}`,
            }))}
            placeholder={k8sVersions.length === 0 ? '—' : t('wizard.basic.k8sVersionPlaceholder')}
            style={{ width: '100%' }}
            data-testid="wizard-k8s-version"
          />
        </Form.Item>
        <Form.Item
          label={t('wizard.basic.runtime')}
          extra={isK3s ? t('wizard.runtime.k3sFixed') : undefined}
        >
          <Radio.Group
            value={isK3s ? 'containerd' : form.runtime}
            onChange={(e) => updateForm({ runtime: e.target.value })}
            disabled={isK3s}
          >
            {RUNTIME_OPTIONS.map((opt) => {
              const blocked = optionDisabled(capabilities, opt.componentId, form.distribution, false);
              return (
                <Radio.Button key={opt.value} value={opt.value} disabled={blocked != null}>
                  {blocked == null
                    ? t(`wizard.runtime.${opt.value}` as never)
                    : `${t(`wizard.runtime.${opt.value}` as never)}（${t(blocked)}）`}
                </Radio.Button>
              );
            })}
          </Radio.Group>
        </Form.Item>
        <Form.Item label={t('wizard.basic.zone')}>
          <Radio.Group
            value={form.zone}
            onChange={(e) => updateForm({ zone: e.target.value })}
          >
            <Radio.Button value="global">{t('wizard.zone.global')}</Radio.Button>
            <Radio.Button value="cn">{t('wizard.zone.cn')}</Radio.Button>
          </Radio.Group>
        </Form.Item>
      </Form>
    </Card>
  );
}

/* -------------------------------- 第 3 步：节点 -------------------------------- */

const ROLE_OPTIONS: Array<{ value: NodeRow['role']; label: string }> = [
  { value: 'control-plane', label: t('wizard.nodes.role.control-plane') },
  { value: 'worker', label: t('wizard.nodes.role.worker') },
  { value: 'lb', label: t('wizard.nodes.role.lb') },
  { value: 'registry', label: t('wizard.nodes.role.registry') },
];

const SUDO_LABEL: Record<string, string> = {
  none: t('wizard.nodes.sudo.none'),
  passwordless: t('wizard.nodes.sudo.passwordless'),
  'needs-password': t('wizard.nodes.sudo.needs-password'),
  unknown: t('wizard.nodes.sudo.unknown'),
};


export function NodesStep({ form, updateForm }: StepProps) {
  const [bulk, setBulk] = useState('');
  const [bulkError, setBulkError] = useState<string | null>(null);
  const [parsing, setParsing] = useState(false);

  const setNodes = (nodes: NodeRow[]) => updateForm({ nodes });

  const applyBulk = async () => {
    setParsing(true);
    setBulkError(null);
    try {
      const existing = form.nodes.map((n) => n.name).filter(Boolean);
      const parsed = await parseNodes(bulk, existing);
      const rows = parsed.nodes.map((node) =>
        buildNodeRow({
          name: node.name,
          address: node.address,
          role: parseRoleToForm(node.role),
        }),
      );
      setNodes([...form.nodes, ...rows]);
      setBulk('');
    } catch (error) {
      const apiError = error as { diagnostics?: Array<{ message: string }>; message?: string };
      setBulkError(
        apiError.diagnostics?.[0]?.message ?? apiError.message ?? t('wizard.nodes.bulkError'),
      );
    } finally {
      setParsing(false);
    }
  };

  const probeRow = async (key: string) => {
    const row = form.nodes.find((n) => n.key === key);
    if (!row || row.address === '') return;
    setNodes(
      form.nodes.map((n) => (n.key === key ? { ...n, probe: { state: 'probing' } } : n)),
    );
    try {
      const response = await probeNode(probeRequestFor(row, form));
      const facts = response.facts;
      setNodes(
        form.nodes.map((n) =>
          n.key === key
            ? {
                ...n,
                probe: {
                  state: 'ok' as const,
                  arch: facts.arch,
                  os: facts.os,
                  osVersion: facts.os_version,
                  cpus: facts.cpus,
                  memMib: facts.mem_mib,
                  diskRootMib: facts.disk_free_mib['/'] ?? 0,
                  sudo: facts.sudo,
                  isRoot: facts.is_root,
                },
              }
            : n,
        ),
      );
    } catch (error) {
      const apiError = error as { message?: string };
      setNodes(
        form.nodes.map((n) =>
          n.key === key ? { ...n, probe: { state: 'failed' as const, error: apiError.message } } : n,
        ),
      );
    }
  };

  const probeAll = async () => {
    for (const row of form.nodes) {
      if (row.address !== '') await probeRow(row.key);
    }
  };

  return (
    <Card>
      <Card
        type="inner"
        title={t('wizard.nodes.bulkTitle')}
        style={{ marginBottom: 16 }}
        data-testid="wizard-bulk-card"
      >
        <Input.TextArea
          rows={4}
          value={bulk}
          onChange={(e) => setBulk(e.target.value)}
          placeholder={t('wizard.nodes.bulkPlaceholder')}
          data-testid="wizard-bulk-input"
        />
        <Button
          style={{ marginTop: 8 }}
          onClick={applyBulk}
          loading={parsing}
          disabled={bulk.trim() === ''}
          data-testid="wizard-bulk-parse"
        >
          {t('wizard.nodes.bulkParse')}
        </Button>
        {bulkError ? (
          <Alert type="error" showIcon message={bulkError} style={{ marginTop: 8 }} />
        ) : null}
      </Card>
      <Table<NodeRow>
        rowKey="key"
        size="small"
        pagination={false}
        dataSource={form.nodes}
        rowClassName={(row) => (row.probe?.state === 'failed' ? 'wizard-row-failed' : '')}
        columns={[
          {
            title: t('wizard.nodes.column.name'),
            dataIndex: 'name',
            width: 120,
            render: (_, row) => (
              <Input
                value={row.name}
                onChange={(e) =>
                  setNodes(form.nodes.map((n) => (n.key === row.key ? { ...n, name: e.target.value } : n)))
                }
                aria-label={`${t('wizard.nodes.column.name')} ${row.key}`}
              />
            ),
          },
          {
            title: t('wizard.nodes.column.address'),
            dataIndex: 'address',
            width: 150,
            render: (_, row) => (
              <Input
                value={row.address}
                onChange={(e) =>
                  setNodes(
                    form.nodes.map((n) => (n.key === row.key ? { ...n, address: e.target.value } : n)),
                  )
                }
                aria-label={`${t('wizard.nodes.column.address')} ${row.key}`}
              />
            ),
          },
          {
            title: t('wizard.nodes.column.internalAddress'),
            dataIndex: 'internalAddress',
            width: 150,
            render: (_, row) => (
              <Input
                value={row.internalAddress}
                onChange={(e) =>
                  setNodes(
                    form.nodes.map((n) =>
                      n.key === row.key ? { ...n, internalAddress: e.target.value } : n,
                    ),
                  )
                }
              />
            ),
          },
          {
            title: t('wizard.nodes.column.role'),
            dataIndex: 'role',
            width: 120,
            render: (_, row) => (
              <Select
                value={row.role}
                style={{ width: '100%' }}
                options={ROLE_OPTIONS}
                onChange={(role) =>
                  setNodes(form.nodes.map((n) => (n.key === row.key ? { ...n, role } : n)))
                }
              />
            ),
          },
          {
            title: t('wizard.nodes.column.user'),
            dataIndex: 'user',
            width: 110,
            render: (_, row) => (
              <Input
                value={row.user}
                placeholder="—"
                onChange={(e) =>
                  setNodes(form.nodes.map((n) => (n.key === row.key ? { ...n, user: e.target.value } : n)))
                }
              />
            ),
          },
          {
            title: t('wizard.nodes.column.port'),
            dataIndex: 'port',
            width: 90,
            render: (_, row) => (
              <Input
                value={row.port}
                placeholder="—"
                onChange={(e) =>
                  setNodes(form.nodes.map((n) => (n.key === row.key ? { ...n, port: e.target.value } : n)))
                }
              />
            ),
          },
          {
            title: t('wizard.nodes.column.labels'),
            dataIndex: 'labels',
            render: (_, row) => (
              <Input
                value={row.labels}
                placeholder="zone=a,env=prod"
                onChange={(e) =>
                  setNodes(form.nodes.map((n) => (n.key === row.key ? { ...n, labels: e.target.value } : n)))
                }
              />
            ),
          },
          {
            title: t('wizard.nodes.column.facts'),
            key: 'facts',
            render: (_, row) => {
              if (row.probe?.state === 'probing') return <Spin size="small" />;
              if (row.probe?.state === 'failed') {
                return (
                  <Typography.Text type="danger" data-testid="wizard-probe-error">
                    {t('wizard.nodes.probeFailed', { error: row.probe.error ?? '' })}
                  </Typography.Text>
                );
              }
              if (row.probe?.state === 'ok') {
                return (
                  <Typography.Text data-testid="wizard-probe-ok">
                    {t('wizard.nodes.probeOk', {
                      os: row.probe.os ?? '',
                      osVersion: row.probe.osVersion ?? '',
                      arch: row.probe.arch ?? '',
                      cpus: row.probe.cpus ?? 0,
                      mem: Math.round((row.probe.memMib ?? 0) / 1024),
                      disk: Math.round((row.probe.diskRootMib ?? 0) / 1024),
                      sudo: SUDO_LABEL[row.probe.sudo ?? 'unknown'] ?? (row.probe.sudo ?? ''),
                    })}
                  </Typography.Text>
                );
              }
              return <Typography.Text type="secondary">—</Typography.Text>;
            },
          },
          {
            title: t('wizard.nodes.column.actions'),
            key: 'actions',
            width: 140,
            render: (_, row) => (
              <Space>
                <Button
                  size="small"
                  onClick={() => probeRow(row.key)}
                  disabled={row.address === ''}
                  data-testid={`wizard-probe-${row.key}`}
                >
                  {row.probe?.state === 'probing'
                    ? t('wizard.nodes.probing')
                    : t('wizard.nodes.probe')}
                </Button>
                <Button
                  size="small"
                  danger
                  onClick={() => setNodes(form.nodes.filter((n) => n.key !== row.key))}
                >
                  {t('wizard.nodes.removeNode')}
                </Button>
              </Space>
            ),
          },
        ]}
      />
      <Space style={{ marginTop: 12 }}>
        <Button
          onClick={() => setNodes([...form.nodes, buildNodeRow({})])}
          data-testid="wizard-add-node"
        >
          {t('wizard.nodes.addNode')}
        </Button>
        <Button onClick={probeAll} disabled={form.nodes.length === 0}>
          {t('wizard.nodes.probeAll')}
        </Button>
      </Space>
    </Card>
  );
}

/* -------------------------------- 第 4 步：认证 -------------------------------- */

export function AuthStep({ form, updateForm }: StepProps) {
  return (
    <Card>
      <Form layout="vertical">
        <Space size="large" wrap>
          <Form.Item label={t('wizard.auth.user')} style={{ minWidth: 200 }}>
            <Input
              value={form.sshUser}
              onChange={(e) => updateForm({ sshUser: e.target.value })}
              data-testid="wizard-ssh-user"
            />
          </Form.Item>
          <Form.Item label={t('wizard.auth.port')}>
            <InputNumber
              min={1}
              max={65535}
              value={form.sshPort === '' ? undefined : Number(form.sshPort)}
              onChange={(v) => updateForm({ sshPort: v == null ? '' : String(v) })}
              data-testid="wizard-ssh-port"
            />
          </Form.Item>
        </Space>
        <Form.Item label={t('wizard.auth.method')}>
          <Radio.Group
            value={form.authMethod}
            onChange={(e) => updateForm({ authMethod: e.target.value })}
            data-testid="wizard-auth-method"
          >
            <Radio value="key">{t('wizard.auth.method.key')}</Radio>
            <Radio value="password-env">{t('wizard.auth.method.password-env')}</Radio>
            <Radio value="password-file">{t('wizard.auth.method.password-file')}</Radio>
          </Radio.Group>
        </Form.Item>
        {form.authMethod === 'key' ? (
          <>
            <Form.Item label={t('wizard.auth.keyPath')}>
              <Input
                value={form.keyPath}
                onChange={(e) => updateForm({ keyPath: e.target.value })}
                placeholder="/root/.ssh/id_ed25519"
                data-testid="wizard-key-path"
              />
            </Form.Item>
            <Form.Item label={t('wizard.auth.keyPassphraseEnv')}>
              <Input
                value={form.keyPassphraseEnv}
                onChange={(e) => updateForm({ keyPassphraseEnv: e.target.value })}
              />
            </Form.Item>
          </>
        ) : null}
        {form.authMethod === 'password-env' ? (
          <Form.Item label={t('wizard.auth.passwordEnv')} extra={t('wizard.auth.passwordEnvExtra')}>
            <Input
              value={form.passwordEnv}
              onChange={(e) => updateForm({ passwordEnv: e.target.value })}
              placeholder="KURIN_SSH_PASSWORD"
              data-testid="wizard-password-env"
            />
          </Form.Item>
        ) : null}
        {form.authMethod === 'password-file' ? (
          <Form.Item label={t('wizard.auth.passwordFile')} extra={t('wizard.auth.passwordFileExtra')}>
            <Input
              value={form.passwordFile}
              onChange={(e) => updateForm({ passwordFile: e.target.value })}
              data-testid="wizard-password-file"
            />
          </Form.Item>
        ) : null}
        <Form.Item label={t('wizard.auth.become')} extra={t('wizard.auth.becomeExtra')}>
          <Checkbox
            checked={form.become}
            onChange={(e) => updateForm({ become: e.target.checked })}
            data-testid="wizard-become"
          >
            sudo
          </Checkbox>
        </Form.Item>
        {form.become ? (
          <Form.Item label={t('wizard.auth.becomePasswordEnv')}>
            <Input
              value={form.becomePasswordEnv}
              onChange={(e) => updateForm({ becomePasswordEnv: e.target.value })}
            />
          </Form.Item>
        ) : null}
        <Form.Item label={t('wizard.auth.knownHosts')}>
          <Select
            value={form.knownHosts}
            onChange={(v) => updateForm({ knownHosts: v })}
            options={[
              { value: 'tofu', label: t('wizard.auth.knownHosts.tofu') },
              { value: 'strict', label: t('wizard.auth.knownHosts.strict') },
              { value: 'accept-new', label: t('wizard.auth.knownHosts.accept-new') },
            ]}
            style={{ width: 220 }}
          />
        </Form.Item>
      </Form>
      <Alert type="info" showIcon message={t('wizard.auth.note')} />
    </Card>
  );
}

/* ---------------------------- 第 5 步：网络与 HA ---------------------------- */

export function NetworkStep({ form, updateForm, capabilities, offlineSelected }: StepProps) {
  const cniBlocked = (componentId: string) =>
    optionDisabled(capabilities, componentId, form.distribution, offlineSelected);
  return (
    <Card>
      <Form layout="vertical">
        <Space size="large" wrap>
          <Form.Item label={t('wizard.network.podCidr')}>
            <Input
              value={form.podCidr}
              onChange={(e) => updateForm({ podCidr: e.target.value })}
              data-testid="wizard-pod-cidr"
            />
          </Form.Item>
          <Form.Item label={t('wizard.network.serviceCidr')}>
            <Input
              value={form.serviceCidr}
              onChange={(e) => updateForm({ serviceCidr: e.target.value })}
              data-testid="wizard-service-cidr"
            />
          </Form.Item>
          <Form.Item label={t('wizard.network.dnsDomain')}>
            <Input
              value={form.dnsDomain}
              onChange={(e) => updateForm({ dnsDomain: e.target.value })}
            />
          </Form.Item>
        </Space>
        <Form.Item label={t('wizard.network.cni')}>
          <Space wrap>
            {CNI_OPTIONS.map((opt) => {
              const blocked = cniBlocked(opt.componentId);
              return (
                <Card.Grid
                  key={opt.value}
                  style={{ width: 180, cursor: blocked == null ? 'pointer' : 'not-allowed', opacity: blocked == null ? 1 : 0.5 }}
                >
                  <Radio
                    checked={form.cni === opt.value}
                    disabled={blocked != null}
                    onChange={() => updateForm({ cni: opt.value })}
                    data-testid={`wizard-cni-${opt.value}`}
                  >
                    {t(opt.titleKey)}
                    {blocked != null ? `（${t(blocked)}）` : ''}
                  </Radio>
                </Card.Grid>
              );
            })}
          </Space>
        </Form.Item>
        <Form.Item label={t('wizard.network.haMode')}>
          <Radio.Group
            value={form.haMode}
            onChange={(e) => updateForm({ haMode: e.target.value })}
          >
            {HA_OPTIONS.map((opt) => {
              const blocked = optionDisabled(capabilities, opt.componentId, form.distribution, offlineSelected);
              return (
                <Radio.Button
                  key={opt.value}
                  value={opt.value}
                  disabled={blocked != null}
                  data-testid={`wizard-ha-${opt.value}`}
                >
                  {t(`wizard.network.ha.${opt.value}` as never)}
                  {blocked != null ? `（${t(blocked)}）` : ''}
                </Radio.Button>
              );
            })}
          </Radio.Group>
        </Form.Item>
        {form.haMode !== 'none' ? (
          <Form.Item label={t('wizard.network.vip')}>
            <Input
              value={form.vip}
              onChange={(e) => updateForm({ vip: e.target.value })}
              placeholder="192.168.10.100"
              style={{ width: 220 }}
              data-testid="wizard-vip"
            />
          </Form.Item>
        ) : null}
      </Form>
    </Card>
  );
}

/* -------------------------------- 第 6 步：组件 -------------------------------- */

export function ComponentsStep({ form, updateForm, capabilities, catalog, offlineSelected }: StepProps) {
  const selectedNames = Object.keys(form.addons);
  const required = useMemo(
    () => addonClosure(catalog, selectedNames),
    [catalog, selectedNames],
  );

  const toggleAddon = (entry: AddonCatalogEntry, enabled: boolean) => {
    const next = { ...form.addons };
    if (enabled) {
      next[entry.name] = { name: entry.name, values: {} };
    } else {
      delete next[entry.name];
    }
    updateForm({ addons: next });
  };

  const setAddonValues = (name: string, values: Record<string, unknown>) => {
    updateForm({ addons: { ...form.addons, [name]: { name, values } } });
  };

  return (
    <Card>
      <Form layout="vertical">
        <Form.Item label={t('wizard.components.registry')}>
          <Radio.Group
            value={form.registryChoice}
            onChange={(e) => updateForm({ registryChoice: e.target.value })}
            data-testid="wizard-registry"
          >
            {REGISTRY_OPTIONS.map((opt) => {
              const blocked = optionDisabled(capabilities, opt.componentId, form.distribution, offlineSelected);
              return (
                <Radio.Button
                  key={opt.value}
                  value={opt.value}
                  disabled={blocked != null}
                  data-testid={`wizard-registry-${opt.value}`}
                >
                  {t(`wizard.components.registry.${opt.value}` as never)}
                  {blocked != null ? `（${t(blocked)}）` : ''}
                </Radio.Button>
              );
            })}
          </Radio.Group>
        </Form.Item>
        {form.registryChoice === 'external' ? (
          <Form.Item label={t('wizard.components.externalAddress')}>
            <Input
              value={form.externalRegistryAddress}
              onChange={(e) => updateForm({ externalRegistryAddress: e.target.value })}
              placeholder="https://registry.example.com:5000"
              data-testid="wizard-registry-address"
            />
          </Form.Item>
        ) : null}
      </Form>
      <Typography.Title level={5}>{t('wizard.components.addons')}</Typography.Title>
      <Space direction="vertical" size={12} style={{ width: '100%' }}>
        {catalog.map((entry) => {
          const conflicts = addonConflictsWith(catalog, selectedNames, entry.name);
          const autoRequired = !selectedNames.includes(entry.name) && required.has(entry.name);
          const disabled = conflicts.length > 0;
          const checked = selectedNames.includes(entry.name) || autoRequired;
          return (
            <Card
              key={entry.name}
              size="small"
              data-testid={`wizard-addon-${entry.name}`}
              title={
                <Space>
                  <Checkbox
                    checked={checked}
                    disabled={disabled}
                    onChange={(e) => toggleAddon(entry, e.target.checked)}
                    data-testid={`wizard-addon-check-${entry.name}`}
                  >
                    {entry.title ?? entry.name}
                  </Checkbox>
                  <Tag>{entry.version}</Tag>
                  <Tag>{entry.category}</Tag>
                  {disabled ? (
                    <Typography.Text type="danger">
                      {t('wizard.components.addonConflict', { name: conflicts.join(', ') })}
                    </Typography.Text>
                  ) : null}
                  {autoRequired ? (
                    <Typography.Text type="secondary">
                      {t('wizard.components.addonAutoCheck', { name: entry.name })}
                    </Typography.Text>
                  ) : null}
                </Space>
              }
            >
              <Typography.Paragraph type="secondary" style={{ marginBottom: 8 }}>
                {entry.description}
              </Typography.Paragraph>
              {checked && !disabled ? (
                <AddonValuesForm
                  schema={entry.values_schema}
                  values={form.addons[entry.name]?.values ?? {}}
                  onChange={(values) => setAddonValues(entry.name, values)}
                />
              ) : null}
            </Card>
          );
        })}
        {catalog.length === 0 ? (
          <Typography.Text type="secondary">{t('common.empty')}</Typography.Text>
        ) : null}
      </Space>
    </Card>
  );
}

/* -------------------------------- 第 7 步：离线 -------------------------------- */

export function OfflineStep({ form, updateForm, packages }: StepProps) {
  const selected = (packages ?? []).find((p) => p.id === form.packageId) ?? null;
  const archs = useMemo(() => {
    const set = new Set<string>();
    for (const node of form.nodes) {
      if (node.probe?.state === 'ok' && node.probe.arch) set.add(node.probe.arch);
    }
    return [...set];
  }, [form.nodes]);
  const coverage = checkPackageCoverage(selected, form, archs);
  return (
    <Card>
      <Form layout="vertical">
        <Form.Item label={t('wizard.offline.mode')}>
          <Radio.Group
            value={form.packageMode}
            onChange={(e) => updateForm({ packageMode: e.target.value })}
            data-testid="wizard-package-mode"
          >
            <Radio.Button value="online" data-testid="wizard-package-online">
              {t('wizard.offline.mode.online')}
            </Radio.Button>
            <Radio.Button value="existing" data-testid="wizard-package-existing">
              {t('wizard.offline.mode.existing')}
            </Radio.Button>
            <Radio.Button value="uri" data-testid="wizard-package-uri">
              {t('wizard.offline.mode.uri')}
            </Radio.Button>
          </Radio.Group>
        </Form.Item>
        {form.packageMode === 'existing' ? (
          <Form.Item label={t('wizard.offline.selectPackage')}>
            <Select
              value={form.packageId ?? undefined}
              onChange={(v) => updateForm({ packageId: v })}
              options={(packages ?? []).map((p) => ({
                value: p.id,
                label: `${p.name} ${p.version} (${p.distribution}, ${p.arch.join('/')})`,
              }))}
              placeholder={(packages ?? []).length === 0 ? t('wizard.offline.noPackage') : undefined}
              style={{ width: 480 }}
              data-testid="wizard-package-select"
            />
          </Form.Item>
        ) : null}
        {form.packageMode === 'uri' ? (
          <Form.Item label={t('wizard.offline.packageUri')} extra={t('wizard.offline.packageUriExtra')}>
            <Input
              value={form.packageUri}
              onChange={(e) => updateForm({ packageUri: e.target.value })}
              placeholder="file:///media/usb/kurin-offline-v1.33.1.tar.zst"
              style={{ width: 480 }}
              data-testid="wizard-package-uri-input"
            />
          </Form.Item>
        ) : null}
      </Form>
      {form.packageMode !== 'online' && selected != null ? (
        <Card type="inner" title={t('wizard.offline.coverage')}>
          {coverage.length === 0 ? (
            <Alert type="success" showIcon message={t('wizard.valid')} />
          ) : (
            coverage.map((issue, i) => (
              <Alert
                key={i}
                type={issue.severity === 'error' ? 'error' : 'warning'}
                showIcon
                message={t(issue.messageKey, issue.vars ?? {})}
                style={{ marginBottom: 8 }}
              />
            ))
          )}
        </Card>
      ) : null}
    </Card>
  );
}

/* -------------------------------- 第 8 步：检查 -------------------------------- */

export interface ReviewState {
  precheck: PrecheckReportResult | null;
  precheckError: string | null;
  precheckRunning: boolean;
  plan: PlanResult | null;
  planError: string | null;
  planRunning: boolean;
  acknowledged: boolean;
}

export function ReviewStep({
  form,
  toml,
  state,
  setState,
}: {
  form: WizardForm;
  toml: string;
  state: ReviewState;
  setState: (patch: Partial<ReviewState>) => void;
}) {
  const failed = state.precheck?.failed ?? 0;
  const warned = state.precheck?.warned ?? 0;
  const total = state.precheck?.checks.length ?? 0;
  const ok = total - failed - warned;
  const hasFailures = failed > 0 || state.precheckError != null;
  const needsAck = warned > 0;

  const runPrecheck = async () => {
    // 渲染/校验尚未产出 TOML 时不能登记（空文档会被服务端拒绝）。
    if (toml === '') {
      setState({ precheckError: t('wizard.review.precheckWaiting') });
      return;
    }
    setState({ precheckRunning: true, precheckError: null });
    try {
      const { rev } = await ensureClusterRegistered(form.name, toml);
      const submitted = await submitOperation(form.name, 'precheck');
      const outcome = await waitForOperation(submitted.id);
      if (outcome.state !== 'succeeded') {
        setState({ precheckRunning: false, precheckError: outcome.error ?? outcome.state });
        return;
      }
      setState({
        precheckRunning: false,
        precheck: (outcome.result as PrecheckReportResult) ?? null,
      });
      void rev;
    } catch (error) {
      setState({
        precheckRunning: false,
        precheckError: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const runPlan = async () => {
    setState({ planRunning: true, planError: null });
    try {
      await ensureClusterRegistered(form.name, toml);
      const submitted = await submitPlan(form.name, false);
      const outcome = await waitForOperation(submitted.operation);
      if (outcome.state !== 'succeeded') {
        setState({ planRunning: false, planError: outcome.error ?? outcome.state });
        return;
      }
      setState({ planRunning: false, plan: (outcome.result as PlanResult) ?? null });
    } catch (error) {
      setState({
        planRunning: false,
        planError: error instanceof Error ? error.message : String(error),
      });
    }
  };

  return (
    <Card>
      <Space direction="vertical" size={16} style={{ width: '100%' }}>
        <Card
          type="inner"
          title={t('wizard.review.precheck')}
          extra={
            <Button
              onClick={runPrecheck}
              loading={state.precheckRunning}
              disabled={toml === ''}
              data-testid="wizard-precheck-run"
            >
              {t('wizard.review.precheckRun')}
            </Button>
          }
        >
          {state.precheckError ? (
            <Alert type="error" showIcon message={state.precheckError} />
          ) : null}
          {state.precheck ? (
            <>
              <Alert
                type={failed > 0 ? 'error' : warned > 0 ? 'warning' : 'success'}
                showIcon
                message={
                  failed > 0
                    ? t('wizard.review.precheckFailed', { fail: failed })
                    : t('wizard.review.precheckPassed', { ok, n: total, warn: warned })
                }
                style={{ marginBottom: 12 }}
              />
              <Table
                rowKey={(r: { node: string; check: string }) => `${r.node}/${r.check}`}
                size="small"
                pagination={false}
                dataSource={state.precheck.checks}
                data-testid="wizard-precheck-table"
              >
                <Table.Column title={t('wizard.nodes.column.name')} dataIndex="node" />
                <Table.Column title={t('operationDetail.grid.stage')} dataIndex="check" />
                <Table.Column
                  title={t('operations.column.state')}
                  dataIndex="status"
                  render={(v: string) => (
                    <Tag color={v === 'ok' ? 'green' : v === 'warn' ? 'orange' : 'red'}>{v}</Tag>
                  )}
                />
                <Table.Column
                  title={t('operationDetail.resources.column.detail')}
                  dataIndex="detail"
                />
              </Table>
              {needsAck ? (
                <Checkbox
                  checked={state.acknowledged}
                  onChange={(e) => setState({ acknowledged: e.target.checked })}
                  style={{ marginTop: 12 }}
                  data-testid="wizard-ack-warnings"
                >
                  {t('wizard.review.acknowledge')}
                </Checkbox>
              ) : null}
            </>
          ) : (
            <Typography.Text type="secondary">{t('wizard.review.precheckRun')}</Typography.Text>
          )}
        </Card>
        <Card
          type="inner"
          title={t('wizard.review.plan')}
          extra={
            <Button onClick={runPlan} loading={state.planRunning} data-testid="wizard-plan-run">
              {t('wizard.review.planRun')}
            </Button>
          }
        >
          {state.planError ? <Alert type="error" showIcon message={state.planError} /> : null}
          {state.plan ? (
            <Table
              rowKey={(r: { key: string }) => r.key}
              size="small"
              pagination={false}
              dataSource={state.plan.phases.flatMap((phase) => phase.tasks)}
              data-testid="wizard-plan-table"
            >
              <Table.Column
                title={t('operationDetail.grid.stage')}
                render={(_: unknown, row: { key: string }) => row.key.split('/')[0]}
              />
              <Table.Column
                title={t('wizard.nodes.column.name')}
                render={(_: unknown, row: { node: string }) => row.node}
              />
              <Table.Column dataIndex="describe" />
            </Table>
          ) : (
            <Typography.Text type="secondary">{t('wizard.review.emptyPlan')}</Typography.Text>
          )}
        </Card>
      </Space>
      {hasFailures ? (
        <Alert
          type="error"
          showIcon
          message={t('wizard.review.createBlocked')}
          style={{ marginTop: 16 }}
        />
      ) : null}
    </Card>
  );
}

/* -------------------------------- 第 9 步：确认 -------------------------------- */

export function ConfirmStep({
  form,
  toml,
  tomlMode,
  tomlText,
  onEnterTomlMode,
  onTomlChange,
  addonNames,
  packageLabel,
}: {
  form: WizardForm;
  toml: string;
  tomlMode: boolean;
  tomlText: string;
  onEnterTomlMode: () => void;
  onTomlChange: (text: string) => void;
  addonNames: string[];
  packageLabel: string;
}) {
  const [confirmOpen, setConfirmOpen] = useState(false);
  const download = () => {
    const blob = new Blob([tomlMode ? tomlText : toml], { type: 'application/toml' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `${form.name || 'cluster'}.kurin.toml`;
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <Card>
      <Card type="inner" title={t('wizard.confirm.summary')} style={{ marginBottom: 16 }}>
        <Descriptions column={2} size="small" bordered data-testid="wizard-summary">
          <Descriptions.Item label={t('wizard.confirm.field.name')}>
            <Typography.Text strong>{form.name}</Typography.Text>
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.distribution')}>
            {form.distribution}
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.k8sVersion')}>
            {form.kubernetesVersion}
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.runtime')}>{form.runtime}</Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.zone')}>{form.zone}</Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.cni')}>{form.cni}</Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.ha')}>
            {form.haMode === 'none' ? t('cluster.ha.none') : form.haMode}
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.vip')}>{form.vip || '—'}</Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.podCidr')}>{form.podCidr}</Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.serviceCidr')}>
            {form.serviceCidr}
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.nodes')} span={2}>
            {form.nodes.map((node) => (
              <Tag key={node.key}>
                {node.name}（{node.address} · {t(`wizard.nodes.role.${node.role}` as never)}）
              </Tag>
            ))}
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.addons')} span={2}>
            {addonNames.length === 0 ? '—' : addonNames.join(', ')}
          </Descriptions.Item>
          <Descriptions.Item label={t('wizard.confirm.field.package')} span={2}>
            {packageLabel}
          </Descriptions.Item>
        </Descriptions>
      </Card>
      <Card
        type="inner"
        title={tomlMode ? t('wizard.confirm.tomlMode') : t('wizard.confirm.toml')}
        extra={
          <Space>
            <Button icon={<DownloadOutlined />} onClick={download} data-testid="wizard-download-toml">
              {t('wizard.confirm.download')}
            </Button>
            {!tomlMode ? (
              <Button
                danger
                onClick={() => setConfirmOpen(true)}
                data-testid="wizard-advanced-toml"
              >
                {t('wizard.confirm.advanced')}
              </Button>
            ) : null}
          </Space>
        }
      >
        {tomlMode ? (
          <Input.TextArea
            value={tomlText}
            onChange={(e) => onTomlChange(e.target.value)}
            rows={20}
            style={{ fontFamily: 'monospace' }}
            data-testid="wizard-toml-editor"
          />
        ) : (
          <Input.TextArea
            value={toml}
            readOnly
            rows={20}
            style={{ fontFamily: 'monospace' }}
            data-testid="wizard-toml-preview"
          />
        )}
      </Card>
      <Modal
        open={confirmOpen}
        title={t('wizard.confirm.advancedConfirmTitle')}
        okText={t('wizard.confirm.advancedConfirmOk')}
        cancelText={t('common.cancel')}
        okButtonProps={{ danger: true }}
        onOk={() => {
          setConfirmOpen(false);
          onEnterTomlMode();
        }}
        onCancel={() => setConfirmOpen(false)}
      >
        <Typography.Paragraph>{t('wizard.confirm.advancedConfirmContent')}</Typography.Paragraph>
      </Modal>
    </Card>
  );
}
