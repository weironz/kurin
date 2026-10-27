/**
 * 时间与耗时格式化（WP-61）：总览「最近观测」与操作历史「耗时」列共用。
 * dayjs 已在 App 中设为 zh-cn locale；测试环境为默认 locale，不影响断言。
 */
import dayjs from 'dayjs';
import relativeTime from 'dayjs/plugin/relativeTime';
import utc from 'dayjs/plugin/utc';

dayjs.extend(relativeTime);
dayjs.extend(utc);

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

/** 字节数 → 人类可读（如「1.1 GB」）；未知/负数显示「—」。 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/* ---------------------------- 日志控制台（WP-63） ---------------------------- */

/** 日志时间戳模式（UI §8.5）：本地绝对 / 相对操作开始。 */
export type LogTimeMode = 'absolute' | 'relative';

/** 绝对时间戳（本地时区，毫秒精度）：`10:21:03.412`。 */
export function formatLogTimeAbsolute(ts: string): string {
  return dayjs(ts).format('HH:mm:ss.SSS');
}

/** 相对操作开始的偏移：`+01:42.381`（超过 1 小时为 `+1:01:02.381`）。 */
export function formatLogTimeRelative(ts: string, startTs?: string | null): string {
  const start = startTs ? new Date(startTs).getTime() : 0;
  let ms = new Date(ts).getTime() - start;
  if (!Number.isFinite(ms) || ms < 0) ms = 0;
  const totalMs = Math.floor(ms);
  const msPart = totalMs % 1000;
  const totalSec = Math.floor(totalMs / 1000);
  const sec = totalSec % 60;
  const min = Math.floor(totalSec / 60) % 60;
  const hr = Math.floor(totalSec / 3600);
  const pad = (n: number, w = 2) => String(n).padStart(w, '0');
  if (hr > 0) return `+${hr}:${pad(min)}:${pad(sec)}.${pad(msPart, 3)}`;
  return `+${pad(min)}:${pad(sec)}.${pad(msPart, 3)}`;
}

/** UTC 原值（悬停提示）：`2026-10-27T08:21:03.412Z`。 */
export function formatLogTimeUtc(ts: string): string {
  const ms = new Date(ts).getTime();
  if (!Number.isFinite(ms)) return ts;
  return dayjs(ms).utc().format('YYYY-MM-DDTHH:mm:ss.SSS[Z]');
}

/** 运行中任务的已耗时（秒级刷新即可）：`1m 42s`。 */
export function formatElapsedSince(startTs: string | undefined, now: number): string {
  if (!startTs) return '';
  return formatDurationMs(now - new Date(startTs).getTime());
}

export { formatPreciseMs } from '../operation/logmodel';
