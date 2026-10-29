#!/usr/bin/env bash
# tier-3 assertions: 3 CP nodes Ready, kube-vip leases the VIP, VIP-backed
# API serves, all CP members joined, second run fully skipped.
set -euo pipefail

BIN=${1:-target/tier3/debug/kurin}
CFG=${2:-e2e/tier3/config.toml}
M=kurin-tier3-master01
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
VIP=$(awk -F'"' '/^vip = /{print $2}' "$CFG")
EVENTS1=$(mktemp) EVENTS2=$(mktemp)
trap 'rm -f "$EVENTS1" "$EVENTS2"' EXIT

echo "[tier3] pass 1: create HA cluster (vip=$VIP)"
"$BIN" create cluster -f "$CFG" --json > "$EVENTS1" 2>/dev/null \
    || { echo "[FAIL] pass 1 nonzero"; grep task-failed "$EVENTS1"; exit 1; }
grep -q '"cluster-ready"' "$EVENTS1" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS1" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS1"; exit 1; }
cp_joins=$(grep -c '"id":"join/kubeadm@master0[23]"' "$EVENTS1" || true)
[ "$cp_joins" -ge 2 ] || { echo "[FAIL] control-plane joins missing ($cp_joins)"; exit 1; }
echo "[ok] pass 1: cluster-ready, 2 CP joins"

echo "[tier3] waiting for 3 control-plane nodes Ready"
for i in $(seq 1 40); do
    READY=$(multipass exec "$M" -- sh -c "$KC get nodes -o jsonpath='{range .items[*]}{.metadata.name}={.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    count_ready=$(echo "$READY" | tr ' ' '\n' | grep -c '=True' || true)
    [ "$count_ready" -eq 3 ] && break
    sleep 10
done
[ "${count_ready:-0}" -eq 3 ] || { echo "[FAIL] nodes not all Ready: $READY"; exit 1; }
echo "[ok] 3 nodes Ready (~$((i*10))s): $READY"

echo "[tier3] checking API via VIP $VIP"
for i in $(seq 1 30); do
    if multipass exec "$M" -- sh -c "$KC --server https://$VIP:6443 get --raw /readyz" >/dev/null 2>&1; then
        echo "[ok] VIP-backed API serves /readyz (~$((i*3))s)"
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] VIP $VIP:6443 not serving"; exit 1; }
    sleep 3
done

echo "[tier3] pod health"
for i in $(seq 1 30); do
    BAD=$(multipass exec "$M" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "30" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

echo "[tier3] pass 2: idempotency"
"$BIN" create cluster -f "$CFG" --json > "$EVENTS2" 2>/dev/null || { echo "[FAIL] pass 2 nonzero"; exit 1; }
# A fresh run re-harvests join material (kubeadm-init) and records the new
# operation id (state-record); both are refresh tasks, not re-applied work.
done2=$(grep '"task-done"' "$EVENTS2" | grep -vE '"id":"(bootstrap/kubeadm-init|post/state-record)@' | wc -l || true)
fail2=$(grep -c '"task-failed"' "$EVENTS2" || true)
[ "$done2" -eq 0 ] && [ "$fail2" -eq 0 ] || { echo "[FAIL] pass 2 re-ran $done2, failed $fail2"; exit 1; }
echo "[ok] pass 2: all skipped"

echo "tier3: PASS"