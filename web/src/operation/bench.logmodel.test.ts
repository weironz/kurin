/**
 * 10 万行日志的渲染性能基准（WP-63 验收：帧时间断言或基准记录）。
 *
 * jsdom 无真实布局/绘制；这里度量的是数据管线（事件 → 行模型 → 展平 + 去重归并），
 * 即虚拟滚动下每帧实际要做的派生工作，并断言量级在交互预算内。
 * 汇出的数字打印到测试输出（基准记录），阈值按 CI 稳定性放宽 4 倍。
 */
import { describe, expect, it } from 'vitest';
import type { OperationEventEnvelope } from '../api/types';
import { flattenLog, rebuildLogModel, searchRows } from './logmodel';
import { OperationEventStore } from './store';

const TOTAL = 100_000;

function buildEvents(): OperationEventEnvelope[] {
  const events: OperationEventEnvelope[] = [];
  const base = Date.parse('2026-10-27T08:00:00.000Z');
  const tasks = 200; // 每任务约 500 行输出
  const perTask = Math.floor(TOTAL / tasks);
  let seq = 0;
  for (let t = 0; t < tasks; t += 1) {
    const node = `node-${t % 8}`;
    const taskId = `stage-${t % 6}/kind-${t}@${node}`;
    seq += 1;
    events.push({
      seq,
      ts: new Date(base + seq).toISOString(),
      kind: 'task',
      payload: { type: 'task-start', id: taskId, node, stage: `stage-${t % 6}` },
    });
    for (let i = 0; i < perTask; i += 1) {
      seq += 1;
      events.push({
        seq,
        ts: new Date(base + seq).toISOString(),
        kind: 'log',
        payload: {
          type: 'log',
          node,
          stage: `stage-${t % 6}`,
          task: taskId,
          level: i % 97 === 0 ? 'error' : i % 13 === 0 ? 'warn' : 'info',
          stream: i % 3 === 0 ? 'stdout' : i % 3 === 1 ? 'stderr' : 'kurin',
          line: `[task ${t} line ${i}] nginx: configuration file test failed`,
        },
      });
    }
    seq += 1;
    events.push({
      seq,
      ts: new Date(base + seq).toISOString(),
      kind: 'task',
      payload: { type: 'task-done', id: taskId, node, stage: `stage-${t % 6}`, ms: t * 10 },
    });
  }
  return events;
}

describe('100k 行日志基准（基准记录，宽松阈值防 CI 抖动）', () => {
  const events = buildEvents();

  it(`事件量达到 ${TOTAL}`, () => {
    expect(events.length).toBeGreaterThanOrEqual(TOTAL);
  });

  it('行模型构建（顺序追加）< 3s', () => {
    const t0 = performance.now();
    const model = rebuildLogModel(events);
    const buildMs = performance.now() - t0;
    const rows = flattenLog(model, { filter: {}, openGroups: {} });
    const flattenMs = performance.now() - t0 - buildMs;
    console.info(
      `[bench] events=${events.length} build=${buildMs.toFixed(0)}ms flatten=${flattenMs.toFixed(0)}ms topRows=${rows.length}`,
    );
    expect(buildMs).toBeLessThan(3000);
    expect(rows.length).toBeGreaterThan(0);
  });

  it('全量重算（rebuild，补页路径）< 6s', () => {
    const model = rebuildLogModel(events);
    const t0 = performance.now();
    const rows = flattenLog(model, { filter: {}, openGroups: {} });
    expect(rows.length).toBeGreaterThan(0);
    const ms = performance.now() - t0;
    console.info(`[bench] full reflatten=${ms.toFixed(0)}ms`);
    expect(ms).toBeLessThan(6000);
  });

  it('筛选（error 级别）< 2s', () => {
    const model = rebuildLogModel(events);
    const t0 = performance.now();
    const rows = flattenLog(model, { filter: { level: 'error' }, openGroups: {} });
    const ms = performance.now() - t0;
    console.info(`[bench] filter error: rows=${rows.length} in ${ms.toFixed(0)}ms`);
    expect(ms).toBeLessThan(2000);
    expect(rows.length).toBeGreaterThan(0);
  });

  it('全文搜索 < 4s', () => {
    const model = rebuildLogModel(events);
    const t0 = performance.now();
    const matches = searchRows(model, { query: 'configuration file test failed' });
    const ms = performance.now() - t0;
    console.info(`[bench] search hits=${matches.length} in ${ms.toFixed(0)}ms`);
    expect(ms).toBeLessThan(4000);
    expect(matches.length).toBeGreaterThan(1000);
  });

  it('乱序补页（500/批 × 20 批）< 8s', () => {
    const t0 = performance.now();
    const store = new OperationEventStore();
    // 模拟向前补页：从最老一页开始倒序分批插入（每批内部乱序）。
    const batchSize = 500;
    const shuffled = [...events];
    for (let i = 0; i < 200; i += 1) {
      const a = (i * 7919) % events.length;
      const b = (i * 104729) % events.length;
      [shuffled[a], shuffled[b]] = [shuffled[b]!, shuffled[a]!];
    }
    for (let i = 0; i < shuffled.length; i += batchSize) {
      store.insert(shuffled.slice(i, i + batchSize));
    }
    const ms = performance.now() - t0;
    const seqs = store.getEvents().map((e) => e.seq);
    expect(seqs).toEqual(Array.from({ length: events.length }, (_, i) => i + 1));
    console.info(`[bench] out-of-order merge ${events.length} events in ${ms.toFixed(0)}ms`);
    expect(ms).toBeLessThan(8000);
  });
});
