# 配置参考（schema v1alpha1）

配置是**声明式**的：只描述"想要什么"，执行顺序由 planner 决定。TOML 为主格式，
YAML（`.yaml`/`.yml`）同构支持。未知字段直接报错（防拼写错误静默生效），
版本不匹配报错（`version = "v1alpha1"` 是当前唯一值）。

完整字段一览，按段组织：

```toml
version = "v1alpha1"          # 必填，schema 版本
name   = "prod"               # 集群名（kubeconfig context、状态文件、日志用）

# ── kubernetes ─────────────────────────────────────────────────
[kubernetes]
version       = "1.33.1"      # 目标版本（semver；缺省用编译期默认）
distribution  = "k8s"         # "k8s"（kubeadm 轨道，默认）| "k3s"

# ── network ────────────────────────────────────────────────────
[network]
pod-cidr     = "10.233.64.0/18"   # 缺省 10.233.64.0/18；k3s 用 10.42.0.0/16
service-cidr = "10.233.0.0/18"    # 缺省 10.233.0.0/18；k3s 用 10.43.0.0/16
dns-domain   = "cluster.local"    # 缺省 cluster.local

[network.cni]                    # 互斥 tagged enum；改 kind 需要重建集群
kind = "calico"                  # "calico" | "flannel" | "cilium" | "kube-ovn" | "none"
# offline bundle carries calico/flannel/cilium images; kube-ovn must be online

# ── ssh ────────────────────────────────────────────────────────
[ssh]
user = "root"                    # 缺省 root；节点级可覆盖
port = 22
become = "auto"                  # 非 root 登录的提权方式："auto" | "sudo" | "none"
become-password = "…"            # sudo 需要密码时使用（经加密通道 stdin 传入）

[ssh.auth]                       # 集群默认鉴权；节点级可覆盖
method = "private_key"           # "private_key" | "password"
key-path = "/root/.ssh/id_ed25519"
passphrase = "…"                 # 私钥口令（可选）

# 等价的密码形式：
# [ssh.auth]
# method = "password"
# password = "…"

# ── ha（高可用控制面入口） ─────────────────────────────────────
[ha]
mode = "kube-vip"               # "kube-vip"（默认）| "external-endpoint" | "kurin-lb"
vip  = "192.168.10.100"         # 见下表：不同 mode 下语义不同

# ── artifacts（制品来源） ──────────────────────────────────────
[artifacts]
offline-dir = "/srv/kurin-bundle"  # 离线安装：bundle 目录（与 --offline 等价）
zone = "cn"                        # 国内镜像；KURIN_ZONE=cn 环境变量优先

# ── registry（私有镜像仓库，可选） ─────────────────────────────
# [registry]
# kind = "zot"                  # docker-registry | zot | harbor
# port = 5000                   # harbor 默认 8443
# username = "kurin"            # harbor 只能是 admin
# password = "s3cret"
# data-dir = "/var/lib/kurin/registry"

# ── storage（动态存储，可选） ──────────────────────────────────
# [storage]
# kind = "local-path"           # local-path | nfs（nfs 需 server+path）
# default = true                # 是否作为集群默认 StorageClass
# server = "192.168.10.5"       # 仅 nfs
# path = "/exports/k8s"         # 仅 nfs

# ── nodes ──────────────────────────────────────────────────────
[nodes.master01]                 # 键即节点名：DNS label（小写/数字/-，≤63）
address          = "192.168.10.11"   # SSH 可达地址
internal-address = "192.168.10.11"   # 集群内通信地址（缺省 = address）
role   = "control-plane"        # "control-plane" | "worker" | "lb" | "registry"
user   = "root"                 # 节点级覆盖（缺省继承 [ssh]）
port   = 22
become = "auto"                  # 节点级提权覆盖（缺省继承 [ssh]）
become-password = "…"            # 节点级 sudo 密码覆盖（缺省继承 [ssh]）
labels = { "env" = "prod" }    # 生效方式见下  节点标签

[nodes.master01.auth]           # 节点级鉴权覆盖（结构同 [ssh.auth]）
method = "private_key"
key-path = "/root/.ssh/master01_key"
```

## 各段语义与校验规则

### `kubernetes.distribution`

| 值 | 行为 |
|---|---|
| `k8s`（默认） | kubeadm 轨道：binary-first 分发 kubeadm/kubelet/kubectl/containerd，`kubeadm init/join` 引导 |
| `k3s` | 单二进制 k3s；HA 走内嵌 etcd（`cluster-init`）。**限制**：cni 仅 `flannel`/`none`；HA 仅 kube-vip 模式；离线暂不支持（校验直接拒绝） |

### `network.cni`（互斥）

| kind | 实现 | 校验 |
|---|---|---|
| `calico` | tigera-operator manifest + Installation CR（pinned sha256） | — |
| `flannel` | 上游 kube-flannel.yml，pod-cidr 自动 patch | — |
| `cilium` | cilium-cli（制品分发）`cilium install --version 1.20.4`，k8sServiceHost/nativeRoutingCIDR 自动注入 | 仅 k8s 发行版 |
| `none` | 什么都不装（自带 CNI 的运维方） | — |

