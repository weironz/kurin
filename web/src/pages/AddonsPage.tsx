/**
 * Addon 管理页 `/clusters/:name/addons`（WP-64，UI §8.1）。
 *
 * 数据流：
 * - `GET /clusters/{name}`：活动操作门控（安装/卸载期间集群不可再发起操作）；
 * - `GET /clusters/{name}/addons`：已安装清单（快照里的 addons JSON，防御式解析）；
 * - `GET /addons/catalog`：目录卡片（分组、兼容性、values schema 表单）；
 * - `POST/PUT/DELETE /clusters/{name}/addons`：安装/升级/卸载 → 202 跳转操作详情。
 */
import { useEffect, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Alert,
  App,
  Button,
  Card,
  Col,
  Empty,
  Modal,
  Result,
  Row,
  Skeleton,
  Space,
  Table,
  Tag,
  Typography,
} from 'antd';
import { ApiError } from '../api/client';
import { fetchClusterDetail } from '../api/clusters';
import {
  addonCompatibility,
  fetchAddonCatalog,
  fetchInstalledAddons,
  flattenValues,
  groupCatalogByCategory,
  installAddon,
  receiptOperationId,
  removeAddon,
  upgradeAddon,
  type AddonCompatibility,
  type InstalledAddonRow,
  type OperationReceipt,
} from '../api/addons';
import type { AddonCatalogEntry } from '../api/types';
import { t, useLocale, type MessageKey } from '../i18n';
import { AddonValuesForm } from '../wizard/AddonValuesForm';
import { isActiveOperation, operationKindLabel, operationStateLabel } from '../utils/operation';

/** 分类展示文案（未知分类回退原文）。 */
const CATEGORY_LABEL_KEYS: Record<string, MessageKey> = {
  monitoring: 'addons.category.monitoring',
  ingress: 'addons.category.ingress',
  security: 'addons.category.security',
  storage: 'addons.category.storage',
  networking: 'addons.category.networking',
  dashboard: 'addons.category.dashboard',
};

/** 清单里的 status → Tag（未知/缺失显示中性占位，不崩溃）。 */
function statusTag(row: InstalledAddonRow) {
  if (row.status === 'installed') {
    return (
      <Tag color="green" data-testid={`addons-status-${row.name}`}>
        {t('addons.status.installed')}
      </Tag>
    );
  }
  if (row.status === 'failed') {
    return (
      <Tag color="red" data-testid={`addons-status-${row.name}`}>
        {t('addons.status.failed')}
      </Tag>
    );
  }
  return row.status ? <Tag>{row.status}</Tag> : <span>—</span>;
}

/** 兼容性徽标：已安装 / 已装旧版（可升级）。 */
function CompatibilityTag({ name, compat }: { name: string; compat: AddonCompatibility }) {
  if (compat.state === 'installed') {
    return (
      <Tag color="green" data-testid={`addons-state-installed-${name}`}>
        {t('addons.card.installed')}
      </Tag>
    );
  }
  if (compat.state === 'upgrade') {
    return (
      <Tag color="blue" data-testid={`addons-state-upgrade-${name}`}>
        {t('addons.card.upgradeFrom', { from: compat.installedVersion ?? '—' })}
      </Tag>
    );
  }
  return null;
}

/* ------------------------------ 目录卡片 ------------------------------ */

