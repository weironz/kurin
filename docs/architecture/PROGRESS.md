# 实施进度台账

> 依据：[06-dev-plan.md](06-dev-plan.md)。分支 `dev/arch-v2`。
> 状态：todo / doing / done / blocked。每个 WP 完成时更新本表并随代码一起提交。

## 总表

| WP | 状态 | 提交 | 快照变化 | 需人工 e2e 命令 | 备注 |
|---|---|---|---|---|---|
| WP-00 行为基线快照 | done | 见提交 | 建立基线（50 个快照） | 无 | 20 个配置样例；记录到基线中的 kubeadm 轨道阶段标签错位见备注 |
| WP-01 修复 upgrade drain 判定 | done | 见提交 | 无（CLI 新增 `--force-drain-failure`） | `just tier2-upgrade-minor`（含人为 drain 失败场景） | 行为变更已写入 CHANGELOG；`run_offline` 保留旧签名 |
| WP-02 workspace 拆分 | done | 见提交 | 无 | 无 | 依赖图 config←core←{ssh,artifact}←components←bin；`just check`/`build`/`release-musl` 通过 |
| WP-03 提权 become/sudo | done | 见提交 | 3 个 config 格式快照（Debug 新增 become 字段） | `e2e/tier1/nonroot.sh`；回归 `just tier0-test`、`just tier1` | 非 root 经 sudo；`ResolvedSsh` 字段名为 `r#become`；秘密 Debug 全部打码 |
| WP-04 节点 facts 与按节点架构 | done | 见提交 | 计划树新增 discover 阶段；config 快照新增 `arch: None` | `e2e/tier1/mixed-arch.md`；回归 `just tier1`（up/offline/precheck/nonroot）与 `just tier2` | 每节点 arch 来自 facts；precheck 基于同一次 discover；`--arch` 接受逗号列表 |
| WP-05 流式 exec 与上传 | done | 见提交 | 无 | `just tier1-offline`（记录控制机 RSS 峰值） | 行 8 KiB 截断、每流 16 MiB 捕获上限；上传 64 KiB 分块 + 远端 sha256 校验 |
| WP-06 事件 v2 / tracing / 日志 | done | 见提交 | v1 事件仅新增 `ts`；新增 v2/终端快照 | `just tier1 && just tier1-test` 后 `kurin logs --last -vv` | EventBus + JSONL + `kurin logs`；默认终端输出不变 |
| WP-07 RunContext 与 Blackboard | done | 见提交 | 无 | `just tier2`、`just tier1-zone`、`just tier1-k3s` | 删除 artifact OnceLock；join 材料/k3s token 走 Blackboard（0600） |
| WP-08 SSH 安全与端口转发 | todo | | | `just tier0 tier0-test` | |
| WP-09 集群锁与 RunState v2 | done | 见提交 | 无 | 双终端 `add nodes` 抢锁 | 标准库 `File::try_lock`；RunState v2 兼容 v1；`--resume` 校验计划指纹 |
| WP-10 PlanBuilder / NodeSel | done | 见提交 | 无（快照零变化） | 无 | Phase/PlanBuilder/NodeSel/OpKind；移除 petgraph；保留既有阶段标签错位待 WP-11/12 修正 |
| WP-11 DAG 执行器 | done | 见提交 | CLI sysctl-failure 快照新增 blocked skip 行 | tier1/tier2/tier3 全量 | 入度表 + 就绪队列；全局 32/节点 4；Strict/BestEffort；rolling；取消；计划指纹 |
| WP-12 删除改为计划 | done | 见提交 | 新增 8 个 teardown 计划快照 | `just tier1-k3s`、`e2e/tier1/k3s-delete.sh`、tier2 删 worker、tier3 删集群 | 全部步骤 BestEffort；锚点不可达仍清理；`--dry-run [--check]` |
| WP-13 升级改为计划 | done | 见提交 | 新增 3 个 upgrade 计划快照 | `just tier2-upgrade-minor`、`e2e/tier2/upgrade-offline.sh` | 每节点任务链 + rolling；drain 失败阻塞后续节点/批次；dry-run 输出计划树 |
| WP-14 kurin-ops | done | 见提交 | 新增 PlanView/certs-renew 快照 | `just tier2-certs`、`just tier2-status`、`just tier1-k3s-ops` | main 只剩参数解析；certs renew 计划化；status 多控制面回退 |
| WP-15 Component（一） | done | 见提交 | 无（快照零变化） | 无 | Component trait/registry；os/runtime/distro 七个组件；`PlanBuilder::add_track` 保持 per-node 布局 |
| WP-16 Component（二）与能力表 | done | 见提交 | 无（快照零变化；新增能力表快照） | `just tier1-offline`、`just tier3-external-lb` | 23 个组件；`kurin capabilities --json`；`just docs-matrix`；bundle/镜像集从组件聚合 |
| WP-17 ClusterApi | done | 见提交 | 无 | `just tier2-status`、tier1 标签任务 | kube 4.2 + k8s-openapi 0.28（仅 v1_33）；二进制 +62%/+65%；Docker k3s 集成测试通过 |
| WP-18 就绪跟踪器 | done | 见提交 | 无 | tier1 错误镜像诊断 | Deployment/STS/DS/Job/Pod/PVC/CRD/APIService/Node 规则；变更事件 + 30s 心跳；卡住自动抓事件/日志 |
| WP-19 kurin-state | done | 见提交 | 计划新增 post/state-record（全部计划快照） | tier2 后 `kubectl -n kube-system get cm kurin-state -o yaml`；`add nodes` 走记录 | 8 个键；spec.toml 脱敏；history 最近 20 条 |
| WP-20 ClusterSpec 与秘密引用 | done | 见提交 | 无（WP-00/诊断快照不变） | tier1 用 env 变量与 0600 文件提供密码 | `SecretRef` 三形态；`ClusterSpec` + `Cluster` 别名；prepare 解析一次并注册脱敏 |
| WP-21 v1alpha2 与迁移 | done | 见提交 | 版本门禁文案快照 + 22 个迁移快照 | `kurin config migrate` 后 `create cluster --dry-run` | v1alpha2 + v1alpha1 双读；注释保留；迁移后 ClusterSpec 等价 |
| WP-22 JSON Schema | done | 见提交 | 无 | 无（纯本地导出） | `kurin config schema`；`docs/schema/cluster.v1alpha2.json` 漂移测试；样例全量校验 |
| WP-25 节点参数解析器 | done | 见提交 | 无 | 无 | 提前实施（与 WP-21 无代码依赖）；69 条表驱动用例 |
| WP-26 快速创建参数 | done | 见提交 | 新增 12 个参数→配置快照 + 摘要快照 | `just tier1-quick`、`just tier3-quick-ha` | QuickArgs → v1alpha2 合并；秘密只落引用；确认/落盘规则 |
| WP-27 --set 与 add nodes 快速参数 | done | 见提交 | 新增 add-nodes 改写快照 | `e2e/tier2/add-worker.sh` | `--set` 路径/类型推断；add nodes 追加节点并保留注释；`--no-save` |
| WP-28 --setup-ssh-key | done | 见提交 | 新增 setup 后落盘配置快照 + ask-become-pass 落盘快照 | `just tier1-ssh-key` | ed25519 生成（私钥 0600，只有私钥时反推 .pub）；幂等追加（grep 守卫）；验证私钥登录；`--ask-become-pass` 只落 env 引用；`--ssh-key-passphrase-env` 兼配 setup |
| WP-30 OCI spike | done | 见提交 | 无 | 无 | 结论见 docs/architecture/spikes/30-oci.md；04 §8/§9 已按实测修正 |
| WP-31 BlobStore 与 OCI layout | done | 见提交 | 无 | 无 | `oci-spec 0.9`；26 单测 + 12 集成测试通过；oras/skopeo 未安装，未做外部 CLI 读取验证 |
| WP-32 包格式与 package create base | done | 见提交 | 新增包元数据快照 | `kurin package create base --arch amd64,arm64 -o file:///tmp/base.tar.zst && kurin package inspect file:///tmp/base.tar.zst` | OCI index 布局；SPDX 边车；确定性构建；`artifact` 转发并弃用提示 |
| WP-33 package 子命令 | done | 见提交 | 无 | zot 容器三形态互转（已本地实跑） | push/pull/verify/inspect/list/prune；`inspect --manifest`（避开全局 --json）；修复 WP-32 镜像 index 未传输 bug |
| WP-34 部署侧接入包 | done | 见提交 | 新增覆盖性诊断快照 | `just tier1-offline`（旧 bundle 不回归） | `ArtifactSources` 扩展 base/os/delivery/verify/trusted-keys；兼容读取器（旧 bundle 目录、裸路径包）；`--offline <URI>` ≡ `--package`；覆盖性校验（K8s 版本/发行版/CNI/节点架构，缺什么列什么）；包分发（preload/registry）留给 WP-35/36，变更运行明确报错 |
| WP-35 preload 分发基于包 | done | 见提交 | 新增 package-base 计划快照 | `just tier1-offline` × `oci-layout://`/`file://`/`oci://` | Blob 流式上传 + 单镜像 layout tar；k3s agent images；`--max-upload-parallel` |
| WP-36 registry 分发与 mirror | todo | | | `e2e/tier3/registry.sh` | |
| WP-37 离线 CNI 补齐 | todo | | | `just tier1-offline-cni flannel/cilium/kube-ovn` | |
| WP-38 OS 依赖包 | todo | | | `e2e/tier1/rocky.sh` 离线变体 | |
| WP-39 签名/SBOM/发布 | todo | | | | |
| WP-40 addon.yaml 规范 | done | 见提交 | 无 | 无 | 41 个 addon 专项测试；schema 漂移测试；目录/内嵌/包三种加载；修复 package 文件集两级索引 bug |
| WP-41 addon 渲染 | done | 见提交 | 新增 4 个渲染快照 | 无（纯本地渲染） | 03 §4.4 上下文；values 合并/校验；秘密脱敏；未声明镜像拒绝 |
| WP-42 helm 执行器 | done | 见提交 | 无 | tier1 安装测试 chart | helm 3.19.0 制品（官方 sha256）；HelmRunner + manifests SSA；渲染镜像检查；返回对象列表 |
| WP-43 addon-engine 组件 | todo | | | | |
| WP-44 kurin addon 命令族 | todo | | | tier1 metrics-server 生命周期 | |
| WP-45 addon 打包与离线 | todo | | | 离线 tier1 ingress-nginx | |
| WP-46 首批内置 addon | todo | | | `e2e/tier1/addons.sh` | |
| WP-47 存储迁移到 addon | todo | | | `e2e/tier1/storage.sh` | |
| WP-48 CNI 改用 chart | todo | | | `just tier1-cilium`、kube-ovn、接管场景 | |
| WP-49 registry zot 默认与 external | todo | | | `e2e/tier3/registry.sh` | |
| WP-50 kurin-server 骨架与认证 | done | 见提交 | 无（纯新增 crate 与 `serve` 子命令） | 无（仅回环监听自测） | axum 0.8 + axum-server(rustls/ring)；错误信封 `camelCase` 对齐 WP-60 前端契约；403 `forbidden-host` 为 §6.10 Host 规则新增状态码 |
| WP-51 Store 与配置修订 | done | 见提交 | 无 | 无（纯本地 DB） | rusqlite WAL + 单写线程/读池；user_version 迁移；秘密哨兵；base_rev 409；镜像 0600 |
| WP-52 OpRunner/事件持久化/SSE | todo | | | `scripts/api-smoke.sh` | |
| WP-53 Collector | todo | | | 3 集群总览 | |
| WP-54 其余 API | todo | | | | |
| WP-55 类型生成与静态资源 | todo | | | | |
| WP-60 前端脚手架 | done | 见提交 | 无 | 无 | 提前实施（先用手写 provisional DTO 对 mock 开发，WP-55 替换为 ts-rs 生成类型）；34 个 Vitest 用例通过 |
| WP-61 集群总览与详情 | done | 见提交 | 无 | 无（WP-53 验收场景待后端） | 15s 轮询、五态徽标、详情四 Tab；46 个 Vitest 用例 |
| WP-62 创建向导 | todo | | | tier1 真实创建 | |
| WP-63 操作详情与日志控制台 | todo | | | tier1 刷新续读 | |
| WP-64 Addon 页面与危险操作 | todo | | | tier1 metrics-server | |
| WP-65 包页面/升级/证书/TOML 编辑器 | todo | | | tier2 页面升级 | |
| WP-66 前端端到端验收 | todo | | | 维护者执行 | |
| WP-70 etcd 备份/恢复 | todo | | | | |
| WP-71 证书自动续期 | todo | | | | |
| WP-72 k3s 升级 | todo | | | | |
| WP-73 kurin import | todo | | | | |
| WP-74 OS 扩展 | todo | | | | |
| WP-75 CIS 加固 | todo | | | | |
| WP-76 conformance 门禁 | todo | | | | |
| WP-77 种子仓库 | todo | | | | |
| WP-78 英文文案与文档站 | todo | | | | |

