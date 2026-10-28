# etcd 备份与恢复

kubeadm 与 k3s 都支持。快照文件**未加密**，请按敏感数据保管。

## 备份

```bash
kurin etcd backup -f cluster.toml
kurin etcd backup -f cluster.toml -o /srv/backup/etcd.db
```

- kubeadm：通过 `crictl exec` 进入 etcd 静态 pod 执行 `etcdctl snapshot save`
  （回退 `kubectl exec`）；每个控制面在 `/var/lib/kurin/etcd-backups` 留一份，
  首个控制面的快照拉回控制机的 `-o` 路径。
- k3s：`k3s etcd-snapshot save`，节点本地保留 db/snapshots。
- 默认输出：`./etcd-<cluster>-<timestamp>.db`。
- `backup` 是只读操作，不抢集群锁。

## 恢复

```bash
kurin etcd restore -f cluster.toml --from /srv/backup/etcd.db
```

**破坏性操作**：集群状态回退到快照时刻。

- kubeadm：停静态 pod → 各成员 `etcdctl snapshot restore` 重建（单/多成员
  两路）→ 启动 → `/healthz/etcd` + `/readyz` 门控。控制机需要能获取
  etcdctl tarball（离线环境请提前准备）。
- k3s：`systemctl stop k3s` → 锚点节点 `k3s server --cluster-reset` → 重启 →
  其余 peer 停旧 db 重新 join。
- 旧数据不删除：移到 `…-pre-restore-<ts>`（kubeadm `/var/lib/etcd`，
  k3s `server/db`）。
- 多成员恢复要求配置与线上控制面拓扑一致（peer URL 由配置推导）。

## 定时备份

```bash
kurin etcd schedule -f cluster.toml --every 6h
kurin etcd schedule -f cluster.toml --every 1d --keep 14
```

在每个控制面安装 `kurin-etcd-backup.timer` + 节点本地脚本：

- kubeadm：`etcdctl snapshot save`；
- k3s：`k3s etcd-snapshot save`；
- 按 `kurin-` 前缀裁剪，保留最近 `--keep` 份（默认 7）。

`restore` 与 `schedule` 抢集群锁。

## 恢复后检查

```bash
kurin status -f cluster.toml
kubectl get nodes
kubectl get pods -A
```

## 相关

- [证书](certificates.md)
- [升级](upgrade.md)：升级前先备份
- [CLI 参考 · etcd](../reference/cli.md#etcd)