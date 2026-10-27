# Kurin Web UI 最终方案

> **状态：定稿 v1.0（取代两份调研稿作为实施依据）**
> **日期：2026-10-27**
> **代码基线：v0.4.0（`da2d4ba`）**
> **输入：** [`ui-architecture-rfc-gemini.md`](ui-architecture-rfc-gemini.md)（下称 G 稿）、[`ui-architecture-proposachatgptl.md`](ui-architecture-proposachatgptl.md)（下称 C 稿），以及对当前仓库源码的逐项核对。
>
> 本文所有 `serve` 命令、HTTP API、Cargo feature、数据库表都是**待实现的设计**；“现状”一节中的结论均附源码位置，可直接复核。

---

## 0. 一页结论

| 问题 | 决定 |
|---|---|
| 一体还是分离？ | **开发上分离、交付上一体。** 同仓库新增独立前端工程 `web/`（React SPA），构建产物 gzip 后经 `rust-embed` 嵌入 `kurin` 二进制，`kurin serve` 一条命令起服务。v1 **不**支持也不测试前后端分离部署，但 API 全部走同源相对路径 `/api/v1`，日后需要时零改动即可拆。 |
| 前端栈 | React + TypeScript + Vite，**Ant Design 5**，TanStack Query，React Router，CodeMirror 6（TOML 编辑）。不用 Monaco、React Flow、xterm.js、Redux、Tailwind。 |
| 后端栈 | **axum**（复用现有 tokio）、**SSE** 推送进度、**rusqlite（bundled）** 单文件 SQLite、**ts-rs** 从 Rust DTO 生成 TS 类型。 |
| 代码放哪 | `kurin` crate 内新增 `server/` 模块，由 Cargo feature `ui` 控制；**不**新建 crate（原因见 §4.1）。 |
| 构建影响 | 默认 `cargo build` / `cargo test` / `just check` **不需要 Node**、行为不变；只有 `--features ui`（发布构建）才需要 `web/dist`。 |
| 执行模型 | 进程内 tokio 后台任务 + SQLite 持久化 Operation 与事件；中断后**靠 kurin 已有的幂等任务重跑恢复**，不另建复杂核对状态机。 |
| 安全基线 | 默认只监听 `127.0.0.1`；**永远需要访问令牌**（防 CSRF / DNS rebinding）；远程访问首推 `ssh -L` 端口转发；非回环地址必须 TLS 或显式 `--allow-insecure-http`。 |
| 先做什么 | **M0 核心改造（无 UI）**：拆出 `ops` 服务层、去掉进程级全局状态、统一进度输出、可取消、跨进程集群锁。这些不做，UI 做不对。 |

---

## 1. 两份调研稿的取舍

两份稿方向一致（React SPA + axum + SSE + SQLite + 嵌入式单二进制），分歧在“做多少”和若干与代码不符的细节。

### 1.1 采纳

| 来源 | 内容 |
|---|---|
| G + C | React/TS/Vite SPA；axum + SSE；SQLite；`rust-embed` 嵌入；Cargo feature 隔离 UI |
| G | CodeMirror 6 而非 Monaco；静态资源**仅嵌入预压缩 gzip 版本**；同一集群变更互斥、新标签页可 attach 到正在进行的执行；ts-rs 生成类型 |
| C | Ant Design（运维表格/表单密集）；对外 DTO 与内部 `Cluster` 模型分离；Operation 持久化、与浏览器生命周期解耦；SSE 事件带序号可断线补读；“操作结果”与“观测健康”分开展示；状态采集走后台快照而非页面触发；“移除纳管”与“销毁集群”是两个不同动作；指出 `artifact.rs` 的 `OnceLock` 与 SSH 透明重试问题 |

### 1.2 否决或修正

| 来源 | 原提议 | 处理 | 理由 |
|---|---|---|---|
| G | `crates/kurin-core` / `kurin-cli` / `kurin-server` 多 crate 结构 | **修正** | 仓库实际只有 `kurin`（lib+bin）与 `kurin-config` 两个成员，没有 `kurin-core`。若新建 `kurin-server` 依赖 `kurin` lib，而 `kurin` bin 又要依赖 `kurin-server`，形成包级循环依赖，必须先把 bin 拆成新包，牵动 release 配方、安装脚本。改为 feature 门控模块（§4.1）。 |
| G | `lto = "thin"` | **否决** | 现有 `Cargo.toml` 已是 `lto = true`（fat）+ `strip` + `codegen-units = 1`，比 thin 更省体积，保持不动。 |
| G | 在 `kurin-config` 结构体上直接 `#[derive(TS)]` | **否决** | 内部 `Cluster` 含原始配置全文 `source` 及 SSH 密码、私钥口令、registry 密码（`kurin-config/src/model.rs:38-69`、`config.rs:231`）。类型应生成在专用 DTO 上。 |
| G | React Flow 画 DAG | **否决** | 计划是**严格串行的 11 个 stage、stage 内按节点并行**（`executor.rs` 头注释、`engine.rs` `Stage`），本质是“阶段 × 节点”矩阵，表格/网格比图布局更清晰，还省一个大依赖。 |
| G | xterm.js 实时终端日志 | **否决（v1）** | `Session::exec` 在命令结束后才返回完整 stdout/stderr（`engine.rs` `Session` trait），没有逐行流。v1 展示结构化事件 + 失败原因即可。 |
| G | “单文件约 15–20 MB”“增量 2–3.5 MB” | **不引用** | 未经测量。v0.4.0 的 x86_64 发布 tar.gz 为 3.6 MiB；Web 版体积在 M1 实测后再定预算（§9）。 |
| C | 一体与分离**两种部署同时正式支持**，四种构建变体（CLI / server / embed-ui / web） | **收缩** | 单人/小团队项目，多一种部署形态就多一套测试矩阵与反向代理文档。v1 只发一种：带 UI 的二进制；分离部署列为触发条件明确的后续项（§12）。 |
| C | utoipa + OpenAPI + openapi-typescript | **改用 ts-rs** | 前后端同仓同 tag 发布、无第三方客户端，OpenAPI 的价值（对外契约、多语言 SDK）暂时用不上，却要给每个 handler 写注解。将来开放 API 再加 utoipa。 |
| C | SQLx | **改用 rusqlite** | 单写者场景不需要异步连接池；SQLx 的编译期 SQL 校验需要 `DATABASE_URL` 或离线数据，增加构建前置条件。 |
| C | 服务端凭据库（加密存储、credential ID 绑定、轮换、主密钥） | **推迟到 M4** | v1 的信任模型与 CLI 一致：服务跑在管理机上，读取管理机上的 TOML 与私钥文件。只做“API 不回显秘密”（§6.6）。 |
| C | `cancel_requested / reconciling / needs_attention / resolution` 状态机、人工解决流程、幂等键 | **简化** | kurin 的核心设计就是每个任务 `check_state → apply → verify` 且 `Unknown ≠ failed`（`engine.rs` `Outcome`）。中断后最可靠的恢复手段就是**重跑同一操作**——已收敛步骤毫秒级跳过。所以 v1 只需 `interrupted` 一个终态 + “重新执行”按钮（§6.4）。 |
| C | Viewer/Operator/Admin RBAC、OIDC | **推迟到 M4** | v1 单令牌；所有写操作记录来源 IP 与会话，满足基本审计。 |

