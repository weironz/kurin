/**
 * 时间与耗时格式化（WP-61）：总览「最近观测」与操作历史「耗时」列共用。
 * dayjs 已在 App 中设为 zh-cn locale；测试环境为默认 locale，不影响断言。
 */
import dayjs from 'dayjs';
import relativeTime from 'dayjs/plugin/relativeTime';

dayjs.extend(relativeTime);

/** 相对时间（如「3 分钟前」），配合 Tooltip 的绝对时间使用。 */
export function formatRelative(ts: string): string {
  return dayjs(ts).fromNow();
}

/** 绝对时间（本地时区，秒级），用于 Tooltip 与操作历史的开始时间列。 */
export function formatAbsolute(ts: string): string {
  return dayjs(ts).format('YYYY-MM-DD HH:mm:ss');
}

/** 两个 ISO 时间点的耗时；任一缺失返回 null（调用方渲染「—」）。 */
export function formatDurationBetween(start?: string | null, end?: string | null): string | null {
  if (!start || !end) return null;
  return formatDurationMs(new Date(end).getTime() - new Date(start).getTime());
}

/** 从 start 到 now 的已耗时；start 缺失返回 null。 */
export function formatDurationSince(
  start?: string | null,
  now: number = Date.now(),
): string | null {
  if (!start) return null;
  return formatDurationMs(now - new Date(start).getTime());
}

/** 毫秒 → 「45s / 1m 23s / 2h 03m」形式的紧凑耗时。 */
function formatDurationMs(ms: number): string {
  if (ms < 0) ms = 0;
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}
