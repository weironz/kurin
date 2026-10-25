#!/usr/bin/env bash
# tier-1 assertions: real install must reach cluster-ready with no task
# failures, node Ready, and a second run must be fully idempotent.
set -euo pipefail

NAME=kurin-tier1
BIN=${1:-target/tier1/debug/kurin}
CFG=${2:-e2e/tier1/config.toml}
EVENTS1=$(mktemp) EVENTS2=$(mktemp)
trap 'rm -f "$EVENTS1" "$EVENTS2"' EXIT

echo "[tier1] pass 1: create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS1" 2> >(tee /tmp/kurin-tier1-err1.log >&2); then
    echo "[FAIL] pass 1 exited nonzero; last events:"
    tail -5 "$EVENTS1" || true
    exit 1
fi

fails=$(grep -c '"task-failed"' "$EVENTS1" || true)
skips=$(grep -c '"task-skipped"' "$EVENTS1" || true)
if [ "$fails" -ne 0 ]; then
    echo "[FAIL] $fails task failures in pass 1:"
    grep '"task-failed"' "$EVENTS1"
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS1" || { echo "[FAIL] no cluster-ready event"; exit 1; }
echo "[ok] pass 1: cluster-ready, no failures ($skips pre-converged skips)"

echo "[tier1] checking node state (CNI may still be starting)"
READY=""
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf get node node01 -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node01 Ready=$READY (after 300s)"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

NODE_JSON=$(multipass exec "$NAME" -- sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf get node node01 -o json)

# node.spec.podCIDR is a /24 slice of the cluster podSubnet (10.233.64.0/18).
POD_CIDR=$(echo "$NODE_JSON" | python3 -c 'import json,sys; n=json.load(sys.stdin); print(n["spec"].get("podCIDR",""))')
case "$POD_CIDR" in
  10.233.*.0/24) : ;;
  *) echo "[FAIL] podCIDR=$POD_CIDR (want a 10.233.x.0/24 slice of 10.233.64.0/18)"; exit 1 ;;
esac
echo "[ok] podCIDR $POD_CIDR (slice of cluster 10.233.64.0/18)"

echo "[tier1] waiting for calico pods"
for i in $(seq 1 40); do
    BAD=$(multipass exec "$NAME" -- sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf get pods -A --no-headers 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    if [ -z "$BAD" ]; then
        echo "[ok] all pods healthy after ~$((i*10))s"
        break
    fi
    [ "$i" = "40" ] && { echo "[FAIL] pods not healthy after 400s:"; echo "$BAD"; exit 1; }
    sleep 10
done

echo "[tier1] pass 2: idempotency"
"$BIN" create cluster -f "$CFG" --json > "$EVENTS2" 2>/dev/null || { echo "[FAIL] pass 2 exited nonzero"; exit 1; }
done2=$(grep -c '"task-done"' "$EVENTS2" || true)
fail2=$(grep -c '"task-failed"' "$EVENTS2" || true)
skip2=$(grep -c '"task-skipped"' "$EVENTS2" || true)
[ "$done2" -eq 0 ] || { echo "[FAIL] pass 2 re-ran $done2 tasks"; exit 1; }
[ "$fail2" -eq 0 ] || { echo "[FAIL] pass 2 had failures"; exit 1; }
echo "[ok] pass 2: $skip2 tasks skipped, 0 re-applied"

echo "tier1: PASS"
