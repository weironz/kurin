/**
 * 创建向导的 API 封装（WP-62，UI §8.2）。
 *
 * 规则 1：每一步都调用 `POST /configs/render` 由 Rust 生成规范 TOML，
 * 再调用 `POST /configs/validate` 拿诊断——前端不复刻校验逻辑，
 * 这里只是把两个端点的调用约定集中起来供向导与测试共用。
 */
import { ApiError, apiFetch } from './client';
import type {
  AddonCatalogResponse,
  ApiErrorEnvelope,
  Diagnostic,
  OperationId,
  PackageListResponse,
  ParsedNodesResponse,
  PlanResult,
  PrecheckReportResult,
  ProbeRequest,
  ProbeResponse,
  RenderedConfig,
  WizardDto,
} from './types';

/* ------------------------------ render + validate ------------------------------ */

/** `POST /configs/render`：向导 DTO → 规范 TOML（不完整的配置也返回文档 + 诊断）。 */
export function renderConfig(dto: WizardDto, signal?: AbortSignal): Promise<RenderedConfig> {
  return apiFetch<RenderedConfig>('/configs/render', {
    method: 'POST',
    body: dto,
    signal,
  });
}

/** `POST /configs/validate` 的结果：通过为 true，否则带诊断。 */
export interface ValidateOutcome {
  valid: boolean;
  diagnostics: Diagnostic[];
}

/**
 * `POST /configs/validate`：对 TOML 全量校验。
 * 422 不是异常而是「校验失败 + 诊断」，向导据此渲染错误；其余错误照常抛出。
 */
export async function validateConfig(toml: string, signal?: AbortSignal): Promise<ValidateOutcome> {
  try {
    await apiFetch<{ valid: boolean }>('/configs/validate', {
      method: 'POST',
      body: { toml },
      signal,
    });
    return { valid: true, diagnostics: [] };
  } catch (error) {
    if (error instanceof ApiError && error.status === 422) {
      return { valid: false, diagnostics: error.diagnostics };
    }
    throw error;
  }
}

/** 每步一次的 render → validate 往返结果。 */
export interface RenderValidateResult {
  toml: string;
  /** render 顺带校验的结论（同一文档）。 */
  renderValid: boolean;
  validate: ValidateOutcome;
}

/**
 * UI §8.2 规则 1 的完整往返：render 生成规范 TOML，再对它做全量 validate。
 * render 自带的诊断与 validate 的诊断合并去重展示。
 */
export async function renderAndValidate(
  dto: WizardDto,
  signal?: AbortSignal,
): Promise<RenderValidateResult> {
  const rendered = await renderConfig(dto, signal);
  const validate = await validateConfig(rendered.toml, signal);
  return { toml: rendered.toml, renderValid: rendered.valid, validate };
}

/* --------------------------------- 节点 ---------------------------------- */

/** `POST /nodes/parse`：批量粘贴 / IP 范围 → 节点列表（与 CLI 同一解析器）。 */
export function parseNodes(
  input: string,
  existing: string[] = [],
  signal?: AbortSignal,
): Promise<ParsedNodesResponse> {
  return apiFetch<ParsedNodesResponse>('/nodes/parse', {
    method: 'POST',
    body: { input, existing },
    signal,
  });
}

/** `POST /nodes/probe`：单节点 discover，返回 facts 或抛 4xx（原因在 message）。 */
export function probeNode(req: ProbeRequest, signal?: AbortSignal): Promise<ProbeResponse> {
  return apiFetch<ProbeResponse>('/nodes/probe', { method: 'POST', body: req, signal });
}

/* -------------------------------- 目录与包 -------------------------------- */

/** `GET /addons/catalog`：可用 addon + values JSON Schema。 */
export function fetchAddonCatalog(signal?: AbortSignal): Promise<AddonCatalogResponse> {
  return apiFetch<AddonCatalogResponse>('/addons/catalog', { signal });
}

/** `GET /packages`：已登记的包源。 */
export function fetchPackages(signal?: AbortSignal): Promise<PackageListResponse> {
  return apiFetch<PackageListResponse>('/packages', { signal });
}

/* ---------------------------- 登记、检查与提交 ---------------------------- */

/** `POST /clusters`（登记：只保存，不安装）在集群已有配置时返回 409。 */
export function isConflict(error: unknown): error is ApiError {
  return error instanceof ApiError && error.status === 409;
}

/** 向导检查页的前置：把当前配置登记成集群草稿（已有则更新修订）。 */
export async function ensureClusterRegistered(
  name: string,
  toml: string,
): Promise<{ rev: number }> {
  try {
    const created = await apiFetch<{ rev: number }>('/clusters', {
      method: 'POST',
      body: { name, toml },
    });
    return { rev: created.rev };
  } catch (error) {
    if (!isConflict(error)) throw error;
    // 已有配置：读取当前修订，内容一致则跳过，否则追加新修订（UI §6.6 base_rev）。
    const current = await apiFetch<{ rev: number; toml: string }>(`/clusters/${name}/config`);
    if (current.toml === toml) return { rev: current.rev };
    const saved = await apiFetch<{ rev: number }>(`/clusters/${name}/config`, {
      method: 'PUT',
      body: { toml, base_rev: current.rev },
    });
    return { rev: saved.rev };
  }
}

/** `POST /clusters/{name}/operations` → 202 + 操作（长任务）。 */
export function submitOperation(
  name: string,
  kind: string,
  params: Record<string, unknown> = {},
): Promise<{ id: OperationId }> {
  return apiFetch<{ id: OperationId }>(`/clusters/${name}/operations`, {
    method: 'POST',
    body: { kind, params },
  });
}

/** `POST /clusters/{name}/plan` → 202 + 操作 id（只读 plan 操作）。 */
export function submitPlan(name: string, check = false): Promise<{ operation: OperationId }> {
  return apiFetch<{ operation: OperationId }>(`/clusters/${name}/plan`, {
    method: 'POST',
    params: { check: check ? 'true' : undefined },
  });
}

/** `GET /operations/{id}` 的最小轮询视图（state / result / error 无大小写差异）。 */
export interface OperationOutcome {
  id: OperationId;
  state: string;
  result?: PrecheckReportResult | PlanResult | null;
  error?: string | null;
}

export function fetchOperationOutcome(id: OperationId): Promise<OperationOutcome> {
  return apiFetch<OperationOutcome>(`/operations/${id}`);
}

/** 阻塞等待操作进入终态（轮询），返回最终快照；abort 由调用方处理。 */
export async function waitForOperation(
  id: OperationId,
  intervalMs = 800,
  maxAttempts = 150,
): Promise<OperationOutcome> {
  let last: OperationOutcome | null = null;
  for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
    if (attempt > 0) {
      // TS 5.6 的 lib 无 Promise.withResolvers；setTimeout 的 Promise 定时器。
      await new Promise((resolve) => {
        setTimeout(resolve, intervalMs);
      });
    }
    last = await fetchOperationOutcome(id);
    if (TERMINAL_STATES[last.state] === true) return last;
  }
  return last ?? { id, state: 'running' };
}

/** 操作终态表（queued/running 之外）；静态键集用属性查询。 */
export const TERMINAL_STATES: Record<string, true> = {
  succeeded: true,
  failed: true,
  cancelled: true,
  interrupted: true,
};

/** 提取 422 校验诊断的便捷读取（测试与页面共用）。 */
export function diagnosticsOf(error: unknown): Diagnostic[] {
  if (error instanceof ApiError) return error.diagnostics;
  const envelope = error as Partial<ApiErrorEnvelope> | null;
  return envelope?.diagnostics ?? [];
}
