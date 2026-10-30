#!/usr/bin/env bash
# tier-1 rocky: the rpm-family track. Boots a Rocky 9 GenericCloud image via
# its published qcow2 URL (multipass can launch remote images), root ssh via
# cloud-injected key, then a single-node kubeadm cluster + calico.
set -euo pipefail

NAME=kurin-rocky9
KEYS=e2e/tier1/.keys
LOCAL_IMG="$HOME/rocky-9-genericcloud.qcow2"
if [ -f "$LOCAL_IMG" ]; then
    IMG_URL="file://$LOCAL_IMG"
else
    # Official CDN is ~1MB/s from this host; the Singapore mirror is 20x.
    IMG_URL="https://sg.mirrors.cicku.me/rocky/9/images/x86_64/Rocky-9-GenericCloud-Base.latest.x86_64.qcow2"
fi
CID="$HOME/kurin-rocky-cloud-init.yaml"
EVENTS=$(mktemp)
trap 'rm -f "$CID" "$EVENTS"; multipass delete -p "$NAME" >/dev/null 2>&1 || true' EXIT

mkdir -p "$KEYS"
[ -f "$KEYS/id_ed25519" ] || ssh-keygen -t ed25519 -N "" -f "$KEYS/id_ed25519" -q -C kurin-rocky
PUB=$(cat "$KEYS/id_ed25519.pub")

cat > "$CID" <<EOF
#cloud-config
ssh_pwauth: false
disable_root: false
ssh_root_authorized_keys: true
users:
  - name: root
    ssh_authorized_keys:
      - $PUB
runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd || systemctl restart ssh
EOF

echo "[rocky] launching from $IMG_URL (first run downloads ~2GB)"
multipass info "$NAME" >/dev/null 2>&1 || \
    multipass launch "$IMG_URL" --name "$NAME" --cpus 2 --memory 3G --disk 20G --cloud-init "$CID" >/dev/null

IP=$(multipass info "$NAME" --format yaml | awk '/^    ipv4:/{getline; print $2; exit}')
[ -n "$IP" ] || { echo "[FAIL] no ipv4"; exit 1; }
ssh-keygen -R "$IP" -f ~/.ssh/known_hosts >/dev/null 2>&1 || true

# Wait for root ssh to actually work (cloud-init runcmd).
for i in $(seq 1 30); do
    if ssh -i "$KEYS/id_ed25519" -o StrictHostKeyChecking=no -o ConnectTimeout=5 \
        -o UserKnownHostsFile=/dev/null -o BatchMode=yes root@"$IP" true 2>/dev/null; then
        break
    fi
    [ "$i" = "30" ] && { echo "[FAIL] root ssh never came up"; exit 1; }
    sleep 10
done
echo "[rocky] $NAME at $IP with root ssh"

CFG=$(mktemp --suffix=.toml)
cat > "$CFG" <<EOF
version = "v1alpha1"
name = "rocky9"
[kubernetes]
version = "1.37.2"
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
BIN=target/rocky9/debug/kurin
CARGO_TARGET_DIR=target/rocky9 cargo build -p kurin
echo "[rocky] precheck (expect os=ok rocky, selinux warn->fixed by baseline)"
"$BIN" precheck -f "$CFG" > /tmp/rocky-precheck.txt || true
grep -E "os|selinux" /tmp/rocky-precheck.txt | head -3

echo "[rocky] create cluster"
if ! "$BIN" create cluster -f "$CFG" --json > "$EVENTS" 2>/tmp/rocky-err.log; then
    echo "[FAIL] create exited nonzero"; tail -6 /tmp/rocky-err.log; cp "$EVENTS" /tmp/rocky-events.jsonl; exit 1
fi
grep -q '"cluster-ready"' "$EVENTS" || { echo "[FAIL] no cluster-ready"; cp "$EVENTS" /tmp/rocky-events.jsonl; exit 1; }
fails=$(grep -c '"task-failed"' "$EVENTS" || true)
[ "$fails" -eq 0 ] || { echo "[FAIL] $fails task failures"; grep task-failed "$EVENTS"; cp "$EVENTS" /tmp/rocky-events.jsonl; exit 1; }
grep -q '"id":"prepare/os-baseline@node01"' "$EVENTS" || { echo "[FAIL] no os-baseline event"; exit 1; }
echo "[ok] rocky9: cluster-ready, zero failures (os-baseline ran)"

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
echo "$BASE" | head -3 | grep -q . || true
SEL=$(echo "$BASE" | head -1); SW=$(echo "$BASE" | sed -n 2p); FW=$(echo "$BASE" | tail -1)
[ "$SEL" = "Permissive" ] || [ "$SEL" = "Disabled" ] || { echo "[FAIL] selinux=$SEL"; exit 1; }
[ "${SW:-0}" = "0" ] || { echo "[FAIL] swap=$SW"; exit 1; }
[ "$FW" = "inactive" ] || { echo "[FAIL] firewalld=$FW"; exit 1; }
echo "[ok] os-baseline held: selinux=$SEL swap=off firewalld=$FW"
echo "tier1-rocky9: PASS"