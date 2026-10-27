/**
 * 包管理 API（WP-65，UI §8.1）：登记 / 删除 / 校验 / 推送 / 制作 base。
 *
 * 202 端点的回执是 `{operation, state, cluster}`（服务端 OperationSubmitted）；
 * 复用 addons.ts 的 OperationReceipt 取操作 id（两种端点形状取一）。
 */
import { apiFetch } from './client';
import { type OperationReceipt } from './addons';
import type { PackageInfo } from './types';

/** `GET /packages`：已登记的包源（向导离线步骤与包管理页共用）。 */
export { fetchPackages } from './wizard';

/** `POST /packages` body：管理机上的包来源 URI。 */
export interface ImportPackageRequest {
  source: string;
}

/** `POST /packages`：打开包源并登记清单（不复制载荷）→ 201 + 登记行。 */
export function importPackage(request: ImportPackageRequest): Promise<PackageInfo> {
  return apiFetch<PackageInfo>('/packages', { method: 'POST', body: request });
}

/** `DELETE /packages/{id}`：移除登记（磁盘载荷不动）→ 204。 */
export async function removePackage(id: string): Promise<void> {
  await apiFetch<void>(`/packages/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/** `POST /packages/{id}/verify`：后台重算摘要闭包 → 202 + 操作。 */
export function verifyPackage(id: string): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/packages/${encodeURIComponent(id)}/verify`, {
    method: 'POST',
  });
}

/** `POST /packages/{id}/push` body（凭据只有引用：环境变量名）。 */
export interface PushPackageRequest {
  destination: string;
  user?: string;
  password_env?: string;
}

/** `POST /packages/{id}/push`：复制包到 `oci://` 注册表（只补缺失 blob）→ 202。 */
export function pushPackage(id: string, request: PushPackageRequest): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/packages/${encodeURIComponent(id)}/push`, {
    method: 'POST',
    body: request,
  });
}

/** `POST /packages/create-base` body（可选字段缺省走服务端默认）。 */
export interface CreateBaseRequest {
  kubernetes?: string;
  distribution?: 'kubeadm' | 'k3s';
  arch?: string;
  cni?: string;
  /** 输出目标：`oci-layout://DIR` 或 `file://X.tar[.zst]`。 */
  output: string;
}

/** `POST /packages/create-base`：后台制作 base 包 → 202 + 操作。 */
export function createBasePackage(request: CreateBaseRequest): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>('/packages/create-base', { method: 'POST', body: request });
}
