/**
 * 集群详情「危险区」（WP-64，UI §8.1）：删除节点 / 销毁集群 / 移除纳管。
 *
 * 三件事的风险等级不同，确认方式也随之不同：
 * - 删除节点：选一个配置里声明的节点，确认框点名节点与集群；
 * - 销毁集群：远端资源全部清除，必须手动输入集群名（confirm == 集群名）；
 * - 移除纳管：只停止采集与操作，远端集群保持原样（文案必须与销毁区分）。
 * 集群有活动操作时全部禁用；失败时展示服务端错误信息。
 */
import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { Alert, App, Button, Card, Input, Modal, Select, Space, Typography } from 'antd';
import type { ClusterDetail } from '../api/types';
import {
  archiveCluster,
  deleteClusterNode,
  destroyCluster,
  receiptOperationId,
  type OperationReceipt,
} from '../api/addons';
import { t } from '../i18n';
import { isActiveOperation } from '../utils/operation';

export function ClusterDangerZone({ detail }: { detail: ClusterDetail }) {
  const name = detail.name;
  const navigate = useNavigate();
  const { message } = App.useApp();
  const queryClient = useQueryClient();

  const busy = isActiveOperation(detail.activeOperation);

  /* ------------------------------ 删除节点 ------------------------------ */

  const [deleteNodeOpen, setDeleteNodeOpen] = useState(false);
  const [node, setNode] = useState<string | undefined>(undefined);

  /* ------------------------------ 销毁集群 ------------------------------ */

  const [destroyOpen, setDestroyOpen] = useState(false);
  const [destroyConfirm, setDestroyConfirm] = useState('');

  /* ------------------------------ 移除纳管 ------------------------------ */

  const [archiveOpen, setArchiveOpen] = useState(false);

  /* ------------------------------- 提交 -------------------------------- */

  const goOperation = async (receipt: OperationReceipt) => {
    await queryClient.invalidateQueries({ queryKey: ['cluster', name] });
    await queryClient.invalidateQueries({ queryKey: ['clusters'] });
    await queryClient.invalidateQueries({ queryKey: ['operations'] });
    message.info(t('danger.submitted'));
    navigate(`/operations/${encodeURIComponent(receiptOperationId(receipt))}`);
  };

  const deleteNodeMutation = useMutation({
    mutationFn: (target: string) => deleteClusterNode(name, target),
    onSuccess: async (receipt) => {
      setDeleteNodeOpen(false);
      await goOperation(receipt);
    },
  });
  const destroyMutation = useMutation({
    mutationFn: () => destroyCluster(name),
    onSuccess: async (receipt) => {
      setDestroyOpen(false);
      await goOperation(receipt);
    },
  });
  const archiveMutation = useMutation({
    mutationFn: () => archiveCluster(name),
    onSuccess: async () => {
      // 归档后集群不再采集/操作：列表与详情都失效，回到集群列表。
      await queryClient.invalidateQueries({ queryKey: ['clusters'] });
      await queryClient.invalidateQueries({ queryKey: ['cluster', name] });
      message.info(t('danger.archived'));
      setArchiveOpen(false);
      navigate('/');
    },
  });

  const failure =
    deleteNodeMutation.error ?? destroyMutation.error ?? archiveMutation.error ?? null;

  return (
    <div data-testid="cluster-danger-zone">
      {busy ? (
        <Alert
          type="info"
          showIcon
          message={t('danger.disabledByOperation')}
          style={{ marginBottom: 16 }}
        />
      ) : null}
      {failure != null ? (
        <Alert
          type="error"
          showIcon
          data-testid="danger-error"
          message={t('danger.error', {
            message: failure instanceof Error ? failure.message : String(failure),
          })}
          style={{ marginBottom: 16 }}
        />
      ) : null}

      <Card size="small" title={t('danger.title')} style={{ marginBottom: 16 }}>
        <Typography.Paragraph type="secondary">{t('danger.description')}</Typography.Paragraph>

        {/* 删除节点 */}
        <Card
          size="small"
          style={{ marginBottom: 16 }}
          title={t('danger.deleteNode')}
          data-testid="danger-delete-node"
        >
          <Typography.Paragraph type="secondary" style={{ marginBottom: 8 }}>
            {t('danger.deleteNodeDescription')}
          </Typography.Paragraph>
          <Space wrap>
            <Select
              placeholder={t('danger.deleteNode.select')}
              style={{ minWidth: 220 }}
              value={node}
              onChange={setNode}
              disabled={busy}
              options={detail.desiredNodes.map((n) => ({ value: n.name, label: n.name }))}
              data-testid="danger-delete-node-select"
            />
            <Button
              danger
              disabled={busy || !node}
              onClick={() => setDeleteNodeOpen(true)}
              data-testid="danger-delete-node-open"
            >
              {t('danger.deleteNode')}
            </Button>
          </Space>
        </Card>

        {/* 销毁集群 */}
        <Card
          size="small"
          style={{ marginBottom: 16 }}
          title={t('danger.destroy')}
          data-testid="danger-destroy"
        >
          <Typography.Paragraph type="secondary" style={{ marginBottom: 8 }}>
            {t('danger.destroyDescription')}
          </Typography.Paragraph>
          <Button
            danger
            type="primary"
            disabled={busy}
            onClick={() => {
              setDestroyConfirm('');
              setDestroyOpen(true);
            }}
            data-testid="danger-destroy-open"
          >
            {t('danger.destroy')}
          </Button>
        </Card>

        {/* 移除纳管 */}
        <Card size="small" title={t('danger.archive')} data-testid="danger-archive">
          <Typography.Paragraph type="secondary" style={{ marginBottom: 8 }}>
            {t('danger.archiveDescription')}
          </Typography.Paragraph>
          <Button
            disabled={busy}
            onClick={() => setArchiveOpen(true)}
            data-testid="danger-archive-open"
          >
            {t('danger.archive')}
          </Button>
        </Card>
      </Card>

      {/* 删除节点确认 */}
      <Modal
        open={deleteNodeOpen}
        title={t('danger.deleteNode.confirmTitle', { node: node ?? '' })}
        okText={t('danger.deleteNode')}
        okButtonProps={{
          danger: true,
          loading: deleteNodeMutation.isPending,
          'data-testid': 'danger-delete-node-confirm',
        }}
        cancelText={t('common.cancel')}
        onCancel={() => setDeleteNodeOpen(false)}
        onOk={() => node != null && deleteNodeMutation.mutate(node)}
      >
        <div data-testid="danger-delete-node-modal">
          <Typography.Paragraph>
            {t('danger.deleteNode.confirmContent', { cluster: name, node: node ?? '' })}
          </Typography.Paragraph>
        </div>
      </Modal>

      {/* 销毁集群确认：必须输入集群名 */}
      <Modal
        open={destroyOpen}
        title={t('danger.destroy.confirmTitle', { cluster: name })}
        okText={t('danger.destroy')}
        okButtonProps={{
          danger: true,
          disabled: destroyConfirm !== name,
          loading: destroyMutation.isPending,
          'data-testid': 'danger-destroy-confirm',
        }}
        cancelText={t('common.cancel')}
        onCancel={() => setDestroyOpen(false)}
        onOk={() => destroyMutation.mutate()}
      >
        <div data-testid="danger-destroy-modal">
          <Typography.Paragraph>
            {t('danger.destroy.confirmContent', { cluster: name })}
          </Typography.Paragraph>
          <Input
            placeholder={t('danger.destroy.inputPlaceholder')}
            value={destroyConfirm}
            onChange={(e) => setDestroyConfirm(e.target.value)}
            data-testid="danger-destroy-input"
          />
        </div>
      </Modal>

      {/* 移除纳管确认：文案强调远端集群不受影响 */}
      <Modal
        open={archiveOpen}
        title={t('danger.archive.confirmTitle', { cluster: name })}
        okText={t('danger.archive.ok')}
        cancelText={t('common.cancel')}
        confirmLoading={archiveMutation.isPending}
        okButtonProps={{ 'data-testid': 'danger-archive-confirm' }}
        onCancel={() => setArchiveOpen(false)}
        onOk={() => archiveMutation.mutate()}
      >
        <div data-testid="danger-archive-modal">
          <Typography.Paragraph>
            {t('danger.archive.confirmContent', { cluster: name })}
          </Typography.Paragraph>
        </div>
      </Modal>
    </div>
  );
}
