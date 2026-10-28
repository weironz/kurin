# W1 轨道与文件所有权

并行纪律（对所有轨道生效）：

1. **只写自己轨道的文件**；共享文件（`Cargo.toml`、`kurin/src/lib.rs`、`kurin/src/tasks/mod.rs`、`docs/project/progress.md`）由集成 owner 统一改。依赖已预装进 `kurin/Cargo.toml`：russh、backon、futures、tempfile。
2. **不跑 `cargo build`/`cargo test` 全量**——用 `CARGO_TARGET_DIR=target/<track>` 跑自己 crate 的 `cargo check`。
3. 完成后 `git add <自己的文件> && git commit -m "<track>: ..."` 并 `git push`；**不 push 不算完成**。
4. 不 rebase、不 force push；push 冲突时 `git pull --rebase` 重试。

| 轨 | 拥有文件 |
|---|---|
| A | `kurin-config/**`（除 lib.rs 契约导出） |
| B | `kurin/templates/**`、`kurin/src/tasks/{prepare,bootstrap,join,cni,post}.rs` 中模板调用点 |
| C | `kurin/src/artifact.rs`（新）+ `kurin/src/artifacts/`（清单数据） |
| D | `kurin/src/ssh.rs`（新，含 pool） |
| E | `kurin/src/{executor,render,engine}.rs`、`kurin/src/state.rs`（新） |
| F | `e2e/**`、`Makefile` |
| G | `kurin/src/version.rs` 的数据行、`scripts/collect-checksums.sh`（新） |

环境事实：rustup 已装（`~/.cargo/bin` 需在 PATH）；docker 28 可用；无 multipass。
