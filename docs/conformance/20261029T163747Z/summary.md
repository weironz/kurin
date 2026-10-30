# Kubernetes conformance — tier3 (3 control planes + 2 workers)

- Date: 2026-10-29
- Cluster: `tier3`, Kubernetes v1.33.1, kubeadm, containerd, Calico, kube-vip HA
- Nodes: master01–03 (control-plane) + worker01–02
- Note: `e2e/tier3/up.sh` now provisions 3 control planes + 2 workers
  (`TIER3_WORKERS`, default 2); this run's workers were added manually
  before the script change, and the 5-node topology is covered by
  `just tier3 && just tier3-test`
- Tool: hydrophone v0.8.0, conformance image `registry.k8s.io/conformance:v1.33.1`
- Result: **423 passed / 0 failed / 0 errors** (6735 total, 6312 skipped)
- Duration: 1h54m41s
- Report: `e2e.log`, `junit_01.xml.gz`

This clears the single-node limitation from the 2026-10-28 tier1 run
(421/424; the three failures were single-node artifacts).
