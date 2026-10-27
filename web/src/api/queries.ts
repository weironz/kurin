/**
 * 页面级查询（WP-61）：集群总览的查询选项集中在模块里，
 * 供页面与测试共用（15 秒轮询需要单一事实来源，可被直接断言）。
 * 真正的类型来源是 ts-rs 产物（WP-55），此处与手写草稿 DTO 对齐。
 */
import { fetchClusters } from './clusters';

/** 集群总览（UI §8.1：15 秒轮询快照）。 */
export const clustersQuery = {
  queryKey: ['clusters'],
  queryFn: () => fetchClusters(),
  refetchInterval: 15_000,
} as const;
