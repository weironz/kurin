# 02 · 核心骨架评估与重构目标架构

> 状态：定稿 v1.0 · 2026-10-27 · 代码基线 v0.4.0（`b67858f`）
> 范围：引擎、计划、执行、传输、制品、配置模型、事件与日志、crate 结构。插件体系、OCI 包和 CLI 快速创建各有专门文档（[03](03-addons.md) / [04](04-oci-packages.md) / [05](05-cli-quick-create.md)），本文只定义它们依赖的核心接口。
> 读者：负责实现的开发者（包括 AI）。每节末尾给出对应的工作包编号，细节见 [06-dev-plan](06-dev-plan.md)。

---

## 1. 结论

**需要重构，但不推倒重来。** kurin 的引擎契约（`Task` 三段式、`Outcome::Unknown`、`SessionPool`、计划/执行分离）是对的，而且比同类项目更严格。问题出在契约**周围**的骨架：

1. 计划构建是一个 300 行、带魔法下标和发行版提前 `return` 的函数，**每加一个组件都要改它**。
2. 有**正确性缺陷**：节点制品按**控制机**架构选取、非 root 用户不提权、进程级全局状态。
3. 只有 create/add 走引擎；upgrade、teardown、certs 是**命令式旁路**，没有 dry-run、续跑和统一事件。
4. 事件没有时间戳，远端命令输出不可见，日志靠 `eprintln!`。
5. 领域模型把 registry、storage 字段平铺在 `Cluster` 上，扩展性差。
6. 单个 `kurin` crate 同时承担 lib 和 bin，无法干净地挂接 server、OCI、kube 客户端这些重依赖。

重构按“绞杀者”方式推进：**先用快照测试锁住现有行为，再一层层替换**。每个工作包合入后，`just check` 和 e2e 都要保持绿色，CLI 输出除非工作包明确说明，否则不变。

---

## 2. 现状评估

### 2.1 保留的设计

| 设计 | 位置 | 保留理由 |
|---|---|---|
| `Task`：`check → apply → verify`，`Drift`/`Outcome::Unknown` | `engine.rs` | 幂等与“未知不等于失败”是 kurin 的核心价值 |
| `Session` / `SessionPool` trait，`MockSession` 测试 | `engine.rs`、`executor.rs`、`kurin/tests/` | 传输可替换，单测不需要真机 |
| 计划零 I/O、执行器是唯一开 SSH 的地方 | `plan.rs` 头注释 | `--dry-run` 与执行不会漂移 |
| 阶段内跨节点并行、阶段间 fail-fast 且让在途任务先收尾 | `executor.rs` | 行为正确、可预测 |
| 制品清单 + sha256 固定，OCI 镜像存储 | `artifacts/manifest.json`、`artifact.rs` | 是 OCI 包化的直接基础 |
| MiniJinja 模板，禁止 `format!` 拼 YAML | `templates/` | 保持 |
| 严格 schema + 带 span 的诊断 | `kurin-config` | 保持，并扩展到 v1alpha2 |

### 2.2 问题清单（均有源码依据）

