# 增删节点

所有变更操作对同一集群互斥：锁文件在
`$XDG_STATE_HOME/kurin/locks/<cluster>.lock`（回退 `~/.local/state`）。
第二个进程会立即失败并显示持有者的 pid、操作 id、命令行与开始时间。

## 增加节点

把新节点写进配置（配置始终描述**完整期望节点集**），然后：

```bash
kurin add nodes -f cluster.toml --dry-run
kurin add nodes -f cluster.toml
```

只加入尚未加入的节点；已存在的节点会被跳过。

也可以先用命令行追加，再执行：

```bash
kurin add nodes -f cluster.toml \
  --worker 192.168.10.22,192.168.10.23 \
  --control-plane 192.168.10.14 \
  --node name=worker-9,address=192.168.10.29,role=worker
```

- 追加使用 `toml_edit`，保留注释、空行与键顺序。
- 自动名称按现有最大序号顺延（如 `worker-2`）。
- `--no-save` 只在内存中合并，不改写配置文件。
- 运行中的 Kubernetes 版本优先从集群内 `kube-system/kurin-state` 读取；
  记录缺失时回退到 `/version` 探测。

## 删除节点

```bash
kurin delete node worker-2 -f cluster.toml --dry-run
kurin delete node worker-2 -f cluster.toml
```

计划：`drain` → 删除 Node 对象 → `kubeadm reset` / `k3s-uninstall` →
清理制品与配置。每一步都有收敛判断（已不存在即 `unchanged`）。

锚点控制面不可达时，API 侧步骤（drain / delete node）跳过并告警，节点本身
照常清理。清理步骤是显式的 `BestEffort`：失败发 `task-warn` 并在结尾按节点
汇总，命令本身仍以 0 退出。

## 删除集群

```bash
kurin delete cluster -f cluster.toml --dry-run
kurin delete cluster -f cluster.toml
```

顺序：先 worker，后 control-plane。删除节点上的 kubeadm/containerd/所有制品
与配置，不删除操作系统。

## 相关

- [CLI 参考 · add/delete](../reference/cli.md#add-nodes)
- [在线安装](online.md)
- [升级](upgrade.md)