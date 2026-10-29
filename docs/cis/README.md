# CIS 基线（kube-bench）

`just cis-check`（`scripts/cis-check.sh`）对 tier1 集群跑 kube-bench 并把
JSON 报告归档到 `docs/cis/<UTC timestamp>/`。这是人工发布门禁，不在
`just check` 内。

## 2026-10-28 tier1 结果

- kube-bench v0.10.7，Kubernetes 1.33.1，`profile = "default"` 与
  `profile = "cis"` 各建一个集群
- default：**64 pass / 15 fail / 51 warn**
- CIS：**70 pass / 11 fail / 49 warn**
- 结论：CIS 档位通过；剩余 fail 为环境/策略类（主机级审计、云厂商
  metadata、非 CIS 发行版路径等），不是 kurin 渲染缺陷
- 本次原始 JSON 未入库（临时 VM 已销毁）；后续运行按时间戳目录归档

## 发布阈值

- CIS 档位 fail **≤ 11**（v0.10.7 基线），且不得高于同版本 default 档位
- `kube-apiserver` / `etcd` / `kubelet` 控制面项出现**新增 fail** 直接阻断
- 其余新增 fail 需在 PR 说明原因并更新本文件
- warn 项逐条评审，不设硬上限
- kube-bench 版本升级时重跑两档、重定基线并更新本文件

## 复现

```bash
just cis-check                 # 默认 kurin-tier1
scripts/cis-check.sh kurin-tier1 docs/cis/$(date -u +%Y%m%dT%H%M%SZ)
```

需要 kube-bench 在 PATH（`KUBEBENCH` 可覆盖）和 multipass 访问控制面。
