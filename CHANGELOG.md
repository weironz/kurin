# Changelog

All notable changes to kurin. Format follows Keep a Changelog; semver.

### Added (WP-42)
- helm-based addon execution: the pinned `helm` 3.19.0 binary (official
  sha256 for amd64/arm64) enters the artifact manifest and the base
  package as a control-machine payload. `HelmRunner` uploads the binary,
  the chart archive and the merged `values.yaml` into a work dir on the
  anchor control plane, renders with `helm template` (the output's
  images are checked against `spec.images` before anything reaches the
  cluster), installs with `helm upgrade --install --namespace N
  --create-namespace -f values.yaml --wait=false`, and reads
  `helm status -o json` / `helm uninstall`. The admin kubeconfig follows
  the distribution (`/etc/kubernetes/admin.conf` for kubeadm,
  `/etc/rancher/k3s/k3s.yaml` for k3s).
- manifests addons are rendered on the control machine and applied
  through `ClusterApi` with server-side apply. Both install paths return
  the applied object list for the readiness tracker (WP-18).

### Fixed (WP-33)
- `kurin package inspect <src>` panicked on every invocation since
  WP-32: the global event-stream `--json` (`Option<JsonArg>`) and the
  subcommand's `--json: bool` shared the arg id `json`, and clap's
  derive resolved the root field against the subcommand's value
  ("Could not downcast to bool"). The subcommand flag is now
  `--manifest` (print `kurin-package.json`); the global `--json`
  keeps its meaning and spelling. New subcommands use `--format json`
  for the same reason — clap requires long names to be unique per
  command, so any subcommand-level `--json` would collide.
- `PackageBuilder::build` never transferred the image's own index
  document (`image.digest`), only its sub-manifest and blobs, leaving a
  dangling descriptor in the layout: `package verify`/`push` on such a
  package failed with `NotFound`. The index document is now part of
  every built package.

### Added (WP-33)
- `kurin package push <src> <dst>` / `pull <src> <dst>`: copy a
  package between any two of the three transport forms
  (`oci://REG/NS/NAME[:TAG]`, `oci-layout://DIR`, `file://X.tar`,
  `file://X.tar.zst`), sending only what the destination is missing
  (registry: HEAD first; local: digest lookup). Registry auth via
  `--user` + `--password-env`, else `~/.docker/config.json`, else
  anonymous. `oci://` loopback registries default to plain HTTP like
  Docker's insecure-localhost rule; `oci://http://`/`oci://https://`
  override. Child manifests are pushed before the index (WP-30 spike:
  distribution/harbor reject an index whose children are missing).
- `kurin package verify <src>`: full digest verification — re-hash
  every blob and manifest in the package's closure, in any form.
  Signature verification stays WP-39.
- `kurin package inspect [--manifest] [--sbom]`: summary (default),
  full `kurin-package.json` (`--manifest`), or the SPDX document
  (`--sbom`, rendered from the verified metadata; referrer
  attachment is WP-39). Registry sources are pulled into a scratch
  layout (verified on arrival) and inspected from there.
- `kurin package list [--format text|json]` / `prune [--dry-run]`:
  maintain the local content-addressed cache
  (`~/.cache/kurin/blobs`); prune removes blobs no artifact cache
  entry references.
- New deps in `kurin-artifact`: `oci-client` 0.16.1 (rustls, no
  default features), `http` 1, `base64` 0.22, `futures-util` 0.3,
  `tokio-util` (io); dev `bcrypt`. Cross-compilation of the same set
  was verified for x86_64-musl and aarch64-musl by the WP-30 spike.

### Changed
- `kurin artifact --out DIR` now builds the OCI package via
  `kurin package create base -o oci-layout://DIR` and prints a
  deprecation warning; the layout it writes (oci-layout +
  kurin-package.json) is not the legacy directory tree, so
  `--offline` consumers should keep using kurin ≥ this release or
  rebuild old-style bundles with a previous version. `artifact` will be
  removed two minors after this release (04 §4 compat note).
