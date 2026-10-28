# kurin 文档

kurin 是一个用 Rust 编写的声明式 Kubernetes 安装器：一条命令完成单节点、
多节点或 HA 集群的安装，支持在线/离线、kubeadm 与 k3s、Calico/Flannel/
Cilium/Kube-OVN、私有 registry 与 Web UI。

本页是文档索引。第一次使用请按 **入门 → 指南 → 参考** 的顺序阅读。

## 入门

| 文档 | 内容 |
|---|---|
| [安装 kurin](getting-started/install.md) | 二进制安装、源码构建、Docker 镜像、支持的节点操作系统 |
| [快速上手](getting-started/quickstart.md) | 3 分钟单节点、HA、离线、国内镜像、常见问题 |
| [Web UI](getting-started/ui.md) | `kurin serve` 启动、登录链接、远程访问、TLS、数据目录 |

## 指南

| 文档 | 内容 |
|---|---|
| [在线安装](guides/online.md) | 最小配置、precheck、dry-run、安装与验证 |
| [离线安装](guides/offline.md) | base/os 包制作、`[packages]`、断网安装、seed 模式 |
| [高可用](guides/ha.md) | kube-vip、external-endpoint、kurin-lb 三种入口 |
| [k3s](guides/k3s.md) | k3s 发行版、CNI 限制、嵌入 etcd HA、升级 |
| [增删节点](guides/nodes.md) | `add nodes`、`delete node`、`delete cluster`、幂等与续跑 |
| [升级](guides/upgrade.md) | kubeadm/k3s 升级、跨 minor、离线升级、drain 失败处理 |
| [证书](guides/certificates.md) | 到期检查、手动续期、systemd 定时续期 |
| [etcd 备份与恢复](guides/backup-restore.md) | 快照、恢复、定时任务 |
| [状态与日志](guides/status-logs.md) | `status`、`logs`、JSON 事件流 |
| [从集群导入配置](guides/import.md) | `import --from-cluster` 重建 v1alpha2 配置 |
| [Addon](guides/addons.md) | 内置目录、安装/升级/卸载、values、离线嵌入 |
| [OCI 包](guides/packages.md) | base/os/addon 包、inspect/verify/push/pull |
| [私有 registry](guides/registry.md) | docker-registry、zot、harbor、external |
| [seed 仓库](guides/seed.md) | 无 registry 节点的 pull 语义离线安装 |
| [国内镜像](guides/mirrors.md) | `KURIN_ZONE=cn`、镜像表、摘要校验 |

## 参考

| 文档 | 内容 |
|---|---|
| [CLI 参考](reference/cli.md) | 全部命令、参数、示例、退出码 |
| [配置参考](reference/configuration.md) | v1alpha1/v1alpha2 字段、校验规则、秘密引用 |
| [命令 × 形态矩阵](reference/command-matrix.md) | 每种 CNI/运行时/发行版/离线组合的支持状态 |
| [Registry 契约](reference/registry-contracts.md) | registry 模式、TLS、阶段与任务 |
| [JSON Schema](schema/cluster.v1alpha2.json) | 编辑器补全与校验；addon/package schema 同目录 |

## 运维

| 文档 | 内容 |
|---|---|
| [故障排查](operations/troubleshooting.md) | SSH、swap、端口、CNI、证书、断点续跑 |
| [安全](operations/security.md) | 秘密处理、SSH 主机密钥、UI 令牌/TLS、CIS 加固 |
| [一致性测试](operations/conformance.md) | hydrophone 报告与单节点/多节点结论 |

## Docker

| 文档 | 内容 |
|---|---|
| [Docker 运行 CLI 与 UI](docker/README.md) | 镜像构建/推送、`docker run` 示例、卷与网络、TLS |

## 开发

| 文档 | 内容 |
|---|---|
| [构建](development/building.md) | 工具链、Web 构建、release 二进制、文档站 |
| [测试](development/testing.md) | `just check`、单元/集成、Playwright、e2e tiers |
| [插件集成](development/plugins.md) | CNI/HA/发行版/制品/任务扩展点 |
| [架构总览](architecture/README.md) | 领域模型、planner/executor、组件、OCI 包 |

## 项目

- [路线图](roadmap.md)
- [v1.0 门槛](v1.0-gates.md)
- [M1 规格](m1-spec.md)
- [开发进度](https://github.com/weironz/kurin/blob/main/docs/progress.md)
- [Wave 1 契约](wave1-contracts.md) · [Wave 2 契约](wave2-contracts.md) · [Tails 契约](tails-contracts.md)
- [架构工作台账](https://github.com/weironz/kurin/blob/main/docs/architecture/PROGRESS.md)（内部）

## 约定

- 配置格式为 TOML（YAML 同构支持）；新配置使用 `v1alpha2`。
- 所有操作都是幂等的：重复执行会跳过已收敛的任务。
- 计划（`--dry-run`）与执行共用同一份任务图，不会出现"预览和实际不一致"。
- 命令输出默认面向终端；`--json` / `--json=v2` 提供稳定的 JSONL 事件流。