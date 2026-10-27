#!/usr/bin/env bash
# tier-1 (quick): build the single-node cluster from CLI arguments only,
# with SSH password authentication — no config file on the command line.
# Asserts the saved config holds a secret reference (never the password),
# the event stream reaches cluster-ready, and the node goes Ready.
#
# Usage: e2e/tier1/quick.sh [BIN]
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
NAME=kurin-tier1
CLUSTER=tier1-quick
PASS='kurin-e2e-fake-pass'
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
WORK=$(mktemp -d) EVENTS=$(mktemp)
trap 'rm -rf "$WORK" "$EVENTS"; e2e/tier1/down.sh' EXIT

echo "[tier1-quick] booting VM via tier1 up.sh"
e2e/tier1/up.sh

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4 found"; exit 1; }

echo "[tier1-quick] enabling root password auth on $IP"
multipass exec "$NAME" -- sudo bash -c "echo 'root:$PASS' | chpasswd"
multipass exec "$NAME" -- sudo bash -c "
    sed -i 's/^PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
    sed -i 's/^PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
    grep -q '^PasswordAuthentication' /etc/ssh/sshd_config || \
        echo 'PasswordAuthentication yes' >> /etc/ssh/sshd_config
    systemctl restart ssh"
# The VM may have been rebuilt on a recycled IP.
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
rm -rf "$HOME/.cache/kurin/clusters/$CLUSTER" "$HOME/.cache/kurin/runs/$CLUSTER.json"

export KURIN_SSH_PASSWORD="$PASS"
cd "$WORK"
echo "[tier1-quick] create cluster from arguments only (password auth)"
if ! "$BIN" create cluster \
    --name "$CLUSTER" \
    --control-plane "$IP" \
    --ssh-user root \
    --ssh-password-env KURIN_SSH_PASSWORD \
    --kubernetes 1.33.1 \
    --yes --json > "$EVENTS" 2> >(tee /tmp/kurin-tier1-quick-err.log >&2); then
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
grep -q 'password = { env = "KURIN_SSH_PASSWORD" }' "$SAVED" || {
    echo "[FAIL] saved config lacks the secret reference:"; cat "$SAVED"; exit 1; }
if grep -q "$PASS" "$SAVED"; then
    echo "[FAIL] saved config contains the plaintext password"; exit 1
fi
grep -q '^version = "v1alpha2"$' "$SAVED" || { echo "[FAIL] saved config is not v1alpha2"; exit 1; }
echo "[ok] saved config references the secret, no plaintext"

echo "[tier1-quick] waiting for $CLUSTER node Ready"
for i in $(seq 1 30); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node -o jsonpath='{range .items[*]}{.status.conditions[?(@.type==\"Ready\")].status}{end}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "${READY:-}" = "True" ] || { echo "[FAIL] node Ready=${READY:-none} (after 300s)"; exit 1; }
echo "[ok] node Ready (~$((i*10))s)"

echo "tier1-quick: PASS"