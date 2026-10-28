# Addon

内置目录（版本见 `addons/<name>/addon.yaml`）：

| addon | 分类 | 安装方式 |
|---|---|---|
| `metrics-server` | monitoring | Helm |
| `local-path` | storage | manifests |
| `nfs-subdir` | storage | manifests |
| `ingress-nginx` | ingress | Helm |
| `cert-manager` | security | Helm (OCI) |
| `metallb` | networking | Helm |
| `headlamp` | dashboard | Helm |
| `kube-prometheus-stack` | monitoring | Helm (OCI) |

`local-path` 与 `nfs-subdir` 不能同时启用（一个集群只有一个动态存储
provisioner）。名单之外的名字必须提供 `source`（外部 addon 包）。

## 配置

```toml
[addons.metrics-server]
enabled = true
version = "0.9.0"

[addons.metrics-server.values]
replicas = 2

[addons.ingress-nginx]
enabled = true
# raw-values 透传 Helm，不做 schema 校验
raw-values = { controller = { replicaCount = 2 } }
```

字段：`enabled`、`version`、`source`、`values`、`raw-values`。合并顺序：
defaults → 内置推荐值（按集群上下文渲染）→ 用户 `values` → `raw-values`。
`values` 中未在 schema 声明的 key 会在渲染期报错；`values` / `raw-values`
都支持 `{ env = "VAR" }` / `{ file = "/path" }` 秘密引用。

## 命令

```bash
kurin addon list -f cluster.toml
kurin addon info metrics-server
kurin addon info metrics-server --schema      # values JSON Schema

kurin addon install metrics-server -f cluster.toml --dry-run
kurin addon install metrics-server -f cluster.toml --set replicas=2
kurin addon install ingress-nginx -f cluster.toml --values values.yaml

kurin addon upgrade metrics-server -f cluster.toml --version 0.9.0
kurin addon remove metrics-server -f cluster.toml [--keep-crds]
```

- 配置改写用 `toml_edit`，保留注释/空行/键顺序；`--no-save` 关闭改写。
- 改写发生在操作**成功之后**：失败的安装不会把配置写成已启用。
- `install` 会一并启用 `requires` 的条目；`remove` 置 `enabled = false`；
  `upgrade` 写入目录提供的版本。
- `--dry-run` 只打印计划（`discover` → `addons/<name>` → `post`），不连接
  节点、不改配置。
- 每个操作只收敛目标 addon（及其依赖），不影响其它 addon。

## 离线

制作 base 包时嵌入 addon（其 chart 与镜像一并进包）：

```bash
kurin package create base --addons ingress-nginx,metrics-server \
  -o file:///srv/kurin-base.tar.zst
```

也可以单独制作 addon 包：

```bash
kurin package create addon -f addons/ingress-nginx/addon.yaml \
  -o file:///srv/ingress-nginx-addon.tar.zst
```

然后在配置里用 `source = "file:///srv/..."` 引用。

## 相关

- [Addon 体系（架构）](../architecture/03-addons.md)
- [addon.yaml schema](../schema/addon.v1alpha1.json)
- [OCI 包](packages.md)