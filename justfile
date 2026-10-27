# kurin — local build & test entry point (`just -l` lists all recipes).
# Local driver for the same commands the self-hosted-runner CI runs.

set shell := ["bash", "-uc"]
set dotenv-load := true

default:
    @just -l

# ── checks ────────────────────────────────────────────────────────────

# everything CI's fast job runs (fmt + clippy + unit tests)
check: fmt-check clippy clippy-ui test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# the `serve`/kurin-server surface behind `--features ui` (UI doc §4.2)
clippy-ui:
    cargo clippy -p kurin --features ui -- -D warnings

test:
    cargo test --workspace


# regenerate the committed config JSON Schema (drift fails `just test`)
schema:
    cargo run -q -p kurin -- config schema --version v1alpha2 > docs/schema/cluster.v1alpha2.json

# regenerate the committed addon.yaml JSON Schema (drift fails `just test`)
addon-schema:
    cargo run -q -p kurin-components --example addon_schema > docs/schema/addon.v1alpha1.json

# regenerate the `create cluster` table in docs/command-matrix.md from
# `kurin capabilities --json` (the e2e column stays hand-maintained)
docs-matrix:
    cargo build -q -p kurin
    python3 scripts/docs-matrix.py

# build the CLI binary
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

# optional arg: a package source (oci-layout://DIR, file://X.tar[.zst], oci://REG/...)
tier1-offline src="":
    e2e/tier1/offline.sh /tmp/kurin-bundle {{src}}

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

# private registry e2e (WP-36/49): kind = zot | docker-registry | harbor | external
tier3-registry kind="zot":
    e2e/tier3/registry.sh {{kind}}

tier1-flannel:
    e2e/tier1/flannel.sh target/tier1/debug/kurin

# pure-argument create with password auth (WP-26)
tier1-quick:
    CARGO_TARGET_DIR=target/tier1 cargo build -q -p kurin
    e2e/tier1/quick.sh target/tier1/debug/kurin

# password create + --setup-ssh-key, then passwordless status (WP-28)
tier1-ssh-key:
    CARGO_TARGET_DIR=target/tier1 cargo build -q -p kurin
    e2e/tier1/ssh-key.sh target/tier1/debug/kurin

# tier-1 built-in addon catalog (WP-46): per addon install -> ready -> remove.
# optional arg: online (default) | cn
tier1-addons mode="online":
    CARGO_TARGET_DIR=target/tier1a cargo build -q -p kurin
    e2e/tier1/addons.sh target/tier1a/debug/kurin "{{mode}}"

tier3-quick-ha:
    CARGO_TARGET_DIR=target/tier3 cargo build -q -p kurin
    e2e/tier3/quick-ha.sh target/tier3/debug/kurin

tier1-cilium:
    e2e/tier1/cilium.sh target/tier1/debug/kurin

tier1-k3s:
    e2e/tier1/k3s.sh target/tier1/debug/kurin

# k3s ops: status / certs / renew / delete on a k3s cluster
tier1-k3s-ops:
    CARGO_TARGET_DIR=target/tier1 cargo build -q -p kurin
    e2e/tier1/k3s-ops.sh target/tier1/debug/kurin

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

# Build + tag + publish a release. Usage: just release 0.4.0 [title]
release ver *title:
    #!/usr/bin/env bash
    set -euo pipefail
    ver="{{ver}}"
    # Always rebuild: a stale release-dist would silently publish the
    # previous version's binaries under the new tag (done once, never again).
    just release-musl
    git rev-parse "v$ver" >/dev/null 2>&1 || git tag "v$ver"
    notes=$(mktemp)
    awk -v ver="$ver" '
        BEGIN { insec=0; found=0 }
        /^## / {
            if (found) exit
            if ($0 ~ "^## " ver "([ .]|$)") { insec=1; found=1; next }
        }
        insec { print }
    ' CHANGELOG.md > "$notes"
    [ -s "$notes" ] || echo "See commits since the previous tag." >> "$notes"
    gh release create -R weironz/kurin \
        --title "{{title}}" --notes-file "$notes" "v$ver" \
        release-dist/kurin-x86_64-linux-musl.tar.gz \
        release-dist/kurin-x86_64-linux-musl.tar.gz.sha256 \
        release-dist/kurin-aarch64-linux-musl.tar.gz \
        release-dist/kurin-aarch64-linux-musl.tar.gz.sha256
    git push origin "v$ver"
    rm -f "$notes"

# tier-1 offline × CNI (flannel | cilium | kube-ovn): needs /tmp/kurin-bundle (just bundle)
tier1-offline-cni cni="flannel":
    e2e/tier1/offline-cni.sh "{{cni}}" /tmp/kurin-bundle


# tier-1 rocky offline (WP-38): base + rocky-9 os package, egress blocked.
# Builds the os package first (needs docker/podman); override with the
# env/arg if it was built elsewhere.
tier1-rocky-offline os="oci-layout:///tmp/kurin-os-rocky9":
    CARGO_TARGET_DIR=target/os cargo build -p kurin
    test -d /tmp/kurin-os-rocky9 || target/os/debug/kurin package create os \
        --target rocky-9 --arch amd64 -o "oci-layout:///tmp/kurin-os-rocky9"
    e2e/tier1/rocky-offline.sh "{{os}}"

# tier-3 k3s × external-endpoint (3 servers behind a script-managed haproxy)
tier3-k3s-external-endpoint:
    e2e/tier3/k3s-external-endpoint.sh
