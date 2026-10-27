#!/usr/bin/env bash
# tier-1 non-root: the become/sudo track, end to end. Boots one ubuntu 24.04
# multipass VM, provisions the stock `ubuntu` user with key auth and
# passwordless sudo, then runs a single-node kubeadm create + delete
# entirely as that non-root user ([ssh] user = "ubuntu", become = "auto").
#
# What this proves:
#   - precheck's sudo row reports "passwordless sudo" for a non-root login;
#   - every privileged command is wrapped in `sudo -n -- sh -c '...'`;
#   - write_file stages to /tmp and `install`s into place as root, so
#     kurin-owned files (/etc/kurin, binaries, units) end up root-owned;
#   - delete cluster works the same way.
#
# Usage: e2e/tier1/nonroot.sh
# (manual e2e — needs multipass; see docs/progress.md for recorded runs)
set -euo pipefail

NAME=kurin-nonroot
KEYS=e2e/tier1/.keys
EVENTS=$(mktemp)
CFG=$(mktemp --suffix=.toml)
trap 'rm -f "$EVENTS" "$CFG"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-nonroot
PUB=$(cat "$KEYS/id_ed25519.pub")

multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch --name "$NAME" --cpus 2 --memory 3G --disk 15G 24.04

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4 found" >&2; exit 1; }
# VM rebuilds keep recycled IPs: purge the stale host key from kurin's
# TOFU store so the rebuilt node is not rejected.
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
echo "[nonroot] $NAME at $IP; provisioning the ubuntu user (key + passwordless sudo)"
multipass exec "$NAME" -- sudo bash -c \
    "install -d -m 700 -o ubuntu -g ubuntu /home/ubuntu/.ssh && \
     echo '$PUB' > /home/ubuntu/.ssh/authorized_keys && \
     chown ubuntu:ubuntu /home/ubuntu/.ssh/authorized_keys && chmod 600 /home/ubuntu/.ssh/authorized_keys"
# Passwordless sudo for the whole track (that is what become = "auto" uses).
multipass exec "$NAME" -- sudo bash -c \
    "echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/90-kurin-nonroot-test && chmod 440 /etc/sudoers.d/90-kurin-nonroot-test"

# Wait for ubuntu ssh to actually work (sshd may still be restarting).
for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes ubuntu@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] ubuntu ssh never came up"; exit 1; }
    sleep 10
done
echo "[nonroot] ubuntu@$IP ready (passwordless sudo configured)"

cat > "$CFG" <<EOF
version = "v1alpha1"
name = "nonroot"

[kubernetes]
version = "1.33.1"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[ssh]
user = "ubuntu"
port = 22
become = "auto"

[ssh.auth]
method = "private_key"
key-path = "$PWD/$KEYS/id_ed25519"

[nodes.node01]
address = "$IP"
internal-address = "$IP"
role = "control-plane"
EOF

export PATH="$HOME/.cargo/bin:$PATH"
BIN=target/nonroot/debug/kurin
CARGO_TARGET_DIR=target/nonroot cargo build -p kurin

echo "[nonroot] precheck (expect: sudo ok 'passwordless sudo', sysctl ok)"
"$BIN" precheck -f "$CFG" > /tmp/nonroot-precheck.txt || true
grep -E "sudo|sysctl" /tmp/nonroot-precheck.txt | head -4
grep -q 'passwordless sudo' /tmp/nonroot-precheck.txt || {
    echo "[FAIL] precheck did not report passwordless sudo"; exit 1
}
grep -qE '^FAIL' /tmp/nonroot-precheck.txt && {
    echo "[FAIL] precheck has failing rows"; exit 1
}

echo "[nonroot] create cluster (as ubuntu, via sudo)"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/nonroot-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/nonroot-err.log; cp "$EVENTS" /tmp/nonroot-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready event"; cp "$EVENTS" /tmp/nonroot-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/nonroot-events.jsonl; exit 1; }
echo "[ok] cluster-ready, zero task failures"

echo "[nonroot] asserting kurin-written files are root-owned"
OWNER=$(multipass exec "$NAME" -- stat -c '%U' /etc/kurin/artifacts.marker)
[ "$OWNER" = "root" ] || { echo "[FAIL] /etc/kurin/artifacts.marker owned by $OWNER, expected root"; exit 1; }
OWNER=$(multipass exec "$NAME" -- stat -c '%U' /usr/local/bin/kubectl)
[ "$OWNER" = "root" ] || { echo "[FAIL] kubectl owned by $OWNER, expected root"; exit 1; }
echo "[ok] /etc/kurin and binaries are root-owned"

KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
for i in $(seq 1 60); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break
    sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node Ready=$READY"; exit 1; }
echo "[ok] node Ready (~$((i*10))s)"

echo "[nonroot] delete cluster (as ubuntu, via sudo)"
if ! "$BIN" delete cluster -f "$CFG" 2> >(tee /tmp/nonroot-delete-err.log >&2); then
    echo "[FAIL] delete cluster exited nonzero"; exit 1
fi
if multipass exec "$NAME" -- sudo test -e /etc/kubernetes; then
    echo "[FAIL] kubeadm state still present after delete"; exit 1
fi
ACTIVE=$(multipass exec "$NAME" -- sh -c 'systemctl is-active kubelet 2>/dev/null || true')
[ "$ACTIVE" = "active" ] && { echo "[FAIL] kubelet still active"; exit 1; }
echo "[ok] delete cluster: node reset (no /etc/kubernetes, kubelet inactive)"

echo "tier1-nonroot: PASS"