---

## 2. 现状核对（直接影响 UI 设计的事实）

| # | 事实 | 位置 | 影响 |
|---|---|---|---|
| F1 | `main.rs` 里仍有业务编排：`create cluster` 的 HA 警告、`add nodes` 的“探测运行版本 → 生成动态 manifest → 刷新 join 材料”整段逻辑 | `kurin/src/main.rs:330-530` | 必须下沉到库，CLI 和 Web 才能共用 |
| F2 | 下载 zone 与 manifest 覆盖是**进程级 `OnceLock`**，只能设置一次；约 12 处任务代码直接调用 `crate::artifact::zone()` | `artifact.rs:180-200`；`tasks/cni/*.rs`、`tasks/k3s.rs`、`tasks/images.rs` 等 | 常驻服务里第一个集群的 zone 会永久“粘住”，第二个集群用错镜像源 |
| F3 | 执行器的进度出口是 `emit: &mut impl FnMut(Event)`，事件已结构化、带 `--json` | `executor.rs:72`、`engine.rs:118-155` | create / add nodes 可直接接入 SSE |
| F4 | `upgrade`、`delete node/cluster`、`certs renew` 的进度是函数内 `eprintln!("[{node}] {mark} {step}")` | `upgrade.rs:35`、`teardown.rs:123`、`certs.rs:63` | 这些操作在 Web 里看不到进度，需要统一出口 |
| F5 | `upgrade::plan` 直接打印，不返回结构 | `upgrade.rs:435-470` | 升级计划需改为返回数据 |
| F6 | `Plan` 持有 `Arc<dyn Task>`，不可序列化；`render_tree` 输出纯文本 | `plan.rs:19`、`render.rs:8` | 需要一个 `PlanView` DTO |
| F7 | `status::collect` 只经第一个控制面节点；任一步失败整体返回 `Err`；每次都对所有控制面做证书检查 | `status.rs:183-235` | 仪表盘需容忍部分失败、证书检查要降频 |
| F8 | `status --json` 的 JSON 是在 `main.rs` 里临时拼的 | `main.rs:284-300` | `StatusReport` 应直接 `Serialize` |
| F9 | 本地状态均**按集群 name** 存放：运行状态 `~/.cache/kurin/runs/<name>.json`、join 材料 `~/.cache/kurin/clusters/<name>`、kubeconfig 合并进 `~/.kube/config` 的 `cluster-<name>` 上下文 | `state.rs:121`、`tasks/bootstrap.rs:19`、`tasks/post.rs:37-50` | 服务端以 **name 作为集群唯一主键**，禁止重名、v1 不支持改名 |
| F10 | 运行状态文件已支持 `--resume`（跳过已完成任务） | `executor.rs:44-60`、`state.rs` | 直接作为“重新执行”的实现 |
| F11 | `ConfigError` 实现了 miette `Diagnostic`，带源码 span | `kurin-config/src/error.rs`、`tests/snapshots/*error*` | 校验错误可以精确映射到编辑器的行列 |
| F12 | `ClusterConfig` 可序列化回 TOML（有 roundtrip 快照测试） | `kurin-config/tests/snapshots.rs` | 向导表单可由服务端渲染出规范 TOML |
| F13 | `--json` 事件类型在 v1.0 门禁中计划冻结 | `docs/v1.0-gates.md` §2 | 新增的 Web 事件不能改动现有 `Event`，只能外包一层信封 |
| F14 | CI `check.yml` 仅手动触发、跑在自托管 runner；发布由 `just release-musl`（x86_64 musl-gcc + aarch64 zig）完成 | `.github/workflows/`、`justfile` | 发布机需加 Node/pnpm；需验证 SQLite C 源码在 musl/zig 下的交叉编译 |

**顺带发现的、与 UI 无关但应修的问题**（单独提 issue，不阻塞 UI）：

