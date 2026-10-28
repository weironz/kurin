# 从运行中的集群导入配置

集群内 `kube-system/kurin-state` 保存了脱敏后的 `spec.toml`。用它可以把
一个已存在的集群重新纳入 kurin 管理：

```bash
kurin import --from-cluster \
  --host 192.168.10.11 \
  --key ~/.ssh/id_ed25519 \
  -o imported.toml
```

要求：

- `--host` 必须是**控制面**节点（worker 没有控制面 kubeconfig，会明确拒绝）；
- 集群必须由 kurin 安装并带有 `kurin-state` 记录（WP-19 之前的集群会报错）；
- `--key PATH` 或 `--password-env NAME` 必须给一个，二者互斥。

凭据处理：

- 记录中的秘密是 `******`。`--password-env NAME` 把每个脱敏字段
  （`[ssh.auth]`、become、`[registry]`、addon values、节点覆盖）写成
  `{ env = "NAME" }` 引用，并在 stderr 列出清单；
- `--key PATH` 只重写 `[ssh.auth]` 为 `private_key`；如果记录里还有密码
  形态字段，会报错并列出字段，改用 `--password-env`；
- 全程只写引用，秘密值不落任何输出。

保真：

- `toml_edit` 逐字段改写，保留键序/注释/非秘密值；
- v1alpha1 记录先迁移为 v1alpha2；
- 产物必须能被配置管道完整加载。

输出：

- `-o FILE` 以 0600 写盘；缺省 stdout；
- `--json` 报告 `{cluster, distribution, kubernetes_version, nodes, output}`，
  且必须配 `-o`（否则在任何网络动作前拒绝）。

只读操作：不抢集群锁，不写 JSONL 操作日志。

## 相关

- [配置参考](../reference/configuration.md)
- [状态与日志](status-logs.md)
- [CLI 参考 · import](../reference/cli.md#import)