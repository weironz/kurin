import { describe, expect, it } from 'vitest';
import type { OperationEventEnvelope } from '../api/types';
import {
  appendLogEvent,
  createLogModel,
  findFirstError,
  flattenLog,
  isGroupOpen,
  rebuildLogModel,
  rowSearchText,
  searchRows,
  serializeEventsJsonl,
  serializeEventsLogText,
  formatPreciseMs,
} from './logmodel';

let seq = 0;
function ev(payload: unknown, ts = '2026-10-27T08:00:00.000Z'): OperationEventEnvelope {
  seq += 1;
  return { seq, ts, kind: 'log', payload: payload as OperationEventEnvelope['payload'] };
}
function reset(): void {
  seq = 0;
}

function buildScenario() {
  reset();
  const events: OperationEventEnvelope[] = [
    ev({ type: 'state', state: 'running' }),
    ev({ type: 'task-start', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare' }),
    ev({
      type: 'log',
      node: 'cp-1',
      stage: 'prepare',
      task: 'prepare/sysctl@cp-1',
      level: 'info',
      stream: 'stdout',
      line: 'sysctl: net.ipv4.ip_forward = 1',
    }),
    ev({ type: 'task-done', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', ms: 812 }),
    ev({ type: 'task-start', id: 'join/join@worker-1', node: 'worker-1', stage: 'join' }),
    ev({
      type: 'log',
      node: 'worker-1',
      stage: 'join',
      task: 'join/join@worker-1',
      level: 'error',
      stream: 'stderr',
      line: 'ERROR FileAvailable--etc-kubernetes-kubelet.conf',
    }),
    ev({
      type: 'task-failed',
      id: 'join/join@worker-1',
      node: 'worker-1',
      stage: 'join',
      reason: 'exit 1',
    }),
    ev({
      type: 'log',
      node: null,
      stage: null,
      task: null,
      level: 'info',
      stream: 'kurin',
      line: 'worker-1 修复建议：删除已存在的 kubelet.conf',
    }),
    ev({
      type: 'resource',
      stage: 'addons',
      kind: 'Deployment',
      namespace: 'ingress-nginx',
      name: 'ingress-nginx-controller',
      ready: true,
      detail: '2/2 ready',
    }),
    ev({ type: 'note', level: 'info', message: '注意：证书将在 210 天后到期' }),
  ];
  return events;
}

describe('日志行模型', () => {
  it('任务分组：远端输出折叠在组下，失败组自动展开', () => {
    const model = rebuildLogModel(buildScenario());
    expect(model.groups.size).toBe(2);
    const okGroup = model.groups.get('prepare/sysctl@cp-1')!;
    expect(okGroup.state).toBe('done');
    expect(okGroup.lineCount).toBe(1);
    const failGroup = model.groups.get('join/join@worker-1')!;
    expect(failGroup.state).toBe('failed');
    expect(isGroupOpen(failGroup, {})).toBe(true);
    expect(isGroupOpen(okGroup, {})).toBe(false);
    expect(isGroupOpen(okGroup, { 'prepare/sysctl@cp-1': true })).toBe(true);
  });

  it('默认视图：折叠组不含子行，展开后含', () => {
    const model = rebuildLogModel(buildScenario());
    const rows = flattenLog(model, { filter: {}, openGroups: {} });
    const ids = rows.map((r) => r.seq);
    // 子行 3（stdout）默认折叠，子行 6（stderr，失败组）展开。
    expect(ids).not.toContain(3);
    expect(ids).toContain(6);
  });

  it('node 筛选：只保留该节点的行与组', () => {
    const model = rebuildLogModel(buildScenario());
    const rows = flattenLog(model, { filter: { node: 'worker-1' }, openGroups: {} });
    for (const r of rows) {
      if (r.rowKind !== 'task') expect(r.node).toBe('worker-1');
    }
    expect(rows.some((r) => r.taskId === 'join/join@worker-1')).toBe(true);
    expect(rows.some((r) => r.taskId === 'prepare/sysctl@cp-1')).toBe(false);
  });

  it('stage / task / level / stream 筛选', () => {
    const model = rebuildLogModel(buildScenario());
    const stageRows = flattenLog(model, { filter: { stage: 'join' }, openGroups: {} });
    expect(stageRows.every((r) => r.stage === 'join')).toBe(true);
    const taskRows = flattenLog(model, { filter: { task: 'prepare/sysctl@cp-1' }, openGroups: {} });
    expect(taskRows.every((r) => r.taskId === 'prepare/sysctl@cp-1')).toBe(true);
    const errRows = flattenLog(model, { filter: { level: 'error' }, openGroups: {} });
    expect(errRows.every((r) => r.level === 'error')).toBe(true);
    expect(errRows.length).toBeGreaterThan(0);
    const outRows = flattenLog(model, { filter: { stream: 'stdout' }, openGroups: {} });
    expect(
      outRows.filter((r) => r.rowKind === 'line').every((r) => r.stream === 'stdout'),
    ).toBe(true);
  });

  it('errorsOnly：仅 error/warn 行（组头为失败的组也保留）', () => {
    const model = rebuildLogModel(buildScenario());
    const rows = flattenLog(model, { filter: { errorsOnly: true }, openGroups: {} });
    for (const r of rows) {
      const isErrOrWarn = r.level === 'error' || r.level === 'warn';
      const isFailedHeader = r.rowKind === 'task' && r.state === 'failed';
      const hasVisibleChildren = r.grouped === false && r.rowKind === 'task' && r.state === 'failed';
      expect(isErrOrWarn || isFailedHeader || hasVisibleChildren || r.rowKind === 'terminal').toBe(
        true,
      );
    }
  });

  it('搜索：命中 seq 与所属组（大小写不敏感）', () => {
    const model = rebuildLogModel(buildScenario());
    const matches = searchRows(model, { query: 'error fileavailable' });
    // 「ERROR FileAvailable…」命中；message 大小写不敏感
    expect(matches.length).toBeGreaterThanOrEqual(1);
    expect(matches.some((m) => m.groupId === 'join/join@worker-1')).toBe(true);
    const upper = searchRows(model, { query: 'ERROR FILEAVAILABLE' });
    expect(upper.length).toBe(matches.length);
  });

  it('跳到第一个错误：返回第一个 error/failed 行', () => {
    const model = rebuildLogModel(buildScenario());
    const first = findFirstError(model, {});
    expect(first).not.toBeNull();
    expect(first!.groupId).toBe('join/join@worker-1');
  });

  it('rowSearchText：行可搜索文本', () => {
    const model = rebuildLogModel(buildScenario());
    const rows = flattenLog(model, { filter: {}, openGroups: {} });
    const bySeq = new Map(rows.map((r) => [r.seq, r]));
    expect(rowSearchText(bySeq.get(10)!)).toContain('证书将在 210 天后到期');
    expect(rowSearchText(bySeq.get(9)!)).toContain('ingress-nginx-controller');
  });
});

describe('增量与重建等价 + 乱序', () => {
  it('增量 append 与全量 rebuild 结果一致', () => {
    const events = buildScenario();
    const incremental = createLogModel();
    for (const e of events) appendLogEvent(incremental, e);
    const rebuilt = rebuildLogModel(events);
    const a = flattenLog(incremental, { filter: {}, openGroups: {} });
    const b = flattenLog(rebuilt, { filter: {}, openGroups: {} });
    expect(a.map((r) => r.seq)).toEqual(b.map((r) => r.seq));
  });
});

describe('下载序列化', () => {
  it('jsonl：每行一个信封，可 round-trip', () => {
    const events = buildScenario();
    const text = serializeEventsJsonl(events);
    const lines = text.trim().split('\n');
    expect(lines).toHaveLength(events.length);
    for (let i = 0; i < events.length; i += 1) {
      expect(JSON.parse(lines[i]!) as OperationEventEnvelope).toEqual(events[i]);
    }
  });
  it('log 文本：人读格式含时间戳/节点/任务', () => {
    const events = buildScenario();
    const text = serializeEventsLogText(events);
    expect(text).toContain('2026-10-27T08:00:00.000Z');
    expect(text).toContain('sysctl: net.ipv4.ip_forward = 1');
    expect(text).toContain('prepare/sysctl@cp-1');
  });
});

describe('formatPreciseMs', () => {
  it('毫秒级耗时格式', () => {
    expect(formatPreciseMs(0)).toBe('0ms');
    expect(formatPreciseMs(42)).toBe('42ms');
    expect(formatPreciseMs(42381)).toBe('42.381s');
    expect(formatPreciseMs(102381)).toBe('1m42.381s');
  });
});
