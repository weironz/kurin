# CLI 参考

`kurin --help` 与各子命令 `--help` 是最终事实来源；本页是结构化索引。
版本：`kurin 0.5.2`。

## 全局选项

以下选项可用于所有命令：

| 选项 | 说明 |
|---|---|
| `--json [<VERSION>]` | 输出 JSONL 事件流。裸 `--json` / `--json=v1` 是冻结的 v1 事件（含 `ts`）；`--json=v2` 是完整信封 `{v, seq, ts, op, body}` |
| `--timestamps` | 终端日志行加 UTC 时间戳 |
| `-v`, `--verbose` | `-v` 显示 info 级进度与远端输出摘要；`-vv` 流式输出每条远端 stdout/stderr |
| `--log-format <FORMAT>` | `auto`（默认，TTY 用 rich、管道用 plain）\| `plain`（历史 `skip/done/FAIL` 行）\| `rich`（阶段头、对齐列、CHECK/VERIFY、耗时、颜色） |
| `--max-upload-parallel <N>` | 单次操作并发上传上限，默认 4 |
| `-h`, `--help` | 帮助 |
| `-V`, `--version` | 版本 |

## 命令树

```text
kurin
├── create
│   ├── cluster
│   └── config
├── config
│   ├── migrate
│   └── schema
├── precheck
├── status
├── upgrade
├── certs
│   ├── check
│   ├── renew
│   └── schedule
├── etcd
│   ├── backup
│   ├── restore
│   └── schedule
├── add nodes
├── delete
│   ├── node
│   └── cluster
├── addon
│   ├── list
│   ├── info
│   ├── install
│   ├── upgrade
│   └── remove
├── package
│   ├── create base|os|addon|offline
│   ├── inspect
│   ├── verify
│   ├── push
│   ├── pull
│   ├── list
│   └── prune
├── artifact            # 弃用别名
├── logs
├── import
├── versions            # 支持的 Kubernetes 版本矩阵
└── serve               # 需要 --features ui
```

## create cluster

不写配置文件的完整示例见 [纯命令行快速部署](../getting-started/cli-quickstart.md)。

```bash
kurin create cluster -f cluster.toml
kurin create cluster -f cluster.toml --dry-run [--check]
kurin create cluster -f cluster.toml --resume
```

| 选项 | 说明 |
|---|---|
| `-f, --file <FILE>` | 配置文件（TOML/YAML）。使用快速创建参数时可省略 |
| `--dry-run` | 只打印计划，不连接、不修改 |
| `--check` | 配合 `--dry-run`，真 SSH 探测并标注 would-change/unchanged |
| `--resume` | 从运行状态文件恢复，跳过已完成任务 |
| `--offline <DIR|URI>` | 离线 bundle 目录或包 URI |
| `--package <URI>` | 包 URI，可重复；`/os/` 路径归入 `packages.os` |
| `--set <PATH=VALUE>` | 覆盖任意配置字段，最后应用，可重复 |
| `--save <PATH>` / `--no-save` / `--force-save` | 快速创建时保存等价配置（默认 `./<name>.kurin.toml`） |
| `--yes` | 跳过摘要后的交互确认 |

快速创建参数（`create cluster` 与 `create config` 共用）：

| 选项 | 说明 |
|---|---|
| `--name <N>` | 集群名；无 `-f` 时必填 |
| `--control-plane <IPS>` / `--worker <IPS>` / `--lb <IPS>` / `--registry <IPS>` | 逗号分隔 IPv4；支持 `192.168.10.11-13` 与完整范围；可重复 |
| `--node <KV>` | `name=…,address=…,role=…[,internal-address=…][,user=…][,port=…][,password-env=…][,password-file=…][,key=…][,labels=k=v;k2=v2]` |
| `--node-name <STRATEGY>` | `role-index`（默认）\| `hostname` \| `ip` |
| `--ssh-user` / `--ssh-port` | SSH 用户/端口 |
| `--ssh-key <PATH>` / `--ssh-key-passphrase-env <VAR>` | 私钥认证 |
| `--ask-pass` / `--ssh-password-env <VAR>` / `--ssh-password-file <PATH>` / `-p, --ssh-password <PW>` | 密码认证（`-p` 会告警） |
| `--setup-ssh-key [<PUB>]` | 用密码会话推送公钥，然后切换为私钥认证 |
| `--become` / `--become-password-env <VAR>` / `--ask-become-pass` | 非 root 用户 sudo 提权 |
| `--known-hosts <MODE>` | `tofu`（默认）\| `strict` \| `accept-new` |
| `--kubernetes <V>` | Kubernetes 版本 |
| `--distribution <DIST>` | `kubeadm`（默认）\| `k3s` |
| `--runtime <RUNTIME>` | `containerd`（默认）\| `crio` |
| `--cni <CNI>` | `calico` \| `flannel` \| `cilium` \| `kube-ovn` \| `none` |
| `--pod-cidr` / `--service-cidr` / `--dns-domain` | 网络参数 |
| `--vip <IP>` / `--ha-mode <MODE>` | HA 入口；`kube-vip` \| `kurin-lb` \| `external-endpoint` |
| `--zone <ZONE>` | `global` \| `cn` |
| `--addon <NAME[=VERSION]>` / `--addon-set <NAME.KEY=VALUE>` | 启用 addon / 设置 values |

