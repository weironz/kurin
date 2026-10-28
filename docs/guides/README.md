# 指南

按任务查找：

## 安装与部署

- [在线安装](online.md) —— 有外网时最短路径
- [离线安装](offline.md) —— 制品包、OS 包、断网节点
- [seed 仓库](seed.md) —— 没有 registry 节点时的 pull 语义
- [高可用](ha.md) —— 三种控制面入口
- [k3s](k3s.md) —— `distribution = "k3s"`
- [国内镜像](mirrors.md) —— `KURIN_ZONE=cn`

## 日常运维

- [增删节点](nodes.md)
- [升级](upgrade.md)
- [证书](certificates.md)
- [etcd 备份与恢复](backup-restore.md)
- [状态与日志](status-logs.md)
- [从集群导入配置](import.md)

## 组件与制品

- [Addon](addons.md)
- [OCI 包](packages.md)
- [私有 registry](registry.md)

配置字段见 [配置参考](../reference/configuration.md)，命令参数见
[CLI 参考](../reference/cli.md)。