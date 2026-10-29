# Changelog

All notable changes to kurin. Format follows Keep a Changelog; semver.

## Unreleased

### Added
- Rich terminal progress for `create cluster` / `add nodes`
  (`--log-format auto|plain|rich`; `auto` is rich on a TTY, plain when
  piped so scripts and e2e output stay stable). The rich renderer prints
  numbered phase headers, aligned node/task columns, `CHECK`/`VERIFY`
  levels with colors (`NO_COLOR` respected), per-task durations, skip
  reasons and indented remote stdout/stderr. `kurin status` now renders
  `NODE / ROLE / INTERNAL-IP / STATUS / RUNTIME / KERNEL` with colored
  readiness; `--json` carries the new fields unchanged in shape.
- `kurin versions` prints the supported Kubernetes matrix (embedded pin,
  online-create version, package-pinned versions, upgrade policy, k3s
  release) as text or `--json`. Online `create cluster` now rejects a
  Kubernetes version other than the embedded pin before any SSH work and
  points at `package create base --kubernetes <ver>`; package/legacy
  payloads keep their coverage-based validation. Configs below Kubernetes
  1.30 are rejected at load time (the `kube_version_min_required` gate).

## 0.5.0 - 2026-10-28

首个包含 Web UI、OCI 包、addon、私有 registry/seed 与 OS 矩阵扩展的发布。

### Added (WP-77)
- Opt-in seed registry (`[packages] delivery = "seed"`, v1alpha2 only): the
  control machine runs a read-only OCI distribution server against its local
  package/blob store on `127.0.0.1:0` and exposes it to every node through an
  SSH remote forward, so clusters without a registry node still get pull
  semantics for images (bootstrap an in-cluster registry addon from it). The
  `registry` phase plans `registry/seed-serve@controller` plus one
  `registry/seed-mirror@node` per member (loopback `http://` mirrors for
  containerd, insecure for CRI-O, http endpoints for k3s); `images/import`
  becomes a no-op and the runtime pulls through the node-local endpoint.
  Binaries keep streaming over SSH preload. The registry is loopback-only,
  unauthenticated, read-only (uploads and writes are 405) and dies with the
  operation. Load-time validation: no `[registry]` configuration may coexist,
  and an offline payload source (`packages.base` or
  `artifacts.offline-dir`) is required; v1alpha1 has no `[packages]` section
  and rejects the setting. Default behavior is unchanged.

### Added (WP-74)
- OS support matrix widened to Ubuntu 22.04, RHEL 9 / AlmaLinux 9,
  openEuler 24.03 LTS and Kylin Linux Advanced Server V10 (Ubuntu 24.04,
  Debian 12 and Rocky 9 unchanged). Facts now expose a stable
  `OsFamily` (`Deb` / `Rpm` / `Euler` / `Kylin` / `Other`) plus an
  `OsRelease` matrix enum; `kernel-deps` picks its package names and
  package manager (apt vs dnf) from the probed family instead of
  probing `command -v` at run time, and an OS that does not classify
  fails with a clear error naming it. openEuler and Kylin keep the
  standalone `ebtables` package RHEL 9 folded into `iptables-nft`.
- `kurin package create os` targets `ubuntu-22.04`, `rhel-9`
  (`redhat/ubi9`), `almalinux-9`, `openeuler-24.03` and `kylin-10`;
  openEuler/Kylin rows match their nodes (Kylin's `VERSION_ID=V10`
  normalizes to `10`). The `kylin-10` target has no verifiable public
  container image and is refused (`UnverifiedTargetImage`) until one is
  pinned locally, rather than shipping an unverifiable upstream.
  Kylin's VM images and ISOs are vendor-portal only: its e2e
  (`e2e/tier1/kylin*.sh`, `just tier1-kylin*`) is manual, like the
  other new per-OS tracks (`just tier1-ubuntu2204*`, `tier1-alma*`,
  `tier1-openeuler*`).

### Changed (WP-47)
- Dynamic storage provisioning moved from first-layer components to the
  declarative addon layer: `local-path` and `nfs-subdir` are installed by
  the addon engine (`addons/local-path@<node>` / `addons/nfs-subdir@<node>`
  in the `addons` phase instead of `storage/install@<node>` in `cni`).