- `upgrade.rs:394-403`：drain 命令以 `; true` 结尾，exit code 恒为 0，因此 `drain_ok` 恒为真，drain 失败不会阻断升级。
- `ssh.rs:354-370`：连接断开后会透明重试一次 `exec`，注释认为“传输层幂等”，但命令可能已在远端执行完、只是丢了返回。建议只读探测可重试，变更命令改为返回 `Outcome::Unknown` 交给引擎重新探测。

---

## 3. 总体架构

```text
                      浏览器（SPA，同源）
                          │  HTTPS/HTTP  /api/v1/*  + SSE
┌─────────────────────────┼──────────────────────────────────────────┐
│ kurin 二进制（--features ui）                                       │
│                         ▼                                          │
│  ┌───────────────── server/ (feature = "ui") ──────────────────┐   │
│  │ axum Router ── auth(token→cookie) ── 静态资源(rust-embed .gz) │   │
│  │     │                                                       │   │
│  │ OpRunner（tokio 任务，全局并发上限）   Collector（定时采集）  │   │
│  │     │            │ 事件 seq 持久化 → broadcast → SSE          │   │
│  │ Store（rusqlite，WAL，单写线程）                              │   │
│  └─────┼──────────────────────────────────────────────────────┘   │
│        ▼                                                          │
│  ops/（新：CLI 与 Web 共用的应用服务）                               │
│   create_cluster · add_nodes · delete_node · delete_cluster        │
│   upgrade · certs_check/renew · status · precheck · plan · artifact│
│        │  RunContext{zone, manifest, offline, cancel, sink, lock}  │
│        ▼                                                          │
│  现有核心：plan · executor · tasks · ssh · teardown · upgrade …     │
└────────┼───────────────────────────────────────────────────────────┘
         ▼ SSH
    目标节点（多个集群）

  CLI：main.rs → ops/ → 核心（终端渲染的 ProgressSink），与 Web 共享同一把集群锁
```

### 3.1 运行形态

```bash
kurin serve                               # 127.0.0.1:7080，数据目录 ~/.local/share/kurin
kurin serve --listen 0.0.0.0:7080 \
            --tls-cert c.pem --tls-key k.pem
kurin serve --data-dir /var/lib/kurin
```

- 首次启动生成访问令牌写入 `<data>/token`（0600），并在终端打印一次 `http://127.0.0.1:7080/#token=…` 登录链接。
- 推荐的远程用法：`ssh -L 7080:127.0.0.1:7080 mgmt-host`，服务本身不暴露到网络。
- 可选附带一个 systemd unit 样例（`docs/` 中给出，不由 kurin 自动安装）。

---

## 4. 代码组织与构建

### 4.1 为什么是 feature 模块而不是新 crate

`kurin` 包同时包含 lib 和 bin。新建 `kurin-server` 必然依赖 `kurin` lib；而 `serve` 子命令在 `kurin` bin 里，bin 又得依赖 `kurin-server`——Cargo 不允许这种包级循环。要拆就得把 bin 移到新的 `kurin-cli` 包，`just release-musl -p kurin`、安装脚本、二进制名都要跟着动。收益（编译隔离）用 feature 也能拿到：所有 web 依赖都是 `optional`，默认构建不编译。

```text
kurin/
  src/
    ops/            # 新：共享应用服务（M0）
      mod.rs  context.rs  progress.rs  lock.rs
      create.rs  nodes.rs  teardown.rs  upgrade.rs  certs.rs  status.rs  plan_view.rs
    server/         # 新：#[cfg(feature = "ui")]
      mod.rs  routes/  auth.rs  store/  runner.rs  collector.rs  sse.rs  assets.rs  dto.rs
    main.rs         # 只剩 argv 解析 + 选择 ProgressSink；cfg(feature="ui") 时多一个 Serve
web/                # 新：独立前端工程
  package.json  pnpm-lock.yaml  vite.config.ts
  src/
    api/generated/  # ts-rs 产物，入库，CI 校验无漂移
    api/            # fetch 封装、SSE 客户端
    pages/  components/  features/
```

### 4.2 Cargo feature

```toml
# kurin/Cargo.toml（示意）
[features]
default = []
ui = ["dep:axum", "dep:tower-http", "dep:rusqlite", "dep:ts-rs", "dep:axum-server", "tokio/net"]
```

- `cargo build` / `cargo test --workspace` / `just check`：与今天完全一致，不需要 Node。
- `cargo build --features ui`：`build.rs` 检查 `web/dist/index.html.gz` 是否存在，缺失时报清晰错误（“先运行 `just web-build`”），而不是让 `rust-embed` 报晦涩的错。
- `just check` 增加一条 `cargo clippy --features ui`，前提是放一个占位 `web/dist`（CI 中先 `just web-build`）。

### 4.3 just 配方（新增）

| 配方 | 作用 |
|---|---|
| `web-install` | `pnpm --dir web install --frozen-lockfile` |
| `web-build` | `pnpm --dir web build`，产物仅保留 `.gz`（构建后脚本压缩并删除原文件） |
| `web-dev` | Vite 开发服务器，`/api` 代理到 `127.0.0.1:7080` |
| `serve-dev` | `cargo run --features ui -- serve --dev`（`--dev` 时静态资源从磁盘读，不嵌入） |
| `ts-bindings` | `cargo test --features ui export_bindings` 生成 `web/src/api/generated/` |
| `release-musl` | 在现有步骤前加 `just web-build`，两个架构都加 `--features ui` |

发布产物保持现在的四个文件（两架构 tar.gz + sha256），**只是里面的二进制多了 `serve`**。不额外发布“纯 CLI”变体；需要的人可以 `cargo build --release` 自行构建。

---

## 5. 技术栈与理由

### 5.1 前端