## 偏离记录

- WP-50：Host 拒绝使用 403 `forbidden-host`（UI §7 状态码清单之外，语义上 400=语法不合适）；错误信封为顶层 `{code, message, diagnostics?}` 且 DTO 字段 `camelCase`（`expiresAt`/`componentVersions`），对齐已交付的 WP-60 前端契约而非 §7 的「snake_case」总则，WP-55 ts-rs 生成时以此为基准；默认 Host 白名单额外含 `[::1]:<port>`；`GET /api/v1/session`（会话自省）与 `GET /api/v1/healthz`（与 `/healthz` 等价）按 WP-60 mock 契约补齐；host 检查同时读 HTTP/2 `:authority`（TLS 下 curl/浏览器走 h2，无 Host 头）；会话 TTL 定为 24h（文档未规定）。新依赖：axum/axum-server/rand 纯 Rust，rustls(ring) 已在树内（russh/kube），musl/zig 交叉编译不受影响。
- WP-00：`ScriptedSession` 在 `(exit, stdout, stderr)` 之外增加了「命令已执行但传输中断」的应答与副作用文件，用于覆盖 `Outcome::Unknown → 重新探测` 路径；`cli_output` 快照对任务耗时与阶段内行序做归一化（并发完成顺序不在 v1 契约内）。
- WP-15：为保持快照的 per-node 任务布局，`PlanBuilder` 增加 `add_track`（phase 内按 track/node/调用序重排）；k3s 校验规则移入 `distro/k3s`，错误文本逐字节一致。
- WP-16：`storage/*` 标为 online-only（镜像在 bundle 但 bootstrap 不预载，WP-47 接管）；`ha/kube-vip` 镜像 pin 在版本表而非 manifest，标 online-only；`capabilities` 为隐藏命令且恒出 JSON。
- WP-50：Host 拒绝用 403 `forbidden-host`（§7 清单外）；错误/DTO 采用顶层 `{code,message,diagnostics?}` + camelCase（对齐 WP-60 前端）；默认白名单含 `[::1]`；会话 TTL 24h；新增 `GET /session` 与 `GET /api/v1/healthz`。
- WP-35：上传闸门为 run 级（stage 串行，实际等价）；`--dry-run` 配 `oci://` 源会先拉包做覆盖校验；为 Blob 变体改动了 `kurin-ssh`/core/ops/main。
- WP-34：`upgrade --offline <包URI>`/`add nodes --offline <包URI>` 在变更期明确拒绝（分发留给 WP-35）；`oci://` 来源报错并指向 `package pull`；`artifacts.offline-dir` 与 `packages.base` 互斥。
- WP-33：`inspect --json` 改为 `--manifest`（全局 `--json` 与子命令长选项冲突），`list` 用 `--format text|json`；`prune` 需全量重哈希（3.7GiB 约 2 分钟）。
- WP-32：`kurin artifact` 现在产出 OCI layout（旧目录树由 WP-34 兼容读取）；镜像按所选架构携带子 manifest，上游 index 原样保留；修复 busybox URL 模板（Docker Hub tag 无 `v` 前缀）。
- WP-51：本 WP 只提供 Rust store API，未接入 HTTP/AppState（WP-52 集成）；`ApiError::Internal` 为增量变体。
- WP-42：helm 制品标 `optional`（旧 bundle 仍兼容，缺 helm 时明确报错）；`oci://` chart 由锚点 helm 拉取；集群级 kind 用内建清单判定。
- WP-41：「内置推荐值」实现为 `values.yaml.j2` 按集群上下文渲染；`kubernetes.distribution` 暴露 kubeadm/k3s；`registry.mirror` 未配置为 null。
- WP-40：`BUILTIN_ADDONS` 与内嵌目录接线留给 WP-43/46（目录暂空）；`oci://` 源明确报错，依赖 WP-33 的 pull。
- WP-27：`add nodes` 复用 `merge()`，v1alpha1 配置会先迁移为 v1alpha2 再写回；`--set` 写在 WP-26 已实现，本 WP 补测试/文档。顺带修复 `package inspect --json` 与全局 `--json` 的 clap 冲突（改用全局开关）。
- WP-26：`--setup-ssh-key` 留给 WP-28；无凭据时落盘 `{ env = "KURIN_SSH_PASSWORD" }` 并告警；k3s 未给 CNI 默认 flannel；`--registry` 未给 kind 默认 docker-registry；快速参数 + YAML 文件拒绝；`--set` 提前在本 WP 实现；`--setup-ssh-key` 已由 WP-28 实现。
- WP-22：`Ipv4Net`/`toml::Value` 无 schemars 原生实现，分别导出为 string/任意 JSON（语义仍由加载器校验）；只入库 v1alpha2 schema。
- WP-21：未知 addon 名默认拒绝，带 `source` 的外部 addon 名放行；v1alpha2 保留 `[artifacts]` 并新增 `[packages]`；新增 `toml_edit`（纯 Rust）。
- WP-20：保留 `pub type Cluster = ClusterSpec` 别名以减少调用点改动；秘密在 `kurin-ops::prepare` 解析一次；`--dry-run` 不读秘密文件（文档已写明）。
- WP-19：记录类型/实现放在 components（ops 只 re-export）；`upgrade` 暂不写记录（其 post 任务独立，留待后续）；`spec.toml` 由 `Cluster.source` 规范化重排。
- WP-14：`prepare` 额外接收 `OutputOpts`（事件 sink 需在 discover 前挂上）；`plan` 为 async；`StatusReport`/`NodeStatus` 直接 Serialize。
- WP-13：首个控制面现在也 cordon/drain/ready-gate/uncordon；`restart-kubelet` 失败严格中止；`preflight/artifacts/addons` 空阶段保留；`upgrade --resume` CLI 开关未加（执行器已带指纹）。
- WP-12：teardown 步骤行仍由任务自身 tracing 输出（成功格式与旧版一致）；`local-state-cleanup` 使用 `local` 伪节点；delete cluster 仍只处理 worker+control-plane；不可达节点不再中断整个删除。
- WP-11：取消由 `OpSession::finish` 发布唯一 `OpFinished{Cancelled}`；BestEffort 仍发 v1 `TaskFailed` 并额外发 v2 `TaskWarn`；多失败错误文本不再带 stage 名。
- WP-10：为保持快照逐字节一致，`plan_os`/`plan_runtime`/`plan_images` 接收 `&Node` 并按节点交错；k3s 分支集中在 `plan_k3s`；现有阶段标签错位（kubeadm-init 在 vip 等）保留并加注释。
- WP-09：Blackboard 的权威持久化仍是 `<cluster>.bb.json`，RunState v2 的 `blackboard` 字段是写穿镜像；锁使用标准库 `File::try_lock`（MSRV 1.89+）。
- WP-18：`track` 实现为 `kurin-kube` 自由函数而非 `ClusterApi` trait 方法（两个后端共用轮询逻辑）。
- WP-17：`get_raw` 为 status 的 `/healthz*` 补充；`connect()` 接收 `Option<bool>`（tcp_forwarding）而非 FactsTable；musl 二进制 x86_64 9.6MB→15.7MB、aarch64 7.5MB→12.4MB。
- WP-07：`RunContext.events` 为 `Arc<EventBus>`；`Plan::build*` 从 `run.facts` 取 facts（不再单独传 `&FactsTable`）；`upgrade::run*` 增加 `&Arc<RunContext>`；新增 `tokio-util`（纯 Rust）。
- WP-06：`Body` 增加 `ClusterReady`；`Event::ClusterReady` 由 unit variant 变为 `{ ts }`（JSON 仅加 `ts`）；`-v` 实现为每条命令一条摘要，原始 stdout/stderr 在 `-vv`；新增 `time`/`tracing`/`tracing-subscriber`（纯 Rust）。
- WP-08：`TransportLost` 定义在 `kurin-core::engine`（SSH 返回该错误，执行器 downcast 后按 `Outcome::Unknown` 重新探测）；`forward_remote` 的完整数据回传仅做了请求/端口与回调接线，端到端验证留给 WP-77。
- WP-04：02 §3.4 的「discover 先于计划构建」与「计划树包含 discover 阶段」按保守方式统一：discover 在构建计划前执行，计划中的 `discover` 阶段是只读记录（check InSync / apply Verified）。`Arch` 移到 `kurin-core::arch`，`kurin-artifact` 继续 re-export。
- WP-05：`exec_stream` 不做透明重试（流式重放会重复行/副作用）；捕获上限按 stdout/stderr 各自 16 MiB 计算，sink 仍收到每一行。
- WP-03：`become` 是 Rust 保留字，运行期字段写作 `r#become`（配置键不变）；「节点 become=none + 继承集群 become-password」按跨字段冲突拒绝；无新依赖。集成时补上了 `SshDefaults`/`NodeConfig`/`RegistryConfig`/`Cluster` 的 Debug 脱敏（含 `Cluster.source` 原文），并加测试。
- WP-61：为详情页扩展了 provisional DTO：`ClusterDetail.collectError`、`configSummary`、`ObservedNode.arch?`、`ClusterSnapshot.podsNotRunning?`；WP-55 生成 ts-rs 绑定时需与后端对齐。
- WP-60：按计划允许“前端先对 mock 开发”，在 WP-55 之前完成脚手架；`src/api/types.ts` 为临时手写 DTO，WP-55 生成 ts-rs 绑定后必须替换并删除该文件的 provisional 标记。
- WP-02：`kurin-core` 的 `tempfile` 是运行时依赖（`state.rs` 原子写状态文件）；`kurin` 移除已无源码使用的 `futures` 直接依赖；`kurin/tests` 通过 facade 原样保留；`Plan::build*` 改为 `kurin::plan::build*`。
- WP-00 基线记录到的现有行为（未修改）：kubeadm 轨道把 `kubeadm-init` 放在 `vip` stage、`api-ready` 放在 `bootstrap`、join 任务放在 `api-ready`，`join` stage 为空；顺序正确，仅标签错位。WP-10 修正时需逐条解释快照变化。

