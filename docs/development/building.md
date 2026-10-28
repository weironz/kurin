# 构建

## 工具链

| 工具 | 用途 |
|---|---|
| Rust（stable） | 编译 workspace |
| Node.js 22 + pnpm 9 | 构建 Web UI |
| `just` | 本地命令入口（可选） |
| Docker | 构建镜像、制作 OS 包 |
| multipass | e2e 虚拟机（可选） |

## 前端

```bash
just web-install     # pnpm --dir web install --frozen-lockfile
just web-build       # tsc + vite build，产物 web/dist
```

`web/dist` 会被 rust-embed 编译进二进制；只构建 CLI 时不需要它。

## 二进制

```bash
# 开发构建（CLI + UI）
cargo build -p kurin --features ui
# 产物：target/debug/kurin

# release
cargo build --release -p kurin --features ui
# 产物：target/release/kurin

# 两个架构的 musl 静态包（release-dist/）
just release-musl
```

`--features ui` 启用 `kurin serve`；不带该 feature 时 `serve` 会提示需要
重新构建。

## 检查

```bash
just check
# = fmt-check + clippy -D warnings + clippy -p kurin --features ui + workspace tests
```

单独运行：

```bash
just fmt
just fmt-check
just clippy
just clippy-ui
just test
```

`clippy-ui` 需要 `web/dist`（先 `just web-build`）。

## 生成物

| 命令 | 产物 |
|---|---|
| `just schema` | `docs/schema/cluster.v1alpha2.json` |
| `just addon-schema` | `docs/schema/addon.v1alpha1.json` |
| `just docs-matrix` | `docs/reference/command-matrix.md` 的 create cluster 表 |
| `just ts-bindings` | Web 的 TypeScript DTO 绑定 |

这些文件有漂移测试；修改对应 Rust 类型后重新生成并提交。

## 文档站

`docs/site/` 是 mdBook 脚手架（源文件仍是 `docs/`）：

```bash
cargo install mdbook --locked
cd docs/site
./sync.sh          # 把 docs/ 的 Markdown 拷进 src/
mdbook build       # 产物 book/index.html
mdbook serve       # http://localhost:3000
```

新增文档：在 `docs/` 对应分类目录放 Markdown，更新
`docs/site/src/SUMMARY.md`，再跑 `./sync.sh`。

## Docker

```bash
docker build -t willdockerhub/kurin:0.4.0 .
```

多阶段构建：Node 构建前端 → Rust 编译 → Debian slim 运行。详见
[Docker](../docker/README.md)。

## 相关

- [测试](testing.md)
- [插件集成](plugins.md)
- [架构总览](../architecture/README.md)