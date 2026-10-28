#!/usr/bin/env bash
# tier-1 alma 9 (WP-74): the RHEL-9 rebuild track. Same shape as rocky.sh
# but from AlmaLinux's GenericCloud qcow2 — proves the alma facts
# (family rpm, release alma-9) and the almalinux-9 os-package row.
set -euo pipefail

NAME=kurin-alma9
KEYS=e2e/tier1/.keys
LOCAL_IMG="$HOME/AlmaLinux-9-GenericCloud-latest.x86_64.qcow2"
if [ -f "$LOCAL_IMG" ]; then
    IMG_URL="file://$LOCAL_IMG"
else
    # The canonical repo host serves the GenericCloud qcow2 directly;
    # dl.almalinux.org redirects to mirrors that may be unreachable.
    IMG_URL="https://repo.almalinux.org/almalinux/9/cloud/x86_64/images/AlmaLinux-9-GenericCloud-latest.x86_64.qcow2"
fi
CID="$HOME/kurin-alma9-cloud-init.yaml"
EVENTS=$(mktemp)
trap 'rm -f "$CID" "$EVENTS"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-alma9
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

echo "[alma9] launching from $IMG_URL (first run downloads ~2GB)"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "$IMG_URL" --name "$NAME" --cpus 2 --memory 3G --disk 20G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true
ssh-keygen -R "$IP" -f ~/.cache/kurin/known_hosts >/dev/null 2>&1 || true
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
echo "[alma9] $NAME at $IP with root ssh"

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "alma9"
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
BIN=target/alma9/debug/kurin
CARGO_TARGET_DIR=target/alma9 cargo build -p kurin
echo "[alma9] precheck (expect os=ok almalinux, selinux warn->fixed by baseline)"
"$BIN" precheck -f "$CFG" > /tmp/alma9-precheck.txt || true
grep -E "os|selinux" /tmp/alma9-precheck.txt | head -3

echo "[alma9] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/alma9-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/alma9-err.log; cp "$EVENTS" /tmp/alma9-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/alma9-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/alma9-events.jsonl; exit 1; }
grep -q '"id":"prepare/os-baseline@node01"' "$EVENTS" || { echo "[FAIL] no os-baseline event"; exit 1; }
echo "[ok] alma9: cluster-ready, zero failures (os-baseline ran)"

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
echo "tier1-alma9: PASS"
