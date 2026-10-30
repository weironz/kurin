# 故障排查输出样本

本页是 [故障排查](troubleshooting.md) 每类现象的对应输出样本，用于确认
诊断信息确实可读、可脚本化。样本来自 2026-10-30 的 tier1 CIS 集群
（Kubernetes 1.33.1）与真实失败运行；地址/令牌已脱敏。

## P. `precheck --json`（安装前检查）

正常项与失败项都是每行一个 JSON 对象：

```json
{"check":"sudo","detail":"running as root","node":"node01","status":"ok","type":"precheck"}
{"check":"swap","detail":"off","node":"node01","status":"ok","type":"precheck"}
{"check":"port:6443","detail":"already in use","node":"node01","status":"fail","type":"precheck"}
{"check":"disk","detail":"8GiB free on / (images need ~5GiB+)","node":"node01","status":"warn","type":"precheck"}
{"check":"hostname","detail":"kurin-tier1 resolves to loopback (127.0.1.1)","node":"node01","status":"warn","type":"precheck"}
```

退出码非 0 时 stderr 给出汇总：

```text
× precheck found 6 failing check(s)
```

## S. SSH 失败

不可达节点在 `precheck` 的 stderr 中给出节点与地址（JSON 流无事件）：

```text
× discover failed on 1 node(s): node01: ssh node01 (root@192.0.2.1:22)
```

`Permission denied (publickey)`、`Host key verification failed`、
`sudo: a terminal is required` 都会作为该 SSH 错误的 detail 出现；
主机密钥缓存位于 `~/.cache/kurin/known_hosts`。

## ST. `status`（健康集群）

```text
CLUSTER STATUS
NODE   ROLE          INTERNAL-IP STATUS   RUNTIME            KERNEL
node01 control-plane 10.97.0.156 Ready    containerd://2.1.4 6.8.0-147-generic

etcd: healthy
version: v1.33.1 (matches kurin-state v1.33.1)
certificates: earliest expiry in 364 day(s)
certificates: auto-renewal not scheduled (no kurin-cert-renew.timer)
overall: healthy
```

NotReady、etcd 成员缺失、证书将过期、LB 后端全挂会替换对应的
`Ready`/`etcd`/`certificates`/`overall` 行，并给出下一步。

## C. `certs check`

```text
NODE     CERT                                NOT AFTER (UTC)      DAYS LEFT  MARK
node01   /etc/kubernetes/pki/apiserver.crt   Oct 30 00:33:33 2027 GMT  364
node01   /etc/kubernetes/pki/ca.crt          Oct 27 00:33:33 2036 GMT 3649
next scheduled renewal: none (install with `kurin certs schedule`)
```

## E. `task-failed`（`--json` 事件流）

```json
{"type":"task-failed","id":"prepare/kernel-deps@node01","node":"node01","stage":"prepare","reason":"os repository install failed (exit 1): username: ","ts":1793296371082}
```

同一操作后续的 `task-skipped` 会带 `reason`（被阻塞的任务）：

```json
{"type":"task-skipped","id":"join/kubeadm@worker01","node":"worker01","stage":"join","reason":"blocked by bootstrap/kubeadm-init@master01","ts":1793296371083}
```

## L. `logs --last --json`

`kurin logs --last --json` 输出与上面相同的 v1 事件行；`--json=v2` 额外带
`v`/`seq`/`op`/`body` 信封。`-vv` 把远端 stdout/stderr 逐行流到终端。

## W. Web UI 错误

Host 白名单拒绝（HTTP 403）：

```json
{"code":"forbidden-host","message":"Host 头不在允许列表：192.168.73.11:7080"}
```

登录链接指向 `0.0.0.0`、非回环明文监听、令牌丢失分别对应启动参数
`--public-host`、`--allow-insecure-http`/TLS、`--rotate-token`。

## 索引

| 排查项 | 样本 |
|---|---|
| precheck：os/swap/module/sysctl/port/memory/cpu/disk/clock/selinux/hostname | P |
| SSH：publickey / host key / sudo / 超时 | S |
| 安装：kubeadm init / CNI / NotReady / 镜像拉取 / 中断 / busy | E、ST、L |
| 升级/证书/etcd：drain / 版本未变 / 证书 / 恢复 | C、ST、L |
| Web UI：forbidden-host / 登录链接 / 非回环 / 令牌 | W |
| 日志与状态 | ST、L |
