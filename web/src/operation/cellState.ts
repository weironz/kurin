/**
 * 格子状态的颜色与文案（UI §8.1：待执行/执行中/跳过/完成/已核对/告警/失败）。
 * 独立模块：ProgressGrid 只导出组件（react-refresh 约束），常量放这里。
 */
import type { TaskCellState } from '../api/types';
import { t } from '../i18n';
import type { MessageKey } from '../i18n';

/** 格子状态 → 背景色（浅色系，failed 红底）。 */
export const cellStateColor: Record<TaskCellState, string> = {
  pending: 'rgba(0,0,0,0.04)',
  running: '#e6f4ff',
  skipped: 'rgba(0,0,0,0.09)',
  reconciled: '#e6fffb',
  done: '#f6ffed',
  warn: '#fffbe6',
  failed: '#fff1f0',
};

const cellStateKey: Record<TaskCellState, MessageKey> = {
  pending: 'operationDetail.cellState.pending',
  running: 'operationDetail.cellState.running',
  skipped: 'operationDetail.cellState.skipped',
  reconciled: 'operationDetail.cellState.reconciled',
  done: 'operationDetail.cellState.done',
  warn: 'operationDetail.cellState.warn',
  failed: 'operationDetail.cellState.failed',
};

export function cellStateLabel(state: TaskCellState): string {
  return t(cellStateKey[state]);
}
