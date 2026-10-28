# 升级 Kubernetes

```bash
kurin upgrade -f cluster.toml --with-kubernetes v1.34.1 --dry-run
kurin upgrade -f cluster.toml --with-kubernetes v1.34.1
```

- 跨 minor 一次只允许 +1（kubeadm 的限制）；k3s 按发布支持。
- 控制面节点始终逐个滚动；worker 按 `--batch` 分批（默认 1）。
- `--offline <DIR|URI>` 使用离线 bundle/包，不联网下载。
- `--force-drain-failure`：`kubectl drain` 失败时继续升级该节点
  （不安全：节点上仍有 Pod 在运行）。

## kubeadm 任务链

每个节点：

```text
version-check → cordon → drain → install-bins → kubeadm-upgrade
→ restart-kubelet → ready-gate → uncordon
```

首个控制面单独执行，其余控制面逐个滚动，worker 按批。任一步失败会中止该
节点并阻塞后续节点/批次。

## k3s 任务链

```text
version-check → stop-service → install-bins → start-service → ready-gate
```

- `--with-kubernetes vX.Y.Z` 映射到 k3s release `vX.Y.Z+k3s1`；显式
  `+k3sN` 后缀原样保留。
- 旧二进制保留为 `/usr/local/bin/k3s.pre-upgrade-<ts>`（永不删除）。
- server 逐台滚动并等 `/readyz`；agent 按批，只等节点 Ready。
- 不 cordon/drain：k3s 官方升级路径就是停机换二进制。
- 在线升级要求制品清单 pin 的正是目标 release，否则报错并提示使用
  `--offline`；不会静默下载未校验的二进制。

## 离线升级

```bash
# kubeadm：bundle 目录或包
kurin upgrade -f cluster.toml --with-kubernetes v1.34.1 \
  --offline /srv/kurin-bundle

# k3s：bundle 需按每个 arch 携带目标 k3s 二进制与校验和
kurin upgrade -f cluster.toml --with-kubernetes v1.34.1 \
  --offline /srv/k3s-1.34.1-bundle
```

## 验证

```bash
kurin status -f cluster.toml
kubectl get nodes
kubectl get --raw /readyz
```

`status` 会比较 `kube-system/kurin-state` 记录的期望版本与 `/version`
观测版本。

## 回滚

kurin 不自动回滚 Kubernetes 版本（kubeadm 不支持降级）。升级前请：

- 备份 etcd（[etcd 备份与恢复](backup-restore.md)）；
- 保留旧版本的离线包；
- 在预发环境先跑一遍 `--dry-run`。

## 相关

- [k3s](k3s.md)
- [证书](certificates.md)：升级后确认证书未过期
- [CLI 参考 · upgrade](../reference/cli.md#upgrade)