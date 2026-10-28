# 国内镜像（zone = cn）

在出口受限的网络里，把制品源切到国内镜像：

```bash
KURIN_ZONE=cn kurin create cluster -f cluster.toml
```

或写进配置：

```toml
[artifacts]
zone = "cn"
```

环境变量优先于配置字段。

## 覆盖范围

| 类别 | 处理 |
|---|---|
| 镜像仓库 | 经 daocloud mirror 拉取（registry.k8s.io、quay.io、ghcr.io、docker.io 等） |
| GitHub Releases | 经 gh-proxy 下载 |
| `dl.k8s.io` | 没有维护中的国内镜像，保持直连 |
| 本地 registry | zone=cn 时**不**改写 registry 引用（本地地址） |
| addon values | 镜像源自动换成 `*.m.daocloud.io`，digest 与上游一致 |

镜像 digest 在清单里是 sha256 pin，镜像站只改变下载路径，不改变信任锚。
`scripts/check-mirrors.sh` 持续校验镜像表的 digest 一致性（CI
`.github/workflows/mirrors.yml`）。

## 离线场景

zone 只影响在线下载路径；离线安装请用 [OCI 包](packages.md) /
[离线安装](offline.md)，不需要 zone。

## 验证

```bash
kurin create cluster -f cluster.toml --dry-run -vv
# 观察下载 URL / 镜像引用是否走 mirror
```

如果某个镜像在 mirror 上缺失，安装会明确报错并给出上游 URL；不会静默
回退到未校验的源。

## 相关

- [配置参考 §artifacts](../reference/configuration.md#artifacts)
- [离线安装](offline.md)