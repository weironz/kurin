import { Suspense, lazy, useEffect, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Alert,
  App,
  Button,
  Card,
  Checkbox,
  Descriptions,
  Empty,
  Input,
  Modal,
  Result,
  Skeleton,
  Space,
  Table,
  Tabs,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import { ApiError, apiFetch } from '../api/client';
import { fetchClusterDetail } from '../api/clusters';
import type {
  ClusterDetail,
  ClusterSnapshot,
  NodeSpec,
  ObservedNode,
  OperationList,
  OperationSummary,
} from '../api/types';
import { renewCertificates } from '../api/maintenance';
import { addClusterNodes, receiptOperationId } from '../api/addons';
import { parseNodes } from '../api/wizard';
import { t, type MessageKey } from '../i18n/zh-CN';
import {
  formatAbsolute,
  formatDurationBetween,
  formatDurationSince,
  formatRelative,
} from '../utils/format';
import {
  isActiveOperation,
  operationKindLabel,
  operationStateColor,
  operationStateLabel,
} from '../utils/operation';
import { ClusterDangerZone } from './ClusterDangerZone';
import { ClusterUpgradeDialog } from './ClusterUpgradeDialog';

// 动态导入：CodeMirror 只进编辑器的懒 chunk（UI §8.4 路由级代码分割同理）。
const TomlEditor = lazy(() => import('../config/TomlEditor'));

/* ------------------------- 节点：期望 vs 观测合并 ------------------------- */

/** 合并后的节点行（UI §8.1「节点」Tab）。 */
interface MergedNodeRow {
  name: string;
  /** 配置修订中声明的节点；观测到但未声明时为 null。 */
  desired: NodeSpec | null;
  /** 快照观测到的节点；声明了但快照缺失时为 null。 */
  observed: ObservedNode | null;
}

/** 按名称合并期望与观测节点：期望在前（配置顺序），未声明的观测节点追加在后。 */
function mergeNodes(desired: NodeSpec[], observed: ObservedNode[]): MergedNodeRow[] {
  const rows = new Map<string, MergedNodeRow>();
  for (const node of desired) {
    rows.set(node.name, { name: node.name, desired: node, observed: null });
  }
  for (const node of observed) {
    const row = rows.get(node.name);
    if (row) {
      row.observed = node;
    } else {
      rows.set(node.name, { name: node.name, desired: null, observed: node });
    }
  }
  return [...rows.values()];
}

/** 生命周期标签（UI §6.7）：颜色与文案集中在这里。 */
const lifecycleMeta: Record<
  ClusterDetail['lifecycle'],
  { color: string; labelKey: MessageKey }
> = {
  draft: { color: 'default', labelKey: 'cluster.lifecycle.draft' },
  provisioning: { color: 'processing', labelKey: 'cluster.lifecycle.provisioning' },
  active: { color: 'green', labelKey: 'cluster.lifecycle.active' },
  deleting: { color: 'orange', labelKey: 'cluster.lifecycle.deleting' },
  deleted: { color: 'red', labelKey: 'cluster.lifecycle.deleted' },
  archived: { color: 'default', labelKey: 'cluster.lifecycle.archived' },
};

const roleLabel: Record<NodeSpec['role'], MessageKey> = {
  'control-plane': 'cluster.role.control-plane',
  worker: 'cluster.role.worker',
  lb: 'cluster.role.lb',
};

const etcdStyle: Record<
  NonNullable<ClusterSnapshot['etcdHealth']>,
  { color: string; labelKey: MessageKey }
> = {
  ok: { color: 'green', labelKey: 'cluster.etcd.ok' },
  degraded: { color: 'orange', labelKey: 'cluster.etcd.degraded' },
  unreachable: { color: 'red', labelKey: 'cluster.etcd.unreachable' },
};

function ReadyTag({ observed }: { observed: ObservedNode | null }) {
  if (!observed) return <>—</>;
  return observed.ready ? (
    <Tag color="green">{t('cluster.node.ready')}</Tag>
  ) : (
    <Tag color="red">{t('cluster.node.notReady')}</Tag>
  );
}

/* --------------------------------- 概览 ---------------------------------- */

function OverviewTab({ detail }: { detail: ClusterDetail }) {
  const snap = detail.snapshot;
  return (
    <div>
      {detail.collectError && (
        <Alert
          type="error"
          showIcon
          message={t('clusterDetail.collectError')}
          description={detail.collectError}
          style={{ marginBottom: 16 }}
        />
      )}
      {snap ? (
        <Card size="small" style={{ marginBottom: 16 }}>
          <Descriptions
            bordered
            size="small"
            column={{ xs: 1, sm: 2, md: 3, lg: 5 }}
            items={[
              {
                key: 'observedAt',
                label: t('clusterDetail.overview.observedAt'),
                children: snap.observedAt ? (
                  <Tooltip title={formatAbsolute(snap.observedAt)}>
                    <span>{formatRelative(snap.observedAt)}</span>
                  </Tooltip>
                ) : (
                  '—'
                ),
              },
              {
                key: 'nodes',
                label: t('clusterDetail.overview.nodesReady'),
                children: `${snap.nodesReady}/${snap.nodesTotal}`,
              },
              {
                key: 'podsNotRunning',
                label: t('clusterDetail.overview.podsNotRunning'),
                children: snap.podsNotRunning ?? '—',
              },
              {
                key: 'etcd',
                label: t('clusterDetail.overview.etcd'),
                children: snap.etcdHealth ? (
                  <Tag color={etcdStyle[snap.etcdHealth].color}>
                    {t(etcdStyle[snap.etcdHealth].labelKey)}
                  </Tag>
                ) : (
                  '—'
                ),
              },
              {
                key: 'cert',
                label: t('clusterDetail.overview.certMinDays'),
                children:
                  snap.certMinDays == null ? '—' : t('clusters.certDays', { n: snap.certMinDays }),
              },
            ]}
          />
        </Card>
      ) : (
        <Empty
          image={Empty.PRESENTED_IMAGE_SIMPLE}
          description={t('clusterDetail.noSnapshot')}
          style={{ marginBottom: 16 }}
        />
      )}
      <Card size="small" title={t('clusterDetail.overview.nodeTable')}>
        <Table<ObservedNode>
          rowKey="name"
          dataSource={detail.observedNodes}
          pagination={false}
          size="small"
          locale={{ emptyText: t('clusterDetail.overview.noNodes') }}
        >
          <Table.Column<ObservedNode>
            title={t('clusterDetail.nodes.column.name')}
            dataIndex="name"
          />
          <Table.Column<ObservedNode>
            title={t('clusterDetail.nodes.column.ready')}
            render={(_, r) => <ReadyTag observed={r} />}
          />
          <Table.Column<ObservedNode>
            title={t('clusterDetail.nodes.column.role')}
            dataIndex="roles"
          />
          <Table.Column<ObservedNode>
            title={t('clusterDetail.nodes.column.kubeletVersion')}
            dataIndex="version"
          />
          <Table.Column<ObservedNode>
            title={t('clusterDetail.nodes.column.os')}
            dataIndex="osImage"
            render={(v?: string) => v ?? '—'}
          />
          <Table.Column<ObservedNode>
            title={t('clusterDetail.nodes.column.arch')}
            dataIndex="arch"
            render={(v?: string) => v ?? '—'}
          />
        </Table>
      </Card>
    </div>
  );
}

/* --------------------------------- 节点 ---------------------------------- */

/** 「添加节点」：批量粘贴 → `/nodes/parse` → 结构化追加（服务端 toml_edit）。 */
function AddNodesModal({
  detail,
  open,
  onClose,
}: {
  detail: ClusterDetail;
  open: boolean;
  onClose: () => void;
}) {
  const navigate = useNavigate();
  const { message } = App.useApp();
  const [input, setInput] = useState('');
  const [rows, setRows] = useState<Array<{ name: string; address: string; role: string }>>([]);
  const [error, setError] = useState<string | null>(null);
  const existing = detail.desiredNodes.map((node) => node.name);

  const parse = useMutation({
    mutationFn: () => parseNodes(input, existing),
    onSuccess: (parsed) => {
      setError(null);
      setRows(
        parsed.nodes.map((node) => ({
          name: node.name,
          address: node.address,
          // 解析结果用 controlplane（无连字符），服务端用 control-plane。
          role: node.role === 'controlplane' ? 'control-plane' : node.role,
        })),
      );
    },
    onError: (err) => setError(err instanceof Error ? err.message : String(err)),
  });
  const submit = useMutation({
    mutationFn: () => addClusterNodes(detail.name, rows),
    onSuccess: async (receipt) => {
      onClose();
      message.info(t('clusterDetail.nodes.addSubmitted'));
      navigate(`/operations/${encodeURIComponent(receiptOperationId(receipt))}`);
    },
    onError: (err) => setError(err instanceof Error ? err.message : String(err)),
  });

  return (
    <Modal
      open={open}
      title={t('clusterDetail.nodes.addTitle')}
      okText={t('clusterDetail.nodes.addSubmit')}
      cancelText={t('common.cancel')}
      okButtonProps={{
        disabled: rows.length === 0,
        loading: submit.isPending,
        'data-testid': 'add-nodes-submit',
      }}
      onCancel={onClose}
      onOk={() => submit.mutate()}
      width={640}
    >
      <div data-testid="add-nodes-modal">
        <Typography.Paragraph type="secondary">
          {t('clusterDetail.nodes.addDescription')}
        </Typography.Paragraph>
        <Input.TextArea
          rows={3}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder={'192.168.10.22-23 worker'}
          data-testid="add-nodes-input"
        />
        <Space style={{ marginTop: 8 }}>
          <Button
            onClick={() => parse.mutate()}
            loading={parse.isPending}
            disabled={input.trim() === ''}
            data-testid="add-nodes-parse"
          >
            {t('clusterDetail.nodes.addParse')}
          </Button>
        </Space>
        {error != null ? (
          <Alert type="error" showIcon message={error} style={{ marginTop: 8 }} data-testid="add-nodes-error" />
        ) : null}
        {rows.length > 0 ? (
          <Table
            rowKey="name"
            size="small"
            pagination={false}
            dataSource={rows}
            style={{ marginTop: 12 }}
            data-testid="add-nodes-table"
          >
            <Table.Column title={t('clusterDetail.nodes.column.name')} dataIndex="name" />
            <Table.Column title={t('clusterDetail.nodes.column.address')} dataIndex="address" />
            <Table.Column title={t('clusterDetail.nodes.column.role')} dataIndex="role" />
          </Table>
        ) : null}
      </div>
    </Modal>
  );
}

function NodesTab({ detail }: { detail: ClusterDetail }) {
  const rows = mergeNodes(detail.desiredNodes, detail.observedNodes);
  const [addOpen, setAddOpen] = useState(false);
  const busy = isActiveOperation(detail.activeOperation);
  return (
    <div>
      <Space style={{ marginBottom: 12 }}>
        <Button
          type="primary"
          disabled={busy}
          onClick={() => setAddOpen(true)}
          data-testid="cluster-add-nodes-open"
        >
          {t('clusterDetail.nodes.add')}
        </Button>
        {busy ? <Typography.Text type="secondary">{t('danger.disabledByOperation')}</Typography.Text> : null}
      </Space>
      <AddNodesModal detail={detail} open={addOpen} onClose={() => setAddOpen(false)} />
      <Table<MergedNodeRow>
      rowKey="name"
      dataSource={rows}
      pagination={false}
      size="middle"
      locale={{ emptyText: t('clusterDetail.nodes.empty') }}
    >
      <Table.Column<MergedNodeRow> title={t('clusterDetail.nodes.column.name')} dataIndex="name" />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.status')}
        render={(_, r) =>
          !r.desired ? (
            <Tag color="orange">{t('clusterDetail.nodes.undeclared')}</Tag>
          ) : !r.observed ? (
            <Tag color="red">{t('clusterDetail.nodes.missing')}</Tag>
          ) : (
            '—'
          )
        }
      />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.role')}
        render={(_, r) => (r.desired ? t(roleLabel[r.desired.role]) : (r.observed?.roles ?? '—'))}
      />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.address')}
        render={(_, r) => r.desired?.address ?? '—'}
      />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.arch')}
        render={(_, r) => r.observed?.arch ?? '—'}
      />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.os')}
        render={(_, r) => r.observed?.osImage ?? '—'}
      />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.kubeletVersion')}
        render={(_, r) => r.observed?.version ?? '—'}
      />
      <Table.Column<MergedNodeRow>
        title={t('clusterDetail.nodes.column.ready')}
        render={(_, r) => <ReadyTag observed={r.observed} />}
      />
      </Table>
    </div>
  );
}

/* --------------------------------- 配置 ---------------------------------- */

/**
 * 配置 Tab：只读表单视图是默认；「高级编辑」显式进入 CodeMirror 6 TOML
 * 编辑器（懒加载 chunk，主 bundle 不含 CodeMirror）。
 */
function ConfigTab({ detail }: { detail: ClusterDetail }) {
  const s = detail.configSummary;
  const [advanced, setAdvanced] = useState(false);
  return (
    <div>
      <Card
        size="small"
        title={t('clusterDetail.config.summaryTitle')}
        style={{ marginBottom: 16 }}
      >
        <Descriptions
          bordered
          size="small"
          column={{ xs: 1, sm: 2, md: 3 }}
          items={[
            {
              key: 'revision',
              label: t('clusterDetail.config.revision'),
              children: `#${detail.currentRev}`,
            },
            {
              key: 'distribution',
              label: t('clusterDetail.config.field.distribution'),
              children: s.distribution,
            },
            {
              key: 'kubernetesVersion',
              label: t('clusterDetail.config.field.kubernetesVersion'),
              children: s.kubernetesVersion,
            },
            {
              key: 'runtime',
              label: t('clusterDetail.config.field.runtime'),
              children: s.runtime,
            },
            { key: 'zone', label: t('clusterDetail.config.field.zone'), children: s.zone },
            { key: 'cni', label: t('clusterDetail.config.field.cni'), children: s.cni },
            { key: 'podCidr', label: t('clusterDetail.config.field.podCidr'), children: s.podCidr },
            {
              key: 'serviceCidr',
              label: t('clusterDetail.config.field.serviceCidr'),
              children: s.serviceCidr,
            },
            {
              key: 'haMode',
              label: t('clusterDetail.config.field.haMode'),
              children: s.haMode === 'none' ? t('cluster.ha.none') : s.haMode,
            },
            {
              key: 'vip',
              label: t('clusterDetail.config.field.vip'),
              children: s.vip ?? '—',
            },
          ]}
        />
      </Card>
      {advanced ? (
        <Suspense fallback={<Skeleton active title={false} paragraph={{ rows: 8 }} />}>
          <TomlEditor cluster={detail.name} />
        </Suspense>
      ) : (
        <Card
          size="small"
          title={t('clusterDetail.config.editorTitle')}
          extra={
            <Button size="small" onClick={() => setAdvanced(true)} data-testid="cluster-config-advanced">
              {t('clusterDetail.config.advanced')}
            </Button>
          }
        >
          <Typography.Paragraph type="secondary" style={{ marginBottom: 0 }}>
            {t('clusterDetail.config.advancedHint')}
          </Typography.Paragraph>
        </Card>
      )}
    </div>
  );
}

/* --------------------------------- 证书 ---------------------------------- */

/** 证书 Tab（WP-65）：剩余有效期（快照 certMinDays）+ 续期操作。 */
function CertificatesTab({ detail }: { detail: ClusterDetail }) {
  const navigate = useNavigate();
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [force, setForce] = useState(false);
  const snap = detail.snapshot;
  const busy = isActiveOperation(detail.activeOperation);

  const renewMutation = useMutation({
    mutationFn: () => renewCertificates(detail.name, force),
    onSuccess: async (receipt) => {
      await queryClient.invalidateQueries({ queryKey: ['cluster', detail.name] });
      await queryClient.invalidateQueries({ queryKey: ['operations'] });
      message.info(t('certs.submitted'));
      navigate(`/operations/${encodeURIComponent(receiptOperationId(receipt))}`);
    },
  });

  return (
    <Card size="small" title={t('certs.title')} data-testid="certs-tab">
      <Typography.Paragraph type="secondary">{t('certs.description')}</Typography.Paragraph>
      <Descriptions
        bordered
        size="small"
        column={{ xs: 1, sm: 2 }}
        items={[
          {
            key: 'certMinDays',
            label: t('certs.certMinDays'),
            children: (
              <span data-testid="certs-min-days">
                {snap?.certMinDays == null
                  ? t('certs.notCollected')
                  : t('clusters.certDays', { n: snap.certMinDays })}
              </span>
            ),
          },
          {
            key: 'observedAt',
            label: t('clusterDetail.overview.observedAt'),
            children: snap?.observedAt ? formatAbsolute(snap.observedAt) : '—',
          },
        ]}
        style={{ marginBottom: 16 }}
      />
      <Space direction="vertical" size="small">
        <Checkbox
          checked={force}
          onChange={(e) => setForce(e.target.checked)}
          data-testid="certs-force"
        >
          {t('certs.force')}
        </Checkbox>
        <Typography.Text type="secondary">{t('certs.forceExtra')}</Typography.Text>
        <Button
          type="primary"
          danger
          disabled={busy}
          loading={renewMutation.isPending}
          onClick={() => renewMutation.mutate()}
          data-testid="certs-renew"
        >
          {t('certs.renew')}
        </Button>
      </Space>
      {renewMutation.isError ? (
        <Alert
          type="error"
          showIcon
          style={{ marginTop: 16 }}
          data-testid="certs-error"
          message={t('certs.actionFailed', {
            message: renewMutation.error instanceof Error
              ? renewMutation.error.message
              : String(renewMutation.error),
          })}
        />
      ) : null}
    </Card>
  );
}

/* ------------------------------- 操作历史 --------------------------------- */

function OperationsTab({ name }: { name: string }) {
  const { data, isLoading } = useQuery({
    queryKey: ['operations', { cluster: name }],
    queryFn: () => apiFetch<OperationList>('/operations', { params: { cluster: name } }),
  });
  return (
    <Table<OperationSummary>
      rowKey="id"
      loading={isLoading}
      dataSource={data?.items ?? []}
      pagination={false}
      size="small"
      locale={{ emptyText: t('operations.empty') }}
    >
      <Table.Column<OperationSummary>
        title={t('operations.column.id')}
        dataIndex="id"
        render={(v: string) => (
          <Link to={`/operations/${v}`}>
            <Typography.Text code>{v}</Typography.Text>
          </Link>
        )}
      />
      <Table.Column<OperationSummary>
        title={t('operations.column.state')}
        dataIndex="state"
        render={(v: OperationSummary['state']) => (
          <Tag color={operationStateColor[v]}>{operationStateLabel(v)}</Tag>
        )}
      />
      <Table.Column<OperationSummary>
        title={t('operations.column.kind')}
        dataIndex="kind"
        render={(v: OperationSummary['kind']) => operationKindLabel(v)}
      />
      <Table.Column<OperationSummary>
        title={t('operations.column.started')}
        dataIndex="startedAt"
        render={(v?: string | null) => (v ? formatAbsolute(v) : '—')}
      />
      <Table.Column<OperationSummary>
        title={t('operations.column.duration')}
        render={(_, r) =>
          formatDurationBetween(r.startedAt, r.finishedAt) ??
          formatDurationSince(r.startedAt) ??
          '—'
        }
      />
    </Table>
  );
}

/* --------------------------------- 页面 ---------------------------------- */

/** 集群详情 `/clusters/:name`（UI §8.1）：概览 / 节点 / 配置 / 证书 / 操作历史。 */
export default function ClusterDetailPage() {
  const { name } = useParams<{ name: string }>();
  const [upgradeOpen, setUpgradeOpen] = useState(false);
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ['cluster', name],
    queryFn: () => fetchClusterDetail(name ?? ''),
    enabled: name != null,
  });

  useEffect(() => {
    document.title = `${t('page.clusterDetail.title')} · kurin`;
  }, []);

  if (!name) {
    return null;
  }

  const notFound = error instanceof ApiError && error.status === 404;
  const errorMessage = error instanceof Error ? error.message : String(error);
  const busy = data != null && isActiveOperation(data.activeOperation);

  return (
    <div>
      <Typography.Title level={3} style={{ marginBottom: 4 }}>
        {t('page.clusterDetail.title')}
      </Typography.Title>
      <Space align="center" style={{ marginBottom: 8 }} wrap>
        <Typography.Text code>{name}</Typography.Text>
        {data ? (
          <Tag color={lifecycleMeta[data.lifecycle].color} data-testid="cluster-lifecycle">
            {t(lifecycleMeta[data.lifecycle].labelKey)}
          </Tag>
        ) : null}
        <Typography.Text type="secondary">{t('page.clusterDetail.description')}</Typography.Text>
        <Link to={`/clusters/${encodeURIComponent(name)}/addons`}>{t('addons.link')}</Link>
        <Button
          size="small"
          disabled={busy}
          onClick={() => setUpgradeOpen(true)}
          data-testid="cluster-upgrade-open"
        >
          {t('upgrade.action')}
        </Button>
      </Space>
      {isLoading ? (
        <Skeleton active title={false} paragraph={{ rows: 8 }} />
      ) : error ? (
        notFound ? (
          <Result
            status="404"
            title={t('clusterDetail.notFound')}
            extra={<Link to="/">{t('clusterDetail.backToList')}</Link>}
          />
        ) : (
          <Result
            status="error"
            title={t('clusterDetail.loadFailed')}
            subTitle={errorMessage}
            extra={
              <Button type="primary" onClick={() => void refetch()}>
                {t('common.retry')}
              </Button>
            }
          />
        )
      ) : data ? (
        <>
          {isActiveOperation(data.activeOperation) && (
            <Alert
              type="info"
              showIcon
              message={t('clusterDetail.activeOperation')}
              description={
                <Link to={`/operations/${data.activeOperation.id}`}>
                  {operationKindLabel(data.activeOperation.kind)} ·{' '}
                  {operationStateLabel(data.activeOperation.state)}
                </Link>
              }
              style={{ marginBottom: 16 }}
            />
          )}
          <Tabs
            defaultActiveKey="overview"
            items={[
              {
                key: 'overview',
                label: t('clusterDetail.tab.overview'),
                children: <OverviewTab detail={data} />,
              },
              {
                key: 'nodes',
                label: t('clusterDetail.tab.nodes'),
                children: <NodesTab detail={data} />,
              },
              {
                key: 'config',
                label: t('clusterDetail.tab.config'),
                children: <ConfigTab detail={data} />,
              },
              {
                key: 'certificates',
                label: t('clusterDetail.tab.certificates'),
                children: <CertificatesTab detail={data} />,
              },
              {
                key: 'operations',
                label: t('clusterDetail.tab.operations'),
                children: <OperationsTab name={name} />,
              },
              {
                key: 'danger',
                label: t('clusterDetail.tab.danger'),
                children: <ClusterDangerZone detail={data} />,
              },
            ]}
          />
          <ClusterUpgradeDialog
            cluster={name}
            currentVersion={data.kubernetesVersion}
            open={upgradeOpen}
            busy={busy}
            onClose={() => setUpgradeOpen(false)}
          />
        </>
      ) : null}
    </div>
  );
}
