# 纯命令行快速部署

不写配置文件，直接用参数创建集群。`create cluster` 会把参数合并成一份
v1alpha2 配置，走与 `-f` 完全相同的校验、计划和执行路径。

## 完全不留配置文件

默认会写一份 `./<name>.kurin.toml` 作为等价配置。如果连这份文件也不想要，
加 `--no-save`：

```bash
kurin create cluster --name demo \
  --control-plane 192.168.10.11 \
  --ssh-user root \
  --ssh-key ~/.ssh/id_ed25519 \
  --no-save --yes
```

整个过程不读、不写任何 TOML/YAML 文件；配置只在进程内存中构建。代价是
后续 `status` / `add nodes` / `delete cluster` 没有配置文件可用——需要时用
`kurin create config ... --save demo.toml` 再生成一份，或重新带参数执行。

## 最小单节点

```bash
kurin create cluster --name demo --control-plane 192.168.10.11 --ask-pass
```

流程：

1. SSH discover（架构、OS、sudo、资源）；
2. 打印集群摘要；
3. 交互确认（回车继续）；
4. 默认把等价配置写到 `./demo.kurin.toml`；
5. 执行安装。

`--ask-pass` 的密码只存在于进程内存，落盘的是
`password = { env = "KURIN_SSH_PASSWORD" }`。后续操作（`status`、`add nodes`、
`delete cluster`）需要先导出该变量：

```bash
export KURIN_SSH_PASSWORD='...'
kurin status -f demo.kurin.toml
```

非交互环境（CI、脚本）必须加 `--yes`，否则 stdin 不是终端会直接报错：

```bash
kurin create cluster --name demo --control-plane 192.168.10.11 \
  --ssh-password-env KURIN_SSH_PASSWORD --yes
```

## 私钥 + 非 root

```bash
kurin create cluster --name demo \
  --control-plane 192.168.10.11 \
  --ssh-user ubuntu \
  --ssh-key ~/.ssh/id_ed25519 \
  --become
```

sudo 需要密码时：

```bash
kurin create cluster ... --become --ask-become-pass
# 或 --become-password-env KURIN_SUDO_PASSWORD
```

## 首次用密码，随后切私钥

```bash
kurin create cluster --name demo \
  --control-plane 192.168.10.11 \
  --ask-pass \
  --setup-ssh-key
```

`--setup-ssh-key` 先用密码会话把控制机公钥（默认 `~/.ssh/id_ed25519.pub`，
不存在时生成）追加到每个节点，验证私钥可登录后，落盘配置改为私钥认证。
它不能与 `--dry-run` 一起用。

## 3 控制面 + 3 worker + kube-vip

```bash
kurin create cluster --name prod \
  --control-plane 192.168.10.11-13 \
  --worker 192.168.10.21-23 \
  --vip 192.168.10.100 \
  --ssh-key ~/.ssh/id_ed25519 \
  --kubernetes 1.36.6 \
  --cni cilium \
  --addon metrics-server \
  --addon ingress-nginx
```

地址列表语法：

- 逗号分隔：`192.168.10.11,192.168.10.12`
- 末段范围：`192.168.10.11-13`
- 完整范围：`192.168.10.11-192.168.10.13`
- 单次展开上限 512；IPv6 明确拒绝。

需要精确控制单个节点时用 `--node`：

```bash
kurin create cluster --name prod \
  --ssh-key ~/.ssh/id_ed25519 \
  --node name=cp1,address=192.168.10.11,role=control-plane \
  --node name=w1,address=192.168.10.21,role=worker,labels=disk=ssd
```

## k3s

```bash
kurin create cluster --name edge \
  --control-plane 192.168.10.11-13 \
  --vip 192.168.10.100 \
  --ssh-key ~/.ssh/id_ed25519 \
  --distribution k3s \
  --cni flannel
```

k3s 只支持 `flannel` / `none`，HA 只支持 kube-vip 模式。

## CRI-O / 网络 / 镜像源

