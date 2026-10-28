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
| WP-08 SSH 安全与端口转发 | done | 见提交 | 无 | `just tier0 tier0-test` | host-key TOFU/known_hosts 模式；本地/远程转发（TunnelApi 复用）；TransportLost 映射 Unknown；凭据不回显 |
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
| WP-36 registry 分发与 mirror | done | 见提交 | 计划快照 phase 顺序变化（registry 前移）+ 3 个 mirror 渲染快照 | `e2e/tier3/registry.sh zot`；`KURIN_DISCONNECTED=1 KURIN_BASE_PACKAGE=…` | external registry；controller push；节点按 digest fetch；containerd/CRI-O/k3s mirror；`delivery=auto` |
| WP-37 离线 CNI 补齐 | done | 见提交 | 能力表快照（kube-ovn offline=bundle） | `just tier1-offline-cni flannel/cilium/kube-ovn` | kube-ovn 两个镜像真实 pin；离线拒绝移除；镜像集由组件 artifacts 派生 |
| WP-38 OS 依赖包 | done | 见提交 | 新增 os 包/匹配/安装脚本快照 | `just tier1-rocky-offline` | 容器化 deb/rpm 闭包 + 仓库元数据；节点本地源安装；离线匹配诊断；CI 矩阵；ghcr 镜像核实 |
| WP-39 签名/SBOM/发布 | done | 见提交 | 无 | 签名包校验/篡改拒绝/未签名拒绝（本地测试） | SBOM referrer；cosign simple-signing（p256）；push/pull 携带 referrer；CI 签名发布工作流 |
| WP-40 addon.yaml 规范 | done | 见提交 | 无 | 无 | 41 个 addon 专项测试；schema 漂移测试；目录/内嵌/包三种加载；修复 package 文件集两级索引 bug |
| WP-41 addon 渲染 | done | 见提交 | 新增 4 个渲染快照 | 无（纯本地渲染） | 03 §4.4 上下文；values 合并/校验；秘密脱敏；未声明镜像拒绝 |
| WP-42 helm 执行器 | done | 见提交 | 无 | tier1 安装测试 chart | helm 3.19.0 制品（官方 sha256）；HelmRunner + manifests SSA；渲染镜像检查；返回对象列表 |
| WP-43 addon-engine 组件 | done | 见提交 | 计划新增 addons 阶段（全部计划快照）；能力表新增 addon/engine | tier1 带 `[addons.x]` 创建并观察 hooks | requires/conflicts/环/compat 校验；check 读 kurin-state+helm；apply render→apply→track→hooks→record；remove |
| WP-44 kurin addon 命令族 | done | 见提交 | 新增 8 个 addon CLI 快照（list/info/schema/install/upgrade/remove/dry-run） | tier1 metrics-server 生命周期（需 WP-46 目录或 `source`） | list/info/install/upgrade/remove；toml_edit 改写（注释保留、成功后写入、`--no-save`）；`--set`/`--values` 合并并预校验 values schema；`--dry-run` 只打印计划；`--keep-crds`；每次操作只收敛目标 addon 及其 requires |
| WP-45 addon 打包与离线 | done | 见提交 | 新增 addon 包快照 | 离线 tier1 ingress-nginx | `package create addon` + `package create base --addons`；addon 载荷/渲染在节点侧；离线覆盖校验 |
| WP-46 首批内置 addon | done | 见提交 | 无（catalog 是数据；嵌入目录不触发计划快照） | `e2e/tier1/addons.sh <bin> online|cn`（需人工） | 8 个 addon；chart 版本/镜像 digest 实抓（quay/ghcr OCI 与 GitHub release URL 两种 chart 来源，values 模板按 zone 换镜像源）；check-mirrors.sh 纳入 addon 镜像并修复 zone.rs 路径漂移 |
| WP-47 存储迁移到 addon | done | 见提交 | storage 任务从 cni 移到 addons 阶段；能力表移除 storage/* | `e2e/tier1/storage.sh local-path/nfs` | `[storage]` 映射为 addons；StorageClass 名不变；已有集群 InSync；删除第 1 层 storage |
| WP-48 CNI 改用 chart | done | 见提交 | cilium/kube-ovn 计划 describe 行变化 | `just tier1-cilium`、`e2e/tier1/kube-ovn.sh`、`just tier1-cni-takeover` | cilium/kube-ovn chart 实抓 pin；HelmRunner + tracker；`--take-ownership` 接管旧安装，失败不删除 |
| WP-49 registry zot 默认与 external | done | 见提交 | 新增 registry-default-v1alpha2 计划快照；quick 快照 kind 变 zot | `just tier3-registry zot/docker-registry/external` | v1alpha2 无 kind 默认 zot；v1alpha1 保持旧默认；external 校验与文档 |
| WP-50 kurin-server 骨架与认证 | done | 见提交 | 无（纯新增 crate 与 `serve` 子命令） | 无（仅回环监听自测） | axum 0.8 + axum-server(rustls/ring)；错误信封 `camelCase` 对齐 WP-60 前端契约；403 `forbidden-host` 为 §6.10 Host 规则新增状态码 |
| WP-51 Store 与配置修订 | done | 见提交 | 无 | 无（纯本地 DB） | rusqlite WAL + 单写线程/读池；user_version 迁移；秘密哨兵；base_rev 409；镜像 0600 |
| WP-52 OpRunner/事件持久化/SSE | done | 见提交 | 无 | `scripts/api-smoke.sh <tier1.toml>` | SSE 先订阅后补读/seq 去重/心跳；JSONL 日志分页；取消/重试；重启标记 interrupted；并发上限 |
| WP-53 Collector | done | 见提交 | 无 | 导入 3 集群（1 不可达）核对总览 | 抖动间隔/超时/并发/失败保留/证书 6h 降频/多 CP 回退/刷新去重/操作期间暂停；五态健康 |
| WP-54 其余 API | done | 见提交 | 无（新增 25 个 kurin-server 集成测试，每端点成功+失败各一） | `scripts/api-smoke.sh` 扩展 plan/addons/packages（需人工） | /meta 组件表+能力表；/schema/cluster；render（不完整配置仍出 TOML+诊断）；validate span→labels；plan 只读操作（result=PlanView，?check 漂移）；nodes/probe 不回显凭据；nodes/parse；addons/catalog+集群 addons+addon 操作（remove 需 confirm）；packages 注册+verify/push/create-base 无集群操作；schema V4（operations.result/snapshots.addons/packages 表/索引豁免新只读 kind） |
| WP-55 类型生成与静态资源 | done | 见提交 | 无 | `just release-musl` 后 `kurin serve` 打开页面；`just ts-bindings` 漂移测试 | 58 个 ts-rs 绑定；嵌入 gzip dist；SPA/`/api` 404 分离；二进制 +6.3MiB |
| WP-60 前端脚手架 | done | 见提交 | 无 | 无 | 提前实施（先用手写 provisional DTO 对 mock 开发，WP-55 替换为 ts-rs 生成类型）；34 个 Vitest 用例通过 |
| WP-61 集群总览与详情 | done | 见提交 | 无 | 无（WP-53 验收场景待后端） | 15s 轮询、五态徽标、详情四 Tab；46 个 Vitest 用例 |
| WP-62 创建向导 | done | 见提交 | 无 | Playwright mock 走完 3 主 HA 模板；tier1 真实创建 | 9 步向导；每步 render→validate；23 个 Vitest + 1 个 Playwright |
| WP-63 操作详情与日志控制台 | done | 见提交 | 无 | tier1 真实 create 中刷新页面，日志从断点继续 | 阶段×节点网格；资源表；日志控制台（筛选/搜索/跟随/虚拟滚动/下载）；取消/重试；91 Vitest + 1 Playwright |
| WP-64 Addon 页面与危险操作 | done | 见提交 | 无 | Playwright mock 覆盖安装/卸载/删除节点/销毁/移除纳管；tier1 页面安装卸载 metrics-server | Addon 页 + 危险区；156 个 Vitest + 4 个 Playwright；补齐 `POST /clusters/{name}/archive` 与 addon values 落修订 |
| WP-65 包页面/升级/证书/TOML 编辑器 | done | 见提交 | 无 | Playwright mock 覆盖包/升级/证书/编辑器；tier2 页面升级一个小版本 | 包页 + 升级对话框 + 证书 Tab + CodeMirror 6 懒加载；179 个 Vitest + 7 个 Playwright |
| WP-66 前端端到端验收 | done | 见提交 | 无 | tier1 实跑：lifecycle PASS、关浏览器 PASS、服务重启 interrupted→retry PASS、CLI 抢锁 PASS、密码明文扫描 PASS | `web/e2e/` 5 个场景 + 加节点 UI；tier1 两次实跑通过 |
| WP-70 etcd 备份/恢复 | done | 见提交 | 无 | `just tier1-etcd` PASS（tier1 单节点：备份 → 建 ns → 恢复 → ns 消失、`/readyz`+`/healthz/etcd` ok）| kubeadm：etcdctl 经 `crictl exec`（回退 `kubectl exec`）快照到 `/var/lib/etcd` 后移至 `/var/lib/kurin/etcd-backups`；恢复在节点 stage 上游 etcdctl 3.5.21 tarball（distroless etcd 镜像无 shell，不能从容器取）；多成员恢复用 etcd DR flags（`--name/--initial-cluster/--initial-advertise-peer-urls/--initial-cluster-token`）重建 quorum；k3s：`k3s etcd-snapshot save` + `k3s server --cluster-reset`（新 k3s 已移除 `etcd-snapshot restore` 子命令）；跨机传输分块 `dd|base64 -w 4096`（6 MiB/块）绕过 exec 捕获上限；旧数据一律 `…-pre-restore-<ts>` 保留不删 |
| WP-71 证书自动续期 | done | 见提交 | 无 | tier1 实跑：`certs schedule --every 1h --warn-days 3650` 安装 timer，`status` 显示下次续期；手动触发 service 完成 renew + 静态 pod 轮转 + `/readyz` ok | 每控制面装 `kurin-cert-renew.{service,timer}` + `/usr/local/bin/kurin-cert-renew.sh`（幂等重装、`--every` 复用 WP-70 解析器、`WARN_DAYS` 固化进脚本）；kubeadm：`check-expiration` 最小剩余 < warn-days 才 `renew all` + 静态 pod 轮转 + `/readyz` 门控（CA 表不计入：renew all 无法续 CA）；k3s：`certificate check --output table` 门控 + `rotate` + `systemctl restart k3s`；`status`/`certs check` 显示下次续期（`NextElapseUSecRealtime`，`Monotonic` 回退）|
| WP-72 k3s 升级 | done | 见提交 | `kurin/tests/upgrade_k3s.rs` 新增 2 个计划快照；`kurin-core` `Stage` 枚举新增 `stop-service`/`start-service`（纯新增，无既有快照变化）| tier1 实跑：k3s 1.33.1 → 1.34.1（离线 bundle `k3s-1.34.1+k3s1-amd64`）PASS；`k3s --version`、node Ready、`/readyz` 均 ok | 计划按发行版分支：kubeadm 链不变；k3s 链 = `version-check` → `stop-service` → `install-bins`（上传暂存 + `cp -a` 保留旧二进制 `k3s.pre-upgrade-<ts>` 永不删除 + 原子 rename）→ `start-service` → `ready-gate`；server 逐台且 gate 含 `/readyz`，agent 按 `--batch`；agent 单元探测 `k3s-agent`（上游装法）回退 kurin 的 `k3s`；在线仅当清单 pin 目标 release，否则明确报错指引 `--offline`（绝不静默下载未校验二进制）；已在目标的节点全链 InSync |
| WP-73 kurin import | done | 见提交 | 无 | 在另一台控制机 `kurin import --from-cluster … -o tier2.toml` 后 `kurin add nodes -f tier2.toml` | 只读直连（无计划/锁/操作日志）；v1alpha1 记录先迁移为 v1alpha2；kubectl 形态按节点上的 kubeconfig 探测（kubeadm/k3s）；`--password-env` 把每个 `******` 都写成同一 env 引用并列出清单告警，`--key` 只重写 `[ssh.auth]`，密码形态缺口明确报错列出字段；`-o` 0600 落盘、缺省 stdout；`--json` 报告 `{cluster, distribution, kubernetes_version, nodes, output}`（需配 `-o`） |
| WP-74 OS 扩展 | done | 见提交 | 22 个新单测 + `kurin/tests/os_families.rs`（真实 discover 脚本往返 + 包行交叉校验） | tier1 实跑：Ubuntu 22.04.5、AlmaLinux 9.8、openEuler 24.03 均 PASS（cluster-ready、node Ready、pods healthy、os-baseline）；Kylin 无公共镜像、离线变体仍待人工 | facts 支持 Ubuntu 22.04/24.04、RHEL/Alma/Rocky 9、openEuler 24.03、Kylin V10（ID_LIKE 兜底、V10 归一）；kernel-deps 按族选包名/包管理器（Ubuntu/Rocky 命令逐字节不变）；OS 包新增 5 个目标；Kylin 无公共镜像时校验期拒绝 |
| WP-75 CIS 加固 | done | 见提交 | cis 计划快照新增 sysctl 三键；default 档位全部既有快照不变 | tier1 实跑：CIS 集群创建成功；kube-bench v0.10.7：default 64 pass/15 fail/51 warn，CIS 70 pass/11 fail/49 warn | `[kubernetes] profile = "cis"`（v1alpha2）：apiserver/kubelet CIS 参数、审计策略、PSA restricted（仅 default ns）、kubeconfig/kubelet 0600；default 渲染逐字节不变 |
| WP-76 conformance 门禁 | done | 见提交 | 无 | tier1 k3s 单节点实跑 hydrophone v0.8.0：421/424 PASS，3 失败（2 节点用例、API chunking、DS rollback），报告 `docs/conformance/20261028T074500Z/` | `scripts/conformance.sh`（复制 kubeconfig、改 server 地址、hydrophone run、归档） |
| WP-77 种子仓库 | done | 见提交 | `just check` 全绿；快照：capabilities +registry/seed 行、plan 快照描述文案 | tier1 实跑：无 registry 节点、`file://` base + ubuntu-24.04 os 包、`delivery = "seed"` 创建 PASS（镜像经控制机只读 registry + SSH -R 回环 pull） | `crates/kurin-components/src/seed.rs`；`[packages] delivery = "seed"`（v1alpha2，加载期校验：不得与 [registry] 并存、需离线载荷源）；registry/seed-serve@controller + registry/seed-mirror@node（SSH -R）；containerd/CRI-O/k3s 回环 http mirror；镜像走 pull、二进制仍 preload；操作收尾拆除（仅回环、无认证、只读 405） |
| WP-78 英文文案与文档站 | done | 见提交 | 无 | `mdbook build`（docs/site，需安装 mdbook）| 593 键 en 字典（类型强制键集一致）、运行时语言切换（antd/dayjs 跟随、localStorage 持久化）、189 Vitest + 8 Playwright；mdBook 脚手架 + 同步脚本 |

## 偏离记录

- WP-70：三处偏离——(1) `etcd backup/restore` 不走计划/执行器（WP-70 行文隐含计划化，但恢复的步骤互相不幂等、失败后需要人工判断，故与 `certs check`/`status` 同构直接执行；`OpKind::EtcdBackup/EtcdRestore` 保留为事件 kind 与阶段模板）。(2) k3s 恢复用 `k3s server --cluster-reset --cluster-reset-restore-path=…`（k3s 官方现行程序），而非计划中的 `k3s etcd-snapshot restore` 子命令——新版 k3s 已移除该子命令。(3) kubeadm 恢复所需的 etcdctl 不能从 etcd 静态 pod 容器内取（`registry.k8s.io/etcd` 是 distroless，无 shell/coreutils），改为在控制机经既有 artifact 缓存抓取上游 etcd-io/etcd v3.5.21 release tarball（sha256 实抓自官方 SHA256SUMS，与镜像清单 pin 的 etcd 3.5.21-0 同源），上传到节点 `/var/lib/kurin/etcdctl`。

- WP-73：三处偏离——(1) `import` 不走计划/执行器，与 `status`/`certs check` 同构直接执行（读取一次 ConfigMap + 本地文件写，无可收敛漂移）；但比它们更进一步**不建 RunContext/JSONL 操作日志**：`import` 的输入不是配置文件（配置正是它的输出），集群名在读到记录前未知，事件总线的锚点不存在。集群锁也不取（只读）。(2) 记录里 `spec.toml` 可能是 v1alpha1（WP-19 存储的是源文件原文的重排）：按 WP 标题「reconstruct a v1alpha2 TOML」，先经 `kurin config migrate` 的 toml_edit 结构化迁移再补凭据（注释/键序保留）。(3) 凭据回填语义：`--password-env NAME` 把**每个** `******`（含 `[registry]`/`[addons]`/become 密码/节点覆盖）都写成 `{ env = "NAME" }` 并在 stderr 列出清单告警（SSH 密码 ≠ registry 密码，操作者按需改 env 名）；`--key PATH` 只能重写 `[ssh.auth]`（导入已用该私钥连通过 ⇒ 无 passphrase），密码形态字段明确报错列出字段清单（建议改用 `--password-env` 重跑），绝不输出带 `******` 的残缺配置。另：`--json` 必须配 `-o`（stdout 归 JSON 报告），`--key`/`--password-env` 互斥且必须给其一。

- WP-72：三处偏离——(1) **不 cordon/drain k3s 节点**：WP 行文只说"二进制替换 + 滚动重启"，未规定 cordon/drain；k3s 官方升级程序就是停机换二进制，且排空 k3s server 会驱逐其内嵌控制面（无独立 apiserver pod 可留），故链里没有 cordon/drain/uncordon 步骤（`--force-drain-failure` 对 k3s 无意义，保留参数不报错）。(2) **在线目标解析按清单 pin，不动态重 pin**：kubeadm 路径用 dl.k8s.io 的签名 `.sha256` 端点动态重 pin 到任意目标；k3s 无对应信任端点，kurin 不静默下载未校验二进制——清单 pin 的是目标 release 时在线可用，否则报错指引 `--offline <dir>`（bundle 按集群 arch 校验携带目标二进制）。(3) **`k3s --version` 作版本探测与目标映射**：`--with-kubernetes vX.Y.Z` 映射 `vX.Y.Z+k3s1`（显式 `+k3sN` 后缀原样保留以 pin 特定 k3s 补丁）；节点版本用节点本地 `k3s --version`（kubeadm 路径的 kubeletVersion 对 k3s 报的是 `vX.Y.Z+k3s1` 全串，node-local 探测同样可靠且不依赖锚点 API 可用）。

- WP-50：Host 拒绝使用 403 `forbidden-host`（UI §7 状态码清单之外，语义上 400=语法不合适）；错误信封为顶层 `{code, message, diagnostics?}` 且 DTO 字段 `camelCase`（`expiresAt`/`componentVersions`），对齐已交付的 WP-60 前端契约而非 §7 的「snake_case」总则，WP-55 ts-rs 生成时以此为基准；默认 Host 白名单额外含 `[::1]:<port>`；`GET /api/v1/session`（会话自省）与 `GET /api/v1/healthz`（与 `/healthz` 等价）按 WP-60 mock 契约补齐；host 检查同时读 HTTP/2 `:authority`（TLS 下 curl/浏览器走 h2，无 Host 头）；会话 TTL 定为 24h（文档未规定）。新依赖：axum/axum-server/rand 纯 Rust，rustls(ring) 已在树内（russh/kube），musl/zig 交叉编译不受影响。
- WP-00：`ScriptedSession` 在 `(exit, stdout, stderr)` 之外增加了「命令已执行但传输中断」的应答与副作用文件，用于覆盖 `Outcome::Unknown → 重新探测` 路径；`cli_output` 快照对任务耗时与阶段内行序做归一化（并发完成顺序不在 v1 契约内）。
- WP-15：为保持快照的 per-node 任务布局，`PlanBuilder` 增加 `add_track`（phase 内按 track/node/调用序重排）；k3s 校验规则移入 `distro/k3s`，错误文本逐字节一致。
- WP-16：`storage/*` 标为 online-only（镜像在 bundle 但 bootstrap 不预载，WP-47 接管）；`ha/kube-vip` 镜像 pin 在版本表而非 manifest，标 online-only；`capabilities` 为隐藏命令且恒出 JSON。
- WP-50：Host 拒绝用 403 `forbidden-host`（§7 清单外）；错误/DTO 采用顶层 `{code,message,diagnostics?}` + camelCase（对齐 WP-60 前端）；默认白名单含 `[::1]`；会话 TTL 24h；新增 `GET /session` 与 `GET /api/v1/healthz`。
- WP-39：`sigstore` crate 不可用（会引入第二套加密栈/Fulcio/rekor），按 04 §6 预案用 `p256` 实现 cosign simple-signing；新增依赖仅 `p256`（已在依赖图中）。
- WP-63：事件 DTO 沿用 v1 kebab-case + 信封；`GET /operations/{id}` 额外返回 `lastSeq`/`progress.tasks`；`/logs` 返回升序页；Playwright 用真实 Node HTTP 服务模拟 SSE 断线（route.fulfill 无法流式推送）。
- WP-55：`just check` 的 `clippy-ui` 需先 `just web-build`；服务器 DTO 64 位整数标 `#[ts(type = "number")]`；`serde_json::Value` 用 `JsonValue`。
- WP-54：`render` 语义为 200+TOML+诊断（validate 是裁决者）；package 操作作为无集群 Operation；V4 迁移新增 `packages` 表并豁免只读 kind 的 `one_active_op`。
- WP-48：cilium chart 1.20.4 / kube-ovn chart 1.16.15（sha256 实抓）；接管仅在无 helm release 且旧对象存在时启用；`HelmPayload::resolve` 改走 PayloadSet 三源。
- WP-47：任务 id `storage/install@node` → `addons/local-path@node`；k3s 带 `[storage]` 现在也会安装；修复 WP-46 模板缺失的 `{% endset %}`。
- WP-46：`local-path`/`nfs-subdir` 在 WP-47 前仍由第一层 storage 组件安装（模板逐字段一致）；`spec.install.helm.chart` 新增 https tgz URL；`check-mirrors.sh` 原指向已空的 artifact zone.rs，已改指 core zone.rs。
- WP-45：chart 作为 file-set 层并在 `charts` 元数据行记录 digest（设计写的是标准 Helm OCI 制品）；`upstream_hosts` 签名改为 `(cluster, run)`；纯 digest 镜像以 digest 作为 mirror reference。
- WP-38：04 §4 的 `package create base --with-os` 未实现（os 行来自独立 os 包或 base 自带 os 行）；arm64 容器路径仅由 CI/qemu 覆盖。
- WP-37：kube-ovn 镜像 pin 取自 Docker Hub 上游 manifest；`--cni all` 的 base 包含全部 CNI 镜像（双架构）。
- WP-36：`registry` 阶段前移到 `artifacts` 之前（节点需先从仓库取文件），所有计划快照的 phase 顺序随之变化；多架构镜像按上游完整 index 推送；Harbor 仅 kubeadm 轨道。
- WP-35：上传闸门为 run 级（stage 串行，实际等价）；`--dry-run` 配 `oci://` 源会先拉包做覆盖校验；为 Blob 变体改动了 `kurin-ssh`/core/ops/main。
- WP-34：`upgrade --offline <包URI>`/`add nodes --offline <包URI>` 在变更期明确拒绝（分发留给 WP-35）；`oci://` 来源报错并指向 `package pull`；`artifacts.offline-dir` 与 `packages.base` 互斥。
- WP-33：`inspect --json` 改为 `--manifest`（全局 `--json` 与子命令长选项冲突），`list` 用 `--format text|json`；`prune` 需全量重哈希（3.7GiB 约 2 分钟）。
- WP-32：`kurin artifact` 现在产出 OCI layout（旧目录树由 WP-34 兼容读取）；镜像按所选架构携带子 manifest，上游 index 原样保留；修复 busybox URL 模板（Docker Hub tag 无 `v` 前缀）。
- WP-53：`STALE_AFTER = 180s`；`GET /clusters` 沿用后端 snake_case（前端 camelCase 对齐留给 WP-54/55 ts-rs）；`connect()` 的 log 回调加 `Send`。
- WP-54：`POST /configs/render` 对不完整配置返回 200+TOML+诊断（merge 内部校验失败时从 ConfigError 携带的源文本恢复文档），422 仅留给无法产出文档的输入；plan/package/addon 操作通过 Operation 落地，`plan` 与三种 package kind 纳入只读豁免（不加集群锁、不占用 one_active_op）；包操作挂合成集群 `packages`（Draft，仅作持久化锚点）；内置 addon 目录为空（WP-46），catalog 同时聚合已登记集群的 `[addons.*] source`。
- WP-52：schema V3 让 `one_active_op` 排除 precheck/status（只读操作不加锁）；操作 ID 用时间戳+随机后缀；EventBus 丢弃事件时以 seq 跳变等价实现 Lagged 断开；新增 `serve --max-concurrent-ops`。
- WP-51：本 WP 只提供 Rust store API，未接入 HTTP/AppState（WP-52 集成）；`ApiError::Internal` 为增量变体。
- WP-44：`info` 增加 `-f`（dir source 需要）；`upgrade`/`remove` 增加 `--no-save`；`--version` 必须与目录当前版本一致；`list -f` 集群不可达时降级为告警。
- WP-43：内置 addon 目录为空（WP-46），已知内置名在 `resolve_selection` 中延后并告警；`ApplyRequest.helm` 改为 Option；`CONTROL_ARTIFACTS` 特例移除，helm 由引擎 `artifacts()` 声明。
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

### 已执行（本机，2026-10-28）
- WP-62/63/66：真实 `kurin serve` + tier1 上跑 `pnpm test:e2e:real`（lifecycle + resilience 5 个 spec）：9 步向导创建、创建中关浏览器重开继续、serve 重启 interrupted→retry、CLI 抢锁、密码明文扫描，全部 PASS。
- WP-71：tier1 cert timer 安装、`status` 下次续期、手动触发续期 + 静态 pod 轮转 + `/readyz`，PASS。
- WP-72：tier1 k3s 1.33.1 → 1.34.1（手工离线 bundle），`k3s --version` / node Ready / `/readyz`，PASS。
- WP-73：tier1 `kurin import --from-cluster` 重建 v1alpha2 配置，`status` 可用，PASS。
- WP-74：Ubuntu 22.04.5、AlmaLinux 9.8、openEuler 24.03 的 tier1 在线安装，PASS。
- WP-75：tier1 CIS 集群创建；kube-bench v0.10.7：default 64 pass/15 fail/51 warn，CIS 70 pass/11 fail/49 warn。
- WP-76：hydrophone v0.8.0 在 tier1 k3s 单节点：421/424 PASS，报告 `docs/conformance/20261028T074500Z/`。
- WP-77：tier1 无 registry 节点、`file://` base + os 包、`delivery = "seed"` 创建，PASS。

### 仍无法在本机执行
- **WP-74 Kylin V10**：无公共 qcow2，镜像只在厂商门户；脚本已就绪，需本地提供 qcow2。
- **WP-74 各 OS 离线变体**：需为每个 OS 构建 os 包并断网跑（`tier1-*-offline` 脚本已就绪）。
- **WP-72 `just tier1-k3s-upgrade` 原脚本**：需要制品清单 pin 目标 k3s release；本次用等价的手工离线 bundle 验证了升级链。
- **WP-76 tier3 多节点 conformance**：单节点 tier1 的 3 个失败中「至少两个 untainted node」要求 ≥2 节点；完整门禁需 tier3（约 2h）。
- **WP-75 kube-bench 正式阈值**：报告已出，阈值需维护者按 kube-bench 版本/适用项确定。
- **WP-64 页面安装/卸载 metrics-server**：Playwright mock 已覆盖，tier1 真实 UI 操作未跑。
- **WP-65 tier2 页面升级一个小版本**：Playwright mock 已覆盖，tier2 真实 UI 升级未跑。
- **WP-71/WP-73 的 tier2 多控制面场景**：已在 tier1 验证等价路径，tier2 未跑。

## 待决问题

（无）

## 环境备注

- 本机无 Rust 工具链，已安装 rustup stable（`~/.cargo/bin`）与 `just`；所有命令需先 `export PATH="$HOME/.cargo/bin:$PATH"`。
- 真机 e2e 依赖 multipass；本机 UFM 在运行，禁止改动主机网络配置。