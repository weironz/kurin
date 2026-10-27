import { describe, expect, it } from 'vitest';
import type { OperationEventEnvelope, OperationProgress } from '../api/types';
import { applyGridEvent, cellKey, createGrid, parseTaskKey, TASK_STATE_RANK } from './grid';

function ev(seq: number, payload: unknown, ts = `2026-10-27T08:00:${String(seq).padStart(2, '0')}.000Z`): OperationEventEnvelope {
  return { seq, ts, kind: 'task', payload: payload as OperationEventEnvelope['payload'] };
}

const progress: OperationProgress = {
  tasks: [
    { id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', state: 'pending' },
    { id: 'prepare/sysctl@cp-2', node: 'cp-2', stage: 'prepare', state: 'pending' },
    { id: 'bootstrap/init@cp-1', node: 'cp-1', stage: 'bootstrap', state: 'pending' },
    { id: 'join/join@worker-1', node: 'worker-1', stage: 'join', state: 'pending' },
    { id: 'addons/helm@cp-1', node: 'cp-1', stage: 'addons', state: 'pending' },
  ],
};

describe('parseTaskKey', () => {
  it('解析 `<stage>/<kind>@<node>`', () => {
    expect(parseTaskKey('prepare/sysctl@cp-1')).toEqual({
      stage: 'prepare',
      kind: 'sysctl',
      node: 'cp-1',
    });
  });
  it('无 `@` 视为集群级任务', () => {
    expect(parseTaskKey('addons/helm').node).toBeNull();
  });
});

describe('网格聚合：事件驱动格子状态', () => {
  it('progress 种子建立 pending 格子；事件更新为 done/failed', () => {
    const grid = createGrid(progress);
    expect(grid.stages).toEqual(['prepare', 'bootstrap', 'join', 'addons']);
    expect(grid.nodes).toEqual(['cp-1', 'cp-2', 'worker-1']);
    // prepare × cp-1 格子：pending
    let cell = grid.cells[grid.index.get(cellKey('prepare', 'cp-1'))!]!;
    expect(cell.worst).toBe('pending');

    applyGridEvent(
      grid,
      ev(1, { type: 'task-start', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare' }),
    );
    cell = grid.cells[grid.index.get(cellKey('prepare', 'cp-1'))!]!;
    expect(cell.worst).toBe('running');

    applyGridEvent(
      grid,
      ev(2, {
        type: 'task-done',
        id: 'prepare/sysctl@cp-1',
        node: 'cp-1',
        stage: 'prepare',
        ms: 812,
      }),
    );
    cell = grid.cells[grid.index.get(cellKey('prepare', 'cp-1'))!]!;
    expect(cell.worst).toBe('done');
    expect(cell.tasks[0]!.ms).toBe(812);

    // join × worker-1：失败 → 格子 failed
    applyGridEvent(
      grid,
      ev(3, { type: 'task-start', id: 'join/join@worker-1', node: 'worker-1', stage: 'join' }),
    );
    applyGridEvent(
      grid,
      ev(4, {
        type: 'task-failed',
        id: 'join/join@worker-1',
        node: 'worker-1',
        stage: 'join',
        reason: 'exit 1',
      }),
    );
    const joinCell = grid.cells[grid.index.get(cellKey('join', 'worker-1'))!]!;
    expect(joinCell.worst).toBe('failed');
    expect(joinCell.tasks[0]!.reason).toBe('exit 1');
  });

  it('同一格多个任务：格子取最重状态', () => {
    const grid = createGrid(progress);
    applyGridEvent(
      grid,
      ev(1, {
        type: 'task-done',
        id: 'prepare/sysctl@cp-1',
        node: 'cp-1',
        stage: 'prepare',
        ms: 10,
      }),
    );
    applyGridEvent(
      grid,
      ev(2, {
        type: 'task-warn',
        id: 'prepare/other@cp-1',
        node: 'cp-1',
        stage: 'prepare',
        message: 'best-effort 失败',
      }),
    );
    const cell = grid.cells[grid.index.get(cellKey('prepare', 'cp-1'))!]!;
    expect(cell.tasks).toHaveLength(2);
    expect(cell.worst).toBe('warn');
  });

  it('乱序：done 之后迟到的 start 不降级', () => {
    const grid = createGrid(progress);
    applyGridEvent(
      grid,
      ev(5, {
        type: 'task-done',
        id: 'bootstrap/init@cp-1',
        node: 'cp-1',
        stage: 'bootstrap',
        ms: 1000,
      }),
    );
    applyGridEvent(
      grid,
      ev(2, { type: 'task-start', id: 'bootstrap/init@cp-1', node: 'cp-1', stage: 'bootstrap' }),
    );
    const run = grid.tasks.get('bootstrap/init@cp-1')!;
    expect(run.state).toBe('done');
    expect(grid.cells[grid.index.get(cellKey('bootstrap', 'cp-1'))!]!.worst).toBe('done');
  });

  it('task-reconciled / task-skipped 状态', () => {
    const grid = createGrid(progress);
    applyGridEvent(
      grid,
      ev(1, {
        type: 'task-reconciled',
        id: 'prepare/sysctl@cp-2',
        node: 'cp-2',
        stage: 'prepare',
      }),
    );
    expect(grid.tasks.get('prepare/sysctl@cp-2')!.state).toBe('reconciled');
    applyGridEvent(
      grid,
      ev(2, {
        type: 'task-skipped',
        id: 'join/join@worker-1',
        node: 'worker-1',
        stage: 'join',
        reason: 'already in sync',
      }),
    );
    expect(grid.tasks.get('join/join@worker-1')!.state).toBe('skipped');
  });

  it('无 progress 时从事件流自举（新任务/新节点/新阶段）', () => {
    const grid = createGrid();
    applyGridEvent(
      grid,
      ev(1, { type: 'task-start', id: 'prepare/sysctl@cp-9', node: 'cp-9', stage: 'prepare' }),
    );
    expect(grid.stages).toContain('prepare');
    expect(grid.nodes).toContain('cp-9');
    expect(grid.cells[grid.index.get(cellKey('prepare', 'cp-9'))!]!.worst).toBe('running');
  });

  it('集群级任务进入「集群」列', () => {
    const grid = createGrid();
    applyGridEvent(grid, ev(1, { type: 'task-start', id: 'addons/helm', stage: 'addons' } as never));
    expect(grid.nodes).toContain('—');
    expect(grid.cells[grid.index.get(cellKey('addons', '—'))!]).toBeDefined();
  });

  it('状态等级全序', () => {
    expect(TASK_STATE_RANK.failed).toBeGreaterThan(TASK_STATE_RANK.warn);
    expect(TASK_STATE_RANK.warn).toBeGreaterThan(TASK_STATE_RANK.done);
    expect(TASK_STATE_RANK.done).toBeGreaterThan(TASK_STATE_RANK.reconciled);
    expect(TASK_STATE_RANK.reconciled).toBeGreaterThan(TASK_STATE_RANK.skipped);
    expect(TASK_STATE_RANK.skipped).toBeGreaterThan(TASK_STATE_RANK.running);
    expect(TASK_STATE_RANK.running).toBeGreaterThan(TASK_STATE_RANK.pending);
  });
});
