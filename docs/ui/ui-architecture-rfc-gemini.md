**`kurin` Web UI 架构设计与技术选型方案文档**。你可以直接将其保存为 `docs/ui-architecture-rfc.md` 放入你的项目仓库中，作为后续开发的蓝图。

---

```markdown
# Kurin Web UI 架构设计与技术选型方案 (RFC)

## 1. 背景与目标

`kurin` 目前已经具备了极其扎实的底层架构：包括清晰解耦的 `Planner`（编译期执行计划生成）与 `Executor`（运行期三段式幂等引擎 `check_state` -> `apply` -> `verify`），以及强类型的 `kurin-config` TOML 配置模型。

为了降低用户的心智负担、提供直观的多集群生命周期管理能力，现计划为 `kurin` 引入图形化 Web 界面（UI）。核心设计目标如下：

1. **单二进制分发（Single Binary）**：坚守运维工具的极简部署原则，不依赖外部 Nginx、Node.js 或独立数据库进程，通过 `kurin ui --port 8080` 一键启动。
2. **核心功能全覆盖**：支持多集群状态大盘、可视化向导创建集群、TOML 配置在线编辑、`--dry-run` 执行计划（DAG）预览、实时安装日志流展示、节点扩缩容、证书检查与集群销毁。
3. **极致的体积控制**：将 Web 服务与前端静态资源打包进二进制后，整体体积增量需严格控制在 **1.5MB ~ 4MB** 以内。
4. **复用现有核心能力**：Web 后端直接将 `kurin-core`、`kurin-config` 作为 Library 调用，保证 CLI 与 UI 行为 100% 一致。

---

## 2. 核心设计理念

无论选择何种前端技术路线，整体交付架构均采用 **“开发期模块化分离，编译期静态内嵌”** 的模式：

```text
+-------------------------------------------------------------------+
|                        kurin (Single Binary)                      |
|                                                                   |
|  +---------------+     +---------------------------------------+  |
|  |   kurin-cli   |     |             kurin-server              |  |
|  |  (Clap CLI)   |     |  +---------------+  +--------------+  |  |
|  +-------+-------+     |  |  Axum Router  |  |  rust-embed  |  |  |
|          |             |  | (REST / SSE)  |  | (UI Assets)  |  |  |
|          |             |  +-------+-------+  +--------------+  |  |
|          |             +----------|----------------------------+  |
|          v                        v                               |
|  +-------------------------------------------------------------+  |
|  |                         kurin-core                          |  |
|  |    Planner (DAG / Dry-run)  <--->  Executor (Idempotent)    |  |
|  +-------------------------------------------------------------+  |
|                                   |                               |
+-----------------------------------|-------------------------------+
                                    v
                     +------------------------------+
                     |  ~/.kurin/state.db (SQLite)  |
                     |  (多集群配置、状态与历史日志)  |
                     +------------------------------+
```

---

## 3. 技术选型方案对比

针对前端与服务端交互方式，提供两套成熟的技术方案供抉择：

### 方案一：现代云管平台架构 —— `Axum` + `React SPA`（推荐）

采用前后端职责分离的现代 Web 架构。在仓库中建立独立的 `ui/` 目录进行 React 单页应用（SPA）开发，构建产物经预压缩后通过 `rust-embed` 嵌入 `Axum` 后端。

#### 技术栈清单
* **Rust 后端层 (`crates/kurin-server`)**：
  * **Web 框架**：`axum` + `tower-http`（与现有 `tokio` 运行时无缝集成）。
  * **实时通信**：`SSE (Server-Sent Events)` 或 `WebSocket`（实时推送节点安装进度与 SSH 终端日志流）。
  * **持久化存储**：`redb`（纯 Rust 嵌入式 KV 库，体积 <500KB）或 `rusqlite`（开启 `bundled` 特性）。
  * **前后端契约同步**：`ts-rs`（在 Rust 的 `kurin-config` 结构体上标注 `#[derive(TS)]`，自动生成前端 TypeScript 接口定义，确保类型 100% 同步）。
* **前端层 (`ui/`)**：
  * **核心底座**：`React 19` + `Vite` + `TypeScript` + `TanStack Query` (状态管理与轮询)。
  * **UI 组件库**：`shadcn/ui` + `Tailwind CSS` + `Lucide Icons`（现代极客风格，按需打包体积最小）。
  * **代码编辑器**：`CodeMirror 6` (`@uiw/react-codemirror`) —— **坚决不用 Monaco Editor**，CodeMirror 6 仅需 `<50KB` 即可实现丝滑的 TOML 语法高亮与在线编辑。
  * **拓扑与计划可视化**：`@xyflow/react` (React Flow) —— 用于将 `Planner` 生成的阶段与节点拓扑渲染为可视化 DAG 图。
  * **终端日志组件**：`@xterm/xterm` (Xterm.js) —— 完美渲染带 ANSI 颜色的远端命令输出。

