import { describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { server } from '../mocks/server';
import { apiFetch, ApiError } from './client';

/** 断言 Promise 拒绝为 ApiError 并返回它（instanceof 收窄，无需 cast）。 */
async function rejectsWithApiError(p: Promise<unknown>): Promise<ApiError> {
  const err = await p.catch((e: unknown) => e);
  if (!(err instanceof ApiError)) {
    throw new Error(`expected ApiError, got: ${String(err)}`);
  }
  return err;
}

describe('apiFetch 错误码映射（UI §7）', () => {
  it('401 映射为 unauthorized', async () => {
    server.use(
      http.get('/api/v1/whoami', () =>
        HttpResponse.json({ code: 'unauthorized', message: '未登录' }, { status: 401 }),
      ),
    );
    const err = await rejectsWithApiError(apiFetch('/whoami'));
    expect(err.status).toBe(401);
    expect(err.code).toBe('unauthorized');
    expect(err.isUnauthorized).toBe(true);
  });

  it('409 映射为 conflict 且携带冲突原因', async () => {
    server.use(
      http.post('/api/v1/clusters/tier1/operations', () =>
        HttpResponse.json(
          {
            code: 'conflict',
            message: '集群已有活动操作',
            diagnostics: [{ message: 'operation 01JD… is running' }],
          },
          { status: 409 },
        ),
      ),
    );
    const err = await rejectsWithApiError(
      apiFetch('/clusters/tier1/operations', { method: 'POST', body: { kind: 'precheck' } }),
    );
    expect(err.status).toBe(409);
    expect(err.code).toBe('conflict');
    expect(err.diagnostics).toHaveLength(1);
    expect(err.diagnostics[0]?.message).toContain('running');
  });

  it('422 映射为 validation-failed 且携带诊断 span', async () => {
    server.use(
      http.post('/api/v1/configs/validate', () =>
        HttpResponse.json(
          {
            code: 'validation-failed',
            message: '校验失败',
            diagnostics: [
              { message: 'VIP 不在节点地址内', labels: [{ offset: 42, len: 13, text: 'vip' }] },
            ],
          },
          { status: 422 },
        ),
      ),
    );
    const err = await rejectsWithApiError(
      apiFetch('/configs/validate', { method: 'POST', body: { toml: '' } }),
    );
    expect(err.status).toBe(422);
    expect(err.code).toBe('validation-failed');
    expect(err.diagnostics[0]?.labels?.[0]?.offset).toBe(42);
  });

  it('非 JSON 错误体退化为默认 code', async () => {
    server.use(
      http.get(
        '/api/v1/broken',
        () => new HttpResponse('<html>bad gateway</html>', { status: 502 }),
      ),
    );
    const err = await rejectsWithApiError(apiFetch('/broken'));
    expect(err.status).toBe(502);
    expect(err.code).toBe('http-502');
  });

  it('成功请求带 JSON Content-Type 并解析 body', async () => {
    let capturedContentType = '';
    server.use(
      http.post('/api/v1/echo', ({ request }) => {
        capturedContentType = request.headers.get('content-type') ?? '';
        return HttpResponse.json({ ok: true });
      }),
    );
    const res = await apiFetch<{ ok: boolean }>('/echo', { method: 'POST', body: { a: 1 } });
    expect(res.ok).toBe(true);
    expect(capturedContentType).toBe('application/json');
  });

  it('查询参数拼接（分页 / 筛选，undefined 参数被忽略）', async () => {
    let capturedUrl = '';
    server.use(
      http.get('/api/v1/operations', ({ request }) => {
        capturedUrl = request.url;
        return HttpResponse.json({ items: [], total: 0, page: 1, pageSize: 50 });
      }),
    );
    await apiFetch('/operations', {
      params: { cluster: 'tier1', state: 'running', kind: undefined },
    });
    const u = new URL(capturedUrl);
    expect(u.searchParams.get('cluster')).toBe('tier1');
    expect(u.searchParams.get('state')).toBe('running');
    expect(u.searchParams.has('kind')).toBe(false);
  });
});