| # | 问题 | 证据 | 后果 | 处理 |
|---|---|---|---|---|
| P1 | **节点制品按控制机架构选择** | `plan.rs:71`、`tasks/cni/calico.rs:60`、`flannel.rs:40`、`cri_o.rs:67`、`upgrade.rs:151`、`main.rs:483` 都用 `std::env::consts::ARCH`，遇到其他架构直接 `panic!` | x86 控制机部署 arm64 节点会装错二进制；不支持混合架构集群 | §3.4 facts |
| P2 | **不提权** | 只有 `precheck.rs:150-152` 认可“免密 sudo”，`ssh.rs` 的 exec 从不包 `sudo` | precheck 通过，安装却失败；只能用 root | §3.5 become |
| P3 | **进程级全局状态** | `artifact.rs:180-200` 用 `OnceLock` 存 zone 与 manifest 覆盖，约 12 处任务直接读 | 常驻服务（Web）或单进程管理多集群时串用 | §3.3 RunContext |
| P4 | **计划构建不可扩展** | `plan.rs` 用魔法下标 `push(4, …)`；k3s 走提前 `return`；“排除 lb/registry 节点”的过滤写了 4 遍 | 新组件必须改 `plan.rs`；容易放错阶段 | §3.6 PlanBuilder + §3.8 Component |
| P5 | **upgrade/teardown/certs 不走引擎** | `upgrade.rs`、`teardown.rs`、`certs.rs` 各自循环节点并 `eprintln!` | 这些操作没有 dry-run、续跑、统一事件和 UI 进度 | §3.7 一切操作皆计划 |
| P6 | **静默容错** | `upgrade.rs:397` 的 drain 以 `; true` 结尾，exit code 恒为 0，`drain_ok` 恒真；teardown 多处 `; true` | drain 失败不阻断升级（真 bug）；清理失败不可见 | WP-01 修复；§3.7 显式 `BestEffort` 策略 |
| P7 | **制品整文件读入内存再逐节点推送** | `tasks/install.rs:193` `std::fs::read` 后 `write_file` | 内存占用随镜像大小线性增长；控制机上行带宽是瓶颈 | §3.5 流式上传；[04](04-oci-packages.md) 分发策略 |
| P8 | **跨任务数据靠控制机本地文件交换** | join 材料 `~/.cache/kurin/clusters/<name>`（`bootstrap.rs:19`）、k3s token（`k3s.rs` 头注释：“executor 只给每个任务自己节点的 session”） | 换控制机或服务端多集群时路径冲突；秘密明文落盘 | §3.3 Blackboard |
| P9 | **事件无时间戳，远端输出不可见** | `engine.rs` `Event` 只有 `ms`；`ExecOutput` 在命令结束后才返回 | 无法做详细日志界面；排障只能重跑 | §3.10 事件 v2 + tracing；§3.5 流式 exec |
| P10 | **K8s API 只能通过 SSH 执行 kubectl 文本** | `status.rs`、`storage.rs`、各 CNI 都在锚点节点上执行 `kubectl … -o json` 并解析 | 做不到就绪跟踪（watch）、Pod 日志流和服务端 apply 冲突检测 | §3.11 ClusterApi |
| P11 | **领域模型平铺** | `model.rs:38-69` 有 `registry_username/password/data_dir`、`storage_kind/server/path` 等字段 | 每个组件都要改核心结构、`from_config` 和 `validate` | §3.12 ClusterSpec |
| P12 | **集群“记忆”只在控制机本地** | `state.rs`（运行状态）、`post.rs`（kubeconfig 合并）、join 材料都在控制机 | 换一台控制机就不知道集群装了什么；`add nodes` 只能临时探测版本 | §3.9 集群内状态记录 |
| P13 | **单 crate 承担 lib 与 bin** | 工作区只有 `kurin`、`kurin-config` | server/OCI/kube 这些重依赖无处安放；会出现包循环依赖（见 UI 文档 §4.1） | §3.1 workspace 拆分 |
| P14 | **SSH 连接断开后透明重试 exec** | `ssh.rs:354-370` | 已执行的变更命令可能被重复执行 | §3.5：变更命令断连时返回 `Unknown`，交给引擎重新探测 |
| P15 | **主机密钥只有 TOFU** | `ssh.rs` 头注释 | 首次连接存在中间人风险 | §3.5：增加 `known-hosts = "strict"` 选项 |

---

## 3. 目标架构

### 3.1 Workspace 拆分（ADR-01）

