# kurin 文档站（mdBook）

`docs/` 下的 Markdown 通过本目录的脚手架生成一个可构建的 mdBook 站点。
文档源文件仍以 `docs/` 为准 —— 本目录只存放构建配置，`src/` 与 `book/`
均为生成物（已 gitignore）。

## 构建

```bash
# 1) 安装 mdBook（需要 Rust 工具链；也可从 https://mdbook.rs 的 release 下载二进制）
cargo install mdbook --locked

# 2) 同步 ../ 的 Markdown 到 src/（每次文档更新后执行）
cd docs/site && ./sync.sh

# 3) 构建（产物在 book/，入口 book/index.html）
mdbook build

# 本地预览（http://localhost:3000，改动自动刷新）
mdbook serve
```

## 结构

- `book.toml` — mdBook 配置（书名 / 语言 / src 目录）
- `src/SUMMARY.md` — 目录（手写：章节顺序）
- `sync.sh` — 把 `docs/*.md`、`docs/{getting-started,guides,reference,operations,docker,development,project}/*.md`、
  `docs/architecture/*.md`、`docs/architecture/spikes/*.md`、`docs/ui/*.md` 拷进 `src/`，
  排除工作台账（`progress.md`、`architecture/PROGRESS.md`、`architecture/HANDOFF.md`）
- `src/`、`book/` — 生成物

新增文档时：在 `docs/` 放源文件，然后在 `src/SUMMARY.md` 加一行并重跑 `./sync.sh`。
