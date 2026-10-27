import { useEffect } from 'react';
import { useLocation, useParams } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { Table, Tag, Typography } from 'antd';
import { apiFetch } from '../api/client';
import type { OperationList } from '../api/types';
import { t } from '../i18n/zh-CN';

const stateColor: Record<string, string> = {
  queued: 'default',
  running: 'processing',
  succeeded: 'success',
  failed: 'error',
  cancelled: 'warning',
  interrupted: 'warning',
};

/** `/operations` 列表与 `/operations/:id` 占位详情（WP-63 实现日志控制台）。 */
export default function OperationsPage() {
  const location = useLocation();
  const params = useParams<{ id: string }>();
  const isDetail = location.pathname.startsWith('/operations/') && params.id;

  const { data, isLoading } = useQuery({
    queryKey: ['operations'],
    queryFn: () => apiFetch<OperationList>('/operations'),
    enabled: !isDetail,
  });

  useEffect(() => {
    document.title = `${t('page.operations.title')} · kurin`;
  }, []);

  if (isDetail) {
    return (
      <div>
        <Typography.Title level={3}>{t('page.operationDetail.title')}</Typography.Title>
        <Typography.Paragraph type="secondary">
          {t('page.operationDetail.description')}
        </Typography.Paragraph>
        <Typography.Text code>{params.id}</Typography.Text>
      </div>
    );
  }

  return (
    <div>
      <Typography.Title level={3}>{t('page.operations.title')}</Typography.Title>
      <Table
        rowKey="id"
        loading={isLoading}
        dataSource={data?.items ?? []}
        locale={{ emptyText: t('common.empty') }}
        pagination={false}
      >
        <Table.Column
          title="ID"
          dataIndex="id"
          render={(v: string) => <Typography.Text code>{v}</Typography.Text>}
        />
        <Table.Column title={t('nav.clusters')} dataIndex="cluster" />
        <Table.Column title="类型" dataIndex="kind" />
        <Table.Column
          title="状态"
          dataIndex="state"
          render={(v: string) => <Tag color={stateColor[v]}>{v}</Tag>}
        />
      </Table>
    </div>
  );
}
