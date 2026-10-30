#!/usr/bin/env bash
# tier-2 e2e: add a second control-plane node (master2) to a running 1CP+1W
# cluster. Asserts: join/kubeadm@master2 event, master2 Ready with the
# control-plane role, 2 Ready etcd members. Cleans up after itself: removes
# the node from the cluster, purges the VM, rolls the config back.
# Prereq: e2e/tier2/up.sh + e2e/tier2/assert.sh have run (cluster installed).
set -euo pipefail

BIN=${1:-target/tier2/debug/kurin}
CFG=e2e/tier2/config.toml
PREFIX=kurin-tier2
M=${PREFIX}-master
NODE=master2
KEYS=e2e/tier2/.keys
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
EVENTS=$(mktemp)


echo "[add-cp] waiting for existing nodes Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$M" -- sh -c "$KC get node master01 worker01 -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{\" \"}{end}'" 2>/dev/null | xargs || true)
    [ "$READY" = "True True" ] && break
    sleep 10
done
[ "$READY" = "True True" ] || { echo "[FAIL] base cluster not Ready: $READY"; exit 1; }
echo "[ok] base cluster healthy"

cleanup() {
    echo "[add-cp] cleanup: remove node, purge VM, roll back config"
    local deleted=0
    if grep -q "^\[nodes\.$NODE\]" "$CFG"; then
        # delete node removes the etcd member before the node object, so a
        # 2-member etcd keeps quorum. If it fails, keep the VM and config
        # for diagnosis instead of purging the only remaining member.
        if "$BIN" delete node "$NODE" -f "$CFG" >/tmp/add-cp-delete.log 2>&1; then
            deleted=1
        else
            echo "[add-cp] delete node failed; keeping $PREFIX-$NODE and $CFG"
            tail -20 /tmp/add-cp-delete.log || true
        fi
    else
        deleted=1
    fi
    if [ "$deleted" = 1 ]; then
        multipass delete --purge "$PREFIX-$NODE" >/dev/null 2>&1 || true
        # config.toml is generated and untracked; regenerate it from the
        # live VMs instead of relying on git checkout.
        e2e/tier2/up.sh >/dev/null 2>&1 || true
    fi
    rm -f "$EVENTS"
}
trap cleanup EXIT

if ! multipass info "$PREFIX-$NODE" >/dev/null 2>&1; then
    echo "[add-cp] launching $PREFIX-$NODE (24.04, 2 cpu / 1800M)"
    multipass launch --name "$PREFIX-$NODE" --cpus 2 --memory 1800M --disk 15G 24.04
fi
IP=$(multipass info "$PREFIX-$NODE" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4 found for $PREFIX-$NODE" >&2; exit 1; }

PUB=$(cat "$KEYS/id_ed25519.pub")
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
multipass exec "$PREFIX-$NODE" -- sudo mkdir -p /root/.ssh
multipass exec "$PREFIX-$NODE" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
multipass exec "$PREFIX-$NODE" -- sudo bash -c "grep -q '^PermitRootLogin' /etc/ssh/sshd_config && sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config; systemctl restart ssh"
echo "[add-cp] $PREFIX-$NODE ready at $IP"

cat >> "$CFG" <<EOF

[nodes.$NODE]
address = "$IP"
internal-address = "$IP"
role = "control-plane"
EOF
echo "[add-cp] appended [nodes.$NODE] to $CFG"

echo "[add-cp] kurin add nodes"
if ! "$BIN" add nodes -f "$CFG" --json > "$EVENTS" 2>/dev/null; then
    echo "[FAIL] add nodes nonzero"
    grep task-failed "$EVENTS" || true
    exit 1
fi
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; exit 1; }
joined=$(grep -c "\"id\":\"join/kubeadm@$NODE\"" "$EVENTS" || true)
[ "$joined" -ge 1 ] || { echo "[FAIL] join/kubeadm@$NODE task missing"; exit 1; }
echo "[ok] join/kubeadm@$NODE done, 0 task failures"

echo "[add-cp] waiting for $NODE Ready with control-plane role"
for i in $(seq 1 30); do
    READY=$(multipass exec "$M" -- sh -c "$KC get node $NODE -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    # The label value is the empty string, so jsonpath prints nothing even
    # when the label exists; grep --show-labels instead.
    ROLE=$(multipass exec "$M" -- sh -c "$KC get node $NODE --show-labels --no-headers" 2>/dev/null | grep -o 'node-role.kubernetes.io/control-plane' || true)
    [ "$READY" = "True" ] && [ -n "$ROLE" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] $NODE not Ready: $READY"; exit 1; }
[ -n "$ROLE" ] || { echo "[FAIL] $NODE missing control-plane role"; exit 1; }
echo "[ok] $NODE Ready, role=control-plane (~$((i*10))s)"

echo "[add-cp] waiting for 2 Ready etcd members"
for i in $(seq 1 30); do
    # POSIX awk has no backreferences; compare the ready pair directly.
    ETCD=$(multipass exec "$M" -- sh -c "$KC -n kube-system get pods -l component=etcd --no-headers" 2>/dev/null | awk '$2 == "1/1" {n++} END {print n+0}')
    [ "$ETCD" -eq 2 ] && break
    sleep 10
done
[ "$ETCD" -eq 2 ] || { echo "[FAIL] Ready etcd members: $ETCD (expected 2)"; exit 1; }
echo "[ok] etcd: 2/2 Ready (~$((i*10))s)"

echo "tier2 add-cp: PASS"
