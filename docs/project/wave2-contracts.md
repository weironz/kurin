# Wave 2 契约（子 agent 必读，先于一切改动）

冻结于 2026-10-26。集成者已完成（不要重复/不要改）：

- `kurin-config`：`CniConfig::{Calico,Flannel,Cilium,None}`、`KubernetesConfig.distribution = "k8s"|"k3s"`、`Distribution` enum、`Cluster.distribution`；k3s 仅允许 flannel/none、k3s 不支持离线（校验已加）。
- `tasks/cni.rs` 现为模块根：`pub fn plugin_task(cluster) -> Result<Arc<dyn Task>>` 分派；Calico 移至 `cni/calico.rs`；`cni/flannel.rs` 完整实现（CIDR patch + zone 镜像改写 + pinned manifest `flannel@0.27.2`）；`NoCni` 在模块根。`Cilium` 分派位已存在，当前返回错误——由本波接入。
- `artifact.rs`：`set_manifest_override()/install_manifest()`（进程级覆盖，供 add-nodes/upgrade 动态版本）；`ArtifactsInstall` 已改为读 `install_manifest()`。
- `Plan::build` 现在返回 `miette::Result<Plan>`（main/tests 调用点已改）。
- 制品清单新增（sha256 均上游实抓）：`k3s@1.33.1+k3s1`（binary，两架构）、`cilium-cli@0.20.1`（binary，两架构）、`flannel@0.27.2`（manifest）。
- `upgrade::manifest_for` 现签名：`manifest_for(&store, target, arch)` 只改 kubeadm/kubelet/kubectl 三件套的版本+sha。

## 共享文件所有权（不变）

`main.rs`、`plan.rs`、`lib.rs`、`tasks/mod.rs`、`tasks/cni.rs`（模块根）、`Cargo.toml`、`justfile`、`artifacts/manifest.json`、`kurin-config/**` —— 集成者所有。轨道只写自己文件。

## 轨道与文件

| 轨 | 独占文件 |
|---|---|
| Cilium | `kurin/src/tasks/cni/cilium.rs`（新） |
| K3s | `kurin/src/tasks/k3s.rs`（新） |
| UpgradeExt | `kurin/src/upgrade.rs`（扩展，独占）、`kurin/src/dynamic_manifest.rs`（新） |
| E2e | `e2e/tier1/flannel.sh`、`e2e/tier1/cilium.sh`、`e2e/tier1/k3s.sh`、`e2e/tier3/k3s-ha.sh`、`e2e/tier2/upgrade-minor.sh`（全部新建，不执行 multipass） |

## 接口契约

```rust
// tasks/cni/cilium.rs —— 集成者把 cni.rs 分派改为调它
pub fn plugin(cluster: &Cluster) -> miette::Result<Arc<dyn Task>>;

// tasks/k3s.rs —— 集成者在 plan.rs 加 distribution 分支调它
// 返回 k3s 特有任务（含 stage 归属）。通用 preflight/artifacts/sysctl/
// kernel-deps 仍走现有路径；k3s 场景集成者会跳过 containerd/kubelet
// 三个 prepare 任务。stage 取值: Prepare/Vip/Bootstrap/ApiReady/Join/Post。
pub fn stage_tasks(cluster: &Cluster) -> miette::Result<Vec<(Stage, Arc<dyn Task>)>>;

// dynamic_manifest.rs —— add-nodes(集成者)与 upgrade(UpgradeExt)共用
pub async fn k8s_manifest_for(target: &Version, arch: Arch) -> miette::Result<ArtifactManifest>;
```

## 关键事实与坑（全部实测过，别重复踩）

- **cilium-cli 已会装到 `/usr/local/bin/cilium`**（ArtifactsInstall 自动），直接用它 `cilium install --version <cilium镜像版本>`。cilium 镜像版本 ≠ cilium-cli 版本：CLI 0.20.x 装 cilium 1.20 系。
- k3s 单二进制也已被 ArtifactsInstall 装到 `/usr/local/bin/k3s`（manifest 里的名字就叫 k3s）。
- kubeadm 集群里 kubectl 在 CP 上要 `--kubeconfig /etc/kubernetes/admin.conf`；k3s 的 kubeconfig 是 `/etc/rancher/k3s/k3s.yaml`，kubectl 二进制 k3s 集群**没有**——用 `k3s kubectl`。
- k3s server token 在 `/var/lib/rancher/k3s/server/node-token`；HA 需首台 `--cluster-init`，kube-vip 静态 pod 目录是 `/var/lib/rancher/k3s/agent/pod-manifests/`。
- 跨 minor：kubeadm 只允许 +1 minor；目标版本的 CP 镜像 tag 可用目标版本 kubeadm 本地 `kubeadm config images list` 拿到（kubeadm 二进制制品已在手），digest 用 registry HEAD（经 zone 改写）匿名取。
- 二进制替换一律临时名+`mv -f`（ETXTBSY 死锁）；所有等待有界。
- 测试不起网络：mock Session 见 `kurin/tests/engine.rs`、`kurin/tests/certs.rs`。

## 验收（集成者串行真机执行）

flannel/cilium → tier-1 各一次；k3s → tier-1 + 3CP HA；跨 minor → tier-2（1.33.13 → 1.34.x）。

## 纪律

同 wave1：`CARGO_TARGET_DIR=target/track-<x>`、不跑 multipass、不改共享文件、commit+push、`git pull --rebase` 解冲突。