```text
crates/
  kurin-config/      配置 schema（v1alpha1 + v1alpha2）、转换、校验诊断、JSON Schema 导出、秘密引用
  kurin-core/        引擎：Task/TaskCtx、Plan/PlanBuilder、Phase、Executor(DAG)、RunContext、
                     Blackboard、Facts 类型、Event v2 + EventBus、RunState、ClusterLock、Session trait
  kurin-ssh/         russh 实现：become、流式 exec、流式上传、本地/远程端口转发、known_hosts 策略
  kurin-artifact/    制品清单、本地缓存、OCI 包格式、oci-client、layout/tar、校验、zone 镜像源改写
  kurin-kube/        ClusterApi：SSH 隧道 + kube-rs 后端、kubectl-over-ssh 后端、SSA apply、
                     就绪跟踪器、helm 执行器
  kurin-components/  内置组件：distro/{kubeadm,k3s}、runtime/{containerd,crio}、
                     cni/{calico,flannel,cilium,kube-ovn,none}、ha/{kube-vip,kurin-lb,external}、
                     registry/{zot,docker-registry,harbor}、os/{baseline,sysctl,kernel-deps}、
                     addon 引擎 + 内置 addon 目录
  kurin-ops/         操作：create / add-nodes / delete-node / delete-cluster / upgrade / certs /
                     status / precheck / addon / package / etcd；集群状态记录
  kurin-server/      Web 服务（feature 门控由 bin 决定）
kurin/               bin 包（包名保持 `kurin`，release 配方不变）：clap、快速创建参数、终端渲染
web/                 前端工程
```

依赖方向（只允许向左依赖）：

```text
kurin-config ← kurin-core ← {kurin-ssh, kurin-artifact, kurin-kube} ← kurin-components ← kurin-ops ← kurin-server ← kurin(bin)
```

- bin 成为独立包，`kurin-server` 不再与 bin 形成循环，因此 UI 文档 §4.1 中“server 只能作为 feature 模块”的限制解除（UI 文档已同步更新）。
- `kurin/templates/`、`artifacts/manifest.json` 随相应 crate 迁移（模板进 `kurin-components`，清单进 `kurin-artifact`）。
- 拆分本身（WP-02）是纯移动 + 改 `use`，不改行为。

### 3.2 运行时对象关系

```text
OpRequest(kind, ClusterSpec, params)
   │  kurin-ops::prepare()
   ▼
RunContext ──────────────┐  (zone, package/manifest, become, cancel, EventBus, Blackboard, lock, op_id)
   │                     │
   │ discover：Facts 计划（所有节点并行）+ 读取集群状态记录
   ▼                     │
Plan = kurin-ops::<op>::plan(spec, facts, state, registry)   ← 每个 Component 贡献任务
   │                     │
   ▼                     │
Executor(DAG) ──► Task.check/apply(TaskCtx{run, node(facts+session), cluster, api?, bb, log})
   │
   ▼
EventBus ──► 终端渲染 / --json / JSONL 日志文件 / Web SSE
```

### 3.3 RunContext 与 Blackboard（ADR-18）

```rust
// kurin-core
pub struct RunContext {
    pub op_id: OpId,                       // ULID；CLI 与 Web 一致
    pub cluster: String,
    pub zone: Zone,
    pub sources: Arc<ArtifactSources>,     // 包 / 清单 / 离线目录，取代 MANIFEST_OVERRIDE
    pub cancel: CancellationToken,
    pub events: EventBus,
    pub bb: Blackboard,
    pub facts: FactsTable,                 // discover 之后才填充
    pub settings: RunSettings,             // 并发上限、超时、become 默认值等
}

/// 类型化的跨任务数据交换，取代控制机本地文件 rendezvous。
pub struct Blackboard { /* RwLock<HashMap<&'static str, Box<dyn Any>>> + 可持久化部分 */ }
pub struct Key<T> { name: &'static str, persist: Persist, _t: PhantomData<T> }
pub enum Persist { No, RunState, RunStateSecret }   // Secret：落盘时 0600，并纳入日志脱敏表

pub const JOIN_MATERIAL: Key<JoinMaterial> = Key::new("kubeadm.join", Persist::RunStateSecret);
pub const K3S_TOKEN:     Key<SecretString> = Key::new("k3s.token",   Persist::RunStateSecret);
```

