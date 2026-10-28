# 证书检查与续期

kubeadm 与 k3s 都支持。控制面证书路径：

| 发行版 | 路径 |
|---|---|
| kubeadm | `/etc/kubernetes/pki` |
| k3s | `/var/lib/rancher/k3s/server/tls` |

## 检查

```bash
kurin certs check -f cluster.toml
kurin certs check -f cluster.toml --warn-days 60
```

输出每个控制面的证书表与剩余天数，以及下次自动续期时间（如果装了
timer）。`--warn-days` 默认 30。

## 手动续期

```bash
kurin certs renew -f cluster.toml
kurin certs renew -f cluster.toml --force   # 证书看起来还新也续
```

流程：

- kubeadm：`kubeadm certs renew` → 静态 pod 轮转 → `/readyz` 门控，
  逐节点 `rolling(1)`。
- k3s：`k3s certificate rotate` → 重启 → `/readyz` 门控，逐节点滚动。

门控只看叶子证书表；CA 不计入（`certs renew all` 不会续 CA）。

## 定时续期

```bash
kurin certs schedule -f cluster.toml
kurin certs schedule -f cluster.toml --warn-days 30 --every 24h
```

在每个控制面安装：

- `kurin-cert-renew.service` / `kurin-cert-renew.timer`
- `/usr/local/bin/kurin-cert-renew.sh`

脚本检查最小剩余时间，小于 `--warn-days` 才续期并轮转静态 pod，最后做
`/readyz` 门控；日志写入 journald（`logger -t kurin-cert-renew`）。

`schedule` 是幂等的：重跑会重写同内容 unit/脚本并更新参数，然后重启 timer。
它抢集群锁。

验证：

```bash
systemctl list-timers kurin-cert-renew.timer
journalctl -t kurin-cert-renew
kurin status -f cluster.toml     # cert_next_renewal 前移
```

## 注意

- 续期会重启控制面静态 pod；在业务低峰执行。
- 多控制面逐个滚动，不会同时中断所有 API server。
- 证书过期后 `kubectl` 会失败；先用 `certs check` 确认，再续期。
- k3s 旧版本没有 `certificate rotate` 时，脚本会在日志中给出手工升级指引。

## 相关

- [etcd 备份与恢复](backup-restore.md)
- [状态与日志](status-logs.md)
- [CLI 参考 · certs](../reference/cli.md#certs)