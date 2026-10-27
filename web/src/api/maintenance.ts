/**
 * 集群维护操作 API（WP-65，UI §8.1）：升级（含 dry-run 计划预览）与证书续期。
 *
 * 两者都是 `POST /clusters/{name}/operations` 的 202 长任务；真实服务端返回
 * 完整操作视图（含 `id`），WP 约定的回执形状是 `{operation, state, cluster}`，
 * OperationReceipt 两种都收。
 */
import { apiFetch } from './client';
import { type OperationReceipt } from './addons';

/** `kind=upgrade` 的参数（服务端 semver 校验 target，batch 下限 1）。 */
export interface UpgradeRequest {
  /** 目标版本，semver（如 `1.34.0`）。 */
  target: string;
  /** 每批升级的节点数。 */
  batch: number;
  /** 只生成计划不执行（预览）。 */
  dryRun: boolean;
  /** 节点排空失败时是否强制继续。 */
  forceDrainFailure: boolean;
}

/** `POST /clusters/{name}/operations` kind=upgrade → 202 + 操作。 */
export function upgradeCluster(name: string, request: UpgradeRequest): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/clusters/${encodeURIComponent(name)}/operations`, {
    method: 'POST',
    body: {
      kind: 'upgrade',
      params: {
        target: request.target,
        batch: request.batch,
        dry_run: request.dryRun,
        force_drain_failure: request.forceDrainFailure,
      },
    },
  });
}

/** `POST /clusters/{name}/operations` kind=certs-renew → 202 + 操作。 */
export function renewCertificates(name: string, force: boolean): Promise<OperationReceipt> {
  return apiFetch<OperationReceipt>(`/clusters/${encodeURIComponent(name)}/operations`, {
    method: 'POST',
    body: { kind: 'certs-renew', params: { force } },
  });
}
