# kurin — local build & test entry point (`just -l` lists all recipes).
# Local driver for the same commands the self-hosted-runner CI runs.

set shell := ["bash", "-uc"]
set dotenv-load := true

default:
    @just -l

# ── checks ────────────────────────────────────────────────────────────

# everything CI's fast job runs (fmt + clippy + unit tests)
check: fmt-check clippy test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

build:
    cargo build --release

# ── release (musl static) ─────────────────────────────────────────────

# musl static binaries for both arch into release-dist/
release-musl:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf release-dist && mkdir -p release-dist/x86_64 release-dist/aarch64
    echo "[release] x86_64-unknown-linux-musl (native musl-gcc)"
    CARGO_TARGET_DIR=target/release-musl cargo build --release \
        --target x86_64-unknown-linux-musl -p kurin
    cp target/release-musl/x86_64-unknown-linux-musl/release/kurin release-dist/x86_64/kurin
    echo "[release] aarch64-unknown-linux-musl (zig cross)"
    CARGO_TARGET_DIR=target/release-musl cargo zigbuild --release \
        --target aarch64-unknown-linux-musl -p kurin
    cp target/release-musl/aarch64-unknown-linux-musl/release/kurin release-dist/aarch64/kurin
    # Sidecars must carry bare filenames: they are verified wherever
    # they are downloaded (curl | sh from any cwd).
    for a in x86_64 aarch64; do
        tar -czf "release-dist/kurin-$a-linux-musl.tar.gz" -C release-dist/$a kurin
        ( cd release-dist && sha256sum "kurin-$a-linux-musl.tar.gz" > "kurin-$a-linux-musl.tar.gz.sha256" )
        echo "[release] release-dist/kurin-$a-linux-musl.tar.gz"
    done

# ── e2e (multipass / containers) ─────────────────────────────────────

tier0:
    e2e/tier0/up.sh

tier0-test:
    CARGO_TARGET_DIR=target/tier0 cargo build -p kurin
    python3 e2e/tier0/assert.py --bin target/tier0/debug/kurin --config e2e/tier0/config.toml

tier0-down:
    e2e/tier0/down.sh

tier1:
    e2e/tier1/up.sh

tier1-test:
    CARGO_TARGET_DIR=target/tier1 cargo build -p kurin
    e2e/tier1/assert.sh target/tier1/debug/kurin e2e/tier1/config.toml

tier1-offline:
    e2e/tier1/offline.sh /tmp/kurin-bundle

tier1-down:
    e2e/tier1/down.sh

tier2:
    e2e/tier2/up.sh

tier2-test:
    CARGO_TARGET_DIR=target/tier2 cargo build -p kurin
    e2e/tier2/assert.sh target/tier2/debug/kurin e2e/tier2/config.toml

tier2-down:
    e2e/tier2/down.sh

tier3:
    e2e/tier3/up.sh

tier3-test:
    CARGO_TARGET_DIR=target/tier3 cargo build -p kurin
    e2e/tier3/assert.sh target/tier3/debug/kurin e2e/tier3/config.toml

tier3-down:
    e2e/tier3/down.sh

tier1-flannel:
    e2e/tier1/flannel.sh target/tier1/debug/kurin

tier1-cilium:
    e2e/tier1/cilium.sh target/tier1/debug/kurin

tier1-k3s:
    e2e/tier1/k3s.sh target/tier1/debug/kurin

tier3-k3s-ha:
    e2e/tier3/k3s-ha.sh target/tier3/debug/kurin

tier3-external-lb:
    e2e/tier3/external-lb.sh target/tier3/debug/kurin

tier2-upgrade-minor:
    e2e/tier2/upgrade-minor.sh target/tier2/debug/kurin

tier1-zone:
    e2e/tier1/zone.sh target/tier1/debug/kurin

tier2-add-cp:
    e2e/tier2/add-cp.sh

tier2-certs:
    e2e/tier2/certs.sh

tier2-status:
    e2e/tier2/status.sh

# full local gate before pushing anything that touches behavior
ci-gate: check tier0
    just tier0-test
    just tier0-down

# offline bundle for tier1-offline
bundle:
    target/debug/kurin artifact --out /tmp/kurin-bundle || cargo run -- artifact --out /tmp/kurin-bundle
