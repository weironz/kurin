# Kubernetes conformance — 2026-10-28

- Cluster: tier1 k3s, single node, `v1.34.1+k3s1` (after the WP-72 offline upgrade)
- Tool: hydrophone v0.8.0, conformance image `registry.k8s.io/conformance:v1.34.1`
- Result: **421 passed / 3 failed** (424 run of 7137 specs; 6713 skipped)
- Failures:
  1. `[sig-architecture] should have at least two untainted nodes` — the tier1
     cluster has one node; the test is inapplicable by construction.
  2. `[sig-api-machinery] API chunking ... original version has been compacted`
     — k3s/etcd compaction behavior.
  3. `[sig-apps] DaemonSet should rollback without unnecessary restarts` —
     k3s/kubelet behavior.
- Full ginkgo log: `e2e.log`.
