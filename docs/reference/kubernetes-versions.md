# Kubernetes 版本支持

kurin 的版本策略：**默认 pin 一个版本；在线 create 可为其他受支持版本
动态解析制品 pin，离线场景由 base 包显式携带**。不支持的组合在开始安装前
报错。

## 当前构建（Unreleased，基于 0.5.4）

| 场景 | 版本 | 说明 |
|---|---|---|
| 在线 `create cluster` | **1.33.1（默认）** | 其他受支持版本（≥1.30）在线解析动态 manifest：kubeadm/kubelet/kubectl 用 dl.k8s.io 签名 sha256，镜像用 digest |
| 带 base 包的 `create cluster` | 包 manifest 的版本 | `package create base --kubernetes <ver>` 决定 |
| `add nodes` | 集群当前版本 | 动态 manifest 按集群版本重新 pin |
| `upgrade` | 当前版本 + 1 个 minor | 动态 manifest；已验证 1.33.1 → 1.34.1 |
| k3s（embedded） | `v1.33.1+k3s1` | k3s 包/离线 bundle 可携带其他 release |
| k3s 升级 | `v1.34.1+k3s1` | 已验证 1.33.1 → 1.33.2（`just tier1-k3s-upgrade` 自动构造离线 bundle）与 1.33.1 → 1.34.1（手工 bundle） |

支持策略：Kubernetes **≥ 1.30**；低于 1.30 的配置在加载期直接拒绝
（对应 KubeKey 的 `kube_version_min_required`）。跨 minor 升级一次只允许
+1（kubeadm 的限制）。实际可部署的版本由 embedded manifest、在线动态 manifest 或你提供的包决定。

## 命令行查看

```bash
kurin versions
kurin versions --json
```

输出示例：

```text
kurin 0.5.4
kubernetes (embedded): 1.33.1
create online: 1.33.1 (default) or any supported version via dynamic manifest
create with package: version pinned by the base package
upgrade: current + 1 minor (dynamic manifest; tested 1.33.1 -> 1.34.1)
k3s: v1.33.1+k3s1 (embedded)
```

`--json` 输出同内容的机器可读对象，便于 CI/脚本判断。

## 安装其他版本

在线 create 可直接指定其他受支持版本（≥1.30）：

```bash
kurin create cluster -f cluster.toml --kubernetes 1.34.1
# 或配置里写 [kubernetes] version = "1.34.1"
```

kurin 会为目标版本拉取 kubeadm/kubelet/kubectl 的签名 sha256，并解析
控制面镜像 digest；多架构集群会按每个节点的架构分别 pin。`--dry-run`
（不带 `--check`）保持零网络，因此非 embedded 版本的纯预览会要求改用
`--dry-run --check`。

离线场景仍用 base 包携带完整制品：

```bash
kurin package create base --kubernetes 1.34.1 --arch amd64 \
  -o file:///srv/kurin-base-1.34.1.tar.zst
kurin create cluster -f cluster.toml \
  --package file:///srv/kurin-base-1.34.1.tar.zst
```

包的覆盖性在 discover 之后校验：必须覆盖配置里的 Kubernetes 版本、发行版、
CNI 和每个节点架构。

## 不支持时会发生什么

在线 create 如果目标版本没有可用的上游制品（版本不存在、低于 1.30、
或 dl.k8s.io 缺少对应架构的 sha256），会在 SSH/安装动作之前失败，并指出
缺失的版本/架构。离线场景缺少包覆盖时由包覆盖性诊断报错。

升级时如果跨了多个 minor 或试图降级，动态 manifest 也会在联网前拒绝：

- `major` 变化 → 拒绝；
- 目标 minor 小于当前 → 拒绝（kubeadm 不支持降级）；
- 目标 minor 大于当前 +1 → 拒绝。

## 相关

- [配置参考 · kubernetes](../reference/configuration.md)
- [升级](../guides/upgrade.md)
- [离线安装](../guides/offline.md)
- [CLI · versions](cli.md#versions)