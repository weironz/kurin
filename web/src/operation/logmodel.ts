/**
 * 日志控制台的行模型（WP-63，UI §8.5）。
 *
 * - 每个事件映射为一行：任务生命周期行、日志行（远端输出折叠在任务组下）、
 *   note、resource、state、stage 分隔；
 * - 纯函数 + 可变模型（append 增量、rebuild 全量），供 10 万行规模使用；
 * - 过滤/搜索/展平（collapse）为独立函数，Vitest 直接覆盖。
 */
import type {
  LogStream,
  LogLevel,
  OperationEventBody,
  OperationEventEnvelope,
  OperationState,
  TaskCellState,
} from '../api/types';

export type LogRowKind = 'task' | 'line' | 'terminal' | 'note' | 'resource' | 'stage' | 'opstate';

/** 控制台的一行（扁平渲染单元）。 */
export interface LogRow {
  seq: number;
  ts: string;
  rowKind: LogRowKind;
  /** 原始事件 `payload.type`（视图文案映射）。 */
  eventType: string;
  node?: string | null;
  stage?: string | null;
  taskId?: string | null;
  /** 行文本（log/note/task-warn）。 */
  text?: string;
  /** 行级别（渲染配色 + 级别筛选；所有行都有有效级别）。 */
  level: LogLevel;
  stream?: LogStream;
  /** terminal/task 行的任务终态。 */
  state?: TaskCellState | OperationState;
  ms?: number;
  reason?: string;
  /** resource 行。 */
  resourceKind?: string;
  resourceName?: string;
  namespace?: string | null;
  ready?: boolean;
  /** 所属任务组（组内行 + 组头行）；顶层行为 null。 */
  groupId?: string | null;
  /** 是否为组内子行（远端输出）。 */
  grouped?: boolean;
}

/** 任务组：一个任务的远端输出折叠在其下（UI §8.5「分组」）。 */
export interface TaskGroup {
  taskId: string;
  node: string | null;
  stage: string | null;
  state: TaskCellState;
  /** 组锚点（task-start 或第一条带 task 的日志行）。 */
  startSeq: number;
  startTs: string;
  ms?: number;
  reason?: string;
  lineCount: number;
}

export interface LogModel {
  /** 顶层条目按 seq 顺序：组锚点 / 独立行 / 终态行。 */
  entries: Array<{ kind: 'group'; group: TaskGroup } | { kind: 'row'; row: LogRow }>;
  groups: Map<string, TaskGroup>;
  /** taskId → 组内行。 */
  children: Map<string, LogRow[]>;
}

export function createLogModel(): LogModel {
  return { entries: [], groups: new Map(), children: new Map() };
}

function ensureGroup(model: LogModel, taskId: string, ev: OperationEventEnvelope): TaskGroup {
  let group = model.groups.get(taskId);
  if (group == null) {
    group = {
      taskId,
      node: null,
      stage: null,
      state: 'running',
      startSeq: ev.seq,
      startTs: ev.ts,
      lineCount: 0,
    };
    model.groups.set(taskId, group);
    model.children.set(taskId, []);
    model.entries.push({ kind: 'group', group });
  }
  return group;
}

function pushRow(model: LogModel, row: LogRow): void {
  model.entries.push({ kind: 'row', row });
}

function pushChild(model: LogModel, group: TaskGroup, row: LogRow): void {
  model.children.get(group.taskId)!.push(row);
  group.lineCount += 1;
}

