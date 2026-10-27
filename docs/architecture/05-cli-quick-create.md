# 05 · 命令行快速创建集群

> 状态：定稿 v1.0 · 2026-10-27
> 需求：`kurin` 命令行直接传入节点角色、节点 IP、认证账号和密码，不写配置文件就能快速建集群。
> 参照：Sealos `sealos run … --masters … --nodes … -p`、KubeKey `kk create cluster --with-kubernetes`、kubeasz `ezctl new`（[01](01-landscape.md) §2.2）。

---

## 1. 原则（ADR-10）

1. **只有一个模型**：命令行参数解析后得到的是与配置文件**同一个** `ClusterConfig`（v1alpha2），走同一套默认值、同一套校验、同一条执行路径。不另写一套“快捷安装”逻辑。
2. **可以落盘**：每次快速创建都把等价的配置文件写出来（默认 `./<name>.kurin.toml`），之后的 `add nodes`、`upgrade`、`delete` 都基于它。
3. **秘密默认不进 argv、不进文件**：密码可以交互输入、从环境变量或文件读取；落盘的配置里只保存**秘密引用**（`{ env = "…" }` / `{ file = "…" }`），不保存明文。
4. **参数能覆盖配置文件**：`-f cluster.toml` 加上参数时，参数优先；`--set` 可以覆盖任意字段。
5. **先显示计划再执行**：不加 `--yes` 时，打印集群摘要和计划，需要确认（非交互终端必须加 `--yes`）。

---

## 2. 用法示例

```bash
# 单节点（本机 IP 或远端都行），交互输入 root 密码
kurin create cluster --name demo --control-plane 192.168.10.11 --ask-pass

# 3 主 3 从 + kube-vip，密码取自环境变量
export KURIN_SSH_PASSWORD='***'
kurin create cluster --name prod \
  --control-plane 192.168.10.11-13 \
  --worker 192.168.10.21,192.168.10.22,192.168.10.23 \
  --vip 192.168.10.100 \
  --ssh-user root --ssh-password-env KURIN_SSH_PASSWORD \
  --kubernetes 1.33.1 --cni cilium --addon metrics-server --addon ingress-nginx

# 非 root 用户 + 私钥 + sudo
kurin create cluster --name lab --control-plane 10.0.0.5 --worker 10.0.0.6-8 \
  --ssh-user ubuntu --ssh-key ~/.ssh/id_ed25519 --become

# k3s，离线包，kurin-lb 负载均衡节点，独立 registry 节点
kurin create cluster --name edge --distribution k3s \
  --control-plane 172.16.0.11-13 --worker 172.16.0.21-24 \
  --lb 172.16.0.31,172.16.0.32 --vip 172.16.0.100 --ha-mode kurin-lb \
  --registry 172.16.0.41 \
  --package file:///media/usb/kurin-offline-v1.33.1.tar.zst --ask-pass

# 节点逐个精确描述（名称、内网地址、单独的账号与密码）
kurin create cluster --name mixed \
  --node name=m1,address=10.1.0.11,internal-address=192.168.0.11,role=control-plane \
  --node name=w1,address=10.1.0.21,role=worker,user=ops,port=2222,password-env=W1_PASS,labels=zone=a \
  --node name=w2,address=10.1.0.22,role=worker,key=/keys/w2

# 只生成配置，不安装（与 create cluster 参数完全相同）
kurin create config --name prod --control-plane 192.168.10.11-13 --worker 192.168.10.21-23 --vip 192.168.10.100 > prod.kurin.toml

# 在配置文件基础上临时覆盖
kurin create cluster -f prod.kurin.toml --set network.pod-cidr=10.244.0.0/16 --cni calico

# 快速加节点（同时改写配置文件）
kurin add nodes -f prod.kurin.toml --worker 192.168.10.24-25
```

---

## 3. 参数定义

### 3.1 节点

