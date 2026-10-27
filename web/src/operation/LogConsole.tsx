/**
 * 日志控制台（WP-63，UI §8.5）。
 *
 * - 虚拟滚动（@tanstack/react-virtual）：10 万行不卡；
 * - 任务分组折叠：失败的组自动展开，用户可手动开关（优先）；
 * - 筛选：节点/阶段/任务/级别/流；进度格子的点击写入「阶段 + 节点」；
 * - 搜索：高亮 + 上一个/下一个 + 跳到第一个错误（自动展开所在组）；
 * - 跟随：默认开；用户向上滚动自动暂停，出现「回到最新」；出现错误/失败自动暂停一次；
 * - 下载：`.log`（人读）/ `.jsonl`（机读），当前筛选或完整。
 */
import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import {
  Alert,
  App,
  Button,
  Checkbox,
  Dropdown,
  Input,
  Select,
  Space,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import {
  ArrowDownOutlined,
  CaretRightOutlined,
  DownloadOutlined,
  DownOutlined,
  EnterOutlined,
  UpOutlined,
} from '@ant-design/icons';
import type { LogStream, OperationEventEnvelope } from '../api/types';
import { t } from '../i18n';
import {
  formatLogTimeAbsolute,
  formatLogTimeRelative,
  formatLogTimeUtc,
  formatPreciseMs,
  type LogTimeMode,
} from '../utils/format';
import { splitHighlight } from './highlight';
import {
  findFirstError,
  flattenLog,
  isErrorRow,
  rowSearchText,
  searchRows,
  type LogFilter,
  type LogModel,
  type LogRow,
} from './logmodel';

const LINE_HEIGHT = 20;
const VIEWPORT_HEIGHT = 480;
const OVERSCAN = 12;

const LEVEL_COLOR: Record<string, string> = {
  debug: '#8c8c8c',
  info: '#262626',
  warn: '#d46b08',
  error: '#cf1322',
};
const STREAM_COLOR: Record<LogStream, string> = {
  kurin: '#531dab',
  stdout: '#0958d9',
  stderr: '#a8071a',
};

export interface LogConsoleProps {
  model: LogModel;
  /** 事件总数（下载提示用）。 */
  totalEvents: number;
  /** 事件序列（完整下载用；与 model 同源）。 */
  events: readonly OperationEventEnvelope[];
  /** 操作是否仍在进行（进行中提示下载为当前内容）。 */
  running: boolean;
  /** 操作开始时间（相对时间模式锚点）。 */
  opStartTs?: string | null;
  /** 外部写入的格子筛选（进度网格点击）。 */
  cellFilter: { stage?: string; node?: string } | null;
  onClearCellFilter: () => void;
}

export function LogConsole({
  model,
  totalEvents,
  events,
  running,
  opStartTs,
  cellFilter,
  onClearCellFilter,
}: LogConsoleProps) {
  const { message } = App.useApp();
  const [node, setNode] = useState<string | undefined>(undefined);
  const [stage, setStage] = useState<string | undefined>(undefined);
  const [task, setTask] = useState<string | undefined>(undefined);
  const [level, setLevel] = useState<string | undefined>(undefined);
  const [stream, setStream] = useState<string | undefined>(undefined);
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [query, setQuery] = useState('');
  const [matchIndex, setMatchIndex] = useState(0);
  const [timeMode, setTimeMode] = useState<LogTimeMode>('absolute');
  const [follow, setFollow] = useState(true);
  const [openGroups, setOpenGroups] = useState<Record<string, boolean>>({});
  const [autoPausedByError, setAutoPausedByError] = useState(false);

  const scrollRef = useRef<HTMLDivElement>(null);
  const wasAtBottomRef = useRef(true);
  const followRef = useRef(true);
  followRef.current = follow;
  const modelRef = useRef(model);
  modelRef.current = model;

  /* ------------------------------ 筛选与行 ------------------------------ */

  // 格子筛选合并进 stage/node（点击进度网格）。
  const effectiveStage = cellFilter?.stage ?? stage;
  const effectiveNode = cellFilter?.node ?? node;

  const filter: LogFilter = useMemo(
    () => ({
      node: effectiveNode || undefined,
      stage: effectiveStage || undefined,
      task: task || undefined,
      level: level as LogFilter['level'],
      stream: stream as LogFilter['stream'],
      errorsOnly,
      query,
    }),
    [effectiveNode, effectiveStage, task, level, stream, errorsOnly, query],
  );

  // model 是可变单例（性能）：以事件数为版本戳驱动重算，identity 永不变。
  const rows = useMemo(
    () => flattenLog(model, { filter, openGroups }),
    // totalEvents 是可变 model 的版本戳（identity 永不变），必须入依赖。
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [model, totalEvents, filter, openGroups],
  );
  const rowCount = rows.length;

  const nodes = useMemo(() => {
    const s = new Set<string>();
    for (const g of model.groups.values()) if (g.node) s.add(g.node);
    return Array.from(s).sort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, totalEvents]);
  const stages = useMemo(() => {
    const s = new Set<string>();
    for (const g of model.groups.values()) if (g.stage) s.add(g.stage);
    return Array.from(s).sort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, totalEvents]);
  const tasks = useMemo(
    () => Array.from(model.groups.keys()).sort(),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [model, totalEvents],
  );
  /* -------------------------------- 虚拟滚动 ------------------------------ */

  // jsdom（无布局）下 clientHeight=0，虚拟器不产出可见行；
  // 用「已渲染行数」探测，退化为普通渲染（浏览器里不会触发——首帧即有可见行）。
  const virtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => LINE_HEIGHT,
    overscan: OVERSCAN,
  });
  const seenErrorRef = useRef(false);
  if (running && !seenErrorRef.current && model.groups.size > 0) {
    let hasError = false;
    for (const g of model.groups.values()) {
      if (g.state === 'failed' || g.state === 'warn') {
        hasError = true;
        break;
      }
    }
    if (hasError) {
      seenErrorRef.current = true;
      if (followRef.current && !autoPausedByError) {
        setAutoPausedByError(true);
        setFollow(false);
      }
    }
  }
  if (!running) seenErrorRef.current = false;

  useLayoutEffect(() => {
    if (!follow || rowCount === 0) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [follow, rowCount]);

  const handleScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 4;
    wasAtBottomRef.current = atBottom;
    if (!atBottom && follow) {
      setFollow(false); // 用户向上滚动：自动暂停
      setAutoPausedByError(false);
    }
  };

  const backToLatest = () => {
    setAutoPausedByError(false);
    setFollow(true);
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  };

  /* --------------------------------- 搜索 --------------------------------- */

  const matches = useMemo(() => searchRows(model, filter), [model, filter]);
  const matchesRef = useRef(matches);
  matchesRef.current = matches;
  const queryRef = useRef(query);
  queryRef.current = query;

  // 搜索命中行：seq 集合（高亮当前条）。
  const currentMatch = matches.length > 0 ? matches[matchIndex % matches.length] : null;
  const matchSeqs = useMemo(() => {
    if (!query) return null;
    return new Set(matches.map((m) => m.seq));
  }, [matches, query]);

  const scrollToSeq = (seq: number, groupId: string | null) => {
    if (groupId) {
      setOpenGroups((prev) => ({ ...prev, [groupId]: true }));
      message.success(t('logs.search.openGroup'));
    }
    // 等展开重渲染后定位。
    requestAnimationFrame(() => {
      const idx = modelRef.current
        ? rows.findIndex((r) => r.seq === seq)
        : -1;
      if (idx >= 0) virtualizer.scrollToIndex(idx, { align: 'center' });
    });
  };

  const jumpMatch = (dir: 1 | -1) => {
    const ms = matchesRef.current;
    if (ms.length === 0) return;
    const next = (matchIndex + dir + ms.length) % ms.length;
    setMatchIndex(next);
    const m = ms[next]!;
    scrollToSeq(m.seq, m.groupId);
  };

  const jumpToFirstError = () => {
    const m = findFirstError(modelRef.current, filter);
    if (!m) {
      message.info(t('logs.search.noError'));
      return;
    }
    scrollToSeq(m.seq, m.groupId);
  };

  /* -------------------------------- 下载 ---------------------------------- */

  const download = (format: 'log' | 'jsonl', scope: 'visible' | 'all') => {
    const lines = scope === 'visible' ? rows : [];
    let content: string;
    let filename: string;
    if (format === 'jsonl') {
      const source = scope === 'all' ? events : null;
      content =
        source != null
          ? serializeAllJsonl(source)
          : lines.map((r) => JSON.stringify(r)).join('\n') + (lines.length ? '\n' : '');
      filename = 'kurin-log.jsonl';
    } else {
      const source = scope === 'all' ? events : null;
      content =
        source != null
          ? serializeAllLogText(source)
          : lines
              .map((r) => `${r.ts}  ${String(r.level).toUpperCase().padEnd(5)}  ${r.text ?? ''}`)
              .join('\n') + (lines.length ? '\n' : '');
      filename = 'kurin.log';
    }
    const blob = new Blob([content], { type: 'text/plain;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  };

  /* -------------------------------- 渲染 ---------------------------------- */

  const realVisible = virtualizer.getVirtualItems();
  const fallbackVisible =
    realVisible.length > 0
      ? realVisible
      : rows.slice(0, 200).map((_, i) => {
          const start = i * LINE_HEIGHT;
          return { index: i, key: i, size: LINE_HEIGHT, start, end: start + LINE_HEIGHT };
        });
  const visible = fallbackVisible;
  const totalHeight = Math.max(rowCount * LINE_HEIGHT, virtualizer.getTotalSize());
  const paddingTop = visible.length > 0 ? visible[0]!.start : 0;
  const paddingBottom =
    visible.length > 0 ? totalHeight - visible[visible.length - 1]!.end : 0;

  return (
    <div data-testid="log-console">
      <Space wrap style={{ marginBottom: 8 }} size={[8, 8]}>
        <Select
          allowClear
          placeholder={t('logs.filter.node')}
          style={{ minWidth: 120 }}
          value={node}
          onChange={setNode}
          options={nodes.map((n) => ({ value: n, label: n }))}
          size="small"
          data-filter="node"
        />
        <Select
          allowClear
          placeholder={t('logs.filter.stage')}
          style={{ minWidth: 120 }}
          value={stage}
          onChange={setStage}
          options={stages.map((s) => ({ value: s, label: s }))}
          size="small"
          data-filter="stage"
        />
        <Select
          allowClear
          showSearch
          placeholder={t('logs.filter.task')}
          style={{ minWidth: 160 }}
          value={task}
          onChange={setTask}
          options={tasks.map((s) => ({ value: s, label: s }))}
          size="small"
          data-filter="task"
        />
        <Select
          allowClear
          placeholder={t('logs.filter.level')}
          style={{ minWidth: 110 }}
          value={level}
          onChange={setLevel}
          size="small"
          data-filter="level"
          options={[
            { value: 'debug', label: t('logs.level.debug') },
            { value: 'info', label: t('logs.level.info') },
            { value: 'warn', label: t('logs.level.warn') },
            { value: 'error', label: t('logs.level.error') },
          ]}
        />
        <Select
          allowClear
          placeholder={t('logs.filter.stream')}
          style={{ minWidth: 110 }}
          value={stream}
          onChange={setStream}
          size="small"
          data-filter="stream"
          options={[
            { value: 'kurin', label: t('logs.stream.kurin') },
            { value: 'stdout', label: t('logs.stream.stdout') },
            { value: 'stderr', label: t('logs.stream.stderr') },
          ]}
        />
        <Checkbox
          checked={errorsOnly}
          onChange={(e) => setErrorsOnly(e.target.checked)}
          data-filter="errorsOnly"
        >
          {t('logs.filter.errorsOnly')}
        </Checkbox>
        <Input.Search
          placeholder={t('logs.search.placeholder')}
          style={{ width: 220 }}
          size="small"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setMatchIndex(0);
          }}
          onSearch={() => jumpMatch(1)}
          data-filter="search"
        />
        {query && (
          <Space size={4}>
            <Button size="small" icon={<UpOutlined />} onClick={() => jumpMatch(-1)} aria-label={t('logs.search.prev')} />
            <Button size="small" icon={<DownOutlined />} onClick={() => jumpMatch(1)} aria-label={t('logs.search.next')} />
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {matches.length > 0
                ? t('logs.search.match', { i: (matchIndex % matches.length) + 1, n: matches.length })
                : t('logs.search.noMatch')}
            </Typography.Text>
          </Space>
        )}
        <Button size="small" onClick={jumpToFirstError}>
          {t('logs.search.jumpError')}
        </Button>
        <Button
          size="small"
          type={follow ? 'primary' : 'default'}
          icon={<ArrowDownOutlined />}
          onClick={() => (follow ? setFollow(false) : backToLatest())}
          data-testid="follow-button"
        >
          {follow ? t('logs.follow') : t('logs.backToLatest')}
        </Button>
        <Button
          size="small"
          type={timeMode === 'relative' ? 'primary' : 'default'}
          onClick={() => setTimeMode(timeMode === 'absolute' ? 'relative' : 'absolute')}
          data-testid="time-mode-button"
        >
          {t('logs.timeMode')}
        </Button>
        <Dropdown
          menu={{
            items: [
              {
                key: 'log-visible',
                label: t('logs.download.visible', { n: rowCount }),
                onClick: () => download('log', 'visible'),
              },
              {
                key: 'log-all',
                label: t('logs.download.all', { n: totalEvents }),
                onClick: () => download('log', 'all'),
              },
              { type: 'divider' },
              {
                key: 'jsonl-visible',
                label: `${t('logs.download.visible', { n: rowCount })} · .jsonl`,
                onClick: () => download('jsonl', 'visible'),
              },
              {
                key: 'jsonl-all',
                label: `${t('logs.download.all', { n: totalEvents })} · .jsonl`,
                onClick: () => download('jsonl', 'all'),
              },
            ],
          }}
        >
          <Button size="small" icon={<DownloadOutlined />}>
            {t('logs.download')}
          </Button>
        </Dropdown>
      </Space>

      {cellFilter && (
        <Alert
          style={{ marginBottom: 8 }}
          type="info"
          showIcon={false}
          message={t('operationDetail.gridFilter.active', {
            stage: cellFilter.stage ?? '',
            node: cellFilter.node || t('operationDetail.grid.cluster'),
          })}
          action={
            <Button size="small" onClick={onClearCellFilter}>
              {t('operationDetail.gridFilter.clear')}
            </Button>
          }
        />
      )}

      {!follow && (
        <Alert
          style={{ marginBottom: 8 }}
          type={autoPausedByError ? 'warning' : 'info'}
          message={autoPausedByError ? t('logs.followPaused') : t('logs.followPaused')}
          action={
            <Button size="small" type="primary" icon={<EnterOutlined />} onClick={backToLatest}>
              {t('logs.backToLatest')}
            </Button>
          }
        />
      )}

      <div
        ref={scrollRef}
        onScroll={handleScroll}
        data-testid="log-viewport"
        style={{
          height: VIEWPORT_HEIGHT,
          overflowY: 'auto',
          background: '#0d1117',
          borderRadius: 6,
          padding: '4px 0',
        }}
      >
        {rowCount === 0 ? (
          <div style={{ color: '#8b949e', padding: '12px 12px', fontSize: 12 }}>
            {t('logs.empty')}
          </div>
        ) : (
          <div style={{ height: totalHeight, position: 'relative' }}>
            {paddingTop > 0 && <div style={{ height: paddingTop }} />}
            {visible.map((vi) => {
              const row = rows[vi.index]!;
              return (
                <div
                  key={row.seq}
                  data-seq={row.seq}
                  data-row-kind={row.rowKind}
                  style={{
                    position: 'absolute',
                    top: 0,
                    left: 0,
                    width: '100%',
                    height: vi.size,
                    transform: `translateY(${vi.start - paddingTop}px)`,
                    fontFamily:
                      'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace',
                    fontSize: 12,
                    lineHeight: `${LINE_HEIGHT}px`,
                    whiteSpace: 'pre',
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                  }}
                >
                  <LogRowView
                    row={row}
                    timeMode={timeMode}
                    opStartTs={opStartTs}
                    query={query}
                    isCurrentMatch={currentMatch?.seq === row.seq}
                    isMatch={matchSeqs?.has(row.seq) ?? false}
                    openGroups={openGroups}
                    onToggleGroup={(taskId, open) =>
                      setOpenGroups((prev) => ({ ...prev, [taskId]: open }))
                    }
                  />
                </div>
              );
            })}
            {paddingBottom > 0 && <div style={{ height: paddingBottom }} />}
          </div>
        )}
      </div>
    </div>
  );
}

