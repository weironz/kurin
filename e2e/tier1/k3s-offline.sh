#!/usr/bin/env bash
# tier-1 (k3s offline): single-node k3s install from the prebuilt bundle with
# the VM's egress blocked — proves the k3s airgap path is self-contained (no
# registry, no github: k3s binary from the bundle, images from the
# k3s-airgap tar in /var/lib/rancher/k3s/agent/images/). Mirrors
# e2e/tier1/offline.sh (kubeadm) with the k3s distribution overrides of
# e2e/tier1/k3s.sh. Usage: e2e/tier1/k3s-offline.sh [bundle-dir]
set -euo pipefail

BUNDLE=${1:-/tmp/kurin-bundle-k3s}
NAME=kurin-tier1
CFG=e2e/tier1/config.toml
K3KC="sudo k3s kubectl"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

[ -d "$BUNDLE" ] || { echo "bundle $BUNDLE missing (kurin artifact --out $BUNDLE)" >&2; exit 1; }

# fresh VM via the normal tier1 setup
e2e/tier1/up.sh

# Block egress to the registries/download hosts kurin normally uses.
multipass exec "$NAME" -- sudo bash -c '
  for h in registry.k8s.io dl.k8s.io github.com raw.githubusercontent.com quay.io auth.docker.io registry-1.docker.io ghcr.io; do
    for a in $(getent ahostsv4 "$h" | awk "{print \$1}" | sort -u); do
      iptables -I OUTPUT -d "$a" -j REJECT 2>/dev/null || true
    done
  done
  echo "[offline] egress to artifact hosts blocked"'

# Same config surgery as k3s.sh: inject distribution = "k3s" under
# [kubernetes], replace the CNI with flannel, then append the offline dir.
awk '
    /^\[kubernetes\]$/ && !ins { print; print "distribution = \"k3s\""; ins=1; next }
    /^\[network\.cni\]$/ { incni=1; next }
    incni && /^\[/         { incni=0 }
    incni                  { next }
    { print }
    END {
        print ""
        print "[network.cni]"
        print "kind = \"flannel\""
    }
' "$CFG" > "$TMPCFG"
cat >> "$TMPCFG" <<EOF

[artifacts]
offline-dir = "$BUNDLE"
EOF
grep -q '^distribution = "k3s"$' "$TMPCFG" || { echo "[FAIL] distribution injection failed"; exit 1; }
grep -q '^offline-dir = ' "$TMPCFG" || { echo "[FAIL] offline-dir injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"
echo "[tier1-k3s-offline] config ready (offline-dir: $BUNDLE, cluster=$CLUSTER)"

BIN=target/tier1k3soff/debug/kurin
CARGO_TARGET_DIR=target/tier1k3soff cargo build -p kurin

echo "[tier1-k3s-offline] create cluster (distribution=k3s, offline)"
if ! "$BIN" create cluster -f "$TMPCFG" --json > /tmp/tier1-k3s-offline-events.jsonl 2>/tmp/tier1-k3s-offline.err; then
    echo "[FAIL] offline k3s install failed"; tail -5 /tmp/tier1-k3s-offline.err; exit 1
fi
grep -q '"cluster-ready"' /tmp/tier1-k3s-offline-events.jsonl || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' /tmp/tier1-k3s-offline-events.jsonl || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed /tmp/tier1-k3s-offline-events.jsonl; exit 1; }
echo "[ok] offline k3s install: cluster-ready, zero failures"

echo "[tier1-k3s-offline] airgap tar landed on the node"
AIRGAP=$(multipass exec "$NAME" -- sudo ls /var/lib/rancher/k3s/agent/images/ 2>/dev/null | head -1 || true)
[ -n "$AIRGAP" ] || { echo "[FAIL] no airgap tar in /var/lib/rancher/k3s/agent/images/"; exit 1; }
echo "[ok] airgap tar: $AIRGAP"

echo "[tier1-k3s-offline] waiting for node01 Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$K3KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node01 Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

echo "[tier1-k3s-offline] pod health (all pods Running/Succeeded)"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$NAME" -- sh -c "$K3KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2 " "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods not healthy after 300s:"; echo "$BAD"; exit 1; }
    sleep 10
done

# Idempotency: a second offline run must converge with zero re-applied tasks.
if ! "$BIN" create cluster -f "$TMPCFG" --json > /tmp/tier1-k3s-offline-2.jsonl 2>/dev/null; then
    echo "[FAIL] second offline run failed"; exit 1
fi
done2=$(grep '"task-done"' /tmp/tier1-k3s-offline-2.jsonl | grep -vE '"id":"(bootstrap/kubeadm-init|bootstrap/k3s-init|post/state-record)@' | wc -l || true)
[ "$done2" -eq 0 ] || { echo "[FAIL] second run re-applied $done2 tasks"; exit 1; }
echo "[ok] second run fully skipped"

echo "tier1-k3s-offline: PASS"
