# 进度日志

## Kube-OVN + haproxy 细化 + CI 手动化（2026-10-26）✅

- **Kube-OVN CNI**（`network.cni.kind = "kube-ovn"`）：上游只发 helm chart，故把 v1.16.15 的
  `helm template` 输出固化进 `kurin/templates/cni/kube-ovn.yaml`（39KB），运行时按集群
  CIDR/注册表改写。**上游 install.sh 的隐式步骤**：给控制面打 `kube-ovn/role=master`
  标签（否则 ovn-central 永久 Pending、控制器连不上 NB）——已内置。
  tier-1 e2e PASS（controller/ovs-ovn/ovn-default subnet 全部收敛、pinger 网关可达、二遍全 skip）。
  明确限制：kube-ovn 镜像未入离线 bundle，离线安装校验拒绝（诚实失败）。
- **haproxy 细化**：后端健康检查从裸 TCP 改为 HTTPS `GET /readyz`（`http-check expect
  status 200` + `check-ssl verify none`，数据面保持 TCP 透传）；`:8404/haproxy-stats`
  统计页；e2e 用 `;csv=true` 断言全部后端 UP。
- **实机 triage 三连**：minijinja 默认丢弃模板末尾换行 → haproxy 2.8 视为截断拒绝启动
  （`set_keep_trailing_newline(true)` + 回归测试）；`default-server ssl` 会把客户端 TLS
  二次包一层（已移除，仅 check-ssl）；e2e stats 断言中的 awk 字段错位。
- **CI 手动化**：check/mirrors 改为仅 `workflow_dispatch`（push 触发的 CI 与本机 e2e
  抢内存导致连续失败堆积）；release 保留 tag 触发并加手动入口。

## Tails 收尾（2026-10-26）✅ 全部完成

四轨并行 + 集成，全部真机验证：

| 尾项 | 结果 |
|---|---|
| CRI-O 运行时（`kubernetes.runtime = "crio"`） | tier-1 PASS：node runtime `cri-o://1.33.4`；containerd 制品自动跳过；criSocket 切换；启动竞态修复（同 stage 前置等待） |
| k3s 离线（airgap tar） | tier-1 断网 PASS：airgap tar 落 `/var/lib/rancher/k3s/agent/images/`，二遍全 skip |
| k3s × kurin-lb | tier-3 PASS：2 CP + 1 LB，keepalived 持 VIP、无 kube-vip pod、VIP /readyz、全 pod 健康 |
| 离线升级（`artifact --with-kubernetes` + `upgrade --offline`） | tier-2 断网升级 1.33.13 → 1.34.12 PASS，二遍 up-to-date |
| k3s arm64 资产名 | `{asset_arch}` 模板变量；arm64 bundle 实测（k3s-arm64 二进制下载校验通过） |
| 安装报告 | `post/summary` 哈希门控（报告变化才重报，不破幂等）；tier-2 二遍全 skip 已回归 |

集成修复（e2e 暴露）：plan 的 runtime gate 两次静默未生效（crio 集群仍装 containerd）；
k3s 分支 ExternalLb 重复规划（stage_tasks 已含）；Summary "always drift" 破坏二遍幂等；
crio unit 与安装/配置并行启动竞态。

## 文档补齐（2026-10-26）
## M2 步骤 1 — upgrade ✅（2026-10-26）

（M2 后续全部任务清单见 `docs/roadmap.md`：批次 1 证书轮转/CP 追加/status；批次 2 国内镜像、CNI 矩阵、K3s、外部 LB；批次 3 跨 minor 升级、离线增强、夜间 e2e 备选方案。约定：e2e 全手工，主线功能开发。）

`kurin upgrade -f cluster.toml --with-kubernetes vX.Y.Z [--dry-run] [--batch N]`

- **动态版本表**：kubeadm/kubelet/kubectl 的 sha256 从 `dl.k8s.io` 官方 `.sha256` 端点实时获取并校验下载（内容寻址），目标版本无需重编译。
- **顺序**：init 节点 `kubeadm upgrade plan/apply` → 其余控制面串行 `kubeadm upgrade node` + drain/uncordon → worker 按 batch 推进；每节点按 kubelet 版本幂等跳过。
- 实机验证（tier-2）：1.33.1 → **1.33.13** 成功，双节点 Ready；重跑全部 `up-to-date`。
- **重大 bug（e2e 挖出）**：直接 `cat > /usr/local/bin/kubelet` 会因该文件正被 kubelet 进程持有而 `ETXTBSY`，远端 cat 秒退后 russh 通道挂在已关闭窗口上，整个上传静默卡 300s。修复：所有二进制替换改为「临时名写入 + `mv -f` 原子改名」（install 与 upgrade 两条路径都改），并加 `/usr/local/bin/*.kurin-new` 清理。探针二分证明与大小无关、只与目标 inode 是否被占用有关。