## create config

```bash
kurin create config > cluster.toml
kurin create config --schema-version v1alpha1
kurin create config --print-versions
kurin create config --name demo --control-plane 192.168.10.11
```

`--schema-version`（默认 `v1alpha2`）、`--print-versions`，以及上面全部快速
创建参数。带快速参数时输出合并后的配置，不安装。

## config

```bash
kurin config migrate -f old.toml [-o new.toml]
kurin config schema [--version v1alpha1|v1alpha2]
```

`migrate` 用 `toml_edit` 保留注释/空行/键顺序，并在写盘前验证迁移结果与
原始 `ClusterSpec` 等价。

## precheck

```bash
kurin precheck -f cluster.toml
```

只读主机审计：swap、内核模块、端口、磁盘、时间同步、OS 支持等。

## status

```bash
kurin status -f cluster.toml [--json]
```

节点、Pod、etcd、证书健康；首个控制面不可达时依次尝试其余控制面。
退出码非 0 表示有异常。表格列：`NODE / ROLE / INTERNAL-IP / STATUS /
RUNTIME / KERNEL`，下方汇总 etcd、版本、证书与 overall。

## upgrade

```bash
kurin upgrade -f cluster.toml --with-kubernetes v1.34.1
kurin upgrade -f cluster.toml --with-kubernetes v1.34.1 --dry-run
```

| 选项 | 说明 |
|---|---|
| `--with-kubernetes <V>` | 目标版本 |
| `--dry-run` | 只打印升级计划 |
| `--batch <N>` | worker 批次大小，默认 1；控制面始终逐个 |
| `--offline <DIR|URI>` | 离线 bundle/包 |
| `--force-drain-failure` | drain 失败仍继续（不安全） |

## certs

```bash
kurin certs check  -f cluster.toml [--warn-days 30]
kurin certs renew  -f cluster.toml [--force]
kurin certs schedule -f cluster.toml [--warn-days 30] [--every 24h]
```

`renew` 逐节点滚动并做 `/readyz` 门控；`schedule` 安装 systemd timer。

## etcd

```bash
kurin etcd backup   -f cluster.toml [-o FILE]
kurin etcd restore  -f cluster.toml --from FILE
kurin etcd schedule -f cluster.toml --every 6h [--keep 7]
```

`restore` 是破坏性操作：集群状态回退到快照时刻，旧数据移到
`…-pre-restore-<ts>`。

## add nodes

```bash
kurin add nodes -f cluster.toml
kurin add nodes -f cluster.toml --worker 192.168.10.22-23
```

| 选项 | 说明 |
|---|---|
| `-f, --file <FILE>` | 完整期望节点集的配置 |
| `--control-plane` / `--worker` / `--node` | 追加节点后再 join |
| `--no-save` | 只在内存合并，不改写配置 |
| `--offline <DIR|URI>` | 离线来源 |

## delete

```bash
kurin delete node worker-2 -f cluster.toml [--dry-run] [--check]
kurin delete cluster -f cluster.toml [--dry-run] [--check]
```

## addon

```bash
kurin addon list   [-f cluster.toml]
kurin addon info   <NAME> [--schema]
kurin addon install <NAME> -f cluster.toml [--version V] [--set k=v] [--values f.yaml] [--dry-run] [--no-save]
kurin addon upgrade <NAME> -f cluster.toml [--version V] [--no-save]
kurin addon remove  <NAME> -f cluster.toml [--keep-crds] [--no-save]
```

配置改写只在操作成功后发生；`--dry-run` 不连接节点、不改配置。

## package

