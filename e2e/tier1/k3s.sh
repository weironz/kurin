#!/usr/bin/env bash
# tier-1 (k3s): single-node k3s cluster. Uses the tier1 VM but installs the
# k3s distribution (distribution = "k3s", flannel CNI). k3s has no separate
# kubectl: assertions go through `k3s kubectl` on the node.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
K3KC="sudo k3s kubectl"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"' EXIT

echo "[tier1-k3s] booting VM via tier1 up.sh"
e2e/tier1/up.sh

# Temp config copy: k3s distribution + flannel CNI. Insert
# distribution = "k3s" under the existing [kubernetes] table and append a
# fresh [network.cni] table.
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
# The kubeadm default (1.37.2) and the k3s release (1.37.1+k3s1) can differ
# by a patch; the k3s track installs the embedded k3s release.
sed -i 's/^version = "1.37.2"/version = "1.37.1"/' "$TMPCFG"
grep -q '^kind = "flannel"$' "$TMPCFG" || { echo "[FAIL] cni injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"
echo "[tier1-k3s] distribution=k3s, cni=flannel, cluster=$CLUSTER"

echo "[tier1-k3s] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-k3s-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

echo "[tier1-k3s] waiting for node01 Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$K3KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node01 Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

echo "[tier1-k3s] pod health (all pods Running/Succeeded)"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$NAME" -- sh -c "$K3KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods not healthy after 300s:"; echo "$BAD"; exit 1; }
    sleep 10
done

echo "tier1-k3s: PASS"