```bash
kurin create cluster --name prod \
  --control-plane 192.168.10.11-13 \
  --worker 192.168.10.21-23 \
  --ssh-key ~/.ssh/id_ed25519 \
  --runtime crio \
  --cni calico \
  --pod-cidr 10.233.64.0/18 \
  --service-cidr 10.233.0.0/18 \
  --zone cn
```

`--zone cn` 等价于配置里的 `[artifacts] zone = "cn"`。

## 离线包

```bash
kurin create cluster --name airgap \
  --control-plane 192.168.10.11 \
  --ssh-key ~/.ssh/id_ed25519 \
  --package file:///srv/kurin-base.tar.zst \
  --package file:///srv/os-ubuntu-24.04.tar.zst \
  --yes
```

- `--package` 可重复；路径含 `/os/` 或 `os-…` 名字的归入 `packages.os`，
  其余为 `packages.base`（base 最多一个）。
- `--offline <URI>` 含 `://` 时等价于 `--package`；不含时作为旧 bundle 目录
  写入 `artifacts.offline-dir`。

## 只生成配置 / 只预览

```bash
# 打印合并后的 v1alpha2 配置（不安装）
kurin create config --name prod --control-plane 192.168.10.11-13 \
  --worker 192.168.10.21-23 --vip 192.168.10.100 \
  --ssh-key ~/.ssh/id_ed25519 > prod.toml

# 只打印摘要与计划，不确认、不落盘
kurin create cluster --name prod --control-plane 192.168.10.11 \
  --ssh-key ~/.ssh/id_ed25519 --dry-run

# 真 SSH 探测漂移
kurin create cluster --name prod --control-plane 192.168.10.11 \
  --ssh-key ~/.ssh/id_ed25519 --dry-run --check
```

`create config` 也支持 `--save <PATH>`；`--print-versions` 打印当前构建
pin 的组件版本。

`precheck` 只接受配置文件（`-f`）。想先预检再安装时：

```bash
kurin create config --name prod --control-plane 192.168.10.11 \
  --ssh-key ~/.ssh/id_ed25519 --save prod.toml
kurin precheck -f prod.toml
```

## 覆盖任意字段

```bash
kurin create cluster --name prod --control-plane 192.168.10.11 \
  --ssh-key ~/.ssh/id_ed25519 \
  --set network.pod-cidr=10.244.0.0/16 \
  --set kubernetes.profile=cis
```

`--set` 最后应用，可覆盖文件或前面参数的结果；值按 TOML 字面量解析
（`true`、`3`、`"x"`、`[1,2]`），解析失败当字符串。未知字段在校验期拒绝。

## 配置落盘控制

| 参数 | 行为 |
|---|---|
| `--save <PATH>` | 写出等价配置；默认 `./<name>.kurin.toml` |
| `--no-save` | 不写文件 |
| `--force-save` | 文件已存在且内容不同时覆盖（默认拒绝） |

`--dry-run` 不确认、不落盘。

## 用命令行追加节点

```bash
kurin add nodes -f prod.kurin.toml --worker 192.168.10.24
```

`add nodes` 要求 `-f`（完整期望节点集）；`--worker` / `--control-plane` /
`--node` 会先用 `toml_edit` 追加进配置（保留注释与键顺序），再执行 join。
`--no-save` 只在内存合并。

## 注意事项

- 没有 `-f` 时 `--name` 与至少一个控制面节点是必需的。
- 快速参数与 YAML 文件混用会被拒绝（合并用 `toml_edit`，先转成 TOML）。
- `-p/--ssh-password` 会进入 shell 历史与 `ps`，只建议交互环境临时使用。
- 明文密码永远不会写进配置文件；文件里只有 env/file 引用。
- 完全不给认证参数时，配置会引用 `KURIN_SSH_PASSWORD` 并打印告警；安装前
  需要导出该变量，或改用 `--ssh-key` / `--ask-pass`。
- 完整字段与校验规则见 [配置参考](../reference/configuration.md)；
  全部参数见 [CLI 参考](../reference/cli.md#create-cluster)。