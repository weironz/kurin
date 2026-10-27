#!/usr/bin/env bash
# tier-2 e2e (WP-27): add a worker with the quick parameter
# `kurin add nodes -f cfg --worker IP`, which appends the node to the config
# (toml_edit, comments preserved, role-index name continued) and then joins
# it. Asserts: the config gained [nodes.worker-1], join/kubeadm@worker-1
# fired, the node is Ready without the control-plane role. Cleans up after
# itself: removes the node from the cluster, purges the VM, rolls the config
# back.
# Prereq: e2e/tier2/up.sh + e2e/tier2/assert.sh have run (cluster installed).
set -euo pipefail

BIN=${1:-target/tier2/debug/kurin}
CFG=e2e/tier2/config.toml
PREFIX=kurin-tier2
M=${PREFIX}-master
NODE=worker-1
KEYS=e2e/tier2/.keys
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp)

echo "[add-worker] waiting for existing nodes Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$M" -- sh -c "$KC get node master01 worker01 -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$READY" = "True True" ] && break
    sleep 10
done
[ "$READY" = "True True" ] || { echo "[FAIL] base cluster not Ready: $READY"; exit 1; }
echo "[ok] base cluster healthy"

cleanup() {
    echo "[add-worker] cleanup: remove node, purge VM, roll back config"
    if grep -q "^\[nodes\.$NODE\]" "$CFG"; then
        "$BIN" delete node "$NODE" -f "$CFG" >/dev/null 2>&1 || true
    fi
    multipass delete --purge "$PREFIX-worker2" >/dev/null 2>&1 || true
    git checkout -- "$CFG" 2>/dev/null || true
    rm -f "$EVENTS"
}
trap cleanup EXIT

if ! multipass info "$PREFIX-worker2" >/dev/null 2>&1; then
    echo "[add-worker] launching $PREFIX-worker2 (24.04, 2 cpu / 1800M)"
    multipass launch --name "$PREFIX-worker2" --cpus 2 --memory 1800M --disk 15G 24.04
fi
IP=$(multipass info "$PREFIX-worker2" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4 found for $PREFIX-worker2" >&2; exit 1; }

PUB=$(cat "$KEYS/id_ed25519.pub")
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
multipass exec "$PREFIX-worker2" -- sudo mkdir -p /root/.ssh
multipass exec "$PREFIX-worker2" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
multipass exec "$PREFIX-worker2" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
echo "[add-worker] $PREFIX-worker2 ready at $IP"

echo "[add-worker] kurin add nodes --worker $IP"
if ! "$BIN" add nodes -f "$CFG" --worker "$IP" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] add nodes nonzero"
    grep task-failed "$EVENTS" || true
    exit 1
fi
# WP-27: the config file is the desired state and must carry the new node.
grep -q "^\[nodes\.$NODE\]" "$CFG" || { echo "[FAIL] $CFG missing [nodes.$NODE]"; exit 1; }
grep -A2 "^\[nodes\.$NODE\]" "$CFG" | grep -q "address = \"$IP\"" \
    || { echo "[FAIL] [nodes.$NODE] address is not $IP"; exit 1; }
echo "[ok] config gained [nodes.$NODE] address=$IP"

fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
joined=$(grep -c "\"id\":\"join/kubeadm@$NODE\"" "$EVENTS" || true)
[ "$joined" -ge 1 ] || { echo "[FAIL] join/kubeadm@$NODE task missing"; exit 1; }
echo "[ok] join/kubeadm@$NODE done, 0 task failures"

echo "[add-worker] waiting for $NODE Ready as a plain worker"
for i in $(seq 1 30); do
    READY=$(multipass exec "$M" -- sh -c "$KC get node $NODE -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    ROLE=$(multipass exec "$M" -- sh -c "$KC get node $NODE -o jsonpath='{.metadata.labels.node-role\.kubernetes\.io/control-plane}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && [ -z "$ROLE" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] $NODE not Ready: $READY"; exit 1; }
[ -z "$ROLE" ] || { echo "[FAIL] $NODE unexpectedly has the control-plane role"; exit 1; }
echo "[ok] $NODE Ready, role=worker (~$((i*10))s)"

echo "tier2 add-worker: PASS"