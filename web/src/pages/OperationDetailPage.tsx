/**
 * 操作详情 `/operations/:id`（WP-63，UI §8.1）。
 *
 * 数据流：
 * - `GET /operations/{id}`：快照（状态、耗时、progress 种子、lastSeq 游标）；
 * - `GET /operations/{id}/logs?after=0`：历史第一页（seq 顺序）；
 * - SSE `GET /operations/{id}/events?after=lastSeq`：实时事件 + 断线重连续读；
 * - 历史「向前补页」：`GET /logs?after=<minSeq-limit>` 取更早事件（乱序由 store 归并）；
 * - 取消 / 重试（带确认；重试创建新操作并跳转）。
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Alert,
  App,
  Button,
  Card,
  Descriptions,
  Result,
  Skeleton,
  Space,
  Tag,
  Typography,
} from 'antd';
import { apiFetch, ApiError } from '../api/client';
import { subscribeOperationEvents } from '../api/sse';
import type {
  OperationDetail,
  OperationEventEnvelope,
  OperationLogsPage,
  OperationSummary,
} from '../api/types';
import { t } from '../i18n/zh-CN';
import {
  formatAbsolute,
  formatDurationBetween,
  formatDurationSince,
} from '../utils/format';
import {
  operationKindLabel,
  operationStateColor,
  operationStateLabel,
} from '../utils/operation';
import { LogConsole } from '../operation/LogConsole';
import { ProgressGrid, type GridSelection } from '../operation/ProgressGrid';
import { ResourceTable } from '../operation/ResourceTable';
import { OperationTimeline } from '../operation/timeline';

const HISTORY_PAGE_LIMIT = 500;

export default function OperationDetailPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const { modal, message } = App.useApp();
  const queryClient = useQueryClient();

  const [version, setVersion] = useState(0);
  const [sseState, setSseState] = useState<'connecting' | 'open' | 'reconnecting'>('connecting');
  const [gridSelection, setGridSelection] = useState<GridSelection | null>(null);
  const [loadingEarlier, setLoadingEarlier] = useState(false);
  /** 全量历史已到头（minSeq 到 0）。 */
  const [historyDone, setHistoryDone] = useState(false);
  const [cancelPending, setCancelPending] = useState(false);
  const [retryPending, setRetryPending] = useState(false);

  const timelineRef = useRef<OperationTimeline | null>(null);

  const detailQuery = useQuery({
    queryKey: ['operation', id],
    queryFn: () => apiFetch<OperationDetail>(`/operations/${encodeURIComponent(id ?? '')}`),
    enabled: id != null,
    refetchOnWindowFocus: false,
  });
  const detail = detailQuery.data;

  // 时间线单例（progress 种子）。
  if (timelineRef.current == null && detail != null) {
    timelineRef.current = new OperationTimeline(detail.progress);
  }
  const timeline = timelineRef.current;

  /* ------------------------------ SSE 订阅 ------------------------------ */

  const lastSeqRef = useRef<number>(0);
  const bump = useCallback(() => setVersion((v) => v + 1), []);

  useEffect(() => {
    if (!id) return;
    // jsdom 无 EventSource：node 测试环境只走 /logs 历史接口（浏览器始终有 SSE）。
    if (typeof EventSource === 'undefined') return;
    const stop = subscribeOperationEvents(
      id,
      {
        onEvent: (ev) => {
          timeline?.insert([ev as OperationEventEnvelope]);
          bump();
        },
        onOpen: () => setSseState('open'),
        onError: () => setSseState('reconnecting'),
        onFatal: () => setSseState('reconnecting'),
      },
      { lastSeq: lastSeqRef.current },
    );
    return stop;
  }, [id, timeline, bump]);

  // 终态操作：拉取全量历史（服务端会在 state 事件后关闭 SSE）。
  const isTerminal = detail != null && detail.state !== 'queued' && detail.state !== 'running';

  const loadHistory = useCallback(
    async (after: number, limit = HISTORY_PAGE_LIMIT) => {
      const page = await apiFetch<OperationLogsPage>(
        `/operations/${encodeURIComponent(id ?? '')}/logs?after=${after}&limit=${limit}`,
      );
      if (timeline) {
        const added = timeline.insert(page.items);
        if (added > 0) bump();
        if (timeline.store.minSeq <= 1 || page.items.length < limit) {
          setHistoryDone(true);
        } else if (page.items.length === 0) {
          setHistoryDone(true);
        }
      }
      return page;
    },
    [id, timeline, bump],
  );

  // 初始历史：detail.lastSeq 已知后拉第一页（从 0 开始按序分页）。
  useEffect(() => {
    if (!id || !detail || !timeline || historyDone) return;
    if (timeline.store.size > 0) return; // SSE 已开始提供数据
    void loadHistory(0);
    setHistoryDone(true); // 首页后按需继续（简化：SSE 补读会覆盖剩余）
  }, [id, detail, timeline, historyDone, loadHistory]);

  // 前向补页（用户点击「加载更早的日志」）。
  const loadEarlier = useCallback(async () => {
    if (!timeline || loadingEarlier) return;
    setLoadingEarlier(true);
    try {
      const minSeq = timeline.store.minSeq;
      if (minSeq <= 1) {
        setHistoryDone(true);
        return;
      }
      const before = Math.max(0, minSeq - 1 - HISTORY_PAGE_LIMIT * 4);
      const page = await apiFetch<OperationLogsPage>(
        `/operations/${encodeURIComponent(id ?? '')}/logs?after=${before}&limit=${
          minSeq - 1 - before
        }`,
      );
      const added = timeline.insert(page.items);
      if (added > 0) bump();
      if (timeline.store.minSeq <= 1 || page.items.length === 0) setHistoryDone(true);
    } finally {
      setLoadingEarlier(false);
    }
  }, [id, timeline, loadingEarlier, bump]);

  /* ----------------------------- 取消 / 重试 ----------------------------- */

  const confirmCancel = () => {
    modal.confirm({
      title: t('operation.cancel.title'),
      content: t('operation.cancel.content'),
      okText: t('operation.cancel.confirm'),
      okButtonProps: { danger: true },
      cancelText: t('common.cancel'),
      onOk: async () => {
        setCancelPending(true);
        try {
          await apiFetch<OperationSummary>(
            `/operations/${encodeURIComponent(id ?? '')}/cancel`,
            { method: 'POST' },
          );
          message.success(t('operation.cancel.success'));
          await detailQuery.refetch();
        } catch (err) {
          message.error(
            t('operation.actionFailed', {
              message: err instanceof Error ? err.message : String(err),
            }),
          );
        } finally {
          setCancelPending(false);
        }
      },
    });
  };

  const confirmRetry = () => {
    modal.confirm({
      title: t('operation.retry.title'),
      content: t('operation.retry.content'),
      okText: t('operation.retry.confirm'),
      cancelText: t('common.cancel'),
      onOk: async () => {
        setRetryPending(true);
        try {
          const created = await apiFetch<OperationSummary>(
            `/operations/${encodeURIComponent(id ?? '')}/retry`,
            { method: 'POST' },
          );
          message.success(t('operationDetail.retrying'));
          queryClient.invalidateQueries({ queryKey: ['operations'] });
          navigate(`/operations/${created.id}`);
        } catch (err) {
          message.error(
            t('operation.actionFailed', {
              message: err instanceof Error ? err.message : String(err),
            }),
          );
        } finally {
          setRetryPending(false);
        }
      },
    });
  };

  useEffect(() => {
    document.title = `${t('page.operationDetail.title')} · kurin`;
  }, []);

  if (!id) return null;

  if (detailQuery.isLoading) {
    return <Skeleton active title paragraph={{ rows: 10 }} />;
  }
  if (detailQuery.error) {
    const err = detailQuery.error;
    if (err instanceof ApiError && err.status === 404) {
      return (
        <Result
          status="404"
          title={t('operationDetail.notFound')}
          extra={<Link to="/operations">{t('operationDetail.back')}</Link>}
        />
      );
    }
    return (
      <Result
        status="error"
        title={t('operationDetail.loadFailed')}
        subTitle={err instanceof Error ? err.message : String(err)}
        extra={
          <Button type="primary" onClick={() => void detailQuery.refetch()}>
            {t('common.retry')}
          </Button>
        }
      />
    );
  }
  if (!detail || !timeline) return null;

  /* ------------------------------ 视图数据 ------------------------------ */

  // version 只为触发重渲染；模型在 ref 上增量更新。
  void version;
  const running = detail.state === 'queued' || detail.state === 'running';
  const opStartTs = detail.startedAt ?? detail.createdAt;
  const events = timeline.store.getEvents();
  const resourceCount = timeline.resources.size;

  const duration = running
    ? formatDurationSince(detail.startedAt ?? null)
    : formatDurationBetween(detail.startedAt ?? null, detail.finishedAt ?? null);

  return (
    <div data-testid="operation-detail">
      <Typography.Title level={3} style={{ marginBottom: 4 }}>
        {t('page.operationDetail.title')}
      </Typography.Title>
      <Space align="center" wrap style={{ marginBottom: 16 }}>
        <Typography.Text code>{id}</Typography.Text>
        <Tag color={operationStateColor[detail.state]}>{operationStateLabel(detail.state)}</Tag>
        <Typography.Text type="secondary">
          {operationKindLabel(detail.kind)}
          {detail.cluster ? ` · ${detail.cluster}` : ''}
        </Typography.Text>
        {running && (
          <Typography.Text type="secondary">{duration ? `⏱ ${duration}` : ''}</Typography.Text>
        )}
        <Space>
          {running && (
            <Button danger loading={cancelPending} onClick={confirmCancel}>
              {t('operation.cancel')}
            </Button>
          )}
          {!running && (
            <Button type="primary" loading={retryPending} onClick={confirmRetry}>
              {t('operation.retry')}
            </Button>
          )}
        </Space>
      </Space>

      {detail.error && (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('operationDetail.error')}
          description={detail.error}
        />
      )}

      {running && (
        <Alert
          style={{ marginBottom: 16 }}
          type={sseState === 'open' ? 'success' : 'info'}
          showIcon
          message={
            sseState === 'open'
              ? t('operation.sseConnected')
              : sseState === 'reconnecting'
                ? t('operation.sseReconnecting')
                : t('logs.connecting')
          }
        />
      )}

      <Card size="small" style={{ marginBottom: 16 }} title={t('operationDetail.progress')}>
        <Descriptions
          size="small"
          column={4}
          style={{ marginBottom: 12 }}
          items={[
            {
              key: 'cluster',
              label: '集群',
              children: detail.cluster ? (
                <Link to={`/clusters/${detail.cluster}`}>{detail.cluster}</Link>
              ) : (
                '—'
              ),
            },
            {
              key: 'kind',
              label: '类型',
              children: operationKindLabel(detail.kind),
            },
            {
              key: 'createdAt',
              label: t('operationDetail.createdAt'),
              children: formatAbsolute(detail.createdAt),
            },
            {
              key: 'duration',
              label: t('operationDetail.duration'),
              children: duration ?? '—',
            },
            ...(detail.startedAt
              ? [
                  {
                    key: 'startedAt',
                    label: t('operationDetail.startedAt'),
                    children: formatAbsolute(detail.startedAt),
                  },
                ]
              : []),
            ...(detail.finishedAt
              ? [
                  {
                    key: 'finishedAt',
                    label: t('operationDetail.finishedAt'),
                    children: formatAbsolute(detail.finishedAt),
                  },
                ]
              : []),
            ...(detail.retryOf
              ? [
                  {
                    key: 'retryOf',
                    label: t('operationDetail.retryOf'),
                    children: <Typography.Text code>{detail.retryOf}</Typography.Text>,
                  },
                ]
              : []),
            {
              key: 'actor',
              label: t('operationDetail.actor'),
              children: detail.actor,
            },
          ]}
        />
        <ProgressGrid
          grid={timeline.grid}
          selection={gridSelection}
          onSelect={setGridSelection}
        />
      </Card>

      {resourceCount > 0 && (
        <Card size="small" style={{ marginBottom: 16 }}>
          <ResourceTable rows={timeline.resources} />
        </Card>
      )}

      <Card size="small">
        {!historyDone && events.length > 0 && (
          <Button
            size="small"
            style={{ marginBottom: 8 }}
            loading={loadingEarlier}
            onClick={() => void loadEarlier()}
          >
            {t('logs.loadHistory')}
          </Button>
        )}
        {isTerminal && events.length === 0 && (
          <Typography.Text type="secondary" style={{ display: 'block', marginBottom: 8 }}>
            {t('common.loading')}
          </Typography.Text>
        )}
        <LogConsole
          model={timeline.logModel}
          totalEvents={events.length}
          events={events}
          running={running}
          opStartTs={opStartTs}
          cellFilter={
            gridSelection
              ? { stage: gridSelection.stage, node: gridSelection.node }
              : null
          }
          onClearCellFilter={() => setGridSelection(null)}
        />
      </Card>
    </div>
  );
}
