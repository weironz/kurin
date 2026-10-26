# 路线图（M1 交付后）

M1 已完成并验收（见 `progress.md`）：安装 / HA / 多节点 / 增删节点 / 离线 / 升级，
tier-0~3 + offline 五层 e2e 全绿，CI 为 check + tier0（push/PR）+ release（tag）。

本文件是 M2 的全部剩余任务清单。每项含验收标准、依赖与建议批次。
"人工 e2e"：按当前约定，multipass e2e 只在本地手工跑，不进定时任务。

## 批次 1 — 补齐 kubekey 语义必备项 ✅（2026-10-26 wave1 完成）

### 1.1 证书检查与轮转（`kurin certs check|renew`）✅
- `certs check`：读取 `/etc/kubernetes/pki`，报告每个证书的 `NotAfter`、
  剩余天数、SA/CA 层级；默认阈值 30 天，`--warn-days N` 可调。
- `certs renew`：`kubeadm certs renew all` + 逐控制面节点轮转
  kubeconfig、重启 apiserver/controller-manager/scheduler/etcd 静态 pod，
  健康门控（沿用 upgrade 的串行 + apiready 模式）。
- 幂等：`certs check` 不写；`renew` 对 90 天内新证书跳过。
- 验收：tier-2 集群下 `certs check` 输出全部证书与剩余天数；
  `--force` 轮转后 apiserver 仍 Ready、kubeconfig 认证可用。

### 1.2 `add nodes` 追加控制面成员（e2e 缺口）✅（并解禁无 VIP stacked 多 CP）
- 机制已具备（join 任务对 control-plane 走 `--control-plane --certificate-key`），
  但从未在实机上验证过"运行中集群追加 CP"。
- 验收：tier-2 起手（1 CP + 1 worker），追加第 2 个 CP，三节点 Ready、
  etcd member 数 = 2，`kubectl get nodes` roles=control-plane 正确。

### 1.3 集群健康探针（post 阶段 + `kurin status`）✅
- `kurin status -f cluster.toml`：节点 Ready、控制面组件可用、CNI 就绪、
  etcd quorum、证书剩余天数汇总（复用 1.1 的检查代码）。
- 安装/升级结束后自动跑一次（事件流输出），失败不算安装失败但明确告警。
- 验收：健康集群 exit 0；人为 stop kubelet 一台后 exit 非 0 并指出节点。

## 批次 2 — 部署形态扩展（每项独立，按需选择）

### 2.1 国内镜像源（`KURIN_ZONE=cn`）✅（wave1 完成，见 README）
- URL 重写 trait 已在 `artifact` 层预留（manifest URL 模板可整体改写），
  缺：镜像表数据 + 环境变量接线。
- 镜像源已验证（2026-10）：`k8s.m.daocloud.io` / `quay.m.daocloud.io` /
  `docker.m.daocloud.io` 与上游 digest 逐字节一致；GitHub 二进制走
  `gh-proxy.com`/`ghfast.top` 前缀。
- 必须带 **CI digest 比对任务**：每个 mirror 条目 HEAD + digest 对比，
  不一致告警（镜像源最大风险是悄悄换内容）。
- 验收：`KURIN_ZONE=cn` 下 tier-1 完整安装；mirror 校验 job 绿。

### 2.2 CNI 矩阵：Flannel ✅ / Cilium ✅ / Kube-OVN（明确推迟）
- 配置结构已就绪（`network.cni` 是 tagged enum，`Calico`/`None` 已存在）。
- 每个 CNI：manifest 进制品清单（sha256 pin）、`tasks/cni/` 子模块、
  探针（正确性 = Node Ready + DNS 解析 + Pod 间连通）。
- 验收：每种 CNI 至少 tier-1 通过一次；`kurin delete cluster` 清理干净。
- 建议顺序：Flannel（最简）→ Cilium（eBPF，需内核版本前置检查）。

### 2.3 K3s 发行版选项 ✅（单节点 + 内嵌 etcd HA + 离线 airgap + arm64 资产名；kurin-lb HA ✅）
- `kubernetes.distribution = "k8s" | "k3s"`；k3s 走单二进制 + systemd，
  不依赖 kubeadm，HA 用内嵌 etcd（3 节点）或 external DB。
