import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest';
import { OperationEventStream, subscribeOperationEvents } from './sse';
import type { OperationEventLike, SseHandlers } from './sse';

/** 测试用假 EventSource：可编程地推送事件 / 错误。 */
class FakeEventSource {
  static instances: FakeEventSource[] = [];
  readonly onmessageListeners = new Set<(ev: { data: string; lastEventId: string }) => void>();
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  readonly url: string;
  closed = false;

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(
    _type: string,
    listener: (ev: { data: string; lastEventId: string }) => void,
  ): void {
    this.onmessageListeners.add(listener);
  }

  close(): void {
    this.closed = true;
  }

  /** 测试驱动：推送一个事件信封（无 id: 字段）。 */
  emit(ev: OperationEventLike): void {
    this.emitWithId(undefined, ev);
  }

  /** 测试驱动：推送带 `id:` 字段的事件（模拟服务端设置 SSE id）。 */
  emitWithId(id: number | undefined, ev: OperationEventLike): void {
    for (const listener of this.onmessageListeners) {
      listener({ data: JSON.stringify(ev), lastEventId: id === undefined ? '' : String(id) });
    }
  }

  open(): void {
    this.onopen?.();
  }

  error(): void {
    this.onerror?.();
  }
}

function factory(url: string): EventSource {
  return new FakeEventSource(url) as unknown as EventSource;
}

function makeEvent(seq: number, kind = 'note'): OperationEventLike {
  return { seq, ts: '2026-10-27T08:00:00.000Z', kind, payload: { type: kind } };
}

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

/** 序列断言助手：收集事件并暴露 awaited 结果。 */
function collectingHandlers(): SseHandlers & { events: OperationEventLike[] } {
  const events: OperationEventLike[] = [];
  return {
    events,
    onEvent: (ev) => events.push(ev),
  };
}

describe('OperationEventStream seq 去重与续读', () => {
  it('按 seq 去重：重复与乱序回退的 seq 被丢弃', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, { eventSourceFactory: factory });
    stream.start();
    const es = FakeEventSource.instances[0]!;

    es.emit(makeEvent(1));
    es.emit(makeEvent(2));
    es.emit(makeEvent(2)); // 补读重叠：重复
    es.emit(makeEvent(1)); // 回退
    es.emit(makeEvent(3));

    expect(h.events.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(stream.currentSeq).toBe(3);
    stream.close();
  });

  it('SSE id: 字段（lastEventId）作为权威 seq', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, { eventSourceFactory: factory });
    stream.start();
    const es = FakeEventSource.instances[0]!;

    es.emitWithId(42, makeEvent(42)); // id 与信封 seq 一致
    // 浏览器原生重连场景：信封 seq 较小（补读重叠），但 id 字段权威
    es.emitWithId(50, makeEvent(43));
    expect(h.events.map((e) => e.seq)).toEqual([42, 43]);
    expect(stream.currentSeq).toBe(50);
    stream.close();
  });

  it('Last-Event-ID / after 参数：已有 seq 时重连带上 ?after=', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, {
      lastSeq: 41,
      eventSourceFactory: factory,
    });
    stream.start();
    const es = FakeEventSource.instances[0]!;
    expect(es.url).toBe('/api/v1/operations/op-1/events?after=41');

    es.emit(makeEvent(42));
    es.error(); // 触发重连
    vi.advanceTimersByTime(500);
    const es2 = FakeEventSource.instances[1]!;
    expect(es2.url).toBe('/api/v1/operations/op-1/events?after=42');
    expect(es2.closed).toBe(false);
    stream.close();
  });

  it('首轮无 seq 时 URL 不带 after', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, { eventSourceFactory: factory });
    stream.start();
    expect(FakeEventSource.instances[0]!.url).toBe('/api/v1/operations/op-1/events');
    stream.close();
  });
});

