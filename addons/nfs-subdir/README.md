# nfs-subdir

kubernetes-sigs [nfs-subdir-external-provisioner](https://github.com/kubernetes-sigs/nfs-subdir-external-provisioner)：
对接用户自备的 NFS 服务器，StorageClass 名为 `kurin-nfs`。

- 版本 / version: **4.0.2**（manifest 安装，无 chart；
  资源与第一层 storage 组件逐字段一致，WP-47 迁移时不重建）
- 安装 / install: `[addons.nfs-subdir] enabled = true`
  （目前由第一层 storage 组件执行，直到 WP-47 把它交给 addon 引擎）
- 命名空间 / namespace: `default`
- 镜像 / image:
  `registry.k8s.io/sig-storage/nfs-subdir-external-provisioner:v4.0.2`
  （`zone = "cn"` 时经 `k8s.m.daocloud.io` 拉取）

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `server` | — | NFS 服务器地址（启用时必填）/ NFS server (required) |
| `path` | — | NFS 导出路径（启用时必填）/ NFS export path (required) |
| `default-class` | `false` | 设为默认 StorageClass / default StorageClass |

```toml
[addons.nfs-subdir]
enabled = true
values = { server = "10.0.0.5", path = "/exports/k8s", default-class = true }
```

`server` / `path` 缺失时配置在校验期被拒绝；模板直接引用这两个值，
缺省渲染也会立刻失败（不产生半装状态）。

## 依赖 / dependencies

`conflicts: [local-path]`——一个集群只有一个动态存储供应者（与
`local-path` 同时启用会在配置校验期被拒绝）。提供
`provides: [default-storage-class]`。

## 与第一层的过渡 / transitional note

在 WP-47 之前，启用本 addon 仍由第一层 `storage/nfs` 组件安装（任务
`storage/install@<node>`）；`kurin addon remove nfs-subdir` 因此没有
可卸载的记录（no-op）。WP-47 移除第一层组件后，本目录的 manifest
模板接手，资源名称与字段保持不变。
