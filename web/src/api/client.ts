/**
 * `/api/v1` fetch 封装：JSON、credentials same-origin、错误码映射（UI §7）。
 *
 * 错误码：400 语法、401 未登录、404、409（rev 冲突 / 集群已有活动操作 / 锁被
 * CLI 占用 / 重名）、422（校验失败，附诊断）、503（正在关闭）。
 */
import type { ApiErrorEnvelope, SessionInfo } from './types';

export const API_BASE = '/api/v1';

/** 带 HTTP 状态与后端诊断的客户端错误。 */
export class ApiError extends Error {
  readonly status: number;
  /** 机读错误码，如 `conflict`。 */
  readonly code: string;
  /** 422 字段级诊断 / 409 冲突原因。 */
  readonly diagnostics: NonNullable<ApiErrorEnvelope['diagnostics']>;

  constructor(status: number, envelope: Partial<ApiErrorEnvelope>) {
    super(envelope.message ?? `HTTP ${status}`);
    this.name = 'ApiError';
    this.status = status;
    this.code = envelope.code ?? defaultCode(status);
    this.diagnostics = envelope.diagnostics ?? [];
  }
  /** 401 → 未登录（触发跳转登录页）。 */
  get isUnauthorized(): boolean {
    return this.status === 401;
  }
}

function defaultCode(status: number): string {
  switch (status) {
    case 400:
      return 'invalid-request';
    case 401:
      return 'unauthorized';
    case 404:
      return 'not-found';
    case 409:
      return 'conflict';
    case 422:
      return 'validation-failed';
    case 503:
      return 'unavailable';
    default:
      return `http-${status}`;
  }
}

/** 从响应解析错误信封；body 缺失时退化为默认 code。 */
async function toApiError(res: Response): Promise<ApiError> {
  let envelope: Partial<ApiErrorEnvelope> = {};
  try {
    const body = (await res.json()) as Partial<ApiErrorEnvelope>;
    if (body && typeof body === 'object') {
      envelope = body;
    }
  } catch {
    // 非 JSON 错误体：只保留状态码语义
  }
  return new ApiError(res.status, envelope);
}

/** 断言 2xx，否则抛 ApiError。 */
async function ensureOk(res: Response): Promise<Response> {
  if (!res.ok) {
    throw await toApiError(res);
  }
  return res;
}

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  /** 变更请求一律 JSON（UI §6.10：跨站表单无法伪造）。 */
  body?: unknown;
  /** 额外查询参数。 */
  params?: Record<string, string | number | undefined>;
  signal?: AbortSignal;
}

/** `/api/v1` 下的 JSON 请求；credentials 同源（SSE 的 EventSource 同理）。 */
export async function apiFetch<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  const url = new URL(`${API_BASE}${path}`, window.location.origin);
  for (const [k, v] of Object.entries(opts.params ?? {})) {
    if (v !== undefined && v !== '') url.searchParams.set(k, String(v));
  }
  const init: RequestInit = {
    method: opts.method ?? 'GET',
    credentials: 'same-origin',
    signal: opts.signal,
    headers: {},
  };
  if (opts.body !== undefined) {
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(opts.body);
  }
  const res = await fetch(url, init);
  await ensureOk(res);
  if (res.status === 204) {
    return undefined as T;
  }
  return (await res.json()) as T;
}

/* ------------------------------ 会话 (UI §6.10) ----------------------------- */

const TOKEN_HASH_KEY = 'token';

/** 从 `#token=…` 提取登录令牌；不存在返回 null。 */
export function readTokenFromHash(): string | null {
  const hash = window.location.hash;
  if (!hash.startsWith(`#${TOKEN_HASH_KEY}=`)) return null;
  return decodeURIComponent(hash.slice(`#${TOKEN_HASH_KEY}=`.length));
}

/** 清掉地址栏 hash（保留历史记录干净，不触发路由跳转）。 */
function stripTokenHash(): void {
  // replaceState 避免 SPA 路由把 `#token=…` 当成路径
  window.history.replaceState(null, '', window.location.pathname + window.location.search);
}

/** 令牌换会话：`POST /session` → HttpOnly cookie；成功后剥离 hash。 */
export async function loginWithToken(token: string): Promise<SessionInfo> {
  const info = await apiFetch<SessionInfo>('/session', { method: 'POST', body: { token } });
  stripTokenHash();
  return info;
}

/**
 * 应用启动时的登录流程：hash 里有令牌则换取会话并清掉 hash；
 * `GET /session` 已有有效会话也返回其元数据。
 */
export async function establishSession(): Promise<SessionInfo | null> {
  const token = readTokenFromHash();
  if (token) {
    try {
      return await loginWithToken(token);
    } catch (err) {
      // 令牌无效：清掉 hash，走未登录分支
      stripTokenHash();
      if (err instanceof ApiError && err.isUnauthorized) return null;
      throw err;
    }
  }
  try {
    return await apiFetch<SessionInfo>('/session');
  } catch (err) {
    if (err instanceof ApiError && err.isUnauthorized) return null;
    throw err;
  }
}

/** 登出：`DELETE /session` 使 cookie 失效。 */
export async function logout(): Promise<void> {
  await apiFetch<void>('/session', { method: 'DELETE' });
}
