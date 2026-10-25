# kurin M1 实施 spec

Rust 重写的 Kubernetes 集群安装器，对标 [kubekey](https://github.com/kubesphere/kubekey)（功能对标，非代码移植）。

## 目标（M1 完成即达成）

在满足以下环境的机器上，用一条命令装出生产可用集群，并支持增删节点与销毁：

- Linux amd64 / arm64 控制机；目标节点 Ubuntu 22.04+ / Debian 12，amd64 或 arm64
- containerd（唯一运行时）、Calico（唯一 CNI，tigera-operator 安装）
- K8s ≥ v1.30，版本表内嵌于编译期
- 单控制面 与 HA 控制面（kube-vip，非外部 LB）
- `create cluster` / `create config` / `add nodes` / `delete node` / `delete cluster` 五条命令

## 非目标（M1 明确不做，进 backlog）

| 项 | 去向 |
|---|---|
| KubeSphere 全部逻辑（`--with-kubesphere`、ks-installer、artifact 打包含 KubeSphere 部分） | **永久剔除，不做**（2026-10-25 决策） |
| K3s、Docker/CRI-O/iSula 运行时 | M2+ |
| Flannel/Cilium/Kube-OVN/Multus | M2+ |
| `upgrade`、证书轮转命令 | M2 |
| air-gap artifact 打包与部署 | M2 |
| 外部 LB（haproxy + keepalived） | M2，仅用户显式要求时 |
| 中国区镜像表（`KURIN_ZONE=cn`） | M2；M1 预留 URL 重写接口，不接数据 |

## 配置 schema

- **格式**：TOML 与 YAML 双格式，按文件扩展名识别（`.toml` / `.yml|.yaml`），同一 serde 类型双向服务。
- **核心结构**：`BTreeMap<NodeName, NodeConfig>`——hosts 与 roleGroups 合一，节点名全局唯一，跨区域引用错误（kubekey 的 hosts↔roleGroups 双表）在类型层消除。控制面成员与 worker 由 `NodeConfig` 上的角色字段表达，VIP 地址为集群级字段。
- **类型安全**（parse, don't validate）：互斥配置用 tagged enum（SSH 鉴权：password / private key；Calico 配置）；`IpAddr`、`Ipv4Net`（CIDR）、`semver::Version` 直接反序列化。目标：90% 配置错误止步于反序列化。
- **兼容性**：不兼容 kubekey 的 `config-sample.yaml`（schema 结构性不同，KubeSphere 剔除后无对齐意义）。
- schema 类型独立于 `kurin-config` crate（M1 唯一预先拆出的 crate），供 e2e 与后续 air-gap 清单生成复用。

## 架构决策

| 决策点 | 选择 | 理由摘要 |
|---|---|---|
| 引导机制 | 调 `kubeadm`（init/join） | CNCF 一致性、升级路径、证书由生态兜底 |
| 远程执行 | 内嵌 `russh` + rustls + tokio 并发 | 单二进制自包含、结构化输出；M1 仅 password/private key 两种鉴权 |
| 编排模型 | 任务 DAG（节点间并发、任务级幂等 `check` 谓词、`--resume`）；M1 只注册线性阶段图 prepare → bootstrap → join → cni → post | 断点续跑安全性；图引擎通用性留给 M2 |
| 失败语义 | fail-fast + 可诊断上下文 + `--resume`；不做自动回滚 | 半状态回滚比手动续跑更危险；`delete cluster` 是逃生门 |
| HA VIP | kube-vip 静态 pod | 无额外进程/节点；haproxy+keepalived 仅 M2 外部 LB 场景 |
| CNI 安装 | apply 官方 tigera-operator manifest + 内联 `Installation` CR | 不自写 Calico 资源，不接上游版本兼容责任 |
| 版本表 | 编译期内嵌 const 表；`--with-kubernetes` 唯一覆盖入口，表外版本警告放行 | 可复现性 |
| 下载源 | URL 重写 trait 预留，M1 只有直连实现 | 镜像表 M2 |

## 节点前置条件（preflight）

- 自动做：`apt install` conntrack/socat/ebtables/ipset/ipvsadm；加载 `br_netfilter`、`ip_forward` sysctl；时间同步检查。
- 检测到即**报错**（不自动改，附修复命令）：swap 未关、SELinux 非 permissive/disabled。
- preflight 检查结构化输出，`--skip-preflight=<name>` 按名豁免。

## 进度与输出

- 每任务一行日志，`[node]` 前缀 + 阶段缩进；无 TUI。
- `--json` 事件流（task-start/task-end/task-fail/stage-done/cluster-ready）——e2e 与自动化的消费接口。
- kubeconfig：安装成功后拉取 admin.conf，合并进 `$KUBECONFIG`（缺省 `~/.kube/config`），`--kubeconfig` 覆盖；不包装 kubectl。

## 实施顺序（每步 multipass 验证通过才进下一步）

1. 单节点 all-in-one：create / delete 全闭环
2. 多节点：1 master + N worker
3. HA 控制面：3 master + kube-vip
4. `add nodes` / `delete node` 增量操作

## e2e 验证回路

multipass 创建/销毁虚拟机，断言 `--json` 事件流与集群真实状态（node Ready、pod 网络连通、join 后计数正确、delete 后无残留）。

- **tier-1**：单节点 all-in-one，~8 分钟，每次改动必跑
- **tier-2**：3 节点 + HA + add/delete，~25 分钟，里程碑前跑

## 构建与分发

- x86_64 与 aarch64 musl 静态二进制（rustls，无 OpenSSL）
- `cargo-dist` 产出 GitHub Releases + `curl | sh` 安装脚本
- license：Apache-2.0

## 仓库结构

```
kurin/            # 单 crate 主程序（CLI、russh 编排、任务）
kurin-config/     # 配置 schema 类型（M1 唯一预拆 crate）
e2e/              # tier-1 / tier-2 harness（脚本 + 断言）
```
