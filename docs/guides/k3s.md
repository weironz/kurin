# k3s

把发行版切到 k3s：

```toml
version = "v1alpha2"
name = "edge"

[kubernetes]
version = "1.33.1"
distribution = "k3s"

[network.cni]
kind = "flannel"           # k3s 支持 flannel | none

[ssh]
user = "root"

[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"

[nodes.node01]
address = "192.168.10.11"
role = "control-plane"
```

## 与 kubeadm 的差异

| 能力 | k3s |
|---|---|
| CNI | `flannel`（内嵌）或 `none`；Calico/Cilium/Kube-OVN 不支持 |
| HA | 前 3 个控制面自动 `cluster-init`，嵌入 etcd；只支持 kube-vip 模式 |
| 运行时 | k3s 自带 containerd，不单独安装 |
| 升级 | 二进制替换 + 滚动重启（见下） |
| 删除 | `k3s-uninstall` / `k3s-agent-uninstall`，清理 `/var/lib/rancher`、`/etc/rancher` |
| 证书 | `k3s certificate check/rotate`，路径 `/var/lib/rancher/k3s/server/tls` |
| etcd 备份 | `k3s etcd-snapshot` |

`kurin status`、`certs check/renew/schedule`、`etcd backup/restore/schedule`、
`add nodes`、`delete node/cluster`、`import --from-cluster` 都支持 k3s。

## HA

```toml
[kubernetes]
version = "1.33.1"
distribution = "k3s"

[ha]
vip = "192.168.10.100"

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"

[nodes.master02]
address = "192.168.10.12"
role = "control-plane"

[nodes.master03]
address = "192.168.10.13"
role = "control-plane"
```

## 升级

```bash
# 目标版本映射到 k3s release vX.Y.Z+k3s1（显式 +k3sN 原样保留）
kurin upgrade -f cluster.toml --with-kubernetes 1.34.1 --dry-run
kurin upgrade -f cluster.toml --with-kubernetes 1.34.1
```

在线升级要求制品清单 pin 的正是目标 release；否则用离线 bundle：

```bash
# bundle 需按集群每个 arch 携带目标 k3s 二进制 + 校验和
kurin upgrade -f cluster.toml --with-kubernetes 1.34.1 \
  --offline /srv/k3s-1.34.1-bundle
```

每个节点：`version-check` → `stop-service` → `install-bins`（旧二进制保留为
`/usr/local/bin/k3s.pre-upgrade-<ts>`，永不删除）→ `start-service` →
`ready-gate`。server 逐台滚动并等 `/readyz`；agent 按 `--batch` 分批，只等
节点 Ready。已在目标版本的节点为 `InSync`，不停服务。

k3s 不执行 cordon/drain：官方升级路径就是停机换二进制。

## 验证

```bash
k3s --version
kubectl get nodes
kubectl get --raw /readyz
```

## 相关

- [命令 × 形态矩阵](../reference/command-matrix.md)
- [升级](upgrade.md)
- [离线安装](offline.md)