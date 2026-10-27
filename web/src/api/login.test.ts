import { describe, expect, it } from 'vitest';
import { readTokenFromHash, loginWithToken, establishSession, logout, ApiError } from './client';

/** 用 instanceof 判定并收窄（避免 cast 读取成员）。 */
async function errOrNull(p: Promise<unknown>): Promise<ApiError | null> {
  const err = await p.catch((e: unknown) => e);
  return err instanceof ApiError ? err : null;
}

describe('hash 令牌登录流程（UI §6.10）', () => {
  it('readTokenFromHash 提取 #token=… 值（含 URL 编码）', () => {
    window.history.replaceState(null, '', '/#token=abc123');
    expect(readTokenFromHash()).toBe('abc123');
    window.history.replaceState(null, '', `/#token=${encodeURIComponent('a/b+c=')}`);
    expect(readTokenFromHash()).toBe('a/b+c=');
    window.history.replaceState(null, '', '/');
    expect(readTokenFromHash()).toBeNull();
    window.history.replaceState(null, '', '/#/clusters');
    expect(readTokenFromHash()).toBeNull();
  });

  it('loginWithToken 成功后剥离 hash 并返回会话信息', async () => {
    window.history.replaceState(null, '', '/#token=valid-token');
    const info = await loginWithToken('valid-token');
    expect(info.version).toBe('0.4.0');
    expect(window.location.hash).toBe('');
    expect(window.location.pathname).toBe('/');
  });

  it('无效令牌：401 → establishSession 返回 null 且清掉 hash', async () => {
    window.history.replaceState(null, '', '/#token=wrong-token');
    const session = await establishSession();
    expect(session).toBeNull();
    expect(window.location.hash).toBe('');
  });

  it('无 hash 且无会话：establishSession 返回 null', async () => {
    const session = await establishSession();
    expect(session).toBeNull();
  });

  it('无 hash 但已有会话：establishSession 返回会话元数据', async () => {
    await loginWithToken('valid-token');
    const session = await establishSession();
    expect(session?.version).toBe('0.4.0');
  });

  it('登出使会话失效', async () => {
    await loginWithToken('valid-token');
    await logout();
    const session = await establishSession();
    expect(session).toBeNull();
  });

  it('loginWithToken 直接传错令牌抛 401 ApiError', async () => {
    const err = await errOrNull(loginWithToken('wrong-token'));
    expect(err?.status).toBe(401);
    expect(err?.isUnauthorized).toBe(true);
  });
});