zone=cn 时全部镜像引用自动改写为国内 mirror（calico CR registry、flannel manifest、
cilium `--set image.repository`、kube-vip image）。

### `[ha]` 三模式

| mode | vip 语义 | kurin 做什么 | 校验 |
|---|---|---|---|
| `kube-vip`（默认） | kube-vip 静态 pod 持有的 VIP | init 前写静态 pod manifest 到每个 CP | vip 不得与节点地址冲突；单 CP 设 vip 报错 |
| `external-endpoint` | **你的 LB 的地址** | 不安装任何 LB 组件；仅校验 | ≥2 CP + 必须给 vip |
| `kurin-lb` | keepalived VRRP VIP | 在 `role = "lb"` 节点装 haproxy+keepalived（apt/dnf），haproxy :6443 → 全部 CP | ≥2 CP + ≥1 个 lb 节点 + vip；lb role 在其它模式下报错 |

注：`kube-vip` 模式下多 CP 无 vip 也允许（stacked 控制面，首 master 是 API 单点，
join 时 CLI 显式警告）。

### `[artifacts]`

- `offline-dir`：目录必须存在（`kurin artifact --out` 产出）。离线时全部制品
  （含控制面镜像、CNI manifest）从 bundle 读取，零网络请求。
- `zone = "cn"`：`KURIN_ZONE=cn` 环境变量优先于本字段。

### `[registry]`（私有镜像仓库）

三种实现（详见 `docs/registry-contracts.md`），装在 `role = "registry"` 的节点上：

| kind | 实现 | 说明 |
|---|---|---|
| `docker-registry` | CNCF distribution v3 | 单二进制 + systemd，最简 |
| `zot` | project-zot | OCI 原生，开 `docker2s2` 兼容 ctr 推送 |
| `harbor` | Harbor v2 离线安装器 | 唯一实现自带 UI；仅单 registry 节点；username 必须 admin |

- 校验：存在 `role = "registry"` 节点 ⇔ 必须有 `[registry]`；`harbor` 仅支持
  单个 registry 节点；username/password 必须成对
- TLS：kurin 生成一次自签 CA，给每个 registry 节点签发证书（SAN 含节点 IP）
- 认证：docker-registry/zot 用 htpasswd（bcrypt）；harbor 用 admin 密码
- 镜像源：`docker-registry`/`zot` 在 k8s 节点上安装 containerd mirror（已知
  上游未命中时回退公网）；`harbor` 因 project/repo 命名形状不做上游 mirror，
  控制面镜像走 `imageRepository`（seed 到 `library` 项目）
- 离线：zot / docker-registry / harbor 的安装包都进 `kurin artifact` bundle

### `[storage]`（动态存储）

| kind | StorageClass | 说明 |
|---|---|---|
| `local-path` | `kurin-local-path` | rancher local-path-provisioner（hostPath） |
| `nfs` | `kurin-nfs` | sig-storage nfs-subdir-external-provisioner |

- `default = true` 加 `storageclass.kubernetes.io/is-default-class` 注解
- `nfs` 必须提供 `server` + `path`（用户自备 NFS 服务器）
- 离线：local-path 的两个镜像（provisioner + busybox helper）入 bundle

### `[ssh.auth]` / 节点级 `[nodes.<n>.auth]`（含不同密码示例）

password 与 private_key 互斥（同一对 `[ssh.auth]` 不可能同时写两个）。鉴权解析
顺序：**节点 auth > 集群 `[ssh.auth]`**；两者都没有 → 报错
"`no ssh auth and no default [ssh] auth`"。

**每个节点一个不同密码**（不配全局 `[ssh.auth]`）：

```toml
version = "v1alpha1"
name = "pw-demo"

[ssh]
user = "root"
# 故意不写 [ssh.auth]

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"

[nodes.master01.auth]
method = "password"
password = "M@ster01-pass"

[nodes.worker01]
address = "192.168.10.21"
role = "worker"

[nodes.worker01.auth]
method = "password"
password = "W0rker01-pass!"

[nodes.worker02]
address = "192.168.10.22"
role = "worker"

[nodes.worker02.auth]
method = "password"
password = "An0ther-different"
```

**全局默认密码 + 个别节点覆盖 + 混用密钥：**

```toml
[ssh]
user = "ubuntu"

[ssh.auth]                 # 大多数节点的默认
method = "password"
password = "common-pass"

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"    # 继承 common-pass

[nodes.worker01]
address = "192.168.10.21"
role = "worker"           # 继承 common-pass

[nodes.worker02]
address = "192.168.10.22"
role = "worker"           # 覆盖

[nodes.worker02.auth]
method = "password"
password = "special-pass"

[nodes.worker03]
address = "192.168.10.23"
role = "worker"           # 鉴权方式可混用

[nodes.worker03.auth]
method = "private_key"
key-path = "/home/ubuntu/.ssh/id_ed25519"
```

