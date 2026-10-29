# Kubernetes conformance — tier3 (3 control planes + 2 workers)

- Date: 2026-10-29
- Cluster: `tier3`, Kubernetes v1.33.1, kubeadm, containerd, Calico, kube-vip HA
- Nodes: master01–03 (control-plane) + worker01–02
- Tool: hydrophone v0.8.0, conformance image `registry.k8s.io/conformance:v1.33.1`
- Result: **423 passed / 0 failed / 0 errors** (6735 total, 6312 skipped)
- Duration: 1h54m41s
- Report: `e2e.log`, `junit_01.xml.gz`

This clears the single-node limitation from the 2026-10-28 tier1 run
(421/424; the three failures were single-node artifacts).
