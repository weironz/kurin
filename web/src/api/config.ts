/**
 * 集群配置修订 API（WP-65，UI §8.1 配置 Tab 的高级 TOML 编辑器）。
 *
 * GET 返回当前修订（秘密为哨兵值，不回显明文）；PUT 以 `base_rev` 做
 * 乐观并发控制，过期修订服务端回 409。
 */
import { apiFetch } from './client';

/** `GET /clusters/{name}/config`：当前修订的 TOML 与指纹。 */
export interface ClusterConfigView {
  rev: number;
  toml: string;
  sha256: string;
}

/** `GET /clusters/{name}/config`（409：集群还没有配置）。 */
export function fetchClusterConfig(
  name: string,
  signal?: AbortSignal,
): Promise<ClusterConfigView> {
  return apiFetch<ClusterConfigView>(`/clusters/${encodeURIComponent(name)}/config`, { signal });
}

/** `PUT /clusters/{name}/config` body：编辑后的 TOML + 读取时的基准修订。 */
export interface SaveConfigRequest {
  toml: string;
  baseRev: number;
}

/** `PUT /clusters/{name}/config`：追加修订 → 新 rev 与指纹；base_rev 过期 409。 */
export function saveClusterConfig(
  name: string,
  request: SaveConfigRequest,
): Promise<{ rev: number; sha256: string }> {
  return apiFetch<{ rev: number; sha256: string }>(
    `/clusters/${encodeURIComponent(name)}/config`,
    { method: 'PUT', body: { toml: request.toml, base_rev: request.baseRev } },
  );
}