/** 增量追加一个事件（seq 必须递增；乱序/补页请用 rebuildLogModel）。 */
export function appendLogEvent(model: LogModel, ev: OperationEventEnvelope): void {
  const body = ev.payload as OperationEventBody;
  switch (body.type) {
    case 'task-start': {
      // 组锚点即表头：不另推顶层行（flatten 由 TaskGroup 合成表头行）。
      const group = ensureGroup(model, body.id, ev);
      group.node = body.node;
      group.stage = body.stage;
      group.state = 'running';
      break;
    }
    case 'task-skipped':
    case 'task-done':
    case 'task-failed':
    case 'task-reconciled': {
      const group = ensureGroup(model, body.id, ev);
      group.node = body.node;
      group.stage = body.stage;
      const state: TaskCellState =
        body.type === 'task-done'
          ? 'done'
          : body.type === 'task-failed'
            ? 'failed'
            : body.type === 'task-skipped'
              ? 'skipped'
              : 'reconciled';
      group.state = state;
      if (body.type === 'task-done') group.ms = body.ms;
      if (body.type === 'task-failed') group.reason = body.reason;
      if (body.type === 'task-skipped') group.reason = body.reason;
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'terminal',
        eventType: body.type,
        node: body.node,
        stage: body.stage,
        taskId: body.id,
        level: state === 'failed' ? 'error' : 'info',
        state,
        ms: body.type === 'task-done' ? body.ms : undefined,
        reason: body.type === 'task-failed' || body.type === 'task-skipped' ? body.reason : undefined,
        groupId: body.id,
      });
      break;
    }
    case 'task-warn': {
      const group = ensureGroup(model, body.id, ev);
      group.node = body.node;
      group.stage = body.stage;
      group.state = 'warn';
      group.reason = body.message;
      pushChild(
        model,
        group,
        {
          seq: ev.seq,
          ts: ev.ts,
          rowKind: 'line',
          eventType: body.type,
          node: body.node,
          stage: body.stage,
          taskId: body.id,
          text: body.message,
          level: 'warn',
          stream: 'kurin',
          groupId: body.id,
          grouped: true,
        },
      );
      break;
    }
    case 'log': {
      const common = {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'line' as const,
        eventType: body.type,
        node: body.node ?? null,
        stage: body.stage ?? null,
        taskId: body.task ?? null,
        text: body.line,
        level: body.level,
        stream: body.stream,
      };
      if (body.task) {
        const group = ensureGroup(model, body.task, ev);
        if (group.node == null && body.node) group.node = body.node;
        if (group.stage == null && body.stage) group.stage = body.stage;
        pushChild(model, group, { ...common, groupId: body.task, grouped: true });
      } else {
        pushRow(model, { ...common, groupId: null, grouped: false });
      }
      break;
    }
    case 'step': {
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'line',
        eventType: body.type,
        node: body.node,
        stage: null,
        taskId: null,
        text: `${body.step}: ${body.detail}`,
        level: body.ok ? 'info' : 'error',
        stream: 'kurin',
        groupId: null,
        grouped: false,
      });
      break;
    }
    case 'note': {
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'note',
        eventType: body.type,
        level: body.level,
        text: body.message,
        groupId: null,
      });
      break;
    }
    case 'resource': {
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'resource',
        eventType: body.type,
        stage: body.stage,
        resourceKind: body.kind,
        resourceName: body.name,
        namespace: body.namespace ?? null,
        ready: body.ready,
        text: body.detail,
        level: body.ready ? 'info' : 'warn',
        groupId: null,
      });
      break;
    }
    case 'state': {
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'opstate',
        eventType: body.type,
        level:
          body.state === 'failed'
            ? 'error'
            : body.state === 'cancelled' || body.state === 'interrupted'
              ? 'warn'
              : 'info',
        state: body.state,
        reason: body.error,
        groupId: null,
      });
      break;
    }
    case 'stage-done': {
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'stage',
        eventType: body.type,
        stage: body.stage,
        level: 'info',
        groupId: null,
      });
      break;
    }
    case 'cluster-ready': {
      pushRow(model, {
        seq: ev.seq,
        ts: ev.ts,
        rowKind: 'note',
        eventType: body.type,
        level: 'info',
        groupId: null,
      });
      break;
    }
    default:
      break;
  }
}

/** 全量重建（历史向前补页 / 乱序到达后）。 */
export function rebuildLogModel(events: Iterable<OperationEventEnvelope>): LogModel {
  const model = createLogModel();
  for (const ev of events) appendLogEvent(model, ev);
  return model;
}

/* --------------------------------- 过滤 ---------------------------------- */

export interface LogFilter {
  node?: string;
  stage?: string;
  task?: string;
  /** 最低级别（含更高级别）。 */
  level?: LogLevel;
  stream?: LogStream;
  /** 仅错误与告警。 */
  errorsOnly?: boolean;
  /** 全文搜索（控制台内部维护）。 */
  query?: string;
}