function CatalogCard({
  entry,
  compat,
  disabled,
  onOpenDialog,
}: {
  entry: AddonCatalogEntry;
  compat: AddonCompatibility;
  /** 活动操作期间禁用（兼容性冲突在 compat.conflicts 里另行给出原因）。 */
  disabled: boolean;
  onOpenDialog: (mode: 'install' | 'upgrade') => void;
}) {
  const conflictBlocked = compat.conflicts.length > 0;
  return (
    <Card
      size="small"
      data-testid={`addons-catalog-${entry.name}`}
      title={
        <Space size={8} wrap>
          <span>{entry.title ?? entry.name}</span>
          <Tag>{entry.version}</Tag>
          <CompatibilityTag name={entry.name} compat={compat} />
        </Space>
      }
    >
      <Typography.Paragraph type="secondary" style={{ marginBottom: 8 }}>
        {entry.description}
      </Typography.Paragraph>
      <Space size={6} wrap style={{ marginBottom: 8 }}>
        <Tag color="geekblue">
          {t('addons.card.namespace')}: {entry.namespace}
        </Tag>
        <Tag>
          {t('addons.card.origin')}: {entry.origin}
        </Tag>
        {entry.requires.map((dep) => (
          <Tag key={dep} color={compat.missingRequires.includes(dep) ? 'orange' : 'default'}>
            {t('addons.card.requires')}: {dep}
          </Tag>
        ))}
      </Space>
      {conflictBlocked ? (
        <Typography.Text
          type="danger"
          data-testid={`addons-conflict-${entry.name}`}
          style={{ display: 'block', marginBottom: 8 }}
        >
          {t('addons.card.conflict', { names: compat.conflicts.join(', ') })}
        </Typography.Text>
      ) : null}
      {compat.missingRequires.length > 0 ? (
        <Typography.Text type="warning" style={{ display: 'block', marginBottom: 8 }}>
          {t('addons.card.requiresMissing', { names: compat.missingRequires.join(', ') })}
        </Typography.Text>
      ) : null}
      <Space style={{ float: 'right' }}>
        {compat.state === 'installable' ? (
          <Button
            type="primary"
            disabled={disabled || conflictBlocked}
            onClick={() => onOpenDialog('install')}
            data-testid={`addons-install-${entry.name}`}
          >
            {t('addons.action.install')}
          </Button>
        ) : null}
        {compat.state === 'upgrade' ? (
          <Button
            type="primary"
            disabled={disabled || conflictBlocked}
            onClick={() => onOpenDialog('upgrade')}
            data-testid={`addons-upgrade-${entry.name}`}
          >
            {t('addons.action.upgrade')}
          </Button>
        ) : null}
      </Space>
    </Card>
  );
}

/** 安装/升级对话框顶部的目标摘要（集群 / addon / 版本 / 命名空间）。 */
function DialogSummary({ addon, cluster }: { addon: AddonCatalogEntry; cluster: string }) {
  return (
    <Space size={6} wrap style={{ marginBottom: 12 }}>
      <Tag color="geekblue">
        {t('addons.install.cluster')}: {cluster}
      </Tag>
      <Tag>
        {t('addons.install.addon')}: {addon.name}
      </Tag>
      <Tag>
        {t('addons.install.version')}: {addon.version}
      </Tag>
      <Tag>
        {t('addons.card.namespace')}: {addon.namespace}
      </Tag>
    </Space>
  );
}

/* --------------------------------- 页面 --------------------------------- */

