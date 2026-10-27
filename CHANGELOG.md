# Changelog

All notable changes to kurin. Format follows Keep a Changelog; semver.

## 0.4.0 - 2026-10-27 (unreleased)

### Added
- `kurin precheck`: read-only host audit before create (swap, kernel
  modules, sysctls, role-dependent ports, disk, clock, SELinux...),
  ok/warn/FAIL rows + `--json` event stream, exit 2 on failures.
- Private registry on `role = "registry"` nodes: `[registry]` with
  kind docker-registry | zot | harbor, TLS from a kurin-minted CA,
  basic auth, containerd mirror/trust wiring and bundle image seeding.
- `[storage]`: local-path and nfs provisioners, `kurin-local-path` /
  `kurin-nfs` classes, optional cluster-default.
- `prepare/os-baseline` (rpm-family convergence): swap off + fstab,
  SELinux permissive, firewalld stopped; kernel modules persisted in
  modules-load.d; rpm package names for kernel deps.
- Rocky 9 e2e (rpm family).
- Non-root SSH users are supported via sudo: `[ssh] become = "auto" |
  "sudo" | "none"` (default auto) escalates privileged commands with
  `sudo -n -- sh -c …`, or `sudo -S` when `[ssh] become-password` is
  set (the password travels the encrypted channel's stdin, never the
  command line). Node-level `become`/`become-password` overrides,
  become-aware precheck (sudo row + sysctl "settable via sudo"), and
  file uploads staged through `install` so modes survive escalation.
- Remote command output is streamed line by line (`Session::exec_stream`,
  `Stream::Stdout`/`Stderr`) with per-line redaction, 8 KiB line
  truncation and a 16 MiB capture cap, and uploads no longer buffer whole
  files: `Session::upload` streams `UploadSource::File` in 64 KiB chunks,
  `UploadReport` (bytes, digest, duration).
- Mixed-architecture clusters and facts-based precheck (WP-04): a new
  `discover` phase runs before the plan on `create cluster` / `add nodes`
  / `upgrade`, collecting per-node facts (arch, os-release, kernel,
  cgroup, CPU/mem/disk, sudo capability, existing kubelet/k3s/containerd)
  with one read-only script per node; artifacts, images, CRI-O and CNI
  payloads are then selected per discovered node arch, so an x86_64
  control machine can install arm64 nodes. `kurin precheck` renders its
  unchanged ok/warn/FAIL rows from the same probe. `nodes.<n>.arch`
  (amd64|arm64) is the `--dry-run` fallback (warned when unset), and
  `kurin artifact --arch amd64,arm64` builds an offline bundle covering
  every architecture.

### Fixed
- `kurin upgrade`: a failed `kubectl drain` now aborts that node's upgrade
  with `drain <node> failed: <reason>` instead of being masked by `; true`;
  no binaries are installed, no `kubeadm upgrade` runs and the node stays
  cordoned. This is a user-visible behavior change: upgrades that used to
  "succeed" while draining failed now stop. `--force-drain-failure` restores
  the old warn-and-continue behavior (explicitly marked as forced).
- containerd: `registry.config_path = /etc/containerd/certs.d` is now set;
  without it every certs.d mirror/CA/auth was silently ignored.
- base64 table: UVWYYZ typo (Y replaced X) — visible only in credentials
  whose bytes used the X slot; now covered by a 0..255 sweep test.

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