- `--resume` 时，`Persist::RunState*` 的值从运行状态文件恢复，保证续跑的任务拿得到上游产出。
- 所有 `crate::artifact::zone()` 和 `install_manifest()` 调用改为 `cx.run.zone` 与 `cx.run.sources`。
- 验收：单进程内并行跑两个 `RunContext`（不同 zone、不同 manifest），生成的计划和渲染出的镜像地址互不影响。（WP-07）

### 3.4 节点 Facts（ADR-03）

```rust
pub struct NodeFacts {
    pub arch: Arch,                       // uname -m → amd64/arm64
    pub os: OsInfo,                       // /etc/os-release：id、version_id、family(Deb|Rpm|Other)、pretty
    pub kernel: String,
    pub cgroup_v2: bool,
    pub init_systemd: bool,
    pub cpus: u32, pub mem_mib: u64, pub disk_free_mib: BTreeMap<String, u64>, // /, /var/lib
    pub hostname: String,
    pub primary_ip: IpAddr,
    pub user: String, pub is_root: bool, pub sudo: SudoCapability,   // None | Passwordless | NeedsPassword
    pub tcp_forwarding: Option<bool>,      // sshd AllowTcpForwarding，决定 ClusterApi 后端
    pub existing: ExistingInstall,         // kubelet/k3s/containerd 版本、/etc/kubernetes/admin.conf 是否存在
}
```

- **discover 阶段**：每个节点执行一次合并脚本（与 precheck 相同的“一次往返”模式），解析后存入 `RunContext.facts`。
- `precheck` 改为在 facts 之上做判断，不再重复探测。
- **制品按 `facts.arch` 选择**；所有 `std::env::consts::ARCH` 的用法删除（控制机架构只在“为本机下载制品”时使用）。混合架构集群自然得到支持：包与缓存同时持有多个架构。
- OS 家族决定内核依赖（deb/rpm）与 OS 依赖包的选择。
- 验收：x86_64 控制机 + arm64 节点（multipass on arm 或 qemu）的 tier-1 安装通过；新增混合架构单测（MockSession 返回不同 `uname -m`）。（WP-04）

### 3.5 传输层：become、流式、隧道（ADR-03、ADR-05）

```rust
#[async_trait]
pub trait Session: Send + Sync {
    async fn exec(&self, cmd: &Cmd) -> Result<ExecOutput>;
    async fn exec_stream(&self, cmd: &Cmd, sink: &mut (dyn FnMut(Stream, &str) + Send)) -> Result<ExecOutput>;
    async fn upload(&self, src: UploadSource, dst: &RemotePath, mode: u32) -> Result<UploadReport>; // 流式 + 远端 sha256 校验
    async fn file_sha256(&self, path: &str) -> Result<Option<String>>;
    async fn service_active(&self, name: &str) -> Result<bool>;
    async fn forward_local(&self, remote: SocketAddr) -> Result<LocalForward>;   // ssh -L，用于 ClusterApi
    async fn forward_remote(&self, local: SocketAddr) -> Result<RemoteForward>;  // ssh -R，用于种子仓库（04 §5.3）
}

pub struct Cmd { pub script: String, pub privileged: bool, pub mutating: bool, pub timeout: Option<Duration>, pub redact: Vec<SecretString> }
pub enum UploadSource { File(PathBuf), Blob { store: Arc<BlobStore>, digest: Digest }, Bytes(Bytes) }
```

