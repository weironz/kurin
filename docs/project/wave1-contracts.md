# Wave 1 契约（子 agent 必读，先于一切改动）

冻结于 2026-10-26，commit `ff50dc6`。所有 wave1 轨道在此契约内编码。

## 已冻结的共享面（**不要改**）

- `kurin-config`：`[artifacts] zone = "cn"`、`ArtifactsConfig`、`Cluster { zone }`（已实现）。
- `kurin/src/zone.rs`：`Zone::{Global,Cn}`、`rewrite()`、`rewrite_image_ref()`（已实现）。
- `kurin/src/artifact.rs`：`set_zone()` / `zone()`、下载镜像改写（已实现）。
- `kurin/src/main.rs`、`plan.rs`、`lib.rs`、`Cargo.toml`、`justfile`：**集成者所有**。你要导出新模块，只写自己的文件；集成者负责 `pub mod` 与 CLI 接线。
- `Session` / `SessionPool` / `Event` / `Task` trait：语义不变，可加方法。

## 轨道与文件所有权

| 轨 | 文件（新增或独占） |
|---|---|
| A certs | `kurin/src/certs.rs`、`kurin/tests/certs.rs` |
| B status | `kurin/src/status.rs`、`kurin/tests/status.rs` |
| C mirror-ci | `scripts/check-mirrors.sh`、`.github/workflows/mirrors.yml`（新建，不改 check.yml） |
| D e2e | `e2e/tier1/zone.sh`、`e2e/tier2/add-cp.sh`、`e2e/tier2/certs.sh`、`e2e/tier2/status.sh` |

## API 契约（签名不得偏离）

```rust
// kurin/src/certs.rs
pub struct CertInfo { pub path: String, pub subject: String, pub not_after_utc: String, pub days_left: i64, pub is_ca: bool }
pub struct NodeCerts { pub node: String, pub certs: Vec<CertInfo> }
pub async fn check(cluster: &Cluster, pool: &Arc<dyn SessionPool>, warn_days: i64) -> Result<Vec<NodeCerts>>;
pub async fn renew(cluster: &Cluster, pool: &Arc<dyn SessionPool>, force: bool) -> Result<()>;
pub fn render(reports: &[NodeCerts], warn_days: i64); // 人读表格式到 stderr

// kurin/src/status.rs
pub struct NodeStatus { pub name: String, pub ready: bool, pub kubelet_version: String, pub roles: String }
pub struct StatusReport { pub healthy: bool, pub nodes: Vec<NodeStatus>, pub pods_not_running: Vec<String>, pub etcd_ok: bool, pub cert_min_days: Option<i64> }
pub async fn collect(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<StatusReport>;
pub fn render(r: &StatusReport); // healthy 时 exit 语义由 CLI 处理
```

## 实现要点（经验教训，别重复踩坑）

- 远端二进制替换一律"临时名 + `mv -f`"（ETXTBSY 会静默卡死 russh 通道，见 ssh.rs 注释）。
- kubectl 在控制面一律显式 `--kubeconfig /etc/kubernetes/admin.conf`；HA 集群 API 入口是 VIP，控制面本地操作用 `--server https://127.0.0.1:6443`（certSANs 已含）。
- 所有等待循环必须有界（轮询 + 超时上限），禁止裸 wait。
- 证书探测：`openssl x509 -in <crt> -noout -enddate -subject`；CA 判定 `-checkend` 不可用就解析 `CA:TRUE`。
- renew：`kubeadm certs renew all`（每 CP）→ 重启静态 pod（`mv manifests/*.yaml /tmp && sleep && mv 回`，四个：apiserver/cm/scheduler/etcd）→ apiready 门控 → 幂等（90 天内新证书跳过，除非 --force）。
- D 轨脚本模板：复用 `e2e/tier2/assert.sh` 的结构与清理约定；`just tierN-*` 之外的 recipe 由集成者加。
- 单测用 mock Session（参考 `kurin/tests/engine.rs` 的 MockSession），不起网络。

## 纪律（对所有轨道）

1. 只 `cargo check/test -p kurin`（或 config），带 `CARGO_TARGET_DIR=target/<track>`。
2. 不跑 multipass / 不改共享文件 / 不 rebase / 不 force push。
3. 完成即 `git add <自己文件> && git commit -m "<track>: ..." && git push`；冲突 `git pull --rebase` 重试。
4. 环境事实：rustup 在 `~/.cargo/bin`（先 export PATH）；主机 31G 内存紧张，勿编译全 workspace。
