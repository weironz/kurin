# 安装 kurin

kurin 是单文件静态二进制（Linux x86_64 / aarch64），同时包含 CLI 和
Web UI。控制机只需要能运行它并通过 SSH 访问目标节点；目标节点不需要预装
kurin。

## 支持的节点

| 节点 OS | 版本 | 包管理器 |
|---|---|---|
| Ubuntu | 22.04 / 24.04 | apt |
| Debian | 12 | apt |
| Rocky Linux | 9.x | dnf |
| RHEL | 9.x | dnf |
| AlmaLinux | 9.x | dnf |
| openEuler | 24.03 LTS | dnf |
| 麒麟 | V10（高级服务器） | dnf |

架构：amd64 / arm64（混合架构集群需要在包中同时携带两种架构）。
完整矩阵与已验证组合见 [命令 × 形态矩阵](../reference/command-matrix.md)。

## 方式一：安装脚本（推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/weironz/kurin/main/scripts/install.sh | bash
```

脚本从 GitHub Releases 下载 `kurin-<arch>-linux-musl.tar.gz`，校验 sha256 后
安装到 `/usr/local/bin/kurin`。

可覆盖的环境变量：

| 变量 | 默认 | 说明 |
|---|---|---|
| `KURIN_VERSION` | `latest` | 固定版本，例如 `v0.5.3` |
| `KURIN_PREFIX` | `/usr/local/bin` | 安装目录 |
| `KURIN_REPO` | `weironz/kurin` | release 所在仓库 |
| `KURIN_BASE_URL` | GitHub Releases | 自定义下载源（镜像/内网） |

验证：

```bash
kurin --version
kurin create config | head -20
```

## 方式二：手动下载

从 [Releases](https://github.com/weironz/kurin/releases) 下载对应架构的
`kurin-<arch>-linux-musl.tar.gz` 与 `.sha256`，校验后解压：

```bash
sha256sum -c kurin-x86_64-linux-musl.tar.gz.sha256
tar -xzf kurin-x86_64-linux-musl.tar.gz
install -m755 kurin /usr/local/bin/kurin
```

## 方式三：源码构建

依赖：

- Rust（当前 stable；仓库使用 edition 2021）
- Node.js 22 + pnpm 9（仅构建 Web UI 需要）
- `just`（可选，仓库自带 justfile）

```bash
git clone https://github.com/weironz/kurin
cd kurin

# 构建前端（产物 web/dist 会被 rust-embed 编译进二进制）
just web-build

# CLI + UI
cargo build --release -p kurin --features ui
# 产物：target/release/kurin
```

只构建 CLI（不含 `serve`）：

```bash
cargo build --release -p kurin
```

构建两个架构的静态 release 包：

```bash
just release-musl
# 产物在 release-dist/
```

更多构建细节见 [开发 · 构建](../development/building.md)。

## 方式四：Docker

```bash
docker run --rm willdockerhub/kurin:latest --help
```

CLI 与 UI 的完整容器用法（挂载配置/SSH 密钥、端口、数据卷、TLS）见
[Docker 运行 CLI 与 UI](../docker/README.md)。

## 下一步

- [快速上手：单节点集群](quickstart.md)
- [启动 Web UI](ui.md)
- [配置参考](../reference/configuration.md)