- Fixed a pre-existing pin bug exposed by the package e2e: the embedded
  manifest addressed busybox as `docker.io/library/busybox:v1.36.1`,
  but Docker Hub tags busybox without the `v` prefix, so the fetch
  404'd. The template now uses `{version}`; the pinned index digest
  (`sha256:73aaf090…`) was verified to match tag `1.36.1` upstream.

### Added
- Addon rendering (WP-41): `[addons.<name>]` values are now merged
  (`defaults` → the addon's built-in recommended values from its
  `values.yaml.j2` → user `values` → `raw-values`) and rendered with the
  documented MiniJinja context (`cluster`, `kubernetes`, `network`,
  `ha`, `registry.mirror`, `nodes`, `addon.values`, `zone`). User
  `values` are validated against the addon's `values-schema`; violations
  name the config location (`addons.<name>.values.<path>`) and point at
  the offending line. `{ env = "…" }` / `{ file = "…" }` secret
  references inside values are resolved in memory and registered with the
  redactor, so they never reach logs or diagnostics; `raw-values` pass
  through explicitly unvalidated. Rendered manifests are checked for
  image integrity: every image they reference must be declared
  (digest-pinned) in `spec.images`.
- `kurin add nodes -f cfg --worker IPS [--control-plane IPS] [--node KV]`
  (WP-27): the nodes are appended to the config file with `toml_edit`
  (comments, blank lines and key order preserved; automatic names
  continue the `role-index` sequence, e.g. `worker-2`), then the same
  add-nodes operation runs. `--no-save` merges the nodes in memory only.
  `--set PATH=VALUE` (WP-26) is covered end to end here: kebab-case
  dotted paths reach nested tables such as `nodes.cp-1.labels.zone`,
  values are parsed as TOML literals with a string fallback, and an
  unknown path is rejected at validation time as `unknown field`,
  pointing at the merged document titled “来自命令行参数”.
- `--setup-ssh-key [PUB]` and `--ask-become-pass` (WP-28):
  `create cluster` can bootstrap key authentication from a password —
  the control machine's public key (default `~/.ssh/id_ed25519.pub`; an
  ed25519 pair is generated with a 0600 private key when missing, or the
  `.pub` derived from an existing private key) is appended to every
  node's `~/.ssh/authorized_keys` over the password session before
  discover. The push is idempotent: nodes that already carry the key are
  skipped, and the append command repeats the same `grep` guard so a
  replay can never duplicate the line. The private key's login is then
  verified on every node and the saved config switches to
  `ssh.auth.method = "private_key"` (per-node password overrides
  included); later operations such as `status` need no password. `--dry-run`
  and `create config` reject the flag (no session to push over).
  `--ask-become-pass` prompts for the sudo password without echoing and
  never writes it down — the saved config keeps only
  `{ env = "KURIN_BECOME_PASSWORD" }`. `--ssh-key-passphrase-env` now
  also pairs with `--setup-ssh-key` (for an existing encrypted key).
- Quick-create parameters (WP-26): `kurin create cluster` and
  `kurin create config` can build a cluster from command-line arguments
  only — `--control-plane/--worker/--lb/--registry` (with IP ranges),
  `--node key=value`, `--node-name`, `--ssh-user/--ssh-key/--ask-pass/
  --ssh-password-env/--ssh-password-file/-p`, `--become`,
  `--known-hosts`, `--kubernetes/--distribution/--runtime/--cni`,
  `--pod-cidr/--service-cidr/--dns-domain`, `--vip/--ha-mode`,
  `--zone`, `--package/--offline`, `--addon/--addon-set`, `--set`.
  Arguments lower to a `ConfigPatch`, merge into a v1alpha2 `toml_edit`
  document (file → appended nodes → named arguments → `--set`) and load
  through the same pipeline as a file; diagnostics point at the merged
  TOML titled “来自命令行参数”. After discover a summary table is
  printed and confirmation is required unless `--yes` (a non-TTY without
  `--yes` is an error). The equivalent config is saved to
  `./<name>.kurin.toml` (`--save`/`--no-save`/`--force-save`); plaintext
  passwords from `-p`/`--ask-pass` never reach the file, which stores
  `{ env = "KURIN_SSH_PASSWORD" }` instead.
