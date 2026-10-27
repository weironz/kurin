/**
 * `/operations/{id}/events` 的 EventSource 封装（UI §6.8）。
 *
 * - `Last-Event-ID` 断线续读：浏览器 EventSource 原生在重连时自动携带，
 *   服务端按 `?after=` 补读；本封装在手动重连时也显式设置。
 * - 按 `seq` 去重：广播补读 + 订阅两路可能重叠，客户端按 `seq` 幂等过滤。
 * - 重连退避：指数 + 上限（cap），避免服务端关闭风暴。
 * - 心跳：15s 注释行（`:ping`），EventSource 不触发 onmessage；
 *   超过阈值没有数据时主动 close → 触发重连。
 */

export interface SseOptions {
  /** 初始 seq（从 React Query 缓存的快照续读）。 */
  lastSeq?: number;
  /** 退避基数，默认 500ms。 */
  baseDelayMs?: number;
  /** 退避上限，默认 15_000ms（与心跳同频）。 */
  maxDelayMs?: number;
  /** 心跳超时，默认 45_000ms（3 个心跳周期）。 */
  heartbeatTimeoutMs?: number;
  /** 总重试上限；默认 Infinity。达到后回调 onFatal。 */
  maxRetries?: number;
  /** 测试注入。 */
  eventSourceFactory?: (url: string) => EventSource;
  /** 测试注入时钟。 */
  now?: () => number;
  /** 测试注入调度器。 */
  schedule?: (fn: () => void, ms: number) => () => void;
}

export interface SseHandlers {
  /** 每个通过去重的事件信封。 */
  onEvent: (ev: OperationEventLike) => void;
  /** 连接建立（含重连后）。 */
  onOpen?: () => void;
  /** 非致命错误（将退避重连）。 */
  onError?: (err: Error, retryInMs: number) => void;
  /** 超过 maxRetries 或显式 close。 */
  onFatal?: (err: Error) => void;
}

/** 事件信封的最小形状（避免环依赖 types.ts 的完整定义）。 */
export interface OperationEventLike {
  seq: number;
  ts: string;
  kind: string;
  payload: unknown;
}

/** SSE 默认事件名为 `message`；服务端也可能用自定义 `event:` 字段。 */
const SSE_EVENT_NAME = 'message';

export class OperationEventStream {
  private lastSeq: number;
  private es: EventSource | null = null;
  private closed = false;
  private retries = 0;
  private timer: (() => void) | null = null;
  private heartbeatTimer: (() => void) | null = null;
  private readonly opts: Required<
    Omit<SseOptions, 'lastSeq' | 'eventSourceFactory' | 'now' | 'schedule'>
  > & {
    eventSourceFactory: (url: string) => EventSource;
    now: () => number;
    schedule: (fn: () => void, ms: number) => () => void;
  };

  constructor(
    private readonly operationId: string,
    private readonly handlers: SseHandlers,
    opts: SseOptions = {},
  ) {
    this.lastSeq = opts.lastSeq ?? 0;
    const schedule =
      opts.schedule ??
      ((fn: () => void, ms: number) => {
        const id = setTimeout(fn, ms);
        return () => clearTimeout(id);
      });
    this.opts = {
      baseDelayMs: opts.baseDelayMs ?? 500,
      maxDelayMs: opts.maxDelayMs ?? 15_000,
      heartbeatTimeoutMs: opts.heartbeatTimeoutMs ?? 45_000,
      maxRetries: opts.maxRetries ?? Infinity,
      eventSourceFactory: opts.eventSourceFactory ?? ((url: string) => new EventSource(url)),
      now: opts.now ?? (() => Date.now()),
      schedule,
    };
  }

  /** 当前最大已见 seq（重连 / 缓存续读依据）。 */
  get currentSeq(): number {
    return this.lastSeq;
  }

  start(): void {
    if (this.closed) return;
    this.connect();
  }

  private connect(): void {
    if (this.closed) return;
    const base = `/api/v1/operations/${encodeURIComponent(this.operationId)}/events`;
    const url = this.lastSeq > 0 ? `${base}?after=${this.lastSeq}` : base;
    const es = this.opts.eventSourceFactory(url);
    this.es = es;

    es.onopen = () => {
      this.retries = 0;
      this.armHeartbeat();
      this.handlers.onOpen?.();
    };

    es.addEventListener(SSE_EVENT_NAME, (ev) => {
      this.armHeartbeat();
      this.handleMessage(ev as MessageEvent<string>);
    });

    es.onerror = () => {
      if (this.closed) return;
      // EventSource 在 readyState CLOSED 后不会自动重连，其余情况由浏览器处理；
      // 但为防长时间挂起，这里主动关闭并自行退避。
      this.teardown();
      this.retry(new Error(`SSE connection error for operation ${this.operationId}`));
    };
  }

  private handleMessage(ev: MessageEvent<string>): void {
    let parsed: OperationEventLike;
    try {
      parsed = JSON.parse(ev.data) as OperationEventLike;
    } catch {
      return; // 非事件负载（理论上不会出现）
    }
    if (typeof parsed.seq !== 'number') return;
    // 服务端带 `id:` 字段时以 lastEventId 为准（seq 与信封一致）：
    // 浏览器自动重连会把它作为 Last-Event-ID 头带回，与服务端补读口径相同。
    const idField = Number(ev.lastEventId);
    const seq = Number.isFinite(idField) && idField > 0 ? idField : parsed.seq;
    // seq 去重：补读 + 订阅两路重叠时按 seq 幂等。
    if (seq <= this.lastSeq) return;
    this.lastSeq = seq;
    this.handlers.onEvent(parsed);
  }

  private retry(err: Error): void {
    if (this.closed) return;
    if (this.retries >= this.opts.maxRetries) {
      this.handlers.onFatal?.(err);
      this.close();
      return;
    }
    // 指数退避 + 上限
    const backoff = Math.min(this.opts.baseDelayMs * 2 ** this.retries, this.opts.maxDelayMs);
    this.retries += 1;
    this.handlers.onError?.(err, backoff);
    this.timer = this.opts.schedule(() => this.connect(), backoff);
  }

  /** 15s 心跳注释行不会触发 message；45s 无任何数据视为死连接。 */
  private armHeartbeat(): void {
    this.heartbeatTimer?.();
    const { heartbeatTimeoutMs, schedule, now } = this.opts;
    const startedAt = now();
    this.heartbeatTimer = schedule(() => {
      if (now() - startedAt >= heartbeatTimeoutMs && !this.closed) {
        this.teardown();
        this.retry(new Error('SSE heartbeat timeout'));
      }
    }, heartbeatTimeoutMs);
  }

  private teardown(): void {
    this.heartbeatTimer?.();
    this.heartbeatTimer = null;
    this.timer?.();
    this.timer = null;
    if (this.es) {
      // 摘除 handler 再 close，避免 onerror 被再次触发
      this.es.onopen = null;
      this.es.onerror = null;
      this.es.close();
      this.es = null;
    }
  }

  close(): void {
    this.closed = true;
    this.teardown();
  }
}

/**
 * 便捷工厂：回调 API，适配 React state（事件 append）。
 * 用法：
 * ```ts
 * const stop = subscribeOperationEvents(opId, {
 *   lastSeq: cached?.seq ?? 0,
 *   onEvent: (ev) => setEvents((prev) => [...prev, ev]),
 * });
 * ```
 */
export function subscribeOperationEvents(
  operationId: string,
  handlers: SseHandlers,
  opts?: SseOptions,
): () => void {
  const stream = new OperationEventStream(operationId, handlers, opts);
  stream.start();
  return () => stream.close();
}
