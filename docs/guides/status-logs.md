# 状态与日志

## 集群健康

```bash
kurin status -f cluster.toml
kurin status -f cluster.toml --json
```

表格列：

```text
NODE       ROLE            INTERNAL-IP    STATUS   RUNTIME               KERNEL
master01   control-plane   192.168.1.11   Ready    containerd://1.7.22   6.8.0-45-generic
worker01   worker          192.168.1.21   Ready    containerd://1.7.22   6.8.0-45-generic
```

`Ready` 绿色、`NotReady` 红色（TTY 且未设置 `NO_COLOR`）。

检查项：

- 节点 Ready 与版本；
- 系统 Pod 状态；
- etcd 健康（kubeadm 看 Pod 计数，k3s 看 `/healthz/etcd`）；
- 控制面证书剩余天数与下次续期时间；
- 期望版本（`kube-system/kurin-state`）vs 观测版本（`/version`）。

首个控制面不可达时依次尝试其余控制面。**退出码可用于门禁**：非 0 表示
有异常。

## 进度输出格式

`create cluster` / `add nodes` 默认在 TTY 上使用 rich 格式，管道/CI 保持
历史 plain 行（脚本兼容）。强制选择：

```bash
kurin create cluster -f cluster.toml --log-format rich
kurin create cluster -f cluster.toml --log-format plain
```

rich 格式示例：

```text
[09:42:10] 🚀 Starting cluster execution for [prod] (op 20261028T...)

━━▶ Phase 1/9: Preflight ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
[09:42:11] [master01] [CHECK ] preflight  ✔ satisfied -> skipped
[09:42:11] [master02] [CHECK ] preflight  checking…
[09:42:12] [master02] [VERIFY] preflight  ✔ passed (0.31s)
```

颜色可通过 `NO_COLOR=1` 关闭。`--json` 始终输出机器可读事件，不受该选项影响。

## 操作日志

每次变更操作都写 JSONL：

```text
$XDG_STATE_HOME/kurin/logs/<cluster>/<op>.jsonl
# 回退 ~/.local/state/kurin/logs/...
```

读取：

```bash
kurin logs                      # 默认最新一次操作
kurin logs --last
kurin logs --op <id|前缀>
kurin logs --cluster <name> --op <id>
kurin logs --follow             # 跟随追加
kurin logs --node node01        # 只看某节点
kurin logs --level warn         # trace|debug|info|warn|error
kurin logs --json               # JSONL 原样输出
```

`-vv` 会把远端 stdout/stderr 也流到终端；日志级别默认 `info`。

## 事件流

`--json` / `--json=v2` 输出逐行 JSON：

| 事件 | 含义 |
|---|---|
| `task-start` | 任务开始 |
| `task-skip` | 已收敛，跳过 |
| `task-done` | 任务完成 |
| `task-failed` | 任务失败 |
| `task-reconciled` | 结果未知后重新探测并确认 |
| `stage-done` | 阶段完成 |
| `cluster-ready` | 集群就绪 |

`--json`（v1）是冻结的字段集 + `ts`；`--json=v2` 是完整信封
`{v, seq, ts, op, body}`。自动化消费建议用 v2。

## 断点与重试

- `create cluster --resume` 从运行状态文件恢复；
- 服务重启后，进行中的 Web 操作变为 `interrupted`，可在 UI 重试；
- 重试操作带 `retry_of` 字段，日志保留原操作。

## 相关

- [故障排查](../operations/troubleshooting.md)
- [CLI 参考 · logs](../reference/cli.md#logs)