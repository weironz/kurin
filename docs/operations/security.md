# 安全

## 秘密处理

配置里的密码/令牌支持三种写法（`SecretRef`）：

```toml
password = "literal"                    # 1) 字面值（兼容写法）
password = { env = "KURIN_SSH_PASSWORD" } # 2) 环境变量，prepare 阶段读取
password = { file = "/run/secrets/ssh" }  # 3) 文件，prepare 阶段读取
```

- 明文密码（`-p`、`--ask-pass`、`--ask-become-pass`）只存在于进程内存，
  **永远不会写进配置文件**；落盘只写秘密引用。
- 文件引用在读取时去掉一个结尾换行；文件权限宽于 0600 会告警。
- 解析后的秘密注册进日志脱敏表，不会出现在日志、事件、错误文本或
  `kurin-state` 里（记录中的 `spec.toml` 已脱敏）。
- `--resume` 运行状态（含 kubeadm join 材料）写入
  `~/.local/state/kurin/runs/<cluster>.bb.json`，权限 0600。

## SSH 主机密钥

`--known-hosts`：

| 模式 | 行为 |
|---|---|
| `tofu`（默认） | 首次连接信任并记录，之后不匹配则拒绝 |
| `strict` | 只接受已知密钥，未知直接拒绝 |
| `accept-new` | 接受新主机，已知密钥变化仍拒绝 |

TOFU 存储：`~/.cache/kurin/known_hosts`。节点重装后需要删除对应条目：

```bash
ssh-keygen -R <ip> -f ~/.cache/kurin/known_hosts
```

## 非 root 登录

用 `--become` / `[ssh] become = true` 通过 sudo 提权；sudo 密码可用
`--become-password-env` 或 `--ask-become-pass`。建议为 kurin 配置最小权限的
NOPASSWD sudo 规则，而不是直接使用 root 密码。

## Web UI

- 默认只监听 `127.0.0.1:7080`；远程访问优先用 SSH 隧道。
- 非回环监听必须显式 TLS 或 `--allow-insecure-http`（后者会告警）。
- 访问令牌 256-bit，写入 `<data-dir>/token`（0600），登录链接只打印一次。
- 令牌换取 `kurin_session` cookie；cookie `SameSite=Strict`。
- `Host` 头白名单（`--public-host`）防 DNS rebinding。
- 变更类请求只接受 `application/json`，防 CSRF 表单。
- `--rotate-token` 立即注销所有会话；令牌丢失时用它重新生成。
- 反向代理终止 TLS 时，kurin 侧继续监听回环，并把外部域名写进
  `--public-host`。

## 加固档位（CIS）

v1alpha2 支持：

```toml
[kubernetes]
profile = "cis"     # default（缺省，行为不变）| cis
```

`cis` 会设置 apiserver/kubelet 加固参数、审计策略、Pod Security
Admission `restricted`，并把相关文件权限收紧到 0600。验收可用
`just cis-check`（kube-bench，人工判定阈值）。

## 制品与包

- 所有制品在清单里以 sha256 pin；镜像以 OCI index digest 锁定。
- `KURIN_ZONE=cn` 只改下载路径，不改变摘要校验。
- 包校验：

```toml
[packages]
verify = "signature"
trusted-keys = ["/etc/kurin/cosign.pub"]
```

`signature` 要求包带有能被任一公钥验证的 cosign 签名，并在其上重哈希整个
digest 闭包。命令行等价：`kurin package verify <src> --key cosign.pub`。

## 运行时

- Kubernetes 节点使用 containerd（二进制安装），不依赖 docker。
- registry 节点安装 docker 仅用于运行 registry（不是 k8s 节点）。
- 集群内状态记录 `kube-system/kurin-state` 只保存脱敏配置、组件/ addon
  清单与最近 20 次操作历史。

## 相关

- [UI 架构 §6.10](../ui/ui-architecture-final.md)
- [配置参考 §秘密引用](../reference/configuration.md#秘密引用secretref)
- [Web UI](../getting-started/ui.md)