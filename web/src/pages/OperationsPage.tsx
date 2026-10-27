import { useEffect } from 'react';
import { useLocation, useParams } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { Table, Tag, Typography } from 'antd';
import { apiFetch } from '../api/client';
import type { OperationKind, OperationList, OperationState } from '../api/types';
import { t } from '../i18n/zh-CN';
import { operationKindLabel, operationStateColor, operationStateLabel } from '../utils/operation';

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
      <Typography.Paragraph type="secondary">
        {t('page.operations.description')}
      </Typography.Paragraph>
      <Table
        rowKey="id"
        loading={isLoading}
        dataSource={data?.items ?? []}
        locale={{ emptyText: t('operations.empty') }}
        pagination={false}
      >
        <Table.Column
          title={t('operations.column.id')}
          dataIndex="id"
          render={(v: string) => <Typography.Text code>{v}</Typography.Text>}
        />
        <Table.Column title={t('operations.column.cluster')} dataIndex="cluster" />
        <Table.Column
          title={t('operations.column.kind')}
          dataIndex="kind"
          render={(v: OperationKind) => operationKindLabel(v)}
        />
        <Table.Column
          title={t('operations.column.state')}
          dataIndex="state"
          render={(v: OperationState) => (
            <Tag color={operationStateColor[v]}>{operationStateLabel(v)}</Tag>
          )}
        />
      </Table>
    </div>
  );
}