## W2 — 发布工程与 CI（2026-10-26）

- **just 取代 Makefile**：`just -l` 列出全部 recipe；`check`（fmt + clippy -D warnings + 单测）、tier0-3、`tier1-offline`、`release-musl`、`ci-gate`。
- **musl 静态双架构**：x86_64 走 musl-gcc，aarch64 走 `cargo zigbuild`（zig 装在 /usr/local/bin）；`just release-musl` 产出 tarball + 裸文件名 sha256 sidecar。
- **install.sh**：`KURIN_BASE_URL` 可覆盖；本地 http 端到端实测（下载 → 校验 → 安装 → 可跑）。
- **CI（.github/workflows/）**：push/PR 跑 check + tier0，tag v* 跑 musl release（self-hosted runner；依赖 rustup+just+zig+docker）。
- **clippy 清零**：修 8 处真 lint，`result_large_err` 项目级 allow（理由注释在 kurin/src/lib.rs）。
- 国内镜像源维持留口不实现（2026-10-26 决策）。

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

## M1 步骤 4 — 多节点 ✅（2026-10-26）

- **join 真正接线**：bootstrap 在 init 后收割 `kubeadm token create --print-join-command` + `upload-certs` key，写入本地 `~/.cache/kurin/clusters/<name>/join.env`（密钥不进模板、不持久化到节点）；join 任务读取后以 flags 形式（`--node-name`/`--apiserver-advertise-address`/CP 的 `--control-plane --certificate-key`）执行——kubeadm 禁止 `--config` 与 discovery flags 混用。
- **host key 语义**：TOFU 记录失配时不再盲目重试 3 次，改为 fail-fast 并给出「VM 重建需删除 ~/.cache/kurin/known_hosts 对应条目」指引；harness 在自己管理的 IP 上主动清理旧条目。
- **CNI 竞态**：Calico ≥3.26 的 CRD 由 operator 运行时创建，`kubectl wait` 会撞上 nil conditions；两个 readiness wait 改为带超时的容错轮询。
- **tier-2 干净循环 PASS**（新 VM×2 → 安装 → 断言 → 幂等，exit 0，296s）：worker join 事件出现、双节点 Ready、worker podCIDR、全 pod Running、二遍 0 重跑。

## M1 步骤 5 — HA 控制面 ✅（2026-10-26）

新增 `vip` 与 `api-ready` 阶段：kube-vip 静态 pod 必须在 init 前落盘，join 前必须确认 VIP 后端真的在服务。

kubeadm 1.33 + VIP 的三个真实坑（都是 tier-3 暴露的）：

1. **admin RBAC 自举死锁**：1.33 把 admin.conf 移出 `system:masters`（组 `kubeadm:cluster-admins`），补建 CRB 的调用走 controlPlaneEndpoint=VIP，而 VIP 需要 kube-vip 的 lease、lease 又要这个 CRB。解法：用 kubeadm 自带的 `super-admin.conf`（system:masters）经 `127.0.0.1` 补建（apiserver certSANs 增加 127.0.0.1）。
2. **post-init phases 静默缺失**：`upload-config`（kubeadm-config CM + token RBAC）、`bootstrap-token`（cluster-info CM + 匿名 RBAC）、`addon`（kube-proxy + CoreDNS）都经 VIP 访问 API，init 时不可达 → 全部缺失 → join 在 discovery/preflight 阶段饿死、CNI 无法调度。解法：init 后按序等待 API ready 并重放这三个幂等 phase。
3. **kube-vip address 语义**：v0.8 把 `address + vip_subnet` 拼成 CIDR，`vip_subnet` 必须带斜杠（`/32`）。

tier-3 干净循环 PASS（新 3 VM → 安装 → 断言 → 幂等，exit 0，~13 分钟）：3 节点 Ready、2 个 CP join、VIP `/readyz`、全 pod Running、二遍全 skip。

