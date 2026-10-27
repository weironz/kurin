# 命令 × 形态支持矩阵

每一列是一个配置维度。`✅` = 真机 e2e 验证过；`❌ 拒绝` = 配置/命令校验时明确报错（诚实失败，不静默腐蚀）。

## create cluster

| 形态 | containerd | crio | k3s | 离线 | zone=cn |
|---|---|---|---|---|---|
| Calico | ✅ | ✅ | —（k3s 默认 canal）| ✅ | ✅ |
| Flannel | ✅ | ✅ | ✅ | ❌ 拒绝 | ✅ |
| Cilium | ✅ | ✅ | ✅ | ❌ 拒绝 | ✅ |
| Kube-OVN | ✅ | ✅ | ✅ | ❌ 拒绝 | ✅ |
| kube-vip HA | ✅ | ✅ | ✅ | ✅ | ✅ |
| kurin-lb HA | ✅ | ✅ | — | — | ✅ |
| external-endpoint HA | ✅ | ✅ | ❌ 拒绝（未验证组合）| — | — |
| arm64 | ✅ | — | ✅ | ✅ | ✅ |

## add nodes / delete node / delete cluster

| | k8s（kubeadm） | k3s |
|---|---|---|
| add nodes | ✅（动态版本贯穿）| ✅ |
| delete node | ✅（计划：drain + delete-node-object + `kubeadm reset` + 清理）| ✅（计划：`k3s-uninstall` + 停服务 + `/var/lib/rancher`、`/etc/rancher`）|
| delete cluster | ✅（workers 先、control-plane 后）| ✅ |
| `delete --dry-run [--check]` | ✅（计划树 / 漂移）| ✅ |
| 单节点挂 node | ✅ | ✅ |

删除的每一步都是计划中的任务，并带收敛判断（例如目录已不存在即
`unchanged`）。清理步骤是显式的 `BestEffort`：失败会发 `task-warn`
（终端 `warn:`）并在结尾按节点汇总，命令本身仍以 0 退出。锚点控制面
不可达时 API 侧步骤（drain / delete node）跳过并告警，节点本身照常清理。

## upgrade

| | k8s（kubeadm） | k3s |
|---|---|---|
| 常规升级 | ✅ | ❌ 拒绝（换二进制或按目标版本重装）|
| 跨 minor +1 | ✅ | ❌ 拒绝 |
| `upgrade --offline <dir>` | ✅ | ❌ 拒绝 |

## 互斥与续跑

变更操作（`create cluster`、`add nodes`、`delete node`、`delete cluster`、
`upgrade`、`certs renew`）对同一集群互斥：锁文件位于
`$XDG_STATE_HOME/kurin/locks/<cluster>.lock`（回退 `~/.local/state`），
第二个进程立即失败并显示持有者的 pid、操作 id、命令行与开始时间。
只读命令（`status`、`precheck`、`certs check`、`logs`、`artifact`、
`config`）不加锁；`--dry-run` 同样不加锁。

`create cluster --resume` 会校验计划指纹（任务 key + `describe()` 的
sha256）。计划变化时拒绝续跑并提示去掉 `--resume`；v1 状态文件没有
指纹，仍可续跑。

## status（以首控制面为锚）

| | k8s | k3s |
|---|---|---|
| node / pod 摘要 | ✅ | ✅（`k3s kubectl` + k3s kubeconfig）|
| etcd 健康 | ✅（pod 计数）| ✅（`/healthz/etcd`）|
| 证书剩余天数 | ✅（`/etc/kubernetes/pki`）| ✅（`/var/lib/rancher/k3s/server/tls`）|

## certs check / renew

| | k8s | k3s |
|---|---|---|
| check | ✅ | ✅ |
| renew | ✅（`kubeadm certs renew`）| ✅（`k3s certificate rotate` + restart + `/readyz` 门控）|

## artifact / curl | sh

`--arch` 接受逗号分隔列表（如 `--arch amd64,arm64`）：混合架构集群的
离线包需要覆盖全部涉及架构；缺省为控制机架构。

| | amd64 | arm64 |
|---|---|---|
| `artifact --out <dir>` | ✅ | `--arch arm64` |
| `install.sh` | ✅ | ✅ |

## logs

每次操作（`create cluster`、`add nodes`、`delete node/cluster`、`upgrade`、
`certs check/renew`、`precheck`、`status`）都会把事件信封写入
`$XDG_STATE_HOME/kurin/logs/<cluster>/<op-id>.jsonl`（回退
`~/.local/state`），并在操作开始时把路径打印到 stderr 一次。

| 参数 | 行为 |
|---|---|
| `--cluster C` | 指定集群；只有一个集群有日志时可省略 |
| `--op ID` | 按操作 id（或唯一前缀）选择日志文件 |
| `--last` | 读取最新一次操作（缺省行为） |
| `--follow` | 持续输出新追加的信封 |
| `--node N` | 只显示属于该节点的行（含远端 stdout/stderr） |
| `--level L` | `trace`/`debug`/`info`/`warn`/`error`，缺省 `info`；`-vv` 等价于 `debug` |
| `--json` | 每行输出一个完整 v2 信封，便于管道处理 |

全局参数：`--timestamps`（终端日志加 UTC 时间戳）、`-v`（info 级进度 + 每条命令的远端输出摘要）、
`-vv`（全部远端 stdout/stderr）、`--json`/`--json=v1`（冻结的 v1 事件，
新增 `ts` 字段）、`--json=v2`（完整信封）。

## 矩阵之外的恒真约束

- 所有 kubectl/k3s 命令经 session 下发到锚点节点执行；控制机（跑 kurin 的机器）无需安装任何集群组件。
- `❌ 拒绝` 是**校验期**的明确报错，不是运行到一半失败。
- kurin-lb / external-endpoint 与 distribution 正交；`role = "lb"` 节点只在这两种模式下出现。
- 离线安装仅对 Calico 完整（其它 CNI 的镜像不在 bundle 内，校验期拒绝）。
