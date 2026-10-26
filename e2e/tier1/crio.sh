#!/usr/bin/env bash
# tier-1 (CRI-O): single-node kubeadm cluster on the CRI-O runtime
# (kubernetes.runtime = "crio"). Asserts cluster-ready with no task
# failures, crio as the active runtime, containerd's unit absent, node01
# Ready and all pods healthy. Reuses the tier1 VM + base config.
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
NAME=kurin-tier1
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp) TMPCFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$TMPCFG"' EXIT

echo "[tier1-crio] booting VM via tier1 up.sh"
e2e/tier1/up.sh

# Temp config copy: switch the kubeadm track to CRI-O.
awk '/^\[kubernetes\]$/ && !ins { print; print "runtime = \"crio\""; ins=1; next } { print }' \
    "$CFG" > "$TMPCFG"
grep -q '^runtime = "crio"$' "$TMPCFG" || { echo "[FAIL] runtime injection failed"; exit 1; }
CLUSTER=$(awk -F'"' '/^name = /{print $2}' "$TMPCFG")
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"
echo "[tier1-crio] runtime=crio, cluster=$CLUSTER"

echo "[tier1-crio] create cluster"
if ! "$BIN" create cluster -f "$TMPCFG" --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-crio-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

echo "[tier1-crio] runtime checks: crio active, containerd absent"
multipass exec "$NAME" -- sudo systemctl is-active crio || { echo "[FAIL] crio not active"; exit 1; }
multipass exec "$NAME" -- sudo /usr/local/bin/crio -v
if multipass exec "$NAME" -- sudo systemctl is-active containerd >/dev/null 2>&1; then
    echo "[FAIL] containerd is active in the crio lane"; exit 1
fi
CRTSOCK=$(multipass exec "$NAME" -- sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf \
    get node node01 -o jsonpath='{.status.nodeInfo.containerRuntimeVersion}')
case "$CRTSOCK" in
    cri-o://*) echo "[ok] node runtime $CRTSOCK" ;;
    *) echo "[FAIL] runtime is '${CRTSOCK:-none}', not cri-o"; exit 1 ;;
esac

echo "[tier1-crio] waiting for node01 Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node01 Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

echo "[tier1-crio] pod health (all pods Running/Succeeded)"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$NAME" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}' || true)
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods not healthy after 300s:"; echo "$BAD"; exit 1; }
    sleep 10
done

echo "tier1-crio: PASS"