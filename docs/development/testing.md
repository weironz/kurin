# 测试

## 快速检查

```bash
just check
```

包含：

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo clippy -p kurin --features ui -- -D warnings`
- `cargo test --workspace`

`clippy-ui` 需要 `web/dist`；先 `just web-build`。

CI 的快速门禁：`just ci-gate`（`check` + `tier0`）。

## 单元与集成测试

```bash
cargo test --workspace
cargo test -p kurin-components
cargo test -p kurin --test plan_snapshots
cargo test -p kurin --test cli_output
```

快照测试使用 insta；有意变更时：

```bash
cargo insta test --accept
# 或 INSTA_UPDATE=always cargo test ...
```

提交前审查快照 diff，确认只有预期变化。

## Web 测试

```bash
pnpm --dir web test          # Vitest
pnpm --dir web test:e2e      # Playwright（mock 后端）
pnpm --dir web test:e2e:real # Playwright（真实 kurin serve + 集群）
```

真实 e2e 需要 `KURIN_E2E_URL` 等环境变量，见 `web/e2e/README.md`。
场景：9 步向导创建、加/删 worker、销毁、创建中关浏览器、服务重启
interrupted→retry、CLI 抢锁、密码明文扫描。

## e2e tiers（multipass）

| 命令 | 场景 |
|---|---|
| `just tier0` / `tier0-test` / `tier0-down` | systemd 容器，幂等双遍 |
| `just tier1` / `tier1-test` / `tier1-down` | 单节点 all-in-one |
| `just tier2` / `tier2-test` / `tier2-down` | 1 CP + 1 worker |
| `just tier3` / `tier3-test` / `tier3-down` | 3 CP + kube-vip HA |
| `just tier1-offline [src]` | 断网安装 |
| `just tier1-flannel` / `tier1-cilium` / `tier1-cni-takeover` | CNI 变体 |
| `just tier1-k3s` / `tier1-k3s-ops` / `tier1-k3s-upgrade` | k3s |
| `just tier3-k3s-ha` / `tier3-k3s-external-endpoint` | k3s HA |
| `just tier2-upgrade-minor` / `tier2-add-cp` / `tier2-certs` / `tier2-status` | 升级、扩控制面、证书、状态 |
| `just tier1-etcd` | etcd 备份/恢复/定时 |
| `just tier1-addons [online|cn]` | addon 安装/卸载 |
| `just tier1-ubuntu2204` / `tier1-alma` / `tier1-openeuler` | 其他 OS |
| `just tier1-*-offline` | 对应 OS 的离线变体 |
| `just tier1-kylin <img>` | 麒麟（需本地 qcow2） |

这些命令会创建/销毁 multipass 虚拟机；运行前确认本机资源与
`multipass list` 为空。

## 一致性与加固

```bash
scripts/conformance.sh   # hydrophone，需要 tier3 集群与 kubeconfig
just cis-check           # kube-bench，默认 VM 名 kurin-tier1
```

报告归档到 `docs/conformance/`、`docs/cis/`。单节点 conformance 的已知
3 个失败见 [一致性测试](../operations/conformance.md)。

## 相关

- [构建](building.md)
- [故障排查](../operations/troubleshooting.md)