- **become**：`privileged = true` 且登录用户不是 root 时包成 `sudo -n -- sh -c '<script>'`；如果 `[ssh] become-password`（秘密引用）存在，改用 `sudo -S`，经 stdin 传入。`upload` 先写 `/tmp/kurin-<rand>`，再 `sudo install -m <mode>` 到目标路径。默认 `privileged = true`，只读探测显式设为 false。
- **流式 exec**：russh channel 每收到一块数据就切行回调；`TaskCtx.log` 把行以 `Log{stream, line}` 事件发出（默认 debug 级，Web 可见，终端在 `-v` 时显示）。
- **流式上传**：每块 64 KiB，经 channel 写入 `cat > tmp`，同时在本地算 sha256，结束后与远端 `sha256sum` 比对；不再整文件读入内存（修 P7）。
- **断连语义**（修 P14）：`mutating = true` 的命令在连接中断后**不再透明重试**，直接返回 `TransportLost` 错误，由引擎映射为 `Outcome::Unknown` 并重新探测；只读命令保留一次重试。
- **主机密钥**：`[ssh] known-hosts = "tofu"（默认）| "strict" | "accept-new"`。
- （WP-03、WP-05、WP-08）

### 3.6 计划：PlanBuilder 与 DAG 执行器（ADR-02）

```rust
pub struct Plan { pub op: OpKind, pub phases: Vec<Phase>, pub tasks: Vec<PlannedTask>, pub edges: Vec<(TaskIdx, TaskIdx)>, pub fingerprint: Sha256 }
pub struct Phase { pub id: &'static str, pub label: &'static str }      // 仅用于展示与默认屏障

pub struct PlanBuilder<'a> { /* spec, facts, state, op */ }
impl PlanBuilder<'_> {
    pub fn phase(&mut self, id: &'static str) -> PhaseHandle;            // 按操作模板声明的顺序
    pub fn add(&mut self, phase: PhaseHandle, task: Arc<dyn Task>) -> TaskIdx;
    pub fn after(&mut self, t: TaskIdx, dep: TaskIdx);                   // 细粒度依赖
    pub fn rolling(&mut self, tasks: &[TaskIdx], max_parallel: usize);  // 升级批次
    pub fn nodes(&self) -> NodeSel<'_>;                                  // .k8s() .control_plane() .workers() .role(Lb) .first_cp()
}
```

- **默认语义等同现状**：同一 phase 内的任务都依赖上一 phase 的屏障，所以不写 `after` 时，行为与今天的阶段链完全一致。组件可以加更细的依赖（例如“cert-manager 就绪后再装 ingress”）。
- **调度器**：就绪队列 + 全局并发上限 + 每节点并发上限（不超过 SSH channel 数）。失败时停止调度依赖它的任务，在途任务先收尾，然后报告第一个错误（现有语义）。任务可以声明 `FailurePolicy::BestEffort`：失败记为告警事件、不阻断下游（用于 teardown，取代 `; true`）。
- **取消**：每次从就绪队列取任务前检查 `cancel`；正在执行的远端命令总是等它结束。
- **计划指纹**：任务 key 与 describe 的哈希，用于 `--resume` 校验（计划变了就拒绝续跑）和 Web 的“确认的计划即执行的计划”。
- `NodeSel` 收拢“排除 lb/registry 节点”等过滤，消除 P4 的重复代码。
- 操作模板定义 phase 顺序：

| 操作 | phases |
|---|---|
| create / add-nodes | discover · preflight · artifacts · prepare · registry · images · vip · bootstrap · api-ready · join · cni · addons · post |
| delete-node | discover · drain · reset · cleanup · api-cleanup · local |
| delete-cluster | discover · reset(workers) · reset(control-plane) · cleanup · local |
| upgrade | discover · preflight · artifacts · first-cp · control-plane(rolling=1) · workers(rolling=N) · addons · post |
| certs-renew | discover · renew(rolling=1) · verify |
| addon-install / addon-remove | discover · addons · post |
| etcd-backup / restore | discover · snapshot · fetch / restore(rolling=1) |

- （WP-10、WP-11）

### 3.7 一切操作皆计划（ADR-02）

