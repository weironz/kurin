# 离线安装

离线安装的核心是两类包：

| 包 | 内容 | 命令 |
|---|---|---|
| base | Kubernetes 二进制、控制面/CNI 镜像、CNI manifest、可选 addon | `kurin package create base` |
| os | 某个目标 OS 的系统依赖包仓库（apt/dnf 元数据 + 包） | `kurin package create os` |

包可以放在 `file://`（tar/tar.zst）、`oci-layout://`（目录）或
`oci://`（registry）。安装时节点不需要访问公网。

## 0. 一条命令生成完整离线介质（推荐）

```bash
kurin package create offline \
  --kubernetes 1.37.2 \
  --arch amd64 \
  --cni calico \
  --target ubuntu-24.04 --target rocky-9 \
  --addons ingress-nginx,metrics-server \
  --iso \
  -o /srv/kurin-offline
```

产出：

```text
kurin-offline/
  kurin-base-1.37.2-amd64.tar.zst
  kurin-os-ubuntu-24.04-amd64.tar.zst
  kurin-os-ubuntu-24.04-amd64.iso
  kurin-os-rocky-9-amd64.tar.zst
  kurin-os-rocky-9-amd64.iso
  kurin-base-1.37.2-amd64.spdx.json
  kurin-os-*.spdx.json
  offline.json          # 包索引：路径、sha256、size、target、ISO、SBOM
  SHA256SUMS            # 所有文件的校验和
  cluster.example.toml  # 可直接使用的 [packages] 配置
  README.md
```

- `--target` 可重复或逗号分隔；不写则不生成 os 包。
- `--iso` 为每个 os 包额外生成 ISO9660 镜像（容器内 `genisoimage`，与
  KubeKey 的仓库 ISO 布局一致）。
- `--addons` 把 addon 文件/镜像嵌入 base 包；示例配置会同时生成
  `[addons.<name>] enabled = true`。
- `offline.json` 是机器可读索引；`cluster.example.toml` 使用绝对路径，
  移动目录后需要调整。

## 1. 在联网机器上制作 base 包

```bash
# 单架构；--cni 默认 all，可写 calico,flannel 等
kurin package create base \
  --kubernetes 1.37.2 \
  --arch amd64 \
  --cni calico \
  -o file:///srv/kurin-base-v1.37.2.tar.zst

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

# 同时生成可挂载的 ISO（容器内 genisoimage）
kurin package create os --target rocky-9 --arch amd64 \
  --iso /srv/rocky-9.iso \
  -o file:///srv/os-rocky-9.tar.zst
```

`--iso` 可以给文件路径（单架构）或目录（多架构写
`<target>-<arch>.iso`）。ISO 内容与 os 包里的仓库目录一致，可挂载后作为
apt/dnf 本地源。

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