## M1 步骤 6 — 增删节点与销毁 ✅（2026-10-26）

实机验证（tier-2 集群）：

- `kurin add nodes -f`：新 worker 2 秒 join，既有节点/阶段全部 skip（同一条命令兼作 reconcile）。
- `kurin delete node <name> -f`：drain → kubectl delete node → kubeadm reset → 服务停用 → 制品清除；验证节点从集群消失且 `/etc/kubernetes`、kubelet 二进制被清空。
- `kurin delete cluster -f`：workers 先、控制面后逐个擦除 —— 两节点均确认 `/etc/kubernetes` 不存在、containerd/kubelet 停止、`/usr/local/bin` kubernetes 制品清空（含 containerd-stress），本地 join 材料与 run-state 一并删除。
- 修复：drain 在 NotReady 节点上会被优雅驱逐卡死，改用 `--disable-eviction --grace-period=0 --timeout=90s`。

## 下一步（M1 实施顺序）

1. ✅ Planner + dry-run + Task 引擎骨架（tier-0 PASS）
2. ✅ 制品模型 + 分发（tier-0 PASS）
3. ✅ 单节点 all-in-one 真实闭环（multipass 已装：snap + `sudo` 组 socket 认证；tier-1 PASS）

   - 修复并验证：cni kubectl 显式 `--kubeconfig`；CRD 由 operator 自建需轮询（Calico ≥3.26 operator manifest 不含 CRD）；exec 超时 120s→900s（kubeadm init ~55s + 镜像拉取）；kernel-deps 任务（apt/dnf 兜底 conntrack/socat/ebtables/ipset/ipvsadm）；kubeadm 节点名取自配置（`nodeRegistration.name`，缺省会用 hostname）。
   - tier-1 断言：cluster-ready 事件零 task-failed、node01 Ready（轮询）、podCIDR 为集群 /18 的 /24 切片、全部 pod Running（含 calico-system）、第二遍全 skip 幂等。
   - **干净环境全流程证明**（新 VM→create cluster→断言→销毁，exit 0，225s）：`make tier1-down && make tier1 && make tier1-test`。
4. ✅ 多节点（tier-2 PASS）；5. ✅ HA/kube-vip（tier-3 PASS）；6. ✅ add/delete nodes；7. ✅ 离线验收（tier-1-offline PASS）

## M1 步骤 7 — 离线部署 ✅（2026-10-26）

- **制品清单扩容到 23 项**：二进制 7 + calico-operator manifest + 镜像 15（kube-apiserver/controller-manager/scheduler/proxy、etcd、coredns、pause、calico 全家桶、tigera operator）。镜像以 OCI index digest 锁定，下载走 registry v2 API（quay.io 匿名可拉，Docker Hub 需 token 故统一 quay；Installation CR 设 `registry: quay.io` 使引用一致）。
- **`kurin artifact --out <dir>`**：经 sha256 校验抓取全部制品为离线 bundle（目录布局与在线缓存一致）。
- **`create cluster --offline <dir>` / `[artifacts] offline-dir`**：安装全程零下载——制品读 bundle、镜像以 docker-archive（内存 ustar 构造）经 `ctr -n k8s.io images import` 播入。
- 修复：bundle 目录名解析改为清单驱动（`etcd 3.5.21-0` 带连字符的版本曾致 naive split 错位）。
- **tier-1-offline PASS**：VM 出站到全部制品源 IP 被 iptables REJECT 的前提下，单命令装出 Ready 集群、全部 pod 健康、二遍全 skip（镜像导入幂等按参考集合核对）。

## M1 交付总结（全部 7 步完成）

| 验收 | 结果 |
|---|---|
| tier-0（systemd 容器，幂等双遍） | PASS |
| tier-1（单节点 all-in-one，干净循环 225s） | PASS |
| tier-2（1CP+1worker，join+幂等，296s） | PASS |
| tier-3（3CP+kube-vip HA，干净循环） | PASS |
| tier-1-offline（断网安装+幂等） | PASS |
| 单元测试 | 71 全绿 |

已知留白：join 任务需要控制面 bootstrap token 的跨节点协作（kubeadm token create → worker join flag 注入，M1 步骤 3 接线）；`delete cluster` 命令未实现。
