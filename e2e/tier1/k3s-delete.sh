#!/usr/bin/env bash
# tier-1 (k3s delete): single-node k3s cluster torn down by
# `kurin delete cluster`. Asserts the wipe is distribution-aware: k3s state
# (`/var/lib/rancher`, `/etc/rancher`, the k3s binary) is gone, the k3s
# service is inactive, no kubeadm leftovers were created, and a second delete
# is idempotent (no fatal error). Not part of the default tier-1 loop.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
K3KC="sudo k3s kubectl"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

echo "[tier1-k3s-delete] booting VM via tier1 up.sh"
e2e/tier1/up.sh

# Temp config copy: k3s distribution + flannel CNI (same shape as k3s.sh).
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
grep -q '^distribution = "k3s"$' "$TMPCFG" || { echo "[FAIL] distribution injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"
echo "[tier1-k3s-delete] distribution=k3s, cni=flannel, cluster=$CLUSTER"

echo "[tier1-k3s-delete] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-k3s-del-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
echo "[ok] cluster-ready"

echo "[tier1-k3s-delete] kurin delete cluster"
"$BIN" delete cluster -f "$TMPCFG" || {
    echo "[FAIL] delete cluster exited nonzero"
    exit 1
}
echo "[ok] delete cluster exited 0"

check_absent() {
    local what=$1 path=$2
    if multipass exec "$NAME" -- sudo test -e "$path"; then
        echo "[FAIL] $what still present: $path"
        exit 1
    fi
    echo "[ok] $what removed ($path)"
}

echo "[tier1-k3s-delete] asserting node is k3s-clean"
check_absent "rancher data"   /var/lib/rancher
check_absent "rancher config" /etc/rancher
check_absent "k3s binary"     /usr/local/bin/k3s
check_absent "kubeadm state"  /etc/kubernetes

ACTIVE=$(multipass exec "$NAME" -- sh -c 'systemctl is-active k3s 2>/dev/null || true')
[ "$ACTIVE" = "active" ] && { echo "[FAIL] k3s service still active"; exit 1; }
echo "[ok] k3s service not active (is-active: ${ACTIVE:-unknown})"

echo "[tier1-k3s-delete] second delete must be idempotent"
if ! "$BIN" delete cluster -f "$TMPCFG" 2> >(tee /tmp/kurin-tier1-k3s-del2.log >&2); then
    echo "[FAIL] second delete cluster exited nonzero"
    exit 1
fi
echo "[ok] second delete exited 0"

echo "tier1-k3s-delete: PASS"
