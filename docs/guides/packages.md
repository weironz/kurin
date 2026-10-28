# OCI 包

包是离线安装与分发的单位。三种类型：

| 类型 | 命令 | 内容 |
|---|---|---|
| base | `package create base` | Kubernetes 二进制、控制面/CNI 镜像、CNI manifest、可选 addon |
| os | `package create os` | 某个目标 OS 的依赖包仓库（apt/dnf 元数据 + 包） |
| addon | `package create addon` | 一个 addon 的 `addon.yaml`、模板、chart、镜像 |

## 制作

```bash
# base
kurin package create base --kubernetes 1.33.1 --arch amd64 --cni calico \
  -o file:///srv/kurin-base.tar.zst

# 多架构 + 嵌入 addon
kurin package create base --arch amd64,arm64 \
  --addons ingress-nginx,metrics-server \
  -o oci-layout:///srv/kurin-base

# OS
kurin package create os --target ubuntu-24.04 --arch amd64 \
  -o file:///srv/os-ubuntu-24.04.tar.zst

# addon
kurin package create addon -f addons/ingress-nginx/addon.yaml \
  -o file:///srv/ingress-nginx-addon.tar.zst
```

输出形式：

| 形式 | 说明 |
|---|---|
| `oci-layout://DIR` | 本地 OCI layout 目录 |
| `file://X.tar` / `file://X.tar.zst` | 打包成 tar（可 zstd 压缩） |
| `oci://REG/NS/NAME:TAG` | 推送到 registry（`package push`） |

## 检查与校验

```bash
kurin package inspect file:///srv/kurin-base.tar.zst
kurin package inspect file:///srv/kurin-base.tar.zst --manifest   # 完整 kurin-package.json
kurin package inspect file:///srv/kurin-base.tar.zst --sbom       # SPDX SBOM

kurin package verify file:///srv/kurin-base.tar.zst
kurin package verify oci://ghcr.io/weironz/kurin/base:v0.5.0 \
  --key cosign.pub
```

- `verify` 重哈希整个 digest 闭包；带 `--key` 时还要求 cosign 签名能被任一
  公钥验证。
- 配置侧等价：`[packages] verify = "digest" | "signature"` +
  `trusted-keys = [...]`。

## 分发

```bash
# 推到 registry（只发缺失 blob）
kurin package push file:///srv/kurin-base.tar.zst \
  oci://registry.example.com/kurin/base:v1.33.1

# 从 registry 拉到本地
kurin package pull oci://registry.example.com/kurin/base:v1.33.1 \
  file:///srv/kurin-base.tar.zst

# 查看本地内容寻址缓存
kurin package list
kurin package list --format json
```

`push` 与 `pull` 是同一条传输路径的两个方向；`oci://` 源需要 `--user` /
`--password-env` 时按 registry 凭据传入。

## 在安装中使用

```toml
[packages]
base = "file:///srv/kurin-base.tar.zst"
os = ["file:///srv/os-ubuntu-24.04.tar.zst"]
delivery = "preload"        # preload | registry | seed | auto
verify = "digest"
```

或命令行：

```bash
kurin create cluster -f cluster.toml --offline file:///srv/kurin-base.tar.zst
```

包覆盖性在 discover 之后校验：必须覆盖所选 K8s 版本、发行版、CNI 和每个
节点架构，缺什么列什么。

## 弃用别名

`kurin artifact --out DIR` 等价于
`kurin package create base -o oci-layout://DIR`，仅为兼容保留。

## 相关

- [OCI 包设计（架构）](../architecture/04-oci-packages.md)
- [离线安装](offline.md)
- [私有 registry](registry.md)
- [CLI 参考 · package](../reference/cli.md#package)