/**
 * 操作事件存储（WP-63）：按 `seq` 去重、乱序安全的单一时间线。
 *
 * 三个来源汇入同一存储（UI §6.8：补读 + 订阅两路可能重叠）：
 * - `GET /operations/{id}` 的 progress 快照（种子，不进本存储）；
 * - `GET /operations/{id}/logs` 历史页 —— 可能晚于 SSE 到达（向前补页）；
 * - SSE 实时事件 —— 与历史页/重连补读重叠。
 *
 * 插入返回是否发生了「前插」（prepend）：调用方据此决定增量追加还是全量重建派生模型。
 */
import type { OperationEventEnvelope } from '../api/types';

/** 订阅回调：插入批次后同步通知一次（同一批 500 行只刷一次）。 */
export type StoreListener = () => void;

export interface InsertResult {
  /** 实际新增的事件数（重复 `seq` 丢弃）。 */
  added: number;
  /** 是否有事件插到了既有事件之前（历史向前补页 / 乱序到达）。 */
  prepended: boolean;
}

export class OperationEventStore {
  private events: OperationEventEnvelope[] = [];
  private seen = new Set<number>();
  private listeners = new Set<StoreListener>();

  /** 已收集事件数。 */
  get size(): number {
    return this.events.length;
  }

  /** 按序事件（只读视图；调用方不得修改）。 */
  getEvents(): readonly OperationEventEnvelope[] {
    return this.events;
  }

  /** 最大 seq；空存储为 0。 */
  get maxSeq(): number {
    return this.events.length > 0 ? this.events[this.events.length - 1]!.seq : 0;
  }

  /** 最小 seq；空存储为 0（历史是否还有更早页的判断依据）。 */
  get minSeq(): number {
    return this.events.length > 0 ? this.events[0]!.seq : 0;
  }

  /** 插入一批事件：seq 去重 + 归并到升序时间线。 */
  insert(events: Iterable<OperationEventEnvelope>): InsertResult {
    const pending: OperationEventEnvelope[] = [];
    for (const ev of events) {
      if (ev == null || typeof ev.seq !== 'number' || !Number.isFinite(ev.seq)) continue;
      if (this.seen.has(ev.seq)) continue;
      this.seen.add(ev.seq);
      pending.push(ev);
    }
    if (pending.length === 0) return { added: 0, prepended: false };
    pending.sort((a, b) => a.seq - b.seq);

    const prevLast = this.events.length > 0 ? this.events[this.events.length - 1]!.seq : -Infinity;
    const prepended = pending[0]!.seq < prevLast;
    if (prepended) {
      // 向前补页：归并两个有序数组（低频路径，重建方负责派生模型）。
      this.events = mergeSorted(this.events, pending);
    } else {
      this.events.push(...pending);
    }
    for (const fn of this.listeners) fn();
    return { added: pending.length, prepended };
  }

  subscribe(fn: StoreListener): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }
}

/** 归并两个按 seq 升序的数组（不去重——去重已在 insert 完成）。 */
function mergeSorted(
  a: OperationEventEnvelope[],
  b: OperationEventEnvelope[],
): OperationEventEnvelope[] {
  const out = new Array<OperationEventEnvelope>(a.length + b.length);
  let i = 0;
  let j = 0;
  let k = 0;
  while (i < a.length && j < b.length) {
    out[k++] = a[i]!.seq < b[j]!.seq ? a[i++]! : b[j++]!;
  }
  while (i < a.length) out[k++] = a[i++]!;
  while (j < b.length) out[k++] = b[j++]!;
  return out;
}
