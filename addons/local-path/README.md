# local-path

Rancher [local-path-provisioner](https://github.com/rancher/local-path-provisioner)：
用节点本地磁盘（`/opt/local-path-provisioner`）提供动态存储，
StorageClass 名为 `kurin-local-path`，helper Pod 使用 busybox。

- 版本 / version: **0.0.31**（manifest 安装，无 chart；
  对象名称、命名空间与 StorageClass 和迁移前一致，升级 kurin 不删除重建）
- 安装 / install: `[addons.local-path] enabled = true`，由 addon 引擎
  执行（任务 `addons/local-path@<node>`）
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

## 迁移 / migration

v1alpha1 的 `[storage] kind = "local-path"` 在加载时自动映射为本
addon（`values = { default-class }`），StorageClass 仍叫
`kurin-local-path`；已有集群重跑 `create cluster` 时由服务端 apply
收敛，资源不会被删除重建，第一次运行写入 `kurin-state` 记录后即为
InSync。