- v1alpha1 `[storage]` lowers to `[addons.local-path]` /
  `[addons.nfs-subdir]` at load time, so both schema versions produce the
  same `ClusterSpec`; the object names, namespaces and StorageClass
  names stay exactly as before (`kurin-local-path` / `kurin-nfs`), so an
  existing cluster converges through server-side apply — nothing is
  deleted or recreated — and reports the storage task `InSync` once
  `kurin-state` records the addon. local-path's rendered image
  references now use their canonical `docker.io/` form (same digests).
- The addon engine now declares the built-in storage addons' images, so
  the cluster-agnostic offline bundle and base package keep carrying
  `local-path-provisioner`, `busybox` and `nfs-subdir-provisioner`.

### Changed (WP-48)
- cilium and kube-ovn install through their pinned Helm charts instead
  of the node-side `cilium` CLI and the pre-rendered
  `templates/cni/kube-ovn.yaml`: the chart archives (`cilium-chart`
  1.20.4 from helm.cilium.io, `kube-ovn-chart` v1.16.15 from
  kubeovn.github.io) are pinned in the artifact manifest with real
  upstream sha256, so they enter the offline bundle and base package
  like any other payload, and `cilium-cli` is gone from the node-level
  artifact set. The apply path reuses the addon engine's `HelmRunner`:
  upload helm + chart + values to the anchor, `helm template` for the
  image-integrity gate, `helm upgrade --install --wait=false`, then the
  readiness tracker (WP-18) watches the cilium DaemonSet + operator
  (kube-ovn: controller + ovn-central + ovs-ovn) with stuck diagnostics.
  kube-ovn's CIDRs and gateway now ride as chart values (`MASTER_NODES`
  pins the control planes), keeping the `kube-ovn/role=master` label
  pass upstream `install.sh` requires.
- existing-cluster takeover: when the legacy install left the resources
  without a matching helm release (raw `kubectl apply` for kube-ovn, a
  manual cilium install), the upgrade runs with `--take-ownership`
  (helm ≥ 3.17; the pinned 3.19.0 carries it — verified against
  `pkg/action/install.go`, which patches the unowned resources into
  the release). A failed takeover errors out with the old resources
  untouched; nothing is deleted. cilium keeps the CLI-era release name
  `cilium`, so a CLI-era cluster upgrades in place instead of leaving
  an orphaned release.
- `check` converges on the release itself: `helm status` must report
  `deployed`, the pinned chart version and values matching the rendered
  template (compared as parsed YAML→JSON, so key order does not matter)

### Added (WP-45)
- `kurin package create addon -f addons/<name>/addon.yaml` builds an
  offline-installable addon package (04 §3.4): every addon file becomes a
  file-set layer, an `oci://` chart is pulled into the package and
  `addon.yaml` is rewritten to the in-package archive, and every
  `spec.images` reference is pulled at its pinned digest (index or
  single-platform manifest) with the selected architectures'
  sub-manifests and blobs. `--arch` defaults to the addon's
  `spec.compat.arch`, else the host.
- `kurin package create base --addons a,b` embeds the selected addons:
  their files under `addons/<name>/`, their chart rows and their images
  join the base package, so one base package installs the selected
  addons offline. Entries are built-in catalog names, directories,
  `addon.yaml` paths or addon package sources.
- offline addon delivery (04 §5): preload adds an
  `addons/<name>/images@<node>` task per Kubernetes node that imports the
  addon's images before the addon is applied; registry delivery pushes
  them once in `registry/push` and adds their upstream hosts to the
  runtime mirror set. Addon images are resolved by reference from the
  package or the cache (`PayloadSet::image_ref` /
  `image_for_push_ref`), including single-platform manifests.
- coverage validation now checks the selected addons: a base package
  missing an addon image (or its files) is refused before anything
  touches a node, and `Addon::from_base_package` lets the selection read
  an addon straight out of `[packages] base`.
### Added (WP-55)
- TypeScript bindings generated from the server DTOs (`ts-rs`): every
  request/response type in `crates/kurin-server` derives `TS`, and
  `just ts-bindings` writes `web/src/api/generated/`. The same test that
  exports them compares the output with the committed files, so a DTO
  change without regenerating the bindings fails `just test`.
- The `serve` binary embeds the built SPA (`rust-embed`, only the `.gz`
  files Vite produces). `kurin-server/build.rs` fails with a clear
  "run `just web-build`" message when `--features ui` is built without
  `web/dist`; plain `cargo build`/`cargo test` still need no Node.
  Non-`/api` paths fall back to `index.html.gz` (client-side routes),
  while unknown `/api` paths keep the JSON error envelope.