- `upgrade.rs`、`teardown.rs`、`certs.rs` 中的步骤逐个改写成 `Task`。例如 upgrade 的 `drain/install-bins/kubeadm-upgrade/restart-kubelet/uncordon/ready-gate` 是同一节点上串行的任务链，节点之间用 `rolling` 控制。
- 收益：所有操作都自动获得 `--dry-run [--check]`、`--resume`、统一事件与 UI 进度、计划指纹，以及每个节点的显式结果。
- 现有的 `report("[node] ✓ step")` 输出改由终端渲染器根据事件生成；CLI 文本格式保持兼容（快照测试锁定）。
- （WP-12、WP-13、WP-14）

### 3.8 Component：内置组件的统一入口（ADR-07）

```rust
pub trait Component: Send + Sync {
    fn id(&self) -> &'static str;                         // "cni/calico"
    fn kind(&self) -> ComponentKind;                      // Distribution | Runtime | Cni | Ha | Registry | Os | AddonEngine
    fn enabled(&self, spec: &ClusterSpec) -> bool;
    fn validate(&self, spec: &ClusterSpec, d: &mut Diagnostics);
    fn capabilities(&self) -> Capabilities;               // 支持的发行版 / 架构 / 离线 / 操作
    fn artifacts(&self, spec: &ClusterSpec, arch: Arch) -> Vec<ArtifactRef>;   // 制作包与预载时使用
    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()>; // 按 b.op() 贡献任务，不支持的操作什么也不做
}

pub struct ComponentRegistry { items: Vec<Arc<dyn Component>> }   // 显式注册表，kurin-components::registry()
```

- `plan.rs` 的 300 行分派缩成：对每个 `enabled` 组件调用 `plan(&mut builder)`。
- `validate` 与 `capabilities` 汇总后同时用于配置校验、`command-matrix.md` 自动生成和 Web 能力表（单一事实来源）。
- 组件的配置段由组件自己定义类型（例如 `network.cni.calico.*`），通过 `kurin-config` 中的 `#[serde(flatten)]` 按 kind 区分的枚举承载，保持 `deny_unknown_fields`。
- 迁移顺序：os → runtime → distro → ha → cni → registry → storage（storage 最终改成声明式 addon，见 [03](03-addons.md)）。
- （WP-15、WP-16）

### 3.9 集群内状态记录（ADR-09）

- 每次变更操作在 `post` 阶段把以下内容写入 `kube-system/ConfigMap kurin-state`（服务端 apply，field manager 为 `kurin`）：
  - `kurin-version`、`op-id`、`op-kind`、`finished-at`；
  - `spec.toml`：规范化、**已脱敏**的配置；
  - `components.json`：组件 id → 版本、来源包 digest；
  - `addons.json`：已安装 addon → 版本、values 摘要；
  - `history.json`：最近 20 次操作摘要。
- 读取方：`add nodes`（取代现在探测 `/version` 的逻辑）、`upgrade`（当前版本）、`addon`（已装列表）、`status`（期望与观测对比）、`kurin import --from-cluster <ip>`（从集群反推配置，用户只需补凭据）。
- 记录不存在时（老集群）回退到现有的探测逻辑，并在下次变更后补写。
- （WP-19）

### 3.10 事件 v2 与日志（ADR-04）

```rust
pub struct Envelope { pub v: u8 /*2*/, pub seq: u64, pub ts: Timestamp /*UTC，毫秒*/, pub op: OpId, pub body: Body }
pub enum Body {
    OpStarted { kind: OpKind, cluster: String, plan_fingerprint: String, kurin: String },
    PhaseStarted { phase: &'static str }, PhaseDone { phase: &'static str, ms: u64 },
    TaskStart { key: String, node: String, phase: &'static str },
    TaskSkipped { .. }, TaskDone { .., ms: u64 }, TaskReconciled { .. }, TaskFailed { .., reason: String },
    TaskWarn { key: String, node: String, message: String },                 // BestEffort 失败等
    Log { level: Level, node: Option<String>, task: Option<String>, stream: Stream /*kurin|stdout|stderr*/, line: String },
    Resource { kind: String, namespace: Option<String>, name: String, ready: bool, status: String },  // 就绪跟踪
    OpFinished { result: OpResult /*succeeded|failed|cancelled*/, ms: u64, error: Option<String> },
}
```

