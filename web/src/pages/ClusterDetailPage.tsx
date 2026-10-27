import { useEffect } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import {
  Alert,
  Button,
  Card,
  Descriptions,
  Empty,
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
import type {
  ClusterDetail,
  ClusterSnapshot,
  NodeSpec,
  ObservedNode,
  OperationList,
  OperationSummary,
} from '../api/types';
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

function NodesTab({ detail }: { detail: ClusterDetail }) {
  const rows = mergeNodes(detail.desiredNodes, detail.observedNodes);
  return (
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
  );
}

/* --------------------------------- 配置 ---------------------------------- */

function ConfigTab({ detail }: { detail: ClusterDetail }) {
  const s = detail.configSummary;
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
      {/* CodeMirror 6 TOML 编辑器由 WP-65 提供（懒加载），此处是占位。 */}
      <Card size="small" title={t('clusterDetail.config.editorTitle')}>
        <Typography.Paragraph type="secondary" style={{ marginBottom: 0 }}>
          {t('clusterDetail.config.editorPlaceholder')}
        </Typography.Paragraph>
      </Card>
    </div>
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

/** 集群详情 `/clusters/:name`（UI §8.1）：概览 / 节点 / 配置 / 操作历史。 */
export default function ClusterDetailPage() {
  const { name } = useParams<{ name: string }>();
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ['cluster', name],
    queryFn: () => apiFetch<ClusterDetail>(`/clusters/${encodeURIComponent(name ?? '')}`),
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

  return (
    <div>
      <Typography.Title level={3} style={{ marginBottom: 4 }}>
        {t('page.clusterDetail.title')}
      </Typography.Title>
      <Space align="center" style={{ marginBottom: 8 }}>
        <Typography.Text code>{name}</Typography.Text>
        <Typography.Text type="secondary">{t('page.clusterDetail.description')}</Typography.Text>
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
                key: 'operations',
                label: t('clusterDetail.tab.operations'),
                children: <OperationsTab name={name} />,
              },
            ]}
          />
        </>
      ) : null}
    </div>
  );
}