- `just web-build` (install + Vite build, keeping only gzip) and
  `just ts-bindings`; `just release-musl` builds both architectures with
  `--features ui` and prints the embedded asset and binary sizes.

### Added (WP-54)
- The remaining `kurin serve` API surface (UI §7 v1.1):
  `GET /meta` (kurin version, the pinned component table of
  `create config --print-versions`, the component capability table),
  `GET /schema/cluster` (the v1alpha2 JSON Schema behind schemars),
  `POST /configs/render` (wizard DTO → canonical v1alpha2 TOML; an
  incomplete wizard step still gets its document plus diagnostics, since
  the wizard renders at every step and `POST /configs/validate` stays
  the judge), `POST /configs/validate` (diagnostics carry
  `{message, help, labels:[{offset,len,text}]}` spans into the submitted
  text), `POST /clusters/{name}/plan` (a read-only operation whose
  `result` is the `PlanView`; `?check=true` additionally discovers live
  facts and annotates every task with its drift), `POST /nodes/probe`
  (one-node discover; credentials are accepted only as env/file
  references and never echoed back), `POST /nodes/parse` (the quick-create
  bulk parser), `GET /addons/catalog` (configured sources of every
  registered cluster plus the embedded catalog, with values JSON
  Schemas), `GET /clusters/{name}/addons` (the `kurin-state` inventory
  the collector persists), the addon operations
  (`POST`/`PUT`/`DELETE /clusters/{name}/addons`, remove requires
  `confirm`), and `/packages*`: list/import/get/delete registrations,
  plus verify/push/create-base as clusterless operations whose reports
  land in the operation's `result`.
- Store schema v4: `operations.result` (JSON payload),
  `snapshots.addons` (the collected addon inventory), the `packages`
  registration table, and `one_active_op` extended to exempt the new
  lock-free kinds (`plan`, `package-verify`, `package-push`,
  `package-create-base`) the way `precheck`/`status` already were.
- The collector now reads the addon inventory alongside the core report,
  so `GET /clusters/{name}/addons` answers from the snapshot row.

### Changed (WP-49)
- v1alpha2 defaults a `role = "registry"` node without an explicit
  `[registry] kind` to `zot` (the OCI-native registry that carries
  packages and signatures). An explicit kind still wins, and v1alpha1
  keeps its original rule: a registry node requires `kind` and is never
  silently switched. `kurin create config` / quick-create now write
  `kind = "zot"` when `--registry` adds a node.
- `kind = "external"` is documented end to end: `address` is required,
  `namespace` (default `kurin`) must be slash-separated lowercase path
  segments, and `ca-file` must exist and only accompanies `external`.
  `e2e/tier3/registry.sh external` proves the external path against a
  zot registry (no install task, trust + push under `kurin/`).

### Added (WP-37)
- flannel, cilium and kube-ovn are air-gap ready: their images are
  pinned in the artifact manifest and enter the base package and the
  offline bundle through each CNI component's `artifacts()`, so the
  offline image set stays derived from the component registry. Kube-OVN
  (`docker.io/kubeovn/kube-ovn`, `docker.io/kubeovn/vpc-nat-gateway`) no
  longer needs online access, and its validation-time offline rejection
  is gone.
### Added (WP-44)
- `kurin addon` command family (03 §4.6): `list` (catalog plus the
  cluster's `kurin-state` inventory), `info [--schema]` (metadata, or the
  values JSON Schema alone for piping), `install` (`--version`,
  `--set`, `--values`, `--dry-run`, `--no-save`), `upgrade` (`--version`)
  and `remove` (`--keep-crds`).
- `install` / `upgrade` / `remove` rewrite the config's
  `[addons.<name>]` section with `toml_edit` (comments, blank lines and
  key order preserved) **after** the operation succeeds; `--no-save`
  turns the rewrite off. `install` also enables the entries the addon
  `requires`; `remove` sets `enabled = false` and keeps the values.
- each operation plans only the target addon (plus its `requires`), so
  `kurin addon` never re-converges unrelated addons.
- values are validated against the addon's `values-schema` before the
  operation starts; `--dry-run` prints the `discover → addons → post`
  plan without connecting to any node.
