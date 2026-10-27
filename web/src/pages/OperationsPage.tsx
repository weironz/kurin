import { useEffect } from 'react';
import { Link } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
import { Table, Tag, Typography } from 'antd';
import { apiFetch } from '../api/client';
import type { OperationKind, OperationList, OperationState } from '../api/types';
import { t } from '../i18n/zh-CN';
import { operationKindLabel, operationStateColor, operationStateLabel } from '../utils/operation';

/** `/operations` 列表（WP-61）；详情页在 `/operations/:id`（WP-63）。 */
export default function OperationsPage() {
  const { data, isLoading } = useQuery({
    queryKey: ['operations'],
    queryFn: () => apiFetch<OperationList>('/operations'),
  });

  useEffect(() => {
    document.title = `${t('page.operations.title')} · kurin`;
  }, []);

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
          render={(v: string) => (
            <Link to={`/operations/${v}`}>
              <Typography.Text code>{v}</Typography.Text>
            </Link>
          )}
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
