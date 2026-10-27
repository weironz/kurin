/**
 * 包管理页 `/packages`（WP-65，UI §8.1）。
 *
 * - 列表：GET /packages（名称 / 类型 / 版本 / 发行版 / 架构 / 大小 / 来源 /
 *   签名数 / SBOM / 校验时间），支持手动刷新；
 * - 登记：POST /packages（source → 201）；删除：DELETE /packages/{id}（确认）；
 * - 校验 / 推送 / 制作 base：202 长任务 → 跳转 /operations/:id 并刷新列表；
 * - 服务端错误就地展示（Alert），不打断其余操作。
 */
import { useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Alert,
  App,
  Button,
  Card,
  Empty,
  Form,
  Input,
  Modal,
  Select,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import {
  createBasePackage,
  fetchPackages,
  importPackage,
  pushPackage,
  removePackage,
  verifyPackage,
} from '../api/packages';
import { receiptOperationId, type OperationReceipt } from '../api/addons';
import type { PackageInfo } from '../api/types';
import { t, useLocale, type MessageKey } from '../i18n';
import { formatAbsolute, formatBytes } from '../utils/format';
import { isValidSemver } from '../utils/semver';

/** 登记来源 → 中文标签（未知值回退原文）。 */
const ORIGIN_LABEL: Record<string, MessageKey> = {
  imported: 'packages.origin.imported',
  'create-base': 'packages.origin.create-base',
};

/* -------------------------------- 对话框 -------------------------------- */

/** 导入包：source → POST /packages → 201 + 新行。 */
function ImportModal({
  open,
  onClose,
  onImported,
}: {
  open: boolean;
  onClose: () => void;
  onImported: (row: PackageInfo) => void;
}) {
  const [form] = Form.useForm<{ source: string }>();
  const mutation = useMutation({
    mutationFn: (values: { source: string }) => importPackage(values),
    onSuccess: (row) => {
      form.resetFields();
      onImported(row);
    },
  });
  return (
    <Modal
      open={open}
      title={t('packages.import.title')}
      okText={t('packages.import')}
      okButtonProps={{ 'data-testid': 'packages-import-confirm' }}
      cancelText={t('common.cancel')}
      confirmLoading={mutation.isPending}
      onCancel={() => {
        mutation.reset();
        onClose();
      }}
      onOk={() =>
        void form.validateFields().then(
          (values) => mutation.mutate(values),
          () => undefined,
        )
      }
      destroyOnHidden
    >
      <Form form={form} layout="vertical" data-testid="packages-import-modal" requiredMark={false}>
        <Form.Item
          name="source"
          label={t('packages.import.source')}
          extra={t('packages.import.sourceExtra')}
          rules={[{ required: true, message: t('packages.import.source') }]}
        >
          <Input
            placeholder="file:///media/usb/kurin-offline.tar.zst"
            data-testid="packages-import-source"
          />
        </Form.Item>
        {mutation.isError ? (
          <Alert
            type="error"
            showIcon
            data-testid="packages-import-error"
            message={t('packages.import.failed', {
              message: mutation.error instanceof Error
                ? mutation.error.message
                : String(mutation.error),
            })}
          />
        ) : null}
      </Form>
    </Modal>
  );
}

/** 推送包：destination（oci://）+ 可选凭据引用 → POST /packages/{id}/push。 */
function PushModal({
  target,
  onClose,
  onSubmitted,
}: {
  target: PackageInfo | null;
  onClose: () => void;
  onSubmitted: (receipt: OperationReceipt) => void;
}) {
  const [form] = Form.useForm<{ destination: string; user?: string; password_env?: string }>();
  const mutation = useMutation({
    mutationFn: (values: { destination: string; user?: string; password_env?: string }) =>
      pushPackage(target?.id ?? '', values),
    onSuccess: (receipt) => {
      form.resetFields();
      onSubmitted(receipt);
    },
  });
  return (
    <Modal
      open={target != null}
      title={target ? t('packages.push.title', { name: target.name }) : ''}
      okText={t('packages.push')}
      okButtonProps={{ 'data-testid': 'packages-push-confirm' }}
      cancelText={t('common.cancel')}
      confirmLoading={mutation.isPending}
      onCancel={() => {
        mutation.reset();
        onClose();
      }}
      onOk={() =>
        void form.validateFields().then(
          (values) => mutation.mutate(values),
          () => undefined,
        )
      }
      destroyOnHidden
    >
      <Form
        form={form}
        layout="vertical"
        data-testid="packages-push-modal"
        requiredMark={false}
      >
        <Typography.Paragraph type="secondary">{t('packages.push.description')}</Typography.Paragraph>
        <Form.Item
          name="destination"
          label={t('packages.push.destination')}
          extra={t('packages.push.destinationExtra')}
          rules={[
            { required: true, message: t('packages.push.destination') },
            {
              validator: (_, value: string) =>
                !value || value.startsWith('oci://')
                  ? Promise.resolve()
                  : Promise.reject(new Error(t('packages.push.destinationInvalid'))),
            },
          ]}
        >
          <Input placeholder="oci://registry.local:5000/kurin/base" data-testid="packages-push-destination" />
        </Form.Item>
        <Form.Item name="user" label={t('packages.push.user')}>
          <Input autoComplete="off" data-testid="packages-push-user" />
        </Form.Item>
        <Form.Item
          name="password_env"
          label={t('packages.push.passwordEnv')}
          extra={t('packages.push.passwordEnvExtra')}
        >
          <Input data-testid="packages-push-password-env" />
        </Form.Item>
        {mutation.isError ? (
          <Alert
            type="error"
            showIcon
            data-testid="packages-push-error"
            message={t('packages.actionFailed', {
              message: mutation.error instanceof Error
                ? mutation.error.message
                : String(mutation.error),
            })}
          />
        ) : null}
      </Form>
    </Modal>
  );
}

/** 制作 base 包：可选组件参数 + output → POST /packages/create-base。 */
function CreateBaseModal({
  open,
  onClose,
  onSubmitted,
}: {
  open: boolean;
  onClose: () => void;
  onSubmitted: (receipt: OperationReceipt) => void;
}) {
  const [form] = Form.useForm<{
    kubernetes?: string;
    distribution?: 'kubeadm' | 'k3s';
    arch?: string;
    cni?: string;
    output: string;
  }>();
  const mutation = useMutation({
    mutationFn: (values: {
      kubernetes?: string;
      distribution?: 'kubeadm' | 'k3s';
      arch?: string;
      cni?: string;
      output: string;
    }) => createBasePackage(values),
    onSuccess: (receipt) => {
      form.resetFields();
      onSubmitted(receipt);
    },
  });
  return (
    <Modal
      open={open}
      title={t('packages.createBase.title')}
      okText={t('packages.createBase')}
      okButtonProps={{ 'data-testid': 'packages-create-base-confirm' }}
      cancelText={t('common.cancel')}
      confirmLoading={mutation.isPending}
      onCancel={() => {
        mutation.reset();
        onClose();
      }}
      onOk={() =>
        void form.validateFields().then(
          (values) => mutation.mutate(values),
          () => undefined,
        )
      }
      destroyOnHidden
    >
      <Form
        form={form}
        layout="vertical"
        initialValues={{ distribution: 'kubeadm' }}
        data-testid="packages-create-base-modal"
        requiredMark={false}
      >
        <Typography.Paragraph type="secondary">
          {t('packages.createBase.description')}
        </Typography.Paragraph>
        <Form.Item
          name="kubernetes"
          label={t('packages.createBase.kubernetes')}
          rules={[
            {
              validator: (_, value: string) =>
                !value || isValidSemver(value)
                  ? Promise.resolve()
                  : Promise.reject(new Error(t('upgrade.targetInvalid'))),
            },
          ]}
        >
          <Input placeholder="1.33.1" data-testid="packages-create-base-kubernetes" />
        </Form.Item>
        <Form.Item name="distribution" label={t('packages.createBase.distribution')}>
          <Select
            data-testid="packages-create-base-distribution"
            options={[
              { value: 'kubeadm', label: 'kubeadm' },
              { value: 'k3s', label: 'k3s' },
            ]}
          />
        </Form.Item>
        <Form.Item name="arch" label={t('packages.createBase.arch')}>
          <Input placeholder="amd64" data-testid="packages-create-base-arch" />
        </Form.Item>
        <Form.Item name="cni" label={t('packages.createBase.cni')}>
          <Input placeholder="calico" data-testid="packages-create-base-cni" />
        </Form.Item>
        <Form.Item
          name="output"
          label={t('packages.createBase.output')}
          extra={t('packages.createBase.outputExtra')}
          rules={[{ required: true, message: t('packages.createBase.output') }]}
        >
          <Input placeholder="oci-layout:///opt/kurin/base" data-testid="packages-create-base-output" />
        </Form.Item>
        {mutation.isError ? (
          <Alert
            type="error"
            showIcon
            data-testid="packages-create-base-error"
            message={t('packages.actionFailed', {
              message: mutation.error instanceof Error
                ? mutation.error.message
                : String(mutation.error),
            })}
          />
        ) : null}
      </Form>
    </Modal>
  );
}

/* --------------------------------- 页面 --------------------------------- */

export default function PackagesPage() {
  const navigate = useNavigate();
  const { message } = App.useApp();
  const queryClient = useQueryClient();

  const locale = useLocale();

  useEffect(() => {
    document.title = `${t('page.packages.title')} · kurin`;
  }, [locale]);

  const packagesQuery = useQuery({
    queryKey: ['packages'],
    queryFn: ({ signal }) => fetchPackages(signal),
  });
  const packages = packagesQuery.data?.packages ?? [];

  const [importOpen, setImportOpen] = useState(false);
  const [pushTarget, setPushTarget] = useState<PackageInfo | null>(null);
  const [createBaseOpen, setCreateBaseOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<PackageInfo | null>(null);

  /** 202 类操作：刷新列表 + 跳转操作详情。 */
  const goOperation = async (receipt: OperationReceipt) => {
    await queryClient.invalidateQueries({ queryKey: ['packages'] });
    await queryClient.invalidateQueries({ queryKey: ['operations'] });
    message.info(t('packages.submitted'));
    navigate(`/operations/${encodeURIComponent(receiptOperationId(receipt))}`);
  };

  const verifyMutation = useMutation({
    mutationFn: (id: string) => verifyPackage(id),
    onSuccess: async (receipt) => {
      await goOperation(receipt);
    },
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) => removePackage(id),
    onSuccess: async () => {
      setDeleteTarget(null);
      await queryClient.invalidateQueries({ queryKey: ['packages'] });
    },
  });

  const rowFailure =
    verifyMutation.error ?? deleteMutation.error ?? null;

  return (
    <div>
      <Typography.Title level={3} style={{ marginBottom: 4 }}>
        {t('page.packages.title')}
      </Typography.Title>
      <Space align="center" style={{ marginBottom: 16 }} wrap>
        <Typography.Text type="secondary">{t('page.packages.description')}</Typography.Text>
        <Button
          size="small"
          onClick={() => void packagesQuery.refetch()}
          loading={packagesQuery.isFetching}
          data-testid="packages-refresh"
        >
          {t('packages.refresh')}
        </Button>
      </Space>

      {packagesQuery.isError ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('packages.actionFailed', {
            message: packagesQuery.error instanceof Error
              ? packagesQuery.error.message
              : String(packagesQuery.error),
          })}
          action={
            <Button size="small" onClick={() => void packagesQuery.refetch()}>
              {t('common.retry')}
            </Button>
          }
        />
      ) : null}
      {rowFailure != null ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          data-testid="packages-row-error"
          message={t('packages.actionFailed', {
            message: rowFailure instanceof Error ? rowFailure.message : String(rowFailure),
          })}
        />
      ) : null}

      <Card size="small">
        <Space style={{ marginBottom: 16 }} wrap>
          <Button
            type="primary"
            onClick={() => setImportOpen(true)}
            data-testid="packages-import"
          >
            {t('packages.import')}
          </Button>
          <Button onClick={() => setCreateBaseOpen(true)} data-testid="packages-create-base">
            {t('packages.createBase')}
          </Button>
        </Space>
        <Table<PackageInfo>
          rowKey="id"
          loading={packagesQuery.isLoading}
          dataSource={packages}
          pagination={false}
          size="small"
          data-testid="packages-table"
          locale={{
            emptyText: (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description={t('packages.empty')}
              />
            ),
          }}
        >
          <Table.Column<PackageInfo>
            title={t('packages.column.name')}
            dataIndex="name"
            render={(v: string) => <Typography.Text code>{v}</Typography.Text>}
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.kind')}
            dataIndex="kind"
            render={(v: string) => <Tag>{v}</Tag>}
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.version')}
            dataIndex="version"
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.distribution')}
            dataIndex="distribution"
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.arch')}
            dataIndex="arch"
            render={(v: string[]) =>
              v.length === 0 ? '—' : v.map((arch) => <Tag key={arch}>{arch}</Tag>)
            }
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.size')}
            dataIndex="size"
            render={(v: number) => formatBytes(v)}
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.origin')}
            dataIndex="origin"
            render={(v: string) =>
              ORIGIN_LABEL[v] != null ? t(ORIGIN_LABEL[v]) : v
            }
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.signatures')}
            dataIndex="signatures"
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.sbom')}
            dataIndex="sbom"
            render={(v: string | null | undefined) =>
              v ? t('packages.sbom.yes') : t('packages.sbom.none')
            }
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.verifiedAt')}
            dataIndex="verified_at"
            render={(v: string) => <Tooltip title={formatAbsolute(v)}>{formatAbsolute(v)}</Tooltip>}
          />
          <Table.Column<PackageInfo>
            title={t('packages.column.actions')}
            render={(_, row) => (
              <Space size="small" wrap>
                <Button
                  size="small"
                  loading={verifyMutation.isPending && verifyMutation.variables === row.id}
                  onClick={() => verifyMutation.mutate(row.id)}
                  data-testid={`packages-verify-${row.id}`}
                >
                  {t('packages.verify')}
                </Button>
                <Button
                  size="small"
                  onClick={() => setPushTarget(row)}
                  data-testid={`packages-push-${row.id}`}
                >
                  {t('packages.push')}
                </Button>
                <Button
                  size="small"
                  danger
                  onClick={() => setDeleteTarget(row)}
                  data-testid={`packages-delete-${row.id}`}
                >
                  {t('packages.delete')}
                </Button>
              </Space>
            )}
          />
        </Table>
      </Card>

      <ImportModal
        open={importOpen}
        onClose={() => setImportOpen(false)}
        onImported={async (row) => {
          setImportOpen(false);
          await queryClient.invalidateQueries({ queryKey: ['packages'] });
          message.success(t('packages.imported', { name: row.name }));
        }}
      />

      <PushModal
        target={pushTarget}
        onClose={() => setPushTarget(null)}
        onSubmitted={async (receipt) => {
          setPushTarget(null);
          await goOperation(receipt);
        }}
      />

      <CreateBaseModal
        open={createBaseOpen}
        onClose={() => setCreateBaseOpen(false)}
        onSubmitted={async (receipt) => {
          setCreateBaseOpen(false);
          await goOperation(receipt);
        }}
      />

      <Modal
        open={deleteTarget != null}
        title={deleteTarget ? t('packages.delete.title', { name: deleteTarget.name }) : ''}
        okText={t('packages.delete')}
        okButtonProps={{ 'data-testid': 'packages-delete-confirm', danger: true }}
        cancelText={t('common.cancel')}
        confirmLoading={deleteMutation.isPending}
        onCancel={() => {
          deleteMutation.reset();
          setDeleteTarget(null);
        }}
        onOk={() => deleteTarget != null && deleteMutation.mutate(deleteTarget.id)}
      >
        {deleteTarget ? (
          <div data-testid="packages-delete-modal">
            <Typography.Paragraph>
              {t('packages.delete.content')}
            </Typography.Paragraph>
            <Typography.Paragraph>
              <Typography.Text code>{deleteTarget.source}</Typography.Text>
            </Typography.Paragraph>
            {deleteMutation.isError ? (
              <Alert
                type="error"
                showIcon
                message={t('packages.actionFailed', {
                  message: deleteMutation.error instanceof Error
                    ? deleteMutation.error.message
                    : String(deleteMutation.error),
                })}
              />
            ) : null}
          </div>
        ) : null}
      </Modal>
    </div>
  );
}
