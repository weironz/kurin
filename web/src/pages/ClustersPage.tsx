import { useEffect } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { Button, Empty, Result, Skeleton, Table, Tag, Tooltip, Typography } from 'antd';
import { PlusOutlined } from '@ant-design/icons';
import type { ClusterSummary } from '../api/types';
import { clustersQuery } from '../api/queries';
import { t, type MessageKey } from '../i18n/zh-CN';
import { formatAbsolute, formatRelative } from '../utils/format';
import { isActiveOperation, operationKindLabel } from '../utils/operation';

/** 健康徽标五态（UI §8.1）。 */
const healthStyle: Record<ClusterSummary['health'], { color: string; labelKey: MessageKey }> = {
  healthy: { color: 'green', labelKey: 'cluster.health.healthy' },
  degraded: { color: 'orange', labelKey: 'cluster.health.degraded' },
  unreachable: { color: 'red', labelKey: 'cluster.health.unreachable' },
  stale: { color: 'gold', labelKey: 'cluster.health.stale' },
  unknown: { color: 'default', labelKey: 'cluster.health.unknown' },
};

/** 集群总览 `/`（UI §8.1）：15 秒轮询快照；「新建集群」进入 WP-62 向导。 */
export default function ClustersPage() {
  const navigate = useNavigate();
  const { data, isLoading, isFetching, error, refetch } = useQuery(clustersQuery);

  useEffect(() => {
    document.title = `${t('page.clusters.title')} · kurin`;
  }, []);


  const errorMessage = error instanceof Error ? error.message : String(error);

  return (
    <div>
      <Typography.Title level={3}>{t('page.clusters.title')}</Typography.Title>
      <Typography.Paragraph type="secondary">{t('page.clusters.description')}</Typography.Paragraph>
      {isLoading ? (
        <Skeleton active title={false} paragraph={{ rows: 6 }} />
      ) : error ? (
        <Result
          status="error"
          title={t('clusters.error.title')}
          subTitle={errorMessage}
          extra={
            <Button type="primary" onClick={() => void refetch()}>
              {t('common.retry')}
            </Button>
          }
        />
      ) : (data?.length ?? 0) === 0 ? (
        <Empty
          image={Empty.PRESENTED_IMAGE_SIMPLE}
          description={
            <>
              <div>{t('clusters.empty.title')}</div>
              <Typography.Text type="secondary" style={{ fontSize: 13 }}>
                {t('clusters.empty.description')}
              </Typography.Text>
            </>
          }
        >
          <Button type="primary" icon={<PlusOutlined />} onClick={() => navigate('/clusters/new')}>
            {t('clusters.empty.create')}
          </Button>
        </Empty>
      ) : (
        <Table<ClusterSummary>
          rowKey="name"
          loading={isFetching}
          dataSource={data ?? []}
          pagination={false}
          size="middle"
        >
          <Table.Column<ClusterSummary>
            title={t('clusters.column.name')}
            dataIndex="name"
            render={(_, r) => <Link to={`/clusters/${r.name}`}>{r.name}</Link>}
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.distribution')}
            dataIndex="distribution"
            render={(v: ClusterSummary['distribution']) => <Tag>{v}</Tag>}
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.k8sVersion')}
            dataIndex="kubernetesVersion"
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.nodes')}
            render={(_, r) =>
              r.nodesReady == null ? '—' : `${r.nodesReady}/${r.nodesTotal ?? '—'}`
            }
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.health')}
            dataIndex="health"
            render={(v: ClusterSummary['health']) => (
              <Tag color={healthStyle[v].color}>{t(healthStyle[v].labelKey)}</Tag>
            )}
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.certMinDays')}
            dataIndex="certMinDays"
            render={(v: number | null) =>
              v == null ? (
                '—'
              ) : (
                <Typography.Text type={v < 30 ? 'danger' : undefined}>
                  {t('clusters.certDays', { n: v })}
                </Typography.Text>
              )
            }
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.observedAt')}
            dataIndex="observedAt"
            render={(v: string | null) =>
              v ? (
                <Tooltip title={formatAbsolute(v)}>
                  <span>{formatRelative(v)}</span>
                </Tooltip>
              ) : (
                '—'
              )
            }
          />
          <Table.Column<ClusterSummary>
            title={t('clusters.column.operation')}
            render={(_, r) =>
              isActiveOperation(r.activeOperation) ? (
                <Link to={`/operations/${r.activeOperation.id}`}>
                  {operationKindLabel(r.activeOperation.kind)}
                </Link>
              ) : (
                '—'
              )
            }
          />
        </Table>
      )}
    </div>
  );
}
