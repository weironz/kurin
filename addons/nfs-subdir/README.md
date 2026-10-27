# nfs-subdir

kubernetes-sigs [nfs-subdir-external-provisioner](https://github.com/kubernetes-sigs/nfs-subdir-external-provisioner)：
对接用户自备的 NFS 服务器，StorageClass 名为 `kurin-nfs`。

- 版本 / version: **4.0.2**（manifest 安装，无 chart；
  对象名称、命名空间与 StorageClass 和迁移前一致，升级 kurin 不删除重建）
- 安装 / install: `[addons.nfs-subdir] enabled = true`，由 addon 引擎
  执行（任务 `addons/nfs-subdir@<node>`）
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

## 迁移 / migration

v1alpha1 的 `[storage] kind = "nfs"` 在加载时自动映射为本 addon
（`values = { server, path, default-class }`），StorageClass 仍叫
`kurin-nfs`；已有集群重跑 `create cluster` 时由服务端 apply 收敛，
资源不会被删除重建，第一次运行写入 `kurin-state` 记录后即为 InSync。