/* ------------------------------- 行渲染 ---------------------------------- */

interface RowViewProps {
  row: LogRow;
  timeMode: LogTimeMode;
  opStartTs?: string | null;
  query: string;
  isCurrentMatch: boolean;
  isMatch: boolean;
  openGroups: Readonly<Record<string, boolean>>;
  onToggleGroup: (taskId: string, open: boolean) => void;
}

function LogRowView({
  row,
  timeMode,
  opStartTs,
  query,
  isCurrentMatch,
  isMatch,
  openGroups,
  onToggleGroup,
}: RowViewProps) {
  const time =
    timeMode === 'absolute' ? formatLogTimeAbsolute(row.ts) : formatLogTimeRelative(row.ts, opStartTs);
  const isErr = isErrorRow(row);
  const bg = isCurrentMatch
    ? '#613617'
    : isErr
      ? '#2d1117'
      : row.rowKind === 'stage'
        ? '#161b22'
        : 'transparent';
  // 任务组头：显示折叠箭头、任务 id、耗时、行数。
  if (row.rowKind === 'task') {
    const groupId = row.groupId ?? row.taskId ?? '';
    const open = openGroups[groupId] ?? row.state === 'failed';
    return (
      <div
        data-task-header={row.taskId}
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 6,
          padding: '0 12px',
          background: bg,
          color: row.state === 'failed' ? '#ff7b72' : row.state === 'warn' ? '#e3b341' : '#c9d1d9',
          cursor: 'pointer',
        }}
        onClick={() => onToggleGroup(groupId, !open)}
        role="button"
        aria-expanded={open}
        aria-label={row.taskId ?? undefined}
      >
        <CaretRightOutlined
          rotate={open ? 90 : 0}
          style={{ fontSize: 10, color: '#8b949e' }}
        />
        <span style={{ color: '#8b949e' }}>{time}</span>
        <span style={{ color: row.state === 'running' ? '#79c0ff' : undefined }}>
          {row.taskId}
        </span>
        {row.state === 'running' && (
          <Tag color="processing" style={{ fontSize: 10, lineHeight: '14px', margin: 0 }}>
            {t('logs.task.running')}
          </Tag>
        )}
        {row.state === 'failed' && row.reason && (
          <span style={{ color: '#ff7b72' }}>{row.reason}</span>
        )}
      </div>
    );
  }
  const text = rowSearchText(row);
  const parts = splitHighlight(text, query);
  const indent = row.grouped ? 24 : 12;
  const color =
    row.rowKind === 'line' && row.stream
      ? STREAM_COLOR[row.stream]
      : LEVEL_COLOR[row.level] ?? '#c9d1d9';
  return (
    <div
      data-log-line={row.seq}
      style={{
        display: 'flex',
        alignItems: 'baseline',
        gap: 8,
        padding: `0 12px 0 ${indent}px`,
        background: bg,
        color,
      }}
      title={formatLogTimeUtc(row.ts)}
    >
      <Tooltip title={formatLogTimeUtc(row.ts)} placement="topLeft">
        <span data-log-time style={{ color: '#8b949e', flexShrink: 0 }}>
          {time}
        </span>
      </Tooltip>
      {row.rowKind === 'line' && row.stream !== 'kurin' && (
        <span style={{ color: STREAM_COLOR[row.stream!], flexShrink: 0, fontSize: 10 }}>
          {row.stream === 'stdout' ? 'OUT' : 'ERR'}
        </span>
      )}
      {row.rowKind === 'terminal' && (
        <span style={{ fontWeight: 600 }}>
          {row.state === 'failed'
            ? t('logs.task.failed', { reason: row.reason ?? '' })
            : t('logs.task.ended', {
                state: String(row.state),
                ms: row.ms != null ? formatPreciseMs(row.ms) : '—',
              })}
        </span>
      )}
      {row.rowKind !== 'terminal' && (
        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>
          {parts.map((p, i) =>
            p.hit ? (
              <mark
                key={i}
                data-highlight="true"
                style={{
                  background: isCurrentMatch ? '#f2cc60' : 'rgba(242,204,96,0.35)',
                  color: '#0d1117',
                  borderRadius: 2,
                }}
              >
                {p.text}
              </mark>
            ) : (
              <span key={i} style={isMatch ? { borderBottom: '1px dotted #f2cc60' } : undefined}>
                {p.text}
              </span>
            ),
          )}
        </span>
      )}
    </div>
  );
}