#### 优缺点评估
* **优点**：
  1. **交互体验天花板**：能够非常轻松地实现“左侧动态表单添加节点/配置 HA，右侧实时双向同步预览 `cluster.toml`”的丝滑体验。
  2. **可视化能力极强**：拥有极其丰富的图表、拓扑图（React Flow）和终端（Xterm）生态，产品质感可直接对标商业级云原生管理平台。
  3. **类型安全闭环**：借助 `ts-rs`，Rust 配置结构体的任何改动都会在前端编译期立刻体现。
* **缺点**：
  1. **工具链引入**：仓库需要引入 Node.js (`pnpm`) 构建工具链，CI/CD 编译流程需增加一步 `pnpm build`。
  2. **开发工作量中等**：需要编写前后端 API 对接代码。

---

### 方案二：极简全栈一体架构 —— `Axum` + `MiniJinja` + `HTMX`

彻底抛弃现代前端构建工具链（无 Node.js、无 `package.json`、无 TypeScript），复用 `kurin` 现有的 `MiniJinja` 模板引擎，在服务端直接渲染 HTML 片段，配合轻量级的 `HTMX` 实现局部刷新与实时日志流。

#### 技术栈清单
* **Rust 后端与渲染层**：
  * **Web 框架**：`axum`。
  * **模板引擎**：直接复用项目中已有的 `minijinja`，在 `templates/ui/` 下编写 `.html.j2` 模板。
  * **持久化存储**：`redb` 或 `rusqlite`。