| 参数 | 可重复 | 值 | 说明 |
|---|---|---|---|
| `--control-plane <IPS>` | 是 | IP 列表 | 逗号分隔；支持末段范围 `192.168.10.11-13`，以及完整范围 `192.168.10.250-192.168.11.5` |
| `--worker <IPS>` | 是 | 同上 | — |
| `--lb <IPS>` | 是 | 同上 | `role = "lb"`，需要 `--ha-mode kurin-lb` |
| `--registry <IPS>` | 是 | 同上 | `role = "registry"` |
| `--node <KV>` | 是 | `name=…,address=…,role=…[,internal-address=…][,user=…][,port=…][,password-env=…][,password-file=…][,key=…][,labels=k=v;k2=v2]` | 精确描述单个节点；也可与上面的参数混用 |
| `--node-name <STRATEGY>` | 否 | `role-index`（默认）\| `hostname` \| `ip` | 自动命名：`cp-1`、`worker-1`、`lb-1`、`registry-1`；`hostname` 会在 discover 阶段改用节点的主机名（需符合 DNS-1123，否则回退并告警） |

范围展开最多 512 个地址，超出报错；同一地址出现两次报错（复用现有的“地址重复”校验）。

### 3.2 认证

| 参数 | 说明 |
|---|---|
| `--ssh-user <U>` | 默认 `root` |
| `--ssh-port <P>` | 默认 22 |
| `--ssh-key <PATH>` | 私钥认证；`--ssh-key-passphrase-env <VAR>` |
| `--ask-pass` | 交互输入 SSH 密码（不回显），只存在于本进程内存 |
| `--ssh-password-env <VAR>` | 从环境变量读取；落盘为 `password = { env = "VAR" }` |
| `--ssh-password-file <PATH>` | 从文件读取（要求权限 ≤ 0600，否则告警）；落盘为 `{ file = "PATH" }` |
| `-p, --ssh-password <PW>` | 为对齐 Sealos/KubeKey 的习惯而提供；**会打印告警**（会进入 shell 历史和 `ps`）；落盘时**不写入**，改为提示用户补充秘密引用 |
| `--become` / `--become-password-env <VAR>` / `--ask-become-pass` | 非 root 时用 sudo 提权（ADR-03）；用户不是 root 时默认开启 `--become` |
| `--setup-ssh-key [PATH]` | 用密码登录后把控制机公钥写入节点 `authorized_keys`（默认 `~/.ssh/id_ed25519.pub`，不存在时生成），落盘配置改为私钥认证。kubeasz 的 ssh-copy-id 思路 |
| `--known-hosts <MODE>` | `tofu`（默认）\| `strict` \| `accept-new` |

### 3.3 集群

| 参数 | 对应配置 |
|---|---|
| `--name <N>` | `name`（必填，除非 `-f`） |
| `--kubernetes <V>` | `kubernetes.version` |
| `--distribution kubeadm\|k3s` | `kubernetes.distribution`（v1alpha1 中的 `k8s` 仍然接受） |
| `--runtime containerd\|crio` | `runtime.kind` |
| `--cni calico\|flannel\|cilium\|kube-ovn\|none` | `network.cni.kind` |
| `--pod-cidr`、`--service-cidr`、`--dns-domain` | `network.*` |
| `--vip <IP>`、`--ha-mode kube-vip\|kurin-lb\|external-endpoint` | `ha.*`；控制面 ≥ 2 且未给 VIP 时沿用现有告警 |
| `--zone global\|cn` | `artifacts.zone` / `KURIN_ZONE` |
| `--package <URI>`（可重复） | `packages.base` / `packages.os`（按包 kind 自动归类） |
| `--offline <DIR\|URI>` | 兼容旧参数，等价于 `--package` |
| `--addon <NAME>[=<VERSION>]`（可重复） | `addons.<name>.enabled = true` |
| `--addon-set <NAME>.<KEY>=<VAL>` | `addons.<name>.values.<key>` |
| `--set <PATH>=<VAL>`（可重复） | 任意字段，PATH 使用配置文件的 kebab-case 路径；VAL 按 TOML 字面量解析（`true`、`3`、`"x"`、`[1,2]`） |
| `--save <PATH>` / `--no-save` | 默认 `./<name>.kurin.toml`；文件已存在且内容不同时拒绝覆盖，需要 `--force-save` |
| `--dry-run [--check]`、`--yes`、`--json` | 与现有含义相同 |

