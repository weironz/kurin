/**
 * 阶段 × 节点进度网格（WP-63，UI §8.1）。
 *
 * - 格子 = 阶段 × 节点；颜色 = 单元格内最重任务状态；
 * - 点击格子 → 按「阶段 + 节点」过滤下方日志（再点同格取消）；
 * - 悬停 Tooltip 列出格子里的任务与状态。
 */
import { useMemo } from 'react';
import { Table, Tooltip, Typography } from 'antd';
import type { ColumnsType } from 'antd/es/table';
import type { TaskCellState } from '../api/types';
import { t } from '../i18n/zh-CN';
import { cellKey, type GridModel } from './grid';
import { cellStateColor, cellStateLabel } from './cellState';

export interface GridSelection {
  stage: string;
  node: string;
}

interface Props {
  grid: GridModel;
  selection: GridSelection | null;
  onSelect: (sel: GridSelection | null) => void;
}

interface GridRow {
  key: string;
  stage: string;
  cells: Array<{ node: string; state: TaskCellState | null; taskCount: number; title: string }>;
}

/** 集群级列的展示值（与 grid.ts 的 CLUSTER_COLUMN 一致）。 */
const CLUSTER_COLUMN_VALUE = '—';

export function ProgressGrid({ grid, selection, onSelect }: Props) {
  const nodes = grid.nodes;
  const rows: GridRow[] = useMemo(
    () =>
      grid.stages.map((stage) => ({
        key: stage,
        stage,
        cells: nodes.map((node) => {
          const cell = grid.cells[grid.index.get(cellKey(stage, node)) ?? -1];
          if (!cell || cell.tasks.length === 0) {
            return { node, state: null, taskCount: 0, title: '' };
          }
          const taskLines = cell.tasks
            .map(
              (task) =>
                `${task.id} · ${cellStateLabel(task.state)}${
                  task.ms != null ? ` · ${(task.ms / 1000).toFixed(1)}s` : ''
                }${task.reason ? ` · ${task.reason}` : ''}`,
            )
            .join('\n');
          return {
            node,
            state: cell.worst,
            taskCount: cell.tasks.length,
            title:
              t('operationDetail.grid.cellTitle', {
                stage,
                node,
                n: cell.tasks.length,
                state: cellStateLabel(cell.worst),
              }) + `\n${taskLines}`,
          };
        }),
      })),
    [grid, nodes],
  );

  if (grid.stages.length === 0) {
    return <Typography.Text type="secondary">{t('operationDetail.grid.empty')}</Typography.Text>;
  }

  const columns: ColumnsType<GridRow> = [
    {
      title: t('operationDetail.grid.stage'),
      dataIndex: 'stage',
      width: 140,
      render: (stage: string) => (
        <Typography.Text strong style={{ fontSize: 12 }}>
          {stage}
        </Typography.Text>
      ),
    },
    ...nodes.map<ColumnsType<GridRow>[number]>((node) => ({
      title: <span style={{ fontSize: 12 }}>{node}</span>,
      dataIndex: 'cells',
      width: 90,
      render: (_: unknown, record: GridRow) => {
        const cell = record.cells.find((c) => c.node === node)!;
        if (cell.state == null) {
          return <span data-empty-cell="" style={{ display: 'inline-block', width: '100%' }} />;
        }
        const selected =
          selection != null && selection.stage === record.stage && selection.node === node;
        return (
          <Tooltip title={cell.title}>
            <button
              type="button"
              data-grid-stage={record.stage}
              data-grid-node={node}
              data-grid-state={cell.state}
              data-grid-count={cell.taskCount}
              aria-label={`${record.stage} ${node} ${cellStateLabel(cell.state)} ${cell.taskCount}`}
              aria-pressed={selected}
              onClick={() =>
                onSelect(
                  selected
                    ? null
                    : {
                        stage: record.stage,
                        node: node === CLUSTER_COLUMN_VALUE ? '' : node,
                      },
                )
              }
              style={{
                display: 'block',
                width: '100%',
                padding: '2px 4px',
                border: selected ? '2px solid #1677ff' : '1px solid rgba(0,0,0,0.15)',
                borderRadius: 4,
                background: cellStateColor[cell.state],
                cursor: 'pointer',
                fontSize: 11,
                textAlign: 'center',
                lineHeight: '16px',
              }}
            >
              {cell.taskCount}
            </button>
          </Tooltip>
        );
      },
    })),
  ];

  return (
    <div data-testid="progress-grid">
      <Table<GridRow>
        size="small"
        pagination={false}
        loading={false}
        dataSource={rows}
        columns={columns}
        scroll={{ x: Math.max(nodes.length * 90 + 160, 320) }}
      />
      <Typography.Text type="secondary" style={{ fontSize: 12, display: 'block', marginTop: 4 }}>
        {t('operationDetail.grid.legend')}
      </Typography.Text>
    </div>
  );
}