```bash
# 制作
kurin package create base  --kubernetes 1.33.1 --arch amd64 --cni all --addons ingress-nginx -o file:///srv/base.tar.zst
kurin package create os    --target ubuntu-24.04 --arch amd64 -o file:///srv/os.tar.zst
kurin package create os    --target ubuntu-24.04 --arch amd64 --iso /srv/ubuntu.iso -o file:///srv/os.tar.zst
kurin package create addon -f addons/ingress-nginx/addon.yaml -o file:///srv/addon.tar.zst

# 完整离线介质：base + 多个 os 包 + 可选 ISO + offline.json + SHA256SUMS
kurin package create offline --kubernetes 1.33.1 --arch amd64 \
  --target ubuntu-24.04 --target rocky-9 --iso -o /srv/kurin-offline

# 检查
kurin package inspect <SOURCE> [--manifest] [--sbom]
kurin package verify  <SOURCE> [--key cosign.pub] [--user U --password-env VAR]

# 传输
kurin package push <SOURCE> oci://REG/NS/NAME:TAG [--user U --password-env VAR]
kurin package pull oci://REG/NS/NAME:TAG file:///srv/base.tar.zst [--user U --password-env VAR]

# 缓存
kurin package list [--format text|json]
kurin package prune [--dry-run]
```

`SOURCE` 支持 `oci://`、`oci-layout://DIR`、`file://X.tar[.zst]`。
`create base` 的 `--cni` 可取 `all`、`none` 或逗号列表；`--addons` 可写
内置名、目录、`addon.yaml` 或 addon 包。

## artifact（弃用）

```bash
kurin artifact --out DIR [--arch amd64,arm64] [--with-kubernetes V]
```

等价于 `kurin package create base -o oci-layout://DIR`，仅为兼容保留。

## logs

```bash
kurin logs [--cluster C] [--op ID|PREFIX] [--last] [--follow]
           [--node N] [--level trace|debug|info|warn|error] [--json]
```

默认读取最新一次操作；日志文件在
`$XDG_STATE_HOME/kurin/logs/<cluster>/<op>.jsonl`。

## import

```bash
kurin import --from-cluster --host <CP-IP> \
  (--key PATH | --password-env NAME) \
  [--user root] [--port 22] [--known-hosts tofu|strict|accept-new] \
  [-o FILE] [--json]
```

从 `kube-system/kurin-state` 重建配置；`--json` 必须配 `-o`。

## versions

```bash
kurin versions
kurin versions --json
```

打印当前构建的 Kubernetes 版本矩阵：embedded 默认版本、在线 create 可用版本、
带包 create 的版本来源、升级策略、k3s embedded 版本。详见
[Kubernetes 版本支持](kubernetes-versions.md)。

## serve

```bash
kurin serve [--listen 127.0.0.1:7080] [--data-dir DIR]
            [--public-host HOST]... [--tls-cert FILE --tls-key FILE]
            [--allow-insecure-http] [--rotate-token]
            [--max-concurrent-ops 2] [--max-upload-parallel 4]
```

详见 [Web UI](../getting-started/ui.md)。

## 隐藏命令

`kurin capabilities` 输出组件能力表（JSON），供 `just docs-matrix` 与 Web UI
使用，不属于稳定 CLI 接口。

## 退出码与状态

| 场景 | 行为 |
|---|---|
| 成功 | 0 |
| `status` 发现异常 | 非 0（可用于健康门禁） |
| 校验/计划错误 | 非 0，错误信息指向字段或任务 |
| 同一集群已有变更操作 | 立即失败，显示持有者 pid/op/命令行/开始时间 |
| 清理步骤失败 | `task-warn` + 结尾汇总，命令仍以 0 退出（BestEffort） |

## 文件位置

| 路径 | 内容 |
|---|---|
| `$XDG_STATE_HOME/kurin/runs/<cluster>.bb.json` | `--resume` 状态（0600） |
| `$XDG_STATE_HOME/kurin/locks/<cluster>.lock` | 集群操作锁 |
| `$XDG_STATE_HOME/kurin/logs/<cluster>/<op>.jsonl` | 操作日志 |
| `~/.cache/kurin/blobs` | 内容寻址制品缓存 |
| `~/.cache/kurin/clusters/<name>` | kubeconfig 等集群状态 |
| `~/.local/share/kurin/`（或 `$XDG_DATA_HOME/kurin`） | UI token 与 `kurin.db` |

`XDG_STATE_HOME` / `XDG_DATA_HOME` 未设置时回退到 `~/.local/state` /
`~/.local/share`。