import { useEffect } from 'react';
import { useLocation } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { Table, Tag, Typography } from 'antd';
import { apiFetch } from '../api/client';
import type { ClusterSummary } from '../api/types';
import { t, type MessageKey } from '../i18n/zh-CN';

/** 健康徽标五态（WP-61 做完整总览；此处是 shell 的最小表格）。 */
const healthStyle: Record<ClusterSummary['health'], { color: string; labelKey: MessageKey }> = {
  healthy: { color: 'green', labelKey: 'cluster.health.healthy' },
  degraded: { color: 'orange', labelKey: 'cluster.health.degraded' },
  unreachable: { color: 'red', labelKey: 'cluster.health.unreachable' },
  stale: { color: 'gold', labelKey: 'cluster.health.stale' },
  unknown: { color: 'default', labelKey: 'cluster.health.unknown' },
};

export default function ClustersPage() {
  const location = useLocation();
  const { data, isLoading, error } = useQuery({
    queryKey: ['clusters'],
    queryFn: () => apiFetch<ClusterSummary[]>('/clusters'),
  });
  useEffect(() => {
    document.title = `${t('page.clusters.title')} · kurin`;
  }, []);

  if (location.pathname === '/clusters/new') {
    return (
      <div>
        <Typography.Title level={3}>{t('page.clusters.new')}</Typography.Title>
        <Typography.Paragraph type="secondary">9 步表单向导（WP-62 实现）</Typography.Paragraph>
      </div>
    );
  }

  if (location.pathname.startsWith('/clusters/')) {
    return (
      <div>
        <Typography.Title level={3}>{t('page.clusterDetail.title')}</Typography.Title>
        <Typography.Paragraph type="secondary">
          {t('page.clusterDetail.description')}
        </Typography.Paragraph>
      </div>
    );
  }

  return (
    <div>
      <Typography.Title level={3}>{t('page.clusters.title')}</Typography.Title>
      <Table<ClusterSummary>
        rowKey="name"
        loading={isLoading}
        dataSource={data ?? []}
        locale={{ emptyText: t('common.empty') }}
        pagination={false}
      >
        <Table.Column<ClusterSummary> title="名称" dataIndex="name" />
        <Table.Column<ClusterSummary>
          title="发行版"
          dataIndex="distribution"
          render={(v) => <Tag>{v}</Tag>}
        />
        <Table.Column<ClusterSummary> title="K8s" dataIndex="kubernetesVersion" />
        <Table.Column<ClusterSummary>
          title="节点"
          render={(_, r) => (r.nodesReady == null ? '—' : `${r.nodesReady}/${r.nodesTotal ?? '—'}`)}
        />
        <Table.Column<ClusterSummary>
          title="健康"
          dataIndex="health"
          render={(v: ClusterSummary['health']) => (
            <Tag color={healthStyle[v].color}>{t(healthStyle[v].labelKey)}</Tag>
          )}
        />
      </Table>
      {error && <Typography.Text type="danger">{String(error)}</Typography.Text>}
    </div>
  );
}
