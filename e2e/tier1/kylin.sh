#!/usr/bin/env bash
# tier-1 Kylin V10 (WP-74): the kylin family track — MANUAL ONLY.
#
# Kylin Linux Advanced Server V10 has no public cloud image or
# container registry to download from: the ISO ships through the
# vendor portal (www.kylinos.cn) after registration. This script
# therefore takes a locally prepared qcow2 and refuses to run without
# one, instead of guessing an unverifiable URL.
#
# Preparing the image (one-time, on a machine with the ISO):
#   # any cloud-init-capable Kylin V10 SPx qcow2 works; to build one
#   # from the server ISO with virt-install:
#   virt-install --name kylin-v10 --ram 4096 --vcpus 2 --disk \
#     size=20,path=$HOME/kylin-v10.qcow2,format=qcow2 \
#     --cdrom Kylin-Server-V10-SP3-x86_64.iso --os-variant rhel8.1 \
#     --graphics none --network network=default
#   # then install cloud-init + openssh-server inside and shut it down.
#
# Usage: e2e/tier1/kylin.sh /path/to/kylin-v10.qcow2
set -euo pipefail

[ $# -ge 1 ] && [ -f "$1" ] || {
    echo "usage: $0 /path/to/kylin-v10.qcow2" >&2
    echo "Kylin V10 images are not publicly downloadable; prepare one" >&2
    echo "from the vendor ISO first (see the header of this script)." >&2
    exit 2
}
IMG=$(cd "$(dirname "$1")" && pwd)/$(basename "$1") # absolute

NAME=kurin-kylin10
KEYS=e2e/tier1/.keys
CID="$HOME/kurin-kylin-cloud-init.yaml"
EVENTS=$(mktemp)
trap 'rm -f "$CID" "$EVENTS"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-kylin
chmod 600 "$KEYS/id_ed25519"
PUB=$(cat "$KEYS/id_ed25519.pub")

cat > "$CID" <<EOF
#cloud-config
ssh_pwauth: false
disable_root: false
users:
  - name: root
    ssh_authorized_keys:
      - $PUB
runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd || systemctl restart ssh
EOF

echo "[kylin] launching from $IMG"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "file://$IMG" --name "$NAME" --cpus 2 --memory 3G --disk 20G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true
# Cloud images may keep root locked; install the key through multipass
# (same as tier1/up.sh) instead of relying on cloud-init users.
multipass exec "$NAME" -- sudo mkdir -p /root/.ssh
multipass exec "$NAME" -- sudo bash -c "echo '$PUB' > /root/.ssh/authorized_keys && chmod 700 /root/.ssh && chmod 600 /root/.ssh/authorized_keys"
multipass exec "$NAME" -- sudo bash -c "sed -i 's/^#\\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config; systemctl restart sshd || systemctl restart ssh"

for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] root ssh never came up"; exit 1; }
    sleep 10
done
echo "[kylin] $NAME at $IP with root ssh"

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "kylin"
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
BIN=target/kylin/debug/kurin
CARGO_TARGET_DIR=target/kylin cargo build -p kurin
echo "[kylin] precheck (expect os=ok kylin)"
"$BIN" precheck -f "$CFG" > /tmp/kylin-precheck.txt || true
grep -E "^ok.*os" /tmp/kylin-precheck.txt | head -2

echo "[kylin] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/kylin-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/kylin-err.log; cp "$EVENTS" /tmp/kylin-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/kylin-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/kylin-events.jsonl; exit 1; }
grep -q '"id":"prepare/os-baseline@node01"' "$EVENTS" || { echo "[FAIL] no os-baseline event"; exit 1; }
echo "[ok] kylin10: cluster-ready, zero failures (os-baseline ran)"

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

BASE=$(multipass exec "$NAME" -- sh -c "getenforce 2>/dev/null || echo Disabled; awk '/^SwapTotal:/{print \$2}' /proc/meminfo; systemctl is-active firewalld 2>/dev/null || echo inactive")
SEL=$(echo "$BASE" | head -1); SW=$(echo "$BASE" | sed -n 2p); FW=$(echo "$BASE" | tail -1)
[ "$SEL" = "Permissive" ] || [ "$SEL" = "Disabled" ] || { echo "[FAIL] selinux=$SEL"; exit 1; }
[ "${SW:-0}" = "0" ] || { echo "[FAIL] swap=$SW"; exit 1; }
[ "$FW" = "inactive" ] || { echo "[FAIL] firewalld=$FW"; exit 1; }
echo "[ok] os-baseline held: selinux=$SEL swap=off firewalld=$FW"
echo "tier1-kylin10: PASS"