| 选择 | 理由 | 被替代方案 |
|---|---|---|
| **React 19 + TypeScript + Vite** | 生态最大，运维控制台所需的表格/表单/虚拟列表都有成熟实现；Vite 产物是纯静态文件 | HTMX + MiniJinja（见 §5.3） |
| **Ant Design 5** | 节点列表的 `Form.List` 可编辑表格、`Steps` 向导、`Table` 筛选排序、`Descriptions`、`Result`、内置 `zh_CN` 语言包——一个运维面板 80% 的组件开箱即用；kubekey 用户群以中文为主 | shadcn/ui + Tailwind：要自己组装表格与表单，并多一套 CSS 工具链 |
| **TanStack Query** | 服务端状态缓存、轮询（总览页 15s 刷新快照）、mutation 状态 | Redux：不需要 |
| **React Router** | 列表 / 详情 / 操作页路由；用 hash 或 history 均可，服务端对非 `/api` 路径回退 `index.html` | — |
| **CodeMirror 6** + `@codemirror/legacy-modes` TOML | 体积小；lint 接口直接显示服务端返回的诊断 span | Monaco：体积数 MB |
| 系统字体栈，不打包字体 | 控制体积，离线可用 | — |

### 5.2 后端

| 选择 | 理由 |
|---|---|
| **axum + tower-http** | 与现有 tokio 运行时同源；原生 SSE；`tower-http` 提供压缩头、超时、追踪 |
| **SSE**（不是 WebSocket） | 进度是单向推送；SSE 自带 `Last-Event-ID` 重连语义；不需要交互式终端 |
| **rusqlite（`bundled`）+ WAL** | 单写者、关系查询（按集群/状态筛选操作）、单文件易备份。用一个专用写线程，读用独立连接。**M0 末尾做交叉编译验证**：x86_64 musl-gcc 与 aarch64 zig 下编译 bundled SQLite；若受阻，回退 `redb`（纯 Rust KV）并把筛选放到内存 |
| **ts-rs** | 只在 `server/dto.rs` 的 DTO 上派生；`cargo test` 导出，CI 用 `git diff --exit-code` 防漂移 |
| **axum-server + rustls** | 仅在 `--tls-*` 时启用；russh 已引入 ring/aws-lc 系密码库，增量有限 |
| **toml_edit** | 服务端对 TOML 做“添加节点”等结构化修改时保留用户注释和格式（`toml` 0.8 已间接依赖它） |

### 5.3 为什么不选 HTMX，也不选 Rust/WASM 前端

- **HTMX + MiniJinja**（G 稿方案二）：零 Node 工具链确实诱人，但本需求的核心交互——按角色联动字段的多节点表格、TOML 编辑器 + 行内诊断、“阶段 × 节点”实时进度网格、多集群自动刷新总览——在 HTMX 下会退化成大量手写 JS，最终维护成本高于一个规范的 SPA。Node 只在发布构建与前端开发时需要，日常 `cargo` 工作流不受影响（§4.2），这个代价可以接受。
- **Leptos / Dioxus**：单语言、可共享类型，但企业级表格/表单组件生态远不如 antd，WASM 产物 1–3 MB 起，迭代速度与可招募贡献者都更差。

---

## 6. 后端设计

### 6.1 M0：共享服务层 `ops/`

每个 CLI 子命令对应一个 `ops` 函数，签名统一为：

```rust
pub async fn create_cluster(ctx: &RunContext, cluster: &Cluster, opts: CreateOpts) -> Result<()>;
pub async fn add_nodes(ctx: &RunContext, cluster: &Cluster) -> Result<()>;      // 含 F1 的动态 manifest 逻辑
pub async fn delete_node(ctx: &RunContext, cluster: &Cluster, node: &str) -> Result<()>;
pub async fn delete_cluster(ctx: &RunContext, cluster: &Cluster) -> Result<()>;
pub async fn upgrade(ctx: &RunContext, cluster: &Cluster, target: &Version, batch: usize) -> Result<()>;
pub async fn upgrade_plan(cluster: &Cluster, target: &Version, pool: &Arc<dyn SessionPool>) -> Result<UpgradePlan>; // F5
pub fn plan_view(plan: &Plan) -> PlanView;                                       // F6
pub async fn status(cluster: &Cluster, pool: &Arc<dyn SessionPool>, opts: StatusOpts) -> Result<StatusReport>; // F7/F8
```

`main.rs` 变成：解析参数 → 构造 `RunContext`（终端 sink）→ 调 `ops::*`。**CLI 的 stdout/stderr 输出必须逐字节不变**，用 insta 快照测试锁定（沿用 `kurin-config/tests/snapshots.rs` 的做法，配合现有 MockSession）。

### 6.2 `RunContext`：取代进程级全局状态

```rust
pub struct RunContext {
    pub zone: Zone,                                   // 取代 artifact::ZONE OnceLock
    pub manifest_override: Option<Arc<ArtifactManifest>>, // 取代 MANIFEST_OVERRIDE
    pub cancel: CancellationToken,
    pub sink: Arc<dyn ProgressSink>,
}
```

- `Plan::build_with_pool` 接收 `&RunContext`，构造任务时把 `Arc<RunContext>`（或其中的 zone/manifest）交给任务；约 12 处 `crate::artifact::zone()` 调用改为读任务自身字段。
- 不能用 `tokio::task_local!`：执行器通过 `JoinSet::spawn` 派生任务，task-local 不会传播。
- 验收：同一进程内用两个不同 zone 的 `RunContext` 构建两份计划，断言渲染出的镜像地址不同。

### 6.3 统一进度出口 `ProgressSink`