- 影响面：bootstrap/join/cni 阶段的实现分支，制品清单加 k3s 二进制。
- 验收：单节点 + 3 节点 HA（k3s 内嵌 etcd）各跑通一次 tier-1/tier-3 变体。

### 2.4 外部 LB（haproxy + keepalived）✅
- 已实现 `ha.mode = "external-endpoint" | "kurin-lb"` + `NodeRole::Lb`，tier3 e2e 双模式通过。
- 剩余：k3s × kurin-lb 组合（校验暂拒）；haproxy stats/健康检查细化。

### 2.5 CRI-O 运行时 ✅
- `kubernetes.runtime = "crio"`（kubeadm 轨道；k3s×crio 校验拒绝）。制品 cri-o@1.33.4 静态包
  （GCS，双架构 sha 下载实测），containerd 制品自动跳过，kubeadm criSocket 切换。
  tier-1 e2e PASS（node runtime cri-o://1.33.4）。

## 批次 3 — 运维与打磨

### 3.1 版本矩阵（跨 minor 升级）✅（1.33→1.34 实测；add-nodes 动态版本已贯穿）
- 现在 `upgrade` 支持同 minor 内任意 patch（1.33.x → 1.33.y）。
- 跨 minor 需：`kubeadm upgrade plan` 的 minor 校验、kubelet 版本矩阵校验、
  CoreDNS/etcd 镜像版本随 minor 变化（当前制品清单固定 1.33 系镜像）→
  需要"按目标 minor 动态解析镜像版本"（kubeadm 的 `config images list`
  在本地即可算出，接入 upgrade 的 manifest_for）。
- 验收：1.33.13 → 1.34.x（下一个 minor）tier-2 升级成功。

### 3.2 离线增强 ✅
- `kurin artifact --arch arm64` 跨架构 bundle（实测 arm64 k3s bundle 成功）。
- 离线升级：`kurin artifact --with-kubernetes vX --out dir`（写 `kurin-manifest.json`）+
  `kurin upgrade --offline dir`。tier-2 断网升级 PASS（1.33.13 → 1.34.12，二遍幂等）。
- k3s 离线：airgap tar 入制品（`k3s-airgap`，optional）+ 分发到
  `/var/lib/rancher/k3s/agent/images/`。tier-1 断网 PASS。

### 3.3 报告与可观测 ✅（核心）
- `post/summary`：安装后打印 CLUSTER SUMMARY（节点/角色/版本/证书最早到期），带
  内容哈希门控（摘要变化才重报，保持"二遍全 skip"契约）；`to_json()` 单行供自动化。
- 事件流已有逐任务 ms。

### 3.4 夜间 e2e（**当前约定：手工，不做**）
- 备选方案已成型：`.github/workflows/nightly.yml`（cron + 串行
  tier1/offline/tier2/tier3 + 失败上传日志 + `if: always()` 清理，
  总时长约 40 分钟，硬截止 90 分钟）。
- 若未来恢复自动化：runner 需要 rustup + just + zig + docker + multipass
  （multipass 需 `sudo` 组 socket 认证），bundle 每周重建一次而不是每晚。

## 明确不做

- KubeSphere / ks-installer（2026-10-25 决策，永久）
- 通用插件系统 / playbook 语言（与 M1 架构精神相悖）
- Docker 运行时（dockershim 已死）
- 自动回滚的半状态事务（fail-fast + resume + delete cluster 是逃生门）

## 建议执行顺序

1. 批次 1（1.1 → 1.2 → 1.3）：一个会话量级，补齐 kubekey 的核心运维语义，
   且全部复用已有的 executor/health-gate/证书读取代码，风险低。
2. 批次 2.1（国内镜像）：目标用户价值最高，可与任何人并行（纯数据 + trait 接线）。
3. 批次 3.1（跨 minor 升级）：M2 最后一个"结构性"能力，做完 kubekey 平权。
4. 其余按真实需求触发（CNI/K3s/外部 LB/CRI-O 不做预设优先级）。