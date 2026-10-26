# Changelog

All notable changes to kurin. Format follows Keep a Changelog; semver.

## 0.3.0 - 2026-10-26

### Added
- Kube-OVN CNI (`network.cni.kind = "kube-ovn"`): embedded helm-template
  output with runtime CIDR/gateway/registry rewrite and the mandatory
  `kube-ovn/role=master` label pass. Tier-1 e2e verified.
- haproxy probes every apiserver backend with HTTPS `GET /readyz`
  (`check-ssl verify none`); data plane stays pure TCP passthrough.
- haproxy stats page on `<lb>:8404/haproxy-stats`; e2e asserts via the
  `;csv=true` endpoint.
- Offline bundles now carry flannel and cilium images (offline × any CNI
  works instead of silently failing with image-pull errors at kubelet).
- `kurin artifact` pulls from ghcr.io through its anonymous token flow.
- k3s × `ha.mode = "external-endpoint"` supported: joins, API gating and
  the recorded root kubeconfig all present the external LB address.
- `just release <ver>` — one-shot build/tag/changelog-notes/publish.

### Fixed
- minijinja trailing-newline stripping made haproxy 2.8 reject rendered
  configs as truncated (`set_keep_trailing_newline(true)`).
- `default-server ssl` rewarmed client TLS through the LB; removed.
- CI check/mirrors workflows no longer trigger on push (they raced local
  e2e and piled failures); manual dispatch only. Release keeps tag trigger.
- Calico readiness budgets raised for slow-registry days (CRD 600 s,
  operator 480 s).

## 0.2.0 - 2026-10-25

Full lifecycle: add/delete nodes, offline bundles, zone=cn mirror set,
upgrades, certs rotation, status probes, k3s distribution, CRI-O runtime.

## 0.1.0 - 2026-10-24

First public release: declarative kubeadm cluster install.