* **前端脚本层（纯静态引入，零构建）**：
  * **交互核心**：`htmx.min.js` (~14KB) + `htmx-ext-sse`（直接对接 Axum 的 SSE 接口追加日志 DOM）。
  * **轻量客户端状态**：`Alpine.js` (~15KB，处理简单的弹窗、Tab 切换）。
  * **样式库**：`Tailwind CSS` (通过独立 CLI 生成精简 CSS) + `DaisyUI`。

#### 优缺点评估
* **优点**：
  1. **100% 纯 Rust 血统**：零前端构建工具链，无需维护复杂的 `node_modules`，`cargo build` 一步到位。
  2. **体积与内存极致轻量**：整个 UI 资源加起来不到 `100KB`，开发初期上手极快，一两个周末即可跑通核心闭环。
* **缺点**：
  1. **复杂交互能力受限**：在实现“动态增删多节点表单联动”、“双向绑定的 TOML 编辑器”以及“复杂的交互式 DAG 拓扑图”时，代码会变得非常别扭且难以维护。
  2. **界面天花板较低**：更偏向传统的后台工具面板，较难做出酷炫的现代化云管平台动效。

---

### 方案横向对比总结表

| 评估维度                    | 方案一：Axum + React SPA (推荐)                 | 方案二：Axum + MiniJinja + HTMX   |
| :-------------------------- | :---------------------------------------------- | :-------------------------------- |
| **用户体验与交互上限**      | ⭐⭐⭐⭐⭐ (媲美商业级云原生平台)                    | ⭐⭐⭐ (传统实用的运维面板)          |
| **复杂动态表单 / 拓扑图**   | 极易实现 (React 状态管理 + React Flow)          | 较难实现 (需手写大量原生 JS 弥补) |
| **构建工具链复杂度**        | 需引入 `pnpm` + `Vite` (可通过 `justfile` 封装) | 零额外工具链，纯 `cargo` 编译     |
| **二进制体积增量 (优化后)** | **约 +2MB ~ 3.5MB**                             | **约 +1MB ~ 1.5MB**               |
| **适用定位**                | 打造惊艳的标杆级开源作品 / 长期演进             | 快速验证功能 / 极度排斥前端工具链 |

---

## 4. `kurin` 后端核心改造设计（通用）

为了支撑 Web UI 的多集群管理与异步执行能力，需要对现有 `kurin` 核心模块进行以下 3 项非侵入式演进：

### 4.1 抽象 `EventSink`（统一 CLI 与 UI 的输出流）
目前 CLI 执行时的状态与日志直接输出至终端。需将 `Executor` 的执行反馈抽象为事件通道：
```rust
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEvent {
    PhaseStarted { phase_name: String },
    TaskStateChecked { node: String, task: String, state: TaskState },
    TaskLog { node: String, task: String, stream: LogStream, line: String },
    TaskCompleted { node: String, task: String, duration_ms: u64 },
    ExecutionFinished { success: bool, summary: String },
}
```
* **CLI 模式**：订阅 `tokio::sync::broadcast::Receiver<ExecutionEvent>`，渲染终端进度条。
* **UI Server 模式**：订阅同一通道，一方面将日志持久化写入本地数据库，另一方面通过 **SSE (`axum::response::Sse`)** 实时推送给浏览器前端。

### 4.2 引入本地状态存储（`~/.kurin/state.db`）
CLI 是无状态的（每次通过 `-f cluster.toml` 传入），而 UI 需要管理多个集群的完整生命周期。在本地存储以下三张核心表（或 KV Bucket）：
1. `clusters`：存储集群名称、创建时间、当前状态（`Planning` / `Applying` / `Running` / `Degraded` / `Failed`）、以及完整的 `cluster.toml` 原文。
2. `jobs`：记录每次针对集群的操作历史（如 `Create`、`Upgrade`、`AddNode`、`CertRenew`）及起止时间。
3. `job_logs`：归档每次 Job 执行过程中的输出日志，支持用户在网页上随时回溯历史安装报错。

### 4.3 集群级别的并发互斥锁（Cluster Mutex）
Web 环境下可能出现多个标签页同时对同一集群发起变更操作。在 `Axum` 的 `AppState` 中维护：
```rust
pub struct AppState {
    pub db: Database,
    // 记录当前正在执行变更任务的集群及其实时事件广播通道
    pub active_jobs: DashMap<String, tokio::sync::broadcast::Sender<ExecutionEvent>>,
}
```
* 当对集群 `prod-k8s` 发起 `Apply` 或 `Destroy` 时，先检查 `active_jobs` 中是否存在该 Key。若存在，直接拒绝并发修改；同时，新打开网页的用户可以直接 attach 到现有的 `broadcast::Sender` 上，无缝观看正在进行的安装进度！
* 只读操作（如 `--dry-run` 计划生成、`status` 状态检查、`certs check`）不受互斥锁限制。

---

## 5. 二进制体积极致优化方案（防膨胀指南）

为确保引入方案一（React SPA）后，`kurin` 二进制文件依然保持轻量（增量控制在 **2MB ~ 3MB**），在工程实现上必须执行以下 4 项规范：

### 5.1 严选前端依赖（避开体积刺客）
* **禁止引入 `@monaco-editor/react`**（体积 >3MB），统一使用模块化的 **`CodeMirror 6`**（压缩后 `<50KB`）实现 TOML 编辑器。
* **禁止打包自定义字体文件（`.woff2`）**，代码与日志区域统一使用系统等宽字体栈：`font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace`。

### 5.2 编译期静态资源预压缩（Gzip / Brotli 直出）
不要将未压缩的 `.js` 和 `.css` 原文件嵌入二进制！
1. 在 `vite.config.ts` 中配置 `vite-plugin-compression`，在构建时直接生成高压缩率的 `.gz`（或 `.br`）文件。
2. `rust-embed` 仅嵌入压缩后的资源文件。
3. `Axum` 静态文件路由拦截请求，直接将二进制内存中的 `.gz` 字节流原样返回，并附带响应头 `Content-Encoding: gzip`。
   * **收益**：前端静态资源占用二进制体积直接缩小 **70% ~ 80%**，且服务端运行时零 CPU 压缩开销。

### 5.3 配置 Cargo Release 编译优化
在工作区根目录的 `Cargo.toml` 中开启符号剥离与链接时优化（LTO）：
```toml
[profile.release]
strip = true        # 自动剥离调试符号表（直接缩减 30%~50% 二进制体积）
lto = "thin"        # 开启跨 Crate 链接时优化，消除死代码
codegen-units = 1   # 提升编译器优化程度
```

### 5.4 使用 Cargo Feature Flag 实现“可插拔 UI”
在 `crates/kurin-cli/Cargo.toml` 中通过特性开关隔离 UI 模块：
```toml
[features]
default = ["ui"]
ui = ["dep:kurin-server"]
```
* **标准发行版**：`cargo build --release`（默认包含全功能 Web UI，单文件约 15MB~20MB）。
* **极简边缘版**：`cargo build --release --no-default-features`（完全剔除 Web 后端与前端资源，专供极端受限环境使用）。

---

## 6. 实施路线图 (Roadmap)

* **Phase 1：后端基础与事件流改造（周末 1）**
  * 在 `kurin-core` 中抽出 `ExecutionEvent` 通道。
  * 新建 `crates/kurin-server`，集成 `Axum` 与 `redb`/`rusqlite`，实现 `/api/clusters` 的增删改查及 `/api/clusters/:name/plan`（调用你的 `Planner` 返回 JSON 格式的 DAG 阶段数据）。
* **Phase 2：前端脚手架与核心闭环（周末 2）**
  * 初始化前端工程，接入 `ts-rs` 自动生成的类型定义。
  * 完成“集群列表大盘”与“创建集群（表单 + CodeMirror TOML 预览 + Dry-Run 计划弹窗）”页面。
* **Phase 3：实时执行引擎对接与打包（周末 3）**
  * 对接 SSE 接口，结合 `Xterm.js` 实现多节点安装进度的实时点亮与日志流滚动。
  * 在 `justfile` 中串联构建脚本，完成 `rust-embed` 预压缩内嵌，发布首个带 UI 的单二进制版本。

```