# Kubernetes 版本支持

kurin 的版本策略和 KubeKey 的 `components.md`、Kubespray 的 supported
versions 表一致：**默认 pin 一个版本，其他版本必须由制品清单/包显式携带，
不支持的组合在开始安装前报错**。

## 当前构建（0.5.3）

| 场景 | 版本 | 说明 |
|---|---|---|
| 在线 `create cluster` | **1.33.1** | 编译进二进制的 embedded manifest 只 pin 这个版本 |
| 带 base 包的 `create cluster` | 包 manifest 的版本 | `package create base --kubernetes <ver>` 决定 |
| `add nodes` | 集群当前版本 | 动态 manifest 按集群版本重新 pin |
| `upgrade` | 当前版本 + 1 个 minor | 动态 manifest；已验证 1.33.1 → 1.34.1 |
| k3s（embedded） | `v1.33.1+k3s1` | k3s 包/离线 bundle 可携带其他 release |
| k3s 升级 | `v1.34.1+k3s1` | 已验证 1.33.1 → 1.34.1（离线 bundle） |

支持策略：Kubernetes **≥ 1.30**；低于 1.30 的配置在加载期直接拒绝
（对应 KubeKey 的 `kube_version_min_required`）。跨 minor 升级一次只允许
+1（kubeadm 的限制）。实际可部署的版本由 embedded manifest 或你提供的包决定。

## 命令行查看

```bash
kurin versions
kurin versions --json
```

输出示例：

```text
kurin 0.5.3
kubernetes (embedded): 1.33.1
create online: 1.33.1
create with package: version pinned by the base package
upgrade: current + 1 minor (dynamic manifest; tested 1.33.1 -> 1.34.1)
k3s: v1.33.1+k3s1 (embedded)
```

`--json` 输出同内容的机器可读对象，便于 CI/脚本判断。

## 安装其他版本

在线 create 只接受 embedded 版本；其他版本先制作对应 base 包：

```bash
# 联网机器上制作 1.34.1 的包（会按目标版本重新 pin 二进制/镜像）
kurin package create base --kubernetes 1.34.1 --arch amd64 \
  -o file:///srv/kurin-base-1.34.1.tar.zst

# 目标集群使用该包
kurin create cluster -f cluster.toml \
  --package file:///srv/kurin-base-1.34.1.tar.zst
```

包的覆盖性在 discover 之后校验：必须覆盖配置里的 Kubernetes 版本、发行版、
CNI 和每个节点架构。

## 不支持时会发生什么

在线 create 如果写了其他版本，会在任何 SSH/安装动作之前失败：

```text
× unsupported Kubernetes version 1.34.1 for an online create: this build's
│ embedded manifest pins 1.33.1. Build a base package with
│ `kurin package create base --kubernetes 1.34.1` and pass it via `--package`,
│ or use 1.33.1. Run `kurin versions` for the supported-version matrix.
```

升级时如果跨了多个 minor 或试图降级，动态 manifest 也会在联网前拒绝：

- `major` 变化 → 拒绝；
- 目标 minor 小于当前 → 拒绝（kubeadm 不支持降级）；
- 目标 minor 大于当前 +1 → 拒绝。

## 相关

- [配置参考 · kubernetes](../reference/configuration.md)
- [升级](../guides/upgrade.md)
- [离线安装](../guides/offline.md)
- [CLI · versions](cli.md#versions)