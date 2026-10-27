/**
 * 操作时间线（WP-63）：事件存储 + 三个派生视图（进度网格 / 日志行模型 / 资源就绪表）。
 *
 * - 尾部追加（SSE 实时、顺序分页）走增量路径；
 * - 向前补页（历史加载更早日志）走全量重建路径（低频）；
 * - 通知合并到 store 的订阅（一次 insert 一次回调）。
 */
import type { OperationEventEnvelope, OperationProgress } from '../api/types';
import { applyGridEvent, createGrid, type GridModel } from './grid';
import {
  appendLogEvent,
  createLogModel,
  rebuildLogModel,
  type LogModel,
} from './logmodel';
import { OperationEventStore } from './store';

/** 资源就绪表的最新一行（`Resource` 事件，同一资源以最大 seq 为准）。 */
export interface ResourceRow {
  seq: number;
  ts: string;
  stage: string;
  kind: string;
  name: string;
  namespace: string | null;
  ready: boolean;
  detail?: string;
}

export class OperationTimeline {
  readonly store = new OperationEventStore();
  grid: GridModel;
  logModel: LogModel;
  /** `kind|ns|name` → 最新状态（就绪跟踪器每 2s 刷新）。 */
  readonly resources = new Map<string, ResourceRow>();

  private readonly seed?: OperationProgress;

  constructor(seed?: OperationProgress) {
    this.seed = seed;
    this.grid = createGrid(seed);
    this.logModel = createLogModel();
  }

  /**
   * 插入一批事件（seq 去重 + 乱序归并），增量或全量更新派生视图。
   * @returns 实际新增事件数。
   */
  insert(events: Iterable<OperationEventEnvelope>): number {
    const { added, prepended } = this.store.insert(events);
    if (added === 0) return 0;
    if (prepended) {
      this.rebuild();
    } else {
      const all = this.store.getEvents();
      for (let i = all.length - added; i < all.length; i += 1) {
        this.applyIncremental(all[i]!);
      }
    }
    return added;
  }

  /** 订阅派生视图变化（store 插入后同步回调一次）。 */
  subscribe(fn: () => void): () => void {
    return this.store.subscribe(fn);
  }

  private applyIncremental(ev: OperationEventEnvelope): void {
    applyGridEvent(this.grid, ev);
    appendLogEvent(this.logModel, ev);
    const body = ev.payload as { type: string; stage?: string; kind?: string; name?: string; namespace?: string | null; ready?: boolean; detail?: string };
    if (body.type === 'resource' && body.kind && body.name) {
      const key = `${body.kind}|${body.namespace ?? ''}|${body.name}`;
      const existing = this.resources.get(key);
      if (existing == null || ev.seq > existing.seq) {
        this.resources.set(key, {
          seq: ev.seq,
          ts: ev.ts,
          stage: body.stage ?? '',
          kind: body.kind,
          name: body.name,
          namespace: body.namespace ?? null,
          ready: body.ready ?? false,
          detail: body.detail,
        });
      }
    }
  }

  private rebuild(): void {
    const events = this.store.getEvents();
    this.grid = createGrid(this.seed);
    for (const ev of events) applyGridEvent(this.grid, ev);
    this.logModel = rebuildLogModel(events);
    this.resources.clear();
    for (const ev of events) {
      const body = ev.payload as { type: string; stage?: string; kind?: string; name?: string; namespace?: string | null; ready?: boolean; detail?: string };
      if (body.type === 'resource' && body.kind && body.name) {
        const key = `${body.kind}|${body.namespace ?? ''}|${body.name}`;
        const existing = this.resources.get(key);
        if (existing == null || ev.seq > existing.seq) {
          this.resources.set(key, {
            seq: ev.seq,
            ts: ev.ts,
            stage: body.stage ?? '',
            kind: body.kind,
            name: body.name,
            namespace: body.namespace ?? null,
            ready: body.ready ?? false,
            detail: body.detail,
          });
        }
      }
    }
  }
}