- `addon-engine`: `build_addon_plan_with` / `AddonPlanOptions` and
  `AddonRemoveTask::keep_crds` back `remove --keep-crds` (recorded CRDs
  survive the uninstall).

### Added (WP-38)
- `kurin package create os --target ubuntu-24.04|debian-12|rocky-9
  [--arch amd64,arm64] [--version 2026.10]`: builds the `os` dependency
  package (04 §3.4) by downloading the closure (conntrack, socat,
  ebtables, ipset, ipvsadm, haproxy, keepalived, nfs utilities) inside
  a container of the target distribution (docker/podman; arm64 under
  qemu) and generating the repository metadata (`dpkg-scanpackages` /
  `createrepo_c`). The repository tar is carried per arch with a
  `kurin-package.json` os row; `package inspect` lists the rows.
- node side: offline runs install system packages from the matched os
  repository — the tar is uploaded to `/opt/kurin/repo/<id>`
  (content-addressed, shared between tasks), apt installs through a
  temporary `[trusted=yes] file:` source scoped so unreachable mirrors
  cannot break it, dnf through `--repofrompath` (per-invocation, no
  config to remove); the temporary apt source list is removed after
  the install. `kernel_deps` and `external_lb` use it; online runs
  keep the previous apt/dnf path.
- os packages are matched from node facts (OS id + version prefix,
  arch), consulting `[packages] os` first and the base package's os
  rows second; an offline run with no match for any node fails in the
  validation phase with per-candidate rejection reasons.
- CI (`.github/workflows/os-packages.yml`) builds ubuntu-24.04,
  debian-12, rocky-9 × amd64/arm64 os packages and pushes them to
  `ghcr.io/weironz/kurin/os/<target>` (arm64 via qemu).
- ghcr domestic-mirror verification recorded in 04 §7 (2026-10-28):
  `ghcr.m.daocloud.io` (digest parity, existing zone entry) and
  `ghcr.nju.edu.cn` (reachability) are usable; the NJU fallback is now
  probed by `scripts/check-mirrors.sh`.

### Added (WP-36)
### Added (WP-46)
- first built-in addon catalog (03 §4.7): `addons/{metrics-server,
  local-path, nfs-subdir, ingress-nginx, cert-manager, metallb,
  headlamp, kube-prometheus-stack}` — each with `addon.yaml`, a values
  template, a `values.schema.json` (zh/en descriptions, common
  parameters only) and a README. Chart versions and image digests are
  fetched from upstream and verified; `scripts/check-mirrors.sh` now
  digest-checks every catalog image (both the upstream and the CN
  mirror form) and reads the mirror table from its new home in
  `kurin-core` (the `kurin-artifact` path had drifted).
- `spec.install.helm.chart` additionally accepts an `https://…tgz`
  chart archive URL next to package-relative paths and `oci://`
  references; in the `cn` zone helm pulls it through `gh-proxy.com`,
  and `oci://` chart registries are mirrored like image registries.
- `zone = "cn"` values templates swap every catalog image registry to
  its `*.m.daocloud.io` twin (digest-identical, verified), and the
  metallb addon turns its `addresses` value into an `IPAddressPool` +
  `L2Advertisement` through the post-install `apply` hook.
- registry-based distribution (04 §5.2): `[registry] kind = "external"`
  accepts `address`, `ca-file`, `username`, `password` and `namespace`
  (default `kurin`), and the new `registry/push@controller` task pushes
  every package image and file **once** from the control machine using
  the registry HTTP API (`curl`), keeping the upstream multi-arch index
  and skipping blobs the registry already has. Layout:
  `<reg>/mirror/<upstream-host>/<path>:<tag>` for images and
  `<reg>/kurin/files/<name>:<version>-<arch>` for single-layer file
  artifacts.
- node-side `artifacts/fetch-registry` downloads file blobs by digest
  with `curl --cacert … -u …`, verifies sha256 and hands them to the
  install task; a node whose facts report no `curl` falls back to
  preload streaming. Images are pulled through the runtime mirror
  instead of being imported.
- runtime mirrors (04 §5.4) cover containerd `certs.d` (`hosts.toml`
  with `override_path`), CRI-O `registries.conf.d/50-kurin.conf` and k3s
  `registries.yaml`; the upstream host set is the union of the package's
  image references. The registry phase now runs before artifact
  installation so nodes can fetch from it.
