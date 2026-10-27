#!/usr/bin/env bash
# tier-3 (quick): 3 control-plane nodes + kube-vip, built from CLI arguments
# only with SSH password authentication. Asserts the summary/save path, the
# saved config carries the VIP and a secret reference (never the password),
# and all three nodes reach Ready.
#
# Usage: e2e/tier3/quick-ha.sh [BIN]
set -euo pipefail

BIN=${1:-target/tier3/debug/kurin}
PREFIX=kurin-tier3
CLUSTER=tier3-quick
PASS='kurin-e2e-fake-pass'
NODES="master01 master02 master03"
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
WORK=$(mktemp -d) EVENTS=$(mktemp)
trap 'rm -rf "$WORK" "$EVENTS"; e2e/tier3/down.sh' EXIT

echo "[tier3-quick] booting VMs via tier3 up.sh"
e2e/tier3/up.sh

ips=""
for n in $NODES; do
    name="$PREFIX-$n"
    ip=$(multipass info "$name" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
    [ -n "$ip" ] || { echo "[FAIL] no ipv4 for $name"; exit 1; }
    echo "[tier3-quick] enabling root password auth on $name ($ip)"
    multipass exec "$name" -- sudo bash -c "echo 'root:$PASS' | chpasswd"
    multipass exec "$name" -- sudo bash -c "
        sed -i 's/^PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
        sed -i 's/^PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
        grep -q '^PasswordAuthentication' /etc/ssh/sshd_config || \
            echo 'PasswordAuthentication yes' >> /etc/ssh/sshd_config
        systemctl restart ssh"
    ssh-keygen -R "$ip" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
    ips="$ips $ip"
done
set -- $ips
IP1=$1 IP2=$2 IP3=$3
VIP="$(echo "$IP1" | cut -d. -f1-3).99"
ssh-keygen -R "$VIP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"

export KURIN_SSH_PASSWORD="$PASS"
cd "$WORK"
echo "[tier3-quick] create HA cluster from arguments only (vip=$VIP)"
if ! "$BIN" create cluster \
    --name "$CLUSTER" \
    --control-plane "$IP1,$IP2,$IP3" \
    --vip "$VIP" \
    --ssh-user root \
    --ssh-password-env KURIN_SSH_PASSWORD \
    --kubernetes 1.33.1 \
    --yes --json > "$EVENTS" 2> >(tee /tmp/kurin-tier3-quick-err.log >&2); then
    echo "[FAIL] create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures:"; grep '"task-failed"' "$EVENTS"; exit 1; }
echo "[ok] cluster-ready, no task failures"

SAVED="$WORK/$CLUSTER.kurin.toml"
[ -f "$SAVED" ] || { echo "[FAIL] no saved config at $SAVED"; exit 1; }
grep -q "vip = \"$VIP\"" "$SAVED" || { echo "[FAIL] saved config lacks the VIP"; cat "$SAVED"; exit 1; }
grep -q 'password = { env = "KURIN_SSH_PASSWORD" }' "$SAVED" || {
    echo "[FAIL] saved config lacks the secret reference:"; cat "$SAVED"; exit 1; }
if grep -q "$PASS" "$SAVED"; then
    echo "[FAIL] saved config contains the plaintext password"; exit 1
fi
cp_count=$(grep -c 'role = "control-plane"' "$SAVED" || true)
[ "$cp_count" -eq 3 ] || { echo "[FAIL] expected 3 control-plane nodes, got $cp_count"; exit 1; }
echo "[ok] saved config: 3 control planes, VIP, secret reference, no plaintext"

echo "[tier3-quick] waiting for 3 nodes Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$PREFIX-master01" -- sh -c "$KC get nodes -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{\"\\n\"}{end}'" 2>/dev/null | grep -c '^True$' || true)
    [ "${READY:-0}" -eq 3 ] && break
    sleep 10
done
[ "${READY:-0}" -eq 3 ] || { echo "[FAIL] Ready nodes=${READY:-0}/3 (after 300s)"; exit 1; }
echo "[ok] 3/3 nodes Ready (~$((i*10))s)"

echo "tier3-quick-ha: PASS"