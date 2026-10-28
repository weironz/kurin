# 故障排查

先跑这三条，通常能定位大部分问题：

```bash
kurin precheck -f cluster.toml
kurin create cluster -f cluster.toml --dry-run --check
kurin logs --last --level debug
```

`-vv` 会把远端 stdout/stderr 流到终端；`--json=v2` 适合喂给脚本。

## precheck 检查项

| 检查 | 失败/警告含义 | 处理 |
|---|---|---|
| `os` | 未测试的发行版 | 确认包管理器与包名；未知族会明确报错 |
| `swap` | 节点启用了 swap | `swapoff -a` 并注释 `/etc/fstab` 中的 swap 行 |
| `module:overlay` / `module:br_netfilter` | 模块不可用 | 安装内核模块包或换内核 |
| `sysctl:net.ipv4.ip_forward` / `bridge-nf-call-iptables` | 不可写/不存在 | kurin 会设置；`br_netfilter` 可加载时缺失属正常 |
| `port:6443` / `port:10250` 等 | 端口被占用 | 停掉旧集群/进程，或换节点 |
| `memory` / `cpu` / `disk` | 资源不足 | 控制面建议 ≥1700MiB、≥2 核、≥5GiB 可用 |
| `clock` | NTP 未同步 | 启用 chrony/systemd-timesyncd；TLS 握手会受影响 |
| `selinux` | enforcing | CNI 挂载可能需要 permissive；按发行版策略处理 |
| `hostname` | 不解析或解析到回环 | 修 `/etc/hosts` 或 hostname |

## SSH

| 现象 | 原因 | 处理 |
|---|---|---|
| `Permission denied (publickey)` | 密钥/用户不对 | 检查 `[ssh] user`、`key-path`、节点 `authorized_keys` |
| `Host key verification failed` | 节点重装后主机密钥变化 | 删除 kurin 的 TOFU 缓存条目：`ssh-keygen -R <ip> -f ~/.cache/kurin/known_hosts`；或改用 `--known-hosts accept-new` |
| `sudo: a terminal is required` | 非 root 且未配置免密 sudo | 配置 NOPASSWD，或用 `--become-password-env` / `--ask-become-pass` |
| 连接超时 | 防火墙/端口 | 确认控制机到节点 22 端口；节点间放行 6443/10250/2379-2380 等 |

## 安装阶段

| 现象 | 排查 |
|---|---|
| `kubeadm init` 失败 | 看 `kurin logs --last --level debug` 里的 preflight ERROR；常见是 swap、ip_forward、端口、cgroup |
| CNI Pod 一直不 Ready | `kubectl get pods -A`、`kubectl describe pod`；单节点首次拉镜像可能较慢，等待几分钟 |
| 节点 NotReady | `systemctl status kubelet`、`journalctl -u kubelet`；确认 containerd 正常 |
| 镜像拉取失败 | 在线检查网络/镜像源；离线检查包覆盖性与 `delivery` 模式 |
| 操作中断 | `kurin create cluster -f … --resume`；计划变化时去掉 `--resume` 重跑（幂等） |
| `cluster "x" is busy` | 另一个变更操作持有锁；等待或确认持有者已退出。锁文件在 `~/.local/state/kurin/locks/` |

## 升级/证书/etcd

| 现象 | 处理 |
|---|---|
| 升级卡在 drain | 检查 PDB、裸 Pod、本地存储；必要时 `--force-drain-failure`（不安全） |
| 升级后版本没变 | 确认目标版本与制品来源；k3s 在线升级要求清单 pin 目标 release |
| 证书快过期 | `kurin certs check`；`kurin certs renew`；长期用 `certs schedule` |
| etcd 恢复后 Pod 异常 | 等控制面重新选主；检查 `/healthz/etcd` 与 `/readyz`；旧数据在 `…-pre-restore-<ts>` |

## Web UI

| 现象 | 原因 | 处理 |
|---|---|---|
| `{"code":"forbidden-host","message":"Host 头不在允许列表：…"}` | 访问地址不在 `Host` 白名单 | 启动时加 `--public-host <host:port>`，例如 `--public-host 192.168.73.11:7080` |
| 登录链接指向 `0.0.0.0` | 非回环监听且没给 `--public-host` | 加 `--public-host <实际访问地址:端口>`；或改用 SSH 隧道 |
| 非回环监听启动失败 | 明文 HTTP 需要显式允许 | 加 `--allow-insecure-http`，或配置 `--tls-cert/--tls-key` |
| 令牌丢失 | 只打印一次 | `kurin serve --rotate-token` 重新生成 |

## 日志与状态

```bash
kurin status -f cluster.toml
kurin logs --last --level debug
kurin logs --op <id> --node node01 --follow
```

日志文件：`~/.local/state/kurin/logs/<cluster>/<op>.jsonl`。

## 仍然无法定位

收集以下信息再求助：

```bash
kurin --version
kurin precheck -f cluster.toml --json
kurin create cluster -f cluster.toml --dry-run --check --json=v2
kurin logs --last --json > op.jsonl
```

以及节点上的 `kubectl get nodes -o wide`、`kubectl get pods -A`、
`journalctl -u kubelet -n 200`。