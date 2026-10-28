# kurin

Declarative Kubernetes cluster installer written in Rust — a ground-up rewrite of
[kubekey](https://github.com/kubesphere/kubekey)'s install path, minus KubeSphere.

Status: **M1 + wave2 complete** — every roadmap batch landed and is verified by
real end-to-end tests ([`docs/project/progress.md`](docs/project/progress.md)).

## Documentation

- [文档索引](docs/README.md) — 入门 / 指南 / 参考 / 运维 / Docker / 开发
- [安装](docs/getting-started/install.md) · [快速上手](docs/getting-started/quickstart.md) · [纯命令行快速部署](docs/getting-started/cli-quickstart.md) · [Web UI](docs/getting-started/ui.md)
- [CLI 参考](docs/reference/cli.md) · [配置参考](docs/reference/configuration.md)
- [Docker：运行 CLI 与 UI](docs/docker/README.md)

## What it does

- **Nodes**: Ubuntu 22.04/24.04, Debian 12, Rocky/RHEL/Alma 9, openEuler 24.03,
  Kylin V10 (WP-74), amd64 + arm64
- **Distributions**: kubeadm-track (binary-first) or `kubernetes.distribution = "k3s"`
  (embedded-etcd HA included)
- **Runtime**: containerd (binary-first; no apt/yum for the Kubernetes stack)
- **CNI**: Calico (tigera-operator), Flannel, Cilium, or `none` — per config, not per build
- **HA**: kube-vip (static pod), `ha.mode = "external-endpoint"` (your own LB), or
  `ha.mode = "kurin-lb"` (kurin installs haproxy + keepalived on `role = "lb"` nodes;
  haproxy health-checks every backend via HTTPS `/readyz` and exposes
  `http://<lb>:8404/haproxy-stats`)
- **Kubernetes ≥ v1.30** — pinned compile-time version table; `--with-kubernetes`
  override; cross-minor upgrades via kubeadm's +1-minor path
- **Operations**: install / HA / multi-node / add-remove nodes / offline / mirror
  zone (`KURIN_ZONE=cn`) / upgrade / certs renewal / health status
- `create cluster` · `create config` · `add nodes` · `delete node` · `delete cluster`
  · `package create base|inspect` · `artifact` (deprecated alias) · `upgrade`
  · `certs check|renew` · `status`
  · `artifact` · `upgrade` · `certs check|renew` · `status`
- `serve` — the web UI backend (`--features ui` build; the release binaries
  include it): loopback by default, one-time token login, TLS optional
- `--dry-run` plan preview (fully offline); `--dry-run --check` live drift annotation

## Design

- **`kurin serve` (web UI).** Same binary, built with `--features ui`: axum
  skeleton, token→cookie auth and the security baseline from the
  [UI architecture](docs/ui/ui-architecture-final.md) §6.10 — loopback-only
  by default, `Host` allowlist, `application/json`-only mutations,
  `SameSite=Strict` session cookies, TLS via rustls. The first start prints
  the login link once and stores the token in `~/.local/share/kurin/token`
  (0600); `--rotate-token` replaces it and logs everyone out.

- **Planner / Executor split.** Planning is pure and offline; the executor is the
  only thing that opens SSH sessions. `plan` and `apply` cannot drift.
- **Idempotent tasks.** Every task is `check_state` → `apply` → `verify`. Re-running
  the same command skips everything already converged in milliseconds.
- **Unknown ≠ failed.** If SSH drops mid-apply the engine re-probes before retrying;
  it never blindly re-runs a change whose outcome is unknown.
- **Artifacts.** One manifest (versions, arch, URL, sha256) serves online and
  offline installs. `<cache>` doubles as the offline bundle; `kurin artifact` packs it.
- **Templates.** systemd units, kubeadm configs, CNI manifests, kube-vip and
  haproxy/keepalived configs live in `kurin/templates/` (MiniJinja), embedded via
  `rust-embed`. No YAML is ever assembled with `format!` in Rust code.

## Quick start

详细手把手教程：[`docs/getting-started/quickstart.md`](docs/getting-started/quickstart.md)（单节点 / HA / 离线 / 国内镜像 / FAQ）。
字段全表：[`docs/reference/configuration.md`](docs/reference/configuration.md)；集成插件见 [`docs/development/plugins.md`](docs/development/plugins.md)。

```bash
# install (musl static, x86_64/aarch64)
curl -fsSL https://raw.githubusercontent.com/weironz/kurin/main/scripts/install.sh | bash

# generate an annotated example config
kurin create config > cluster.toml

# preview the plan without touching anything
kurin create cluster -f cluster.toml --dry-run

# probe live drift (needs SSH access)
kurin create cluster -f cluster.toml --dry-run --check

# install
kurin create cluster -f cluster.toml
```

Minimal config (TOML or YAML, same shape):

```toml
version = "v1alpha1"
name = "prod"

[kubernetes]
version = "1.33.1"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[network.cni]
kind = "calico"

[ssh]
user = "root"

[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"

[nodes.worker01]
address = "192.168.10.21"
role = "worker"
```

### Cluster shapes

k3s distribution:

```toml
[kubernetes]
version = "1.33.1"
distribution = "k3s"

[network.cni]
kind = "flannel"   # k3s supports "flannel" | "none" in this build

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"
# 3 control-plane nodes + kv-vip gives k3s embedded-etcd HA
```

kubeadm HA with kube-vip:

```toml
[ha]
vip = "192.168.10.100"   # must NOT collide with any node address
mode = "kube-vip"        # default
```

HA with your own load balancer:

```toml
[ha]
vip = "192.168.10.100"       # <— your LB's address/port entry point
mode = "external-endpoint"   # kurin installs nothing; >= 2 control planes required
```

HA with kurin-managed LB nodes (haproxy + keepalived):

```toml
[ha]
vip = "192.168.10.100"
mode = "kurin-lb"

[nodes.lb1]
address = "192.168.10.31"
role = "lb"                  # excluded from every k8s task; gets haproxy+keepalived
```

## Air-gapped installs

```bash
# online machine: build the OCI package (binaries + all pinned control-plane/CNI
# images); `kurin artifact --out DIR` is the deprecated oci-layout:// alias
kurin package create base --arch amd64,arm64 -o file:///srv/kurin-base.tar.zst
kurin package inspect file:///srv/kurin-base.tar.zst

# offline targets: bundle path in config or via flag
# [artifacts]
# offline-dir = "/srv/kurin-bundle"
kurin create cluster -f cluster.toml
# or: kurin create cluster -f cluster.toml --offline /srv/kurin-bundle
```

Zero network requests during install. Image digests are pinned by OCI index.

## CN mirrors

```bash
KURIN_ZONE=cn kurin create cluster -f cluster.toml
# or in the config:
# [artifacts]
# zone = "cn"
```

Verified mirror table (digest parity checked by `scripts/check-mirrors.sh`, CI
`.github/workflows/mirrors.yml`): registries via daocloud mirrors, GitHub releases
via gh-proxy. `dl.k8s.io` has no maintained domestic mirror and stays direct. The
sha256 pins in the manifest remain the trust anchor regardless of mirror.

## Operations

```bash
kurin certs check  -f cluster.toml [--warn-days N]   # expiry report per control plane
kurin certs renew  -f cluster.toml [--force]         # kubeadm certs renew + gated restart
kurin status       -f cluster.toml [--json]          # nodes/pods/etcd/certs; exit code gates health
kurin upgrade      -f cluster.toml --with-kubernetes vX.Y.Z [--dry-run]
                                                   # cross-minor allowed (one minor at a time)
kurin logs [--cluster C] [--op ID | --last] [--follow] [--node N] [--level L] [--json]
                                                   # read the JSONL operation log
kurin import --from-cluster --host <cp-ip> --password-env VAR [-o FILE]
                                                   # rebuild the config from the running
                                                   # cluster's kurin-state record (WP-73)
```

Every operation except `import` writes
`$XDG_STATE_HOME/kurin/logs/<cluster>/<op-id>.jsonl` (fallback `~/.local/state`)
and prints that path once at start. Global flags apply to every command:
`--timestamps` prefixes terminal log lines with their UTC timestamp, `-v` shows
info-level progress plus a per-command remote-output summary, `-vv` also streams
every remote stdout/stderr line, and `--json` / `--json=v1` keep the frozen v1
event stream (now with a `ts` field) while `--json=v2` emits full envelopes.

## Adding, removing, tearing down

```bash
kurin add nodes   -f cluster.toml                 # joins only new nodes (incl. control planes)
kurin delete node worker01 -f cluster.toml        # drain + reset + wipe artifacts
kurin delete cluster -f cluster.toml              # everything, provably clean
```

`add nodes` installs the running cluster's version on new nodes (dynamic manifest),
not whatever was pinned at build time.

## Testing / contributing

[`just`](https://github.com/casey/just) is the local driver; these recipes also run
in CI on a self-hosted runner (`.github/workflows/`).

```bash
just check            # fmt + clippy -D warnings + unit tests
just tier0            # systemd containers: SSH, pool, artifact stages
just tier0-test       # two-pass idempotency assertions
just tier1            # single-node multipass cluster (kubeadm)
just tier1-offline    # install with egress to artifact hosts blocked (true air-gap)
just tier1-cilium     # single-node with Cilium CNI
just tier1-zone       # single-node install via domestic mirrors
just tier2            # 1 CP + 1 worker (real join + upgrade e2e)
just tier3            # 3 CP + kube-vip HA
just tier3-external-lb  # 2 CP + haproxy/keepalived LB node
just release-musl     # static x86_64+aarch64 tarballs into release-dist/
```

## License

Apache-2.0