```rust
pub trait ProgressSink: Send + Sync {
    fn task(&self, ev: Event);          // 现有执行器事件，原样不动（F13）
    fn step(&self, ev: StepEvent);      // 新：upgrade/teardown/certs 的 "[node] ✓ step: detail"
    fn note(&self, level: Level, msg: String); // 警告类输出，如 "even control-plane count"
}
pub struct StepEvent { pub node: String, pub step: String, pub ok: bool, pub detail: String }
```

- CLI 实现：`task` 按现在的 `skip/done/ok/FAIL` 格式或 `--json` 行输出；`step` 输出与现有 `report()` 完全相同的字符串。
- 服务端实现：写入事件表并广播。
- 执行器内部的 `emit` 回调签名不改，由 `ops` 层把 `sink.task` 适配进去。

### 6.4 取消与中断

- **取消**：`CancellationToken` 在执行器**每个 stage 开始前**、teardown/upgrade **每个节点开始前**检查；正在执行的远端命令永远等它结束。UI 文案明确“将在当前阶段结束后停止，已做的变更不会回滚”。
- **服务重启**：启动时把所有 `running/queued` 操作标记为 `interrupted`。集群锁是 `flock`，进程死亡即释放。
- **恢复**：操作详情页对 `failed / cancelled / interrupted` 提供“重新执行”，创建一个 `retry_of` 指向原操作的新操作：
  - create / add nodes：以 `resume = true` 运行（F10），每个任务仍先 `check_state`，已收敛的直接跳过；
  - upgrade：已在目标版本的节点本就会被跳过（`upgrade.rs:381-391`）；
  - delete：teardown 各步骤本就按“尽力清理”编写（drain 等命令以 `; true` 结尾），可重复执行。
- 这就是 kurin “幂等 + Unknown ≠ failed” 设计带来的红利，不需要 C 稿中的人工核对/解锁流程。

### 6.5 互斥：跨进程集群锁

- `ops::lock::ClusterLock`：对 `~/.cache/kurin/runs/<name>.lock` 加独占 `flock`（`std::fs::File::try_lock`，Rust ≥ 1.89；否则用 `fs4`）。
- **CLI 的所有变更命令和服务端操作都获取同一把锁**——这样同一管理机上 CLI 与 Web 不会同时改同一集群。拿不到锁时 CLI 报错退出、API 返回 409 并附正在运行的操作 ID。
- 数据库层再加一道：`operations` 表上对 `(cluster) WHERE state IN ('queued','running')` 建部分唯一索引。
- 只读操作（status、precheck、plan、certs check）不加锁，走全局读信号量（默认 4）。
- 不同集群的变更并发上限默认 2（`--max-concurrent-ops`）。
- 跨管理机的竞争不在 v1 解决，文档写明“一个集群只由一台管理机管理”。

### 6.6 集群与配置存储

- **集群唯一键 = `name`**（F9），API 中路径即 `/clusters/{name}`，拒绝重名。
- 配置以**不可变修订**存入 SQLite；当前修订同时导出到 `<data>/clusters/<name>/cluster.toml`（0600），方便用户随时用 CLI 操作同一集群。该文件是导出镜像，说明中写明“直接编辑会被下次保存覆盖，请用导入”。
- 保存时携带 `base_rev`，不匹配返回 409（防多标签页互相覆盖）。
- **秘密字段**（`ssh.auth.password`、`passphrase`、`registry.password`，含节点级覆盖）：
  - 存储：v1 明文存于 0600 的数据库 / 文件，**与 CLI 读取的 `cluster.toml` 同等信任级别**，文档明示；
  - 读取：`GET config` 返回的 TOML 中这些值替换为哨兵 `"<kurin:secret>"`；
  - 写回：提交的 TOML 中出现哨兵时，服务端用 toml_edit 从上一修订取回原值，因此“读出来改一改再存回去”不会丢密码；
  - 事件、错误、日志输出前统一过一遍已知秘密值的替换。
- `key-path`、`offline-dir` 都是**服务端所在机器的路径**。表单上标明“管理机路径”，校验接口检查文件存在且可读；v1 不支持上传私钥。

### 6.7 数据库表（v1）

```sql
CREATE TABLE clusters (
  name        TEXT PRIMARY KEY,
  lifecycle   TEXT NOT NULL,          -- draft | provisioning | active | deleting | deleted | archived
  current_rev INTEGER NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE TABLE config_revisions (
  cluster TEXT NOT NULL REFERENCES clusters(name),
  rev     INTEGER NOT NULL,
  toml    TEXT NOT NULL,
  sha256  TEXT NOT NULL,
  created_at TEXT NOT NULL,
  note    TEXT,
  PRIMARY KEY (cluster, rev)
);
CREATE TABLE operations (
  id          TEXT PRIMARY KEY,       -- ULID
  cluster     TEXT REFERENCES clusters(name),   -- artifact 构建等可为空
  kind        TEXT NOT NULL,          -- create | add-nodes | delete-node | delete-cluster | upgrade | certs-renew | precheck | artifact
  state       TEXT NOT NULL,          -- queued | running | succeeded | failed | cancelled | interrupted
  config_rev  INTEGER,
  params      TEXT NOT NULL,          -- JSON：目标版本、节点名、batch…
  retry_of    TEXT REFERENCES operations(id),
  actor       TEXT NOT NULL,          -- 会话 ID + 来源 IP
  error       TEXT,
  created_at TEXT NOT NULL, started_at TEXT, finished_at TEXT
);
CREATE UNIQUE INDEX one_active_op ON operations(cluster) WHERE state IN ('queued','running');
CREATE TABLE op_events (
  op_id   TEXT NOT NULL REFERENCES operations(id),
  seq     INTEGER NOT NULL,
  ts      TEXT NOT NULL,
  kind    TEXT NOT NULL,              -- task | step | note | state
  payload TEXT NOT NULL,              -- 已脱敏 JSON；task 类即现有 --json 事件原文
  PRIMARY KEY (op_id, seq)
);
CREATE TABLE snapshots (
  cluster       TEXT PRIMARY KEY REFERENCES clusters(name),
  attempted_at  TEXT NOT NULL,
  observed_at   TEXT,                 -- 最近一次成功采集
  report        TEXT,                 -- 最近一次成功的 StatusReport JSON
  error         TEXT,                 -- 最近一次失败原因（成功时清空）
  certs_at      TEXT, cert_min_days INTEGER
);
```

