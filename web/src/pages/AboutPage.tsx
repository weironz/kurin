import { useEffect } from 'react';
import { useQuery } from '@tanstack/react-query';
import { Card, Descriptions, Spin, Table, Tag, Typography } from 'antd';
import { apiFetch } from '../api/client';
import type { CapabilityRow, Meta } from '../api/types';
import { t } from '../i18n/zh-CN';

/** 离线就绪 → 展示标签颜色（component/mod.rs `Offline`）。 */
const offlineColor: Record<string, string> = {
  bundle: 'green',
  rejected: 'red',
  'online-only': 'gold',
};

/** `/about`：版本、组件版本表、能力表（UI §8.1）。 */
export default function AboutPage() {
  const { data, isPending } = useQuery({
    queryKey: ['meta'],
    queryFn: () => apiFetch<Meta>('/meta'),
  });

  useEffect(() => {
    document.title = `${t('page.about.title')} · kurin`;
  }, []);

  if (isPending) {
    return <Spin />;
  }

  return (
    <div>
      <Typography.Title level={3}>{t('page.about.title')}</Typography.Title>
      <Card>
        <Descriptions column={1} bordered size="small">
          <Descriptions.Item label="kurin">{data?.version}</Descriptions.Item>
        </Descriptions>
      </Card>
      <Typography.Title level={5} style={{ marginTop: 24 }}>
        {t('about.componentVersions')}
      </Typography.Title>
      <Table
        rowKey={(r) => r.name}
        pagination={false}
        size="small"
        dataSource={Object.entries(data?.components ?? {}).map(([name, version]) => ({
          name,
          version,
        }))}
      >
        <Table.Column title={t('about.column.component')} dataIndex="name" />
        <Table.Column title={t('about.column.version')} dataIndex="version" />
      </Table>
      <Typography.Title level={5} style={{ marginTop: 24 }}>
        {t('about.capabilities')}
      </Typography.Title>
      <Table<CapabilityRow>
        rowKey={(r) => r.id}
        pagination={false}
        size="small"
        dataSource={data?.capabilities ?? []}
      >
        <Table.Column
          title={t('about.column.constraint')}
          dataIndex="id"
          render={(v: string) => <Typography.Text code>{v}</Typography.Text>}
        />
        <Table.Column
          title={t('about.column.reason')}
          dataIndex="offline"
          render={(v: string) => <Tag color={offlineColor[v] ?? 'default'}>{v}</Tag>}
        />
      </Table>
    </div>
  );
}
