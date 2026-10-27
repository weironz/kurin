import { beforeEach, describe, expect, it } from 'vitest';
import type { OperationEventEnvelope } from '../api/types';
import { OperationEventStore } from './store';
import { OperationTimeline } from './timeline';

let counter = 0;
/** 显式 seq 的事件构造器（去重/乱序测试必须控制 seq）。 */
function evAt(seq: number, payload: unknown): OperationEventEnvelope {
  return { seq, ts: '2026-10-27T08:00:00.000Z', kind: 'log', payload: payload as OperationEventEnvelope['payload'] };
}
/** 自增 seq（场景构造用）。 */
function ev(payload: unknown): OperationEventEnvelope {
  counter += 1;
  return evAt(counter, payload);
}
beforeEach(() => {
  counter = 0;
});

function logLine(): unknown {
  return {
    type: 'log',
    node: 'cp-1',
    stage: 'prepare',
    task: 'prepare/sysctl@cp-1',
    level: 'info',
    stream: 'stdout',
    line: `line ${Math.random()}`,
  };
}

describe('OperationEventStore：seq 去重与乱序', () => {
  it('重复 seq 幂等丢弃', () => {
    const store = new OperationEventStore();
    const a = evAt(1, logLine());
    const b = evAt(2, logLine());
    const r1 = store.insert([a, b]);
    expect(r1).toEqual({ added: 2, prepended: false });
    const r2 = store.insert([a, b, b]);
    expect(r2).toEqual({ added: 0, prepended: false });
    expect(store.size).toBe(2);
    expect(store.maxSeq).toBe(2);
    expect(store.minSeq).toBe(1);
  });

  it('乱序：seq2 到达后补 seq1 → 前插归并，顺序正确', () => {
    const store = new OperationEventStore();
    const e2 = evAt(2, logLine());
    const e1 = evAt(1, logLine());
    expect(store.insert([e2]).prepended).toBe(false);
    const r2 = store.insert([e1]);
    expect(r2).toEqual({ added: 1, prepended: true });
    expect(store.getEvents().map((e) => e.seq)).toEqual([1, 2]);
  });

  it('大段乱序归并保持全局升序', () => {
    const store = new OperationEventStore();
    const late = Array.from({ length: 50 }, (_, i) => evAt(i + 2, logLine()));
    const first = evAt(1, logLine());
    store.insert(late);
    store.insert([first]);
    expect(store.getEvents().map((e) => e.seq)).toEqual(
      Array.from({ length: 51 }, (_, i) => i + 1),
    );
  });

  it('非法事件跳过', () => {
    const store = new OperationEventStore();
    const r = store.insert([
      null as unknown as OperationEventEnvelope,
      { seq: NaN, ts: 'x', kind: 'log', payload: {} } as unknown as OperationEventEnvelope,
    ]);
    expect(r.added).toBe(0);
  });

  it('订阅在插入后触发；去重批次不触发', () => {
    const store = new OperationEventStore();
    let calls = 0;
    const stop = store.subscribe(() => {
      calls += 1;
    });
    store.insert([evAt(1, logLine())]);
    store.insert([evAt(2, logLine()), evAt(3, logLine())]);
    store.insert([evAt(1, logLine())]); // seq 已存在：不触发
    expect(calls).toBe(2);
    stop();
    store.insert([evAt(4, logLine())]);
    expect(calls).toBe(2);
  });
});

describe('OperationTimeline：视图一致性', () => {
  it('尾部追加增量路径与全量重建等价', () => {
    const events = [
      ev({ type: 'state', state: 'running' }),
      ev({ type: 'task-start', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare' }),
      ev({
        type: 'log',
        node: 'cp-1',
        stage: 'prepare',
        task: 'prepare/sysctl@cp-1',
        level: 'info',
        stream: 'stdout',
        line: 'hello',
      }),
      ev({ type: 'task-done', id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', ms: 10 }),
      ev({
        type: 'resource',
        stage: 'addons',
        kind: 'Deployment',
        namespace: 'ingress-nginx',
        name: 'controller',
        ready: false,
        detail: '0/2',
      }),
      ev({
        type: 'resource',
        stage: 'addons',
        kind: 'Deployment',
        namespace: 'ingress-nginx',
        name: 'controller',
        ready: true,
        detail: '2/2',
      }),
    ];
    const t1 = new OperationTimeline();
    for (const e of events) t1.insert([e]);
    // t2 用「乱序批次」插入（先资源后任务），触发 rebuild 路径。
    const t2 = new OperationTimeline();
    t2.insert([events[4]!, events[5]!]);
    t2.insert([events[0]!, events[1]!, events[2]!, events[3]!]);
    expect(t1.store.getEvents().map((e) => e.seq)).toEqual(
      t2.store.getEvents().map((e) => e.seq),
    );
    expect(t1.grid.tasks.get('prepare/sysctl@cp-1')!.state).toBe('done');
    expect(t2.grid.tasks.get('prepare/sysctl@cp-1')!.state).toBe('done');
    expect(t1.resources.get('Deployment|ingress-nginx|controller')!.ready).toBe(true);
    expect(t2.resources.get('Deployment|ingress-nginx|controller')!.ready).toBe(true);
    expect(t1.logModel.groups.size).toBe(1);
    expect(t2.logModel.groups.size).toBe(1);
  });

  it('progress 种子不进 store，但进网格', () => {
    const t = new OperationTimeline({
      tasks: [
        { id: 'prepare/sysctl@cp-1', node: 'cp-1', stage: 'prepare', state: 'pending' },
      ],
    });
    expect(t.store.size).toBe(0);
    expect(t.grid.tasks.size).toBe(1);
    // rebuild 后种子仍保留（pending 不被无事件清除）。
    t.insert([ev(logLine())]);
    expect(t.grid.tasks.get('prepare/sysctl@cp-1')!.state).toBe('pending');
  });
});