迁移用 `PRAGMA user_version` 递增脚本，不引入迁移框架。一次 create 操作约数百到一千余条事件，直接存表即可，无需日志文件。

### 6.8 事件与 SSE

```json
{"seq": 42, "ts": "2026-10-27T04:00:00Z", "kind": "task",
 "payload": {"type": "task-done", "id": "prepare/sysctl@worker01", "node": "worker01", "stage": "prepare", "ms": 812}}
```

- 写入流程：执行器 → `mpsc` → 单写线程**先落库、再广播**（`tokio::sync::broadcast`，容量 1024）。
- `GET /api/v1/operations/{id}/events`：先订阅广播，再按 `Last-Event-ID`（或 `?after=`）从库里补读，按 `seq` 去重后推送——无漏、无重。
- 广播落后（`Lagged`）时断开该订阅，客户端自动重连并从库补读；执行永远不等浏览器。
- 15 秒心跳注释行；操作进入终态后发送 `state` 事件并关闭流。
- 浏览器原生 `EventSource` 会自动带同源 cookie，认证无需在 URL 中放令牌。

### 6.9 多集群状态采集（Collector）

- 对 `lifecycle = active` 的集群，每 60 秒（加 ±10% 抖动）采集一次核心状态，单次超时 30 秒，全局并发 4。
- `status` 拆成两部分（F7）：核心（nodes / pods / etcd）每轮采；证书最早到期天数每 6 小时采一次。
- 采集失败**不覆盖** `report`，只更新 `error` 与 `attempted_at`；UI 据此显示“不可达 / 数据已过期（>180s）”，绝不把旧数据显示成当前健康。
- 首个控制面不可达时，依次尝试其余控制面（对 `status::collect` 的小改动），全部失败才判为不可达。
- “刷新”按钮只是把该集群插到下一轮队首，并做 10 秒去重，多个标签页不会放大 SSH 扫描。
- 操作运行期间暂停该集群采集，结束后立即补采一次。

### 6.10 认证与 Web 安全

- 令牌换会话：前端从 URL hash 取令牌 → `POST /api/v1/session` → `HttpOnly; SameSite=Strict` cookie，随即清掉地址栏 hash。
- 所有 `/api/v1/*`（除 `/session`、`/healthz`）都校验会话——**即使监听在 127.0.0.1 也如此**，防止任意网页对本机端口发起 CSRF。
- 校验 `Host` 头属于允许列表（默认 `127.0.0.1:<port>`、`localhost:<port>` 及 `--public-host`），防 DNS rebinding。
- 变更请求要求 `Content-Type: application/json`（跨站表单无法伪造）。
- 监听非回环地址时，必须提供 `--tls-cert/--tls-key`，或显式 `--allow-insecure-http`（启动时打印警告）。
- 破坏性操作（delete node / delete cluster）要求请求体中带 `confirm: "<集群名>"`，服务端比对。
- `kurin serve --rotate-token` 轮换令牌并使所有会话失效。

---

## 7. API（v1）

所有路径同源、前缀 `/api/v1`，JSON 字段 snake_case。长任务一律 `202 + operation`。

| 方法与路径 | 用途 | 里程碑 |
|---|---|---|
| `POST /session` / `DELETE /session` | 登录 / 登出 | M1 |
| `GET /meta` | kurin 版本、组件版本表（`create config --print-versions`）、能力表 | M1 |
| `GET /clusters` | 列表 + 最近快照 + 当前活动操作 | M1 |
| `POST /clusters` | 登记：TOML 文本或向导 DTO；只保存，不安装 | M1 |
| `GET /clusters/{name}` | 详情（配置摘要、节点期望 vs 观测、快照） | M1 |
| `GET /clusters/{name}/config` | 当前修订 TOML（秘密已替换为哨兵）+ rev | M1 |
| `PUT /clusters/{name}/config` | 保存新修订，带 `base_rev` | M1 |
| `GET /clusters/{name}/config/revisions[/{rev}]` | 修订历史 / 某版内容 | M3 |
| `POST /configs/validate` | 校验，返回诊断 `{message, help, labels:[{offset,len,text}]}` | M1 |
| `POST /configs/render` | 向导 DTO → 规范 TOML | M2 |
| `POST /clusters/{name}/plan` | 生成 `PlanView`；`?check=true` 时做在线漂移探测（作为只读操作执行） | M1 |
| `POST /clusters/{name}/refresh` | 请求立即采集 | M1 |
| `POST /clusters/{name}/certs/check` | 证书报告 | M1 |
| `POST /clusters/{name}/operations` | 提交操作：`{kind, params, confirm?}` → 202 | M1（precheck）/ M2 / M3 |
| `POST /clusters/{name}/archive` | 移除纳管（不触碰远端）；有活动操作时 409 | M2 |
| `GET /operations?cluster=&state=&kind=` | 操作列表，分页 | M2 |
| `GET /operations/{id}` | 操作快照（状态 + 按阶段/节点聚合的进度） | M1 |
| `GET /operations/{id}/events` | SSE，支持 `Last-Event-ID` | M1 |
| `POST /operations/{id}/cancel` | 请求取消 | M2 |
| `POST /operations/{id}/retry` | 重新执行（新操作，`retry_of`） | M2 |
| `GET /healthz` | 存活 | M1 |

