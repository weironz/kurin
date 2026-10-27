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
| delete node | ✅（drain + kubeadm reset 清理）| ✅（k3s 语义清理：停服务 + `/var/lib/rancher`、`/etc/rancher`）|
| delete cluster | ✅ | ✅ |
| 单节点挂 node | ✅ | ✅ |

## upgrade

| | k8s（kubeadm） | k3s |
|---|---|---|
| 常规升级 | ✅ | ❌ 拒绝（换二进制或按目标版本重装）|
| 跨 minor +1 | ✅ | ❌ 拒绝 |
| `upgrade --offline <dir>` | ✅ | ❌ 拒绝 |

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

## 矩阵之外的恒真约束

- 所有 kubectl/k3s 命令经 session 下发到锚点节点执行；控制机（跑 kurin 的机器）无需安装任何集群组件。
- `❌ 拒绝` 是**校验期**的明确报错，不是运行到一半失败。
- kurin-lb / external-endpoint 与 distribution 正交；`role = "lb"` 节点只在这两种模式下出现。
- 离线安装仅对 Calico 完整（其它 CNI 的镜像不在 bundle 内，校验期拒绝）。