/* ------------------------------ 下载序列化 ------------------------------- */

function serializeAllJsonl(events: readonly OperationEventEnvelope[]): string {
  const lines: string[] = [];
  for (const ev of events) lines.push(JSON.stringify(ev));
  return lines.length > 0 ? `${lines.join('\n')}\n` : '';
}

function serializeAllLogText(events: readonly OperationEventEnvelope[]): string {
  const lines: string[] = [];
  for (const ev of events) {
    const body = ev.payload as { type: string; line?: string; message?: string; node?: string | null; task?: string | null; level?: string; reason?: string; ms?: number; id?: string; state?: string; error?: string };
    let level = 'INFO';
    let node = '-';
    let task = '-';
    let text = '';
    switch (body.type) {
      case 'task-start':
        node = body.node ?? '-';
        task = body.id ?? '-';
        text = 'started';
        break;
      case 'task-done':
        level = 'DONE';
        node = body.node ?? '-';
        task = body.id ?? '-';
        text = `done in ${formatPreciseMs(body.ms ?? 0)}`;
        break;
      case 'task-failed':
        level = 'FAIL';
        node = body.node ?? '-';
        task = body.id ?? '-';
        text = body.reason ?? '';
        break;
      case 'task-skipped':
        level = 'SKIP';
        node = body.node ?? '-';
        task = body.id ?? '-';
        text = body.reason ?? 'skipped';
        break;
      case 'log':
        level = (body.level ?? 'info').toUpperCase();
        node = body.node ?? '-';
        task = body.task ?? '-';
        text = body.line ?? '';
        break;
      case 'note':
        level = (body.level ?? 'info').toUpperCase();
        text = body.message ?? '';
        break;
      case 'state':
        level = 'STATE';
        text = body.error ? `${body.state}: ${body.error}` : (body.state ?? '');
        break;
      default:
        text = body.line ?? body.message ?? body.type;
        break;
    }
    lines.push(`${ev.ts}  ${level.padEnd(5)}  ${node.padEnd(12)}  ${task.padEnd(34)}  ${text}`);
  }
  return lines.length > 0 ? `${lines.join('\n')}\n` : '';
}