export default function AddonsPage() {
  const { name } = useParams<{ name: string }>();
  const navigate = useNavigate();
  const { message } = App.useApp();
  const queryClient = useQueryClient();

  const locale = useLocale();

  useEffect(() => {
    document.title = `${t('page.addons.title')} · kurin`;
  }, [locale]);

  const clusterQuery = useQuery({
    queryKey: ['cluster', name],
    queryFn: () => fetchClusterDetail(name ?? ''),
    enabled: name != null,
  });
  const catalogQuery = useQuery({
    queryKey: ['addons-catalog'],
    queryFn: ({ signal }) => fetchAddonCatalog(signal),
  });
  const inventoryQuery = useQuery({
    queryKey: ['cluster-addons', name],
    queryFn: ({ signal }) => fetchInstalledAddons(name ?? '', signal),
    enabled: name != null,
  });

  /** 安装/升级对话框（mode + 目录条目）；值由 rjsf 表单持有。 */
  const [dialog, setDialog] = useState<{
    mode: 'install' | 'upgrade';
    entry: AddonCatalogEntry;
  } | null>(null);
  const [dialogValues, setDialogValues] = useState<Record<string, unknown>>({});
  const [uninstallTarget, setUninstallTarget] = useState<InstalledAddonRow | null>(null);

  const invalidateAndGo = async (receipt: OperationReceipt) => {
    await queryClient.invalidateQueries({ queryKey: ['cluster-addons', name] });
    await queryClient.invalidateQueries({ queryKey: ['cluster', name] });
    await queryClient.invalidateQueries({ queryKey: ['operations'] });
    message.info(t('addons.submitted'));
    navigate(`/operations/${encodeURIComponent(receiptOperationId(receipt))}`);
  };

  const installMutation = useMutation({
    mutationFn: async (payload: { mode: 'install' | 'upgrade'; entry: AddonCatalogEntry }) => {
      const values = flattenValues(dialogValues);
      const request = {
        addon: payload.entry.name,
        version: payload.entry.version,
        ...(values.length > 0 ? { values } : {}),
      };
      return payload.mode === 'install'
        ? installAddon(name ?? '', request)
        : upgradeAddon(name ?? '', request);
    },
    onSuccess: async (receipt) => {
      setDialog(null);
      await invalidateAndGo(receipt);
    },
  });

  const uninstallMutation = useMutation({
    mutationFn: (addon: string) => removeAddon(name ?? '', addon),
    onSuccess: async (receipt) => {
      setUninstallTarget(null);
      await invalidateAndGo(receipt);
    },
  });

  if (!name) {
    return null;
  }

  const busy = isActiveOperation(clusterQuery.data?.activeOperation);
  const notFound = clusterQuery.error instanceof ApiError && clusterQuery.error.status === 404;

  if (clusterQuery.isLoading) {
    return <Skeleton active title={false} paragraph={{ rows: 8 }} />;
  }
  if (clusterQuery.error) {
    return notFound ? (
      <Result
        status="404"
        title={t('clusterDetail.notFound')}
        extra={<Link to="/">{t('clusterDetail.backToList')}</Link>}
      />
    ) : (
      <Result
        status="error"
        title={t('clusterDetail.loadFailed')}
        subTitle={clusterQuery.error.message}
      />
    );
  }

  const catalog = catalogQuery.data?.addons ?? [];
  const inventory = inventoryQuery.data?.addons ?? [];
  const groups = groupCatalogByCategory(catalog);
  const active = clusterQuery.data?.activeOperation;

  return (
    <div>
      <Typography.Title level={3} style={{ marginBottom: 4 }}>
        {t('page.addons.title')}
      </Typography.Title>
      <Space align="center" style={{ marginBottom: 16 }} wrap>
        <Typography.Text code>{name}</Typography.Text>
        <Typography.Text type="secondary">{t('page.addons.description')}</Typography.Text>
        <Link to={`/clusters/${encodeURIComponent(name)}`}>{t('addons.backToCluster')}</Link>
      </Space>

      {busy && active ? (
        <Alert
          type="info"
          showIcon
          message={t('addons.activeOperation')}
          description={
            <Link to={`/operations/${active.id}`}>
              {operationKindLabel(active.kind)} · {operationStateLabel(active.state)}
            </Link>
          }
          style={{ marginBottom: 16 }}
        />
      ) : null}

      {/* 已安装清单 */}
      <Card
        size="small"
        title={t('addons.installed.title')}
        style={{ marginBottom: 16 }}
        data-testid="addons-installed-table"
      >
        <Table<InstalledAddonRow>
          rowKey="name"
          loading={inventoryQuery.isLoading}
          dataSource={inventory}
          pagination={false}
          size="small"
          locale={{
            emptyText: (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description={
                  inventoryQuery.data?.note
                    ? t('addons.installed.note', { note: inventoryQuery.data.note })
                    : t('addons.installed.empty')
                }
              />
            ),
          }}
        >
          <Table.Column<InstalledAddonRow>
            title={t('addons.column.name')}
            dataIndex="name"
            render={(v: string) => <Typography.Text code>{v}</Typography.Text>}
          />
          <Table.Column<InstalledAddonRow>
            title={t('addons.column.version')}
            render={(_, row) => row.version ?? '—'}
          />
          <Table.Column<InstalledAddonRow>
            title={t('addons.column.status')}
            render={(_, row) => statusTag(row)}
          />
          <Table.Column<InstalledAddonRow>
            title={t('addons.column.values')}
            render={(_, row) => row.valuesSummary ?? '—'}
          />
          <Table.Column<InstalledAddonRow>
            title={t('addons.column.actions')}
            render={(_, row) => (
              <Button
                danger
                size="small"
                disabled={busy}
                onClick={() => setUninstallTarget(row)}
                data-testid={`addons-uninstall-${row.name}`}
              >
                {t('addons.action.uninstall')}
              </Button>
            )}
          />
        </Table>
      </Card>

      {/* 目录：按分类分组的卡片 */}
      {groups.map((group) => (
        <Card
          key={group.category}
          size="small"
          title={
            CATEGORY_LABEL_KEYS[group.category] != null
              ? t(CATEGORY_LABEL_KEYS[group.category])
              : group.category
          }
          style={{ marginBottom: 16 }}
        >
          <Row gutter={[12, 12]}>
            {group.addons.map((entry) => (
              <Col key={entry.name} xs={24} md={12} xl={8}>
                <CatalogCard
                  entry={entry}
                  compat={addonCompatibility(catalog, inventory, entry.name)}
                  disabled={busy}
                  onOpenDialog={(mode) => {
                    setDialogValues({});
                    setDialog({ mode, entry });
                  }}
                />
              </Col>
            ))}
          </Row>
        </Card>
      ))}
      {catalogQuery.isLoading ? <Skeleton active title={false} paragraph={{ rows: 4 }} /> : null}

      {/* 安装/升级对话框：rjsf values 表单 → key=value */}
      <Modal
        open={dialog != null}
        title={
          dialog
            ? t(dialog.mode === 'install' ? 'addons.install.title' : 'addons.upgrade.title', {
                addon: dialog.entry.title ?? dialog.entry.name,
              })
            : ''
        }
        okText={t(dialog?.mode === 'upgrade' ? 'addons.action.upgrade' : 'addons.action.install')}
        okButtonProps={{ 'data-testid': 'addons-install-confirm' }}
        cancelText={t('common.cancel')}
        onCancel={() => setDialog(null)}
        onOk={() =>
          dialog != null && installMutation.mutate({ mode: dialog.mode, entry: dialog.entry })
        }
        destroyOnHidden
      >
        {dialog ? (
          <div data-testid="addons-install-modal">
            <DialogSummary addon={dialog.entry} cluster={name} />
            <AddonValuesForm
              schema={dialog.entry.values_schema}
              values={dialogValues}
              onChange={setDialogValues}
            />
            {installMutation.isError ? (
              <Alert
                type="error"
                showIcon
                message={t('addons.actionFailed', {
                  message:
                    installMutation.error instanceof Error
                      ? installMutation.error.message
                      : String(installMutation.error),
                })}
                style={{ marginTop: 12 }}
              />
            ) : null}
          </div>
        ) : null}
      </Modal>

      {/* 卸载确认：必须点名集群（服务端校验 confirm == 集群名） */}
      <Modal
        open={uninstallTarget != null}
        title={uninstallTarget ? t('addons.uninstall.title', { addon: uninstallTarget.name }) : ''}
        okText={t('addons.action.uninstall')}
        okButtonProps={{ 'data-testid': 'addons-uninstall-confirm', danger: true }}
        cancelText={t('common.cancel')}
        confirmLoading={uninstallMutation.isPending}
        onCancel={() => setUninstallTarget(null)}
        onOk={() => uninstallTarget != null && uninstallMutation.mutate(uninstallTarget.name)}
      >
        {uninstallTarget ? (
          <div data-testid="addons-uninstall-modal">
            <Typography.Paragraph>
              {t('addons.uninstall.content', {
                cluster: name,
                addon: uninstallTarget.name,
              })}
            </Typography.Paragraph>
            {uninstallMutation.isError ? (
              <Alert
                type="error"
                showIcon
                message={t('addons.actionFailed', {
                  message:
                    uninstallMutation.error instanceof Error
                      ? uninstallMutation.error.message
                      : String(uninstallMutation.error),
                })}
              />
            ) : null}
          </div>
        ) : null}
      </Modal>
    </div>
  );
}
