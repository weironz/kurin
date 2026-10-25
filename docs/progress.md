# 进度日志

## W0 — 契约冻结（2026-10-25）✅

- **kurin-config**：`v1alpha1` schema（TOML/YAML、deny_unknown_fields、tagged enum、强类型）、四段流水线、miette 行列诊断。
- **kurin**：`Task`（check/apply + `Verified/Unknown`）、`Session`、petgraph 线性阶段图、dry-run 树渲染、rust-embed 模板、编译期版本表、CLI。

## W1 — 七轨并行（2026-10-26）✅

| 轨 | 交付 | commit |
|---|---|---|
| A kurin-config | DNS-label/地址唯一/CIDR 范围/vip 冲突校验、YAML 示例等价测试、11 快照 | `26ec316` |
| B templates | systemd unit、join 配置、calico CR、kube-vip 模板；消灭 format! YAML | `a85d2a2` |
| C artifact | 制品清单（9 项双架构 sha256 上游实抓）、缓存/下载/校验、离线 from_dir、11 单测 | `ca2672b` |
| D ssh | russh SshSession/Pool（复用/限额/退避/超时/TOFU）、docker 实测 3 用例过 | `8d0a9a4` |
| E engine | executor 重写（实时事件流、fail-fast、resume 状态文件、probe）、14 单测 | `b6421d3` |
| F e2e | tier-0 Makefile + 3 systemd 容器 + 断言脚本（降级策略） | `49d60f9` |
| G 版本表 | 全组件 sha256 收集脚本 + `ARTIFACT_SHA256` 表（版本核实无需改号） | `9daed33` |

## 集成（W1 → M1 步骤 1+2）✅

- tokio features 补 `process/fs/signal`；lib.rs 声明 `artifact`/`ssh`/`state`。
- plan.rs 接线 TrackB 新任务（ContainerdUnit/KubeletUnit/KubeVip）。
- **新增 `artifacts` 阶段**（preflight 与 prepare 之间）：`ArtifactsInstall` 任务经 ArtifactStore 取缓存/下载校验后，把 kubeadm/kubelet/kubectl/containerd/runc/crictl/cni-plugins 装到 `/usr/local`、`/opt/cni/bin`，marker 记录 sha256 供 check。并行下载全局串行化避免缓存竞态。
- main.rs 接真实执行路径：`SshSessionPool::from_cluster` → `probe`/`run_with`，`--json` 逐行事件、`--resume` 状态文件。
- 全 workspace 测试 72+ 全绿。
- **tier-0 全量 PASS（干净环境）**：3 systemd 容器，dry-run 确定性 + 阶段序列 + 首遍 18/18 settle（17 applied，1 因镜像自带状态 skip）+ 二遍 18/18 全 skip（幂等）。
- e2e 暴露并修复的 4 个真缺陷：containerd tgz `--strip-components` 剥错目录 → unit 指向消失的二进制；marker 不含安装配方版本 → 配方修复无法生效（marker 现为 `sha@r<N>`）；sysctl check 用 `stdout.trim()` 比带尾换行的 expected → 永久漂移；任务 id 前缀与 stage 不一致（`prepare/artifacts@` → `artifacts/install@`）。

## 下一步（M1 实施顺序）

1. ✅ Planner + dry-run + Task 引擎骨架（tier-0 PASS）
2. ✅ 制品模型 + 分发（tier-0 PASS）
3. ⏳ 单节点 all-in-one 真实闭环（multipass + kubeadm init；**本机无 multipass，等待安装或替代 VM 方案**）
4. 多节点；5. HA/kube-vip；6. add/delete nodes；7. 离线验收

已知留白：join 任务需要控制面 bootstrap token 的跨节点协作（kubeadm token create → worker join flag 注入，M1 步骤 3 接线）；`delete cluster` 命令未实现。
