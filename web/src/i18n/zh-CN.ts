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
  'page.clusters.newDescription': '9 步表单向导（WP-62 实现）',
  'page.clusters.description': '多集群总览，15 秒自动刷新',
  'page.clusterDetail.title': '集群详情',
  'page.clusterDetail.description': '概览 / 节点 / 配置 / 操作历史',
  'page.operations.title': '操作',
  'page.operations.description': '按集群、类型、状态筛选',
  'page.operationDetail.title': '操作详情',
  'page.operationDetail.description': '进度网格与日志控制台（WP-63）',
  'page.packages.title': '包',
  'page.packages.description': '管理机本地缓存与已登记的包源（WP-65）',
  'page.about.title': '关于',
  'page.about.description': '版本、组件版本表、能力表、数据目录',

  // 集群总览（WP-61）
  'clusters.column.name': '名称',
  'clusters.column.distribution': '发行版',
  'clusters.column.k8sVersion': 'K8s 版本',
  'clusters.column.nodes': '节点 Ready',
  'clusters.column.health': '健康',
  'clusters.column.certMinDays': '证书最早到期',
  'clusters.column.observedAt': '最近观测',
  'clusters.column.operation': '当前操作',
  'clusters.certDays': '{n} 天',
  'clusters.empty.title': '还没有集群',
  'clusters.empty.description': '通过导入 TOML 或创建向导登记第一个集群',
  'clusters.empty.create': '新建集群',
  'clusters.error.title': '集群列表加载失败',

  // 集群详情（WP-61）
  'clusterDetail.tab.overview': '概览',
  'clusterDetail.tab.nodes': '节点',
  'clusterDetail.tab.config': '配置',
  'clusterDetail.tab.operations': '操作历史',
  'clusterDetail.notFound': '集群不存在',
  'clusterDetail.backToList': '返回集群列表',
  'clusterDetail.loadFailed': '集群详情加载失败',
  'clusterDetail.activeOperation': '当前操作',
  'clusterDetail.collectError': '状态采集失败',
  'clusterDetail.noSnapshot': '暂无快照',
  'clusterDetail.overview.observedAt': '最近观测',
  'clusterDetail.overview.nodesReady': '节点 Ready',
  'clusterDetail.overview.podsNotRunning': '未运行 Pod',
  'clusterDetail.overview.etcd': 'etcd 状态',
  'clusterDetail.overview.certMinDays': '证书最早到期',
  'clusterDetail.overview.nodeTable': '节点（观测）',
  'clusterDetail.overview.noNodes': '暂无观测节点',
  'clusterDetail.nodes.column.name': '名称',
  'clusterDetail.nodes.column.status': '状态',
  'clusterDetail.nodes.column.role': '角色',
  'clusterDetail.nodes.column.address': '地址',
  'clusterDetail.nodes.column.arch': '架构',
  'clusterDetail.nodes.column.os': '操作系统',
  'clusterDetail.nodes.column.kubeletVersion': 'kubelet 版本',
  'clusterDetail.nodes.column.ready': '就绪',
  'clusterDetail.nodes.missing': '期望缺失',
  'clusterDetail.nodes.undeclared': '未声明',
  'clusterDetail.nodes.empty': '暂无节点',
  'clusterDetail.config.summaryTitle': '配置摘要',
  'clusterDetail.config.revision': '配置修订',
  'clusterDetail.config.field.distribution': '发行版',
  'clusterDetail.config.field.kubernetesVersion': 'Kubernetes 版本',
  'clusterDetail.config.field.runtime': '容器运行时',
  'clusterDetail.config.field.zone': '镜像区域',
  'clusterDetail.config.field.cni': 'CNI',
  'clusterDetail.config.field.podCidr': 'Pod CIDR',
  'clusterDetail.config.field.serviceCidr': 'Service CIDR',
  'clusterDetail.config.field.haMode': '高可用模式',
  'clusterDetail.config.field.vip': '虚拟 IP（VIP）',
  'clusterDetail.config.editorTitle': 'TOML（高级）',
  'clusterDetail.config.editorPlaceholder':
    'CodeMirror 6 TOML 编辑器与行内诊断将在 WP-65 提供，当前为只读表单视图。',

  // 节点 / etcd / 高可用标签
  'cluster.node.ready': '就绪',
  'cluster.node.notReady': '未就绪',
  'cluster.role.control-plane': '控制面',
  'cluster.role.worker': '工作节点',
  'cluster.role.lb': '负载均衡',
  'cluster.etcd.ok': '正常',
  'cluster.etcd.degraded': '异常',
  'cluster.etcd.unreachable': '不可达',
  'cluster.ha.none': '无',

  // 操作（详情页操作历史 + 列表页共用）
  'operations.column.id': 'ID',
  'operations.column.cluster': '集群',
  'operations.column.kind': '类型',
  'operations.column.state': '状态',
  'operations.column.started': '开始时间',
  'operations.column.duration': '耗时',
  'operations.empty': '暂无操作记录',
  'operation.state.queued': '排队中',
  'operation.state.running': '执行中',
  'operation.state.succeeded': '成功',
  'operation.state.failed': '失败',
  'operation.state.cancelled': '已取消',
  'operation.state.interrupted': '已中断',
  'operation.kind.create': '创建集群',
  'operation.kind.add-nodes': '添加节点',
  'operation.kind.delete-node': '删除节点',
  'operation.kind.delete-cluster': '销毁集群',
  'operation.kind.upgrade': '升级',
  'operation.kind.certs-renew': '证书续期',
  'operation.kind.precheck': '预检查',
  'operation.kind.artifact': '制品',
  'operation.kind.package': '包',

  // 健康徽标（五态）
  'cluster.health.healthy': '健康',
  'cluster.health.degraded': '异常',
  'cluster.health.unreachable': '不可达',
  'cluster.health.stale': '数据过期',
  'cluster.health.unknown': '无快照',

  // 关于页
  'about.componentVersions': '组件版本',
  'about.capabilities': '能力约束',
  'about.column.component': '组件',
  'about.column.version': '版本',
  'about.column.constraint': '约束',
  'about.column.reason': '说明',

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