`PlanView`：

```json
{"stages": [{"stage": "prepare", "tasks": [
  {"id": "prepare/sysctl@worker01", "node": "worker01", "describe": "...", "drift": "would-change: net.ipv4.ip_forward=0"}]}]}
```

错误码：400 语法、401 未登录、404、409（rev 冲突 / 集群已有活动操作 / 锁被 CLI 占用 / 重名）、422（校验失败，附诊断）、503（正在关闭）。

---

## 8. 前端设计

### 8.1 页面

| 页面 | 内容 |
|---|---|
| **集群总览** `/` | 表格：名称、发行版（kubeadm/k3s）、K8s 版本、节点 Ready x/y、健康徽标（健康 / 异常 / 不可达 / 过期）、证书最早到期、最近观测时间、当前操作（可点进）。15 秒轮询快照。 |
| **新建集群** `/clusters/new` | 两个入口：**向导** 或 **导入 TOML**（粘贴/选择文件）。向导步骤：基本信息（名称、发行版、版本、zone、离线目录）→ 网络与 CNI → HA（kube-vip / external-endpoint / kurin-lb）→ 节点（可编辑表格：名称、地址、内网地址、角色、可选 SSH 覆盖）→ SSH 认证 → 可选组件（registry / storage）→ TOML 预览 → 校验 + precheck → 计划 → 确认执行。 |
| **集群详情** `/clusters/:name` | Tab：概览（节点表、未运行 Pod、etcd、证书）· 节点（配置期望与观测合并展示；“添加节点”“删除节点”入口）· 配置（CodeMirror + 行内诊断 + 保存）· 操作历史 · 危险区（销毁集群、移除纳管，两者分开、文案说明差异）。 |
| **操作详情** `/operations/:id` | 头部状态与耗时；**阶段 × 节点进度网格**（任务 ID 格式 `<stage>/<kind>@<node>` 直接聚合，格子颜色：待执行/执行中/跳过/完成/已核对/失败）；step 类事件按节点分组的时间线；失败原因高亮；取消 / 重新执行按钮。断线自动续读。 |
| **操作列表** `/operations` | 按集群、类型、状态筛选。 |
| **关于** `/about` | kurin 版本、组件版本表、能力表、数据目录。 |

### 8.2 表单与 TOML：单向而非双向同步

双向绑定“表单 ⇄ TOML”实现复杂且容易丢注释。v1 规则：

1. **创建时**：向导维护一个 DTO，每一步调用 `POST /configs/render` 由 Rust 渲染出规范 TOML（F12）并展示；用户可在最后一步切换到 TOML 手改，**一旦手改就以 TOML 为准**，不再回填表单。
2. **创建后**：TOML 编辑器是主编辑界面；常用的结构化动作（添加节点）由服务端用 toml_edit 修改 TOML 后生成新修订，保留注释。
3. 校验永远以 `POST /configs/validate` 为准；`ConfigError` 的 span（F11）转换为 CodeMirror lint 标注。

### 8.3 能力约束来自后端

`GET /meta` 返回能力表（由 Rust 中与校验相邻的常量生成），例如：k3s 只支持 `flannel | none`；kube-ovn 离线不可用；k3s × kurin-lb 暂不支持；K8s ≥ 1.30。前端只用它**禁用选项并解释原因**，不在 TS 中复刻校验逻辑，最终结论仍以 validate 接口为准。

### 8.4 其他约定

- 文案集中在一个字典文件，首版 `zh-CN`；antd 使用 `zh_CN` locale；`en` 在 M3 补。
- 不使用任何 CDN；所有资源打包进 `dist`。
- 路由懒加载 CodeMirror（只在配置页加载）。

---

## 9. 体积与资源

- 静态资源只嵌入 `.gz` 版本，响应带 `Content-Encoding: gzip`，运行时零压缩开销；不同时嵌入原文件/Brotli 副本。
- 预期增量来源：axum/hyper/tower、bundled SQLite、rustls 服务端、前端 gz 资源。**M1 结束时实测**两架构 release 二进制大小，写入本文档并设为 CI 告警阈值（建议增长不超过实测值 +10%）。
- 发布配方打印 `web/dist` 总量与二进制大小，便于追踪。
- 运行内存：广播通道有界、事件分页读取、前端事件列表虚拟滚动。

---

## 10. CLI 覆盖矩阵

| CLI | UI 形态 | 执行方式 | 里程碑 |
|---|---|---|---|
| `create config` | 向导 / 示例模板 / 导入 | `configs/render`、`clusters` | M1（导入）/ M2（向导） |
| `create config --print-versions` | 关于页、向导版本选择 | `meta` | M1 |
| `create cluster --dry-run [--check]` | 计划页（阶段 × 任务表，漂移标注） | `plan` | M1 |
| `precheck` | 逐节点检查结果表 | 只读操作 | M1 |
| `status` | 总览 + 详情概览 | Collector 快照 | M1 |
| `certs check` | 详情证书 Tab | 只读 | M1 |
| `create cluster [--resume]` | 向导最后一步 / 详情“安装” | 操作 `create` | M2 |
| `add nodes` | 详情“添加节点” | 操作 `add-nodes` | M2 |
| `delete node` | 节点行“删除” + 确认 | 操作 `delete-node` | M2 |
| `delete cluster` | 危险区“销毁集群” + 输入集群名确认 | 操作 `delete-cluster` | M2 |
| `upgrade [--dry-run] --with-kubernetes --batch` | 升级对话框：目标版本、批大小、计划预览 | 操作 `upgrade` | M3 |
| `certs renew [--force]` | 证书 Tab “续期” | 操作 `certs-renew` | M3 |
| `artifact --out --arch --with-kubernetes` | 制品页：在管理机上构建离线包 | 操作 `artifact`（无集群） | M3 |
| （新）移除纳管 / 重新执行 / 取消 / 历史 | 详情、操作页 | 服务端能力 | M2 |

