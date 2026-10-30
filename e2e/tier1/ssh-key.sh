#!/usr/bin/env bash
# tier-1 (ssh-key): build the single-node cluster from CLI arguments with
# password auth + `--setup-ssh-key`, then prove later operations need no
# password: `status` runs against the saved key-auth config with the
# password variable unset. Asserts the saved config holds private-key
# auth (never the password) and that authorized_keys carries the control
# key exactly once — re-running the create converges and does not append a
# second copy (the push is idempotent).
#
# Usage: e2e/tier1/ssh-key.sh [BIN]
set -euo pipefail

BIN=${1:-target/tier1/debug/kurin}
NAME=kurin-tier1
CLUSTER=tier1-sshkey
PASS='kurin-e2e-fake-pass'
KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
WORK=$(mktemp -d) EVENTS=$(mktemp)
trap 'rm -rf "$WORK" "$EVENTS"; e2e/tier1/down.sh' EXIT

echo "[tier1-sshkey] booting VM via tier1 up.sh"
e2e/tier1/up.sh

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4 found"; exit 1; }

echo "[tier1-sshkey] enabling root password auth on $IP"
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

# Keep the control key away from the operator's real ~/.ssh: a throwaway
# HOME makes the default path generate a fresh pair inside $WORK.
export HOME="$WORK/home"
mkdir -p "$HOME"

export KURIN_SSH_PASSWORD="$PASS"
cd "$WORK"

create_args=(
    --name "$CLUSTER"
    --control-plane "$IP"
    --ssh-user root
    --ssh-password-env KURIN_SSH_PASSWORD
    --setup-ssh-key
    --kubernetes 1.37.2
    --yes --json
)

echo "[tier1-sshkey] create cluster with --setup-ssh-key (password auth)"
if ! "$BIN" create cluster "${create_args[@]}" \
    > "$EVENTS" 2> >(tee /tmp/kurin-tier1-sshkey-err.log >&2); then
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
grep -q 'method = "private_key"' "$SAVED" || {
    echo "[FAIL] saved config is not private-key auth:"; cat "$SAVED"; exit 1; }
if grep -q "$PASS" "$SAVED"; then
    echo "[FAIL] saved config contains the plaintext password"; exit 1
fi
grep -q '^version = "v1alpha2"$' "$SAVED" || { echo "[FAIL] saved config is not v1alpha2"; exit 1; }
echo "[ok] saved config uses private-key auth, no plaintext"

KEYPATH=$(awk -F'"' '/^key-path =/{print $2}' "$SAVED")
[ -n "$KEYPATH" ] || { echo "[FAIL] no key-path in saved config"; exit 1; }
[ -f "$KEYPATH" ] || { echo "[FAIL] key-path $KEYPATH missing"; exit 1; }
[ -f "$KEYPATH.pub" ] || { echo "[FAIL] public key $KEYPATH.pub missing"; exit 1; }
PUBLINE=$(cut -d' ' -f1-2 "$KEYPATH.pub")
[ -n "$PUBLINE" ] || { echo "[FAIL] empty public key"; exit 1; }

count_authorized() {
    multipass exec "$NAME" -- grep -cF "$PUBLINE" /root/.ssh/authorized_keys
}

echo "[tier1-sshkey] authorized_keys carries the key exactly once"
COUNT=$(count_authorized)
[ "$COUNT" = "1" ] || { echo "[FAIL] authorized_keys has $COUNT copies"; exit 1; }
echo "[ok] one authorized_keys entry"

echo "[tier1-sshkey] status without the password (key auth from the saved config)"
unset KURIN_SSH_PASSWORD
if ! "$BIN" status -f "$SAVED" > "$WORK/status.txt" 2> >(tee /tmp/kurin-tier1-sshkey-status.log >&2); then
    echo "[FAIL] status without the password failed:"; cat "$WORK/status.txt"; exit 1
fi
echo "[ok] status needs no password"

echo "[tier1-sshkey] re-running create converges without duplicating the key"
export KURIN_SSH_PASSWORD="$PASS"
if ! "$BIN" create cluster "${create_args[@]}" \
    > "$EVENTS" 2> >(tee /tmp/kurin-tier1-sshkey-err2.log >&2); then
    echo "[FAIL] second create exited nonzero; last events:"
    tail -5 "$EVENTS" || true
    exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event (re-run)"; exit 1; }
COUNT=$(count_authorized)
[ "$COUNT" = "1" ] || { echo "[FAIL] authorized_keys has $COUNT copies after the re-run"; exit 1; }
echo "[ok] idempotent re-run"

echo "tier1-sshkey: PASS"
