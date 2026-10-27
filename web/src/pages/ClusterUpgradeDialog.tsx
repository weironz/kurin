/**
 * 集群升级对话框（WP-65，UI §8.1）：目标版本 + 每批节点数 + 强制排空。
 *
 * 「预览计划」提交 `upgrade` + `dry_run: true`（只生成计划），
 * 「执行升级」提交真实操作；两者都跳转 `/operations/:id`。
 * 目标版本在提交前先做 semver 形状检查（服务端同样校验）。
 */
import { useEffect } from 'react';
import { useNavigate } from 'react-router-dom';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import { receiptOperationId, type OperationReceipt } from '../api/addons';
import { upgradeCluster } from '../api/maintenance';
import { Alert, App, Button, Checkbox, Form, Input, InputNumber, Modal, Space } from 'antd';
import { t } from '../i18n';
import { isValidSemver } from '../utils/semver';

interface UpgradeFormValues {
  target: string;
  batch: number;
  forceDrainFailure: boolean;
}

/** 升级对话框（open 受控；提交成功后由父级关闭）。 */
export function ClusterUpgradeDialog({
  cluster,
  currentVersion,
  open,
  busy,
  onClose,
}: {
  cluster: string;
  currentVersion: string;
  open: boolean;
  /** 集群有活动操作时禁用提交。 */
  busy: boolean;
  onClose: () => void;
}) {
  const [form] = Form.useForm<UpgradeFormValues>();
  const navigate = useNavigate();
  const { message } = App.useApp();
  const queryClient = useQueryClient();

  useEffect(() => {
    if (open) {
      form.resetFields();
    }
  }, [open, form]);

  const submitMutation = useMutation({
    mutationFn: (dryRun: boolean) => {
      const values = form.getFieldsValue() as UpgradeFormValues;
      return upgradeCluster(cluster, {
        target: values.target.trim(),
        batch: values.batch,
        dryRun,
        forceDrainFailure: values.forceDrainFailure,
      });
    },
    onSuccess: async (receipt: OperationReceipt) => {
      onClose();
      await queryClient.invalidateQueries({ queryKey: ['cluster', cluster] });
      await queryClient.invalidateQueries({ queryKey: ['operations'] });
      message.info(t('upgrade.submitted'));
      navigate(`/operations/${encodeURIComponent(receiptOperationId(receipt))}`);
    },
  });

  /** 校验表单后按模式提交（预览 = dry_run）。 */
  const submit = (dryRun: boolean) => {
    void form.validateFields().then(
      () => submitMutation.mutate(dryRun),
      () => undefined,
    );
  };

  return (
    <Modal
      open={open}
      title={t('upgrade.title', { cluster })}
      cancelText={t('common.cancel')}
      onCancel={onClose}
      footer={null}
      destroyOnHidden
    >
      <Form
        form={form}
        layout="vertical"
        initialValues={{ batch: 1, forceDrainFailure: false }}
        data-testid="cluster-upgrade-modal"
        disabled={busy}
      >
        <Form.Item label={t('upgrade.currentVersion')}>
          <span data-testid="cluster-upgrade-current">{currentVersion}</span>
        </Form.Item>
        <Form.Item
          name="target"
          label={t('upgrade.target')}
          rules={[
            { required: true, message: t('upgrade.targetRequired') },
            {
              validator: (_, value: string) =>
                !value || isValidSemver(value)
                  ? Promise.resolve()
                  : Promise.reject(new Error(t('upgrade.targetInvalid'))),
            },
          ]}
        >
          <Input placeholder={t('upgrade.targetPlaceholder')} data-testid="cluster-upgrade-target" />
        </Form.Item>
        <Form.Item
          name="batch"
          label={t('upgrade.batch')}
          extra={t('upgrade.batchExtra')}
          rules={[
            { required: true, message: t('upgrade.batchInvalid') },
            {
              validator: (_, value: number) =>
                value == null || (Number.isInteger(value) && value >= 1)
                  ? Promise.resolve()
                  : Promise.reject(new Error(t('upgrade.batchInvalid'))),
            },
          ]}
        >
          {/* 范围校验交给表单规则（min 会吞掉输入，导致 0 不触发校验） */}
          <InputNumber precision={0} style={{ width: '100%' }} data-testid="cluster-upgrade-batch" />
        </Form.Item>
        <Form.Item
          name="forceDrainFailure"
          valuePropName="checked"
          extra={t('upgrade.forceDrainFailureExtra')}
          style={{ marginBottom: 8 }}
        >
          <Checkbox data-testid="cluster-upgrade-force-drain">
            {t('upgrade.forceDrainFailure')}
          </Checkbox>
        </Form.Item>
        {submitMutation.isError ? (
          <Alert
            type="error"
            showIcon
            style={{ marginBottom: 16 }}
            data-testid="cluster-upgrade-error"
            message={t('upgrade.actionFailed', {
              message: submitMutation.error instanceof Error
                ? submitMutation.error.message
                : String(submitMutation.error),
            })}
          />
        ) : null}
        <Form.Item style={{ marginBottom: 0 }}>
          <Space>
            <Button onClick={() => submit(true)} data-testid="cluster-upgrade-preview">
              {t('upgrade.preview')}
            </Button>
            <Button
              type="primary"
              danger
              onClick={() => submit(false)}
              data-testid="cluster-upgrade-execute"
            >
              {t('upgrade.execute')}
            </Button>
          </Space>
        </Form.Item>
      </Form>
    </Modal>
  );
}
