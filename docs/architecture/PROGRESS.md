# 实施进度台账

> 依据：[06-dev-plan.md](06-dev-plan.md)。分支 `dev/arch-v2`。
> 状态：todo / doing / done / blocked。每个 WP 完成时更新本表并随代码一起提交。

## 总表

| WP | 状态 | 提交 | 快照变化 | 需人工 e2e 命令 | 备注 |
|---|---|---|---|---|---|
| WP-00 行为基线快照 | todo | | | | |
| WP-01 修复 upgrade drain 判定 | done | 见提交 | 无（CLI 新增 `--force-drain-failure`） | `just tier2-upgrade-minor`（含人为 drain 失败场景） | 行为变更已写入 CHANGELOG；`run_offline` 保留旧签名 |
| WP-02 workspace 拆分 | todo | | | | |
| WP-03 提权 become/sudo | todo | | | `e2e/tier1/nonroot.sh` | |
| WP-04 节点 facts 与按节点架构 | todo | | | tier1/tier2 不回归；`e2e/tier1/mixed-arch.md` | |
| WP-05 流式 exec 与上传 | todo | | | `just tier1-offline` | |
| WP-06 事件 v2 / tracing / 日志 | todo | | | tier1 后 `kurin logs --last -vv` | |
| WP-07 RunContext 与 Blackboard | todo | | | `just tier2`、`just tier1-zone`、`just tier1-k3s` | |
| WP-08 SSH 安全与端口转发 | todo | | | `just tier0 tier0-test` | |
| WP-09 集群锁与 RunState v2 | todo | | | 双终端 `add nodes` 抢锁 | |
| WP-10 PlanBuilder / NodeSel | todo | | | | |
| WP-11 DAG 执行器 | todo | | | tier1/tier2/tier3 全量 | |
| WP-12 删除改为计划 | todo | | | `just tier1-k3s`、tier2 删 worker、tier3 删集群 | |
| WP-13 升级改为计划 | todo | | | `just tier2-upgrade-minor`、`e2e/tier2/upgrade-offline.sh` | |
| WP-14 kurin-ops | todo | | | `just tier2-certs`、`just tier2-status`、`just tier1-k3s-ops` | |
| WP-15 Component（一） | todo | | | | |
| WP-16 Component（二）与能力表 | todo | | | `just tier1-offline`、`just tier3-external-lb` | |
| WP-17 ClusterApi | todo | | | `just tier2-status`、tier1 标签 | |
| WP-18 就绪跟踪器 | todo | | | tier1 错误镜像诊断 | |
| WP-19 kurin-state | todo | | | tier2 后检查 ConfigMap | |
| WP-20 ClusterSpec 与秘密引用 | todo | | | | |
| WP-21 v1alpha2 与迁移 | todo | | | | |
| WP-22 JSON Schema | todo | | | | |
| WP-25 节点参数解析器 | done | 见提交 | 无 | 无 | 提前实施（与 WP-21 无代码依赖）；69 条表驱动用例 |
| WP-26 快速创建参数 | todo | | | `e2e/tier1/quick.sh`、`e2e/tier3/quick-ha.sh` | |
| WP-27 --set 与 add nodes 快速参数 | todo | | | tier2 `add nodes --worker` | |
| WP-28 --setup-ssh-key | todo | | | tier1 密码 + setup-key | |
| WP-30 OCI spike | todo | | | | |
| WP-31 BlobStore 与 OCI layout | todo | | | | |
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
| WP-60 前端脚手架 | todo | | | | |
| WP-61 集群总览与详情 | todo | | | | |
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

（无）

## 待人工 e2e

（随 WP 完成追加）

- WP-01：`just tier2-upgrade-minor`。额外验证：让某 worker 的 drain 失败（如带 PDB 的 Pod），确认升级在该节点前中止且节点保持 cordon；再用 `--force-drain-failure` 复跑确认告警与继续执行。

## 待决问题

（无）

## 环境备注

- 本机无 Rust 工具链，已安装 rustup stable（`~/.cargo/bin`）与 `just`；所有命令需先 `export PATH="$HOME/.cargo/bin:$PATH"`。
- 真机 e2e 依赖 multipass；本机 UFM 在运行，禁止改动主机网络配置。