# Changelog

All notable changes to kurin. Format follows Keep a Changelog; semver.

## 0.4.0 - 2026-10-27 (unreleased)

### Added
- `kurin-kube` ClusterApi (WP-17): one interface for `apply` (server-side,
  field manager `kurin`), `delete`, `get`/`list`, raw apiserver paths and pod
  logs, with two interchangeable backends — a kube-rs client over an SSH
  local forward to the anchor's `127.0.0.1:6443` (admin kubeconfig rewritten
  in memory, `tls-server-name: kubernetes`) and a `kubectl`-over-SSH
  fallback for nodes that forbid TCP forwarding. `status`, node labels and
  the storage provisioners now go through it; `Cmd`/`Session` gained stdin
  so manifests no longer need a staging file.
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
- `[ssh] known-hosts = "tofu" | "strict" | "accept-new"` (default `tofu`,
  node-level override): `strict` refuses unknown hosts instead of trusting
  them, `accept-new` learns them non-interactively. All modes reject a key
  that disagrees with the recorded entry, with the known_hosts path in the
  error. See `docs/configuration.md`.
- SSH port forwarding primitives: `Session::forward_local` (`ssh -L`,
  binds `127.0.0.1:0`, one `direct-tcpip` channel per connection, stops on
  `Drop`) and `Session::forward_remote` (`ssh -R`, returns the assigned
  port). These are the ClusterApi tunnel building blocks.
- Event v2 and operation logs (WP-06): `kurin-core::events` defines the
  `Envelope { v, seq, ts, op, body }` / `Body` schema, a monotonic
  `EventBus` with synchronous `EventSink`s (terminal renderer, JSONL
  writer, `--json=v2`) and bounded async subscribers that never block a
  publisher (a lagging subscriber resumes from JSONL by `seq`). Every
  operation writes `$XDG_STATE_HOME/kurin/logs/<cluster>/<op-id>.jsonl`
  (fallback `~/.local/state`) one envelope per line, flushed per event,
  and prints the path once at start. `kurin logs [--cluster C]
  [--op ID|--last] [--follow] [--node N] [--level L] [--json]` reads and
  filters those files.
- `tracing` is the task/logging bridge: a custom `tracing-subscriber`
  layer turns span fields (`op`/`phase`/`task`/`node`) and messages into
  `Body::Log`; the executor opens one span per task and wraps each session
  so every remote stdout/stderr line is published at debug level. Tasks
  and the upgrade/teardown/certs progress reporters use `tracing` instead
  of `eprintln!`.
- Global CLI flags: `--timestamps` (UTC timestamp prefix on terminal log
  lines), `-v` (info-level progress), `-vv` (every remote stdout/stderr
  line), and `--json=v2` (full envelopes). `--json` / `--json=v1` keep the
  frozen v1 event stream.
- Redaction: the bus carries a `Redactor` populated from the cluster's
  SSH, sudo and registry secrets; registered values are replaced with
  `******` in every log line, warning, task failure and operation error
  before publication.

### Changed
- `--json` v1 events gain exactly one field, `ts` (UTC milliseconds from
  the v2 envelope). The frozen v1 field sets are otherwise unchanged
  (docs/v1.0-gates.md §2 allows additive fields). The default terminal
  output is byte-identical to the WP-00 snapshots: remote stdout/stderr is
  hidden unless `-vv`, and `--timestamps` is opt-in.
- Mutating SSH commands are no longer transparently retried after a
  transport loss: the remote side may already have executed them. The
  transport returns a typed `kurin::transport_lost` error and the executor
  re-probes the node (`Outcome::Unknown`), never blind-retrying. Read-only
  probes (`Cmd::probe`/`readonly`, `file_sha256`, `service_active`) keep
  their single retry.
- Process-global artifact state is gone (WP-07): the download zone and the
  dynamic manifest override no longer live in `OnceLock`s. Every operation
  builds one `RunContext` carrying its `ArtifactSources` (zone, manifest,
  offline dir), so a resident process or one process driving several
  clusters can no longer leak one run's zone/manifest into another.
- Cross-task data now travels through the run's Blackboard instead of the
  control machine's `~/.cache/kurin/clusters/<cluster>` directory: kubeadm
  join material and the k3s node token are `Persist::RunStateSecret` keys,
  written to `$XDG_STATE_HOME/kurin/runs/<cluster>.bb.json` with mode 0600
  and registered with the event redactor. `--resume` restores them, so a
  resumed run can still join nodes; secrets are never written to the old
  cache path. The stale directory is removed once by `delete cluster`.

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
