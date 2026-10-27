/**
 * 操作的展示映射（WP-61）：状态颜色、状态/类型中文标签、活动操作判定。
 * 集群详情「操作历史」Tab 与 `/operations` 列表页共用，保证文案一致。
 */
import type { OperationKind, OperationState, OperationSummary } from '../api/types';
import { t, type MessageKey } from '../i18n';

/** 操作状态 → Tag 颜色（详情操作历史与 `/operations` 列表的徽标必须一致）。 */
export const operationStateColor: Record<OperationState, string> = {
  queued: 'default',
  running: 'processing',
  succeeded: 'success',
  failed: 'error',
  cancelled: 'warning',
  interrupted: 'warning',
};

const stateLabelKey: Record<OperationState, MessageKey> = {
  queued: 'operation.state.queued',
  running: 'operation.state.running',
  succeeded: 'operation.state.succeeded',
  failed: 'operation.state.failed',
  cancelled: 'operation.state.cancelled',
  interrupted: 'operation.state.interrupted',
};

const kindLabelKey: Record<OperationKind, MessageKey> = {
  create: 'operation.kind.create',
  'add-nodes': 'operation.kind.add-nodes',
  'delete-node': 'operation.kind.delete-node',
  'delete-cluster': 'operation.kind.delete-cluster',
  upgrade: 'operation.kind.upgrade',
  'certs-renew': 'operation.kind.certs-renew',
  precheck: 'operation.kind.precheck',
  artifact: 'operation.kind.artifact',
  package: 'operation.kind.package',
  unknown: 'operation.kind.unknown',
};

export function operationStateLabel(state: OperationState): string {
  return t(stateLabelKey[state]);
}

export function operationKindLabel(kind: OperationKind): string {
  return t(kindLabelKey[kind]);
}

/** 是否为活动操作（queued/running）——总览与详情的「当前操作」判定。 */
export function isActiveOperation(op: OperationSummary | null | undefined): op is OperationSummary {
  return op != null && (op.state === 'queued' || op.state === 'running');
}
