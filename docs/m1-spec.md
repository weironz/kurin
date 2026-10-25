# kurin M1 实施 spec

Rust 重写的 Kubernetes 集群安装器，对标 [kubekey](https://github.com/kubesphere/kubekey)（功能对标，非代码移植）。

## 目标（M1 完成即达成）

在满足以下环境的机器上，用一条命令装出生产可用集群，并支持增删节点与销毁：

- Linux amd64 / arm64 控制机；目标节点 Ubuntu 22.04+ / Debian 12 / Rocky Linux 9，amd64 或 arm64
- containerd（唯一运行时，CRI-O 仅留枚举扩展位）、Calico（唯一 CNI，tigera-operator 安装）
- K8s ≥ v1.30，版本表内嵌于编译期
- 单控制面 与 HA 控制面（kube-vip，非外部 LB）
- `create cluster` / `create config` / `add nodes` / `delete node` / `delete cluster` 五条命令
- 全部变更类命令支持 `--dry-run` 执行计划预览

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
| Helm SDK / 模板渲染引擎复刻 | 永久不做；插件用静态 manifest 或远端 helm 二进制 |

## 配置 schema

- **格式**：TOML 与 YAML 双格式，按文件扩展名识别（`.toml` / `.yml|.yaml`），同一 serde 类型双向服务。
- **核心结构**：`BTreeMap<NodeName, NodeConfig>`——hosts 与 roleGroups 合一，节点名全局唯一，跨区域引用错误（kubekey 的 hosts↔roleGroups 双表）在类型层消除。控制面成员与 worker 由 `NodeConfig` 上的角色字段表达，VIP 地址为集群级字段。
- **类型安全**（parse, don't validate）：互斥配置用 tagged enum（SSH 鉴权：password / private key；Calico 配置）；`IpAddr`、`Ipv4Net`（CIDR）、`semver::Version` 直接反序列化。目标：90% 配置错误止步于反序列化。
- **错误呈现**：解析错误经 miette 报告，高亮配置文件行/列并给修复建议（rustc 风格）；不输出裸 `parse error`。
- **兼容性**：不兼容 kubekey 的 `config-sample.yaml`（schema 结构性不同，KubeSphere 剔除后无对齐意义）。
- schema 类型独立于 `kurin-config` crate（M1 唯一预先拆出的 crate），供 e2e 与后续 air-gap 清单生成复用。

## Planner / Executor 分层（第一天就强制）

程序严格分两段，`--dry-run` 在第一天就必须可用：

- **Planner（编译期）**：读配置，在内存构建任务 DAG（petgraph），**不发起任何网络连接**。产物 = 阶段图 + 每任务的渲染后负载（kubeadm.yaml、config.toml、systemd unit、shell 片段）。
- **Executor（运行期）**：消费 Planner 产物，走 SSH 执行。

- 两档 dry-run：`--dry-run` 纯离线，只渲染计划不建连（可 100% 单测）；`--dry-run --check` 额外对每个任务跑 `check_state`，计划上标注 `would-change` / `unchanged`（Terraform `plan` 语义）。默认离线，因为用户常在配置阶段先看计划。

收益（也是验收标准）：核心逻辑不碰真实虚拟机即可 100% 单测；`apply` 与 `plan` 永远不会漂移。

## 任务模型：基于状态检测的幂等引擎

统一 `Task` trait，两步走（借鉴 Ansible/Terraform）：

```rust
trait Task {
    fn check_state(&self, ssh: &Session) -> Result<State>; // InSync / Drifted(原因)
    fn apply(&self, ssh: &Session) -> Result<()>;
}
```

- `check_state` 做真实远端探测：目标文件 SHA256 与预期比对、`systemctl is-active`、端口监听、kubeadm 状态等。
- 状态一致 → 输出 `Skipped/Unchanged`，毫秒级跳过；不一致才执行 `apply`。
- 任何一秒中断后重跑同一命令：已完成部分全部命中 check，直接从断点继续。
- 顺序：Planner 先对全图跑 check（得到 Drifted 子图），dry-run 即展示这个差集。

## 架构决策

| 决策点 | 选择 | 理由摘要 |
|---|---|---|
| 引导机制 | 调 `kubeadm`（init/join） | CNCF 一致性、升级路径、证书由生态兜底 |
| 远程执行 | 内嵌 `russh` + rustls + tokio 并发；自封装连接池：单机连接复用、`Semaphore` 限单机并发 Channel、`backon` 指数退避重试（防目标机 sshd `MaxStartups` 随机拒绝） | 单二进制自包含；openssh crate 备选但引入本地客户端依赖；M1 仅 password/private key 鉴权 |
| 编排模型 | Planner 产 petgraph DAG，Executor 执行；M1 只注册线性阶段图 prepare → bootstrap → join → cni → post；支持 `--resume` | 断点续跑安全性；图引擎通用性留给 M2 |
| 失败语义 | fail-fast + 可诊断上下文 + `--resume`；不做自动回滚 | 半状态回滚比手动续跑更危险；`delete cluster` 是逃生门 |
| 失败呈现 | 远程命令失败时完整框出：节点、命令、exit code、stdout、stderr | 运维排障的第一需求；miette 排版 |
| HA VIP | kube-vip 静态 pod | 无额外进程/节点；haproxy+keepalived 仅 M2 外部 LB 场景 |
| CNI 安装 | apply 官方 tigera-operator manifest + 内联 `Installation` CR | 不自写 Calico 资源，不接上游版本兼容责任；不引入 Helm |
| 版本表 | 编译期内嵌 const 表；`--with-kubernetes` 唯一覆盖入口，表外版本警告放行 | 可复现性 |
| 下载源 | URL 重写 trait 预留，M1 只有直连实现 | 镜像表 M2 |
| 模板管理 | `templates/` 目录 + minijinja 语法编写 systemd unit、kubeadm.yaml、containerd config 等；`rust-embed` 编译期嵌入二进制 | K8s 版本升级只需改模板文件，不动 Rust 逻辑；严禁在 `.rs` 里 `format!` 拼长 YAML/shell |

## 节点软件分发：二进制优先

- runc、containerd、crictl、kubelet、kubeadm、CNI 插件二进制由 kurin 下载/分发到 `/usr/local/bin`，systemd unit 由模板渲染后落盘——**不依赖 apt/yum 装这些**。抹平 90% 发行版差异（含未测试的 RHEL 系/信创系统实际可跑，官方支持面仍限上表矩阵）。
- 内核态/无法二进制分发的（conntrack、socat、ebtables、ipset、ipvsadm）：优先静态二进制，不可得则 apt/dnf 兜底安装。
- 内核模块与 sysctl（`br_netfilter`、`ip_forward` 等）：自动加载/设置，幂等。
- 检测到即**报错**（不自动改，附修复命令）：swap 未关、SELinux 非 permissive/disabled。
- preflight 检查结构化输出，`--skip-preflight=<name>` 按名豁免。

## 进度与输出

- 每任务一行日志，`[node]` 前缀 + 阶段缩进；无 TUI。
- `--json` 事件流（task-start/task-end/task-skip/task-fail/stage-done/cluster-ready）——e2e 与自动化的消费接口。
- kubeconfig：安装成功后拉取 admin.conf，合并进 `$KUBECONFIG`（缺省 `~/.kube/config`），`--kubeconfig` 覆盖；不包装 kubectl。

## 实施顺序（每步真实集群验证通过才进下一步）

1. Planner + `--dry-run` + 幂等 Task 引擎骨架（全单测，无 SSH）
2. 单节点 all-in-one：create / delete 全闭环
3. 多节点：1 master + N worker
4. HA 控制面：3 master + kube-vip
5. `add nodes` / `delete node` 增量操作

## e2e 验证回路（三层）

- **tier-0（秒级，每次改动必跑）**：systemd 容器节点（`jrei/systemd-ubuntu` 类镜像，Docker/Podman 起 3 节点，~3 秒）——验证 SSH 连接、连接池、文件分发、sysctl、containerd 启动、幂等重跑（跑两遍第二遍全 Skipped）。
- **tier-1（~8 分钟，每次改动必跑）**：multipass 单节点 all-in-one，含真实 kubeadm init；断言 `--json` 事件流与集群状态（node Ready、pod 网络连通）。
- **tier-2（~25 分钟，里程碑前跑）**：multipass 3 节点 + HA + add/delete；断言 join 计数、VIP 漂移、delete 后无残留（kubeadm reset、容器数据目录、CNI 残留、kube-vip 静态 pod）。

tier-0/tier-1 由 Makefile 驱动创建销毁。

## 构建与分发

- x86_64 与 aarch64 musl 静态二进制（rustls，无 OpenSSL）
- `cargo-dist` 产出 GitHub Releases + `curl | sh` 安装脚本
- license：Apache-2.0

## 仓库结构

```
kurin/            # 单 crate 主程序（Planner、Executor、russh 编排、任务）
kurin-config/     # 配置 schema 类型（M1 唯一预拆 crate）
templates/        # minijinja 模板，rust-embed 嵌入
e2e/              # tier-0/1/2 harness（Makefile + 断言）
```