export const LEVEL_RANK: Record<LogLevel, number> = { debug: 0, info: 1, warn: 2, error: 3 };

/** 行的搜索文本。 */
export function rowSearchText(row: LogRow): string {
  if (row.rowKind === 'task') return row.taskId ?? '';
  if (row.rowKind === 'terminal') return `${row.taskId ?? ''} ${row.reason ?? ''}`;
  if (row.rowKind === 'resource')
    return `${row.resourceKind ?? ''} ${row.resourceName ?? ''} ${row.text ?? ''}`;
  if (row.rowKind === 'opstate') return `${row.state ?? ''} ${row.reason ?? ''}`;
  if (row.rowKind === 'stage') return row.stage ?? '';
  return row.text ?? '';
}

/** 是否为错误行（跳到第一个错误 / 仅错误告警筛选的判定基础）。 */
export function isErrorRow(row: LogRow): boolean {
  return row.level === 'error' || row.state === 'failed';
}

function rowMatchesFilter(row: LogRow, f: LogFilter): boolean {
  if (f.node && row.node !== f.node) return false;
  if (f.stage && row.stage !== f.stage) return false;
  if (f.task && row.taskId !== f.task) return false;
  if (f.stream && row.stream !== f.stream) return false;
  if (f.level != null && LEVEL_RANK[row.level] < LEVEL_RANK[f.level]) return false;
  if (f.errorsOnly && LEVEL_RANK[row.level] < LEVEL_RANK.warn) return false;
  return true;
}

/** 组头行（由 TaskGroup 合成；搜索/跳转的锚点）。 */
function groupHeaderRow(group: TaskGroup): LogRow {
  return {
    seq: group.startSeq,
    ts: group.startTs,
    rowKind: 'task',
    eventType: 'task-start',
    node: group.node,
    stage: group.stage,
    taskId: group.taskId,
    level: group.state === 'failed' ? 'error' : group.state === 'warn' ? 'warn' : 'info',
    state: group.state,
    ms: group.ms,
    reason: group.reason,
    groupId: group.taskId,
    grouped: false,
  };
}

/** 组是否展开：默认失败自动展开（UI §8.5），用户手动开关优先。 */
export function isGroupOpen(
  group: TaskGroup,
  manual: Readonly<Record<string, boolean>>,
  allOpen = false,
): boolean {
  if (allOpen) return true;
  const override = manual[group.taskId];
  if (override != null) return override;
  return group.state === 'failed';
}

export interface FlattenOptions {
  filter: LogFilter;
  /** 用户手动开关（taskId → 展开？）。 */
  openGroups: Readonly<Record<string, boolean>>;
  /** 全展开（搜索命中计算用）。 */
  allOpen?: boolean;
}

/**
 * 展平为渲染行序列（过滤 + 分组折叠）。
 * 注意 `filter.query` 不参与过滤：搜索只负责高亮与跳转，不缩小行集。
 */
export function flattenLog(model: LogModel, opts: FlattenOptions): LogRow[] {
  const f = opts.filter;
  const rows: LogRow[] = [];
  for (const entry of model.entries) {
    if (entry.kind === 'row') {
      if (rowMatchesFilter(entry.row, f)) rows.push(entry.row);
      continue;
    }
    const group = entry.group;
    const children = model.children.get(group.taskId) ?? [];
    const visibleChildren: LogRow[] = [];
    for (const child of children) {
      if (rowMatchesFilter(child, f)) visibleChildren.push(child);
    }
    const header = groupHeaderRow(group);
    if (!rowMatchesFilter(header, f) && visibleChildren.length === 0) continue;
    rows.push(header);
    if (isGroupOpen(group, opts.openGroups, opts.allOpen)) rows.push(...visibleChildren);
  }
  return rows;
}

/** 搜索命中（不考虑折叠——折叠组里的命中可跳转并自动展开）。 */
export interface LogMatch {
  seq: number;
  groupId: string | null;
}

