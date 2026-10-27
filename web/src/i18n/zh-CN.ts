/**
 * 文案字典（UI §8.4）：键名英文，值中文；首版只有 zh-CN，`en` 在 M3 补（WP-78）。
 * `t()` 是最简单的查表 + 插值，不做 ICU 复数等。
 */

const zhCN = {
  // 通用
  'app.title': 'kurin 控制台',
  'common.loading': '加载中…',
  'common.empty': '暂无数据',
  'common.error': '请求失败',
  'common.retry': '重试',
  'common.logout': '退出登录',
  'common.cancel': '取消',

  // 登录
  'login.title': '登录 kurin',
  'login.description':
    '请使用启动命令打印的访问链接登录（链接形如 http://127.0.0.1:7080/#token=…）。',
  'login.tokenLabel': '访问令牌',
  'login.submit': '登录',
  'login.failed': '登录失败：令牌无效或已过期',
  'login.generating': '正在换取会话…',

  // 导航（侧边栏：集群 / 操作 / 包 / 关于 —— WP-60）
  'nav.clusters': '集群',
  'nav.operations': '操作',
  'nav.packages': '包',
  'nav.about': '关于',

  // 页面
  'page.clusters.title': '集群',
  'page.clusters.new': '新建集群',
  'page.clusters.description': '多集群总览（WP-61 实现完整表格）',
  'page.clusterDetail.title': '集群详情',
  'page.clusterDetail.description': '概览 / 节点 / 配置 / 操作历史（WP-61）',
  'page.operations.title': '操作',
  'page.operations.description': '按集群、类型、状态筛选（WP-61）',
  'page.operationDetail.title': '操作详情',
  'page.operationDetail.description': '进度网格与日志控制台（WP-63）',
  'page.packages.title': '包',
  'page.packages.description': '管理机本地缓存与已登记的包源（WP-65）',
  'page.about.title': '关于',
  'page.about.description': '版本、组件版本表、能力表、数据目录',

  // 健康徽标（五态）
  'cluster.health.healthy': '健康',
  'cluster.health.degraded': '异常',
  'cluster.health.unreachable': '不可达',
  'cluster.health.stale': '数据过期',
  'cluster.health.unknown': '无快照',

  // 错误码
  'error.invalid-request': '请求语法错误',
  'error.unauthorized': '未登录',
  'error.not-found': '资源不存在',
  'error.conflict': '状态冲突（修订不一致 / 已有活动操作 / 锁被占用）',
  'error.validation-failed': '校验失败',
  'error.unavailable': '服务正在关闭',
} as const;

export type Dictionary = typeof zhCN;
export type MessageKey = keyof Dictionary;

/** 查表 + `{var}` 插值。 */
export function t(key: MessageKey, vars?: Record<string, string | number>): string {
  let text: string = zhCN[key] ?? key;
  if (vars) {
    for (const [k, v] of Object.entries(vars)) {
      text = text.replaceAll(`{${k}}`, String(v));
    }
  }
  return text;
}
