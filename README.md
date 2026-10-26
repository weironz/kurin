# kurin

Declarative Kubernetes cluster installer written in Rust — a ground-up rewrite of
[kubekey](https://github.com/kubesphere/kubekey)'s install path, minus KubeSphere.

Status: **M1 in progress** (W0 contracts + W1 parallel tracks merged; tier-0 e2e wired).

## What it does (M1 scope)

- Nodes: Ubuntu 24.04+ / Debian 12 / Rocky Linux 9, amd64 + arm64
- Runtime: containerd (binary-first; no apt/yum for the Kubernetes stack); K3s
  distribution available (`kubernetes.distribution = "k3s"`)
- CNI: Calico (tigera-operator), Flannel, or `none` (Cilium in wave2 integration)
- Kubernetes ≥ v1.30 (pinned compile-time version table, `--with-kubernetes` override,
  cross-minor upgrades supported via kubeadm's +1-minor path)
- Single control plane and HA (kube-vip)
- Commands: `create cluster` / `create config` / `add nodes` / `delete node` /
  `delete cluster` / `artifact`
- `--dry-run` plan preview (offline); `--dry-run --check` live drift annotation

## Design

- **Planner / Executor split.** Planning is pure and offline; the executor is the
  only thing that opens SSH sessions. `plan` and `apply` cannot drift.
- **Idempotent tasks.** Every task is `check_state` → `apply` → `verify`.
  Re-running the same command skips everything already converged in milliseconds.
- **Unknown ≠ failed.** If SSH drops mid-apply the engine re-probes before
  retrying; it never blindly re-runs a change whose outcome is unknown.
- **Artifacts.** One manifest (versions, arch, URL, sha256) serves online and
  offline installs. `<cache>` doubles as the offline bundle.
- **Templates.** systemd units, kubeadm configs and CNI manifests live in
  `kurin/templates/` (MiniJinja), embedded via `rust-embed`. No YAML assembled
  with `format!` in Rust code.

Details: [`docs/m1-spec.md`](docs/m1-spec.md), progress: [`docs/progress.md`](docs/progress.md).

## Usage

```bash
# install a release (musl static, x86_64/aarch64)
curl -fsSL https://raw.githubusercontent.com/weironz/kurin/main/scripts/install.sh | bash

# or build locally
just build

# generate an annotated example config
./target/release/kurin create config > cluster.toml

# preview the plan without touching anything
./target/release/kurin create cluster -f cluster.toml --dry-run

# probe live drift (needs SSH access)
./target/release/kurin create cluster -f cluster.toml --dry-run --check

# install
./target/release/kurin create cluster -f cluster.toml

# machine-readable event stream
./target/release/kurin create cluster -f cluster.toml --json
```

Config (TOML primary, YAML equivalent accepted):

```toml
version = "v1alpha1"
name = "prod"

[kubernetes]
version = "1.33.1"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"
```

## Air-gapped installs

```bash
# on a machine with network: build the bundle (binaries + all 15 pinned images)
kurin artifact --out /srv/kurin-bundle

# on the target network: point the config at the bundle (or --offline <dir>)
# [artifacts]
# offline-dir = "/srv/kurin-bundle"
kurin create cluster -f cluster.toml
```

Everything (binaries, control-plane images, CNI) comes from the bundle; the
install makes zero network requests. Image digests are pinned by OCI index.

## CN mirrors

```bash
KURIN_ZONE=cn kurin create cluster -f cluster.toml
# or in the config:
# [artifacts]
# zone = "cn"
```

Verified mirror table (digest parity checked by `scripts/check-mirrors.sh`,
CI: `.github/workflows/mirrors.yml`): registries via daocloud mirrors,
GitHub releases via gh-proxy. `dl.k8s.io` has no maintained domestic mirror
and stays direct. The sha256 pins in the manifest remain the trust anchor.

## Operations

```bash
kurin certs check  -f cluster.toml [--warn-days N]
kurin certs renew  -f cluster.toml [--force]      # kubeadm certs renew + gated restart
kurin status       -f cluster.toml [--json]        # nodes/pods/etcd/certs; exit code gates health
kurin upgrade      -f cluster.toml --with-kubernetes vX.Y.Z [--dry-run]
```

## Adding, removing, tearing down

```bash
kurin add nodes   -f cluster.toml       # idempotent: joins only new nodes
kurin delete node worker01 -f cluster.toml   # drain + reset + wipe artifacts
kurin delete cluster -f cluster.toml
```

## Testing / CI

[`just`](https://github.com/casey/just) is the local driver; the same recipes
run in CI on a self-hosted runner (`.github/workflows/`).

```bash
just check            # fmt + clippy -D warnings + unit tests
just tier0            # 3 systemd containers, real SSH apply
just tier0-test       # two-pass idempotency assertions
just tier0-down

just tier1            # single-node multipass + real kubeadm
just tier1-offline    # install with egress to artifact hosts blocked
just tier2            # 1 control-plane + 1 worker (real join)
just tier3            # 3 control-plane + kube-vip HA
just ci-gate          # check + tier0 + tier0-test + down
just release-musl     # static binaries both arches into release-dist/
```

## License

Apache-2.0