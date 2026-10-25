# kurin

Declarative Kubernetes cluster installer written in Rust — a ground-up rewrite of
[kubekey](https://github.com/kubesphere/kubekey)'s install path, minus KubeSphere.

Status: **M1 in progress** (W0 contracts + W1 parallel tracks merged; tier-0 e2e wired).

## What it does (M1 scope)

- Nodes: Ubuntu 24.04+ / Debian 12 / Rocky Linux 9, amd64 + arm64
- Runtime: containerd (binary-first; no apt/yum for the Kubernetes stack)
- CNI: Calico via tigera-operator manifest
- Kubernetes ≥ v1.30 (pinned compile-time version table, `--with-kubernetes` override)
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
cargo build --release

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

## Testing

```bash
cargo test --workspace       # unit tests, no network
make tier0 && make tier0-test && make tier0-down   # 3 systemd containers, real SSH apply
make tier1                   # single-node multipass + real kubeadm (requires multipass)
```

## License

Apache-2.0