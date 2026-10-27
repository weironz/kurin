# local-path

Rancher [local-path-provisioner](https://github.com/rancher/local-path-provisioner)：
用节点本地磁盘（`/opt/local-path-provisioner`）提供动态存储，
StorageClass 名为 `kurin-local-path`，helper Pod 使用 busybox。

- 版本 / version: **0.0.31**（manifest 安装，无 chart；
  资源与第一层 storage 组件逐字段一致，WP-47 迁移时不重建）
- 安装 / install: `[addons.local-path] enabled = true`（目前由第一层
  storage 组件执行，直到 WP-47 把它交给 addon 引擎）
- 命名空间 / namespace: `local-path-storage`
- 镜像 / images: `rancher/local-path-provisioner:v0.0.31`、`busybox:1.36.1`
  （`zone = "cn"` 时经 `docker.m.daocloud.io` 拉取）

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `default-class` | `false` | 设为默认 StorageClass / default StorageClass |

```toml
[addons.local-path]
enabled = true
values = { default-class = true }
```

## 依赖 / dependencies

`conflicts: [nfs-subdir]`——一个集群只有一个动态存储供应者（与
`nfs-subdir` 同时启用会在配置校验期被拒绝）。提供
`provides: [default-storage-class]`。

## 与第一层的过渡 / transitional note

在 WP-47 之前，启用本 addon 仍由第一层 `storage/local-path` 组件安装
（任务 `storage/install@<node>`）；`kurin addon remove local-path`
因此没有可卸载的记录（no-op）。WP-47 移除第一层组件后，本目录的
manifest 模板接手，资源名称与字段保持不变。