两种写法都实测 `--dry-run` 解析与规划通过（2026-10-26）。

### `[ssh] become` / 节点级 `become`（非 root 用户提权）

kurin 的特权命令（写 `/etc`、`kubeadm init`、`systemctl` 等）默认按登录用户执行；
非 root 登录时按 become 策略包装成 `sudo`。解析顺序：**节点级 `become` >
集群 `[ssh] become` > `auto`**（`become-password` 同理）。

| 值 | 行为 |
|---|---|
| `auto`（默认） | 登录用户是 `root` 时原样执行；非 root 时包成 `sudo -n -- sh -c '<脚本>'` |
| `sudo` | 与 `auto` 相同（root 永不 sudo） |
| `none` | 从不提权；非 root 用户在 precheck 阶段失败（`ssh user is not root and become is disabled`） |

- `become-password`：sudo 需要密码时，命令改为 `sudo -S -p '' -- sh -c '<脚本>'`，
  密码经 SSH 加密通道的 stdin 传入，**不进命令行**，也不会出现在日志、事件与
  错误文本里（R8）。`become = "none"` 时配置 `become-password` 是配置错误，
  解析阶段直接拒绝。
- 文件写入（`write_file`）：先以登录用户身份写入 `/tmp/kurin-<随机>`，再
  `install -D -m <mode>` 到目标路径（需要提权时经 sudo），临时文件随后删除；
  所以 mode/属主在 root 与非 root 登录下行为一致。
- precheck 的 `sudo` 检查项基于 become 判定：root => ok；非 root + `none` =>
  fail；非 root + `auto`/`sudo` 时探测 `sudo -n true`，可用 => ok
  （"passwordless sudo"），否则配置了 `become-password` => ok（"sudo with
  configured password"），否则 fail 并给出可操作提示。
- 等待 WP-20（秘密引用）之前，`become-password` 只接受字面值。

示例（大多数节点用免密 sudo 的 `ubuntu`，个别节点覆盖）：

```toml
[ssh]
user = "ubuntu"
become = "auto"               # 缺省即 auto，写出来仅为示意

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"
# 继承 [ssh]：ubuntu + auto（该用户已配置免密 sudo）

[nodes.worker01]
address = "192.168.10.21"
role = "worker"
become = "sudo"                # 节点级覆盖
become-password = "s3cret"     # 该节点的 sudo 需要密码
```

### 节点名

### `labels`（节点标签）

配置里每节点 `labels` 是对该节点打上的 k8s metadata 标签。**不依赖 kubeadm
的 `nodeRegistration.labels`**（kubeadm v1beta4 接受该字段但不应用，已在
1.33 实测），kurin 改为 `post/labels` 任务经 kubectl API 汇总式打
（`kubectl label --overwrite`），所以：

- 每次 `create cluster` 都会把 config 的 labels 收敛到集群（改的、删的同步）；
- 幂等：一致时跳过；
- 只影响 config 声明的节点（`role = "lb"` 排除）。

### 节点名

DNS label 规则强制（小写字母/数字/`-`，不以 `-` 开头结尾，≤63）：kubeadm/k8s
对大写或下划线主机名的安装失败是经典坑，kurin 在解析层就拒绝。节点名会
成为 k8s 的 NodeName（`nodeRegistration.name` / k3s `node-name`）。

## 常见组合示例

### 最小单节点

```toml
version = "v1alpha1"
name = "dev"
[ssh.auth]
method = "password"
password = "rootpass"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
```

### 3 控制面 + 2 worker + kube-vip HA（calico）

```toml
version = "v1alpha1"
name = "prod"
[ha]
vip = "192.168.10.100"
[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"
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
[nodes.worker02]
address = "192.168.10.22"
role = "worker"
```

### 离线 + 国内出口受限

```toml
[artifacts]
offline-dir = "/srv/kurin-bundle"
```

（离线优先：offline-dir 存在时 zone 无关——没有任何下载。）

## CLI 与配置的关系

| 场景 | 用法 |
|---|---|
| 预览（不联网） | `kurin create cluster -f cluster.toml --dry-run` |
| 预览（探测漂移） | `kurin create cluster -f cluster.toml --dry-run --check` |
| 安装 | `kurin create cluster -f cluster.toml` |
| 离线 flag 覆盖 | `kurin create cluster -f cluster.toml --offline /srv/bundle` |
| 镜像 zone 覆盖 | `KURIN_ZONE=cn kurin …` |
| 增删节点 | `kurin add nodes -f …` / `kurin delete node <name> -f …` |
| 升级到任意版本 | `kurin upgrade -f … --with-kubernetes v1.34.12`（跨 minor 一次一级） |
| 销毁 | `kurin delete cluster -f …` |

事件流（`--json`）逐行 JSONL：`task-start/skip/done/failed/reconciled`、
`stage-done`、`cluster-ready`——e2e 与自动化消费的稳定接口。