- **tracing**：任务内部用 `tracing::{info, debug, warn}!`，处在 `op / phase / task / node` 的 span 里；一个自定义 `Layer` 把日志转成 `Body::Log` 推入 `EventBus`。
- **EventBus**：单调递增 `seq`；订阅者包括终端渲染器、`--json` 输出、JSONL 文件写入器、Web 持久化与 SSE。写入有界；慢订阅者断开后按 `seq` 从文件补读。
- **每次操作都落 JSONL 日志**（CLI 也一样）：`$XDG_STATE_HOME/kurin/logs/<cluster>/<op-id>.jsonl`，新增 `kurin logs [--cluster X] [--op ID] [--follow] [--level debug]` 命令查看。
- **脱敏**：`RunContext` 维护秘密值表（密码、token、join 命令），所有 `Log` 行与错误文本在发布前替换为 `******`。
- **兼容**：`--json` 默认继续输出 v1 事件形状，只**新增 `ts` 字段**（v1.0 门禁允许新增可选字段）；`--json=v2` 输出完整信封。终端默认格式不变，新增 `--timestamps` 和 `-v/-vv`。
- （WP-05、WP-06）

### 3.11 ClusterApi：访问 Kubernetes API（ADR-06）

```rust
#[async_trait]
pub trait ClusterApi: Send + Sync {
    async fn apply(&self, objs: &[DynamicObject], opts: ApplyOpts) -> Result<ApplyReport>;   // 服务端 apply，field manager = kurin
    async fn delete(&self, refs: &[ObjRef], opts: DeleteOpts) -> Result<()>;
    async fn get_json(&self, r: &ObjRef) -> Result<Option<serde_json::Value>>;
    async fn list_json(&self, gvk: &Gvk, ns: Option<&str>, selector: Option<&str>) -> Result<Vec<serde_json::Value>>;
    async fn track(&self, refs: &[ObjRef], opts: TrackOpts, sink: &EventBus) -> Result<TrackResult>;  // nelm 式就绪跟踪
    async fn pod_logs(&self, pod: &ObjRef, opts: LogOpts, sink: &EventBus) -> Result<()>;
}
```

- **主后端 `TunnelApi`**：在锚点控制面上开 `ssh -L` 等价的本地转发到 `127.0.0.1:6443`，用拉回来的 admin kubeconfig 构造 kube-rs 客户端，`tls-server-name = "kubernetes"`（kubeadm 与 k3s 的 apiserver 证书都包含该 SAN）。锚点不可用时依次尝试其他控制面。
- **回退后端 `KubectlApi`**：sshd 禁止转发时（`facts.tcp_forwarding = false`），在锚点执行 `kubectl apply --server-side -f -` 与 `kubectl get -o json` 轮询，功能相同、精度略低。
- **就绪跟踪器**（借鉴 nelm）：Deployment/StatefulSet/DaemonSet/Job/Pod/PVC/CRD 有内建就绪规则，未知 CR 按 `status.conditions[type=Ready]` 启发式判断；每 2 秒发一次 `Resource` 状态事件；超时或失败时自动抓取未就绪 Pod 的最近事件和容器日志尾部，作为 `Log` 事件输出。这就是 UI 日志界面里“为什么卡住”的直接答案。
- 使用者：CNI（就绪判定）、addons、status collector、kurin-state 记录、节点标签、delete-node 的 API 清理。
- 体积：kube-rs + k8s-openapi 会增加二进制体积，WP-18 必须实测并记录；`k8s-openapi` 只启用一个 K8s 版本特性。
- （WP-17、WP-18）

### 3.12 领域模型：ClusterSpec（ADR-15）

