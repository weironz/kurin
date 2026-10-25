/target
# kurin M1 实施 spec

Rust 重写的 Kubernetes 集群安装器，对标 [kubekey](https://github.com/kubesphere/kubekey)（功能对标，非代码移植）。

## 目标（M1 完成即达成）

在满足以下环境的机器上，用一条命令装出生产可用集群，支持增删节点、销毁与**离线部署**：

- Linux amd64 / arm64 控制机；目标节点 Ubuntu 24.04+ / Debian 12 / Rocky Linux 9，amd64 或 arm64
- containerd（唯一运行时，CRI-O 仅留枚举扩展位）、Calico（唯一 CNI，tigera-operator 安装）
- K8s ≥ v1.30，版本表内嵌于编译期
- 单控制面 与 HA 控制面（kube-vip，非外部 LB）
- 命令：`create cluster` / `create config` / `add nodes` / `delete node` / `delete cluster` / `artifact`（打包离线包）
- 全部变更类命令支持 `--dry-run` 执行计划预览
- 在线安装与离线安装走同一套制品模型与安装流程；安装完成输出实际使用的组件版本清单

## 非目标（M1 明确不做，进 backlog）

| 项 | 去向 |
|---|---|
| KubeSphere 全部逻辑（`--with-kubesphere`、ks-installer、artifact 打包含 KubeSphere 部分） | **永久剔除，不做**（2026-10-25 决策） |
| K3s、Docker/CRI-O/iSula 运行时 | M2+ |
| Flannel/Cilium/Kube-OVN/Multus | M2+ |
| `upgrade`、证书轮转命令 | M2，显式命令入口 |
| 外部 LB（haproxy + keepalived） | M2，仅用户显式要求时 |
| 中国区镜像表（`KURIN_ZONE=cn`） | M2；M1 制品源为可配置 URL 列表，不内置国内镜像数据 |
| Helm SDK / 模板渲染引擎复刻 | 永久不做；插件用静态 manifest 或远端 helm 二进制 |
| 通用插件系统、playbook 语言 | 不做；先按模块组织，职责稳定后再拆 crate |

## 配置 schema

- **版本化**：配置带 `version` 字段，schema 演进有显式迁移路径；未知字段报错（deny_unknown_fields），不静默忽略。
- **格式**：TOML 与 YAML 双格式，按文件扩展名识别（`.toml` / `.yml|.yaml`），同一 serde 类型双向服务。
- **声明式**：配置只描述"想要什么"（节点、角色、集群版本、网络、制品来源），执行顺序由 Planner 决定。
- **核心结构**：`BTreeMap<NodeName, NodeConfig>`——节点名全局唯一，角色在 `NodeConfig` 上表达，VIP 为集群级字段。
- **处理流水线**：解析 → 补默认值 → 业务校验（跨字段冲突、不支持的取值）→ 内部模型，全部在执行前完成；执行器只见内部模型。
- **类型安全**（parse, don't validate）：互斥配置用 tagged enum（SSH 鉴权：password / private key；Calico 配置）；`IpAddr`、`Ipv4Net`（CIDR）、`semver::Version` 直接反序列化。目标：90% 配置错误止步于解析/校验阶段。
- **错误呈现**：解析与校验错误经 miette 报告，高亮配置文件行/列并给修复建议（rustc 风格）；不输出裸 `parse error`。
- **兼容性**：不兼容 kubekey 的 `config-sample.yaml`（schema 结构性不同，KubeSphere 剔除后无对齐意义）。
- schema 类型独立成 `kurin-config` crate（类型层职责天然稳定），供 e2e 与制品清单生成复用。

## 模块边界：核心是库，CLI 是壳

- 核心逻辑（配置、远程连接、制品管理、计划生成、任务执行）组织为可复用的 Rust library，CLI 只做参数接收、调用核心、展示结果——为将来 Web/API 接入留缝。
- 首版不做 crate 拆分（`kurin-config` 除外）；库内部先按模块组织（`config` / `ssh` / `artifact` / `plan` / `exec`），职责稳定后再拆。
- 不建设通用插件系统与 playbook 语言。

## Planner / Executor 分层（第一天就强制）

程序严格分两段，`--dry-run` 在第一天就必须可用：

- **Planner（编译期）**：读内部模型，在内存构建任务 DAG（petgraph），**不发起任何网络连接**。产物 = 阶段图 + 每任务的渲染后负载（kubeadm.yaml、config.toml、systemd unit、shell 片段）+ 制品需求清单。
- **Executor（运行期）**：消费 Planner 产物，走 SSH 执行。
- 两档 dry-run：`--dry-run` 纯离线，只渲染计划不建连（可 100% 单测）；`--dry-run --check` 额外对每个任务跑 `check_state`，计划上标注 `would-change` / `unchanged`（Terraform `plan` 语义）。默认离线，因为用户常在配置阶段先看计划。

收益（也是验收标准）：核心逻辑不碰真实虚拟机即可 100% 单测；`apply` 与 `plan` 永远不会漂移。

## 制品模型：在线离线一套

- 制品清单（artifact manifest）记录每个二进制、镜像、系统依赖的**版本、架构、来源 URL、校验值（sha256）**——单一事实源，内嵌于编译期版本表，可被配置覆盖来源。
- 在线模式：安装前按清单获取 → 本地缓存目录（含校验）→ 分发。缓存目录即天然离线包素材。
- 离线模式：从预先准备的制品目录读取（校验值必须匹配），**不发起任何下载**。
- 两种模式走同一套安装流程，仅制品来源不同。
- `kurin artifact` 子命令：从清单+缓存生成离线包（目录或 tar）；安装完成输出实际使用的组件版本清单（人读 + `--json`）。
- 校验失败、制品缺失在**分发前**报错，不允许跑到一半才发现。

## 任务模型：基于状态检测的幂等引擎

统一 `Task` trait，四步循环（检查 → 判定 → 执行 → 验证）：

```rust
trait Task {
    fn check_state(&self, ssh: &Session) -> Result<State>; // InSync / Drifted(原因)
    fn apply(&self, ssh: &Session) -> Result<Verify>;      // Verified(证据) / Unknown
}
```

- `check_state` 做真实远端探测：目标文件 SHA256 与预期比对、`systemctl is-active`、端口监听、kubeadm 状态等。
- 状态一致 → 输出 `Skipped/Unchanged`，毫秒级跳过；不一致才执行 `apply`。
- **区分"执行失败"与"结果未知"**：SSH 断开/超时/进程退出时远端操作可能已完成——`apply` 返回 `Unknown`，恢复路径是**先重新探测**再决定重试或跳过，不盲目重跑。本地执行记录仅辅助诊断，恢复判断以远端实际状态为准。
- `apply` 后必须 `verify`（同一探测逻辑复用），验证不通过视同失败。
- 任何一秒中断后重跑同一命令：已完成部分全部命中 check，直接从断点继续。

## 架构决策

| 决策点 | 选择 | 理由摘要 |
|---|---|---|
| 引导机制 | 调 `kubeadm`（init/join） | CNCF 一致性、升级路径、证书由生态兜底 |
| 远程执行 | 内嵌 `russh` + rustls + tokio 并发；自封装连接池：单机连接复用、`Semaphore` 限单机并发 Channel、`backon` 指数退避（防目标机 sshd `MaxStartups` 随机拒绝） | 单二进制自包含；openssh crate 备选但引入本地客户端依赖；M1 仅 password/private key 鉴权 |
| 编排模型 | Planner 产 petgraph DAG，Executor 执行；M1 只注册线性阶段图 prepare → bootstrap → join → cni → post；支持 `--resume` | 断点续跑安全性；图引擎通用性留给 M2 |
| 失败语义 | fail-fast + 可诊断上下文 + `--resume`；不做自动回滚 | 半状态回滚比手动续跑更危险；`delete cluster` 是逃生门；恢复策略按步骤定义，不承诺通用事务回滚 |
| 失败呈现 | 远程命令失败时完整框出：节点、命令、exit code、stdout、stderr | 运维排障第一需求；miette 排版 |
| HA VIP | kube-vip 静态 pod | 无额外进程/节点；haproxy+keepalived 仅 M2 外部 LB 场景 |
| CNI 安装 | apply 官方 tigera-operator manifest + 内联 `Installation` CR | 不自写 Calico 资源，不接上游版本兼容责任；不引入 Helm |
| 版本表 | 编译期内嵌 const 表；`--with-kubernetes` 唯一覆盖入口，表外版本警告放行 | 可复现性 |
| 模板管理 | `templates/` 目录 + minijinja 语法编写 systemd unit、kubeadm.yaml、containerd config 等；`rust-embed` 编译期嵌入二进制 | K8s 版本升级只需改模板文件，不动 Rust 逻辑；严禁在 `.rs` 里 `format!` 拼长 YAML/shell |

## 并发与变更边界

- **可并发**：preflight 检查、制品下载、文件分发、worker 节点准备。
- **串行 + 健康检查**：控制面与 etcd 关键变更逐节点执行，每步健康检查通过才进下一个。
- **批次**：worker join 按批处理，批内并发。
- 危险操作（升级、删除节点、重置集群）只从**显式命令**进入，配置文件改动永不隐式触发删除/重装。

## 节点软件分发：二进制优先

- runc、containerd、crictl、kubelet、kubeadm、CNI 插件二进制由制品模型分发到 `/usr/local/bin`，systemd unit 由模板渲染后落盘——**不依赖 apt/yum 装这些**。抹平 90% 发行版差异（含未测试的 RHEL 系/信创系统实际可跑，官方支持面仍限上表矩阵）。
- 内核态/无法二进制分发的（conntrack、socat、ebtables、ipset、ipvsadm）：优先静态二进制（进制品清单），不可得则 apt/dnf 兜底安装（离线模式下需在制品包内附带 deb/rpm）。
- 内核模块与 sysctl（`br_netfilter`、`ip_forward` 等）：自动加载/设置，幂等。
- 检测到即**报错**（不自动改，附修复命令）：swap 未关、SELinux 非 permissive/disabled。
- preflight 检查结构化输出，`--skip-preflight=<name>` 按名豁免。

## 进度与输出

- 每任务一行日志，`[node]` 前缀 + 阶段缩进；无 TUI。每条日志可定位**集群、节点、步骤、耗时、失败原因**五要素。
- `--json` 事件流（task-start/task-end/task-skip/task-fail/stage-done/cluster-ready）——e2e 与自动化的消费接口；结构化结果与终端进度展示彻底分离。
- kubeconfig：安装成功后拉取 admin.conf，合并进 `$KUBECONFIG`（缺省 `~/.kube/config`），`--kubeconfig` 覆盖；不包装 kubectl。

## 实施顺序（每步真实集群验证通过才进下一步）

1. Planner + `--dry-run` + 幂等 Task 引擎骨架（全单测，无 SSH）
2. 制品模型：清单 + 缓存 + 校验 + 分发（tier-0 验证）
3. 单节点 all-in-one（在线）：create / delete 全闭环
4. 多节点：1 master + N worker
5. HA 控制面：3 master + kube-vip
6. `add nodes` / `delete node` 增量操作
7. 离线部署：`artifact` 打包 → 无外网环境全流程验收

## e2e 验证回路（三层 + 故障场景）

- **tier-0（秒级，每次改动必跑）**：systemd 容器节点（`jrei/systemd-ubuntu` 类镜像，Docker/Podman 起 3 节点，~3 秒）——验证 SSH 连接、连接池、文件分发、sysctl、containerd 启动、幂等重跑（跑两遍第二遍全 Skipped）。
- **tier-1（~8 分钟，每次改动必跑）**：multipass 单节点 all-in-one，含真实 kubeadm init；断言 `--json` 事件流与集群状态（node Ready、pod 网络连通）。含离线变体（制品目录预热后断网跑）。
- **tier-2（~25 分钟，里程碑前跑）**：multipass 3 节点 + HA + add/delete；断言 join 计数、VIP 漂移、delete 后无残留（kubeadm reset、容器数据目录、CNI 残留、kube-vip 静态 pod）。
- **故障场景验收（优先级高于功能扩展，进 tier-1 轮转）**：
  - 同一配置重复执行 → 已完成操作全部正确跳过
  - SSH 中途断开 / kill 进程 → 重跑能探测实际进度并续跑
  - 制品缺失或校验失败 → 分发前报错，不留半安装状态
  - 部分节点失败 → 明确报告受影响节点/步骤范围，未失败节点状态清晰
  - 健康检查未通过 → 后续步骤全部停止
- tier-0/tier-1 由 Makefile 驱动创建销毁。

## 构建与分发

- x86_64 与 aarch64 musl 静态二进制（rustls，无 OpenSSL）
- `cargo-dist` 产出 GitHub Releases + `curl | sh` 安装脚本
- license：Apache-2.0

## 仓库结构

```
kurin/            # 单 crate 主程序（lib: config/ssh/artifact/plan/exec 模块 + thin CLI bin）
kurin/templates/  # minijinja 模板（rust-embed 以 crate 根为基准，编译期嵌入二进制）
kurin/examples/   # 示例配置（cluster.example.toml）
kurin-config/     # 配置 schema 类型（M1 唯一预拆 crate）
e2e/              # tier-0/1/2 harness（Makefile + 断言）
docs/             # m1-spec.md / progress.md / w1-tracks.md
```
