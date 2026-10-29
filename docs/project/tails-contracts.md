# Tails 契约（2026-10-26，0388038 后）

集成者已冻结：`kubernetes.runtime = "containerd"|"crio"`（`RuntimeKind`，
k3s × crio 已被校验拒绝）；`{asset_arch}` URL/file 模板变量（amd64→""、
arm64→"-arm64"，k3s 条目已改用）；`artifact --arch` 已可用（跨架构 bundle）。

## 共享文件（集成者所有，同前）

main.rs / plan.rs / lib.rs / tasks/mod.rs / tasks/cni.rs / Cargo.toml /
justfile / manifest.json / kurin-config/** / e2e 已有脚本。

## 轨道与独占文件

| 轨 | 独占文件 |
|---|---|
| CRI-O | `kurin/src/tasks/cri_o.rs`、`kurin/templates/crio/`、`kurin/tests/cri_o.rs`、`e2e/tier1/crio.sh` |
| K3sTails | `kurin/src/tasks/k3s.rs`（独占）、`e2e/tier1/k3s-offline.sh`、`e2e/tier3/k3s-lb.sh` |
| OfflineUpgrade | `kurin/src/upgrade.rs`（独占）、`kurin/src/artifact_bundle.rs`（新：bundle manifest 落盘/读取）、`e2e/tier2/upgrade-offline.sh` |
| Summary | `kurin/src/tasks/post.rs` 的 Summary 任务部分（post.rs 中 NodeLabels 之后追加；与 KubeconfigPull 无冲突的独立 struct）、`kurin/tests/summary.rs` |

## 接口契约

```rust
// tasks/cri_o.rs
pub fn runtime_tasks(cluster: &Cluster) -> miette::Result<Vec<(Stage, Arc<dyn Task>)>>;
// Prepare 阶段：CrioInstall（二进制经制品清单条目 "crio"，见下）+ CrioConfig（模板落
// /etc/crio/crio.conf.d/）+ CrioUnit（enable --now）。禁用冲突：集成者会在
// plan.rs 里 runtime==Crio 时跳过 ContainerdConfig/ContainerdUnit，并在 kubeadm
// 模板的 criSocket 上切 crio（模板变量 runtime_socket）。制品清单条目由集成者加。

// artifact_bundle.rs
pub fn write_bundle_manifest(dir: &Path, manifest: &ArtifactManifest) -> miette::Result<PathBuf>;
// → <dir>/kurin-manifest.json
pub fn read_bundle_manifest(dir: &Path) -> miette::Result<ArtifactManifest>;
// 缺文件时报错并提示用 `kurin artifact --with-kubernetes vX --out <dir>` 重建
```

## 制品事实（上游已核）

- CRI-O 静态包：`https://github.com/cri-o/cri-o/releases/download/v<ver>/cri-o.{arch}.<ver>.tar.gz`
  （含 `bin/crio`、`bin/pinns`；1.33.x 最新补丁自取；两架构 sha256 从
  `.../cri-o.{arch}.<ver>.tar.gz.sha256sum`? —— 若无该文件，下载实测 sha 后写死）。
  conmon/crun 等 runt 依赖：Ubuntu24/Debian12 上 `crio` 静态包自带 handling，
  依赖由 KernelDeps 式 apt 兜底（conmon、containernetworking-plugins 即 cni-plugins 已有）。
- k3s airgap：`k3s-airgap-images-{arch}.tar.gz`（每版有 sha256 行，见
  sha256sum-{arch}.txt；amd64=e32338ce…）。安装位置
  `/var/lib/rancher/k3s/agent/images/`（k3s server 启动时自动导入）。
- 升级离线：bundle 需含目标版本 kubeadm/kubelet/kubectl + 镜像（空气间隙下镜像
  已在节点上、无需重拉——kubeadm upgrade 用本地镜像 tag 匹配，所以 bundle 里镜像
  条目只做存在性校验）。用 dynamic manifest + `write_bundle_manifest` 写盘。

## 验收（集成者串行）

crio → tier-1（runtime=crio 装 kubeadm 集群）；k3s-offline → tier-1 断网；
k3s-lb → 2 CP + 1 LB；upgrade-offline → tier-2 断网升级。

## 纪律

同前几波：独立 target dir、不跑 multipass、不碰共享文件、push 前 rebase。