#!/usr/bin/env bash
# tier-2 assertions: two nodes, both Ready, worker joined with a pod CIDR,
# second run fully skipped.
set -euo pipefail

BIN=${1:-target/tier2/debug/kurin}
CFG=${2:-e2e/tier2/config.toml}
M=kurin-tier2-master
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS1=$(mktemp) EVENTS2=$(mktemp)
trap 'rm -f "$EVENTS1" "$EVENTS2"' EXIT

echo "[tier2] pass 1: create cluster"
"$BIN" create cluster -f "$CFG" --json > "$EVENTS1" 2>/dev/null \
    || { echo "[FAIL] pass 1 nonzero"; grep task-failed "$EVENTS1"; exit 1; }
grep -q '"cluster-ready"' "$EVENTS1" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS1" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS1"; exit 1; }
joined=$(grep -c '"id":"join/kubeadm@worker01"' "$EVENTS1" || true)
[ "$joined" -ge 1 ] || { echo "[FAIL] worker join task missing"; exit 1; }
echo "[ok] pass 1: cluster-ready, worker joined"

echo "[tier2] waiting for nodes Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$M" -- sh -c "$KC get node master01 worker01 -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$READY" = "True True" ] && break
    sleep 10
done
[ "$READY" = "True True" ] || { echo "[FAIL] nodes not both Ready: $READY"; exit 1; }
echo "[ok] master01 + worker01 Ready (~$((i*10))s)"

WPOD=$(multipass exec "$M" -- sh -c "$KC get node worker01 -o jsonpath='{.spec.podCIDR}'" 2>/dev/null || true)
case "$WPOD" in
  10.233.*.0/24) : ;;
  *) echo "[FAIL] worker podCIDR=$WPOD"; exit 1 ;;
esac
echo "[ok] worker podCIDR $WPOD"

echo "[tier2] pod health"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$M" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

echo "[tier2] pass 2: idempotency"
"$BIN" create cluster -f "$CFG" --json > "$EVENTS2" 2>/dev/null || { echo "[FAIL] pass 2 nonzero"; exit 1; }
done2=$(grep -c '"task-done"' "$EVENTS2" || true)
fail2=$(grep -c '"task-failed"' "$EVENTS2" || true)
[ "$done2" -eq 0 ] && [ "$fail2" -eq 0 ] || { echo "[FAIL] pass 2 re-ran $done2, failed $fail2"; exit 1; }
echo "[ok] pass 2: all skipped, nothing re-applied"

echo "tier2: PASS"
