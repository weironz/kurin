#!/usr/bin/env bash
# tier-1 (cilium): single-node kubeadm cluster with the cilium CNI,
# installed via the cilium-cli that ArtifactsInstall drops into
# /usr/local/bin. Config override happens on a TEMP copy; the checked-out
# config.toml is never modified.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"; e2e/tier1/down.sh' EXIT

echo "[tier1-cilium] booting VM via tier1 up.sh"
e2e/tier1/up.sh

# Rewrite the generated config into a temp copy with kind = "cilium".
awk '
    /^\[network\.cni\]$/ { incni=1; next }
    incni && /^\[/         { incni=0 }
    incni                  { next }
    { print }
    END {
        print ""
        print "[network.cni]"
        print "kind = \"cilium\""
    }
' "$CFG" > "$TMPCFG"
grep -q '^kind = "cilium"$' "$TMPCFG" || { echo "[FAIL] cni injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"
echo "[tier1-cilium] cni=cilium, cluster=$CLUSTER"

echo "[tier1-cilium] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-cilium-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

echo "[tier1-cilium] waiting for cilium ds ready==desired (<=5min)"
for i in $(seq 1 30); do
    DS=$(multipass exec "$NAME" -- sh -c "$KC -n kube-system get ds cilium -o jsonpath='{.status.numberReady}/{.status.desiredNumberScheduled}'" 2>/dev/null || true)
    [ "$DS" = "1/1" ] && break
    sleep 10
done
[ "${DS:-}" = "1/1" ] || { echo "[FAIL] cilium ds ready/desired=${DS:-none} (after 300s)"; exit 1; }
echo "[ok] cilium ds 1/1 (~$((i*10))s)"

echo "[tier1-cilium] waiting for node01 Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node01 Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

echo "tier1-cilium: PASS"