M1 + M2 即满足“创建、销毁、多集群状态展示”的核心需求。

---

## 11. 实施里程碑与验收

### M0 — 核心改造（无 UI，可单独合并发布）

1. 新建 `ops/`，把 `main.rs` 的编排逻辑下沉（F1）。
2. `RunContext` 取代 `OnceLock`（F2）。
3. `ProgressSink` + `StepEvent`，改造 upgrade / teardown / certs 输出（F4）；`upgrade_plan` 返回结构（F5）。
4. `CancellationToken` 接入执行器与 teardown/upgrade 循环。
5. `ClusterLock`，CLI 变更命令全部加锁。
6. `PlanView`、`StatusReport: Serialize`、status 拆分证书与多控制面回退（F6–F8）。
7. 交叉编译 spike：`rusqlite` bundled 在 x86_64 musl 与 aarch64 zig 下可构建。

**验收**：`just check` 绿；tier0 / tier1 / tier2 e2e 通过且 CLI 输出与改造前一致（快照测试）；双 zone 单进程单测通过；两个终端同时 `kurin add nodes` 同一集群，第二个立即报锁冲突。

### M1 — `kurin serve` 与只读能力

axum 服务、令牌会话、Host 校验、Store 与迁移、静态资源嵌入、集群登记/导入、TOML 编辑与校验、计划页、precheck、Collector 与总览/详情、证书检查、操作详情页 + SSE（以 precheck 验证事件链路）、`just` 配方与发布流程。

**验收**：发布二进制 `kurin serve` 在断网环境打开页面正常；导入 3 个集群（含一个不可达），总览中不可达集群显示“不可达”且不影响其他集群；关掉页面再打开，precheck 历史仍在；未带会话的请求全部 401；体积实测写入 §9。

### M2 — 生命周期闭环

create / add nodes / delete node / delete cluster 操作、向导、取消、重新执行、中断标记、移除纳管、操作列表。

**验收**（在现有 multipass 环境手工执行，记录到 `docs/progress.md`）：
- 浏览器完成：tier1 单节点创建 → 状态正常 → tier2 添加 worker → 删除 worker → 销毁集群；
- 创建中途关闭浏览器，操作继续完成；重新打开后进度可完整回放；
- 创建中途 `kill` 服务 → 重启后操作为 `interrupted` → “重新执行”成功且已完成任务被跳过；
- Web 正在创建时，CLI 对同一集群执行 `add nodes` 被锁拒绝；
- 数据库、事件、API 响应中搜索不到配置里的密码明文（除 0600 的修订存储本身）。

### M3 — 运维扩展

upgrade、certs renew、artifact 构建、配置修订对比、kubeconfig 下载（需二次确认并记审计）、英文文案。

### M4 — 按触发条件再做（§12）

---

## 12. 明确不做与后续触发条件

**v1 不做**：通用 K8s 资源管理（Deployment/Service 编辑等）、监控指标与告警、浏览器内 SSH 终端、逐行实时命令输出、多用户 RBAC/SSO、服务端凭据库、私钥上传、前后端分离部署、多实例服务、远程 CLI 模式、纳管非 kurin 创建的集群。

| 能力 | 触发条件 |
|---|---|
| 前后端分离部署（Nginx + server-only） | 有企业网关/门户集成需求，或 UI 迭代频率显著高于后端 |
| OpenAPI（utoipa） | 出现第三方 API 调用方 |
| RBAC / OIDC | 多人共用一个服务实例 |
| 服务端凭据库 | 需要在不落盘私钥文件的前提下管理集群，或多人共享时需隔离凭据 |
| 逐行日志 / xterm | `Session` 增加流式 exec 接口之后 |
| 远程 CLI 模式（CLI 把操作提交给 server） | 多台管理机协作管理同一批集群 |
| PostgreSQL / 外部队列 | 多实例服务或测得 SQLite 写入瓶颈 |

---

## 13. 决策记录

| # | 决策 | 结论 |
|---|---|---|
| D1 | 前端工程 | 同仓 `web/`，独立构建 |
| D2 | 交付 | 嵌入单二进制，`kurin serve`；v1 不支持分离部署 |
| D3 | 前端栈 | React + TS + Vite + Ant Design 5 + TanStack Query + React Router + CodeMirror 6 |
| D4 | 后端栈 | axum + SSE + rusqlite（WAL）+ ts-rs + toml_edit；TLS 用 rustls（可选） |
| D5 | 代码结构 | `kurin` crate 内 `ops/`（共享）+ `server/`（feature `ui`），不新建 crate |
| D6 | 默认构建 | 不含 UI、无需 Node；发布构建含 UI |
| D7 | 集群身份 | 集群 `name` 唯一，v1 不支持改名 |
| D8 | 恢复语义 | 中断 → `interrupted` → 幂等重跑；不做回滚、不做人工核对流程 |
| D9 | 互斥 | 跨进程 flock（CLI 与 Web 共用）+ 数据库部分唯一索引 |
| D10 | 秘密 | 存储与 CLI 同信任级（0600）；API/事件不回显；哨兵回写 |
| D11 | 安全 | 始终需要令牌；回环默认；非回环需 TLS 或显式放行 |
| D12 | 事件兼容 | 现有 `--json` 事件不改，Web 事件外包信封 |
| D13 | 首个可用版本 | M0 + M1 + M2 |
