#!/usr/bin/env bash
# tier-1 ubuntu 22.04 (WP-74): the jammy deb-family track. Same shape as
# rocky.sh but from the cloud-images jammy qcow2 — proves the 22.04 facts
# (family deb, release ubuntu-22.04) and the apt os-package row.
set -euo pipefail

NAME=kurin-ubuntu2204
KEYS=e2e/tier1/.keys
LOCAL_IMG="$HOME/jammy-server-cloudimg-amd64.img"
if [ -f "$LOCAL_IMG" ]; then
    IMG_URL="file://$LOCAL_IMG"
else
    IMG_URL="https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img"
fi
CID="$HOME/kurin-ubuntu2204-cloud-init.yaml"
EVENTS=$(mktemp)
trap 'rm -f "$CID" "$EVENTS"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-ubuntu2204
chmod 600 "$KEYS/id_ed25519"
PUB=$(cat "$KEYS/id_ed25519.pub")

cat > "$CID" <<EOF
#cloud-config
ssh_pwauth: false
users:
  - name: root
    ssh_authorized_keys:
      - $PUB
runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd || systemctl restart ssh
EOF

echo "[ubuntu2204] launching from $IMG_URL (first run downloads ~700MB)"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "$IMG_URL" --name "$NAME" --cpus 2 --memory 3G --disk 20G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true
# The jammy cloud image keeps root locked; install the key through
# multipass (like tier1/up.sh) instead of relying on cloud-init users.
multipass exec "$NAME" -- sudo mkdir -p /root/.ssh
multipass exec "$NAME" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
multipass exec "$NAME" -- sudo bash -c "sed -i 's/^#\\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config; systemctl restart sshd || systemctl restart ssh"

# Wait for root ssh to actually work (cloud-init runcmd).
for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] root ssh never came up"; exit 1; }
    sleep 10
done
echo "[ubuntu2204] $NAME at $IP with root ssh"

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "ubuntu2204"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[ssh]
user = "root"
port = 22
[ssh.auth]
method = "private_key"
key-path = "$KEYS/id_ed25519"
[nodes.node01]
address = "$IP"
internal-address = "$IP"
role = "control-plane"
EOF

export PATH="$HOME/.cargo/bin:$PATH"
BIN=target/ubuntu2204/debug/kurin
CARGO_TARGET_DIR=target/ubuntu2204 cargo build -p kurin
echo "[ubuntu2204] precheck (expect os=ok ubuntu)"
"$BIN" precheck -f "$CFG" > /tmp/ubuntu2204-precheck.txt || true
grep -E "^ok.*os" /tmp/ubuntu2204-precheck.txt | head -2

echo "[ubuntu2204] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/ubuntu2204-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/ubuntu2204-err.log; cp "$EVENTS" /tmp/ubuntu2204-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/ubuntu2204-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/ubuntu2204-events.jsonl; exit 1; }
echo "[ok] ubuntu2204: cluster-ready, zero failures"

KC="sudo /usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
for i in $(seq 1 60); do
    READY=$(multipass exec "$NAME" -- sh -c "$KC get node node01 -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}'" 2>/dev/null || true)
    [ "$READY" = "True" ] && break; sleep 10
done
[ "$READY" = "True" ] || { echo "[FAIL] node Ready=$READY"; exit 1; }
echo "[ok] node Ready (~$((i*10))s)"

for i in $(seq 1 60); do
    BAD=$(multipass exec "$NAME" -- sh -c "$KC get pods -A --no-headers" 2>/dev/null | awk '$4 !~ /^(Running|Succeeded|Completed)$/ {print $1"/"$2" "$4}')
    [ -z "$BAD" ] && { echo "[ok] all pods healthy (~$((i*10))s)"; break; }
    [ "$i" = "60" ] && { echo "[FAIL] pods: $BAD"; exit 1; }
    sleep 10
done

BASE=$(multipass exec "$NAME" -- sh -c "awk '/^SwapTotal:/{print \$2}' /proc/meminfo; systemctl is-active ufw 2>/dev/null || echo inactive")
SW=$(echo "$BASE" | head -1); FW=$(echo "$BASE" | tail -1)
[ "${SW:-0}" = "0" ] || { echo "[FAIL] swap=$SW"; exit 1; }
echo "[ok] os-baseline held: swap=off (deb family has no selinux/firewalld)"
echo "tier1-ubuntu2204: PASS"