- Machine-readable config schema (WP-22): `kurin config schema
  [--version v1alpha2]` prints the JSON Schema derived from the config
  types (doc comments become `description`, unknown fields are closed).
  The v1alpha2 export is committed at `docs/schema/cluster.v1alpha2.json`
  (`just schema` regenerates it; a test fails on drift), and every sample
  config validates against it via the `jsonschema` crate.
- Schema `v1alpha2` (WP-21): new configs may use `version = "v1alpha2"`
  with `[runtime] kind` (replacing `kubernetes.runtime`),
  `kubernetes.distribution = "kubeadm" | "k3s"` (`"k8s"` remains an
  alias), `[packages]`, `[addons.<name>]`
  (`enabled/version/source/values/raw-values`, unknown names rejected),
  `[registry] kind = "external"` (reserved) and no `[storage]`
  (`addons.local-path` / `addons.nfs-subdir` replace it). The loader
  dispatches on `version`; both versions lower to the same `ClusterSpec`,
  so v1alpha1 files keep loading and behaving identically.
- `kurin config migrate -f old.toml [-o new.toml]`: rewrites a v1alpha1
  document as v1alpha2 with `toml_edit`, preserving comments, blank lines
  and key order. The result is verified to load to an identical
  `ClusterSpec`; fields without an automatic mapping fail with the manual
  step. Without `-o` the document is printed to stdout.
- `kurin create config` now prints the v1alpha2 example by default;
  `--schema-version v1alpha1` prints the old one.
- `docs/configuration.md`: v1alpha1/v1alpha2 comparison table and the
  v1alpha2 `[addons]` / `[packages]` reference.
- Secret references (WP-20): every secret config field (`ssh.auth.password`,
  `ssh.auth.passphrase`, `ssh.become-password`, node overrides and
  `registry.password`) now accepts `{ env = "VAR" }` and
  `{ file = "/path" }` in addition to the literal v1alpha1 form. Secrets
  are resolved exactly once in `kurin-ops::prepare` into `SecretString`s,
  registered in the run's redaction table, and used by the SSH pool and
  registry tasks; `Debug` masks literals and serialization keeps the
  reference form. A secret file wider than `0600` warns. `--dry-run`
  without `--check` does not read env/file secrets.
- The internal model is now `ClusterSpec` (ADR-15): component-owned
  sections (`registry`, `storage`, `cni`) and artifact sources
  (`packages`) are sub-structs; `Cluster`/`Node` remain type aliases.
- `kurin serve` (WP-50, built with `--features ui` — the release binaries
  include it): the `kurin-server` crate skeleton — axum router behind the
  UI §6.10 guards (Host allowlist incl. HTTP/2 `:authority`, JSON-only
  mutations, session cookies `HttpOnly; SameSite=Strict`, drain → 503),
  `GET /healthz`, `POST|GET|DELETE /api/v1/session` (token → cookie
  exchange, logout) and a placeholder `GET /api/v1/meta` returning the
  version. First start writes the access token to
  `<data>/token` (0600, default `~/.local/share/kurin`) and prints the
  `#token=` login link exactly once; `--rotate-token` replaces it and
  invalidates every session. Non-loopback listeners require
  `--tls-cert/--tls-key` (rustls) or an explicit `--allow-insecure-http`;
  extra hosts go through `--public-host`. Request logging goes to `tracing`
  under the `kurin_server::request` target, separate from operation logs.
  New CLI surface only — default builds are unchanged.