describe('OperationEventStream 重连退避', () => {
  it('指数退避且封顶：500ms → 1s → 2s → 4s（cap 内）', () => {
    const h = collectingHandlers();
    const retryDelays: number[] = [];
    const stream = new OperationEventStream(
      'op-1',
      {
        ...h,
        onError: (_err, delay) => retryDelays.push(delay),
      },
      {
        eventSourceFactory: factory,
        baseDelayMs: 500,
        maxDelayMs: 2_000,
      },
    );
    stream.start();

    const es1 = FakeEventSource.instances[0]!;
    es1.error();
    expect(retryDelays).toEqual([500]);
    // 退避 500ms 内不应重连
    expect(FakeEventSource.instances).toHaveLength(1);
    vi.advanceTimersByTime(499);
    expect(FakeEventSource.instances).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeEventSource.instances).toHaveLength(2);

    const es2 = FakeEventSource.instances[1]!;
    es2.error();
    expect(retryDelays).toEqual([500, 1_000]);
    vi.advanceTimersByTime(1_000);
    expect(FakeEventSource.instances).toHaveLength(3);

    const es3 = FakeEventSource.instances[2]!;
    es3.error();
    expect(retryDelays).toEqual([500, 1_000, 2_000]); // 封顶
    stream.close();
  });

  it('onopen 重置退避计数', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, {
      eventSourceFactory: factory,
      baseDelayMs: 500,
    });
    stream.start();
    const es1 = FakeEventSource.instances[0]!;
    es1.error();
    vi.advanceTimersByTime(500);
    const es2 = FakeEventSource.instances[1]!;
    es2.open(); // 连接成功 → retries 归零
    es2.error();
    // 重连延迟回到基数 500 而不是 2_000
    vi.advanceTimersByTime(500);
    expect(FakeEventSource.instances).toHaveLength(3);
    stream.close();
  });

  it('maxRetries 超限后回调 onFatal 并停止', () => {
    const h = collectingHandlers();
    let fatal: Error | null = null;
    const stream = new OperationEventStream(
      'op-1',
      { ...h, onFatal: (err) => (fatal = err) },
      { eventSourceFactory: factory, maxRetries: 2, baseDelayMs: 500 },
    );
    stream.start();
    FakeEventSource.instances[0]!.error();
    vi.advanceTimersByTime(500);
    FakeEventSource.instances[1]!.error();
    vi.advanceTimersByTime(1_000);
    FakeEventSource.instances[2]!.error(); // 第 3 次失败 → 超过 maxRetries=2
    expect(fatal).toBeInstanceOf(Error);
    vi.advanceTimersByTime(60_000);
    expect(FakeEventSource.instances).toHaveLength(3);
  });
});

describe('OperationEventStream 心跳', () => {
  it('45s 无数据（无心跳）视为死连接并重连', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, {
      eventSourceFactory: factory,
      heartbeatTimeoutMs: 45_000,
      baseDelayMs: 500,
    });
    stream.start();
    const es1 = FakeEventSource.instances[0]!;
    es1.open();
    vi.advanceTimersByTime(45_000);
    // 心跳超时 → teardown + retry → 500ms 后新连接
    vi.advanceTimersByTime(500);
    expect(FakeEventSource.instances).toHaveLength(2);
    expect(es1.closed).toBe(true);
    stream.close();
  });

  it('收到数据会重置心跳窗口', () => {
    const h = collectingHandlers();
    const stream = new OperationEventStream('op-1', h, {
      eventSourceFactory: factory,
      heartbeatTimeoutMs: 45_000,
      baseDelayMs: 500,
    });
    stream.start();
    const es1 = FakeEventSource.instances[0]!;
    es1.open();
    vi.advanceTimersByTime(30_000);
    es1.emit(makeEvent(1)); // armHeartbeat 重置
    vi.advanceTimersByTime(30_000); // 距上次数据 30s < 45s
    expect(FakeEventSource.instances).toHaveLength(1);
    vi.advanceTimersByTime(15_000); // 到 45s
    vi.advanceTimersByTime(500);
    expect(FakeEventSource.instances).toHaveLength(2);
    stream.close();
  });
});

describe('subscribeOperationEvents 便捷工厂', () => {
  it('返回的 stop 函数关闭连接且停止重连', () => {
    const h = collectingHandlers();
    const stop = subscribeOperationEvents('op-9', h, {
      eventSourceFactory: factory,
      baseDelayMs: 500,
    });
    const es = FakeEventSource.instances[0]!;
    stop();
    expect(es.closed).toBe(true);
    es.error(); // close 后错误不应触发重连
    vi.advanceTimersByTime(60_000);
    expect(FakeEventSource.instances).toHaveLength(1);
  });
});
