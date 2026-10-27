import { useEffect } from 'react';
import { useQuery } from '@tanstack/react-query';
import { Card, Descriptions, Spin, Table, Tag, Typography } from 'antd';
import { apiFetch } from '../api/client';
import type { Meta } from '../api/types';
import { t } from '../i18n/zh-CN';

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
        组件版本
      </Typography.Title>
      <Table
        rowKey={(r) => r.name}
        pagination={false}
        size="small"
        dataSource={Object.entries(data?.componentVersions ?? {}).map(([name, version]) => ({
          name,
          version,
        }))}
      >
        <Table.Column title="组件" dataIndex="name" />
        <Table.Column title="版本" dataIndex="version" />
      </Table>
      <Typography.Title level={5} style={{ marginTop: 24 }}>
        能力约束
      </Typography.Title>
      <Table
        rowKey={(r) => r.name}
        pagination={false}
        size="small"
        dataSource={Object.entries(data?.capabilities ?? {}).map(([name, reason]) => ({
          name,
          reason,
        }))}
      >
        <Table.Column
          title="约束"
          dataIndex="name"
          render={(v: string) => <Typography.Text code>{v}</Typography.Text>}
        />
        <Table.Column title="说明" dataIndex="reason" render={(v: string) => <Tag>{v}</Tag>} />
      </Table>
    </div>
  );
}
