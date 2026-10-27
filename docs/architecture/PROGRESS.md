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
| WP-10 PlanBuilder / NodeSel | todo | | | | |
| WP-11 DAG 执行器 | todo | | | tier1/tier2/tier3 全量 | |
| WP-12 删除改为计划 | todo | | | `just tier1-k3s`、tier2 删 worker、tier3 删集群 | |
| WP-13 升级改为计划 | todo | | | `just tier2-upgrade-minor`、`e2e/tier2/upgrade-offline.sh` | |
| WP-14 kurin-ops | todo | | | `just tier2-certs`、`just tier2-status`、`just tier1-k3s-ops` | |
| WP-15 Component（一） | todo | | | | |
| WP-16 Component（二）与能力表 | todo | | | `just tier1-offline`、`just tier3-external-lb` | |
| WP-17 ClusterApi | done | 见提交 | 无 | `just tier2-status`、tier1 标签任务 | kube 4.2 + k8s-openapi 0.28（仅 v1_33）；二进制 +62%/+65%；Docker k3s 集成测试通过 |
| WP-18 就绪跟踪器 | done | 见提交 | 无 | tier1 错误镜像诊断 | Deployment/STS/DS/Job/Pod/PVC/CRD/APIService/Node 规则；变更事件 + 30s 心跳；卡住自动抓事件/日志 |
| WP-19 kurin-state | todo | | | tier2 后检查 ConfigMap | |
| WP-20 ClusterSpec 与秘密引用 | todo | | | | |
| WP-21 v1alpha2 与迁移 | todo | | | | |
| WP-22 JSON Schema | todo | | | | |
| WP-25 节点参数解析器 | done | 见提交 | 无 | 无 | 提前实施（与 WP-21 无代码依赖）；69 条表驱动用例 |
| WP-26 快速创建参数 | todo | | | `e2e/tier1/quick.sh`、`e2e/tier3/quick-ha.sh` | |
| WP-27 --set 与 add nodes 快速参数 | todo | | | tier2 `add nodes --worker` | |
| WP-28 --setup-ssh-key | todo | | | tier1 密码 + setup-key | |
| WP-30 OCI spike | done | 见提交 | 无 | 无 | 结论见 docs/architecture/spikes/30-oci.md；04 §8/§9 已按实测修正 |
| WP-31 BlobStore 与 OCI layout | done | 见提交 | 无 | 无 | `oci-spec 0.9`；26 单测 + 12 集成测试通过；oras/skopeo 未安装，未做外部 CLI 读取验证 |
| WP-32 包格式与 package create base | todo | | | | |
| WP-33 package inspect/verify/push/pull | todo | | | | |
| WP-34 部署侧接入包 | todo | | | | |
| WP-35 preload 分发基于包 | todo | | | `just tier1-offline` × 三种来源 | |
| WP-36 registry 分发与 mirror | todo | | | `e2e/tier3/registry.sh` | |
| WP-37 离线 CNI 补齐 | todo | | | `just tier1-offline-cni flannel/cilium/kube-ovn` | |
| WP-38 OS 依赖包 | todo | | | `e2e/tier1/rocky.sh` 离线变体 | |
| WP-39 签名/SBOM/发布 | todo | | | | |
| WP-40 addon.yaml 规范 | todo | | | | |
| WP-41 addon 渲染 | todo | | | | |
| WP-42 helm 执行器 | todo | | | tier1 测试 chart | |
| WP-43 addon-engine 组件 | todo | | | | |
| WP-44 kurin addon 命令族 | todo | | | tier1 metrics-server 生命周期 | |
| WP-45 addon 打包与离线 | todo | | | 离线 tier1 ingress-nginx | |
| WP-46 首批内置 addon | todo | | | `e2e/tier1/addons.sh` | |
| WP-47 存储迁移到 addon | todo | | | `e2e/tier1/storage.sh` | |
| WP-48 CNI 改用 chart | todo | | | `just tier1-cilium`、kube-ovn、接管场景 | |
| WP-49 registry zot 默认与 external | todo | | | `e2e/tier3/registry.sh` | |
| WP-50 kurin-server 骨架与认证 | todo | | | | |
| WP-51 Store 与配置修订 | todo | | | | |
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

- WP-00：`ScriptedSession` 在 `(exit, stdout, stderr)` 之外增加了「命令已执行但传输中断」的应答与副作用文件，用于覆盖 `Outcome::Unknown → 重新探测` 路径；`cli_output` 快照对任务耗时与阶段内行序做归一化（并发完成顺序不在 v1 契约内）。
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