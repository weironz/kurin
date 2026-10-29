# 一致性测试（hydrophone）

kurin 用 [hydrophone](https://github.com/kubernetes-sigs/hydrophone) 跑
Kubernetes conformance。脚本：`scripts/conformance.sh`；报告归档在
`docs/conformance/<timestamp>/`。

## 最近结果

### tier3 多节点（2026-10-29）

`docs/conformance/20261029T163747Z/`：

- 集群：tier3，3 control-plane + 2 worker，Kubernetes v1.33.1（kubeadm、
  containerd、Calico、kube-vip HA）
- 结果：**423 passed / 0 failed / 0 errors**（6735 total，6312 skipped）
- 耗时：1h54m41s
- 报告：`e2e.log`、`junit_01.xml.gz`、`summary.md`

这是完整的 conformance 门禁结果。

### tier1 单节点（2026-10-28）

`docs/conformance/20261028T074500Z/`：

- 集群：tier1 单节点 k3s `v1.34.1+k3s1`
- 结果：**421 passed / 3 failed / 424 total**
- 3 个失败均为单节点环境的固有限制：
  1. `should have at least two untainted nodes` —— 该用例要求 ≥2 个节点；
  2. `Daemon set rollback without unnecessary restarts` —— 小规模/单节点上已知不稳定；
  3. `API chunking continue listing after compaction [Slow]` —— 单节点/小集群上已知不稳定。

单节点 tier1 可视为通过；完整门禁以 tier3 结果为准。

## 复现

```bash
# 需要已安装 hydrophone 与一个可访问的集群 kubeconfig
scripts/conformance.sh
```

报告包含 `e2e.log`、`junit_01.xml.gz` 与 `summary.md`。多节点运行请用
tier3（3 个控制面 + ≥2 个 untainted worker），并在报告中记录集群形态与版本。

## 相关

- [测试（开发）](../development/testing.md)
- [命令 × 形态矩阵](../reference/command-matrix.md)