- The cluster records what kurin installed in
  `kube-system/ConfigMap kurin-state` (WP-19, ADR-09): `kurin-version`,
  `op-id`, `op-kind`, `finished-at`, the canonical **redacted** `spec.toml`,
  `components.json` (enabled component id -> version/source),
  `addons.json` and `history.json` (last 20 operations). `add nodes` reads
  the running version from the record instead of probing `/version` (old
  clusters without a record keep the probe), and `status` prints the
  expected (record) vs observed (`/version`) version.
- `kurin package create base` / `kurin package inspect` (WP-32): the
  OCI package format of 04 §3 — one OCI image index
  (`artifactType: application/vnd.kurin.package.v1`) carrying the
  metadata manifest (`kurin-package.json`, schema
  `docs/schema/package.v1.json`), one platform-tagged file-set manifest
  per architecture, and every pinned image at its **upstream digest**
  (annotation `dev.kurin.image.ref`). Outputs: `oci-layout://DIR`,
  `file://X.tar`, `file://X.tar.zst`. `--kubernetes` re-pins via the
  dynamic manifest, `--cni all|none|calico,flannel,cilium,kube-ovn`
  and `--arch amd64,arm64` filter the component aggregation. An SPDX
  2.3 JSON SBOM is written next to the output as `<out>.spdx.json`
  (WP-39 will attach it as a referrer). `oci://` push/pull and
  `verify` land in WP-33.
- `kurin capabilities --json` (hidden subcommand): the aggregated
  component capability table (id, kind, distributions, archs, offline,
  zones, ops) — the single source of truth behind `just docs-matrix` and
  the upcoming Web capability table. All built-in components (ha, cni,
  registry, storage, post join the WP-15 os/runtime/distro set) are now
  registered in `kurin_components::registry()`; the offline bundle and
  the bootstrap image import set are aggregated from each component's
  `artifacts()` declaration instead of hand-maintained lists (parity
  tests pin both to the previous content).
- `just docs-matrix`: regenerates the `create cluster` table in
  `docs/command-matrix.md` from `kurin capabilities --json`. The
  support/reject cells are generated; the e2e column stays manually
  maintained.

- nelm-style readiness tracking (WP-18): `kurin-kube::track` waits for a set
  of objects until every one satisfies its readiness rule — Deployment,
  StatefulSet, DaemonSet, Job, Pod, PVC, CRD, APIService and Node have
  built-in rules, unknown kinds use the `Ready`/`Available` condition
  heuristic — publishing `resource` events only on state change plus a
  30 s heartbeat. On timeout or a failed state (`CrashLoopBackOff`,
  `ImagePullBackOff`, `ErrImagePull`, failed Job) it lists the related
  pods' last 20 events and the problem container's last 50 log lines as
  warn logs and names the most likely cause (e.g. an image pull failure
  with the registry mirror hint). The CNI installs (calico, flannel,
  cilium, kube-ovn) and the storage provisioners now wait through it,
  keeping their previous pass/fail semantics and timeouts.
- Mutating operations are mutually exclusive per cluster: `create cluster`,
  `add nodes`, `delete node`, `delete cluster`, `upgrade` and `certs renew`
  take an OS file lock on `$XDG_STATE_HOME/kurin/locks/<cluster>.lock`
  (fallback `~/.local/state`). A second invocation fails immediately and
  names the holder's pid, operation id, command line and start time.
  Read-only commands never lock. `--resume` now refuses a changed plan:
  the run state records a `sha256` fingerprint of the task keys and their
  descriptions, and a mismatch reports
  `计划已变化，请去掉 --resume 重新运行（已收敛的任务会被检查跳过）`.
  RunState is v2 (`op_id`, `plan_fingerprint`, `done`, `blackboard`, mode
  0600); v1 files still resume with no fingerprint.
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
- Operations moved behind `kurin-ops` (WP-14): `kurin/src/main.rs` now only
  parses argv, and every operation goes through
  `OpRequest -> prepare -> plan -> execute`. No user-visible change: CLI
  output, plan trees and `--json` events are unchanged. `status` serializes
  its `StatusReport` directly for `--json` (same fields), can skip the
  certificate probe (`StatusOpts { certs }`), and tries the remaining
  control planes when the first one is unreachable. `certs renew` is now a
  plan: each control plane runs `renew -> restart-static-pods ->
  apiready-gate` as a `rolling(1)` group, so the next node starts only
  after the previous apiserver answers. The `create cluster` HA warnings
  are published as warn-level log events (terminal text unchanged; `--json`
  and the JSONL log now contain them).
