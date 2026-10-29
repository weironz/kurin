#!/usr/bin/env bash
# tier-1 offline: single-node install from the prebuilt bundle with the VM's
# egress blocked — proves the bundle is self-contained (no registry, no
# dl.k8s.io, no github).
#
# Usage: e2e/tier1/offline.sh [bundle-dir] [package-uri]
#
# The optional second argument selects a package source instead of the
# legacy bundle directory (WP-35 preload delivery):
#   oci-layout:///tmp/kurin-pkg
#   file:///tmp/kurin-base.tar.zst
#   oci://127.0.0.1:5000/kurin/base:v1.33.1-kubeadm   (local zot)
set -euo pipefail

BUNDLE=${1:-/tmp/kurin-bundle}
SOURCE=${2:-}
NAME=kurin-tier1
CFG=e2e/tier1/config.toml
OFFCFG=e2e/tier1/config-offline.toml

if [ -z "$SOURCE" ]; then
    [ -d "$BUNDLE" ] || { echo "bundle $BUNDLE missing (kurin artifact --out)" >&2; exit 1; }
fi

# fresh VM via the normal tier1 setup
e2e/tier1/up.sh

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')

# Block egress to the registries/download hosts kurin normally uses.
multipass exec "$NAME" -- sudo bash -c '
  for h in registry.k8s.io dl.k8s.io github.com raw.githubusercontent.com quay.io auth.docker.io registry-1.docker.io ghcr.io; do
    for a in $(getent ahostsv4 "$h" | awk "{print \$1}" | sort -u); do
      iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
    done
  done
  echo "[offline] egress to artifact hosts blocked"'

if [ -n "$SOURCE" ]; then
    # Package sources are a v1alpha2 feature; the rest of the tier1 config
    # is version-neutral.
    sed 's|^version = "v1alpha1"$|version = "v1alpha2"|' "$CFG" > "$OFFCFG"
    cat >> "$OFFCFG" <<EOF

[packages]
base = "$SOURCE"
delivery = "preload"
EOF
    echo "[offline] config: $OFFCFG (package: $SOURCE)"
else
    sed "s|^name = \"tier1\"|name = \"tier1\"|" "$CFG" > "$OFFCFG"
    cat >> "$OFFCFG" <<EOF

[artifacts]
offline-dir = "$BUNDLE"
EOF
    echo "[offline] config: $OFFCFG (offline-dir: $BUNDLE)"
fi

BIN=target/tier1off/debug/kurin
CARGO_TARGET_DIR=target/tier1off cargo build -p kurin

if ! "$BIN" create cluster -f "$OFFCFG" --json > /tmp/tier1-offline-events.jsonl 2>/tmp/tier1-offline.err; then
    echo "[FAIL] offline install failed"; tail -5 /tmp/tier1-offline.err; exit 1
fi
grep -q '"cluster-ready"' /tmp/tier1-offline-events.jsonl || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' /tmp/tier1-offline-events.jsonl || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed /tmp/tier1-offline-events.jsonl; exit 1; }
echo "[ok] offline install: cluster-ready, zero failures"

for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf get node node01 -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node01 Ready=$READY"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

for i in $(seq 1 30); do
    BAD=$(multipass exec "$NAME" -- sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf get pods -A --no-headers 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

if ! "$BIN" create cluster -f "$OFFCFG" --json > /tmp/tier1-offline-2.jsonl 2>/dev/null; then
    echo "[FAIL] second offline run failed"; exit 1
fi
done2=$(grep '"task-done"' /tmp/tier1-offline-2.jsonl | grep -vE '"id":"(bootstrap/kubeadm-init|bootstrap/k3s-init|post/state-record)@' | wc -l || true)
[ "$done2" -eq 0 ] || { echo "[FAIL] second run re-applied $done2 tasks"; exit 1; }
echo "[ok] second run fully skipped"

echo "tier1-offline: PASS"
