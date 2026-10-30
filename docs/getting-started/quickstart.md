# 快速入门

三个最常操作的场景。不想写配置文件？见
[纯命令行快速部署](cli-quickstart.md)。每步可直接复制。全部在你自己的机器上完成
（装 kurin 的机器 = 操作机；要装集群的机器 = 目标节点）。

---

## 0. 一键安装 kurin

```bash
curl -fsSL https://raw.githubusercontent.com/weironz/kurin/main/scripts/install.sh | bash
```

干的事：从 GitHub Releases 拉 `kurin-<arch>-linux-musl.tar.gz`（musl 静态、无依赖），
sha256 校验后装到 `/usr/local/bin/kurin`。覆盖路径改 `KURIN_PREFIX`，固定版本改
`KURIN_VERSION=v0.1.0`。验证：

```bash
kurin --version           # kurin 0.1.0
kurin create config | head -20
```

---

## 1. 部署单节点集群（3 分钟）

假定：目标节点 Ubuntu 24.04 / Debian 12 / Rocky 9，`ssh root@<ip>` 已通（root 或 sudo-able）。

### 1.1 写配置

```bash
cat > demo.toml <<'EOF'
version = "v1alpha1"
name = "demo"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[ssh]
user = "root"
port = 22

[ssh.auth]
method = "private_key"
key-path = "~/.ssh/id_ed25519"

[nodes.node01]
address = "192.168.10.11"
role = "control-plane"
EOF
```

密码登录就用：

```toml
[ssh.auth]
method = "password"
password = "<root-password>"
```

### 1.2 先看计划（不联网、不碰节点）

```bash
kurin create cluster -f demo.toml --dry-run
# --dry-run --check 会真的 SSH 探测、标注每步 would-change / unchanged
```

### 1.3 安装

```bash
kurin create cluster -f demo.toml
```

结束时 kurin 已经把 admin.conf 合并进本地 kubeconfig（kubeconfig context
`cluster-demo`，自动切为当前 context）：

```bash
kubectl get nodes   # node01 Ready
kubectl get pods -A # kube-system 全 Running
```

### 1.4 撤销

```bash
kurin delete cluster -f demo.toml
```

删除的是**节点上**的 kubeadm/containerd/所有制品与配置。`--resume` 续跑任意中断的安装。

---

## 2. 部署高可用集群（3 控制面）

三种入口形态选一个。都先通读第 0、1.1 节——下面只写不同点。

### 2a. kube-vip（默认，最简单，不需要额外机器）

配置在单节点基础上加：

```toml
[ha]
vip = "192.168.10.100"        # 必须没被占用、不与任何节点地址相同
mode = "kube-vip"             # 默认；可以省略

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"

[nodes.master02]
address = "192.168.10.12"
role = "control-plane"

[nodes.master03]
address = "192.168.10.13"
role = "control-plane"

[nodes.worker01]
address = "192.168.10.21"
role = "worker"
```

安装与验证：

```bash
kurin create cluster -f ha.toml
kubectl get nodes                     # 3 个 control-plane + worker
kubectl --server=https://192.168.10.100:6443 get --raw /readyz   # VIP 通
```

kill 一个 master 再查一次：etcd 有 3 投票成员（`kubectl -n kube-system get pods -l component=etcd`）。

### 2b. 外部负载均衡器（你运维 LB，kurin 不碰它）

```toml
[ha]
vip = "192.168.10.100"
mode = "external-endpoint"
```

校验：≥2 个 control-plane + vip 必填。kurin 不在 LB 节点装任何东西；
API 走 LB 后端到各 CP 的 :6443。

### 2c. kurin 托管 LB（haproxy + keepalived，kurin 自己装）

给集群加一类节点 `role = "lb"`，kurin 只在它们上面装 haproxy + keepalived。
LB 节点不参与容器编排（不会跑 kubelet）：

```toml
[ha]
vip = "192.168.10.100"
mode = "kurin-lb"

[nodes.lb1]
address = "192.168.10.31"
role = "lb"

[nodes.lb2]              # 第二个 LB 自动成为 keepalived BACKUP（single-LB 单点可接受可以只留 lb1）
address = "192.168.10.32"
role = "lb"
```

装完后：

```bash
kubectl --server=https://192.168.10.100:6443 get --raw /readyz
multipass exec lb1 -- ip -4 addr | grep 192.168.10.100  # vip 在 lb1
```

### 2d. K3s 高可用（嵌入 etcd，无需保持 kubeadm 一致性）

```toml
[kubernetes]
version = "1.37.2"
distribution = "k3s"

[ha]
vip = "192.168.10.100"
# k3s 只支持 kube-vip 模式
```

CNI 只能 `flannel`（内嵌）或 `none`。前 3 台 `role = "control-plane"` 自动
`cluster-init`、嵌入 etcd 集群形成。

---

## 3. 离线部署与镜像源

### 离线（目标网络 → 不往外发）

在线机器上：

```bash
kurin artifact --out /srv/kurin-bundle   # 二进制+kubeadm 控制面镜像+Calico manifest 打包
```

把捆绑目录送达目标网络（scp/U盘/内网文件服务），再：

```bash
kurin create cluster -f cluster.toml --offline /srv/kurin-bundle
# 或写进配置： [artifacts] offline-dir = "/srv/kurin-bundle"
```

### 国内镜像

```bash
KURIN_ZONE=cn kurin create cluster -f cluster.toml
```

效果：registry.k8s.io → k8s.m.daocloud.io、quay.io → quay.m.daocloud.io、GitHub release →
gh-proxy 前缀。校验值不变——manifest 的 sha256 依旧兜底。

---

## 4. 常用操作（装完之后）

```bash
kurin status -f cluster.toml          # 节点/pod/etcd/证书总览，exit 0 = 健康
kurin certs check -f cluster.toml     # 每个 CP 的证书到期报告
kurin upgrade -f cluster.toml --with-kubernetes v1.37.2   # 在线升级（含跨 minor 一级）
kurin add nodes -f cluster.toml       # 追加节点（含追加控制面；新节点安装集群当前版本）
kurin certs renew -f cluster.toml --force                  # 轮转控制面证书
```

---

## 常见问题（命中全世界的同一批坑）

- **"invalid configuration: unknown field X"**：拼写错误。kurin 拒绝任何未知字段，
  行号提示。改对再跑。
- **"no ssh auth and no default [ssh] auth"**：节点或 `[ssh.auth]` 至少给一个
  鉴权。password 与 private_key 互斥不可同段并存。
- **"ha.vip must not collide with any node address"**：VIP 进了节点地址池，换一个。
- **"host key mismatch"**：目标节点系统重装过，`ssh-keygen -R <ip> -f ~/.cache/kurin/known_hosts`。
- **`Unknown` 后一直 fail**：ssh 中断或 kurin 被 kill 在 apply 中途。重跑同命令：
  前面未变动就变 `Skipped/Unchanged`，`--resume` 从断点续。
- **`pod network not ready, kubelet not healthy`**：改 CIDR 冲突
  （`pod-cidr` / `service-cidr` 不得重叠），或检查 swap/SELinux（preflight 会明报）。
- **k3s 找不到 pod**：k3s 集群没有独立 kubectl——`k3s kubectl`。

更多字段与规则见 [`configuration.md`](../reference/configuration.md)，集成新 CNI/HA/发行版
见 [`plugins.md`](../development/plugins.md)。