- Internal: the os, runtime and distro areas now contribute to the create
  plan through a `Component` trait and an explicit registry in
  `kurin-components` (WP-15) instead of planner functions; the k3s
  distribution's validation rules moved from `kurin-config` into the
  `distro/k3s` component with identical error text. No user-visible
  change: plan snapshots, CLI output and `--json` events are byte-identical.
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
- The executor now schedules over the plan's dependency graph instead of
  running phase by phase (WP-11): a task starts as soon as its edges are
  satisfied, at most 32 tasks run at once and at most 4 per node (the SSH
  channel count), and rolling groups run one batch at a time. Phase
  barriers are ordinary edges, so the CLI output and the plan snapshots are
  unchanged for a converging run. Tasks that never run because a dependency
  failed are now reported instead of disappearing: `task-skipped` gained
  one **optional** `reason` field (`blocked by <key>`), absent for an
  already-in-sync task, so the frozen v1 shape is unchanged; the terminal
  prints `skip  <id> (blocked by <key>)`. A task that declares
  `FailurePolicy::BestEffort` publishes `task-warn` (v2) / `warn:` on the
  terminal and its successors still run. Cancellation stops new tasks,
  lets in-flight ones finish and reports `OpFinished { result: cancelled }`.
- `delete node` and `delete cluster` run through the planner and executor
  (WP-12, ADR-02). Every teardown step is a task with a convergence check —
  `drain`, `delete-node-object`, `kubeadm-reset` / `k3s-uninstall`,
  `stop-services`, `wipe-dirs`, `cni-cleanup`, `iptables-cleanup` and, for a
  whole cluster, `local-state-cleanup` — so both commands gain
  `--dry-run [--check]` (plan tree / drift) and a plan fingerprint.
  `delete cluster` resets and wipes workers before the control plane
  (explicit edges); the API-side steps run on the anchor control plane and
  an unreachable anchor is a warning, not a failure. An unreachable node no
  longer aborts the whole teardown: its steps warn and the rest of the
  cluster is still cleaned up.
- Teardown cleanup failures are explicit instead of silent: the old
  `; true` steps became `FailurePolicy::BestEffort`, so a failing step
  publishes `task-warn` (`warn:` on the terminal) and is listed per node in
  the closing summary, while the command still exits 0. The terminal keeps
  the historical `[node] ok <step>` lines; the executor's per-task
  `done`/`skip` lines are not printed for teardown. `delete cluster` now
  runs k3s's own `/usr/local/bin/k3s-uninstall.sh` when present (falling
  back to `systemctl disable --now k3s`), and both commands remove leftover
  CNI devices, the pod-CIDR route and kube-proxy/CNI iptables chains.
- `kurin upgrade` runs as a plan through the executor (WP-13, ADR-02).
  Each node is a task chain — `version-check` → `cordon` → `drain` →
  `install-bins` → `kubeadm-upgrade` (first control plane `apply`, the rest
  `node`) → `restart-kubelet` → `ready-gate` → `uncordon`. The first
  control plane runs alone, the remaining control planes roll one at a
  time, and workers roll in `--batch` groups. `upgrade --dry-run` now
  prints the plan tree (the current → target version header is unchanged),
  `--offline` reuses the same plan with the bundle as the artifact source,
  and nodes already at the target are `InSync` (no cordon, drain, install
  or restart). A failed drain still aborts the node and blocks every later
  node/batch, and never uncordons; `--force-drain-failure` keeps the old
  warn-and-continue behavior.

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
