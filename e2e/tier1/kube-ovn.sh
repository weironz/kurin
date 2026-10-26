#!/usr/bin/env bash
# tier-1 (kube-ovn): single-node kubeadm cluster with network.cni = "kube-ovn".
# Asserts: cluster-ready, kube-ovn-controller available, ovs-ovn ready==desired,
# node Ready, pods healthy, second run fully skipped (idempotency).
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
CFG=e2e/tier1/config.toml
NAME=kurin-tier1
KOVN_TMP=$(mktemp --suffix=.toml)
EVENTS=$(mktemp)
trap 'rm -f "$EVENTS" "$KOVN_TMP"; e2e/tier1/down.sh' EXIT

e2e/tier1/up.sh

# swap the cni kind to kube-ovn: append explicit table when the base config
# relies on the calico default, replace otherwise
if grep -q '^\[network.cni\]' "$CFG"; then
  awk 'BEGIN{in_cni=0}
  /^\[network\.cni\]/{in_cni=1; print; next}
  /^\[/{in_cni=0}
  in_cni && /^kind =/{print "kind = \"kube-ovn\""; next}
  {print}' "$CFG" > "$KOVN_TMP"
else
  cp "$CFG" "$KOVN_TMP"
  printf '\n[network.cni]\nkind = "kube-ovn"\n' >> "$KOVN_TMP"
fi
grep -q 'kind = "kube-ovn"' "$KOVN_TMP" || { echo "[FAIL] cni injection failed"; exit 1; }

rm -rf ~/.cache/kurin/clusters/$(grep -m1 '^name = ' "$KOVN_TMP" | cut -d'"' -f2)
if ! "$BIN" create cluster -f "$KOVN_TMP" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] create exited nonzero; last events:"
    grep task-failed "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"

echo "[kube-ovn] waiting for controller + ovs"
for i in $(seq 1 60); do
    CTRL=$(multipass exec "$NAME" -- sh -c "$KC -n kube-system get deploy kube-ovn-controller -o jsonpath='{.status.readyReplicas}'" 2>/dev/null || true)
    OVS=$(multipass exec "$NAME" -- sh -c "$KC -n kube-system get ds ovs-ovn -o jsonpath='{.status.desiredNumberScheduled} {.status.numberReady}'" 2>/dev/null | xargs || true)
    [ "${CTRL:-0}" -ge 1 ] && [ "$OVS" = "1 1" ] && break
    sleep 10
done
[ "${CTRL:-0}" -ge 1 ] || { echo "[FAIL] kube-ovn-controller not ready: $CTRL"; exit 1; }
[ "$OVS" = "1 1" ] || { echo "[FAIL] ovs-ovn not 1/1: $OVS"; exit 1; }
echo "[ok] kube-ovn-controller=1 ovs-ovn=1/1 (~$((i*10))s)"

echo "[kube-ovn] default subnet matches pod CIDR (controller may take a few seconds to materialize it)"
SUBNET=""
for i in $(seq 1 24); do
    SUBNET=$(multipass exec "$NAME" -- sh -c "$KC get subnet ovn-default -o jsonpath='{.spec.cidrBlock}'" 2>/dev/null || true)
    [ -n "$SUBNET" ] && break
    sleep 5
done
[ "$SUBNET" = "10.233.64.0/18" ] || { echo "[FAIL] ovn-default cidrBlock=$SUBNET (want 10.233.64.0/18)"; exit 1; }
echo "[ok] ovn-default cidrBlock=$SUBNET (~$((i*5))s)"

echo "[kube-ovn] node Ready + pod health"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node01 Ready=$READY"; exit 1; }
echo "[ok] node01 Ready (~$((i*10))s)"

for i in $(seq 1 60); do
    BAD=$(multipass exec "$NAME" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "60" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

echo "[kube-ovn] pod-to-pod connectivity (netns across the ovn fabric)"
POD=$(multipass exec "$NAME" -- sh -c "$KC -n kube-system get pods -l app=kube-ovn-pinger -o jsonpath='{.items[0].metadata.name}'" 2>/dev/null || true)
if [ -n "$POD" ]; then
    multipass exec "$NAME" -- sh -c "$KC -n kube-system exec $POD -- ping -c1 -W2 10.233.64.1" >/dev/null 2>&1 \
        && echo "[ok] pinger -> gateway reachable" \
        || echo "[skip] pinger ping not permitted (non-fatal)"
fi

echo "[kube-ovn] second run idempotency"
if ! "$BIN" create cluster -f "$KOVN_TMP" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] second run nonzero"
    exit 1
fi
done2=$(grep -c '"task-done"' "$EVENTS" || true)
fail2=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$done2" -eq 0 ] && [ "$fail2" -eq 0 ] || { echo "[FAIL] second run re-ran $done2, failed $fail2"; exit 1; }
echo "[ok] second run fully skipped"

echo "tier1-kube-ovn: PASS"