/** 在「全部展开」的行序列里找 query 命中（其余筛选条件照常生效）。 */
export function searchRows(model: LogModel, filter: LogFilter): LogMatch[] {
  const query = filter.query?.trim().toLowerCase() ?? '';
  if (!query) return [];
  const rows = flattenLog(model, { filter, openGroups: {}, allOpen: true });
  const matches: LogMatch[] = [];
  for (const row of rows) {
    if (rowSearchText(row).toLowerCase().includes(query)) {
      matches.push({ seq: row.seq, groupId: row.groupId ?? null });
    }
  }
  return matches;
}

/** 第一个错误行（全展开视图）；没有则 null。 */
export function findFirstError(model: LogModel, filter: LogFilter): LogMatch | null {
  const rows = flattenLog(model, { filter, openGroups: {}, allOpen: true });
  for (const row of rows) {
    if (isErrorRow(row)) return { seq: row.seq, groupId: row.groupId ?? null };
  }
  return null;
}

/* --------------------------- 下载序列化（§8.5） --------------------------- */

/** `.jsonl`：与 `kurin logs --json` 一致——每行一个事件信封。 */
export function serializeEventsJsonl(events: Iterable<OperationEventEnvelope>): string {
  const lines: string[] = [];
  for (const ev of events) lines.push(JSON.stringify(ev));
  return lines.length > 0 ? `${lines.join('\n')}\n` : '';
}

/** `.log`（人读）：`UTC 时间戳 级别 节点 任务 文本`。 */
export function serializeEventsLogText(events: Iterable<OperationEventEnvelope>): string {
  const lines: string[] = [];
  for (const ev of events) {
    const body = ev.payload as OperationEventBody;
    let level = 'INFO';
    let node = '-';
    let task = '-';
    let text = '';
    switch (body.type) {
      case 'task-start':
        level = 'INFO';
        node = body.node;
        task = body.id;
        text = 'started';
        break;
      case 'task-done':
        level = 'DONE';
        node = body.node;
        task = body.id;
        text = `done in ${formatPreciseMs(body.ms)}`;
        break;
      case 'task-failed':
        level = 'FAIL';
        node = body.node;
        task = body.id;
        text = body.reason;
        break;
      case 'task-skipped':
        level = 'SKIP';
        node = body.node;
        task = body.id;
        text = body.reason ?? 'skipped';
        break;
      case 'task-reconciled':
        level = 'RECON';
        node = body.node;
        task = body.id;
        text = 'reconciled';
        break;
      case 'task-warn':
        level = 'WARN';
        node = body.node;
        task = body.id;
        text = body.message;
        break;
      case 'log':
        level = body.level.toUpperCase();
        node = body.node ?? '-';
        task = body.task ?? '-';
        text = body.line;
        break;
      case 'note':
        level = body.level.toUpperCase();
        text = body.message;
        break;
      case 'step':
        level = body.ok ? 'INFO' : 'ERROR';
        node = body.node;
        text = `${body.step}: ${body.detail}`;
        break;
      case 'state':
        level = 'STATE';
        text = body.error ? `${body.state}: ${body.error}` : body.state;
        break;
      case 'resource':
        level = body.ready ? 'RES' : 'RES!';
        task = `${body.stage}`;
        text = `${body.kind} ${body.namespace ? `${body.namespace}/` : ''}${body.name} ${
          body.detail ?? (body.ready ? 'ready' : 'not ready')
        }`;
        break;
      case 'stage-done':
        level = 'STAGE';
        task = body.stage;
        text = 'stage done';
        break;
      case 'cluster-ready':
        level = 'DONE';
        text = 'cluster ready';
        break;
      default:
        break;
    }
    lines.push(`${ev.ts}  ${level.padEnd(5)}  ${node.padEnd(12)}  ${task.padEnd(34)}  ${text}`);
  }
  return lines.length > 0 ? `${lines.join('\n')}\n` : '';
}

/** 毫秒 → 「42.381s / 1m42.381s」（§8.5 示例口径）。 */
export function formatPreciseMs(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '0ms';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const totalSec = ms / 1000;
  const m = Math.floor(totalSec / 60);
  const s = totalSec - m * 60;
  if (m < 1) return `${s.toFixed(3)}s`;
  return `${m}m${s.toFixed(3)}s`;
}