## 待人工 e2e

（随 WP 完成追加）
- WP-35：`just tier1-offline oci-layout:///tmp/kurin-pkg`、`file:///tmp/kurin-base.tar.zst`、`oci://127.0.0.1:5000/kurin/base:v1.33.1-kubeadm`（三种来源各一次）。
- WP-34：`just tier1-offline`（用既有旧 bundle 目录，验证不回归）；
  另可用 `kurin package create base -o file:///tmp/base.tar.zst` 后
  `create cluster --offline file:///tmp/base.tar.zst --dry-run` 验证
  覆盖性诊断（真包、真缺口列表）。

- WP-15：纯重构，快照零变化；回归 `just tier1`/`tier2` 即可。
- WP-16：`just tier1-offline`、`just tier3-external-lb`。
- WP-50：`kurin serve` 回环冒烟（一次性登录链接、`/session`、401/403/400、TLS h2）；`cargo clippy -p kurin --features ui`。
- WP-35：`just tier1-offline oci-layout:///tmp/kurin-pkg`、`file:///tmp/kurin-base.tar.zst`、`oci://127.0.0.1:5000/kurin/base:v1.33.1-kubeadm`（三种来源各一次）。
- WP-34：`just tier1-offline`（旧 bundle 不回归）；`kurin package create base -o file:///tmp/base.tar.zst && kurin create cluster -f cfg --offline file:///tmp/base.tar.zst --dry-run`。
- WP-33：本地 zot 容器三形态互转（layout→zot→tar→verify→inspect，二次 push 零发送，错误凭据 401）；已清理容器。
- WP-32：`kurin package create base --arch amd64,arm64 -o file:///tmp/base.tar.zst`，再 `kurin package inspect file:///tmp/base.tar.zst`（真实网络下载约 10 分钟；本地假上游测试已覆盖校验路径）。
- WP-51：`cargo test -p kurin-server --test store`；`rusqlite bundled` 的 musl/zig 交叉编译需在发布机验证。
- WP-42：tier1 安装测试 chart（`helm upgrade --install ... --wait=false`），确认 `helm status` 与对象列表，再卸载。
- WP-41：纯本地渲染/校验，无需远端 e2e；13 个集成 + 5 个单测。
- WP-40：纯本地解析/校验，无需远端 e2e；`just addon-schema` 与漂移测试。
- WP-27：`just tier2 && just tier2-test`，再 `e2e/tier2/add-worker.sh target/tier2/debug/kurin`。
- WP-26：`just tier1-quick`（纯参数 + 密码认证单节点）、`just tier3-quick-ha`（3 控制面 + kube-vip）。
- WP-22：`just schema && git diff --exit-code docs/schema/cluster.v1alpha2.json`；无需远端 e2e。
- WP-21：`kurin config migrate` 后 `create cluster --dry-run`；全部 v1alpha1 样例迁移快照与等价断言已自动覆盖。
- WP-20：tier1 用 `{ env = ... }` 与 `{ file = ... }`（0600）提供 SSH 密码；`--dry-run` 不读取秘密文件。
- WP-19：tier2 安装后检查 `kube-system/kurin-state`（8 键、密码为 `******`、history 含本次 op）；改配置 `add nodes` 确认不再探测 `/version`；`status` 显示 expected vs observed。
- WP-14：`just tier2-certs`、`just tier2-status`、`just tier1-k3s-ops`。
- WP-13：`just tier2-upgrade-minor`、`e2e/tier2/upgrade-offline.sh`。
- WP-12：`just tier1-k3s`（含 delete）、`e2e/tier1/k3s-delete.sh`、tier2 删除 worker、tier3 删除集群。
- WP-11：`just tier1 && just tier1-test && just tier1-down`、`just tier2 ...`、`just tier3 ...`（执行器最高风险，必须全量回归）。
- WP-10：结构性重构，快照零变化；回归 `just tier1`/`tier2` 即可。
- WP-09：两个终端同时对 tier1 执行 `add nodes`，第二个立即失败并显示第一个的 pid 与命令。
- WP-18：tier1 故意配错镜像，确认日志出现 Pod 事件与容器日志尾部及原因提示。
- WP-17：`just tier2-status`、tier1 标签任务；`KURIN_IT_CLUSTER=<kubeconfig> cargo test -p kurin-kube --test cluster_it -- --ignored`。
- WP-07：`just tier2`、`just tier1-zone`、`just tier1-k3s`（验证 zone/Blackboard/续跑）。
- WP-06：`just tier1 && just tier1-test`，再执行 `target/tier1/debug/kurin logs --last -vv`，确认包含 kubeadm init 全量输出与时间戳。
- WP-08：`just tier0 tier0-test`；转发集成 `cargo test -p kurin --test ssh_docker -- --ignored --nocapture`。
- WP-04：`e2e/tier1/mixed-arch.md`（x86 控制机 + qemu/multipass arm64 节点，含 `--dry-run --check` 断言 discover 阶段与 arm64 制品行、离线 `--arch amd64,arm64`）；回归 `just tier1`（up/offline/precheck/nonroot）与 `just tier2` 升级路径。
- WP-05：`just tier1-offline`（最大上传路径），记录上传期间控制机 RSS 峰值，确认无整文件缓冲回归。
- WP-03：`e2e/tier1/nonroot.sh`（multipass 中 ubuntu 用户 + 免密 sudo，单节点 create + delete）；回归 `just tier0-test`、`just tier1`。
- WP-01：`just tier2-upgrade-minor`。额外验证：让某 worker 的 drain 失败（如带 PDB 的 Pod），确认升级在该节点前中止且节点保持 cordon；再用 `--force-drain-failure` 复跑确认告警与继续执行。

## 待决问题

（无）

## 环境备注

- 本机无 Rust 工具链，已安装 rustup stable（`~/.cargo/bin`）与 `just`；所有命令需先 `export PATH="$HOME/.cargo/bin:$PATH"`。
- 真机 e2e 依赖 multipass；本机 UFM 在运行，禁止改动主机网络配置。