```rust
pub struct ClusterSpec {
    pub meta: Meta,                         // name、labels
    pub kubernetes: KubernetesSpec,         // version、distribution、cluster-domain…
    pub runtime: RuntimeSpec,               // kind + 运行时专属参数
    pub network: NetworkSpec,               // pod/service CIDR、dns、cni: CniSpec(kind + 专属参数)
    pub ha: HaSpec,
    pub ssh: SshDefaults,                   // user、port、auth（秘密引用）、become、known-hosts
    pub nodes: Vec<NodeSpec>,               // 保持 name 唯一；role 单值；可选 ssh 覆盖、labels、taints
    pub registry: Option<RegistrySpec>,     // 节点上的私有仓库，或 external（仅地址与凭据）
    pub packages: PackageSources,           // 见 04
    pub addons: BTreeMap<String, AddonSpec>,// 见 03
    pub source: SourceText,                 // 原文，仅用于诊断渲染，Debug/Serialize 时跳过
}
```

- **秘密引用**（ADR-10 同样依赖）：所有秘密字段接受 `"字面值"`，或 `{ env = "VAR" }`、`{ file = "/path" }`，在 `kurin-ops` 准备阶段解析为 `SecretString`（`secrecy` crate，Debug 时打码）。
- `v1alpha1` 继续可读：加载时转换为 `ClusterSpec`；`[storage]` 映射为 `addons.local-path` / `addons.nfs-subdir`。提供 `kurin config migrate`，输出 v1alpha2（用 toml_edit 尽量保留注释）。
- `kurin-config` 用 `schemars` 导出 JSON Schema，供 Web 表单和外部工具使用。
- （WP-20、WP-21、WP-22）

### 3.13 互斥与续跑

- `ClusterLock`：对 `$XDG_STATE_HOME/kurin/locks/<cluster>.lock` 加 flock，所有变更操作（CLI 与 Web）都要获取（UI 文档 §6.5）。
- `RunState` 升级到 v2：记录 `op_id`、计划指纹、已完成任务、Blackboard 中需要持久化的部分；`--resume` 时校验指纹。
- （WP-09）

---

## 4. 测试策略

| 层 | 新增 | 目的 |
|---|---|---|
| 计划快照 | `crates/kurin-ops/tests/plans/`：固定配置矩阵（单节点 / 3CP kube-vip / kurin-lb / external / k3s / k3s-HA / crio / 每种 CNI / registry 三种 / storage / 离线 / cn zone / arm64 / 混合架构）× 每种操作，`insta` 快照 `PlanView` | 重构等价性的主要防线（WP-00） |
| CLI 输出快照 | 用 MockSession 跑完整 create/delete/upgrade，快照 stderr/stdout | 锁定“CLI 输出不变” |
| 事件契约 | 断言 v1 事件字段集合（v1.0 门禁 §2）与 v2 信封 | 防止破坏机器读者 |
| 脚本化 MockSession | 按命令正则返回预设输出，能模拟断连、超时、非 root、不同 `uname -m` | 测 become、Unknown、facts |
| 组件单测 | 每个 Component 的 validate/artifacts/plan | 插件化后的回归 |
| e2e | 现有 tier0～3 不变；新增混合架构、非 root sudo 用户、OCI 包离线（见 07） | 真机验证 |

---

## 5. 迁移顺序与原则

1. **WP-00 先行**：没有计划快照和输出快照，不开始任何重构。
2. 每个工作包都是一个独立 PR：合入后 `just check` 通过、tier0 通过；涉及执行语义的还要 tier1/tier2 通过。
3. 快照有变化时，PR 描述里逐条解释原因（例如“新增 discover 阶段”），不允许整体 `--accept`。
4. 行为改变（例如 drain 失败现在会阻断升级）必须写进 `CHANGELOG.md` 的 Changed 段。
5. 公开接口（CLI 参数、`--json` v1 字段、v1alpha1 配置）只增不删。

具体顺序与依赖见 [06-dev-plan](06-dev-plan.md) 的阶段 P0、P1。
