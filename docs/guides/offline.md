# 离线安装

离线安装的核心是两类包：

| 包 | 内容 | 命令 |
|---|---|---|
| base | Kubernetes 二进制、控制面/CNI 镜像、CNI manifest、可选 addon | `kurin package create base` |
| os | 某个目标 OS 的系统依赖包仓库（apt/dnf 元数据 + 包） | `kurin package create os` |

包可以放在 `file://`（tar/tar.zst）、`oci-layout://`（目录）或
`oci://`（registry）。安装时节点不需要访问公网。

## 1. 在联网机器上制作 base 包

```bash
# 单架构；--cni 默认 all，可写 calico,flannel 等
kurin package create base \
  --kubernetes 1.33.1 \
  --arch amd64 \
  --cni calico \
  -o file:///srv/kurin-base-v1.33.1.tar.zst

# 混合架构
kurin package create base --arch amd64,arm64 -o file:///srv/kurin-base.tar.zst

# 嵌入 addon（其镜像也进包，离线可装）
kurin package create base --addons ingress-nginx,metrics-server \
  -o file:///srv/kurin-base.tar.zst
```

检查：

```bash
kurin package inspect file:///srv/kurin-base.tar.zst
kurin package verify  file:///srv/kurin-base.tar.zst
```

`inspect` 打印版本、组件、制品与镜像；`verify` 重哈希整个 digest 闭包。
需要签名校验时加 `--key cosign.pub`。

## 2. 制作 OS 包（可选但推荐）

如果节点是全新系统、断网且没有本地 yum/apt 源，需要对应 OS 的依赖包：

```bash
kurin package create os --target ubuntu-24.04 --arch amd64 \
  -o file:///srv/os-ubuntu-24.04.tar.zst

kurin package create os --target rocky-9 --arch amd64 \
  -o file:///srv/os-rocky-9.tar.zst
```

目标列表：`ubuntu-24.04`、`ubuntu-22.04`、`debian-12`、`rocky-9`、`rhel-9`、
`almalinux-9`、`openeuler-24.03`、`kylin-10`（Kylin 需要本地固定容器镜像）。

## 3. 配置

v1alpha2：

```toml
version = "v1alpha2"
name = "offline"

[packages]
base = "file:///srv/kurin-base.tar.zst"
os = ["file:///srv/os-ubuntu-24.04.tar.zst"]
# delivery = "preload"   # 默认；有 [registry] 时 auto 走 registry
# verify = "digest"      # none | digest | signature
# trusted-keys = ["/etc/kurin/cosign.pub"]
```

或者用命令行覆盖：

```bash
kurin create cluster -f cluster.toml \
  --offline file:///srv/kurin-base.tar.zst
```

`--offline` 与 `--package` 等价；URI 中含 `/os/` 路径段的归入 `packages.os`，
其余作为 base。

旧版 bundle 目录（含 `kurin-manifest.json`）仍可通过
`[artifacts] offline-dir = "/srv/bundle"` 或 `--offline /srv/bundle` 使用。

## 4. 分发方式

`[packages] delivery` 决定镜像如何到达节点：

| delivery | 行为 | 适用 |
|---|---|---|
| `preload`（默认） | 控制机把镜像以 docker-archive 流式上传到节点并 `ctr images import` | 无 registry 节点，最简单 |
| `registry` | 控制机先把包推入私有 registry，节点从 registry 拉取 | 配置了 `[registry]` |
| `seed` | 控制机内置只读 registry，经 SSH `-R` 暴露到节点回环，节点以 pull 语义拉取 | 无 registry 节点但想要 pull 语义 |

`seed` 的配置与限制见 [seed 仓库](seed.md)。

## 5. 断网安装

在控制机上（能 SSH 到节点即可，不需要公网）：

```bash
kurin precheck -f cluster.toml
kurin create cluster -f cluster.toml --dry-run
kurin create cluster -f cluster.toml
```

安装期间不应有任何外网请求。可用节点上的 `ss -tnp` / 出口防火墙验证。
离线安装同样支持 `--resume` 与幂等重跑。

## 6. 常见问题

- **包覆盖性错误**：`create cluster` 在 discover 后检查包是否覆盖所选
  K8s 版本、发行版、CNI 和每个节点架构；错误信息会列出缺什么。
- **OS 包匹配失败**：`packages.os` 不写时从 base 包的 os 行按节点 facts
  自动匹配；匹配不到会在校验期报错并列出每个候选被拒绝的原因。
- **Kylin**：没有可验证的公共镜像，`package create os --target kylin-10`
  需要先本地导入并 pin 镜像，否则明确拒绝构建。

参考：[配置参考 §packages](../reference/configuration.md#packagesv1alpha2)、
[OCI 包](packages.md)、[Registry 契约](../reference/registry-contracts.md)。