### 3.4 组合规则

- 有 `-f`：先加载文件，再按“节点参数追加 → 具名参数覆盖 → `--set` 覆盖”的顺序合并。
- 没有 `-f`：从内置默认值出发，节点参数必填（至少一个 `--control-plane` 或带 `role=control-plane` 的 `--node`）。
- 合并结果走 `ClusterConfig` 完整校验，错误信息指向**参数**而不是文件（诊断里的源码文本换成重建出的等价 TOML，并标注“来自命令行参数”）。

---

## 4. 执行流程

```text
解析参数 → 合并成 ClusterConfig → 解析秘密（交互 / 环境变量 / 文件）
  → 校验（与配置文件路径相同）
  → [--setup-ssh-key] 推送公钥
  → discover（facts：架构、OS、sudo、资源）→ 打印集群摘要表
  → 计划（与 --dry-run 相同的树）→ 确认（--yes 跳过）
  → 写出配置文件（--save）
  → 执行（与 create cluster -f 完全相同）
```

确认前打印的摘要示例：

```text
cluster prod  (kubeadm v1.33.1, containerd, cilium, kube-vip 192.168.10.100)
NODE      ADDRESS         ROLE           ARCH   OS             CPU  MEM    AUTH
cp-1      192.168.10.11   control-plane  amd64  Ubuntu 24.04   4    8 GiB  password(env)
cp-2      192.168.10.12   control-plane  amd64  Ubuntu 24.04   4    8 GiB  password(env)
…
addons: metrics-server 0.8.0, ingress-nginx 4.13.0
config will be saved to ./prod.kurin.toml (secrets referenced, not stored)
proceed? [y/N]
```

---

## 5. 落盘配置示例

```toml
version = "v1alpha2"
name = "prod"

[kubernetes]
version = "1.33.1"

[network.cni]
kind = "cilium"

[ha]
vip = "192.168.10.100"

[ssh]
user = "root"

[ssh.auth]
method = "password"
password = { env = "KURIN_SSH_PASSWORD" }

[nodes.cp-1]
address = "192.168.10.11"
role = "control-plane"
# … cp-2、cp-3、worker-1…3

[addons.metrics-server]
enabled = true
[addons.ingress-nginx]
enabled = true
```

使用 `-p` 明文密码时，落盘配置写成 `password = { env = "KURIN_SSH_PASSWORD" }`，并提示“后续操作前请导出该变量”。

---

## 6. 实现要点

| 项 | 说明 |
|---|---|
| 位置 | `kurin/src/cli/quick.rs`（解析与合并）；`kurin-config` 提供 `ConfigPatch`（参数到配置的结构化补丁）与 `set_path(&mut toml_edit::Document, path, value)` |
| 共享 | `create cluster`、`create config`、`add nodes` 共用同一组 clap 参数结构（`#[command(flatten)] QuickArgs`） |
| 范围解析 | `kurin-config::iprange`，单测覆盖：末段范围、完整范围、跨段、IPv6 拒绝（IPv6 支持另立项）、上限、重复 |
| 交互 | `--ask-pass` 使用 `rpassword`（或同类小 crate）；非 TTY 时报错并提示使用 `--ssh-password-env` |
| 秘密引用 | 依赖 ADR-15 的 v1alpha2 `SecretRef`；在 v1alpha1 配置上使用快速参数时，落盘一律写 v1alpha2 |
| `add nodes` 改写配置 | 用 toml_edit 追加 `[nodes.x]`，保留注释；名称按现有最大序号递增 |
| 测试 | 参数 → 配置的快照测试（insta）；错误诊断快照；`--save` 不覆盖已有文件；`-p` 告警；端到端：tier1 用纯参数建单节点集群 |

---

## 7. 不做

- 不在参数里接受内联的私钥内容。
- 不提供“全局默认密码写进 `~/.kurin`”之类的持久化秘密功能。
- 不支持 IPv6 地址范围（IPv6 单栈/双栈集群另立项）。
