#!/usr/bin/env bash
# tier-1 (flannel): single-node kubeadm cluster with the flannel CNI.
# Boots the VM, overrides [network.cni] to flannel in a TEMP copy of the
# config (the checked-out config.toml is never modified), runs
# `kurin create cluster`, asserts convergence, then tears down.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

echo "[tier1-flannel] booting VM via tier1 up.sh"
e2e/tier1/up.sh

# Rewrite the generated config into a temp copy with kind = "flannel":
# strip any existing [network.cni] section, append a fresh one.
awk '
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
grep -q '^kind = "flannel"$' "$TMPCFG" || { echo "[FAIL] cni injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"
echo "[tier1-flannel] cni=flannel, cluster=$CLUSTER"

echo "[tier1-flannel] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-flannel-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

echo "[tier1-flannel] waiting for kube-flannel-ds ready==desired"
for i in $(seq 1 30); do
    DS=$(multipass exec "$NAME" -- sh -c "$KC -n kube-flannel get ds -o jsonpath='{range .items[*]}{.status.numberReady}/{.status.desiredNumberScheduled}{end}'" 2>/dev/null || true)
    [ "$DS" = "1/1" ] && break
    sleep 10
done
[ "${DS:-}" = "1/1" ] || { echo "[FAIL] flannel ds ready/desired=${DS:-none} (after 300s)"; exit 1; }
echo "[ok] kube-flannel-ds 1/1 (~$((i*10))s)"

echo "[tier1-flannel] waiting for node01 Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node01 Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

echo "tier1-flannel: PASS"
