# CIS 基线（kube-bench）

`just cis-check`（`scripts/cis-check.sh`）对 tier1 集群跑 kube-bench 并把
JSON 报告归档到 `docs/cis/<UTC timestamp>/`。这是人工发布门禁，不在
`just check` 内。

## 2026-10-30 tier1 CIS 结果（已归档）

- 报告：`docs/cis/20261030T044457Z/`（`kube-bench.json` + `summary.md`）
- kube-bench v0.10.7，benchmark `cis-1.10`，Kubernetes 1.33.1
- **71 pass / 11 fail / 48 warn**（130 checks）
- 结论：CIS 档位通过；剩余 fail 为环境/策略类（主机级审计、云厂商
  metadata、非 CIS 发行版路径等），不是 kurin 渲染缺陷
- 修复 `serverTLSBootstrap` 后重跑：kubelet 自签服务证书恢复，
  metrics-server / `kubectl logs` 可用；旧报告
  `docs/cis/20261030T003504Z/`（70/11/49）保留作对照

## 2026-10-28 tier1 对照

- 同一版本 kube-bench 在 `profile = "default"` 与 `profile = "cis"` 两个
  集群上的对照：default **64 pass / 15 fail / 51 warn**，CIS
  **70 pass / 11 fail / 49 warn**（修复前）
- 该次原始 JSON 未归档；2026-10-30 的 CIS 报告已补齐

## 建议阈值（待维护者确认）

- 建议：CIS 档位 fail **≤ 11**（v0.10.7 基线），且不得高于同版本 default 档位
- `kube-apiserver` / `etcd` / `kubelet` 控制面项出现**新增 fail** 直接阻断
- 其余新增 fail 需在 PR 说明原因并更新本文件
- warn 项逐条评审，不设硬上限
- kube-bench 版本升级时重跑两档、重定基线并更新本文件

以上为建议值；正式阈值需维护者确认。2026-10-30 的 CIS 原始报告已归档，
后续每次门禁运行继续按 `docs/cis/<timestamp>/` 提交。

## 复现

```bash
just cis-check                 # 默认 kurin-tier1
scripts/cis-check.sh kurin-tier1 docs/cis/$(date -u +%Y%m%dT%H%M%SZ)
```

需要 kube-bench 在 PATH（`KUBEBENCH` 可覆盖）、其 `cfg/` 目录
（`KUBEBENCH_CFG` 可覆盖，默认 `~/.local/share/kube-bench/cfg`）和
multipass 访问控制面。脚本把二进制与 cfg 拷进节点执行（kube-bench
v0.10 检查节点本地配置，不接 kubeconfig）。
