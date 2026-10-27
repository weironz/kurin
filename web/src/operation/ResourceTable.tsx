/**
 * 资源就绪表（WP-63，UI §8.1）：addon 阶段的 `Resource` 事件聚合。
 * 同一资源以最新 seq 为准（就绪跟踪器每 2s 刷新一次）。
 */
import { useMemo } from 'react';
import { Table, Tag, Typography } from 'antd';
import type { ColumnsType } from 'antd/es/table';
import { t } from '../i18n';
import { formatLogTimeAbsolute } from '../utils/format';
import type { ResourceRow } from './timeline';

interface Props {
  rows: Map<string, ResourceRow>;
}

export function ResourceTable({ rows }: Props) {
  const data = useMemo(
    () =>
      Array.from(rows.values())
        .sort((a, b) => a.seq - b.seq)
        .map((r) => ({ key: `${r.kind}|${r.namespace ?? ''}|${r.name}`, ...r })),
    [rows],
  );

  const columns: ColumnsType<ResourceRow> = [
    {
      title: t('operationDetail.resources.column.stage'),
      dataIndex: 'stage',
      width: 120,
    },
    {
      title: t('operationDetail.resources.column.kind'),
      dataIndex: 'kind',
      width: 120,
    },
    {
      title: t('operationDetail.resources.column.namespace'),
      dataIndex: 'namespace',
      width: 120,
      render: (ns: string | null) => ns ?? '—',
    },
    {
      title: t('operationDetail.resources.column.name'),
      dataIndex: 'name',
    },
    {
      title: t('operationDetail.resources.column.ready'),
      dataIndex: 'ready',
      width: 100,
      render: (ready: boolean) =>
        ready ? (
          <Tag color="success">{t('operationDetail.resources.ready')}</Tag>
        ) : (
          <Tag color="processing">{t('operationDetail.resources.notReady')}</Tag>
        ),
    },
    {
      title: t('operationDetail.resources.column.detail'),
      dataIndex: 'detail',
      render: (detail: string | undefined, row: ResourceRow) =>
        detail ?? `${formatLogTimeAbsolute(row.ts)}`,
    },
  ];

  return (
    <div data-testid="resource-table">
      <Typography.Text strong style={{ display: 'block', marginBottom: 8 }}>
        {t('operationDetail.resources')}
      </Typography.Text>
      <Table<ResourceRow>
        size="small"
        pagination={false}
        loading={false}
        dataSource={data}
        columns={columns}
        locale={{ emptyText: t('operationDetail.resources.empty') }}
      />
    </div>
  );
}
