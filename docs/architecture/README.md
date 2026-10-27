# kurin 架构演进文档集

> 2026-10-27 · 代码基线 v0.4.0
> 这组文档回答一个问题：kurin 从“能用的安装器”走到“可扩展、可视化、离线优先的集群生命周期工具”，骨架要怎么调整、要借鉴谁、先做什么。

## 文档地图

| 文档 | 内容 | 主要读者 |
|---|---|---|
| [01-landscape.md](01-landscape.md) | 与 KubeKey、Kubespray、kubeasz、RKE/RKE2、KubeOperator、PangeeCluster、werf、Zarf（附 Sealos）的对比，差异与借鉴清单 | 决策者 |
| [02-core-architecture.md](02-core-architecture.md) | 核心骨架问题清单（附源码证据）与目标架构：crate 拆分、RunContext、facts、become、流式传输、DAG 执行器、一切操作皆计划、Component、ClusterApi、集群状态记录、事件 v2 | 实现者 |
| [03-addons.md](03-addons.md) | 扩展性评估与两层组件模型：内置 Component + 声明式 addon（addon.yaml） | 实现者、addon 作者 |
| [04-oci-packages.md](04-oci-packages.md) | OCI 离线包格式、三种传输形态、preload/registry 分发、运行时 mirror 改写、签名与 SBOM | 实现者、离线交付 |
| [05-cli-quick-create.md](05-cli-quick-create.md) | 命令行直接传节点、角色、认证信息快速建集群 | 实现者、用户 |
| [06-dev-plan.md](06-dev-plan.md) | **分阶段开发计划**：P0～P7，60 余个工作包，每个都有依赖、步骤、测试、验收 | 编码 AI、维护者 |
| [../ui/ui-architecture-final.md](../ui/ui-architecture-final.md) | Web UI 方案（v1.1：表单优先、日志控制台） | 实现者 |

阅读顺序：决策者读本页 + 01；实现者读 02 → 对应专题 → 06 里自己负责的 WP。

## 决策记录（ADR）

| ADR | 决策 | 文档 |
|---|---|---|
| ADR-01 | workspace 拆分为 `kurin-config / core / ssh / artifact / kube / components / ops / server` + bin 包 `kurin` | 02 §3.1 |
| ADR-02 | 一切操作（create/add/delete/upgrade/certs/addon/etcd）都编译成计划，由同一个 DAG 执行器运行；“尽力清理”用显式 `BestEffort` 策略取代 `; true` | 02 §3.6、§3.7 |
| ADR-03 | 先 discover 节点 facts；制品按**节点**架构选择（支持混合架构）；非 root 用户自动 sudo 提权 | 02 §3.4、§3.5 |
| ADR-04 | 事件 v2（毫秒时间戳、seq、日志行、资源状态）+ `tracing`；每次操作都写 JSONL 日志，`kurin logs` 查看；`--json` v1 只新增 `ts` 字段 | 02 §3.10 |
| ADR-05 | SSH 流式 exec（逐行回调）与流式上传（分块 + 远端 sha256 校验）；变更命令断连不透明重试 | 02 §3.5 |
| ADR-06 | K8s API 访问统一为 `ClusterApi`：SSH 隧道 + kube-rs 为主，kubectl-over-ssh 回退；服务端 apply；nelm 式就绪跟踪与“卡住时自动诊断” | 02 §3.11 |
| ADR-07 | 两层组件模型：参与节点或 bootstrap 的是内置 Rust `Component`（显式注册表）；集群就绪后 apply 的是声明式 addon | 03 §3 |
| ADR-08 | addon 规范 `addon.yaml`：Helm chart 或 manifests + 带 digest 的镜像清单 + values JSON Schema + 依赖 + 就绪规则 + 受限 hooks；Helm 只负责渲染与记录 release（`--wait=false`），就绪由 kurin 跟踪 | 03 §4 |
| ADR-09 | 集群内状态记录 `kube-system/ConfigMap kurin-state`（脱敏配置、组件版本、addon、操作历史） | 02 §3.9 |
| ADR-10 | CLI 快速创建：参数与配置文件进入同一个 `ClusterConfig`；默认落盘配置；秘密只以引用形式落盘 | 05 |
| ADR-11 | 包 = OCI image index（ORAS 兼容），镜像保留上游 digest；仓库 / OCI layout 目录 / 单文件（tar.zst）三种传输形态；包分 base / addon / os / bundle / custom 五类 | 04 §3 |
| ADR-12 | 分发策略 `auto`：配置了仓库就推一次、节点并行拉取（registry）；否则流式预载（preload）；种子仓库是远期可选项 | 04 §5 |
| ADR-13 | 镜像改写放在运行时 mirror 层（containerd certs.d / CRI-O registries.conf.d / k3s registries.yaml），不用 webhook | 04 §5.4 |
| ADR-14 | 信任链：digest 全程校验；cosign 签名可选校验（CI 签名）；SPDX SBOM 以 referrer 附在包上 | 04 §6 |
| ADR-15 | 配置 schema v1alpha2（`ClusterSpec`、秘密引用 `{env}`/`{file}`、`[packages]`、`[addons]`）；v1alpha1 长期可读，`kurin config migrate` 保留注释；schemars 导出 JSON Schema | 02 §3.12 |
| ADR-16 | Web：表单优先的 9 步向导（批量录入、连通性测试、schema 表单），带时间戳的日志控制台；server 为独立 crate | UI 文档 v1.1 |
| ADR-17 | 节点上的 registry 默认 zot（v1alpha2 起）；新增 `external` 仓库类型 | 03 §5 |
| ADR-18 | `RunContext` 取代进程级全局状态；`Blackboard` 取代控制机本地文件 rendezvous；跨进程 `ClusterLock` | 02 §3.3、§3.13 |

## 不变的原则

这些是 kurin 的核心价值，所有决策都不能破坏它们（详见 01 §3）：单静态二进制、零控制机依赖；计划与执行分离；`Unknown ≠ failed`；全制品 digest 固定；不支持的组合在校验期诚实拒绝；严格 schema。

## 与既有文档的关系

- `docs/plugins.md`：P1/P4 完成后改写为“如何写一个 Component / 如何写一个 addon”。在那之前，它仍然描述现有代码。
- `docs/ui/ui-architecture-final.md`：已更新到 v1.1，与本组文档一致；两份 UI 调研稿保留作为历史输入。
- `docs/roadmap.md`、`docs/v1.0-gates.md`：本计划在 v1.0 门禁之前完成；门禁中的配置兼容和 `--json` 冻结规则，已体现在 06 §0.2 的 R4 中。