- `delivery = "auto"` resolves to `registry` when `[registry]` is
  configured (a `role = "registry"` node or `kind = "external"`),
  `preload` otherwise.

### Added (WP-35)
- package-based preload distribution (04 §5.1): `ArtifactsInstall`,
  `ImagesImport` and the registry node's own binary now take their
  payloads from the run's `ArtifactSources` — a base package
  (`oci-layout://`, `file://X.tar[.zst]`), a legacy offline bundle, or
  the online cache — and select the blob by the node's `facts.arch`.
  Payloads stream through `UploadSource::Blob` (path + pinned digest +
  size) in 64 KiB chunks, so a multi-hundred-MB binary or layer no
  longer sits in memory on the control host.
- images are re-hosted as a **single-image OCI layout tar** generated on
  the fly (`oci-layout` + `index.json` naming the selected platform's
  manifest + its blobs, deterministic headers) and streamed to the node:
  `ctr -n k8s.io images import` on kubeadm, the k3s agent images
  directory (`/var/lib/rancher/k3s/agent/images/`) on k3s. Only the
  requested architecture is carried.
- `--max-upload-parallel N` caps concurrent payload uploads across one
  operation (default 4), independently of task concurrency: uploads
  share the control host's uplink.
- a base package now executes with preload delivery instead of being
  refused at the delivery boundary; `registry` delivery still refuses
  until WP-36.

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
### Added (WP-52)
- `kurin serve` now runs operations: `POST /api/v1/clusters/{name}/operations`
  persists a `queued` row, takes the cluster `flock` and the database
  active-operation index, waits for the global `--max-concurrent-ops`
  semaphore (default 2) and executes `kurin-ops` in a background task.
- `GET /api/v1/operations/{id}/events` streams SSE: subscribe first, then
  backfill by `seq` (structured events from `op_events` merged with JSONL
  log lines), 15 s heartbeat, `Last-Event-ID` resume, and a disconnect when
  the bounded subscriber channel drops events so the client reconnects.
- `GET /api/v1/operations/{id}/logs` pages/filters the JSONL log
  (`after`, `before`, `limit`, `node`, `level`, `q`) through a sparse
  per-file `seq` index.
- `POST /api/v1/operations/{id}/cancel` and `/retry`; retries create a new
  operation with `retry_of` and, for create/add-nodes, `resume = true`.
- `GET /api/v1/clusters`, `POST /api/v1/clusters` (TOML import),
  `GET /api/v1/clusters/{name}` and the config revision endpoints needed
  by the operation flow.
- Startup marks `queued`/`running` operations `interrupted`; graceful
  shutdown refuses new submissions, cancels running operations at the next
  stage boundary and marks the rest interrupted.
- `scripts/api-smoke.sh`: manual curl flow import → precheck → create →
  live logs → status → delete.

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
- Deploy-side package access (WP-34): the `[packages]` config section
  (`base`/`os`/`delivery`/`verify`/`trusted-keys`) and `--package`
  now feed the run's `ArtifactSources`. `create cluster` validates a
  selected base package against the cluster after discover — the
  Kubernetes version, distribution, CNI and every node architecture
  must be covered, and each gap is listed (missing files, missing
  image sets, uncovered archs with their node names) before anything
  is installed. `artifacts.offline-dir` and `packages.base` are
  mutually exclusive; `verify = "signature"` requires
  `trusted-keys`.
- `--offline <DIR|URI>` is now equivalent to `--package <URI>` when it
  carries a URI (an `/os/` path segment files it under `packages.os`);
  a bare path keeps the legacy bundle-directory meaning. A bare path
  that turns out to be a package layout or a `.tar`/`.tar.zst`
  archive is recognised as one (compatibility reader), and a legacy
  bundle directory (`kurin-manifest.json`, no `oci-layout`) keeps its
  exact pre-package behaviour — existing air-gapped bundles need no
  rebuild. Because payload distribution from packages lands in
  WP-35/36, a mutating run that selected a base package fails
  explicitly instead of silently downloading online; `--dry-run`
  plans fully.
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
- `docs/reference/configuration.md`: v1alpha1/v1alpha2 comparison table and the
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
  `docs/reference/command-matrix.md` from `kurin capabilities --json`. The
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
  error. See `docs/reference/configuration.md`.
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
  (docs/project/v1.0-gates.md §2 allows additive fields). The default terminal
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
